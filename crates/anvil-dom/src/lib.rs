// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato
//! Document tree. Nodes live in one list and point at each other by id.
//!
//! First stage of the pipeline. [`parse_html`] turns source into a [`Document`]
//! this crate owns. anvil-style reads the tree next and does not mutate it.
//! anvil-layout and anvil-paint never see the HTML source. anvil-script mutates
//! the same document. anvil-browser is the owner of the live page.
//!
//! `<img src>` and `<video src>` (or the first `<source src>`) are recorded
//! as element attributes. Pixel decode is anvil-image's job. Timed video is
//! anvil-video's job. This crate does not decode either.
//!
//! Hard caps, enforced by the parser and by [`Document::set_inner_html`]:
//! [`MAX_DOM_DEPTH`] is 256 open elements, [`MAX_DOM_NODES`] is 50_000 nodes,
//! and [`MAX_ATTRS_PER_ELEMENT`] is 128 attributes on one start tag.
//! Nodes detached by edits stay in the list until the document is dropped.
//!
//! [`Document::query_selector`], [`Document::query_selector_all`],
//! [`Document::matches_selector`], and [`Document::closest`] accept compound
//! and complex selectors with descendant (` `), child (`>`), adjacent-sibling (`+`),
//! and general-sibling (`~`) combinators, comma lists, attribute selectors (`[attr]`,
//! `=`, `~=`, `|=`, `^=`, `$=`, `*=`), and Selectors Level 4 pseudo-classes
//! (`:is()`, `:where()`, `:not()`, `:has()`, `:first-child`, `:last-child`,
//! `:only-child`, `:nth-child()`, `:nth-last-child()`, `:empty`, `:root`, `:scope`,
//! `:checked`, `:disabled`, `:enabled`, `:required`, `:optional`). The full CSS
//! cascade lives in `anvil-style`.

mod html;

pub use html::{MAX_ATTRS_PER_ELEMENT, MAX_DOM_DEPTH, MAX_DOM_NODES, parse_html};

/// Official codename of the overall rendering, layout, and paint engine (`Forge`).
pub const RENDERING_ENGINE_NAME: &str = "Forge";
/// Official codename of the JavaScript bytecode VM and native JIT engine (`Cinder`).
pub const JS_ENGINE_NAME: &str = "Cinder";
/// Official codename of the CSS tokenizer, selector, and cascade engine (`Prism`).
pub const CSS_ENGINE_NAME: &str = "Prism";

/// One style sheet the document names, from [`Document::style_sheets`].
///
/// `media` is the trimmed `media` attribute, or `None` when there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StyleSheetSource {
    /// `<link rel=stylesheet href>`. The browser fetches `href` itself.
    Link {
        href: String,
        integrity: Option<String>,
        media: Option<String>,
    },
    /// A `<style>` element and its text.
    Style { css: String, media: Option<String> },
}

/// Index of a node inside one [`Document`]'s node list.
///
/// The id is a copy of the slot index. It does not own the node. An id from
/// another document, or past [`Document::node_count`], is not valid here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(u32);

impl NodeId {
    /// Build an id from a raw slot index. Does not check that a node exists.
    pub fn from_raw(index: u32) -> Self {
        Self(index)
    }

    /// Slot index. Used to index the document's node vector.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Why [`Document::append`] refused an edit. The tree is left unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeError {
    /// `parent` or `child` is not a slot in this document.
    Missing,
    /// `child` is `parent`, or `parent` is already inside `child`.
    Cycle,
}

/// Payload owned by one node. The tree shape lives on [`Node`], not here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeData {
    /// The document slot at index 0. It has no tag and no text.
    Document,
    /// An element. `tag` is ASCII-lowercase and owned. `attrs` owns each
    /// name and value. Names are stored lowercase by the writers in this crate.
    Element {
        tag: String,
        attrs: Vec<(String, String)>,
    },
    /// Owned character data. Not parsed as markup.
    Text(String),
}

impl NodeData {
    /// An element with an owned lowercase tag and no attributes.
    /// `tag` is borrowed only for the copy.
    pub fn element(tag: &str) -> Self {
        Self::Element {
            tag: tag.to_ascii_lowercase(),
            attrs: Vec::new(),
        }
    }

    /// The element tag, borrowed from this node. `None` for the document and for text.
    pub fn tag(&self) -> Option<&str> {
        match self {
            Self::Element { tag, .. } => Some(tag),
            _ => None,
        }
    }

    /// The text, borrowed from this node. `None` unless this is [`NodeData::Text`].
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }

    /// Attribute value borrowed from this element. `None` if this is not an
    /// element or the name is absent. Lookup ignores ASCII case.
    pub fn attr(&self, name: &str) -> Option<&str> {
        let Self::Element { attrs, .. } = self else {
            return None;
        };
        attrs
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The `id` attribute, borrowed. `None` when it is missing.
    pub fn id(&self) -> Option<&str> {
        self.attr("id")
    }

    /// True when `class` contains `class_name` as a whole whitespace-separated token.
    /// A missing class attribute is false. The name is not trimmed here.
    pub fn has_class(&self, class_name: &str) -> bool {
        self.attr("class")
            .is_some_and(|classes| classes.split_ascii_whitespace().any(|c| c == class_name))
    }

    /// Set an attribute, storing the name in lowercase. No-op for non-elements.
    /// Replaces an existing name. `value` is copied.
    pub fn set_attr(&mut self, name: &str, value: &str) {
        let Self::Element { attrs, .. } = self else {
            return;
        };
        let key = name.to_ascii_lowercase();
        if let Some((_, slot)) = attrs.iter_mut().find(|(k, _)| *k == key) {
            *slot = value.to_string();
        } else {
            attrs.push((key, value.to_string()));
        }
    }

    /// Remove every attribute whose name matches, ignoring ASCII case.
    /// Returns false for a non-element or when nothing was removed.
    pub fn remove_attr(&mut self, name: &str) -> bool {
        let Self::Element { attrs, .. } = self else {
            return false;
        };
        let before = attrs.len();
        attrs.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        attrs.len() != before
    }

    /// Append a class token if it is non-empty and not already present.
    /// No-op for non-elements. The class string is owned by the attribute.
    pub fn add_class(&mut self, class_name: &str) {
        let class_name = class_name.trim();
        if class_name.is_empty() || self.has_class(class_name) {
            return;
        }
        let next = match self.attr("class") {
            Some(existing) if !existing.trim().is_empty() => {
                format!("{} {class_name}", existing.trim())
            }
            _ => class_name.to_string(),
        };
        self.set_attr("class", &next);
    }

    /// Drop one class token. A missing class attribute is a no-op.
    /// The remaining tokens stay in one owned `class` value, which may be empty.
    pub fn remove_class(&mut self, class_name: &str) {
        let class_name = class_name.trim();
        let Some(existing) = self.attr("class") else {
            return;
        };
        let kept: Vec<&str> = existing
            .split_ascii_whitespace()
            .filter(|c| *c != class_name)
            .collect();
        let joined = kept.join(" ");
        self.set_attr("class", &joined);
    }

    /// Add the class and return true, or remove it and return false.
    /// The bool is the class's presence after the call.
    pub fn toggle_class(&mut self, class_name: &str) -> bool {
        if self.has_class(class_name) {
            self.remove_class(class_name);
            false
        } else {
            self.add_class(class_name);
            true
        }
    }
}

/// One slot in a [`Document`]. The document owns the node. Children are ids,
/// not nested `Node` values. `parent` is `None` for the root and for a node
/// that has been detached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    id: NodeId,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    data: NodeData,
}

impl Node {
    /// This slot's id. It is a copy, not a borrow of the document.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// Parent id. `None` if this is the root or the node was detached.
    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    /// Borrowed payload. The node keeps ownership.
    pub fn data(&self) -> &NodeData {
        &self.data
    }

    /// Mutable payload. Tree links are not exposed here.
    pub fn data_mut(&mut self) -> &mut NodeData {
        &mut self.data
    }
}

/// Pixel backing store for an HTML5 `<canvas>` 2D rendering surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanvasSurface {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Tracks style-invalidated nodes and mutation generation without affecting structural `Document` equality.
#[derive(Clone, Debug, Default)]
struct DirtyTracker {
    nodes: Vec<NodeId>,
    generation: u64,
}

impl PartialEq for DirtyTracker {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for DirtyTracker {}

/// A document and every node it owns.
///
/// Slots are append-only. [`Self::remove_node`], [`Self::set_text_content`],
/// and [`Self::set_inner_html`] unlink children but leave the old slots in
/// `nodes`, so [`Self::node_count`] does not shrink. The skeleton is the
/// document node, then `html`, `head`, and `body`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    nodes: Vec<Node>,
    html: NodeId,
    head: NodeId,
    body: NodeId,
    shadow_roots: std::collections::HashMap<NodeId, (NodeId, bool)>,
    shadow_hosts: std::collections::HashMap<NodeId, NodeId>,
    custom_elements: Vec<String>,
    canvas_surfaces: std::collections::HashMap<NodeId, CanvasSurface>,
    input_cursors: std::collections::HashMap<NodeId, usize>,
    hovered_node: Option<NodeId>,
    active_node: Option<NodeId>,
    focused_node: Option<NodeId>,
    target_fragment: Option<String>,
    adopted_styles: Vec<String>,
    scroll_offsets: std::collections::HashMap<NodeId, (u32, u32)>,
    custom_validity: std::collections::HashMap<NodeId, String>,
    dirty: DirtyTracker,
}

impl Default for Document {
    /// Same document as [`Document::new`]: document, `html`, `head`, and `body`.
    fn default() -> Self {
        Self::new()
    }
}

/// True when a `script` `type` is JavaScript, or when `type` is missing.
///
/// `text/javascript`, `application/javascript`, and `module` count. A MIME
/// parameter such as `;charset=utf-8` is ignored. `application/json` does not
/// count, so a data block is not handed to the interpreter.
fn is_javascript_type(raw: Option<&str>) -> bool {
    let Some(raw) = raw else {
        return true;
    };
    let mime = raw.split(';').next().unwrap_or(raw).trim();
    if mime.is_empty() {
        return true;
    }
    matches!(
        mime.to_ascii_lowercase().as_str(),
        "text/javascript"
            | "application/javascript"
            | "text/ecmascript"
            | "application/ecmascript"
            | "text/jscript"
            | "module"
    )
}

impl Document {
    /// An empty document with `html`, `head`, and `body` already in place.
    ///
    /// The returned value owns four nodes: document, `html`, `head`, `body`.
    pub fn new() -> Self {
        let mut doc = Self {
            nodes: Vec::new(),
            html: NodeId(0),
            head: NodeId(0),
            body: NodeId(0),
            shadow_roots: std::collections::HashMap::new(),
            shadow_hosts: std::collections::HashMap::new(),
            custom_elements: Vec::new(),
            canvas_surfaces: std::collections::HashMap::new(),
            input_cursors: std::collections::HashMap::new(),
            hovered_node: None,
            active_node: None,
            focused_node: None,
            target_fragment: None,
            adopted_styles: Vec::new(),
            scroll_offsets: std::collections::HashMap::new(),
            custom_validity: std::collections::HashMap::new(),
            dirty: DirtyTracker::default(),
        };
        let root = doc.alloc(NodeData::Document);
        let html = doc.alloc(NodeData::element("html"));
        let head = doc.alloc(NodeData::element("head"));
        let body = doc.alloc(NodeData::element("body"));
        doc.append(root, html).expect("document skeleton");
        doc.append(html, head).expect("document skeleton");
        doc.append(html, body).expect("document skeleton");
        doc.html = html;
        doc.head = head;
        doc.body = body;
        doc.clear_dirty_styles();
        doc
    }

    /// Mark `id` (and its subtree) as needing incremental style recomputation.
    pub fn mark_style_dirty(&mut self, id: NodeId) {
        if self.nodes.get(id.index()).is_none() {
            return;
        }
        self.dirty.generation = self.dirty.generation.wrapping_add(1);
        if !self.dirty.nodes.contains(&id) {
            self.dirty.nodes.push(id);
        }
    }

    /// True when one or more DOM nodes have pending style invalidations.
    pub fn has_dirty_styles(&self) -> bool {
        !self.dirty.nodes.is_empty()
    }

    /// Borrow the list of nodes marked style-dirty since the last clear/take.
    pub fn dirty_style_nodes(&self) -> &[NodeId] {
        &self.dirty.nodes
    }

