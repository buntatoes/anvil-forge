// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato
//! Live DOM, `Storage`, `Location`, `Event`, and `console` host implementation
//! ([`CinderDomHost`]) for the `cinder` bytecode virtual machine ([`cinder::vm::Vm`]).

use std::any::Any;
use std::collections::VecDeque;

use crate::cinder;
use anvil_dom::{Document, NodeData, NodeId, parse_html};
use cinder::gc::roots::Persistent;
use cinder::value::object::{HostData, ObjectClass};
use cinder::value::{ObjectRef, Value as JsValue};
use cinder::vm::{Host, Job, NativeCx, NativeFn, RunError, Vm};

use crate::{
    camel_to_kebab, kebab_to_camel, read_inline_style, remove_inline_style_prop,
    update_inline_style,
};

const TAG_MASK: u64 = 0xF000_0000_0000_0000;
const TAG_NODE: u64 = 0x1000_0000_0000_0000;
const TAG_CLASS_LIST: u64 = 0x2000_0000_0000_0000;
const TAG_STYLE: u64 = 0x3000_0000_0000_0000;
const TAG_DATASET: u64 = 0x4000_0000_0000_0000;

/// Live DOM, `localStorage`, `sessionStorage`, `location`, event listener, and
/// microtask host for [`cinder::vm::Vm`].
#[derive(Debug)]
pub struct CinderDomHost {
    /// Live HTML document mutated directly by JavaScript DOM operations.
    pub doc: Document,
    /// FIFO microtask queue for `Promise` reactions and `queueMicrotask`.
    pub jobs: VecDeque<Job>,
    /// Formatted `console.log` / `info` / `warn` / `error` output lines.
    pub console: Vec<String>,
    /// Active origin's `localStorage` entries.
    pub local_storage: Vec<(String, String)>,
    /// Active origin's `sessionStorage` entries.
    pub session_storage: Vec<(String, String)>,
    /// Current page URL (`window.location.href` / `document.URL`).
    pub location_href: String,
    /// Pending navigation URL requested by `location.assign` / `replace` / `href = ...`.
    pub pending_navigation: Option<String>,
    /// True when the active event handler called `event.preventDefault()`.
    pub event_default_prevented: bool,
    /// True when the active event handler called `event.stopPropagation()`.
    pub event_propagation_stopped: bool,
    /// Monotonic clock in milliseconds for `Date.now()` and `performance.now()`.
    pub clock_ms: f64,
    /// Prototype for wrapped DOM `Element` / `Node` instances.
    pub element_proto: Option<ObjectRef>,
    /// Prototype for `element.classList` `DOMTokenList` wrappers.
    pub class_list_proto: Option<ObjectRef>,
    /// Prototype for `element.style` `CSSStyleDeclaration` wrappers.
    pub style_proto: Option<ObjectRef>,
    /// Prototype for `element.dataset` `DOMStringMap` wrappers.
    pub dataset_proto: Option<ObjectRef>,
    /// Global `document` object.
    pub document_obj: Option<ObjectRef>,
    /// Registered live DOM event listeners: `(target_node, event_type_lowercase, callback)`.
    pub listeners: Vec<(NodeId, String, Persistent)>,
    /// Scheduled virtual timers: `(timer_id, due_ms, interval_ms, callback, is_raf)`.
    pub timers: Vec<(u32, f64, Option<f64>, Persistent, bool)>,
    /// Next monotonically increasing timer handle id.
    pub next_timer_id: u32,
    /// Active 2D `<canvas>` subpaths per canvas `NodeId`.
    pub canvas_paths: std::collections::HashMap<NodeId, Vec<(i32, i32)>>,
    /// Active `MutationObserver` registrations: `(callback, observed_node)`.
    pub mutation_observers: Vec<(Persistent, NodeId)>,
    /// True when one or more custom elements have been defined via `customElements.define`.
    pub has_custom_elements: bool,
    /// Cached stylesheet, resolved `StyleMap`, and `Animator` for incremental style invalidation.
    style_cache: Option<(
        String,
        u32,
        u32,
        anvil_style::Stylesheet,
        anvil_style::StyleMap,
        anvil_style::Animator,
    )>,
    /// Registered mock responses for `fetch(url)`: `(url, status, body)`.
    pub fetch_mocks: Vec<(String, u16, String)>,
    /// Active layout viewport width in CSS pixels (`window.innerWidth`).
    pub viewport_width: f32,
    /// Active layout viewport height in CSS pixels (`window.innerHeight`).
    pub viewport_height: f32,
}

impl Default for CinderDomHost {
    fn default() -> Self {
        Self::new(Document::new(), "https://localhost/")
    }
}

impl CinderDomHost {
    /// Create a new [`CinderDomHost`] owning `doc` and `location_href`.
    pub fn new(doc: Document, location_href: &str) -> Self {
        Self {
            doc,
            jobs: VecDeque::new(),
            console: Vec::new(),
            local_storage: Vec::new(),
            session_storage: Vec::new(),
            location_href: if location_href.trim().is_empty() {
                "https://localhost/".to_string()
            } else {
                location_href.trim().to_string()
            },
            pending_navigation: None,
            event_default_prevented: false,
            event_propagation_stopped: false,
            clock_ms: 1_700_000_000_000.0,
            element_proto: None,
            class_list_proto: None,
            style_proto: None,
            dataset_proto: None,
            document_obj: None,
            listeners: Vec::new(),
            timers: Vec::new(),
            next_timer_id: 1,
            canvas_paths: std::collections::HashMap::new(),
            mutation_observers: Vec::new(),
            has_custom_elements: false,
            style_cache: None,
            fetch_mocks: Vec::new(),
            viewport_width: 1280.0,
            viewport_height: 800.0,
        }
    }

    /// Collect document `<style>` rules and `adoptedStyleSheets` and cascade styles
    /// (incrementally re-evaluating only dirty subtrees when the stylesheet and viewport are unchanged).
    pub fn compute_styles(&mut self) -> anvil_style::StyleMap {
        let css_src = self.doc.embedded_css();
        let vw_bits = self.viewport_width.to_bits();
        let vh_bits = self.viewport_height.to_bits();
        if let Some((cached_css, cached_vw, cached_vh, sheet, styles, animator)) =
            self.style_cache.as_mut()
            && *cached_css == css_src
            && *cached_vw == vw_bits
            && *cached_vh == vh_bits
        {
            let _ = anvil_style::compute_styles_incremental(
                &mut self.doc,
                sheet,
                styles,
                self.viewport_width,
                self.viewport_height,
                Some(animator),
            );
            return styles.clone();
        }
        self.doc.clear_dirty_styles();
        let sheet = anvil_style::parse_css(&css_src);
        let mut animator = anvil_style::Animator::new();
        let styles = anvil_style::compute_styles_animated(
            &self.doc,
            &sheet,
            self.viewport_width,
            self.viewport_height,
            &mut animator,
        );
        self.style_cache = Some((css_src, vw_bits, vh_bits, sheet, styles.clone(), animator));
        styles
    }

    /// Resolve styles and compute layout for `self.doc`, returning the [`anvil_layout::LayoutBox`]
    /// for `nid` if it participates in layout.
    pub fn compute_node_box(&mut self, nid: NodeId) -> Option<anvil_layout::LayoutBox> {
        let styles = self.compute_styles();
        let layout = anvil_layout::layout_document(&self.doc, &styles, self.viewport_width);
        layout.find_node_box(nid).cloned()
    }

    /// Install all Web/DOM globals (`window`, `document`, `Element.prototype`,
    /// `localStorage`, `sessionStorage`, `location`, `navigator`, `customElements`,
    /// `DOMParser`, `CSSStyleSheet`, `matchMedia`, `getComputedStyle`) onto `vm`.
    pub fn install(&mut self, vm: &mut Vm) -> Result<(), RunError> {
        let g = vm.global();

        // 1. Element.prototype
        let el_proto = vm.new_plain_object()?;
        for (name, func) in [
            ("getAttribute", el_get_attribute as NativeFn),
            ("setAttribute", el_set_attribute),
            ("removeAttribute", el_remove_attribute),
            ("hasAttribute", el_has_attribute),
            ("getAttributeNames", el_get_attribute_names),
            ("hasAttributes", el_has_attributes),
            ("appendChild", el_append_child),
            ("removeChild", el_remove_child),
            ("insertBefore", el_insert_before),
            ("replaceChild", el_replace_child),
            ("cloneNode", el_clone_node),
            ("remove", el_remove),
            ("querySelector", el_query_selector),
            ("querySelectorAll", el_query_selector_all),
            ("getElementById", el_get_element_by_id),
            ("getElementsByTagName", el_get_elements_by_tag_name),
            ("getElementsByClassName", el_get_elements_by_class_name),
            ("insertAdjacentHTML", el_insert_adjacent_html),
            ("insertAdjacentText", el_insert_adjacent_text),
            ("insertAdjacentElement", el_insert_adjacent_element),
            ("matches", el_matches),
            ("closest", el_closest),
            ("contains", el_contains),
            ("attachShadow", el_attach_shadow),
            ("addEventListener", el_add_event_listener),
            ("removeEventListener", el_remove_event_listener),
            ("dispatchEvent", el_dispatch_event),
            ("getBoundingClientRect", el_get_bounding_client_rect),
            ("getContext", el_get_context),
            ("setTextContent", el_set_text_content_method),
            ("getTextContent", el_get_text_content_method),
            ("setInnerHTML", el_set_inner_html_method),
            ("setValue", el_set_value_method),
            ("focus", el_focus),
            ("blur", el_blur),
            ("scrollIntoView", el_noop),
            ("scrollTo", el_scroll_to),
            ("scrollBy", el_scroll_by),
            ("show", el_dialog_show),
            ("showModal", el_dialog_show_modal),
            ("close", el_dialog_close),
            ("checkValidity", el_check_validity),
            ("reportValidity", el_report_validity),
            ("setCustomValidity", el_set_custom_validity),
            ("assignedNodes", el_assigned_nodes),
            ("assignedElements", el_assigned_elements),
        ] {
            vm.define_method(el_proto, name, func)?;
        }

        for (prop, getter, setter) in [
            (
                "id",
                Some(el_get_id as NativeFn),
                Some(el_set_id as NativeFn),
            ),
            (
                "className",
                Some(el_get_class_name),
                Some(el_set_class_name),
            ),
            (
                "textContent",
                Some(el_get_text_content),
                Some(el_set_text_content),
            ),
            (
                "innerText",
                Some(el_get_text_content),
                Some(el_set_text_content),
            ),
            (
                "innerHTML",
                Some(el_get_inner_html),
                Some(el_set_inner_html),
            ),
            ("outerHTML", Some(el_get_outer_html), None),
            ("value", Some(el_get_value), Some(el_set_value)),
            ("checked", Some(el_get_checked), Some(el_set_checked)),
            ("tagName", Some(el_get_tag_name), None),
            ("nodeName", Some(el_get_node_name), None),
            ("nodeType", Some(el_get_node_type), None),
            (
                "nodeValue",
                Some(el_get_node_value),
                Some(el_set_node_value),
            ),
            ("data", Some(el_get_node_value), Some(el_set_node_value)),
            ("ownerDocument", Some(el_get_owner_document), None),
            ("isConnected", Some(el_get_is_connected), None),
            ("parentElement", Some(el_get_parent_element), None),
            ("parentNode", Some(el_get_parent_node), None),
            ("firstElementChild", Some(el_get_first_element_child), None),
            ("lastElementChild", Some(el_get_last_element_child), None),
            ("firstChild", Some(el_get_first_child), None),
            ("lastChild", Some(el_get_last_child), None),
            ("childNodes", Some(el_get_child_nodes), None),
            ("nextSibling", Some(el_get_next_sibling), None),
            ("previousSibling", Some(el_get_previous_sibling), None),
            (
                "nextElementSibling",
                Some(el_get_next_element_sibling),
                None,
            ),
            (
                "previousElementSibling",
                Some(el_get_previous_element_sibling),
                None,
            ),
            ("children", Some(el_get_children), None),
            ("childElementCount", Some(el_get_child_element_count), None),
            ("shadowRoot", Some(el_get_shadow_root), None),
            ("host", Some(el_get_shadow_host), None),
            ("mode", Some(el_get_shadow_mode), None),
            ("slot", Some(el_get_slot), Some(el_set_slot)),
            ("assignedSlot", Some(el_get_assigned_slot), None),
            ("classList", Some(el_get_class_list), None),
            ("style", Some(el_get_style), None),
            ("dataset", Some(el_get_dataset), None),
            ("offsetWidth", Some(el_get_offset_width), None),
            ("offsetHeight", Some(el_get_offset_height), None),
            ("clientWidth", Some(el_get_client_width), None),
            ("clientHeight", Some(el_get_client_height), None),
            ("scrollWidth", Some(el_get_scroll_width), None),
            ("scrollHeight", Some(el_get_scroll_height), None),
            (
                "scrollTop",
                Some(el_get_scroll_top),
                Some(el_set_scroll_top),
            ),
            (
                "scrollLeft",
                Some(el_get_scroll_left),
                Some(el_set_scroll_left),
            ),
            ("open", Some(el_get_dialog_open), Some(el_set_dialog_open)),
            (
                "returnValue",
                Some(el_get_dialog_return_value),
                Some(el_set_dialog_return_value),
            ),
            ("validity", Some(el_get_validity), None),
            ("validationMessage", Some(el_get_validation_message), None),
            ("willValidate", Some(el_get_will_validate), None),
        ] {
            vm.define_accessor(el_proto, prop, getter, setter)?;
        }
        self.element_proto = Some(el_proto);

        // 2. DOMTokenList (element.classList) prototype
        let cl_proto = vm.new_plain_object()?;
        for (name, func) in [
            ("add", cl_add as NativeFn),
            ("remove", cl_remove),
            ("toggle", cl_toggle),
            ("contains", cl_contains),
            ("toString", cl_to_string),
        ] {
            vm.define_method(cl_proto, name, func)?;
        }
        vm.define_accessor(cl_proto, "value", Some(cl_to_string), None)?;
        self.class_list_proto = Some(cl_proto);

        // 3. CSSStyleDeclaration (element.style) prototype
        let st_proto = vm.new_plain_object()?;
        for (name, func) in [
            ("setProperty", style_set_property as NativeFn),
            ("getPropertyValue", style_get_property_value),
            ("removeProperty", style_remove_property),
        ] {
            vm.define_method(st_proto, name, func)?;
        }
        vm.define_accessor(
            st_proto,
            "cssText",
            Some(style_get_css_text),
            Some(style_set_css_text),
        )?;
        for prop in [
            "color",
            "backgroundColor",
            "background",
            "display",
            "width",
            "height",
            "minWidth",
            "minHeight",
            "maxWidth",
            "maxHeight",
            "opacity",
            "transform",
            "fontSize",
            "fontWeight",
            "margin",
            "marginTop",
            "marginBottom",
            "marginLeft",
            "marginRight",
            "padding",
            "paddingTop",
            "paddingBottom",
            "paddingLeft",
            "paddingRight",
            "border",
            "borderRadius",
            "visibility",
            "position",
            "top",
            "left",
            "right",
            "bottom",
            "zIndex",
            "overflow",
            "cursor",
            "textAlign",
            "lineHeight",
            "gap",
            "flexDirection",
            "alignItems",
            "justifyContent",
        ] {
            self.install_style_accessor(vm, st_proto, prop)?;
        }
        self.style_proto = Some(st_proto);

        // 4. DOMStringMap (element.dataset) prototype
        let ds_proto = vm.new_plain_object()?;
        vm.define_method(ds_proto, "get", dataset_get)?;
        vm.define_method(ds_proto, "set", dataset_set)?;
        for key in [
            "id",
            "index",
            "value",
            "state",
            "mode",
            "tab",
            "section",
            "url",
            "action",
            "target",
            "type",
            "name",
            "role",
            "theme",
            "status",
            "count",
            "active",
            "open",
            "loaded",
            "recovered",
            "threadId",
            "testid",
        ] {
            self.install_dataset_accessor(vm, ds_proto, key)?;
        }
        self.dataset_proto = Some(ds_proto);

        // 5. document object
        let doc_obj = vm.new_plain_object()?;
        for (name, func) in [
            ("getElementById", doc_get_element_by_id as NativeFn),
            ("querySelector", doc_query_selector),
            ("querySelectorAll", doc_query_selector_all),
            ("createElement", doc_create_element),
            ("createTextNode", doc_create_text_node),
            ("createComment", doc_create_text_node),
            ("createDocumentFragment", doc_create_document_fragment),
            ("getElementsByTagName", doc_get_elements_by_tag_name),
            ("getElementsByClassName", doc_get_elements_by_class_name),
            ("getElementsByName", doc_get_elements_by_name),
            ("addEventListener", doc_add_event_listener),
            ("removeEventListener", doc_remove_event_listener),
            ("dispatchEvent", doc_dispatch_event),
            ("adoptNode", doc_adopt_node),
            ("importNode", doc_import_node),
            ("__parseHTML", doc_parse_html_fragment),
            ("__addAdoptedSheet", doc_add_adopted_sheet),
            ("__setAdoptedSheets", doc_set_adopted_sheets),
        ] {
            vm.define_method(doc_obj, name, func)?;
        }
        vm.define_accessor(doc_obj, "title", Some(doc_get_title), Some(doc_set_title))?;
        vm.define_accessor(doc_obj, "body", Some(doc_get_body), None)?;
        vm.define_accessor(doc_obj, "head", Some(doc_get_head), None)?;
        vm.define_accessor(
            doc_obj,
            "documentElement",
            Some(doc_get_document_element),
            None,
        )?;
        vm.define_accessor(doc_obj, "activeElement", Some(doc_get_active_element), None)?;
        vm.define_accessor(doc_obj, "readyState", Some(doc_get_ready_state), None)?;
        vm.define_accessor(doc_obj, "URL", Some(doc_get_url), None)?;
        vm.define_accessor(doc_obj, "documentURI", Some(doc_get_url), None)?;
        vm.define_accessor(doc_obj, "nodeType", Some(doc_get_node_type), None)?;
        vm.define_accessor(doc_obj, "nodeName", Some(doc_get_node_name), None)?;
        vm.define_accessor(doc_obj, "childNodes", Some(doc_get_child_nodes), None)?;
        vm.define_accessor(doc_obj, "firstChild", Some(doc_get_document_element), None)?;
        vm.define_accessor(doc_obj, "lastChild", Some(doc_get_document_element), None)?;
        vm.define_accessor(doc_obj, "isConnected", Some(doc_get_is_connected), None)?;
        let empty_sheets = vm.new_array_from(&[])?;
        vm.define_data(doc_obj, "_adoptedSheets", empty_sheets)?;
        self.document_obj = Some(doc_obj);

        // 6. localStorage & sessionStorage
        let local_obj = vm.new_plain_object()?;
        for (name, func) in [
            ("getItem", local_storage_get_item as NativeFn),
            ("setItem", local_storage_set_item),
            ("removeItem", local_storage_remove_item),
            ("clear", local_storage_clear),
            ("key", local_storage_key),
        ] {
            vm.define_method(local_obj, name, func)?;
        }
        vm.define_accessor(local_obj, "length", Some(local_storage_length), None)?;

        let session_obj = vm.new_plain_object()?;
        for (name, func) in [
            ("getItem", session_storage_get_item as NativeFn),
            ("setItem", session_storage_set_item),
            ("removeItem", session_storage_remove_item),
            ("clear", session_storage_clear),
            ("key", session_storage_key),
        ] {
            vm.define_method(session_obj, name, func)?;
        }
        vm.define_accessor(session_obj, "length", Some(session_storage_length), None)?;

        // 7. location, history, navigator, customElements
        let loc_obj = vm.new_plain_object()?;
        vm.define_accessor(loc_obj, "href", Some(loc_get_href), Some(loc_set_href))?;
        vm.define_accessor(loc_obj, "origin", Some(loc_get_origin), None)?;
        vm.define_accessor(loc_obj, "pathname", Some(loc_get_pathname), None)?;
        vm.define_accessor(loc_obj, "search", Some(loc_get_search), None)?;
        vm.define_accessor(loc_obj, "hash", Some(loc_get_hash), None)?;
        vm.define_method(loc_obj, "assign", loc_assign)?;
        vm.define_method(loc_obj, "replace", loc_assign)?;
        vm.define_method(loc_obj, "toString", loc_get_href)?;

        let hist_obj = vm.new_plain_object()?;
        vm.define_method(hist_obj, "pushState", hist_push_state)?;
        vm.define_method(hist_obj, "replaceState", hist_push_state)?;

        let nav_obj = vm.new_plain_object()?;
        let ua = vm.string("AnvilBrowser/0.24")?;
        let lang = vm.string("en-US")?;
        vm.define_data(nav_obj, "userAgent", ua)?;
        vm.define_data(nav_obj, "language", lang)?;

        let ce_obj = vm.new_plain_object()?;
        vm.define_method(ce_obj, "define", custom_elements_define)?;
        vm.define_method(ce_obj, "get", custom_elements_get)?;

        // 8. Global bindings
        vm.define_data(g, "window", JsValue::object(g))?;
        vm.define_data(g, "self", JsValue::object(g))?;
        vm.define_data(g, "__elementProto", JsValue::object(el_proto))?;
        vm.define_data(g, "document", JsValue::object(doc_obj))?;
        vm.define_data(g, "localStorage", JsValue::object(local_obj))?;
        vm.define_data(g, "sessionStorage", JsValue::object(session_obj))?;
        vm.define_data(g, "location", JsValue::object(loc_obj))?;
        vm.define_data(g, "history", JsValue::object(hist_obj))?;
        vm.define_data(g, "navigator", JsValue::object(nav_obj))?;
        vm.define_data(g, "customElements", JsValue::object(ce_obj))?;
        for (name, func) in [
            ("getComputedStyle", window_get_computed_style as NativeFn),
            ("matchMedia", window_match_media),
            ("addEventListener", doc_add_event_listener),
            ("removeEventListener", doc_remove_event_listener),
            ("dispatchEvent", doc_dispatch_event),
            ("setTimeout", window_set_timeout),
            ("clearTimeout", window_clear_timer),
            ("setInterval", window_set_interval),
            ("clearInterval", window_clear_timer),
            ("requestAnimationFrame", window_request_animation_frame),
            ("cancelAnimationFrame", window_clear_timer),
            ("__nativePreventDefault", ev_prevent_default),
            ("__nativeStopPropagation", ev_stop_propagation),
            ("__nativeFetch", window_native_fetch),
            ("__sha256Bytes", window_sha256_bytes),
            (
                "__registerMutationObserver",
                window_register_mutation_observer,
            ),
            (
                "__unregisterMutationObserver",
                window_unregister_mutation_observer,
            ),
        ] {
            vm.define_method(g, name, func)?;
        }

        self.install_web_api_classes(vm)?;

        Ok(())
    }

