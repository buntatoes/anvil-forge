// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato
//! IPC bridge to the standalone Go `anvil-json` (`v.04`) engine binary
//! (`github.com/buntatoes/anvil-json/cmd/anvil-json --worker`).

use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};

/// Default maximum nesting depth for arrays and objects.
pub const MAX_DEPTH: usize = 64;

/// Default maximum input length in bytes (16 MiB).
pub const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;

pub use MAX_DEPTH as MAX_JSON_DEPTH;
pub use MAX_INPUT_BYTES as MAX_JSON_BYTES;

const OP_PARSE: u8 = 1;
const OP_STRINGIFY: u8 = 2;
const OP_STRINGIFY_PRETTY: u8 = 3;
const OP_ESCAPE_STRING: u8 = 4;
const OP_RESOLVE_POINTER: u8 = 5;

const WIRE_TAG_NULL: u8 = 0;
const WIRE_TAG_BOOL: u8 = 1;
const WIRE_TAG_NUMBER: u8 = 2;
const WIRE_TAG_STRING: u8 = 3;
const WIRE_TAG_ARRAY: u8 = 4;
const WIRE_TAG_OBJECT: u8 = 5;

static EMBEDDED_GO_BIN: &[u8] = include_bytes!(env!("ANVIL_JSON_GO_BIN"));