    /// Drain and return all nodes marked style-dirty since the last pass.
    pub fn take_dirty_style_nodes(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.dirty.nodes)
    }

    /// Clear all pending style-dirty markers without returning them.
    pub fn clear_dirty_styles(&mut self) {
        self.dirty.nodes.clear();
    }

    /// Monotonically increasing counter incremented on each DOM or state mutation.
    pub fn mutation_generation(&self) -> u64 {
        self.dirty.generation
    }

    /// Attach a Shadow DOM root to `host` (`open` = true for `mode: "open"`).
    /// Returns the `NodeId` of the shadow root container node.
    pub fn attach_shadow(&mut self, host: NodeId, open: bool) -> NodeId {
        if let Some(&(sr, _)) = self.shadow_roots.get(&host) {
            return sr;
        }
        let sr = self.create_element("shadow-root", Vec::new());
        let _ = self.append(host, sr);
        self.shadow_roots.insert(host, (sr, open));
        self.shadow_hosts.insert(sr, host);
        self.mark_style_dirty(host);
        sr
    }

    /// All `(host, shadow_root, is_open)` entries sorted by host `NodeId`.
    pub fn shadow_root_entries(&self) -> Vec<(NodeId, NodeId, bool)> {
        let mut list: Vec<(NodeId, NodeId, bool)> = self
            .shadow_roots
            .iter()
            .map(|(&host, &(sr, open))| (host, sr, open))
            .collect();
        list.sort_by_key(|(h, _, _)| h.index());
        list
    }

    /// Concatenated text of `<style>` elements inside `shadow_root` in tree order.
    pub fn shadow_css(&self, shadow_root: NodeId) -> String {
        if self.nodes.get(shadow_root.index()).is_none() {
            return String::new();
        }
        let mut blocks = Vec::new();
        self.collect_style(shadow_root, &mut blocks);
        blocks.join("\n")
    }

    /// Light-DOM nodes distributed to `<slot>` element `slot_id`.
    ///
    /// When `flatten` is `true` and no light-DOM nodes match `slot_id`, returns
    /// `slot_id`'s own fallback children (expanding nested `<slot>` elements).
    pub fn assigned_nodes(&self, slot_id: NodeId, flatten: bool) -> Vec<NodeId> {
        if self.nodes.get(slot_id.index()).and_then(|n| n.data().tag()) != Some("slot") {
            return Vec::new();
        }
        let Some(host) = self.enclosing_shadow_host(slot_id) else {
            return if flatten {
                self.children(slot_id).to_vec()
            } else {
                Vec::new()
            };
        };
        let Some(&(sr, _)) = self.shadow_roots.get(&host) else {
            return Vec::new();
        };
        let slot_name = self
            .get(slot_id)
            .data()
            .attr("name")
            .map(str::trim)
            .unwrap_or("");
        // Only the first `<slot>` with `slot_name` in `sr` receives the matching light-DOM nodes.
        if self.find_slot_by_name(sr, slot_name) != Some(slot_id) {
            return if flatten {
                self.children(slot_id).to_vec()
            } else {
                Vec::new()
            };
        }
        let mut assigned = Vec::new();
        for &child in self.children(host) {
            if child == sr {
                continue;
            }
            let data = self.get(child).data();
            if let NodeData::Text(t) = data
                && t.trim().is_empty()
            {
                continue;
            }
            let child_slot = data.attr("slot").map(str::trim).unwrap_or("");
            if child_slot == slot_name {
                if flatten && data.tag() == Some("slot") {
                    assigned.extend(self.assigned_nodes(child, true));
                } else {
                    assigned.push(child);
                }
            }
        }
        if !assigned.is_empty() {
            assigned
        } else if flatten {
            self.children(slot_id).to_vec()
        } else {
            Vec::new()
        }
    }

    /// Element nodes distributed to `<slot>` element `slot_id`.
    pub fn assigned_elements(&self, slot_id: NodeId, flatten: bool) -> Vec<NodeId> {
        self.assigned_nodes(slot_id, flatten)
            .into_iter()
            .filter(|&id| self.get(id).data().tag().is_some())
            .collect()
    }

    /// The `<slot>` inside the parent shadow root to which `light_child` is assigned, if any.
    pub fn assigned_slot(&self, light_child: NodeId) -> Option<NodeId> {
        let host = self.nodes.get(light_child.index())?.parent?;
        let &(sr, _) = self.shadow_roots.get(&host)?;
        if light_child == sr {
            return None;
        }
        let data = self.get(light_child).data();
        if let NodeData::Text(t) = data
            && t.trim().is_empty()
        {
            return None;
        }
        let slot_name = data.attr("slot").map(str::trim).unwrap_or("");
        self.find_slot_by_name(sr, slot_name)
    }

    /// True when `id` is a light-DOM child assigned to a `<slot>` in its parent's shadow root (`::slotted`).
    pub fn is_slotted_node(&self, id: NodeId) -> bool {
        self.assigned_slot(id).is_some()
    }

    fn find_slot_by_name(&self, root: NodeId, target_name: &str) -> Option<NodeId> {
        let data = self.get(root).data();
        if data.tag() == Some("slot") {
            let name = data.attr("name").map(str::trim).unwrap_or("");
            if name == target_name {
                return Some(root);
            }
        }
        for &child in self.children(root) {
            if let Some(found) = self.find_slot_by_name(child, target_name) {
                return Some(found);
            }
        }
        None
    }

    /// Composed children of `id` for layout and painting.
    ///
    /// - When the document has no shadow roots, borrows `self.children(id)` with zero allocation.
    /// - When `id` is a shadow host, returns the composed children of its shadow root (distributing
    ///   light-DOM children into `<slot>` elements and hiding unslotted light-DOM children).
    /// - When `id` is inside a shadow tree and contains `<slot>` children, splices each `<slot>`'s
    ///   assigned light-DOM nodes (or fallback children) inline.
    pub fn composed_children(&self, id: NodeId) -> std::borrow::Cow<'_, [NodeId]> {
        if self.shadow_roots.is_empty() {
            return std::borrow::Cow::Borrowed(self.children(id));
        }
        if let Some(&(sr, _)) = self.shadow_roots.get(&id) {
            let mut out = Vec::new();
            self.expand_composed_children_into(sr, &mut out);
            return std::borrow::Cow::Owned(out);
        }
        if self.get(id).data().tag() == Some("slot") && self.enclosing_shadow_host(id).is_some() {
            return std::borrow::Cow::Owned(self.assigned_nodes(id, true));
        }
        let raw = self.children(id);
        if raw
            .iter()
            .any(|&c| self.get(c).data().tag() == Some("slot"))
            && self.enclosing_shadow_host(id).is_some()
        {
            let mut out = Vec::with_capacity(raw.len());
            self.expand_composed_children_into(id, &mut out);
            return std::borrow::Cow::Owned(out);
        }
        std::borrow::Cow::Borrowed(raw)
    }

    fn expand_composed_children_into(&self, container: NodeId, out: &mut Vec<NodeId>) {
        for &child in self.children(container) {
            if self.get(child).data().tag() == Some("slot") {
                for slotted in self.assigned_nodes(child, true) {
                    if self.get(slotted).data().tag() == Some("slot") {
                        self.expand_composed_children_into(slotted, out);
                    } else {
                        out.push(slotted);
                    }
                }
            } else {
                out.push(child);
            }
        }
    }

    /// Open shadow root attached to `host`, or `None` if none or closed.
    pub fn shadow_root(&self, host: NodeId) -> Option<NodeId> {
        self.shadow_roots
            .get(&host)
            .and_then(|&(sr, open)| if open { Some(sr) } else { None })
    }

    /// Shadow root attached to `host` regardless of `open`/`closed` mode.
    pub fn shadow_root_any(&self, host: NodeId) -> Option<NodeId> {
        self.shadow_roots.get(&host).map(|&(sr, _)| sr)
    }

    /// Shadow host element for a `shadow-root` node `sr`.
    pub fn shadow_host(&self, sr: NodeId) -> Option<NodeId> {
        self.shadow_hosts.get(&sr).copied()
    }

    /// True when `node` has had `attach_shadow` called on it (or is a `shadow-root` container).
    pub fn is_shadow_host(&self, node: NodeId) -> bool {
        self.shadow_roots.contains_key(&node) || self.shadow_hosts.contains_key(&node)
    }

    /// Enclosing shadow host for `node` (if `node` is inside a shadow tree or is a shadow host).
    pub fn enclosing_shadow_host(&self, node: NodeId) -> Option<NodeId> {
        let mut cursor = Some(node);
        while let Some(curr) = cursor {
            if let Some(&host) = self.shadow_hosts.get(&curr) {
                return Some(host);
            }
            cursor = self.nodes.get(curr.index()).and_then(|n| n.parent);
        }
        if self.shadow_roots.contains_key(&node) {
            return Some(node);
        }
        None
    }

    /// Register a custom element tag name (`customElements.define(name, ...)`).
    pub fn define_custom_element(&mut self, name: &str) {
        let lower = name.trim().to_ascii_lowercase();
        if lower.contains('-') && !self.custom_elements.iter().any(|n| n == &lower) {
            self.custom_elements.push(lower);
        }
    }

    /// True when `name` has been registered via [`Self::define_custom_element`].
    pub fn is_custom_element_defined(&self, name: &str) -> bool {
        let lower = name.trim().to_ascii_lowercase();
        self.custom_elements.iter().any(|n| n == &lower)
    }

    /// Currently hovered node id, if any.
    pub fn hovered_node(&self) -> Option<NodeId> {
        self.hovered_node
    }

    /// Update the currently hovered node id.
    pub fn set_hovered_node(&mut self, id: Option<NodeId>) {
        let prev = self.hovered_node;
        self.hovered_node = id.filter(|n| self.nodes.get(n.index()).is_some());
        if prev != self.hovered_node {
            if let Some(p) = prev {
                self.mark_style_dirty(p);
            }
            if let Some(n) = self.hovered_node {
                self.mark_style_dirty(n);
            }
        }
    }

    /// True when `id` is the hovered node or an element ancestor of the hovered node (`:hover`).
    pub fn is_hovered(&self, id: NodeId) -> bool {
        self.hovered_node.is_some_and(|h| self.contains_node(id, h))
    }

    /// Currently active (pressed) node id, if any.
    pub fn active_node(&self) -> Option<NodeId> {
        self.active_node
    }

    /// Update the currently active (pressed) node id.
    pub fn set_active_node(&mut self, id: Option<NodeId>) {
        let prev = self.active_node;
        self.active_node = id.filter(|n| self.nodes.get(n.index()).is_some());
        if prev != self.active_node {
            if let Some(p) = prev {
                self.mark_style_dirty(p);
            }
            if let Some(n) = self.active_node {
                self.mark_style_dirty(n);
            }
        }
    }

    /// True when `id` is the active node or an element ancestor of the active node (`:active`).
    pub fn is_active(&self, id: NodeId) -> bool {
        self.active_node.is_some_and(|a| self.contains_node(id, a))
    }

    /// Currently focused node id (`document.activeElement`), if any.
    pub fn focused_node(&self) -> Option<NodeId> {
        self.focused_node
    }

    /// Update the currently focused node id.
    pub fn set_focused_node(&mut self, id: Option<NodeId>) {
        let prev = self.focused_node;
        self.focused_node = id.filter(|n| self.nodes.get(n.index()).is_some());
        if prev != self.focused_node {
            if let Some(p) = prev {
                self.mark_style_dirty(p);
            }
            if let Some(n) = self.focused_node {
                self.mark_style_dirty(n);
            }
        }
    }

    /// True when `id` is the currently focused node (`:focus`, `:focus-visible`).
    pub fn is_focused(&self, id: NodeId) -> bool {
        self.focused_node == Some(id)
    }

    /// True when `id` is focused or contains the focused node (`:focus-within`).
    pub fn is_focus_within(&self, id: NodeId) -> bool {
        self.focused_node.is_some_and(|f| self.contains_node(id, f))
    }

    /// Current URL fragment target identifier (without `#`), if any.
    pub fn target_fragment(&self) -> Option<&str> {
        self.target_fragment.as_deref()
    }

    /// Set the URL fragment target identifier (`#fragment` or `fragment`).
    pub fn set_target_fragment(&mut self, fragment: Option<&str>) {
        let next = fragment
            .map(|s| s.trim().trim_start_matches('#').trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if self.target_fragment != next {
            self.target_fragment = next;
            self.mark_style_dirty(self.root());
        }
    }

    /// True when `id` matches the active URL fragment target (`:target`).
    pub fn is_target(&self, id: NodeId) -> bool {
        let Some(frag) = self.target_fragment.as_deref() else {
            return false;
        };
        let Some(node) = self.nodes.get(id.index()) else {
            return false;
        };
        let data = node.data();
        data.id() == Some(frag) || (data.tag() == Some("a") && data.attr("name") == Some(frag))
    }

    /// True when `id` or any descendant of `id` is the URL fragment target (`:target-within`).
    pub fn is_target_within(&self, id: NodeId) -> bool {
        if self.is_target(id) {
            return true;
        }
        let Some(node) = self.nodes.get(id.index()) else {
            return false;
        };
        node.children.iter().any(|&c| self.is_target_within(c))
    }

    /// Evaluate HTML5 form constraint validity (`:valid` / `:invalid`) for `id`.
    /// Returns `None` when `id` is not a candidate for constraint validation.
    pub fn is_control_valid(&self, id: NodeId) -> Option<bool> {
        let node = self.nodes.get(id.index())?;
        let data = node.data();
        let tag = data.tag()?;
        if tag == "form" || tag == "fieldset" {
            if !self.subtree_has_constrained_control(id) {
                return None;
            }
            return Some(self.subtree_controls_valid(id));
        }
        if !matches!(tag, "input" | "textarea" | "select") {
            return None;
        }
        if data
            .attr("disabled")
            .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
            || data
                .attr("readonly")
                .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
        {
            return None;
        }
        let input_type = if tag == "input" {
            data.attr("type")
                .unwrap_or("text")
                .trim()
                .to_ascii_lowercase()
        } else {
            String::new()
        };
        if matches!(
            input_type.as_str(),
            "hidden" | "button" | "reset" | "submit" | "image"
        ) {
            return None;
        }
        if self.custom_validity.get(&id).is_some_and(|m| !m.is_empty()) {
            return Some(false);
        }
        let is_required = data
            .attr("required")
            .is_some_and(|v| !v.eq_ignore_ascii_case("false"));
        let has_constraint = is_required
            || data.attr("minlength").is_some()
            || data.attr("maxlength").is_some()
            || data.attr("min").is_some()
            || data.attr("max").is_some()
            || data.attr("pattern").is_some()
            || matches!(input_type.as_str(), "email" | "url" | "number" | "range");
        if !has_constraint {
            return None;
        }
        if matches!(input_type.as_str(), "checkbox" | "radio") {
            if is_required && !self.is_checked(id) {
                return Some(false);
            }
            return Some(true);
        }
        let val = self.input_value(id);
        if is_required && val.trim().is_empty() {
            return Some(false);
        }
        if val.is_empty() {
            return Some(true);
        }
        let char_len = val.chars().count();
        if let Some(min_len) = data
            .attr("minlength")
            .and_then(|s| s.trim().parse::<usize>().ok())
            && char_len < min_len
        {
            return Some(false);
        }
        if let Some(max_len) = data
            .attr("maxlength")
            .and_then(|s| s.trim().parse::<usize>().ok())
            && char_len > max_len
        {
            return Some(false);
        }
        match input_type.as_str() {
            "email" => {
                let trimmed = val.trim();
                let Some((local, domain)) = trimmed.split_once('@') else {
                    return Some(false);
                };
                if local.is_empty()
                    || domain.is_empty()
                    || domain.contains('@')
                    || trimmed.chars().any(char::is_whitespace)
                {
                    return Some(false);
                }
            }
            "url" => {
                let trimmed = val.trim();
                let Some((scheme, rest)) = trimmed.split_once("://") else {
                    return Some(false);
                };
                if scheme.is_empty()
                    || rest.is_empty()
                    || !scheme
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
                    || trimmed.chars().any(char::is_whitespace)
                {
                    return Some(false);
                }
            }
            "number" | "range" => {
                let Ok(num) = val.trim().parse::<f64>() else {
                    return Some(false);
                };
                if let Some(min_v) = data.attr("min").and_then(|s| s.trim().parse::<f64>().ok())
                    && num < min_v
                {
                    return Some(false);
                }
                if let Some(max_v) = data.attr("max").and_then(|s| s.trim().parse::<f64>().ok())
                    && num > max_v
                {
                    return Some(false);
                }
            }
            _ => {}
        }
        Some(true)
    }

    fn subtree_has_constrained_control(&self, root: NodeId) -> bool {
        let Some(node) = self.nodes.get(root.index()) else {
            return false;
        };
        for &child in &node.children {
            if matches!(
                self.get(child).data().tag(),
                Some("input" | "textarea" | "select")
            ) && self.is_control_valid(child).is_some()
            {
                return true;
            }
            if self.subtree_has_constrained_control(child) {
                return true;
            }
        }
        false
    }

    fn subtree_controls_valid(&self, root: NodeId) -> bool {
        let Some(node) = self.nodes.get(root.index()) else {
            return true;
        };
        for &child in &node.children {
            if matches!(
                self.get(child).data().tag(),
                Some("input" | "textarea" | "select")
            ) && self.is_control_valid(child) == Some(false)
            {
                return false;
            }
            if !self.subtree_controls_valid(child) {
                return false;
            }
        }
        true
    }

    /// Evaluate range constraint status (`:in-range` / `:out-of-range`) for `id`.
    /// Returns `None` when `id` has no range limitations or is not a range-able control.
    pub fn is_control_in_range(&self, id: NodeId) -> Option<bool> {
        let node = self.nodes.get(id.index())?;
        let data = node.data();
        if data.tag() != Some("input") {
            return None;
        }
        if data
            .attr("disabled")
            .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
            || data
                .attr("readonly")
                .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
        {
            return None;
        }
        let kind = data
            .attr("type")
            .unwrap_or("text")
            .trim()
            .to_ascii_lowercase();
        if !matches!(kind.as_str(), "number" | "range") {
            return None;
        }
        let min_v = data.attr("min").and_then(|s| s.trim().parse::<f64>().ok());
        let max_v = data.attr("max").and_then(|s| s.trim().parse::<f64>().ok());
        if min_v.is_none() && max_v.is_none() {
            return None;
        }
        let val = self.input_value(id);
        let num = if val.trim().is_empty() {
            if kind == "range" {
                self.range_state(id).3 as f64
            } else {
                return None;
            }
        } else {
            let Ok(parsed) = val.trim().parse::<f64>() else {
                return Some(false);
            };
            parsed
        };
        if let Some(lo) = min_v
            && num < lo
        {
            return Some(false);
        }
        if let Some(hi) = max_v
            && num > hi
        {
            return Some(false);
        }
        Some(true)
    }

    /// Constructable stylesheets attached via `document.adoptedStyleSheets`.
    pub fn adopted_style_sheets(&self) -> &[String] {
        &self.adopted_styles
    }

    /// Replace `document.adoptedStyleSheets` with `sheets`.
    pub fn set_adopted_style_sheets(&mut self, sheets: Vec<String>) {
        self.adopted_styles = sheets
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
    }

    /// Scroll offset `(scrollLeft, scrollTop)` in CSS pixels for element `id`.
    pub fn node_scroll_offset(&self, id: NodeId) -> (f32, f32) {
        self.scroll_offsets
            .get(&id)
            .map(|&(xb, yb)| (f32::from_bits(xb), f32::from_bits(yb)))
            .unwrap_or((0.0, 0.0))
    }

    /// Set scroll offset `(scrollLeft, scrollTop)` in CSS pixels for element `id`.
    pub fn set_node_scroll_offset(&mut self, id: NodeId, x: f32, y: f32) {
        if self.nodes.get(id.index()).is_none() {
            return;
        }
        let cx = if x.is_finite() { x.max(0.0) } else { 0.0 };
        let cy = if y.is_finite() { y.max(0.0) } else { 0.0 };
        if cx == 0.0 && cy == 0.0 {
            self.scroll_offsets.remove(&id);
        } else {
            self.scroll_offsets.insert(id, (cx.to_bits(), cy.to_bits()));
        }
    }

    /// Custom constraint validation message set via `setCustomValidity(msg)` for `id`.
    pub fn custom_validity(&self, id: NodeId) -> &str {
        self.custom_validity
            .get(&id)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// Set or clear the custom constraint validation message (`setCustomValidity`) for `id`.
    pub fn set_custom_validity(&mut self, id: NodeId, message: &str) {
        if self.nodes.get(id.index()).is_none() {
            return;
        }
        if message.is_empty() {
            self.custom_validity.remove(&id);
        } else {
            self.custom_validity.insert(id, message.to_string());
        }
    }

    /// Append one constructable stylesheet CSS text to `document.adoptedStyleSheets`.
    pub fn add_adopted_style_sheet(&mut self, css: String) {
        if !css.trim().is_empty() {
            self.adopted_styles.push(css);
        }
    }

    /// Clone a node (and optionally its descendants when `deep` is true) from `src_doc`
    /// as a detached subtree in `self` (`document.importNode` / `document.adoptNode`).
    pub fn import_subtree(
        &mut self,
        src_doc: &Document,
        src_id: NodeId,
        deep: bool,
    ) -> Option<NodeId> {
        if self.nodes.len() >= MAX_DOM_NODES || src_doc.nodes.get(src_id.index()).is_none() {
            return None;
        }
        if deep {
            self.clone_subtree_detached(src_doc, src_id)
        } else {
            match src_doc.get(src_id).data() {
                NodeData::Text(t) => Some(self.create_text(t)),
                NodeData::Element { tag, attrs } => Some(self.create_element(tag, attrs.clone())),
                NodeData::Document => None,
            }
        }
    }

    /// Intrinsic pixel dimensions `(width, height)` of a `<canvas>` element
    /// (defaults to `300x150` per HTML5 specification, clamped to `1..=2048`).
    pub fn canvas_dimensions(&self, id: NodeId) -> (u32, u32) {
        let Some(node) = self.nodes.get(id.index()) else {
            return (300, 150);
        };
        let w = node
            .data()
            .attr("width")
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(300)
            .clamp(1, 2048);
        let h = node
            .data()
            .attr("height")
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(150)
            .clamp(1, 2048);
        (w, h)
    }

    /// Ensure and return a mutable 2D RGBA backing store for `<canvas>` element `id`.
    pub fn ensure_canvas_surface(&mut self, id: NodeId) -> &mut CanvasSurface {
        let (w, h) = self.canvas_dimensions(id);
        let len = (w as usize) * (h as usize) * 4;
        let entry = self
            .canvas_surfaces
            .entry(id)
            .or_insert_with(|| CanvasSurface {
                width: w,
                height: h,
                rgba: vec![0u8; len],
            });
        if entry.width != w || entry.height != h || entry.rgba.len() != len {
            entry.width = w;
            entry.height = h;
            entry.rgba = vec![0u8; len];
        }
        entry
    }

    /// Borrow the 2D RGBA backing store for `<canvas>` element `id`, if initialized.
    pub fn canvas_surface(&self, id: NodeId) -> Option<&CanvasSurface> {
        self.canvas_surfaces.get(&id)
    }

    /// Replace the 2D RGBA backing store for `<canvas>` element `id`.
    pub fn set_canvas_surface(&mut self, id: NodeId, width: u32, height: u32, rgba: Vec<u8>) {
        let w = width.clamp(1, 2048);
        let h = height.clamp(1, 2048);
        let expected = (w as usize) * (h as usize) * 4;
        let mut buf = rgba;
        buf.resize(expected, 0);
        self.canvas_surfaces.insert(
            id,
            CanvasSurface {
                width: w,
                height: h,
                rgba: buf,
            },
        );
    }

    /// Fill a rectangle on `<canvas>` element `id`'s 2D surface (`fillRect`).
    pub fn canvas_fill_rect(&mut self, id: NodeId, x: i32, y: i32, w: u32, h: u32, rgba: [u8; 4]) {
        let surf = self.ensure_canvas_surface(id);
        let cw = surf.width as i32;
        let ch = surf.height as i32;
        let x0 = x.clamp(0, cw);
        let y0 = y.clamp(0, ch);
        let x1 = (x.saturating_add(w as i32)).clamp(0, cw);
        let y1 = (y.saturating_add(h as i32)).clamp(0, ch);
        for py in y0..y1 {
            for px in x0..x1 {
                let off = ((py as usize) * (surf.width as usize) + (px as usize)) * 4;
                if off + 4 <= surf.rgba.len() {
                    surf.rgba[off..off + 4].copy_from_slice(&rgba);
                }
            }
        }
    }

    /// Clear a rectangle to transparent black on `<canvas>` element `id`'s 2D surface (`clearRect`).
    pub fn canvas_clear_rect(&mut self, id: NodeId, x: i32, y: i32, w: u32, h: u32) {
        self.canvas_fill_rect(id, x, y, w, h, [0, 0, 0, 0]);
    }

    /// Stroke a rectangle outline on `<canvas>` element `id`'s 2D surface (`strokeRect`).
    #[allow(clippy::too_many_arguments)]
    pub fn canvas_stroke_rect(
        &mut self,
        id: NodeId,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        line_width: u32,
        rgba: [u8; 4],
    ) {
        let lw = line_width.max(1);
        self.canvas_fill_rect(id, x, y, w, lw, rgba);
        self.canvas_fill_rect(
            id,
            x,
            y.saturating_add(h as i32).saturating_sub(lw as i32),
            w,
            lw,
            rgba,
        );
        self.canvas_fill_rect(id, x, y, lw, h, rgba);
        self.canvas_fill_rect(
            id,
            x.saturating_add(w as i32).saturating_sub(lw as i32),
            y,
            lw,
            h,
            rgba,
        );
    }

    /// Blit raw `ImageData` RGBA bytes into `<canvas>` element `id`'s 2D surface (`putImageData`).
    pub fn canvas_put_image_data(
        &mut self,
        id: NodeId,
        dx: i32,
        dy: i32,
        src_w: u32,
        src_h: u32,
        src_rgba: &[u8],
    ) {
        let surf = self.ensure_canvas_surface(id);
        let cw = surf.width as i32;
        let ch = surf.height as i32;
        for sy in 0..(src_h as i32) {
            let py = dy + sy;
            if py < 0 || py >= ch {
                continue;
            }
            for sx in 0..(src_w as i32) {
                let px = dx + sx;
                if px < 0 || px >= cw {
                    continue;
                }
                let src_off = ((sy as usize) * (src_w as usize) + (sx as usize)) * 4;
                let dst_off = ((py as usize) * (surf.width as usize) + (px as usize)) * 4;
                if src_off + 4 <= src_rgba.len() && dst_off + 4 <= surf.rgba.len() {
                    surf.rgba[dst_off..dst_off + 4]
                        .copy_from_slice(&src_rgba[src_off..src_off + 4]);
                }
            }
        }
    }

    /// Draw a Bresenham line segment on `<canvas>` element `id`'s 2D surface (`lineTo` / `stroke`).
    pub fn canvas_draw_line(
        &mut self,
        id: NodeId,
        mut x0: i32,
        mut y0: i32,
        x1: i32,
        y1: i32,
        rgba: [u8; 4],
    ) {
        let surf = self.ensure_canvas_surface(id);
        let cw = surf.width as i32;
        let ch = surf.height as i32;
        let dx = (x1 - x0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        for _ in 0..4096 {
            if x0 >= 0 && x0 < cw && y0 >= 0 && y0 < ch {
                let off = ((y0 as usize) * (surf.width as usize) + (x0 as usize)) * 4;
                if off + 4 <= surf.rgba.len() {
                    surf.rgba[off..off + 4].copy_from_slice(&rgba);
                }
            }
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    /// Read back a rectangular RGBA pixel region (`sw * sh * 4` bytes) from `<canvas>` element `id`'s
    /// 2D surface (`getImageData`). Out-of-bounds pixels are transparent black (`[0, 0, 0, 0]`).
    pub fn canvas_get_image_data(&self, id: NodeId, sx: i32, sy: i32, sw: u32, sh: u32) -> Vec<u8> {
        let w = sw.clamp(1, 2048);
        let h = sh.clamp(1, 2048);
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        let Some(surf) = self.canvas_surfaces.get(&id) else {
            return out;
        };
        let cw = surf.width as i32;
        let ch = surf.height as i32;
        for row in 0..(h as i32) {
            let py = sy + row;
            if py < 0 || py >= ch {
                continue;
            }
            for col in 0..(w as i32) {
                let px = sx + col;
                if px < 0 || px >= cw {
                    continue;
                }
                let src_off = ((py as usize) * (surf.width as usize) + (px as usize)) * 4;
                let dst_off = ((row as usize) * (w as usize) + (col as usize)) * 4;
                if src_off + 4 <= surf.rgba.len() && dst_off + 4 <= out.len() {
                    out[dst_off..dst_off + 4].copy_from_slice(&surf.rgba[src_off..src_off + 4]);
                }
            }
        }
        out
    }

    /// Draw a line segment with configurable pixel thickness (`lineWidth`) on `<canvas>` `id`.
    #[allow(clippy::too_many_arguments)]
    pub fn canvas_draw_line_width(
        &mut self,
        id: NodeId,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        width: u32,
        rgba: [u8; 4],
    ) {
        let w = width.max(1) as i32;
        if w <= 1 {
            self.canvas_draw_line(id, x0, y0, x1, y1, rgba);
            return;
        }
        let half = w / 2;
        for oy in -half..=(w - 1 - half) {
            for ox in -half..=(w - 1 - half) {
                self.canvas_draw_line(id, x0 + ox, y0 + oy, x1 + ox, y1 + oy, rgba);
            }
        }
    }

    /// Fill a disk/circle centered at `(cx, cy)` with radius `r` on `<canvas>` `id`.
    pub fn canvas_fill_circle(&mut self, id: NodeId, cx: i32, cy: i32, r: i32, rgba: [u8; 4]) {
        if r <= 0 {
            return;
        }
        let surf = self.ensure_canvas_surface(id);
        let cw = surf.width as i32;
        let ch = surf.height as i32;
        let r2 = (r as i64) * (r as i64);
        let y0 = (cy - r).clamp(0, ch);
        let y1 = (cy + r + 1).clamp(0, ch);
        let x0 = (cx - r).clamp(0, cw);
        let x1 = (cx + r + 1).clamp(0, cw);
        for py in y0..y1 {
            let dy = (py - cy) as i64;
            for px in x0..x1 {
                let dx = (px - cx) as i64;
                if dx * dx + dy * dy <= r2 {
                    let off = ((py as usize) * (surf.width as usize) + (px as usize)) * 4;
                    if off + 4 <= surf.rgba.len() {
                        surf.rgba[off..off + 4].copy_from_slice(&rgba);
                    }
                }
            }
        }
    }

    /// Fill a closed polygon defined by `points` using scanline even-odd/nonzero winding on `<canvas>` `id`.
    pub fn canvas_fill_polygon(&mut self, id: NodeId, points: &[(f32, f32)], rgba: [u8; 4]) {
        if points.len() < 3 {
            return;
        }
        let surf = self.ensure_canvas_surface(id);
        let cw = surf.width as i32;
        let ch = surf.height as i32;
        let mut min_y = ch;
        let mut max_y = 0i32;
        for &(_, y) in points {
            min_y = min_y.min(y.floor() as i32);
            max_y = max_y.max(y.ceil() as i32);
        }
        let y0 = min_y.clamp(0, ch);
        let y1 = (max_y + 1).clamp(0, ch);
        let mut nodes = Vec::with_capacity(points.len());
        for py in y0..y1 {
            let scan_y = py as f32 + 0.5;
            nodes.clear();
            let mut j = points.len() - 1;
            for i in 0..points.len() {
                let (xi, yi) = points[i];
                let (xj, yj) = points[j];
                if (yi < scan_y && yj >= scan_y) || (yj < scan_y && yi >= scan_y) {
                    let dy = yj - yi;
                    if dy.abs() > 1e-6 {
                        nodes.push(xi + (scan_y - yi) / dy * (xj - xi));
                    }
                }
                j = i;
            }
            nodes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            for pair in nodes.as_chunks::<2>().0 {
                let x_start = (pair[0].floor() as i32).clamp(0, cw);
                let x_end = (pair[1].ceil() as i32).clamp(0, cw);
                for px in x_start..x_end {
                    let off = ((py as usize) * (surf.width as usize) + (px as usize)) * 4;
                    if off + 4 <= surf.rgba.len() {
                        surf.rgba[off..off + 4].copy_from_slice(&rgba);
                    }
                }
            }
        }
    }

    /// Fill a rectangle with a multi-stop linear gradient on `<canvas>` `id`.
    #[allow(clippy::too_many_arguments)]
    pub fn canvas_fill_linear_gradient(
        &mut self,
        id: NodeId,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        gx0: f32,
        gy0: f32,
        gx1: f32,
        gy1: f32,
        stops: &[(f32, [u8; 4])],
    ) {
        if stops.is_empty() {
            return;
        }
        let surf = self.ensure_canvas_surface(id);
        let cw = surf.width as i32;
        let ch = surf.height as i32;
        let x0 = x.clamp(0, cw);
        let y0 = y.clamp(0, ch);
        let x1 = (x.saturating_add(w as i32)).clamp(0, cw);
        let y1 = (y.saturating_add(h as i32)).clamp(0, ch);
        let vx = gx1 - gx0;
        let vy = gy1 - gy0;
        let len2 = (vx * vx + vy * vy).max(1e-6);
        for py in y0..y1 {
            for px in x0..x1 {
                let t = (((px as f32 + 0.5 - gx0) * vx + (py as f32 + 0.5 - gy0) * vy) / len2)
                    .clamp(0.0, 1.0);
                let rgba = sample_gradient_stops(stops, t);
                let off = ((py as usize) * (surf.width as usize) + (px as usize)) * 4;
                if off + 4 <= surf.rgba.len() {
                    surf.rgba[off..off + 4].copy_from_slice(&rgba);
                }
            }
        }
    }

    /// Rasterize a compact 5x7 bitmap glyph string onto `<canvas>` element `id`'s 2D surface (`fillText`).
    pub fn canvas_draw_text_bitmap(
        &mut self,
        id: NodeId,
        text: &str,
        x: i32,
        y: i32,
        rgba: [u8; 4],
    ) {
        let mut cx = x;
        let top_y = y.saturating_sub(7);
        for ch in text.chars().take(128) {
            if !ch.is_whitespace() {
                self.canvas_fill_rect(id, cx, top_y, 5, 7, rgba);
            }
            cx = cx.saturating_add(6);
        }
    }

    /// The document node. Always slot 0. Not the `html` element.
    pub fn root(&self) -> NodeId {
        NodeId(0)
    }

    /// The `html` element created by [`Self::new`].
    pub fn html(&self) -> NodeId {
        self.html
    }

    /// Alias for [`Self::html`] (`document.documentElement`).
    pub fn document_element(&self) -> NodeId {
        self.html
    }

    /// The `head` element created by [`Self::new`].
    pub fn head(&self) -> NodeId {
        self.head
    }

    /// Alias for [`Self::head`] (`document.head`).
    pub fn head_element(&self) -> NodeId {
        self.head
    }

    /// The `body` element created by [`Self::new`].
    pub fn body(&self) -> NodeId {
        self.body
    }

    /// Alias for [`Self::body`] (`document.body`).
    pub fn body_element(&self) -> NodeId {
        self.body
    }

    /// Slots allocated, including detached nodes that are no longer in the tree.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Borrow the node at `id`. Panics if `id` is outside this document.
    pub fn get(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    /// Mutably borrow the node. `None` if `id` is outside this document.
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(id.index())
    }

    /// Child ids in tree order, borrowed from the parent. Panics if `id` is outside.
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id.index()].children
    }

    /// Allocate an element that is not yet in the tree (`parent` is `None`).
    ///
    /// `tag` is copied and lowercased. `attrs` is consumed; each name is lowercased.
    /// The returned id is owned by this document.
    pub fn create_element(&mut self, tag: &str, attrs: Vec<(String, String)>) -> NodeId {
        let mut data = NodeData::element(tag);
        if let NodeData::Element { attrs: slot, .. } = &mut data {
            *slot = attrs
                .into_iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value))
                .collect();
        }
        self.alloc(data)
    }

    /// Allocate a text node that is not yet in the tree. `text` is copied.
    pub fn create_text(&mut self, text: &str) -> NodeId {
        self.alloc(NodeData::Text(text.to_string()))
    }

    /// Set one attribute. No-op when `id` is outside the document or not an element.
    pub fn set_attribute(&mut self, id: NodeId, name: &str, value: &str) {
        if let Some(node) = self.nodes.get_mut(id.index()) {
            node.data.set_attr(name, value);
            self.mark_style_dirty(id);
        }
    }

    /// Remove one attribute. False when `id` is missing or the attribute was absent.
    pub fn remove_attribute(&mut self, id: NodeId, name: &str) -> bool {
        let removed = self
            .nodes
            .get_mut(id.index())
            .is_some_and(|node| node.data.remove_attr(name));
        if removed {
            self.mark_style_dirty(id);
        }
        removed
    }

    /// Add a class token on `id`. No-op when `id` is missing or not an element.
    pub fn add_class(&mut self, id: NodeId, class_name: &str) {
        if let Some(node) = self.nodes.get_mut(id.index()) {
            node.data.add_class(class_name);
            self.mark_style_dirty(id);
        }
    }

    /// Remove a class token on `id`. No-op when `id` is missing.
    pub fn remove_class(&mut self, id: NodeId, class_name: &str) {
        if let Some(node) = self.nodes.get_mut(id.index()) {
            node.data.remove_class(class_name);
            self.mark_style_dirty(id);
        }
    }

    /// Toggle a class on `id`. False when `id` is missing, or when the class
    /// was removed. True when the class is present after the call.
    pub fn toggle_class(&mut self, id: NodeId, class_name: &str) -> bool {
        let Some(node) = self.nodes.get_mut(id.index()) else {
            return false;
        };
        let res = node.data.toggle_class(class_name);
        self.mark_style_dirty(id);
        res
    }

    /// Owned current value of a form control.
    ///
    /// A missing node yields an empty string. A `select` uses [`Self::select_value`].
    /// Any element with a `value` attribute returns that attribute, including a
    /// `textarea`. A `textarea` without `value` returns its text content.
    /// Other nodes without `value` yield an empty string.
    pub fn input_value(&self, id: NodeId) -> String {
        let Some(node) = self.nodes.get(id.index()) else {
            return String::new();
        };
        let data = node.data();
        if data.tag() == Some("select") {
            return self.select_value(id);
        }
        if let Some(val) = data.attr("value") {
            return val.to_string();
        }
        if data.tag() == Some("textarea") {
            return self.text_content(id);
        }
        String::new()
    }

    /// Store `value` on the `value` attribute. For a `textarea`, also replace
    /// its text content with the same string. No-op when `id` is missing.
    /// `value` is copied.
    pub fn set_input_value(&mut self, id: NodeId, value: &str) {
        let tag = self
            .nodes
            .get(id.index())
            .and_then(|n| n.data().tag().map(str::to_string));
        self.set_attribute(id, "value", value);
        if tag.as_deref() == Some("textarea") {
            self.set_text_content(id, value);
        }
        let max_len = value.chars().count();
        if let Some(cur) = self.input_cursors.get_mut(&id) {
            *cur = (*cur).min(max_len);
        }
    }

    /// Character caret index (`0..=chars.count()`) for text control `id`.
    /// Defaults to the end of [`Self::input_value`] when not explicitly set.
    pub fn input_cursor(&self, id: NodeId) -> usize {
        let len = self.input_value(id).chars().count();
        self.input_cursors.get(&id).copied().unwrap_or(len).min(len)
    }

    /// Set the character caret index (`0..=chars.count()`) for text control `id`.
    pub fn set_input_cursor(&mut self, id: NodeId, pos: usize) {
        let len = self.input_value(id).chars().count();
        self.input_cursors.insert(id, pos.min(len));
    }

    /// Move the character caret one position left in text control `id`.
    pub fn move_input_cursor_left(&mut self, id: NodeId) -> usize {
        let next = self.input_cursor(id).saturating_sub(1);
        self.input_cursors.insert(id, next);
        next
    }

    /// Move the character caret one position right in text control `id`.
    pub fn move_input_cursor_right(&mut self, id: NodeId) -> usize {
        let len = self.input_value(id).chars().count();
        let next = (self.input_cursor(id) + 1).min(len);
        self.input_cursors.insert(id, next);
        next
    }

    /// Move the character caret to index `0` in text control `id`.
    pub fn move_input_cursor_home(&mut self, id: NodeId) -> usize {
        self.input_cursors.insert(id, 0);
        0
    }

    /// Move the character caret to the end of text control `id`.
    pub fn move_input_cursor_end(&mut self, id: NodeId) -> usize {
        let len = self.input_value(id).chars().count();
        self.input_cursors.insert(id, len);
        len
    }

    /// Insert `text` at the current caret index in text control `id` and advance the caret.
    pub fn insert_input_text_at_cursor(&mut self, id: NodeId, text: &str) -> String {
        let mut chars: Vec<char> = self.input_value(id).chars().collect();
        let cur = self.input_cursor(id).min(chars.len());
        let inserted: Vec<char> = text.chars().collect();
        let ins_len = inserted.len();
        chars.splice(cur..cur, inserted);
        let next_val: String = chars.into_iter().collect();
        self.set_input_value(id, &next_val);
        self.input_cursors.insert(id, cur + ins_len);
        next_val
    }

    /// Delete the character immediately before the caret in text control `id` (`Backspace`).
    pub fn backspace_input_at_cursor(&mut self, id: NodeId) -> String {
        let mut chars: Vec<char> = self.input_value(id).chars().collect();
        let cur = self.input_cursor(id).min(chars.len());
        if cur > 0 {
            chars.remove(cur - 1);
            let next_val: String = chars.into_iter().collect();
            self.set_input_value(id, &next_val);
            self.input_cursors.insert(id, cur - 1);
            next_val
        } else {
            chars.into_iter().collect()
        }
    }

    /// Delete the character at the caret in text control `id` (`Delete`).
    pub fn delete_input_at_cursor(&mut self, id: NodeId) -> String {
        let mut chars: Vec<char> = self.input_value(id).chars().collect();
        let cur = self.input_cursor(id).min(chars.len());
        if cur < chars.len() {
            chars.remove(cur);
            let next_val: String = chars.into_iter().collect();
            self.set_input_value(id, &next_val);
            self.input_cursors.insert(id, cur);
            next_val
        } else {
            chars.into_iter().collect()
        }
    }

    /// Delete the word immediately before the caret in text control `id` (`Ctrl+Backspace`).
    pub fn delete_input_word_at_cursor(&mut self, id: NodeId) -> String {
        let mut chars: Vec<char> = self.input_value(id).chars().collect();
        let cur = self.input_cursor(id).min(chars.len());
        if cur == 0 {
            return chars.into_iter().collect();
        }
        let mut start = cur;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        chars.drain(start..cur);
        let next_val: String = chars.into_iter().collect();
        self.set_input_value(id, &next_val);
        self.input_cursors.insert(id, start);
        next_val
    }

    /// True when a `checked` attribute is present and is not the word `false`.
    /// A missing node is false.
    pub fn is_checked(&self, id: NodeId) -> bool {
        let Some(node) = self.nodes.get(id.index()) else {
            return false;
        };
        node.data()
            .attr("checked")
            .is_some_and(|val| !val.eq_ignore_ascii_case("false"))
    }

    /// Set `checked="checked"`, or remove `checked` when `checked` is false.
    pub fn set_checked(&mut self, id: NodeId, checked: bool) {
        if checked {
            self.set_attribute(id, "checked", "checked");
        } else {
            self.remove_attribute(id, "checked");
        }
    }

    /// Flip [`Self::is_checked`] and return the new state.
    pub fn toggle_checked(&mut self, id: NodeId) -> bool {
        let next = !self.is_checked(id);
        self.set_checked(id, next);
        next
    }

    /// True for `textarea`, or for `input` whose type is empty, `text`,
    /// `search`, `password`, `email`, `url`, `tel`, or `number`.
    /// A missing node is false. A missing `type` counts as text.
    pub fn is_text_input(&self, id: NodeId) -> bool {
        let Some(node) = self.nodes.get(id.index()) else {
            return false;
        };
        let data = node.data();
        match data.tag() {
            Some("textarea") => true,
            Some("input") => {
                let kind = data.attr("type").unwrap_or("text").trim();
                matches!(
                    kind.to_ascii_lowercase().as_str(),
                    "" | "text" | "search" | "password" | "email" | "url" | "tel" | "number"
                )
            }
            _ => false,
        }
    }

    /// True only for `<input type="checkbox">`. A missing node is false.
    pub fn is_checkbox_input(&self, id: NodeId) -> bool {
        let Some(node) = self.nodes.get(id.index()) else {
            return false;
        };
        let data = node.data();
        data.tag() == Some("input")
            && data
                .attr("type")
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("checkbox"))
    }

    /// True only for `<input type="radio">`. A missing node is false.
    pub fn is_radio_input(&self, id: NodeId) -> bool {
        let Some(node) = self.nodes.get(id.index()) else {
            return false;
        };
        let data = node.data();
        data.tag() == Some("input")
            && data
                .attr("type")
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("radio"))
    }

    /// Mark `id` checked as a radio button and uncheck sibling radio buttons
    /// sharing the same non-empty `name` within the same `<form>` (or document
    /// when not inside a `<form>`). Returns `true` when `id` is a radio input.
    pub fn select_radio(&mut self, id: NodeId) -> bool {
        if !self.is_radio_input(id) {
            return false;
        }
        let name = self
            .get(id)
            .data()
            .attr("name")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if !name.is_empty() {
            let form_owner = self.enclosing_form(id);
            let candidates = self.get_elements_by_name(&name);
            for other in candidates {
                if other != id
                    && self.is_radio_input(other)
                    && self.enclosing_form(other) == form_owner
                {
                    self.set_checked(other, false);
                }
            }
        }
        self.set_checked(id, true);
        true
    }

    /// True when `details_id` is a `<details>` element with an active `open` attribute.
    pub fn is_details_open(&self, details_id: NodeId) -> bool {
        let Some(node) = self.nodes.get(details_id.index()) else {
            return false;
        };
        let data = node.data();
        data.tag() == Some("details")
            && data
                .attr("open")
                .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
    }

    /// If `start` is inside a `<summary>` that is a direct child of a `<details>`
    /// element, returns that parent `<details>` `NodeId`.
    pub fn enclosing_summary_details(&self, start: NodeId) -> Option<NodeId> {
        let mut cursor = Some(start);
        while let Some(curr) = cursor {
            let node = self.nodes.get(curr.index())?;
            if node.data().tag() == Some("summary")
                && let Some(parent) = self.parent_element(curr)
                && self.get(parent).data().tag() == Some("details")
            {
                return Some(parent);
            }
            cursor = node.parent();
        }
        None
    }

    /// Toggle the `open` attribute on a `<details>` element and return the new open state.
    pub fn toggle_details(&mut self, details_id: NodeId) -> bool {
        if self
            .nodes
            .get(details_id.index())
            .and_then(|n| n.data().tag())
            != Some("details")
        {
            return false;
        }
        let next = !self.is_details_open(details_id);
        if next {
            self.set_attribute(details_id, "open", "open");
        } else {
            self.remove_attribute(details_id, "open");
        }
        next
    }

    /// Resolve a click on or inside a `<label>` to its associated form control
    /// (`for="target_id"` or the first descendant `<input>`, `<textarea>`,
    /// `<select>`, or `<button>`).
    pub fn label_target(&self, start: NodeId) -> Option<NodeId> {
        let mut cursor = Some(start);
        let mut label_id = None;
        while let Some(curr) = cursor {
            let node = self.nodes.get(curr.index())?;
            if matches!(
                node.data().tag(),
                Some("input" | "textarea" | "select" | "button")
            ) {
                return Some(curr);
            }
            if node.data().tag() == Some("label") {
                label_id = Some(curr);
                break;
            }
            cursor = node.parent();
        }
        let label = label_id?;
        if let Some(for_attr) = self.get(label).data().attr("for") {
            let trimmed = for_attr.trim();
            if !trimmed.is_empty()
                && let Some(target) = self.get_element_by_id(trimmed)
            {
                return Some(target);
            }
        }
        self.find_first_labelable_control(label)
    }

    fn find_first_labelable_control(&self, root: NodeId) -> Option<NodeId> {
        for &child in self.children(root) {
            let data = self.get(child).data();
            match data.tag() {
                Some("input") => {
                    let kind = data.attr("type").unwrap_or("text").trim();
                    if !kind.eq_ignore_ascii_case("hidden") {
                        return Some(child);
                    }
                }
                Some("textarea" | "select" | "button") => return Some(child),
                _ => {}
            }
            if let Some(found) = self.find_first_labelable_control(child) {
                return Some(found);
            }
        }
        None
    }

    /// All visible, non-disabled text `<input>` and `<textarea>` elements in tree order.
    pub fn focusable_inputs(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.collect_focusable_inputs(self.body(), &mut out);
        out
    }

    fn collect_focusable_inputs(&self, id: NodeId, out: &mut Vec<NodeId>) {
        let Some(node) = self.nodes.get(id.index()) else {
            return;
        };
        let data = node.data();
        if let Some(tag) = data.tag() {
            if matches!(tag, "noscript" | "template" | "head" | "script" | "style") {
                return;
            }
            if data.attr("hidden").is_some() {
                return;
            }
            if tag == "dialog"
                && !data
                    .attr("open")
                    .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
            {
                return;
            }
            if self.is_text_input(id)
                && data.attr("disabled").is_none()
                && data.attr("readonly").is_none()
            {
                out.push(id);
                return;
            }
            if tag == "details" && !self.is_details_open(id) {
                for &child in self.children(id) {
                    if self.get(child).data().tag() == Some("summary") {
                        self.collect_focusable_inputs(child, out);
                    }
                }
                return;
            }
        }
        for &child in self.children(id) {
            self.collect_focusable_inputs(child, out);
        }
    }

    /// True when `id` is a `select` element. A missing node is false.
    pub fn is_select_control(&self, id: NodeId) -> bool {
        self.nodes
            .get(id.index())
            .is_some_and(|n| n.data().tag() == Some("select"))
    }

    /// Direct `option` children, in tree order. An owned list of ids.
    /// A missing node yields an empty vec. Nested options are not walked.
    pub fn select_options(&self, select_id: NodeId) -> Vec<NodeId> {
        let Some(node) = self.nodes.get(select_id.index()) else {
            return Vec::new();
        };
        node.children
            .iter()
            .copied()
            .filter(|cid| self.get(*cid).data().tag() == Some("option"))
            .collect()
    }

    /// The first option with `selected` set and not equal to `false`.
    /// If none is marked, the first option. `None` when there are no options.
    pub fn selected_option(&self, select_id: NodeId) -> Option<NodeId> {
        let opts = self.select_options(select_id);
        opts.iter()
            .copied()
            .find(|oid| {
                self.get(*oid)
                    .data()
                    .attr("selected")
                    .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
            })
            .or_else(|| opts.first().copied())
    }

    /// Owned value of the selected option: its `value` attribute, or its
    /// trimmed text when `value` is absent. Empty when nothing is selected.
    pub fn select_value(&self, select_id: NodeId) -> String {
        let Some(opt_id) = self.selected_option(select_id) else {
            return String::new();
        };
        if let Some(val) = self.get(opt_id).data().attr("value") {
            return val.to_string();
        }
        self.text_content(opt_id).trim().to_string()
    }

    /// Owned visible label of the selected option. Falls back to
    /// [`Self::select_value`] when the option's text is empty. Empty when
    /// nothing is selected.
    pub fn select_display_label(&self, select_id: NodeId) -> String {
        let Some(opt_id) = self.selected_option(select_id) else {
            return String::new();
        };
        let text = self.text_content(opt_id).trim().to_string();
        if !text.is_empty() {
            text
        } else {
            self.select_value(select_id)
        }
    }

    /// Mark the next direct option as `selected` and clear the others.
    ///
    /// Wraps to the first option. Also writes the new value onto the `select`.
    /// Returns that owned value. `None` when `select_id` has no option children.
    pub fn cycle_select_option(&mut self, select_id: NodeId) -> Option<String> {
        let opts = self.select_options(select_id);
        if opts.is_empty() {
            return None;
        }
        let current = self.selected_option(select_id);
        let next_idx = match current.and_then(|c| opts.iter().position(|&id| id == c)) {
            Some(idx) => (idx + 1) % opts.len(),
            None => 0,
        };
        for (i, &oid) in opts.iter().enumerate() {
            if i == next_idx {
                self.set_attribute(oid, "selected", "selected");
            } else {
                self.remove_attribute(oid, "selected");
            }
        }
        let new_val = self.select_value(select_id);
        self.set_attribute(select_id, "value", &new_val);
        Some(new_val)
    }

    /// Return `(value, display_label, is_selected)` for every direct `<option>` child of `select_id`.
    pub fn select_options_list(&self, select_id: NodeId) -> Vec<(String, String, bool)> {
        let opts = self.select_options(select_id);
        let selected = self.selected_option(select_id);
        opts.into_iter()
            .map(|oid| {
                let text = self.text_content(oid).trim().to_string();
                let val = self
                    .get(oid)
                    .data()
                    .attr("value")
                    .map(str::to_string)
                    .unwrap_or_else(|| text.clone());
                let label = if text.is_empty() { val.clone() } else { text };
                (val, label, selected == Some(oid))
            })
            .collect()
    }

    /// Select the direct `<option>` at `index` inside `select_id`, updating `selected` attributes
    /// and the `<select>`'s `value` attribute. Returns the selected option's value.
    pub fn select_option_by_index(&mut self, select_id: NodeId, index: usize) -> Option<String> {
        let opts = self.select_options(select_id);
        if index >= opts.len() {
            return None;
        }
        for (i, &oid) in opts.iter().enumerate() {
            if i == index {
                self.set_attribute(oid, "selected", "selected");
            } else {
                self.remove_attribute(oid, "selected");
            }
        }
        let new_val = self.select_value(select_id);
        self.set_attribute(select_id, "value", &new_val);
        Some(new_val)
    }

    /// True only for `<input type="range">`. A missing node is false.
    pub fn is_range_input(&self, id: NodeId) -> bool {
        let Some(node) = self.nodes.get(id.index()) else {
            return false;
        };
        let data = node.data();
        data.tag() == Some("input")
            && data
                .attr("type")
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("range"))
    }

    /// Return `(min, max, step, value)` for an `<input type="range">` element.
    /// Defaults to `min = 0.0`, `max = 100.0`, `step = 1.0`, and `value = (min + max) * 0.5`.
    pub fn range_state(&self, id: NodeId) -> (f32, f32, f32, f32) {
        let Some(node) = self.nodes.get(id.index()) else {
            return (0.0, 100.0, 1.0, 50.0);
        };
        let data = node.data();
        let min = data
            .attr("min")
            .and_then(|s| s.trim().parse::<f32>().ok())
            .unwrap_or(0.0);
        let max = data
            .attr("max")
            .and_then(|s| s.trim().parse::<f32>().ok())
            .unwrap_or(100.0)
            .max(min + f32::EPSILON);
        let step = data
            .attr("step")
            .and_then(|s| s.trim().parse::<f32>().ok())
            .filter(|&v| v > 0.0)
            .unwrap_or(1.0);
        let default_val = min + (max - min) * 0.5;
        let val = data
            .attr("value")
            .and_then(|s| s.trim().parse::<f32>().ok())
            .unwrap_or(default_val)
            .clamp(min, max);
        (min, max, step, val)
    }

    /// Set the value of `<input type="range">` `id` from a normalized horizontal ratio (`0.0..=1.0`),
    /// quantizing to `step` within `min..=max`, and return the formatted value string.
    pub fn set_range_ratio(&mut self, id: NodeId, ratio: f32) -> String {
        let (min, max, step, _) = self.range_state(id);
        let clamped = ratio.clamp(0.0, 1.0);
        let raw = min + clamped * (max - min);
        let quantized = if step > 0.0 {
            let steps = ((raw - min) / step).round();
            (min + steps * step).clamp(min, max)
        } else {
            raw.clamp(min, max)
        };
        let formatted = if step.fract().abs() < 1e-5 && min.fract().abs() < 1e-5 {
            format!("{}", quantized.round() as i64)
        } else {
            let s = format!("{quantized:.4}");
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        };
        self.set_input_value(id, &formatted);
        formatted
    }

    /// Normalized fill ratio (`0.0..=1.0`) for `<input type="range">`, `<progress>`, or `<meter>`.
    /// Returns `None` for other elements.
    pub fn control_fill_ratio(&self, id: NodeId) -> Option<f32> {
        let node = self.nodes.get(id.index())?;
        let data = node.data();
        match data.tag()? {
            "input" if self.is_range_input(id) => {
                let (min, max, _, val) = self.range_state(id);
                Some(((val - min) / (max - min).max(f32::EPSILON)).clamp(0.0, 1.0))
            }
            "progress" => {
                let max = data
                    .attr("max")
                    .and_then(|s| s.trim().parse::<f32>().ok())
                    .filter(|&v| v > 0.0)
                    .unwrap_or(1.0);
                let val = data
                    .attr("value")
                    .and_then(|s| s.trim().parse::<f32>().ok())
                    .unwrap_or(max * 0.5);
                Some((val / max).clamp(0.0, 1.0))
            }
            "meter" => {
                let min = data
                    .attr("min")
                    .and_then(|s| s.trim().parse::<f32>().ok())
                    .unwrap_or(0.0);
                let max = data
                    .attr("max")
                    .and_then(|s| s.trim().parse::<f32>().ok())
                    .unwrap_or(1.0)
                    .max(min + f32::EPSILON);
                let val = data
                    .attr("value")
                    .and_then(|s| s.trim().parse::<f32>().ok())
                    .unwrap_or(min);
                Some(((val - min) / (max - min)).clamp(0.0, 1.0))
            }
            _ => None,
        }
    }

    /// True for `<input type="submit">`, or a `button` whose type is `submit`
    /// or omitted. A missing node is false.
    pub fn is_submit_control(&self, id: NodeId) -> bool {
        let Some(node) = self.nodes.get(id.index()) else {
            return false;
        };
        let data = node.data();
        match data.tag() {
            Some("input") => data
                .attr("type")
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("submit")),
            Some("button") => {
                let kind = data.attr("type").unwrap_or("submit").trim();
                kind.eq_ignore_ascii_case("submit")
            }
            _ => false,
        }
    }

    /// The nearest `form` ancestor, including `id` itself.
    /// `None` if `id` is missing or no ancestor is a form.
    pub fn enclosing_form(&self, id: NodeId) -> Option<NodeId> {
        let mut cursor = Some(id);
        while let Some(curr) = cursor {
            let node = self.nodes.get(curr.index())?;
            if node.data().tag() == Some("form") {
                return Some(curr);
            }
            cursor = node.parent();
        }
        None
    }

    /// Owned `(action, method, urlencoded body)` for a `form` node.
    ///
    /// A missing node yields `("", "GET", "")`. `method` is `POST` only when
    /// the attribute is `POST` ignoring case; every other value, including a
    /// missing one, is `GET`. `action` is the trimmed attribute or empty.
    /// Named `input`, `textarea`, and `select` descendants are included.
    /// Checkboxes and radios are included only when checked, defaulting the
    /// value to `on`. Submit, button, and reset inputs are skipped.
    /// Successful controls are joined with `&` using [`urlencode_component`].
    pub fn serialize_form(&self, form_id: NodeId) -> (String, String, String) {
        let Some(form_node) = self.nodes.get(form_id.index()) else {
            return (String::new(), "GET".to_string(), String::new());
        };
        let action = form_node
            .data()
            .attr("action")
            .unwrap_or("")
            .trim()
            .to_string();
        let method = form_node
            .data()
            .attr("method")
            .unwrap_or("GET")
            .trim()
            .to_ascii_uppercase();
        let method = if method == "POST" {
            "POST".to_string()
        } else {
            "GET".to_string()
        };
        let mut pairs = Vec::new();
        self.collect_form_pairs(form_id, &mut pairs);
        let encoded = pairs
            .into_iter()
            .map(|(k, v)| format!("{}={}", urlencode_component(&k), urlencode_component(&v)))
            .collect::<Vec<_>>()
            .join("&");
        (action, method, encoded)
    }

    /// Replace the children of `id` with one text node, or with nothing when
    /// `text` is empty.
    ///
    /// Previous children stay allocated with `parent` cleared. A missing `id`
    /// is a no-op. `text` is copied.
    pub fn set_text_content(&mut self, id: NodeId, text: &str) {
        if let Some(node) = self.nodes.get_mut(id.index())
            && let NodeData::Text(slot) = &mut node.data
        {
            *slot = text.to_string();
            let parent = node.parent;
            if let Some(p) = parent {
                self.mark_style_dirty(p);
            }
            self.mark_style_dirty(id);
            return;
        }
        if self.nodes.get(id.index()).is_none() {
            return;
        }
        let old_children = std::mem::take(&mut self.nodes[id.index()].children);
        for child in old_children {
            if self.shadow_hosts.contains_key(&child) {
                self.nodes[id.index()].children.push(child);
                continue;
            }
            if let Some(node) = self.nodes.get_mut(child.index()) {
                node.parent = None;
            }
        }
        self.mark_style_dirty(id);
        if !text.is_empty() {
            let text_id = self.create_text(text);
            let _ = self.append(id, text_id);
        }
    }

    /// Make `child` the last child of `parent`.
    ///
    /// Both ids must already be in this document. On success the child is
    /// detached from its previous parent. [`TreeError::Missing`] means an id
    /// is out of range. [`TreeError::Cycle`] means the child is the parent or
    /// an ancestor of the parent. The error leaves the tree unchanged.
    pub fn append(&mut self, parent: NodeId, child: NodeId) -> Result<(), TreeError> {
        if self.nodes.get(parent.index()).is_none() || self.nodes.get(child.index()).is_none() {
            return Err(TreeError::Missing);
        }
        if parent == child {
            return Err(TreeError::Cycle);
        }
        let mut cursor = Some(parent);
        while let Some(id) = cursor {
            if id == child {
                return Err(TreeError::Cycle);
            }
            cursor = self.nodes[id.index()].parent;
        }
        self.detach(child);
        self.nodes[child.index()].parent = Some(parent);
        self.nodes[parent.index()].children.push(child);
        self.mark_style_dirty(parent);
        self.mark_style_dirty(child);
        Ok(())
    }

    /// Owned trimmed text of the first `title` element. Empty if there is none.
    pub fn title(&self) -> String {
        let Some(id) = self.find_element(self.root(), "title") else {
            return String::new();
        };
        self.text_content(id).trim().to_string()
    }

    /// Replace the first `title` element's text, or create one under `head`.
    /// `title` is copied.
    pub fn set_title(&mut self, title: &str) {
        let title_id = match self.find_element(self.root(), "title") {
            Some(id) => id,
            None => {
                let id = self.create_element("title", Vec::new());
                let _ = self.append(self.head, id);
                id
            }
        };
        self.set_text_content(title_id, title);
    }

    /// Serialize the live tree as HTML, starting at the `html` element.
    ///
    /// Detached nodes are left out. Text and attribute values are escaped.
    /// The parser decodes those escapes, so a later `parse_html` of this
    /// string restores the text. Comments are not stored on the tree, so they
    /// are not written back.
    pub fn to_html(&self) -> String {
        let mut out = String::from("<!DOCTYPE html>");
        self.write_html(self.html, &mut out);
        out
    }

    /// Write `id` and its live children. The tag string is copied before the
    /// child walk so the recursive borrow can use `self` again.
    fn write_html(&self, id: NodeId, out: &mut String) {
        match self.get(id).data() {
            NodeData::Text(text) => push_escaped(out, text, false),
            NodeData::Document => {
                let kids: Vec<NodeId> = self.children(id).to_vec();
                for child in kids {
                    self.write_html(child, out);
                }
            }
            NodeData::Element { tag, attrs } => {
                let tag = tag.clone();
                let attrs = attrs.clone();
                out.push('<');
                out.push_str(&tag);
                for (name, value) in &attrs {
                    out.push(' ');
                    out.push_str(name);
                    out.push_str("=\"");
                    push_escaped(out, value, true);
                    out.push('"');
                }
                let kids: Vec<NodeId> = self.children(id).to_vec();
                if kids.is_empty() && is_void_tag(&tag) {
                    out.push('>');
                    return;
                }
                out.push('>');
                for child in kids {
                    self.write_html(child, out);
                }
                out.push_str("</");
                out.push_str(&tag);
                out.push('>');
            }
        }
    }

    /// First element in tree order whose `id` attribute equals `target_id`.
    ///
    /// An empty `target_id` is `None`. The match is case-sensitive. The id is
    /// copied; the document is borrowed.
    pub fn get_element_by_id(&self, target_id: &str) -> Option<NodeId> {
        if target_id.is_empty() {
            return None;
        }
        self.find_by_id(self.root(), target_id)
    }

    /// Resolve a URL fragment (`#section` or `section`) to a target element in tree order.
    ///
    /// Follows HTML fragment navigation rules:
    /// 1. Strips any leading `#` and percent-decodes `%XX` sequences.
    /// 2. Searches for the first element with matching `id`.
    /// 3. Falls back to the first `<a name="...">` element with matching `name`.
    /// 4. When the fragment is `"top"` (case-insensitive) and no element matched, returns `html`.
    pub fn find_fragment_target(&self, fragment: &str) -> Option<NodeId> {
        let raw = fragment.trim().strip_prefix('#').unwrap_or(fragment.trim());
        if raw.is_empty() {
            return None;
        }
        let decoded = percent_decode_fragment(raw);
        if let Some(id) = self.get_element_by_id(&decoded) {
            return Some(id);
        }
        if decoded != raw
            && let Some(id) = self.get_element_by_id(raw)
        {
            return Some(id);
        }
        for id in self.get_elements_by_name(&decoded) {
            if self.get(id).data().tag() == Some("a") {
                return Some(id);
            }
        }
        if raw.eq_ignore_ascii_case("top") {
            return Some(self.html());
        }
        None
    }

    /// First element in tree order that matches a simple selector.
    ///
    /// Accepted forms: `tag`, `#id`, `.class`, `tag#id`, `tag.class`.
    /// Whitespace is trimmed. An empty selector is `None`. Combinators are
    /// not supported; a descendant selector does not match. `None` if nothing matches.
    pub fn query_selector(&self, selector: &str) -> Option<NodeId> {
        let selector = selector.trim();
        if selector.is_empty() {
            return None;
        }
        self.find_by_simple_selector(self.root(), selector)
    }

    /// Every element matching [`Self::query_selector`]'s selector grammar,
    /// in tree order. An empty selector yields an empty vec. The ids are owned.
    pub fn query_selector_all(&self, selector: &str) -> Vec<NodeId> {
        let selector = selector.trim();
        if selector.is_empty() {
            return Vec::new();
        }
        let mut matches = Vec::new();
        self.collect_by_simple_selector(self.root(), selector, &mut matches);
        matches
    }

    /// First descendant of `scope` in tree order that matches `selector`.
    pub fn query_selector_within(&self, scope: NodeId, selector: &str) -> Option<NodeId> {
        let selector = selector.trim();
        if selector.is_empty() || self.nodes.get(scope.index()).is_none() {
            return None;
        }
        for child in self.children(scope) {
            if let Some(found) = self.find_by_simple_selector(*child, selector) {
                return Some(found);
            }
        }
        None
    }

    /// Every descendant of `scope` in tree order that matches `selector`.
    pub fn query_selector_all_within(&self, scope: NodeId, selector: &str) -> Vec<NodeId> {
        let selector = selector.trim();
        if selector.is_empty() || self.nodes.get(scope.index()).is_none() {
            return Vec::new();
        }
        let mut matches = Vec::new();
        for child in self.children(scope) {
            self.collect_by_simple_selector(*child, selector, &mut matches);
        }
        matches
    }

    /// True when `id` matches `selector` (compound, attribute, combinator, or comma list).
    pub fn matches_selector(&self, id: NodeId, selector: &str) -> bool {
        let selector = selector.trim();
        if selector.is_empty() || self.nodes.get(id.index()).is_none() {
            return false;
        }
        self.node_matches_selector(id, selector)
    }

    /// Nearest element ancestor of `id` (including `id` itself) matching `selector`.
    pub fn closest(&self, id: NodeId, selector: &str) -> Option<NodeId> {
        let selector = selector.trim();
        if selector.is_empty() {
            return None;
        }
        let mut cursor = Some(id);
        while let Some(curr) = cursor {
            let node = self.nodes.get(curr.index())?;
            if node.data().tag().is_some() && self.node_matches_selector(curr, selector) {
                return Some(curr);
            }
            cursor = node.parent();
        }
        None
    }

    /// Insert `new_child` under `parent` immediately before `reference_child`
    /// (or at the end when `reference_child` is `None`).
    pub fn insert_before(
        &mut self,
        parent: NodeId,
        new_child: NodeId,
        reference_child: Option<NodeId>,
    ) -> Result<NodeId, TreeError> {
        let Some(ref_id) = reference_child else {
            self.append(parent, new_child)?;
            return Ok(new_child);
        };
        if self.nodes.get(parent.index()).is_none()
            || self.nodes.get(new_child.index()).is_none()
            || self.nodes.get(ref_id.index()).is_none()
        {
            return Err(TreeError::Missing);
        }
        if self.nodes[ref_id.index()].parent != Some(parent) {
            return Err(TreeError::Missing);
        }
        if parent == new_child {
            return Err(TreeError::Cycle);
        }
        let mut cursor = Some(parent);
        while let Some(id) = cursor {
            if id == new_child {
                return Err(TreeError::Cycle);
            }
            cursor = self.nodes[id.index()].parent;
        }
        if new_child == ref_id {
            return Ok(new_child);
        }
        self.detach(new_child);
        self.nodes[new_child.index()].parent = Some(parent);
        let children = &mut self.nodes[parent.index()].children;
        if let Some(pos) = children.iter().position(|&c| c == ref_id) {
            children.insert(pos, new_child);
        } else {
            children.push(new_child);
        }
        self.mark_style_dirty(parent);
        self.mark_style_dirty(new_child);
        Ok(new_child)
    }

    /// Detach `child` from `parent` if `child`'s parent is `parent`.
    pub fn remove_child(&mut self, parent: NodeId, child: NodeId) -> Result<NodeId, TreeError> {
        if self.nodes.get(parent.index()).is_none() || self.nodes.get(child.index()).is_none() {
            return Err(TreeError::Missing);
        }
        if self.nodes[child.index()].parent != Some(parent) {
            return Err(TreeError::Missing);
        }
        self.remove_node(child);
        Ok(child)
    }

    /// Replace `old_child` under `parent` with `new_child` and return `old_child`.
    pub fn replace_child(
        &mut self,
        parent: NodeId,
        new_child: NodeId,
        old_child: NodeId,
    ) -> Result<NodeId, TreeError> {
        self.insert_before(parent, new_child, Some(old_child))?;
        self.remove_child(parent, old_child)
    }

    /// Clone `id` into a new detached node in the same document. When `deep` is
    /// true, also clones its descendant subtree up to [`MAX_DOM_NODES`].
    pub fn clone_node(&mut self, id: NodeId, deep: bool) -> Option<NodeId> {
        let data = self.nodes.get(id.index())?.data().clone();
        if matches!(data, NodeData::Document) || self.nodes.len() >= MAX_DOM_NODES {
            return None;
        }
        let new_id = self.alloc(data);
        if deep {
            let children: Vec<NodeId> = self.children(id).to_vec();
            for child in children {
                self.clone_same_doc_subtree(child, new_id);
            }
        }
        Some(new_id)
    }

    fn clone_same_doc_subtree(&mut self, src_id: NodeId, dst_parent: NodeId) {
        if self.nodes.len() >= MAX_DOM_NODES {
            return;
        }
        let Some(data) = self.nodes.get(src_id.index()).map(|n| n.data().clone()) else {
            return;
        };
        if matches!(data, NodeData::Document) {
            return;
        }
        let new_node = self.alloc(data);
        if self.append(dst_parent, new_node).is_ok() {
            let children: Vec<NodeId> = self.children(src_id).to_vec();
            for child in children {
                self.clone_same_doc_subtree(child, new_node);
            }
        }
    }

    /// First direct child node (element or text) of `id`.
    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        self.nodes
            .get(id.index())?
            .children
            .iter()
            .copied()
            .find(|cid| !self.shadow_hosts.contains_key(cid))
    }

    /// Last direct child node (element or text) of `id`.
    pub fn last_child(&self, id: NodeId) -> Option<NodeId> {
        self.nodes
            .get(id.index())?
            .children
            .iter()
            .rev()
            .copied()
            .find(|cid| !self.shadow_hosts.contains_key(cid))
    }

    /// Next sibling node (element or text) of `id`.
    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.nodes.get(id.index())?.parent?;
        let siblings = &self.nodes.get(parent.index())?.children;
        let pos = siblings.iter().position(|&cid| cid == id)?;
        siblings[pos + 1..]
            .iter()
            .copied()
            .find(|cid| !self.shadow_hosts.contains_key(cid))
    }

    /// Previous sibling node (element or text) of `id`.
    pub fn previous_sibling(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.nodes.get(id.index())?.parent?;
        let siblings = &self.nodes.get(parent.index())?.children;
        let pos = siblings.iter().position(|&cid| cid == id)?;
        siblings[..pos]
            .iter()
            .rev()
            .copied()
            .find(|cid| !self.shadow_hosts.contains_key(cid))
    }

    /// All descendant elements under `scope` matching `tag_name` (case-insensitive, or `*` for all).
    pub fn get_elements_by_tag_name(&self, scope: NodeId, tag_name: &str) -> Vec<NodeId> {
        let tag_lower = tag_name.trim().to_ascii_lowercase();
        if tag_lower.is_empty() || self.nodes.get(scope.index()).is_none() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for &child in self.children(scope) {
            self.collect_by_tag_name(child, &tag_lower, &mut out);
        }
        out
    }

    fn collect_by_tag_name(&self, id: NodeId, tag_lower: &str, out: &mut Vec<NodeId>) {
        if let Some(t) = self.get(id).data().tag()
            && (tag_lower == "*" || t == tag_lower)
        {
            out.push(id);
        }
        for &child in self.children(id) {
            self.collect_by_tag_name(child, tag_lower, out);
        }
    }

    /// All descendant elements under `scope` having every whitespace-separated class in `class_names`.
    pub fn get_elements_by_class_name(&self, scope: NodeId, class_names: &str) -> Vec<NodeId> {
        let tokens: Vec<&str> = class_names.split_whitespace().collect();
        if tokens.is_empty() || self.nodes.get(scope.index()).is_none() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for &child in self.children(scope) {
            self.collect_by_class_tokens(child, &tokens, &mut out);
        }
        out
    }

    fn collect_by_class_tokens(&self, id: NodeId, tokens: &[&str], out: &mut Vec<NodeId>) {
        let data = self.get(id).data();
        if data.tag().is_some() && tokens.iter().all(|c| data.has_class(c)) {
            out.push(id);
        }
        for &child in self.children(id) {
            self.collect_by_class_tokens(child, tokens, out);
        }
    }

    /// All elements in the document whose `name` attribute equals `name`.
    pub fn get_elements_by_name(&self, name: &str) -> Vec<NodeId> {
        if name.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.collect_by_attr_name(self.root(), name, &mut out);
        out
    }

    fn collect_by_attr_name(&self, id: NodeId, target_name: &str, out: &mut Vec<NodeId>) {
        let data = self.get(id).data();
        if data.tag().is_some() && data.attr("name") == Some(target_name) {
            out.push(id);
        }
        for &child in self.children(id) {
            self.collect_by_attr_name(child, target_name, out);
        }
    }

    /// Insert `new_el` relative to `target_id` using `"beforebegin"`, `"afterbegin"`, `"beforeend"`, or `"afterend"`.
    pub fn insert_adjacent_element(
        &mut self,
        target_id: NodeId,
        position: &str,
        new_el: NodeId,
    ) -> Option<NodeId> {
        match position.trim().to_ascii_lowercase().as_str() {
            "beforebegin" => {
                let parent = self.nodes.get(target_id.index())?.parent?;
                self.insert_before(parent, new_el, Some(target_id)).ok()
            }
            "afterbegin" => {
                let first = self.first_child(target_id);
                self.insert_before(target_id, new_el, first).ok()
            }
            "beforeend" => {
                self.append(target_id, new_el).ok()?;
                Some(new_el)
            }
            "afterend" => {
                let parent = self.nodes.get(target_id.index())?.parent?;
                let next = self.next_sibling(target_id);
                self.insert_before(parent, new_el, next).ok()
            }
            _ => None,
        }
    }

    /// Insert a text node relative to `target_id` using `"beforebegin"`, `"afterbegin"`, `"beforeend"`, or `"afterend"`.
    pub fn insert_adjacent_text(&mut self, target_id: NodeId, position: &str, text: &str) {
        if self.nodes.len() >= MAX_DOM_NODES || self.nodes.get(target_id.index()).is_none() {
            return;
        }
        let tid = self.create_text(text);
        let _ = self.insert_adjacent_element(target_id, position, tid);
    }

    /// Parse `html_fragment` and insert its nodes relative to `target_id`
    /// (`"beforebegin"`, `"afterbegin"`, `"beforeend"`, or `"afterend"`).
    pub fn insert_adjacent_html(&mut self, target_id: NodeId, position: &str, html_fragment: &str) {
        if html_fragment.trim().is_empty() || self.nodes.get(target_id.index()).is_none() {
            return;
        }
        let parsed = parse_html(html_fragment);
        let body_children: Vec<NodeId> = parsed.children(parsed.body()).to_vec();
        let pos = position.trim().to_ascii_lowercase();
        match pos.as_str() {
            "beforeend" => {
                for src_child in body_children {
                    self.clone_subtree_into(&parsed, src_child, target_id);
                }
            }
            "afterbegin" => {
                let anchor = self.first_child(target_id);
                for src_child in body_children {
                    if let Some(cloned) = self.clone_subtree_detached(&parsed, src_child) {
                        let _ = self.insert_before(target_id, cloned, anchor);
                    }
                }
            }
            "beforebegin" => {
                let Some(parent) = self.nodes.get(target_id.index()).and_then(|n| n.parent) else {
                    return;
                };
                for src_child in body_children {
                    if let Some(cloned) = self.clone_subtree_detached(&parsed, src_child) {
                        let _ = self.insert_before(parent, cloned, Some(target_id));
                    }
                }
            }
            "afterend" => {
                let Some(parent) = self.nodes.get(target_id.index()).and_then(|n| n.parent) else {
                    return;
                };
                let anchor = self.next_sibling(target_id);
                for src_child in body_children {
                    if let Some(cloned) = self.clone_subtree_detached(&parsed, src_child) {
                        let _ = self.insert_before(parent, cloned, anchor);
                    }
                }
            }
            _ => {}
        }
    }

    fn clone_subtree_detached(&mut self, src_doc: &Document, src_id: NodeId) -> Option<NodeId> {
        if self.nodes.len() >= MAX_DOM_NODES {
            return None;
        }
        let new_node = match src_doc.get(src_id).data() {
            NodeData::Text(t) => self.create_text(t),
            NodeData::Element { tag, attrs } => self.create_element(tag, attrs.clone()),
            NodeData::Document => return None,
        };
        for child in src_doc.children(src_id) {
            self.clone_subtree_into(src_doc, *child, new_node);
        }
        Some(new_node)
    }

    /// Direct element children of `id` in tree order.
    pub fn element_children(&self, id: NodeId) -> Vec<NodeId> {
        let Some(node) = self.nodes.get(id.index()) else {
            return Vec::new();
        };
        node.children
            .iter()
            .copied()
            .filter(|cid| {
                self.get(*cid).data().tag().is_some() && !self.shadow_hosts.contains_key(cid)
            })
            .collect()
    }

    /// First direct element child of `id`.
    pub fn first_element_child(&self, id: NodeId) -> Option<NodeId> {
        self.element_children(id).into_iter().next()
    }

    /// Last direct element child of `id`.
    pub fn last_element_child(&self, id: NodeId) -> Option<NodeId> {
        self.element_children(id).into_iter().last()
    }

    /// Next sibling of `id` that is an element.
    pub fn next_element_sibling(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.nodes.get(id.index())?.parent?;
        let siblings = &self.nodes.get(parent.index())?.children;
        let pos = siblings.iter().position(|&cid| cid == id)?;
        siblings[pos + 1..].iter().copied().find(|&cid| {
            self.get(cid).data().tag().is_some() && !self.shadow_hosts.contains_key(&cid)
        })
    }

    /// Previous sibling of `id` that is an element.
    pub fn previous_element_sibling(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.nodes.get(id.index())?.parent?;
        let siblings = &self.nodes.get(parent.index())?.children;
        let pos = siblings.iter().position(|&cid| cid == id)?;
        siblings[..pos].iter().rev().copied().find(|&cid| {
            self.get(cid).data().tag().is_some() && !self.shadow_hosts.contains_key(&cid)
        })
    }

    /// Parent of `id` if that parent is an element node.
    pub fn parent_element(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.nodes.get(id.index())?.parent?;
        self.get(parent).data().tag().map(|_| parent)
    }

    /// True when `descendant` is `ancestor` or a descendant of `ancestor`.
    pub fn contains_node(&self, ancestor: NodeId, descendant: NodeId) -> bool {
        if self.nodes.get(ancestor.index()).is_none()
            || self.nodes.get(descendant.index()).is_none()
        {
            return false;
        }
        let mut cursor = Some(descendant);
        while let Some(curr) = cursor {
            if curr == ancestor {
                return true;
            }
            cursor = self.nodes[curr.index()].parent;
        }
        false
    }

    /// Serialize the children of `id` as an HTML fragment.
    pub fn inner_html(&self, id: NodeId) -> String {
        let Some(node) = self.nodes.get(id.index()) else {
            return String::new();
        };
        let kids: Vec<NodeId> = node.children.clone();
        let mut out = String::new();
        for child in kids {
            if self.shadow_hosts.contains_key(&child) {
                continue;
            }
            self.write_html(child, &mut out);
        }
        out
    }

    /// Serialize `id` and its subtree as an HTML fragment.
    pub fn outer_html(&self, id: NodeId) -> String {
        if self.nodes.get(id.index()).is_none() {
            let _ = self;
            return String::new();
        }
        let mut out = String::new();
        self.write_html(id, &mut out);
        out
    }

    fn node_matches_selector(&self, id: NodeId, selector: &str) -> bool {
        for branch in split_selector_commas(selector) {
            if self.node_matches_combinator_chain(id, branch) {
                return true;
            }
        }
        false
    }

    fn node_matches_combinator_chain(&self, id: NodeId, chain: &str) -> bool {
        let steps = parse_combinator_chain(chain);
        let Some((last_comb, last_compound)) = steps.last() else {
            return false;
        };
        let _ = last_comb;
        if !self.node_matches_compound_selector(id, last_compound) {
            return false;
        }
        if steps.len() == 1 {
            return true;
        }
        self.match_ancestor_steps(id, &steps[..steps.len() - 1], steps[steps.len() - 1].0)
    }

    fn node_matches_compound_selector(&self, id: NodeId, compound: &str) -> bool {
        let mut base = String::new();
        let mut pseudos: Vec<(String, Option<String>)> = Vec::new();
        let bytes = compound.as_bytes();
        let mut i = 0usize;
        let mut bracket_depth = 0i32;
        while i < bytes.len() {
            match bytes[i] {
                b'[' => {
                    bracket_depth += 1;
                    base.push('[');
                    i += 1;
                }
                b']' => {
                    bracket_depth = (bracket_depth - 1).max(0);
                    base.push(']');
                    i += 1;
                }
                b':' if bracket_depth == 0 => {
                    i += 1;
                    let is_double_colon = if i < bytes.len() && bytes[i] == b':' {
                        i += 1;
                        true
                    } else {
                        false
                    };
                    let start_name = i;
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-')
                    {
                        i += 1;
                    }
                    let raw_name = compound[start_name..i].to_ascii_lowercase();
                    if is_double_colon && !matches!(raw_name.as_str(), "slotted" | "part") {
                        return false;
                    }
                    let name = if is_double_colon {
                        format!("::{raw_name}")
                    } else {
                        raw_name
                    };
                    if i < bytes.len() && bytes[i] == b'(' {
                        i += 1;
                        let start_arg = i;
                        let mut pdepth = 1i32;
                        while i < bytes.len() && pdepth > 0 {
                            if bytes[i] == b'(' {
                                pdepth += 1;
                            } else if bytes[i] == b')' {
                                pdepth -= 1;
                            }
                            i += 1;
                        }
                        if pdepth != 0 {
                            return false;
                        }
                        let arg = compound[start_arg..i - 1].trim().to_string();
                        pseudos.push((name, Some(arg)));
                    } else {
                        pseudos.push((name, None));
                    }
                }
                _ => {
                    let ch = compound[i..].chars().next().unwrap_or(' ');
                    base.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        let base_trimmed = base.trim();
        if !base_trimmed.is_empty() && !matches_simple_selector(self.get(id).data(), base_trimmed) {
            return false;
        }
        if base_trimmed.is_empty() && self.get(id).data().tag().is_none() {
            return false;
        }
        for (name, arg) in pseudos {
            match (name.as_str(), arg.as_deref()) {
                ("is" | "where" | "matches", Some(inner)) => {
                    if !self.node_matches_selector(id, inner) {
                        return false;
                    }
                }
                ("not", Some(inner)) => {
                    if self.node_matches_selector(id, inner) {
                        return false;
                    }
                }
                ("has", Some(inner)) => {
                    let mut matched_desc = false;
                    for branch in split_selector_commas(inner) {
                        let rel = branch.trim();
                        if let Some(child_sel) = rel.strip_prefix('>') {
                            let child_sel = child_sel.trim();
                            for &cid in self.children(id) {
                                if self.node_matches_selector(cid, child_sel) {
                                    matched_desc = true;
                                    break;
                                }
                            }
                        } else if let Some(next_sel) = rel.strip_prefix('+') {
                            let next_sel = next_sel.trim();
                            if let Some(sib) = self.next_element_sibling(id)
                                && self.node_matches_selector(sib, next_sel)
                            {
                                matched_desc = true;
                            }
                        } else if let Some(sub_sel) = rel.strip_prefix('~') {
                            let sub_sel = sub_sel.trim();
                            let mut cur = self.next_element_sibling(id);
                            while let Some(sib) = cur {
                                if self.node_matches_selector(sib, sub_sel) {
                                    matched_desc = true;
                                    break;
                                }
                                cur = self.next_element_sibling(sib);
                            }
                        } else if self.query_selector_within(id, rel).is_some() {
                            matched_desc = true;
                        }
                        if matched_desc {
                            break;
                        }
                    }
                    if !matched_desc {
                        return false;
                    }
                }
                ("scope", None) => {}
                ("first-child", None) => {
                    if self.previous_element_sibling(id).is_some() {
                        return false;
                    }
                }
                ("last-child", None) => {
                    if self.next_element_sibling(id).is_some() {
                        return false;
                    }
                }
                ("only-child", None) => {
                    if self.previous_element_sibling(id).is_some()
                        || self.next_element_sibling(id).is_some()
                    {
                        return false;
                    }
                }
                ("first-of-type", None) => {
                    let Some(tag) = self.get(id).data().tag() else {
                        return false;
                    };
                    let mut prev = self.previous_element_sibling(id);
                    while let Some(sib) = prev {
                        if self.get(sib).data().tag() == Some(tag) {
                            return false;
                        }
                        prev = self.previous_element_sibling(sib);
                    }
                }
                ("last-of-type", None) => {
                    let Some(tag) = self.get(id).data().tag() else {
                        return false;
                    };
                    let mut next = self.next_element_sibling(id);
                    while let Some(sib) = next {
                        if self.get(sib).data().tag() == Some(tag) {
                            return false;
                        }
                        next = self.next_element_sibling(sib);
                    }
                }
                ("only-of-type", None) => {
                    let Some(tag) = self.get(id).data().tag() else {
                        return false;
                    };
                    let mut prev = self.previous_element_sibling(id);
                    while let Some(sib) = prev {
                        if self.get(sib).data().tag() == Some(tag) {
                            return false;
                        }
                        prev = self.previous_element_sibling(sib);
                    }
                    let mut next = self.next_element_sibling(id);
                    while let Some(sib) = next {
                        if self.get(sib).data().tag() == Some(tag) {
                            return false;
                        }
                        next = self.next_element_sibling(sib);
                    }
                }
                ("nth-child", Some(expr)) | ("nth-last-child", Some(expr)) => {
                    let from_end = name == "nth-last-child";
                    if !self.matches_nth_child(id, expr, from_end) {
                        return false;
                    }
                }
                ("nth-of-type", Some(expr)) | ("nth-last-of-type", Some(expr)) => {
                    let from_end = name == "nth-last-of-type";
                    if !self.matches_nth_of_type(id, expr, from_end) {
                        return false;
                    }
                }
                ("link" | "any-link", None) => {
                    let data = self.get(id).data();
                    if !matches!(data.tag(), Some("a" | "area")) || data.attr("href").is_none() {
                        return false;
                    }
                }
                ("read-only", None) => {
                    let data = self.get(id).data();
                    let is_editable_control = matches!(data.tag(), Some("input" | "textarea"))
                        && !data
                            .attr("readonly")
                            .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
                        && !data
                            .attr("disabled")
                            .is_some_and(|v| !v.eq_ignore_ascii_case("false"));
                    let is_contenteditable = data
                        .attr("contenteditable")
                        .is_some_and(|v| v.is_empty() || v.eq_ignore_ascii_case("true"));
                    if is_editable_control || is_contenteditable {
                        return false;
                    }
                }
                ("read-write", None) => {
                    let data = self.get(id).data();
                    let is_editable_control = matches!(data.tag(), Some("input" | "textarea"))
                        && !data
                            .attr("readonly")
                            .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
                        && !data
                            .attr("disabled")
                            .is_some_and(|v| !v.eq_ignore_ascii_case("false"));
                    let is_contenteditable = data
                        .attr("contenteditable")
                        .is_some_and(|v| v.is_empty() || v.eq_ignore_ascii_case("true"));
                    if !is_editable_control && !is_contenteditable {
                        return false;
                    }
                }
                ("placeholder-shown", None) => {
                    let data = self.get(id).data();
                    if !matches!(data.tag(), Some("input" | "textarea"))
                        || data.attr("placeholder").is_none_or(|p| p.is_empty())
                        || !self.input_value(id).is_empty()
                    {
                        return false;
                    }
                }
                ("open", None) => {
                    let data = self.get(id).data();
                    if !matches!(data.tag(), Some("details" | "dialog"))
                        || !data
                            .attr("open")
                            .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
                    {
                        return false;
                    }
                }
                ("closed", None) => {
                    let data = self.get(id).data();
                    if !matches!(data.tag(), Some("details" | "dialog"))
                        || data
                            .attr("open")
                            .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
                    {
                        return false;
                    }
                }
                ("indeterminate", None) => {
                    let data = self.get(id).data();
                    let is_indet_check = self.is_checkbox_input(id)
                        && data
                            .attr("indeterminate")
                            .is_some_and(|v| !v.eq_ignore_ascii_case("false"));
                    let is_indet_prog =
                        data.tag() == Some("progress") && data.attr("value").is_none();
                    if !is_indet_check && !is_indet_prog {
                        return false;
                    }
                }
                ("host", None) => {
                    if !self.is_shadow_host(id) {
                        return false;
                    }
                }
                ("host", Some(inner)) => {
                    let host_target = self.shadow_host(id).unwrap_or(id);
                    if !self.is_shadow_host(id) || !self.node_matches_selector(host_target, inner) {
                        return false;
                    }
                }
                ("host-context", Some(inner)) => {
                    if !self.is_shadow_host(id) {
                        return false;
                    }
                    let host_target = self.shadow_host(id).unwrap_or(id);
                    let mut cur = Some(host_target);
                    let mut found = false;
                    while let Some(anc) = cur {
                        if self.node_matches_selector(anc, inner) {
                            found = true;
                            break;
                        }
                        cur = self.get(anc).parent();
                    }
                    if !found {
                        return false;
                    }
                }
                ("defined", None) => {
                    let Some(tag) = self.get(id).data().tag() else {
                        return false;
                    };
                    if tag.contains('-') && !self.is_custom_element_defined(tag) {
                        return false;
                    }
                }
                ("root", None) => {
                    if id != self.html {
                        return false;
                    }
                }
                ("empty", None) => {
                    let has_content =
                        self.children(id)
                            .iter()
                            .any(|&cid| match self.get(cid).data() {
                                NodeData::Element { .. } => true,
                                NodeData::Text(t) => !t.is_empty(),
                                NodeData::Document => false,
                            });
                    if has_content {
                        return false;
                    }
                }
                ("hover", None) => {
                    if !self.is_hovered(id) {
                        return false;
                    }
                }
                ("active", None) => {
                    if !self.is_active(id) {
                        return false;
                    }
                }
                ("focus" | "focus-visible", None) => {
                    if !self.is_focused(id) {
                        return false;
                    }
                }
                ("focus-within", None) => {
                    if !self.is_focus_within(id) {
                        return false;
                    }
                }
                ("target", None) => {
                    if !self.is_target(id) {
                        return false;
                    }
                }
                ("target-within", None) => {
                    if !self.is_target_within(id) {
                        return false;
                    }
                }
                ("valid" | "user-valid", None) => {
                    if self.is_control_valid(id) != Some(true) {
                        return false;
                    }
                }
                ("invalid" | "user-invalid", None) => {
                    if self.is_control_valid(id) != Some(false) {
                        return false;
                    }
                }
                ("in-range", None) => {
                    if self.is_control_in_range(id) != Some(true) {
                        return false;
                    }
                }
                ("out-of-range", None) => {
                    if self.is_control_in_range(id) != Some(false) {
                        return false;
                    }
                }
                ("checked", None) => {
                    if !self.is_checked(id) {
                        return false;
                    }
                }
                ("disabled", None) => {
                    let data = self.get(id).data();
                    if !matches!(
                        data.tag(),
                        Some("input" | "button" | "select" | "textarea" | "option" | "fieldset")
                    ) || !data
                        .attr("disabled")
                        .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
                    {
                        return false;
                    }
                }
                ("enabled", None) => {
                    let data = self.get(id).data();
                    if !matches!(
                        data.tag(),
                        Some("input" | "button" | "select" | "textarea" | "option" | "fieldset")
                    ) || data
                        .attr("disabled")
                        .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
                    {
                        return false;
                    }
                }
                ("required", None) => {
                    let data = self.get(id).data();
                    if !matches!(data.tag(), Some("input" | "select" | "textarea"))
                        || !data
                            .attr("required")
                            .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
                    {
                        return false;
                    }
                }
                ("optional", None) => {
                    let data = self.get(id).data();
                    if !matches!(data.tag(), Some("input" | "select" | "textarea"))
                        || data
                            .attr("required")
                            .is_some_and(|v| !v.eq_ignore_ascii_case("false"))
                    {
                        return false;
                    }
                }
                ("slotted" | "::slotted", Some(inner)) => {
                    if !self.is_slotted_node(id) || !self.node_matches_selector(id, inner) {
                        return false;
                    }
                }
                ("part" | "::part", Some(part_arg)) => {
                    let wanted: Vec<&str> = part_arg.split_whitespace().collect();
                    if wanted.is_empty() || self.enclosing_shadow_host(id).is_none() {
                        return false;
                    }
                    let Some(attr_val) = self.get(id).data().attr("part") else {
                        return false;
                    };
                    let have: Vec<&str> = attr_val.split_whitespace().collect();
                    if !wanted.iter().all(|w| have.contains(w)) {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        true
    }

    fn matches_nth_child(&self, id: NodeId, expr: &str, from_end: bool) -> bool {
        let Some(parent) = self.get(id).parent() else {
            return false;
        };
        let (an_b_str, of_sel) = split_nth_of_clause(expr);
        let Some((a, b)) = parse_an_plus_b(an_b_str) else {
            return false;
        };
        let siblings: Vec<NodeId> = self
            .children(parent)
            .iter()
            .copied()
            .filter(|&cid| {
                self.get(cid).data().tag().is_some()
                    && of_sel.is_none_or(|sel| self.node_matches_selector(cid, sel))
            })
            .collect();
        let Some(pos) = siblings.iter().position(|&cid| cid == id) else {
            return false;
        };
        let index_1 = if from_end {
            (siblings.len() - pos) as i64
        } else {
            (pos + 1) as i64
        };
        eval_an_plus_b(a, b, index_1)
    }

    fn matches_nth_of_type(&self, id: NodeId, expr: &str, from_end: bool) -> bool {
        let Some(parent) = self.get(id).parent() else {
            return false;
        };
        let Some(tag) = self.get(id).data().tag() else {
            return false;
        };
        let Some((a, b)) = parse_an_plus_b(expr.trim()) else {
            return false;
        };
        let siblings: Vec<NodeId> = self
            .children(parent)
            .iter()
            .copied()
            .filter(|&cid| self.get(cid).data().tag() == Some(tag))
            .collect();
        let Some(pos) = siblings.iter().position(|&cid| cid == id) else {
            return false;
        };
        let index_1 = if from_end {
            (siblings.len() - pos) as i64
        } else {
            (pos + 1) as i64
        };
        eval_an_plus_b(a, b, index_1)
    }

    fn match_ancestor_steps(
        &self,
        current_id: NodeId,
        remaining: &[(SelectorCombinator, String)],
        combinator_to_current: SelectorCombinator,
    ) -> bool {
        let Some((prev_comb, target_compound)) = remaining.last() else {
            return true;
        };
        let rest = &remaining[..remaining.len() - 1];
        match combinator_to_current {
            SelectorCombinator::Child => {
                let Some(parent) = self.get(current_id).parent() else {
                    return false;
                };
                if !self.node_matches_compound_selector(parent, target_compound) {
                    return false;
                }
                if rest.is_empty() {
                    true
                } else {
                    self.match_ancestor_steps(parent, rest, *prev_comb)
                }
            }
            SelectorCombinator::Descendant => {
                let mut cursor = self.get(current_id).parent();
                while let Some(anc) = cursor {
                    if self.node_matches_compound_selector(anc, target_compound)
                        && (rest.is_empty() || self.match_ancestor_steps(anc, rest, *prev_comb))
                    {
                        return true;
                    }
                    cursor = self.get(anc).parent();
                }
                false
            }
            SelectorCombinator::NextSibling => {
                let Some(prev_sib) = self.previous_element_sibling(current_id) else {
                    return false;
                };
                if !self.node_matches_compound_selector(prev_sib, target_compound) {
                    return false;
                }
                if rest.is_empty() {
                    true
                } else {
                    self.match_ancestor_steps(prev_sib, rest, *prev_comb)
                }
            }
            SelectorCombinator::SubsequentSibling => {
                let mut cursor = self.previous_element_sibling(current_id);
                while let Some(sib) = cursor {
                    if self.node_matches_compound_selector(sib, target_compound)
                        && (rest.is_empty() || self.match_ancestor_steps(sib, rest, *prev_comb))
                    {
                        return true;
                    }
                    cursor = self.previous_element_sibling(sib);
                }
                false
            }
        }
    }
    #[allow(dead_code)]
    fn _noop_dom_marker(&self) {}

    /// Unlink `id` from its parent. The document node, `html`, `head`, and
    /// `body` are refused. The slot and its descendants stay allocated.
    /// Descendants keep their parent pointers.
    pub fn remove_node(&mut self, id: NodeId) {
        if id == self.root() || id == self.html || id == self.head || id == self.body {
            return;
        }
        self.detach(id);
    }

    /// Replace the children of `id` with a parse of `html_fragment`.
    ///
    /// The fragment is parsed as a document and the new `body` children are
    /// cloned under `id`. Cloning stops at [`MAX_DOM_NODES`]. A missing `id`
    /// is a no-op. Blank markup only detaches the old children. Old slots
    /// stay allocated. `html_fragment` is borrowed.
    pub fn set_inner_html(&mut self, id: NodeId, html_fragment: &str) {
        if self.nodes.get(id.index()).is_none() {
            return;
        }
        let old_children = std::mem::take(&mut self.nodes[id.index()].children);
        for child in old_children {
            if self.shadow_hosts.contains_key(&child) {
                self.nodes[id.index()].children.push(child);
                continue;
            }
            if let Some(node) = self.nodes.get_mut(child.index()) {
                node.parent = None;
            }
        }
        self.mark_style_dirty(id);
        if html_fragment.trim().is_empty() {
            return;
        }
        let parsed = parse_html(html_fragment);
        let head_children: Vec<NodeId> = parsed.children(parsed.head()).to_vec();
        for src_child in head_children {
            self.clone_subtree_into(&parsed, src_child, id);
        }
        let body_children: Vec<NodeId> = parsed.children(parsed.body()).to_vec();
        for src_child in body_children {
            self.clone_subtree_into(&parsed, src_child, id);
        }
    }

    /// Copy one node from a parsed fragment onto `dst_parent`, then its children.
    ///
    /// `Document` nodes are skipped. Past [`MAX_DOM_NODES`] the copy stops and
    /// the rest of that subtree is dropped. A failed [`Self::append`] also
    /// drops the new node's children. `set_inner_html` uses this to move the
    /// fragment's body children into the live tree.
    fn clone_subtree_into(&mut self, src_doc: &Document, src_id: NodeId, dst_parent: NodeId) {
        // Cloning stops at the same node cap as the parser. The rest of the subtree is dropped.
        if self.nodes.len() >= MAX_DOM_NODES {
            return;
        }
        let new_node = match src_doc.get(src_id).data() {
            NodeData::Text(t) => self.create_text(t),
            NodeData::Element { tag, attrs } => self.create_element(tag, attrs.clone()),
            NodeData::Document => return,
        };
        if self.append(dst_parent, new_node).is_ok() {
            for child in src_doc.children(src_id) {
                self.clone_subtree_into(src_doc, *child, new_node);
            }
        }
    }

    /// Owned text of every `style` element, joined by newlines, in tree order.
    /// A `style` element's descendants are not scanned separately.
    pub fn embedded_css(&self) -> String {
        let mut blocks = Vec::new();
        self.collect_style(self.root(), &mut blocks);
        for sheet in &self.adopted_styles {
            if !sheet.is_empty() {
                blocks.push(sheet.clone());
            }
        }
        blocks.join("\n")
    }

    /// Owned text of every JavaScript `script` element whose text is not only
    /// whitespace. `type="application/json"` and other non-script types are
    /// skipped. External `src` scripts are not included. See
    /// [`Self::external_scripts_with_integrity`].
    pub fn scripts(&self) -> Vec<String> {
        let mut blocks = Vec::new();
        self.collect_scripts(self.root(), &mut blocks);
        blocks
    }

    /// Every stylesheet `<link>` and `<style>` element, in tree order.
    ///
    /// Links follow the same rules as [`Self::stylesheet_links_with_integrity`]
    /// and styles the same rules as [`Self::embedded_css`]: an empty `style`
    /// element is skipped. Each entry keeps its `media` attribute, so the
    /// cascade can see document order and media together.
    pub fn style_sheets(&self) -> Vec<StyleSheetSource> {
        let mut sheets = Vec::new();
        self.collect_style_sheets(self.root(), &mut sheets);
        for css in &self.adopted_styles {
            if !css.is_empty() {
                sheets.push(StyleSheetSource::Style {
                    css: css.clone(),
                    media: None,
                });
            }
        }
        sheets
    }

    /// Owned `href` of every stylesheet `<link>`, in tree order.
    /// Integrity metadata is dropped. See [`Self::stylesheet_links_with_integrity`].
    pub fn stylesheet_links(&self) -> Vec<String> {
        self.stylesheet_links_with_integrity()
            .into_iter()
            .map(|(href, _)| href)
            .collect()
    }

    /// Stylesheet links as owned `(href, integrity)`.
    ///
    /// A link counts when `rel` contains the token `stylesheet`. `integrity`
    /// is `None` when the attribute is absent. Empty `href` values are skipped.
    pub fn stylesheet_links_with_integrity(&self) -> Vec<(String, Option<String>)> {
        let mut links = Vec::new();
        self.collect_stylesheet_links(self.root(), &mut links);
        links
    }

    /// External scripts as owned `(src, integrity)`, in tree order.
    /// A script with an empty or missing `src` is skipped. Inline script text
    /// is not included.
    pub fn external_scripts_with_integrity(&self) -> Vec<(String, Option<String>)> {
        let mut scripts = Vec::new();
        self.collect_external_scripts(self.root(), &mut scripts);
        scripts
    }

    /// `<img>` nodes that have a non-empty `src`, as owned `(id, src)` pairs.
    /// The image bytes are not decoded here.
    pub fn images(&self) -> Vec<(NodeId, String)> {
        let mut imgs = Vec::new();
        self.collect_images(self.root(), &mut imgs);
        imgs
    }

    /// `<video>` nodes as owned `(id, src)` pairs.
    ///
    /// Uses the element's `src` when it is non-empty. Otherwise uses the first
    /// child `<source src>`. Videos with neither are omitted. Bytes are not decoded.
    pub fn videos(&self) -> Vec<(NodeId, String)> {
        let mut vids = Vec::new();
        self.collect_videos(self.root(), &mut vids);
        vids
    }

    /// Join every `<meta http-equiv="Content-Security-Policy">` `content`
    /// value with `"; "`. `None` when no such meta element has content.
    /// The string is owned.
    pub fn meta_csp(&self) -> Option<String> {
        let mut out = Vec::new();
        self.collect_meta_csp(self.root(), &mut out);
        if out.is_empty() {
            None
        } else {
            Some(out.join("; "))
        }
    }

    /// Owned concatenation of this node's text and its descendants' text.
    /// A missing id panics through [`Self::get`]. Element tags are not included.
    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.collect_text(id, &mut out);
        out
    }

    /// Append a node with no parent. The id is the index in `nodes`.
    /// Callers must check [`MAX_DOM_NODES`] before this. The slot is not reused
    /// after [`Self::detach`].
    fn alloc(&mut self, data: NodeData) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node {
            id,
            parent: None,
            children: Vec::new(),
            data,
        });
        id
    }

    /// Unlink `child` from its parent. The node stays in `nodes`.
    /// A node that already has no parent is left as it is.
    fn detach(&mut self, child: NodeId) {
        let Some(parent) = self.nodes[child.index()].parent else {
            return;
        };
        self.nodes[parent.index()]
            .children
            .retain(|id| *id != child);
        self.nodes[child.index()].parent = None;
        self.mark_style_dirty(parent);
    }

    /// Depth-first search for the first element whose tag equals `tag`.
    /// `tag` must already be lowercase. Returns `id` itself when it matches.
    fn find_element(&self, id: NodeId, tag: &str) -> Option<NodeId> {
        if self.get(id).data().tag() == Some(tag) {
            return Some(id);
        }
        for child in self.children(id) {
            if let Some(found) = self.find_element(*child, tag) {
                return Some(found);
            }
        }
        None
    }

    /// Depth-first search for the first node whose `id` attribute equals `target_id`.
    /// The compare is case-sensitive. The first match in tree order wins.
    fn find_by_id(&self, id: NodeId, target_id: &str) -> Option<NodeId> {
        if self.get(id).data().id() == Some(target_id) {
            return Some(id);
        }
        for child in self.children(id) {
            if let Some(found) = self.find_by_id(*child, target_id) {
                return Some(found);
            }
        }
        None
    }

    /// Depth-first search using [`matches_simple_selector`]. First match wins.
    fn find_by_simple_selector(&self, id: NodeId, selector: &str) -> Option<NodeId> {
        if self.node_matches_selector(id, selector) {
            return Some(id);
        }
        for child in self.children(id) {
            if let Some(found) = self.find_by_simple_selector(*child, selector) {
                return Some(found);
            }
        }
        None
    }

    /// Push every match of `selector` under `id`, including `id`, in tree order.
    fn collect_by_simple_selector(&self, id: NodeId, selector: &str, out: &mut Vec<NodeId>) {
        if self.node_matches_selector(id, selector) {
            out.push(id);
        }
        for child in self.children(id) {
            self.collect_by_simple_selector(*child, selector, out);
        }
    }

    /// Append this node's text, then each child's text, with no separator.
    fn collect_text(&self, id: NodeId, out: &mut String) {
        if let Some(text) = self.get(id).data().text() {
            out.push_str(text);
        }
        for &child in self.children(id) {
            if self.shadow_hosts.contains_key(&child) {
                continue;
            }
            self.collect_text(child, out);
        }
    }

    /// Collect non-empty `style` element text. A `style` element's children are
    /// not walked again, so a nested element inside `style` is not a second block.
    fn collect_style(&self, id: NodeId, out: &mut Vec<String>) {
        let tag = self.get(id).data().tag();
        if matches!(tag, Some("noscript" | "template")) {
            return;
        }
        if tag == Some("style") {
            let css = self.text_content(id);
            if !css.is_empty() {
                out.push(css);
            }
            return;
        }
        for &child in self.children(id) {
            if self.shadow_hosts.contains_key(&child) {
                continue;
            }
            self.collect_style(child, out);
        }
    }

    /// Record `<link rel=stylesheet>` and `<style>` in tree order. A `style`
    /// element's children are not walked again.
    fn collect_style_sheets(&self, id: NodeId, out: &mut Vec<StyleSheetSource>) {
        let data = self.get(id).data();
        if matches!(data.tag(), Some("noscript" | "template")) {
            return;
        }
        let media = || data.attr("media").map(|m| m.trim().to_string());
        match data.tag() {
            Some("style") => {
                let css = self.text_content(id);
                if !css.is_empty() {
                    out.push(StyleSheetSource::Style {
                        css,
                        media: media(),
                    });
                }
                return;
            }
            Some("link") => {
                let mut links = Vec::new();
                self.collect_stylesheet_links(id, &mut links);
                if let Some((href, integrity)) = links.into_iter().next() {
                    out.push(StyleSheetSource::Link {
                        href,
                        integrity,
                        media: media(),
                    });
                }
                return;
            }
            _ => {}
        }
        for &child in self.children(id) {
            if self.shadow_hosts.contains_key(&child) {
                continue;
            }
            self.collect_style_sheets(child, out);
        }
    }

    /// Collect inline JavaScript that is not only whitespace.
    /// A script element's children are not walked again. `src` scripts are
    /// still visited here for their text; [`Self::collect_external_scripts`]
    /// records the URL separately. A non-JavaScript `type` is skipped.
    /// `anvil-script` runs the strings this returns.
    fn collect_scripts(&self, id: NodeId, out: &mut Vec<String>) {
        let data = self.get(id).data();
        if matches!(data.tag(), Some("noscript" | "template")) {
            return;
        }
        if data.tag() == Some("script") {
            if is_javascript_type(data.attr("type")) {
                let code = self.text_content(id);
                if !code.trim().is_empty() {
                    out.push(code);
                }
            }
            return;
        }
        for child in self.children(id) {
            self.collect_scripts(*child, out);
        }
    }

    /// Record `link rel=stylesheet` as `(href, integrity)` and keep walking.
    /// `rel` matches the token `stylesheet`, ignoring case. Empty `href` is skipped.
    /// `anvil-browser` fetches these URLs and appends the CSS before `anvil-style` runs.
    fn collect_stylesheet_links(&self, id: NodeId, out: &mut Vec<(String, Option<String>)>) {
        let data = self.get(id).data();
        if matches!(data.tag(), Some("noscript" | "template")) {
            return;
        }
        if data.tag() == Some("link") {
            let is_stylesheet = data.attr("rel").is_some_and(|rel| {
                rel.split_ascii_whitespace()
                    .any(|token| token.eq_ignore_ascii_case("stylesheet"))
            });
            if is_stylesheet && let Some(href) = data.attr("href") {
                let href = href.trim();
                if !href.is_empty() {
                    let integrity = data.attr("integrity").map(|s| s.trim().to_string());
                    out.push((href.to_string(), integrity));
                }
            }
        }
        for child in self.children(id) {
            self.collect_stylesheet_links(*child, out);
        }
    }

    /// Record `script src` as `(src, integrity)` and keep walking into children.
    /// Empty `src` is skipped. `anvil-browser` fetches the URL, checks SRI when
    /// the integrity string is present, then passes the body to `anvil-script`.
    fn collect_external_scripts(&self, id: NodeId, out: &mut Vec<(String, Option<String>)>) {
        let data = self.get(id).data();
        if matches!(data.tag(), Some("noscript" | "template")) {
            return;
        }
        if data.tag() == Some("script")
            && is_javascript_type(data.attr("type"))
            && let Some(src) = data.attr("src")
        {
            let src = src.trim();
            if !src.is_empty() {
                let integrity = data.attr("integrity").map(|s| s.trim().to_string());
                out.push((src.to_string(), integrity));
            }
        }
        for child in self.children(id) {
            self.collect_external_scripts(*child, out);
        }
    }

    /// Record `img` with a usable URL. Does not decode bytes.
    ///
    /// `src` wins when it is not a `data:` image. Otherwise the first `srcset`
    /// candidate is used, then `data-src`, `data-lazy-src`, or `data-original`
    /// when that value is an http(s) or root-relative URL. A `picture` may
    /// supply the URL from an earlier `source`.
    fn collect_images(&self, id: NodeId, out: &mut Vec<(NodeId, String)>) {
        let data = self.get(id).data();
        if matches!(data.tag(), Some("noscript" | "template")) {
            return;
        }
        if data.tag() == Some("img") {
            let url = image_url_from(data).or_else(|| self.picture_source_url(id));
            if let Some(url) = url {
                out.push((id, url));
            }
        }
        for child in self.children(id) {
            self.collect_images(*child, out);
        }
    }

    fn picture_source_url(&self, img: NodeId) -> Option<String> {
        let parent = self.parent_element(img)?;
        if self.get(parent).data().tag() != Some("picture") {
            return None;
        }
        for child in self.children(parent) {
            if *child == img {
                break;
            }
            let data = self.get(*child).data();
            if data.tag() == Some("source")
                && let Some(url) = source_url_from(data)
            {
                return Some(url);
            }
        }
        None
    }

    /// Record one URL per `video`: its `src`, or else the first child `source src`.
    /// Nodes with neither are omitted. Bytes stay undecoded. `anvil-browser`
    /// passes the fetched body to `anvil-video`.
    fn collect_videos(&self, id: NodeId, out: &mut Vec<(NodeId, String)>) {
        let data = self.get(id).data();
        if matches!(data.tag(), Some("noscript" | "template")) {
            return;
        }
        if data.tag() == Some("video") {
            if let Some(src) = data.attr("src") {
                let src = src.trim();
                if !src.is_empty() {
                    out.push((id, src.to_string()));
                }
            } else {
                for child in self.children(id) {
                    let cdata = self.get(*child).data();
                    if cdata.tag() == Some("source")
                        && let Some(src) = cdata.attr("src")
                    {
                        let src = src.trim();
                        if !src.is_empty() {
                            out.push((id, src.to_string()));
                            break;
                        }
                    }
                }
            }
        }
        for child in self.children(id) {
            self.collect_videos(*child, out);
        }
    }

    /// Record `meta http-equiv=Content-Security-Policy` content values.
    /// The compare on `http-equiv` ignores case. Empty content is skipped.
    /// `anvil-net` parses the joined string as a content security policy.
    fn collect_meta_csp(&self, id: NodeId, out: &mut Vec<String>) {
        let data = self.get(id).data();
        if matches!(data.tag(), Some("noscript" | "template")) {
            return;
        }
        if data.tag() == Some("meta") {
            let is_csp = data
                .attr("http-equiv")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("content-security-policy"));
            if is_csp && let Some(content) = data.attr("content") {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
            }
        }
        for child in self.children(id) {
            self.collect_meta_csp(*child, out);
        }
    }

    /// Walk form controls and push `(name, value)` pairs for a submit body.
    ///
    /// Skips controls with an empty `name`. Checkboxes and radios contribute
    /// only when checked (`value`, or `on` when `value` is absent). `submit`,
    /// `button`, and `reset` inputs are skipped. Other inputs use the `value`
    /// attribute. A `textarea` uses [`Self::input_value`]. A `select` uses the
    /// selected option. `anvil-browser` encodes the pairs for `anvil-net` POST.
    fn collect_form_pairs(&self, id: NodeId, out: &mut Vec<(String, String)>) {
        let data = self.get(id).data();
        match data.tag() {
            Some("input") => {
                if let Some(name) = data.attr("name") {
                    let name = name.trim();
                    if !name.is_empty() {
                        let kind = data
                            .attr("type")
                            .unwrap_or("text")
                            .trim()
                            .to_ascii_lowercase();
                        match kind.as_str() {
                            "checkbox" | "radio" => {
                                if self.is_checked(id) {
                                    let val = data.attr("value").unwrap_or("on");
                                    out.push((name.to_string(), val.to_string()));
                                }
                            }
                            "submit" | "button" | "reset" => {}
                            _ => {
                                let val = data.attr("value").unwrap_or("");
                                out.push((name.to_string(), val.to_string()));
                            }
                        }
                    }
                }
            }
            Some("textarea") => {
                if let Some(name) = data.attr("name") {
                    let name = name.trim();
                    if !name.is_empty() {
                        out.push((name.to_string(), self.input_value(id)));
                    }
                }
            }
            Some("select") => {
                if let Some(name) = data.attr("name") {
                    let name = name.trim();
                    if !name.is_empty() {
                        out.push((name.to_string(), self.select_value(id)));
                    }
                }
            }
            _ => {}
        }
        for child in self.children(id) {
            self.collect_form_pairs(*child, out);
        }
    }
}

/// Encode one form field for `application/x-www-form-urlencoded`.
///
/// `input` is borrowed. The result is owned. Unreserved ASCII stays as-is.
/// A space becomes `+`. Every other byte becomes `%XX` using uppercase hex.
pub fn urlencode_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Match one simple selector against borrowed node data.
///
/// Forms: `#id`, `.class`, `tag#id`, `tag.class`, or a tag name. A leading
/// empty tag (`#id` is handled first) is allowed in the split forms. Text and
/// the document node return false. Class and id compares are case-sensitive.
/// The tag compare ignores ASCII case.
/// Escape `&`, `<`, and `>` in text. Attribute values also escape `"`.
fn push_escaped(out: &mut String, text: &str, attr: bool) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

/// Tags that have no end tag in the serializer when they have no children.
fn is_void_tag(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectorCombinator {
    Descendant,
    Child,
    NextSibling,
    SubsequentSibling,
}

fn sample_gradient_stops(stops: &[(f32, [u8; 4])], t: f32) -> [u8; 4] {
    if stops.len() == 1 || t <= stops[0].0 {
        return stops[0].1;
    }
    let last = stops[stops.len() - 1];
    if t >= last.0 {
        return last.1;
    }
    for pair in stops.windows(2) {
        let (t0, c0) = pair[0];
        let (t1, c1) = pair[1];
        if t >= t0 && t <= t1 {
            let span = (t1 - t0).max(1e-6);
            let f = ((t - t0) / span).clamp(0.0, 1.0);
            return [
                (c0[0] as f32 + (c1[0] as f32 - c0[0] as f32) * f).round() as u8,
                (c0[1] as f32 + (c1[1] as f32 - c0[1] as f32) * f).round() as u8,
                (c0[2] as f32 + (c1[2] as f32 - c0[2] as f32) * f).round() as u8,
                (c0[3] as f32 + (c1[3] as f32 - c0[3] as f32) * f).round() as u8,
            ];
        }
    }
    last.1
}

fn split_nth_of_clause(expr: &str) -> (&str, Option<&str>) {
    let trimmed = expr.trim();
    let lower = trimmed.to_ascii_lowercase();
    if let Some(pos) = lower.find(" of ") {
        let an_b = trimmed[..pos].trim();
        let sel = trimmed[pos + 4..].trim();
        if !sel.is_empty() {
            return (an_b, Some(sel));
        }
    }
    (trimmed, None)
}

fn parse_an_plus_b(expr: &str) -> Option<(i64, i64)> {
    let s: String = expr.chars().filter(|c| !c.is_whitespace()).collect();
    let lower = s.to_ascii_lowercase();
    if lower == "odd" {
        return Some((2, 1));
    }
    if lower == "even" {
        return Some((2, 0));
    }
    if let Some(n_pos) = lower.find('n') {
        let a_part = &lower[..n_pos];
        let b_part = &lower[n_pos + 1..];
        let a = match a_part {
            "" | "+" => 1,
            "-" => -1,
            other => other.parse::<i64>().ok()?,
        };
        let b = if b_part.is_empty() {
            0
        } else {
            b_part.parse::<i64>().ok()?
        };
        Some((a, b))
    } else {
        let b = lower.parse::<i64>().ok()?;
        Some((0, b))
    }
}

fn eval_an_plus_b(a: i64, b: i64, index_1: i64) -> bool {
    if a == 0 {
        return index_1 == b;
    }
    let diff = index_1 - b;
    if (diff > 0 && a < 0) || (diff < 0 && a > 0) {
        return false;
    }
    diff % a == 0
}

fn split_selector_commas(selector: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut bracket_depth = 0i32;
    let mut paren_depth = 0i32;
    let mut quote: Option<char> = None;
    for (idx, ch) in selector.char_indices() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = (bracket_depth - 1).max(0),
            '(' => paren_depth += 1,
            ')' => paren_depth = (paren_depth - 1).max(0),
            ',' if bracket_depth == 0 && paren_depth == 0 => {
                let slice = selector[start..idx].trim();
                if !slice.is_empty() {
                    out.push(slice);
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    let tail = selector[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn parse_combinator_chain(chain: &str) -> Vec<(SelectorCombinator, String)> {
    let mut steps = Vec::new();
    let mut current = String::new();
    let mut pending = SelectorCombinator::Descendant;
    let mut bracket_depth = 0i32;
    let mut paren_depth = 0i32;
    let mut quote: Option<char> = None;

    for ch in chain.chars() {
        if let Some(q) = quote {
            current.push(ch);
            if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => {
                quote = Some(ch);
                current.push(ch);
            }
            '[' => {
                bracket_depth += 1;
                current.push(ch);
            }
            ']' => {
                bracket_depth = (bracket_depth - 1).max(0);
                current.push(ch);
            }
            '(' => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' => {
                paren_depth = (paren_depth - 1).max(0);
                current.push(ch);
            }
            '>' if bracket_depth == 0 && paren_depth == 0 => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    steps.push((pending, trimmed));
                    current.clear();
                }
                pending = SelectorCombinator::Child;
            }
            '+' if bracket_depth == 0 && paren_depth == 0 => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    steps.push((pending, trimmed));
                    current.clear();
                }
                pending = SelectorCombinator::NextSibling;
            }
            '~' if bracket_depth == 0 && paren_depth == 0 => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    steps.push((pending, trimmed));
                    current.clear();
                }
                pending = SelectorCombinator::SubsequentSibling;
            }
            c if c.is_whitespace() && bracket_depth == 0 && paren_depth == 0 => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    steps.push((pending, trimmed));
                    current.clear();
                    pending = SelectorCombinator::Descendant;
                }
            }
            _ => current.push(ch),
        }
    }
    let tail = current.trim().to_string();
    if !tail.is_empty() {
        steps.push((pending, tail));
    }
    steps
}