    fn install_web_api_classes(&mut self, vm: &mut Vm) -> Result<(), RunError> {
        let bootstrap = cinder::DOM_WEB_BOOTSTRAP_JS;
        let limits = vm.limits().clone();
        let code = cinder::compile_script(bootstrap, &limits)
            .map_err(|e| RunError::internal(e.to_string()))?;
        let id = vm.load(code)?;
        vm.run(self, id)?;
        vm.drain_jobs(self)?;
        Ok(())
    }

    /// Execute due virtual timers (`setTimeout`, `setInterval`, `requestAnimationFrame`)
    /// and drain microtasks after each callback.
    pub fn run_timers(&mut self, vm: &mut Vm) -> Result<bool, RunError> {
        let mut ran = false;
        let mut fired_intervals = std::collections::HashSet::new();
        for _ in 0..64 {
            let mut next_idx: Option<usize> = None;
            for (i, (id, due, iv, _, _)) in self.timers.iter().enumerate() {
                if iv.is_some() && fired_intervals.contains(id) {
                    continue;
                }
                if next_idx.is_none_or(|best| *due < self.timers[best].1) {
                    next_idx = Some(i);
                }
            }
            let Some(idx) = next_idx else {
                break;
            };
            let (id, due_ms, interval_ms, cb_pers, is_raf) = self.timers[idx];
            if due_ms > self.clock_ms + 60_000.0 {
                break;
            }
            self.clock_ms = self.clock_ms.max(due_ms);
            if let Some(iv) = interval_ms {
                self.timers[idx].1 = self.clock_ms + iv;
                fired_intervals.insert(id);
            } else {
                self.timers.remove(idx);
            }
            let cb_val = vm
                .heap()
                .persistent(cb_pers)
                .map_err(|e| RunError::internal(format!("{e:?}")))?;
            let args = if is_raf {
                vec![JsValue::number(self.clock_ms)]
            } else {
                Vec::new()
            };
            let _ = vm.call(self, cb_val, JsValue::UNDEFINED, &args)?;
            vm.drain_jobs(self)?;
            ran = true;
        }
        Ok(ran)
    }

    fn install_style_accessor(
        &self,
        vm: &mut Vm,
        st_proto: ObjectRef,
        prop: &str,
    ) -> Result<(), RunError> {
        let css_name = camel_to_kebab(prop);
        let getter_src = format!(
            "(function(prop) {{ return function() {{ return this.getPropertyValue(prop); }}; }})({:?})",
            css_name
        );
        let setter_src = format!(
            "(function(prop) {{ return function(val) {{ this.setProperty(prop, val); }}; }})({:?})",
            css_name
        );
        let mut dummy_host = cinder::vm::BasicHost::default();
        let limits = vm.limits().clone();
        let get_code = cinder::compile_script(&getter_src, &limits)
            .map_err(|e| RunError::internal(e.to_string()))?;
        let get_id = vm.load(get_code)?;
        let get_fn = vm.run(&mut dummy_host, get_id)?;

        let set_code = cinder::compile_script(&setter_src, &limits)
            .map_err(|e| RunError::internal(e.to_string()))?;
        let set_id = vm.load(set_code)?;
        let set_fn = vm.run(&mut dummy_host, set_id)?;

        let key = vm
            .heap_mut()
            .key_from_str(prop)
            .map_err(|e| RunError::internal(format!("{e:?}")))?;
        vm.heap_mut()
            .define_own(
                st_proto,
                key,
                cinder::value::object::Property {
                    attrs: cinder::value::shape::Attrs(
                        cinder::value::shape::Attrs::ENUMERABLE
                            | cinder::value::shape::Attrs::CONFIGURABLE,
                    ),
                    slot: cinder::value::object::Slot::Accessor {
                        get: get_fn,
                        set: set_fn,
                    },
                },
            )
            .map_err(|e| RunError::internal(format!("{e:?}")))?;
        Ok(())
    }

    fn install_dataset_accessor(
        &self,
        vm: &mut Vm,
        ds_proto: ObjectRef,
        key_name: &str,
    ) -> Result<(), RunError> {
        let getter_src = format!(
            "(function(k) {{ return function() {{ return this.get(k); }}; }})({:?})",
            key_name
        );
        let setter_src = format!(
            "(function(k) {{ return function(v) {{ this.set(k, v); }}; }})({:?})",
            key_name
        );
        let mut dummy_host = cinder::vm::BasicHost::default();
        let limits = vm.limits().clone();
        let get_code = cinder::compile_script(&getter_src, &limits)
            .map_err(|e| RunError::internal(e.to_string()))?;
        let get_id = vm.load(get_code)?;
        let get_fn = vm.run(&mut dummy_host, get_id)?;

        let set_code = cinder::compile_script(&setter_src, &limits)
            .map_err(|e| RunError::internal(e.to_string()))?;
        let set_id = vm.load(set_code)?;
        let set_fn = vm.run(&mut dummy_host, set_id)?;

        let key = vm
            .heap_mut()
            .key_from_str(key_name)
            .map_err(|e| RunError::internal(format!("{e:?}")))?;
        vm.heap_mut()
            .define_own(
                ds_proto,
                key,
                cinder::value::object::Property {
                    attrs: cinder::value::shape::Attrs(
                        cinder::value::shape::Attrs::ENUMERABLE
                            | cinder::value::shape::Attrs::CONFIGURABLE,
                    ),
                    slot: cinder::value::object::Slot::Accessor {
                        get: get_fn,
                        set: set_fn,
                    },
                },
            )
            .map_err(|e| RunError::internal(format!("{e:?}")))?;
        Ok(())
    }

    /// Dispatch a live DOM event (`click`, `input`, `change`, `submit`, `keydown`)
    /// along the target's ancestor chain in `self.doc`, invoking all matching
    /// JS callbacks registered via `addEventListener` and draining microtasks.
    pub fn dispatch_event(
        &mut self,
        vm: &mut Vm,
        target: NodeId,
        event_type: &str,
        value: Option<&str>,
        key: Option<&str>,
    ) -> Result<bool, RunError> {
        self.event_default_prevented = false;
        self.event_propagation_stopped = false;
        let wanted = event_type.trim().to_ascii_lowercase();
        let mut chain = Vec::new();
        let mut cursor = Some(target);
        while let Some(id) = cursor {
            chain.push(id);
            cursor = self.doc.get(id).parent();
        }
        if !chain.contains(&self.doc.body()) {
            chain.push(self.doc.body());
        }
        if !chain.contains(&self.doc.root()) {
            chain.push(self.doc.root());
        }

        let target_val = self.wrap_node_in_vm(vm, target)?;
        let g = vm.global();
        if let Ok(fn_key) = vm.heap_mut().key_from_str("__dispatchNativeDomEvent")
            && let Ok(Some(prop)) = vm.heap().get_own(g, fn_key)
            && let Some(fn_val) = prop.slot.data()
            && fn_val.as_object().is_some()
        {
            let ev_js = vm.string(&wanted)?;
            let val_js = match value {
                Some(v) => vm.string(v)?,
                None => JsValue::UNDEFINED,
            };
            let key_js = match key {
                Some(k) => vm.string(k)?,
                None => JsValue::UNDEFINED,
            };
            let ret = vm.call(
                self,
                fn_val,
                JsValue::object(g),
                &[target_val, ev_js, val_js, key_js],
            )?;
            vm.drain_jobs(self)?;
            if ret.as_bool() == Some(true) {
                return Ok(true);
            }
        }
        let mut handled = false;

        for current_id in chain {
            if self.event_propagation_stopped {
                break;
            }
            let matching: Vec<Persistent> = self
                .listeners
                .iter()
                .filter(|(nid, ev, _)| {
                    *nid == current_id && (ev == &wanted || (wanted == "input" && ev == "change"))
                })
                .map(|(_, _, cb)| *cb)
                .collect();
            if matching.is_empty() {
                continue;
            }
            let cur_val = self.wrap_node_in_vm(vm, current_id)?;
            for cb_pers in matching {
                let cb_val = vm
                    .heap()
                    .persistent(cb_pers)
                    .map_err(|e| RunError::internal(format!("{e:?}")))?;
                let ev_obj = vm.new_plain_object()?;
                let t_str = vm.string(&wanted)?;
                vm.define_data(ev_obj, "type", t_str)?;
                vm.define_data(ev_obj, "target", target_val)?;
                vm.define_data(ev_obj, "currentTarget", cur_val)?;
                vm.define_data(ev_obj, "bubbles", JsValue::TRUE)?;
                vm.define_data(ev_obj, "cancelable", JsValue::TRUE)?;
                if let Some(v) = value {
                    let vs = vm.string(v)?;
                    vm.define_data(ev_obj, "value", vs)?;
                    vm.define_data(ev_obj, "data", vs)?;
                }
                if let Some(k) = key {
                    let ks = vm.string(k)?;
                    vm.define_data(ev_obj, "key", ks)?;
                    vm.define_data(ev_obj, "code", ks)?;
                }
                vm.define_accessor(
                    ev_obj,
                    "defaultPrevented",
                    Some(ev_get_default_prevented),
                    None,
                )?;
                vm.define_method(ev_obj, "preventDefault", ev_prevent_default)?;
                vm.define_method(ev_obj, "stopPropagation", ev_stop_propagation)?;

                let ret = vm.call(self, cb_val, cur_val, &[JsValue::object(ev_obj)])?;
                if ret.as_bool() == Some(false) {
                    self.event_default_prevented = true;
                }
                handled = true;
                vm.drain_jobs(self)?;
            }
        }
        Ok(handled)
    }

    /// Invoke `window.__reportUncaughtError(msg)` if installed, firing `window.onerror`
    /// and any registered `'error'` event listeners without aborting the host session.
    pub fn report_uncaught_error(&mut self, vm: &mut Vm, message: &str) {
        let g = vm.global();
        if let Ok(fn_key) = vm.heap_mut().key_from_str("__reportUncaughtError")
            && let Ok(Some(prop)) = vm.heap().get_own(g, fn_key)
            && let Some(fn_val) = prop.slot.data()
            && fn_val.as_object().is_some()
            && let Ok(msg_js) = vm.string(message)
        {
            let _ = vm.call(self, fn_val, JsValue::object(g), &[msg_js]);
            let _ = vm.drain_jobs(self);
        }
    }

    fn wrap_node_in_vm(&self, vm: &mut Vm, nid: NodeId) -> Result<JsValue, RunError> {
        let data = HostData(TAG_NODE | (nid.index() as u64));
        if let Some(existing) = vm.heap().wrapper(data) {
            return Ok(JsValue::object(existing));
        }
        let obj = vm.new_host_object(self.element_proto, data)?;
        vm.heap_mut().set_wrapper(data, obj);
        Ok(JsValue::object(obj))
    }
}

impl Host for CinderDomHost {
    fn enqueue_job(&mut self, job: Job) {
        self.jobs.push_back(job);
    }

    fn take_job(&mut self) -> Option<Job> {
        self.jobs.pop_front()
    }

    fn trace(&self, roots: &mut Vec<JsValue>) {
        for proto in [
            self.element_proto,
            self.class_list_proto,
            self.style_proto,
            self.dataset_proto,
            self.document_obj,
        ]
        .into_iter()
        .flatten()
        {
            roots.push(JsValue::object(proto));
        }
    }

    fn now_ms(&mut self) -> f64 {
        let cur = self.clock_ms;
        self.clock_ms += 1.0;
        cur
    }