/// Parsed RFC 8259 JSON value produced or consumed by the Go `anvil-json` engine.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    pub fn empty_object() -> Self {
        Self::Object(Vec::new())
    }

    pub fn empty_array() -> Self {
        Self::Array(Vec::new())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        let n = self.as_f64()?;
        if n.is_finite() && n.fract() == 0.0 && n >= (i64::MIN as f64) && n < (i64::MAX as f64) {
            Some(n as i64)
        } else {
            None
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        let n = self.as_f64()?;
        if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n < (u64::MAX as f64) {
            Some(n as u64)
        } else {
            None
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            Self::Array(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<JsonValue>> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_members(&self) -> Option<&[(String, JsonValue)]> {
        match self {
            Self::Object(entries) => Some(entries.as_slice()),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, JsonValue)]> {
        self.as_members()
    }

    pub fn as_object_mut(&mut self) -> Option<&mut Vec<(String, JsonValue)>> {
        match self {
            Self::Object(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        let entries = self.as_members()?;
        entries.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut JsonValue> {
        let entries = self.as_object_mut()?;
        entries
            .iter_mut()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key)?.as_bool()
    }

    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.get(key)?.as_f64()
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.get(key)?.as_i64()
    }

    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.get(key)?.as_u64()
    }

    pub fn get_array(&self, key: &str) -> Option<&[JsonValue]> {
        self.get(key)?.as_array()
    }

    pub fn get_object(&self, key: &str) -> Option<&[(String, JsonValue)]> {
        self.get(key)?.as_object()
    }

    pub fn get_path(&self, path: &[&str]) -> Option<&JsonValue> {
        let mut cur = self;
        for segment in path {
            cur = cur.get(segment)?;
        }
        Some(cur)
    }

    pub fn index(&self, idx: usize) -> Option<&JsonValue> {
        self.as_array()?.get(idx)
    }

    pub fn get_index(&self, idx: usize) -> Option<&JsonValue> {
        self.index(idx)
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Array(items) => items.len(),
            Self::Object(entries) => entries.len(),
            _ => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn set(&mut self, key: impl Into<String>, value: JsonValue) -> bool {
        let Some(entries) = self.as_object_mut() else {
            return false;
        };
        let key = key.into();
        if let Some(first_idx) = entries.iter().position(|(k, _)| *k == key) {
            entries[first_idx].1 = value;
            let mut i = first_idx + 1;
            while i < entries.len() {
                if entries[i].0 == key {
                    entries.remove(i);
                } else {
                    i += 1;
                }
            }
        } else {
            entries.push((key, value));
        }
        true
    }

    pub fn insert(&mut self, key: impl Into<String>, value: JsonValue) -> bool {
        self.set(key, value)
    }

    pub fn push(&mut self, value: JsonValue) -> bool {
        if let Self::Array(items) = self {
            items.push(value);
            true
        } else {
            false
        }
    }

    pub fn to_json_string(&self) -> String {
        stringify(self)
    }

    pub fn to_json_pretty(&self, indent: usize) -> String {
        stringify_pretty(self, indent)
    }

    pub fn resolve_pointer(&self, pointer: &str) -> Result<Option<JsonValue>, ParseError> {
        resolve_pointer(self, pointer)
    }
}

/// Parse failure with byte offset returned by the Go `anvil-json` engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

pub type JsonError = ParseError;

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.message, self.offset)
    }
}

impl std::error::Error for ParseError {}

fn encode_binary_value(dst: &mut Vec<u8>, v: &JsonValue) {
    match v {
        JsonValue::Null => dst.push(WIRE_TAG_NULL),
        JsonValue::Bool(b) => {
            dst.push(WIRE_TAG_BOOL);
            dst.push(u8::from(*b));
        }
        JsonValue::Number(n) => {
            dst.push(WIRE_TAG_NUMBER);
            dst.extend_from_slice(&n.to_bits().to_le_bytes());
        }
        JsonValue::String(s) => {
            dst.push(WIRE_TAG_STRING);
            dst.extend_from_slice(&(s.len() as u32).to_le_bytes());
            dst.extend_from_slice(s.as_bytes());
        }
        JsonValue::Array(items) => {
            dst.push(WIRE_TAG_ARRAY);
            dst.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for elem in items {
                encode_binary_value(dst, elem);
            }
        }
        JsonValue::Object(members) => {
            dst.push(WIRE_TAG_OBJECT);
            dst.extend_from_slice(&(members.len() as u32).to_le_bytes());
            for (k, val) in members {
                dst.extend_from_slice(&(k.len() as u32).to_le_bytes());
                dst.extend_from_slice(k.as_bytes());
                encode_binary_value(dst, val);
            }
        }
    }
}

fn wire_err(offset: usize) -> ParseError {
    ParseError {
        message: "truncated binary JSON wire value".to_string(),
        offset,
    }
}

fn decode_binary_value(src: &[u8], pos: &mut usize) -> Result<JsonValue, ParseError> {
    let tag = *src.get(*pos).ok_or_else(|| wire_err(*pos))?;
    *pos += 1;
    match tag {
        WIRE_TAG_NULL => Ok(JsonValue::Null),
        WIRE_TAG_BOOL => {
            let b = *src.get(*pos).ok_or_else(|| wire_err(*pos))? != 0;
            *pos += 1;
            Ok(JsonValue::Bool(b))
        }
        WIRE_TAG_NUMBER => {
            let bytes = src.get(*pos..*pos + 8).ok_or_else(|| wire_err(*pos))?;
            *pos += 8;
            let bits = u64::from_le_bytes(bytes.try_into().unwrap());
            Ok(JsonValue::Number(f64::from_bits(bits)))
        }
        WIRE_TAG_STRING => {
            let len_bytes = src.get(*pos..*pos + 4).ok_or_else(|| wire_err(*pos))?;
            *pos += 4;
            let n = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
            let s_bytes = src.get(*pos..*pos + n).ok_or_else(|| wire_err(*pos))?;
            *pos += n;
            let s = String::from_utf8(s_bytes.to_vec()).map_err(|_| wire_err(*pos))?;
            Ok(JsonValue::String(s))
        }
        WIRE_TAG_ARRAY => {
            let len_bytes = src.get(*pos..*pos + 4).ok_or_else(|| wire_err(*pos))?;
            *pos += 4;
            let count = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
            let mut items = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                items.push(decode_binary_value(src, pos)?);
            }
            Ok(JsonValue::Array(items))
        }
        WIRE_TAG_OBJECT => {
            let len_bytes = src.get(*pos..*pos + 4).ok_or_else(|| wire_err(*pos))?;
            *pos += 4;
            let count = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
            let mut members = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                let klen_bytes = src.get(*pos..*pos + 4).ok_or_else(|| wire_err(*pos))?;
                *pos += 4;
                let klen = u32::from_le_bytes(klen_bytes.try_into().unwrap()) as usize;
                let k_bytes = src.get(*pos..*pos + klen).ok_or_else(|| wire_err(*pos))?;
                *pos += klen;
                let key = String::from_utf8(k_bytes.to_vec()).map_err(|_| wire_err(*pos))?;
                let val = decode_binary_value(src, pos)?;
                members.push((key, val));
            }
            Ok(JsonValue::Object(members))
        }
        _ => Err(wire_err(*pos)),
    }
}