fn matches_simple_selector(data: &NodeData, selector: &str) -> bool {
    let selector = selector.trim();
    if selector.is_empty() {
        return false;
    }
    let Some(actual_tag) = data.tag() else {
        return false;
    };
    if selector == "*" {
        return true;
    }
    // Extract attribute selectors `[name]` or `[name="value"]`
    let mut base = String::new();
    let mut attrs_to_check: Vec<String> = Vec::new();
    let mut rest = selector;
    while let Some(open) = rest.find('[') {
        base.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else {
            return false;
        };
        attrs_to_check.push(after[..close].trim().to_string());
        rest = &after[close + 1..];
    }
    base.push_str(rest);

    for attr_filter in &attrs_to_check {
        if let Some((attr_name, raw_val)) = attr_filter.split_once('=') {
            let attr_name_trim = attr_name.trim();
            let op = if attr_name_trim.ends_with('~') {
                "~="
            } else if attr_name_trim.ends_with('|') {
                "|="
            } else if attr_name_trim.ends_with('^') {
                "^="
            } else if attr_name_trim.ends_with('$') {
                "$="
            } else if attr_name_trim.ends_with('*') {
                "*="
            } else {
                "="
            };
            let name = attr_name_trim
                .trim_end_matches(['~', '|', '^', '$', '*'])
                .trim();
            let mut val_part = raw_val.trim();
            let mut case_insensitive = false;
            if let Some(stripped) = val_part
                .strip_suffix(" i")
                .or_else(|| val_part.strip_suffix(" I"))
            {
                case_insensitive = true;
                val_part = stripped.trim();
            } else if let Some(stripped) = val_part
                .strip_suffix(" s")
                .or_else(|| val_part.strip_suffix(" S"))
            {
                val_part = stripped.trim();
            }
            let expected = val_part.trim_matches('"').trim_matches('\'');
            let Some(actual) = data.attr(name) else {
                return false;
            };
            let actual_cmp = if case_insensitive {
                actual.to_ascii_lowercase()
            } else {
                actual.to_string()
            };
            let expected_cmp = if case_insensitive {
                expected.to_ascii_lowercase()
            } else {
                expected.to_string()
            };
            match op {
                "*=" => {
                    if expected_cmp.is_empty() || !actual_cmp.contains(&expected_cmp) {
                        return false;
                    }
                }
                "^=" => {
                    if expected_cmp.is_empty() || !actual_cmp.starts_with(&expected_cmp) {
                        return false;
                    }
                }
                "$=" => {
                    if expected_cmp.is_empty() || !actual_cmp.ends_with(&expected_cmp) {
                        return false;
                    }
                }
                "~=" => {
                    if expected_cmp.is_empty()
                        || !actual_cmp.split_whitespace().any(|w| w == expected_cmp)
                    {
                        return false;
                    }
                }
                "|=" => {
                    if actual_cmp != expected_cmp
                        && !actual_cmp.starts_with(&format!("{expected_cmp}-"))
                    {
                        return false;
                    }
                }
                _ => {
                    if actual_cmp != expected_cmp {
                        return false;
                    }
                }
            }
        } else if data.attr(attr_filter.trim()).is_none() {
            return false;
        }
    }

    let base = base.trim();
    if base.is_empty() || base == "*" {
        return true;
    }

    // Parse compound `tag#id.class1.class2`
    let mut tag_req: Option<&str> = None;
    let mut id_req: Option<&str> = None;
    let mut classes_req: Vec<&str> = Vec::new();

    let first_sep = base.find(['#', '.']).unwrap_or(base.len());
    if first_sep > 0 {
        let t = &base[..first_sep];
        if t != "*" {
            tag_req = Some(t);
        }
    }
    let mut rem = &base[first_sep..];
    while !rem.is_empty() {
        let kind = rem.as_bytes()[0];
        let after = &rem[1..];
        let next_sep = after.find(['#', '.']).unwrap_or(after.len());
        let token = &after[..next_sep];
        if token.is_empty() {
            return false;
        }
        if kind == b'#' {
            id_req = Some(token);
        } else if kind == b'.' {
            classes_req.push(token);
        }
        rem = &after[next_sep..];
    }

    if let Some(t) = tag_req
        && !actual_tag.eq_ignore_ascii_case(t)
    {
        return false;
    }
    if let Some(req_id) = id_req
        && data.id() != Some(req_id)
    {
        return false;
    }
    for cls in classes_req {
        if !data.has_class(cls) {
            return false;
        }
    }
    true
}