    fn console(&mut self, level: &str, text: &str) {
        let prefix = match level {
            "warn" => "[warn] ",
            "error" => "[error] ",
            _ => "",
        };
        self.console.push(format!("{prefix}{text}"));
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// ---- Host/Wrapper helpers -------------------------------------------------

fn wrap_node(cx: &mut NativeCx<'_>, nid: NodeId) -> Result<JsValue, RunError> {
    let data = HostData(TAG_NODE | (nid.index() as u64));
    if let Some(existing) = cx.heap().wrapper(data) {
        return Ok(JsValue::object(existing));
    }
    let proto = cx.host_as::<CinderDomHost>().and_then(|h| h.element_proto);
    let obj = cx.vm().new_host_object(proto, data)?;
    cx.heap().set_wrapper(data, obj);
    Ok(JsValue::object(obj))
}

fn wrap_optional_node(cx: &mut NativeCx<'_>, nid: Option<NodeId>) -> Result<JsValue, RunError> {
    match nid {
        Some(id) => wrap_node(cx, id),
        None => Ok(JsValue::NULL),
    }
}

fn wrap_node_list(cx: &mut NativeCx<'_>, ids: Vec<NodeId>) -> Result<JsValue, RunError> {
    let mut vals = Vec::with_capacity(ids.len());
    for id in ids {
        let v = wrap_node(cx, id)?;
        cx.keep(v);
        vals.push(v);
    }
    cx.vm().new_array_from(&vals)
}

fn unwrap_node(cx: &mut NativeCx<'_>, val: JsValue) -> Option<NodeId> {
    let obj = val.as_object()?;
    if let Some(doc_obj) = cx.host_as::<CinderDomHost>().and_then(|h| h.document_obj)
        && obj == doc_obj
    {
        return cx.host_as::<CinderDomHost>().map(|h| h.doc.root());
    }
    let class = cx.heap().object(obj).ok()?.class.clone();
    match class {
        ObjectClass::Host(HostData(raw)) => {
            let tag = raw & TAG_MASK;
            if matches!(tag, TAG_NODE | TAG_CLASS_LIST | TAG_STYLE | TAG_DATASET) {
                Some(NodeId::from_raw((raw & 0xFFFF_FFFF) as u32))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn arg_str(cx: &mut NativeCx<'_>, args: &[JsValue], idx: usize) -> Result<String, RunError> {
    match args.get(idx).copied() {
        Some(v) if !v.is_undefined() => cx.to_display_string(v),
        _ => Ok(String::new()),
    }
}

// ---- Event methods --------------------------------------------------------

fn ev_get_default_prevented(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let prevented = cx
        .host_as::<CinderDomHost>()
        .is_some_and(|h| h.event_default_prevented);
    Ok(JsValue::boolean(prevented))
}

fn ev_prevent_default(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.event_default_prevented = true;
    }
    Ok(JsValue::UNDEFINED)
}

fn ev_stop_propagation(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.event_propagation_stopped = true;
    }
    Ok(JsValue::UNDEFINED)
}

// ---- Document methods & accessors -----------------------------------------

fn doc_get_element_by_id(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let id_str = arg_str(cx, args, 0)?;
    let found = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get_element_by_id(&id_str));
    wrap_optional_node(cx, found)
}

fn doc_query_selector(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let sel = arg_str(cx, args, 0)?;
    let found = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.query_selector(&sel));
    wrap_optional_node(cx, found)
}

fn doc_query_selector_all(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let sel = arg_str(cx, args, 0)?;
    let ids = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.query_selector_all(&sel))
        .unwrap_or_default();
    wrap_node_list(cx, ids)
}

fn doc_create_element(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let tag = arg_str(cx, args, 0)?;
    let tag = if tag.trim().is_empty() {
        "div"
    } else {
        tag.trim()
    };
    let Some(h) = cx.host_as::<CinderDomHost>() else {
        return Ok(JsValue::NULL);
    };
    let nid = h.doc.create_element(tag, Vec::new());
    wrap_node(cx, nid)
}

fn doc_create_text_node(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let text = arg_str(cx, args, 0)?;
    let Some(h) = cx.host_as::<CinderDomHost>() else {
        return Ok(JsValue::NULL);
    };
    let nid = h.doc.create_text(&text);
    wrap_node(cx, nid)
}

fn doc_create_document_fragment(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(h) = cx.host_as::<CinderDomHost>() else {
        return Ok(JsValue::NULL);
    };
    let nid = h.doc.create_element("anvil-fragment", Vec::new());
    wrap_node(cx, nid)
}

fn doc_get_elements_by_tag_name(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let tag = arg_str(cx, args, 0)?;
    let ids = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.get_elements_by_tag_name(h.doc.root(), &tag))
        .unwrap_or_default();
    wrap_node_list(cx, ids)
}

fn doc_get_elements_by_class_name(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let cls = arg_str(cx, args, 0)?;
    let ids = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.get_elements_by_class_name(h.doc.root(), &cls))
        .unwrap_or_default();
    wrap_node_list(cx, ids)
}

fn doc_get_elements_by_name(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let name = arg_str(cx, args, 0)?;
    let ids = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.get_elements_by_name(&name))
        .unwrap_or_default();
    wrap_node_list(cx, ids)
}

fn doc_add_event_listener(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let ev_type = arg_str(cx, args, 0)?.trim().to_ascii_lowercase();
    let Some(cb) = args.get(1).copied().filter(|v| v.is_object()) else {
        return Ok(JsValue::UNDEFINED);
    };
    if matches!(
        ev_type.as_str(),
        "domcontentloaded" | "load" | "readystatechange"
    ) {
        cx.enqueue_job(cb, JsValue::UNDEFINED, &[])?;
        return Ok(JsValue::UNDEFINED);
    }
    let pers = cx
        .heap()
        .persist(cb)
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        let root = h.doc.root();
        h.listeners.push((root, ev_type, pers));
    }
    Ok(JsValue::UNDEFINED)
}

fn doc_remove_event_listener(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let ev_type = arg_str(cx, args, 0)?.trim().to_ascii_lowercase();
    let Some(cb) = args.get(1).copied().filter(|v| v.is_object()) else {
        return Ok(JsValue::UNDEFINED);
    };
    let root = cx.host_as::<CinderDomHost>().map(|h| h.doc.root());
    if let Some(root_id) = root {
        remove_listener_for_node(cx, root_id, &ev_type, cb);
    }
    Ok(JsValue::UNDEFINED)
}

fn doc_dispatch_event(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let root = cx.host_as::<CinderDomHost>().map(|h| h.doc.root());
    let Some(root_id) = root else {
        return Ok(JsValue::TRUE);
    };
    dispatch_js_event_on_node(cx, root_id, args)
}

fn doc_adopt_node(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = args.first().copied().and_then(|v| unwrap_node(cx, v)) else {
        return Ok(JsValue::NULL);
    };
    let parent = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).parent());
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.doc.remove_node(nid);
    }
    if let Some(p) = parent {
        notify_child_mutation(cx, p, &[], &[nid], None, None)?;
    }
    let has_ce = cx
        .host_as::<CinderDomHost>()
        .is_some_and(|h| h.has_custom_elements);
    let node_val = wrap_node(cx, nid)?;
    if has_ce {
        cx.keep(node_val);
        let g = cx.vm().global();
        if let Ok(key) = cx.heap().key_from_str("__onCustomElementAdopted")
            && let Ok(Some(prop)) = cx.heap().get_own(g, key)
            && let Some(fn_val) = prop.slot.data()
            && fn_val.as_object().is_some()
        {
            let doc_val = el_get_owner_document(cx, JsValue::UNDEFINED, &[])?;
            let _ = cx.call(fn_val, JsValue::object(g), &[node_val, doc_val, doc_val]);
        }
    }
    Ok(node_val)
}

fn doc_import_node(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = args.first().copied().and_then(|v| unwrap_node(cx, v)) else {
        return Ok(JsValue::NULL);
    };
    let deep = args
        .get(1)
        .copied()
        .is_none_or(|v| v.is_undefined() || v.as_bool() == Some(true));
    let cloned = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.clone_node(nid, deep));
    wrap_optional_node(cx, cloned)
}

fn doc_parse_html_fragment(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let markup = arg_str(cx, args, 0)?;
    let parsed = parse_html(&markup);
    let Some(h) = cx.host_as::<CinderDomHost>() else {
        return Ok(JsValue::NULL);
    };
    let doc_root = h.doc.create_element("document", Vec::new());
    let head_el = h.doc.create_element("head", Vec::new());
    let body_el = h.doc.create_element("body", Vec::new());
    let _ = h.doc.append(doc_root, head_el);
    let _ = h.doc.append(doc_root, body_el);
    for &c in parsed.children(parsed.head()) {
        if let Some(imported) = h.doc.import_subtree(&parsed, c, true) {
            let _ = h.doc.append(head_el, imported);
        }
    }
    for &c in parsed.children(parsed.body()) {
        if let Some(imported) = h.doc.import_subtree(&parsed, c, true) {
            let _ = h.doc.append(body_el, imported);
        }
    }
    let doc_val = wrap_node(cx, doc_root)?;
    let head_val = wrap_node(cx, head_el)?;
    let body_val = wrap_node(cx, body_el)?;
    if let Some(obj) = doc_val.as_object() {
        cx.vm().define_data(obj, "head", head_val)?;
        cx.vm().define_data(obj, "body", body_val)?;
        cx.vm().define_data(obj, "documentElement", doc_val)?;
    }
    Ok(doc_val)
}

fn doc_add_adopted_sheet(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let css = arg_str(cx, args, 0)?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.doc.add_adopted_style_sheet(css);
    }
    Ok(JsValue::UNDEFINED)
}

fn doc_set_adopted_sheets(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let joined = arg_str(cx, args, 0)?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        if joined.trim().is_empty() {
            h.doc.set_adopted_style_sheets(Vec::new());
        } else {
            h.doc.set_adopted_style_sheets(vec![joined]);
        }
    }
    Ok(JsValue::UNDEFINED)
}

fn doc_get_active_element(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let active = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.focused_node().or_else(|| Some(h.doc.body())));
    wrap_optional_node(cx, active)
}

fn doc_get_title(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let title = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.title())
        .unwrap_or_default();
    cx.vm().string(&title)
}

fn doc_set_title(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let title = arg_str(cx, args, 0)?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.doc.set_title(&title);
    }
    Ok(JsValue::UNDEFINED)
}

fn doc_get_body(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let body = cx.host_as::<CinderDomHost>().map(|h| h.doc.body());
    wrap_optional_node(cx, body)
}

fn doc_get_head(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let head = cx.host_as::<CinderDomHost>().map(|h| h.doc.head());
    wrap_optional_node(cx, head)
}

fn doc_get_document_element(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let html = cx.host_as::<CinderDomHost>().map(|h| h.doc.html());
    wrap_optional_node(cx, html)
}

fn doc_get_ready_state(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    cx.vm().string("complete")
}

fn doc_get_url(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let url = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.location_href.clone())
        .unwrap_or_default();
    cx.vm().string(&url)
}

// ---- Element methods & accessors ------------------------------------------

fn el_get_attribute(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let name = arg_str(cx, args, 0)?;
    let val = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).data().attr(&name).map(str::to_string));
    match val {
        Some(s) => cx.vm().string(&s),
        None => Ok(JsValue::NULL),
    }
}

fn el_set_attribute(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let name = arg_str(cx, args, 0)?;
    let val = arg_str(cx, args, 1)?;
    let old_val = cx.host_as::<CinderDomHost>().and_then(|h| {
        let prev = h.doc.get(nid).data().attr(&name).map(str::to_string);
        h.doc.set_attribute(nid, &name, &val);
        prev
    });
    notify_attr_mutation(cx, nid, &name, old_val, Some(val))?;
    Ok(JsValue::UNDEFINED)
}

fn el_remove_attribute(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let name = arg_str(cx, args, 0)?;
    let old_val = cx.host_as::<CinderDomHost>().and_then(|h| {
        let prev = h.doc.get(nid).data().attr(&name).map(str::to_string);
        h.doc.remove_attribute(nid, &name);
        prev
    });
    notify_attr_mutation(cx, nid, &name, old_val, None)?;
    Ok(JsValue::UNDEFINED)
}

fn el_has_attribute(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::FALSE);
    };
    let name = arg_str(cx, args, 0)?;
    let has = cx
        .host_as::<CinderDomHost>()
        .is_some_and(|h| h.doc.get(nid).data().attr(&name).is_some());
    Ok(JsValue::boolean(has))
}

fn el_append_child(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(parent_id) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let child_val = args.first().copied().unwrap_or(JsValue::NULL);
    let Some(child_id) = unwrap_node(cx, child_val) else {
        return Ok(child_val);
    };
    let mut added = Vec::new();
    let mut prev_sib = None;
    let mut old_parent_removed: Option<(NodeId, NodeId)> = None;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        prev_sib = h.doc.last_child(parent_id);
        if h.doc.get(child_id).data().tag() == Some("anvil-fragment") {
            let kids: Vec<NodeId> = h.doc.children(child_id).to_vec();
            for k in kids {
                let _ = h.doc.append(parent_id, k);
                added.push(k);
            }
        } else {
            if let Some(old_p) = h.doc.get(child_id).parent()
                && old_p != parent_id
            {
                old_parent_removed = Some((old_p, child_id));
            }
            let _ = h.doc.append(parent_id, child_id);
            added.push(child_id);
        }
    }
    if let Some((old_p, rem_id)) = old_parent_removed {
        notify_child_mutation(cx, old_p, &[], &[rem_id], None, None)?;
    }
    if !added.is_empty() {
        notify_child_mutation(cx, parent_id, &added, &[], prev_sib, None)?;
    }
    Ok(child_val)
}

fn el_remove_child(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(parent_id) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let child_val = args.first().copied().unwrap_or(JsValue::NULL);
    let mut removed = Vec::new();
    let mut prev_sib = None;
    let mut next_sib = None;
    if let Some(child_id) = unwrap_node(cx, child_val)
        && let Some(h) = cx.host_as::<CinderDomHost>()
    {
        prev_sib = h.doc.previous_sibling(child_id);
        next_sib = h.doc.next_sibling(child_id);
        if h.doc.remove_child(parent_id, child_id).is_ok() {
            removed.push(child_id);
        }
    }
    if !removed.is_empty() {
        notify_child_mutation(cx, parent_id, &[], &removed, prev_sib, next_sib)?;
    }
    Ok(child_val)
}

fn el_insert_before(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(parent_id) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let new_val = args.first().copied().unwrap_or(JsValue::NULL);
    let Some(new_id) = unwrap_node(cx, new_val) else {
        return Ok(new_val);
    };
    let ref_id = args.get(1).copied().and_then(|v| unwrap_node(cx, v));
    let mut added = Vec::new();
    let mut prev_sib = None;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        prev_sib = match ref_id {
            Some(rid) => h.doc.previous_sibling(rid),
            None => h.doc.last_child(parent_id),
        };
        if h.doc.get(new_id).data().tag() == Some("anvil-fragment") {
            let kids: Vec<NodeId> = h.doc.children(new_id).to_vec();
            for k in kids {
                let _ = h.doc.insert_before(parent_id, k, ref_id);
                added.push(k);
            }
        } else {
            let _ = h.doc.insert_before(parent_id, new_id, ref_id);
            added.push(new_id);
        }
    }
    if !added.is_empty() {
        notify_child_mutation(cx, parent_id, &added, &[], prev_sib, ref_id)?;
    }
    Ok(new_val)
}

fn el_replace_child(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(parent_id) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let new_val = args.first().copied().unwrap_or(JsValue::NULL);
    let old_val = args.get(1).copied().unwrap_or(JsValue::NULL);
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut prev_sib = None;
    let mut next_sib = None;
    if let (Some(new_id), Some(old_id)) = (unwrap_node(cx, new_val), unwrap_node(cx, old_val))
        && let Some(h) = cx.host_as::<CinderDomHost>()
    {
        prev_sib = h.doc.previous_sibling(old_id);
        next_sib = h.doc.next_sibling(old_id);
        if h.doc.replace_child(parent_id, new_id, old_id).is_ok() {
            added.push(new_id);
            removed.push(old_id);
        }
    }
    if !added.is_empty() || !removed.is_empty() {
        notify_child_mutation(cx, parent_id, &added, &removed, prev_sib, next_sib)?;
    }
    Ok(old_val)
}

fn el_clone_node(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let deep = args
        .first()
        .copied()
        .is_some_and(|v| v.as_bool() == Some(true));
    let cloned = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.clone_node(nid, deep));
    wrap_optional_node(cx, cloned)
}

fn el_remove(cx: &mut NativeCx<'_>, this: JsValue, _args: &[JsValue]) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let mut parent = None;
    let mut prev_sib = None;
    let mut next_sib = None;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        parent = h.doc.get(nid).parent();
        prev_sib = h.doc.previous_sibling(nid);
        next_sib = h.doc.next_sibling(nid);
        h.doc.remove_node(nid);
    }
    if let Some(p) = parent {
        notify_child_mutation(cx, p, &[], &[nid], prev_sib, next_sib)?;
    }
    Ok(JsValue::UNDEFINED)
}

fn el_query_selector(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let sel = arg_str(cx, args, 0)?;
    let found = cx.host_as::<CinderDomHost>().and_then(|h| {
        if nid == h.doc.root() {
            h.doc.query_selector(&sel)
        } else {
            h.doc.query_selector_within(nid, &sel)
        }
    });
    wrap_optional_node(cx, found)
}

fn el_query_selector_all(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().new_array_from(&[]);
    };
    let sel = arg_str(cx, args, 0)?;
    let ids = cx
        .host_as::<CinderDomHost>()
        .map(|h| {
            if nid == h.doc.root() {
                h.doc.query_selector_all(&sel)
            } else {
                h.doc.query_selector_all_within(nid, &sel)
            }
        })
        .unwrap_or_default();
    wrap_node_list(cx, ids)
}

fn el_get_element_by_id(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let id_str = arg_str(cx, args, 0)?;
    let nid = unwrap_node(cx, this);
    let found = cx.host_as::<CinderDomHost>().and_then(|h| {
        if let Some(sub_root) = nid
            && sub_root != h.doc.root()
        {
            h.doc
                .query_selector_within(sub_root, &format!("#{id_str}"))
                .or_else(|| h.doc.get_element_by_id(&id_str))
        } else {
            h.doc.get_element_by_id(&id_str)
        }
    });
    wrap_optional_node(cx, found)
}

fn el_get_elements_by_tag_name(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().new_array_from(&[]);
    };
    let tag = arg_str(cx, args, 0)?;
    let ids = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.get_elements_by_tag_name(nid, &tag))
        .unwrap_or_default();
    wrap_node_list(cx, ids)
}

fn el_get_elements_by_class_name(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().new_array_from(&[]);
    };
    let cls = arg_str(cx, args, 0)?;
    let ids = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.get_elements_by_class_name(nid, &cls))
        .unwrap_or_default();
    wrap_node_list(cx, ids)
}

fn el_insert_adjacent_html(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let pos = arg_str(cx, args, 0)?;
    let html = arg_str(cx, args, 1)?;
    let mut target_parent = nid;
    let mut added = Vec::new();
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        let p_norm = pos.trim().to_ascii_lowercase();
        target_parent = if matches!(p_norm.as_str(), "beforebegin" | "afterend") {
            h.doc.get(nid).parent().unwrap_or(nid)
        } else {
            nid
        };
        let before: Vec<NodeId> = h.doc.children(target_parent).to_vec();
        h.doc.insert_adjacent_html(nid, &pos, &html);
        for &cid in h.doc.children(target_parent) {
            if !before.contains(&cid) && h.doc.shadow_host(cid).is_none() {
                added.push(cid);
            }
        }
    }
    if !added.is_empty() {
        notify_child_mutation(cx, target_parent, &added, &[], None, None)?;
    }
    Ok(JsValue::UNDEFINED)
}