struct GoJsonWorker {
    _child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

fn resolve_go_binary_path() -> PathBuf {
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let tmp_path =
        std::env::temp_dir().join(format!("anvil-json-go-v04-{}{}", std::process::id(), ext));
    if !tmp_path.exists() {
        let _ = std::fs::write(&tmp_path, EMBEDDED_GO_BIN);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755));
        }
    }
    tmp_path
}

fn spawn_worker() -> Result<GoJsonWorker, ParseError> {
    let bin = resolve_go_binary_path();
    let mut cmd = Command::new(bin);
    cmd.arg("--worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => {
            let mut retry = Command::new(resolve_go_binary_path());
            retry
                .arg("--worker")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                retry.creation_flags(CREATE_NO_WINDOW);
            }
            retry.spawn().map_err(|_| ParseError {
                message: "failed to spawn Go anvil-json worker".to_string(),
                offset: 0,
            })?
        }
    };
    let mut child = child;
    let stdin = child.stdin.take().ok_or_else(|| ParseError {
        message: "missing Go worker stdin".to_string(),
        offset: 0,
    })?;
    let stdout = child.stdout.take().ok_or_else(|| ParseError {
        message: "missing Go worker stdout".to_string(),
        offset: 0,
    })?;
    Ok(GoJsonWorker {
        _child: child,
        stdin,
        stdout,
    })
}

fn worker_slot() -> &'static Mutex<Option<GoJsonWorker>> {
    static WORKER: OnceLock<Mutex<Option<GoJsonWorker>>> = OnceLock::new();
    WORKER.get_or_init(|| Mutex::new(None))
}

/// Pre-spawns the persistent Go `anvil-json` worker process before sandbox lock-down.
pub fn warmup_worker() {
    if let Ok(mut guard) = worker_slot().lock()
        && guard.is_none()
    {
        *guard = spawn_worker().ok();
    }
}

pub fn parse(input: &str) -> Result<JsonValue, ParseError> {
    parse_with_limits(input, MAX_DEPTH, MAX_INPUT_BYTES)
}

pub fn parse_bytes(input: &[u8]) -> Result<JsonValue, ParseError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(ParseError {
            message: "input byte length exceeds cap".to_string(),
            offset: MAX_INPUT_BYTES,
        });
    }
    let text = std::str::from_utf8(input).map_err(|e| ParseError {
        message: "input is not valid UTF-8".to_string(),
        offset: e.valid_up_to(),
    })?;
    parse_with_limits(text, MAX_DEPTH, MAX_INPUT_BYTES)
}

