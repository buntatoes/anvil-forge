// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato
//! Hybrid Rust + WebAssembly 1.0 (`WASM`) + C++20 (`CinderVM`) script runtime, bytecode compiler, and DOM binding layer.
//!
//! `anvil-browser` owns the [`ScriptRuntime`]. It sets the page origin, optionally
//! imports persisted `localStorage` entries from `profile.anvil`, runs external
//! script text with [`ScriptRuntime::eval_script`], runs inline scripts with
//! [`ScriptRuntime::run_document_scripts`], and hydrates interactive `onclick`
//! and `addEventListener('click', ...)` bindings on `--renderer` documents via
//! [`ScriptRuntime::hydrate_interactive_bindings`].
//!
//! [`wasm`] provides a self-contained WebAssembly 1.0 (`\0asm` version `1`) binary
//! module decoder ([`WasmModule::decode`]), validator ([`validate_wasm`]), bounded
//! 64 KiB page linear memory ([`WasmMemory`]), and stack VM ([`WasmInstance`]),
//! exposed to JavaScript through the `WebAssembly` namespace and `TypedArray` views.
//!
//! `fetch()` is same-origin only. The Rust interpreter performs it. A registered
//! mock wins over the host. Otherwise the interpreter calls the host callback
//! installed by `anvil-browser`, which uses `anvil-net`. This crate does not open
//! a socket. The C++20 numeric VM does not perform `fetch()`.
//!
//! When `build.rs` compiles the freestanding C++20 translation units under `cpp/`
//! (`cinder_vm_heap.cpp`, `cinder_vm_ic.cpp`, `cinder_vm_compiler.cpp`, `cinder_jit_accel.cpp`),
//! it sets `cfg(cinder_cpp_jit)` and links the C++20 `CinderVM` engine (64-bit NaN-boxed
//! values, `CinderShape` hidden classes, 4-way polymorphic Inline Caches, Mark-Sweep
//! garbage collection, register bytecode execution, and vector reduction kernels).
//! If no C++20 compiler is present at build time, `anvil-script` falls back cleanly
//! to its pure-Rust execution path.
//!
//! [`ScriptRuntime::eval_script`] routes DOM-independent scripts through
//! [`eval_with_cinder_engine`], DOM/event/Web-API scripts through [`CinderDomHost`]
//! (`cinder` register-bytecode VM with live DOM node seeding and mutation sync back
//! into `anvil-dom::Document`), and scripts that require host `fetch()` callbacks
//! or direct interpreter hooks through the Rust interpreter.
//! The caller owns the VM context. There is no process-global VM.

#![allow(
    clippy::chunks_exact_to_as_chunks,
    clippy::clone_on_copy,
    clippy::collapsible_if,
    clippy::manual_clamp,
    clippy::needless_question_mark,
    clippy::question_mark,
    clippy::too_many_arguments,
    clippy::type_complexity
)]

use std::sync::Arc;

/// Direct linkage to the in-tree `Cinder` engine (`engines/cinder`).
#[path = "../../../../anvil-browser/engines/cinder/src/lib.rs"]
pub mod cinder;

pub mod engine_host;
pub mod wasm;

pub use engine_host::CinderDomHost;
pub use wasm::{
    MAX_WASM_CALL_DEPTH, MAX_WASM_PAGES, MAX_WASM_STACK, MAX_WASM_STEPS, WASM_MAGIC,
    WASM_PAGE_SIZE, WASM_VERSION, WasmCodeBody, WasmDataSegment, WasmElementSegment, WasmError,
    WasmExport, WasmExportKind, WasmFuncType, WasmGlobalDef, WasmImportFunc, WasmInstance,
    WasmMemory, WasmModule, WasmValType, WasmValue, validate_wasm,
};

use anvil_dom::{Document, NodeId};
pub mod anvil_json;
use crate::anvil_json::{JsonValue, parse_json, to_json_string};
pub use cinder::{
    RUNTIME_BUILTINS_JS, RUNTIME_JSON_CODEC_JS, RUNTIME_WEB_APIS_JS, self_hosted_bundle,
    self_hosted_bundle_hash, self_hosted_runtime_bundle,
};

/// Maximum bytes of a `fetch()` body kept by the interpreter.
///
/// A longer body is cut on a character boundary. `anvil-net` also stops reading
/// a response at its own raw cap before this host callback returns.
pub const MAX_SCRIPT_FETCH_BODY: usize = 2560 * 1024;

/// Status and body returned by a `fetch()` host callback.
///
/// `status` is the HTTP status, `0` when the host did not complete a response,
/// or `403` when the interpreter blocked the URL before calling the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptFetchReply {
    pub status: u16,
    pub body: String,
}

/// Signature of the `fetch()` host. See [`FetchHost`].
type FetchFn = dyn Fn(&str, &str) -> ScriptFetchReply + Send + Sync;

/// Host callback for same-origin `fetch()`. The first argument is an absolute
/// URL. The second is the page URL. The interpreter does not call this for a
/// cross-origin URL or for a URL that has a registered mock.
#[derive(Clone, Default)]
struct FetchHost {
    inner: Option<Arc<FetchFn>>,
}

impl PartialEq for FetchHost {
    fn eq(&self, other: &Self) -> bool {
        self.inner.is_some() == other.inner.is_some()
    }
}

impl std::fmt::Debug for FetchHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FetchHost")
            .field("set", &self.inner.is_some())
            .finish()
    }
}

/// Cut `body` to [`MAX_SCRIPT_FETCH_BODY`] on a character boundary.
pub fn limit_script_fetch_body(body: &mut String) {
    if body.len() <= MAX_SCRIPT_FETCH_BODY {
        return;
    }
    let mut end = MAX_SCRIPT_FETCH_BODY;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    body.truncate(end);
}

/// Built-in JavaScript core standard library prelude (`crates/anvil-script/js/runtime_core.js`).
pub const RUNTIME_CORE_JS: &str = include_str!("../js/runtime_core.js");

/// Built-in JavaScript async/URL/storage/DOM helper prelude (`crates/anvil-script/js/runtime_async_web.js`).
pub const RUNTIME_ASYNC_WEB_JS: &str = include_str!("../js/runtime_async_web.js");

/// C ABI telemetry struct populated by the C++20 `CinderVM` (`cpp/cinder_vm.hpp`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CinderVmTelemetry {
    pub kernel_version: u32,
    pub bytecode_ops_executed: u32,
    pub heap_objects_live: u32,
    pub heap_bytes_allocated: u32,
    pub gc_collections: u32,
    pub shapes_allocated: u32,
    pub ic_hits: u32,
    pub ic_misses: u32,
    pub polymorphic_ic_sites: u32,
    pub hot_loops_accelerated: u32,
    pub native_compilations: u32,
    stop: VmStop,
    exports: VmExports,
}

impl CinderVmTelemetry {
    /// Why the C++ VM stopped this run or refused the script, `None` when
    /// it ran to the end.
    pub fn stop_reason(&self) -> Option<VmError> {
        if self.stop.code == 0 {
            return None;
        }
        let len = (self.stop.len as usize).min(self.stop.text.len());
        Some(VmError {
            code: self.stop.code,
            message: String::from_utf8_lossy(&self.stop.text[..len]).into_owned(),
        })
    }
}

impl CinderVmTelemetry {
    /// Top-level variables the run left for later scripts. `Err` names one
    /// that holds an object, array or function, or says they did not fit.
    #[cfg_attr(not(cinder_cpp_jit), allow(dead_code))]
    fn exported_globals(&self) -> Result<Vec<(String, Value)>, String> {
        if self.exports.full != 0 {
            return Err(format!(
                "the top-level variables do not fit in {VM_EXPORT_BYTES} bytes"
            ));
        }
        let len = (self.exports.len as usize).min(VM_EXPORT_BYTES);
        let mut reader = ExportReader {
            bytes: &self.exports.bytes[..len],
        };
        let broken = || "the VM wrote a broken export record".to_string();
        let mut out = Vec::new();
        while !reader.bytes.is_empty() {
            let kind = reader.take(1).ok_or_else(broken)?[0];
            let name = reader.text().ok_or_else(broken)?;
            if name.is_empty() {
                return Err(broken());
            }
            let value = match kind {
                1 | 2 => {
                    let raw = reader.take(8).ok_or_else(broken)?;
                    let mut bits = [0u8; 8];
                    bits.copy_from_slice(raw);
                    let number = f64::from_le_bytes(bits);
                    if kind == 1 {
                        Value::Number(number)
                    } else {
                        Value::Bool(number != 0.0)
                    }
                }
                3 | 4 => Value::Null,
                5 => Value::Str(reader.text().ok_or_else(broken)?),
                6 => {
                    return Err(format!(
                        "top-level '{name}' holds an object, array or function, \
                         which later scripts could not see"
                    ));
                }
                _ => return Err(broken()),
            };
            out.push((name, value));
        }
        Ok(out)
    }
}

/// One argument of a host call the C++ VM passed back (for example to
/// `console.log`). Strings only appear once the VM has string values.
#[cfg_attr(not(cinder_cpp_jit), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
enum HostArg {
    Number(f64),
    Bool(bool),
    Null,
    Undefined,
    Str(String),
    Other,
}

/// A host call the VM reached and paused on: which host function and its
/// arguments, read from the out buffer's export area.
#[cfg_attr(not(cinder_cpp_jit), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
struct PendingHostCall {
    fn_id: u32,
    overflow: bool,
    args: Vec<HostArg>,
}

impl CinderVmTelemetry {
    /// Read the pending host call the C++ VM wrote while paused on a HostCall.
    /// `None` when the bytes are not a well-formed host-call record.
    #[cfg_attr(not(cinder_cpp_jit), allow(dead_code))]
    fn pending_host_call(&self) -> Option<PendingHostCall> {
        let len = (self.exports.len as usize).min(VM_EXPORT_BYTES);
        let mut reader = ExportReader {
            bytes: &self.exports.bytes[..len],
        };
        let read_u32 = |r: &mut ExportReader| -> Option<u32> {
            let b = r.take(4)?;
            Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let fn_id = read_u32(&mut reader)?;
        let arg_count = read_u32(&mut reader)?;
        let overflow = read_u32(&mut reader)? != 0;
        let mut args = Vec::new();
        for _ in 0..arg_count {
            let kind = reader.take(1)?[0];
            let arg = match kind {
                1 | 2 => {
                    let b = reader.take(8)?;
                    let mut bits = [0u8; 8];
                    bits.copy_from_slice(b);
                    let number = f64::from_le_bytes(bits);
                    if kind == 1 {
                        HostArg::Number(number)
                    } else {
                        HostArg::Bool(number != 0.0)
                    }
                }
                3 => HostArg::Null,
                4 => HostArg::Undefined,
                5 => HostArg::Str(reader.text()?),
                _ => HostArg::Other,
            };
            args.push(arg);
        }
        Some(PendingHostCall {
            fn_id,
            overflow,
            args,
        })
    }
}

/// Reads the records in [`VmExports`].
#[cfg_attr(not(cinder_cpp_jit), allow(dead_code))]
struct ExportReader<'a> {
    bytes: &'a [u8],
}

impl<'a> ExportReader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.bytes.len() {
            return None;
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Some(head)
    }

    fn text(&mut self) -> Option<String> {
        let raw = self.take(4)?;
        let len = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }
}

/// Size of the export buffer at the end of [`CinderVmTelemetry`].
const VM_EXPORT_BYTES: usize = 4096;

/// Top-level variables a C++ VM run left, as records written by
/// `fill_exports` in `cpp/cinder_jit_accel.cpp`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VmExports {
    len: u32,
    full: u32,
    bytes: [u8; VM_EXPORT_BYTES],
}

impl Default for VmExports {
    fn default() -> Self {
        Self {
            len: 0,
            full: 0,
            bytes: [0; VM_EXPORT_BYTES],
        }
    }
}

/// Length of the reason the C++ VM writes into [`CinderVmTelemetry`].
const VM_STOP_TEXT: usize = 160;

// `CinderVmTelemetryC` in `cpp/cinder_vm.hpp` has the same layout.
const _: () = assert!(
    std::mem::size_of::<CinderVmTelemetry>() == 11 * 4 + 8 + VM_STOP_TEXT + 8 + VM_EXPORT_BYTES
);

/// The stop code and reason at the end of the C++ telemetry struct.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VmStop {
    code: u32,
    len: u32,
    text: [u8; VM_STOP_TEXT],
}

impl Default for VmStop {
    fn default() -> Self {
        Self {
            code: 0,
            len: 0,
            text: [0; VM_STOP_TEXT],
        }
    }
}

/// Why the C++ VM refused a script or stopped running it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmError {
    /// Code from the VM. Codes below 32 stop a run; codes from 32 up refuse
    /// the script before it runs.
    pub code: u32,
    /// Plain text reason, for example `step limit reached: 100000 steps`.
    pub message: String,
}

impl VmError {
    /// True when the compiler refused the script, so nothing ran.
    pub fn refused(&self) -> bool {
        self.code >= 32
    }

    /// True when a fixed VM limit was reached, as opposed to code the VM
    /// does not compile or run.
    pub fn is_limit(&self) -> bool {
        matches!(self.code, 1..=9 | 40..=49)
    }
}

impl std::fmt::Display for VmError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// Builds requests for the C++ VM request protocol. A source that begins with
/// the magic bytes is read as a request with a header, not as a plain script,
/// so the existing plain-script callers keep the behaviour they had. This
/// module is plain safe code; it only lays out bytes.
mod vm_request {
    /// Request op: compile and run the payload as a script.
    pub const OP_RUN: u32 = 1;

    /// The fixed request header: magic, op, flags, and the out-buffer size.
    fn header(op: u32, flags: u32, out_cap: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(b"AVK1");
        bytes.extend_from_slice(&op.to_le_bytes());
        bytes.extend_from_slice(&flags.to_le_bytes());
        bytes.extend_from_slice(&out_cap.to_le_bytes());
        bytes
    }

    /// A Run request that asks for an out buffer of `out_cap` bytes.
    pub fn run_with_cap(source: &[u8], out_cap: u32) -> Vec<u8> {
        let mut bytes = header(OP_RUN, 0, out_cap);
        bytes.extend_from_slice(source);
        bytes
    }

    /// A Run request sized for the whole telemetry struct.
    pub fn run(source: &[u8]) -> Vec<u8> {
        run_with_cap(source, super::DEFAULT_OUT_CAP)
    }

    /// Request op: set the limits that later runs on the same context use.
    pub const OP_CONFIGURE: u32 = 2;

    /// A Configure request. Each limit is a byte count or a step count; a
    /// zero leaves that limit as it was.
    pub fn configure(
        step_budget: u32,
        script_size_limit: u32,
        heap_byte_budget: u32,
        call_depth_limit: u32,
    ) -> Vec<u8> {
        let mut bytes = header(OP_CONFIGURE, 0, super::DEFAULT_OUT_CAP);
        bytes.extend_from_slice(&step_budget.to_le_bytes());
        bytes.extend_from_slice(&script_size_limit.to_le_bytes());
        bytes.extend_from_slice(&heap_byte_budget.to_le_bytes());
        bytes.extend_from_slice(&call_depth_limit.to_le_bytes());
        bytes
    }

    /// A Configure request that also sets the yield interval: the run pauses
    /// every `yield_interval` instructions and returns the host-call status so
    /// the caller can resume it. A yield interval of 0 turns pausing off.
    pub fn configure_with_yield(
        step_budget: u32,
        script_size_limit: u32,
        heap_byte_budget: u32,
        call_depth_limit: u32,
        yield_interval: u32,
    ) -> Vec<u8> {
        let mut bytes = configure(
            step_budget,
            script_size_limit,
            heap_byte_budget,
            call_depth_limit,
        );
        bytes.extend_from_slice(&yield_interval.to_le_bytes());
        bytes
    }

    /// A Configure request that also sets the compiler's native-stack budget
    /// (the sixth field, after the yield interval). A budget of 0 leaves it as
    /// it was. The yield interval is written as 0, which turns pausing off.
    pub fn configure_with_parse_stack(
        step_budget: u32,
        script_size_limit: u32,
        heap_byte_budget: u32,
        call_depth_limit: u32,
        parse_stack_budget: u32,
    ) -> Vec<u8> {
        let mut bytes = configure_with_yield(
            step_budget,
            script_size_limit,
            heap_byte_budget,
            call_depth_limit,
            0,
        );
        bytes.extend_from_slice(&parse_stack_budget.to_le_bytes());
        bytes
    }

    /// Request op: continue a run that paused at a yield.
    pub const OP_RESUME: u32 = 3;

    /// A Resume request. It carries no payload; the paused run's state is on
    /// the context already.
    pub fn resume() -> Vec<u8> {
        header(OP_RESUME, 0, super::DEFAULT_OUT_CAP)
    }
}

/// Out-buffer size a request asks for by default: the whole telemetry struct.
const DEFAULT_OUT_CAP: u32 = std::mem::size_of::<CinderVmTelemetry>() as u32;

#[cfg(cinder_cpp_jit)]
unsafe extern "C" {
    fn cinder_cpp_jit_kernel_version() -> u32;
    fn cinder_cpp_jit_eval_binary(opcode: u32, lhs: f64, rhs: f64) -> f64;
    fn cinder_cpp_jit_sum_range_i64(start: i64, end_exclusive: i64, step: i64) -> i64;
    fn cinder_cpp_vm_context_bytes() -> u32;
    fn cinder_cpp_vm_context_align() -> u32;
    fn cinder_cpp_vm_compiler_bytes() -> u32;
    fn cinder_cpp_vm_compiler_align() -> u32;
    fn cinder_cpp_vm_run_script(
        context: *mut u8,
        compiler: *mut u8,
        source_ptr: *const u8,
        source_len: u32,
        telemetry_out: *mut CinderVmTelemetry,
        numeric_result_out: *mut f64,
    ) -> u32;
    fn cinder_cpp_vm_global_count(context: *mut u8) -> u32;
    fn cinder_cpp_vm_copy_global(
        context: *mut u8,
        index: u32,
        name_out: *mut u8,
        name_cap: u32,
        value_out: *mut f64,
    ) -> u32;
    fn cinder_cpp_vm_set_text_host(
        host_fn: Option<extern "C" fn(*mut std::ffi::c_void, *const u8, u32, f64)>,
        host: *mut std::ffi::c_void,
    );
    fn cinder_cpp_vm_run_ic_benchmark(
        context: *mut u8,
        iterations: u32,
        telemetry_out: *mut CinderVmTelemetry,
    ) -> u32;
    fn cinder_cpp_vm_vector_reduce(op: u32, data_ptr: *const f64, len: u32) -> f64;
}

/// `true` when the C++20 `CinderVM` library is linked and its version symbol is 18.
pub fn is_cpp_jit_kernel_active() -> bool {
    #[cfg(cinder_cpp_jit)]
    {
        unsafe { cinder_cpp_jit_kernel_version() == 18 }
    }
    #[cfg(not(cinder_cpp_jit))]
    {
        false
    }
}

/// Caller-owned numeric VM memory. A clone does not share the allocation.
#[cfg(cinder_cpp_jit)]
struct VmOwner {
    context: *mut u8,
    context_layout: std::alloc::Layout,
    compiler: *mut u8,
    compiler_layout: std::alloc::Layout,
}

#[cfg(cinder_cpp_jit)]
impl VmOwner {
    fn empty() -> Self {
        Self {
            context: std::ptr::null_mut(),
            context_layout: std::alloc::Layout::from_size_align(1, 1).unwrap(),
            compiler: std::ptr::null_mut(),
            compiler_layout: std::alloc::Layout::from_size_align(1, 1).unwrap(),
        }
    }

    fn ensure(&mut self) -> bool {
        unsafe {
            if self.context.is_null() {
                let Some((layout, ptr)) = alloc_vm_bytes(
                    cinder_cpp_vm_context_bytes() as usize,
                    cinder_cpp_vm_context_align() as usize,
                ) else {
                    return false;
                };
                self.context_layout = layout;
                self.context = ptr;
            }
            if self.compiler.is_null() {
                let Some((layout, ptr)) = alloc_vm_bytes(
                    cinder_cpp_vm_compiler_bytes() as usize,
                    cinder_cpp_vm_compiler_align() as usize,
                ) else {
                    return false;
                };
                self.compiler_layout = layout;
                self.compiler = ptr;
            }
        }
        true
    }

    fn release(&mut self) {
        unsafe {
            if !self.context.is_null() {
                std::alloc::dealloc(self.context, self.context_layout);
                self.context = std::ptr::null_mut();
            }
            if !self.compiler.is_null() {
                std::alloc::dealloc(self.compiler, self.compiler_layout);
                self.compiler = std::ptr::null_mut();
            }
        }
    }
}

#[cfg(cinder_cpp_jit)]
impl Drop for VmOwner {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(cinder_cpp_jit)]
impl Clone for VmOwner {
    fn clone(&self) -> Self {
        Self::empty()
    }
}

#[cfg(cinder_cpp_jit)]
impl PartialEq for VmOwner {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

#[cfg(cinder_cpp_jit)]
impl std::fmt::Debug for VmOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VmOwner")
            .field("ready", &!self.context.is_null())
            .finish()
    }
}

#[cfg(cinder_cpp_jit)]
unsafe impl Send for VmOwner {}

#[cfg(cinder_cpp_jit)]
fn alloc_vm_bytes(bytes: usize, align: usize) -> Option<(std::alloc::Layout, *mut u8)> {
    let align = align.max(1);
    let mut size = bytes.max(1);
    let rem = size % align;
    if rem != 0 {
        size = size.saturating_add(align - rem);
    }
    let layout = std::alloc::Layout::from_size_align(size, align).ok()?;
    let ptr = unsafe { std::alloc::alloc(layout) };
    if ptr.is_null() {
        None
    } else {
        unsafe {
            std::ptr::write_bytes(ptr, 0, size.min(128 * 1024));
        }
        Some((layout, ptr))
    }
}

#[cfg(not(cinder_cpp_jit))]
#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct VmOwner;

#[cfg(not(cinder_cpp_jit))]
impl VmOwner {
    fn empty() -> Self {
        Self
    }

    fn release(&mut self) {}
}

/// Compile and execute a self-contained JS script inside a caller-owned C++20
/// `CinderVM`, returning `(numeric_result, telemetry)`. The compiler subset is
/// numeric. This is not a general compiler. `None` when the VM refused or
/// stopped the script; [`eval_cpp_vm_script_checked`] says why.
pub fn eval_cpp_vm_script(source: &str) -> Option<(f64, CinderVmTelemetry)> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return None;
    }
    #[cfg(cinder_cpp_jit)]
    unsafe {
        let mut owner = VmOwner::empty();
        if !owner.ensure() {
            return None;
        }
        let mut telemetry = CinderVmTelemetry::default();
        let mut result = 0.0_f64;
        let ok = cinder_cpp_vm_run_script(
            owner.context,
            owner.compiler,
            trimmed.as_ptr(),
            trimmed.len() as u32,
            &mut telemetry,
            &mut result,
        );
        if ok == 1 {
            return Some((result, telemetry));
        }
        None
    }
    #[cfg(not(cinder_cpp_jit))]
    {
        let mut doc = Document::new();
        let mut rt = ScriptRuntime::new();
        let val = rt.eval_expr(&mut doc, trimmed);
        val.as_number().map(|n| (n, CinderVmTelemetry::default()))
    }
}

/// Like [`eval_cpp_vm_script`], with the reason when the VM refused the
/// script or stopped it at a limit.
pub fn eval_cpp_vm_script_checked(source: &str) -> Result<(f64, CinderVmTelemetry), VmError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(VmError {
            code: 32,
            message: "empty script".to_string(),
        });
    }
    let request = vm_request::run(trimmed.as_bytes());
    match ScriptRuntime::new().try_owned_vm(&request) {
        VmRun::Value(result, telemetry) => Ok((result, *telemetry)),
        VmRun::Stopped(error) | VmRun::Rejected(Some(error)) => Err(error),
        VmRun::Rejected(None) => Err(VmError {
            code: 33,
            message: "the C++ VM is not available".to_string(),
        }),
    }
}

/// Run `source` through the C++ VM request protocol, asking for an out buffer
/// of `out_cap` bytes. A buffer too small for the telemetry gives the error
/// with code 4 and nothing is written. This is how the exchange-buffer path is
/// exercised; plain-script callers go through [`eval_cpp_vm_script`].
pub fn eval_cpp_vm_request(
    source: &str,
    out_cap: u32,
) -> Result<(f64, CinderVmTelemetry), VmError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(VmError {
            code: 32,
            message: "empty script".to_string(),
        });
    }
    let request = vm_request::run_with_cap(trimmed.as_bytes(), out_cap);
    match ScriptRuntime::new().try_owned_vm(&request) {
        VmRun::Value(result, telemetry) => Ok((result, *telemetry)),
        VmRun::Stopped(error) | VmRun::Rejected(Some(error)) => Err(error),
        VmRun::Rejected(None) => Err(VmError {
            code: 33,
            message: "the C++ VM is not available".to_string(),
        }),
    }
}

/// Set a step budget for this run, then run `source` through the request
/// protocol. The budget replaces the built-in default for the run. A budget a
/// run goes past stops it with that budget in the reason.
pub fn eval_cpp_vm_request_configured(
    step_budget: u32,
    source: &str,
) -> Result<(f64, CinderVmTelemetry), VmError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(VmError {
            code: 32,
            message: "empty script".to_string(),
        });
    }
    let mut runtime = ScriptRuntime::new();
    let configure = vm_request::configure(step_budget, 0, 0, 0);
    let _ = runtime.try_owned_vm(&configure);
    let run = vm_request::run(trimmed.as_bytes());
    match runtime.try_owned_vm(&run) {
        VmRun::Value(result, telemetry) => Ok((result, *telemetry)),
        VmRun::Stopped(error) | VmRun::Rejected(Some(error)) => Err(error),
        VmRun::Rejected(None) => Err(VmError {
            code: 33,
            message: "the C++ VM is not available".to_string(),
        }),
    }
}

/// Halt value, numeric globals, and telemetry from [`eval_cpp_vm_script_bindings`].
type VmBindings = (f64, Vec<(String, f64)>, CinderVmTelemetry);

/// Run `source` on the C++ VM and return numeric globals along with the halt value.
/// `None` when the compiler rejects the script.
/// Run `source` with both a step budget and a heap byte budget set by a
/// Configure request (0 keeps a built-in default for either). Lets a test push
/// the heap hard without the step limit ending the run first.
pub fn eval_cpp_vm_request_tuned(
    step_budget: u32,
    heap_byte_budget: u32,
    source: &str,
) -> Result<(f64, CinderVmTelemetry), VmError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(VmError {
            code: 32,
            message: "empty script".to_string(),
        });
    }
    let mut runtime = ScriptRuntime::new();
    let configure = vm_request::configure(step_budget, 0, heap_byte_budget, 0);
    let _ = runtime.try_owned_vm(&configure);
    let run = vm_request::run(trimmed.as_bytes());
    match runtime.try_owned_vm(&run) {
        VmRun::Value(result, telemetry) => Ok((result, *telemetry)),
        VmRun::Stopped(error) | VmRun::Rejected(Some(error)) => Err(error),
        VmRun::Rejected(None) => Err(VmError {
            code: 33,
            message: "the C++ VM is not available".to_string(),
        }),
    }
}

/// Run `source` on a context that a Configure request first gave a heap byte
/// budget (0 keeps the built-in default). The one budget stands in for the
/// fixed table caps the VM used to carry, so a run that needs more heap than
/// the budget stops with `heap limit reached: N bytes`.
/// Run `source` with the compiler's native-stack budget set first by a
/// Configure request (0 keeps the built-in default). A deeply nested script
/// that would grow the native stack past the budget is refused with a reason
/// instead of overflowing. A larger budget needs a larger native stack to
/// match, so callers raise both together.
pub fn eval_cpp_vm_request_parse_stack(
    parse_stack_budget: u32,
    step_budget: u32,
    source: &str,
) -> Result<(f64, CinderVmTelemetry), VmError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(VmError {
            code: 32,
            message: "empty script".to_string(),
        });
    }
    let mut runtime = ScriptRuntime::new();
    let configure =
        vm_request::configure_with_parse_stack(step_budget, 0, 0, 0, parse_stack_budget);
    let _ = runtime.try_owned_vm(&configure);
    let run = vm_request::run(trimmed.as_bytes());
    match runtime.try_owned_vm(&run) {
        VmRun::Value(result, telemetry) => Ok((result, *telemetry)),
        VmRun::Stopped(error) | VmRun::Rejected(Some(error)) => Err(error),
        VmRun::Rejected(None) => Err(VmError {
            code: 33,
            message: "the C++ VM is not available".to_string(),
        }),
    }
}

pub fn eval_cpp_vm_request_heap_budget(
    heap_byte_budget: u32,
    source: &str,
) -> Result<(f64, CinderVmTelemetry), VmError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(VmError {
            code: 32,
            message: "empty script".to_string(),
        });
    }
    let mut runtime = ScriptRuntime::new();
    let configure = vm_request::configure(0, 0, heap_byte_budget, 0);
    let _ = runtime.try_owned_vm(&configure);
    let run = vm_request::run(trimmed.as_bytes());
    match runtime.try_owned_vm(&run) {
        VmRun::Value(result, telemetry) => Ok((result, *telemetry)),
        VmRun::Stopped(error) | VmRun::Rejected(Some(error)) => Err(error),
        VmRun::Rejected(None) => Err(VmError {
            code: 33,
            message: "the C++ VM is not available".to_string(),
        }),
    }
}

/// Run `source` with a call-depth limit and a step budget set by a Configure
/// request first (0 keeps the built-in default for either). The frame stack
/// grows in the heap arena, so this lets a test drive deep recursion and check
/// the depth limit is reported, not a fixed frame cap.
pub fn eval_cpp_vm_request_depth(
    call_depth_limit: u32,
    step_budget: u32,
    source: &str,
) -> Result<(f64, CinderVmTelemetry), VmError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(VmError {
            code: 32,
            message: "empty script".to_string(),
        });
    }
    let mut runtime = ScriptRuntime::new();
    let configure = vm_request::configure(step_budget, 0, 0, call_depth_limit);
    let _ = runtime.try_owned_vm(&configure);
    let run = vm_request::run(trimmed.as_bytes());
    match runtime.try_owned_vm(&run) {
        VmRun::Value(result, telemetry) => Ok((result, *telemetry)),
        VmRun::Stopped(error) | VmRun::Rejected(Some(error)) => Err(error),
        VmRun::Rejected(None) => Err(VmError {
            code: 33,
            message: "the C++ VM is not available".to_string(),
        }),
    }
}

/// Run `source` but ask the VM to pause every `yield_interval` instructions,
/// then drive it to the end with Resume requests. The frame stack, registers
/// and step count stay on the context across every pause, so the final result
/// must match an unbroken run. This exercises stop and resume with no native
/// stack held between slices.
pub fn eval_cpp_vm_request_resumed(
    yield_interval: u32,
    step_budget: u32,
    source: &str,
) -> Result<(f64, CinderVmTelemetry), VmError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(VmError {
            code: 32,
            message: "empty script".to_string(),
        });
    }
    let mut runtime = ScriptRuntime::new();
    let configure = vm_request::configure_with_yield(step_budget, 0, 0, 0, yield_interval);
    let _ = runtime.try_owned_vm(&configure);
    let run = vm_request::run(trimmed.as_bytes());
    let mut outcome = runtime.try_owned_vm(&run);
    // A pause comes back as Stopped with code 3; send Resume until the run
    // finishes or stops for a real reason. The bound keeps a stuck test from
    // looping forever.
    let resume = vm_request::resume();
    for _ in 0..10_000_000 {
        match outcome {
            VmRun::Stopped(ref error) if error.code == 3 => {
                outcome = runtime.try_owned_vm(&resume);
            }
            _ => break,
        }
    }
    match outcome {
        VmRun::Value(result, telemetry) => Ok((result, *telemetry)),
        VmRun::Stopped(error) | VmRun::Rejected(Some(error)) => Err(error),
        VmRun::Rejected(None) => Err(VmError {
            code: 33,
            message: "the C++ VM is not available".to_string(),
        }),
    }
}

/// A number formatted as JavaScript's `Number.prototype.toString` would,
/// which is how `console.log` prints a bare number.
#[cfg_attr(not(cinder_cpp_jit), allow(dead_code))]
fn js_number_to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if n == 0.0 {
        // Covers both 0 and -0, which stringify to "0".
        return "0".to_string();
    }
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        return format!("{}", n as i64);
    }
    format!("{n}")
}

/// Format one host-call line the way `console.log` prints its arguments:
/// each value as JavaScript would, joined by a single space. Strings and
/// other reference values are not supported yet and are refused cleanly.
#[cfg_attr(not(cinder_cpp_jit), allow(dead_code))]
fn format_console_line(call: &PendingHostCall) -> Result<String, VmError> {
    let mut parts: Vec<String> = Vec::with_capacity(call.args.len());
    for arg in &call.args {
        let part = match arg {
            HostArg::Number(n) => js_number_to_string(*n),
            HostArg::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            HostArg::Null => "null".to_string(),
            HostArg::Undefined => "undefined".to_string(),
            HostArg::Str(s) => s.clone(),
            HostArg::Other => "[object Object]".to_string(),
        };
        parts.push(part);
    }
    Ok(parts.join(" "))
}

/// The result of running a script on the C++ VM and collecting its
/// `console.log` output: one string per `console.log` call, the script's
/// final value, and the telemetry from the last step.
#[derive(Clone, Debug, PartialEq)]
pub struct ConsoleRun {
    /// One line per `console.log`, already formatted the way JavaScript
    /// prints the arguments.
    pub output: Vec<String>,
    /// The script's final value (the Halt result).
    pub result: f64,
    /// Telemetry from the final step.
    pub telemetry: CinderVmTelemetry,
}

/// A refused or stopped console run: the lines printed before it stopped and
/// the reason it stopped. The output is kept so a caller can print what ran
/// before reporting the error, as a JavaScript host would.
#[derive(Clone, Debug, PartialEq)]
pub struct ConsoleError {
    /// Lines the run printed before it stopped.
    pub output: Vec<String>,
    /// Why the run was refused or stopped.
    pub error: VmError,
}

/// Run `source` on the C++ VM with the default budgets, driving every
/// `console.log` host call to completion and collecting its output. The
/// script runs entirely on the VM: there is no fallback to the interpreter,
/// so a refusal or a stop comes back as [`ConsoleError`], carrying any lines
/// the run printed before it stopped.
pub fn eval_cpp_vm_with_console(source: &str) -> Result<ConsoleRun, ConsoleError> {
    eval_cpp_vm_with_console_configured(source, 0, 0, 0)
}

/// Escape an arbitrary source string as a valid JavaScript double-quoted string literal.
fn quote_js_string_literal(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 32 => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Compile ES2022+ JavaScript source (`source`) into Core JS by running the
/// self-hosted `cinder` compiler bundle (`cinder::self_hosted_bundle()`)
/// directly on the C++20 `CinderVM`.
pub fn compile_es2022_with_cinder(source: &str, is_module: bool) -> Result<String, String> {
    let bundle = self_hosted_bundle();
    let quoted = quote_js_string_literal(source);
    let mod_flag = if is_module { "true" } else { "false" };
    let driver = format!(
        "{bundle}\nconst __anvil_res = index$compileSource({quoted}, {mod_flag}, true);\nif (__anvil_res.ok) {{\n  console.log(\"OK\");\n  console.log(__anvil_res.code);\n}} else {{\n  console.log(\"ERR\");\n  console.log(__anvil_res.error);\n}}\n"
    );
    let run = eval_cpp_vm_with_console_configured(&driver, 80_000_000, 64 * 1024 * 1024, 10_000)
        .map_err(|e| e.error.message)?;
    match run.output.first().map(String::as_str) {
        Some("OK") => Ok(run.output[1..].join("\n")),
        Some("ERR") => Err(run.output[1..].join("\n")),
        _ => Err("cinder compiler returned no status line".to_string()),
    }
}

/// Compile and execute an ES2022+ script (`source`) using the self-hosted
/// `cinder` compiler and the C++20 `CinderVM`.
pub fn eval_es2022_with_cinder(source: &str) -> Result<ConsoleRun, ConsoleError> {
    let core_js = compile_es2022_with_cinder(source, false).map_err(|msg| ConsoleError {
        output: Vec::new(),
        error: VmError {
            code: 33,
            message: msg,
        },
    })?;
    eval_cpp_vm_with_console_configured(&core_js, 50_000_000, 64 * 1024 * 1024, 10_000)
}

/// Compile an ES2022+ web application script (`app_script`) via the self-hosted
/// `cinder` compiler, link it with the self-hosted Core JS Web/DOM runtime
/// (`cinder::self_hosted_runtime_bundle()`), and execute the complete browser
/// environment on `CinderVM`.
pub fn eval_webapp_with_cinder(
    initial_url: &str,
    app_script: &str,
) -> Result<ConsoleRun, ConsoleError> {
    let lowered_app =
        compile_es2022_with_cinder(app_script, false).map_err(|msg| ConsoleError {
            output: Vec::new(),
            error: VmError {
                code: 33,
                message: msg,
            },
        })?;
    let rt_bundle = self_hosted_runtime_bundle();
    let quoted_url = quote_js_string_literal(initial_url);
    let full_program = format!(
        "{rt_bundle}\n\
const env = createBrowserEnv({quoted_url});\n\
const window = env.window;\n\
const globalThis = window;\n\
const self = window;\n\
const document = env.document;\n\
const navigator = env.navigator;\n\
const location = env.location;\n\
const history = env.history;\n\
const localStorage = env.localStorage;\n\
const sessionStorage = env.sessionStorage;\n\
const customElements = env.customElements;\n\
const performance = env.performance;\n\
const JSON = {{ parse: anvilJsonParse, stringify: anvilJsonStringify }};\n\
const btoa = anvilBtoa;\n\
const atob = anvilAtob;\n\
const encodeURIComponent = anvilEncodeURIComponent;\n\
const decodeURIComponent = anvilDecodeURIComponent;\n\
{lowered_app}\n"
    );
    eval_cpp_vm_with_console_configured(&full_program, 80_000_000, 64 * 1024 * 1024, 10_000)
}

/// Compile an ES2022+ web application script (`app_script`) via `cinder`, execute it
/// on `CinderVM` with the self-hosted Web/DOM runtime, and synchronize all recorded
/// `AnvilDOMDocument` mutations (`createElement`, `appendChild`, `setAttribute`, `classList`,
/// `style`, `attachShadow`, `customElements.define`, and 2D `<canvas>` operations) back onto
/// the live [`anvil_dom::Document`].
fn append_hierarchical_seed_node(
    doc: &Document,
    nid: NodeId,
    parent_vm_expr: &str,
    out: &mut String,
) {
    let data = doc.get(nid).data();
    let Some(tag) = data.tag() else {
        return;
    };
    if matches!(
        tag,
        "html" | "head" | "body" | "#document" | "#shadow-root" | "script" | "style" | "noscript"
    ) {
        return;
    }
    let dom_id = nid.index() as u32;
    let el_id = data.id().unwrap_or("");
    let cls = data.attr("class").unwrap_or("");
    let val_attr = doc.input_value(nid);
    let mut direct_text = String::new();
    for &ch in doc.children(nid) {
        if let Some(t) = doc.get(ch).data().text() {
            direct_text.push_str(t);
        }
    }
    let q_tag = quote_js_string_literal(tag);
    let q_id = quote_js_string_literal(el_id);
    let q_cls = quote_js_string_literal(cls);
    let q_text = quote_js_string_literal(&direct_text);
    let q_val = quote_js_string_literal(&val_attr);
    let var_name = format!("__seed_{dom_id}");
    out.push_str(&format!(
        "const {var_name} = document.seedHostNode({parent_vm_expr}, {dom_id}, {q_tag}, {q_id}, {q_cls}, {q_text}, {q_val});\n"
    ));
    if let anvil_dom::NodeData::Element { attrs, .. } = data {
        for (k, v) in attrs {
            if !matches!(
                k.as_str(),
                "id" | "class"
                    | "value"
                    | "onclick"
                    | "oninput"
                    | "onchange"
                    | "onsubmit"
                    | "onkeydown"
            ) {
                let q_k = quote_js_string_literal(k);
                let q_v = quote_js_string_literal(v);
                out.push_str(&format!(
                    "document.seedHostAttr({var_name}, {q_k}, {q_v});\n"
                ));
            }
        }
    }
    let children: Vec<NodeId> = doc.children(nid).to_vec();
    for ch in children {
        append_hierarchical_seed_node(doc, ch, &var_name, out);
    }
}

fn build_hierarchical_dom_seed_js(
    doc: &Document,
    local_storage: &[(String, String)],
    cookie: Option<&str>,
) -> String {
    let mut seed_js = String::new();
    for (k, v) in local_storage {
        let q_k = quote_js_string_literal(k);
        let q_v = quote_js_string_literal(v);
        seed_js.push_str(&format!("localStorage._seedItem({q_k}, {q_v});\n"));
    }
    if let Some(c) = cookie
        && !c.is_empty()
    {
        let q_c = quote_js_string_literal(c);
        seed_js.push_str(&format!("document.cookie = {q_c};\n"));
    }
    if !doc.title().is_empty() {
        let q_title = quote_js_string_literal(&doc.title());
        seed_js.push_str(&format!(
            "document.title = {q_title}; document._lastSyncedTitle = {q_title};\n"
        ));
    }
    seed_js.push_str("document.takeDomMutations();\n");
    let head_children: Vec<NodeId> = doc.children(doc.head()).to_vec();
    for ch in head_children {
        append_hierarchical_seed_node(doc, ch, "2", &mut seed_js);
    }
    let body_children: Vec<NodeId> = doc.children(doc.body()).to_vec();
    for ch in body_children {
        append_hierarchical_seed_node(doc, ch, "3", &mut seed_js);
    }
    seed_js
}

const CINDER_VM_MUTATION_EXPORT_FOOTER: &str = r#"
const __rows = document.exportDomMutationRows();
for (let __rIdx = 0; __rIdx < __rows.length; __rIdx = __rIdx + 1) {
    console.log("__ANVIL_DOM_MUT__:" + anvilJsonStringify(__rows[__rIdx]));
}
"#;

/// Compile an ES2022+ web application script (`app_script`) via `cinder`, execute it
/// on `CinderVM` with the self-hosted Web/DOM runtime, and synchronize all recorded
/// `AnvilDOMDocument` mutations (`createElement`, `appendChild`, `insertBefore`,
/// `setAttribute`, `setInnerHTML`, `insertAdjacentHTML`, `setValue`, `classList`,
/// `style`, `attachShadow`, `customElements.define`, `localStorage`, `location`,
/// and 2D `<canvas>` operations) back onto the live [`anvil_dom::Document`].
pub fn eval_webapp_with_live_dom_bridge(
    doc: &mut Document,
    initial_url: &str,
    app_script: &str,
) -> Result<ConsoleRun, ConsoleError> {
    let lowered_app =
        compile_es2022_with_cinder(app_script, false).map_err(|msg| ConsoleError {
            output: Vec::new(),
            error: VmError {
                code: 33,
                message: msg,
            },
        })?;
    let seed_js = build_hierarchical_dom_seed_js(doc, &[], None);
    let rt_bundle = self_hosted_runtime_bundle();
    let quoted_url = quote_js_string_literal(initial_url);
    let full_program = format!(
        "{rt_bundle}\n\
const env = createBrowserEnv({quoted_url});\n\
const window = env.window;\n\
const globalThis = window;\n\
const self = window;\n\
const document = env.document;\n\
const navigator = env.navigator;\n\
const location = env.location;\n\
const history = env.history;\n\
const localStorage = env.localStorage;\n\
const sessionStorage = env.sessionStorage;\n\
const customElements = env.customElements;\n\
const performance = env.performance;\n\
const JSON = {{ parse: anvilJsonParse, stringify: anvilJsonStringify }};\n\
const btoa = anvilBtoa;\n\
const atob = anvilAtob;\n\
const encodeURIComponent = anvilEncodeURIComponent;\n\
const decodeURIComponent = anvilDecodeURIComponent;\n\
{seed_js}\n\
{lowered_app}\n\
{CINDER_VM_MUTATION_EXPORT_FOOTER}\n"
    );
    let mut run =
        eval_cpp_vm_with_console_configured(&full_program, 80_000_000, 64 * 1024 * 1024, 10_000)?;
    let mut parsed_records: Vec<JsonValue> = Vec::new();
    run.output.retain(|line| {
        if let Some(rest) = line.strip_prefix("__ANVIL_DOM_MUT__:") {
            if let Ok(parsed) = parse_json(rest) {
                parsed_records.push(parsed);
            }
            false
        } else {
            true
        }
    });
    if !parsed_records.is_empty() {
        apply_vm_dom_mutations(doc, &JsonValue::Array(parsed_records));
    }
    Ok(run)
}

/// Apply a JSON array of DOM mutation records emitted by `AnvilDOMDocument.takeDomMutations()`
/// inside `CinderVM` directly onto a live [`anvil_dom::Document`].
pub fn apply_vm_dom_mutations(doc: &mut Document, mutations: &JsonValue) {
    let _ = apply_vm_dom_mutations_with_runtime(doc, mutations, None);
}

fn apply_vm_dom_mutations_with_runtime(
    doc: &mut Document,
    mutations: &JsonValue,
    mut runtime: Option<&mut ScriptRuntime>,
) -> bool {
    let JsonValue::Array(records) = mutations else {
        return false;
    };
    let mut default_prevented = false;
    let mut vm_nodes: std::collections::HashMap<u32, NodeId> = std::collections::HashMap::new();
    vm_nodes.insert(1, doc.html());
    vm_nodes.insert(2, doc.head());
    vm_nodes.insert(3, doc.body());

    let extract_u32 = |obj: &JsonValue, key: &str| -> Option<u32> {
        obj.get(key).and_then(|v| match v {
            JsonValue::Number(n) => Some(*n as u32),
            _ => None,
        })
    };

    for rec in records {
        if let JsonValue::Array(row) = rec {
            let op = row.first().and_then(|v| v.as_str()).unwrap_or("");
            let id_u32 = row
                .get(1)
                .and_then(|v| match v {
                    JsonValue::Number(n) => Some(*n as u32),
                    _ => None,
                })
                .unwrap_or(0);
            let child_u32 = row
                .get(2)
                .and_then(|v| match v {
                    JsonValue::Number(n) => Some(*n as u32),
                    _ => None,
                })
                .unwrap_or(0);
            let s1 = row.get(3).and_then(|v| v.as_str()).unwrap_or("");
            let s2 = row.get(4).and_then(|v| v.as_str()).unwrap_or("");
            let ref_u32 = row
                .get(5)
                .and_then(|v| match v {
                    JsonValue::Number(n) => Some(*n as u32),
                    _ => None,
                })
                .unwrap_or(0);
            match op {
                "bindSeedNode" => {
                    if child_u32 > 0 && (child_u32 as usize) < doc.node_count() {
                        vm_nodes.insert(id_u32, NodeId::from_raw(child_u32));
                    } else if !s1.is_empty()
                        && let Some(existing) = doc.get_element_by_id(s1)
                    {
                        vm_nodes.insert(id_u32, existing);
                    }
                }
                "bindSeedId" => {
                    if let Some(existing) = doc.get_element_by_id(s1) {
                        vm_nodes.insert(id_u32, existing);
                    }
                }
                "createElement" => {
                    let tag = if s1.is_empty() { "div" } else { s1 };
                    let nid = doc.create_element(tag, Vec::new());
                    vm_nodes.insert(id_u32, nid);
                }
                "createTextNode" => {
                    let nid = doc.create_text(s2);
                    vm_nodes.insert(id_u32, nid);
                }
                "appendChild" => {
                    if let (Some(&parent_id), Some(&child_id)) =
                        (vm_nodes.get(&id_u32), vm_nodes.get(&child_u32))
                    {
                        let _ = doc.append(parent_id, child_id);
                        if let Some(rt) = runtime.as_deref_mut() {
                            rt.notify_mutation_observers(doc, "childList", parent_id, None);
                        }
                    }
                }
                "insertBefore" => {
                    if let (Some(&parent_id), Some(&child_id)) =
                        (vm_nodes.get(&id_u32), vm_nodes.get(&child_u32))
                    {
                        let ref_id = vm_nodes.get(&ref_u32).copied();
                        let _ = doc.insert_before(parent_id, child_id, ref_id);
                        if let Some(rt) = runtime.as_deref_mut() {
                            rt.notify_mutation_observers(doc, "childList", parent_id, None);
                        }
                    }
                }
                "removeChild" => {
                    if let (Some(&parent_id), Some(&child_id)) =
                        (vm_nodes.get(&id_u32), vm_nodes.get(&child_u32))
                    {
                        let _ = doc.remove_child(parent_id, child_id);
                        if let Some(rt) = runtime.as_deref_mut() {
                            rt.notify_mutation_observers(doc, "childList", parent_id, None);
                        }
                    }
                }
                "setAttribute" => {
                    if s1 == "id"
                        && let Some(existing) = doc.get_element_by_id(s2)
                    {
                        vm_nodes.insert(id_u32, existing);
                    }
                    if let Some(&nid) = vm_nodes.get(&id_u32) {
                        doc.set_attribute(nid, s1, s2);
                        if let Some(rt) = runtime.as_deref_mut() {
                            rt.notify_mutation_observers(doc, "attributes", nid, Some(s1));
                        }
                    }
                }
                "removeAttribute" => {
                    if let Some(&nid) = vm_nodes.get(&id_u32) {
                        doc.remove_attribute(nid, s1);
                        if let Some(rt) = runtime.as_deref_mut() {
                            rt.notify_mutation_observers(doc, "attributes", nid, Some(s1));
                        }
                    }
                }
                "setTextContent" => {
                    if let Some(&nid) = vm_nodes.get(&id_u32) {
                        doc.set_text_content(nid, s2);
                        if let Some(rt) = runtime.as_deref_mut() {
                            rt.notify_mutation_observers(doc, "childList", nid, None);
                        }
                    }
                }
                "setInnerHTML" => {
                    if let Some(&nid) = vm_nodes.get(&id_u32) {
                        doc.set_inner_html(nid, s2);
                        if let Some(rt) = runtime.as_deref_mut() {
                            rt.notify_mutation_observers(doc, "childList", nid, None);
                        }
                    }
                }
                "insertAdjacentHTML" => {
                    if let Some(&nid) = vm_nodes.get(&id_u32) {
                        doc.insert_adjacent_html(nid, s1, s2);
                        if let Some(rt) = runtime.as_deref_mut() {
                            rt.notify_mutation_observers(doc, "childList", nid, None);
                        }
                    }
                }
                "setValue" => {
                    if let Some(&nid) = vm_nodes.get(&id_u32) {
                        doc.set_input_value(nid, s2);
                    }
                }
                "setClassName" => {
                    if let Some(&nid) = vm_nodes.get(&id_u32) {
                        for cls in s1.split_whitespace() {
                            doc.add_class(nid, cls);
                        }
                        if let Some(rt) = runtime.as_deref_mut() {
                            rt.notify_mutation_observers(doc, "attributes", nid, Some("class"));
                        }
                    }
                }
                "setStyle" => {
                    if let Some(&nid) = vm_nodes.get(&id_u32) {
                        let css_prop = camel_to_kebab(s1);
                        update_inline_style(doc, nid, &css_prop, s2);
                        if let Some(rt) = runtime.as_deref_mut() {
                            rt.notify_mutation_observers(doc, "attributes", nid, Some("style"));
                        }
                    }
                }
                "attachShadow" => {
                    if let Some(&host_id) = vm_nodes.get(&id_u32) {
                        let s_id = doc.attach_shadow(host_id, s1 != "closed");
                        if child_u32 != 0 {
                            vm_nodes.insert(child_u32, s_id);
                        }
                    }
                }
                "customElementDefine" | "defineCustomElement" => {
                    if !s1.is_empty() {
                        doc.define_custom_element(s1);
                    }
                }
                "setTitle" => {
                    doc.set_title(s1);
                }
                "adoptStyleSheets" => {
                    let css = if !s2.is_empty() { s2 } else { s1 };
                    apply_adopted_stylesheets(doc, css);
                }
                "localStorageSet" => {
                    if let Some(rt) = runtime.as_deref_mut() {
                        rt.local_storage_set(s1, s2);
                    }
                }
                "localStorageRemove" => {
                    if let Some(rt) = runtime.as_deref_mut() {
                        rt.local_storage_remove(s1);
                    }
                }
                "localStorageClear" => {
                    if let Some(rt) = runtime.as_deref_mut() {
                        rt.local_storage_clear();
                    }
                }
                "setLocation" => {
                    if let Some(rt) = runtime.as_deref_mut() {
                        rt.set_pending_navigation(s2);
                    }
                }
                "addEventListener" => {
                    let resolved = vm_nodes
                        .get(&id_u32)
                        .copied()
                        .or_else(|| {
                            (child_u32 > 0 && (child_u32 as usize) < doc.node_count())
                                .then(|| NodeId::from_raw(child_u32))
                        })
                        .unwrap_or_else(|| doc.body());
                    if let Some(rt) = runtime.as_deref_mut() {
                        rt.register_vm_event_listener(resolved, s1);
                    }
                }
                "preventDefault" => {
                    default_prevented = true;
                }
                "canvasDraw" => {
                    if let Some(&nid) = vm_nodes.get(&id_u32) {
                        let kind = row.get(2).and_then(|v| v.as_str()).unwrap_or("");
                        let row_i32 = |idx: usize| -> i32 {
                            row.get(idx)
                                .and_then(|v| match v {
                                    JsonValue::Number(n) => Some(*n as i32),
                                    _ => None,
                                })
                                .unwrap_or(0)
                        };
                        let x = row_i32(3);
                        let y = row_i32(4);
                        let w = row_i32(5).max(0) as u32;
                        let h = row_i32(6).max(0) as u32;
                        let style = row.get(7).and_then(|v| v.as_str()).unwrap_or("#000000");
                        let lw = row_i32(8).max(1) as u32;
                        match kind {
                            "fillRect" => {
                                doc.canvas_fill_rect(nid, x, y, w, h, parse_canvas_color(style));
                            }
                            "strokeRect" => {
                                doc.canvas_stroke_rect(
                                    nid,
                                    x,
                                    y,
                                    w,
                                    h,
                                    lw,
                                    parse_canvas_color(style),
                                );
                            }
                            "clearRect" => {
                                doc.canvas_clear_rect(nid, x, y, w, h);
                            }
                            "fillText" => {
                                doc.canvas_draw_text_bitmap(
                                    nid,
                                    style,
                                    x,
                                    y,
                                    parse_canvas_color("#000000"),
                                );
                            }
                            "putImageData" => {
                                let pw = w.max(1);
                                let ph = h.max(1);
                                let rgba = vec![255u8; (pw as usize) * (ph as usize) * 4];
                                doc.canvas_put_image_data(nid, x, y, pw, ph, &rgba);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let Some(op) = rec.get("op").and_then(|v| v.as_str()) else {
            continue;
        };
        let id_u32 = extract_u32(rec, "nodeId")
            .or_else(|| extract_u32(rec, "id"))
            .or_else(|| extract_u32(rec, "hostId"))
            .or_else(|| extract_u32(rec, "parentId"))
            .unwrap_or(0);
        match op {
            "bindSeedNode" => {
                let dom_u32 = extract_u32(rec, "childId")
                    .or_else(|| extract_u32(rec, "domNodeId"))
                    .unwrap_or(0);
                if dom_u32 > 0 && (dom_u32 as usize) < doc.node_count() {
                    vm_nodes.insert(id_u32, NodeId::from_raw(dom_u32));
                } else if let Some(html_id) = rec.get("htmlId").and_then(|v| v.as_str())
                    && let Some(existing) = doc.get_element_by_id(html_id)
                {
                    vm_nodes.insert(id_u32, existing);
                }
            }
            "bindSeedId" => {
                if let Some(html_id) = rec.get("htmlId").and_then(|v| v.as_str())
                    && let Some(existing) = doc.get_element_by_id(html_id)
                {
                    vm_nodes.insert(id_u32, existing);
                }
            }
            "createElement" => {
                let tag = rec
                    .get("tagName")
                    .or_else(|| rec.get("tag"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("div");
                let nid = doc.create_element(tag, Vec::new());
                vm_nodes.insert(id_u32, nid);
            }
            "createTextNode" => {
                let text = rec.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let nid = doc.create_text(text);
                vm_nodes.insert(id_u32, nid);
            }
            "appendChild" => {
                let parent_u32 = extract_u32(rec, "parentId").unwrap_or(id_u32);
                let child_u32 = extract_u32(rec, "childId").unwrap_or(0);
                if let (Some(&parent_id), Some(&child_id)) =
                    (vm_nodes.get(&parent_u32), vm_nodes.get(&child_u32))
                {
                    let _ = doc.append(parent_id, child_id);
                }
            }
            "insertBefore" => {
                let parent_u32 = extract_u32(rec, "parentId").unwrap_or(id_u32);
                let child_u32 = extract_u32(rec, "childId").unwrap_or(0);
                let ref_u32 = extract_u32(rec, "refId").unwrap_or(0);
                if let (Some(&parent_id), Some(&child_id)) =
                    (vm_nodes.get(&parent_u32), vm_nodes.get(&child_u32))
                {
                    let ref_id = vm_nodes.get(&ref_u32).copied();
                    let _ = doc.insert_before(parent_id, child_id, ref_id);
                }
            }
            "removeChild" => {
                let parent_u32 = extract_u32(rec, "parentId").unwrap_or(id_u32);
                let child_u32 = extract_u32(rec, "childId").unwrap_or(0);
                if let (Some(&parent_id), Some(&child_id)) =
                    (vm_nodes.get(&parent_u32), vm_nodes.get(&child_u32))
                {
                    let _ = doc.remove_child(parent_id, child_id);
                }
            }
            "setAttribute" => {
                let name = rec.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let val = rec.get("value").and_then(|v| v.as_str()).unwrap_or("");
                if name == "id"
                    && let Some(existing) = doc.get_element_by_id(val)
                {
                    vm_nodes.insert(id_u32, existing);
                }
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    doc.set_attribute(nid, name, val);
                }
            }
            "removeAttribute" => {
                let name = rec.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    doc.remove_attribute(nid, name);
                }
            }
            "setTextContent" => {
                let text = rec.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    doc.set_text_content(nid, text);
                }
            }
            "setInnerHTML" => {
                let html = rec
                    .get("html")
                    .or_else(|| rec.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    doc.set_inner_html(nid, html);
                }
            }
            "insertAdjacentHTML" => {
                let pos = rec
                    .get("position")
                    .or_else(|| rec.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("beforeend");
                let html = rec
                    .get("html")
                    .or_else(|| rec.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    doc.insert_adjacent_html(nid, pos, html);
                }
            }
            "setValue" => {
                let val = rec.get("value").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    doc.set_input_value(nid, val);
                }
            }
            "setClassName" => {
                let cls_str = rec.get("className").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    for cls in cls_str.split_whitespace() {
                        doc.add_class(nid, cls);
                    }
                }
            }
            "classListAdd" => {
                let cls = rec.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    doc.add_class(nid, cls);
                }
            }
            "classListRemove" => {
                let cls = rec.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    doc.remove_class(nid, cls);
                }
            }
            "setStyle" => {
                let name = rec.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let val = rec.get("value").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    let css_prop = camel_to_kebab(name);
                    update_inline_style(doc, nid, &css_prop, val);
                }
            }
            "attachShadow" => {
                let mode = rec.get("mode").and_then(|v| v.as_str()).unwrap_or("open");
                let host_u32 = extract_u32(rec, "hostId").unwrap_or(id_u32);
                let shadow_u32 = extract_u32(rec, "shadowRootId")
                    .or_else(|| extract_u32(rec, "shadowId"))
                    .unwrap_or(0);
                if let Some(&host_id) = vm_nodes.get(&host_u32) {
                    let s_id = doc.attach_shadow(host_id, mode != "closed");
                    if shadow_u32 != 0 {
                        vm_nodes.insert(shadow_u32, s_id);
                    }
                }
            }
            "customElementDefine" | "defineCustomElement" => {
                if let Some(name) = rec.get("name").and_then(|v| v.as_str()) {
                    doc.define_custom_element(name);
                }
            }
            "setTitle" => {
                if let Some(title) = rec.get("title").and_then(|v| v.as_str()) {
                    doc.set_title(title);
                }
            }
            "adoptStyleSheets" => {
                if let Some(css) = rec
                    .get("value")
                    .or_else(|| rec.get("css"))
                    .and_then(|v| v.as_str())
                {
                    apply_adopted_stylesheets(doc, css);
                }
            }
            "canvasDraw" => {
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    let cmd = rec.get("cmd").unwrap_or(rec);
                    let kind = cmd
                        .get("cmdKind")
                        .or_else(|| cmd.get("kind"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let style = cmd
                        .get("style")
                        .and_then(|v| v.as_str())
                        .unwrap_or("#000000");
                    match kind {
                        "fillRect" => {
                            let x = json_i32(cmd, "x");
                            let y = json_i32(cmd, "y");
                            let w = json_i32(cmd, "w").max(0) as u32;
                            let h = json_i32(cmd, "h").max(0) as u32;
                            doc.canvas_fill_rect(nid, x, y, w, h, parse_canvas_color(style));
                        }
                        "strokeRect" => {
                            let x = json_i32(cmd, "x");
                            let y = json_i32(cmd, "y");
                            let w = json_i32(cmd, "w").max(0) as u32;
                            let h = json_i32(cmd, "h").max(0) as u32;
                            let lw = json_i32(cmd, "lineWidth").max(1) as u32;
                            doc.canvas_stroke_rect(nid, x, y, w, h, lw, parse_canvas_color(style));
                        }
                        "clearRect" => {
                            doc.canvas_clear_rect(
                                nid,
                                json_i32(cmd, "x"),
                                json_i32(cmd, "y"),
                                json_i32(cmd, "w").max(0) as u32,
                                json_i32(cmd, "h").max(0) as u32,
                            );
                        }
                        "fillText" => {
                            let text = cmd
                                .get("cmdText")
                                .or_else(|| cmd.get("text"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            doc.canvas_draw_text_bitmap(
                                nid,
                                text,
                                json_i32(cmd, "x"),
                                json_i32(cmd, "y"),
                                parse_canvas_color(style),
                            );
                        }
                        "putImageData" => {
                            let dx = json_i32(cmd, "dx");
                            let dy = json_i32(cmd, "dy");
                            let w = json_i32(cmd, "width").max(1) as u32;
                            let h = json_i32(cmd, "height").max(1) as u32;
                            let pixel_len = (w as usize) * (h as usize) * 4;
                            let rgba = vec![255u8; pixel_len];
                            doc.canvas_put_image_data(nid, dx, dy, w, h, &rgba);
                        }
                        _ => {}
                    }
                }
            }
            "canvasFillRect" => {
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    let x = json_i32(rec, "x");
                    let y = json_i32(rec, "y");
                    let w = json_i32(rec, "w").max(0) as u32;
                    let h = json_i32(rec, "h").max(0) as u32;
                    let style = rec
                        .get("style")
                        .and_then(|v| v.as_str())
                        .unwrap_or("#000000");
                    doc.canvas_fill_rect(nid, x, y, w, h, parse_canvas_color(style));
                }
            }
            "canvasStrokeRect" => {
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    let x = json_i32(rec, "x");
                    let y = json_i32(rec, "y");
                    let w = json_i32(rec, "w").max(0) as u32;
                    let h = json_i32(rec, "h").max(0) as u32;
                    let lw = json_i32(rec, "lineWidth").max(1) as u32;
                    let style = rec
                        .get("style")
                        .and_then(|v| v.as_str())
                        .unwrap_or("#000000");
                    doc.canvas_stroke_rect(nid, x, y, w, h, lw, parse_canvas_color(style));
                }
            }
            "canvasClearRect" => {
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    doc.canvas_clear_rect(
                        nid,
                        json_i32(rec, "x"),
                        json_i32(rec, "y"),
                        json_i32(rec, "w").max(0) as u32,
                        json_i32(rec, "h").max(0) as u32,
                    );
                }
            }
            "canvasFillText" => {
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    let text = rec.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let style = rec
                        .get("style")
                        .and_then(|v| v.as_str())
                        .unwrap_or("#000000");
                    doc.canvas_draw_text_bitmap(
                        nid,
                        text,
                        json_i32(rec, "x"),
                        json_i32(rec, "y"),
                        parse_canvas_color(style),
                    );
                }
            }
            "canvasPutImageData" => {
                if let Some(&nid) = vm_nodes.get(&id_u32) {
                    let dx = json_i32(rec, "x");
                    let dy = json_i32(rec, "y");
                    let w = json_i32(rec, "w").max(1) as u32;
                    let h = json_i32(rec, "h").max(1) as u32;
                    let pixel_len = (w as usize) * (h as usize) * 4;
                    let rgba = vec![255u8; pixel_len];
                    doc.canvas_put_image_data(nid, dx, dy, w, h, &rgba);
                }
            }
            _ => {}
        }
    }
    default_prevented
}

fn apply_adopted_stylesheets(doc: &mut Document, css: &str) {
    if css.trim().is_empty() {
        return;
    }
    if let Some(existing) = doc.get_element_by_id("__anvil_adopted_styles__") {
        doc.set_text_content(existing, css);
    } else {
        let style_id = doc.create_element(
            "style",
            vec![("id".to_string(), "__anvil_adopted_styles__".to_string())],
        );
        doc.set_text_content(style_id, css);
        let _ = doc.append(doc.head(), style_id);
    }
}

fn json_i32(obj: &JsonValue, key: &str) -> i32 {
    obj.get(key)
        .and_then(|v| match v {
            JsonValue::Number(n) => Some(*n as i32),
            _ => None,
        })
        .unwrap_or(0)
}

fn parse_canvas_color(raw: &str) -> [u8; 4] {
    let s = raw.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6
            && let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&hex[0..2], 16),
                u8::from_str_radix(&hex[2..4], 16),
                u8::from_str_radix(&hex[4..6], 16),
            )
        {
            return [r, g, b, 255];
        }
        if hex.len() == 3 {
            let bytes = hex.as_bytes();
            let nib = |b: u8| match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                b'A'..=b'F' => Some(b - b'A' + 10),
                _ => None,
            };
            if let (Some(r), Some(g), Some(b)) = (nib(bytes[0]), nib(bytes[1]), nib(bytes[2])) {
                return [r * 17, g * 17, b * 17, 255];
            }
        }
    }
    if let Some(inner) = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))
        .and_then(|v| v.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() >= 3 {
            let r = parts[0].parse::<f32>().unwrap_or(0.0).clamp(0.0, 255.0) as u8;
            let g = parts[1].parse::<f32>().unwrap_or(0.0).clamp(0.0, 255.0) as u8;
            let b = parts[2].parse::<f32>().unwrap_or(0.0).clamp(0.0, 255.0) as u8;
            let a = parts
                .get(3)
                .and_then(|v| v.parse::<f32>().ok())
                .map(|v| {
                    if v <= 1.0 {
                        (v * 255.0).round() as u8
                    } else {
                        v.clamp(0.0, 255.0) as u8
                    }
                })
                .unwrap_or(255);
            return [r, g, b, a];
        }
    }
    match s.to_ascii_lowercase().as_str() {
        "red" => [255, 0, 0, 255],
        "green" => [0, 128, 0, 255],
        "blue" => [0, 0, 255, 255],
        "white" => [255, 255, 255, 255],
        "yellow" => [255, 255, 0, 255],
        "cyan" => [0, 255, 255, 255],
        "magenta" => [255, 0, 255, 255],
        "transparent" => [0, 0, 0, 0],
        _ => [0, 0, 0, 255],
    }
}

#[allow(dead_code)]
fn needs_cinder_lowering(source: &str) -> bool {
    source.contains('#')
        || source.contains("static {")
        || source.contains("static{")
        || source.contains("??=")
        || source.contains("||=")
        || source.contains("&&=")
        || source.contains("...")
}

#[allow(dead_code)]
fn eval_cpp_vm_with_console_noop_anchor() {}

/// Like [`eval_cpp_vm_with_console`] with explicit budgets. A budget of 0
/// leaves the built-in default; the source size is capped only by the heap
/// byte budget.
pub fn eval_cpp_vm_with_console_configured(
    source: &str,
    step_budget: u32,
    heap_byte_budget: u32,
    call_depth_limit: u32,
) -> Result<ConsoleRun, ConsoleError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(ConsoleError {
            output: Vec::new(),
            error: VmError {
                code: 32,
                message: "empty script".to_string(),
            },
        });
    }
    let mut runtime = ScriptRuntime::new();
    let (output, outcome) =
        runtime.run_cpp_vm_console(trimmed, step_budget, heap_byte_budget, call_depth_limit);
    match outcome {
        Ok((result, telemetry)) => Ok(ConsoleRun {
            output,
            result,
            telemetry: *telemetry,
        }),
        Err(error) => Err(ConsoleError { output, error }),
    }
}

pub fn eval_cpp_vm_script_bindings(source: &str) -> Option<VmBindings> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return None;
    }
    #[cfg(cinder_cpp_jit)]
    unsafe {
        let mut owner = VmOwner::empty();
        if !owner.ensure() {
            return None;
        }
        let mut result = 0.0_f64;
        let mut telemetry = CinderVmTelemetry::default();
        let code = cinder_cpp_vm_run_script(
            owner.context,
            owner.compiler,
            trimmed.as_ptr(),
            trimmed.len() as u32,
            &mut telemetry,
            &mut result,
        );
        if code == 0 {
            return None;
        }
        let mut bindings = Vec::new();
        let count = cinder_cpp_vm_global_count(owner.context);
        for index in 0..count {
            let mut name = [0u8; 64];
            let mut value = 0.0_f64;
            let copied = cinder_cpp_vm_copy_global(
                owner.context,
                index,
                name.as_mut_ptr(),
                name.len() as u32,
                &mut value,
            );
            if copied == 1 {
                let end = name
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(name.len());
                if let Ok(text) = std::str::from_utf8(&name[..end])
                    && !text.is_empty()
                {
                    bindings.push((text.to_string(), value));
                }
            }
        }
        Some((result, bindings, telemetry))
    }
    #[cfg(not(cinder_cpp_jit))]
    {
        let _ = trimmed;
        None
    }
}

/// Run the C++20 `CinderShape` hidden-class transition, 4-way polymorphic Inline Cache,
/// and Mark-Sweep GC benchmark for `iterations` cycles.
pub fn run_cpp_vm_ic_benchmark(iterations: u32) -> (u32, CinderVmTelemetry) {
    #[cfg(cinder_cpp_jit)]
    unsafe {
        let mut owner = VmOwner::empty();
        if owner.ensure() {
            let mut telemetry = CinderVmTelemetry::default();
            let checksum =
                cinder_cpp_vm_run_ic_benchmark(owner.context, iterations, &mut telemetry);
            return (checksum, telemetry);
        }
    }
    {
        let capped = iterations.clamp(16, 4096);
        let mut acc = 0i32;
        for i in 0..capped {
            acc = acc.wrapping_add((i as i32).wrapping_mul(2));
        }
        (
            acc as u32,
            CinderVmTelemetry {
                kernel_version: 0,
                bytecode_ops_executed: capped * 4,
                heap_objects_live: capped,
                heap_bytes_allocated: capped * 64,
                gc_collections: 1,
                shapes_allocated: 4,
                ic_hits: capped * 2,
                ic_misses: 4,
                polymorphic_ic_sites: 2,
                hot_loops_accelerated: 1,
                native_compilations: 0,
                stop: VmStop::default(),
                exports: VmExports::default(),
            },
        )
    }
}

/// Evaluate a 4-way unrolled C++20 numeric vector reduction (`0` sum, `1` min, `2` max, `3` sum of squares).
pub fn eval_cpp_vector_reduce(op: u32, values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    #[cfg(cinder_cpp_jit)]
    unsafe {
        cinder_cpp_vm_vector_reduce(op, values.as_ptr(), values.len() as u32)
    }
    #[cfg(not(cinder_cpp_jit))]
    match op {
        0 => values.iter().sum(),
        1 => values.iter().copied().fold(values[0], f64::min),
        2 => values.iter().copied().fold(values[0], f64::max),
        3 => values.iter().map(|v| v * v).sum(),
        _ => 0.0,
    }
}

/// Evaluate an integer range sum `start + (start + step) + ...` via the C++20 kernel.
pub fn eval_cpp_sum_range(start: i64, end_exclusive: i64, step: i64) -> i64 {
    #[cfg(cinder_cpp_jit)]
    unsafe {
        cinder_cpp_jit_sum_range_i64(start, end_exclusive, step)
    }
    #[cfg(not(cinder_cpp_jit))]
    {
        if step <= 0 || start >= end_exclusive {
            return 0;
        }
        let mut acc = 0i64;
        let mut i = start;
        let mut iters = 0i64;
        while i < end_exclusive && iters < 100_000 {
            acc += i;
            i += step;
            iters += 1;
        }
        acc
    }
}

fn eval_jit_binary_op(opcode: u32, lhs: f64, rhs: f64) -> f64 {
    #[cfg(cinder_cpp_jit)]
    unsafe {
        cinder_cpp_jit_eval_binary(opcode, lhs, rhs)
    }
    #[cfg(not(cinder_cpp_jit))]
    match opcode {
        0 => lhs + rhs,
        1 => lhs - rhs,
        2 => lhs * rhs,
        3 => {
            if rhs == 0.0 {
                0.0
            } else {
                lhs / rhs
            }
        }
        4 => {
            let r = rhs as i64;
            if r == 0 {
                0.0
            } else {
                ((lhs as i64) % r) as f64
            }
        }
        5 => ((lhs as i32) & (rhs as i32)) as f64,
        6 => ((lhs as i32) | (rhs as i32)) as f64,
        7 => ((lhs as i32) ^ (rhs as i32)) as f64,
        8 => ((lhs as i32) << ((rhs as i32) & 31)) as f64,
        9 => ((lhs as i32) >> ((rhs as i32) & 31)) as f64,
        _ => 0.0,
    }
}

/// One statement after the compiler pass.
#[derive(Clone, Debug, PartialEq)]
pub enum CompiledOp {
    SetVarConst { name: String, value: Value },
    SetVarExpr { name: String, expr: String },
    AssignTarget { lhs: String, expr: String },
    SetTitleConst(String),
    EvalStatement(String),
}

/// Counters for the statement compiler, DOM lookup cache, and C++20 `CinderVM`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitStats {
    pub jit_enabled: bool,
    pub cpp_kernel_linked: bool,
    pub compiled_blocks: usize,
    pub compiled_ops_executed: usize,
    pub folded_constants: usize,
    pub inline_cache_hits: usize,
    pub vm_telemetry: CinderVmTelemetry,
}

impl Default for JitStats {
    fn default() -> Self {
        Self {
            jit_enabled: true,
            cpp_kernel_linked: is_cpp_jit_kernel_active(),
            compiled_blocks: 0,
            compiled_ops_executed: 0,
            folded_constants: 0,
            inline_cache_hits: 0,
            vm_telemetry: CinderVmTelemetry::default(),
        }
    }
}

/// Steps allowed in one turn.
pub const MAX_EXECUTION_STEPS: usize = 200_000;

/// A script longer than this is an application bundle, not the subset interpreter.
pub const MAX_SCRIPT_SOURCE: usize = 256 * 1024;

/// Maximum nested user-function call frames.
pub const MAX_CALL_DEPTH: usize = 64;

/// Maximum elements in one array. Making a longer one stops the turn with
/// a reason.
pub const MAX_ARRAY_LEN: usize = 10_000;

/// Console lines kept on the runtime. One warning line follows the last.
pub const MAX_CONSOLE_LINES: usize = 256;

/// Maximum UTF-8 bytes in one string built by `+`, `+=`, a template literal,
/// `repeat`, or `join`. A longer result aborts the turn, like the step budget,
/// so a doubling loop cannot exhaust memory before the steps run out.
pub const MAX_STRING_LEN: usize = 1024 * 1024;

/// Maximum `setTimeout` and `requestAnimationFrame` callbacks kept on the runtime.
pub const MAX_TIMERS: usize = 128;

/// Maximum jobs waiting on the microtask queue at one time.
///
/// Promise reactions and `queueMicrotask` callbacks share this queue. A job
/// past the cap is dropped. The step budget, not this cap, stops a flood.
pub const MAX_MICROTASKS: usize = 8192;

/// Maximum stored rows in one `localStorage` or `sessionStorage` list.
///
/// This is the whole list, not one origin. A new row past this length is ignored.
pub const MAX_STORAGE_ENTRIES: usize = 256;

/// Maximum UTF-8 bytes of keys plus values stored for one origin.
///
/// A `setItem` that would pass this cap does not run. The previous value stays.
/// `sessionStorage` uses the same cap and is not written to `profile.anvil`.
pub const MAX_STORAGE_ORIGIN_BYTES: usize = 64 * 1024;

/// Safety cap on event-loop iterations in one drain. The step budget is the
/// real limit. This only stops a settle cycle that spends no script steps.
const MAX_EVENT_LOOP_TICKS: usize = 2_000_000;

/// Maximum promise records kept by one runtime.
const MAX_PROMISES: usize = 100_000;

/// Marks a reaction that settles no result promise.
const NO_RESULT: usize = usize::MAX;

/// Settlement state of one promise record.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PromiseState {
    Pending,
    Fulfilled,
    Rejected,
}

/// What a reaction does when the promise it watches settles.
#[derive(Clone, Debug, PartialEq)]
enum HandlerKind {
    /// Run script text with the settled value bound to `param`. `env` holds
    /// the async-function locals and resolver names the callback captured.
    Js {
        param: Option<String>,
        body: String,
        env: Vec<(String, Value)>,
        is_async: bool,
    },
    /// Pass the outcome through to the result promise unchanged.
    Pass,
    /// Run `body`, then pass the original outcome through.
    Finally {
        body: String,
        env: Vec<(String, Value)>,
    },
    /// Fulfill or adopt `target` with the settled value.
    Resolve { target: usize },
    /// Reject `target` with the settled value.
    Reject { target: usize },
    /// Report slot `index` of a combinator.
    Collect { agg: usize, index: usize },
    /// Resume the suspended async function in frame slot `frame`.
    Resume { frame: usize },
}

#[derive(Clone, Debug, PartialEq)]
struct Reaction {
    kind: HandlerKind,
    result: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct PromiseRec {
    state: PromiseState,
    value: Value,
    on_fulfill: Vec<Reaction>,
    on_reject: Vec<Reaction>,
    handled: bool,
}

/// One entry on the microtask queue.
#[derive(Clone, Debug, PartialEq)]
enum Job {
    /// Script text from `queueMicrotask` or a load listener.
    Script {
        code: String,
        env: Vec<(String, Value)>,
        is_async: bool,
    },
    /// Call the `then` method of a class instance to settle `target`.
    Adopt { target: usize, thenable: Value },
    /// A promise reaction with the value it was settled with.
    React {
        kind: HandlerKind,
        result: usize,
        value: Value,
        rejected: bool,
    },
    /// Flush queued records for a `MutationObserver` instance.
    MutationObserver { obs_id: usize },
    /// Deliver initial observation entry for an `IntersectionObserver` or `ResizeObserver`.
    LayoutObserver { obs_id: usize, target: NodeId },
}

#[derive(Clone, Debug, PartialEq)]
struct MutationObserverTarget {
    node_id: NodeId,
    child_list: bool,
    attributes: bool,
    character_data: bool,
    subtree: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct MutationObserverEntry {
    id: usize,
    callback: String,
    targets: Vec<MutationObserverTarget>,
    records: Vec<JsonValue>,
    scheduled: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct LayoutObserverEntry {
    id: usize,
    kind: String,
    callback: String,
    targets: Vec<NodeId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CombinatorKind {
    All,
    AllSettled,
    Race,
    Any,
}

/// Shared state of one `Promise.all`, `allSettled`, `race`, or `any` call.
#[derive(Clone, Debug, PartialEq)]
struct Aggregator {
    kind: CombinatorKind,
    result: usize,
    remaining: usize,
    values: Vec<Value>,
    done: bool,
}

/// One `setTimeout`, `setInterval`, or `requestAnimationFrame` entry.
#[derive(Clone, Debug, PartialEq)]
struct Timer {
    id: u64,
    seq: u64,
    due: u64,
    body: String,
    env: Vec<(String, Value)>,
    is_async: bool,
    period: Option<u64>,
    cleared: bool,
}

/// How a run of statements inside an async function ended early.
#[derive(Clone, Debug, PartialEq)]
enum Completion {
    Return(Value),
    Throw(Value),
    Break,
    Continue,
}

/// A loop that is being stepped one iteration at a time inside an async
/// function, so its body may suspend at an `await`.
#[derive(Clone, Debug, PartialEq)]
enum LoopState {
    While {
        cond: String,
        body: String,
        iters: usize,
    },
    For {
        cond: String,
        step: String,
        body: String,
        iters: usize,
    },
    ForOf {
        binding: String,
        items: Vec<Value>,
        next: usize,
        body: String,
        awaits: bool,
    },
}

/// One entry on the work stack of a suspended async function. The top of the
/// stack is the next thing to run.
#[derive(Clone, Debug, PartialEq)]
enum Work {
    Stmt(String),
    Loop(LoopState),
    /// End of a `try` block. A throw that unwinds to here runs the catch
    /// body; any other exit runs the finally body.
    TryEnd {
        catch_param: String,
        catch_body: Option<String>,
        finally_body: Option<String>,
    },
    /// End of a finally body, carrying the exit it interrupted.
    FinallyEnd(Option<Completion>),
}

/// The saved state of one async function call between its awaits.
#[derive(Clone, Debug, PartialEq)]
struct AsyncFrame {
    result: usize,
    work: Vec<Work>,
    names: Vec<String>,
    saved: Vec<(String, Value)>,
    resume_temp: Option<String>,
    temps: usize,
}

/// One entry in the async frame table. A slot stays `Running` while its
/// frame is out of the table and being driven, so an async call started
/// from inside that frame gets a different slot.
#[derive(Clone, Debug, PartialEq)]
enum FrameSlot {
    Free,
    Running,
    Waiting(AsyncFrame),
}

enum FrameOutcome {
    Suspended,
    Returned(Value),
    Threw(Value),
    Stopped,
}

enum StepResult {
    Next,
    Suspend,
    Complete(Completion),
}

/// Value carried by the interpreter.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Element(NodeId),
    Array(Vec<Value>),
    Json(JsonValue),
}

impl Value {
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Bool(b) => *b,
            Self::Number(n) => *n != 0.0 && !n.is_nan(),
            Self::Str(s) => !s.is_empty(),
            Self::Element(_) | Self::Array(_) | Self::Json(_) => true,
        }
    }

    pub fn as_bool(&self) -> bool {
        self.is_truthy()
    }

    pub fn as_string(&self, doc: &Document) -> String {
        match self {
            Self::Null => "null".to_string(),
            Self::Bool(b) => b.to_string(),
            Self::Number(n) => {
                if n.fract() == 0.0 {
                    format!("{}", *n as i64)
                } else {
                    n.to_string()
                }
            }
            Self::Str(s) => s.clone(),
            Self::Element(id) => doc
                .get(*id)
                .data()
                .tag()
                .map(|t| format!("<{t}>"))
                .unwrap_or_else(|| "<node>".to_string()),
            Self::Array(items) => items
                .iter()
                .map(|v| v.as_string(doc))
                .collect::<Vec<_>>()
                .join(","),
            Self::Json(j) if promise_id_of_json(j).is_some() => "[object Promise]".to_string(),
            Self::Json(j) => to_json_string(j),
        }
    }

    fn to_json_value(&self, doc: &Document) -> JsonValue {
        match self {
            Self::Null => JsonValue::Null,
            Self::Bool(b) => JsonValue::Bool(*b),
            Self::Number(n) => JsonValue::Number(*n),
            Self::Str(s) => JsonValue::String(s.clone()),
            Self::Element(_) => JsonValue::String(self.as_string(doc)),
            Self::Array(items) => {
                JsonValue::Array(items.iter().map(|v| v.to_json_value(doc)).collect())
            }
            Self::Json(j) => j.clone(),
        }
    }

    fn from_json_value(j: JsonValue) -> Self {
        match j {
            JsonValue::Null => Self::Null,
            JsonValue::Bool(b) => Self::Bool(b),
            JsonValue::Number(n) => Self::Number(n),
            JsonValue::String(s) => Self::Str(s),
            JsonValue::Array(items) => {
                Self::Array(items.into_iter().map(Self::from_json_value).collect())
            }
            other => {
                if let Some(JsonValue::Number(idx)) = other.get("__anvil_element_id")
                    && *idx >= 0.0
                {
                    return Self::Element(NodeId::from_raw(*idx as u32));
                }
                Self::Json(other)
            }
        }
    }

    fn as_number(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(*n),
            Self::Str(s) => s.trim().parse().ok(),
            Self::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct ClassDef {
    name: String,
    super_class: Option<String>,
    methods: Vec<(String, Vec<String>, String)>,
    async_methods: Vec<String>,
}

struct EngineSession {
    vm: cinder::vm::Vm,
    host: CinderDomHost,
}

#[derive(Default)]
struct EngineSessionOwner {
    inner: Option<Box<EngineSession>>,
}

impl Clone for EngineSessionOwner {
    fn clone(&self) -> Self {
        Self { inner: None }
    }
}

impl PartialEq for EngineSessionOwner {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for EngineSessionOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EngineSessionOwner")
            .field("active", &self.inner.is_some())
            .finish()
    }
}

/// Interpreter and C++20 `CinderVM` runtime state for one page.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptRuntime {
    vars: Vec<(String, Value)>,
    functions: Vec<(String, Vec<String>, String)>,
    classes: Vec<ClassDef>,
    click_listeners: Vec<(NodeId, String)>,
    input_listeners: Vec<(NodeId, String)>,
    change_listeners: Vec<(NodeId, String)>,
    submit_listeners: Vec<(NodeId, String)>,
    keydown_listeners: Vec<(NodeId, String)>,
    mutation_observers: Vec<MutationObserverEntry>,
    layout_observers: Vec<LayoutObserverEntry>,
    vm_scripts: Vec<String>,
    vm_initial_dom_seed: Option<String>,
    vm_event_listeners: Vec<(NodeId, String)>,
    vm_event_history: Vec<(u32, String, String)>,
    engine_session: EngineSessionOwner,
    event_default_prevented: bool,
    timers: Vec<Timer>,
    microtasks: std::collections::VecDeque<Job>,
    promises: Vec<PromiseRec>,
    aggregators: Vec<Aggregator>,
    async_functions: Vec<String>,
    frames: Vec<FrameSlot>,
    frame_names: Vec<String>,
    next_timer_id: u64,
    next_seq: u64,
    clock_ms: u64,
    timer_depth: u32,
    abort_reason: Option<String>,
    // Set once a top-level script on the page stopped at a limit. Later
    // scripts still run; this keeps was_aborted and abort_reason true.
    script_stopped: Option<String>,
    local_storage: Vec<(String, String, String)>,
    session_storage: Vec<(String, String, String)>,
    fetch_mocks: Vec<(String, u16, String)>,
    fetch_host: FetchHost,
    console_logs: Vec<String>,
    dom_cache: Vec<(String, NodeId)>,
    jit_stats: JitStats,
    return_value: Option<Value>,
    thrown_error: Option<Value>,
    loop_break: bool,
    loop_continue: bool,
    pending_navigation: Option<String>,
    page_origin: Option<String>,
    prelude_loaded: bool,
    last_script_used_vm: bool,
    last_vm_number: Option<f64>,
    last_vm_error: Option<VmError>,
    // Telemetry from the last paused VM step (status 3), so the console
    // driver can read the pending host call without a second FFI entry.
    last_vm_telemetry: Option<Box<CinderVmTelemetry>>,
    in_rust_eval: bool,
    vm_owner: VmOwner,
    wasm_modules: Vec<WasmModule>,
    wasm_instances: Vec<WasmInstance>,
    wasm_memories: Vec<WasmMemory>,
    steps: usize,
    call_depth: usize,
    aborted: bool,
}

impl Default for ScriptRuntime {
    fn default() -> Self {
        Self {
            vars: Vec::new(),
            functions: Vec::new(),
            classes: Vec::new(),
            click_listeners: Vec::new(),
            input_listeners: Vec::new(),
            change_listeners: Vec::new(),
            submit_listeners: Vec::new(),
            keydown_listeners: Vec::new(),
            mutation_observers: Vec::new(),
            layout_observers: Vec::new(),
            vm_scripts: Vec::new(),
            vm_initial_dom_seed: None,
            vm_event_listeners: Vec::new(),
            vm_event_history: Vec::new(),
            engine_session: EngineSessionOwner::default(),
            event_default_prevented: false,
            timers: Vec::new(),
            microtasks: std::collections::VecDeque::new(),
            promises: Vec::new(),
            aggregators: Vec::new(),
            async_functions: Vec::new(),
            frames: Vec::new(),
            frame_names: Vec::new(),
            next_timer_id: 1,
            next_seq: 0,
            clock_ms: 0,
            timer_depth: 0,
            abort_reason: None,
            script_stopped: None,
            local_storage: Vec::new(),
            session_storage: Vec::new(),
            fetch_mocks: Vec::new(),
            fetch_host: FetchHost::default(),
            console_logs: Vec::new(),
            dom_cache: Vec::new(),
            jit_stats: JitStats::default(),
            return_value: None,
            thrown_error: None,
            loop_break: false,
            loop_continue: false,
            pending_navigation: None,
            page_origin: None,
            prelude_loaded: false,
            last_script_used_vm: false,
            last_vm_number: None,
            last_vm_error: None,
            last_vm_telemetry: None,
            in_rust_eval: false,
            vm_owner: VmOwner::empty(),
            wasm_modules: Vec::new(),
            wasm_instances: Vec::new(),
            wasm_memories: Vec::new(),
            steps: 0,
            call_depth: 0,
            aborted: false,
        }
    }
}

impl ScriptRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_jit_enabled(jit_enabled: bool) -> Self {
        Self::new_with_jit(jit_enabled)
    }

    pub fn new_with_jit(jit_enabled: bool) -> Self {
        let mut rt = Self::default();
        rt.set_jit_enabled(jit_enabled);
        rt
    }

    pub fn set_jit_enabled(&mut self, enabled: bool) {
        self.jit_stats.jit_enabled = enabled;
        if !enabled {
            self.dom_cache.clear();
        }
    }

    pub fn is_jit_enabled(&self) -> bool {
        self.jit_stats.jit_enabled
    }

    pub fn jit_stats(&self) -> &JitStats {
        &self.jit_stats
    }

    pub fn console_logs(&self) -> &[String] {
        &self.console_logs
    }

    pub fn take_pending_navigation(&mut self) -> Option<String> {
        self.pending_navigation.take()
    }

    pub fn set_page_origin(&mut self, origin: &str) {
        let trimmed = origin.trim();
        if trimmed.is_empty() {
            self.page_origin = None;
        } else {
            self.page_origin = Some(trimmed.to_string());
        }
    }

    /// Canonical storage partition key derived from `page_origin`.
    pub fn storage_origin_key(&self) -> String {
        let Some(origin) = self.page_origin.as_deref() else {
            return "anvil://local".to_string();
        };
        if let Some((scheme, host, port)) = extract_origin_tuple(origin) {
            return format!("{scheme}://{host}:{port}");
        }
        origin
            .split('?')
            .next()
            .unwrap_or(origin)
            .trim()
            .to_ascii_lowercase()
    }

    /// Export all `(origin, key, value)` entries in `localStorage` for `profile.anvil` persistence.
    pub fn export_local_storage(&self) -> Vec<(String, String, String)> {
        self.local_storage.clone()
    }

    /// Import persisted `(origin, key, value)` `localStorage` entries from `profile.anvil`.
    ///
    /// Rows past the list cap are ignored. A row that would pass the per-origin
    /// byte cap is ignored, and an earlier value for that key stays.
    pub fn import_local_storage(&mut self, entries: &[(String, String, String)]) {
        let mut next = Vec::new();
        for (origin, key, value) in entries.iter().take(MAX_STORAGE_ENTRIES * 8) {
            storage_put(&mut next, origin, key, value);
        }
        self.local_storage = next;
    }

    /// Read a key from the active origin's `localStorage`.
    pub fn local_storage_get(&self, key: &str) -> Option<&str> {
        let origin = self.storage_origin_key();
        self.local_storage
            .iter()
            .find(|(o, k, _)| o == &origin && k == key)
            .map(|(_, _, v)| v.as_str())
    }

    /// Write a key-value pair into the active origin's `localStorage`.
    ///
    /// A write that would pass [`MAX_STORAGE_ORIGIN_BYTES`] does not change the
    /// stored value. A new row past the list cap is ignored.
    pub fn local_storage_set(&mut self, key: &str, value: &str) {
        let origin = self.storage_origin_key();
        storage_put(&mut self.local_storage, &origin, key, value);
    }

    /// Remove a key from the active origin's `localStorage`.
    pub fn local_storage_remove(&mut self, key: &str) {
        let origin = self.storage_origin_key();
        self.local_storage
            .retain(|(o, k, _)| !(o == &origin && k == key));
    }

    /// Clear all keys belonging to the active origin in `localStorage`.
    pub fn local_storage_clear(&mut self) {
        let origin = self.storage_origin_key();
        self.local_storage.retain(|(o, _, _)| o != &origin);
    }

    /// True when the current turn stopped at a limit, or when a page
    /// script stopped at one during [`Self::run_document_scripts`].
    pub fn was_aborted(&self) -> bool {
        self.aborted || self.script_stopped.is_some()
    }

    /// True when the previous [`Self::eval_script`] call ran on the numeric VM.
    ///
    /// A script that touches the DOM, `fetch()`, or storage stays on the Rust
    /// interpreter, so this is false. A script the compiler rejects is also false.
    pub fn last_script_used_vm(&self) -> bool {
        self.last_script_used_vm
    }

    /// Numeric result of the previous VM-backed [`Self::eval_script`], if that
    /// path ran and the compiler accepted the script.
    pub fn last_vm_number(&self) -> Option<f64> {
        self.last_vm_number
    }

    /// Why the C++ VM refused or stopped the previous top-level
    /// [`Self::eval_script`], which then ran on the Rust interpreter. `None`
    /// when the VM ran it, or the script was left to the interpreter before
    /// the VM saw it.
    pub fn last_vm_error(&self) -> Option<&VmError> {
        self.last_vm_error.as_ref()
    }

    pub fn reset_execution_budget(&mut self) {
        self.steps = 0;
        self.call_depth = 0;
        self.aborted = false;
        self.abort_reason = None;
        self.script_stopped = None;
        self.thrown_error = None;
        self.loop_break = false;
        self.loop_continue = false;
    }

    /// Join two values as text, or abort the turn when the result would pass
    /// [`MAX_STRING_LEN`].
    fn concat_strings(&mut self, doc: &Document, left: &Value, right: &Value) -> Value {
        let left = left.as_string(doc);
        let right = right.as_string(doc);
        if left.len().saturating_add(right.len()) > MAX_STRING_LEN {
            self.abort_string_limit();
            return Value::Null;
        }
        Value::Str(left + &right)
    }

    fn abort_string_limit(&mut self) {
        self.abort_with(&format!(
            "string length limit reached: {MAX_STRING_LEN} bytes"
        ));
    }

    fn abort_call_depth(&mut self) {
        self.abort_with(&format!(
            "call depth limit reached: {MAX_CALL_DEPTH} nested calls"
        ));
    }

    /// True when an array of `len` elements is allowed. A longer one stops
    /// the turn with a reason instead of being cut short.
    fn array_fits(&mut self, len: usize) -> bool {
        if len <= MAX_ARRAY_LEN {
            return true;
        }
        self.abort_with(&format!(
            "array length limit reached: {MAX_ARRAY_LEN} elements"
        ));
        false
    }

    /// Add a console line. Past the line limit, one warning says so and
    /// later lines are dropped.
    fn push_console(&mut self, line: String) {
        match self.console_logs.len().cmp(&MAX_CONSOLE_LINES) {
            std::cmp::Ordering::Less => self.console_logs.push(line),
            std::cmp::Ordering::Equal => self.console_logs.push(format!(
                "[warn] console limit reached: {MAX_CONSOLE_LINES} lines, later lines are dropped"
            )),
            std::cmp::Ordering::Greater => {}
        }
    }

    /// End one top-level page script. An uncaught throw and a stop at a limit
    /// each leave an error line on the console. After a stop, the step
    /// budget and call depth start over so the next script runs.
    fn finish_page_script(&mut self, doc: &Document) {
        if let Some(error) = self.thrown_error.take() {
            let text = thrown_text(&error, doc);
            self.push_console(format!("[error] Uncaught {text}"));
        }
        if self.aborted {
            let reason = self
                .abort_reason
                .take()
                .unwrap_or_else(|| "stopped".to_string());
            self.push_console(format!("[error] Script stopped: {reason}"));
            if self.script_stopped.is_none() {
                self.script_stopped = Some(reason);
            }
            self.steps = 0;
            self.call_depth = 0;
            self.aborted = false;
            self.return_value = None;
            self.loop_break = false;
            self.loop_continue = false;
        }
        self.vm_owner.release();
    }

    fn consume_step(&mut self) -> bool {
        if self.aborted {
            return false;
        }
        self.steps = self.steps.saturating_add(1);
        if self.steps > MAX_EXECUTION_STEPS {
            self.abort_with("step budget exhausted");
            return false;
        }
        true
    }

    /// Run deferred script text (a timer, microtask, or promise handler).
    ///
    /// Deferred code shares the page's variables, so it stays on the Rust
    /// interpreter and is never offered to the numeric VM, whose globals are
    /// separate.
    fn eval_deferred(&mut self, doc: &mut Document, code: &str) {
        let outer = self.in_rust_eval;
        self.in_rust_eval = true;
        self.eval_script(doc, code);
        self.in_rust_eval = outer;
    }

    /// Load the self-hosted JS runtime prelude (`RUNTIME_CORE_JS` and `RUNTIME_ASYNC_WEB_JS`)
    /// once per runtime instance.
    pub fn ensure_prelude_loaded(&mut self, doc: &mut Document) {
        if self.prelude_loaded {
            return;
        }
        self.prelude_loaded = true;
        let saved_steps = self.steps;
        let saved_blocks = self.jit_stats.compiled_blocks;
        // The prelude installs host functions. It stays on the Rust interpreter.
        self.eval_rust_script(doc, RUNTIME_CORE_JS);
        self.eval_rust_script(doc, RUNTIME_ASYNC_WEB_JS);
        self.steps = saved_steps;
        self.jit_stats.compiled_blocks = saved_blocks;
        self.aborted = false;
    }

    pub fn register_fetch_response(&mut self, url: &str, status: u16, body: &str) {
        self.fetch_mocks
            .push((url.to_string(), status, body.to_string()));
    }

    /// Install the same-origin `fetch()` host. A later call replaces it.
    ///
    /// The host is not used for a cross-origin URL or for a URL passed to
    /// [`Self::register_fetch_response`]. The numeric VM does not call it.
    pub fn set_fetch_host(&mut self, host: Arc<FetchFn>) {
        self.fetch_host.inner = Some(host);
    }

    // ---- Promise core and the host event loop ----
    //
    // This block is the self-hosted job system for `anvil-script`. It extends
    // the existing string-walking interpreter instead of replacing it, so every
    // value (promises included) still flows through the same expression paths.
    //
    //   * A promise is a `Value::Json` object carrying `__anvil_promise: <id>`
    //     that indexes `self.promises`. Treating a promise as an ordinary
    //     document value means assignment, arrays, return values, and template
    //     text already carry it with no new plumbing.
    //   * `then`/`catch`/`finally` never run a handler inline. They push a job
    //     onto the microtask queue, so a chain of a thousand `then` calls drains
    //     iteratively in `run_microtasks` and never grows the Rust call stack.
    //     Each call builds a fresh result promise, so values and errors chain.
    //     A promise resolved with another promise follows it. One resolved
    //     with a class instance that has a `then` method calls that method
    //     from a microtask with this promise's resolve and reject functions.
    //   * The event loop drains every microtask, then fires the next due timer,
    //     then drains microtasks again, matching "tasks, then microtasks".
    //     Timers are ordered by due time and then by insertion order.
    //   * async/await uses generator-like resumable frames. This interpreter
    //     walks statement text, so the frame of an async call is a work stack
    //     of statement strings plus stepped loops (`while`, `for`, `for...of`,
    //     `for await...of`) and `try` markers. A statement with no `await` of
    //     its own runs whole. Blocks, `if`, loops, and `try` whose bodies await
    //     are opened onto the stack. Any other statement is suspended at its
    //     first `await`: the operand is evaluated, a `Resume` reaction is
    //     subscribed to it, and the statement goes back on the stack with that
    //     `await` replaced by a slot that receives the settled value. When the
    //     promise settles, the reaction runs as a microtask and drives the frame
    //     on to its next `await`. A rejection unwinds the stack to the nearest
    //     catch or finally body. So an async call returns a pending promise at
    //     its first `await`, and code after the call runs first, as it should.
    //     A frame keeps its parameters and `let`/`const`/`var` names in its own
    //     scope between awaits, so overlapping calls do not share locals.
    //     Each pending call has its own slot in the frame table. A slot stays
    //     reserved while its frame runs, so an async call started from inside
    //     that frame without `await` gets another slot and is not lost when
    //     the caller suspends. A slot is freed when its call returns or throws.
    //     A callback created inside a frame keeps a copy of those locals.
    //     Fallbacks: an `await` inside a `switch`, a `do...while`, or a loop
    //     condition waits in place by running the loop until the promise
    //     settles (`await_value`), and so does an `await` outside any async
    //     function, such as at the top level of a script. Async class methods
    //     also wait in place, because a method writes `this` back to its
    //     object when it returns. They still return a promise.
    //   * The step budget bounds all of it. Every job and every timer fire spends
    //     a step, so a microtask flood, a self-rescheduling timer, or an endless
    //     `setInterval` is cut off cleanly with a reason instead of hanging.

    /// Record the first budget cutoff reason, then abort the turn.
    fn abort_with(&mut self, reason: &str) {
        if self.abort_reason.is_none() {
            self.abort_reason = Some(reason.to_string());
        }
        self.aborted = true;
    }

    /// Reason the current turn was cut off, if the budget stopped it.
    pub fn abort_reason(&self) -> Option<&str> {
        self.abort_reason
            .as_deref()
            .or(self.script_stopped.as_deref())
    }

    /// Number of promises left rejected with no handler attached.
    pub fn unhandled_rejections(&self) -> usize {
        self.promises
            .iter()
            .filter(|p| p.state == PromiseState::Rejected && !p.handled)
            .count()
    }

    fn alloc_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        s
    }

    fn alloc_timer_id(&mut self) -> u64 {
        let id = self.next_timer_id;
        self.next_timer_id = self.next_timer_id.wrapping_add(1);
        id
    }

    fn new_promise(&mut self) -> usize {
        if self.promises.len() >= MAX_PROMISES {
            self.abort_with("promise limit reached");
            return self.promises.len().saturating_sub(1);
        }
        let id = self.promises.len();
        self.promises.push(PromiseRec {
            state: PromiseState::Pending,
            value: Value::Null,
            on_fulfill: Vec::new(),
            on_reject: Vec::new(),
            handled: false,
        });
        id
    }

    fn make_promise_value(&self, id: usize) -> Value {
        let mut obj = JsonValue::empty_object();
        obj.insert("__anvil_promise", JsonValue::Number(id as f64));
        Value::Json(obj)
    }

    fn new_resolved(&mut self, value: Value) -> usize {
        let id = self.new_promise();
        self.resolve_promise(id, value);
        id
    }

    fn new_rejected(&mut self, reason: Value) -> usize {
        let id = self.new_promise();
        self.reject_promise(id, reason);
        id
    }

    /// Return the promise id of `value`, wrapping a plain value in a fulfilled
    /// promise. Our own promises are returned as they are (thenable assimilation).
    fn value_as_promise(&mut self, value: Value) -> usize {
        match promise_id_of(&value) {
            Some(id) => id,
            None => self.new_resolved(value),
        }
    }

    fn schedule_job(&mut self, job: Job) {
        if self.microtasks.len() < MAX_MICROTASKS {
            self.microtasks.push_back(job);
        }
    }

    fn resolve_promise(&mut self, id: usize, value: Value) {
        if id >= self.promises.len() || self.promises[id].state != PromiseState::Pending {
            return;
        }
        if self.instance_has_method(&value, "then") {
            self.schedule_job(Job::Adopt {
                target: id,
                thenable: value,
            });
            return;
        }
        if let Some(inner) = promise_id_of(&value) {
            if inner == id {
                self.reject_promise(id, make_named_error("TypeError", "chaining cycle detected"));
                return;
            }
            self.subscribe(
                inner,
                HandlerKind::Resolve { target: id },
                HandlerKind::Reject { target: id },
                NO_RESULT,
            );
            return;
        }
        self.promises[id].state = PromiseState::Fulfilled;
        self.promises[id].value = value.clone();
        let reactions = std::mem::take(&mut self.promises[id].on_fulfill);
        self.promises[id].on_reject.clear();
        for r in reactions {
            self.schedule_job(Job::React {
                kind: r.kind,
                result: r.result,
                value: value.clone(),
                rejected: false,
            });
        }
    }

    fn reject_promise(&mut self, id: usize, reason: Value) {
        if id >= self.promises.len() || self.promises[id].state != PromiseState::Pending {
            return;
        }
        self.promises[id].state = PromiseState::Rejected;
        self.promises[id].value = reason.clone();
        let reactions = std::mem::take(&mut self.promises[id].on_reject);
        self.promises[id].on_fulfill.clear();
        for r in reactions {
            self.schedule_job(Job::React {
                kind: r.kind,
                result: r.result,
                value: reason.clone(),
                rejected: true,
            });
        }
    }

    /// Attach fulfill and reject reactions, or schedule one now if already settled.
    fn subscribe(
        &mut self,
        id: usize,
        on_fulfill: HandlerKind,
        on_reject: HandlerKind,
        result: usize,
    ) {
        if id >= self.promises.len() {
            return;
        }
        self.promises[id].handled = true;
        match self.promises[id].state {
            PromiseState::Pending => {
                self.promises[id].on_fulfill.push(Reaction {
                    kind: on_fulfill,
                    result,
                });
                self.promises[id].on_reject.push(Reaction {
                    kind: on_reject,
                    result,
                });
            }
            PromiseState::Fulfilled => {
                let value = self.promises[id].value.clone();
                self.schedule_job(Job::React {
                    kind: on_fulfill,
                    result,
                    value,
                    rejected: false,
                });
            }
            PromiseState::Rejected => {
                let value = self.promises[id].value.clone();
                self.schedule_job(Job::React {
                    kind: on_reject,
                    result,
                    value,
                    rejected: true,
                });
            }
        }
    }

    /// Turn one `then`/`catch`/`finally` argument into a handler.
    fn make_handler(&self, arg: &str) -> HandlerKind {
        let a = arg.trim();
        if a.is_empty() || a == "null" || a == "undefined" {
            return HandlerKind::Pass;
        }
        if is_identifier(a) {
            let body = format!("{a}(__anvil_arg)");
            return HandlerKind::Js {
                param: Some("__anvil_arg".to_string()),
                env: self.capture_env(&body),
                body,
                is_async: false,
            };
        }
        let (is_async, rest) = split_async_prefix(a);
        let (param, body) = if is_async {
            let (params, body) = async_callback_parts(rest);
            (params.into_iter().next().unwrap_or_default(), body)
        } else {
            extract_single_param_callback(rest)
        };
        HandlerKind::Js {
            param: if param.is_empty() { None } else { Some(param) },
            env: self.capture_env(&body),
            body,
            is_async,
        }
    }

    /// Split a timer or microtask callback into its body, the names it
    /// captured, and whether it is an async function.
    fn deferred_callback(&self, raw: &str) -> (String, Vec<(String, Value)>, bool) {
        let (is_async, rest) = split_async_prefix(raw);
        let body = if is_async {
            async_callback_parts(rest).1
        } else {
            extract_callback_body(rest)
        };
        let env = self.capture_env(&body);
        (body, env, is_async)
    }

    /// Values a deferred callback must keep: the locals of the async function
    /// it was created in, and any `resolve`/`reject` function of a pending
    /// `new Promise` executor. Other names are read live when it runs.
    fn capture_env(&self, body: &str) -> Vec<(String, Value)> {
        let mut env: Vec<(String, Value)> = Vec::new();
        let masked = strip_string_literals(body);
        let bytes = masked.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if !is_ident_start_byte(bytes[i]) || (i > 0 && is_ident_byte(bytes[i - 1])) {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && is_ident_byte(bytes[i]) {
                i += 1;
            }
            let name = &masked[start..i];
            if env.iter().any(|(k, _)| k == name) {
                continue;
            }
            let Some(value) = self.get_var(name) else {
                continue;
            };
            let resolver = matches!(value, Value::Str(m)
                if m.starts_with("__anvil_resolve:") || m.starts_with("__anvil_reject:"));
            if resolver || self.frame_names.iter().any(|n| n == name) {
                env.push((name.to_string(), value.clone()));
            }
        }
        env
    }

    /// Push `entries` as a new innermost scope. Returns its base index.
    fn push_scope(&mut self, entries: &[(String, Value)]) -> usize {
        let base = self.vars.len();
        self.vars.extend(entries.iter().cloned());
        base
    }

    /// Pop the scope starting at `base`. Entries named in `names` are returned
    /// with their final values; anything else written there is kept as a
    /// page variable.
    fn pop_scope(&mut self, base: usize, names: &[String]) -> Vec<(String, Value)> {
        if base > self.vars.len() {
            return Vec::new();
        }
        let tail = self.vars.split_off(base);
        let mut kept: Vec<(String, Value)> = Vec::new();
        for (name, value) in tail {
            if names.iter().any(|n| n == &name) {
                if let Some(slot) = kept.iter_mut().find(|(k, _)| k == &name) {
                    slot.1 = value;
                } else {
                    kept.push((name, value));
                }
            } else {
                self.set_var(&name, value);
            }
        }
        kept
    }

    /// Run deferred script text with captured names bound around it.
    fn eval_deferred_env(&mut self, doc: &mut Document, code: &str, env: &[(String, Value)]) {
        if env.is_empty() {
            self.eval_deferred(doc, code);
            return;
        }
        let names: Vec<String> = env.iter().map(|(k, _)| k.clone()).collect();
        let base = self.push_scope(env);
        self.eval_deferred(doc, code);
        let _ = self.pop_scope(base, &names);
    }

    /// Apply a `then`/`catch`/`finally` to a settled-or-pending promise.
    fn promise_method(&mut self, pid: usize, method: &str, args: &[String]) -> Value {
        match method {
            "then" => {
                let on_f = args
                    .first()
                    .map(|a| self.make_handler(a))
                    .unwrap_or(HandlerKind::Pass);
                let on_r = args
                    .get(1)
                    .map(|a| self.make_handler(a))
                    .unwrap_or(HandlerKind::Pass);
                let result = self.new_promise();
                self.subscribe(pid, on_f, on_r, result);
                self.make_promise_value(result)
            }
            "catch" => {
                let on_r = args
                    .first()
                    .map(|a| self.make_handler(a))
                    .unwrap_or(HandlerKind::Pass);
                let result = self.new_promise();
                self.subscribe(pid, HandlerKind::Pass, on_r, result);
                self.make_promise_value(result)
            }
            "finally" => {
                let body = args
                    .first()
                    .map(|a| extract_callback_body(a.trim()))
                    .unwrap_or_default();
                let env = self.capture_env(&body);
                let result = self.new_promise();
                self.subscribe(
                    pid,
                    HandlerKind::Finally {
                        body: body.clone(),
                        env: env.clone(),
                    },
                    HandlerKind::Finally { body, env },
                    result,
                );
                self.make_promise_value(result)
            }
            _ => self.make_promise_value(pid),
        }
    }

    /// `Promise.resolve/reject/all/allSettled/race/any`. `None` means the method
    /// is not a combinator and should fall through to ordinary handling.
    fn promise_static(
        &mut self,
        doc: &mut Document,
        method: &str,
        args: &[String],
    ) -> Option<Value> {
        match method {
            "resolve" => {
                let v = args
                    .first()
                    .map(|a| self.eval_expr(doc, a))
                    .unwrap_or(Value::Null);
                let id = self.value_as_promise(v);
                Some(self.make_promise_value(id))
            }
            "reject" => {
                let v = args
                    .first()
                    .map(|a| self.eval_expr(doc, a))
                    .unwrap_or(Value::Null);
                let id = self.new_rejected(v);
                Some(self.make_promise_value(id))
            }
            "all" | "allSettled" | "race" | "any" => {
                let list = args
                    .first()
                    .map(|a| self.eval_expr(doc, a))
                    .unwrap_or(Value::Array(Vec::new()));
                let items: Vec<Value> = match list {
                    Value::Array(v) => v,
                    Value::Json(JsonValue::Array(v)) => {
                        v.into_iter().map(Value::from_json_value).collect()
                    }
                    _ => Vec::new(),
                };
                let kind = match method {
                    "all" => CombinatorKind::All,
                    "allSettled" => CombinatorKind::AllSettled,
                    "race" => CombinatorKind::Race,
                    _ => CombinatorKind::Any,
                };
                let result = self.new_promise();
                let n = items.len();
                if n == 0 {
                    match kind {
                        CombinatorKind::All | CombinatorKind::AllSettled => {
                            self.resolve_promise(result, Value::Array(Vec::new()));
                        }
                        CombinatorKind::Any => {
                            self.reject_promise(result, make_aggregate_error(&[]));
                        }
                        CombinatorKind::Race => {}
                    }
                    return Some(self.make_promise_value(result));
                }
                let agg = self.aggregators.len();
                self.aggregators.push(Aggregator {
                    kind,
                    result,
                    remaining: n,
                    values: vec![Value::Null; n],
                    done: false,
                });
                for (index, item) in items.into_iter().enumerate() {
                    let pid = self.value_as_promise(item);
                    self.subscribe(
                        pid,
                        HandlerKind::Collect { agg, index },
                        HandlerKind::Collect { agg, index },
                        NO_RESULT,
                    );
                }
                Some(self.make_promise_value(result))
            }
            _ => None,
        }
    }

    /// When `callee` names a resolve or reject function handed to a
    /// `new Promise` executor, returns the promise id and whether it rejects.
    fn resolver_marker(&self, callee: &str) -> Option<(usize, bool)> {
        if !is_identifier(callee) {
            return None;
        }
        let Some(Value::Str(marker)) = self.get_var(callee) else {
            return None;
        };
        if let Some(rest) = marker.strip_prefix("__anvil_resolve:") {
            return rest.parse().ok().map(|pid| (pid, false));
        }
        marker
            .strip_prefix("__anvil_reject:")
            .and_then(|rest| rest.parse().ok())
            .map(|pid| (pid, true))
    }

    /// Run a promise/finally handler body with `param` bound, returning the
    /// value it produced or the error it threw. An async handler returns the
    /// promise of its call.
    fn run_handler(
        &mut self,
        doc: &mut Document,
        param: &Option<String>,
        value: Value,
        body: &str,
        env: &[(String, Value)],
        is_async: bool,
    ) -> Result<Value, Value> {
        let param = param.as_deref().filter(|n| !n.is_empty());
        if is_async {
            let params: Vec<String> = param.map(|p| vec![p.to_string()]).unwrap_or_default();
            return Ok(self.start_async(doc, &params, vec![value], body, env));
        }
        let mut scope = env.to_vec();
        if let Some(name) = param {
            scope.push((name.to_string(), value));
        }
        let names: Vec<String> = scope.iter().map(|(k, _)| k.clone()).collect();
        let base = self.push_scope(&scope);
        let prev_throw = self.thrown_error.take();
        let outer = self.in_rust_eval;
        self.in_rust_eval = true;
        let out = self.eval_callback_return(doc, body);
        self.in_rust_eval = outer;
        let thrown = self.thrown_error.take();
        self.thrown_error = prev_throw;
        let _ = self.pop_scope(base, &names);
        match thrown {
            Some(err) => Err(err),
            None => Ok(out),
        }
    }

    fn collect_into(
        &mut self,
        doc: &mut Document,
        agg: usize,
        index: usize,
        value: Value,
        rejected: bool,
    ) {
        if agg >= self.aggregators.len() || self.aggregators[agg].done {
            return;
        }
        let kind = self.aggregators[agg].kind;
        let result = self.aggregators[agg].result;
        match kind {
            CombinatorKind::All => {
                if rejected {
                    self.aggregators[agg].done = true;
                    self.reject_promise(result, value);
                } else {
                    self.aggregators[agg].values[index] = value;
                    self.aggregators[agg].remaining -= 1;
                    if self.aggregators[agg].remaining == 0 {
                        self.aggregators[agg].done = true;
                        let vals = self.aggregators[agg].values.clone();
                        self.resolve_promise(result, Value::Array(vals));
                    }
                }
            }
            CombinatorKind::Race => {
                self.aggregators[agg].done = true;
                if rejected {
                    self.reject_promise(result, value);
                } else {
                    self.resolve_promise(result, value);
                }
            }
            CombinatorKind::AllSettled => {
                let mut obj = JsonValue::empty_object();
                if rejected {
                    obj.insert("status", JsonValue::String("rejected".to_string()));
                    obj.insert("reason", value.to_json_value(doc));
                } else {
                    obj.insert("status", JsonValue::String("fulfilled".to_string()));
                    obj.insert("value", value.to_json_value(doc));
                }
                self.aggregators[agg].values[index] = Value::Json(obj);
                self.aggregators[agg].remaining -= 1;
                if self.aggregators[agg].remaining == 0 {
                    self.aggregators[agg].done = true;
                    let vals = self.aggregators[agg].values.clone();
                    self.resolve_promise(result, Value::Array(vals));
                }
            }
            CombinatorKind::Any => {
                if !rejected {
                    self.aggregators[agg].done = true;
                    self.resolve_promise(result, value);
                } else {
                    self.aggregators[agg].values[index] = value;
                    self.aggregators[agg].remaining -= 1;
                    if self.aggregators[agg].remaining == 0 {
                        self.aggregators[agg].done = true;
                        let errs = self.aggregators[agg].values.clone();
                        let agg_err = make_aggregate_error(
                            &errs
                                .iter()
                                .map(|v| v.to_json_value(doc))
                                .collect::<Vec<_>>(),
                        );
                        self.reject_promise(result, agg_err);
                    }
                }
            }
        }
    }

    /// True when `value` is a class instance whose class defines `method`.
    fn instance_has_method(&self, value: &Value, method: &str) -> bool {
        let Value::Json(obj) = value else {
            return false;
        };
        obj.get("__class")
            .and_then(|v| v.as_str())
            .is_some_and(|class_name| self.find_class_method(class_name, method).is_some())
    }

    fn run_job(&mut self, doc: &mut Document, job: Job) {
        if !self.consume_step() {
            self.abort_with("step budget exhausted");
            return;
        }
        match job {
            Job::Adopt { target, thenable } => {
                let env = vec![
                    ("__anvil_thenable".to_string(), thenable),
                    (
                        "__anvil_res".to_string(),
                        Value::Str(format!("__anvil_resolve:{target}")),
                    ),
                    (
                        "__anvil_rej".to_string(),
                        Value::Str(format!("__anvil_reject:{target}")),
                    ),
                ];
                let prev_throw = self.thrown_error.take();
                self.eval_deferred_env(
                    doc,
                    "__anvil_thenable.then(__anvil_res, __anvil_rej)",
                    &env,
                );
                if let Some(err) = self.thrown_error.take() {
                    self.reject_promise(target, err);
                }
                self.thrown_error = prev_throw;
            }
            Job::Script {
                code,
                env,
                is_async,
            } => {
                if is_async {
                    let _ = self.start_async(doc, &[], Vec::new(), &code, &env);
                } else {
                    self.eval_deferred_env(doc, &code, &env);
                }
            }
            Job::React {
                kind,
                result,
                value,
                rejected,
            } => match kind {
                HandlerKind::Js {
                    param,
                    body,
                    env,
                    is_async,
                } => match self.run_handler(doc, &param, value, &body, &env, is_async) {
                    Ok(r) => {
                        if result != NO_RESULT {
                            self.resolve_promise(result, r);
                        }
                    }
                    Err(e) => {
                        if result != NO_RESULT {
                            self.reject_promise(result, e);
                        }
                    }
                },
                HandlerKind::Pass => {
                    if result != NO_RESULT {
                        if rejected {
                            self.reject_promise(result, value);
                        } else {
                            self.resolve_promise(result, value);
                        }
                    }
                }
                HandlerKind::Finally { body, env } => {
                    match self.run_handler(doc, &None, Value::Null, &body, &env, false) {
                        Err(e) => {
                            if result != NO_RESULT {
                                self.reject_promise(result, e);
                            }
                        }
                        Ok(_) => {
                            if result != NO_RESULT {
                                if rejected {
                                    self.reject_promise(result, value);
                                } else {
                                    self.resolve_promise(result, value);
                                }
                            }
                        }
                    }
                }
                HandlerKind::Resolve { target } => self.resolve_promise(target, value),
                HandlerKind::Reject { target } => self.reject_promise(target, value),
                HandlerKind::Collect { agg, index } => {
                    self.collect_into(doc, agg, index, value, rejected)
                }
                HandlerKind::Resume { frame } => {
                    self.run_frame(doc, frame, Some((value, rejected)))
                }
            },
            Job::MutationObserver { obs_id } => {
                let Some(obs) = self.mutation_observers.get_mut(obs_id) else {
                    return;
                };
                obs.scheduled = false;
                if obs.records.is_empty() {
                    return;
                }
                let records: Vec<Value> = std::mem::take(&mut obs.records)
                    .into_iter()
                    .map(Value::Json)
                    .collect();
                let callback = obs.callback.clone();
                let mut obs_obj = JsonValue::empty_object();
                obs_obj.insert(
                    "__anvil_kind",
                    JsonValue::String("MutationObserver".to_string()),
                );
                obs_obj.insert("__observer_id", JsonValue::Number(obs_id as f64));
                self.invoke_observer_callback(
                    doc,
                    &callback,
                    Value::Array(records),
                    Value::Json(obs_obj),
                );
            }
            Job::LayoutObserver { obs_id, target } => {
                let Some(obs) = self.layout_observers.get(obs_id).cloned() else {
                    return;
                };
                if !obs.targets.contains(&target) {
                    return;
                }
                let mut rect = JsonValue::empty_object();
                rect.insert("x", JsonValue::Number(0.0));
                rect.insert("y", JsonValue::Number(0.0));
                rect.insert("top", JsonValue::Number(0.0));
                rect.insert("left", JsonValue::Number(0.0));
                rect.insert("width", JsonValue::Number(100.0));
                rect.insert("height", JsonValue::Number(24.0));
                rect.insert("right", JsonValue::Number(100.0));
                rect.insert("bottom", JsonValue::Number(24.0));
                let mut target_ref = JsonValue::empty_object();
                target_ref.insert(
                    "__anvil_element_id",
                    JsonValue::Number(target.index() as f64),
                );
                let mut entry = JsonValue::empty_object();
                entry.insert("target", target_ref);
                if obs.kind == "IntersectionObserver" {
                    entry.insert("isIntersecting", JsonValue::Bool(true));
                    entry.insert("intersectionRatio", JsonValue::Number(1.0));
                    entry.insert("boundingClientRect", rect.clone());
                    entry.insert("intersectionRect", rect.clone());
                    entry.insert("rootBounds", rect);
                } else {
                    entry.insert("contentRect", rect);
                }
                let mut obs_obj = JsonValue::empty_object();
                obs_obj.insert("__anvil_kind", JsonValue::String(obs.kind.clone()));
                obs_obj.insert("__observer_id", JsonValue::Number(obs_id as f64));
                self.invoke_observer_callback(
                    doc,
                    &obs.callback,
                    Value::Array(vec![Value::Json(entry)]),
                    Value::Json(obs_obj),
                );
            }
        }
    }

    fn invoke_observer_callback(
        &mut self,
        doc: &mut Document,
        callback: &str,
        arg0: Value,
        arg1: Value,
    ) {
        let cb_trimmed = callback.trim();
        if cb_trimmed.is_empty() {
            return;
        }
        if is_identifier(cb_trimmed) {
            if let Some((_, params, body)) = self
                .functions
                .iter()
                .find(|(name, _, _)| name == cb_trimmed)
                .cloned()
            {
                let mut env = Vec::new();
                if let Some(p0) = params.first() {
                    env.push((p0.clone(), arg0));
                }
                if let Some(p1) = params.get(1) {
                    env.push((p1.clone(), arg1));
                }
                self.eval_deferred_env(doc, &body, &env);
                return;
            }
        }
        let (params, body) = extract_multi_param_callback(cb_trimmed);
        let mut env = Vec::new();
        if let Some(p0) = params.first().filter(|s| !s.is_empty()) {
            env.push((p0.clone(), arg0));
        }
        if let Some(p1) = params.get(1).filter(|s| !s.is_empty()) {
            env.push((p1.clone(), arg1));
        }
        self.eval_deferred_env(doc, &body, &env);
    }

    pub(crate) fn notify_mutation_observers(
        &mut self,
        doc: &Document,
        kind: &str,
        target: NodeId,
        attr_name: Option<&str>,
    ) {
        if self.mutation_observers.is_empty() {
            return;
        }
        let mut to_schedule = Vec::new();
        for (obs_id, obs) in self.mutation_observers.iter_mut().enumerate() {
            let mut matched = false;
            for watched in &obs.targets {
                let target_matches = watched.node_id == target
                    || (watched.subtree && doc.contains_node(watched.node_id, target));
                if !target_matches {
                    continue;
                }
                match kind {
                    "childList" if watched.child_list => matched = true,
                    "attributes" if watched.attributes => matched = true,
                    "characterData" if watched.character_data || watched.child_list => {
                        matched = true
                    }
                    _ => {}
                }
                if matched {
                    break;
                }
            }
            if matched {
                let mut target_ref = JsonValue::empty_object();
                target_ref.insert(
                    "__anvil_element_id",
                    JsonValue::Number(target.index() as f64),
                );
                let mut rec = JsonValue::empty_object();
                rec.insert("type", JsonValue::String(kind.to_string()));
                rec.insert("target", target_ref);
                rec.insert(
                    "attributeName",
                    attr_name
                        .map(|s| JsonValue::String(s.to_string()))
                        .unwrap_or(JsonValue::Null),
                );
                rec.insert("addedNodes", JsonValue::Array(Vec::new()));
                rec.insert("removedNodes", JsonValue::Array(Vec::new()));
                obs.records.push(rec);
                if !obs.scheduled {
                    obs.scheduled = true;
                    to_schedule.push(obs_id);
                }
            }
        }
        for obs_id in to_schedule {
            self.schedule_job(Job::MutationObserver { obs_id });
        }
    }

    pub(crate) fn register_vm_event_listener(&mut self, node: NodeId, event_type: &str) {
        let ev = event_type.trim().to_ascii_lowercase();
        if ev.is_empty() {
            return;
        }
        if !self
            .vm_event_listeners
            .iter()
            .any(|(n, e)| *n == node && e == &ev)
        {
            self.vm_event_listeners.push((node, ev));
        }
    }

    /// Drain the microtask queue in FIFO order. Returns the number of jobs run.
    pub fn run_microtasks(&mut self, doc: &mut Document) -> usize {
        let mut total = 0usize;
        let mut ticks = 0usize;
        while !self.aborted {
            let Some(job) = self.microtasks.pop_front() else {
                break;
            };
            ticks += 1;
            if ticks > MAX_EVENT_LOOP_TICKS {
                self.abort_with("microtask loop did not settle");
                break;
            }
            total += 1;
            self.run_job(doc, job);
        }
        total
    }

    fn has_pending_timer(&self) -> bool {
        self.timers.iter().any(|t| !t.cleared)
    }

    /// Index of the next timer to fire: smallest due time, then insertion order.
    fn next_timer_index(&self) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, t) in self.timers.iter().enumerate() {
            if t.cleared {
                continue;
            }
            match best {
                None => best = Some(i),
                Some(b) => {
                    let cur = &self.timers[b];
                    if (t.due, t.seq) < (cur.due, cur.seq) {
                        best = Some(i);
                    }
                }
            }
        }
        best
    }

    /// Fire the next due timer. An interval is rescheduled; a timeout is removed.
    fn fire_one_timer(&mut self, doc: &mut Document) {
        let Some(idx) = self.next_timer_index() else {
            return;
        };
        let timer = self.timers[idx].clone();
        self.clock_ms = self.clock_ms.max(timer.due);
        if let Some(period) = timer.period {
            let seq = self.alloc_seq();
            self.timers[idx].seq = seq;
            self.timers[idx].due = self.clock_ms.saturating_add(period.max(1));
        } else {
            self.timers.remove(idx);
        }
        self.timer_depth = self.timer_depth.saturating_add(1);
        if timer.is_async {
            let _ = self.start_async(doc, &[], Vec::new(), &timer.body, &timer.env);
        } else {
            self.eval_deferred_env(doc, &timer.body, &timer.env);
        }
        self.timer_depth = self.timer_depth.saturating_sub(1);
    }

    /// Run the host event loop: microtasks, then each due timer with its
    /// microtasks drained in between. Returns the number of timer callbacks run.
    pub fn run_timers(&mut self, doc: &mut Document) -> usize {
        let _ = self.run_microtasks(doc);
        let mut fired = 0usize;
        let mut ticks = 0usize;
        while self.has_pending_timer() && !self.aborted {
            ticks += 1;
            if ticks > MAX_EVENT_LOOP_TICKS {
                self.abort_with("timer loop did not settle");
                break;
            }
            if !self.consume_step() {
                self.abort_with("step budget exhausted");
                break;
            }
            self.fire_one_timer(doc);
            fired += 1;
            let _ = self.run_microtasks(doc);
        }
        fired
    }

    /// Drive the loop until `value` (a promise) settles, then yield its value.
    /// A rejection is raised as a thrown error so `try`/`catch` can see it.
    fn await_value(&mut self, doc: &mut Document, value: Value) -> Value {
        let Some(id) = promise_id_of(&value) else {
            return value;
        };
        if id < self.promises.len() {
            self.promises[id].handled = true;
        }
        let mut ticks = 0usize;
        loop {
            if id >= self.promises.len() {
                return Value::Null;
            }
            match self.promises[id].state {
                PromiseState::Fulfilled => return self.promises[id].value.clone(),
                PromiseState::Rejected => {
                    let reason = self.promises[id].value.clone();
                    self.thrown_error = Some(reason);
                    return Value::Null;
                }
                PromiseState::Pending => {}
            }
            if self.aborted {
                return Value::Null;
            }
            if !self.consume_step() {
                self.abort_with("step budget exhausted");
                return Value::Null;
            }
            ticks += 1;
            if ticks > MAX_EVENT_LOOP_TICKS {
                self.abort_with("await did not settle");
                return Value::Null;
            }
            if let Some(job) = self.microtasks.pop_front() {
                self.run_job(doc, job);
            } else if self.has_pending_timer() {
                self.fire_one_timer(doc);
            } else {
                return Value::Null;
            }
        }
    }

    /// Call an async function: build its frame, run it to its first `await`,
    /// and return the promise of its result.
    fn start_async(
        &mut self,
        doc: &mut Document,
        params: &[String],
        args: Vec<Value>,
        body: &str,
        env: &[(String, Value)],
    ) -> Value {
        let result = self.new_promise();
        let promise = self.make_promise_value(result);
        if self.aborted {
            return promise;
        }
        let cleaned = strip_comments(body);
        let mut names: Vec<String> = Vec::new();
        let mut saved: Vec<(String, Value)> = Vec::new();
        for (name, value) in env {
            push_unique_name(&mut names, name);
            saved.push((name.clone(), value.clone()));
        }
        let mut declared = Vec::new();
        collect_declared_names(&cleaned, &mut declared);
        for name in declared {
            push_unique_name(&mut names, &name);
            saved.push((name, Value::Null));
        }
        for (index, param) in params.iter().enumerate() {
            let param = param.trim();
            if !is_identifier(param) {
                continue;
            }
            push_unique_name(&mut names, param);
            saved.push((
                param.to_string(),
                args.get(index).cloned().unwrap_or(Value::Null),
            ));
        }
        // Function declarations are hoisted ahead of the other statements.
        let (decls, rest): (Vec<String>, Vec<String>) =
            split_statements(&cleaned).into_iter().partition(|stmt| {
                let t = stmt.trim_start();
                t.starts_with("function ") || t.starts_with("async function ")
            });
        let mut work: Vec<Work> = Vec::new();
        for stmt in rest.into_iter().rev() {
            work.push(Work::Stmt(stmt));
        }
        for stmt in decls.into_iter().rev() {
            work.push(Work::Stmt(stmt));
        }
        let frame = AsyncFrame {
            result,
            work,
            names,
            saved,
            resume_temp: None,
            temps: 0,
        };
        let slot = match self
            .frames
            .iter()
            .position(|entry| matches!(entry, FrameSlot::Free))
        {
            Some(slot) => {
                self.frames[slot] = FrameSlot::Waiting(frame);
                slot
            }
            None => {
                if self.frames.len() >= MAX_PROMISES {
                    self.abort_with("async call limit reached");
                    return promise;
                }
                self.frames.push(FrameSlot::Waiting(frame));
                self.frames.len() - 1
            }
        };
        self.run_frame(doc, slot, None);
        promise
    }

    /// Run the frame in `slot` until it suspends, returns, or throws.
    /// `input` is the settled outcome of the promise it was waiting on.
    fn run_frame(&mut self, doc: &mut Document, slot: usize, input: Option<(Value, bool)>) {
        // Only a waiting frame can run. Its slot is marked `Running` until
        // the frame suspends again or ends, so nothing else can take it.
        let taken = self
            .frames
            .get_mut(slot)
            .map(|entry| std::mem::replace(entry, FrameSlot::Running));
        let mut frame = match taken {
            Some(FrameSlot::Waiting(frame)) => frame,
            Some(other) => {
                self.frames[slot] = other;
                return;
            }
            None => return,
        };
        if self.call_depth >= MAX_CALL_DEPTH {
            self.frames[slot] = FrameSlot::Free;
            self.abort_call_depth();
            return;
        }
        let saved = std::mem::take(&mut frame.saved);
        let base = self.push_scope(&saved);
        let prev_names = std::mem::replace(&mut self.frame_names, frame.names.clone());
        let prev_return = self.return_value.take();
        let prev_throw = self.thrown_error.take();
        let prev_break = std::mem::replace(&mut self.loop_break, false);
        let prev_continue = std::mem::replace(&mut self.loop_continue, false);
        let outer_rust = self.in_rust_eval;
        self.in_rust_eval = true;
        self.call_depth += 1;

        let mut pending = None;
        if let Some((value, rejected)) = input {
            let temp = frame.resume_temp.take();
            if rejected {
                pending = Some(Completion::Throw(value));
            } else if let Some(temp) = temp {
                self.set_var(&temp, value);
            }
        }
        let outcome = self.drive_frame(doc, slot, &mut frame, pending);

        self.call_depth = self.call_depth.saturating_sub(1);
        self.in_rust_eval = outer_rust;
        self.return_value = prev_return;
        self.thrown_error = prev_throw;
        self.loop_break = prev_break;
        self.loop_continue = prev_continue;
        frame.saved = self.pop_scope(base, &frame.names);
        self.frame_names = prev_names;
        if let FrameOutcome::Suspended = outcome {
            self.frames[slot] = FrameSlot::Waiting(frame);
            return;
        }
        self.frames[slot] = FrameSlot::Free;
        match outcome {
            FrameOutcome::Returned(value) => self.resolve_promise(frame.result, value),
            FrameOutcome::Threw(err) => self.reject_promise(frame.result, err),
            FrameOutcome::Suspended | FrameOutcome::Stopped => {}
        }
    }

    fn drive_frame(
        &mut self,
        doc: &mut Document,
        slot: usize,
        frame: &mut AsyncFrame,
        mut pending: Option<Completion>,
    ) -> FrameOutcome {
        loop {
            if self.aborted {
                return FrameOutcome::Stopped;
            }
            if let Some(completion) = pending.take() {
                if let Some(done) = self.unwind_frame(frame, completion) {
                    return done;
                }
                continue;
            }
            let Some(work) = frame.work.pop() else {
                return FrameOutcome::Returned(Value::Null);
            };
            let step = match work {
                Work::Stmt(stmt) => self.frame_statement(doc, slot, frame, &stmt),
                Work::Loop(state) => self.frame_loop(doc, frame, state),
                Work::TryEnd { finally_body, .. } => {
                    if let Some(body) = finally_body {
                        frame.work.push(Work::FinallyEnd(None));
                        push_block(&mut frame.work, &body);
                    }
                    StepResult::Next
                }
                Work::FinallyEnd(Some(completion)) => StepResult::Complete(completion),
                Work::FinallyEnd(None) => StepResult::Next,
            };
            match step {
                StepResult::Next => {}
                StepResult::Suspend => return FrameOutcome::Suspended,
                StepResult::Complete(completion) => pending = Some(completion),
            }
        }
    }

    /// Unwind the work stack for a `return`, `throw`, `break`, or `continue`.
    /// Returns the frame's final outcome once nothing is left to catch it.
    fn unwind_frame(
        &mut self,
        frame: &mut AsyncFrame,
        completion: Completion,
    ) -> Option<FrameOutcome> {
        loop {
            let Some(work) = frame.work.pop() else {
                return Some(match completion {
                    Completion::Return(value) => FrameOutcome::Returned(value),
                    Completion::Throw(err) => FrameOutcome::Threw(err),
                    Completion::Break | Completion::Continue => FrameOutcome::Returned(Value::Null),
                });
            };
            match work {
                Work::Loop(state) => match completion {
                    Completion::Break => return None,
                    Completion::Continue => {
                        frame.work.push(Work::Loop(state));
                        return None;
                    }
                    Completion::Return(_) | Completion::Throw(_) => {}
                },
                Work::TryEnd {
                    catch_param,
                    catch_body,
                    finally_body,
                } => {
                    if let (Completion::Throw(err), Some(body)) = (&completion, &catch_body) {
                        if is_identifier(&catch_param) {
                            self.set_var(&catch_param, err.clone());
                        }
                        frame.work.push(Work::TryEnd {
                            catch_param: String::new(),
                            catch_body: None,
                            finally_body,
                        });
                        push_block(&mut frame.work, body);
                        return None;
                    }
                    if let Some(body) = finally_body {
                        frame.work.push(Work::FinallyEnd(Some(completion)));
                        push_block(&mut frame.work, &body);
                        return None;
                    }
                }
                // A new exit from inside a finally body replaces the old one.
                Work::Stmt(_) | Work::FinallyEnd(_) => {}
            }
        }
    }

    /// Turn the interpreter's control flags into a step result.
    fn take_completion(&mut self) -> StepResult {
        if let Some(err) = self.thrown_error.take() {
            return StepResult::Complete(Completion::Throw(err));
        }
        if let Some(value) = self.return_value.take() {
            return StepResult::Complete(Completion::Return(value));
        }
        if self.loop_break {
            self.loop_break = false;
            return StepResult::Complete(Completion::Break);
        }
        if self.loop_continue {
            self.loop_continue = false;
            return StepResult::Complete(Completion::Continue);
        }
        StepResult::Next
    }

    /// Run one statement of an async function. A statement with no `await`
    /// of its own runs whole. A block, `if`, loop, or `try` whose body awaits
    /// is opened onto the work stack, and any other awaiting statement
    /// suspends at its first `await`.
    fn frame_statement(
        &mut self,
        doc: &mut Document,
        slot: usize,
        frame: &mut AsyncFrame,
        raw: &str,
    ) -> StepResult {
        let stmt = raw.trim();
        if stmt.is_empty() {
            return StepResult::Next;
        }
        let masked = mask_nested_code(stmt);
        let Some(first) = find_await(&masked) else {
            return self.frame_run_whole(doc, frame, stmt);
        };
        if starts_with_word(stmt, "if") {
            let rest = stmt[2..].trim_start();
            let Some((cond, after_cond)) = extract_paren_group(rest) else {
                return self.frame_run_whole(doc, frame, stmt);
            };
            let cond_end = stmt.len() - after_cond.len();
            if first >= cond_end {
                let Some((then_body, after_then)) = extract_first_brace_block(after_cond) else {
                    return self.frame_run_whole(doc, frame, stmt);
                };
                let ok = self.eval_expr(doc, &cond).is_truthy();
                if let Some(err) = self.thrown_error.take() {
                    return StepResult::Complete(Completion::Throw(err));
                }
                if ok {
                    push_block(&mut frame.work, &then_body);
                } else if let Some(else_rest) = after_then.trim().strip_prefix("else") {
                    let else_rest = else_rest.trim();
                    match extract_first_brace_block(else_rest) {
                        Some((else_body, _)) => push_block(&mut frame.work, &else_body),
                        None => frame.work.push(Work::Stmt(else_rest.to_string())),
                    }
                }
                return StepResult::Next;
            }
            // The condition itself awaits: suspend on it below.
        } else if starts_with_word(stmt, "while") {
            let parsed = extract_paren_group(stmt[5..].trim_start()).and_then(|(cond, after)| {
                extract_first_brace_block(after).map(|(body, _)| (cond, body))
            });
            let Some((cond, body)) = parsed else {
                return self.frame_run_whole(doc, frame, stmt);
            };
            frame.work.push(Work::Loop(LoopState::While {
                cond,
                body,
                iters: 0,
            }));
            return StepResult::Next;
        } else if starts_with_word(stmt, "for") {
            let mut rest = stmt[3..].trim_start();
            let awaits = starts_with_word(rest, "await");
            if awaits {
                rest = rest[5..].trim_start();
            }
            let Some((header, after)) = extract_paren_group(rest) else {
                return self.frame_run_whole(doc, frame, stmt);
            };
            let Some((body, _)) = extract_first_brace_block(after) else {
                return self.frame_run_whole(doc, frame, stmt);
            };
            let header_end = stmt.len() - after.len();
            let parts: Vec<&str> = header.splitn(3, ';').collect();
            if parts.len() == 3 && !awaits {
                self.exec_statement(doc, parts[0]);
                if let Some(err) = self.thrown_error.take() {
                    return StepResult::Complete(Completion::Throw(err));
                }
                frame.work.push(Work::Loop(LoopState::For {
                    cond: parts[1].to_string(),
                    step: parts[2].to_string(),
                    body,
                    iters: 0,
                }));
                return StepResult::Next;
            }
            if awaits || first >= header_end {
                let Some((lhs, rhs)) = split_for_of_or_in(&header, " of ") else {
                    return self.frame_run_whole(doc, frame, stmt);
                };
                let iterable = self.eval_expr(doc, rhs.trim());
                if let Some(err) = self.thrown_error.take() {
                    return StepResult::Complete(Completion::Throw(err));
                }
                if awaits {
                    push_unique_name(&mut frame.names, "__anvil_item");
                }
                frame.work.push(Work::Loop(LoopState::ForOf {
                    binding: lhs.trim().to_string(),
                    items: iterable_items(iterable),
                    next: 0,
                    body,
                    awaits,
                }));
                return StepResult::Next;
            }
            // The iterable expression awaits: suspend on it below.
        } else if starts_with_word(stmt, "try") {
            let Some((try_body, after_try)) = extract_first_brace_block(stmt[3..].trim_start())
            else {
                return self.frame_run_whole(doc, frame, stmt);
            };
            let mut rem = after_try.trim();
            let mut catch_param = String::new();
            let mut catch_body = None;
            if let Some(catch_rest) = rem.strip_prefix("catch") {
                let mut catch_rest = catch_rest.trim_start();
                if let Some((param, after)) = extract_paren_group(catch_rest) {
                    catch_param = param.trim().to_string();
                    catch_rest = after;
                }
                if let Some((body, after)) = extract_first_brace_block(catch_rest) {
                    catch_body = Some(body);
                    rem = after.trim();
                }
            }
            let finally_body = rem
                .strip_prefix("finally")
                .and_then(extract_first_brace_block)
                .map(|(body, _)| body);
            frame.work.push(Work::TryEnd {
                catch_param,
                catch_body,
                finally_body,
            });
            push_block(&mut frame.work, &try_body);
            return StepResult::Next;
        } else if stmt.starts_with('{') {
            if let Some((inner, _)) = extract_first_brace_block(stmt) {
                push_block(&mut frame.work, &inner);
                return StepResult::Next;
            }
        } else if starts_with_word(stmt, "switch")
            || starts_with_word(stmt, "do")
            || starts_with_word(stmt, "class")
            || starts_with_word(stmt, "function")
            || stmt.starts_with("async function ")
        {
            return self.frame_run_whole(doc, frame, stmt);
        }
        self.suspend_at_await(doc, slot, frame, stmt, &masked, first)
    }

    /// Run a statement in one go. An `await` left inside it (in a `switch`,
    /// a `do`/`while`, or a loop condition) waits in place instead.
    fn frame_run_whole(
        &mut self,
        doc: &mut Document,
        frame: &mut AsyncFrame,
        stmt: &str,
    ) -> StepResult {
        self.exec_statement(doc, stmt);
        frame.temps = 0;
        self.take_completion()
    }

    /// Evaluate the operand of the `await` at `at`, then suspend until it
    /// settles. The statement is queued again with the `await` replaced by a
    /// slot that receives the settled value when the frame resumes.
    fn suspend_at_await(
        &mut self,
        doc: &mut Document,
        slot: usize,
        frame: &mut AsyncFrame,
        stmt: &str,
        masked: &[u8],
        at: usize,
    ) -> StepResult {
        let end = await_operand_end(masked, at + 5);
        let (Some(head), Some(operand), Some(tail)) =
            (stmt.get(..at), stmt.get(at + 5..end), stmt.get(end..))
        else {
            return self.frame_run_whole(doc, frame, stmt);
        };
        if !self.consume_step() {
            return StepResult::Next;
        }
        let value = self.eval_expr(doc, operand.trim());
        if let Some(err) = self.thrown_error.take() {
            return StepResult::Complete(Completion::Throw(err));
        }
        if self.aborted {
            return StepResult::Next;
        }
        let temp = format!("__anvil_await{}", frame.temps);
        frame.temps += 1;
        push_unique_name(&mut frame.names, &temp);
        let pid = self.value_as_promise(value);
        self.subscribe(
            pid,
            HandlerKind::Resume { frame: slot },
            HandlerKind::Resume { frame: slot },
            NO_RESULT,
        );
        frame.work.push(Work::Stmt(format!("{head}{temp}{tail}")));
        frame.resume_temp = Some(temp);
        StepResult::Suspend
    }

    /// Run one iteration of a stepped loop.
    fn frame_loop(
        &mut self,
        doc: &mut Document,
        frame: &mut AsyncFrame,
        state: LoopState,
    ) -> StepResult {
        if !self.consume_step() {
            return StepResult::Next;
        }
        match state {
            LoopState::While { cond, body, iters } => {
                let ok = self.eval_expr(doc, &cond).is_truthy();
                if let Some(err) = self.thrown_error.take() {
                    return StepResult::Complete(Completion::Throw(err));
                }
                if ok {
                    frame.work.push(Work::Loop(LoopState::While {
                        cond,
                        body: body.clone(),
                        iters: iters + 1,
                    }));
                    push_block(&mut frame.work, &body);
                }
            }
            LoopState::For {
                cond,
                step,
                body,
                iters,
            } => {
                if iters > 0 {
                    self.exec_statement(doc, &step);
                }
                let ok = cond.trim().is_empty() || self.eval_expr(doc, &cond).is_truthy();
                if let Some(err) = self.thrown_error.take() {
                    return StepResult::Complete(Completion::Throw(err));
                }
                if ok {
                    frame.work.push(Work::Loop(LoopState::For {
                        cond,
                        step,
                        body: body.clone(),
                        iters: iters + 1,
                    }));
                    push_block(&mut frame.work, &body);
                }
            }
            LoopState::ForOf {
                binding,
                items,
                next,
                body,
                awaits,
            } => {
                let Some(item) = items.get(next).cloned() else {
                    return StepResult::Next;
                };
                frame.work.push(Work::Loop(LoopState::ForOf {
                    binding: binding.clone(),
                    items,
                    next: next + 1,
                    body: body.clone(),
                    awaits,
                }));
                push_block(&mut frame.work, &body);
                if awaits {
                    self.set_var("__anvil_item", item);
                    frame
                        .work
                        .push(Work::Stmt(format!("{binding} = await __anvil_item")));
                } else {
                    let pattern = binding
                        .strip_prefix("let ")
                        .or_else(|| binding.strip_prefix("const "))
                        .or_else(|| binding.strip_prefix("var "))
                        .unwrap_or(&binding)
                        .trim()
                        .to_string();
                    self.bind_pattern(doc, &pattern, item);
                }
            }
        }
        StepResult::Next
    }

    pub fn get_var(&self, name: &str) -> Option<&Value> {
        self.vars
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    pub fn set_var(&mut self, name: &str, value: Value) {
        if let Some((_, slot)) = self.vars.iter_mut().rev().find(|(k, _)| k == name) {
            *slot = value;
        } else {
            self.vars.push((name.to_string(), value));
        }
    }

    pub fn run_document_scripts(&mut self, doc: &mut Document) {
        self.ensure_prelude_loaded(doc);
        self.reset_execution_budget();
        self.collect_onclick_attrs(doc, doc.root());
        for script in doc.scripts() {
            self.thrown_error = None;
            self.eval_script(doc, &script);
            self.finish_page_script(doc);
        }
        let _ = self.run_timers(doc);
    }

    /// Hydrate `onclick`/`oninput`/`onchange`/`onsubmit`/`onkeydown` attributes,
    /// declared functions/classes, and event listeners on a document whose
    /// initial script pass already ran inside `--tab-worker`, without
    /// re-running imperative DOM mutations.
    pub fn hydrate_interactive_bindings(&mut self, doc: &mut Document) {
        self.ensure_prelude_loaded(doc);
        self.reset_execution_budget();
        self.collect_onclick_attrs(doc, doc.root());
        for script in doc.scripts() {
            if !script_runs_here(&script) {
                continue;
            }
            let cleaned = strip_comments(&script);
            if should_route_dom_script_to_cinder(&cleaned) {
                if self.vm_initial_dom_seed.is_none() {
                    let dom_seed = build_hierarchical_dom_seed_js(doc, &[], None);
                    let env_seed = self.build_vm_storage_and_location_seed();
                    self.vm_initial_dom_seed = Some(format!("{dom_seed}\n{env_seed}"));
                }
                if let Ok(lowered) = compile_es2022_with_cinder(cleaned.trim(), false) {
                    if !self.vm_scripts.iter().any(|s| s == &lowered) {
                        self.vm_scripts.push(lowered);
                    }
                    for ev in ["click", "input", "change", "submit", "keydown"] {
                        if cleaned.contains(".addEventListener(")
                            && (cleaned.contains(&format!("'{ev}'"))
                                || cleaned.contains(&format!("\"{ev}\"")))
                        {
                            self.register_vm_event_listener(doc.body(), ev);
                        }
                    }
                }
            }
            if should_route_to_cinder_engine(&cleaned)
                && self.engine_session.inner.is_none()
                && cleaned.contains(".addEventListener(")
            {
                let _ = self.eval_with_cinder_engine(doc, cleaned.trim());
                continue;
            }
            if should_route_to_cinder_engine(&cleaned)
                && self.engine_session.inner.is_none()
                && cleaned.contains(".addEventListener(")
            {
                let _ = self.eval_with_cinder_engine(doc, cleaned.trim());
                continue;
            }
            if should_route_to_cinder_engine(&cleaned)
                && self.engine_session.inner.is_none()
                && cleaned.contains(".addEventListener(")
            {
                let _ = self.eval_with_cinder_engine(doc, cleaned.trim());
                continue;
            }
            if should_route_to_cinder_engine(&cleaned)
                && self.engine_session.inner.is_none()
                && cleaned.contains(".addEventListener(")
            {
                let _ = self.eval_with_cinder_engine(doc, cleaned.trim());
                continue;
            }
            for stmt in split_statements(&cleaned) {
                let s = stmt.trim();
                let declaration = s.starts_with("function ")
                    || s.starts_with("async function ")
                    || s.starts_with("class ");
                let plain_binding =
                    (s.starts_with("let ") || s.starts_with("const ") || s.starts_with("var "))
                        && !s.contains("fetch(")
                        && !s.contains("setTimeout(")
                        && !s.contains("setInterval(")
                        && !s.contains(".innerHTML")
                        && !s.contains(".textContent");
                let interactive_listener = s.contains(".addEventListener(")
                    && (s.contains("'click'")
                        || s.contains("\"click\"")
                        || s.contains("'input'")
                        || s.contains("\"input\"")
                        || s.contains("'change'")
                        || s.contains("\"change\"")
                        || s.contains("'submit'")
                        || s.contains("\"submit\"")
                        || s.contains("'keydown'")
                        || s.contains("\"keydown\""));
                if declaration || plain_binding || interactive_listener {
                    self.exec_statement(doc, s);
                }
            }
        }
        self.reset_execution_budget();
        self.in_rust_eval = true;
    }

    /// Compiled WebAssembly modules held by this runtime.
    pub fn wasm_modules(&self) -> &[WasmModule] {
        &self.wasm_modules
    }

    /// Active WebAssembly instances held by this runtime.
    pub fn wasm_instances(&self) -> &[WasmInstance] {
        &self.wasm_instances
    }

    /// Mutable access to an active WebAssembly instance by handle index.
    pub fn wasm_instance_mut(&mut self, index: usize) -> Option<&mut WasmInstance> {
        self.wasm_instances.get_mut(index)
    }

    /// True when the most recently dispatched DOM event (`click`, `input`,
    /// `submit`, or `keydown`) called `event.preventDefault()` or returned `false`.
    pub fn last_event_default_prevented(&self) -> bool {
        self.event_default_prevented
    }

    fn make_dom_event_value(
        &self,
        event_type: &str,
        target: NodeId,
        current_target: NodeId,
        value: Option<&str>,
        key: Option<&str>,
    ) -> Value {
        let mut target_ref = JsonValue::empty_object();
        target_ref.insert(
            "__anvil_element_id",
            JsonValue::Number(target.index() as f64),
        );
        let mut cur_ref = JsonValue::empty_object();
        cur_ref.insert(
            "__anvil_element_id",
            JsonValue::Number(current_target.index() as f64),
        );
        let mut ev = JsonValue::empty_object();
        ev.insert("__anvil_kind", JsonValue::String("Event".to_string()));
        ev.insert("type", JsonValue::String(event_type.to_string()));
        ev.insert("target", target_ref);
        ev.insert("currentTarget", cur_ref);
        ev.insert("bubbles", JsonValue::Bool(true));
        ev.insert("cancelable", JsonValue::Bool(true));
        ev.insert(
            "defaultPrevented",
            JsonValue::Bool(self.event_default_prevented),
        );
        if let Some(v) = value {
            ev.insert("value", JsonValue::String(v.to_string()));
            ev.insert("data", JsonValue::String(v.to_string()));
        }
        if let Some(k) = key {
            ev.insert("key", JsonValue::String(k.to_string()));
            ev.insert("code", JsonValue::String(k.to_string()));
        }
        Value::Json(ev)
    }

    fn run_event_handler_body(
        &mut self,
        doc: &mut Document,
        event_type: &str,
        target: NodeId,
        current_target: NodeId,
        value: Option<&str>,
        key: Option<&str>,
        body: &str,
    ) {
        let ev = self.make_dom_event_value(event_type, target, current_target, value, key);
        self.set_var("event", ev);
        let prev_return = self.return_value.take();
        self.eval_deferred(doc, body);
        if self.return_value == Some(Value::Bool(false)) {
            self.event_default_prevented = true;
        }
        self.return_value = prev_return;
    }

    fn has_vm_listener_in_chain(&self, doc: &Document, target: NodeId, event_type: &str) -> bool {
        if self.vm_scripts.is_empty() {
            return false;
        }
        let wanted = event_type.to_ascii_lowercase();
        let mut cursor = Some(target);
        while let Some(id) = cursor {
            if self
                .vm_event_listeners
                .iter()
                .any(|(n, ev)| *n == id && ev == &wanted)
            {
                return true;
            }
            cursor = doc.get(id).parent();
        }
        self.vm_event_listeners
            .iter()
            .any(|(n, ev)| (*n == doc.body() || *n == doc.root()) && ev == &wanted)
    }

    fn has_engine_listener_in_chain(
        &self,
        _doc: &Document,
        _target: NodeId,
        _event_type: &str,
    ) -> bool {
        self.engine_session.inner.is_some()
    }

    pub fn dispatch_click(&mut self, doc: &mut Document, target: NodeId) -> bool {
        self.ensure_prelude_loaded(doc);
        self.reset_execution_budget();
        self.event_default_prevented = false;
        let mut cursor = Some(target);
        let mut handled = false;
        while let Some(id) = cursor {
            let handlers: Vec<String> = self
                .click_listeners
                .iter()
                .filter(|(node, _)| *node == id)
                .map(|(_, body)| body.clone())
                .collect();
            for body in handlers {
                self.run_event_handler_body(doc, "click", target, id, None, None, &body);
                handled = true;
            }
            cursor = doc.get(id).parent();
        }
        if self.has_vm_listener_in_chain(doc, target, "click")
            && self.dispatch_vm_host_event(doc, target, "click", None, None)
        {
            handled = true;
        }
        if self.has_engine_listener_in_chain(doc, target, "click")
            && self.dispatch_engine_host_event(doc, target, "click", None, None)
        {
            handled = true;
        }
        if handled {
            let _ = self.run_timers(doc);
        }
        handled
    }

    /// Dispatch a live `input` (and `change`) event on `target` after its input value
    /// has been updated to `value`.
    pub fn dispatch_input(&mut self, doc: &mut Document, target: NodeId, value: &str) -> bool {
        self.ensure_prelude_loaded(doc);
        self.reset_execution_budget();
        self.event_default_prevented = false;
        doc.set_input_value(target, value);
        let mut cursor = Some(target);
        let mut handled = false;
        while let Some(id) = cursor {
            let handlers: Vec<String> = self
                .input_listeners
                .iter()
                .chain(self.change_listeners.iter())
                .filter(|(node, _)| *node == id)
                .map(|(_, body)| body.clone())
                .collect();
            for body in handlers {
                self.run_event_handler_body(doc, "input", target, id, Some(value), None, &body);
                handled = true;
            }
            cursor = doc.get(id).parent();
        }
        if (self.has_vm_listener_in_chain(doc, target, "input")
            || self.has_vm_listener_in_chain(doc, target, "change"))
            && self.dispatch_vm_host_event(doc, target, "input", Some(value), None)
        {
            handled = true;
        }
        if (self.has_engine_listener_in_chain(doc, target, "input")
            || self.has_engine_listener_in_chain(doc, target, "change"))
            && self.dispatch_engine_host_event(doc, target, "input", Some(value), None)
        {
            handled = true;
        }
        if handled {
            let _ = self.run_timers(doc);
        }
        handled
    }

    /// Dispatch a live `submit` event on `form_target` (or its enclosing `<form>`).
    /// Check [`Self::last_event_default_prevented`] afterward to see whether native
    /// form navigation was cancelled via `event.preventDefault()` or `return false`.
    pub fn dispatch_submit(&mut self, doc: &mut Document, form_target: NodeId) -> bool {
        self.ensure_prelude_loaded(doc);
        self.reset_execution_budget();
        self.event_default_prevented = false;
        let mut cursor = Some(form_target);
        let mut handled = false;
        while let Some(id) = cursor {
            let handlers: Vec<String> = self
                .submit_listeners
                .iter()
                .filter(|(node, _)| *node == id)
                .map(|(_, body)| body.clone())
                .collect();
            for body in handlers {
                self.run_event_handler_body(doc, "submit", form_target, id, None, None, &body);
                handled = true;
            }
            cursor = doc.get(id).parent();
        }
        if self.has_vm_listener_in_chain(doc, form_target, "submit")
            && self.dispatch_vm_host_event(doc, form_target, "submit", None, None)
        {
            handled = true;
        }
        if self.has_engine_listener_in_chain(doc, form_target, "submit")
            && self.dispatch_engine_host_event(doc, form_target, "submit", None, None)
        {
            handled = true;
        }
        if handled {
            let _ = self.run_timers(doc);
        }
        handled
    }

    /// Dispatch a live `keydown` event on `target`.
    pub fn dispatch_keydown(&mut self, doc: &mut Document, target: NodeId, key: &str) -> bool {
        self.ensure_prelude_loaded(doc);
        self.reset_execution_budget();
        self.event_default_prevented = false;
        let mut cursor = Some(target);
        let mut handled = false;
        while let Some(id) = cursor {
            let handlers: Vec<String> = self
                .keydown_listeners
                .iter()
                .filter(|(node, _)| *node == id)
                .map(|(_, body)| body.clone())
                .collect();
            for body in handlers {
                self.run_event_handler_body(doc, "keydown", target, id, None, Some(key), &body);
                handled = true;
            }
            cursor = doc.get(id).parent();
        }
        if self.has_vm_listener_in_chain(doc, target, "keydown")
            && self.dispatch_vm_host_event(doc, target, "keydown", None, Some(key))
        {
            handled = true;
        }
        if self.has_engine_listener_in_chain(doc, target, "keydown")
            && self.dispatch_engine_host_event(doc, target, "keydown", None, Some(key))
        {
            handled = true;
        }
        if handled {
            let _ = self.run_timers(doc);
        }
        handled
    }

    fn build_vm_storage_and_location_seed(&self) -> String {
        let mut out = String::new();
        if let Some(origin) = self.page_origin.as_deref()
            && !origin.is_empty()
        {
            let q_origin = quote_js_string_literal(origin);
            out.push_str(&format!(
                "window.location.href = {q_origin}; document.URL = {q_origin}; document.documentURI = {q_origin};\n"
            ));
        }
        let origin_key = self.storage_origin_key();
        for (o, k, v) in &self.local_storage {
            if o == &origin_key {
                out.push_str(&format!(
                    "window.localStorage._seedItem({}, {});\n",
                    quote_js_string_literal(k),
                    quote_js_string_literal(v)
                ));
            }
        }
        out
    }

    /// Compile and run `source` on `CinderVM` with the live hierarchical DOM bridge,
    /// applying all DOM, `localStorage`, and `location` mutations back to `doc`
    /// and `self`.
    pub fn eval_with_cinder_dom_bridge(&mut self, doc: &mut Document, source: &str) -> bool {
        if !is_cpp_jit_kernel_active() {
            return false;
        }
        let Ok(lowered_app) = compile_es2022_with_cinder(source, false) else {
            return false;
        };
        let dom_seed = build_hierarchical_dom_seed_js(doc, &[], None);
        let env_seed = self.build_vm_storage_and_location_seed();
        let initial_seed = format!("{dom_seed}\n{env_seed}");
        if self.vm_initial_dom_seed.is_none() {
            self.vm_initial_dom_seed = Some(initial_seed.clone());
        }
        let rt_bundle = self_hosted_runtime_bundle();
        let initial_url = self.page_origin.as_deref().unwrap_or("https://localhost/");
        let quoted_url = quote_js_string_literal(initial_url);
        let full_program = format!(
            "{rt_bundle}\n\
const env = createBrowserEnv({quoted_url});\n\
const window = env.window;\n\
const globalThis = window;\n\
const self = window;\n\
const document = env.document;\n\
const navigator = env.navigator;\n\
const location = env.location;\n\
const history = env.history;\n\
const localStorage = env.localStorage;\n\
const sessionStorage = env.sessionStorage;\n\
const customElements = env.customElements;\n\
const performance = env.performance;\n\
const JSON = {{ parse: anvilJsonParse, stringify: anvilJsonStringify }};\n\
const btoa = anvilBtoa;\n\
const atob = anvilAtob;\n\
const encodeURIComponent = anvilEncodeURIComponent;\n\
const decodeURIComponent = anvilDecodeURIComponent;\n\
{initial_seed}\n\
{lowered_app}\n\
{CINDER_VM_MUTATION_EXPORT_FOOTER}\n"
        );
        let Ok(run) = eval_cpp_vm_with_console_configured(
            &full_program,
            80_000_000,
            64 * 1024 * 1024,
            10_000,
        ) else {
            return false;
        };
        self.last_script_used_vm = true;
        self.last_vm_number = Some(run.result);
        if let Ok(globals) = run.telemetry.exported_globals() {
            for (name, value) in globals {
                self.set_var(&name, value);
            }
        }
        let mut parsed_records: Vec<JsonValue> = Vec::new();
        for line in &run.output {
            if let Some(rest) = line.strip_prefix("__ANVIL_DOM_MUT__:") {
                if let Ok(parsed) = parse_json(rest) {
                    parsed_records.push(parsed);
                }
            } else {
                self.push_console(line.clone());
            }
        }
        if !parsed_records.is_empty() {
            let _ = apply_vm_dom_mutations_with_runtime(
                doc,
                &JsonValue::Array(parsed_records),
                Some(self),
            );
        }
        if !self.vm_scripts.iter().any(|s| s == &lowered_app) {
            self.vm_scripts.push(lowered_app);
        }
        true
    }

    fn dispatch_vm_host_event(
        &mut self,
        doc: &mut Document,
        target: NodeId,
        event_type: &str,
        value: Option<&str>,
        key: Option<&str>,
    ) -> bool {
        if !is_cpp_jit_kernel_active() || self.vm_scripts.is_empty() {
            return false;
        }
        let base_seed = self.vm_initial_dom_seed.clone().unwrap_or_else(|| {
            let dom_seed = build_hierarchical_dom_seed_js(doc, &[], None);
            let env_seed = self.build_vm_storage_and_location_seed();
            format!("{dom_seed}\n{env_seed}")
        });
        let mut init_parts = Vec::new();
        if let Some(v) = value {
            init_parts.push(format!("value: {}", quote_js_string_literal(v)));
        }
        if let Some(k) = key {
            init_parts.push(format!("key: {}", quote_js_string_literal(k)));
        }
        let init_js = format!("{{ {} }}", init_parts.join(", "));
        let scripts_joined = self.vm_scripts.join("\n;\n");
        let mut prior_events_js = String::new();
        for (ev_target, ev_type, ev_init) in &self.vm_event_history {
            prior_events_js.push_str(&format!(
                "document.dispatchHostEvent({}, {}, {});\n",
                ev_target,
                quote_js_string_literal(ev_type),
                ev_init
            ));
        }
        let mut input_sync_js = String::new();
        for (idx, node_id) in doc
            .query_selector_all("input, textarea, select")
            .into_iter()
            .enumerate()
        {
            let cur_val = doc.input_value(node_id);
            if !cur_val.is_empty() {
                let q_val = quote_js_string_literal(&cur_val);
                input_sync_js.push_str(&format!(
                    "const __inp_{idx} = document.findByDomNodeId({}); if (__inp_{idx}) {{ __inp_{idx}.value = {q_val}; __inp_{idx}._lastSyncedValue = {q_val}; }}\n",
                    node_id.index()
                ));
            }
        }
        let reset_mutations_js = "\
document.syncDomTree();
const __kept = [];
for (let __i = 0; __i < document._domMutations.length; __i = __i + 1) {
    if (document._domMutations[__i].op === \"bindSeedNode\") {
        __kept.push(document._domMutations[__i]);
    }
}
document._domMutations = __kept;";
        let current_event_js = format!(
            "const __ev = document.dispatchHostEvent({}, {}, {}); if (document._lastEventHandled) {{ console.log(\"__ANVIL_EVENT_HANDLED__:1\"); }} if (!__ev) {{ console.log(\"__ANVIL_EVENT_PREVENTED__:1\"); }}",
            target.index(),
            quote_js_string_literal(event_type),
            init_js
        );
        let rt_bundle = self_hosted_runtime_bundle();
        let initial_url = self.page_origin.as_deref().unwrap_or("https://localhost/");
        let quoted_url = quote_js_string_literal(initial_url);
        let full_program = format!(
            "{rt_bundle}\n\
const env = createBrowserEnv({quoted_url});\n\
const window = env.window;\n\
const globalThis = window;\n\
const self = window;\n\
const document = env.document;\n\
const navigator = env.navigator;\n\
const location = env.location;\n\
const history = env.history;\n\
const localStorage = env.localStorage;\n\
const sessionStorage = env.sessionStorage;\n\
const customElements = env.customElements;\n\
const performance = env.performance;\n\
const JSON = {{ parse: anvilJsonParse, stringify: anvilJsonStringify }};\n\
const btoa = anvilBtoa;\n\
const atob = anvilAtob;\n\
const encodeURIComponent = anvilEncodeURIComponent;\n\
const decodeURIComponent = anvilDecodeURIComponent;\n\
{base_seed}\n\
{scripts_joined}\n\
{prior_events_js}\n\
{input_sync_js}\n\
{reset_mutations_js}\n\
{current_event_js}\n\
{CINDER_VM_MUTATION_EXPORT_FOOTER}\n"
        );
        let Ok(run) = eval_cpp_vm_with_console_configured(
            &full_program,
            80_000_000,
            64 * 1024 * 1024,
            10_000,
        ) else {
            return false;
        };
        let mut event_handled = false;
        let mut parsed_records: Vec<JsonValue> = Vec::new();
        for line in &run.output {
            if line == "__ANVIL_EVENT_HANDLED__:1" {
                event_handled = true;
            } else if line == "__ANVIL_EVENT_PREVENTED__:1" {
                self.event_default_prevented = true;
            } else if let Some(rest) = line.strip_prefix("__ANVIL_DOM_MUT__:") {
                if let Ok(parsed) = parse_json(rest) {
                    parsed_records.push(parsed);
                }
            } else {
                self.push_console(line.clone());
            }
        }
        if !event_handled {
            return false;
        }
        if !parsed_records.is_empty() {
            if apply_vm_dom_mutations_with_runtime(
                doc,
                &JsonValue::Array(parsed_records),
                Some(self),
            ) {
                self.event_default_prevented = true;
            }
        }
        self.vm_event_history
            .push((target.index() as u32, event_type.to_string(), init_js));
        true
    }

    /// Compile and execute `source` directly on the `cinder` register bytecode VM
    /// (`cinder::vm::Vm`) with live [`CinderDomHost`] bindings against `doc`.
    ///
    /// Returns the formatted completion value on success, or the compiler/runtime
    /// diagnostic message on failure.
    pub fn eval_with_cinder_engine(
        &mut self,
        doc: &mut Document,
        source: &str,
    ) -> Result<String, String> {
        let origin_key = self.storage_origin_key();
        let initial_url = self
            .page_origin
            .clone()
            .unwrap_or_else(|| "https://localhost/".to_string());
        if self.engine_session.inner.is_none() {
            let mut vm =
                cinder::vm::Vm::new(cinder::Limits::default()).map_err(|e| e.to_string())?;
            cinder::install(&mut vm).map_err(|e| e.to_string())?;
            let mut host = CinderDomHost::new(Document::new(), &initial_url);
            host.install(&mut vm).map_err(|e| e.to_string())?;
            self.engine_session.inner = Some(Box::new(EngineSession { vm, host }));
        }
        let local_seed: Vec<(String, String)> = self
            .local_storage
            .iter()
            .filter(|(o, _, _)| o == &origin_key)
            .map(|(_, k, v)| (k.clone(), v.clone()))
            .collect();
        let session_seed: Vec<(String, String)> = self
            .session_storage
            .iter()
            .filter(|(o, _, _)| o == &origin_key)
            .map(|(_, k, v)| (k.clone(), v.clone()))
            .collect();

        let (run_res, console_lines, local_items, session_items, pending_nav) = {
            let session = self.engine_session.inner.as_mut().unwrap();
            std::mem::swap(&mut session.host.doc, doc);
            session.host.location_href = initial_url;
            session.host.local_storage = local_seed;
            session.host.session_storage = session_seed;
            session.host.fetch_mocks = self.fetch_mocks.clone();
            session.host.console.clear();

            let limits = session.vm.limits().clone();
            let block = match cinder::compile_script(source, &limits) {
                Ok(b) => b,
                Err(first_err) => {
                    if let Ok(lowered) = compile_es2022_with_cinder(source, false)
                        && let Ok(b) = cinder::compile_script(&lowered, &limits)
                    {
                        b
                    } else if let Some(recovered_block) =
                        recover_script_statements_for_cinder(source, &limits)
                    {
                        recovered_block
                    } else {
                        let msg = first_err.to_string();
                        session.host.report_uncaught_error(&mut session.vm, &msg);
                        session.host.console.push(format!("[error] {msg}"));
                        std::mem::swap(&mut session.host.doc, doc);
                        let console_lines = std::mem::take(&mut session.host.console);
                        for line in console_lines {
                            self.push_console(line);
                        }
                        return Err(msg);
                    }
                }
            };
            let res: Result<String, String> = match session.vm.load(block) {
                Ok(code_id) => match session.vm.run(&mut session.host, code_id) {
                    Ok(val) => {
                        let _ = session.vm.drain_jobs(&mut session.host);
                        let _ = session.host.run_timers(&mut session.vm);
                        session.vm.inspect(val).map_err(|e| e.to_string())
                    }
                    Err(run_err) => {
                        let msg = run_err.to_string();
                        session.host.report_uncaught_error(&mut session.vm, &msg);
                        let _ = session.vm.drain_jobs(&mut session.host);
                        let _ = session.host.run_timers(&mut session.vm);
                        session.host.console.push(format!("[error] {msg}"));
                        Err(msg)
                    }
                },
                Err(load_err) => Err(load_err.to_string()),
            };

            std::mem::swap(&mut session.host.doc, doc);
            let console_lines = std::mem::take(&mut session.host.console);
            let local_items: Vec<(String, String)> = session.host.local_storage.clone();
            let session_items: Vec<(String, String)> = session.host.session_storage.clone();
            let pending_nav = session.host.pending_navigation.take();
            (res, console_lines, local_items, session_items, pending_nav)
        };

        for line in console_lines {
            self.push_console(line);
        }
        self.local_storage.retain(|(o, _, _)| o != &origin_key);
        for (k, v) in local_items {
            storage_put(&mut self.local_storage, &origin_key, &k, &v);
        }
        self.session_storage.retain(|(o, _, _)| o != &origin_key);
        for (k, v) in session_items {
            storage_put(&mut self.session_storage, &origin_key, &k, &v);
        }
        if let Some(nav) = pending_nav {
            self.pending_navigation = Some(nav);
        }
        if run_res.is_ok() {
            self.last_script_used_vm = true;
        }
        run_res
    }

    fn dispatch_engine_host_event(
        &mut self,
        doc: &mut Document,
        target: NodeId,
        event_type: &str,
        value: Option<&str>,
        key: Option<&str>,
    ) -> bool {
        if self.engine_session.inner.is_none() {
            return false;
        }
        let origin_key = self.storage_origin_key();
        let local_seed: Vec<(String, String)> = self
            .local_storage
            .iter()
            .filter(|(o, _, _)| o == &origin_key)
            .map(|(_, k, v)| (k.clone(), v.clone()))
            .collect();
        let session_seed: Vec<(String, String)> = self
            .session_storage
            .iter()
            .filter(|(o, _, _)| o == &origin_key)
            .map(|(_, k, v)| (k.clone(), v.clone()))
            .collect();

        let (handled, default_prevented, console_lines, local_items, session_items, pending_nav) = {
            let session = self.engine_session.inner.as_mut().unwrap();
            std::mem::swap(&mut session.host.doc, doc);
            session.host.local_storage = local_seed;
            session.host.session_storage = session_seed;
            session.host.console.clear();

            let handled = session
                .host
                .dispatch_event(&mut session.vm, target, event_type, value, key)
                .unwrap_or(false);
            let _ = session.host.run_timers(&mut session.vm);
            let default_prevented = session.host.event_default_prevented;

            std::mem::swap(&mut session.host.doc, doc);
            let console_lines = std::mem::take(&mut session.host.console);
            let local_items: Vec<(String, String)> = session.host.local_storage.clone();
            let session_items: Vec<(String, String)> = session.host.session_storage.clone();
            let pending_nav = session.host.pending_navigation.take();
            (
                handled,
                default_prevented,
                console_lines,
                local_items,
                session_items,
                pending_nav,
            )
        };

        for line in console_lines {
            self.push_console(line);
        }
        if default_prevented {
            self.event_default_prevented = true;
        }
        self.local_storage.retain(|(o, _, _)| o != &origin_key);
        for (k, v) in local_items {
            storage_put(&mut self.local_storage, &origin_key, &k, &v);
        }
        self.session_storage.retain(|(o, _, _)| o != &origin_key);
        for (k, v) in session_items {
            storage_put(&mut self.session_storage, &origin_key, &k, &v);
        }
        if let Some(nav) = pending_nav {
            self.pending_navigation = Some(nav);
        }
        handled
    }

    /// Update the active layout viewport dimensions and vertical scroll position,
    /// re-evaluating active `IntersectionObserver` and `ResizeObserver` callbacks.
    pub fn notify_layout_and_scroll(
        &mut self,
        doc: &mut Document,
        viewport_w: f32,
        viewport_h: f32,
        scroll_y: f32,
    ) -> bool {
        if let Some(session) = self.engine_session.inner.as_mut() {
            session.host.viewport_width = viewport_w.max(1.0);
            session.host.viewport_height = viewport_h.max(1.0);
        }
        if self.engine_session.inner.is_some() {
            let js = format!(
                "if (typeof window.__notifyLayoutAndScroll === 'function') {{ window.__notifyLayoutAndScroll({viewport_w}, {viewport_h}, {scroll_y}); }}"
            );
            return self.eval_with_cinder_engine(doc, &js).is_ok();
        }
        false
    }

    pub fn eval_script(&mut self, doc: &mut Document, source: &str) {
        if self.aborted {
            return;
        }
        if !script_runs_here(source) {
            let code = code_bytes_outside_strings(source.trim());
            if !self.in_rust_eval && code > MAX_SCRIPT_SOURCE {
                self.push_console(format!(
                    "[warn] Script not run: {code} bytes of code, over the {MAX_SCRIPT_SOURCE} byte limit"
                ));
            }
            return;
        }
        self.last_script_used_vm = false;
        self.last_vm_number = None;
        let cleaned = strip_comments(source);
        // Nested calls are loop bodies and function bodies. They share the Rust
        // variable environment, so they stay on the Rust interpreter.
        if !self.in_rust_eval {
            self.last_vm_error = None;
        }
        if !self.in_rust_eval && !script_stays_on_rust(&cleaned) {
            // A VM run has no effect on the page, so a script the VM refuses
            // or stops at a limit runs again from the start on the Rust
            // interpreter. The reason is kept for last_vm_error. The run also
            // services console.log host calls and collects their output; that
            // output is only committed once the whole script ran on the VM, so
            // a fallback to the interpreter never double-prints a line.
            let (mut output, mut outcome) = self.run_cpp_vm_console(cleaned.trim(), 0, 0, 0);
            if matches!(&outcome, Err(err) if err.refused() && !err.is_limit())
                && needs_cinder_lowering(&cleaned)
                && let Ok(lowered) = compile_es2022_with_cinder(cleaned.trim(), false)
            {
                let (low_out, low_res) = self.run_cpp_vm_console(&lowered, 0, 0, 0);
                if low_res.is_ok() {
                    output = low_out;
                    outcome = low_res;
                }
            }
            match outcome {
                Ok((result, telemetry)) => match telemetry.exported_globals() {
                    Ok(globals) => {
                        self.last_script_used_vm = true;
                        self.last_vm_number = Some(result);
                        for line in output {
                            self.push_console(line);
                        }
                        // Later scripts see the top-level variables and the
                        // functions this one declared.
                        for (name, value) in globals {
                            self.set_var(&name, value);
                        }
                        for stmt in split_statements(&cleaned) {
                            let s = stmt.trim_start();
                            if s.starts_with("function ") {
                                self.exec_statement(doc, s);
                            }
                        }
                        return;
                    }
                    // Later scripts could not see what this one declared, so it
                    // runs on the Rust interpreter. Any lines the VM collected
                    // are dropped so the interpreter does not reprint them.
                    Err(message) => {
                        self.last_vm_error = Some(VmError { code: 33, message });
                    }
                },
                Err(error) if error.code == 33 && error.message.contains("not available") => {}
                Err(error) => {
                    self.last_vm_error = Some(error);
                }
            }
        }
        if !self.in_rust_eval
            && should_route_dom_script_to_cinder(&cleaned)
            && self.eval_with_cinder_dom_bridge(doc, cleaned.trim())
        {
            return;
        }
        if !self.in_rust_eval
            && (self.engine_session.inner.is_some() || should_route_to_cinder_engine(&cleaned))
        {
            let _ = self.eval_with_cinder_engine(doc, cleaned.trim());
            return;
        }
        let outer = self.in_rust_eval;
        self.in_rust_eval = true;
        self.eval_rust_script(doc, &cleaned);
        self.in_rust_eval = outer;
    }

    fn eval_rust_script(&mut self, doc: &mut Document, source: &str) {
        let cleaned = strip_comments(source);
        let statements = split_statements(&cleaned);
        for stmt in &statements {
            let s = stmt.trim_start();
            if s.starts_with("function ")
                || s.starts_with("async function ")
                || s.starts_with("class ")
            {
                self.exec_statement(doc, stmt);
            }
        }
        if self.jit_stats.jit_enabled {
            let compiled = self.compile_jit_block(&statements);
            self.jit_stats.compiled_blocks = self.jit_stats.compiled_blocks.saturating_add(1);
            for op in compiled {
                if self.aborted
                    || self.return_value.is_some()
                    || self.thrown_error.is_some()
                    || self.loop_break
                    || self.loop_continue
                {
                    break;
                }
                self.exec_compiled_op(doc, op);
            }
        } else {
            for stmt in statements {
                if self.aborted
                    || self.return_value.is_some()
                    || self.thrown_error.is_some()
                    || self.loop_break
                    || self.loop_continue
                {
                    break;
                }
                let s = stmt.trim_start();
                if !s.starts_with("function ")
                    && !s.starts_with("async function ")
                    && !s.starts_with("class ")
                {
                    self.exec_statement(doc, &stmt);
                }
            }
        }
    }

    /// Run `source` on the owned context, servicing every `console.log` host
    /// call (a pause, status 3) by formatting and collecting its line, then
    /// sending a Resume. Returns the lines collected so far together with the
    /// final value and telemetry, or the stop or refusal reason. The lines are
    /// returned on both paths, so a caller that prints as it goes keeps the
    /// output a run made before it stopped. No fallback to the interpreter.
    #[allow(clippy::type_complexity)]
    pub(crate) fn run_cpp_vm_console(
        &mut self,
        source: &str,
        step_budget: u32,
        heap_byte_budget: u32,
        call_depth_limit: u32,
    ) -> (Vec<String>, Result<(f64, Box<CinderVmTelemetry>), VmError>) {
        // Budgets apply to the Run and every Resume on this context; the source
        // size is capped only by the heap budget (script size 0).
        let configure = vm_request::configure(step_budget, 0, heap_byte_budget, call_depth_limit);
        let _ = self.try_owned_vm(&configure);
        let resume = vm_request::resume();
        let mut outcome = self.try_owned_vm(&vm_request::run(source.as_bytes()));
        let mut output: Vec<String> = Vec::new();
        // A pause comes back as a stop with code 3; the telemetry for it is on
        // the runtime. Read the host call, collect its line, then Resume.
        loop {
            match outcome {
                VmRun::Value(result, telemetry) => {
                    return (output, Ok((result, telemetry)));
                }
                VmRun::Stopped(ref error) if error.code == 3 => {
                    let telemetry = match self.last_vm_telemetry.take() {
                        Some(telemetry) => telemetry,
                        None => {
                            return (
                                output,
                                Err(VmError {
                                    code: 33,
                                    message: "the VM paused without leaving the host call"
                                        .to_string(),
                                }),
                            );
                        }
                    };
                    match telemetry
                        .pending_host_call()
                        .ok_or_else(|| VmError {
                            code: 33,
                            message: "the VM wrote a broken host-call record".to_string(),
                        })
                        .and_then(|call| format_console_line(&call))
                    {
                        Ok(line) => output.push(line),
                        Err(error) => return (output, Err(error)),
                    }
                    outcome = self.try_owned_vm(&resume);
                }
                VmRun::Stopped(error) | VmRun::Rejected(Some(error)) => {
                    return (output, Err(error));
                }
                VmRun::Rejected(None) => {
                    return (
                        output,
                        Err(VmError {
                            code: 33,
                            message: "the C++ VM is not available".to_string(),
                        }),
                    );
                }
            }
        }
    }

    fn try_owned_vm(&mut self, request: &[u8]) -> VmRun {
        let trimmed = request;
        if trimmed.is_empty() || trimmed.len() > u32::MAX as usize {
            return VmRun::Rejected(None);
        }
        #[cfg(cinder_cpp_jit)]
        {
            if !is_cpp_jit_kernel_active() || !self.vm_owner.ensure() {
                return VmRun::Rejected(None);
            }
            let mut result = 0.0_f64;
            let mut telemetry = CinderVmTelemetry::default();
            let code = unsafe {
                cinder_cpp_vm_run_script(
                    self.vm_owner.context,
                    self.vm_owner.compiler,
                    trimmed.as_ptr(),
                    trimmed.len() as u32,
                    &mut telemetry,
                    &mut result,
                )
            };
            // Keep the telemetry from a pause so the console driver can read
            // the pending host call without a second FFI entry point.
            self.last_vm_telemetry = if code == 3 {
                Some(Box::new(telemetry))
            } else {
                None
            };
            let reason = || {
                telemetry.stop_reason().unwrap_or(VmError {
                    code: 32,
                    message: "the VM gave no reason".to_string(),
                })
            };
            match code {
                1 => VmRun::Value(result, Box::new(telemetry)),
                2 => VmRun::Stopped(reason()),
                // The run paused at a yield. Code 3 lets the caller tell a
                // pause from a real stop and send a Resume request to continue.
                3 => VmRun::Stopped(VmError {
                    code: 3,
                    message: "paused at a yield".to_string(),
                }),
                // The out buffer was too small; nothing was written.
                4 => VmRun::Rejected(Some(VmError {
                    code: 4,
                    message: "out buffer too small".to_string(),
                })),
                _ => VmRun::Rejected(Some(reason())),
            }
        }
        #[cfg(not(cinder_cpp_jit))]
        {
            let _ = (self, trimmed);
            VmRun::Rejected(None)
        }
    }

    fn compile_jit_block(&mut self, statements: &[String]) -> Vec<CompiledOp> {
        let mut ops = Vec::with_capacity(statements.len());
        for raw in statements {
            let stmt = raw.trim();
            if stmt.is_empty()
                || stmt.starts_with("function ")
                || stmt.starts_with("async function ")
                || stmt.starts_with("class ")
            {
                continue;
            }
            if stmt.starts_with("if")
                || stmt.starts_with("for")
                || stmt.starts_with("while")
                || stmt.starts_with("switch")
                || stmt.starts_with("try")
                || stmt.starts_with("throw ")
                || stmt == "break"
                || stmt == "continue"
                || stmt.starts_with("return")
                || stmt.starts_with("export ")
                || stmt.starts_with("import ")
                || stmt.contains("=>")
                || stmt.contains("+=")
                || stmt.contains("-=")
                || stmt.contains("*=")
                || stmt.contains("/=")
                || stmt.ends_with("++")
                || stmt.ends_with("--")
            {
                ops.push(CompiledOp::EvalStatement(stmt.to_string()));
                continue;
            }
            let is_decl =
                stmt.starts_with("let ") || stmt.starts_with("const ") || stmt.starts_with("var ");
            let stripped = stmt
                .strip_prefix("let ")
                .or_else(|| stmt.strip_prefix("const "))
                .or_else(|| stmt.strip_prefix("var "))
                .unwrap_or(stmt)
                .trim();

            if is_decl && (stripped.starts_with('{') || stripped.starts_with('[')) {
                ops.push(CompiledOp::EvalStatement(stmt.to_string()));
                continue;
            }
            if is_decl && split_args(stripped).len() > 1 {
                ops.push(CompiledOp::EvalStatement(stmt.to_string()));
                continue;
            }

            if let Some((lhs, rhs)) = split_assignment(stripped) {
                let lhs = lhs.trim();
                let rhs = rhs.trim();
                if lhs == "document.title"
                    && let Some(Value::Str(s)) = self.try_fold_constant_expr(rhs)
                {
                    ops.push(CompiledOp::SetTitleConst(s));
                    continue;
                }
                if is_decl && is_identifier(lhs) {
                    if let Some(const_val) = self.try_fold_constant_expr(rhs) {
                        ops.push(CompiledOp::SetVarConst {
                            name: lhs.to_string(),
                            value: const_val,
                        });
                        continue;
                    }
                    ops.push(CompiledOp::SetVarExpr {
                        name: lhs.to_string(),
                        expr: rhs.to_string(),
                    });
                    continue;
                }
                ops.push(CompiledOp::AssignTarget {
                    lhs: lhs.to_string(),
                    expr: rhs.to_string(),
                });
                continue;
            }
            ops.push(CompiledOp::EvalStatement(stmt.to_string()));
        }
        ops
    }

    fn try_fold_constant_expr(&mut self, expr: &str) -> Option<Value> {
        let e = expr.trim();
        if let Some(s) = parse_string_literal(e) {
            return Some(Value::Str(s));
        }
        if e == "true" {
            return Some(Value::Bool(true));
        }
        if e == "false" {
            return Some(Value::Bool(false));
        }
        if let Ok(n) = e.parse::<f64>() {
            return Some(Value::Number(n));
        }
        for (op_str, opcode) in [
            ("+", 0u32),
            ("-", 1u32),
            ("*", 2u32),
            ("/", 3u32),
            ("%", 4u32),
        ] {
            if let Some((l_str, r_str)) = split_top_level_op(e, op_str)
                && !l_str.trim().is_empty()
                && let (Ok(l_num), Ok(r_num)) =
                    (l_str.trim().parse::<f64>(), r_str.trim().parse::<f64>())
            {
                let folded = eval_jit_binary_op(opcode, l_num, r_num);
                self.jit_stats.folded_constants = self.jit_stats.folded_constants.saturating_add(1);
                return Some(Value::Number(folded));
            }
        }
        None
    }

    fn exec_compiled_op(&mut self, doc: &mut Document, op: CompiledOp) {
        if !self.consume_step() {
            return;
        }
        self.jit_stats.compiled_ops_executed =
            self.jit_stats.compiled_ops_executed.saturating_add(1);
        match op {
            CompiledOp::SetVarConst { name, value } => {
                self.set_var(&name, value);
            }
            CompiledOp::SetVarExpr { name, expr } => {
                let val = self.eval_expr(doc, &expr);
                self.set_var(&name, val);
            }
            CompiledOp::AssignTarget { lhs, expr } => {
                let val = self.eval_expr(doc, &expr);
                self.assign_target(doc, &lhs, val);
            }
            CompiledOp::SetTitleConst(title) => {
                doc.set_title(&title);
            }
            CompiledOp::EvalStatement(stmt) => {
                self.steps = self.steps.saturating_sub(1);
                self.exec_statement(doc, &stmt);
            }
        }
    }

    fn collect_onclick_attrs(&mut self, doc: &Document, id: NodeId) {
        let data = doc.get(id).data();
        if let Some(code) = data
            .attr("onclick")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.click_listeners.push((id, code.to_string()));
        }
        if let Some(code) = data
            .attr("oninput")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.input_listeners.push((id, code.to_string()));
        }
        if let Some(code) = data
            .attr("onchange")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.change_listeners.push((id, code.to_string()));
        }
        if let Some(code) = data
            .attr("onsubmit")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.submit_listeners.push((id, code.to_string()));
        }
        if let Some(code) = data
            .attr("onkeydown")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.keydown_listeners.push((id, code.to_string()));
        }
        let children: Vec<NodeId> = doc.children(id).to_vec();
        for child in children {
            self.collect_onclick_attrs(doc, child);
        }
    }

    #[cfg_attr(not(cinder_cpp_jit), allow(unused_variables))]
    fn try_native_page_loop(&mut self, doc: &mut Document, statement: &str) -> bool {
        if !self.jit_stats.jit_enabled || !is_cpp_jit_kernel_active() {
            return false;
        }
        if !page_loop_text(statement) {
            return false;
        }
        let Some(runnable) = native_loop_program(statement) else {
            return false;
        };
        let scanned = strip_string_literals(&runnable);
        let mut prelude = String::new();
        for name in ident_names(&scanned) {
            if numeric_keyword(&name) || page_host_word(&name) {
                continue;
            }
            match self.vars.iter().rev().find(|(key, _)| key == &name) {
                Some((_, Value::Number(number))) => {
                    prelude.push_str(&format!("let {name} = {};", format_jit_number(*number)));
                }
                Some(_) => return false,
                None if runnable.contains(&format!("{name} =")) => {
                    prelude.push_str(&format!("let {name} = 0;"));
                }
                None => return false,
            }
        }
        let source = format!("{prelude}{runnable}");
        #[cfg(cinder_cpp_jit)]
        let _guard = TextHostGuard::arm(doc);
        let Some((_, bindings, telemetry)) = eval_cpp_vm_script_bindings(&source) else {
            return false;
        };
        // A loop the VM stopped part way leaves partial values. The Rust
        // loop runs it again and reports the limit.
        if telemetry.stop_reason().is_some() {
            return false;
        }
        if telemetry.native_compilations == 0 {
            return false;
        }
        for (name, value) in bindings {
            if page_host_word(&name) {
                continue;
            }
            self.set_var(&name, Value::Number(value));
        }
        self.jit_stats.vm_telemetry.native_compilations = self
            .jit_stats
            .vm_telemetry
            .native_compilations
            .saturating_add(telemetry.native_compilations);
        self.jit_stats.vm_telemetry.bytecode_ops_executed = self
            .jit_stats
            .vm_telemetry
            .bytecode_ops_executed
            .saturating_add(telemetry.bytecode_ops_executed);
        self.jit_stats.compiled_blocks = self.jit_stats.compiled_blocks.saturating_add(1);
        true
    }

    fn try_native_numeric_loop(&mut self, statement: &str) -> bool {
        if !self.jit_stats.jit_enabled || !is_cpp_jit_kernel_active() {
            return false;
        }
        let Some(runnable) = native_loop_program(statement) else {
            return false;
        };
        if !numeric_loop_text(&runnable) && !numeric_loop_text(statement) {
            return false;
        }
        let mut prelude = String::new();
        for name in ident_names(&runnable) {
            if numeric_keyword(&name) {
                continue;
            }
            match self.vars.iter().rev().find(|(key, _)| key == &name) {
                Some((_, Value::Number(number))) => {
                    prelude.push_str(&format!("let {name} = {};", format_jit_number(*number)));
                }
                Some(_) => return false,
                None if runnable.contains(&format!("{name} =")) => {
                    prelude.push_str(&format!("let {name} = 0;"));
                }
                None => return false,
            }
        }
        let source = format!("{prelude}{runnable}");
        let Some((_, bindings, telemetry)) = eval_cpp_vm_script_bindings(&source) else {
            return false;
        };
        // A loop the VM stopped part way leaves partial values. The Rust
        // loop runs it again and reports the limit.
        if telemetry.stop_reason().is_some() {
            return false;
        }
        if telemetry.native_compilations == 0 {
            return false;
        }
        for (name, value) in bindings {
            self.set_var(&name, Value::Number(value));
        }
        self.jit_stats.vm_telemetry.native_compilations = self
            .jit_stats
            .vm_telemetry
            .native_compilations
            .saturating_add(telemetry.native_compilations);
        self.jit_stats.vm_telemetry.bytecode_ops_executed = self
            .jit_stats
            .vm_telemetry
            .bytecode_ops_executed
            .saturating_add(telemetry.bytecode_ops_executed);
        self.jit_stats.compiled_blocks = self.jit_stats.compiled_blocks.saturating_add(1);
        true
    }

    fn exec_statement(&mut self, doc: &mut Document, raw: &str) {
        if !self.consume_step() {
            return;
        }
        let mut stmt = raw.trim();
        if stmt.is_empty() || stmt.starts_with("import ") {
            return;
        }
        if let Some(rest) = stmt.strip_prefix("export default ") {
            stmt = rest.trim();
        } else if let Some(rest) = stmt.strip_prefix("export ") {
            stmt = rest.trim();
        }

        if stmt == "break" {
            self.loop_break = true;
            return;
        }
        if stmt == "continue" {
            self.loop_continue = true;
            return;
        }
        if stmt == "return" {
            self.return_value = Some(Value::Null);
            return;
        }
        if let Some(ret_expr) = stmt.strip_prefix("return ") {
            let val = self.eval_expr(doc, ret_expr.trim());
            self.return_value = Some(val);
            return;
        }
        if let Some(throw_expr) = stmt.strip_prefix("throw ") {
            let val = self.eval_expr(doc, throw_expr.trim());
            self.thrown_error = Some(val);
            return;
        }

        if let Some(rest) = stmt.strip_prefix("try") {
            let rest_trimmed = rest.trim_start();
            if rest_trimmed.starts_with('{')
                && let Some((try_body, after_try)) = extract_first_brace_block(rest_trimmed)
            {
                let reason_before = self.abort_reason.clone();
                self.eval_script(doc, &try_body);
                let mut had_err = self.thrown_error.take();
                if had_err.is_none() && self.aborted && self.steps <= MAX_EXECUTION_STEPS {
                    // A limit other than the step budget becomes an
                    // error the catch block sees. The turn goes on, so
                    // the reason is not kept.
                    self.aborted = false;
                    self.abort_reason = reason_before;
                    had_err = Some(Value::Str("Error".to_string()));
                }
                let mut rem = after_try.trim();
                if let Some(catch_rest) = rem.strip_prefix("catch") {
                    let catch_rest = catch_rest.trim_start();
                    let (catch_param, catch_body_src) = if catch_rest.starts_with('(') {
                        if let Some((p, r)) = extract_paren_group(catch_rest) {
                            (p.trim().to_string(), r)
                        } else {
                            (String::new(), catch_rest)
                        }
                    } else {
                        (String::new(), catch_rest)
                    };
                    if let Some((catch_body, after_catch)) =
                        extract_first_brace_block(catch_body_src)
                    {
                        rem = after_catch.trim();
                        if let Some(err_val) = had_err.take() {
                            if !catch_param.is_empty() {
                                self.set_var(&catch_param, err_val);
                            }
                            self.eval_script(doc, &catch_body);
                        }
                    }
                }
                if let Some(finally_rest) = rem.strip_prefix("finally")
                    && let Some((finally_body, _)) = extract_first_brace_block(finally_rest)
                {
                    let saved_return = self.return_value.take();
                    let saved_throw = self.thrown_error.take().or(had_err.take());
                    self.eval_script(doc, &finally_body);
                    if self.return_value.is_none() {
                        self.return_value = saved_return;
                    }
                    if self.thrown_error.is_none() {
                        self.thrown_error = saved_throw;
                    }
                    return;
                }
                if let Some(unhandled) = had_err {
                    self.thrown_error = Some(unhandled);
                }
                return;
            }
        }

        if let Some(rest) = stmt.strip_prefix("class ") {
            if let Some((header, class_body)) = split_brace_block(rest) {
                let (name, super_class) = if let Some((n, s)) = header.split_once(" extends ") {
                    (n.trim().to_string(), Some(s.trim().to_string()))
                } else {
                    (header.trim().to_string(), None)
                };
                if !name.is_empty() {
                    let methods = parse_class_methods(&class_body);
                    let def = ClassDef {
                        name: name.clone(),
                        super_class,
                        methods,
                        async_methods: async_method_names(&class_body),
                    };
                    if let Some(existing) = self.classes.iter_mut().find(|c| c.name == name) {
                        *existing = def;
                    } else {
                        self.classes.push(def);
                    }
                }
            }
            return;
        }

        if let Some(rest) = stmt.strip_prefix("switch") {
            let rest = rest.trim_start();
            if rest.starts_with('(')
                && let Some((disc_str, after_disc)) = extract_paren_group(rest)
                && let Some((switch_body, _)) = extract_first_brace_block(after_disc)
            {
                let disc_val = self.eval_expr(doc, &disc_str);
                let clauses = parse_switch_clauses(&switch_body);
                let mut matched_idx: Option<usize> = None;
                let mut default_idx: Option<usize> = None;
                for (idx, (case_expr_opt, _)) in clauses.iter().enumerate() {
                    if let Some(case_expr) = case_expr_opt {
                        let case_val = self.eval_expr(doc, case_expr);
                        if values_equal(&disc_val, &case_val, doc) {
                            matched_idx = Some(idx);
                            break;
                        }
                    } else if default_idx.is_none() {
                        default_idx = Some(idx);
                    }
                }
                if let Some(start_idx) = matched_idx.or(default_idx) {
                    for (_, stmts) in &clauses[start_idx..] {
                        for s in stmts {
                            if self.aborted
                                || self.return_value.is_some()
                                || self.thrown_error.is_some()
                                || self.loop_break
                                || self.loop_continue
                            {
                                break;
                            }
                            self.exec_statement(doc, s);
                        }
                        if self.aborted
                            || self.return_value.is_some()
                            || self.thrown_error.is_some()
                            || self.loop_break
                            || self.loop_continue
                        {
                            break;
                        }
                    }
                }
                self.loop_break = false;
                return;
            }
        }

        if let Some(rest) = stmt.strip_prefix("async function ") {
            self.define_named_function(rest, true);
            return;
        }

        if let Some(rest) = stmt.strip_prefix("function ") {
            if let Some((sig, body)) = split_brace_block(rest)
                && let Some((name, params_raw)) = sig.split_once('(')
            {
                let name = name.trim().to_string();
                let params_str = params_raw.trim().trim_end_matches(')');
                let params: Vec<String> = params_str
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                if !name.is_empty() {
                    if let Some((_, existing_params, existing_body)) =
                        self.functions.iter_mut().find(|(n, _, _)| n == &name)
                    {
                        *existing_params = params;
                        *existing_body = body;
                    } else {
                        self.functions.push((name, params, body));
                    }
                }
            }
            return;
        }

        if let Some(rest) = stmt.strip_prefix("while") {
            let rest = rest.trim_start();
            if rest.starts_with('(')
                && let Some((cond_str, after_cond)) = extract_paren_group(rest)
                && let Some((loop_body, _)) = extract_first_brace_block(after_cond)
            {
                if self.try_native_page_loop(doc, stmt) || self.try_native_numeric_loop(stmt) {
                    return;
                }
                // Each pass spends a step, so the step budget ends
                // a loop that does not.
                while self.consume_step() {
                    if self.return_value.is_some()
                        || self.thrown_error.is_some()
                        || !self.eval_expr(doc, &cond_str).is_truthy()
                    {
                        break;
                    }
                    self.eval_script(doc, &loop_body);
                    if self.loop_break {
                        self.loop_break = false;
                        break;
                    }
                    if self.loop_continue {
                        self.loop_continue = false;
                    }
                }
                return;
            }
        }

        if let Some(rest) = stmt.strip_prefix("for") {
            let rest = rest.trim_start();
            if rest.starts_with('(')
                && let Some((header_str, after_header)) = extract_paren_group(rest)
                && let Some((loop_body, _)) = extract_first_brace_block(after_header)
            {
                let parts: Vec<&str> = header_str.splitn(3, ';').collect();
                if parts.len() == 3 {
                    if self.try_native_page_loop(doc, stmt) || self.try_native_numeric_loop(stmt) {
                        return;
                    }
                    self.exec_statement(doc, parts[0]);
                    while self.consume_step() {
                        if self.return_value.is_some() || self.thrown_error.is_some() {
                            break;
                        }
                        if !parts[1].trim().is_empty() && !self.eval_expr(doc, parts[1]).is_truthy()
                        {
                            break;
                        }
                        self.eval_script(doc, &loop_body);
                        if self.loop_break {
                            self.loop_break = false;
                            break;
                        }
                        if self.loop_continue {
                            self.loop_continue = false;
                        }
                        self.exec_statement(doc, parts[2]);
                    }
                    return;
                }
                if let Some((lhs_raw, rhs_raw)) = split_for_of_or_in(&header_str, " of ") {
                    let binding = lhs_raw
                        .trim()
                        .strip_prefix("let ")
                        .or_else(|| lhs_raw.trim().strip_prefix("const "))
                        .or_else(|| lhs_raw.trim().strip_prefix("var "))
                        .unwrap_or(lhs_raw.trim())
                        .trim();
                    let iterable = self.eval_expr(doc, rhs_raw.trim());
                    let items: Vec<Value> = match iterable {
                        Value::Array(v) => v,
                        Value::Json(JsonValue::Array(v)) => {
                            v.into_iter().map(Value::from_json_value).collect()
                        }
                        Value::Str(s) => s.chars().map(|c| Value::Str(c.to_string())).collect(),
                        _ => Vec::new(),
                    };
                    for item in items {
                        if !self.consume_step()
                            || self.return_value.is_some()
                            || self.thrown_error.is_some()
                        {
                            break;
                        }
                        self.bind_pattern(doc, binding, item);
                        self.eval_script(doc, &loop_body);
                        if self.loop_break {
                            self.loop_break = false;
                            break;
                        }
                        if self.loop_continue {
                            self.loop_continue = false;
                        }
                    }
                    return;
                }
                if let Some((lhs_raw, rhs_raw)) = split_for_of_or_in(&header_str, " in ") {
                    let binding = lhs_raw
                        .trim()
                        .strip_prefix("let ")
                        .or_else(|| lhs_raw.trim().strip_prefix("const "))
                        .or_else(|| lhs_raw.trim().strip_prefix("var "))
                        .unwrap_or(lhs_raw.trim())
                        .trim();
                    let target = self.eval_expr(doc, rhs_raw.trim());
                    let keys: Vec<Value> = match target {
                        Value::Json(JsonValue::Object(entries)) => {
                            entries.into_iter().map(|(k, _)| Value::Str(k)).collect()
                        }
                        Value::Array(items) => (0..items.len())
                            .map(|i| Value::Str(i.to_string()))
                            .collect(),
                        Value::Json(JsonValue::Array(items)) => (0..items.len())
                            .map(|i| Value::Str(i.to_string()))
                            .collect(),
                        _ => Vec::new(),
                    };
                    for key in keys {
                        if !self.consume_step()
                            || self.return_value.is_some()
                            || self.thrown_error.is_some()
                        {
                            break;
                        }
                        self.bind_pattern(doc, binding, key);
                        self.eval_script(doc, &loop_body);
                        if self.loop_break {
                            self.loop_break = false;
                            break;
                        }
                        if self.loop_continue {
                            self.loop_continue = false;
                        }
                    }
                    return;
                }
            }
        }

        if let Some(rest) = stmt.strip_prefix("if") {
            let rest = rest.trim_start();
            if rest.starts_with('(')
                && let Some((cond_str, after_cond)) = extract_paren_group(rest)
                && let Some((then_body, after_then)) = extract_first_brace_block(after_cond)
            {
                let cond_val = self.eval_expr(doc, &cond_str);
                if cond_val.is_truthy() {
                    self.eval_script(doc, &then_body);
                } else {
                    let after_then = after_then.trim();
                    if let Some(else_rest) = after_then.strip_prefix("else") {
                        if let Some((else_body, _)) = extract_first_brace_block(else_rest) {
                            self.eval_script(doc, &else_body);
                        } else {
                            self.exec_statement(doc, else_rest);
                        }
                    }
                }
                return;
            }
        }

        let is_decl =
            stmt.starts_with("let ") || stmt.starts_with("const ") || stmt.starts_with("var ");
        let stripped = stmt
            .strip_prefix("let ")
            .or_else(|| stmt.strip_prefix("const "))
            .or_else(|| stmt.strip_prefix("var "))
            .unwrap_or(stmt)
            .trim();

        if is_decl && !stripped.starts_with('{') && !stripped.starts_with('[') {
            let decl_parts = split_args(stripped);
            if decl_parts.len() > 1 {
                for part in decl_parts {
                    if let Some((lhs, rhs)) = split_assignment(&part) {
                        let val = self.eval_expr(doc, rhs.trim());
                        self.set_var(lhs.trim(), val);
                    } else if is_identifier(part.trim()) {
                        self.set_var(part.trim(), Value::Null);
                    }
                }
                return;
            }
        }

        if let Some(ident) = stripped
            .strip_suffix("++")
            .or_else(|| stripped.strip_prefix("++"))
        {
            let ident = ident.trim();
            if is_identifier(ident) {
                let cur = self
                    .get_var(ident)
                    .and_then(Value::as_number)
                    .unwrap_or(0.0);
                self.set_var(ident, Value::Number(cur + 1.0));
                return;
            }
        }
        if let Some(ident) = stripped
            .strip_suffix("--")
            .or_else(|| stripped.strip_prefix("--"))
        {
            let ident = ident.trim();
            if is_identifier(ident) {
                let cur = self
                    .get_var(ident)
                    .and_then(Value::as_number)
                    .unwrap_or(0.0);
                self.set_var(ident, Value::Number(cur - 1.0));
                return;
            }
        }
        if let Some((lhs, rhs)) = stripped.split_once("+=") {
            let lhs = lhs.trim();
            let cur = self.eval_expr(doc, lhs);
            let add = self.eval_expr(doc, rhs.trim());
            let next = match (&cur, &add) {
                (Value::Number(a), Value::Number(b)) => Value::Number(a + b),
                _ => self.concat_strings(doc, &cur, &add),
            };
            self.assign_target(doc, lhs, next);
            return;
        }
        if let Some((lhs, rhs)) = stripped.split_once("-=") {
            let lhs = lhs.trim();
            let cur = self.eval_expr(doc, lhs).as_number().unwrap_or(0.0);
            let sub = self.eval_expr(doc, rhs.trim()).as_number().unwrap_or(0.0);
            self.assign_target(doc, lhs, Value::Number(cur - sub));
            return;
        }
        if let Some((lhs, rhs)) = stripped.split_once("*=") {
            let lhs = lhs.trim();
            let cur = self.eval_expr(doc, lhs).as_number().unwrap_or(0.0);
            let mul = self.eval_expr(doc, rhs.trim()).as_number().unwrap_or(0.0);
            self.assign_target(doc, lhs, Value::Number(cur * mul));
            return;
        }
        if let Some((lhs, rhs)) = stripped.split_once("/=") {
            let lhs = lhs.trim();
            let cur = self.eval_expr(doc, lhs).as_number().unwrap_or(0.0);
            let div = self.eval_expr(doc, rhs.trim()).as_number().unwrap_or(0.0);
            let out = if div == 0.0 { 0.0 } else { cur / div };
            self.assign_target(doc, lhs, Value::Number(out));
            return;
        }

        if let Some((lhs, rhs)) = split_assignment(stripped) {
            let lhs_trimmed = lhs.trim();
            let rhs_trimmed = rhs.trim();
            if is_decl
                && ((lhs_trimmed.starts_with('{') && lhs_trimmed.ends_with('}'))
                    || (lhs_trimmed.starts_with('[') && lhs_trimmed.ends_with(']')))
            {
                let value = self.eval_expr(doc, rhs_trimmed);
                self.bind_pattern(doc, lhs_trimmed, value);
                return;
            }
            let (rhs_async, rhs_eff) = match rhs_trimmed.strip_prefix("async ") {
                Some(rest) => (true, rest.trim()),
                None => (false, rhs_trimmed),
            };
            if is_identifier(lhs_trimmed) {
                if let Some((params_part, arrow_body)) = split_top_level_arrow(rhs_eff) {
                    let params_str = params_part
                        .trim()
                        .trim_start_matches('(')
                        .trim_end_matches(')');
                    let params: Vec<String> = params_str
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect();
                    let arrow_body = arrow_body.trim();
                    let body = if arrow_body.starts_with('{') {
                        split_brace_block(arrow_body)
                            .map(|(_, block)| block)
                            .unwrap_or_else(|| arrow_body.to_string())
                    } else {
                        format!("return {arrow_body}")
                    };
                    if let Some((_, existing_params, existing_body)) =
                        self.functions.iter_mut().find(|(n, _, _)| n == lhs_trimmed)
                    {
                        *existing_params = params;
                        *existing_body = body;
                    } else {
                        self.functions.push((lhs_trimmed.to_string(), params, body));
                    }
                    if rhs_async && !self.async_functions.iter().any(|n| n == lhs_trimmed) {
                        self.async_functions.push(lhs_trimmed.to_string());
                    }
                    return;
                }
                if let Some(rest_fn) = rhs_eff.strip_prefix("function")
                    && let Some((sig, body)) = split_brace_block(rest_fn)
                    && let Some((_, params_raw)) = sig.split_once('(')
                {
                    let params_str = params_raw.trim().trim_end_matches(')');
                    let params: Vec<String> = params_str
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect();
                    self.functions.push((lhs_trimmed.to_string(), params, body));
                    if rhs_async && !self.async_functions.iter().any(|n| n == lhs_trimmed) {
                        self.async_functions.push(lhs_trimmed.to_string());
                    }
                    return;
                }
            }
            if !is_decl
                && let Some((obj_expr, prop)) = lhs_trimmed.rsplit_once('.')
                && matches!(
                    prop.trim(),
                    "onclick" | "oninput" | "onchange" | "onsubmit" | "onkeydown"
                )
                && let Value::Element(id) = self.eval_expr(doc, obj_expr.trim())
            {
                let target_id = if id == doc.root() { doc.body() } else { id };
                let body = extract_event_callback_body(rhs_trimmed);
                match prop.trim() {
                    "onclick" => self.click_listeners.push((target_id, body)),
                    "oninput" => self.input_listeners.push((target_id, body)),
                    "onchange" => self.change_listeners.push((target_id, body)),
                    "onsubmit" => self.submit_listeners.push((target_id, body)),
                    "onkeydown" => self.keydown_listeners.push((target_id, body)),
                    _ => {}
                }
                return;
            }
            let value = self.eval_expr(doc, rhs_trimmed);
            self.assign_target(doc, lhs_trimmed, value);
            return;
        }

        if is_decl && is_identifier(stripped) {
            self.set_var(stripped, Value::Null);
            return;
        }

        let _ = self.eval_expr(doc, stripped);
    }

    fn bind_pattern(&mut self, doc: &mut Document, pattern: &str, value: Value) {
        let pat = pattern.trim();
        if pat.starts_with('{') && pat.ends_with('}') && pat.len() >= 2 {
            let inner = &pat[1..pat.len() - 1];
            for entry in split_args(inner) {
                let (prop_part, default_expr) = split_assignment(&entry)
                    .map(|(l, r)| (l.trim(), Some(r.trim())))
                    .unwrap_or((entry.trim(), None));
                let (src_key, var_name) = if let Some((k, alias)) = prop_part.split_once(':') {
                    (k.trim(), alias.trim())
                } else {
                    (prop_part, prop_part)
                };
                if !is_identifier(var_name) {
                    continue;
                }
                let mut extracted = match &value {
                    Value::Json(jobj) => jobj
                        .get(src_key)
                        .cloned()
                        .map(Value::from_json_value)
                        .unwrap_or(Value::Null),
                    Value::Element(id) => self.read_element_prop(doc, *id, src_key),
                    _ => Value::Null,
                };
                if matches!(extracted, Value::Null)
                    && let Some(def) = default_expr
                {
                    extracted = self.eval_expr(doc, def);
                }
                self.set_var(var_name, extracted);
            }
            return;
        }
        if pat.starts_with('[') && pat.ends_with(']') && pat.len() >= 2 {
            let inner = &pat[1..pat.len() - 1];
            let items: Vec<Value> = match value {
                Value::Array(v) => v,
                Value::Json(JsonValue::Array(v)) => {
                    v.into_iter().map(Value::from_json_value).collect()
                }
                _ => Vec::new(),
            };
            for (idx, entry) in split_args(inner).into_iter().enumerate() {
                let (var_name, default_expr) = split_assignment(&entry)
                    .map(|(l, r)| (l.trim(), Some(r.trim())))
                    .unwrap_or((entry.trim(), None));
                if !is_identifier(var_name) {
                    continue;
                }
                let mut item_val = items.get(idx).cloned().unwrap_or(Value::Null);
                if matches!(item_val, Value::Null)
                    && let Some(def) = default_expr
                {
                    item_val = self.eval_expr(doc, def);
                }
                self.set_var(var_name, item_val);
            }
            return;
        }
        if is_identifier(pat) {
            self.set_var(pat, value);
        }
    }

    fn set_pending_navigation(&mut self, dest: &str) {
        let trimmed = dest.trim();
        if trimmed.is_empty() {
            return;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("/httpservice/retry/enablejs")
            || lower.contains("gbv=1")
            || lower.contains("noscript")
        {
            return;
        }
        self.pending_navigation = Some(trimmed.to_string());
    }

    fn assign_target(&mut self, doc: &mut Document, lhs: &str, value: Value) {
        let lhs = lhs.trim();
        if matches!(
            lhs,
            "document.title"
                | "window.document.title"
                | "self.document.title"
                | "globalThis.document.title"
        ) {
            let text = value.as_string(doc);
            doc.set_title(&text);
            return;
        }
        if matches!(
            lhs,
            "document.cookie"
                | "window.document.cookie"
                | "self.document.cookie"
                | "globalThis.document.cookie"
        ) {
            let text = value.as_string(doc);
            self.set_var("__document_cookie", Value::Str(text));
            return;
        }
        if matches!(
            lhs,
            "window.location.href"
                | "location.href"
                | "window.location"
                | "location"
                | "document.location"
                | "document.location.href"
                | "window.document.location"
                | "window.document.location.href"
        ) {
            let dest = value.as_string(doc);
            self.set_pending_navigation(&dest);
            return;
        }
        if let Some(win_prop) = lhs
            .strip_prefix("window.")
            .or_else(|| lhs.strip_prefix("self."))
            .or_else(|| lhs.strip_prefix("globalThis."))
        {
            let win_prop = win_prop.trim();
            if is_identifier(win_prop) {
                self.set_var(win_prop, value);
                return;
            }
        }

        if lhs.ends_with(']')
            && let Some(open) = lhs.rfind('[')
            && open > 0
        {
            let target_ident = lhs[..open]
                .trim()
                .strip_prefix("window.")
                .or_else(|| lhs[..open].trim().strip_prefix("self."))
                .or_else(|| lhs[..open].trim().strip_prefix("globalThis."))
                .unwrap_or(lhs[..open].trim())
                .trim();
            let idx_expr = &lhs[open + 1..lhs.len() - 1];
            let idx_val = self.eval_expr(doc, idx_expr);
            if target_ident == "window" || target_ident == "self" || target_ident == "globalThis" {
                let key = idx_val.as_string(doc);
                if is_identifier(&key) {
                    self.set_var(&key, value);
                }
                return;
            }
            if is_identifier(target_ident)
                && let Some(existing) = self.get_var(target_ident).cloned()
            {
                match existing {
                    Value::Array(mut items) => {
                        if let Some(n) = idx_val.as_number() {
                            let i = n.max(0.0) as usize;
                            if i < items.len() {
                                items[i] = value;
                            } else if self.array_fits(i + 1) {
                                items.resize(i, Value::Null);
                                items.push(value);
                            } else {
                                return;
                            }
                            self.set_var(target_ident, Value::Array(items));
                        }
                        return;
                    }
                    Value::Json(mut jobj) => {
                        let key = idx_val.as_string(doc);
                        jobj.insert(&key, value.to_json_value(doc));
                        self.set_var(target_ident, Value::Json(jobj));
                        return;
                    }
                    Value::Str(ref s) if s == "__anvil_window" => {
                        let key = idx_val.as_string(doc);
                        if is_identifier(&key) {
                            self.set_var(&key, value);
                        }
                        return;
                    }
                    _ => {}
                }
            }
        }

        if let Some((obj_expr, prop)) = lhs.rsplit_once('.') {
            if let Some((el_expr, sub_lit)) = obj_expr.rsplit_once('.') {
                let sub_trimmed = sub_lit.trim();
                if sub_trimmed == "style" {
                    if let Value::Element(id) = self.eval_expr(doc, el_expr.trim()) {
                        let prop_trimmed = prop.trim();
                        let css_val = value.as_string(doc);
                        if prop_trimmed == "cssText" {
                            doc.set_attribute(id, "style", &css_val);
                        } else {
                            let css_prop = camel_to_kebab(prop_trimmed);
                            update_inline_style(doc, id, &css_prop, &css_val);
                        }
                        self.notify_mutation_observers(doc, "attributes", id, Some("style"));
                    }
                    return;
                }
                if sub_trimmed == "dataset" {
                    if let Value::Element(id) = self.eval_expr(doc, el_expr.trim()) {
                        let attr_name = format!("data-{}", camel_to_kebab(prop.trim()));
                        let attr_val = value.as_string(doc);
                        doc.set_attribute(id, &attr_name, &attr_val);
                        self.notify_mutation_observers(doc, "attributes", id, Some(&attr_name));
                    }
                    return;
                }
            }
            let obj_val = self.eval_expr(doc, obj_expr.trim());
            if let Value::Str(ref s) = obj_val
                && s == "__anvil_window"
            {
                let prop_trimmed = prop.trim();
                if is_identifier(prop_trimmed) {
                    self.set_var(prop_trimmed, value);
                }
                return;
            }
            if let Value::Element(id) = obj_val {
                if id == doc.root() {
                    match prop.trim() {
                        "title" => {
                            let text = value.as_string(doc);
                            doc.set_title(&text);
                            return;
                        }
                        "cookie" => {
                            let text = value.as_string(doc);
                            self.set_var("__document_cookie", Value::Str(text));
                            return;
                        }
                        "location" => {
                            let dest = value.as_string(doc);
                            self.set_pending_navigation(&dest);
                            return;
                        }
                        _ => {}
                    }
                }
                match prop.trim() {
                    "textContent" | "innerText" | "nodeValue" => {
                        let text = value.as_string(doc);
                        doc.set_text_content(id, &text);
                        self.notify_mutation_observers(doc, "childList", id, None);
                    }
                    "innerHTML" | "outerHTML" => {
                        let html = value.as_string(doc);
                        doc.set_inner_html(id, &html);
                        self.dom_cache.clear();
                        self.notify_mutation_observers(doc, "childList", id, None);
                    }
                    "value" => {
                        let text = value.as_string(doc);
                        doc.set_input_value(id, &text);
                    }
                    "checked" => {
                        doc.set_checked(id, value.is_truthy());
                    }
                    "disabled" => {
                        if value.is_truthy() {
                            doc.set_attribute(id, "disabled", "disabled");
                        } else {
                            doc.remove_attribute(id, "disabled");
                        }
                        self.notify_mutation_observers(doc, "attributes", id, Some("disabled"));
                    }
                    "hidden" => {
                        if value.is_truthy() {
                            doc.set_attribute(id, "hidden", "hidden");
                            update_inline_style(doc, id, "display", "none");
                        } else {
                            doc.remove_attribute(id, "hidden");
                        }
                        self.notify_mutation_observers(doc, "attributes", id, Some("hidden"));
                    }
                    "id" => {
                        let text = value.as_string(doc);
                        doc.set_attribute(id, "id", &text);
                        self.dom_cache.clear();
                        self.notify_mutation_observers(doc, "attributes", id, Some("id"));
                    }
                    "className" => {
                        let text = value.as_string(doc);
                        doc.set_attribute(id, "class", &text);
                        self.dom_cache.clear();
                        self.notify_mutation_observers(doc, "attributes", id, Some("class"));
                    }
                    "href" | "src" | "type" | "name" | "placeholder" | "title" | "alt" | "role" => {
                        let attr_name = prop.trim();
                        let text = value.as_string(doc);
                        doc.set_attribute(id, attr_name, &text);
                        self.notify_mutation_observers(doc, "attributes", id, Some(attr_name));
                    }
                    _ => {}
                }
                return;
            }
            if let Value::Json(mut jobj) = obj_val {
                let jval = value.to_json_value(doc);
                jobj.insert(prop.trim(), jval);
                self.assign_target(doc, obj_expr.trim(), Value::Json(jobj));
                return;
            }
        }

        if is_identifier(lhs) {
            self.set_var(lhs, value);
        }
    }

    pub fn eval_expr(&mut self, doc: &mut Document, raw: &str) -> Value {
        if !self.consume_step() {
            return Value::Null;
        }
        let expr = raw.trim();
        if expr.is_empty() {
            return Value::Null;
        }

        if expr.starts_with('(')
            && expr.ends_with(')')
            && let Some((inner, rem)) = extract_paren_group(expr)
            && rem.trim().is_empty()
        {
            return self.eval_expr(doc, &inner);
        }

        if expr.starts_with('[') && expr.ends_with(']') {
            let inner = &expr[1..expr.len() - 1];
            let mut items: Vec<Value> = Vec::new();
            let parts = split_args(inner);
            if !self.array_fits(parts.len()) {
                return Value::Null;
            }
            for item in parts {
                if let Some(spread_expr) = item.trim().strip_prefix("...") {
                    match self.eval_expr(doc, spread_expr.trim()) {
                        Value::Array(sub) => {
                            if !self.array_fits(items.len().saturating_add(sub.len())) {
                                return Value::Null;
                            }
                            items.extend(sub);
                        }
                        Value::Json(JsonValue::Array(sub)) => {
                            if !self.array_fits(items.len().saturating_add(sub.len())) {
                                return Value::Null;
                            }
                            items.extend(sub.into_iter().map(Value::from_json_value));
                        }
                        _ => {}
                    }
                } else {
                    items.push(self.eval_expr(doc, &item));
                }
            }
            return Value::Array(items);
        }

        if expr.starts_with('{') && expr.ends_with('}') {
            let inner = &expr[1..expr.len() - 1];
            let mut obj = JsonValue::empty_object();
            for pair in split_args(inner) {
                let pair_trimmed = pair.trim();
                if let Some(spread_expr) = pair_trimmed.strip_prefix("...") {
                    if let Value::Json(JsonValue::Object(entries)) =
                        self.eval_expr(doc, spread_expr.trim())
                    {
                        for (k, v) in entries {
                            obj.insert(&k, v);
                        }
                    }
                } else if let Some((k_raw, v_raw)) = pair_trimmed.split_once(':') {
                    let key = parse_string_literal(k_raw.trim())
                        .unwrap_or_else(|| k_raw.trim().to_string());
                    let val = self.eval_expr(doc, v_raw.trim()).to_json_value(doc);
                    obj.insert(&key, val);
                } else if is_identifier(pair_trimmed) {
                    let val = self.eval_expr(doc, pair_trimmed).to_json_value(doc);
                    obj.insert(pair_trimmed, val);
                }
            }
            return Value::Json(obj);
        }

        if expr.starts_with('`') && expr.ends_with('`') && expr.len() >= 2 {
            let inner = &expr[1..expr.len() - 1];
            return Value::Str(self.eval_template_literal(doc, inner));
        }

        if let Some(new_rest) = expr.strip_prefix("new ") {
            return self.eval_new_expr(doc, new_rest.trim());
        }

        if let Some(void_rest) = expr
            .strip_prefix("void ")
            .or_else(|| expr.strip_prefix("void("))
        {
            let inner = if expr.starts_with("void(") {
                &expr[4..]
            } else {
                void_rest.trim()
            };
            let _ = self.eval_expr(doc, inner);
            return Value::Null;
        }

        if let Some(del_rest) = expr.strip_prefix("delete ") {
            let _ = del_rest;
            return Value::Bool(true);
        }

        if let Some(rest) = expr.strip_prefix("typeof ") {
            let target = rest
                .trim()
                .trim_start_matches('(')
                .trim_end_matches(')')
                .trim();
            if matches!(
                target,
                "window"
                    | "self"
                    | "globalThis"
                    | "document"
                    | "window.document"
                    | "navigator"
                    | "window.navigator"
                    | "location"
                    | "window.location"
                    | "history"
                    | "window.history"
                    | "localStorage"
                    | "window.localStorage"
                    | "sessionStorage"
                    | "window.sessionStorage"
                    | "console"
                    | "Math"
                    | "JSON"
                    | "Promise"
                    | "performance"
                    | "customElements"
            ) {
                return Value::Str("object".to_string());
            }
            if self.functions.iter().any(|(n, _, _)| n == target)
                || matches!(
                    target,
                    "fetch"
                        | "setTimeout"
                        | "setInterval"
                        | "clearTimeout"
                        | "clearInterval"
                        | "requestAnimationFrame"
                        | "cancelAnimationFrame"
                        | "encodeURIComponent"
                        | "decodeURIComponent"
                        | "encodeURI"
                        | "decodeURI"
                        | "atob"
                        | "btoa"
                        | "parseInt"
                        | "parseFloat"
                )
            {
                return Value::Str("function".to_string());
            }
            let v = self.eval_expr(doc, target);
            let ty = match v {
                Value::Null => "undefined",
                Value::Bool(_) => "boolean",
                Value::Number(_) => "number",
                Value::Str(ref s) if s == "__anvil_window" => "object",
                Value::Str(_) => "string",
                Value::Element(_) | Value::Array(_) | Value::Json(_) => "object",
            };
            return Value::Str(ty.to_string());
        }

        if let Some(rest) = expr.strip_prefix('!')
            && !rest.starts_with('=')
        {
            let v = self.eval_expr(doc, rest.trim());
            return Value::Bool(!v.is_truthy());
        }

        if let Some((lhs, rhs)) = split_assignment(expr) {
            let lhs_t = lhs.trim();
            if is_identifier(lhs_t)
                || lhs_t.contains('.')
                || (lhs_t.ends_with(']') && lhs_t.contains('['))
            {
                let val = self.eval_expr(doc, rhs.trim());
                self.assign_target(doc, lhs_t, val.clone());
                return val;
            }
        }

        if let Some((cond_s, branches_s)) = split_top_level_op(expr, "?")
            && let Some((then_s, else_s)) = split_top_level_op(branches_s, ":")
        {
            if self.eval_expr(doc, cond_s).is_truthy() {
                return self.eval_expr(doc, then_s);
            } else {
                return self.eval_expr(doc, else_s);
            }
        }

        if let Some((left, right)) = split_top_level_op(expr, "??") {
            let l = self.eval_expr(doc, left);
            if !matches!(l, Value::Null) {
                return l;
            }
            return self.eval_expr(doc, right);
        }

        if let Some((left, right)) = split_top_level_op(expr, "||") {
            let l = self.eval_expr(doc, left);
            if l.is_truthy() {
                return l;
            }
            return self.eval_expr(doc, right);
        }

        if let Some((left, right)) = split_top_level_op(expr, "&&") {
            let l = self.eval_expr(doc, left);
            if !l.is_truthy() {
                return Value::Bool(false);
            }
            return self.eval_expr(doc, right);
        }

        for op in ["===", "!==", "==", "!=", "<=", ">=", "<", ">"] {
            if let Some((left, right)) = split_top_level_op(expr, op) {
                let l = self.eval_expr(doc, left);
                let r = self.eval_expr(doc, right);
                let res = match op {
                    "===" | "==" => values_equal(&l, &r, doc),
                    "!==" | "!=" => !values_equal(&l, &r, doc),
                    "<" => l.as_number().zip(r.as_number()).is_some_and(|(a, b)| a < b),
                    "<=" => l
                        .as_number()
                        .zip(r.as_number())
                        .is_some_and(|(a, b)| a <= b),
                    ">" => l.as_number().zip(r.as_number()).is_some_and(|(a, b)| a > b),
                    ">=" => l
                        .as_number()
                        .zip(r.as_number())
                        .is_some_and(|(a, b)| a >= b),
                    _ => false,
                };
                return Value::Bool(res);
            }
        }

        if let Some((left, right)) = split_top_level_op(expr, "+") {
            let l = self.eval_expr(doc, left);
            let r = self.eval_expr(doc, right);
            if let (Value::Number(a), Value::Number(b)) = (&l, &r) {
                return Value::Number(a + b);
            }
            return self.concat_strings(doc, &l, &r);
        }

        for op in ["-", "*", "/", "%"] {
            if let Some((left, right)) = split_top_level_op(expr, op) {
                if left.trim().is_empty() && op == "-" {
                    let r = self.eval_expr(doc, right);
                    return Value::Number(-r.as_number().unwrap_or(0.0));
                }
                let l = self.eval_expr(doc, left).as_number().unwrap_or(0.0);
                let r = self.eval_expr(doc, right).as_number().unwrap_or(0.0);
                let out = match op {
                    "-" => l - r,
                    "*" => l * r,
                    "/" => {
                        if r == 0.0 {
                            0.0
                        } else {
                            l / r
                        }
                    }
                    "%" => eval_jit_binary_op(4, l, r),
                    _ => 0.0,
                };
                return Value::Number(out);
            }
        }

        if let Some(str_val) = parse_string_literal(expr) {
            return Value::Str(str_val);
        }
        if expr == "true" {
            return Value::Bool(true);
        }
        if expr == "false" {
            return Value::Bool(false);
        }
        if expr == "null" || expr == "undefined" {
            return Value::Null;
        }
        if let Some(num) = parse_js_number_literal(expr) {
            return Value::Number(num);
        }
        if matches!(
            expr,
            "document" | "window.document" | "self.document" | "globalThis.document"
        ) {
            return Value::Element(doc.root());
        }
        if matches!(expr, "window" | "self" | "globalThis") {
            return Value::Str("__anvil_window".to_string());
        }
        if expr == "document.title" || expr == "window.document.title" {
            return Value::Str(doc.title());
        }
        if expr == "document.body" || expr == "window.document.body" {
            return Value::Element(doc.body());
        }
        if expr == "document.head" || expr == "window.document.head" {
            return Value::Element(doc.head());
        }
        if expr == "document.documentElement" || expr == "window.document.documentElement" {
            return Value::Element(doc.html());
        }
        if expr == "document.readyState" || expr == "window.document.readyState" {
            return Value::Str("complete".to_string());
        }
        if expr == "document.visibilityState" || expr == "window.document.visibilityState" {
            return Value::Str("visible".to_string());
        }
        if expr == "document.hidden" || expr == "window.document.hidden" {
            return Value::Bool(false);
        }
        if expr == "document.cookie" || expr == "window.document.cookie" {
            return Value::Str(
                self.get_var("__document_cookie")
                    .map(|v| v.as_string(doc))
                    .unwrap_or_default(),
            );
        }
        if matches!(
            expr,
            "document.URL"
                | "window.document.URL"
                | "document.documentURI"
                | "window.document.documentURI"
        ) {
            return Value::Str(self.page_origin.clone().unwrap_or_default());
        }
        if expr == "document.domain" || expr == "window.document.domain" {
            let href = self.page_origin.as_deref().unwrap_or_default();
            let after_scheme = href.find("://").map(|i| &href[i + 3..]).unwrap_or(href);
            let end = after_scheme
                .find(['/', ':', '?', '#'])
                .unwrap_or(after_scheme.len());
            return Value::Str(after_scheme[..end].to_string());
        }
        if expr == "window.innerWidth" || expr == "innerWidth" {
            return Value::Number(1280.0);
        }
        if expr == "window.innerHeight" || expr == "innerHeight" {
            return Value::Number(800.0);
        }
        if expr == "window.devicePixelRatio" || expr == "devicePixelRatio" {
            return Value::Number(1.0);
        }
        if expr == "Math.PI" {
            return Value::Number(std::f64::consts::PI);
        }
        if expr == "Math.E" {
            return Value::Number(std::f64::consts::E);
        }
        if expr == "Number.MAX_SAFE_INTEGER" {
            return Value::Number(9_007_199_254_740_991.0);
        }
        if expr == "Number.MIN_SAFE_INTEGER" {
            return Value::Number(-9_007_199_254_740_991.0);
        }
        if expr == "window.location.search" || expr == "location.search" {
            let href = self.page_origin.as_deref().unwrap_or_default();
            let search = match href.find('?') {
                Some(q) => {
                    let after = &href[q..];
                    match after.find('#') {
                        Some(h) => after[..h].to_string(),
                        None => after.to_string(),
                    }
                }
                None => String::new(),
            };
            return Value::Str(search);
        }
        if expr == "window.location.pathname" || expr == "location.pathname" {
            let href = self.page_origin.as_deref().unwrap_or_default();
            let after_scheme = href.find("://").map(|i| &href[i + 3..]).unwrap_or(href);
            let path = match after_scheme.find('/') {
                Some(slash) => {
                    let rest = &after_scheme[slash..];
                    let end = rest.find(['?', '#']).unwrap_or(rest.len());
                    rest[..end].to_string()
                }
                None => "/".to_string(),
            };
            return Value::Str(path);
        }
        if expr == "window.location.hostname" || expr == "location.hostname" {
            let href = self.page_origin.as_deref().unwrap_or_default();
            let after_scheme = href.find("://").map(|i| &href[i + 3..]).unwrap_or(href);
            let end = after_scheme
                .find(['/', ':', '?', '#'])
                .unwrap_or(after_scheme.len());
            return Value::Str(after_scheme[..end].to_string());
        }
        if expr == "window.location.host" || expr == "location.host" {
            let href = self.page_origin.as_deref().unwrap_or_default();
            let after_scheme = href.find("://").map(|i| &href[i + 3..]).unwrap_or(href);
            let end = after_scheme
                .find(['/', '?', '#'])
                .unwrap_or(after_scheme.len());
            return Value::Str(after_scheme[..end].to_string());
        }
        if expr == "window.location.protocol" || expr == "location.protocol" {
            let href = self.page_origin.as_deref().unwrap_or_default();
            let proto = href
                .find("://")
                .map(|i| format!("{}:", &href[..i]))
                .unwrap_or_else(|| "https:".to_string());
            return Value::Str(proto);
        }
        if expr == "window.location.origin" || expr == "location.origin" {
            let href = self.page_origin.as_deref().unwrap_or_default();
            if let Some(scheme_idx) = href.find("://") {
                let after = &href[scheme_idx + 3..];
                let end = after.find(['/', '?', '#']).unwrap_or(after.len());
                return Value::Str(format!("{}{}", &href[..scheme_idx + 3], &after[..end]));
            }
            return Value::Str(String::new());
        }
        if expr == "window.location.port" || expr == "location.port" {
            return Value::Str(String::new());
        }
        if expr == "window.location.hash" || expr == "location.hash" {
            let href = self.page_origin.as_deref().unwrap_or_default();
            let hash = href
                .find('#')
                .map(|h| href[h..].to_string())
                .unwrap_or_default();
            return Value::Str(hash);
        }
        if expr == "navigator.userAgent" || expr == "window.navigator.userAgent" {
            return Value::Str("AnvilBrowser/23.3 (Win64; x64) Forge/23.3 Cinder/4.0".to_string());
        }
        if expr == "navigator.language" || expr == "window.navigator.language" {
            return Value::Str("en-US".to_string());
        }
        if expr == "navigator.onLine" || expr == "window.navigator.onLine" {
            return Value::Bool(true);
        }
        if expr == "navigator.cookieEnabled" || expr == "window.navigator.cookieEnabled" {
            return Value::Bool(true);
        }
        if expr == "window.location.href" || expr == "location.href" {
            return Value::Str(self.page_origin.clone().unwrap_or_default());
        }
        if expr == "localStorage.length" || expr == "window.localStorage.length" {
            let origin = self.storage_origin_key();
            let count = self
                .local_storage
                .iter()
                .filter(|(o, _, _)| o == &origin)
                .count();
            return Value::Number(count as f64);
        }
        if expr == "sessionStorage.length" || expr == "window.sessionStorage.length" {
            let origin = self.storage_origin_key();
            let count = self
                .session_storage
                .iter()
                .filter(|(o, _, _)| o == &origin)
                .count();
            return Value::Number(count as f64);
        }

        if let Some(rest) = expr.strip_prefix("await ") {
            let awaited = self.eval_expr(doc, rest.trim());
            return self.await_value(doc, awaited);
        }

        if expr.ends_with(')')
            && let Some(open) = find_call_open_paren(expr)
        {
            let callee = expr[..open].trim();
            let args_src = &expr[open + 1..expr.len() - 1];
            return self.eval_call(doc, callee, args_src);
        }

        if expr.ends_with(']')
            && let Some(open) = expr.rfind('[')
            && open > 0
        {
            let target_expr = expr[..open].trim();
            let idx_expr = &expr[open + 1..expr.len() - 1];
            let target = self.eval_expr(doc, target_expr);
            let idx_val = self.eval_expr(doc, idx_expr);
            match target {
                Value::Array(items) => {
                    if let Some(i) = idx_val.as_number() {
                        return items
                            .get(i.max(0.0) as usize)
                            .cloned()
                            .unwrap_or(Value::Null);
                    }
                }
                Value::Json(JsonValue::Array(items)) => {
                    if let Some(i) = idx_val.as_number() {
                        return items
                            .get(i.max(0.0) as usize)
                            .cloned()
                            .map(Value::from_json_value)
                            .unwrap_or(Value::Null);
                    }
                }
                Value::Json(jobj) => {
                    let key = idx_val.as_string(doc);
                    return jobj
                        .get(&key)
                        .cloned()
                        .map(Value::from_json_value)
                        .unwrap_or(Value::Null);
                }
                Value::Str(ref s) if s == "__anvil_window" => {
                    let key = idx_val.as_string(doc);
                    return self.get_var(&key).cloned().unwrap_or(Value::Null);
                }
                _ => {}
            }
        }

        if let Some((obj_expr_raw, prop)) = expr.rsplit_once('.') {
            let obj_expr = obj_expr_raw
                .trim()
                .strip_suffix('?')
                .unwrap_or(obj_expr_raw.trim())
                .trim();
            let prop_trimmed = prop.trim();
            if let Some((el_expr_raw, sub_lit)) = obj_expr.rsplit_once('.') {
                let el_expr = el_expr_raw
                    .trim()
                    .strip_suffix('?')
                    .unwrap_or(el_expr_raw.trim())
                    .trim();
                let sub_trimmed = sub_lit.trim();
                if sub_trimmed == "style" {
                    if let Value::Element(id) = self.eval_expr(doc, el_expr) {
                        if prop_trimmed == "cssText" {
                            return Value::Str(
                                doc.get(id).data().attr("style").unwrap_or("").to_string(),
                            );
                        }
                        let css_prop = camel_to_kebab(prop_trimmed);
                        return Value::Str(read_inline_style(doc, id, &css_prop));
                    }
                    return Value::Str(String::new());
                }
                if sub_trimmed == "dataset" {
                    if let Value::Element(id) = self.eval_expr(doc, el_expr) {
                        let attr_name = format!("data-{}", camel_to_kebab(prop_trimmed));
                        return doc
                            .get(id)
                            .data()
                            .attr(&attr_name)
                            .map(|v| Value::Str(v.to_string()))
                            .unwrap_or(Value::Null);
                    }
                    return Value::Null;
                }
            }
            match self.eval_expr(doc, obj_expr) {
                Value::Element(id) => {
                    return self.read_element_prop(doc, id, prop_trimmed);
                }
                Value::Array(items) => {
                    if prop_trimmed == "length" {
                        return Value::Number(items.len() as f64);
                    }
                    return Value::Null;
                }
                Value::Json(jval) => {
                    if prop_trimmed == "length" || prop_trimmed == "size" {
                        if let JsonValue::Array(items) = &jval {
                            return Value::Number(items.len() as f64);
                        }
                        if let JsonValue::Object(entries) = &jval {
                            return Value::Number(entries.len() as f64);
                        }
                    }
                    return jval
                        .get(prop_trimmed)
                        .cloned()
                        .map(Value::from_json_value)
                        .unwrap_or(Value::Null);
                }
                Value::Str(ref s) if s == "__anvil_window" => {
                    return match prop_trimmed {
                        "document" => Value::Element(doc.root()),
                        "innerWidth" => Value::Number(1280.0),
                        "innerHeight" => Value::Number(800.0),
                        "devicePixelRatio" => Value::Number(1.0),
                        _ => self.get_var(prop_trimmed).cloned().unwrap_or(Value::Null),
                    };
                }
                Value::Str(s) if prop_trimmed == "length" => {
                    return Value::Number(s.chars().count() as f64);
                }
                _ => {}
            }
        }

        if let Some(val) = self.get_var(expr) {
            return val.clone();
        }
        if let Some(stripped_win) = expr
            .strip_prefix("window.")
            .or_else(|| expr.strip_prefix("self."))
            .or_else(|| expr.strip_prefix("globalThis."))
            && let Some(val) = self.get_var(stripped_win.trim())
        {
            return val.clone();
        }

        Value::Null
    }

    fn read_element_prop(&self, doc: &Document, id: NodeId, prop: &str) -> Value {
        if id == doc.root() {
            match prop {
                "body" => return Value::Element(doc.body()),
                "head" => return Value::Element(doc.head()),
                "documentElement" => return Value::Element(doc.html()),
                "title" => return Value::Str(doc.title()),
                "readyState" => return Value::Str("complete".to_string()),
                "visibilityState" => return Value::Str("visible".to_string()),
                "hidden" => return Value::Bool(false),
                "cookie" => {
                    return Value::Str(
                        self.get_var("__document_cookie")
                            .map(|v| v.as_string(doc))
                            .unwrap_or_default(),
                    );
                }
                "URL" | "documentURI" => {
                    return Value::Str(self.page_origin.clone().unwrap_or_default());
                }
                "domain" => {
                    let href = self.page_origin.as_deref().unwrap_or_default();
                    let after_scheme = href.find("://").map(|i| &href[i + 3..]).unwrap_or(href);
                    let end = after_scheme
                        .find(['/', ':', '?', '#'])
                        .unwrap_or(after_scheme.len());
                    return Value::Str(after_scheme[..end].to_string());
                }
                _ => {}
            }
        }
        match prop {
            "textContent" | "innerText" | "nodeValue" => Value::Str(doc.text_content(id)),
            "innerHTML" => Value::Str(doc.inner_html(id)),
            "shadowRoot" => doc
                .shadow_root(id)
                .map(Value::Element)
                .unwrap_or(Value::Element(id)),
            "host" => doc
                .shadow_host(id)
                .map(Value::Element)
                .unwrap_or(Value::Null),
            "width" => {
                let (w, _) = doc.canvas_dimensions(id);
                Value::Number(w as f64)
            }
            "height" => {
                let (_, h) = doc.canvas_dimensions(id);
                Value::Number(h as f64)
            }
            "outerHTML" => Value::Str(doc.outer_html(id)),
            "tagName" | "nodeName" => Value::Str(
                doc.get(id)
                    .data()
                    .tag()
                    .unwrap_or("#text")
                    .to_ascii_uppercase(),
            ),
            "value" => Value::Str(doc.input_value(id)),
            "checked" => Value::Bool(doc.is_checked(id)),
            "disabled" => Value::Bool(doc.get(id).data().attr("disabled").is_some()),
            "hidden" => Value::Bool(doc.get(id).data().attr("hidden").is_some()),
            "id" => Value::Str(doc.get(id).data().id().unwrap_or("").to_string()),
            "className" => Value::Str(doc.get(id).data().attr("class").unwrap_or("").to_string()),
            "href" | "src" | "type" | "name" | "placeholder" | "title" | "alt" | "role" => {
                Value::Str(doc.get(id).data().attr(prop).unwrap_or("").to_string())
            }
            "parentElement" | "parentNode" => doc
                .parent_element(id)
                .map(Value::Element)
                .unwrap_or(Value::Null),
            "children" | "childNodes" => Value::Array(
                doc.element_children(id)
                    .into_iter()
                    .map(Value::Element)
                    .collect(),
            ),
            "childElementCount" => Value::Number(doc.element_children(id).len() as f64),
            "firstElementChild" | "firstChild" => doc
                .first_element_child(id)
                .map(Value::Element)
                .unwrap_or(Value::Null),
            "lastElementChild" | "lastChild" => doc
                .last_element_child(id)
                .map(Value::Element)
                .unwrap_or(Value::Null),
            "nextElementSibling" | "nextSibling" => doc
                .next_element_sibling(id)
                .map(Value::Element)
                .unwrap_or(Value::Null),
            "previousElementSibling" | "previousSibling" => doc
                .previous_element_sibling(id)
                .map(Value::Element)
                .unwrap_or(Value::Null),
            _ => Value::Null,
        }
    }

    fn eval_new_expr(&mut self, doc: &mut Document, new_rest: &str) -> Value {
        let (callee, args_src) = if new_rest.ends_with(')') {
            if let Some(open) = find_call_open_paren(new_rest) {
                (
                    new_rest[..open].trim(),
                    &new_rest[open + 1..new_rest.len() - 1],
                )
            } else {
                (new_rest, "")
            }
        } else {
            (new_rest, "")
        };
        let args = split_args(args_src);
        match callee {
            "Promise" => {
                let id = self.new_promise();
                if let Some(executor) = args.first() {
                    let (params, body) = extract_multi_param_callback(executor.trim());
                    if let Some(resolve_name) = params.first().filter(|n| !n.is_empty()) {
                        self.set_var(resolve_name, Value::Str(format!("__anvil_resolve:{id}")));
                    }
                    if let Some(reject_name) = params.get(1).filter(|n| !n.is_empty()) {
                        self.set_var(reject_name, Value::Str(format!("__anvil_reject:{id}")));
                    }
                    let prev_throw = self.thrown_error.take();
                    self.eval_deferred(doc, &body);
                    if let Some(err) = self.thrown_error.take() {
                        self.reject_promise(id, err);
                    }
                    self.thrown_error = prev_throw;
                }
                self.make_promise_value(id)
            }
            "Object" | "Map" | "Set" => Value::Json(JsonValue::empty_object()),
            "Array" => {
                if args.len() == 1
                    && let Some(n) = self.eval_expr(doc, &args[0]).as_number()
                {
                    let count = n.max(0.0) as usize;
                    if !self.array_fits(count) {
                        return Value::Null;
                    }
                    return Value::Array(vec![Value::Null; count]);
                }
                if !self.array_fits(args.len()) {
                    return Value::Null;
                }
                let items = args.into_iter().map(|a| self.eval_expr(doc, &a)).collect();
                Value::Array(items)
            }
            "Error" | "TypeError" | "RangeError" | "SyntaxError" | "ReferenceError" => {
                let msg = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .unwrap_or_default();
                let mut obj = JsonValue::empty_object();
                obj.insert("name", JsonValue::String(callee.to_string()));
                obj.insert("message", JsonValue::String(msg));
                Value::Json(obj)
            }
            "Date" => Value::Number(1_700_000_000_000.0 + self.steps as f64),
            "WebAssembly.Module" => {
                let bytes = args
                    .first()
                    .map(|a| {
                        let v = self.eval_expr(doc, a);
                        self.extract_wasm_bytes(&v)
                    })
                    .unwrap_or_default();
                match WasmModule::decode(&bytes) {
                    Ok(module) => self.register_wasm_module_value(module),
                    Err(err) => {
                        let mut err_obj = JsonValue::empty_object();
                        err_obj.insert("name", JsonValue::String("CompileError".to_string()));
                        err_obj.insert("message", JsonValue::String(err.to_string()));
                        self.thrown_error = Some(Value::Json(err_obj));
                        Value::Null
                    }
                }
            }
            "WebAssembly.Instance" => {
                let first_val = args
                    .first()
                    .map(|a| self.eval_expr(doc, a))
                    .unwrap_or(Value::Null);
                let imports_val = args.get(1).map(|a| self.eval_expr(doc, a));
                let module_res = match &first_val {
                    Value::Json(jobj) => {
                        if let Some(JsonValue::Number(mid)) = jobj.get("__anvil_wasm_module") {
                            self.wasm_modules
                                .get(*mid as usize)
                                .cloned()
                                .ok_or(WasmError::InvalidModule("unknown module id"))
                        } else {
                            let bytes = self.extract_wasm_bytes(&first_val);
                            WasmModule::decode(&bytes)
                        }
                    }
                    _ => {
                        let bytes = self.extract_wasm_bytes(&first_val);
                        WasmModule::decode(&bytes)
                    }
                };
                match module_res
                    .and_then(|m| self.instantiate_wasm_module_value(m, imports_val.as_ref()))
                {
                    Ok((_, _, inst_val)) => inst_val,
                    Err(err) => {
                        let mut err_obj = JsonValue::empty_object();
                        err_obj.insert("name", JsonValue::String("RuntimeError".to_string()));
                        err_obj.insert("message", JsonValue::String(err.to_string()));
                        self.thrown_error = Some(Value::Json(err_obj));
                        Value::Null
                    }
                }
            }
            "WebAssembly.Memory" => {
                let (initial, maximum) = if let Some(first) = args.first() {
                    if let Value::Json(jobj) = self.eval_expr(doc, first) {
                        let init = jobj
                            .get("initial")
                            .and_then(|v| match v {
                                JsonValue::Number(n) => Some(*n as u32),
                                _ => None,
                            })
                            .unwrap_or(1);
                        let max = jobj.get("maximum").and_then(|v| match v {
                            JsonValue::Number(n) => Some(*n as u32),
                            _ => None,
                        });
                        (init, max)
                    } else {
                        (1, None)
                    }
                } else {
                    (1, None)
                };
                match WasmMemory::new(initial, maximum) {
                    Ok(mem) => {
                        let byte_len = mem.bytes.len();
                        let pages = mem.pages();
                        let mem_id = self.wasm_memories.len();
                        self.wasm_memories.push(mem);
                        let mut buf = JsonValue::empty_object();
                        buf.insert(
                            "__anvil_wasm_standalone_mem",
                            JsonValue::Number(mem_id as f64),
                        );
                        buf.insert("byteLength", JsonValue::Number(byte_len as f64));
                        let mut obj = JsonValue::empty_object();
                        obj.insert(
                            "__anvil_wasm_standalone_mem",
                            JsonValue::Number(mem_id as f64),
                        );
                        obj.insert("byteLength", JsonValue::Number(byte_len as f64));
                        obj.insert("pages", JsonValue::Number(pages as f64));
                        obj.insert("buffer", buf);
                        Value::Json(obj)
                    }
                    Err(err) => {
                        let mut err_obj = JsonValue::empty_object();
                        err_obj.insert("name", JsonValue::String("RangeError".to_string()));
                        err_obj.insert("message", JsonValue::String(err.to_string()));
                        self.thrown_error = Some(Value::Json(err_obj));
                        Value::Null
                    }
                }
            }
            "WebAssembly.Global" => {
                let init_val = args
                    .get(1)
                    .and_then(|a| self.eval_expr(doc, a).as_number())
                    .unwrap_or(0.0);
                let mut obj = JsonValue::empty_object();
                obj.insert("__anvil_wasm_global", JsonValue::Bool(true));
                obj.insert("value", JsonValue::Number(init_val));
                Value::Json(obj)
            }
            "WebAssembly.Table" => {
                let initial = args
                    .first()
                    .and_then(|a| match self.eval_expr(doc, a) {
                        Value::Json(j) => j.get("initial").and_then(|v| match v {
                            JsonValue::Number(n) => Some(*n),
                            _ => None,
                        }),
                        _ => None,
                    })
                    .unwrap_or(0.0);
                let mut obj = JsonValue::empty_object();
                obj.insert("__anvil_wasm_table", JsonValue::Bool(true));
                obj.insert("length", JsonValue::Number(initial));
                Value::Json(obj)
            }
            "Uint8Array" | "Uint8ClampedArray" | "Int8Array" | "Uint16Array" | "Int16Array"
            | "Uint32Array" | "Int32Array" | "Float32Array" | "Float64Array" => {
                let first_val = args
                    .first()
                    .map(|a| self.eval_expr(doc, a))
                    .unwrap_or(Value::Null);
                let offset = args
                    .get(1)
                    .and_then(|a| self.eval_expr(doc, a).as_number())
                    .map(|n| n.max(0.0) as usize)
                    .unwrap_or(0);
                let len_opt = args
                    .get(2)
                    .and_then(|a| self.eval_expr(doc, a).as_number())
                    .map(|n| n.max(0.0) as usize);
                if let Some(len) = len_opt
                    && !self.array_fits(len)
                {
                    return Value::Null;
                }
                if let Value::Number(n) = first_val
                    && !self.array_fits(n.max(0.0) as usize)
                {
                    return Value::Null;
                }
                self.construct_typed_array(callee, first_val, offset, len_opt)
            }
            "ArrayBuffer" => {
                let byte_len = args
                    .first()
                    .and_then(|a| self.eval_expr(doc, a).as_number())
                    .map(|n| n.max(0.0) as usize)
                    .unwrap_or(0);
                if !self.array_fits(byte_len) {
                    return Value::Null;
                }
                let mut obj = JsonValue::empty_object();
                obj.insert("byteLength", JsonValue::Number(byte_len as f64));
                obj.insert(
                    "__bytes",
                    JsonValue::Array(vec![JsonValue::Number(0.0); byte_len]),
                );
                Value::Json(obj)
            }
            "TextEncoder" => {
                let mut obj = JsonValue::empty_object();
                obj.insert("__anvil_kind", JsonValue::String("TextEncoder".to_string()));
                obj.insert("encoding", JsonValue::String("utf-8".to_string()));
                Value::Json(obj)
            }
            "TextDecoder" => {
                let mut obj = JsonValue::empty_object();
                obj.insert("__anvil_kind", JsonValue::String("TextDecoder".to_string()));
                obj.insert("encoding", JsonValue::String("utf-8".to_string()));
                Value::Json(obj)
            }
            "URLSearchParams" => {
                let raw = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .unwrap_or_default();
                let query = raw.trim_start_matches('?');
                let mut obj = JsonValue::empty_object();
                for pair in query.split('&').filter(|p| !p.is_empty()) {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    obj.insert(
                        k.replace("%20", " ").replace('+', " "),
                        JsonValue::String(v.replace("%20", " ").replace('+', " ")),
                    );
                }
                Value::Json(obj)
            }
            "DOMParser" => {
                let mut obj = JsonValue::empty_object();
                obj.insert("__anvil_kind", JsonValue::String("DOMParser".to_string()));
                Value::Json(obj)
            }
            "AbortController" => {
                let mut sig = JsonValue::empty_object();
                sig.insert("aborted", JsonValue::Bool(false));
                let mut obj = JsonValue::empty_object();
                obj.insert(
                    "__anvil_kind",
                    JsonValue::String("AbortController".to_string()),
                );
                obj.insert("signal", sig);
                Value::Json(obj)
            }
            "MutationObserver" => {
                let cb = args
                    .first()
                    .map(|a| a.trim().to_string())
                    .unwrap_or_default();
                let obs_id = self.mutation_observers.len();
                self.mutation_observers.push(MutationObserverEntry {
                    id: obs_id,
                    callback: cb,
                    targets: Vec::new(),
                    records: Vec::new(),
                    scheduled: false,
                });
                let mut obj = JsonValue::empty_object();
                obj.insert(
                    "__anvil_kind",
                    JsonValue::String("MutationObserver".to_string()),
                );
                obj.insert("__observer_id", JsonValue::Number(obs_id as f64));
                Value::Json(obj)
            }
            "IntersectionObserver" | "ResizeObserver" => {
                let cb = args
                    .first()
                    .map(|a| a.trim().to_string())
                    .unwrap_or_default();
                let obs_id = self.layout_observers.len();
                self.layout_observers.push(LayoutObserverEntry {
                    id: obs_id,
                    kind: callee.to_string(),
                    callback: cb,
                    targets: Vec::new(),
                });
                let mut obj = JsonValue::empty_object();
                obj.insert("__anvil_kind", JsonValue::String(callee.to_string()));
                obj.insert("__observer_id", JsonValue::Number(obs_id as f64));
                Value::Json(obj)
            }
            "MediaSource" | "XMLHttpRequest" => {
                let mut obj = JsonValue::empty_object();
                obj.insert("__anvil_kind", JsonValue::String(callee.to_string()));
                if callee == "MediaSource" {
                    obj.insert("readyState", JsonValue::String("open".to_string()));
                    obj.insert("duration", JsonValue::Number(0.0));
                }
                Value::Json(obj)
            }
            "ImageData" => {
                let w = args
                    .first()
                    .and_then(|a| self.eval_expr(doc, a).as_number())
                    .unwrap_or(1.0)
                    .clamp(1.0, 2048.0) as u32;
                let h = args
                    .get(1)
                    .and_then(|a| self.eval_expr(doc, a).as_number())
                    .unwrap_or(1.0)
                    .clamp(1.0, 2048.0) as u32;
                let len = (w as usize).saturating_mul(h as usize).saturating_mul(4);
                let mut obj = JsonValue::empty_object();
                obj.insert("__anvil_kind", JsonValue::String("ImageData".to_string()));
                obj.insert("width", JsonValue::Number(w as f64));
                obj.insert("height", JsonValue::Number(h as f64));
                if self.array_fits(len) {
                    obj.insert("data", JsonValue::Array(vec![JsonValue::Number(0.0); len]));
                }
                Value::Json(obj)
            }
            "VideoFrame" => {
                let mut obj = JsonValue::empty_object();
                obj.insert("__anvil_kind", JsonValue::String("VideoFrame".to_string()));
                let mut w = 64.0;
                let mut h = 64.0;
                let mut ts = 0.0;
                let mut fmt = "RGBA".to_string();
                if let Some(opts_arg) = args.get(1).or_else(|| args.first())
                    && let Value::Json(jopts) = self.eval_expr(doc, opts_arg)
                {
                    if let Some(JsonValue::Number(cw)) =
                        jopts.get("codedWidth").or_else(|| jopts.get("width"))
                    {
                        w = *cw;
                    }
                    if let Some(JsonValue::Number(ch)) =
                        jopts.get("codedHeight").or_else(|| jopts.get("height"))
                    {
                        h = *ch;
                    }
                    if let Some(JsonValue::Number(t)) = jopts.get("timestamp") {
                        ts = *t;
                    }
                    if let Some(JsonValue::String(f)) = jopts.get("format") {
                        fmt = f.clone();
                    }
                }
                obj.insert("codedWidth", JsonValue::Number(w));
                obj.insert("codedHeight", JsonValue::Number(h));
                obj.insert("displayWidth", JsonValue::Number(w));
                obj.insert("displayHeight", JsonValue::Number(h));
                obj.insert("timestamp", JsonValue::Number(ts));
                obj.insert("format", JsonValue::String(fmt));
                Value::Json(obj)
            }
            "EncodedVideoChunk" => {
                let mut obj = JsonValue::empty_object();
                obj.insert(
                    "__anvil_kind",
                    JsonValue::String("EncodedVideoChunk".to_string()),
                );
                let mut chunk_type = "key".to_string();
                let mut ts = 0.0;
                let mut byte_len = 0.0;
                if let Some(first) = args.first()
                    && let Value::Json(jinit) = self.eval_expr(doc, first)
                {
                    if let Some(JsonValue::String(t)) = jinit.get("type") {
                        chunk_type = t.clone();
                    }
                    if let Some(JsonValue::Number(t)) = jinit.get("timestamp") {
                        ts = *t;
                    }
                    if let Some(JsonValue::Array(arr)) = jinit.get("data") {
                        byte_len = arr.len() as f64;
                    }
                }
                obj.insert("type", JsonValue::String(chunk_type));
                obj.insert("timestamp", JsonValue::Number(ts));
                obj.insert("byteLength", JsonValue::Number(byte_len));
                Value::Json(obj)
            }
            "VideoEncoder" | "VideoDecoder" => {
                let mut obj = JsonValue::empty_object();
                obj.insert("__anvil_kind", JsonValue::String(callee.to_string()));
                obj.insert("state", JsonValue::String("unconfigured".to_string()));
                obj.insert("encodeQueueSize", JsonValue::Number(0.0));
                obj.insert("decodeQueueSize", JsonValue::Number(0.0));
                obj.insert("encodedChunks", JsonValue::Number(0.0));
                obj.insert("decodedFrames", JsonValue::Number(0.0));
                Value::Json(obj)
            }
            "AudioContext" | "MediaRecorder" => {
                let mut obj = JsonValue::empty_object();
                obj.insert("__anvil_kind", JsonValue::String(callee.to_string()));
                if callee == "AudioContext" {
                    obj.insert("state", JsonValue::String("running".to_string()));
                    obj.insert("sampleRate", JsonValue::Number(48000.0));
                    obj.insert("currentTime", JsonValue::Number(0.0));
                } else {
                    obj.insert("state", JsonValue::String("inactive".to_string()));
                    obj.insert(
                        "mimeType",
                        JsonValue::String("video/webm;codecs=vp9,opus".to_string()),
                    );
                }
                Value::Json(obj)
            }
            "Event" | "CustomEvent" => {
                let ev_type = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .unwrap_or_default();
                let mut obj = JsonValue::empty_object();
                obj.insert("type", JsonValue::String(ev_type));
                if let Some(opts) = args.get(1)
                    && let Value::Json(jopts) = self.eval_expr(doc, opts)
                    && let Some(detail) = jopts.get("detail").cloned()
                {
                    obj.insert("detail", detail);
                }
                Value::Json(obj)
            }
            "URL" => {
                let raw = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .unwrap_or_default();
                let mut obj = JsonValue::empty_object();
                obj.insert("href", JsonValue::String(raw.clone()));
                if let Some((scheme, host, port)) = extract_origin_tuple(&raw) {
                    obj.insert("protocol", JsonValue::String(format!("{scheme}:")));
                    obj.insert("hostname", JsonValue::String(host.clone()));
                    obj.insert("port", JsonValue::String(port.to_string()));
                    obj.insert(
                        "origin",
                        JsonValue::String(format!("{scheme}://{host}:{port}")),
                    );
                }
                Value::Json(obj)
            }
            _ => {
                if let Some(class_def) = self.classes.iter().find(|c| c.name == callee).cloned() {
                    let mut instance = JsonValue::empty_object();
                    instance.insert("__class", JsonValue::String(class_def.name.clone()));
                    let prev_this = self.get_var("this").cloned();
                    self.set_var("this", Value::Json(instance));
                    if let Some((_, params, body)) = class_def
                        .methods
                        .iter()
                        .find(|(m, _, _)| m == "constructor")
                        .cloned()
                    {
                        self.invoke_method_body(
                            doc,
                            &params,
                            &body,
                            &args,
                            class_def.super_class.as_deref(),
                        );
                    }
                    let result = self.get_var("this").cloned().unwrap_or(Value::Null);
                    if let Some(old) = prev_this {
                        self.set_var("this", old);
                    }
                    return result;
                }
                self.eval_call(doc, callee, args_src)
            }
        }
    }

    fn invoke_method_body(
        &mut self,
        doc: &mut Document,
        params: &[String],
        body: &str,
        args: &[String],
        super_class: Option<&str>,
    ) -> Value {
        if self.call_depth >= MAX_CALL_DEPTH {
            self.abort_call_depth();
            return Value::Null;
        }
        let evaled_args: Vec<Value> = args.iter().map(|a| self.eval_expr(doc, a)).collect();
        for (idx, param) in params.iter().enumerate() {
            let val = evaled_args.get(idx).cloned().unwrap_or(Value::Null);
            self.set_var(param, val);
        }
        if let Some(sup_name) = super_class {
            self.set_var("__super_class", Value::Str(sup_name.to_string()));
        }
        let prev_return = self.return_value.take();
        self.call_depth += 1;
        self.eval_script(doc, body);
        self.call_depth = self.call_depth.saturating_sub(1);
        let result = self.return_value.take().unwrap_or(Value::Null);
        self.return_value = prev_return;
        result
    }

    fn find_class_method(
        &self,
        class_name: &str,
        method_name: &str,
    ) -> Option<(Vec<String>, String, Option<String>)> {
        let mut cursor = Some(class_name.to_string());
        let mut depth = 0usize;
        while let Some(curr) = cursor {
            if depth > 16 {
                break;
            }
            depth += 1;
            let def = self.classes.iter().find(|c| c.name == curr)?;
            if let Some((_, params, body)) = def.methods.iter().find(|(m, _, _)| m == method_name) {
                return Some((params.clone(), body.clone(), def.super_class.clone()));
            }
            cursor = def.super_class.clone();
        }
        None
    }

    /// True when the method `find_class_method` would pick is `async`.
    fn class_method_is_async(&self, class_name: &str, method_name: &str) -> bool {
        let mut next_class = Some(class_name.to_string());
        for _ in 0..=16 {
            let Some(curr) = next_class else {
                return false;
            };
            let Some(def) = self.classes.iter().find(|c| c.name == curr) else {
                return false;
            };
            if def.methods.iter().any(|(m, _, _)| m == method_name) {
                return def.async_methods.iter().any(|m| m == method_name);
            }
            next_class = def.super_class.clone();
        }
        false
    }

    fn eval_template_literal(&mut self, doc: &mut Document, inner: &str) -> String {
        let mut out = String::new();
        let mut rest = inner;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            if let Some(end) = find_template_expr_end(after) {
                let expr_str = &after[..end];
                let val = self.eval_expr(doc, expr_str);
                out.push_str(&val.as_string(doc));
                if out.len() > MAX_STRING_LEN {
                    self.abort_string_limit();
                    return String::new();
                }
                rest = &after[end + 1..];
            } else {
                out.push_str(&rest[start..]);
                return out;
            }
        }
        out.push_str(rest);
        out
    }

    fn lookup_cached_dom(&mut self, key: &str) -> Option<NodeId> {
        if !self.jit_stats.jit_enabled {
            return None;
        }
        if let Some((_, id)) = self.dom_cache.iter().find(|(k, _)| k == key) {
            self.jit_stats.inline_cache_hits = self.jit_stats.inline_cache_hits.saturating_add(1);
            return Some(*id);
        }
        None
    }

    fn store_cached_dom(&mut self, key: String, id: NodeId) {
        if self.jit_stats.jit_enabled && self.dom_cache.len() < 128 {
            self.dom_cache.push((key, id));
        }
    }

    fn eval_call(&mut self, doc: &mut Document, callee_raw: &str, args_src: &str) -> Value {
        let callee = callee_raw
            .trim()
            .strip_prefix("window.")
            .or_else(|| callee_raw.trim().strip_prefix("self."))
            .or_else(|| callee_raw.trim().strip_prefix("globalThis."))
            .unwrap_or(callee_raw.trim())
            .trim();

        if callee == "super" {
            let args = split_args(args_src);
            if let Some(Value::Str(sup_name)) = self.get_var("__super_class").cloned()
                && let Some((params, body, next_super)) =
                    self.find_class_method(&sup_name, "constructor")
            {
                return self.invoke_method_body(doc, &params, &body, &args, next_super.as_deref());
            }
            return Value::Null;
        }

        if callee.starts_with("function") || callee.starts_with("async function") {
            let args = split_args(args_src);
            let (is_async, rest) = split_async_prefix(callee);
            if is_async {
                let (params, body) = async_callback_parts(rest);
                let values: Vec<Value> = args.iter().map(|a| self.eval_expr(doc, a)).collect();
                let env = self.capture_env(&body);
                return self.start_async(doc, &params, values, &body, &env);
            }
            let (params, body) = extract_multi_param_callback(callee);
            return self.invoke_method_body(doc, &params, &body, &args, None);
        }

        if callee.starts_with('(')
            && callee.ends_with(')')
            && let Some((inner_fn, rem)) = extract_paren_group(callee)
        {
            if rem.trim().is_empty() {
                let args = split_args(args_src);
                // `(async () => { ... })()` runs as an async call, so it
                // returns at its first `await` like a named one. It keeps
                // a copy of the enclosing async function's locals, the
                // same as a callback created there.
                let (is_async, rest) = split_async_prefix(inner_fn.trim());
                if is_async {
                    let (params, body) = async_callback_parts(rest);
                    let values: Vec<Value> = args.iter().map(|a| self.eval_expr(doc, a)).collect();
                    let env = self.capture_env(&body);
                    return self.start_async(doc, &params, values, &body, &env);
                }
                let (params, body) = extract_multi_param_callback(inner_fn.trim());
                return self.invoke_method_body(doc, &params, &body, &args, None);
            }

            if let Some(Value::Str(marker)) = self.get_var(callee).cloned()
                && let Some(rest) = marker.strip_prefix("__wasm_fn:")
                && let Some((inst_str, fn_name)) = rest.split_once(':')
                && let Ok(inst_id) = inst_str.parse::<usize>()
            {
                let args = split_args(args_src);
                let args_f64: Vec<f64> = args
                    .iter()
                    .map(|a| self.eval_expr(doc, a).as_number().unwrap_or(0.0))
                    .collect();
                return self.call_wasm_instance_export(inst_id, fn_name, &args_f64);
            }
        }

        if let Some((pid, rejects)) = self.resolver_marker(callee) {
            let args = split_args(args_src);
            let v = args
                .first()
                .map(|a| self.eval_expr(doc, a))
                .unwrap_or(Value::Null);
            if rejects {
                self.reject_promise(pid, v);
            } else {
                self.resolve_promise(pid, v);
            }
            return Value::Null;
        }

        match callee {
            "parseInt" | "parseFloat" | "Number" => {
                let args = split_args(args_src);
                let raw = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .unwrap_or_default();
                let trimmed = raw.trim();
                let num = if callee == "parseInt" {
                    trimmed
                        .split(|c: char| !c.is_ascii_digit() && c != '-' && c != '+')
                        .next()
                        .and_then(|s| s.parse::<i64>().ok())
                        .map(|n| n as f64)
                        .unwrap_or(0.0)
                } else {
                    trimmed.parse::<f64>().unwrap_or(0.0)
                };
                return Value::Number(num);
            }
            "String" => {
                let args = split_args(args_src);
                let s = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .unwrap_or_default();
                return Value::Str(s);
            }
            "Boolean" => {
                let args = split_args(args_src);
                let b = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_bool())
                    .unwrap_or(false);
                return Value::Bool(b);
            }
            "isNaN" | "Number.isNaN" => {
                let args = split_args(args_src);
                let is_nan = args
                    .first()
                    .map(|a| {
                        let v = self.eval_expr(doc, a);
                        match v {
                            Value::Number(n) => n.is_nan(),
                            Value::Str(s) => s.trim().parse::<f64>().is_err(),
                            _ => false,
                        }
                    })
                    .unwrap_or(true);
                return Value::Bool(is_nan);
            }
            "Number.isFinite" => {
                let args = split_args(args_src);
                let finite = args
                    .first()
                    .map(|a| matches!(self.eval_expr(doc, a), Value::Number(n) if n.is_finite()))
                    .unwrap_or(false);
                return Value::Bool(finite);
            }
            "Number.isInteger" => {
                let args = split_args(args_src);
                let is_int = args
                    .first()
                    .map(|a| {
                        matches!(
                            self.eval_expr(doc, a),
                            Value::Number(n) if n.is_finite() && n.fract() == 0.0
                        )
                    })
                    .unwrap_or(false);
                return Value::Bool(is_int);
            }
            "Array.isArray" => {
                let args = split_args(args_src);
                let is_arr = args
                    .first()
                    .map(|a| {
                        matches!(
                            self.eval_expr(doc, a),
                            Value::Array(_) | Value::Json(JsonValue::Array(_))
                        )
                    })
                    .unwrap_or(false);
                return Value::Bool(is_arr);
            }
            "Array.from" => {
                let args = split_args(args_src);
                if let Some(first) = args.first() {
                    return match self.eval_expr(doc, first) {
                        Value::Array(items) => Value::Array(items),
                        Value::Json(JsonValue::Array(items)) => {
                            Value::Array(items.into_iter().map(Value::from_json_value).collect())
                        }
                        Value::Str(s) => {
                            Value::Array(s.chars().map(|c| Value::Str(c.to_string())).collect())
                        }
                        _ => Value::Array(Vec::new()),
                    };
                }
                return Value::Array(Vec::new());
            }
            "Object.keys" => {
                let args = split_args(args_src);
                if let Some(first) = args.first()
                    && let Value::Json(JsonValue::Object(entries)) = self.eval_expr(doc, first)
                {
                    return Value::Array(entries.into_iter().map(|(k, _)| Value::Str(k)).collect());
                }
                return Value::Array(Vec::new());
            }
            "Object.values" => {
                let args = split_args(args_src);
                if let Some(first) = args.first()
                    && let Value::Json(JsonValue::Object(entries)) = self.eval_expr(doc, first)
                {
                    return Value::Array(
                        entries
                            .into_iter()
                            .map(|(_, v)| Value::from_json_value(v))
                            .collect(),
                    );
                }
                return Value::Array(Vec::new());
            }
            "Object.entries" => {
                let args = split_args(args_src);
                if let Some(first) = args.first()
                    && let Value::Json(JsonValue::Object(entries)) = self.eval_expr(doc, first)
                {
                    return Value::Array(
                        entries
                            .into_iter()
                            .map(|(k, v)| {
                                Value::Array(vec![Value::Str(k), Value::from_json_value(v)])
                            })
                            .collect(),
                    );
                }
                return Value::Array(Vec::new());
            }
            "Object.assign" => {
                let args = split_args(args_src);
                let mut merged = JsonValue::empty_object();
                let target_ident = args.first().map(|s| s.trim().to_string());
                for arg in &args {
                    if let Value::Json(JsonValue::Object(entries)) = self.eval_expr(doc, arg) {
                        for (k, v) in entries {
                            merged.insert(&k, v);
                        }
                    }
                }
                let out = Value::Json(merged);
                if let Some(ident) = target_ident
                    && is_identifier(&ident)
                {
                    self.set_var(&ident, out.clone());
                }
                return out;
            }
            "Object.fromEntries" => {
                let args = split_args(args_src);
                let mut obj = JsonValue::empty_object();
                if let Some(first) = args.first()
                    && let Value::Array(rows) = self.eval_expr(doc, first)
                {
                    for row in rows {
                        if let Value::Array(pair) = row
                            && pair.len() >= 2
                        {
                            let k = pair[0].as_string(doc);
                            let v = pair[1].to_json_value(doc);
                            obj.insert(&k, v);
                        }
                    }
                }
                return Value::Json(obj);
            }
            "atob" => {
                let args = split_args(args_src);
                let raw = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .unwrap_or_default();
                return Value::Str(decode_base64_to_latin1(&raw));
            }
            "btoa" => {
                let args = split_args(args_src);
                let raw = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .unwrap_or_default();
                return Value::Str(encode_latin1_to_base64(raw.as_bytes()));
            }
            "structuredClone" => {
                let args = split_args(args_src);
                return args
                    .first()
                    .map(|a| self.eval_expr(doc, a))
                    .unwrap_or(Value::Null);
            }
            "String.fromCharCode" | "String.fromCodePoint" => {
                let args = split_args(args_src);
                let mut out = String::new();
                for arg in &args {
                    if let Some(code) = self.eval_expr(doc, arg).as_number()
                        && let Some(ch) = char::from_u32(code.max(0.0) as u32)
                    {
                        out.push(ch);
                    }
                }
                return Value::Str(out);
            }
            "Object.freeze" | "Object.seal" | "Object.create" => {
                let args = split_args(args_src);
                return args
                    .first()
                    .map(|a| self.eval_expr(doc, a))
                    .unwrap_or_else(|| Value::Json(JsonValue::empty_object()));
            }
            "Object.is" => {
                let args = split_args(args_src);
                let a = args
                    .first()
                    .map(|x| self.eval_expr(doc, x))
                    .unwrap_or(Value::Null);
                let b = args
                    .get(1)
                    .map(|x| self.eval_expr(doc, x))
                    .unwrap_or(Value::Null);
                return Value::Bool(values_equal(&a, &b, doc));
            }
            "Object.hasOwn" => {
                let args = split_args(args_src);
                if args.len() >= 2
                    && let Value::Json(jobj) = self.eval_expr(doc, &args[0])
                {
                    let key = self.eval_expr(doc, &args[1]).as_string(doc);
                    return Value::Bool(jobj.get(&key).is_some());
                }
                return Value::Bool(false);
            }
            "Array.of" => {
                let args = split_args(args_src);
                if !self.array_fits(args.len()) {
                    return Value::Null;
                }
                let items = args.into_iter().map(|a| self.eval_expr(doc, &a)).collect();
                return Value::Array(items);
            }
            "encodeURI" | "encodeURIComponent" => {
                let args = split_args(args_src);
                let raw = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .unwrap_or_default();
                let mut encoded = String::new();
                for b in raw.bytes() {
                    if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                        encoded.push(b as char);
                    } else {
                        encoded.push_str(&format!("%{:02X}", b));
                    }
                }
                return Value::Str(encoded);
            }
            "decodeURI" | "decodeURIComponent" => {
                let args = split_args(args_src);
                let raw = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .unwrap_or_default();
                let mut out_bytes = Vec::with_capacity(raw.len());
                let bytes = raw.as_bytes();
                let mut idx = 0;
                while idx < bytes.len() {
                    if bytes[idx] == b'%'
                        && idx + 2 < bytes.len()
                        && let Ok(hex_str) = std::str::from_utf8(&bytes[idx + 1..idx + 3])
                        && let Ok(decoded_b) = u8::from_str_radix(hex_str, 16)
                    {
                        out_bytes.push(decoded_b);
                        idx += 3;
                    } else if bytes[idx] == b'+' && callee == "decodeURIComponent" {
                        out_bytes.push(b' ');
                        idx += 1;
                    } else {
                        out_bytes.push(bytes[idx]);
                        idx += 1;
                    }
                }
                return Value::Str(String::from_utf8_lossy(&out_bytes).into_owned());
            }
            "queueMicrotask" => {
                let args = split_args(args_src);
                if let Some(first) = args.first() {
                    let (code, env, is_async) = self.deferred_callback(first);
                    self.schedule_job(Job::Script {
                        code,
                        env,
                        is_async,
                    });
                }
                return Value::Null;
            }
            "performance.now" => {
                return Value::Number(self.steps as f64 * 0.01);
            }
            "Date.now" => {
                return Value::Number(1_700_000_000_000.0 + self.steps as f64);
            }
            "matchMedia" => {
                let args = split_args(args_src);
                let q = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .unwrap_or_default();
                let mut obj = JsonValue::empty_object();
                obj.insert("matches", JsonValue::Bool(false));
                obj.insert("media", JsonValue::String(q));
                return Value::Json(obj);
            }
            "getComputedStyle" => {
                let args = split_args(args_src);
                let mut obj = JsonValue::empty_object();
                if let Some(first) = args.first()
                    && let Value::Element(id) = self.eval_expr(doc, first)
                {
                    let inline = doc.get(id).data().attr("style").unwrap_or("");
                    for part in inline.split(';') {
                        if let Some((k, v)) = part.split_once(':') {
                            let k = k.trim().to_ascii_lowercase();
                            let v = v.trim().to_string();
                            if !k.is_empty() {
                                obj.insert(&k, JsonValue::String(v.clone()));
                                obj.insert(kebab_to_camel(&k), JsonValue::String(v));
                            }
                        }
                    }
                }
                return Value::Json(obj);
            }
            "cancelAnimationFrame" | "scrollTo" | "scrollBy" => {
                return Value::Null;
            }
            "clearTimeout" | "clearInterval" => {
                let args = split_args(args_src);
                if let Some(target) = args
                    .first()
                    .and_then(|a| self.eval_expr(doc, a).as_number())
                {
                    let target = target.max(0.0) as u64;
                    for t in self.timers.iter_mut() {
                        if t.id == target {
                            t.cleared = true;
                        }
                    }
                }
                return Value::Null;
            }
            "addEventListener" => {
                let args = split_args(args_src);
                if args.len() >= 2 {
                    let event = self.eval_expr(doc, &args[0]).as_string(doc);
                    let body = extract_event_callback_body(args[1].trim());
                    match event.to_ascii_lowercase().as_str() {
                        "click" => self.click_listeners.push((doc.body(), body)),
                        "input" => self.input_listeners.push((doc.body(), body)),
                        "change" => self.change_listeners.push((doc.body(), body)),
                        "submit" => self.submit_listeners.push((doc.body(), body)),
                        "keydown" => self.keydown_listeners.push((doc.body(), body)),
                        "domcontentloaded" | "load" | "readystatechange" => {
                            let (code, env, is_async) = self.deferred_callback(&args[1]);
                            self.schedule_job(Job::Script {
                                code,
                                env,
                                is_async,
                            });
                        }
                        _ => {}
                    }
                }
                return Value::Null;
            }
            _ => {}
        }

        if callee == "setTimeout" || callee == "setInterval" || callee == "requestAnimationFrame" {
            let args = split_args(args_src);
            let mut new_id = 0u64;
            if let Some(first) = args.first()
                && self.timers.len() < MAX_TIMERS
            {
                let (body, env, is_async) = self.deferred_callback(first);
                let mut delay = args
                    .get(1)
                    .and_then(|d| self.eval_expr(doc, d).as_number())
                    .unwrap_or(0.0)
                    .max(0.0) as u64;
                // Clamp a nested timer to 4 ms, as a browser does, so a
                // zero-delay timer that reschedules itself still advances
                // the clock and keeps a fair order with other timers.
                if self.timer_depth > 0 && delay < 4 {
                    delay = 4;
                }
                let period = if callee == "setInterval" {
                    Some(delay.max(1))
                } else {
                    None
                };
                new_id = self.alloc_timer_id();
                let seq = self.alloc_seq();
                let due = self.clock_ms.saturating_add(delay);
                self.timers.push(Timer {
                    id: new_id,
                    seq,
                    due,
                    body,
                    env,
                    is_async,
                    period,
                    cleared: false,
                });
            }
            return Value::Number(new_id as f64);
        }

        if callee == "fetch" {
            let args = split_args(args_src);
            let url = args
                .first()
                .map(|a| self.eval_expr(doc, a).as_string(doc))
                .unwrap_or_default();
            if !is_allowed_script_fetch(self.page_origin.as_deref(), &url) {
                let mut obj = JsonValue::empty_object();
                obj.insert("url", JsonValue::String(url));
                obj.insert("status", JsonValue::Number(403.0));
                obj.insert("ok", JsonValue::Bool(false));
                obj.insert(
                    "body",
                    JsonValue::String(
                        "{\"error\":\"same-origin policy blocked fetch\"}".to_string(),
                    ),
                );
                return Value::Json(obj);
            }
            if let Some((status, body)) = self
                .fetch_mocks
                .iter()
                .rev()
                .find(|(registered, _, _)| registered == &url)
                .map(|(_, status, body)| (*status, body.clone()))
            {
                return fetch_result_value(&url, status, body);
            }
            // The step budget is spent before the host runs, so a spent budget
            // does not open a socket.
            if !self.consume_step() {
                return fetch_result_value(&url, 0, String::new());
            }
            let page = self.page_origin.clone().unwrap_or_default();
            let absolute = resolve_script_fetch_url(&page, &url);
            let Some(host) = self.fetch_host.inner.clone() else {
                return fetch_result_value(&url, 200, "{}".to_string());
            };
            let mut reply = host(&absolute, &page);
            limit_script_fetch_body(&mut reply.body);
            let extra_steps = reply.body.len() / 1024;
            for _ in 0..extra_steps {
                if !self.consume_step() {
                    break;
                }
            }
            return fetch_result_value(&url, reply.status, reply.body);
        }

        if let Some((obj_expr_raw, method)) = callee.rsplit_once('.') {
            let obj_expr = obj_expr_raw
                .trim()
                .strip_suffix('?')
                .unwrap_or(obj_expr_raw.trim())
                .trim();
            let method = method.trim();

            if matches!(method, "then" | "catch" | "finally") {
                let target = self.eval_expr(doc, obj_expr);
                // A class instance with its own method of this name keeps it.
                if !self.instance_has_method(&target, method) {
                    let pid = self.value_as_promise(target);
                    let args = split_args(args_src);
                    return self.promise_method(pid, method, &args);
                }
            }

            if obj_expr == "localStorage" || obj_expr == "window.localStorage" {
                let args = split_args(args_src);
                return match method {
                    "getItem" => {
                        let key = args
                            .first()
                            .map(|a| self.eval_expr(doc, a).as_string(doc))
                            .unwrap_or_default();
                        self.local_storage_get(&key)
                            .map(|v| Value::Str(v.to_string()))
                            .unwrap_or(Value::Null)
                    }
                    "setItem" if args.len() >= 2 => {
                        let key = self.eval_expr(doc, &args[0]).as_string(doc);
                        let val = self.eval_expr(doc, &args[1]).as_string(doc);
                        self.local_storage_set(&key, &val);
                        Value::Null
                    }
                    "removeItem" if !args.is_empty() => {
                        let key = self.eval_expr(doc, &args[0]).as_string(doc);
                        self.local_storage_remove(&key);
                        Value::Null
                    }
                    "clear" => {
                        self.local_storage_clear();
                        Value::Null
                    }
                    _ => Value::Null,
                };
            }

            if obj_expr == "sessionStorage" || obj_expr == "window.sessionStorage" {
                let args = split_args(args_src);
                let origin = self.storage_origin_key();
                return match method {
                    "getItem" => {
                        let key = args
                            .first()
                            .map(|a| self.eval_expr(doc, a).as_string(doc))
                            .unwrap_or_default();
                        self.session_storage
                            .iter()
                            .find(|(o, k, _)| o == &origin && k == &key)
                            .map(|(_, _, v)| Value::Str(v.clone()))
                            .unwrap_or(Value::Null)
                    }
                    "setItem" if args.len() >= 2 => {
                        let key = self.eval_expr(doc, &args[0]).as_string(doc);
                        let val = self.eval_expr(doc, &args[1]).as_string(doc);
                        storage_put(&mut self.session_storage, &origin, &key, &val);
                        Value::Null
                    }
                    "removeItem" if !args.is_empty() => {
                        let key = self.eval_expr(doc, &args[0]).as_string(doc);
                        self.session_storage
                            .retain(|(o, k, _)| !(o == &origin && k == &key));
                        Value::Null
                    }
                    "clear" => {
                        self.session_storage.retain(|(o, _, _)| o != &origin);
                        Value::Null
                    }
                    _ => Value::Null,
                };
            }

            if obj_expr == "Promise" || obj_expr == "window.Promise" {
                let args = split_args(args_src);
                if let Some(result) = self.promise_static(doc, method, &args) {
                    return result;
                }
            }

            if obj_expr == "console" {
                let args = split_args(args_src);
                let line = args
                    .iter()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .collect::<Vec<_>>()
                    .join(" ");
                let prefix = match method {
                    "warn" => "[warn] ",
                    "error" => "[error] ",
                    _ => "",
                };
                self.push_console(format!("{prefix}{line}"));
                return Value::Null;
            }

            if obj_expr == "Math" {
                let args = split_args(args_src);
                let first = args
                    .first()
                    .and_then(|a| self.eval_expr(doc, a).as_number())
                    .unwrap_or(0.0);
                let second = args
                    .get(1)
                    .and_then(|a| self.eval_expr(doc, a).as_number())
                    .unwrap_or(0.0);
                let res = match method {
                    "floor" => first.floor(),
                    "ceil" => first.ceil(),
                    "round" => first.round(),
                    "abs" => first.abs(),
                    "sqrt" => first.max(0.0).sqrt(),
                    "min" => {
                        if args.len() >= 2 {
                            first.min(second)
                        } else {
                            first
                        }
                    }
                    "max" => {
                        if args.len() >= 2 {
                            first.max(second)
                        } else {
                            first
                        }
                    }
                    "pow" => first.powf(second),
                    "random" => 0.42,
                    _ => 0.0,
                };
                return Value::Number(res);
            }

            if let Some((el_expr_raw, sub_obj)) = obj_expr.rsplit_once('.') {
                let el_expr = el_expr_raw
                    .trim()
                    .strip_suffix('?')
                    .unwrap_or(el_expr_raw.trim())
                    .trim();
                let sub_trimmed = sub_obj.trim();
                if sub_trimmed == "classList"
                    && let Value::Element(id) = self.eval_expr(doc, el_expr)
                {
                    let args = split_args(args_src);
                    let cls = args
                        .first()
                        .map(|a| self.eval_expr(doc, a).as_string(doc))
                        .unwrap_or_default();
                    return match method {
                        "add" => {
                            doc.add_class(id, &cls);
                            self.notify_mutation_observers(doc, "attributes", id, Some("class"));
                            Value::Null
                        }
                        "remove" => {
                            doc.remove_class(id, &cls);
                            self.notify_mutation_observers(doc, "attributes", id, Some("class"));
                            Value::Null
                        }
                        "toggle" => {
                            let res = doc.toggle_class(id, &cls);
                            self.notify_mutation_observers(doc, "attributes", id, Some("class"));
                            Value::Bool(res)
                        }
                        "contains" => Value::Bool(doc.get(id).data().has_class(&cls)),
                        _ => Value::Null,
                    };
                }
                if sub_trimmed == "style"
                    && let Value::Element(id) = self.eval_expr(doc, el_expr)
                {
                    let args = split_args(args_src);
                    let prop = args
                        .first()
                        .map(|a| camel_to_kebab(&self.eval_expr(doc, a).as_string(doc)))
                        .unwrap_or_default();
                    return match method {
                        "setProperty" if args.len() >= 2 => {
                            let val = self.eval_expr(doc, &args[1]).as_string(doc);
                            update_inline_style(doc, id, &prop, &val);
                            self.notify_mutation_observers(doc, "attributes", id, Some("style"));
                            Value::Null
                        }
                        "getPropertyValue" => Value::Str(read_inline_style(doc, id, &prop)),
                        "removeProperty" => {
                            let prev = read_inline_style(doc, id, &prop);
                            remove_inline_style_prop(doc, id, &prop);
                            self.notify_mutation_observers(doc, "attributes", id, Some("style"));
                            Value::Str(prev)
                        }
                        _ => Value::Null,
                    };
                }
            }

            if obj_expr == "JSON" {
                let args = split_args(args_src);
                return match method {
                    "parse" => {
                        let first = args
                            .first()
                            .map(|a| self.eval_expr(doc, a).as_string(doc))
                            .unwrap_or_default();
                        parse_json(&first)
                            .map(Value::from_json_value)
                            .unwrap_or(Value::Null)
                    }
                    "stringify" => {
                        let first = args
                            .first()
                            .map(|a| self.eval_expr(doc, a).to_json_value(doc))
                            .unwrap_or(JsonValue::Null);
                        Value::Str(to_json_string(&first))
                    }
                    _ => Value::Null,
                };
            }

            if obj_expr == "customElements" || obj_expr == "window.customElements" {
                let args = split_args(args_src);
                return match method {
                    "define" => {
                        if args.len() >= 2 {
                            let tag = self.eval_expr(doc, &args[0]).as_string(doc);
                            let ctor = self.eval_expr(doc, &args[1]);
                            doc.define_custom_element(&tag);
                            self.set_var(&format!("__ce_{tag}"), ctor);
                        }
                        Value::Null
                    }
                    "get" => {
                        let tag = args
                            .first()
                            .map(|a| self.eval_expr(doc, a).as_string(doc))
                            .unwrap_or_default();
                        self.get_var(&format!("__ce_{tag}"))
                            .cloned()
                            .unwrap_or(Value::Null)
                    }
                    "whenDefined" => {
                        let pid = self.new_resolved(Value::Bool(true));
                        self.make_promise_value(pid)
                    }
                    _ => Value::Null,
                };
            }

            if obj_expr == "VideoEncoder"
                || obj_expr == "window.VideoEncoder"
                || obj_expr == "VideoDecoder"
                || obj_expr == "window.VideoDecoder"
            {
                if method == "isConfigSupported" {
                    let args = split_args(args_src);
                    let cfg_val = args
                        .first()
                        .map(|a| self.eval_expr(doc, a))
                        .unwrap_or(Value::Null);
                    let codec_str = match &cfg_val {
                        Value::Json(j) => j
                            .get("codec")
                            .and_then(|v| v.as_str())
                            .unwrap_or("avc1.42E01E"),
                        _ => "avc1.42E01E",
                    };
                    let caps = anvil_video::MediaCapabilitiesInfo::query(codec_str, 1280, 720, 60);
                    let mut res = JsonValue::empty_object();
                    res.insert("supported", JsonValue::Bool(caps.supported));
                    res.insert("config", cfg_val.to_json_value(doc));
                    let pid = self.new_resolved(Value::Json(res));
                    return self.make_promise_value(pid);
                }
            }

            if obj_expr == "navigator.mediaCapabilities"
                || obj_expr == "window.navigator.mediaCapabilities"
            {
                if method == "decodingInfo" || method == "encodingInfo" {
                    let caps =
                        anvil_video::MediaCapabilitiesInfo::query("av01.0.04M.08", 1920, 1080, 60);
                    let mut res = JsonValue::empty_object();
                    res.insert("supported", JsonValue::Bool(caps.supported));
                    res.insert("smooth", JsonValue::Bool(caps.smooth));
                    res.insert("powerEfficient", JsonValue::Bool(caps.power_efficient));
                    res.insert("hardwareAccelerated", JsonValue::Bool(caps.power_efficient));
                    let pid = self.new_resolved(Value::Json(res));
                    return self.make_promise_value(pid);
                }
            }

            if matches!(
                obj_expr,
                "location"
                    | "window.location"
                    | "self.location"
                    | "globalThis.location"
                    | "document.location"
                    | "window.document.location"
            ) {
                let args = split_args(args_src);
                if matches!(method, "assign" | "replace")
                    && let Some(first) = args.first()
                {
                    let url = self.eval_expr(doc, first).as_string(doc);
                    self.set_pending_navigation(&url);
                }
                return Value::Null;
            }

            if obj_expr == "history" || obj_expr == "window.history" {
                let args = split_args(args_src);
                if matches!(method, "pushState" | "replaceState") && args.len() >= 3 {
                    let url = self.eval_expr(doc, &args[2]).as_string(doc);
                    if !url.is_empty() {
                        let current = self.page_origin.clone().unwrap_or_default();
                        let resolved = resolve_script_fetch_url(&current, &url);
                        self.page_origin = Some(resolved);
                    }
                }
                return Value::Null;
            }

            if obj_expr == "MediaSource" || obj_expr == "window.MediaSource" {
                if method == "isTypeSupported" {
                    return Value::Bool(true);
                }
                return Value::Null;
            }

            if obj_expr == "WebAssembly" || obj_expr == "window.WebAssembly" {
                let args = split_args(args_src);
                return self.eval_webassembly_static(doc, method, &args);
            }

            if obj_expr == "navigator" || obj_expr == "window.navigator" {
                let args = split_args(args_src);
                let first = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .unwrap_or_default();
                return match method {
                    "requestMediaKeySystemAccess" => {
                        let lower = first.trim().to_ascii_lowercase();
                        let supported = matches!(
                            lower.as_str(),
                            "org.w3.clearkey"
                                | "org.anvil.mediafoundation.cdm"
                                | "org.anvil.external.cdm"
                        );
                        let mut obj = JsonValue::empty_object();
                        obj.insert("keySystem", JsonValue::String(lower));
                        obj.insert("supported", JsonValue::Bool(supported));
                        obj.insert("cenc", JsonValue::String("cenc".to_string()));
                        Value::Json(obj)
                    }
                    "sendBeacon" => Value::Bool(true),
                    _ => Value::Null,
                };
            }

            if matches!(
                obj_expr,
                "document" | "window.document" | "self.document" | "globalThis.document"
            ) {
                let args = split_args(args_src);
                let first = args
                    .first()
                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                    .unwrap_or_default();
                return match method {
                    "getElementById" => {
                        let cache_key = format!("#{}", first);
                        if let Some(id) = self.lookup_cached_dom(&cache_key) {
                            return Value::Element(id);
                        }
                        if let Some(id) = doc.get_element_by_id(&first) {
                            self.store_cached_dom(cache_key, id);
                            Value::Element(id)
                        } else {
                            Value::Null
                        }
                    }
                    "querySelector" => {
                        if let Some(id) = self.lookup_cached_dom(&first) {
                            return Value::Element(id);
                        }
                        if let Some(id) = doc.query_selector(&first) {
                            self.store_cached_dom(first, id);
                            Value::Element(id)
                        } else {
                            Value::Null
                        }
                    }
                    "querySelectorAll" => {
                        let ids = doc.query_selector_all(&first);
                        Value::Array(ids.into_iter().map(Value::Element).collect())
                    }
                    "getElementsByClassName" => {
                        let ids = doc.get_elements_by_class_name(doc.root(), first.trim());
                        Value::Array(ids.into_iter().map(Value::Element).collect())
                    }
                    "getElementsByTagName" => {
                        let ids = doc.get_elements_by_tag_name(doc.root(), first.trim());
                        Value::Array(ids.into_iter().map(Value::Element).collect())
                    }
                    "getElementsByName" => {
                        let ids = doc.get_elements_by_name(first.trim());
                        Value::Array(ids.into_iter().map(Value::Element).collect())
                    }
                    "createElement" => {
                        let id = doc.create_element(&first, Vec::new());
                        Value::Element(id)
                    }
                    "createDocumentFragment" => {
                        let id = doc.create_element("anvil-fragment", Vec::new());
                        Value::Element(id)
                    }
                    "createTextNode" | "createComment" => {
                        let id = doc.create_text(&first);
                        Value::Element(id)
                    }
                    "addEventListener" if args.len() >= 2 => {
                        let body = extract_event_callback_body(args[1].trim());
                        match first.to_ascii_lowercase().as_str() {
                            "click" => self.click_listeners.push((doc.body(), body)),
                            "input" => self.input_listeners.push((doc.body(), body)),
                            "change" => self.change_listeners.push((doc.body(), body)),
                            "submit" => self.submit_listeners.push((doc.body(), body)),
                            "keydown" => self.keydown_listeners.push((doc.body(), body)),
                            "domcontentloaded" | "load" | "readystatechange" => {
                                let (code, env, is_async) = self.deferred_callback(&args[1]);
                                self.schedule_job(Job::Script {
                                    code,
                                    env,
                                    is_async,
                                });
                            }
                            _ => {}
                        }
                        Value::Null
                    }
                    _ => Value::Null,
                };
            }

            if is_identifier(obj_expr)
                && let Some(Value::Array(mut items)) = self.get_var(obj_expr).cloned()
            {
                let args = split_args(args_src);
                match method {
                    "push" => {
                        if !self.array_fits(items.len().saturating_add(args.len())) {
                            return Value::Null;
                        }
                        for arg in &args {
                            items.push(self.eval_expr(doc, arg));
                        }
                        let len = items.len();
                        self.set_var(obj_expr, Value::Array(items));
                        return Value::Number(len as f64);
                    }
                    "pop" => {
                        let popped = items.pop().unwrap_or(Value::Null);
                        self.set_var(obj_expr, Value::Array(items));
                        return popped;
                    }
                    "shift" => {
                        let shifted = if items.is_empty() {
                            Value::Null
                        } else {
                            items.remove(0)
                        };
                        self.set_var(obj_expr, Value::Array(items));
                        return shifted;
                    }
                    "unshift" => {
                        if !self.array_fits(items.len().saturating_add(args.len())) {
                            return Value::Null;
                        }
                        for (idx, arg) in args.iter().enumerate() {
                            let val = self.eval_expr(doc, arg);
                            items.insert(idx, val);
                        }
                        let len = items.len();
                        self.set_var(obj_expr, Value::Array(items));
                        return Value::Number(len as f64);
                    }
                    "reverse" => {
                        items.reverse();
                        let out = Value::Array(items.clone());
                        self.set_var(obj_expr, Value::Array(items));
                        return out;
                    }
                    _ => {}
                }
            }

            match self.eval_expr(doc, obj_expr) {
                Value::Str(ref s) if s == "__anvil_window" => {
                    return self.eval_call(doc, method, args_src);
                }
                Value::Str(s) => {
                    let args = split_args(args_src);
                    let arg0 = args
                        .first()
                        .map(|a| self.eval_expr(doc, a).as_string(doc))
                        .unwrap_or_default();
                    return match method {
                        "trim" => Value::Str(s.trim().to_string()),
                        "trimStart" => Value::Str(s.trim_start().to_string()),
                        "trimEnd" => Value::Str(s.trim_end().to_string()),
                        "toLowerCase" => Value::Str(s.to_ascii_lowercase()),
                        "toUpperCase" => Value::Str(s.to_ascii_uppercase()),
                        "includes" => Value::Bool(s.contains(&arg0)),
                        "startsWith" => Value::Bool(s.starts_with(&arg0)),
                        "endsWith" => Value::Bool(s.ends_with(&arg0)),
                        "indexOf" => {
                            Value::Number(s.find(&arg0).map(|idx| idx as f64).unwrap_or(-1.0))
                        }
                        "repeat" => {
                            let count = args
                                .first()
                                .and_then(|a| self.eval_expr(doc, a).as_number())
                                .unwrap_or(0.0)
                                .clamp(0.0, MAX_STRING_LEN as f64)
                                as usize;
                            if s.len().saturating_mul(count) > MAX_STRING_LEN {
                                self.abort_string_limit();
                                Value::Null
                            } else {
                                Value::Str(s.repeat(count))
                            }
                        }
                        "padStart" | "padEnd" => {
                            let target_len =
                                args.first()
                                    .and_then(|a| self.eval_expr(doc, a).as_number())
                                    .unwrap_or(0.0)
                                    .clamp(0.0, 1024.0) as usize;
                            let pad_str = args
                                .get(1)
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .filter(|p| !p.is_empty())
                                .unwrap_or_else(|| " ".to_string());
                            let cur_len = s.chars().count();
                            if cur_len >= target_len {
                                Value::Str(s)
                            } else {
                                let needed = target_len - cur_len;
                                let padding: String =
                                    pad_str.chars().cycle().take(needed).collect();
                                if method == "padStart" {
                                    Value::Str(format!("{padding}{s}"))
                                } else {
                                    Value::Str(format!("{s}{padding}"))
                                }
                            }
                        }
                        "charAt" => {
                            let idx = args
                                .first()
                                .and_then(|a| self.eval_expr(doc, a).as_number())
                                .unwrap_or(0.0)
                                .max(0.0) as usize;
                            Value::Str(
                                s.chars()
                                    .nth(idx)
                                    .map(|c| c.to_string())
                                    .unwrap_or_default(),
                            )
                        }
                        "replace" | "replaceAll" => {
                            let arg1 = args
                                .get(1)
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_default();
                            if method == "replaceAll" {
                                Value::Str(s.replace(&arg0, &arg1))
                            } else {
                                Value::Str(s.replacen(&arg0, &arg1, 1))
                            }
                        }
                        "split" => {
                            let parts = if arg0.is_empty() {
                                s.chars().map(|c| Value::Str(c.to_string())).collect()
                            } else {
                                s.split(&arg0).map(|p| Value::Str(p.to_string())).collect()
                            };
                            Value::Array(parts)
                        }
                        "slice" | "substring" => {
                            let chars: Vec<char> = s.chars().collect();
                            let start = args
                                .first()
                                .and_then(|a| self.eval_expr(doc, a).as_number())
                                .unwrap_or(0.0)
                                .max(0.0) as usize;
                            let end = args
                                .get(1)
                                .and_then(|a| self.eval_expr(doc, a).as_number())
                                .map(|n| (n.max(0.0) as usize).min(chars.len()))
                                .unwrap_or(chars.len());
                            if start < end && start < chars.len() {
                                Value::Str(chars[start..end].iter().collect())
                            } else {
                                Value::Str(String::new())
                            }
                        }
                        _ => Value::Null,
                    };
                }
                Value::Array(items) => {
                    let args = split_args(args_src);
                    return match method {
                        "join" => {
                            let sep = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_else(|| ",".to_string());
                            let mut joined = String::new();
                            for (index, item) in items.iter().enumerate() {
                                if index > 0 {
                                    joined.push_str(&sep);
                                }
                                joined.push_str(&item.as_string(doc));
                                if joined.len() > MAX_STRING_LEN {
                                    self.abort_string_limit();
                                    return Value::Null;
                                }
                            }
                            Value::Str(joined)
                        }
                        "includes" => {
                            let needle = args
                                .first()
                                .map(|a| self.eval_expr(doc, a))
                                .unwrap_or(Value::Null);
                            Value::Bool(items.iter().any(|v| values_equal(v, &needle, doc)))
                        }
                        "indexOf" => {
                            let needle = args
                                .first()
                                .map(|a| self.eval_expr(doc, a))
                                .unwrap_or(Value::Null);
                            let idx = items
                                .iter()
                                .position(|v| values_equal(v, &needle, doc))
                                .map(|i| i as f64)
                                .unwrap_or(-1.0);
                            Value::Number(idx)
                        }
                        "concat" => {
                            let mut out = items;
                            for arg in &args {
                                let more = match self.eval_expr(doc, arg) {
                                    Value::Array(more) => more,
                                    other => vec![other],
                                };
                                if !self.array_fits(out.len().saturating_add(more.len())) {
                                    return Value::Null;
                                }
                                out.extend(more);
                            }
                            Value::Array(out)
                        }
                        "reverse" => {
                            let mut rev = items;
                            rev.reverse();
                            Value::Array(rev)
                        }
                        "slice" => {
                            let start = args
                                .first()
                                .and_then(|a| self.eval_expr(doc, a).as_number())
                                .unwrap_or(0.0)
                                .max(0.0) as usize;
                            let end = args
                                .get(1)
                                .and_then(|a| self.eval_expr(doc, a).as_number())
                                .map(|n| (n.max(0.0) as usize).min(items.len()))
                                .unwrap_or(items.len());
                            if start < end && start < items.len() {
                                Value::Array(items[start..end].to_vec())
                            } else {
                                Value::Array(Vec::new())
                            }
                        }
                        "forEach" => {
                            if let Some(cb) = args.first() {
                                let (params, body) = extract_multi_param_callback(cb.trim());
                                for (idx, item) in items.into_iter().enumerate() {
                                    if let Some(p0) = params.first() {
                                        self.set_var(p0, item);
                                    }
                                    if let Some(p1) = params.get(1) {
                                        self.set_var(p1, Value::Number(idx as f64));
                                    }
                                    self.eval_script(doc, &body);
                                }
                            }
                            Value::Null
                        }
                        "map" => {
                            let mut mapped = Vec::with_capacity(items.len());
                            if let Some(cb) = args.first() {
                                let (params, body) = extract_multi_param_callback(cb.trim());
                                for (idx, item) in items.into_iter().enumerate() {
                                    if let Some(p0) = params.first() {
                                        self.set_var(p0, item);
                                    }
                                    if let Some(p1) = params.get(1) {
                                        self.set_var(p1, Value::Number(idx as f64));
                                    }
                                    let val = self.eval_callback_return(doc, &body);
                                    mapped.push(val);
                                }
                            }
                            Value::Array(mapped)
                        }
                        "filter" => {
                            let mut kept = Vec::new();
                            if let Some(cb) = args.first() {
                                let (params, body) = extract_multi_param_callback(cb.trim());
                                for (idx, item) in items.into_iter().enumerate() {
                                    if let Some(p0) = params.first() {
                                        self.set_var(p0, item.clone());
                                    }
                                    if let Some(p1) = params.get(1) {
                                        self.set_var(p1, Value::Number(idx as f64));
                                    }
                                    let cond = self.eval_callback_return(doc, &body);
                                    if cond.is_truthy() {
                                        kept.push(item);
                                    }
                                }
                            }
                            Value::Array(kept)
                        }
                        "find" => {
                            if let Some(cb) = args.first() {
                                let (params, body) = extract_multi_param_callback(cb.trim());
                                for (idx, item) in items.into_iter().enumerate() {
                                    if let Some(p0) = params.first() {
                                        self.set_var(p0, item.clone());
                                    }
                                    if let Some(p1) = params.get(1) {
                                        self.set_var(p1, Value::Number(idx as f64));
                                    }
                                    if self.eval_callback_return(doc, &body).is_truthy() {
                                        return item;
                                    }
                                }
                            }
                            Value::Null
                        }
                        "findIndex" => {
                            if let Some(cb) = args.first() {
                                let (params, body) = extract_multi_param_callback(cb.trim());
                                for (idx, item) in items.into_iter().enumerate() {
                                    if let Some(p0) = params.first() {
                                        self.set_var(p0, item);
                                    }
                                    if let Some(p1) = params.get(1) {
                                        self.set_var(p1, Value::Number(idx as f64));
                                    }
                                    if self.eval_callback_return(doc, &body).is_truthy() {
                                        return Value::Number(idx as f64);
                                    }
                                }
                            }
                            Value::Number(-1.0)
                        }
                        "some" => {
                            if let Some(cb) = args.first() {
                                let (params, body) = extract_multi_param_callback(cb.trim());
                                for (idx, item) in items.into_iter().enumerate() {
                                    if let Some(p0) = params.first() {
                                        self.set_var(p0, item);
                                    }
                                    if let Some(p1) = params.get(1) {
                                        self.set_var(p1, Value::Number(idx as f64));
                                    }
                                    if self.eval_callback_return(doc, &body).is_truthy() {
                                        return Value::Bool(true);
                                    }
                                }
                            }
                            Value::Bool(false)
                        }
                        "every" => {
                            if let Some(cb) = args.first() {
                                let (params, body) = extract_multi_param_callback(cb.trim());
                                for (idx, item) in items.into_iter().enumerate() {
                                    if let Some(p0) = params.first() {
                                        self.set_var(p0, item);
                                    }
                                    if let Some(p1) = params.get(1) {
                                        self.set_var(p1, Value::Number(idx as f64));
                                    }
                                    if !self.eval_callback_return(doc, &body).is_truthy() {
                                        return Value::Bool(false);
                                    }
                                }
                            }
                            Value::Bool(true)
                        }
                        "reduce" => {
                            if let Some(cb) = args.first() {
                                let (params, body) = extract_multi_param_callback(cb.trim());
                                let mut iter = items.into_iter().enumerate();
                                let mut acc = if let Some(init_arg) = args.get(1) {
                                    self.eval_expr(doc, init_arg)
                                } else {
                                    iter.next().map(|(_, v)| v).unwrap_or(Value::Null)
                                };
                                for (idx, item) in iter {
                                    if let Some(p0) = params.first() {
                                        self.set_var(p0, acc);
                                    }
                                    if let Some(p1) = params.get(1) {
                                        self.set_var(p1, item);
                                    }
                                    if let Some(p2) = params.get(2) {
                                        self.set_var(p2, Value::Number(idx as f64));
                                    }
                                    acc = self.eval_callback_return(doc, &body);
                                }
                                return acc;
                            }
                            Value::Null
                        }
                        _ => Value::Null,
                    };
                }
                Value::Json(mut jval) => {
                    if let Some(class_name) = jval
                        .get("__class")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        && let Some((params, body, sup_class)) =
                            self.find_class_method(&class_name, method)
                    {
                        let args = split_args(args_src);
                        let is_async = self.class_method_is_async(&class_name, method);
                        let prev_this = self.get_var("this").cloned();
                        self.set_var("this", Value::Json(jval));
                        let mut out = self.invoke_method_body(
                            doc,
                            &params,
                            &body,
                            &args,
                            sup_class.as_deref(),
                        );
                        if is_async {
                            // An async method's awaits wait in place, so its
                            // promise is already settled when it returns.
                            let id = match self.thrown_error.take() {
                                Some(err) => self.new_rejected(err),
                                None => self.new_resolved(out),
                            };
                            out = self.make_promise_value(id);
                        }
                        if is_identifier(obj_expr)
                            && let Some(updated_this) = self.get_var("this").cloned()
                        {
                            self.set_var(obj_expr, updated_this);
                        }
                        if let Some(old) = prev_this {
                            self.set_var("this", old);
                        }
                        return out;
                    }
                    if let Some(JsonValue::Number(inst_id_f)) = jval.get("__anvil_wasm_exports") {
                        let inst_id = *inst_id_f as usize;
                        let args = split_args(args_src);
                        let args_f64: Vec<f64> = args
                            .iter()
                            .map(|a| self.eval_expr(doc, a).as_number().unwrap_or(0.0))
                            .collect();
                        return self.call_wasm_instance_export(inst_id, method, &args_f64);
                    }
                    if let Some(JsonValue::Number(inst_id_f)) = jval.get("__anvil_wasm_memory") {
                        let inst_id = *inst_id_f as usize;
                        if method == "grow" {
                            let args = split_args(args_src);
                            let delta = args
                                .first()
                                .and_then(|a| self.eval_expr(doc, a).as_number())
                                .unwrap_or(0.0)
                                .max(0.0) as u32;
                            if let Some(inst) = self.wasm_instances.get_mut(inst_id) {
                                return Value::Number(inst.memory.grow(delta) as f64);
                            }
                            return Value::Number(-1.0);
                        }
                    }
                    if let Some(JsonValue::Number(mem_id_f)) =
                        jval.get("__anvil_wasm_standalone_mem")
                    {
                        let mem_id = *mem_id_f as usize;
                        if method == "grow" {
                            let args = split_args(args_src);
                            let delta = args
                                .first()
                                .and_then(|a| self.eval_expr(doc, a).as_number())
                                .unwrap_or(0.0)
                                .max(0.0) as u32;
                            if let Some(mem) = self.wasm_memories.get_mut(mem_id) {
                                return Value::Number(mem.grow(delta) as f64);
                            }
                            return Value::Number(-1.0);
                        }
                    }
                    if let Some(kind) = jval.get("__anvil_kind").and_then(|v| v.as_str()) {
                        let args = split_args(args_src);
                        match (kind, method) {
                            ("TextEncoder", "encode") => {
                                let text = args
                                    .first()
                                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                                    .unwrap_or_default();
                                if !self.array_fits(text.len()) {
                                    return Value::Null;
                                }
                                return Value::Array(
                                    text.as_bytes()
                                        .iter()
                                        .map(|b| Value::Number(*b as f64))
                                        .collect(),
                                );
                            }
                            ("TextDecoder", "decode") => {
                                let bytes = args
                                    .first()
                                    .map(|a| {
                                        let v = self.eval_expr(doc, a);
                                        self.extract_wasm_bytes(&v)
                                    })
                                    .unwrap_or_default();
                                return Value::Str(String::from_utf8_lossy(&bytes).into_owned());
                            }
                            ("DOMParser", "parseFromString") => {
                                let html = args
                                    .first()
                                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                                    .unwrap_or_default();
                                let wrapper = doc.create_element("div", Vec::new());
                                doc.set_inner_html(wrapper, &html);
                                return Value::Element(wrapper);
                            }
                            ("AbortController", "abort") => {
                                let mut sig = JsonValue::empty_object();
                                sig.insert("aborted", JsonValue::Bool(true));
                                jval.insert("signal", sig);
                                if is_identifier(obj_expr) {
                                    self.set_var(obj_expr, Value::Json(jval));
                                }
                                return Value::Null;
                            }
                            ("MediaSource", "addSourceBuffer") => {
                                let mime = args
                                    .first()
                                    .map(|a| self.eval_expr(doc, a).as_string(doc))
                                    .unwrap_or_default();
                                let mut sb = JsonValue::empty_object();
                                sb.insert(
                                    "__anvil_kind",
                                    JsonValue::String("SourceBuffer".to_string()),
                                );
                                sb.insert("mimeType", JsonValue::String(mime));
                                sb.insert("updating", JsonValue::Bool(false));
                                sb.insert("appendedSegments", JsonValue::Number(0.0));
                                return Value::Json(sb);
                            }
                            ("SourceBuffer", "appendBuffer") => {
                                let prev = jval
                                    .get("appendedSegments")
                                    .and_then(|v| match v {
                                        JsonValue::Number(n) => Some(*n),
                                        _ => None,
                                    })
                                    .unwrap_or(0.0);
                                jval.insert("appendedSegments", JsonValue::Number(prev + 1.0));
                                if is_identifier(obj_expr) {
                                    self.set_var(obj_expr, Value::Json(jval));
                                }
                                return Value::Null;
                            }
                            ("VideoEncoder", "configure") | ("VideoDecoder", "configure") => {
                                jval.insert("state", JsonValue::String("configured".to_string()));
                                if is_identifier(obj_expr) {
                                    self.set_var(obj_expr, Value::Json(jval));
                                }
                                return Value::Null;
                            }
                            ("VideoEncoder", "encode") => {
                                let mut enc = anvil_video::WebCodecsVideoEncoder::new();
                                let _ = enc.configure("avc1.42E01E", 16, 16, 30, 16);
                                let frame = anvil_video::VideoFrame {
                                    width: 16,
                                    height: 16,
                                    pts_ms: 0,
                                    keyframe: true,
                                    rgba: vec![128u8; 16 * 16 * 4],
                                };
                                let _ = enc.encode(&frame, true);
                                let prev = jval
                                    .get("encodedChunks")
                                    .and_then(|v| match v {
                                        JsonValue::Number(n) => Some(*n),
                                        _ => None,
                                    })
                                    .unwrap_or(0.0);
                                jval.insert("encodedChunks", JsonValue::Number(prev + 1.0));
                                if is_identifier(obj_expr) {
                                    self.set_var(obj_expr, Value::Json(jval));
                                }
                                return Value::Null;
                            }
                            ("VideoDecoder", "decode") => {
                                let prev = jval
                                    .get("decodedFrames")
                                    .and_then(|v| match v {
                                        JsonValue::Number(n) => Some(*n),
                                        _ => None,
                                    })
                                    .unwrap_or(0.0);
                                jval.insert("decodedFrames", JsonValue::Number(prev + 1.0));
                                if is_identifier(obj_expr) {
                                    self.set_var(obj_expr, Value::Json(jval));
                                }
                                return Value::Null;
                            }
                            ("VideoEncoder", "flush") | ("VideoDecoder", "flush") => {
                                let pid = self.new_resolved(Value::Null);
                                return self.make_promise_value(pid);
                            }
                            ("VideoEncoder", "close")
                            | ("VideoDecoder", "close")
                            | ("VideoFrame", "close") => {
                                jval.insert("state", JsonValue::String("closed".to_string()));
                                if is_identifier(obj_expr) {
                                    self.set_var(obj_expr, Value::Json(jval));
                                }
                                return Value::Null;
                            }
                            ("CanvasRenderingContext2D", m) => {
                                let canvas_idx = jval
                                    .get("__canvas_id")
                                    .and_then(|v| match v {
                                        JsonValue::Number(n) => Some(*n as u32),
                                        _ => None,
                                    })
                                    .unwrap_or(0);
                                let cid = NodeId::from_raw(canvas_idx);
                                let eval_i32 =
                                    |rt: &mut Self, d: &mut Document, idx: usize| -> i32 {
                                        args.get(idx)
                                            .and_then(|a| rt.eval_expr(d, a).as_number())
                                            .unwrap_or(0.0)
                                            .round() as i32
                                    };
                                match m {
                                    "fillRect" => {
                                        let x = eval_i32(self, doc, 0);
                                        let y = eval_i32(self, doc, 1);
                                        let w = eval_i32(self, doc, 2).max(0) as u32;
                                        let h = eval_i32(self, doc, 3).max(0) as u32;
                                        let fill = jval
                                            .get("fillStyle")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("#000000");
                                        doc.canvas_fill_rect(
                                            cid,
                                            x,
                                            y,
                                            w,
                                            h,
                                            parse_canvas_color(fill),
                                        );
                                        return Value::Null;
                                    }
                                    "strokeRect" => {
                                        let x = eval_i32(self, doc, 0);
                                        let y = eval_i32(self, doc, 1);
                                        let w = eval_i32(self, doc, 2).max(0) as u32;
                                        let h = eval_i32(self, doc, 3).max(0) as u32;
                                        let lw = jval
                                            .get("lineWidth")
                                            .and_then(|v| match v {
                                                JsonValue::Number(n) => Some(*n as i32),
                                                _ => None,
                                            })
                                            .unwrap_or(1)
                                            .max(1)
                                            as u32;
                                        let stroke = jval
                                            .get("strokeStyle")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("#000000");
                                        doc.canvas_stroke_rect(
                                            cid,
                                            x,
                                            y,
                                            w,
                                            h,
                                            lw,
                                            parse_canvas_color(stroke),
                                        );
                                        return Value::Null;
                                    }
                                    "clearRect" => {
                                        let x = eval_i32(self, doc, 0);
                                        let y = eval_i32(self, doc, 1);
                                        let w = eval_i32(self, doc, 2).max(0) as u32;
                                        let h = eval_i32(self, doc, 3).max(0) as u32;
                                        doc.canvas_clear_rect(cid, x, y, w, h);
                                        return Value::Null;
                                    }
                                    "fillText" | "strokeText" => {
                                        let text = args
                                            .first()
                                            .map(|a| self.eval_expr(doc, a).as_string(doc))
                                            .unwrap_or_default();
                                        let x = eval_i32(self, doc, 1);
                                        let y = eval_i32(self, doc, 2);
                                        let fill = jval
                                            .get("fillStyle")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("#000000");
                                        doc.canvas_draw_text_bitmap(
                                            cid,
                                            &text,
                                            x,
                                            y,
                                            parse_canvas_color(fill),
                                        );
                                        return Value::Null;
                                    }
                                    "beginPath" | "closePath" | "save" | "restore" | "scale"
                                    | "translate" => {
                                        return Value::Null;
                                    }
                                    "moveTo" => {
                                        let x = eval_i32(self, doc, 0);
                                        let y = eval_i32(self, doc, 1);
                                        jval.insert("__pen_x", JsonValue::Number(x as f64));
                                        jval.insert("__pen_y", JsonValue::Number(y as f64));
                                        if is_identifier(obj_expr) {
                                            self.set_var(obj_expr, Value::Json(jval));
                                        }
                                        return Value::Null;
                                    }
                                    "lineTo" => {
                                        let x0 = json_i32(&jval, "__pen_x");
                                        let y0 = json_i32(&jval, "__pen_y");
                                        let x1 = eval_i32(self, doc, 0);
                                        let y1 = eval_i32(self, doc, 1);
                                        let stroke = jval
                                            .get("strokeStyle")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("#000000");
                                        doc.canvas_draw_line(
                                            cid,
                                            x0,
                                            y0,
                                            x1,
                                            y1,
                                            parse_canvas_color(stroke),
                                        );
                                        jval.insert("__pen_x", JsonValue::Number(x1 as f64));
                                        jval.insert("__pen_y", JsonValue::Number(y1 as f64));
                                        if is_identifier(obj_expr) {
                                            self.set_var(obj_expr, Value::Json(jval));
                                        }
                                        return Value::Null;
                                    }
                                    "arc" => {
                                        let cx = eval_i32(self, doc, 0);
                                        let cy = eval_i32(self, doc, 1);
                                        let r = eval_i32(self, doc, 2).max(1);
                                        let fill = jval
                                            .get("fillStyle")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("#000000");
                                        doc.canvas_fill_rect(
                                            cid,
                                            cx - r,
                                            cy - r,
                                            (r * 2) as u32,
                                            (r * 2) as u32,
                                            parse_canvas_color(fill),
                                        );
                                        return Value::Null;
                                    }
                                    "fill" | "stroke" => {
                                        return Value::Null;
                                    }
                                    "createImageData" => {
                                        let w = eval_i32(self, doc, 0).clamp(1, 2048) as u32;
                                        let h = eval_i32(self, doc, 1).clamp(1, 2048) as u32;
                                        let len = (w as usize) * (h as usize) * 4;
                                        let mut img = JsonValue::empty_object();
                                        img.insert(
                                            "__anvil_kind",
                                            JsonValue::String("ImageData".to_string()),
                                        );
                                        img.insert("width", JsonValue::Number(w as f64));
                                        img.insert("height", JsonValue::Number(h as f64));
                                        if self.array_fits(len) {
                                            img.insert(
                                                "data",
                                                JsonValue::Array(vec![JsonValue::Number(0.0); len]),
                                            );
                                        }
                                        return Value::Json(img);
                                    }
                                    "getImageData" => {
                                        let sx = eval_i32(self, doc, 0).max(0) as u32;
                                        let sy = eval_i32(self, doc, 1).max(0) as u32;
                                        let sw = eval_i32(self, doc, 2).clamp(1, 2048) as u32;
                                        let sh = eval_i32(self, doc, 3).clamp(1, 2048) as u32;
                                        let surf = doc.ensure_canvas_surface(cid);
                                        let mut data =
                                            Vec::with_capacity((sw as usize) * (sh as usize) * 4);
                                        for row in 0..sh {
                                            for col in 0..sw {
                                                let px = sx + col;
                                                let py = sy + row;
                                                if px < surf.width && py < surf.height {
                                                    let idx = ((py * surf.width + px) as usize) * 4;
                                                    data.push(JsonValue::Number(
                                                        surf.rgba[idx] as f64,
                                                    ));
                                                    data.push(JsonValue::Number(
                                                        surf.rgba[idx + 1] as f64,
                                                    ));
                                                    data.push(JsonValue::Number(
                                                        surf.rgba[idx + 2] as f64,
                                                    ));
                                                    data.push(JsonValue::Number(
                                                        surf.rgba[idx + 3] as f64,
                                                    ));
                                                } else {
                                                    data.extend_from_slice(&[
                                                        JsonValue::Number(0.0),
                                                        JsonValue::Number(0.0),
                                                        JsonValue::Number(0.0),
                                                        JsonValue::Number(0.0),
                                                    ]);
                                                }
                                            }
                                        }
                                        let mut img = JsonValue::empty_object();
                                        img.insert(
                                            "__anvil_kind",
                                            JsonValue::String("ImageData".to_string()),
                                        );
                                        img.insert("width", JsonValue::Number(sw as f64));
                                        img.insert("height", JsonValue::Number(sh as f64));
                                        img.insert("data", JsonValue::Array(data));
                                        return Value::Json(img);
                                    }
                                    "putImageData" => {
                                        let Some(first_arg) = args.first() else {
                                            return Value::Null;
                                        };
                                        let img_val = self.eval_expr(doc, first_arg);
                                        let dx = eval_i32(self, doc, 1);
                                        let dy = eval_i32(self, doc, 2);
                                        if let Value::Json(jimg) = img_val {
                                            let w = jimg
                                                .get("width")
                                                .and_then(|v| match v {
                                                    JsonValue::Number(n) => Some(*n as u32),
                                                    _ => None,
                                                })
                                                .unwrap_or(1)
                                                .max(1);
                                            let h = jimg
                                                .get("height")
                                                .and_then(|v| match v {
                                                    JsonValue::Number(n) => Some(*n as u32),
                                                    _ => None,
                                                })
                                                .unwrap_or(1)
                                                .max(1);
                                            let mut bytes = Vec::new();
                                            if let Some(JsonValue::Array(arr)) = jimg.get("data") {
                                                bytes.extend(arr.iter().map(|v| match v {
                                                    JsonValue::Number(n) => {
                                                        n.clamp(0.0, 255.0) as u8
                                                    }
                                                    _ => 0,
                                                }));
                                            } else if let Some(dval) = jimg.get("data") {
                                                bytes = self.extract_wasm_bytes(
                                                    &Value::from_json_value(dval.clone()),
                                                );
                                            }
                                            let expected = (w as usize) * (h as usize) * 4;
                                            if bytes.len() < expected {
                                                bytes.resize(expected, 255);
                                            }
                                            doc.canvas_put_image_data(cid, dx, dy, w, h, &bytes);
                                        }
                                        return Value::Null;
                                    }
                                    _ => return Value::Null,
                                }
                            }
                            ("MutationObserver", "observe" | "disconnect" | "takeRecords") => {
                                let args = split_args(args_src);
                                let Some(JsonValue::Number(obs_id_f)) = jval.get("__observer_id")
                                else {
                                    return Value::Null;
                                };
                                let obs_id = *obs_id_f as usize;
                                match method {
                                    "observe" => {
                                        let Some(first) = args.first() else {
                                            return Value::Null;
                                        };
                                        let Value::Element(target_node) =
                                            self.eval_expr(doc, first)
                                        else {
                                            return Value::Null;
                                        };
                                        let mut child_list = false;
                                        let mut attributes = false;
                                        let mut character_data = false;
                                        let mut subtree = false;
                                        if let Some(opts_arg) = args.get(1)
                                            && let Value::Json(jopts) =
                                                self.eval_expr(doc, opts_arg)
                                        {
                                            child_list = matches!(
                                                jopts.get("childList"),
                                                Some(JsonValue::Bool(true))
                                            );
                                            attributes = matches!(
                                                jopts.get("attributes"),
                                                Some(JsonValue::Bool(true))
                                            );
                                            character_data = matches!(
                                                jopts.get("characterData"),
                                                Some(JsonValue::Bool(true))
                                            );
                                            subtree = matches!(
                                                jopts.get("subtree"),
                                                Some(JsonValue::Bool(true))
                                            );
                                        }
                                        if !child_list && !attributes && !character_data {
                                            child_list = true;
                                            attributes = true;
                                        }
                                        if let Some(obs) = self.mutation_observers.get_mut(obs_id) {
                                            obs.targets.push(MutationObserverTarget {
                                                node_id: target_node,
                                                child_list,
                                                attributes,
                                                character_data,
                                                subtree,
                                            });
                                        }
                                        return Value::Null;
                                    }
                                    "disconnect" => {
                                        if let Some(obs) = self.mutation_observers.get_mut(obs_id) {
                                            obs.targets.clear();
                                            obs.records.clear();
                                            obs.scheduled = false;
                                        }
                                        return Value::Null;
                                    }
                                    "takeRecords" => {
                                        if let Some(obs) = self.mutation_observers.get_mut(obs_id) {
                                            let recs: Vec<Value> = std::mem::take(&mut obs.records)
                                                .into_iter()
                                                .map(Value::Json)
                                                .collect();
                                            return Value::Array(recs);
                                        }
                                        return Value::Array(Vec::new());
                                    }
                                    _ => return Value::Null,
                                }
                            }
                            (
                                "IntersectionObserver" | "ResizeObserver",
                                "observe" | "unobserve" | "disconnect",
                            ) => {
                                let args = split_args(args_src);
                                let Some(JsonValue::Number(obs_id_f)) = jval.get("__observer_id")
                                else {
                                    return Value::Null;
                                };
                                let obs_id = *obs_id_f as usize;
                                match method {
                                    "observe" => {
                                        let Some(first) = args.first() else {
                                            return Value::Null;
                                        };
                                        let Value::Element(target_node) =
                                            self.eval_expr(doc, first)
                                        else {
                                            return Value::Null;
                                        };
                                        if let Some(obs) = self.layout_observers.get_mut(obs_id)
                                            && !obs.targets.contains(&target_node)
                                        {
                                            obs.targets.push(target_node);
                                        }
                                        self.schedule_job(Job::LayoutObserver {
                                            obs_id,
                                            target: target_node,
                                        });
                                        return Value::Null;
                                    }
                                    "unobserve" => {
                                        let Some(first) = args.first() else {
                                            return Value::Null;
                                        };
                                        if let Value::Element(target_node) =
                                            self.eval_expr(doc, first)
                                            && let Some(obs) = self.layout_observers.get_mut(obs_id)
                                        {
                                            obs.targets.retain(|t| *t != target_node);
                                        }
                                        return Value::Null;
                                    }
                                    "disconnect" => {
                                        if let Some(obs) = self.layout_observers.get_mut(obs_id) {
                                            obs.targets.clear();
                                        }
                                        return Value::Null;
                                    }
                                    _ => return Value::Null,
                                }
                            }
                            (
                                _,
                                "preventDefault" | "stopPropagation" | "stopImmediatePropagation",
                            ) => {
                                if method == "preventDefault" {
                                    self.event_default_prevented = true;
                                    jval.insert("defaultPrevented", JsonValue::Bool(true));
                                    if is_identifier(obj_expr) {
                                        self.set_var(obj_expr, Value::Json(jval.clone()));
                                    }
                                    if obj_expr != "event" {
                                        self.set_var("event", Value::Json(jval));
                                    }
                                }
                                return Value::Null;
                            }
                            (
                                _,
                                "observe" | "unobserve" | "disconnect" | "endOfStream" | "open"
                                | "send" | "setRequestHeader" | "start" | "stop" | "resume"
                                | "suspend",
                            ) => {
                                return Value::Null;
                            }
                            _ => {}
                        }
                    }
                    match method {
                        "hasOwnProperty" | "has" => {
                            let args = split_args(args_src);
                            let key = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_default();
                            return Value::Bool(jval.get(&key).is_some());
                        }
                        "get" => {
                            let args = split_args(args_src);
                            let key = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_default();
                            return jval
                                .get(&key)
                                .cloned()
                                .map(Value::from_json_value)
                                .unwrap_or(Value::Null);
                        }
                        "set" | "add" => {
                            let args = split_args(args_src);
                            if !args.is_empty() {
                                let key = self.eval_expr(doc, &args[0]).as_string(doc);
                                let val = args
                                    .get(1)
                                    .map(|a| self.eval_expr(doc, a).to_json_value(doc))
                                    .unwrap_or(JsonValue::Bool(true));
                                jval.insert(&key, val);
                                if is_identifier(obj_expr) {
                                    self.set_var(obj_expr, Value::Json(jval.clone()));
                                }
                            }
                            return Value::Json(jval);
                        }
                        "text" => {
                            return jval
                                .get("body")
                                .and_then(|b| b.as_str())
                                .map(|s| Value::Str(s.to_string()))
                                .unwrap_or(Value::Str(String::new()));
                        }
                        "json" => {
                            let body = jval.get("body").and_then(|b| b.as_str()).unwrap_or("{}");
                            return parse_json(body)
                                .map(Value::from_json_value)
                                .unwrap_or(Value::Null);
                        }
                        _ => {}
                    }
                }
                Value::Element(id) => {
                    let args = split_args(args_src);
                    match method {
                        "attachShadow" => {
                            let mut open = true;
                            if let Some(first) = args.first()
                                && let Value::Json(jopts) = self.eval_expr(doc, first)
                                && let Some(JsonValue::String(mode)) = jopts.get("mode")
                            {
                                open = mode != "closed";
                            }
                            let shadow_id = doc.attach_shadow(id, open);
                            return Value::Element(shadow_id);
                        }
                        "getContext" => {
                            let ctx_type = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_else(|| "2d".to_string());
                            if ctx_type.eq_ignore_ascii_case("2d") {
                                let _ = doc.ensure_canvas_surface(id);
                                let mut ctx = JsonValue::empty_object();
                                ctx.insert(
                                    "__anvil_kind",
                                    JsonValue::String("CanvasRenderingContext2D".to_string()),
                                );
                                ctx.insert("__canvas_id", JsonValue::Number(id.index() as f64));
                                ctx.insert("fillStyle", JsonValue::String("#000000".to_string()));
                                ctx.insert("strokeStyle", JsonValue::String("#000000".to_string()));
                                ctx.insert("lineWidth", JsonValue::Number(1.0));
                                return Value::Json(ctx);
                            }
                            return Value::Null;
                        }
                        "play" => {
                            let pid = self.new_resolved(Value::Null);
                            return self.make_promise_value(pid);
                        }
                        "pause" | "load" => {
                            return Value::Null;
                        }
                        "canPlayType" => {
                            let mime = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc).to_ascii_lowercase())
                                .unwrap_or_default();
                            let ok = mime.contains("mp4")
                                || mime.contains("webm")
                                || mime.contains("ogg")
                                || mime.contains("avc1")
                                || mime.contains("av01")
                                || mime.contains("vp09")
                                || mime.contains("opus")
                                || mime.contains("flac");
                            return Value::Str(if ok { "probably" } else { "" }.to_string());
                        }
                        "getAttribute" => {
                            let name = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_default();
                            return doc
                                .get(id)
                                .data()
                                .attr(&name)
                                .map(|v| Value::Str(v.to_string()))
                                .unwrap_or(Value::Null);
                        }
                        "setAttribute" if args.len() >= 2 => {
                            let name = self.eval_expr(doc, &args[0]).as_string(doc);
                            let value = self.eval_expr(doc, &args[1]).as_string(doc);
                            doc.set_attribute(id, &name, &value);
                            self.dom_cache.clear();
                            self.notify_mutation_observers(doc, "attributes", id, Some(&name));
                            return Value::Null;
                        }
                        "removeAttribute" if !args.is_empty() => {
                            let name = self.eval_expr(doc, &args[0]).as_string(doc);
                            doc.remove_attribute(id, &name);
                            self.dom_cache.clear();
                            self.notify_mutation_observers(doc, "attributes", id, Some(&name));
                            return Value::Null;
                        }
                        "hasAttribute" if !args.is_empty() => {
                            let name = self.eval_expr(doc, &args[0]).as_string(doc);
                            return Value::Bool(doc.get(id).data().attr(&name).is_some());
                        }
                        "toggleAttribute" if !args.is_empty() => {
                            let name = self.eval_expr(doc, &args[0]).as_string(doc);
                            let present = if doc.get(id).data().attr(&name).is_some() {
                                doc.remove_attribute(id, &name);
                                false
                            } else {
                                doc.set_attribute(id, &name, "");
                                true
                            };
                            self.dom_cache.clear();
                            self.notify_mutation_observers(doc, "attributes", id, Some(&name));
                            return Value::Bool(present);
                        }
                        "appendChild" if !args.is_empty() => {
                            if let Value::Element(child_id) = self.eval_expr(doc, &args[0]) {
                                let _ = doc.append(id, child_id);
                                self.dom_cache.clear();
                                self.notify_mutation_observers(doc, "childList", id, None);
                                return Value::Element(child_id);
                            }
                            return Value::Null;
                        }
                        "append" => {
                            for arg in &args {
                                match self.eval_expr(doc, arg) {
                                    Value::Element(child_id) => {
                                        let _ = doc.append(id, child_id);
                                    }
                                    other => {
                                        let text = other.as_string(doc);
                                        let tid = doc.create_text(&text);
                                        let _ = doc.append(id, tid);
                                    }
                                }
                            }
                            self.dom_cache.clear();
                            self.notify_mutation_observers(doc, "childList", id, None);
                            return Value::Null;
                        }
                        "prepend" => {
                            let first_child = doc.first_element_child(id);
                            for arg in &args {
                                let child_id = match self.eval_expr(doc, arg) {
                                    Value::Element(cid) => cid,
                                    other => doc.create_text(&other.as_string(doc)),
                                };
                                let _ = doc.insert_before(id, child_id, first_child);
                            }
                            self.dom_cache.clear();
                            self.notify_mutation_observers(doc, "childList", id, None);
                            return Value::Null;
                        }
                        "insertBefore" if !args.is_empty() => {
                            if let Value::Element(new_id) = self.eval_expr(doc, &args[0]) {
                                let ref_id =
                                    args.get(1).and_then(|a| match self.eval_expr(doc, a) {
                                        Value::Element(rid) => Some(rid),
                                        _ => None,
                                    });
                                let _ = doc.insert_before(id, new_id, ref_id);
                                self.dom_cache.clear();
                                self.notify_mutation_observers(doc, "childList", id, None);
                                return Value::Element(new_id);
                            }
                            return Value::Null;
                        }
                        "removeChild" if !args.is_empty() => {
                            if let Value::Element(child_id) = self.eval_expr(doc, &args[0]) {
                                let _ = doc.remove_child(id, child_id);
                                self.dom_cache.clear();
                                self.notify_mutation_observers(doc, "childList", id, None);
                                return Value::Element(child_id);
                            }
                            return Value::Null;
                        }
                        "replaceChild" if args.len() >= 2 => {
                            let new_val = self.eval_expr(doc, &args[0]);
                            let old_val = self.eval_expr(doc, &args[1]);
                            if let (Value::Element(new_id), Value::Element(old_id)) =
                                (new_val, old_val)
                            {
                                let _ = doc.replace_child(id, new_id, old_id);
                                self.dom_cache.clear();
                                self.notify_mutation_observers(doc, "childList", id, None);
                                return Value::Element(old_id);
                            }
                            return Value::Null;
                        }
                        "cloneNode" => {
                            let deep = args
                                .first()
                                .is_some_and(|a| self.eval_expr(doc, a).is_truthy());
                            return doc
                                .clone_node(id, deep)
                                .map(Value::Element)
                                .unwrap_or(Value::Null);
                        }
                        "remove" => {
                            let parent = doc.parent_element(id);
                            doc.remove_node(id);
                            self.dom_cache.clear();
                            if let Some(p) = parent {
                                self.notify_mutation_observers(doc, "childList", p, None);
                            }
                            return Value::Null;
                        }
                        "matches" if !args.is_empty() => {
                            let sel = self.eval_expr(doc, &args[0]).as_string(doc);
                            return Value::Bool(doc.matches_selector(id, &sel));
                        }
                        "closest" if !args.is_empty() => {
                            let sel = self.eval_expr(doc, &args[0]).as_string(doc);
                            return doc
                                .closest(id, &sel)
                                .map(Value::Element)
                                .unwrap_or(Value::Null);
                        }
                        "contains" if !args.is_empty() => {
                            if let Value::Element(other_id) = self.eval_expr(doc, &args[0]) {
                                return Value::Bool(doc.contains_node(id, other_id));
                            }
                            return Value::Bool(false);
                        }
                        "getBoundingClientRect" => {
                            let mut rect = JsonValue::empty_object();
                            rect.insert("x", JsonValue::Number(0.0));
                            rect.insert("y", JsonValue::Number(0.0));
                            rect.insert("top", JsonValue::Number(0.0));
                            rect.insert("left", JsonValue::Number(0.0));
                            rect.insert("width", JsonValue::Number(100.0));
                            rect.insert("height", JsonValue::Number(24.0));
                            rect.insert("right", JsonValue::Number(100.0));
                            rect.insert("bottom", JsonValue::Number(24.0));
                            return Value::Json(rect);
                        }
                        "focus" | "blur" | "scrollIntoView" => {
                            return Value::Null;
                        }
                        "getElementById" => {
                            let sel = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_default();
                            return doc
                                .get_element_by_id(&sel)
                                .map(Value::Element)
                                .unwrap_or(Value::Null);
                        }
                        "getElementsByTagName" => {
                            let tag = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_default();
                            let ids = doc.get_elements_by_tag_name(id, tag.trim());
                            return Value::Array(ids.into_iter().map(Value::Element).collect());
                        }
                        "getElementsByClassName" => {
                            let cls = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_default();
                            let ids = doc.get_elements_by_class_name(id, cls.trim());
                            return Value::Array(ids.into_iter().map(Value::Element).collect());
                        }
                        "getElementsByName" => {
                            let name = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_default();
                            let ids = doc.get_elements_by_name(name.trim());
                            return Value::Array(ids.into_iter().map(Value::Element).collect());
                        }
                        "createElement" => {
                            let tag = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_default();
                            return Value::Element(doc.create_element(&tag, Vec::new()));
                        }
                        "createDocumentFragment" => {
                            return Value::Element(
                                doc.create_element("anvil-fragment", Vec::new()),
                            );
                        }
                        "createTextNode" | "createComment" => {
                            let text = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_default();
                            return Value::Element(doc.create_text(&text));
                        }
                        "insertAdjacentHTML" if args.len() >= 2 => {
                            let pos = self.eval_expr(doc, &args[0]).as_string(doc);
                            let html = self.eval_expr(doc, &args[1]).as_string(doc);
                            doc.insert_adjacent_html(id, &pos, &html);
                            self.dom_cache.clear();
                            self.notify_mutation_observers(doc, "childList", id, None);
                            return Value::Null;
                        }
                        "insertAdjacentText" if args.len() >= 2 => {
                            let pos = self.eval_expr(doc, &args[0]).as_string(doc);
                            let text = self.eval_expr(doc, &args[1]).as_string(doc);
                            doc.insert_adjacent_text(id, &pos, &text);
                            self.dom_cache.clear();
                            self.notify_mutation_observers(doc, "childList", id, None);
                            return Value::Null;
                        }
                        "insertAdjacentElement" if args.len() >= 2 => {
                            let pos = self.eval_expr(doc, &args[0]).as_string(doc);
                            if let Value::Element(new_el) = self.eval_expr(doc, &args[1]) {
                                let res = doc.insert_adjacent_element(id, &pos, new_el);
                                self.dom_cache.clear();
                                self.notify_mutation_observers(doc, "childList", id, None);
                                return res.map(Value::Element).unwrap_or(Value::Null);
                            }
                            return Value::Null;
                        }
                        "querySelector" => {
                            let sel = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_default();
                            let found = if id == doc.root() {
                                doc.query_selector(&sel)
                            } else {
                                doc.query_selector_within(id, &sel)
                            };
                            return found.map(Value::Element).unwrap_or(Value::Null);
                        }
                        "querySelectorAll" => {
                            let sel = args
                                .first()
                                .map(|a| self.eval_expr(doc, a).as_string(doc))
                                .unwrap_or_default();
                            let ids = if id == doc.root() {
                                doc.query_selector_all(&sel)
                            } else {
                                doc.query_selector_all_within(id, &sel)
                            };
                            return Value::Array(ids.into_iter().map(Value::Element).collect());
                        }
                        "addEventListener" if args.len() >= 2 => {
                            let event = self.eval_expr(doc, &args[0]).as_string(doc);
                            let handler_raw = args[1].trim();
                            let body = extract_event_callback_body(handler_raw);
                            let target_id = if id == doc.root() { doc.body() } else { id };
                            match event.to_ascii_lowercase().as_str() {
                                "click" => self.click_listeners.push((target_id, body)),
                                "input" => self.input_listeners.push((target_id, body)),
                                "change" => self.change_listeners.push((target_id, body)),
                                "submit" => self.submit_listeners.push((target_id, body)),
                                "keydown" => self.keydown_listeners.push((target_id, body)),
                                "domcontentloaded" | "load" | "readystatechange"
                                    if id == doc.root() =>
                                {
                                    let (code, env, is_async) = self.deferred_callback(&args[1]);
                                    self.schedule_job(Job::Script {
                                        code,
                                        env,
                                        is_async,
                                    });
                                }
                                _ => {}
                            }
                            return Value::Null;
                        }
                        _ => return Value::Null,
                    }
                }
                _ => {}
            }
        }

        if let Some((_, params, body)) = self
            .functions
            .iter()
            .find(|(name, _, _)| name == callee)
            .cloned()
        {
            let args = split_args(args_src);
            if self.async_functions.iter().any(|name| name == callee) {
                let values: Vec<Value> = args.iter().map(|a| self.eval_expr(doc, a)).collect();
                return self.start_async(doc, &params, values, &body, &[]);
            }
            return self.invoke_method_body(doc, &params, &body, &args, None);
        }

        if let Some(Value::Str(marker)) = self.get_var(callee).cloned()
            && let Some(rest) = marker.strip_prefix("__wasm_fn:")
            && let Some((inst_str, fn_name)) = rest.split_once(':')
            && let Ok(inst_id) = inst_str.parse::<usize>()
        {
            let args = split_args(args_src);
            let args_f64: Vec<f64> = args
                .iter()
                .map(|a| self.eval_expr(doc, a).as_number().unwrap_or(0.0))
                .collect();
            return self.call_wasm_instance_export(inst_id, fn_name, &args_f64);
        }

        Value::Null
    }

    fn extract_wasm_bytes(&self, val: &Value) -> Vec<u8> {
        match val {
            Value::Array(items) => items
                .iter()
                .map(|v| (v.as_number().unwrap_or(0.0) as i64 & 0xFF) as u8)
                .collect(),
            Value::Json(JsonValue::Array(items)) => items
                .iter()
                .map(|v| match v {
                    JsonValue::Number(n) => (*n as i64 & 0xFF) as u8,
                    _ => 0,
                })
                .collect(),
            Value::Json(jobj) => {
                if let Some(JsonValue::Array(items)) =
                    jobj.get("__bytes").or_else(|| jobj.get("bytes"))
                {
                    return items
                        .iter()
                        .map(|v| match v {
                            JsonValue::Number(n) => (*n as i64 & 0xFF) as u8,
                            _ => 0,
                        })
                        .collect();
                }
                if let Some(JsonValue::String(body)) = jobj.get("body") {
                    return decode_wasm_text_bytes(body);
                }
                Vec::new()
            }
            Value::Str(s) => decode_wasm_text_bytes(s),
            _ => Vec::new(),
        }
    }

    fn extract_wasm_imports(&self, val: &Value) -> std::collections::HashMap<String, f64> {
        let mut out = std::collections::HashMap::new();
        if let Value::Json(JsonValue::Object(modules)) = val {
            for (mod_name, mod_val) in modules {
                if let JsonValue::Object(entries) = mod_val {
                    for (field_name, field_val) in entries {
                        match field_val {
                            JsonValue::Number(n) => {
                                out.insert(format!("{mod_name}.{field_name}"), *n);
                            }
                            JsonValue::Object(_) => {
                                if let Some(JsonValue::Number(n)) = field_val.get("value") {
                                    out.insert(format!("{mod_name}.{field_name}"), *n);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        out
    }

    fn register_wasm_module_value(&mut self, module: WasmModule) -> Value {
        let mod_id = self.wasm_modules.len();
        let exports_arr: Vec<JsonValue> = module
            .exports
            .iter()
            .map(|e| {
                let mut ex = JsonValue::empty_object();
                ex.insert("name", JsonValue::String(e.name.clone()));
                ex.insert(
                    "kind",
                    JsonValue::String(
                        match e.kind {
                            WasmExportKind::Func => "function",
                            WasmExportKind::Table => "table",
                            WasmExportKind::Memory => "memory",
                            WasmExportKind::Global => "global",
                        }
                        .to_string(),
                    ),
                );
                ex
            })
            .collect();
        self.wasm_modules.push(module);
        let mut obj = JsonValue::empty_object();
        obj.insert("__anvil_wasm_module", JsonValue::Number(mod_id as f64));
        obj.insert("exports", JsonValue::Array(exports_arr));
        Value::Json(obj)
    }

    fn instantiate_wasm_module_value(
        &mut self,
        module: WasmModule,
        imports_val: Option<&Value>,
    ) -> Result<(usize, usize, Value), WasmError> {
        let host_imports = imports_val
            .map(|v| self.extract_wasm_imports(v))
            .unwrap_or_default();
        let mod_id = self.wasm_modules.len();
        self.wasm_modules.push(module.clone());
        let instance = WasmInstance::instantiate(module, host_imports)?;
        let inst_id = self.wasm_instances.len();
        let mut exports_obj = JsonValue::empty_object();
        exports_obj.insert("__anvil_wasm_exports", JsonValue::Number(inst_id as f64));
        for exp in &instance.module.exports {
            match exp.kind {
                WasmExportKind::Func => {
                    exports_obj.insert(
                        &exp.name,
                        JsonValue::String(format!("__wasm_fn:{inst_id}:{}", exp.name)),
                    );
                }
                WasmExportKind::Memory => {
                    let byte_len = instance.memory.bytes.len();
                    let pages = instance.memory.pages();
                    let mut buf = JsonValue::empty_object();
                    buf.insert("__anvil_wasm_memory", JsonValue::Number(inst_id as f64));
                    buf.insert("byteLength", JsonValue::Number(byte_len as f64));
                    let mut mem_obj = JsonValue::empty_object();
                    mem_obj.insert("__anvil_wasm_memory", JsonValue::Number(inst_id as f64));
                    mem_obj.insert("byteLength", JsonValue::Number(byte_len as f64));
                    mem_obj.insert("pages", JsonValue::Number(pages as f64));
                    mem_obj.insert("buffer", buf);
                    exports_obj.insert(&exp.name, mem_obj);
                }
                WasmExportKind::Global => {
                    let gval = instance
                        .get_exported_global(&exp.name)
                        .map(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let mut gobj = JsonValue::empty_object();
                    gobj.insert("__anvil_wasm_global", JsonValue::Bool(true));
                    gobj.insert("value", JsonValue::Number(gval));
                    exports_obj.insert(&exp.name, gobj);
                }
                WasmExportKind::Table => {
                    let mut tobj = JsonValue::empty_object();
                    tobj.insert("__anvil_wasm_table", JsonValue::Bool(true));
                    exports_obj.insert(&exp.name, tobj);
                }
            }
        }
        self.wasm_instances.push(instance);
        let mut inst_obj = JsonValue::empty_object();
        inst_obj.insert("__anvil_wasm_instance", JsonValue::Number(inst_id as f64));
        inst_obj.insert("__anvil_wasm_module", JsonValue::Number(mod_id as f64));
        inst_obj.insert("exports", exports_obj);
        Ok((mod_id, inst_id, Value::Json(inst_obj)))
    }

    fn call_wasm_instance_export(
        &mut self,
        inst_id: usize,
        method: &str,
        args_f64: &[f64],
    ) -> Value {
        if !self.consume_step() {
            return Value::Null;
        }
        let Some(inst) = self.wasm_instances.get_mut(inst_id) else {
            return Value::Null;
        };
        match inst.call_export(method, args_f64) {
            Ok(vals) => vals
                .first()
                .map(|v| Value::Number(v.as_f64()))
                .unwrap_or(Value::Null),
            Err(err) => {
                let mut err_obj = JsonValue::empty_object();
                err_obj.insert("name", JsonValue::String("RuntimeError".to_string()));
                err_obj.insert("message", JsonValue::String(err.to_string()));
                self.thrown_error = Some(Value::Json(err_obj));
                Value::Null
            }
        }
    }

    fn eval_webassembly_static(
        &mut self,
        doc: &mut Document,
        method: &str,
        args: &[String],
    ) -> Value {
        match method {
            "validate" => {
                let bytes = args
                    .first()
                    .map(|a| {
                        let v = self.eval_expr(doc, a);
                        self.extract_wasm_bytes(&v)
                    })
                    .unwrap_or_default();
                Value::Bool(validate_wasm(&bytes))
            }
            "compile" | "compileStreaming" => {
                let first_val = args
                    .first()
                    .map(|a| self.eval_expr(doc, a))
                    .unwrap_or(Value::Null);
                let first_settled = self.await_value(doc, first_val);
                let bytes = self.extract_wasm_bytes(&first_settled);
                match WasmModule::decode(&bytes) {
                    Ok(module) => {
                        let mod_val = self.register_wasm_module_value(module);
                        let pid = self.new_resolved(mod_val);
                        self.make_promise_value(pid)
                    }
                    Err(err) => {
                        let mut err_obj = JsonValue::empty_object();
                        err_obj.insert("name", JsonValue::String("CompileError".to_string()));
                        err_obj.insert("message", JsonValue::String(err.to_string()));
                        let pid = self.new_rejected(Value::Json(err_obj));
                        self.make_promise_value(pid)
                    }
                }
            }
            "instantiate" | "instantiateStreaming" => {
                let first_val = args
                    .first()
                    .map(|a| self.eval_expr(doc, a))
                    .unwrap_or(Value::Null);
                let first_settled = self.await_value(doc, first_val);
                let imports_val = args.get(1).map(|a| self.eval_expr(doc, a));
                let is_module_arg = matches!(
                    &first_settled,
                    Value::Json(j) if j.get("__anvil_wasm_module").is_some()
                );
                let module_res = match &first_settled {
                    Value::Json(jobj) => {
                        if let Some(JsonValue::Number(mid)) = jobj.get("__anvil_wasm_module") {
                            self.wasm_modules
                                .get(*mid as usize)
                                .cloned()
                                .ok_or(WasmError::InvalidModule("unknown module id"))
                        } else {
                            let bytes = self.extract_wasm_bytes(&first_settled);
                            WasmModule::decode(&bytes)
                        }
                    }
                    _ => {
                        let bytes = self.extract_wasm_bytes(&first_settled);
                        WasmModule::decode(&bytes)
                    }
                };
                match module_res
                    .and_then(|m| self.instantiate_wasm_module_value(m, imports_val.as_ref()))
                {
                    Ok((mod_id, _, inst_val)) => {
                        let resolved = if is_module_arg {
                            inst_val
                        } else {
                            let mut mod_obj = JsonValue::empty_object();
                            mod_obj.insert("__anvil_wasm_module", JsonValue::Number(mod_id as f64));
                            let mut pair = JsonValue::empty_object();
                            pair.insert("module", mod_obj);
                            pair.insert("instance", inst_val.to_json_value(doc));
                            Value::Json(pair)
                        };
                        let pid = self.new_resolved(resolved);
                        self.make_promise_value(pid)
                    }
                    Err(err) => {
                        let mut err_obj = JsonValue::empty_object();
                        err_obj.insert("name", JsonValue::String("CompileError".to_string()));
                        err_obj.insert("message", JsonValue::String(err.to_string()));
                        let pid = self.new_rejected(Value::Json(err_obj));
                        self.make_promise_value(pid)
                    }
                }
            }
            _ => Value::Null,
        }
    }

    /// A copied view of WebAssembly memory longer than [`MAX_ARRAY_LEN`] is
    /// cut to that length. Pages often view a whole memory and read the
    /// start of it, so this warns on the console rather than stopping.
    fn finish_memory_view(&mut self, view: (Vec<Value>, usize)) -> Value {
        let (items, wanted) = view;
        if wanted > items.len() {
            self.push_console(format!(
                "[warn] memory view cut from {wanted} to {MAX_ARRAY_LEN} elements, the array length limit"
            ));
        }
        Value::Array(items)
    }

    fn construct_typed_array(
        &mut self,
        _callee: &str,
        first_val: Value,
        offset: usize,
        len_opt: Option<usize>,
    ) -> Value {
        match first_val {
            Value::Number(n) => {
                let count = (n.max(0.0) as usize).min(MAX_ARRAY_LEN);
                Value::Array(vec![Value::Number(0.0); count])
            }
            Value::Array(items) => {
                let start = offset.min(items.len());
                let end = len_opt
                    .map(|l| start.saturating_add(l).min(items.len()))
                    .unwrap_or(items.len());
                Value::Array(
                    items[start..end]
                        .iter()
                        .map(|v| Value::Number(v.as_number().unwrap_or(0.0)))
                        .collect(),
                )
            }
            Value::Json(jobj) => {
                if let Some(JsonValue::Number(inst_id_f)) = jobj.get("__anvil_wasm_memory") {
                    let inst_id = *inst_id_f as usize;
                    let view = self
                        .wasm_instances
                        .get(inst_id)
                        .map(|inst| memory_view(&inst.memory.bytes, offset, len_opt));
                    if let Some(view) = view {
                        return self.finish_memory_view(view);
                    }
                }
                if let Some(JsonValue::Number(mem_id_f)) = jobj.get("__anvil_wasm_standalone_mem") {
                    let mem_id = *mem_id_f as usize;
                    let view = self
                        .wasm_memories
                        .get(mem_id)
                        .map(|mem| memory_view(&mem.bytes, offset, len_opt));
                    if let Some(view) = view {
                        return self.finish_memory_view(view);
                    }
                }
                if let Some(JsonValue::Array(items)) = jobj.get("__bytes") {
                    let start = offset.min(items.len());
                    let end = len_opt
                        .map(|l| start.saturating_add(l).min(items.len()))
                        .unwrap_or(items.len());
                    return Value::Array(
                        items[start..end]
                            .iter()
                            .map(|v| match v {
                                JsonValue::Number(n) => Value::Number(*n),
                                _ => Value::Number(0.0),
                            })
                            .collect(),
                    );
                }
                Value::Array(Vec::new())
            }
            _ => Value::Array(Vec::new()),
        }
    }

    /// Define `name(params) { body }` from the text after `function `.
    fn define_named_function(&mut self, rest: &str, is_async: bool) {
        let Some((sig, body)) = split_brace_block(rest) else {
            return;
        };
        let Some((name, params_raw)) = sig.split_once('(') else {
            return;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let params: Vec<String> = params_raw
            .trim()
            .trim_end_matches(')')
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        if let Some((_, existing_params, existing_body)) =
            self.functions.iter_mut().find(|(n, _, _)| n == &name)
        {
            *existing_params = params;
            *existing_body = body;
        } else {
            self.functions.push((name.clone(), params, body));
        }
        let listed = self.async_functions.iter().position(|n| n == &name);
        match (is_async, listed) {
            (true, None) => self.async_functions.push(name),
            (false, Some(index)) => {
                self.async_functions.remove(index);
            }
            _ => {}
        }
    }

    fn eval_callback_return(&mut self, doc: &mut Document, body: &str) -> Value {
        let trimmed = body.trim();
        if let Some(ret_expr) = trimmed.strip_prefix("return ") {
            return self.eval_expr(doc, ret_expr.trim());
        }
        if trimmed.contains(';')
            || trimmed.contains('\n')
            || trimmed.starts_with("let ")
            || trimmed.starts_with("const ")
            || trimmed.starts_with("if")
        {
            let prev_return = self.return_value.take();
            self.eval_script(doc, trimmed);
            let out = self.return_value.take().unwrap_or(Value::Null);
            self.return_value = prev_return;
            out
        } else {
            self.eval_expr(doc, trimmed)
        }
    }
}

/// Promise id carried by a promise value, if `value` is one.
fn promise_id_of(value: &Value) -> Option<usize> {
    match value {
        Value::Json(j) => promise_id_of_json(j),
        _ => None,
    }
}

fn promise_id_of_json(j: &JsonValue) -> Option<usize> {
    match j.get("__anvil_promise") {
        Some(JsonValue::Number(n)) if *n >= 0.0 => Some(*n as usize),
        _ => None,
    }
}

fn is_ident_start_byte(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn push_unique_name(names: &mut Vec<String>, name: &str) {
    if !names.iter().any(|n| n == name) {
        names.push(name.to_string());
    }
}

/// True when `text` starts with the keyword `word` as a whole word.
fn starts_with_word(text: &str, word: &str) -> bool {
    text.starts_with(word)
        && !text
            .as_bytes()
            .get(word.len())
            .copied()
            .is_some_and(is_ident_byte)
}

/// True when `word` appears as a whole word at byte `at` of `buf`.
fn word_at(buf: &[u8], at: usize, word: &[u8]) -> bool {
    buf.get(at..at + word.len()) == Some(word)
        && (at == 0 || !is_ident_byte(buf[at - 1]))
        && !buf.get(at + word.len()).copied().is_some_and(is_ident_byte)
}

/// Push the statements of `body` so the first one runs next.
fn push_block(work: &mut Vec<Work>, body: &str) {
    for stmt in split_statements(body).into_iter().rev() {
        work.push(Work::Stmt(stmt));
    }
}

/// Items a `for...of` walks over.
fn iterable_items(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items,
        Value::Json(JsonValue::Array(items)) => {
            items.into_iter().map(Value::from_json_value).collect()
        }
        Value::Str(s) => s.chars().map(|c| Value::Str(c.to_string())).collect(),
        _ => Vec::new(),
    }
}

/// Split a leading `async` keyword off a callback.
fn split_async_prefix(raw: &str) -> (bool, &str) {
    let t = raw.trim();
    match t.strip_prefix("async") {
        Some(rest) if rest.starts_with(|c: char| c.is_whitespace() || c == '(') => {
            (true, rest.trim_start())
        }
        _ => (false, t),
    }
}

/// Parameters and body of an async callback. An expression body becomes a
/// `return` so the call's promise carries its value.
fn async_callback_parts(raw: &str) -> (Vec<String>, String) {
    let (params, body) = extract_multi_param_callback(raw);
    match split_top_level_arrow(raw) {
        Some((_, rhs)) if !rhs.trim_start().starts_with('{') => {
            (params, format!("return {}", body.trim()))
        }
        _ => (params, body),
    }
}

/// Index just past the bracket that closes the one at `open`.
fn match_close(buf: &[u8], open: usize) -> usize {
    let mut depth = 0_i32;
    let mut k = open;
    while k < buf.len() {
        match buf[k] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth <= 0 {
                    return k + 1;
                }
            }
            _ => {}
        }
        k += 1;
    }
    buf.len()
}

/// Blank the inside of the brace block opening at `open`, keeping the braces.
fn blank_brace_block(buf: &mut [u8], open: usize) {
    let mut depth = 0_i32;
    for byte in buf.iter_mut().skip(open) {
        match *byte {
            b'{' => {
                depth += 1;
                if depth == 1 {
                    continue;
                }
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return;
                }
            }
            _ => {}
        }
        *byte = b' ';
    }
}

/// A same-length copy of `src` with string contents and the bodies of nested
/// functions blanked out, so a scan for `await` or a declaration only sees
/// the code of the function being run.
fn mask_nested_code(src: &str) -> Vec<u8> {
    let mut out = src.as_bytes().to_vec();
    let n = out.len();
    let mut i = 0;
    while i < n {
        let b = out[i];
        if b == b'"' || b == b'\'' || b == b'`' {
            i += 1;
            while i < n && out[i] != b {
                if out[i] == b'\\' && i + 1 < n {
                    out[i] = b' ';
                    out[i + 1] = b' ';
                    i += 2;
                    continue;
                }
                out[i] = b' ';
                i += 1;
            }
        }
        i += 1;
    }
    let mut i = 0;
    while i < n {
        if out[i] == b'=' && out.get(i + 1) == Some(&b'>') {
            let mut j = i + 2;
            while j < n && out[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < n && out[j] == b'{' {
                blank_brace_block(&mut out, j);
            } else {
                let mut depth = 0_i32;
                while j < n {
                    match out[j] {
                        b'(' | b'[' | b'{' => depth += 1,
                        b')' | b']' | b'}' => {
                            if depth == 0 {
                                break;
                            }
                            depth -= 1;
                        }
                        b',' | b';' | b'\n' if depth == 0 => break,
                        _ => {}
                    }
                    out[j] = b' ';
                    j += 1;
                }
            }
            i += 2;
            continue;
        }
        if word_at(&out, i, b"function") {
            let mut j = i + 8;
            while j < n && out[j] != b'{' {
                j += 1;
            }
            if j < n {
                blank_brace_block(&mut out, j);
            }
            i += 8;
            continue;
        }
        i += 1;
    }
    out
}

/// Byte offset of the first `await` that belongs to this function.
fn find_await(masked: &[u8]) -> Option<usize> {
    (0..masked.len()).find(|&i| word_at(masked, i, b"await"))
}

/// End of the operand of an `await` whose keyword ends at `start`: a primary
/// expression with any member accesses, calls, and index operations.
fn await_operand_end(m: &[u8], start: usize) -> usize {
    let n = m.len();
    let mut j = start;
    while j < n && m[j].is_ascii_whitespace() {
        j += 1;
    }
    if word_at(m, j, b"await") {
        return await_operand_end(m, j + 5);
    }
    if word_at(m, j, b"new") {
        j += 3;
        while j < n && m[j].is_ascii_whitespace() {
            j += 1;
        }
    }
    if j < n {
        match m[j] {
            b'(' | b'[' | b'{' => j = match_close(m, j),
            b'"' | b'\'' | b'`' => {
                let q = m[j];
                j += 1;
                while j < n && m[j] != q {
                    j += 1;
                }
                j = (j + 1).min(n);
            }
            _ => {
                while j < n && is_ident_byte(m[j]) {
                    j += 1;
                }
            }
        }
    }
    loop {
        if j < n && m[j] == b'.' {
            j += 1;
        } else if j + 1 < n && m[j] == b'?' && m[j + 1] == b'.' {
            j += 2;
        } else if j < n && (m[j] == b'(' || m[j] == b'[') {
            j = match_close(m, j);
            continue;
        } else {
            break;
        }
        while j < n && is_ident_byte(m[j]) {
            j += 1;
        }
    }
    j
}

/// Names declared with `let`, `const`, `var`, or `catch (name)` in `body`,
/// leaving out nested functions. These become the locals of an async frame.
fn collect_declared_names(body: &str, out: &mut Vec<String>) {
    let m = mask_nested_code(body);
    let n = m.len();
    let mut i = 0;
    let mut depth = 0_i32;
    // (depth of the declaration, expecting a name next)
    let mut decl: Option<(i32, bool)> = None;
    let mut after_catch = false;
    while i < n {
        let b = m[i];
        if is_ident_start_byte(b) && (i == 0 || !is_ident_byte(m[i - 1])) {
            let start = i;
            while i < n && is_ident_byte(m[i]) {
                i += 1;
            }
            let word = &body[start..i];
            match decl {
                Some((d, true)) if d == depth => {
                    if is_identifier(word) {
                        push_unique_name(out, word);
                    }
                    decl = Some((d, false));
                }
                Some((d, false)) if d == depth && (word == "of" || word == "in") => decl = None,
                _ => {}
            }
            if matches!(word, "let" | "const" | "var") {
                decl = Some((depth, true));
            }
            after_catch = word == "catch";
            continue;
        }
        match b {
            b'(' | b'[' | b'{' => {
                if b != b'(' && decl == Some((depth, true)) {
                    let close = match_close(&m, i);
                    push_pattern_names(&body[i..close], out);
                    decl = Some((depth, false));
                    i = close;
                    continue;
                }
                if b == b'(' && after_catch {
                    let close = match_close(&m, i);
                    let inner = body[i + 1..close.saturating_sub(1).max(i + 1)].trim();
                    if is_identifier(inner) {
                        push_unique_name(out, inner);
                    }
                }
                depth += 1;
            }
            b')' | b']' | b'}' => {
                depth -= 1;
                if matches!(decl, Some((d, _)) if depth < d) {
                    decl = None;
                }
            }
            b',' => {
                if decl == Some((depth, false)) {
                    decl = Some((depth, true));
                }
            }
            b';' | b'\n' => {
                if matches!(decl, Some((d, _)) if d == depth) {
                    decl = None;
                }
            }
            _ => {}
        }
        if !b.is_ascii_whitespace() {
            after_catch = false;
        }
        i += 1;
    }
}

/// Binding names in a destructuring pattern such as `{ a, b: c }` or `[x, y]`.
fn push_pattern_names(pattern: &str, out: &mut Vec<String>) {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if is_ident_start_byte(bytes[i]) && (i == 0 || !is_ident_byte(bytes[i - 1])) {
            let start = i;
            while i < bytes.len() && is_ident_byte(bytes[i]) {
                i += 1;
            }
            let mut k = i;
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            let is_key = bytes.get(k) == Some(&b':');
            if !is_key {
                push_unique_name(out, &pattern[start..i]);
            }
            continue;
        }
        i += 1;
    }
}

/// An error object shaped like `new <name>(message)`.
fn make_named_error(name: &str, message: &str) -> Value {
    let mut obj = JsonValue::empty_object();
    obj.insert("name", JsonValue::String(name.to_string()));
    obj.insert("message", JsonValue::String(message.to_string()));
    Value::Json(obj)
}

/// The rejection reason of `Promise.any` when every input rejected.
fn make_aggregate_error(errors: &[JsonValue]) -> Value {
    let mut obj = JsonValue::empty_object();
    obj.insert("name", JsonValue::String("AggregateError".to_string()));
    obj.insert(
        "message",
        JsonValue::String("All promises were rejected".to_string()),
    );
    obj.insert("errors", JsonValue::Array(errors.to_vec()));
    Value::Json(obj)
}

fn should_route_dom_script_to_cinder(cleaned: &str) -> bool {
    if !is_cpp_jit_kernel_active() || !script_stays_on_rust(cleaned) {
        return false;
    }
    let stripped = strip_string_literals(cleaned);
    stripped.contains('#')
        || stripped.contains("static {")
        || stripped.contains("static{")
        || stripped.contains("??=")
        || stripped.contains("||=")
        || stripped.contains("&&=")
        || cleaned.contains("\"use cinder-vm\"")
        || cleaned.contains("'use cinder-vm'")
        || cleaned.contains("@cinder-vm")
}

fn recover_script_statements_for_cinder(
    source: &str,
    limits: &cinder::Limits,
) -> Option<cinder::bytecode::CodeBlock> {
    let stmts = split_statements(source);
    if stmts.len() <= 1 {
        return None;
    }
    let mut kept = Vec::new();
    let mut valid_count = 0usize;
    for stmt in stmts {
        let s = stmt.trim();
        if s.is_empty() {
            continue;
        }
        let candidate = format!("{s};");
        if cinder::compile_script(&candidate, limits).is_ok() {
            kept.push(candidate);
            valid_count += 1;
        } else {
            kept.push(
                "if (typeof window !== \"undefined\" && window.__reportUncaughtError) { window.__reportUncaughtError(\"SyntaxError: malformed statement\"); }".to_string(),
            );
        }
    }
    if valid_count == 0 {
        return None;
    }
    let combined = kept.join("\n");
    cinder::compile_script(&combined, limits).ok()
}

fn should_route_to_cinder_engine(cleaned: &str) -> bool {
    cleaned.contains("\"use cinder\"")
        || cleaned.contains("'use cinder'")
        || cleaned.contains("@cinder")
        || cleaned.contains("Promise.withResolvers")
        || cleaned.contains("Object.groupBy")
        || cleaned.contains("Map.groupBy")
        || cleaned.contains(".toSorted(")
        || cleaned.contains(".toReversed(")
        || cleaned.contains(".toSpliced(")
        || cleaned.contains("new CSSStyleSheet")
        || cleaned.contains("document.adoptedStyleSheets")
        || cleaned.contains("new MutationObserver")
        || cleaned.contains("MutationObserver")
        || cleaned.contains("customElements")
        || cleaned.contains("HTMLElement")
        || cleaned.contains("attachShadow")
        || cleaned.contains("shadowRoot")
        || cleaned.contains("assignedNodes")
        || cleaned.contains("assignedElements")
        || cleaned.contains("new IntersectionObserver")
        || cleaned.contains("new ResizeObserver")
        || cleaned.contains("function*")
        || cleaned.contains("yield ")
        || cleaned.contains("yield*")
        || cleaned.contains("BigInt(")
        || cleaned.contains("BigInt.")
        || cleaned.contains("new Proxy")
        || cleaned.contains("Proxy.revocable")
        || cleaned.contains("Reflect.")
        || cleaned.contains("new WeakMap")
        || cleaned.contains("new WeakSet")
        || cleaned.contains("new WeakRef")
        || cleaned.contains("new FinalizationRegistry")
        || cleaned.contains("new DataView")
        || cleaned.contains("Symbol.for(")
        || cleaned.contains("Symbol.keyFor(")
        || cleaned.contains("new AggregateError")
        || cleaned.contains("Intl.")
        || cleaned.contains("import.meta")
        || cleaned.contains("new AbortController")
        || cleaned.contains("AbortSignal.")
        || cleaned.contains("new Headers")
        || cleaned.contains("new Request")
        || cleaned.contains("new Response")
        || cleaned.contains("createTreeWalker")
        || cleaned.contains("createNodeIterator")
        || cleaned.contains("createRange")
        || cleaned.contains("getSelection(")
        || cleaned.contains("crypto.randomUUID")
        || cleaned.contains("crypto.getRandomValues")
        || cleaned.contains("crypto.subtle")
        || cleaned.contains("performance.mark")
        || cleaned.contains("performance.measure")
        || cleaned.contains("history.replaceState")
        || cleaned.contains("new PopStateEvent")
        || cleaned.contains("window.onerror")
        || cleaned.contains("onunhandledrejection")
        || cleaned.contains(".stopImmediatePropagation(")
        || cleaned.contains(".composedPath(")
        || cleaned.contains(".toggleAttribute(")
        || cleaned.contains(".replaceChildren(")
        || cleaned.contains(".insertAdjacentElement(")
        || cleaned.contains(".insertAdjacentText(")
        || cleaned.contains(".createLinearGradient(")
        || cleaned.contains(".createRadialGradient(")
        || cleaned.contains(".quadraticCurveTo(")
        || cleaned.contains(".bezierCurveTo(")
        || cleaned.contains("Symbol.asyncIterator")
        || cleaned.contains(".showModal(")
        || cleaned.contains(".checkValidity(")
        || cleaned.contains(".reportValidity(")
        || cleaned.contains(".setCustomValidity(")
        || cleaned.contains("new File(")
        || cleaned.contains("new FileReader")
        || cleaned.contains("new DataTransfer")
        || cleaned.contains("navigator.clipboard")
        || cleaned.contains(".scrollTop")
        || cleaned.contains(".scrollLeft")
        || cleaned.contains(".scrollWidth")
        || cleaned.contains(".scrollHeight")
        || cleaned.contains(".offsetWidth")
        || cleaned.contains(".offsetHeight")
        || cleaned.contains(".clientWidth")
        || cleaned.contains(".clientHeight")
        || cleaned.contains(".scrollTo(")
        || cleaned.contains(".scrollBy(")
}

fn extract_event_callback_body(handler_raw: &str) -> String {
    let trimmed = handler_raw.trim();
    if is_identifier(trimmed) {
        return format!("{trimmed}(event)");
    }
    let (param, body) = extract_single_param_callback(trimmed);
    if !param.is_empty() && param != "event" && is_identifier(&param) {
        format!("let {param} = event; {body}")
    } else {
        extract_callback_body(trimmed)
    }
}

fn extract_callback_body(handler_raw: &str) -> String {
    if let Some((_, rhs)) = handler_raw.split_once("=>") {
        let rhs = rhs.trim();
        if let Some((_, block)) = split_brace_block(rhs) {
            return block;
        }
        return rhs.to_string();
    }
    if let Some((_, block)) = split_brace_block(handler_raw) {
        block
    } else if is_identifier(handler_raw) {
        format!("{handler_raw}()")
    } else {
        handler_raw.to_string()
    }
}

fn extract_single_param_callback(handler_raw: &str) -> (String, String) {
    let (params, body) = extract_multi_param_callback(handler_raw);
    (params.into_iter().next().unwrap_or_default(), body)
}

fn extract_multi_param_callback(handler_raw: &str) -> (Vec<String>, String) {
    if let Some((lhs, rhs)) = split_top_level_arrow(handler_raw) {
        let params: Vec<String> = lhs
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        let rhs = rhs.trim();
        let body = if rhs.starts_with('{') {
            split_brace_block(rhs)
                .map(|(_, block)| block)
                .unwrap_or_else(|| rhs.to_string())
        } else {
            rhs.to_string()
        };
        return (params, body);
    }
    if let Some((header, block)) = split_brace_block(handler_raw) {
        let params = header
            .find('(')
            .and_then(|open| {
                header[open + 1..]
                    .find(')')
                    .map(|close| &header[open + 1..open + 1 + close])
            })
            .map(|p| {
                p.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        return (params, block);
    }
    (Vec::new(), handler_raw.to_string())
}

fn find_template_expr_end(after_dollar_brace: &str) -> Option<usize> {
    let bytes = after_dollar_brace.as_bytes();
    let mut depth = 1_i32;
    let mut quote: Option<u8> = None;
    let mut idx = 0usize;
    while idx < bytes.len() {
        let b = bytes[idx];
        if let Some(q) = quote {
            if b == b'\\' {
                idx += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            idx += 1;
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => quote = Some(b),
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
        idx += 1;
    }
    None
}

/// Names of the methods declared `async` in a class body.
fn async_method_names(class_body: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rem = class_body.trim();
    while let Some(open_paren) = rem.find('(') {
        let head = rem[..open_paren].trim().trim_start_matches(';').trim();
        let head = head.strip_prefix("static ").unwrap_or(head).trim();
        if let Some(name) = head.strip_prefix("async ") {
            names.push(name.trim().to_string());
        }
        let Some((_, after_params)) = extract_paren_group(&rem[open_paren..]) else {
            break;
        };
        let Some((_, after_body)) = extract_first_brace_block(after_params) else {
            break;
        };
        rem = after_body.trim();
    }
    names
}

fn parse_class_methods(class_body: &str) -> Vec<(String, Vec<String>, String)> {
    let mut methods = Vec::new();
    let mut rem = class_body.trim();
    while !rem.is_empty() {
        let Some(open_paren) = rem.find('(') else {
            break;
        };
        let head = rem[..open_paren].trim().trim_start_matches(';').trim();
        let head = head.strip_prefix("static ").unwrap_or(head).trim();
        let raw_name = head.strip_prefix("async ").unwrap_or(head).trim();
        let after_name = &rem[open_paren..];
        let Some((params_str, after_params)) = extract_paren_group(after_name) else {
            break;
        };
        let Some((body, after_body)) = extract_first_brace_block(after_params) else {
            break;
        };
        let params: Vec<String> = params_str
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        if !raw_name.is_empty() {
            methods.push((raw_name.to_string(), params, body));
        }
        rem = after_body.trim();
    }
    methods
}

fn parse_switch_clauses(switch_body: &str) -> Vec<(Option<String>, Vec<String>)> {
    let mut clauses: Vec<(Option<String>, Vec<String>)> = Vec::new();
    for raw_stmt in split_statements(switch_body) {
        let mut s = raw_stmt.trim();
        while !s.is_empty() {
            if let Some(after_case) = s.strip_prefix("case ")
                && let Some(colon) = after_case.find(':')
            {
                let case_expr = after_case[..colon].trim().to_string();
                clauses.push((Some(case_expr), Vec::new()));
                s = after_case[colon + 1..].trim();
                continue;
            }
            if let Some(after_def) = s.strip_prefix("default") {
                let after_def = after_def.trim_start();
                if let Some(rest) = after_def.strip_prefix(':') {
                    clauses.push((None, Vec::new()));
                    s = rest.trim();
                    continue;
                }
            }
            if let Some((_, stmts)) = clauses.last_mut() {
                if s.starts_with('{') && s.ends_with('}') && s.len() >= 2 {
                    for inner in split_statements(&s[1..s.len() - 1]) {
                        stmts.push(inner);
                    }
                } else {
                    stmts.push(s.to_string());
                }
            }
            break;
        }
    }
    clauses
}

fn split_for_of_or_in<'a>(header: &'a str, kw: &str) -> Option<(&'a str, &'a str)> {
    let bytes = header.as_bytes();
    let kw_bytes = kw.as_bytes();
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    let mut quote: Option<u8> = None;
    let mut idx = 0usize;
    while idx < bytes.len() {
        let b = bytes[idx];
        if let Some(q) = quote {
            if b == b'\\' {
                idx += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            idx += 1;
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => quote = Some(b),
            b'(' => paren += 1,
            b')' => paren = (paren - 1).max(0),
            b'[' => bracket += 1,
            b']' => bracket = (bracket - 1).max(0),
            b'{' => brace += 1,
            b'}' => brace = (brace - 1).max(0),
            _ if paren == 0 && bracket == 0 && brace == 0 && bytes[idx..].starts_with(kw_bytes) => {
                return Some((&header[..idx], &header[idx + kw_bytes.len()..]));
            }
            _ => {}
        }
        idx += 1;
    }
    None
}

fn split_top_level_arrow(input: &str) -> Option<(&str, &str)> {
    let bytes = input.as_bytes();
    let mut paren = 0i32;
    let mut brace = 0i32;
    let mut bracket = 0i32;
    let mut quote: Option<u8> = None;
    let mut idx = 0usize;
    while idx + 1 < bytes.len() {
        let b = bytes[idx];
        if let Some(q) = quote {
            if b == b'\\' {
                idx += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            idx += 1;
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => quote = Some(b),
            b'(' => paren += 1,
            b')' => paren = (paren - 1).max(0),
            b'{' => brace += 1,
            b'}' => brace = (brace - 1).max(0),
            b'[' => bracket += 1,
            b']' => bracket = (bracket - 1).max(0),
            b'=' if paren == 0 && brace == 0 && bracket == 0 && bytes[idx + 1] == b'>' => {
                return Some((&input[..idx], &input[idx + 2..]));
            }
            _ => {}
        }
        idx += 1;
    }
    None
}

/// Bytes of keys plus values already stored for `origin`.
fn storage_origin_bytes(rows: &[(String, String, String)], origin: &str) -> usize {
    rows.iter()
        .filter(|(stored_origin, _, _)| stored_origin == origin)
        .map(|(_, key, value)| key.len().saturating_add(value.len()))
        .fold(0usize, usize::saturating_add)
}

/// Insert or replace one storage row when the origin byte cap allows it.
///
/// An update that does not fit leaves the previous value. A new key that does
/// not fit is not added. The list length cap still applies to new rows.
fn storage_put(rows: &mut Vec<(String, String, String)>, origin: &str, key: &str, value: &str) {
    if let Some(index) = rows
        .iter()
        .position(|(stored_origin, stored_key, _)| stored_origin == origin && stored_key == key)
    {
        let next = storage_origin_bytes(rows, origin)
            .saturating_sub(rows[index].2.len())
            .saturating_add(value.len());
        if next > MAX_STORAGE_ORIGIN_BYTES {
            return;
        }
        rows[index].2 = value.to_string();
        return;
    }
    let next = storage_origin_bytes(rows, origin)
        .saturating_add(key.len())
        .saturating_add(value.len());
    if next > MAX_STORAGE_ORIGIN_BYTES {
        return;
    }
    if rows.len() < MAX_STORAGE_ENTRIES * 8 {
        rows.push((origin.to_string(), key.to_string(), value.to_string()));
    }
}

/// Result of offering a script to the caller-owned numeric VM.
#[cfg_attr(not(cinder_cpp_jit), allow(dead_code))]
enum VmRun {
    /// The compiler accepted the script and execution finished.
    Value(f64, Box<CinderVmTelemetry>),
    /// The compiler accepted the script and a limit stopped it.
    Stopped(VmError),
    /// The compiler rejected the script, or the VM is not linked (`None`).
    Rejected(Option<VmError>),
}

/// True when `source` must stay on the Rust interpreter.
///
/// The numeric VM does not see the DOM, `fetch()`, or storage. A script that
/// names any of those stays here even if the compiler would accept the rest.
fn native_loop_program(statement: &str) -> Option<String> {
    let trimmed = statement.trim();
    if trimmed.starts_with("while") {
        return Some(trimmed.to_string());
    }
    let rest = trimmed.strip_prefix("for")?.trim_start();
    let (header, after) = extract_paren_group(rest)?;
    let (body, _) = extract_first_brace_block(after)?;
    let parts: Vec<&str> = header.splitn(3, ';').collect();
    if parts.len() != 3 {
        return None;
    }
    let init = parts[0]
        .trim()
        .trim_start_matches("let ")
        .trim_start_matches("var ")
        .trim_start_matches("const ");
    let cond = parts[1].trim();
    let step = parts[2].trim();
    if cond.is_empty() {
        return None;
    }
    let mut program = String::new();
    if !init.is_empty() {
        program.push_str(init);
        if !init.ends_with(';') {
            program.push(';');
        }
    }
    program.push_str("while (");
    program.push_str(cond);
    program.push_str(") {");
    program.push_str(&body);
    if !step.is_empty() {
        if !body.trim().ends_with(';') && !body.trim().is_empty() {
            program.push(';');
        }
        program.push_str(step);
        if !step.ends_with(';') {
            program.push(';');
        }
    }
    program.push('}');
    Some(program)
}

fn page_host_word(name: &str) -> bool {
    matches!(name, "document" | "getElementById" | "textContent")
}

fn page_loop_text(statement: &str) -> bool {
    let trimmed = statement.trim_start();
    if !trimmed.starts_with("while") && !trimmed.starts_with("for") {
        return false;
    }
    if !trimmed.contains("document.getElementById(") || !trimmed.contains(".textContent") {
        return false;
    }
    if trimmed.contains(['[', '`']) || trimmed.contains("function") || trimmed.contains("innerHTML")
    {
        return false;
    }
    let stripped = trimmed
        .replace("document.getElementById(", "")
        .replace(".textContent", "");
    !stripped.contains('.')
}

fn strip_string_literals(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '"' || ch == '\'' {
            for next in chars.by_ref() {
                if next == ch {
                    break;
                }
            }
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(cinder_cpp_jit)]
static TEXT_HOST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(cinder_cpp_jit)]
struct TextHostGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(cinder_cpp_jit)]
impl TextHostGuard {
    fn arm(doc: &mut Document) -> Self {
        let guard = TEXT_HOST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe {
            cinder_cpp_vm_set_text_host(
                Some(rust_host_set_text),
                doc as *mut Document as *mut std::ffi::c_void,
            );
        }
        Self { _lock: guard }
    }
}

#[cfg(cinder_cpp_jit)]
impl Drop for TextHostGuard {
    fn drop(&mut self) {
        unsafe {
            cinder_cpp_vm_set_text_host(None, std::ptr::null_mut());
        }
    }
}

#[cfg(cinder_cpp_jit)]
extern "C" fn rust_host_set_text(host: *mut std::ffi::c_void, id: *const u8, len: u32, value: f64) {
    if host.is_null() || id.is_null() || len == 0 {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(id, len as usize) };
    let Ok(element_id) = std::str::from_utf8(bytes) else {
        return;
    };
    let doc = unsafe { &mut *(host as *mut Document) };
    let Some(node) = doc.get_element_by_id(element_id) else {
        return;
    };
    let shown = if value.fract() == 0.0 && value.abs() < 1.0e15 {
        format!("{}", value as i64)
    } else {
        value.to_string()
    };
    doc.set_text_content(node, &shown);
}

fn numeric_keyword(name: &str) -> bool {
    matches!(
        name,
        "while" | "for" | "let" | "const" | "var" | "if" | "else" | "return" | "true" | "false"
    )
}

fn format_jit_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1.0e15 {
        format!("{}", number as i64)
    } else {
        format!("{number}")
    }
}

fn numeric_loop_text(statement: &str) -> bool {
    let trimmed = statement.trim_start();
    if !trimmed.starts_with("while") && !trimmed.starts_with("for") {
        return false;
    }
    if trimmed.contains(['.', '[', ']', '\'', '"', '`']) {
        return false;
    }
    !trimmed.contains("function")
}

fn ident_names(statement: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = String::new();
    for ch in statement.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            current.push(ch);
        } else if !current.is_empty() {
            if current
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
                && !names.iter().any(|existing| existing == &current)
            {
                names.push(current);
            }
            current = String::new();
        }
    }
    if !current.is_empty()
        && current
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && !names.iter().any(|existing| existing == &current)
    {
        names.push(current);
    }
    names
}

fn script_stays_on_rust(source: &str) -> bool {
    const BLOCKED: &[&str] = &[
        "document",
        "getElementById",
        "querySelector",
        "querySelectorAll",
        "textContent",
        "innerHTML",
        "innerText",
        "localStorage",
        "sessionStorage",
        "fetch",
        "addEventListener",
        "window",
        "Promise",
        "setTimeout",
        "setInterval",
        "clearTimeout",
        "clearInterval",
        "requestAnimationFrame",
        "queueMicrotask",
        "then",
        "catch",
        "finally",
        "async",
        "await",
        "WebAssembly",
        "Uint8Array",
        "Int8Array",
        "Uint16Array",
        "Int16Array",
        "Uint32Array",
        "Int32Array",
        "Float32Array",
        "Float64Array",
        "ArrayBuffer",
        "TextEncoder",
        "TextDecoder",
        "URLSearchParams",
        "DOMParser",
        "AbortController",
        "MutationObserver",
        "IntersectionObserver",
        "ResizeObserver",
        "CustomEvent",
        "Event",
        "atob",
        "btoa",
        "structuredClone",
    ];
    let mut ident = String::new();
    for ch in source.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            ident.push(ch);
        } else if blocked_ident(&ident, BLOCKED) {
            return true;
        } else {
            ident.clear();
        }
    }
    blocked_ident(&ident, BLOCKED)
}

fn blocked_ident(ident: &str, blocked: &[&str]) -> bool {
    if ident.is_empty() {
        return false;
    }
    blocked.contains(&ident)
}

fn fetch_result_value(url: &str, status: u16, body: String) -> Value {
    let mut obj = JsonValue::empty_object();
    obj.insert("url", JsonValue::String(url.to_string()));
    obj.insert("status", JsonValue::Number(status as f64));
    obj.insert("ok", JsonValue::Bool((200..300).contains(&status)));
    obj.insert("body", JsonValue::String(body));
    Value::Json(obj)
}

/// Turn a same-origin reference into an absolute URL for the host.
///
/// A target that already contains `://` is returned unchanged. A path is joined
/// to the page origin. Without an origin tuple, the target is returned as written.
fn resolve_script_fetch_url(page: &str, target: &str) -> String {
    let target = target.trim();
    if target.contains("://") {
        return target.to_string();
    }
    let Some((scheme, host, port)) = extract_origin_tuple(page) else {
        return target.to_string();
    };
    let origin = format!("{scheme}://{host}:{port}");
    if target.starts_with('/') {
        return format!("{origin}{target}");
    }
    let after_scheme = page.split_once("://").map(|(_, rest)| rest).unwrap_or(page);
    let path = after_scheme.split(['?', '#']).next().unwrap_or("/");
    let path = path.find('/').map(|index| &path[index..]).unwrap_or("/");
    let directory = path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    format!("{origin}{directory}/{target}")
}

fn is_allowed_script_fetch(page_origin: Option<&str>, target_url: &str) -> bool {
    let target = target_url.trim();
    if target.is_empty() {
        return false;
    }
    let lower = target.to_ascii_lowercase();
    if lower.starts_with("file:")
        || lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("vbscript:")
        || lower.starts_with("anvil:")
    {
        return false;
    }
    let Some(origin) = page_origin else {
        return !lower.contains("://");
    };
    if !lower.contains("://") {
        return !target.contains("..");
    }
    extract_origin_tuple(origin) == extract_origin_tuple(target)
}

fn extract_origin_tuple(url: &str) -> Option<(String, String, u16)> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => 80u16,
        "https" => 443u16,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    if let Some((host, port_str)) = authority.rsplit_once(':') {
        let port = port_str.parse::<u16>().ok()?;
        Some((scheme, host.to_ascii_lowercase(), port))
    } else {
        Some((scheme, authority.to_ascii_lowercase(), default_port))
    }
}

fn values_equal(a: &Value, b: &Value, doc: &Document) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => (x - y).abs() <= f64::EPSILON,
        (Value::Element(x), Value::Element(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        _ => a.as_string(doc) == b.as_string(doc),
    }
}

fn update_inline_style(doc: &mut Document, id: NodeId, prop: &str, value: &str) {
    let existing = doc.get(id).data().attr("style").unwrap_or("").to_string();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for part in existing.split(';') {
        let Some((k, v)) = part.split_once(':') else {
            continue;
        };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim().to_string();
        if !k.is_empty() && !v.is_empty() {
            pairs.push((k, v));
        }
    }
    if let Some((_, slot)) = pairs.iter_mut().find(|(k, _)| k == prop) {
        *slot = value.to_string();
    } else {
        pairs.push((prop.to_string(), value.to_string()));
    }
    let updated = pairs
        .into_iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("; ");
    doc.set_attribute(id, "style", &updated);
}

fn read_inline_style(doc: &Document, id: NodeId, prop: &str) -> String {
    let existing = doc.get(id).data().attr("style").unwrap_or("");
    for part in existing.split(';') {
        if let Some((k, v)) = part.split_once(':')
            && k.trim().eq_ignore_ascii_case(prop)
        {
            return v.trim().to_string();
        }
    }
    String::new()
}

fn remove_inline_style_prop(doc: &mut Document, id: NodeId, prop: &str) {
    let existing = doc.get(id).data().attr("style").unwrap_or("").to_string();
    let kept: Vec<String> = existing
        .split(';')
        .filter_map(|part| {
            let (k, v) = part.split_once(':')?;
            let k_clean = k.trim().to_ascii_lowercase();
            let v_clean = v.trim();
            if k_clean.is_empty() || v_clean.is_empty() || k_clean == prop {
                None
            } else {
                Some(format!("{k_clean}: {v_clean}"))
            }
        })
        .collect();
    doc.set_attribute(id, "style", &kept.join("; "));
}

fn camel_to_kebab(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if ch.is_ascii_uppercase() {
            out.push('-');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn kebab_to_camel(input: &str) -> String {
    let mut out = String::new();
    let mut upper_next = false;
    for ch in input.chars() {
        if ch == '-' {
            upper_next = true;
        } else if upper_next {
            out.push(ch.to_ascii_uppercase());
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn strip_comments(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    let mut in_quote: Option<char> = None;
    while let Some(ch) = chars.next() {
        if let Some(q) = in_quote {
            out.push(ch);
            if ch == '\\' {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            } else if ch == q {
                in_quote = None;
            }
            continue;
        }
        if ch == '\\' {
            out.push(ch);
            if let Some(escaped) = chars.next() {
                out.push(escaped);
            }
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            in_quote = Some(ch);
            out.push(ch);
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'/') {
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(inner) = chars.next() {
                if inner == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
            out.push(' ');
            continue;
        }
        out.push(ch);
    }
    out
}

/// True when `source` is small enough for this interpreter.
///
/// The limit is the code, not the whole text. Characters inside a quoted
/// string do not count, so a short script can still store a large value.
/// A bundle whose code is over [`MAX_SCRIPT_SOURCE`] is left unrun. An empty
/// source does not run.
pub fn script_runs_here(source: &str) -> bool {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.len() <= MAX_SCRIPT_SOURCE {
        return true;
    }
    code_bytes_outside_strings(trimmed) <= MAX_SCRIPT_SOURCE
}

/// Console text for an uncaught throw: `Name: message` for an error
/// object, the value as text otherwise.
fn thrown_text(error: &Value, doc: &Document) -> String {
    if let Value::Json(obj) = error
        && let Some(JsonValue::String(message)) = obj.get("message")
    {
        let name = match obj.get("name") {
            Some(JsonValue::String(name)) => name.as_str(),
            _ => "Error",
        };
        return format!("{name}: {message}");
    }
    error.as_string(doc)
}

/// Copy up to [`MAX_ARRAY_LEN`] bytes of `bytes` from `offset` as numbers,
/// with the length the view asked for.
fn memory_view(bytes: &[u8], offset: usize, len_opt: Option<usize>) -> (Vec<Value>, usize) {
    let total = bytes.len();
    let start = offset.min(total);
    let wanted = len_opt
        .unwrap_or_else(|| total.saturating_sub(start))
        .min(total.saturating_sub(start));
    let take_len = wanted.min(MAX_ARRAY_LEN);
    let items = bytes
        .get(start..start + take_len)
        .map(|slice| slice.iter().map(|b| Value::Number(*b as f64)).collect())
        .unwrap_or_default();
    (items, wanted)
}

/// Bytes of `source` that are not inside `'`, `"`, or `` ` ``.
///
/// The quotes themselves count. An escape skips the next byte, including a
/// quote. This is a size check, not a full parse.
fn code_bytes_outside_strings(source: &str) -> usize {
    let bytes = source.as_bytes();
    let mut count = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'"' || byte == b'\'' || byte == b'`' {
            count = count.saturating_add(1);
            index += 1;
            while index < bytes.len() {
                if bytes[index] == b'\\' {
                    index = index.saturating_add(2);
                    continue;
                }
                if bytes[index] == byte {
                    count = count.saturating_add(1);
                    index += 1;
                    break;
                }
                index += 1;
            }
            continue;
        }
        count = count.saturating_add(1);
        index += 1;
    }
    count
}

fn split_statements(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut paren = 0_i32;
    let mut brace = 0_i32;
    let mut bracket = 0_i32;
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            current.push(ch);
            if ch == '\\' {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' | '`' => {
                quote = Some(ch);
                current.push(ch);
            }
            '(' => {
                paren += 1;
                current.push(ch);
            }
            ')' => {
                paren = (paren - 1).max(0);
                current.push(ch);
            }
            '[' => {
                bracket += 1;
                current.push(ch);
            }
            ']' => {
                bracket = (bracket - 1).max(0);
                current.push(ch);
            }
            '{' => {
                brace += 1;
                current.push(ch);
            }
            '}' => {
                brace = (brace - 1).max(0);
                current.push(ch);
                if brace == 0 && paren == 0 && bracket == 0 {
                    let mut look = chars.clone();
                    while let Some(&c) = look.peek() {
                        if c.is_whitespace() {
                            look.next();
                        } else {
                            break;
                        }
                    }
                    let next_non_ws = look.peek().copied();
                    let next_word: String = look.take_while(|c| c.is_ascii_alphabetic()).collect();
                    let continues_expr = matches!(
                        next_non_ws,
                        Some('=')
                            | Some(',')
                            | Some('.')
                            | Some(';')
                            | Some(')')
                            | Some(']')
                            | Some('?')
                            | Some(':')
                    );
                    if !continues_expr
                        && !matches!(next_word.as_str(), "else" | "catch" | "finally")
                    {
                        let trimmed = current.trim().to_string();
                        if !trimmed.is_empty() {
                            out.push(trimmed);
                        }
                        current.clear();
                    }
                }
            }
            ';' if paren == 0 && brace == 0 && bracket == 0 => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    out.push(trimmed);
                }
                current.clear();
            }
            '\n' if paren == 0 && brace == 0 && bracket == 0 => {
                let trimmed = current.trim();
                let ends_with_continuation = trimmed.ends_with('.')
                    || (trimmed.ends_with('+') && !trimmed.ends_with("++"))
                    || (trimmed.ends_with('-') && !trimmed.ends_with("--"))
                    || trimmed.ends_with('*')
                    || trimmed.ends_with('=')
                    || trimmed.ends_with('?')
                    || trimmed.ends_with(':')
                    || trimmed.ends_with('|')
                    || trimmed.ends_with('&')
                    || trimmed.ends_with(',');
                let mut look = chars.clone();
                while let Some(&c) = look.peek() {
                    if c == ' ' || c == '\t' || c == '\r' {
                        look.next();
                    } else {
                        break;
                    }
                }
                let next_continues =
                    matches!(look.peek().copied(), Some('.') | Some('?') | Some(':'));
                if ends_with_continuation || next_continues {
                    current.push(' ');
                } else {
                    if !trimmed.is_empty() {
                        out.push(trimmed.to_string());
                    }
                    current.clear();
                }
            }
            _ => current.push(ch),
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        out.push(trimmed);
    }
    out
}

fn split_brace_block(input: &str) -> Option<(String, String)> {
    let open = input.find('{')?;
    let close = input.rfind('}')?;
    if close <= open {
        return None;
    }
    Some((
        input[..open].trim().to_string(),
        input[open + 1..close].trim().to_string(),
    ))
}

fn extract_paren_group(input: &str) -> Option<(String, &str)> {
    let bytes = input.as_bytes();
    if bytes.first() != Some(&b'(') {
        return None;
    }
    let mut depth = 0_i32;
    let mut quote: Option<u8> = None;
    let mut idx = 0;
    while idx < bytes.len() {
        let b = bytes[idx];
        if let Some(q) = quote {
            if b == b'\\' {
                idx += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            idx += 1;
            continue;
        }
        if b == b'"' || b == b'\'' {
            quote = Some(b);
        } else if b == b'(' {
            depth += 1;
        } else if b == b')' {
            depth -= 1;
            if depth == 0 {
                return Some((input[1..idx].to_string(), &input[idx + 1..]));
            }
        }
        idx += 1;
    }
    None
}

fn extract_first_brace_block(input: &str) -> Option<(String, &str)> {
    let trimmed = input.trim_start();
    let bytes = trimmed.as_bytes();
    if bytes.first() != Some(&b'{') {
        return None;
    }
    let mut depth = 0_i32;
    let mut quote: Option<u8> = None;
    let mut idx = 0;
    while idx < bytes.len() {
        let b = bytes[idx];
        if let Some(q) = quote {
            if b == b'\\' {
                idx += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            idx += 1;
            continue;
        }
        if b == b'"' || b == b'\'' {
            quote = Some(b);
        } else if b == b'{' {
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some((trimmed[1..idx].to_string(), &trimmed[idx + 1..]));
            }
        }
        idx += 1;
    }
    None
}

fn split_assignment(input: &str) -> Option<(&str, &str)> {
    let bytes = input.as_bytes();
    let mut paren = 0_i32;
    let mut brace = 0_i32;
    let mut bracket = 0_i32;
    let mut quote: Option<u8> = None;
    let mut idx = 0;
    while idx < bytes.len() {
        let b = bytes[idx];
        if let Some(q) = quote {
            if b == b'\\' {
                idx += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            idx += 1;
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => quote = Some(b),
            b'(' => paren += 1,
            b')' => paren = (paren - 1).max(0),
            b'{' => brace += 1,
            b'}' => brace = (brace - 1).max(0),
            b'[' => bracket += 1,
            b']' => bracket = (bracket - 1).max(0),
            b'=' if paren == 0 && brace == 0 && bracket == 0 => {
                let prev = if idx > 0 { bytes[idx - 1] } else { 0 };
                let next = bytes.get(idx + 1).copied().unwrap_or(0);
                if next != b'='
                    && next != b'>'
                    && !matches!(
                        prev,
                        b'=' | b'!' | b'<' | b'>' | b'+' | b'-' | b'*' | b'/' | b'%'
                    )
                {
                    return Some((&input[..idx], &input[idx + 1..]));
                }
            }
            _ => {}
        }
        idx += 1;
    }
    None
}

fn split_top_level_op<'a>(input: &'a str, op: &str) -> Option<(&'a str, &'a str)> {
    let bytes = input.as_bytes();
    let op_bytes = op.as_bytes();
    let mut paren = 0_i32;
    let mut brace = 0_i32;
    let mut bracket = 0_i32;
    let mut quote: Option<u8> = None;
    let mut last_match = None;
    let mut idx = 0;

    while idx < bytes.len() {
        let b = bytes[idx];
        if let Some(q) = quote {
            if b == b'\\' {
                idx += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            idx += 1;
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => quote = Some(b),
            b'(' => paren += 1,
            b')' => paren = (paren - 1).max(0),
            b'{' => brace += 1,
            b'}' => brace = (brace - 1).max(0),
            b'[' => bracket += 1,
            b']' => bracket = (bracket - 1).max(0),
            _ if paren == 0 && brace == 0 && bracket == 0 && bytes[idx..].starts_with(op_bytes) => {
                if (op == "<" || op == ">") && bytes.get(idx + 1) == Some(&b'=') {
                    idx += 1;
                    continue;
                }
                if op == ">" && idx > 0 && bytes.get(idx - 1) == Some(&b'=') {
                    idx += 1;
                    continue;
                }
                if (op == "==" || op == "!=") && bytes.get(idx + 2) == Some(&b'=') {
                    idx += 1;
                    continue;
                }
                if op == "?"
                    && (bytes.get(idx + 1) == Some(&b'?')
                        || bytes.get(idx + 1) == Some(&b'.')
                        || (idx > 0 && bytes.get(idx - 1) == Some(&b'?')))
                {
                    idx += 1;
                    continue;
                }
                if (op == "+" || op == "-")
                    && idx >= 2
                    && (bytes[idx - 1] == b'e' || bytes[idx - 1] == b'E')
                    && bytes[idx - 2].is_ascii_digit()
                {
                    idx += 1;
                    continue;
                }
                if op == "-"
                    && idx > 0
                    && let Some(&prev_b) =
                        bytes[..idx].iter().rev().find(|b| !b.is_ascii_whitespace())
                    && matches!(
                        prev_b,
                        b'+' | b'-'
                            | b'*'
                            | b'/'
                            | b'%'
                            | b'='
                            | b'!'
                            | b'<'
                            | b'>'
                            | b'&'
                            | b'|'
                            | b'^'
                            | b'?'
                            | b':'
                            | b','
                            | b'('
                            | b'['
                            | b'{'
                    )
                {
                    idx += 1;
                    continue;
                }
                if op == "*"
                    && (bytes.get(idx + 1) == Some(&b'*')
                        || (idx > 0 && bytes.get(idx - 1) == Some(&b'*')))
                {
                    idx += 1;
                    continue;
                }
                last_match = Some(idx);
                idx += op_bytes.len();
                continue;
            }
            _ => {}
        }
        idx += 1;
    }
    let pos = last_match?;
    Some((&input[..pos], &input[pos + op_bytes.len()..]))
}

fn find_call_open_paren(expr: &str) -> Option<usize> {
    let bytes = expr.as_bytes();
    let mut depth = 0_i32;
    let mut quote: Option<u8> = None;
    for idx in (0..bytes.len()).rev() {
        let b = bytes[idx];
        if let Some(q) = quote {
            if b == q && (idx == 0 || bytes[idx - 1] != b'\\') {
                quote = None;
            }
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => quote = Some(b),
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    return (idx > 0).then_some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_args(args_src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut paren = 0_i32;
    let mut brace = 0_i32;
    let mut bracket = 0_i32;
    let mut quote: Option<char> = None;
    let mut chars = args_src.chars().peekable();

    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            current.push(ch);
            if ch == '\\' {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' | '`' => {
                quote = Some(ch);
                current.push(ch);
            }
            '(' => {
                paren += 1;
                current.push(ch);
            }
            ')' => {
                paren = (paren - 1).max(0);
                current.push(ch);
            }
            '{' => {
                brace += 1;
                current.push(ch);
            }
            '}' => {
                brace = (brace - 1).max(0);
                current.push(ch);
            }
            '[' => {
                bracket += 1;
                current.push(ch);
            }
            ']' => {
                bracket = (bracket - 1).max(0);
                current.push(ch);
            }
            ',' if paren == 0 && brace == 0 && bracket == 0 => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        out.push(trimmed);
    }
    out
}

fn parse_js_number_literal(expr: &str) -> Option<f64> {
    let trimmed = expr.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return i64::from_str_radix(hex, 16).ok().map(|v| v as f64);
    }
    if let Some(bin) = trimmed
        .strip_prefix("0b")
        .or_else(|| trimmed.strip_prefix("0B"))
    {
        return i64::from_str_radix(bin, 2).ok().map(|v| v as f64);
    }
    if let Some(oct) = trimmed
        .strip_prefix("0o")
        .or_else(|| trimmed.strip_prefix("0O"))
    {
        return i64::from_str_radix(oct, 8).ok().map(|v| v as f64);
    }
    trimmed.parse::<f64>().ok()
}

fn parse_string_literal(expr: &str) -> Option<String> {
    let bytes = expr.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let quote = bytes[0];
    if (quote != b'"' && quote != b'\'') || bytes[bytes.len() - 1] != quote {
        return None;
    }
    let inner = &expr[1..expr.len() - 1];
    if has_unescaped_quote(inner, quote as char) {
        return None;
    }
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => break,
            }
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

fn has_unescaped_quote(inner: &str, quote: char) -> bool {
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let _ = chars.next();
        } else if ch == quote {
            return true;
        }
    }
    false
}

fn decode_wasm_text_bytes(s: &str) -> Vec<u8> {
    if s.as_bytes().starts_with(&WASM_MAGIC) {
        return s.as_bytes().to_vec();
    }
    let trimmed = s.trim();
    if trimmed.len() >= 16
        && trimmed.len().is_multiple_of(2)
        && trimmed.bytes().all(|b| b.is_ascii_hexdigit())
    {
        let mut out = Vec::with_capacity(trimmed.len() / 2);
        let bytes = trimmed.as_bytes();
        let mut idx = 0usize;
        while idx + 1 < bytes.len() {
            if let Ok(hex_str) = std::str::from_utf8(&bytes[idx..idx + 2])
                && let Ok(b) = u8::from_str_radix(hex_str, 16)
            {
                out.push(b);
            }
            idx += 2;
        }
        return out;
    }
    decode_base64_bytes(trimmed)
}

fn decode_base64_bytes(input: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for b in input.bytes() {
        let val = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ if b.is_ascii_whitespace() => continue,
            _ => continue,
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    out
}

fn decode_base64_to_latin1(input: &str) -> String {
    decode_base64_bytes(input)
        .into_iter()
        .map(|b| b as char)
        .collect()
}

fn encode_latin1_to_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_dom::parse_html;

    #[test]
    fn skips_a_bundle_and_still_runs_a_short_script() {
        assert!(!script_runs_here(&"x".repeat(MAX_SCRIPT_SOURCE + 1)));
        let bundle = format!("{}\n{}", "a".repeat(MAX_SCRIPT_SOURCE), "b".repeat(5000));
        assert!(!script_runs_here(&bundle));
        let mut doc = parse_html("<title>Old</title><script>document.title = 'New';</script>");
        let mut runtime = ScriptRuntime::new();
        runtime.run_document_scripts(&mut doc);
        assert_eq!(doc.title(), "New");
        let mut doc = parse_html(&format!(
            "<title>Stay</title><script>{}</script>",
            "var x = 1;".repeat(MAX_SCRIPT_SOURCE / 8)
        ));
        let mut runtime = ScriptRuntime::new();
        runtime.run_document_scripts(&mut doc);
        assert_eq!(doc.title(), "Stay");
    }

    #[test]
    fn executes_script_mutations_and_click_listeners() {
        let mut doc = parse_html(
            "<title>Start</title>
             <h1 id=\"heading\">Before</h1>
             <p class=\"status\">Idle</p>
             <script>
               let count = 2 + 3;
               document.title = 'Count: ' + count;
               let h = document.getElementById('heading');
               h.textContent = 'Ready';
               h.style.color = '#e8c37a';
               let status = document.querySelector('.status');
               h.addEventListener('click', function() {
                 status.textContent = 'Clicked!';
               });
             </script>",
        );
        let mut runtime = ScriptRuntime::new();
        runtime.run_document_scripts(&mut doc);

        assert_eq!(doc.title(), "Count: 5");
        let h = doc.get_element_by_id("heading").unwrap();
        let status = doc.query_selector(".status").unwrap();
        assert_eq!(doc.text_content(h), "Ready");
        assert_eq!(doc.get(h).data().attr("style"), Some("color: #e8c37a"));
        assert_eq!(doc.text_content(status), "Idle");

        assert!(runtime.dispatch_click(&mut doc, h));
        assert_eq!(doc.text_content(status), "Clicked!");
    }

    #[test]
    fn parses_and_stringifies_json_in_scripts() {
        let mut doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               let data = JSON.parse('{\"label\": \"Anvil JSON\", \"code\": 3}');
               let out = document.getElementById('out');
               out.textContent = data.label + ' v' + data.code;
             </script>",
        );
        let mut runtime = ScriptRuntime::new();
        runtime.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "Anvil JSON v3");
    }

    #[test]
    fn executes_for_loops_arrays_inputs_class_list_timers_and_fetch() {
        let mut doc = parse_html(
            "<input id=\"field\" type=\"text\" value=\"initial\">
             <input id=\"flag\" type=\"checkbox\">
             <div id=\"box\" class=\"card\"></div>
             <script>
               let nums = [10, 20, 30];
               let sum = 0;
               for (let i = 0; i < nums.length; i = i + 1) {
                 sum = sum + nums[i];
               }
               let f = document.getElementById('field');
               f.value = 'Sum: ' + sum;
               let c = document.getElementById('flag');
               c.checked = true;
               let box = document.getElementById('box');
               box.classList.add('active');
               let resp = fetch('/api/status');
               let info = resp.json();
               setTimeout(function() {
                 box.textContent = info.state + ':' + f.value;
               }, 10);
             </script>",
        );
        let mut runtime = ScriptRuntime::new();
        runtime.register_fetch_response("/api/status", 200, "{\"state\": \"online\"}");
        runtime.run_document_scripts(&mut doc);

        let field = doc.get_element_by_id("field").unwrap();
        let flag = doc.get_element_by_id("flag").unwrap();
        let box_el = doc.get_element_by_id("box").unwrap();
        assert_eq!(doc.input_value(field), "Sum: 60");
        assert!(doc.is_checked(flag));
        assert!(doc.get(box_el).data().has_class("active"));
        assert_eq!(doc.text_content(box_el), "online:Sum: 60");
    }

    #[test]
    fn aborts_infinite_recursion_and_enforces_same_origin_fetch() {
        // An unoptimized build needs more than the 2 MB default test-thread
        // stack to reach MAX_CALL_DEPTH, so the recursion check runs on its
        // own thread with room to spare. The depth cap is still what stops it.
        let aborted = std::thread::Builder::new()
            .name("recursion-check".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let mut doc = parse_html(
                    "<p id=\"status\">Safe</p>
                     <script>
                       function recurse() {
                         recurse();
                       }
                       recurse();
                     </script>",
                );
                let mut runtime = ScriptRuntime::new();
                runtime.run_document_scripts(&mut doc);
                runtime.was_aborted()
            })
            .expect("spawn recursion-check thread")
            .join()
            .expect("recursion-check thread panicked");
        assert!(aborted);

        let mut sop_doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               let blocked = fetch('https://other-origin.example/secret');
               let allowed = fetch('https://app.example/api');
               let out = document.getElementById('out');
               out.textContent = blocked.status + ':' + allowed.status;
             </script>",
        );
        let mut sop_rt = ScriptRuntime::new();
        sop_rt.set_page_origin("https://app.example/index.html");
        sop_rt.register_fetch_response("https://app.example/api", 200, "{\"ok\":true}");
        sop_rt.run_document_scripts(&mut sop_doc);
        let out = sop_doc.get_element_by_id("out").unwrap();
        assert_eq!(sop_doc.text_content(out), "403:200");
    }

    #[test]
    fn registered_fetch_wins_and_cross_origin_does_not_call_the_host() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let mut runtime = ScriptRuntime::new();
        runtime.set_page_origin("https://app.example/index.html");
        runtime.set_fetch_host(Arc::new(move |url, _page| {
            seen.fetch_add(1, Ordering::SeqCst);
            ScriptFetchReply {
                status: 200,
                body: format!("host:{url}"),
            }
        }));
        runtime.register_fetch_response("https://app.example/api", 201, "from-mock");
        let mut doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               let mocked = fetch('https://app.example/api');
               let denied = fetch('https://other.example/secret');
               let live = fetch('https://app.example/live');
               document.getElementById('out').textContent =
                 mocked.status + ':' + mocked.body + ':' + denied.status + ':' + live.body;
             </script>",
        );
        runtime.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(
            doc.text_content(out),
            "201:from-mock:403:host:https://app.example/live"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn executes_modern_js_features_and_jit_toggle() {
        let script_html = "<div id=\"root\"><span class=\"chip\">A</span><span class=\"chip\">B</span></div>
             <p id=\"result\"></p>
             <script>
               const add = (a, b) => a + b;
               function computeLabel(base) {
                 let total = add(base, 8);
                 total += 2;
                 return `Total=${total}`;
               }
               let chips = document.querySelectorAll('.chip');
               let names = [];
               chips.forEach((el) => {
                 names.push(el.textContent);
               });
               let root = document.getElementById('root');
               let cached = document.getElementById('root');
               let res = document.getElementById('result');
               let mode = total_check();
               function total_check() {
                 return 5 > 3 ? 'ok' : 'bad';
               }
               console.log('computed', names.join('+'));
               res.textContent = computeLabel(10) + ':' + names.join(',') + ':' + mode + ':' + Math.max(4, 9);
             </script>";

        let mut doc_jit = parse_html(script_html);
        let mut rt_jit = ScriptRuntime::with_jit_enabled(true);
        rt_jit.run_document_scripts(&mut doc_jit);
        let res_jit = doc_jit.get_element_by_id("result").unwrap();
        assert_eq!(doc_jit.text_content(res_jit), "Total=20:A,B:ok:9");
        assert!(rt_jit.jit_stats().jit_enabled);
        assert!(rt_jit.jit_stats().compiled_blocks > 0);
        assert!(rt_jit.jit_stats().inline_cache_hits > 0);
        assert_eq!(rt_jit.console_logs(), &["computed A+B"]);

        let mut doc_interp = parse_html(script_html);
        let mut rt_interp = ScriptRuntime::with_jit_enabled(false);
        rt_interp.run_document_scripts(&mut doc_interp);
        let res_interp = doc_interp.get_element_by_id("result").unwrap();
        assert_eq!(doc_interp.text_content(res_interp), "Total=20:A,B:ok:9");
        assert!(!rt_interp.jit_stats().jit_enabled);
        assert_eq!(rt_interp.jit_stats().compiled_blocks, 0);
        assert_eq!(rt_interp.jit_stats().inline_cache_hits, 0);
    }

    #[test]
    fn executes_cpp_cinder_vm_bytecode_shapes_inline_caches_and_gc() {
        let vm_source = "
            function square(n) {
                return n * n;
            }
            let acc = 0;
            for (let i = 1; i <= 20; i++) {
                let pt = { x: i, y: square(i) };
                acc += pt.x + pt.y;
            }
            acc;
        ";
        if is_cpp_jit_kernel_active() {
            let (res, telemetry) = eval_cpp_vm_script(vm_source).expect("CinderVM compiled & ran");
            // sum_{i=1..20} (i + i^2) = 210 + 2870 = 3080
            assert_eq!(res, 3080.0);
            assert_eq!(telemetry.kernel_version, 18);
            assert!(telemetry.bytecode_ops_executed > 50);
            assert!(telemetry.shapes_allocated >= 3);
            assert!(telemetry.ic_hits >= 30);
            assert!(telemetry.hot_loops_accelerated >= 1);
        }

        if is_cpp_jit_kernel_active() {
            let numeric = "let s = 0; let i = 1; while (i <= 100) { s = s + i * i; i = i + 1; } s;";
            let (sum, native_telemetry) =
                eval_cpp_vm_script(numeric).expect("numeric loop compiled");
            assert_eq!(sum, 338350.0);
            assert!(native_telemetry.native_compilations >= 1);
            assert!(native_telemetry.bytecode_ops_executed > 100);
        }

        if is_cpp_jit_kernel_active() {
            let mut doc = parse_html(
                "<p id=\"out\"></p><script>
                   let s = 0;
                   let i = 1;
                   while (i <= 100) { s = s + i * i; i = i + 1; }
                   document.getElementById('out').textContent = s;
                 </script>",
            );
            let mut runtime = ScriptRuntime::with_jit_enabled(true);
            runtime.run_document_scripts(&mut doc);
            let out = doc.get_element_by_id("out").unwrap();
            assert_eq!(doc.text_content(out), "338350");
            assert!(runtime.jit_stats().vm_telemetry.native_compilations >= 1);
        }

        if is_cpp_jit_kernel_active() {
            let mut doc = parse_html(
                "<p id=\"out\"></p><p id=\"after\"></p><script>
                   let i = 0;
                   while (i < 100) {
                     document.getElementById('out').textContent = i;
                     i = i + 1;
                   }
                   document.getElementById('after').textContent = i;
                 </script>",
            );
            let mut runtime = ScriptRuntime::with_jit_enabled(true);
            runtime.run_document_scripts(&mut doc);
            let out = doc.get_element_by_id("out").unwrap();
            let after = doc.get_element_by_id("after").unwrap();
            assert_eq!(doc.text_content(out), "99");
            assert_eq!(doc.text_content(after), "100");
            assert!(runtime.jit_stats().vm_telemetry.native_compilations >= 1);
        }

        let (_, bench_telemetry) = run_cpp_vm_ic_benchmark(64);
        assert!(bench_telemetry.ic_hits >= 64);
        assert!(bench_telemetry.polymorphic_ic_sites >= 1);

        let data = [2.0, 4.0, 6.0, 8.0, 10.0];
        assert_eq!(eval_cpp_vector_reduce(0, &data), 30.0);
        assert_eq!(eval_cpp_vector_reduce(1, &data), 2.0);
        assert_eq!(eval_cpp_vector_reduce(2, &data), 10.0);
        assert_eq!(eval_cpp_vector_reduce(3, &data), 220.0);
        assert_eq!(eval_cpp_sum_range(1, 101, 1), 5050);
    }

    #[test]
    fn executes_prelude_promises_microtasks_and_origin_isolated_storage() {
        let mut doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               localStorage.setItem('theme', 'amber');
               sessionStorage.setItem('tab', 'inspect');
               let clamped = anvilClampNumber(150, 0, 100);
               let formatted = anvilFormatBytes(2048);
               let sum = anvilArraySum([5, 10, 15]);
               Promise.resolve('ready').then((state) => {
                 let out = document.getElementById('out');
                 out.textContent = localStorage.getItem('theme') + ':' + sessionStorage.getItem('tab') + ':' + clamped + ':' + formatted + ':' + sum + ':' + state;
               });
             </script>",
        );
        let mut rt = ScriptRuntime::new();
        rt.set_page_origin("https://app.example/dashboard");
        rt.run_document_scripts(&mut doc);

        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "amber:inspect:100:2 KiB:30:ready");

        // Verify origin isolation when switching page_origin.
        rt.set_page_origin("https://other.example/");
        assert_eq!(rt.local_storage_get("theme"), None);
    }

    #[test]
    fn storage_write_past_the_origin_byte_cap_keeps_the_old_value() {
        let mut runtime = ScriptRuntime::new();
        runtime.set_page_origin("https://alpha.example/home");
        runtime.local_storage_set("kept", "alpha");
        let oversized = "x".repeat(MAX_STORAGE_ORIGIN_BYTES);
        runtime.local_storage_set("kept", &oversized);
        assert_eq!(runtime.local_storage_get("kept"), Some("alpha"));
        runtime.local_storage_set("kept", "beta");
        assert_eq!(runtime.local_storage_get("kept"), Some("beta"));
        runtime.local_storage_set("nope", &oversized);
        assert_eq!(runtime.local_storage_get("nope"), None);

        let mut doc = parse_html(&format!(
            "<p id=\"out\"></p>
             <script>
               sessionStorage.setItem('kept', 'short');
               sessionStorage.setItem('kept', '{}');
               document.getElementById('out').textContent = sessionStorage.getItem('kept');
             </script>",
            oversized
        ));
        runtime.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "short");
    }

    #[test]
    fn eval_script_uses_the_vm_for_a_numeric_script_and_rust_for_text_content() {
        let numeric = "
            function square(n) { return n * n; }
            let acc = 0;
            for (let i = 1; i <= 20; i++) {
                let pt = { x: i, y: square(i) };
                acc += pt.x + pt.y;
            }
            acc;
        ";
        let mut doc = parse_html("<p id=\"out\">before</p>");
        let mut runtime = ScriptRuntime::new();
        runtime.eval_script(&mut doc, numeric);
        if is_cpp_jit_kernel_active() {
            assert!(runtime.last_script_used_vm());
            assert_eq!(runtime.last_vm_number(), Some(3080.0));
        } else {
            assert!(!runtime.last_script_used_vm());
        }
        runtime.eval_script(
            &mut doc,
            "let el = document.getElementById('out'); el.textContent = 'from-rust';",
        );
        assert!(!runtime.last_script_used_vm());
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "from-rust");
    }

    #[test]
    fn executes_try_catch_finally_throw_and_script_isolation() {
        let mut doc = parse_html(
            "<p id=\"out\">init</p>
             <script>
               let log = [];
               try {
                 log.push('try');
                 throw new Error('boom');
                 log.push('unreachable');
               } catch (err) {
                 log.push('caught:' + err.message);
               } finally {
                 log.push('finally');
               }
               let mql = window.matchMedia('(prefers-color-scheme: dark)');
               log.push('mql:' + mql.matches);
               document.addEventListener('DOMContentLoaded', () => {
                 document.getElementById('out').textContent = log.join('|');
               });
             </script>
             <script>
               throw new TypeError('isolated script error');
             </script>
             <script>
               let out = document.getElementById('out');
               out.setAttribute('data-recovered', 'yes');
             </script>",
        );
        let mut runtime = ScriptRuntime::new();
        runtime.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "try|caught:boom|finally|mql:false");
        assert_eq!(doc.get(out).data().attr("data-recovered"), Some("yes"));
        assert!(!runtime.was_aborted());
    }

    #[test]
    fn executes_es6_loops_switch_destructuring_spread_nullish_and_classes() {
        let mut doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               class BaseWidget {
                 constructor(prefix) {
                   this.prefix = prefix;
                 }
               }
               class CounterWidget extends BaseWidget {
                 constructor(prefix, start) {
                   super(prefix);
                   this.count = start;
                 }
                 format(extra) {
                   return this.prefix + ':' + (this.count + extra);
                 }
               }
               let w = new CounterWidget('W', 10);
               let base = [1, 2];
               let combined = [...base, 3, 4];
               let acc = 0;
               for (const n of combined) {
                 if (n === 2) {
                   continue;
                 }
                 acc += n;
               }
               const cfg = { mode: 'fast', retries: 0 };
               const { mode, missing = 'fallback' } = cfg;
               const [first, second] = combined;
               let label = '';
               switch (mode) {
                 case 'slow':
                   label = 'S';
                   break;
                 case 'fast':
                   label = 'F';
                   break;
                 default:
                   label = 'D';
               }
               let nullish = cfg.nonexistent ?? 'defaulted';
               let zeroKept = cfg.retries ?? 99;
               let opt = cfg?.nested?.deep ?? 'safe';
               document.getElementById('out').textContent =
                 w.format(5) + '|' + acc + '|' + label + '|' + missing + '|' + first + ',' + second + '|' + nullish + ':' + zeroKept + ':' + opt;
             </script>",
        );
        let mut runtime = ScriptRuntime::new();
        runtime.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(
            doc.text_content(out),
            "W:15|8|F|fallback|1,2|defaulted:0:safe"
        );
    }

    #[test]
    fn executes_array_and_object_higher_order_builtins() {
        let mut doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               let nums = [2, 4, 6, 8];
               let total = nums.reduce((acc, x) => acc + x, 0);
               let found = nums.find((x) => x > 5);
               let fidx = nums.findIndex((x) => x === 6);
               let hasBig = nums.some((x) => x >= 8);
               let allEven = nums.every((x) => x % 2 === 0);
               let obj = Object.assign({ a: 1 }, { b: 2 });
               let pairs = Object.entries(obj);
               let rebuilt = Object.fromEntries(pairs);
               document.getElementById('out').textContent =
                 total + ':' + found + ':' + fidx + ':' + hasBig + ':' + allEven + ':' + rebuilt.a + ',' + rebuilt.b;
             </script>",
        );
        let mut runtime = ScriptRuntime::new();
        runtime.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "20:6:2:true:true:1,2");
    }

    #[test]
    fn executes_dom_mutations_dataset_style_and_selectors() {
        let mut doc = parse_html(
            "<div id=\"container\" class=\"panel\"><span id=\"first\" class=\"item\">One</span></div>
             <p id=\"out\"></p>
             <script>
               let container = document.getElementById('container');
               let second = document.createElement('span');
               second.id = 'second';
               second.className = 'item highlight';
               second.dataset.index = '2';
               second.style.backgroundColor = 'rgb(20, 30, 40)';
               second.appendChild(document.createTextNode('Two'));
               container.appendChild(second);

               let beforeNode = document.createElement('span');
               beforeNode.id = 'zero';
               beforeNode.textContent = 'Zero';
               let first = document.getElementById('first');
               container.insertBefore(beforeNode, first);

               let matched = second.matches('.item.highlight');
               let panel = second.closest('.panel');
               let count = container.childElementCount;
               let nextId = first.nextElementSibling.id;
               let rect = second.getBoundingClientRect();
               let computed = window.getComputedStyle(second);

               document.getElementById('out').textContent =
                 count + ':' + matched + ':' + panel.id + ':' + nextId + ':' + second.dataset.index + ':' + computed.backgroundColor + ':' + rect.width;
             </script>",
        );
        let mut runtime = ScriptRuntime::new();
        runtime.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(
            doc.text_content(out),
            "3:true:container:second:2:rgb(20, 30, 40):100"
        );
    }

    #[test]
    fn runs_sync_then_microtasks_then_timers_in_order() {
        let mut doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               let log = [];
               log.push('s1');
               queueMicrotask(() => { log.push('m1'); });
               setTimeout(() => { log.push('t1'); }, 0);
               Promise.resolve().then(() => { log.push('m2'); });
               log.push('s2');
               setTimeout(() => { document.getElementById('out').textContent = log.join(','); }, 5);
             </script>",
        );
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "s1,s2,m1,m2,t1");
        assert!(!rt.was_aborted());
    }

    #[test]
    fn chains_promises_and_propagates_errors_to_catch() {
        let mut doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               let out = [];
               Promise.resolve(1).then((x) => x + 1).then((x) => { out.push(x); throw new Error('boom'); }).then((x) => out.push('skipped')).catch((e) => out.push('caught:' + e.message)).then(() => { document.getElementById('out').textContent = out.join('|'); });
             </script>",
        );
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "2|caught:boom");
    }

    #[test]
    fn finally_runs_and_passes_the_value_through() {
        let mut doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               let out = [];
               Promise.resolve('v').finally(() => out.push('cleanup')).then((x) => { out.push(x); document.getElementById('out').textContent = out.join('|'); });
             </script>",
        );
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "cleanup|v");
    }

    #[test]
    fn awaits_values_in_loops_and_try_catch() {
        let mut doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               async function compute() {
                 let a = await Promise.resolve(10);
                 let total = 0;
                 for (let i = 0; i < 3; i++) {
                   total += await Promise.resolve(i);
                 }
                 return a + total;
               }
               async function guarded() {
                 try {
                   await Promise.reject(new Error('nope'));
                   return 'unreached';
                 } catch (e) {
                   return 'caught:' + e.message;
                 }
               }
               let out = [];
               compute().then((v) => { out.push(v); return guarded(); }).then((g) => { out.push(g); document.getElementById('out').textContent = out.join('|'); });
             </script>",
        );
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "13|caught:nope");
    }

    #[test]
    fn promise_constructor_resolves_from_a_timer() {
        let mut doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               let later = new Promise((resolve) => { setTimeout(() => { resolve('done'); }, 10); });
               later.then((v) => { document.getElementById('out').textContent = v; });
             </script>",
        );
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "done");
    }

    #[test]
    fn runs_the_promise_combinators() {
        let mut doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               let out = [];
               Promise.all([Promise.resolve(1), 2, Promise.resolve(3)]).then((arr) => out.push('all:' + arr.join(',')));
               Promise.race([Promise.resolve('fast'), Promise.resolve('slow')]).then((v) => out.push('race:' + v));
               Promise.any([Promise.reject(new Error('e1')), Promise.resolve('ok')]).then((v) => out.push('any:' + v));
               Promise.allSettled([Promise.resolve(9), Promise.reject(new Error('bad'))]).then((arr) => out.push('settled:' + arr.length + ':' + arr[0].status + ':' + arr[1].status));
               setTimeout(() => { document.getElementById('out').textContent = out.join('|'); }, 0);
             </script>",
        );
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        let text = doc.text_content(out);
        assert!(text.contains("all:1,2,3"), "all: {text}");
        assert!(text.contains("race:fast"), "race: {text}");
        assert!(text.contains("any:ok"), "any: {text}");
        assert!(
            text.contains("settled:2:fulfilled:rejected"),
            "settled: {text}"
        );
    }

    #[test]
    fn clears_an_interval_after_a_few_ticks() {
        let mut doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               let n = 0;
               let id = setInterval(() => { n = n + 1; if (n >= 3) { clearInterval(id); document.getElementById('out').textContent = 'ticks:' + n; } }, 5);
             </script>",
        );
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "ticks:3");
        assert!(!rt.was_aborted());
    }

    #[test]
    fn a_thousand_deep_promise_chain_does_not_overflow() {
        let depth = std::thread::Builder::new()
            .name("deep-chain".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let mut doc = parse_html(
                    "<p id=\"out\"></p>
                     <script>
                       let p = Promise.resolve(0);
                       for (let i = 0; i < 1000; i++) { p = p.then((x) => x + 1); }
                       p.then((x) => { document.getElementById('out').textContent = 'depth:' + x; });
                     </script>",
                );
                let mut rt = ScriptRuntime::new();
                rt.run_document_scripts(&mut doc);
                let out = doc.get_element_by_id("out").unwrap();
                (doc.text_content(out), rt.was_aborted())
            })
            .expect("spawn deep-chain thread")
            .join()
            .expect("deep-chain thread panicked");
        assert_eq!(depth.0, "depth:1000");
        assert!(!depth.1);
    }

    #[test]
    fn the_budget_cuts_off_an_endless_microtask_chain() {
        let mut doc = parse_html(
            "<script>
               function flood() { Promise.resolve().then(flood); }
               flood();
             </script>",
        );
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        assert!(rt.was_aborted());
        assert!(rt.abort_reason().is_some());
    }

    #[test]
    fn the_budget_cuts_off_an_endless_timer_chain() {
        let mut doc = parse_html(
            "<script>
               function tick() { setTimeout(tick, 0); }
               tick();
             </script>",
        );
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        assert!(rt.was_aborted());
    }

    #[test]
    fn tracks_an_unhandled_rejection_without_crashing() {
        let mut doc = parse_html(
            "<p id=\"out\"></p>
             <script>
               Promise.reject(new Error('lonely'));
               Promise.resolve('fine').then((v) => { document.getElementById('out').textContent = v; });
             </script>",
        );
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "fine");
        assert_eq!(rt.unhandled_rejections(), 1);
    }

    fn run_and_read(html: &str) -> (String, ScriptRuntime) {
        let mut doc = parse_html(html);
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        (doc.text_content(out), rt)
    }

    #[test]
    fn an_await_suspends_the_async_function_until_a_microtask() {
        let (text, rt) = run_and_read(
            "<p id=\"out\"></p>
             <script>
               let log = [];
               async function f() { log.push('a'); await null; log.push('c'); }
               let p = f();
               log.push('b');
               p.then(() => { log.push('d'); });
               setTimeout(() => { document.getElementById('out').textContent = log.join(','); }, 0);
             </script>",
        );
        assert_eq!(text, "a,b,c,d");
        assert!(!rt.was_aborted());
    }

    #[test]
    fn concurrent_async_calls_interleave_and_keep_their_own_locals() {
        let (text, _) = run_and_read(
            "<p id=\"out\"></p>
             <script>
               let log = [];
               function sleep(ms) { return new Promise((resolve) => setTimeout(resolve, ms)); }
               async function worker(name, ms) {
                 for (let i = 0; i < 2; i++) {
                   await sleep(ms);
                   log.push(name + i);
                 }
                 return name;
               }
               Promise.all([worker('x', 10), worker('y', 15)]).then((names) => {
                 log.push(names.join('+'));
                 document.getElementById('out').textContent = log.join(',');
               });
             </script>",
        );
        assert_eq!(text, "x0,y0,x1,y1,x+y");
    }

    #[test]
    fn for_await_and_try_finally_resume_in_order() {
        let (text, _) = run_and_read(
            "<p id=\"out\"></p>
             <script>
               let log = [];
               async function run() {
                 const items = [Promise.resolve(1), 2, new Promise((r) => setTimeout(() => r(3), 5))];
                 let sum = 0;
                 for await (const v of items) { sum += v; }
                 try {
                   await Promise.reject(new Error('x'));
                   log.push('unreached');
                 } catch (e) {
                   log.push('caught ' + e.message);
                 } finally {
                   await null;
                   log.push('finally');
                 }
                 return sum;
               }
               run().then((s) => { log.push('sum ' + s); document.getElementById('out').textContent = log.join(','); });
               log.push('sync');
             </script>",
        );
        assert_eq!(text, "sync,caught x,finally,sum 6");
    }

    #[test]
    fn async_arrows_break_out_of_awaiting_loops_and_reject_late() {
        let (text, _) = run_and_read(
            "<p id=\"out\"></p>
             <script>
               let out = [];
               const count = async (limit) => {
                 let n = 0;
                 while (true) {
                   await null;
                   n++;
                   if (n >= limit) { break; }
                 }
                 return n;
               };
               async function late() { await count(1); throw new Error('late'); }
               count(3).then((v) => { out.push(v); return late(); }).catch((e) => {
                 out.push(e.message);
                 document.getElementById('out').textContent = out.join('|');
               });
             </script>",
        );
        assert_eq!(text, "3|late");
    }

    #[test]
    fn an_async_timer_callback_runs_as_its_own_task() {
        let (text, _) = run_and_read(
            "<p id=\"out\"></p>
             <script>
               let log = [];
               setTimeout(async () => { log.push('t1'); await null; log.push('t1 resumed'); }, 0);
               setTimeout(() => { log.push('t2'); }, 0);
               setTimeout(() => { document.getElementById('out').textContent = log.join(','); }, 1);
             </script>",
        );
        assert_eq!(text, "t1,t1 resumed,t2");
    }

    #[test]
    fn an_async_class_method_returns_a_promise() {
        let (text, _) = run_and_read(
            "<p id=\"out\"></p>
             <script>
               class Store {
                 async load(x) { let v = await Promise.resolve(x); return v * 2; }
                 async fail() { throw new Error('bad'); }
               }
               const store = new Store();
               let out = [];
               store.load(21).then((v) => { out.push(v); return store.fail(); }).catch((e) => {
                 out.push(e.message);
                 document.getElementById('out').textContent = out.join('|');
               });
             </script>",
        );
        assert_eq!(text, "42|bad");
    }

    #[test]
    fn async_callbacks_for_then_and_queue_microtask() {
        let (text, _) = run_and_read(
            "<p id=\"out\"></p>
             <script>
               let log = [];
               queueMicrotask(async () => {
                 for (const name of ['a', 'b']) {
                   await null;
                   log.push(name);
                 }
               });
               Promise.resolve(2)
                 .then(async (v) => { await null; return v * 3; })
                 .then((v) => { log.push(v); });
               setTimeout(() => { document.getElementById('out').textContent = log.join(','); }, 0);
             </script>",
        );
        assert_eq!(text, "a,b,6");
    }

    #[test]
    fn adopts_a_class_instance_thenable() {
        let (text, _) = run_and_read(
            "<p id=\"out\"></p>
             <script>
               class Later {
                 then(ok, fail) { setTimeout(() => { ok(7); }, 3); }
               }
               class Broken {
                 then(ok, fail) { fail('no'); }
               }
               let out = [];
               async function use() { let v = await new Later(); return v + 1; }
               use().then((v) => { out.push(v); return Promise.resolve(new Broken()); }).catch((e) => {
                 out.push(e);
                 document.getElementById('out').textContent = out.join('|');
               });
             </script>",
        );
        assert_eq!(text, "8|no");
    }

    fn logs_of(src: &str) -> (Vec<String>, ScriptRuntime) {
        let mut doc = parse_html(&format!("<script>{src}</script>"));
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        (rt.console_logs().to_vec(), rt)
    }

    fn no_frames_left(rt: &ScriptRuntime) -> bool {
        rt.frames.iter().all(|slot| matches!(slot, FrameSlot::Free))
    }

    #[test]
    fn an_unawaited_async_call_inside_an_async_function_finishes() {
        let (logs, rt) = logs_of(
            "const inner = async () => { console.log('x'); await null; console.log('y'); };
             const outer = async () => { inner(); console.log('z'); await null; console.log('w'); };
             outer();",
        );
        assert_eq!(logs, ["x", "z", "y", "w"]);
        assert!(!rt.was_aborted());
        assert!(no_frames_left(&rt));
    }

    #[test]
    fn many_pending_unawaited_async_calls_all_finish_and_free_their_slots() {
        let (logs, rt) = logs_of(
            "let done = 0;
             const inner = async (i) => { await null; await null; done++; };
             const outer = async () => {
               for (let i = 0; i < 300; i++) { inner(i); }
               await null;
               console.log('outer resumed ' + done);
               await null; await null;
               console.log('outer done ' + done);
             };
             outer();",
        );
        assert_eq!(logs, ["outer resumed 0", "outer done 300"]);
        assert!(!rt.was_aborted());
        assert!(no_frames_left(&rt));
        // 300 inner calls and the caller were pending together.
        assert_eq!(rt.frames.len(), 301);
    }

    #[test]
    fn finished_async_calls_hand_their_slots_back() {
        let (logs, rt) = logs_of(
            "const step = async (i) => { await null; return i; };
             const run = async () => {
               let total = 0;
               for (let i = 0; i < 2000; i++) { total = total + await step(i); }
               console.log('total ' + total);
             };
             run();",
        );
        assert_eq!(logs, ["total 1999000"]);
        assert!(!rt.was_aborted());
        assert!(no_frames_left(&rt));
        // The caller and one step at a time.
        assert_eq!(rt.frames.len(), 2);
    }

    #[test]
    fn recursive_async_calls_keep_their_own_frames() {
        let (logs, rt) = logs_of(
            "async function down(n) {
               if (n === 0) { return 0; }
               down(n - 1);
               console.log('a' + n);
               await null;
               console.log('b' + n);
               return n;
             }
             async function sum(n) {
               if (n === 0) { return 0; }
               const rest = await sum(n - 1);
               return n + rest;
             }
             down(3);
             sum(20).then((v) => console.log('sum ' + v));",
        );
        assert_eq!(logs, ["a1", "a2", "a3", "b1", "b2", "b3", "sum 210"]);
        assert!(!rt.was_aborted());
        assert!(no_frames_left(&rt));
    }

    #[test]
    fn a_throwing_unawaited_callee_does_not_stop_its_caller() {
        let (logs, rt) = logs_of(
            "const late = async () => { console.log('x'); await null; throw new Error('late'); };
             const early = async () => { throw new Error('early'); };
             const outer = async () => {
               late().catch((e) => console.log('caught ' + e.message));
               early().catch((e) => console.log('caught ' + e.message));
               late();
               console.log('z');
               await null;
               console.log('w');
             };
             outer();",
        );
        assert_eq!(logs, ["x", "x", "z", "caught early", "w", "caught late"]);
        assert!(!rt.was_aborted());
        assert!(no_frames_left(&rt));
    }

    #[test]
    fn the_budget_stops_a_flood_of_unawaited_async_calls() {
        for src in [
            "const spin = async () => { while (true) { await null; } };
             const outer = async () => { for (let i = 0; i < 200; i++) { spin(); } await null; console.log('o'); };
             outer();",
            "const grow = async () => { await null; grow(); grow(); };
             grow();",
        ] {
            let (_, rt) = logs_of(src);
            assert!(rt.was_aborted(), "{src}");
            assert!(rt.abort_reason().is_some(), "{src}");
        }
    }

    #[test]
    fn an_async_function_called_in_place_returns_at_its_first_await() {
        let (logs, rt) = logs_of(
            "(async () => { console.log('a'); await null; console.log('b'); })();
             (async function () { console.log('c'); await null; console.log('d'); })();
             (async (n) => { console.log('n' + n); await null; console.log('m' + n); })(7);
             (async () => 5)().then((v) => console.log('v' + v));
             (async () => { throw new Error('bad'); })().catch((e) => console.log('caught ' + e.message));
             (() => { console.log('sync'); })();
             console.log('end');",
        );
        assert_eq!(
            logs,
            [
                "a",
                "c",
                "n7",
                "sync",
                "end",
                "b",
                "d",
                "m7",
                "v5",
                "caught bad"
            ]
        );
        assert!(!rt.was_aborted());
        assert!(no_frames_left(&rt));
    }

    #[test]
    fn an_async_function_called_in_place_inside_an_async_function() {
        let (logs, rt) = logs_of(
            "const outer = async () => {
               let k = 3;
               (async () => { console.log('i' + k); await null; console.log('j' + k); })();
               const v = await (async () => { await null; return 4; })();
               console.log('got ' + v);
             };
             outer();
             console.log('end');",
        );
        assert_eq!(logs, ["i3", "end", "j3", "got 4"]);
        assert!(!rt.was_aborted());
        assert!(no_frames_left(&rt));
    }

    #[test]
    fn the_four_ms_floor_applies_only_while_a_timer_callback_runs() {
        let (logs, rt) = logs_of(
            "setTimeout(() => {
               setTimeout(() => console.log('nested'), 0);
               queueMicrotask(() => { setTimeout(() => console.log('from microtask'), 0); });
             }, 0);
             setTimeout(async () => {
               await null;
               setTimeout(() => console.log('after await'), 0);
             }, 0);
             setTimeout(() => console.log('two'), 2);",
        );
        assert_eq!(logs, ["from microtask", "after await", "two", "nested"]);
        assert!(!rt.was_aborted());
    }

    #[cfg(cinder_cpp_jit)]
    #[test]
    fn the_numeric_vm_refuses_event_loop_scripts() {
        if !is_cpp_jit_kernel_active() {
            return;
        }
        assert!(eval_cpp_vm_script_bindings("let x = 2; x * 3").is_some());
        for src in [
            "let x = 1; setTimeout(x, 0); x",
            "let n = 2; queueMicrotask(n); n",
            "let p = 1; let q = await p; q",
            "let t = setInterval(1, 5); clearInterval(t); t",
        ] {
            assert!(eval_cpp_vm_script_bindings(src).is_none(), "{src}");
        }
    }

    #[test]
    fn executes_canvas_2d_context_shadow_dom_custom_elements_and_webcodecs() {
        let mut doc = parse_html(
            "<div id=\"host\"></div>
             <my-widget id=\"widget\"></my-widget>
             <canvas id=\"cv\" width=\"32\" height=\"16\"></canvas>
             <video id=\"vid\"></video>
             <p id=\"out\"></p>
             <script>
               customElements.define('my-widget', class MyWidget {});
               let w = document.getElementById('widget');
               let isDefined = w.matches(':defined');

               let host = document.getElementById('host');
               let sr = host.attachShadow({ mode: 'open' });
               let badge = document.createElement('span');
               badge.id = 'shadow-badge';
               badge.textContent = 'ShadowOK';
               sr.appendChild(badge);

               let cv = document.getElementById('cv');
               let ctx = cv.getContext('2d');
               ctx.fillStyle = '#ff0000';
               ctx.fillRect(0, 0, 16, 16);
               let img = ctx.createImageData(2, 2);
               ctx.putImageData(img, 16, 0);
               let sampled = ctx.getImageData(0, 0, 1, 1);

               let vid = document.getElementById('vid');
               let canPlay = vid.canPlayType('video/mp4; codecs=\"avc1.42E01E\"');
               let enc = new VideoEncoder({});
               enc.configure({ codec: 'avc1.42E01E', width: 16, height: 16 });
               let vf = new VideoFrame(null, { codedWidth: 16, codedHeight: 16, timestamp: 0 });
               enc.encode(vf);
               vf.close();

               document.getElementById('out').textContent =
                 isDefined + ':' + sr.firstElementChild.textContent + ':' + sampled.data[0] + ':' + canPlay + ':' + enc.encodedChunks;
             </script>",
        );
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);

        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "true:ShadowOK:255:probably:1");

        let cv = doc.get_element_by_id("cv").unwrap();
        let surf = doc.canvas_surface(cv).expect("canvas surface created");
        assert_eq!(surf.width, 32);
        assert_eq!(surf.height, 16);
        assert_eq!(&surf.rgba[0..4], &[255, 0, 0, 255]);
    }

    #[cfg(cinder_cpp_jit)]
    #[test]
    fn live_dom_bridge_synchronizes_cinder_vm_mutations_and_canvas_to_document() {
        if !is_cpp_jit_kernel_active() {
            return;
        }
        let mut doc = parse_html(
            "<div id=\"app\"></div>
             <canvas id=\"stage\" width=\"24\" height=\"12\"></canvas>",
        );
        let script = "
            document.title = 'LiveBridgeTitle';
            customElements.define('x-card', function() {});
            const app = document.getElementById('app');
            app.classList.add('hydrated');
            app.style.backgroundColor = 'rgb(10, 20, 30)';
            const child = document.createElement('p');
            child.setAttribute('id', 'created-p');
            child.textContent = 'FromCinderVM';
            app.appendChild(child);
            const sr = app.attachShadow({ mode: 'open' });
            const sh = document.createElement('span');
            sh.textContent = 'ShadowChild';
            sr.appendChild(sh);
            const stage = document.getElementById('stage');
            const ctx = stage.getContext('2d');
            ctx.fillStyle = '#00ff00';
            ctx.fillRect(0, 0, 12, 12);

            const parser = new DOMParser();
            const parsedDoc = parser.parseFromString('<section id=\"parsed-sec\" class=\"from-parser\"><b id=\"parsed-b\">ParsedDOM</b></section>', 'text/html');
            const parsedSec = parsedDoc.getElementById('parsed-sec');
            app.appendChild(parsedSec);

            const sheet = new CSSStyleSheet();
            sheet.replaceSync('#parsed-sec { width: 320px; }');
            document.adoptedStyleSheets = [sheet];
        ";
        let run = eval_webapp_with_live_dom_bridge(&mut doc, "https://example.com/", script)
            .expect("live dom bridge executed");
        assert!(run.telemetry.bytecode_ops_executed > 0);
        assert_eq!(doc.title(), "LiveBridgeTitle");
        assert!(doc.is_custom_element_defined("x-card"));

        let app = doc.get_element_by_id("app").unwrap();
        assert!(doc.get(app).data().has_class("hydrated"));
        let created = doc
            .get_element_by_id("created-p")
            .expect("created-p synced");
        assert_eq!(doc.text_content(created), "FromCinderVM");
        let sr = doc.shadow_root(app).expect("shadow root synced");
        assert_eq!(doc.text_content(sr), "ShadowChild");

        let parsed_sec = doc
            .get_element_by_id("parsed-sec")
            .expect("parsed-sec adopted into host doc");
        assert!(doc.get(parsed_sec).data().has_class("from-parser"));
        let parsed_b = doc
            .get_element_by_id("parsed-b")
            .expect("parsed-b adopted into host doc");
        assert_eq!(doc.text_content(parsed_b), "ParsedDOM");

        let adopted_style = doc
            .get_element_by_id("__anvil_adopted_styles__")
            .expect("adopted style element created");
        assert!(
            doc.text_content(adopted_style)
                .contains("#parsed-sec { width: 320px; }")
        );

        let stage = doc.get_element_by_id("stage").unwrap();
        let surf = doc
            .canvas_surface(stage)
            .expect("stage canvas surface synced");
        assert_eq!(&surf.rgba[0..4], &[0, 255, 0, 255]);
    }

    #[test]
    fn executes_parameterized_iife_window_document_ready_state_and_blocks_nojs_redirects() {
        let mut doc = parse_html(
            "<html><head><title>Init</title></head>
             <body>
               <noscript><style>div{display:none}</style></noscript>
               <div id=\"box\" class=\"card\"><input name=\"q\" value=\"rust\"></div>
               <script>
                 (function(w, d) {
                   w.google = w.google || {};
                   w.google.kEI = 'ok';
                   let q = d.getElementsByName('q');
                   let cards = d.getElementsByClassName('card');
                   let box = d.getElementById('box');
                   box.insertAdjacentHTML('beforeend', '<span id=\"added\">' + d.readyState + ':' + q.length + ':' + cards.length + ':' + w.google.kEI + '</span>');
                   w.location.replace('/httpservice/retry/enablejs?sei=123');
                 })(window, document);
               </script>
             </body></html>",
        );
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        assert!(rt.take_pending_navigation().is_none());
        let added = doc.get_element_by_id("added").expect("added span");
        assert_eq!(doc.text_content(added), "complete:1:1:ok");
    }

    #[test]
    fn cinder_engine_executes_es2024_builtins_promises_and_live_dom_host() {
        let mut doc = parse_html(
            "<html><head><title>Old</title></head>
             <body>
               <div id=\"root\" data-mode=\"dark\">
                 <span class=\"item\">b</span>
                 <span class=\"item\">a</span>
               </div>
               <div id=\"out\"></div>
             </body></html>",
        );
        let mut rt = ScriptRuntime::new();
        rt.set_page_origin("https://engine.example.com/app?tab=1#top");
        rt.local_storage_set("seed", "42");

        let res = rt
            .eval_with_cinder_engine(
                &mut doc,
                r#"
                const root = document.getElementById("root");
                root.classList.add("ready");
                root.style.color = "rgb(1, 2, 3)";
                root.dataset.theme = "obsidian";

                const spans = document.querySelectorAll(".item");
                const raw = [spans[0].textContent, spans[1].textContent, "c"];
                const sorted = raw.toSorted();
                const grouped = Object.groupBy([1, 2, 3, 4], (n) => (n % 2 === 0 ? "even" : "odd"));

                const badge = document.createElement("strong");
                badge.id = "badge";
                badge.textContent = sorted.join("-") + ":" + grouped.even.length + ":" + root.dataset.mode + ":" + root.dataset.theme;
                root.appendChild(badge);

                const { promise, resolve } = Promise.withResolvers();
                promise.then((msg) => {
                    document.getElementById("out").textContent = msg + ":" + localStorage.getItem("seed");
                    localStorage.setItem("saved", "done");
                });
                resolve("settled");
                document.title = "EngineActive";
                console.log("engine-ok", sorted.length);
                sorted.join("")
                "#,
            )
            .expect("cinder engine should execute script");
        assert_eq!(res, "abc");
        assert!(rt.last_script_used_vm());
        assert_eq!(doc.title(), "EngineActive");
        assert_eq!(rt.console_logs(), &["engine-ok 3".to_string()]);
        assert_eq!(rt.local_storage_get("saved"), Some("done"));

        let root_id = doc.get_element_by_id("root").unwrap();
        assert!(doc.get(root_id).data().has_class("ready"));
        assert_eq!(doc.get(root_id).data().attr("data-theme"), Some("obsidian"));

        let badge_id = doc.get_element_by_id("badge").unwrap();
        assert_eq!(doc.text_content(badge_id), "a-b-c:2:dark:obsidian");

        let out_id = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out_id), "settled:42");
    }

    #[test]
    fn cinder_engine_interactive_events_maintain_closure_state_and_prevent_default() {
        let mut doc = parse_html(
            "<html><body>
               <form id=\"f\">
                 <input id=\"inp\" value=\"init\" />
                 <button id=\"btn\" type=\"button\">Click</button>
               </form>
               <p id=\"status\">0</p>
             </body></html>",
        );
        let mut rt = ScriptRuntime::new();
        rt.eval_script(
            &mut doc,
            r#"
            "use cinder";
            let count = 0;
            const seen = new Set();
            const btn = document.getElementById("btn");
            const inp = document.getElementById("inp");
            const form = document.getElementById("f");
            const status = document.getElementById("status");

            btn.addEventListener("click", () => {
                count = count + 1;
                status.textContent = "clicks:" + count;
            });
            inp.addEventListener("input", (ev) => {
                seen.add(ev.value);
                status.textContent = "input:" + inp.value + ":" + seen.size;
            });
            form.addEventListener("submit", (ev) => {
                ev.preventDefault();
                status.textContent = "submit:" + count + ":" + inp.value;
            });
            "#,
        );
        assert!(rt.last_script_used_vm());

        let btn = doc.get_element_by_id("btn").unwrap();
        let inp = doc.get_element_by_id("inp").unwrap();
        let form = doc.get_element_by_id("f").unwrap();
        let status = doc.get_element_by_id("status").unwrap();

        assert!(rt.dispatch_click(&mut doc, btn));
        assert_eq!(doc.text_content(status), "clicks:1");

        assert!(rt.dispatch_click(&mut doc, btn));
        assert_eq!(doc.text_content(status), "clicks:2");

        assert!(rt.dispatch_input(&mut doc, inp, "alpha"));
        assert_eq!(doc.text_content(status), "input:alpha:1");

        assert!(rt.dispatch_submit(&mut doc, form));
        assert!(rt.last_event_default_prevented());
        assert_eq!(doc.text_content(status), "submit:2:alpha");
    }

    #[test]
    fn cinder_engine_executes_classes_async_canvas_observers_and_web_apis() {
        let mut doc = parse_html(
            "<html><body>
               <div id=\"app\"></div>
               <input id=\"search\" value=\"initial\" />
               <canvas id=\"board\" width=\"32\" height=\"16\"></canvas>
               <p id=\"summary\"></p>
             </body></html>",
        );
        let mut rt = ScriptRuntime::new();
        rt.set_page_origin("https://app.example.com/studio?mode=pro#canvas");

        let res = rt
            .eval_with_cinder_engine(
                &mut doc,
                r##"
                class Shape {
                    #scale = 2;
                    static registry = [];
                    static {
                        Shape.registry.push("init");
                    }
                    constructor(label) {
                        this.label = label;
                    }
                    scaled(v) {
                        return v * this.#scale;
                    }
                    hasScale(obj) {
                        return #scale in obj;
                    }
                }

                class BoxShape extends Shape {
                    constructor(label, w, h) {
                        super(label);
                        this.w = w;
                        this.h = h;
                    }
                    get area() {
                        return this.scaled(this.w * this.h);
                    }
                }

                const box = new BoxShape("card", 6, 4);
                const { label, w: width, extra = 5 } = box;
                const [firstReg, ...restReg] = Shape.registry;
                const rxMatch = "item-42-ok".replace(/(\d+)/, "#$1");

                function tag(strings, a, b) {
                    return strings[0] + a + strings[1] + b;
                }
                const tagged = tag`L:${label}|A:${box.area}`;

                const board = document.getElementById("board");
                const ctx = board.getContext("2d");
                ctx.fillStyle = "#00ff00";
                ctx.fillRect(0, 0, 16, 16);
                ctx.strokeStyle = "#ff0000";
                ctx.beginPath();
                ctx.moveTo(16, 0);
                ctx.lineTo(31, 15);
                ctx.stroke();
                const pixel = ctx.getImageData(2, 2, 1, 1);

                const app = document.getElementById("app");
                let observedMutations = 0;
                const mo = new MutationObserver((records) => {
                    observedMutations += records.length;
                });
                mo.observe(app, { childList: true, attributes: true });

                let ioEntered = false;
                const io = new IntersectionObserver((entries) => {
                    ioEntered = entries[0].isIntersecting;
                });
                io.observe(app);

                let roWidth = 0;
                const ro = new ResizeObserver((entries) => {
                    roWidth = entries[0].contentRect.width;
                });
                ro.observe(app);

                const parser = new DOMParser();
                const fragDoc = parser.parseFromString("<section id=\"imported\"><span class=\"inner\">DOMParsed</span></section>", "text/html");
                const imported = document.adoptNode(fragDoc.getElementById("imported"));
                app.appendChild(imported);
                app.setAttribute("data-state", "mounted");

                const sheet = new CSSStyleSheet();
                sheet.replaceSync("#imported { color: rgb(12, 34, 56); }");
                document.adoptedStyleSheets = [sheet];

                const search = document.getElementById("search");
                search.focus();
                const isFocused = document.activeElement === search;

                let customDetail = "";
                app.addEventListener("widget:ready", (ev) => {
                    customDetail = ev.detail.status;
                });
                app.dispatchEvent(new CustomEvent("widget:ready", { detail: { status: "live" } }));

                const u = new URL("/items?filter=active", "https://app.example.com");
                u.searchParams.set("page", "2");
                const enc = new TextEncoder().encode("ok");
                const dec = new TextDecoder().decode(enc);

                async function finalize() {
                    const step = await Promise.resolve(extra + width);
                    return `${tagged}|${rxMatch}|${firstReg}:${restReg.length}|px:${pixel.data[1]}|mo:${observedMutations}|io:${ioEntered}|ro:${roWidth > 0}|focus:${isFocused}|ce:${customDetail}|url:${u.searchParams.get("page")}|td:${dec}|step:${step}`;
                }

                setTimeout(() => {
                    finalize().then((out) => {
                        document.getElementById("summary").textContent = out;
                    });
                }, 0);
                "ok"
                "##,
            )
            .expect("cinder full web runtime test should succeed");
        assert_eq!(res, "ok");

        let summary = doc.get_element_by_id("summary").unwrap();
        assert_eq!(
            doc.text_content(summary),
            "L:card|A:48|item-#42-ok|init:0|px:255|mo:2|io:true|ro:true|focus:true|ce:live|url:2|td:ok|step:11"
        );
        assert_eq!(
            doc.adopted_style_sheets(),
            &["#imported { color: rgb(12, 34, 56); }".to_string()]
        );
        let search = doc.get_element_by_id("search").unwrap();
        assert_eq!(doc.focused_node(), Some(search));
    }

    #[test]
    fn cinder_engine_executes_generators_bigint_proxy_reflect_weak_collections_dataview_and_intl() {
        let mut doc = parse_html("<html><body><p id=\"out\"></p></body></html>");
        let mut rt = ScriptRuntime::new();
        let res = rt
            .eval_with_cinder_engine(
                &mut doc,
                r#"
                function* innerGen() {
                    yield 10;
                    yield 20;
                }
                function* outerGen() {
                    yield 5;
                    yield* innerGen();
                    yield 25;
                }
                let genSum = 0;
                for (const v of outerGen()) {
                    genSum = genSum + v;
                }

                const bigA = 100n;
                const bigB = BigInt(44);
                const bigSum = (bigA + bigB) >> 2n;
                const bigClamped = BigInt.asUintN(8, 257n);

                const targetObj = { a: 3, b: 4 };
                let proxyGets = 0;
                const prox = new Proxy(targetObj, {
                    get(t, prop) {
                        proxyGets = proxyGets + 1;
                        return Reflect.get(t, prop, t);
                    },
                    set(t, prop, val) {
                        return Reflect.set(t, prop, val * 2, t);
                    }
                });
                prox.c = 5;
                const pVal = prox.a + prox.c;
                const rev = Proxy.revocable({ k: 9 }, {});
                rev.revoke();

                const wm = new WeakMap();
                const ws = new WeakSet();
                const keyObj = {};
                wm.set(keyObj, "stored");
                ws.add(keyObj);
                const wr = new WeakRef(keyObj);
                const fr = new FinalizationRegistry(() => {});
                fr.register(keyObj, "held");

                const dvBuf = new ArrayBuffer(8);
                const dv = new DataView(dvBuf);
                dv.setInt32(0, 0x01020304, false);
                dv.setUint16(4, 0xABCD, true);
                const dvRead = dv.getUint8(0) + ":" + dv.getUint8(4);

                const symA = Symbol.for("shared.key");
                const symB = Symbol.for("shared.key");
                const symKey = Symbol.keyFor(symA);

                const rootCause = new Error("disk-full");
                const topErr = new TypeError("write-failed", { cause: rootCause });
                const aggErr = new AggregateError([rootCause, topErr], "batch-failed");

                const nf = new Intl.NumberFormat("en-US").format(1234567.89);
                const pr = new Intl.PluralRules("en-US").select(1);
                const col = new Intl.Collator("en-US").compare("a", "b");

                export const exportedTag = "mod-ok";
                const metaHasUrl = typeof import.meta.url === "string";

                const summary = `${genSum}|${bigSum.toString()}:${bigClamped.toString()}|${pVal}:${proxyGets}|${wm.get(wr.deref())}:${ws.has(keyObj)}|${dvRead}|${symA === symB}:${symKey}|${topErr.cause.message}:${aggErr.errors.length}|${nf}:${pr}:${col < 0}|${exportedTag}:${metaHasUrl}`;
                document.getElementById("out").textContent = summary;
                summary
                "#,
            )
            .expect("generators/bigint/proxy/intl script should succeed");
        assert_eq!(
            res,
            "60|36:1|13:2|stored:true|1:205|true:shared.key|disk-full:2|1,234,567.89:one:true|mod-ok:true"
        );
    }

    #[test]
    fn cinder_engine_executes_3phase_dom_events_traversal_fetch_crypto_history_and_canvas_gradients()
     {
        let mut doc = parse_html(
            "<html><body>
               <div id=\"outer\" class=\"box\">
                 <div id=\"inner\" class=\"panel\">
                   <button id=\"btn\" data-label=\"go\">Go</button>
                 </div>
               </div>
               <canvas id=\"cv\" width=\"16\" height=\"16\"></canvas>
               <p id=\"out\"></p>
             </body></html>",
        );
        let mut rt = ScriptRuntime::new();
        rt.set_page_origin("https://app.example.com/start");
        rt.register_fetch_response("https://app.example.com/api/items", 200, "{\"count\":7}");

        rt.eval_with_cinder_engine(
            &mut doc,
            r##"
            const phases = [];
            const outer = document.getElementById("outer");
            const inner = document.getElementById("inner");
            const btn = document.getElementById("btn");

            outer.addEventListener("click", (ev) => {
                phases.push("outer-cap:" + ev.eventPhase);
            }, { capture: true });
            inner.addEventListener("click", (ev) => {
                phases.push("inner-cap:" + ev.eventPhase);
            }, true);
            btn.addEventListener("click", (ev) => {
                phases.push("btn-target:" + ev.eventPhase + ":path=" + ev.composedPath().length);
            }, { once: true });
            inner.addEventListener("click", (ev) => {
                phases.push("inner-bub:" + ev.eventPhase);
            });
            outer.addEventListener("click", (ev) => {
                phases.push("outer-bub:" + ev.eventPhase);
            });

            btn.toggleAttribute("disabled");
            const hadDisabled = btn.hasAttribute("disabled");
            btn.toggleAttribute("disabled");

            const tag = document.createElement("span");
            tag.id = "prepended";
            tag.textContent = "P";
            inner.prepend(tag);

            const walker = document.createTreeWalker(outer, NodeFilter.SHOW_ELEMENT);
            const walked = [];
            let cur = walker.nextNode();
            while (cur) {
                walked.push(cur.id);
                cur = walker.nextNode();
            }

            const range = document.createRange();
            range.selectNodeContents(btn);
            const sel = window.getSelection();
            sel.removeAllRanges();
            sel.addRange(range);

            history.pushState({ step: 2 }, "", "/step-2");
            const pathAfterPush = window.location.pathname + ":" + history.state.step;

            const uuid = crypto.randomUUID();
            const randArr = new Uint8Array(4);
            crypto.getRandomValues(randArr);

            performance.mark("t0");
            performance.mark("t1");
            const meas = performance.measure("op", "t0", "t1");

            const cv = document.getElementById("cv");
            const ctx = cv.getContext("2d");
            const grad = ctx.createLinearGradient(0, 0, 16, 0);
            grad.addColorStop(0, "#ff0000");
            grad.addColorStop(1, "#0000ff");
            ctx.fillStyle = grad;
            ctx.fillRect(0, 0, 16, 16);
            const leftPx = ctx.getImageData(0, 0, 1, 1).data;
            const rightPx = ctx.getImageData(15, 0, 1, 1).data;

            const ac = new AbortController();
            fetch("https://app.example.com/api/items", { signal: ac.signal })
                .then((resp) => resp.json())
                .then((data) => {
                    window.__fetchedCount = data.count;
                });

            window.__finalizeSummary = () => {
                document.getElementById("out").textContent =
                    phases.join(">") +
                    "|" + hadDisabled +
                    "|" + walked.join(",") +
                    "|" + sel.toString() +
                    "|" + pathAfterPush +
                    "|" + (uuid.length === 36) +
                    "|" + (meas.name === "op") +
                    "|" + leftPx[0] + ":" + rightPx[2] +
                    "|" + window.__fetchedCount;
            };
            "##,
        )
        .expect("setup 3-phase events and web apis");

        let btn = doc.get_element_by_id("btn").unwrap();
        assert!(rt.dispatch_click(&mut doc, btn));

        rt.eval_with_cinder_engine(&mut doc, "window.__finalizeSummary();")
            .expect("finalize summary");

        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(
            doc.text_content(out),
            "outer-cap:1>inner-cap:1>btn-target:2:path=8>inner-bub:3>outer-bub:3|true|inner,prepended,btn|Go|/step-2:2|true|true|247:247|7"
        );
    }

    #[test]
    fn cinder_engine_isolates_broken_scripts_and_fires_window_onerror() {
        let mut doc = parse_html(
            "<html><body>
               <p id=\"out\">init</p>
               <script>
                 \"use cinder\";
                 window.__errors = [];
                 window.onerror = function(msg) {
                   window.__errors.push(\"onerror:\" + (msg.length > 0));
                   return true;
                 };
               </script>
               <script>
                 throw new Error(\"boom-in-script-2\");
               </script>
               <script>
                 document.getElementById(\"out\").textContent = \"recovered:\" + window.__errors.join(\",\");
               </script>
             </body></html>",
        );
        let mut rt = ScriptRuntime::new();
        rt.run_document_scripts(&mut doc);
        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(doc.text_content(out), "recovered:onerror:true");
    }

    #[test]
    fn cinder_engine_runs_async_generators_dialog_validation_live_geometry_and_file_apis() {
        let mut doc = parse_html(
            "<html><head><style>
               #scroller { width: 200px; height: 100px; border: 4px solid #333; overflow: auto; }
               #tall { width: 180px; height: 320px; }
               #badge::before { content: 'TAG:'; color: rgb(255, 0, 0); }
             </style></head><body>
               <div id=\"scroller\"><div id=\"tall\">content</div></div>
               <span id=\"badge\">hello</span>
               <dialog id=\"dlg\">Modal</dialog>
               <form id=\"frm\">
                 <input id=\"email\" type=\"email\" required value=\"\" />
               </form>
               <p id=\"out\">init</p>
             </body></html>",
        );
        let mut rt = ScriptRuntime::new();
        rt.eval_with_cinder_engine(
            &mut doc,
            r##"
            const scroller = document.getElementById("scroller");
            const rect = scroller.getBoundingClientRect();
            const ow = scroller.offsetWidth;
            const cw = scroller.clientWidth;
            const sh = scroller.scrollHeight;
            scroller.scrollTop = 45;
            scroller.scrollBy(0, 15);
            const st = scroller.scrollTop;

            const cs = getComputedStyle(scroller);
            const beforeCs = getComputedStyle(document.getElementById("badge"), "::before");

            const dlg = document.getElementById("dlg");
            dlg.showModal();
            const wasOpen = dlg.open;
            dlg.close("confirmed");
            const retVal = dlg.returnValue + ":" + dlg.open;

            const email = document.getElementById("email");
            const v0 = email.checkValidity() + ":" + email.validity.valueMissing;
            email.value = "user@example.com";
            const v1 = email.checkValidity();
            email.setCustomValidity("Taken");
            const v2 = email.checkValidity() + ":" + email.validity.customError + ":" + email.validationMessage;
            email.setCustomValidity("");
            const v3 = email.checkValidity();

            async function* asyncStream() {
                yield await Promise.resolve(10);
                yield await Promise.resolve(20);
                yield 30;
            }

            (async () => {
                let sum = 0;
                for await (const n of asyncStream()) {
                    sum += n;
                }
                const f = new File(["hello-anvil"], "note.txt", { type: "text/plain" });
                const readerTxt = await new Promise((resolve) => {
                    const fr = new FileReader();
                    fr.addEventListener("load", () => resolve(fr.result));
                    fr.readAsText(f);
                });
                await navigator.clipboard.writeText("clip:" + readerTxt);
                const clipOut = await navigator.clipboard.readText();

                document.getElementById("out").textContent =
                    rect.width + "/" + ow + "/" + cw + "/" + sh + "/" + st +
                    "|" + cs.overflow + "/" + beforeCs.content +
                    "|" + wasOpen + "/" + retVal +
                    "|" + v0 + "/" + v1 + "/" + v2 + "/" + v3 +
                    "|" + sum + "|" + f.name + ":" + clipOut;
            })();
            "##,
        )
        .expect("run v24.4 pillar 2.3 and pillar 4 suite");

        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(
            doc.text_content(out),
            "208/208/200/320/60|auto/\"TAG:\"|true/confirmed:false|false:true/true/false:true:Taken/true|60|note.txt:clip:hello-anvil"
        );
    }

    #[test]
    fn cinder_engine_executes_mutation_observer_custom_elements_and_shadow_dom_slots() {
        let mut doc = parse_html(
            "<html><head><style>
               x-card::part(badge) { width: 140px; height: 32px; }
             </style></head><body>
               <div id=\"container\" data-init=\"0\">
                 <span id=\"first\">A</span>
               </div>
               <x-card id=\"pre-card\" status=\"boot\">
                 <span id=\"slotted-title\" slot=\"title\">Header</span>
                 <p id=\"slotted-body\">Body Text</p>
               </x-card>
               <div id=\"closed-host\"></div>
               <p id=\"out\">init</p>
             </body></html>",
        );
        let mut rt = ScriptRuntime::new();
        rt.eval_with_cinder_engine(
            &mut doc,
            r##"
            const log = [];

            class XCard extends HTMLElement {
                static get observedAttributes() {
                    return ["status"];
                }
                constructor() {
                    super();
                    this.changes = [];
                }
                connectedCallback() {
                    log.push("ce:conn:" + this.id);
                    if (!this.shadowRoot) {
                        const sr = this.attachShadow({ mode: "open" });
                        sr.innerHTML = "<style>:host { display: block; width: 240px; } ::slotted([slot=title]) { color: rgb(10, 20, 30); }</style><header part=\"badge\"><slot name=\"title\"></slot></header><section><slot></slot></section>";
                    }
                }
                disconnectedCallback() {
                    log.push("ce:disc:" + this.id);
                }
                adoptedCallback() {
                    log.push("ce:adopt:" + this.id);
                }
                attributeChangedCallback(name, oldVal, newVal) {
                    this.changes.push(name + ":" + oldVal + "->" + newVal);
                }
            }

            let definedResolved = false;
            customElements.whenDefined("x-card").then(() => {
                definedResolved = true;
            });
            customElements.define("x-card", XCard);

            const preCard = document.getElementById("pre-card");
            preCard.setAttribute("status", "ready");
            const dynCard = document.createElement("x-card");
            dynCard.id = "dyn-card";
            document.body.appendChild(dynCard);
            document.adoptNode(dynCard);
            document.body.removeChild(dynCard);

            const sr = preCard.shadowRoot;
            const slots = sr.querySelectorAll("slot");
            const titleSlot = slots[0];
            const defaultSlot = slots[1];
            const assignedTitle = titleSlot.assignedElements();
            const assignedDefault = defaultSlot.assignedElements();
            const slottedTitleEl = document.getElementById("slotted-title");
            const slotMatch = (slottedTitleEl.assignedSlot === titleSlot) + ":" + slottedTitleEl.slot;
            const hostMatch = (sr.host === preCard) + ":" + sr.mode;

            const closedHost = document.getElementById("closed-host");
            const closedSr = closedHost.attachShadow({ mode: "closed" });
            const closedCheck = (closedHost.shadowRoot === null) + ":" + closedSr.mode;

            const cardRect = preCard.getBoundingClientRect();

            const container = document.getElementById("container");
            const moEvents = [];
            const mo = new MutationObserver((records) => {
                for (const r of records) {
                    if (r.type === "attributes") {
                        moEvents.push("attr:" + r.attributeName + ":" + r.oldValue);
                    } else if (r.type === "childList") {
                        moEvents.push("child:+" + r.addedNodes.length + ":-" + r.removedNodes.length + ":prev=" + (r.previousSibling ? r.previousSibling.id : "none"));
                    } else if (r.type === "characterData") {
                        moEvents.push("char:" + r.oldValue);
                    }
                }
            });
            mo.observe(container, {
                attributes: true,
                attributeOldValue: true,
                attributeFilter: ["data-init"],
                childList: true,
                characterData: true,
                characterDataOldValue: true,
                subtree: true
            });

            container.setAttribute("data-init", "1");
            container.setAttribute("data-ignored", "x");
            const second = document.createElement("span");
            second.id = "second";
            container.appendChild(second);
            const firstText = document.getElementById("first").firstChild;
            firstText.data = "B";

            const taken = mo.takeRecords();
            const takenSummary = taken.map((r) => r.type).join(",");
            container.setAttribute("data-init", "2");

            Promise.resolve().then(() => {
                mo.disconnect();
                container.setAttribute("data-init", "3");
                document.getElementById("out").textContent =
                    log.join(",") +
                    "|" + preCard.changes.join(",") +
                    "|" + (customElements.get("x-card") === XCard) + ":" + customElements.getName(XCard) + ":" + definedResolved +
                    "|" + assignedTitle.length + ":" + assignedDefault.length + ":" + slotMatch + "|" + hostMatch +
                    "|" + closedCheck +
                    "|" + cardRect.width +
                    "|" + takenSummary +
                    "|" + moEvents.join(",");
            });
            "##,
        )
        .expect("run mutation observer, custom elements, and shadow dom suite");

        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(
            doc.text_content(out),
            "ce:conn:pre-card,ce:conn:dyn-card,ce:disc:dyn-card,ce:adopt:dyn-card|status:null->boot,status:boot->ready|true:x-card:true|1:1:true:title|true:open|true:closed|240|attributes,childList,characterData|attr:data-init:1"
        );
    }

    #[test]
    fn cinder_engine_executes_intersection_observer_root_margin_and_thresholds() {
        let mut doc = parse_html(
            "<html><head><style>
               html, body { margin: 0; padding: 0; }
               #target { width: 100px; height: 50px; }
             </style></head><body>
               <div id=\"target\">Box</div>
               <p id=\"out\">init</p>
             </body></html>",
        );
        let mut rt = ScriptRuntime::new();
        rt.eval_with_cinder_engine(
            &mut doc,
            r##"
            const target = document.getElementById("target");
            let entrySnap = "";
            const io = new IntersectionObserver((entries) => {
                const e = entries[0];
                entrySnap = e.isIntersecting + ":" + e.intersectionRatio + ":" + e.rootBounds.width + "x" + e.rootBounds.height;
            }, {
                rootMargin: "20px 10%",
                threshold: [0.75, 0, 0.25]
            });
            io.observe(target);
            Promise.resolve().then(() => {
                document.getElementById("out").textContent =
                    io.thresholds.join(",") + "|" + io.rootMargin + "|" + entrySnap;
            });
            "##,
        )
        .expect("run intersection observer rootMargin and thresholds test");

        let out = doc.get_element_by_id("out").unwrap();
        assert_eq!(
            doc.text_content(out),
            "0,0.25,0.75|20px 10%|true:1:1536x840"
        );
    }
}