fn el_insert_adjacent_text(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let pos = arg_str(cx, args, 0)?;
    let text = arg_str(cx, args, 1)?;
    let mut target_parent = nid;
    let mut added = Vec::new();
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        let p_norm = pos.trim().to_ascii_lowercase();
        target_parent = if matches!(p_norm.as_str(), "beforebegin" | "afterend") {
            h.doc.get(nid).parent().unwrap_or(nid)
        } else {
            nid
        };
        let before: Vec<NodeId> = h.doc.children(target_parent).to_vec();
        h.doc.insert_adjacent_text(nid, &pos, &text);
        for &cid in h.doc.children(target_parent) {
            if !before.contains(&cid) && h.doc.shadow_host(cid).is_none() {
                added.push(cid);
            }
        }
    }
    if !added.is_empty() {
        notify_child_mutation(cx, target_parent, &added, &[], None, None)?;
    }
    Ok(JsValue::UNDEFINED)
}

fn el_insert_adjacent_element(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let pos = arg_str(cx, args, 0)?;
    let Some(new_el) = args.get(1).copied().and_then(|v| unwrap_node(cx, v)) else {
        return Ok(JsValue::NULL);
    };
    let mut target_parent = nid;
    let inserted = cx.host_as::<CinderDomHost>().and_then(|h| {
        let p_norm = pos.trim().to_ascii_lowercase();
        target_parent = if matches!(p_norm.as_str(), "beforebegin" | "afterend") {
            h.doc.get(nid).parent().unwrap_or(nid)
        } else {
            nid
        };
        h.doc.insert_adjacent_element(nid, &pos, new_el)
    });
    if let Some(ins_id) = inserted {
        notify_child_mutation(cx, target_parent, &[ins_id], &[], None, None)?;
    }
    wrap_optional_node(cx, inserted)
}

fn el_matches(cx: &mut NativeCx<'_>, this: JsValue, args: &[JsValue]) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::FALSE);
    };
    let sel = arg_str(cx, args, 0)?;
    let matched = cx
        .host_as::<CinderDomHost>()
        .is_some_and(|h| h.doc.matches_selector(nid, &sel));
    Ok(JsValue::boolean(matched))
}

fn el_closest(cx: &mut NativeCx<'_>, this: JsValue, args: &[JsValue]) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let sel = arg_str(cx, args, 0)?;
    let found = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.closest(nid, &sel));
    wrap_optional_node(cx, found)
}

fn el_contains(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::FALSE);
    };
    let Some(other_id) = args.first().copied().and_then(|v| unwrap_node(cx, v)) else {
        return Ok(JsValue::FALSE);
    };
    let contains = cx
        .host_as::<CinderDomHost>()
        .is_some_and(|h| h.doc.contains_node(nid, other_id));
    Ok(JsValue::boolean(contains))
}

fn el_attach_shadow(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let mut open = true;
    if let Some(init_obj) = args.first().copied().and_then(|v| v.as_object())
        && let Ok(key) = cx.heap().key_from_str("mode")
        && let Ok(Some(prop)) = cx.heap().get_own(init_obj, key)
        && let Some(val) = prop.slot.data()
        && let Ok(mode_str) = cx.to_display_string(val)
        && mode_str.trim().eq_ignore_ascii_case("closed")
    {
        open = false;
    }
    let sr = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.attach_shadow(nid, open));
    wrap_optional_node(cx, sr)
}

fn el_assigned_nodes(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return wrap_node_list(cx, Vec::new());
    };
    let mut flatten = false;
    if let Some(opts_obj) = args.first().copied().and_then(|v| v.as_object())
        && let Ok(key) = cx.heap().key_from_str("flatten")
        && let Ok(Some(prop)) = cx.heap().get_own(opts_obj, key)
        && let Some(val) = prop.slot.data()
    {
        flatten = val.as_bool() == Some(true);
    }
    let assigned = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.assigned_nodes(nid, flatten))
        .unwrap_or_default();
    wrap_node_list(cx, assigned)
}

fn el_assigned_elements(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return wrap_node_list(cx, Vec::new());
    };
    let mut flatten = false;
    if let Some(opts_obj) = args.first().copied().and_then(|v| v.as_object())
        && let Ok(key) = cx.heap().key_from_str("flatten")
        && let Ok(Some(prop)) = cx.heap().get_own(opts_obj, key)
        && let Some(val) = prop.slot.data()
    {
        flatten = val.as_bool() == Some(true);
    }
    let assigned = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.assigned_elements(nid, flatten))
        .unwrap_or_default();
    wrap_node_list(cx, assigned)
}

fn el_get_slot(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().string("");
    };
    let s = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).data().attr("slot").map(str::to_string))
        .unwrap_or_default();
    cx.vm().string(&s)
}

fn el_set_slot(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let val = arg_str(cx, args, 0)?;
    let old_val = cx.host_as::<CinderDomHost>().and_then(|h| {
        let prev = h.doc.get(nid).data().attr("slot").map(str::to_string);
        h.doc.set_attribute(nid, "slot", &val);
        prev
    });
    notify_attr_mutation(cx, nid, "slot", old_val, Some(val))?;
    Ok(JsValue::UNDEFINED)
}

fn el_get_assigned_slot(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let slot_id = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.assigned_slot(nid));
    wrap_optional_node(cx, slot_id)
}

fn el_get_shadow_host(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let host_id = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.shadow_host(nid));
    wrap_optional_node(cx, host_id)
}

fn el_get_shadow_mode(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let mode = cx.host_as::<CinderDomHost>().and_then(|h| {
        let host_id = h.doc.shadow_host(nid)?;
        if h.doc.shadow_root(host_id) == Some(nid) {
            Some("open")
        } else {
            Some("closed")
        }
    });
    match mode {
        Some(m) => cx.vm().string(m),
        None => Ok(JsValue::UNDEFINED),
    }
}

fn el_add_event_listener(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let ev_type = arg_str(cx, args, 0)?.trim().to_ascii_lowercase();
    let Some(cb) = args.get(1).copied().filter(|v| v.is_object()) else {
        return Ok(JsValue::UNDEFINED);
    };
    let pers = cx
        .heap()
        .persist(cb)
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.listeners.push((nid, ev_type, pers));
    }
    Ok(JsValue::UNDEFINED)
}

fn el_remove_event_listener(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let ev_type = arg_str(cx, args, 0)?.trim().to_ascii_lowercase();
    let Some(cb) = args.get(1).copied().filter(|v| v.is_object()) else {
        return Ok(JsValue::UNDEFINED);
    };
    remove_listener_for_node(cx, nid, &ev_type, cb);
    Ok(JsValue::UNDEFINED)
}

fn remove_listener_for_node(cx: &mut NativeCx<'_>, nid: NodeId, ev_type: &str, cb: JsValue) {
    let Some(listeners) = cx.host_as::<CinderDomHost>().map(|h| h.listeners.clone()) else {
        return;
    };
    let mut kept = Vec::with_capacity(listeners.len());
    for (lnid, lev, lpers) in listeners {
        let is_same = lnid == nid
            && lev == ev_type
            && cx
                .heap()
                .persistent(lpers)
                .is_ok_and(|stored| stored.as_object() == cb.as_object());
        if !is_same {
            kept.push((lnid, lev, lpers));
        }
    }
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.listeners = kept;
    }
}

fn el_dispatch_event(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::TRUE);
    };
    dispatch_js_event_on_node(cx, nid, args)
}

fn dispatch_js_event_on_node(
    cx: &mut NativeCx<'_>,
    target: NodeId,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(ev_val) = args.first().copied() else {
        return Ok(JsValue::TRUE);
    };
    let Some(ev_obj) = ev_val.as_object() else {
        return Ok(JsValue::TRUE);
    };
    let type_key = cx
        .heap()
        .key_from_str("type")
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    let ev_type = if let Ok(Some(prop)) = cx.heap().get_own(ev_obj, type_key)
        && let Some(val) = prop.slot.data()
    {
        cx.to_display_string(val)?.trim().to_ascii_lowercase()
    } else {
        String::new()
    };
    if ev_type.is_empty() {
        return Ok(JsValue::TRUE);
    }
    let bubbles_key = cx
        .heap()
        .key_from_str("bubbles")
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    let bubbles = if let Ok(Some(prop)) = cx.heap().get_own(ev_obj, bubbles_key)
        && let Some(val) = prop.slot.data()
    {
        val.as_bool().unwrap_or(false)
    } else {
        false
    };
    let target_val = wrap_node(cx, target)?;
    cx.vm().define_data(ev_obj, "target", target_val)?;

    let mut chain = vec![target];
    if bubbles && let Some(h) = cx.host_as::<CinderDomHost>() {
        let mut cur = h.doc.get(target).parent();
        while let Some(p) = cur {
            chain.push(p);
            cur = h.doc.get(p).parent();
        }
    }
    let stopped_key = cx
        .heap()
        .key_from_str("_stopped")
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    for current_id in chain {
        if let Ok(Some(prop)) = cx.heap().get_own(ev_obj, stopped_key)
            && prop.slot.data().and_then(|v| v.as_bool()) == Some(true)
        {
            break;
        }
        let matching: Vec<Persistent> = cx
            .host_as::<CinderDomHost>()
            .map(|h| {
                h.listeners
                    .iter()
                    .filter(|(nid, ev, _)| *nid == current_id && ev == &ev_type)
                    .map(|(_, _, cb)| *cb)
                    .collect()
            })
            .unwrap_or_default();
        if matching.is_empty() {
            continue;
        }
        let cur_val = wrap_node(cx, current_id)?;
        cx.vm().define_data(ev_obj, "currentTarget", cur_val)?;
        for cb_pers in matching {
            let cb_val = cx
                .heap()
                .persistent(cb_pers)
                .map_err(|e| RunError::internal(format!("{e:?}")))?;
            let _ = cx.call(cb_val, cur_val, &[ev_val])?;
        }
    }
    let dp_key = cx
        .heap()
        .key_from_str("defaultPrevented")
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    let prevented = if let Ok(Some(prop)) = cx.heap().get_own(ev_obj, dp_key)
        && let Some(val) = prop.slot.data()
    {
        val.as_bool().unwrap_or(false)
    } else {
        false
    };
    Ok(JsValue::boolean(!prevented))
}

fn el_focus(cx: &mut NativeCx<'_>, this: JsValue, _args: &[JsValue]) -> Result<JsValue, RunError> {
    if let Some(nid) = unwrap_node(cx, this)
        && let Some(h) = cx.host_as::<CinderDomHost>()
    {
        h.doc.set_focused_node(Some(nid));
    }
    Ok(JsValue::UNDEFINED)
}

fn el_blur(cx: &mut NativeCx<'_>, this: JsValue, _args: &[JsValue]) -> Result<JsValue, RunError> {
    if let Some(nid) = unwrap_node(cx, this)
        && let Some(h) = cx.host_as::<CinderDomHost>()
        && h.doc.focused_node() == Some(nid)
    {
        h.doc.set_focused_node(None);
    }
    Ok(JsValue::UNDEFINED)
}

fn el_get_bounding_client_rect(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let (x, y, w, h) = if let Some(nid) = unwrap_node(cx, this)
        && let Some(host) = cx.host_as::<CinderDomHost>()
    {
        if let Some(b) = host.compute_node_box(nid) {
            (
                b.rect.x as f64,
                b.rect.y as f64,
                b.rect.width as f64,
                b.rect.height as f64,
            )
        } else {
            let styles = host.compute_styles();
            let cs = styles.get(nid);
            let inline = host
                .doc
                .get(nid)
                .data()
                .attr("style")
                .map(str::to_string)
                .unwrap_or_default();
            let parse_px = |prop: &str| -> Option<f64> {
                let s = read_inline_style(&host.doc, nid, prop);
                if s.is_empty() {
                    None
                } else {
                    s.trim().trim_end_matches("px").trim().parse::<f64>().ok()
                }
            };
            let size_px = |sz: anvil_style::Size| -> Option<f64> {
                match sz {
                    anvil_style::Size::Px(v) => Some(v as f64),
                    _ => None,
                }
            };
            if cs.display == anvil_style::Display::None && inline.is_empty() {
                (0.0, 0.0, 0.0, 0.0)
            } else {
                (
                    parse_px("left").or_else(|| size_px(cs.left)).unwrap_or(0.0),
                    parse_px("top").or_else(|| size_px(cs.top)).unwrap_or(0.0),
                    parse_px("width")
                        .or_else(|| size_px(cs.width))
                        .unwrap_or(100.0),
                    parse_px("height")
                        .or_else(|| size_px(cs.height))
                        .unwrap_or(24.0),
                )
            }
        }
    } else {
        (0.0, 0.0, 100.0, 24.0)
    };
    let rect = cx.vm().new_plain_object()?;
    for (k, v) in [
        ("x", x),
        ("y", y),
        ("top", y),
        ("left", x),
        ("width", w),
        ("height", h),
        ("right", x + w),
        ("bottom", y + h),
    ] {
        cx.vm().define_data(rect, k, JsValue::number(v))?;
    }
    Ok(JsValue::object(rect))
}

fn el_get_context(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let ctx_obj = cx
        .vm()
        .new_host_object(None, HostData(TAG_NODE | (nid.index() as u64)))?;
    let black = cx.vm().string("#000000")?;
    let font = cx.vm().string("10px sans-serif")?;
    cx.vm().define_data(ctx_obj, "canvas", this)?;
    cx.vm().define_data(ctx_obj, "fillStyle", black)?;
    cx.vm().define_data(ctx_obj, "strokeStyle", black)?;
    cx.vm().define_data(ctx_obj, "font", font)?;
    cx.vm()
        .define_data(ctx_obj, "lineWidth", JsValue::number(1.0))?;
    cx.vm()
        .define_data(ctx_obj, "globalAlpha", JsValue::number(1.0))?;
    for (name, func) in [
        ("fillRect", canvas_fill_rect as NativeFn),
        ("strokeRect", canvas_stroke_rect),
        ("clearRect", canvas_clear_rect),
        ("fillText", canvas_fill_text),
        ("strokeText", canvas_fill_text),
        ("beginPath", canvas_begin_path),
        ("moveTo", canvas_move_to),
        ("lineTo", canvas_line_to),
        ("rect", canvas_rect),
        ("arc", canvas_arc),
        ("closePath", canvas_close_path),
        ("fill", canvas_fill),
        ("stroke", canvas_stroke),
        ("measureText", canvas_measure_text),
        ("getImageData", canvas_get_image_data),
        ("putImageData", canvas_put_image_data),
        ("createImageData", canvas_create_image_data),
        ("save", el_noop),
        ("restore", el_noop),
        ("scale", el_noop),
        ("translate", el_noop),
        ("rotate", el_noop),
        ("transform", el_noop),
        ("setTransform", el_noop),
        ("resetTransform", el_noop),
        ("setLineDash", el_noop),
        ("clip", el_noop),
        ("__fillLinearGradient", canvas_fill_linear_gradient),
    ] {
        cx.vm().define_method(ctx_obj, name, func)?;
    }
    Ok(JsValue::object(ctx_obj))
}

fn canvas_style_color(cx: &mut NativeCx<'_>, this: JsValue, prop_name: &str) -> [u8; 4] {
    let style_str = if let Some(obj) = this.as_object()
        && let Ok(key) = cx.heap().key_from_str(prop_name)
        && let Ok(Some(prop)) = cx.heap().get_own(obj, key)
        && let Some(val) = prop.slot.data()
    {
        cx.to_display_string(val)
            .unwrap_or_else(|_| "#000000".to_string())
    } else {
        "#000000".to_string()
    };
    crate::parse_canvas_color(&style_str)
}

fn canvas_fill_rect(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let x = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let y = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let w = args
        .get(2)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as u32;
    let h = args
        .get(3)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as u32;
    let rgba = canvas_style_color(cx, this, "fillStyle");
    if let Some(host) = cx.host_as::<CinderDomHost>() {
        host.doc.canvas_fill_rect(nid, x, y, w, h, rgba);
    }
    Ok(JsValue::UNDEFINED)
}

fn canvas_stroke_rect(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let x = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let y = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let w = args
        .get(2)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as u32;
    let h = args
        .get(3)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as u32;
    let rgba = canvas_style_color(cx, this, "strokeStyle");
    if let Some(host) = cx.host_as::<CinderDomHost>() {
        host.doc.canvas_stroke_rect(nid, x, y, w, h, 1, rgba);
    }
    Ok(JsValue::UNDEFINED)
}

fn canvas_clear_rect(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let x = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let y = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let w = args
        .get(2)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as u32;
    let h = args
        .get(3)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as u32;
    if let Some(host) = cx.host_as::<CinderDomHost>() {
        host.doc.canvas_clear_rect(nid, x, y, w, h);
    }
    Ok(JsValue::UNDEFINED)
}

fn canvas_fill_text(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let text = arg_str(cx, args, 0)?;
    let x = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let y = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let rgba = canvas_style_color(cx, this, "fillStyle");
    if let Some(host) = cx.host_as::<CinderDomHost>() {
        host.doc.canvas_draw_text_bitmap(nid, &text, x, y, rgba);
    }
    Ok(JsValue::UNDEFINED)
}

fn canvas_begin_path(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    if let Some(nid) = unwrap_node(cx, this)
        && let Some(h) = cx.host_as::<CinderDomHost>()
    {
        h.canvas_paths.insert(nid, Vec::new());
    }
    Ok(JsValue::UNDEFINED)
}

fn canvas_move_to(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let x = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let y = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.canvas_paths.entry(nid).or_default().push((x, y));
    }
    Ok(JsValue::UNDEFINED)
}

fn canvas_line_to(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    canvas_move_to(cx, this, args)
}

fn canvas_rect(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let x = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let y = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let w = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let h = args.get(3).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    if let Some(host) = cx.host_as::<CinderDomHost>() {
        let pts = host.canvas_paths.entry(nid).or_default();
        pts.push((x, y));
        pts.push((x + w, y));
        pts.push((x + w, y + h));
        pts.push((x, y + h));
        pts.push((x, y));
    }
    Ok(JsValue::UNDEFINED)
}