pub fn parse_with_limits(
    input: &str,
    max_depth: usize,
    max_bytes: usize,
) -> Result<JsonValue, ParseError> {
    let bytes = input.as_bytes();
    if bytes.len() > max_bytes {
        return Err(ParseError {
            message: "input byte length exceeds cap".to_string(),
            offset: max_bytes,
        });
    }
    let mut guard = worker_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        *guard = Some(spawn_worker()?);
    }
    let w = guard.as_mut().unwrap();
    let mut hdr = [0u8; 13];
    hdr[0] = OP_PARSE;
    hdr[1..5].copy_from_slice(&(max_depth.min(u32::MAX as usize) as u32).to_le_bytes());
    hdr[5..9].copy_from_slice(&(max_bytes.min(u32::MAX as usize) as u32).to_le_bytes());
    hdr[9..13].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    if w.stdin.write_all(&hdr).is_err()
        || w.stdin.write_all(bytes).is_err()
        || w.stdin.flush().is_err()
    {
        *guard = None;
        return Err(ParseError {
            message: "Go anvil-json worker pipe closed".to_string(),
            offset: 0,
        });
    }
    let mut status = [0u8; 1];
    if w.stdout.read_exact(&mut status).is_err() {
        *guard = None;
        return Err(ParseError {
            message: "Go anvil-json worker read failed".to_string(),
            offset: 0,
        });
    }
    if status[0] == 0 {
        let mut len_buf = [0u8; 4];
        w.stdout.read_exact(&mut len_buf).map_err(|_| ParseError {
            message: "Go anvil-json worker short read".to_string(),
            offset: 0,
        })?;
        let n = u32::from_le_bytes(len_buf) as usize;
        let mut wire = vec![0u8; n];
        w.stdout.read_exact(&mut wire).map_err(|_| ParseError {
            message: "Go anvil-json worker payload read failed".to_string(),
            offset: 0,
        })?;
        let mut pos = 0;
        decode_binary_value(&wire, &mut pos)
    } else {
        let mut err_hdr = [0u8; 8];
        if w.stdout.read_exact(&mut err_hdr).is_err() {
            *guard = None;
            return Err(ParseError {
                message: "JSON parse error".to_string(),
                offset: 0,
            });
        }
        let offset = u32::from_le_bytes([err_hdr[0], err_hdr[1], err_hdr[2], err_hdr[3]]) as usize;
        let msg_len = u32::from_le_bytes([err_hdr[4], err_hdr[5], err_hdr[6], err_hdr[7]]) as usize;
        let mut msg_bytes = vec![0u8; msg_len];
        let _ = w.stdout.read_exact(&mut msg_bytes);
        Err(ParseError {
            message: String::from_utf8_lossy(&msg_bytes).into_owned(),
            offset,
        })
    }
}

pub fn stringify(value: &JsonValue) -> String {
    let mut wire = Vec::new();
    encode_binary_value(&mut wire, value);
    let mut guard = worker_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        *guard = spawn_worker().ok();
    }
    let Some(w) = guard.as_mut() else {
        return String::new();
    };
    let mut hdr = [0u8; 5];
    hdr[0] = OP_STRINGIFY;
    hdr[1..5].copy_from_slice(&(wire.len() as u32).to_le_bytes());
    if w.stdin.write_all(&hdr).is_err()
        || w.stdin.write_all(&wire).is_err()
        || w.stdin.flush().is_err()
    {
        *guard = None;
        return String::new();
    }
    read_worker_string(&mut w.stdout).unwrap_or_default()
}

pub fn stringify_pretty(value: &JsonValue, indent: usize) -> String {
    let mut wire = Vec::new();
    encode_binary_value(&mut wire, value);
    let mut guard = worker_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        *guard = spawn_worker().ok();
    }
    let Some(w) = guard.as_mut() else {
        return String::new();
    };
    let mut hdr = [0u8; 9];
    hdr[0] = OP_STRINGIFY_PRETTY;
    hdr[1..5].copy_from_slice(&(indent as u32).to_le_bytes());
    hdr[5..9].copy_from_slice(&(wire.len() as u32).to_le_bytes());
    if w.stdin.write_all(&hdr).is_err()
        || w.stdin.write_all(&wire).is_err()
        || w.stdin.flush().is_err()
    {
        *guard = None;
        return String::new();
    }
    read_worker_string(&mut w.stdout).unwrap_or_default()
}