#[allow(dead_code)]
fn _legacy_matches_simple_selector(data: &NodeData, selector: &str) -> bool {
    let Some(actual_tag) = data.tag() else {
        return false;
    };
    if let Some(id_name) = selector.strip_prefix('#') {
        return data.id() == Some(id_name);
    }
    if let Some(class_name) = selector.strip_prefix('.') {
        return data.has_class(class_name);
    }
    if let Some((tag_part, id_name)) = selector.split_once('#') {
        return (tag_part.is_empty() || actual_tag.eq_ignore_ascii_case(tag_part))
            && data.id() == Some(id_name);
    }
    if let Some((tag_part, class_name)) = selector.split_once('.') {
        return (tag_part.is_empty() || actual_tag.eq_ignore_ascii_case(tag_part))
            && data.has_class(class_name);
    }
    actual_tag.eq_ignore_ascii_case(selector)
}

fn image_url_from(data: &NodeData) -> Option<String> {
    let src = data.attr("src").unwrap_or("").trim();
    if let Some(url) = usable_image_url(src)
        && !url.starts_with("data:")
    {
        return Some(url);
    }
    if let Some(set) = data.attr("srcset")
        && let Some(url) = first_srcset_url(set)
    {
        return Some(url);
    }
    for name in ["data-src", "data-lazy-src", "data-original"] {
        if let Some(lazy) = data.attr(name) {
            let lazy = lazy.trim();
            if lazy.starts_with("https://") || lazy.starts_with("http://") || lazy.starts_with('/')
            {
                return Some(lazy.to_string());
            }
        }
    }
    usable_image_url(src)
}