fn canvas_arc(cx: &mut NativeCx<'_>, this: JsValue, args: &[JsValue]) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let cx_pt = args.first().and_then(|v| v.as_number()).unwrap_or(0.0);
    let cy_pt = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0);
    let r = args
        .get(2)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0);
    let a0 = args.get(3).and_then(|v| v.as_number()).unwrap_or(0.0);
    let a1 = args
        .get(4)
        .and_then(|v| v.as_number())
        .unwrap_or(std::f64::consts::TAU);
    if let Some(host) = cx.host_as::<CinderDomHost>() {
        let pts = host.canvas_paths.entry(nid).or_default();
        let steps = 16usize;
        for i in 0..=steps {
            let t = a0 + (a1 - a0) * (i as f64 / steps as f64);
            let px = (cx_pt + r * t.cos()).round() as i32;
            let py = (cy_pt + r * t.sin()).round() as i32;
            pts.push((px, py));
        }
    }
    Ok(JsValue::UNDEFINED)
}

fn canvas_close_path(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    if let Some(nid) = unwrap_node(cx, this)
        && let Some(host) = cx.host_as::<CinderDomHost>()
        && let Some(pts) = host.canvas_paths.get_mut(&nid)
        && let Some(&first) = pts.first()
    {
        pts.push(first);
    }
    Ok(JsValue::UNDEFINED)
}

fn canvas_stroke(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let rgba = canvas_style_color(cx, this, "strokeStyle");
    let lw = if let Some(obj) = this.as_object()
        && let Ok(key) = cx.heap().key_from_str("lineWidth")
        && let Ok(Some(prop)) = cx.heap().get_own(obj, key)
        && let Some(val) = prop.slot.data()
        && let Some(n) = val.as_number()
    {
        n.round().max(1.0) as u32
    } else {
        1
    };
    if let Some(host) = cx.host_as::<CinderDomHost>()
        && let Some(pts) = host.canvas_paths.get(&nid).cloned()
    {
        for win in pts.windows(2) {
            host.doc
                .canvas_draw_line_width(nid, win[0].0, win[0].1, win[1].0, win[1].1, lw, rgba);
        }
    }
    Ok(JsValue::UNDEFINED)
}

fn canvas_fill(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let rgba = canvas_style_color(cx, this, "fillStyle");
    if let Some(host) = cx.host_as::<CinderDomHost>()
        && let Some(pts) = host.canvas_paths.get(&nid).cloned()
        && !pts.is_empty()
    {
        if pts.len() >= 3 {
            let poly: Vec<(f32, f32)> = pts.iter().map(|&(x, y)| (x as f32, y as f32)).collect();
            host.doc.canvas_fill_polygon(nid, &poly, rgba);
        } else {
            let min_x = pts.iter().map(|p| p.0).min().unwrap_or(0);
            let max_x = pts.iter().map(|p| p.0).max().unwrap_or(0);
            let min_y = pts.iter().map(|p| p.1).min().unwrap_or(0);
            let max_y = pts.iter().map(|p| p.1).max().unwrap_or(0);
            let w = (max_x - min_x).max(1) as u32;
            let h = (max_y - min_y).max(1) as u32;
            host.doc.canvas_fill_rect(nid, min_x, min_y, w, h, rgba);
        }
    }
    Ok(JsValue::UNDEFINED)
}

fn canvas_measure_text(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let text = arg_str(cx, args, 0)?;
    let metrics = cx.vm().new_plain_object()?;
    let w = (text.chars().count() as f64) * 6.0;
    cx.vm().define_data(metrics, "width", JsValue::number(w))?;
    Ok(JsValue::object(metrics))
}

fn canvas_create_image_data(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let w = args
        .first()
        .and_then(|v| v.as_number())
        .unwrap_or(1.0)
        .clamp(1.0, 256.0) as u32;
    let h = args
        .get(1)
        .and_then(|v| v.as_number())
        .unwrap_or(1.0)
        .clamp(1.0, 256.0) as u32;
    let len = (w as usize) * (h as usize) * 4;
    let zeros = vec![JsValue::int32(0); len];
    let data_arr = cx.vm().new_array_from(&zeros)?;
    cx.keep(data_arr);
    let obj = cx.vm().new_plain_object()?;
    cx.vm()
        .define_data(obj, "width", JsValue::number(w as f64))?;
    cx.vm()
        .define_data(obj, "height", JsValue::number(h as f64))?;
    cx.vm().define_data(obj, "data", data_arr)?;
    Ok(JsValue::object(obj))
}

fn canvas_get_image_data(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return canvas_create_image_data(cx, this, args);
    };
    let sx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let sy = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let sw = args
        .get(2)
        .and_then(|v| v.as_number())
        .unwrap_or(1.0)
        .clamp(1.0, 256.0) as u32;
    let sh = args
        .get(3)
        .and_then(|v| v.as_number())
        .unwrap_or(1.0)
        .clamp(1.0, 256.0) as u32;
    let mut bytes = Vec::with_capacity((sw as usize) * (sh as usize) * 4);
    if let Some(host) = cx.host_as::<CinderDomHost>() {
        let surf = host.doc.ensure_canvas_surface(nid);
        for ry in 0..(sh as i32) {
            for rx in 0..(sw as i32) {
                let px = sx + rx;
                let py = sy + ry;
                if px >= 0 && px < surf.width as i32 && py >= 0 && py < surf.height as i32 {
                    let off = ((py as usize) * (surf.width as usize) + (px as usize)) * 4;
                    if off + 4 <= surf.rgba.len() {
                        for &b in &surf.rgba[off..off + 4] {
                            bytes.push(JsValue::int32(b as i32));
                        }
                        continue;
                    }
                }
                bytes.extend_from_slice(&[JsValue::int32(0); 4]);
            }
        }
    }
    let data_arr = cx.vm().new_array_from(&bytes)?;
    cx.keep(data_arr);
    let obj = cx.vm().new_plain_object()?;
    cx.vm()
        .define_data(obj, "width", JsValue::number(sw as f64))?;
    cx.vm()
        .define_data(obj, "height", JsValue::number(sh as f64))?;
    cx.vm().define_data(obj, "data", data_arr)?;
    Ok(JsValue::object(obj))
}

fn canvas_put_image_data(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let Some(img_obj) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::UNDEFINED);
    };
    let dx = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let dy = args.get(2).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let w_key = cx
        .heap()
        .key_from_str("width")
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    let h_key = cx
        .heap()
        .key_from_str("height")
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    let d_key = cx
        .heap()
        .key_from_str("data")
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    let sw = cx
        .heap()
        .get_own(img_obj, w_key)
        .ok()
        .flatten()
        .and_then(|p| p.slot.data())
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as u32;
    let sh = cx
        .heap()
        .get_own(img_obj, h_key)
        .ok()
        .flatten()
        .and_then(|p| p.slot.data())
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as u32;
    let data_obj = cx
        .heap()
        .get_own(img_obj, d_key)
        .ok()
        .flatten()
        .and_then(|p| p.slot.data())
        .and_then(|v| v.as_object());
    if sw > 0
        && sh > 0
        && let Some(arr_ref) = data_obj
    {
        let mut rgba = Vec::new();
        if let Ok(obj) = cx.heap().object(arr_ref)
            && let ObjectClass::Array { length } = obj.class
        {
            rgba.reserve(length as usize);
            for i in 0..length {
                let v = obj
                    .elements
                    .get(i)
                    .and_then(|p| p.slot.data())
                    .unwrap_or(JsValue::int32(0));
                let b = v.as_number().unwrap_or(0.0).clamp(0.0, 255.0) as u8;
                rgba.push(b);
            }
        }
        if !rgba.is_empty()
            && let Some(host) = cx.host_as::<CinderDomHost>()
        {
            host.doc.canvas_put_image_data(nid, dx, dy, sw, sh, &rgba);
        }
    }
    Ok(JsValue::UNDEFINED)
}

fn el_noop(_cx: &mut NativeCx<'_>, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, RunError> {
    Ok(JsValue::UNDEFINED)
}

fn window_set_timeout(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(cb) = args.first().copied().filter(|v| v.is_object()) else {
        return Ok(JsValue::int32(0));
    };
    let delay = args
        .get(1)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0);
    let pers = cx
        .heap()
        .persist(cb)
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    let id = if let Some(h) = cx.host_as::<CinderDomHost>() {
        let id = h.next_timer_id;
        h.next_timer_id = h.next_timer_id.wrapping_add(1).max(1);
        let due = h.clock_ms + delay;
        h.timers.push((id, due, None, pers, false));
        id
    } else {
        0
    };
    Ok(JsValue::number(id as f64))
}

fn window_set_interval(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(cb) = args.first().copied().filter(|v| v.is_object()) else {
        return Ok(JsValue::int32(0));
    };
    let interval = args
        .get(1)
        .and_then(|v| v.as_number())
        .unwrap_or(10.0)
        .max(1.0);
    let pers = cx
        .heap()
        .persist(cb)
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    let id = if let Some(h) = cx.host_as::<CinderDomHost>() {
        let id = h.next_timer_id;
        h.next_timer_id = h.next_timer_id.wrapping_add(1).max(1);
        let due = h.clock_ms + interval;
        h.timers.push((id, due, Some(interval), pers, false));
        id
    } else {
        0
    };
    Ok(JsValue::number(id as f64))
}

fn window_request_animation_frame(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(cb) = args.first().copied().filter(|v| v.is_object()) else {
        return Ok(JsValue::int32(0));
    };
    let pers = cx
        .heap()
        .persist(cb)
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    let id = if let Some(h) = cx.host_as::<CinderDomHost>() {
        let id = h.next_timer_id;
        h.next_timer_id = h.next_timer_id.wrapping_add(1).max(1);
        let due = h.clock_ms + 16.0;
        h.timers.push((id, due, None, pers, true));
        id
    } else {
        0
    };
    Ok(JsValue::number(id as f64))
}

fn window_clear_timer(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(id_num) = args.first().and_then(|v| v.as_number()) else {
        return Ok(JsValue::UNDEFINED);
    };
    let target_id = id_num as u32;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.timers.retain(|(id, _, _, _, _)| *id != target_id);
    }
    Ok(JsValue::UNDEFINED)
}

fn window_register_mutation_observer(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(cb) = args.first().copied().filter(|v| v.is_object()) else {
        return Ok(JsValue::UNDEFINED);
    };
    let Some(nid) = args.get(1).copied().and_then(|v| unwrap_node(cx, v)) else {
        return Ok(JsValue::UNDEFINED);
    };
    let pers = cx
        .heap()
        .persist(cb)
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.mutation_observers.push((pers, nid));
    }
    Ok(JsValue::UNDEFINED)
}

fn window_unregister_mutation_observer(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(cb) = args.first().copied().filter(|v| v.is_object()) else {
        return Ok(JsValue::UNDEFINED);
    };
    let Some(existing) = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.mutation_observers.clone())
    else {
        return Ok(JsValue::UNDEFINED);
    };
    let mut kept = Vec::new();
    for (pers, nid) in existing {
        if cx.heap().persistent(pers).ok().and_then(|v| v.as_object()) != cb.as_object() {
            kept.push((pers, nid));
        }
    }
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.mutation_observers = kept;
    }
    Ok(JsValue::UNDEFINED)
}

fn notify_attr_mutation(
    cx: &mut NativeCx<'_>,
    target: NodeId,
    attr_name: &str,
    old_val: Option<String>,
    new_val: Option<String>,
) -> Result<(), RunError> {
    let (matching, has_ce): (Vec<Persistent>, bool) = cx
        .host_as::<CinderDomHost>()
        .map(|h| {
            let m = h
                .mutation_observers
                .iter()
                .filter(|(_, obs_nid)| h.doc.contains_node(*obs_nid, target))
                .map(|(pers, _)| *pers)
                .collect();
            (m, h.has_custom_elements)
        })
        .unwrap_or_default();
    if matching.is_empty() && !has_ce {
        return Ok(());
    }
    let target_val = wrap_node(cx, target)?;
    cx.keep(target_val);
    if !matching.is_empty() {
        let rec = cx.vm().new_plain_object()?;
        let mtype_val = cx.vm().string("attributes")?;
        cx.vm().define_data(rec, "type", mtype_val)?;
        cx.vm().define_data(rec, "target", target_val)?;
        let empty_added = cx.vm().new_array_from(&[])?;
        cx.keep(empty_added);
        let empty_removed = cx.vm().new_array_from(&[])?;
        cx.keep(empty_removed);
        cx.vm().define_data(rec, "addedNodes", empty_added)?;
        cx.vm().define_data(rec, "removedNodes", empty_removed)?;
        cx.vm().define_data(rec, "previousSibling", JsValue::NULL)?;
        cx.vm().define_data(rec, "nextSibling", JsValue::NULL)?;
        let attr_val = cx.vm().string(attr_name)?;
        cx.vm().define_data(rec, "attributeName", attr_val)?;
        cx.vm()
            .define_data(rec, "attributeNamespace", JsValue::NULL)?;
        let old_js = match old_val.as_deref() {
            Some(s) => cx.vm().string(s)?,
            None => JsValue::NULL,
        };
        cx.vm().define_data(rec, "oldValue", old_js)?;
        let rec_val = JsValue::object(rec);
        cx.keep(rec_val);
        for pers in matching {
            if let Ok(cb_val) = cx.heap().persistent(pers) {
                let _ = cx.call(cb_val, JsValue::UNDEFINED, &[rec_val]);
            }
        }
    }
    if has_ce {
        let g = cx.vm().global();
        if let Ok(key) = cx.heap().key_from_str("__onCustomElementAttributeChanged")
            && let Ok(Some(prop)) = cx.heap().get_own(g, key)
            && let Some(fn_val) = prop.slot.data()
            && fn_val.as_object().is_some()
        {
            let name_js = cx.vm().string(attr_name)?;
            cx.keep(name_js);
            let old_js = match old_val.as_deref() {
                Some(s) => cx.vm().string(s)?,
                None => JsValue::NULL,
            };
            cx.keep(old_js);
            let new_js = match new_val.as_deref() {
                Some(s) => cx.vm().string(s)?,
                None => JsValue::NULL,
            };
            cx.keep(new_js);
            let _ = cx.call(
                fn_val,
                JsValue::object(g),
                &[target_val, name_js, old_js, new_js],
            );
        }
    }
    Ok(())
}

fn notify_child_mutation(
    cx: &mut NativeCx<'_>,
    target: NodeId,
    added: &[NodeId],
    removed: &[NodeId],
    prev_sib: Option<NodeId>,
    next_sib: Option<NodeId>,
) -> Result<(), RunError> {
    let (matching, has_ce): (Vec<Persistent>, bool) = cx
        .host_as::<CinderDomHost>()
        .map(|h| {
            let m = h
                .mutation_observers
                .iter()
                .filter(|(_, obs_nid)| h.doc.contains_node(*obs_nid, target))
                .map(|(pers, _)| *pers)
                .collect();
            (m, h.has_custom_elements)
        })
        .unwrap_or_default();
    if matching.is_empty() && !has_ce {
        return Ok(());
    }
    let target_val = wrap_node(cx, target)?;
    cx.keep(target_val);
    let added_arr = wrap_node_list(cx, added.to_vec())?;
    cx.keep(added_arr);
    let removed_arr = wrap_node_list(cx, removed.to_vec())?;
    cx.keep(removed_arr);
    if !matching.is_empty() {
        let prev_val = wrap_optional_node(cx, prev_sib)?;
        cx.keep(prev_val);
        let next_val = wrap_optional_node(cx, next_sib)?;
        cx.keep(next_val);
        let rec = cx.vm().new_plain_object()?;
        let mtype_val = cx.vm().string("childList")?;
        cx.vm().define_data(rec, "type", mtype_val)?;
        cx.vm().define_data(rec, "target", target_val)?;
        cx.vm().define_data(rec, "addedNodes", added_arr)?;
        cx.vm().define_data(rec, "removedNodes", removed_arr)?;
        cx.vm().define_data(rec, "previousSibling", prev_val)?;
        cx.vm().define_data(rec, "nextSibling", next_val)?;
        cx.vm().define_data(rec, "attributeName", JsValue::NULL)?;
        cx.vm()
            .define_data(rec, "attributeNamespace", JsValue::NULL)?;
        cx.vm().define_data(rec, "oldValue", JsValue::NULL)?;
        let rec_val = JsValue::object(rec);
        cx.keep(rec_val);
        for pers in matching {
            if let Ok(cb_val) = cx.heap().persistent(pers) {
                let _ = cx.call(cb_val, JsValue::UNDEFINED, &[rec_val]);
            }
        }
    }
    if has_ce {
        let g = cx.vm().global();
        if let Ok(key) = cx.heap().key_from_str("__onCustomElementChildListChanged")
            && let Ok(Some(prop)) = cx.heap().get_own(g, key)
            && let Some(fn_val) = prop.slot.data()
            && fn_val.as_object().is_some()
        {
            let _ = cx.call(
                fn_val,
                JsValue::object(g),
                &[target_val, added_arr, removed_arr],
            );
        }
    }
    Ok(())
}