pub fn escape_string(s: &str) -> String {
    let mut guard = worker_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        *guard = spawn_worker().ok();
    }
    let Some(w) = guard.as_mut() else {
        return String::new();
    };
    let bytes = s.as_bytes();
    let mut hdr = [0u8; 5];
    hdr[0] = OP_ESCAPE_STRING;
    hdr[1..5].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    if w.stdin.write_all(&hdr).is_err()
        || w.stdin.write_all(bytes).is_err()
        || w.stdin.flush().is_err()
    {
        *guard = None;
        return String::new();
    }
    read_worker_string(&mut w.stdout).unwrap_or_default()
}

fn read_worker_string(stdout: &mut ChildStdout) -> Option<String> {
    let mut status = [0u8; 1];
    stdout.read_exact(&mut status).ok()?;
    let mut len_buf = [0u8; 4];
    stdout.read_exact(&mut len_buf).ok()?;
    let n = u32::from_le_bytes(len_buf) as usize;
    let mut out = vec![0u8; n];
    stdout.read_exact(&mut out).ok()?;
    if status[0] == 0 {
        String::from_utf8(out).ok()
    } else {
        None
    }
}

pub use escape_string as escape_json_string;
pub use parse as parse_json;
pub use parse_bytes as parse_json_bytes;
pub use stringify as to_json_string;

pub fn parse_json_bounded(
    input: &str,
    max_depth: usize,
    max_bytes: usize,
) -> Result<JsonValue, ParseError> {
    parse_with_limits(input, max_depth, max_bytes)
}

pub fn to_json_pretty(value: &JsonValue) -> String {
    stringify_pretty(value, 2)
}

/// Evaluate an RFC 6901 JSON Pointer (`""` or `"/a/b/0"`) against `value`
/// using the Go `anvil-json` (`v.04`) worker.
pub fn resolve_pointer(value: &JsonValue, pointer: &str) -> Result<Option<JsonValue>, ParseError> {
    let mut wire = Vec::new();
    encode_binary_value(&mut wire, value);
    let mut guard = worker_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        *guard = Some(spawn_worker()?);
    }
    let w = guard.as_mut().unwrap();
    let ptr_bytes = pointer.as_bytes();
    let mut hdr = [0u8; 9];
    hdr[0] = OP_RESOLVE_POINTER;
    hdr[1..5].copy_from_slice(&(ptr_bytes.len() as u32).to_le_bytes());
    hdr[5..9].copy_from_slice(&(wire.len() as u32).to_le_bytes());
    if w.stdin.write_all(&hdr).is_err()
        || w.stdin.write_all(ptr_bytes).is_err()
        || w.stdin.write_all(&wire).is_err()
        || w.stdin.flush().is_err()
    {
        *guard = None;
        return Err(ParseError {
            message: "Go anvil-json worker pipe closed".to_string(),
            offset: 0,
        });
    }
    let mut status = [0u8; 1];
    if w.stdout.read_exact(&mut status).is_err() {
        *guard = None;
        return Err(ParseError {
            message: "Go anvil-json worker read failed".to_string(),
            offset: 0,
        });
    }
    match status[0] {
        0 => {
            let mut len_buf = [0u8; 4];
            w.stdout.read_exact(&mut len_buf).map_err(|_| wire_err(0))?;
            let n = u32::from_le_bytes(len_buf) as usize;
            let mut out = vec![0u8; n];
            w.stdout.read_exact(&mut out).map_err(|_| wire_err(0))?;
            let mut pos = 0;
            Ok(Some(decode_binary_value(&out, &mut pos)?))
        }
        2 => {
            let mut len_buf = [0u8; 4];
            let _ = w.stdout.read_exact(&mut len_buf);
            Ok(None)
        }
        _ => {
            let mut len_buf = [0u8; 4];
            let _ = w.stdout.read_exact(&mut len_buf);
            let n = u32::from_le_bytes(len_buf) as usize;
            let mut msg_bytes = vec![0u8; n];
            let _ = w.stdout.read_exact(&mut msg_bytes);
            Err(ParseError {
                message: String::from_utf8_lossy(&msg_bytes).into_owned(),
                offset: 0,
            })
        }
    }
}