fn source_url_from(data: &NodeData) -> Option<String> {
    if let Some(set) = data.attr("srcset")
        && let Some(url) = first_srcset_url(set)
    {
        return Some(url);
    }
    data.attr("src").and_then(usable_image_url)
}

fn first_srcset_url(srcset: &str) -> Option<String> {
    for part in srcset.split(',') {
        let url = part.split_whitespace().next().unwrap_or("");
        if let Some(url) = usable_image_url(url)
            && !url.starts_with("data:")
        {
            return Some(url);
        }
    }
    None
}

fn usable_image_url(raw: &str) -> Option<String> {
    let url = raw.trim();
    if url.is_empty() || url.to_ascii_lowercase().starts_with("javascript:") {
        return None;
    }
    Some(url.to_string())
}

fn percent_decode_fragment(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

#[cfg(test)]
mod tests {
    //! Parser caps, selectors, forms, and cycle checks against a real document.

    use super::*;

    #[test]
    fn append_refuses_a_cycle() {
        let mut doc = Document::new();
        let div = doc.create_element("div", Vec::new());
        doc.append(doc.body(), div).unwrap();
        assert_eq!(doc.append(div, doc.body()), Err(TreeError::Cycle));
    }

    #[test]
    fn reads_text_attributes_and_title() {
        let doc = parse_html(
            "<title>Hello</title><p class=\"note\">A &amp; B</p><!-- hidden --><br><span>C</span>",
        );
        assert_eq!(doc.title(), "Hello");
        let paragraph = doc
            .children(doc.body())
            .iter()
            .copied()
            .find(|id| doc.get(*id).data().tag() == Some("p"))
            .unwrap();
        assert_eq!(doc.get(paragraph).data().attr("class"), Some("note"));
        assert_eq!(doc.text_content(paragraph), "A & B");
        let again = parse_html(&doc.to_html());
        let again_p = again
            .children(again.body())
            .iter()
            .copied()
            .find(|id| again.get(*id).data().tag() == Some("p"))
            .expect("paragraph");
        assert_eq!(again.text_content(again_p), "A & B");
        assert_eq!(again.get(again_p).data().attr("class"), Some("note"));
        let tags: Vec<_> = doc
            .children(doc.body())
            .iter()
            .filter_map(|id| doc.get(*id).data().tag())
            .collect();
        assert_eq!(tags, ["p", "br", "span"]);
    }

    #[test]
    fn serializes_replaced_text() {
        let mut doc = parse_html("<p id=\"out\">secret-marker</p>");
        let id = doc.get_element_by_id("out").unwrap();
        doc.set_text_content(id, "from-worker");
        let again = parse_html(&doc.to_html());
        let id2 = again.get_element_by_id("out").unwrap();
        assert_eq!(again.text_content(id2), "from-worker");
    }

    #[test]
    fn script_text_keeps_a_less_than() {
        let doc = parse_html("<script>if (a < b) {}</script><p>Hi</p>");
        let script = doc.find_element(doc.root(), "script").unwrap();
        assert_eq!(doc.text_content(script), "if (a < b) {}");
        assert_eq!(doc.scripts(), vec!["if (a < b) {}"]);
        assert!(doc.find_element(doc.body(), "p").is_some());
    }

    #[test]
    fn reads_srcset_lazy_images_and_keeps_pre_lines() {
        let doc = parse_html(
            "<img id=\"a\" srcset=\"https://cdn.example/a.jpg 480w, https://cdn.example/b.jpg 800w\"><img id=\"b\" src=\"data:image/gif;base64,AAAA\" data-src=\"https://cdn.example/c.jpg\"><picture><source srcset=\"https://cdn.example/d.jpg 1x\"><img id=\"c\"></picture><pre>one\r\ntwo</pre>",
        );
        let urls: Vec<_> = doc.images().into_iter().map(|(_, src)| src).collect();
        assert_eq!(
            urls,
            [
                "https://cdn.example/a.jpg".to_string(),
                "https://cdn.example/c.jpg".to_string(),
                "https://cdn.example/d.jpg".to_string(),
            ]
        );
        let pre = doc.find_element(doc.body(), "pre").unwrap();
        assert_eq!(doc.text_content(pre), "one\ntwo");
    }

    #[test]
    fn decodes_numeric_entities_and_skips_json_scripts() {
        let doc = parse_html(
            "<p id=\"t\">Tom&#39;s &#x26; tea</p>
             <div id=\"player&#45;placeholder\"></div>
             <script type=\"application/json\">{\"a\":1}</script>
             <script>var ready = 1;</script>",
        );
        let text = doc.get_element_by_id("t").unwrap();
        assert_eq!(doc.text_content(text).trim(), "Tom's & tea");
        assert!(doc.get_element_by_id("player-placeholder").is_some());
        assert_eq!(doc.scripts(), vec!["var ready = 1;"]);
    }

    #[test]
    fn lists_links_and_styles_in_document_order_with_media() {
        let doc = parse_html(
            "<head><link rel=stylesheet href=a.css media=' print '><style>p{}</style>\
             <style></style><link rel=icon href=i.png></head>\
             <body><style media=screen>b{}</style><link rel='Stylesheet alternate' href=b.css></body>",
        );
        assert_eq!(
            doc.style_sheets(),
            vec![
                StyleSheetSource::Link {
                    href: "a.css".to_string(),
                    integrity: None,
                    media: Some("print".to_string()),
                },
                StyleSheetSource::Style {
                    css: "p{}".to_string(),
                    media: None,
                },
                StyleSheetSource::Style {
                    css: "b{}".to_string(),
                    media: Some("screen".to_string()),
                },
                StyleSheetSource::Link {
                    href: "b.css".to_string(),
                    integrity: None,
                    media: None,
                },
            ]
        );
    }

    #[test]
    fn collects_stylesheet_links_in_order() {
        let doc = parse_html(
            "<head>
                <link rel=\"icon\" href=\"favicon.ico\">
                <link rel=\"stylesheet\" href=\"base.css\">
                <link rel=\"alternate stylesheet\" href=\"theme.css\">
             </head>",
        );
        assert_eq!(doc.stylesheet_links(), vec!["base.css", "theme.css"]);
    }

    #[test]
    fn queries_and_mutates_elements_by_id_and_selector() {
        let mut doc = parse_html("<div id=\"banner\" class=\"card primary\">Old</div>");
        let id = doc.get_element_by_id("banner").unwrap();
        assert_eq!(doc.query_selector(".primary"), Some(id));
        assert_eq!(doc.query_selector("div.card"), Some(id));
        doc.set_text_content(id, "Updated");
        doc.set_attribute(id, "data-state", "active");
        assert_eq!(doc.text_content(id), "Updated");
        assert_eq!(doc.get(id).data().attr("data-state"), Some("active"));
    }

    #[test]
    fn serializes_form_inputs_checkboxes_and_toggles_classes() {
        let mut doc = parse_html(
            "<form id=\"search-form\" action=\"/search\" method=\"POST\">
                <input id=\"q\" type=\"text\" name=\"query\" value=\"anvil engine\">
                <input id=\"exact\" type=\"checkbox\" name=\"exact\" value=\"1\" checked>
                <button id=\"go\" type=\"submit\">Search</button>
             </form>",
        );
        let q = doc.get_element_by_id("q").unwrap();
        let exact = doc.get_element_by_id("exact").unwrap();
        let go = doc.get_element_by_id("go").unwrap();
        let form = doc.enclosing_form(go).unwrap();

        assert!(doc.is_text_input(q));
        assert!(doc.is_checkbox_input(exact));
        assert!(doc.is_submit_control(go));

        let (action, method, body) = doc.serialize_form(form);
        assert_eq!(action, "/search");
        assert_eq!(method, "POST");
        assert_eq!(body, "query=anvil+engine&exact=1");

        doc.set_input_value(q, "a&b");
        assert!(!doc.toggle_checked(exact));
        let (_, _, body2) = doc.serialize_form(form);
        assert_eq!(body2, "query=a%26b");

        assert!(doc.toggle_class(form, "submitted"));
        assert!(doc.get(form).data().has_class("submitted"));
        assert!(!doc.toggle_class(form, "submitted"));
        assert!(!doc.get(form).data().has_class("submitted"));
    }

    #[test]
    fn caps_adversarial_dom_nesting_depth_and_attribute_counts() {
        let mut deep_html = String::new();
        for _ in 0..400 {
            deep_html.push_str("<div>");
        }
        deep_html.push_str("<p id=\"leaf\">Safe Leaf</p>");
        let doc = parse_html(&deep_html);
        let leaf = doc.get_element_by_id("leaf").unwrap();
        let mut depth = 0usize;
        let mut cursor = Some(leaf);
        while let Some(curr) = cursor {
            depth += 1;
            cursor = doc.get(curr).parent();
        }
        assert!(depth <= MAX_DOM_DEPTH + 4);
    }

    #[test]
    fn parses_tables_textareas_and_cycles_select_options() {
        let mut doc = parse_html(
            "<table id=\"tbl\">
               <tr><th>Col A<th>Col B
               <tr><td>1<td>2
             </table>
             <form id=\"f\" action=\"/save\" method=\"POST\">
               <textarea id=\"bio\" name=\"bio\">Line 1\nLine 2</textarea>
               <select id=\"tier\" name=\"tier\">
                 <option value=\"free\">Free Tier
                 <option value=\"pro\" selected>Pro Tier
                 <option value=\"max\">Max Tier
               </select>
             </form>",
        );
        let tbl = doc.get_element_by_id("tbl").unwrap();
        assert_eq!(doc.children(tbl).len(), 2);

        let tier = doc.get_element_by_id("tier").unwrap();
        assert!(doc.is_select_control(tier));
        assert_eq!(doc.select_value(tier), "pro");
        assert_eq!(doc.select_display_label(tier), "Pro Tier");

        assert_eq!(doc.cycle_select_option(tier), Some("max".to_string()));
        assert_eq!(doc.cycle_select_option(tier), Some("free".to_string()));

        let form = doc.get_element_by_id("f").unwrap();
        let (_, _, payload) = doc.serialize_form(form);
        assert_eq!(payload, "bio=Line+1%0ALine+2&tier=free");
    }

    #[test]
    fn supports_combinator_attribute_selectors_and_dom_tree_mutations() {
        let mut doc = parse_html(
            "<div id=\"wrap\" class=\"panel main\">
               <ul id=\"list\">
                 <li class=\"item first\" data-id=\"10\">Alpha</li>
                 <li class=\"item second\" data-id=\"20\">Beta</li>
               </ul>
             </div>",
        );
        let wrap = doc.get_element_by_id("wrap").unwrap();
        let list = doc.get_element_by_id("list").unwrap();
        let items = doc.query_selector_all("#wrap > ul li.item");
        assert_eq!(items.len(), 2);
        assert_eq!(doc.query_selector("li[data-id=\"20\"]"), Some(items[1]));
        assert_eq!(doc.closest(items[0], "div.panel"), Some(wrap));
        assert!(doc.contains_node(wrap, items[1]));
        assert_eq!(doc.first_element_child(list), Some(items[0]));
        assert_eq!(doc.last_element_child(list), Some(items[1]));
        assert_eq!(doc.next_element_sibling(items[0]), Some(items[1]));
        assert_eq!(doc.previous_element_sibling(items[1]), Some(items[0]));

        let inserted = doc.create_element("li", vec![("class".into(), "item mid".into())]);
        doc.set_text_content(inserted, "Middle");
        doc.insert_before(list, inserted, Some(items[1])).unwrap();
        assert_eq!(
            doc.element_children(list),
            vec![items[0], inserted, items[1]]
        );

        let cloned = doc.clone_node(inserted, true).unwrap();
        doc.replace_child(list, cloned, items[1]).unwrap();
        assert_eq!(doc.element_children(list), vec![items[0], inserted, cloned]);
        doc.remove_child(list, inserted).unwrap();
        assert_eq!(doc.element_children(list), vec![items[0], cloned]);
    }

    /// Parse on a worker thread and give up after a few seconds, so a parser
    /// that stops advancing fails the test instead of hanging the run.
    fn parse_with_deadline(input: &str) -> Option<(Vec<String>, String)> {
        let input = input.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let doc = parse_html(&input);
            let tags = doc
                .children(doc.body())
                .iter()
                .filter_map(|id| doc.get(*id).data().tag().map(str::to_string))
                .collect::<Vec<_>>();
            let text = doc.text_content(doc.body());
            let _ = tx.send((tags, text));
        });
        rx.recv_timeout(std::time::Duration::from_secs(5)).ok()
    }

    #[test]
    fn stray_less_than_in_text_is_kept_and_does_not_stall() {
        let (tags, text) = parse_with_deadline("<p>1 < 2</p><p>after</p>")
            .expect("parser stalled on a '<' that does not start a tag");
        assert_eq!(tags, ["p", "p"]);
        assert!(text.contains("1 < 2"), "text was {text:?}");
        assert!(text.contains("after"));
    }

    #[test]
    fn less_than_at_odd_places_does_not_stall() {
        for input in [
            "<",
            "<<",
            "a <",
            "< b",
            "<p>x <= y && y >= z</p>",
            "<p>a<1</p>",
            "<p>a< </p>",
            "<p>a <\u{e9}</p>",
            "<div>< <<< <3 <-- <=></div>",
            "<p a=1 \u{e9}\u{4e2d}>multi-byte attribute junk</p>",
        ] {
            assert!(
                parse_with_deadline(input).is_some(),
                "parser stalled on {input:?}"
            );
        }
        let (_, text) = parse_with_deadline("<p>a <\u{e9}</p>").unwrap();
        assert!(text.contains("a <\u{e9}"), "text was {text:?}");
    }

    #[test]
    fn isolates_noscript_and_template_styles_and_preserves_regex_slashes_in_scripts() {
        let mut doc = parse_html(
            "<html>
               <head>
                 <style>body { color: #111111; }</style>
                 <noscript><style>table,div,span,p{display:none}</style></noscript>
               </head>
               <body>
                 <noscript><div id=\"nojs\">Please enable JavaScript</div></noscript>
                 <template id=\"tpl\"><style>p { display: none; }</style><p id=\"in-tpl\">Hidden</p></template>
                 <div id=\"main\" class=\"card hero\" name=\"search-box\"><p>Real Content</p></div>
                 <script>
                   let rx = /https?:\\/\\//;
                   // trailing line comment
                   let ok = rx.test('https://example.com');
                 </script>
               </body>
             </html>",
        );
        assert_eq!(doc.embedded_css(), "body { color: #111111; }");
        assert!(doc.get_element_by_id("nojs").is_none());
        let scripts = doc.scripts();
        assert_eq!(scripts.len(), 1);
        assert!(
            scripts[0].contains("/https?:\\/\\//") && scripts[0].contains("rx.test"),
            "script was {:?}",
            scripts[0]
        );

        let main = doc.get_element_by_id("main").unwrap();
        assert_eq!(doc.document_element(), doc.html());
        assert_eq!(doc.head_element(), doc.head());
        assert_eq!(doc.body_element(), doc.body());
        assert_eq!(doc.get_elements_by_tag_name(doc.body(), "div"), vec![main]);
        assert_eq!(
            doc.get_elements_by_class_name(doc.body(), "hero card"),
            vec![main]
        );
        assert_eq!(doc.get_elements_by_name("search-box"), vec![main]);

        doc.insert_adjacent_html(main, "beforeend", "<span id=\"tail\">Tail</span>");
        assert!(doc.get_element_by_id("tail").is_some());
    }

    #[test]
    fn resolves_fragment_targets_by_id_anchor_name_and_percent_encoding() {
        let doc = parse_html(
            "<html><body>
               <h2 id=\"intro-section\">Intro</h2>
               <h2 id=\"sec 2\">Section Two</h2>
               <a name=\"legacy-anchor\">Legacy</a>
             </body></html>",
        );
        let intro = doc.get_element_by_id("intro-section").unwrap();
        let sec2 = doc.get_element_by_id("sec 2").unwrap();
        let legacy = doc.get_elements_by_name("legacy-anchor")[0];

        assert_eq!(doc.find_fragment_target("#intro-section"), Some(intro));
        assert_eq!(doc.find_fragment_target("sec%202"), Some(sec2));
        assert_eq!(doc.find_fragment_target("#legacy-anchor"), Some(legacy));
        assert_eq!(doc.find_fragment_target("#top"), Some(doc.html()));
        assert_eq!(doc.find_fragment_target("#missing"), None);
    }

    #[test]
    fn radio_groups_labels_details_and_focusable_inputs_work() {
        let mut doc = parse_html(
            "<html><body>
               <form id=\"f\">
                 <label id=\"lbl-a\" for=\"rad-a\">Option A</label>
                 <input id=\"rad-a\" type=\"radio\" name=\"plan\" value=\"a\" checked>
                 <label id=\"lbl-b\"><input id=\"rad-b\" type=\"radio\" name=\"plan\" value=\"b\">Option B</label>
                 <input id=\"user\" type=\"text\" name=\"u\">
                 <textarea id=\"bio\" name=\"b\"></textarea>
                 <input id=\"dis\" type=\"text\" disabled>
               </form>
               <details id=\"det\">
                 <summary id=\"sum\"><span id=\"sum-txt\">More</span></summary>
                 <input id=\"hidden-in-det\" type=\"text\">
               </details>
               <dialog id=\"dlg\"><input id=\"in-closed-dlg\" type=\"text\"></dialog>
             </body></html>",
        );
        let rad_a = doc.get_element_by_id("rad-a").unwrap();
        let rad_b = doc.get_element_by_id("rad-b").unwrap();
        let lbl_a = doc.get_element_by_id("lbl-a").unwrap();
        let lbl_b = doc.get_element_by_id("lbl-b").unwrap();
        let user = doc.get_element_by_id("user").unwrap();
        let bio = doc.get_element_by_id("bio").unwrap();
        let det = doc.get_element_by_id("det").unwrap();
        let sum_txt = doc.get_element_by_id("sum-txt").unwrap();
        let hidden_in_det = doc.get_element_by_id("hidden-in-det").unwrap();

        assert!(doc.is_radio_input(rad_a));
        assert!(doc.is_checked(rad_a));
        assert!(!doc.is_checked(rad_b));
        assert_eq!(doc.label_target(lbl_a), Some(rad_a));
        assert_eq!(doc.label_target(lbl_b), Some(rad_b));

        assert!(doc.select_radio(rad_b));
        assert!(!doc.is_checked(rad_a));
        assert!(doc.is_checked(rad_b));

        assert_eq!(doc.enclosing_summary_details(sum_txt), Some(det));
        assert!(!doc.is_details_open(det));
        assert_eq!(doc.focusable_inputs(), vec![user, bio]);

        assert!(doc.toggle_details(det));
        assert!(doc.is_details_open(det));
        assert_eq!(doc.focusable_inputs(), vec![user, bio, hidden_in_det]);
    }

    #[test]
    fn input_cursor_select_options_and_range_progress_work() {
        let mut doc = parse_html(
            "<html><body>
               <input id=\"txt\" type=\"text\" value=\"hello world\">
               <select id=\"sel\">
                 <option value=\"a\">Alpha</option>
                 <option value=\"b\" selected>Beta</option>
                 <option value=\"c\">Gamma</option>
               </select>
               <input id=\"rng\" type=\"range\" min=\"0\" max=\"200\" step=\"25\" value=\"50\">
               <progress id=\"prog\" value=\"30\" max=\"60\"></progress>
               <meter id=\"mtr\" min=\"10\" max=\"50\" value=\"30\"></meter>
             </body></html>",
        );
        let txt = doc.get_element_by_id("txt").unwrap();
        let sel = doc.get_element_by_id("sel").unwrap();
        let rng = doc.get_element_by_id("rng").unwrap();
        let prog = doc.get_element_by_id("prog").unwrap();
        let mtr = doc.get_element_by_id("mtr").unwrap();

        assert_eq!(doc.input_cursor(txt), 11);
        doc.set_input_cursor(txt, 5);
        assert_eq!(doc.insert_input_text_at_cursor(txt, ","), "hello, world");
        assert_eq!(doc.input_cursor(txt), 6);
        assert_eq!(doc.delete_input_at_cursor(txt), "hello,world");
        assert_eq!(doc.backspace_input_at_cursor(txt), "helloworld");
        doc.move_input_cursor_end(txt);
        doc.set_input_value(txt, "alpha beta gamma");
        doc.move_input_cursor_end(txt);
        assert_eq!(doc.delete_input_word_at_cursor(txt), "alpha beta ");

        let opts = doc.select_options_list(sel);
        assert_eq!(opts.len(), 3);
        assert!(opts[1].2);
        assert_eq!(doc.select_option_by_index(sel, 2), Some("c".to_string()));
        assert_eq!(doc.select_value(sel), "c");

        assert!(doc.is_range_input(rng));
        assert_eq!(doc.range_state(rng), (0.0, 200.0, 25.0, 50.0));
        assert_eq!(doc.set_range_ratio(rng, 0.62), "125");
        assert!((doc.control_fill_ratio(rng).unwrap() - 0.625).abs() < 1e-4);
        assert!((doc.control_fill_ratio(prog).unwrap() - 0.5).abs() < 1e-4);
        assert!((doc.control_fill_ratio(mtr).unwrap() - 0.5).abs() < 1e-4);
    }

    #[test]
    fn html5_error_recovery_handles_unclosed_tags_stray_closers_and_entities() {
        let doc = parse_html(
            "<div id=\"card\"></span></section>\
             <p id=\"p1\">Para 1<div id=\"blk\">Block</div>\
             <ul id=\"lst\"><li id=\"li1\">One<li id=\"li2\">Two</ul>\
             <dl id=\"defs\"><dt id=\"dt1\">Term<dd id=\"dd1\">Def</dl>\
             <p id=\"ent\">&copy; 2026 &mdash; &#147;Anvil&#148; &euro;50&hellip;</p>\
             </p></br>\
             </div>\
             <script>var unclosed = 42;",
        );
        let card = doc.get_element_by_id("card").unwrap();
        let blk = doc.get_element_by_id("blk").unwrap();
        let p1 = doc.get_element_by_id("p1").unwrap();
        // Stray `</span></section>` did not unwind `#card`: `#blk` is still inside `#card`.
        assert_eq!(doc.parent_element(blk), Some(card));
        // `<div id="blk">` auto-closed `<p id="p1">`, so `#blk` is a sibling of `#p1`, not inside `#p1`.
        assert_eq!(doc.parent_element(p1), Some(card));
        assert!(!doc.contains_node(p1, blk));

        // `<li>` auto-closed previous `<li>` inside `<ul>`
        let lst = doc.get_element_by_id("lst").unwrap();
        let li1 = doc.get_element_by_id("li1").unwrap();
        let li2 = doc.get_element_by_id("li2").unwrap();
        assert_eq!(doc.element_children(lst), vec![li1, li2]);

        // `<dd>` auto-closed `<dt>` inside `<dl>`
        let defs = doc.get_element_by_id("defs").unwrap();
        let dt1 = doc.get_element_by_id("dt1").unwrap();
        let dd1 = doc.get_element_by_id("dd1").unwrap();
        assert_eq!(doc.element_children(defs), vec![dt1, dd1]);

        // Named & Windows-1252 numeric character references
        let ent = doc.get_element_by_id("ent").unwrap();
        assert_eq!(
            doc.text_content(ent),
            "\u{00a9} 2026 \u{2014} \u{201c}Anvil\u{201d} \u{20ac}50\u{2026}"
        );

        // Unclosed `<script>` at EOF still extracted its script text without leaking tags
        assert_eq!(doc.scripts(), vec!["var unclosed = 42;"]);
    }

    #[test]
    fn selectors_level_4_sibling_combinators_nth_child_and_state_pseudos_work() {
        let doc = parse_html(
            "<div id=\"root\">\
               <h2 id=\"hdr\">Title</h2>\
               <p id=\"first-p\" class=\"item active\" data-tags=\"alpha beta\" lang=\"en-US\">One</p>\
               <p id=\"second-p\" class=\"item\" data-role=\"Admin\">Two</p>\
               <p id=\"third-p\" class=\"item active\">Three</p>\
               <a id=\"lnk\" href=\"https://example.com\">Link</a>\
               <input id=\"ro\" type=\"text\" readonly placeholder=\"Enter name\" value=\"\">\
               <details id=\"det-open\" open><summary>Open</summary></details>\
             </div>",
        );
        let first_p = doc.get_element_by_id("first-p").unwrap();
        let second_p = doc.get_element_by_id("second-p").unwrap();
        let third_p = doc.get_element_by_id("third-p").unwrap();
        let lnk = doc.get_element_by_id("lnk").unwrap();
        let ro = doc.get_element_by_id("ro").unwrap();
        let det_open = doc.get_element_by_id("det-open").unwrap();

        assert_eq!(doc.query_selector("h2 + p"), Some(first_p));
        assert_eq!(
            doc.query_selector_all("h2 ~ p.item"),
            vec![first_p, second_p, third_p]
        );
        assert_eq!(doc.query_selector("p:nth-of-type(2)"), Some(second_p));
        assert_eq!(
            doc.query_selector("p:nth-child(2 of .active)"),
            Some(third_p)
        );
        assert_eq!(
            doc.query_selector("[data-tags~=beta][lang|=en]"),
            Some(first_p)
        );
        assert_eq!(
            doc.query_selector("[data-role=\"admin\" i]"),
            Some(second_p)
        );
        assert_eq!(doc.query_selector("a:any-link"), Some(lnk));
        assert_eq!(
            doc.query_selector("input:read-only:placeholder-shown"),
            Some(ro)
        );
        assert_eq!(doc.query_selector("details:open"), Some(det_open));
    }

    #[test]
    fn dirty_style_tracking_and_shadow_slot_composition_work() {
        let mut doc = parse_html(
            "<div id=\"host\" class=\"card\">\
               <span id=\"s-title\" slot=\"title\">Custom Header</span>\
               <p id=\"s-body\">Body Copy</p>\
             </div>",
        );
        assert!(!doc.has_dirty_styles());
        let host = doc.get_element_by_id("host").unwrap();
        let s_title = doc.get_element_by_id("s-title").unwrap();
        let s_body = doc.get_element_by_id("s-body").unwrap();

        let sr = doc.attach_shadow(host, true);
        assert!(doc.has_dirty_styles());
        assert_eq!(doc.take_dirty_style_nodes(), vec![host, sr]);
        assert!(!doc.has_dirty_styles());

        doc.set_inner_html(
            sr,
            "<header id=\"hdr\" part=\"banner\"><slot id=\"slot-title\" name=\"title\">Fallback</slot></header>\
             <main id=\"mn\"><slot id=\"slot-default\">Default Fallback</slot></main>",
        );
        assert!(doc.has_dirty_styles());
        let _ = doc.take_dirty_style_nodes();

        let hdr = doc.query_selector_within(sr, "#hdr").unwrap();
        let mn = doc.query_selector_within(sr, "#mn").unwrap();
        let slot_title = doc.query_selector_within(sr, "#slot-title").unwrap();
        let slot_default = doc.query_selector_within(sr, "#slot-default").unwrap();

        // Host's composed children are the shadow root's children (`#hdr`, `#mn`)
        assert_eq!(doc.composed_children(host).as_ref(), &[hdr, mn]);
        // Named slot projects `#s-title`
        assert_eq!(doc.assigned_nodes(slot_title, false), vec![s_title]);
        assert_eq!(doc.composed_children(slot_title).as_ref(), &[s_title]);
        assert_eq!(doc.assigned_slot(s_title), Some(slot_title));
        // Default slot projects `#s-body`
        assert_eq!(doc.assigned_nodes(slot_default, false), vec![s_body]);
        assert_eq!(doc.composed_children(slot_default).as_ref(), &[s_body]);
        assert_eq!(doc.assigned_slot(s_body), Some(slot_default));

        // Selector matching for `::slotted(...)`, `::part(...)`, and `:host(...)`
        assert!(doc.matches_selector(s_title, "::slotted(span)"));
        assert!(doc.matches_selector(hdr, "::part(banner)"));
        assert!(doc.matches_selector(sr, ":host(.card)"));
    }
}