fn notify_char_data_mutation(
    cx: &mut NativeCx<'_>,
    target: NodeId,
    old_val: Option<String>,
) -> Result<(), RunError> {
    let matching: Vec<Persistent> = cx
        .host_as::<CinderDomHost>()
        .map(|h| {
            h.mutation_observers
                .iter()
                .filter(|(_, obs_nid)| h.doc.contains_node(*obs_nid, target))
                .map(|(pers, _)| *pers)
                .collect()
        })
        .unwrap_or_default();
    if matching.is_empty() {
        return Ok(());
    }
    let target_val = wrap_node(cx, target)?;
    cx.keep(target_val);
    let rec = cx.vm().new_plain_object()?;
    let mtype_val = cx.vm().string("characterData")?;
    cx.vm().define_data(rec, "type", mtype_val)?;
    cx.vm().define_data(rec, "target", target_val)?;
    let empty_added = cx.vm().new_array_from(&[])?;
    cx.keep(empty_added);
    let empty_removed = cx.vm().new_array_from(&[])?;
    cx.keep(empty_removed);
    cx.vm().define_data(rec, "addedNodes", empty_added)?;
    cx.vm().define_data(rec, "removedNodes", empty_removed)?;
    cx.vm().define_data(rec, "previousSibling", JsValue::NULL)?;
    cx.vm().define_data(rec, "nextSibling", JsValue::NULL)?;
    cx.vm().define_data(rec, "attributeName", JsValue::NULL)?;
    cx.vm()
        .define_data(rec, "attributeNamespace", JsValue::NULL)?;
    let old_js = match old_val.as_deref() {
        Some(s) => cx.vm().string(s)?,
        None => JsValue::NULL,
    };
    cx.vm().define_data(rec, "oldValue", old_js)?;
    let rec_val = JsValue::object(rec);
    cx.keep(rec_val);
    for pers in matching {
        if let Ok(cb_val) = cx.heap().persistent(pers) {
            let _ = cx.call(cb_val, JsValue::UNDEFINED, &[rec_val]);
        }
    }
    Ok(())
}

fn el_get_id(cx: &mut NativeCx<'_>, this: JsValue, _args: &[JsValue]) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().string("");
    };
    let id = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).data().id().map(str::to_string))
        .unwrap_or_default();
    cx.vm().string(&id)
}

fn el_set_id(cx: &mut NativeCx<'_>, this: JsValue, args: &[JsValue]) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let val = arg_str(cx, args, 0)?;
    let old_val = cx.host_as::<CinderDomHost>().and_then(|h| {
        let prev = h.doc.get(nid).data().attr("id").map(str::to_string);
        h.doc.set_attribute(nid, "id", &val);
        prev
    });
    notify_attr_mutation(cx, nid, "id", old_val, Some(val))?;
    Ok(JsValue::UNDEFINED)
}

fn el_get_class_name(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().string("");
    };
    let cls = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).data().attr("class").map(str::to_string))
        .unwrap_or_default();
    cx.vm().string(&cls)
}

fn el_set_class_name(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let val = arg_str(cx, args, 0)?;
    let old_val = cx.host_as::<CinderDomHost>().and_then(|h| {
        let prev = h.doc.get(nid).data().attr("class").map(str::to_string);
        h.doc.set_attribute(nid, "class", &val);
        prev
    });
    notify_attr_mutation(cx, nid, "class", old_val, Some(val))?;
    Ok(JsValue::UNDEFINED)
}

fn el_get_text_content(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().string("");
    };
    let text = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.text_content(nid))
        .unwrap_or_default();
    cx.vm().string(&text)
}

fn el_set_text_content(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let text = arg_str(cx, args, 0)?;
    let mut is_text_node = false;
    let mut old_text = None;
    let mut removed = Vec::new();
    let mut added = Vec::new();
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        if let NodeData::Text(t) = h.doc.get(nid).data() {
            is_text_node = true;
            old_text = Some(t.clone());
            h.doc.set_text_content(nid, &text);
        } else {
            removed = h
                .doc
                .children(nid)
                .iter()
                .copied()
                .filter(|&cid| h.doc.shadow_host(cid).is_none())
                .collect();
            h.doc.set_text_content(nid, &text);
            added = h
                .doc
                .children(nid)
                .iter()
                .copied()
                .filter(|&cid| h.doc.shadow_host(cid).is_none())
                .collect();
        }
    }
    if is_text_node {
        notify_char_data_mutation(cx, nid, old_text)?;
    } else if !added.is_empty() || !removed.is_empty() {
        notify_child_mutation(cx, nid, &added, &removed, None, None)?;
    }
    Ok(JsValue::UNDEFINED)
}

fn el_get_text_content_method(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    el_get_text_content(cx, this, args)
}

fn el_set_text_content_method(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    el_set_text_content(cx, this, args)
}

fn el_get_inner_html(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().string("");
    };
    let html = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.inner_html(nid))
        .unwrap_or_default();
    cx.vm().string(&html)
}

fn el_set_inner_html(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let html = arg_str(cx, args, 0)?;
    let mut removed = Vec::new();
    let mut added = Vec::new();
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        removed = h
            .doc
            .children(nid)
            .iter()
            .copied()
            .filter(|&cid| h.doc.shadow_host(cid).is_none())
            .collect();
        h.doc.set_inner_html(nid, &html);
        added = h
            .doc
            .children(nid)
            .iter()
            .copied()
            .filter(|&cid| h.doc.shadow_host(cid).is_none())
            .collect();
    }
    if !added.is_empty() || !removed.is_empty() {
        notify_child_mutation(cx, nid, &added, &removed, None, None)?;
    }
    Ok(JsValue::UNDEFINED)
}

fn el_set_inner_html_method(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    el_set_inner_html(cx, this, args)
}

fn el_get_outer_html(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().string("");
    };
    let html = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.outer_html(nid))
        .unwrap_or_default();
    cx.vm().string(&html)
}

fn el_get_value(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().string("");
    };
    let val = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.input_value(nid))
        .unwrap_or_default();
    cx.vm().string(&val)
}

fn el_set_value(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let val = arg_str(cx, args, 0)?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.doc.set_input_value(nid, &val);
    }
    Ok(JsValue::UNDEFINED)
}

fn el_set_value_method(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    el_set_value(cx, this, args)
}

fn el_get_checked(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::FALSE);
    };
    let checked = cx
        .host_as::<CinderDomHost>()
        .is_some_and(|h| h.doc.is_checked(nid));
    Ok(JsValue::boolean(checked))
}

fn el_set_checked(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let checked = args
        .first()
        .copied()
        .is_some_and(|v| v.as_bool() == Some(true) || v.as_number().is_some_and(|n| n != 0.0));
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.doc.set_checked(nid, checked);
    }
    Ok(JsValue::UNDEFINED)
}

fn el_get_tag_name(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().string("");
    };
    let tag = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).data().tag().map(str::to_ascii_uppercase))
        .unwrap_or_default();
    cx.vm().string(&tag)
}

fn el_get_parent_element(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let parent = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.parent_element(nid));
    wrap_optional_node(cx, parent)
}

fn el_get_first_element_child(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let first = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.first_element_child(nid));
    wrap_optional_node(cx, first)
}

fn el_get_last_element_child(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let last = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.last_element_child(nid));
    wrap_optional_node(cx, last)
}

fn el_get_first_child(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let first = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.first_child(nid));
    wrap_optional_node(cx, first)
}

fn el_get_last_child(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let last = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.last_child(nid));
    wrap_optional_node(cx, last)
}

fn el_get_next_element_sibling(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let next = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.next_element_sibling(nid));
    wrap_optional_node(cx, next)
}

fn el_get_previous_element_sibling(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let prev = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.previous_element_sibling(nid));
    wrap_optional_node(cx, prev)
}

fn el_get_children(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().new_array_from(&[]);
    };
    let kids = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.element_children(nid))
        .unwrap_or_default();
    wrap_node_list(cx, kids)
}

fn el_get_child_element_count(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::number(0.0));
    };
    let count = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.element_children(nid).len())
        .unwrap_or(0);
    Ok(JsValue::number(count as f64))
}

fn el_get_shadow_root(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let sr = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.shadow_root(nid));
    wrap_optional_node(cx, sr)
}

fn el_get_class_list(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let data = HostData(TAG_CLASS_LIST | (nid.index() as u64));
    if let Some(existing) = cx.heap().wrapper(data) {
        return Ok(JsValue::object(existing));
    }
    let proto = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.class_list_proto);
    let obj = cx.vm().new_host_object(proto, data)?;
    cx.heap().set_wrapper(data, obj);
    Ok(JsValue::object(obj))
}

fn el_get_style(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let data = HostData(TAG_STYLE | (nid.index() as u64));
    if let Some(existing) = cx.heap().wrapper(data) {
        return Ok(JsValue::object(existing));
    }
    let proto = cx.host_as::<CinderDomHost>().and_then(|h| h.style_proto);
    let obj = cx.vm().new_host_object(proto, data)?;
    cx.heap().set_wrapper(data, obj);
    Ok(JsValue::object(obj))
}

fn el_get_dataset(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let data = HostData(TAG_DATASET | (nid.index() as u64));
    if let Some(existing) = cx.heap().wrapper(data) {
        return Ok(JsValue::object(existing));
    }
    let proto = cx.host_as::<CinderDomHost>().and_then(|h| h.dataset_proto);
    let obj = cx.vm().new_host_object(proto, data)?;
    cx.heap().set_wrapper(data, obj);
    Ok(JsValue::object(obj))
}

// ---- classList methods ----------------------------------------------------

fn cl_add(cx: &mut NativeCx<'_>, this: JsValue, args: &[JsValue]) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let old_val = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).data().attr("class").map(str::to_string));
    for i in 0..args.len() {
        let token = arg_str(cx, args, i)?;
        if let Some(h) = cx.host_as::<CinderDomHost>() {
            h.doc.add_class(nid, token.trim());
        }
    }
    let new_val = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).data().attr("class").map(str::to_string));
    notify_attr_mutation(cx, nid, "class", old_val, new_val)?;
    Ok(JsValue::UNDEFINED)
}

fn cl_remove(cx: &mut NativeCx<'_>, this: JsValue, args: &[JsValue]) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let old_val = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).data().attr("class").map(str::to_string));
    for i in 0..args.len() {
        let token = arg_str(cx, args, i)?;
        if let Some(h) = cx.host_as::<CinderDomHost>() {
            h.doc.remove_class(nid, token.trim());
        }
    }
    let new_val = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).data().attr("class").map(str::to_string));
    notify_attr_mutation(cx, nid, "class", old_val, new_val)?;
    Ok(JsValue::UNDEFINED)
}

fn cl_toggle(cx: &mut NativeCx<'_>, this: JsValue, args: &[JsValue]) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::FALSE);
    };
    let token = arg_str(cx, args, 0)?;
    let old_val = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).data().attr("class").map(str::to_string));
    let present = if let Some(force) = args.get(1).copied()
        && !force.is_undefined()
    {
        let on = force.as_bool() == Some(true) || force.as_number().is_some_and(|n| n != 0.0);
        if let Some(h) = cx.host_as::<CinderDomHost>() {
            if on {
                h.doc.add_class(nid, token.trim());
            } else {
                h.doc.remove_class(nid, token.trim());
            }
        }
        on
    } else {
        cx.host_as::<CinderDomHost>()
            .is_some_and(|h| h.doc.toggle_class(nid, token.trim()))
    };
    let new_val = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).data().attr("class").map(str::to_string));
    notify_attr_mutation(cx, nid, "class", old_val, new_val)?;
    Ok(JsValue::boolean(present))
}

fn cl_contains(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::FALSE);
    };
    let token = arg_str(cx, args, 0)?;
    let has = cx
        .host_as::<CinderDomHost>()
        .is_some_and(|h| h.doc.get(nid).data().has_class(token.trim()));
    Ok(JsValue::boolean(has))
}

fn cl_to_string(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    el_get_class_name(cx, this, args)
}

// ---- style methods --------------------------------------------------------

fn style_set_property(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let raw_prop = arg_str(cx, args, 0)?;
    let css_prop = camel_to_kebab(raw_prop.trim());
    let val = arg_str(cx, args, 1)?;
    let (old_val, new_val) = cx
        .host_as::<CinderDomHost>()
        .map(|h| {
            let prev = h.doc.get(nid).data().attr("style").map(str::to_string);
            if val.trim().is_empty() {
                remove_inline_style_prop(&mut h.doc, nid, &css_prop);
            } else {
                update_inline_style(&mut h.doc, nid, &css_prop, val.trim());
            }
            let next = h.doc.get(nid).data().attr("style").map(str::to_string);
            (prev, next)
        })
        .unwrap_or((None, None));
    notify_attr_mutation(cx, nid, "style", old_val, new_val)?;
    Ok(JsValue::UNDEFINED)
}

fn style_get_property_value(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().string("");
    };
    let raw_prop = arg_str(cx, args, 0)?;
    let css_prop = camel_to_kebab(raw_prop.trim());
    let val = cx
        .host_as::<CinderDomHost>()
        .map(|h| read_inline_style(&h.doc, nid, &css_prop))
        .unwrap_or_default();
    cx.vm().string(&val)
}

fn style_remove_property(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().string("");
    };
    let raw_prop = arg_str(cx, args, 0)?;
    let css_prop = camel_to_kebab(raw_prop.trim());
    let (prev, old_attr, new_attr) = cx
        .host_as::<CinderDomHost>()
        .map(|h| {
            let prev_attr = h.doc.get(nid).data().attr("style").map(str::to_string);
            let old = read_inline_style(&h.doc, nid, &css_prop);
            remove_inline_style_prop(&mut h.doc, nid, &css_prop);
            let next_attr = h.doc.get(nid).data().attr("style").map(str::to_string);
            (old, prev_attr, next_attr)
        })
        .unwrap_or_default();
    notify_attr_mutation(cx, nid, "style", old_attr, new_attr)?;
    cx.vm().string(&prev)
}

fn style_get_css_text(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().string("");
    };
    let css = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).data().attr("style").map(str::to_string))
        .unwrap_or_default();
    cx.vm().string(&css)
}

fn style_set_css_text(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let css = arg_str(cx, args, 0)?;
    let old_val = cx.host_as::<CinderDomHost>().and_then(|h| {
        let prev = h.doc.get(nid).data().attr("style").map(str::to_string);
        h.doc.set_attribute(nid, "style", &css);
        prev
    });
    notify_attr_mutation(cx, nid, "style", old_val, Some(css))?;
    Ok(JsValue::UNDEFINED)
}

// ---- dataset methods ------------------------------------------------------

fn dataset_get(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let key = arg_str(cx, args, 0)?;
    let attr_name = format!("data-{}", camel_to_kebab(key.trim()));
    let val = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).data().attr(&attr_name).map(str::to_string));
    match val {
        Some(s) => cx.vm().string(&s),
        None => Ok(JsValue::UNDEFINED),
    }
}

fn dataset_set(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let key = arg_str(cx, args, 0)?;
    let val = arg_str(cx, args, 1)?;
    let attr_name = format!("data-{}", camel_to_kebab(key.trim()));
    let old_val = cx.host_as::<CinderDomHost>().and_then(|h| {
        let prev = h.doc.get(nid).data().attr(&attr_name).map(str::to_string);
        h.doc.set_attribute(nid, &attr_name, &val);
        prev
    });
    notify_attr_mutation(cx, nid, &attr_name, old_val, Some(val))?;
    Ok(JsValue::UNDEFINED)
}

// ---- Storage methods ------------------------------------------------------

fn local_storage_get_item(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let key = arg_str(cx, args, 0)?;
    let val = cx.host_as::<CinderDomHost>().and_then(|h| {
        h.local_storage
            .iter()
            .find(|(k, _)| k == &key)
            .map(|(_, v)| v.clone())
    });
    match val {
        Some(s) => cx.vm().string(&s),
        None => Ok(JsValue::NULL),
    }
}

fn local_storage_set_item(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let key = arg_str(cx, args, 0)?;
    let val = arg_str(cx, args, 1)?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        if let Some((_, existing)) = h.local_storage.iter_mut().find(|(k, _)| k == &key) {
            *existing = val;
        } else {
            h.local_storage.push((key, val));
        }
    }
    Ok(JsValue::UNDEFINED)
}

fn local_storage_remove_item(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let key = arg_str(cx, args, 0)?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.local_storage.retain(|(k, _)| k != &key);
    }
    Ok(JsValue::UNDEFINED)
}

fn local_storage_clear(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.local_storage.clear();
    }
    Ok(JsValue::UNDEFINED)
}

fn local_storage_key(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let key = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.local_storage.get(idx).map(|(k, _)| k.clone()));
    match key {
        Some(s) => cx.vm().string(&s),
        None => Ok(JsValue::NULL),
    }
}

fn local_storage_length(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let len = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.local_storage.len())
        .unwrap_or(0);
    Ok(JsValue::number(len as f64))
}

fn session_storage_get_item(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let key = arg_str(cx, args, 0)?;
    let val = cx.host_as::<CinderDomHost>().and_then(|h| {
        h.session_storage
            .iter()
            .find(|(k, _)| k == &key)
            .map(|(_, v)| v.clone())
    });
    match val {
        Some(s) => cx.vm().string(&s),
        None => Ok(JsValue::NULL),
    }
}

fn session_storage_set_item(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let key = arg_str(cx, args, 0)?;
    let val = arg_str(cx, args, 1)?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        if let Some((_, existing)) = h.session_storage.iter_mut().find(|(k, _)| k == &key) {
            *existing = val;
        } else {
            h.session_storage.push((key, val));
        }
    }
    Ok(JsValue::UNDEFINED)
}

fn session_storage_remove_item(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let key = arg_str(cx, args, 0)?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.session_storage.retain(|(k, _)| k != &key);
    }
    Ok(JsValue::UNDEFINED)
}

fn session_storage_clear(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.session_storage.clear();
    }
    Ok(JsValue::UNDEFINED)
}

fn session_storage_key(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
    let key = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.session_storage.get(idx).map(|(k, _)| k.clone()));
    match key {
        Some(s) => cx.vm().string(&s),
        None => Ok(JsValue::NULL),
    }
}

fn session_storage_length(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let len = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.session_storage.len())
        .unwrap_or(0);
    Ok(JsValue::number(len as f64))
}

// ---- Location, History, CustomElements & Window helpers -------------------

fn loc_get_href(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let href = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.location_href.clone())
        .unwrap_or_default();
    cx.vm().string(&href)
}

fn loc_set_href(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let next = arg_str(cx, args, 0)?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        let resolved = crate::resolve_script_fetch_url(&h.location_href, &next);
        h.location_href = resolved.clone();
        h.pending_navigation = Some(resolved);
    }
    Ok(JsValue::UNDEFINED)
}

fn loc_assign(cx: &mut NativeCx<'_>, this: JsValue, args: &[JsValue]) -> Result<JsValue, RunError> {
    loc_set_href(cx, this, args)
}

fn loc_get_origin(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let href = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.location_href.clone())
        .unwrap_or_default();
    let origin = if let Some((scheme, host, port)) = crate::extract_origin_tuple(&href) {
        if (scheme == "https" && port == 443) || (scheme == "http" && port == 80) {
            format!("{scheme}://{host}")
        } else {
            format!("{scheme}://{host}:{port}")
        }
    } else {
        href
    };
    cx.vm().string(&origin)
}

fn loc_get_pathname(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let href = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.location_href.clone())
        .unwrap_or_default();
    let after_scheme = href
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(href.as_str());
    let before_q = after_scheme.split(['?', '#']).next().unwrap_or("/");
    let path = before_q.find('/').map(|i| &before_q[i..]).unwrap_or("/");
    cx.vm().string(path)
}

fn loc_get_search(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let href = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.location_href.clone())
        .unwrap_or_default();
    let before_hash = href.split('#').next().unwrap_or("");
    let search = before_hash
        .split_once('?')
        .map(|(_, q)| format!("?{q}"))
        .unwrap_or_default();
    cx.vm().string(&search)
}

fn loc_get_hash(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let href = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.location_href.clone())
        .unwrap_or_default();
    let hash = href
        .split_once('#')
        .map(|(_, h)| format!("#{h}"))
        .unwrap_or_default();
    cx.vm().string(&hash)
}

fn hist_push_state(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    if args.len() >= 3 && !args[2].is_nullish() {
        let next = arg_str(cx, args, 2)?;
        if !next.trim().is_empty()
            && let Some(h) = cx.host_as::<CinderDomHost>()
        {
            h.location_href = crate::resolve_script_fetch_url(&h.location_href, &next);
        }
    }
    Ok(JsValue::UNDEFINED)
}

fn custom_elements_define(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let name = arg_str(cx, args, 0)?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.has_custom_elements = true;
        h.doc.define_custom_element(&name);
    }
    Ok(JsValue::UNDEFINED)
}

fn custom_elements_get(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let name = arg_str(cx, args, 0)?;
    let defined = cx
        .host_as::<CinderDomHost>()
        .is_some_and(|h| h.doc.is_custom_element_defined(&name));
    if defined {
        cx.vm().string(&name)
    } else {
        Ok(JsValue::UNDEFINED)
    }
}

fn window_match_media(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let q = arg_str(cx, args, 0)?;
    let mql = cx.vm().new_plain_object()?;
    let qs = cx.vm().string(&q)?;
    cx.vm().define_data(mql, "matches", JsValue::FALSE)?;
    cx.vm().define_data(mql, "media", qs)?;
    Ok(JsValue::object(mql))
}

fn format_css_color(c: anvil_style::Color) -> String {
    if c.a == 255 {
        format!("rgb({}, {}, {})", c.r, c.g, c.b)
    } else if c.a == 0 {
        "rgba(0, 0, 0, 0)".to_string()
    } else {
        let alpha = (c.a as f32 / 255.0 * 100.0).round() / 100.0;
        format!("rgba({}, {}, {}, {alpha})", c.r, c.g, c.b)
    }
}

fn format_css_px(px: f32) -> String {
    if px.fract().abs() < 1e-4 {
        format!("{}px", px.round() as i64)
    } else {
        format!("{px:.2}px")
    }
}

fn window_get_computed_style(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let obj = cx.vm().new_plain_object()?;
    let pseudo_raw = if args.len() >= 2 && !args[1].is_nullish() {
        arg_str(cx, args, 1).unwrap_or_default()
    } else {
        String::new()
    };
    let pseudo_norm = pseudo_raw.trim().to_ascii_lowercase();
    if let Some(nid) = args.first().copied().and_then(|v| unwrap_node(cx, v)) {
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut inline = String::new();
        if let Some(h) = cx.host_as::<CinderDomHost>() {
            inline = h
                .doc
                .get(nid)
                .data()
                .attr("style")
                .map(str::to_string)
                .unwrap_or_default();
            let styles = h.compute_styles();
            let layout = anvil_layout::layout_document(&h.doc, &styles, h.viewport_width);
            let box_rect = layout.find_node_box(nid).map(|b| b.rect);
            let (cs, pseudo_content) = match pseudo_norm.as_str() {
                "::before" | ":before" => {
                    if let Some((txt, ps)) = styles.before_style(nid) {
                        (ps.clone(), Some(format!("\"{txt}\"")))
                    } else {
                        (styles.get(nid).clone(), Some("none".to_string()))
                    }
                }
                "::after" | ":after" => {
                    if let Some((txt, ps)) = styles.after_style(nid) {
                        (ps.clone(), Some(format!("\"{txt}\"")))
                    } else {
                        (styles.get(nid).clone(), Some("none".to_string()))
                    }
                }
                _ => (styles.get(nid).clone(), None),
            };
            let display_str = match cs.display {
                anvil_style::Display::Block => "block",
                anvil_style::Display::Inline => "inline",
                anvil_style::Display::Flex => "flex",
                anvil_style::Display::Grid => "grid",
                anvil_style::Display::None => "none",
            };
            let position_str = match cs.position {
                anvil_style::Position::Static => "static",
                anvil_style::Position::Relative => "relative",
                anvil_style::Position::Absolute => "absolute",
                anvil_style::Position::Fixed => "fixed",
                anvil_style::Position::Sticky => "sticky",
            };
            let visibility_str = match cs.visibility {
                anvil_style::Visibility::Visible => "visible",
                anvil_style::Visibility::Hidden => "hidden",
                anvil_style::Visibility::Collapse => "collapse",
            };
            let overflow_str = if cs.scrollable_overflow {
                "auto"
            } else {
                match cs.overflow {
                    anvil_style::Overflow::Visible => "visible",
                    anvil_style::Overflow::Hidden => "hidden",
                }
            };
            let font_family_str = match cs.font_family {
                anvil_style::FontFamily::SansSerif => "sans-serif",
                anvil_style::FontFamily::Serif => "serif",
                anvil_style::FontFamily::Monospace => "monospace",
                anvil_style::FontFamily::Custom(_) => "custom",
            };
            let text_transform_str = match cs.text_transform {
                anvil_style::TextTransform::None => "none",
                anvil_style::TextTransform::Uppercase => "uppercase",
                anvil_style::TextTransform::Lowercase => "lowercase",
                anvil_style::TextTransform::Capitalize => "capitalize",
            };
            let object_fit_str = match cs.object_fit {
                anvil_style::ObjectFit::Fill => "fill",
                anvil_style::ObjectFit::Contain => "contain",
                anvil_style::ObjectFit::Cover => "cover",
                anvil_style::ObjectFit::None => "none",
                anvil_style::ObjectFit::ScaleDown => "scale-down",
            };
            let width_px = box_rect.map(|r| r.width).unwrap_or(match cs.width {
                anvil_style::Size::Px(v) => v,
                _ => 0.0,
            });
            let height_px = box_rect.map(|r| r.height).unwrap_or(match cs.height {
                anvil_style::Size::Px(v) => v,
                _ => 0.0,
            });
            pairs.push(("display".to_string(), display_str.to_string()));
            pairs.push(("position".to_string(), position_str.to_string()));
            pairs.push(("visibility".to_string(), visibility_str.to_string()));
            pairs.push(("overflow".to_string(), overflow_str.to_string()));
            pairs.push(("color".to_string(), format_css_color(cs.color)));
            pairs.push((
                "background-color".to_string(),
                cs.background
                    .map(format_css_color)
                    .unwrap_or_else(|| "rgba(0, 0, 0, 0)".to_string()),
            ));
            pairs.push(("width".to_string(), format_css_px(width_px)));
            pairs.push(("height".to_string(), format_css_px(height_px)));
            pairs.push(("font-size".to_string(), format_css_px(cs.font_size)));
            pairs.push(("font-weight".to_string(), cs.font_weight.to_string()));
            pairs.push((
                "font-style".to_string(),
                if cs.italic { "italic" } else { "normal" }.to_string(),
            ));
            pairs.push(("font-family".to_string(), font_family_str.to_string()));
            pairs.push(("text-transform".to_string(), text_transform_str.to_string()));
            pairs.push((
                "letter-spacing".to_string(),
                format_css_px(cs.letter_spacing),
            ));
            pairs.push(("word-spacing".to_string(), format_css_px(cs.word_spacing)));
            pairs.push(("opacity".to_string(), cs.opacity.to_string()));
            pairs.push(("z-index".to_string(), cs.z_index.to_string()));
            pairs.push(("object-fit".to_string(), object_fit_str.to_string()));
            pairs.push(("margin-top".to_string(), format_css_px(cs.margin.top)));
            pairs.push(("margin-right".to_string(), format_css_px(cs.margin.right)));
            pairs.push(("margin-bottom".to_string(), format_css_px(cs.margin.bottom)));
            pairs.push(("margin-left".to_string(), format_css_px(cs.margin.left)));
            pairs.push(("padding-top".to_string(), format_css_px(cs.padding.top)));
            pairs.push(("padding-right".to_string(), format_css_px(cs.padding.right)));
            pairs.push((
                "padding-bottom".to_string(),
                format_css_px(cs.padding.bottom),
            ));
            pairs.push(("padding-left".to_string(), format_css_px(cs.padding.left)));
            pairs.push(("border-top-width".to_string(), format_css_px(cs.border.top)));
            pairs.push((
                "border-right-width".to_string(),
                format_css_px(cs.border.right),
            ));
            pairs.push((
                "border-bottom-width".to_string(),
                format_css_px(cs.border.bottom),
            ));
            pairs.push((
                "border-left-width".to_string(),
                format_css_px(cs.border.left),
            ));
            if let Some(cnt) = pseudo_content {
                pairs.push(("content".to_string(), cnt));
            }
        }
        for (k, v) in pairs {
            let vs = cx.vm().string(&v)?;
            cx.vm().define_data(obj, &k, vs)?;
            let camel = kebab_to_camel(&k);
            cx.vm().define_data(obj, &camel, vs)?;
        }
        for part in inline.split(';') {
            if let Some((k, v)) = part.split_once(':') {
                let k_clean = k.trim().to_ascii_lowercase();
                let v_clean = v.trim().to_string();
                if !k_clean.is_empty() {
                    let vs = cx.vm().string(&v_clean)?;
                    cx.vm().define_data(obj, &k_clean, vs)?;
                    let camel = kebab_to_camel(&k_clean);
                    cx.vm().define_data(obj, &camel, vs)?;
                }
            }
        }
    }
    cx.vm()
        .define_method(obj, "getPropertyValue", computed_style_get_prop)?;
    Ok(JsValue::object(obj))
}

fn computed_style_get_prop(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(obj) = this.as_object() else {
        return cx.vm().string("");
    };
    let name = arg_str(cx, args, 0)?;
    let key = cx
        .heap()
        .key_from_str(name.trim())
        .map_err(|e| RunError::internal(format!("{e:?}")))?;
    if let Ok(Some(prop)) = cx.heap().get_own(obj, key)
        && let Some(val) = prop.slot.data()
    {
        return Ok(val);
    }
    cx.vm().string("")
}

fn el_get_attribute_names(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().new_array_from(&[]);
    };
    let names: Vec<String> = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| {
            if let NodeData::Element { attrs, .. } = h.doc.get(nid).data() {
                Some(attrs.iter().map(|(k, _)| k.clone()).collect())
            } else {
                None
            }
        })
        .unwrap_or_default();
    let mut elems = Vec::with_capacity(names.len());
    for n in names {
        let s = cx.vm().string(&n)?;
        cx.keep(s);
        elems.push(s);
    }
    cx.vm().new_array_from(&elems)
}

fn el_has_attributes(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::FALSE);
    };
    let has = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| {
            if let NodeData::Element { attrs, .. } = h.doc.get(nid).data() {
                Some(!attrs.is_empty())
            } else {
                None
            }
        })
        .unwrap_or(false);
    Ok(JsValue::boolean(has))
}

fn el_get_node_name(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return cx.vm().string("");
    };
    let name = cx
        .host_as::<CinderDomHost>()
        .map(|h| match h.doc.get(nid).data() {
            NodeData::Document => "#document".to_string(),
            NodeData::Element { tag, .. } => tag.to_ascii_uppercase(),
            NodeData::Text(_) => "#text".to_string(),
        })
        .unwrap_or_default();
    cx.vm().string(&name)
}

fn el_get_node_type(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::number(1.0));
    };
    let nt = cx
        .host_as::<CinderDomHost>()
        .map(|h| match h.doc.get(nid).data() {
            NodeData::Element { .. } => 1.0,
            NodeData::Text(_) => 3.0,
            NodeData::Document => 9.0,
        })
        .unwrap_or(1.0);
    Ok(JsValue::number(nt))
}

fn el_get_node_value(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let val = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| match h.doc.get(nid).data() {
            NodeData::Text(t) => Some(t.clone()),
            _ => None,
        });
    match val {
        Some(s) => cx.vm().string(&s),
        None => Ok(JsValue::NULL),
    }
}

fn el_set_node_value(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let text = arg_str(cx, args, 0)?;
    let mut old_val = None;
    let mut is_text = false;
    if let Some(host) = cx.host_as::<CinderDomHost>() {
        if let NodeData::Text(t) = host.doc.get(nid).data() {
            is_text = true;
            old_val = Some(t.clone());
            host.doc.set_text_content(nid, &text);
        }
    }
    if is_text {
        notify_char_data_mutation(cx, nid, old_val)?;
    }
    Ok(JsValue::UNDEFINED)
}

fn el_get_parent_node(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let parent = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.get(nid).parent());
    match parent {
        Some(pid) => wrap_node(cx, pid),
        None => Ok(JsValue::NULL),
    }
}

fn el_get_owner_document(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let g = cx.vm().global();
    if let Ok(key) = cx.heap().key_from_str("document")
        && let Ok(Some(prop)) = cx.heap().get_own(g, key)
        && let Some(val) = prop.slot.data()
    {
        return Ok(val);
    }
    Ok(JsValue::NULL)
}

fn el_get_is_connected(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::FALSE);
    };
    let connected = cx
        .host_as::<CinderDomHost>()
        .map(|h| {
            let root = h.doc.root();
            let mut cur = Some(nid);
            while let Some(id) = cur {
                if id == root {
                    return true;
                }
                cur = h.doc.get(id).parent();
            }
            false
        })
        .unwrap_or(false);
    Ok(JsValue::boolean(connected))
}

fn el_get_child_nodes(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return wrap_node_list(cx, Vec::new());
    };
    let kids: Vec<NodeId> = cx
        .host_as::<CinderDomHost>()
        .map(|h| {
            h.doc
                .children(nid)
                .iter()
                .copied()
                .filter(|&cid| h.doc.shadow_host(cid).is_none())
                .collect()
        })
        .unwrap_or_default();
    wrap_node_list(cx, kids)
}

fn el_get_next_sibling(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let next = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.next_sibling(nid));
    wrap_optional_node(cx, next)
}

fn el_get_previous_sibling(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::NULL);
    };
    let prev = cx
        .host_as::<CinderDomHost>()
        .and_then(|h| h.doc.previous_sibling(nid));
    wrap_optional_node(cx, prev)
}

fn doc_get_node_type(
    _cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    Ok(JsValue::number(9.0))
}

fn doc_get_node_name(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    cx.vm().string("#document")
}

fn doc_get_child_nodes(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let kids: Vec<NodeId> = cx
        .host_as::<CinderDomHost>()
        .map(|h| h.doc.children(h.doc.root()).to_vec())
        .unwrap_or_default();
    wrap_node_list(cx, kids)
}

fn doc_get_is_connected(
    _cx: &mut NativeCx<'_>,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    Ok(JsValue::TRUE)
}

fn window_native_fetch(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let url = arg_str(cx, args, 0)?;
    let (status, body) =
        if let Some(host) = cx.host_as::<CinderDomHost>() {
            if let Some((_, st, mock_body)) = host.fetch_mocks.iter().find(|(k, _, _)| {
                k == &url || url.ends_with(k.as_str()) || k.ends_with(url.as_str())
            }) {
                (*st, mock_body.clone())
            } else {
                (200u16, "{}".to_string())
            }
        } else {
            (200u16, "{}".to_string())
        };
    let obj = cx.vm().new_plain_object()?;
    cx.vm()
        .define_data(obj, "status", JsValue::number(status as f64))?;
    let body_js = cx.vm().string(&body)?;
    cx.vm().define_data(obj, "body", body_js)?;
    let url_js = cx.vm().string(&url)?;
    cx.vm().define_data(obj, "url", url_js)?;
    Ok(JsValue::object(obj))
}

fn window_sha256_bytes(
    cx: &mut NativeCx<'_>,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let mut input = Vec::new();
    if let Some(first) = args.first().copied() {
        if let Some(obj) = first.as_object() {
            let len_key = cx
                .heap()
                .key_from_str("length")
                .map_err(|e| RunError::internal(format!("{e:?}")))?;
            let len = cx
                .heap()
                .get_own(obj, len_key)
                .ok()
                .flatten()
                .and_then(|p| p.slot.data())
                .and_then(|v| v.as_number())
                .unwrap_or(0.0) as usize;
            for i in 0..len.min(1_048_576) {
                let k = cx
                    .heap()
                    .key_from_str(&i.to_string())
                    .map_err(|e| RunError::internal(format!("{e:?}")))?;
                let b = cx
                    .heap()
                    .get_own(obj, k)
                    .ok()
                    .flatten()
                    .and_then(|p| p.slot.data())
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as i64;
                input.push((b & 0xFF) as u8);
            }
        } else if let Ok(s) = cx.to_display_string(first) {
            input.extend_from_slice(s.as_bytes());
        }
    }
    let digest = sha256_digest(&input);
    let elems: Vec<JsValue> = digest.iter().map(|&b| JsValue::number(b as f64)).collect();
    cx.vm().new_array_from(&elems)
}

fn canvas_fill_linear_gradient(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let x = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let y = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
    let w = args
        .get(2)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as u32;
    let h = args
        .get(3)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as u32;
    let c0_str = arg_str(cx, args, 4).unwrap_or_else(|_| "#000000".to_string());
    let c1_str = arg_str(cx, args, 5).unwrap_or_else(|_| "#ffffff".to_string());
    let horizontal = args.get(6).and_then(|v| v.as_bool()).unwrap_or(true);
    let c0 = crate::parse_canvas_color(&c0_str);
    let c1 = crate::parse_canvas_color(&c1_str);
    let x0 = x as f32;
    let y0 = y as f32;
    let x1 = if horizontal {
        (x + w as i32) as f32
    } else {
        x0
    };
    let y1 = if horizontal {
        y0
    } else {
        (y + h as i32) as f32
    };
    if let Some(host) = cx.host_as::<CinderDomHost>() {
        host.doc.canvas_fill_linear_gradient(
            nid,
            x,
            y,
            w,
            h,
            x0,
            y0,
            x1,
            y1,
            &[(0.0, c0), (1.0, c1)],
        );
    }
    Ok(JsValue::UNDEFINED)
}

fn sha256_digest(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64) * 8;
    let mut padded = data.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[allow(dead_code)]
fn _keep_dom_imports_used(d: &Document) -> bool {
    let _ = parse_html;
    matches!(d.get(d.root()).data(), NodeData::Document)
}

// ---- Pillar 2.3 & Pillar 4: Live Geometry, Scroll, <dialog>, & Form Validation ----

fn el_get_offset_width(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let w = unwrap_node(cx, this)
        .and_then(|nid| cx.host_as::<CinderDomHost>()?.compute_node_box(nid))
        .map(|b| b.rect.width.round() as f64)
        .unwrap_or(0.0);
    Ok(JsValue::number(w))
}

fn el_get_offset_height(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let h = unwrap_node(cx, this)
        .and_then(|nid| cx.host_as::<CinderDomHost>()?.compute_node_box(nid))
        .map(|b| b.rect.height.round() as f64)
        .unwrap_or(0.0);
    Ok(JsValue::number(h))
}

fn el_get_client_width(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let w = unwrap_node(cx, this)
        .and_then(|nid| cx.host_as::<CinderDomHost>()?.compute_node_box(nid))
        .map(|b| {
            (b.rect.width - b.border.left - b.border.right)
                .max(0.0)
                .round() as f64
        })
        .unwrap_or(0.0);
    Ok(JsValue::number(w))
}

fn el_get_client_height(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let h = unwrap_node(cx, this)
        .and_then(|nid| cx.host_as::<CinderDomHost>()?.compute_node_box(nid))
        .map(|b| {
            (b.rect.height - b.border.top - b.border.bottom)
                .max(0.0)
                .round() as f64
        })
        .unwrap_or(0.0);
    Ok(JsValue::number(h))
}

fn el_get_scroll_width(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let w = unwrap_node(cx, this)
        .and_then(|nid| cx.host_as::<CinderDomHost>()?.compute_node_box(nid))
        .map(|b| {
            let cw = (b.rect.width - b.border.left - b.border.right).max(0.0);
            b.scroll_size.0.max(cw).round() as f64
        })
        .unwrap_or(0.0);
    Ok(JsValue::number(w))
}

fn el_get_scroll_height(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let h = unwrap_node(cx, this)
        .and_then(|nid| cx.host_as::<CinderDomHost>()?.compute_node_box(nid))
        .map(|b| {
            let ch = (b.rect.height - b.border.top - b.border.bottom).max(0.0);
            b.scroll_size.1.max(ch).round() as f64
        })
        .unwrap_or(0.0);
    Ok(JsValue::number(h))
}

fn el_get_scroll_top(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let sy = unwrap_node(cx, this)
        .and_then(|nid| Some(cx.host_as::<CinderDomHost>()?.doc.node_scroll_offset(nid).1 as f64))
        .unwrap_or(0.0);
    Ok(JsValue::number(sy))
}

fn el_set_scroll_top(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let next_y = args
        .first()
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as f32;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        let (sx, _) = h.doc.node_scroll_offset(nid);
        h.doc.set_node_scroll_offset(nid, sx, next_y);
    }
    Ok(JsValue::UNDEFINED)
}

fn el_get_scroll_left(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let sx = unwrap_node(cx, this)
        .and_then(|nid| Some(cx.host_as::<CinderDomHost>()?.doc.node_scroll_offset(nid).0 as f64))
        .unwrap_or(0.0);
    Ok(JsValue::number(sx))
}

fn el_set_scroll_left(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let next_x = args
        .first()
        .and_then(|v| v.as_number())
        .unwrap_or(0.0)
        .max(0.0) as f32;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        let (_, sy) = h.doc.node_scroll_offset(nid);
        h.doc.set_node_scroll_offset(nid, next_x, sy);
    }
    Ok(JsValue::UNDEFINED)
}

fn extract_scroll_coords(cx: &mut NativeCx<'_>, args: &[JsValue]) -> (Option<f32>, Option<f32>) {
    if let Some(first) = args.first().copied() {
        if let Some(obj) = first.as_object() {
            let mut left = None;
            let mut top = None;
            if let Ok(k_left) = cx.heap().key_from_str("left")
                && let Ok(Some(p)) = cx.heap().get_own(obj, k_left)
                && let Some(v) = p.slot.data().and_then(|v| v.as_number())
            {
                left = Some(v as f32);
            }
            if let Ok(k_top) = cx.heap().key_from_str("top")
                && let Ok(Some(p)) = cx.heap().get_own(obj, k_top)
                && let Some(v) = p.slot.data().and_then(|v| v.as_number())
            {
                top = Some(v as f32);
            }
            return (left, top);
        }
        let x = first.as_number().map(|v| v as f32);
        let y = args.get(1).and_then(|v| v.as_number()).map(|v| v as f32);
        return (x, y);
    }
    (None, None)
}

fn el_scroll_to(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let (nx, ny) = extract_scroll_coords(cx, args);
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        let (cx_off, cy_off) = h.doc.node_scroll_offset(nid);
        h.doc.set_node_scroll_offset(
            nid,
            nx.unwrap_or(cx_off).max(0.0),
            ny.unwrap_or(cy_off).max(0.0),
        );
    }
    Ok(JsValue::UNDEFINED)
}

fn el_scroll_by(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let (dx, dy) = extract_scroll_coords(cx, args);
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        let (cx_off, cy_off) = h.doc.node_scroll_offset(nid);
        h.doc.set_node_scroll_offset(
            nid,
            (cx_off + dx.unwrap_or(0.0)).max(0.0),
            (cy_off + dy.unwrap_or(0.0)).max(0.0),
        );
    }
    Ok(JsValue::UNDEFINED)
}

fn el_get_dialog_open(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let open = unwrap_node(cx, this)
        .and_then(|nid| {
            let h = cx.host_as::<CinderDomHost>()?;
            Some(
                h.doc
                    .get(nid)
                    .data()
                    .attr("open")
                    .is_some_and(|v| !v.eq_ignore_ascii_case("false")),
            )
        })
        .unwrap_or(false);
    Ok(JsValue::boolean(open))
}

fn el_set_dialog_open(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let open = args.first().copied().is_some_and(|v| {
        v.as_bool() == Some(true)
            || v.as_number().is_some_and(|n| n != 0.0 && !n.is_nan())
            || v.is_object()
            || (v.is_string() && cx.to_display_string(v).is_ok_and(|s| !s.is_empty()))
    });
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        if open {
            h.doc.set_attribute(nid, "open", "");
        } else {
            h.doc.remove_attribute(nid, "open");
            h.doc.remove_attribute(nid, "data-modal");
        }
    }
    Ok(JsValue::UNDEFINED)
}

fn el_get_dialog_return_value(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let rv = unwrap_node(cx, this)
        .and_then(|nid| {
            cx.host_as::<CinderDomHost>()?
                .doc
                .get(nid)
                .data()
                .attr("data-return-value")
                .map(str::to_string)
        })
        .unwrap_or_default();
    cx.vm().string(&rv)
}

fn el_set_dialog_return_value(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let rv = arg_str(cx, args, 0)?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.doc.set_attribute(nid, "data-return-value", &rv);
    }
    Ok(JsValue::UNDEFINED)
}

fn el_dialog_show(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    if let Some(nid) = unwrap_node(cx, this)
        && let Some(h) = cx.host_as::<CinderDomHost>()
    {
        h.doc.set_attribute(nid, "open", "");
    }
    Ok(JsValue::UNDEFINED)
}

fn el_dialog_show_modal(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    if let Some(nid) = unwrap_node(cx, this)
        && let Some(h) = cx.host_as::<CinderDomHost>()
    {
        h.doc.set_attribute(nid, "open", "");
        h.doc.set_attribute(nid, "data-modal", "true");
    }
    Ok(JsValue::UNDEFINED)
}

fn el_dialog_close(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let ret_val = if let Some(first) = args.first().copied()
        && !first.is_undefined()
    {
        Some(cx.to_display_string(first)?)
    } else {
        None
    };
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.doc.remove_attribute(nid, "open");
        h.doc.remove_attribute(nid, "data-modal");
        if let Some(rv) = ret_val {
            h.doc.set_attribute(nid, "data-return-value", &rv);
        }
    }
    let ev = cx.vm().new_plain_object()?;
    let close_str = cx.vm().string("close")?;
    cx.vm().define_data(ev, "type", close_str)?;
    cx.vm().define_data(ev, "bubbles", JsValue::FALSE)?;
    cx.vm().define_data(ev, "cancelable", JsValue::FALSE)?;
    let _ = el_dispatch_event(cx, this, &[JsValue::object(ev)]);
    Ok(JsValue::UNDEFINED)
}

struct ValidityBreakdown {
    valid: bool,
    value_missing: bool,
    type_mismatch: bool,
    pattern_mismatch: bool,
    too_long: bool,
    too_short: bool,
    range_underflow: bool,
    range_overflow: bool,
    custom_error: bool,
    message: String,
}

fn evaluate_node_validity(doc: &Document, nid: NodeId) -> ValidityBreakdown {
    let mut out = ValidityBreakdown {
        valid: true,
        value_missing: false,
        type_mismatch: false,
        pattern_mismatch: false,
        too_long: false,
        too_short: false,
        range_underflow: false,
        range_overflow: false,
        custom_error: false,
        message: String::new(),
    };
    let custom = doc.custom_validity(nid);
    if !custom.is_empty() {
        out.valid = false;
        out.custom_error = true;
        out.message = custom.to_string();
    }
    let data = doc.get(nid).data();
    let Some(tag) = data.tag() else {
        return out;
    };
    if tag == "form" || tag == "fieldset" {
        if doc.is_control_valid(nid) == Some(false) {
            out.valid = false;
            if out.message.is_empty() {
                out.message = "Form contains invalid controls.".to_string();
            }
        }
        return out;
    }
    if !matches!(tag, "input" | "textarea" | "select") {
        return out;
    }
    if data
        .attr("disabled")
        .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
        || data
            .attr("readonly")
            .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
    {
        return out;
    }
    let input_type = if tag == "input" {
        data.attr("type")
            .unwrap_or("text")
            .trim()
            .to_ascii_lowercase()
    } else {
        String::new()
    };
    let is_required = data
        .attr("required")
        .is_some_and(|v| !v.eq_ignore_ascii_case("false"));
    if matches!(input_type.as_str(), "checkbox" | "radio") {
        if is_required && !doc.is_checked(nid) {
            out.valid = false;
            out.value_missing = true;
            if out.message.is_empty() {
                out.message = "Please check this box if you want to proceed.".to_string();
            }
        }
        return out;
    }
    let val = doc.input_value(nid);
    if is_required && val.trim().is_empty() {
        out.valid = false;
        out.value_missing = true;
        if out.message.is_empty() {
            out.message = "Please fill out this field.".to_string();
        }
        return out;
    }
    if val.is_empty() {
        return out;
    }
    let char_len = val.chars().count();
    if let Some(min_len) = data
        .attr("minlength")
        .and_then(|s| s.trim().parse::<usize>().ok())
        && char_len < min_len
    {
        out.valid = false;
        out.too_short = true;
        if out.message.is_empty() {
            out.message = format!("Please lengthen this text to {min_len} characters or more.");
        }
    }
    if let Some(max_len) = data
        .attr("maxlength")
        .and_then(|s| s.trim().parse::<usize>().ok())
        && char_len > max_len
    {
        out.valid = false;
        out.too_long = true;
        if out.message.is_empty() {
            out.message = format!("Please shorten this text to {max_len} characters or fewer.");
        }
    }
    match input_type.as_str() {
        "email" => {
            let trimmed = val.trim();
            let valid_email = trimmed.split_once('@').is_some_and(|(l, d)| {
                !l.is_empty()
                    && !d.is_empty()
                    && !d.contains('@')
                    && !trimmed.chars().any(char::is_whitespace)
            });
            if !valid_email {
                out.valid = false;
                out.type_mismatch = true;
                if out.message.is_empty() {
                    out.message = "Please enter an email address.".to_string();
                }
            }
        }
        "url" => {
            let trimmed = val.trim();
            let valid_url = trimmed.split_once("://").is_some_and(|(s, r)| {
                !s.is_empty()
                    && !r.is_empty()
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
                    && !trimmed.chars().any(char::is_whitespace)
            });
            if !valid_url {
                out.valid = false;
                out.type_mismatch = true;
                if out.message.is_empty() {
                    out.message = "Please enter a URL.".to_string();
                }
            }
        }
        "number" | "range" => {
            if let Ok(num) = val.trim().parse::<f64>() {
                if let Some(min_v) = data.attr("min").and_then(|s| s.trim().parse::<f64>().ok())
                    && num < min_v
                {
                    out.valid = false;
                    out.range_underflow = true;
                    if out.message.is_empty() {
                        out.message = format!("Value must be greater than or equal to {min_v}.");
                    }
                }
                if let Some(max_v) = data.attr("max").and_then(|s| s.trim().parse::<f64>().ok())
                    && num > max_v
                {
                    out.valid = false;
                    out.range_overflow = true;
                    if out.message.is_empty() {
                        out.message = format!("Value must be less than or equal to {max_v}.");
                    }
                }
            } else {
                out.valid = false;
                out.type_mismatch = true;
                if out.message.is_empty() {
                    out.message = "Please enter a number.".to_string();
                }
            }
        }
        _ => {}
    }
    if out.valid && doc.is_control_valid(nid) == Some(false) {
        out.valid = false;
        out.pattern_mismatch = true;
        if out.message.is_empty() {
            out.message = "Please match the requested format.".to_string();
        }
    }
    out
}

fn el_get_validity(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let bd = unwrap_node(cx, this)
        .and_then(|nid| {
            Some(evaluate_node_validity(
                &cx.host_as::<CinderDomHost>()?.doc,
                nid,
            ))
        })
        .unwrap_or(ValidityBreakdown {
            valid: true,
            value_missing: false,
            type_mismatch: false,
            pattern_mismatch: false,
            too_long: false,
            too_short: false,
            range_underflow: false,
            range_overflow: false,
            custom_error: false,
            message: String::new(),
        });
    let obj = cx.vm().new_plain_object()?;
    for (k, v) in [
        ("valid", bd.valid),
        ("valueMissing", bd.value_missing),
        ("typeMismatch", bd.type_mismatch),
        ("patternMismatch", bd.pattern_mismatch),
        ("tooLong", bd.too_long),
        ("tooShort", bd.too_short),
        ("rangeUnderflow", bd.range_underflow),
        ("rangeOverflow", bd.range_overflow),
        ("stepMismatch", false),
        ("badInput", false),
        ("customError", bd.custom_error),
    ] {
        cx.vm().define_data(obj, k, JsValue::boolean(v))?;
    }
    Ok(JsValue::object(obj))
}

fn el_get_validation_message(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let msg = unwrap_node(cx, this)
        .and_then(|nid| {
            Some(evaluate_node_validity(&cx.host_as::<CinderDomHost>()?.doc, nid).message)
        })
        .unwrap_or_default();
    cx.vm().string(&msg)
}

fn el_get_will_validate(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let will = unwrap_node(cx, this)
        .and_then(|nid| {
            let h = cx.host_as::<CinderDomHost>()?;
            let data = h.doc.get(nid).data();
            let tag = data.tag()?;
            if !matches!(tag, "input" | "textarea" | "select") {
                return Some(false);
            }
            let disabled = data
                .attr("disabled")
                .is_some_and(|v| !v.eq_ignore_ascii_case("false"));
            let readonly = data
                .attr("readonly")
                .is_some_and(|v| !v.eq_ignore_ascii_case("false"));
            Some(!disabled && !readonly)
        })
        .unwrap_or(false);
    Ok(JsValue::boolean(will))
}

fn el_check_validity(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::TRUE);
    };
    let valid = cx
        .host_as::<CinderDomHost>()
        .map(|h| evaluate_node_validity(&h.doc, nid).valid)
        .unwrap_or(true);
    if !valid {
        let ev = cx.vm().new_plain_object()?;
        let inv_str = cx.vm().string("invalid")?;
        cx.vm().define_data(ev, "type", inv_str)?;
        cx.vm().define_data(ev, "bubbles", JsValue::FALSE)?;
        cx.vm().define_data(ev, "cancelable", JsValue::TRUE)?;
        let _ = el_dispatch_event(cx, this, &[JsValue::object(ev)]);
    }
    Ok(JsValue::boolean(valid))
}

fn el_report_validity(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    el_check_validity(cx, this, args)
}

fn el_set_custom_validity(
    cx: &mut NativeCx<'_>,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, RunError> {
    let Some(nid) = unwrap_node(cx, this) else {
        return Ok(JsValue::UNDEFINED);
    };
    let msg = arg_str(cx, args, 0)?;
    if let Some(h) = cx.host_as::<CinderDomHost>() {
        h.doc.set_custom_validity(nid, &msg);
    }
    Ok(JsValue::UNDEFINED)
}
