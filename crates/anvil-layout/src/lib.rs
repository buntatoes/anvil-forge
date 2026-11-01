// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato
//! Block, inline, flex, grid, table, and positioned layout, plus hit testing.
//!
//! Third stage of the pipeline. anvil-style has computed a `StyleMap` for a
//! anvil-dom `Document`. This crate borrows both and returns an owned
//! [`LayoutBox`] tree. anvil-paint walks that tree into a display list. This
//! crate does not paint pixels.
//!
//! The origin is the top-left of the initial containing block. X increases to
//! the right and y increases downward, in CSS pixels. [`hit_test`] uses the
//! same space. A point on the right or bottom edge is outside the box.
//!
//! `position: fixed` is placed against `(0, 0)` with the parent's containing
//! width and the parent's border-box height. It is not a separate viewport.
//! `position: absolute` uses the parent's border box. A percentage width uses
//! the containing block width. A percentage height uses the containing block
//! height when that height is set, and is treated as `auto` when it is not.
//! A box with a definite `height` keeps it even when its content is taller;
//! the content overflows, and `overflow: hidden` clips it.
//! `em` uses the element's font size. `rem` uses [`StyleMap::root_font_px`].
//! `ch` uses [`ch_width`] at the element's font size.
//! Flex layout supports `row`, `row-reverse`, `column`, and `column-reverse`,
//! `flex-wrap` (`nowrap`, `wrap`, `wrap-reverse`), `flex-grow`, `flex-shrink`
//! (clamped at minimum content width), `flex-basis`, `justify-content`,
//! `align-items`, and `align-self`. Grid layout resolves explicit tracks (`px`,
//! `%`, `fr`, `minmax()`, `repeat()`), `grid-template-areas`, `grid-auto-flow`
//! (`row`, `column`, `dense`), `grid-auto-rows`, and `grid-auto-columns`.
//! Table layout shares intrinsic column widths across rows inside a `<table>`
//! (`thead`, `tbody`, `tfoot`, `tr`, `td`, `th`) with `colspan` and `rowspan`
//! occupancy tracking and `border-collapse`. Proportional text measurement uses
//! [`measure_proportional`] (with `anvil-ttf` font metrics when loaded).
//!
//! `position: sticky` stays in flow. Its scrollport is the nearest ancestor
//! with hidden overflow, which never scrolls, so the shift is applied here;
//! or the viewport, in which case the box keeps its in-flow rect and carries
//! a [`StickyConstraint`] that paint, raster, and [`hit_test_scrolled`] apply
//! for the current scroll. There is no horizontal page scroll, so `left` and
//! `right` only act against a clip scrollport or a viewport narrower than the
//! page. Sticky boxes paint and hit-test above plain siblings with the same
//! `z-index`.
//!
//! `transform` does not change layout. After placement, each transformed
//! element box that is not inline gets a [`Transform2D`] in page space
//! around its `transform-origin`; paint draws through it and [`hit_test`]
//! maps points back through its inverse. Transformed boxes paint and
//! hit-test above plain siblings with the same `z-index`. They do not
//! become the containing block for fixed or absolute descendants, and a
//! sticky box inside one still measures against the untransformed view.
//!
//! Grid columns come from `grid-template-columns` and rows from
//! `grid-template-rows`, with `grid-auto-rows` for rows past that list.
//! Tracks accept `px`, `%`, `fr`, `auto`, `minmax()`, and `repeat()`. An
//! `auto` column acts as `1fr`; an `auto` row fits its tallest single-row
//! item. A row `fr` shares the definite container height, or fits the
//! largest item per `fr` when the height is not definite.
//!
//! Items are placed by `grid-column`, `grid-row`, their `-start` / `-end`
//! longhands, and numeric `grid-area`, with line numbers, negative lines,
//! and `span N`. The rest are auto-placed row by row (sparse), and spans
//! push later items around. Named lines and areas, `grid-auto-flow`,
//! `grid-auto-columns`, and implicit tracks before the first line are not
//! supported; an item that would start before line 1 starts at line 1.

use std::collections::HashMap;
use std::sync::OnceLock;

mod svg;

pub use anvil_dom::NodeId;
use anvil_dom::{Document, NodeData};
pub use anvil_image::Image;
pub use anvil_style::{
    AlignContent, AlignItems, BoxShadow, BoxSizing, Color, Edge, FlexDirection, FlexWrap,
    FontFaceRule, FontFamily, GridAutoFlow, JustifyContent, LinearGradient, ObjectFit, Overflow,
    Position, TextAlign, TextTransform, Visibility, WhiteSpace,
};
use anvil_style::{
    ComputedStyle, Display, GridAutoRepeat, GridLine, GridPlacement, GridTrack, MAX_GRID_LINE,
    Size, StyleMap, TrackBreadth, TransformFn,
};
use anvil_ttf::TtfFont;

/// Axis-aligned box. `x` and `y` are the top-left in y-down CSS pixels.
/// Width and height grow to the right and downward.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    /// True when `(x, y)` is inside, including the top and left edges and
    /// excluding the right and bottom edges. A zero-area rect contains nothing.
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }

    /// True when `self` and `other` have a positive overlapping area.
    pub fn intersects(self, other: Self) -> bool {
        self.width > 0.0
            && self.height > 0.0
            && other.width > 0.0
            && other.height > 0.0
            && self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }
}

/// One painted line fragment. `text` is owned. `x` and `y` are the top-left
/// of the run in y-down pixels. `node` is the text node, or the element that
/// produced a form label or list marker.
#[derive(Clone, Debug, PartialEq)]
pub struct TextRun {
    pub node: NodeId,
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub font_size: f32,
    pub color: Color,
    pub underline: bool,
    pub line_through: bool,
}

/// A border box and the children laid out inside it.
///
/// `rect` is the border box, margin excluded, in y-down pixels. `text` and
/// `children` are owned. `image` is an owned clone of a decoded anvil-image
/// bitmap when one was supplied for this node; otherwise `None`.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutBox {
    pub node: NodeId,
    pub rect: Rect,
    pub position: Position,
    pub z_index: i32,
    pub overflow: Overflow,
    pub background: Option<Color>,
    pub background_gradient: Option<LinearGradient>,
    pub box_shadow: Option<BoxShadow>,
    pub border: Edge,
    pub border_color: Color,
    pub border_colors: [Color; 4],
    pub border_radius: f32,
    pub outline_width: f32,
    pub outline_color: Color,
    pub outline_offset: f32,
    pub scrollable_overflow: bool,
    pub scroll_offset: (f32, f32),
    pub scroll_size: (f32, f32),
    pub object_fit: ObjectFit,
    pub image: Option<Image>,
    /// Set on a `position: sticky` box whose scrollport is the viewport.
    /// The rect stays at the in-flow position; paint and raster apply the
    /// shift for the current scroll. A sticky box inside an `overflow`
    /// clip is already shifted here and has `None`.
    pub sticky: Option<StickyConstraint>,
    /// Page-space map from `transform` and `transform-origin`, applied to
    /// this box and its descendants when drawn and hit-tested. The rect and
    /// every descendant rect stay untransformed. `None` when the box has
    /// no transform.
    pub transform: Option<Transform2D>,
    /// `opacity` of this box, applied to it and its descendants as one
    /// group. 1 is opaque.
    pub opacity: f32,
    pub text: Vec<TextRun>,
    pub children: Vec<LayoutBox>,
}

impl LayoutBox {
    /// Sort key among siblings for paint and hit-test order: `z-index`
    /// first, then boxes that form their own layer after plain boxes.
    ///
    /// Sticky, transformed, and translucent boxes are such layers, so
    /// content that follows a sticky header in the tree scrolls under it
    /// rather than over it. Relative, absolute, and fixed boxes keep plain
    /// tree order, as before.
    pub fn stacking_key(&self) -> (i32, u8) {
        let layered =
            self.position == Position::Sticky || self.transform.is_some() || self.opacity < 1.0;
        (self.z_index, u8::from(layered))
    }

    /// Find the [`LayoutBox`] for `target` anywhere in this layout subtree.
    pub fn find_node_box(&self, target: NodeId) -> Option<&LayoutBox> {
        if self.node == target {
            return Some(self);
        }
        for child in &self.children {
            if let Some(found) = child.find_node_box(target) {
                return Some(found);
            }
        }
        None
    }

    /// Find the innermost scrollable overflow container under `(x, y)` in page coordinates,
    /// returning `(node_id, scroll_offset, max_scroll)` where `max_scroll = (max_scroll_x, max_scroll_y)`.
    #[allow(clippy::type_complexity)]
    pub fn find_scrollable_container(
        &self,
        x: f32,
        y: f32,
    ) -> Option<(NodeId, (f32, f32), (f32, f32))> {
        if !self.rect.contains(x, y) && self.overflow == Overflow::Hidden {
            return None;
        }
        let mut order: Vec<(usize, &LayoutBox)> = self.children.iter().enumerate().collect();
        order.sort_by_key(|(idx, child)| (child.stacking_key(), *idx));
        for (_, child) in order.into_iter().rev() {
            if let Some(found) = child.find_scrollable_container(x, y) {
                return Some(found);
            }
        }
        if self.scrollable_overflow && self.rect.contains(x, y) {
            let client_w = (self.rect.width - self.border.left - self.border.right).max(0.0);
            let client_h = (self.rect.height - self.border.top - self.border.bottom).max(0.0);
            let max_x = (self.scroll_size.0 - client_w).max(0.0);
            let max_y = (self.scroll_size.1 - client_h).max(0.0);
            if max_x > 0.5 || max_y > 0.5 {
                return Some((self.node, self.scroll_offset, (max_x, max_y)));
            }
        }
        None
    }

    /// Find the border-box [`Rect`] for `target` anywhere in this layout subtree.
    pub fn find_node_rect(&self, target: NodeId) -> Option<Rect> {
        if self.node == target {
            return Some(self.rect);
        }
        for child in &self.children {
            if let Some(r) = child.find_node_rect(target) {
                return Some(r);
            }
        }
        for run in &self.text {
            if run.node == target {
                return Some(Rect {
                    x: run.x,
                    y: run.y,
                    width: run.width,
                    height: line_height(run.font_size),
                });
            }
        }
        None
    }

    /// Collect all [`TextRun`] fragments in this layout tree whose bounding rect intersects `region`.
    pub fn collect_text_runs_in_rect(&self, region: Rect, out: &mut Vec<TextRun>) {
        for run in &self.text {
            let r = Rect {
                x: run.x,
                y: run.y,
                width: run.width.max(1.0),
                height: line_height(run.font_size),
            };
            if r.intersects(region) {
                out.push(run.clone());
            }
        }
        for child in &self.children {
            child.collect_text_runs_in_rect(region, out);
        }
    }
}

/// A 2D affine map in page space, y downward.
///
/// A point `(x, y)` maps to `(a*x + c*y + e, b*x + d*y + f)`, the same
/// order as CSS `matrix(a, b, c, d, e, f)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform2D {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Transform2D {
    /// The map that leaves every point where it is.
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// A move by `(x, y)`.
    pub fn translate(x: f32, y: f32) -> Self {
        Self {
            e: x,
            f: y,
            ..Self::IDENTITY
        }
    }

    /// `self` after `inner`: the result maps `p` to `self(inner(p))`.
    pub fn then_inner(&self, inner: &Self) -> Self {
        Self {
            a: self.a * inner.a + self.c * inner.b,
            b: self.b * inner.a + self.d * inner.b,
            c: self.a * inner.c + self.c * inner.d,
            d: self.b * inner.c + self.d * inner.d,
            e: self.a * inner.e + self.c * inner.f + self.e,
            f: self.b * inner.e + self.d * inner.f + self.f,
        }
    }

    /// Map one point.
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// The reverse map, or `None` when the map flattens the plane or is not
    /// finite. A box with such a transform is neither drawn nor hit.
    pub fn inverse(&self) -> Option<Self> {
        let det = self.a * self.d - self.b * self.c;
        if !det.is_finite() || det.abs() < 1e-12 {
            return None;
        }
        let inv = Self {
            a: self.d / det,
            b: -self.b / det,
            c: -self.c / det,
            d: self.a / det,
            e: (self.c * self.f - self.d * self.e) / det,
            f: (self.b * self.e - self.a * self.f) / det,
        };
        inv.is_finite().then_some(inv)
    }

    /// True when every coefficient is finite.
    pub fn is_finite(&self) -> bool {
        [self.a, self.b, self.c, self.d, self.e, self.f]
            .iter()
            .all(|v| v.is_finite())
    }

    /// The same map for a subtree moved by `(dx, dy)`.
    pub fn translated(&self, dx: f32, dy: f32) -> Self {
        Self::translate(dx, dy)
            .then_inner(self)
            .then_inner(&Self::translate(-dx, -dy))
    }

    /// The map in a space scaled by `s` about the origin, as for zoom.
    pub fn scaled(&self, s: f32) -> Self {
        Self {
            e: self.e * s,
            f: self.f * s,
            ..*self
        }
    }

    /// Axis-aligned bounds of `rect` after the map.
    pub fn map_rect(&self, rect: Rect) -> Rect {
        let corners = [
            self.apply(rect.x, rect.y),
            self.apply(rect.x + rect.width, rect.y),
            self.apply(rect.x, rect.y + rect.height),
            self.apply(rect.x + rect.width, rect.y + rect.height),
        ];
        let (mut x0, mut y0) = corners[0];
        let (mut x1, mut y1) = corners[0];
        for (x, y) in corners {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        Rect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        }
    }
}

/// The page-space map for a box with `style`'s `transform` and
/// `transform-origin`, laid out at `rect`.
///
/// Percentages in `translate()` and the origin are of the border box.
/// Functions apply right to left, as in CSS, around the origin.
fn box_transform(rect: Rect, style: &ComputedStyle, styles: &StyleMap) -> Transform2D {
    let len = |size, basis: f32| used_length(size, Some(basis), style, styles).unwrap_or(0.0);
    let mut local = Transform2D::IDENTITY;
    for function in style.transform.functions() {
        let step = match *function {
            TransformFn::Translate(x, y) => {
                Transform2D::translate(len(x, rect.width), len(y, rect.height))
            }
            TransformFn::Scale(x, y) => Transform2D {
                a: x,
                d: y,
                ..Transform2D::IDENTITY
            },
            TransformFn::Rotate(angle) => {
                let (sin, cos) = angle.sin_cos();
                Transform2D {
                    a: cos,
                    b: sin,
                    c: -sin,
                    d: cos,
                    e: 0.0,
                    f: 0.0,
                }
            }
            TransformFn::Skew(x, y) => Transform2D {
                b: y.tan(),
                c: x.tan(),
                ..Transform2D::IDENTITY
            },
            TransformFn::Matrix([a, b, c, d, e, f]) => Transform2D { a, b, c, d, e, f },
        };
        local = local.then_inner(&step);
    }
    let ox = rect.x + len(style.transform_origin.x, rect.width);
    let oy = rect.y + len(style.transform_origin.y, rect.height);
    Transform2D::translate(ox, oy)
        .then_inner(&local)
        .then_inner(&Transform2D::translate(-ox, -oy))
}

/// Give every box its [`LayoutBox::transform`] and [`LayoutBox::opacity`],
/// once boxes have their final positions. Only element boxes that are not
/// inline are transformable; text keeps its parent's style but not a box of
/// its own. Also computes `scroll_size` and applies `scroll_offset` for
/// `overflow: auto | scroll` containers.
fn resolve_effects(tree: &mut LayoutBox, doc: &Document, styles: &StyleMap) {
    let style = styles.get(tree.node);
    let is_element = doc.get(tree.node).data().tag().is_some();
    if is_element && !style.transform.is_none() && style.display != Display::Inline {
        tree.transform = Some(box_transform(tree.rect, style, styles));
    }
    let client_x = tree.rect.x + tree.border.left;
    let client_y = tree.rect.y + tree.border.top;
    let client_w = (tree.rect.width - tree.border.left - tree.border.right).max(0.0);
    let client_h = (tree.rect.height - tree.border.top - tree.border.bottom).max(0.0);
    let mut max_right = client_x + client_w;
    let mut max_bottom = client_y + client_h;
    for run in &tree.text {
        max_right = max_right.max(run.x + run.width + style.padding.right);
        max_bottom = max_bottom.max(run.y + line_height(run.font_size) + style.padding.bottom);
    }
    for child in &tree.children {
        if child.position != Position::Fixed {
            let cs = styles.get(child.node);
            max_right = max_right
                .max(child.rect.x + child.rect.width + cs.margin.right + style.padding.right);
            max_bottom = max_bottom
                .max(child.rect.y + child.rect.height + cs.margin.bottom + style.padding.bottom);
        }
    }
    let scroll_w = client_w.max((max_right - client_x).max(0.0));
    let scroll_h = client_h.max((max_bottom - client_y).max(0.0));
    tree.scroll_size = (scroll_w, scroll_h);
    if tree.scrollable_overflow {
        let (raw_sx, raw_sy) = doc.node_scroll_offset(tree.node);
        let sx = raw_sx.clamp(0.0, (scroll_w - client_w).max(0.0));
        let sy = raw_sy.clamp(0.0, (scroll_h - client_h).max(0.0));
        tree.scroll_offset = (sx, sy);
        if sx != 0.0 || sy != 0.0 {
            for run in &mut tree.text {
                run.x -= sx;
                run.y -= sy;
            }
            for child in &mut tree.children {
                if child.position != Position::Fixed {
                    offset_box(child, -sx, -sy);
                }
            }
        }
    }
    if is_element {
        tree.opacity = style.opacity;
        if style.visibility != Visibility::Visible {
            tree.background = None;
            tree.background_gradient = None;
            tree.box_shadow = None;
            tree.border_color = Color {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            };
            tree.border_colors = [tree.border_color; 4];
            tree.outline_width = 0.0;
            tree.image = None;
            tree.text.clear();
        }
    }
    for child in &mut tree.children {
        resolve_effects(child, doc, styles);
    }
}

/// What a sticky box needs to find its shift for a scroll position.
///
/// All rects are in the same page space as [`LayoutBox::rect`], y downward.
/// Insets are CSS pixels; `None` is `auto` on that side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StickyConstraint {
    /// The box's border box at its in-flow position.
    pub rect: Rect,
    /// The box's margins. The margin box never leaves `containing`.
    pub margin: Edge,
    /// Content box of the parent, which bounds the shift.
    pub containing: Rect,
    pub top: Option<f32>,
    pub right: Option<f32>,
    pub bottom: Option<f32>,
    pub left: Option<f32>,
}

impl StickyConstraint {
    /// Shift `(dx, dy)` that keeps the box inside `scrollport` by the insets.
    ///
    /// `scrollport` is the visible part of the scrolling box in page space;
    /// for the viewport that is `(0, scroll_y, width, height)`. A box already
    /// far enough inside does not move. The shift never moves the margin box
    /// out of `containing`, and never moves the box back past its in-flow
    /// position. When `top` and `bottom` both apply and conflict, `top`
    /// wins; `left` wins over `right` the same way.
    pub fn offset(&self, scrollport: Rect) -> (f32, f32) {
        let r = self.rect;
        let m = self.margin;
        let cb = self.containing;
        let axis = |start: f32,
                    size: f32,
                    margin_start: f32,
                    margin_end: f32,
                    cb_start: f32,
                    cb_size: f32,
                    port_start: f32,
                    port_size: f32,
                    inset_start: Option<f32>,
                    inset_end: Option<f32>|
         -> f32 {
            let mut shift = 0.0_f32;
            if let Some(inset) = inset_end {
                let limit = port_start + port_size - inset;
                let end = start + size;
                if end > limit {
                    let floor = (cb_start - (start - margin_start)).min(0.0);
                    shift = (limit - end).max(floor);
                }
            }
            if let Some(inset) = inset_start {
                let limit = port_start + inset;
                if start + shift < limit {
                    let ceiling = (cb_start + cb_size - (start + size + margin_end)).max(0.0);
                    shift = (limit - start).min(ceiling).max(shift);
                }
            }
            if shift.is_finite() { shift } else { 0.0 }
        };
        let dx = axis(
            r.x,
            r.width,
            m.left,
            m.right,
            cb.x,
            cb.width,
            scrollport.x,
            scrollport.width,
            self.left,
            self.right,
        );
        let dy = axis(
            r.y,
            r.height,
            m.top,
            m.bottom,
            cb.y,
            cb.height,
            scrollport.y,
            scrollport.height,
            self.top,
            self.bottom,
        );
        (dx, dy)
    }

    /// The same constraint moved by `(dx, dy)`.
    pub fn translated(&self, dx: f32, dy: f32) -> Self {
        let mv = |r: Rect| Rect {
            x: r.x + dx,
            y: r.y + dy,
            ..r
        };
        Self {
            rect: mv(self.rect),
            containing: mv(self.containing),
            ..*self
        }
    }
}

/// A slice of inline text before it is broken into line tokens.
/// `text` is owned. The node is the text node or the element that synthesized it.
#[derive(Clone, Debug)]
struct Segment {
    node: NodeId,
    text: String,
    font_size: f32,
    line_height: Option<f32>,
    color: Color,
    underline: bool,
    line_through: bool,
    letter_spacing: f32,
    word_spacing: f32,
}

impl Segment {
    fn plain(
        node: NodeId,
        text: String,
        font_size: f32,
        line_height: Option<f32>,
        color: Color,
        underline: bool,
    ) -> Self {
        Self {
            node,
            text,
            font_size,
            line_height,
            color,
            underline,
            line_through: false,
            letter_spacing: 0.0,
            word_spacing: 0.0,
        }
    }
}

/// One word, a single space, a forced break (`\n`), or one character split
/// from a word that was wider than the line.
#[derive(Clone, Debug)]
struct Token {
    node: NodeId,
    text: String,
    font_size: f32,
    line_height: Option<f32>,
    color: Color,
    underline: bool,
    line_through: bool,
    letter_spacing: f32,
    word_spacing: f32,
}

/// Apply `text-transform` (`uppercase`, `lowercase`, `capitalize`) to `text`.
pub fn apply_text_transform(text: &str, transform: TextTransform) -> String {
    match transform {
        TextTransform::None => text.to_string(),
        TextTransform::Uppercase => text.to_uppercase(),
        TextTransform::Lowercase => text.to_lowercase(),
        TextTransform::Capitalize => {
            let mut out = String::with_capacity(text.len());
            let mut new_word = true;
            for ch in text.chars() {
                if ch.is_whitespace() {
                    new_word = true;
                    out.push(ch);
                } else if new_word {
                    new_word = false;
                    for upper in ch.to_uppercase() {
                        out.push(upper);
                    }
                } else {
                    out.push(ch);
                }
            }
            out
        }
    }
}

/// Lay out `doc` from the `html` element in a viewport `viewport_width` pixels wide.
///
/// `doc` and `styles` are borrowed. No images are supplied, so an `<img>` or
/// `<video>` uses its attributes or the fallback size. No control is focused.
/// The returned tree is owned. The origin is the top-left; y grows downward.
pub fn layout_document(doc: &Document, styles: &StyleMap, viewport_width: f32) -> LayoutBox {
    let empty = HashMap::new();
    layout_document_interactive(doc, styles, viewport_width, &empty, None)
}

/// Like [`layout_document`], with decoded images keyed by node id.
///
/// `images` is borrowed. A missing entry leaves the element without a bitmap.
/// Intrinsic width and height come from the image when the attributes are absent.
pub fn layout_document_with_images(
    doc: &Document,
    styles: &StyleMap,
    viewport_width: f32,
    images: &HashMap<NodeId, Image>,
) -> LayoutBox {
    layout_document_interactive(doc, styles, viewport_width, images, None)
}

/// Lay out `doc`, optionally focusing one control.
///
/// A negative viewport width is treated as 0. When `focused_input` is a text
/// control, the laid-out text gains a trailing `|`. A password is masked with
/// one `*` per character. A block box whose id matches also uses the focus
/// border color. `doc`, `styles`, and `images` are borrowed. The tree is owned.
pub fn layout_document_interactive(
    doc: &Document,
    styles: &StyleMap,
    viewport_width: f32,
    images: &HashMap<NodeId, Image>,
    focused_input: Option<NodeId>,
) -> LayoutBox {
    let mut tree = layout_node(
        doc,
        styles,
        images,
        focused_input,
        doc.html(),
        // Origin: top-left of the initial containing block. Y increases downward.
        0.0,
        0.0,
        viewport_width.max(0.0),
        None,
        None,
    );
    resolve_sticky(&mut tree, styles, None);
    resolve_effects(&mut tree, doc, styles);
    tree
}

/// Incremental layout cache that memoizes computed [`LayoutBox`] subtrees and
/// full-document layout passes across restages when DOM structure, computed
/// styles, images, scroll offsets, and viewport dimensions are unchanged.
#[derive(Clone, Debug, Default)]
pub struct LayoutCache {
    doc_fingerprint: u64,
    cached_root: Option<LayoutBox>,
    node_boxes: HashMap<NodeId, LayoutBox>,
    hits: usize,
    misses: usize,
}

impl LayoutCache {
    /// Create an empty [`LayoutCache`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear all cached layout state.
    pub fn invalidate(&mut self) {
        self.doc_fingerprint = 0;
        self.cached_root = None;
        self.node_boxes.clear();
    }

    /// Invalidate a specific DOM node and force the next incremental pass to recompute.
    pub fn invalidate_node(&mut self, node: NodeId) {
        self.node_boxes.remove(&node);
        self.doc_fingerprint = 0;
        self.cached_root = None;
    }

    /// Number of incremental layout cache hits recorded on this cache.
    pub fn hits(&self) -> usize {
        self.hits
    }

    /// Number of incremental layout cache misses recorded on this cache.
    pub fn misses(&self) -> usize {
        self.misses
    }

    /// Look up the most recently cached [`LayoutBox`] for `node`.
    pub fn cached_box(&self, node: NodeId) -> Option<&LayoutBox> {
        self.node_boxes.get(&node)
    }

    fn index_subtree(&mut self, b: &LayoutBox) {
        self.node_boxes.insert(b.node, b.clone());
        for child in &b.children {
            self.index_subtree(child);
        }
    }
}

fn fnv1a_mix(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

fn hash_node_recursive(
    doc: &Document,
    styles: &StyleMap,
    images: &HashMap<NodeId, Image>,
    id: NodeId,
    mut hash: u64,
) -> u64 {
    hash = fnv1a_mix(hash, &id.index().to_le_bytes());
    let node = doc.get(id);
    match node.data() {
        NodeData::Document => hash = fnv1a_mix(hash, &[1]),
        NodeData::Text(t) => {
            hash = fnv1a_mix(hash, &[2]);
            hash = fnv1a_mix(hash, t.as_bytes());
        }
        NodeData::Element { tag, attrs } => {
            hash = fnv1a_mix(hash, &[3]);
            hash = fnv1a_mix(hash, tag.as_bytes());
            for (k, v) in attrs {
                hash = fnv1a_mix(hash, k.as_bytes());
                hash = fnv1a_mix(hash, v.as_bytes());
            }
        }
    }
    let (sx, sy) = doc.node_scroll_offset(id);
    hash = fnv1a_mix(hash, &sx.to_bits().to_le_bytes());
    hash = fnv1a_mix(hash, &sy.to_bits().to_le_bytes());
    let style_dbg = format!("{:?}", styles.get(id));
    hash = fnv1a_mix(hash, style_dbg.as_bytes());
    if let Some(img) = images.get(&id) {
        hash = fnv1a_mix(hash, &img.width.to_le_bytes());
        hash = fnv1a_mix(hash, &img.height.to_le_bytes());
    }
    for &child in doc.children(id) {
        hash = hash_node_recursive(doc, styles, images, child, hash);
    }
    hash
}

/// Lay out `doc` incrementally using `cache`. When the document, styles,
/// images, scroll offsets, focused input, and viewport dimensions match the
/// cached pass, the cached [`LayoutBox`] tree is returned directly.
pub fn layout_document_incremental(
    doc: &Document,
    styles: &StyleMap,
    viewport_width: f32,
    images: &HashMap<NodeId, Image>,
    focused_input: Option<NodeId>,
    cache: &mut LayoutCache,
) -> LayoutBox {
    let mut fp: u64 = 0xcbf2_9ce4_8422_2325;
    fp = fnv1a_mix(fp, &viewport_width.max(0.0).to_bits().to_le_bytes());
    fp = fnv1a_mix(fp, &styles.viewport_width().to_bits().to_le_bytes());
    fp = fnv1a_mix(fp, &styles.viewport_height().to_bits().to_le_bytes());
    fp = fnv1a_mix(
        fp,
        &focused_input
            .map(|n| n.index())
            .unwrap_or(usize::MAX)
            .to_le_bytes(),
    );
    fp = hash_node_recursive(doc, styles, images, doc.html(), fp);

    if cache.doc_fingerprint == fp
        && let Some(root) = &cache.cached_root
    {
        cache.hits += 1;
        return root.clone();
    }

    cache.misses += 1;
    let tree = layout_document_interactive(doc, styles, viewport_width, images, focused_input);
    cache.node_boxes.clear();
    cache.index_subtree(&tree);
    cache.doc_fingerprint = fp;
    cache.cached_root = Some(tree.clone());
    tree
}

/// Record or apply `position: sticky` once every box has its final position.
///
/// `clip_port` is the padding box of the nearest ancestor with hidden
/// overflow, which acts as a scrollport that never scrolls. A sticky box
/// under one is shifted here. A sticky box whose scrollport is the viewport
/// gets a [`StickyConstraint`] instead, because its shift depends on the
/// scroll position. Percent insets use the scrollport size; for the viewport
/// that is the viewport on `styles`. The containing block is the parent's
/// content box.
fn resolve_sticky(tree: &mut LayoutBox, styles: &StyleMap, clip_port: Option<Rect>) {
    let parent_style = styles.get(tree.node);
    let content = Rect {
        x: tree.rect.x + tree.border.left + parent_style.padding.left,
        y: tree.rect.y + tree.border.top + parent_style.padding.top,
        width: (tree.rect.width
            - tree.border.left
            - tree.border.right
            - parent_style.padding.left
            - parent_style.padding.right)
            .max(0.0),
        height: (tree.rect.height
            - tree.border.top
            - tree.border.bottom
            - parent_style.padding.top
            - parent_style.padding.bottom)
            .max(0.0),
    };
    let own_port = if tree.overflow == Overflow::Hidden {
        Some(Rect {
            x: tree.rect.x + tree.border.left,
            y: tree.rect.y + tree.border.top,
            width: (tree.rect.width - tree.border.left - tree.border.right).max(0.0),
            height: (tree.rect.height - tree.border.top - tree.border.bottom).max(0.0),
        })
    } else {
        clip_port
    };
    for child in &mut tree.children {
        let style = *styles.get(child.node);
        if style.position == Position::Sticky {
            let (port_w, port_h) = match own_port {
                Some(port) => (port.width, port.height),
                None => (styles.viewport_width(), styles.viewport_height()),
            };
            let constraint = StickyConstraint {
                rect: child.rect,
                margin: style.margin,
                containing: content,
                top: used_length(style.top, Some(port_h), &style, styles),
                right: used_length(style.right, Some(port_w), &style, styles),
                bottom: used_length(style.bottom, Some(port_h), &style, styles),
                left: used_length(style.left, Some(port_w), &style, styles),
            };
            match own_port {
                Some(port) => {
                    let (dx, dy) = constraint.offset(port);
                    offset_box(child, dx, dy);
                    child.sticky = None;
                }
                None => child.sticky = Some(constraint),
            }
        }
        resolve_sticky(child, styles, own_port);
    }
}

/// The front-most node under `(x, y)` in y-down pixels.
///
/// `tree` is borrowed. Children are tested from highest `z-index` to lowest,
/// and from last sibling to first when the index ties. Text runs are tested
/// after children, in reverse order, and a hit returns the run's node, which
/// is often a text node. The box itself is last. `None` when the point misses
/// the tree. Overflow `hidden` rejects a point outside this box before
/// descendants are tested, so clipped children are not hits.
///
/// Sticky boxes are tested at their in-flow position. Use
/// [`hit_test_scrolled`] to test them where they are drawn. Transformed
/// boxes are tested where they are drawn: the point is mapped back into the
/// box's untransformed space, so a rotated box is hit only inside its
/// rotated outline, and a box whose transform cannot be inverted is not hit.
pub fn hit_test(tree: &LayoutBox, x: f32, y: f32) -> Option<NodeId> {
    hit_test_inner(tree, x, y, None)
}

/// Like [`hit_test`], with sticky boxes shifted for `viewport`.
///
/// `viewport` is the visible page area in page space, such as
/// `(0, scroll_y, width, height)`. `x` and `y` are page coordinates, so a
/// caller adds the scroll offset to a window point first.
pub fn hit_test_scrolled(tree: &LayoutBox, x: f32, y: f32, viewport: Rect) -> Option<NodeId> {
    hit_test_inner(tree, x, y, Some(viewport))
}

/// Shared walk for [`hit_test`] and [`hit_test_scrolled`].
///
/// A sticky shift moves the point the other way instead of moving boxes,
/// and moves the viewport with it so nested sticky boxes see the same view.
/// A transform maps the point back through its inverse the same way; the
/// viewport is left as it is.
fn hit_test_inner(tree: &LayoutBox, x: f32, y: f32, viewport: Option<Rect>) -> Option<NodeId> {
    let (mut x, mut y, mut viewport) = (x, y, viewport);
    if let (Some(sticky), Some(port)) = (tree.sticky, viewport) {
        let (dx, dy) = sticky.offset(port);
        x -= dx;
        y -= dy;
        viewport = Some(Rect {
            x: port.x - dx,
            y: port.y - dy,
            ..port
        });
    }
    if let Some(transform) = tree.transform {
        (x, y) = transform.inverse()?.apply(x, y);
    }
    // Hidden overflow clips hit testing to the border box, not the padding edge.
    if tree.overflow == Overflow::Hidden && !tree.rect.contains(x, y) {
        return None;
    }
    let mut order: Vec<(usize, &LayoutBox)> = tree.children.iter().enumerate().collect();
    order.sort_by_key(|(idx, child)| (child.stacking_key(), *idx));
    for (_, child) in order.into_iter().rev() {
        if let Some(hit) = hit_test_inner(child, x, y, viewport) {
            return Some(hit);
        }
    }
    for run in tree.text.iter().rev() {
        let rect = Rect {
            x: run.x,
            y: run.y,
            width: run.width,
            height: line_height(run.font_size),
        };
        if rect.contains(x, y) {
            return Some(run.node);
        }
    }
    tree.rect.contains(x, y).then_some(tree.node)
}

#[allow(clippy::too_many_arguments)]
/// Lay out `id` inside a containing block that is `containing_width` pixels wide.
///
/// `x` and `y` are the top-left of the margin edge, y downward. A `tr` uses
/// the table-row algorithm even if its display is flex. Other flex containers
/// use [`layout_flex`]. Everything else is a block. Relative offsets are applied
/// after that used position: a positive `left` or `top` moves right or down.
/// `left` wins over `right`, and `top` wins over `bottom`.
fn layout_node(
    doc: &Document,
    styles: &StyleMap,
    images: &HashMap<NodeId, Image>,
    focused_input: Option<NodeId>,
    id: NodeId,
    x: f32,
    y: f32,
    containing_width: f32,
    containing_height: Option<f32>,
    list_marker: Option<String>,
) -> LayoutBox {
    let style = *styles.get(id);
    let mut b = if doc.get(id).data().tag() == Some("tr") {
        layout_table_row(
            doc,
            styles,
            images,
            focused_input,
            id,
            x,
            y,
            containing_width,
            containing_height,
        )
    } else if style.display == Display::Grid {
        layout_grid(
            doc,
            styles,
            images,
            focused_input,
            id,
            x,
            y,
            containing_width,
            containing_height,
        )
    } else if style.display == Display::Flex {
        layout_flex(
            doc,
            styles,
            images,
            focused_input,
            id,
            x,
            y,
            containing_width,
            containing_height,
        )
    } else {
        layout_block(
            doc,
            styles,
            images,
            focused_input,
            id,
            x,
            y,
            containing_width,
            containing_height,
            list_marker,
        )
    };
    if style.position == Position::Relative {
        // Positive top/left move down/right. Positive bottom/right move up/left.
        // A percentage uses the containing block. An unresolved percentage is 0.
        let dx = match used_length(style.left, Some(containing_width), &style, styles) {
            Some(l) => l,
            None => used_length(style.right, Some(containing_width), &style, styles)
                .map(|r| -r)
                .unwrap_or(0.0),
        };
        let dy = match used_length(style.top, containing_height, &style, styles) {
            Some(t) => t,
            None => used_length(style.bottom, containing_height, &style, styles)
                .map(|bt| -bt)
                .unwrap_or(0.0),
        };
        offset_box(&mut b, dx, dy);
    }
    b
}

/// Resolve `size` against an optional containing-block `basis`.
///
/// `None` means `auto`, including a percentage whose basis was not passed.
/// `em` uses the element's font size. `rem` uses the root font size.
/// `vw`, `vh`, `vmin`, and `vmax` use the viewport on `styles`. `ch` uses
/// [`ch_width`] at the element's font size.
fn used_length(
    size: Size,
    basis: Option<f32>,
    style: &ComputedStyle,
    styles: &StyleMap,
) -> Option<f32> {
    let vw = styles.viewport_width();
    let vh = styles.viewport_height();
    match size {
        Size::Auto => None,
        Size::Vw(value) => Some(vw * value / 100.0),
        Size::Vh(value) => Some(vh * value / 100.0),
        Size::Vmin(value) => Some(vw.min(vh) * value / 100.0),
        Size::Vmax(value) => Some(vw.max(vh) * value / 100.0),
        Size::Ch(value) => Some(ch_width(style.font_size) * value),
        Size::Percent(_) => size.used_px(basis?, style.font_size, styles.root_font_px()),
        Size::Calc { percent, .. } if percent != 0.0 => {
            size.used_px(basis?, style.font_size, styles.root_font_px())
        }
        other => other.used_px(0.0, style.font_size, styles.root_font_px()),
    }
}

/// Used `(row-gap, column-gap)` of a flex or grid container in pixels.
/// A row-gap percentage needs a definite content height and is 0 without
/// one; a column-gap percentage is of the content width. Negative results
/// are 0.
fn used_gaps(
    style: &ComputedStyle,
    content_width: f32,
    content_height: Option<f32>,
    styles: &StyleMap,
) -> (f32, f32) {
    let px = |size: Size, basis: Option<f32>| {
        used_length(size, basis, style, styles)
            .unwrap_or(0.0)
            .max(0.0)
    };
    (
        px(style.row_gap, content_height),
        px(style.column_gap, Some(content_width)),
    )
}

/// Border-box width from `width`, `min-width`, `max-width`, and `box-sizing`.
///
/// A definite `width` is the content width when `content-box` (padding and border
/// are added) or the border-box width when `border-box`. `auto`, and a percentage
/// with no basis, fills the containing block minus horizontal margins.
fn resolve_border_width(style: &ComputedStyle, containing_width: f32, styles: &StyleMap) -> f32 {
    let padding_h = style.padding.left + style.padding.right;
    let border_h = style.border.left + style.border.right;
    let chrome_h = padding_h + border_h;
    let to_border = |len: f32| match style.box_sizing {
        BoxSizing::ContentBox => (len + chrome_h).max(0.0),
        BoxSizing::BorderBox => len.max(chrome_h),
    };
    let mut border_width = match used_length(style.width, Some(containing_width), style, styles) {
        Some(width) => to_border(width),
        None => (containing_width - style.margin.left - style.margin.right).max(0.0),
    };
    if let Some(max_w) = used_length(style.max_width, Some(containing_width), style, styles) {
        border_width = border_width.min(to_border(max_w));
    }
    if let Some(min_w) = used_length(style.min_width, Some(containing_width), style, styles) {
        border_width = border_width.max(to_border(min_w));
    }
    border_width
}

fn resolve_border_x(
    style: &ComputedStyle,
    x: f32,
    containing_width: f32,
    border_width: f32,
) -> f32 {
    let free = (containing_width - border_width - style.margin.left - style.margin.right).max(0.0);
    if style.margin_left_auto && style.margin_right_auto {
        x + style.margin.left + free * 0.5
    } else if style.margin_left_auto {
        x + style.margin.left + free
    } else {
        x + style.margin.left
    }
}

fn clamp_border_width(
    style: &ComputedStyle,
    containing_width: f32,
    styles: &StyleMap,
    mut border_width: f32,
) -> f32 {
    let chrome_h =
        style.padding.left + style.padding.right + style.border.left + style.border.right;
    let to_border = |len: f32| match style.box_sizing {
        BoxSizing::ContentBox => (len + chrome_h).max(0.0),
        BoxSizing::BorderBox => len.max(chrome_h),
    };
    if let Some(max_w) = used_length(style.max_width, Some(containing_width), style, styles) {
        border_width = border_width.min(to_border(max_w));
    }
    if let Some(min_w) = used_length(style.min_width, Some(containing_width), style, styles) {
        border_width = border_width.max(to_border(min_w));
    }
    border_width.max(0.0)
}

fn intrinsic_box_width(
    doc: &Document,
    styles: &StyleMap,
    images: &HashMap<NodeId, Image>,
    id: NodeId,
    containing_width: f32,
) -> f32 {
    let style = styles.get(id);
    let chrome_h =
        style.padding.left + style.padding.right + style.border.left + style.border.right;
    if let Some(w) = used_length(style.width, Some(containing_width), style, styles) {
        let bw = match style.box_sizing {
            BoxSizing::ContentBox => w + chrome_h,
            BoxSizing::BorderBox => w.max(chrome_h),
        };
        return clamp_border_width(style, containing_width, styles, bw);
    }
    if let (Some(ratio), Some(h)) = (
        style.aspect_ratio,
        used_length(style.height, None, style, styles),
    ) && ratio > 0.0
    {
        let chrome_v =
            style.padding.top + style.padding.bottom + style.border.top + style.border.bottom;
        let content_h = match style.box_sizing {
            BoxSizing::ContentBox => h.max(0.0),
            BoxSizing::BorderBox => (h - chrome_v).max(0.0),
        };
        return clamp_border_width(
            style,
            containing_width,
            styles,
            content_h * ratio + chrome_h,
        );
    }
    let data = doc.get(id).data();
    let tag = data.tag().unwrap_or("");
    let raw_content_w = if tag == "img" || tag == "video" {
        data.attr("width")
            .and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
            .or_else(|| images.get(&id).map(|img| img.width as f32))
            .unwrap_or(120.0)
    } else if tag == "svg" {
        svg::intrinsic_svg_size(doc, id).0
    } else if tag == "canvas" {
        data.attr("width")
            .and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
            .unwrap_or(300.0)
    } else if doc.is_checkbox_input(id) {
        20.0 - chrome_h
    } else if tag == "input" && doc.is_submit_control(id) {
        let lbl = data.attr("value").unwrap_or("Submit");
        measure(lbl, style.font_size) + 8.0
    } else if doc.is_text_input(id) {
        360.0_f32.min(containing_width.max(60.0)) - chrome_h
    } else if doc.is_select_control(id) {
        let lbl = doc.select_display_label(id);
        (measure(&lbl, style.font_size) + 32.0).max(140.0 - chrome_h)
    } else if tag == "button" {
        measure(&doc.text_content(id), style.font_size) + 8.0
    } else {
        let before_w = styles
            .before_style(id)
            .map(|(txt, bs)| measure(txt, bs.font_size))
            .unwrap_or(0.0);
        let after_w = styles
            .after_style(id)
            .map(|(txt, as_)| measure(txt, as_.font_size))
            .unwrap_or(0.0);
        let text_w = measure(doc.text_content(id).trim(), style.font_size) + before_w + after_w;
        let mut child_row_w = 0.0_f32;
        let mut child_max_w = 0.0_f32;
        for &cid in doc.children(id) {
            if is_skipped(doc, styles, cid) {
                continue;
            }
            if matches!(doc.get(cid).data(), NodeData::Element { .. }) {
                let cstyle = styles.get(cid);
                let cw = intrinsic_box_width(doc, styles, images, cid, containing_width)
                    + cstyle.margin.left
                    + cstyle.margin.right;
                if (style.display == Display::Flex
                    && matches!(
                        style.flex_direction,
                        FlexDirection::Row | FlexDirection::RowReverse
                    ))
                    || cstyle.inline_level
                {
                    child_row_w += cw;
                } else {
                    child_max_w = child_max_w.max(cw);
                }
            }
        }
        text_w.max(child_row_w).max(child_max_w)
    };
    let bw = (raw_content_w.max(0.0) + chrome_h).min(containing_width.max(chrome_h));
    clamp_border_width(style, containing_width, styles, bw)
}

fn min_content_box_width(
    doc: &Document,
    styles: &StyleMap,
    images: &HashMap<NodeId, Image>,
    id: NodeId,
    containing_width: f32,
) -> f32 {
    if let NodeData::Text(t) = doc.get(id).data() {
        let style = styles.get(id);
        return t
            .split_whitespace()
            .map(|w| measure(w, style.font_size))
            .fold(0.0_f32, f32::max);
    }
    let style = styles.get(id);
    let chrome_h =
        style.padding.left + style.padding.right + style.border.left + style.border.right;
    if let Some(w) = used_length(style.width, Some(containing_width), style, styles) {
        let bw = match style.box_sizing {
            BoxSizing::ContentBox => w + chrome_h,
            BoxSizing::BorderBox => w.max(chrome_h),
        };
        return clamp_border_width(style, containing_width, styles, bw);
    }
    let data = doc.get(id).data();
    let tag = data.tag().unwrap_or("");
    let raw_content_w = if tag == "img" || tag == "video" || tag == "svg" || tag == "canvas" {
        return intrinsic_box_width(doc, styles, images, id, containing_width);
    } else if style.white_space == WhiteSpace::Nowrap || style.white_space == WhiteSpace::Pre {
        measure(doc.text_content(id).trim(), style.font_size)
    } else {
        let longest_word = doc
            .text_content(id)
            .split_whitespace()
            .map(|w| measure(w, style.font_size))
            .fold(0.0_f32, f32::max);
        let mut child_min_w = 0.0_f32;
        for &cid in doc.children(id) {
            if is_skipped(doc, styles, cid) {
                continue;
            }
            if matches!(doc.get(cid).data(), NodeData::Element { .. }) {
                let cstyle = styles.get(cid);
                let cw = min_content_box_width(doc, styles, images, cid, containing_width)
                    + cstyle.margin.left
                    + cstyle.margin.right;
                child_min_w = child_min_w.max(cw);
            }
        }
        longest_word.max(child_min_w)
    };
    let bw = raw_content_w.max(0.0) + chrome_h;
    clamp_border_width(style, containing_width, styles, bw)
}

fn max_content_box_width(
    doc: &Document,
    styles: &StyleMap,
    images: &HashMap<NodeId, Image>,
    id: NodeId,
    containing_width: f32,
) -> f32 {
    if let NodeData::Text(t) = doc.get(id).data() {
        let style = styles.get(id);
        return measure(t.trim(), style.font_size);
    }
    let style = styles.get(id);
    if let Some(w) = used_length(style.width, Some(containing_width), style, styles) {
        let chrome_h =
            style.padding.left + style.padding.right + style.border.left + style.border.right;
        let bw = match style.box_sizing {
            BoxSizing::ContentBox => w + chrome_h,
            BoxSizing::BorderBox => w.max(chrome_h),
        };
        return clamp_border_width(style, containing_width, styles, bw);
    }
    intrinsic_box_width(doc, styles, images, id, f32::MAX)
}

fn flush_inline_blocks(
    pending: &mut Vec<LayoutBox>,
    styles: &StyleMap,
    content_x: f32,
    cursor_y: &mut f32,
    content_width: f32,
    text_align: TextAlign,
    out: &mut Vec<LayoutBox>,
) {
    if pending.is_empty() {
        return;
    }
    let items = std::mem::take(pending);
    let mut rows: Vec<Vec<LayoutBox>> = Vec::new();
    let mut current: Vec<LayoutBox> = Vec::new();
    let mut row_w = 0.0_f32;
    for item in items {
        let cstyle = styles.get(item.node);
        let outer_w = cstyle.margin.left + item.rect.width + cstyle.margin.right;
        if !current.is_empty() && row_w + outer_w > content_width {
            rows.push(std::mem::take(&mut current));
            row_w = 0.0;
        }
        row_w += outer_w;
        current.push(item);
    }
    if !current.is_empty() {
        rows.push(current);
    }
    for row in rows {
        let total_w: f32 = row
            .iter()
            .map(|b| {
                let cs = styles.get(b.node);
                cs.margin.left + b.rect.width + cs.margin.right
            })
            .sum();
        let row_h: f32 = row
            .iter()
            .map(|b| {
                let cs = styles.get(b.node);
                cs.margin.top + b.rect.height + cs.margin.bottom
            })
            .fold(0.0_f32, f32::max);
        let mut cur_x = match text_align {
            TextAlign::Center => content_x + ((content_width - total_w) * 0.5).max(0.0),
            TextAlign::Right => content_x + (content_width - total_w).max(0.0),
            TextAlign::Left | TextAlign::Justify => content_x,
        };
        for mut b in row {
            let cs = styles.get(b.node);
            let outer_h = cs.margin.top + b.rect.height + cs.margin.bottom;
            let y_off = ((row_h - outer_h) * 0.5).max(0.0);
            let target_x = cur_x + cs.margin.left;
            let target_y = *cursor_y + y_off + cs.margin.top;
            let dx = target_x - b.rect.x;
            let dy = target_y - b.rect.y;
            offset_box(&mut b, dx, dy);
            cur_x += cs.margin.left + b.rect.width + cs.margin.right;
            out.push(b);
        }
        *cursor_y += row_h;
    }
}

#[allow(clippy::too_many_arguments)]
/// Block layout. In-flow children stack downward from the content top.
///
/// Absolute and fixed children are collected and placed after the in-flow
/// height is known, so they do not advance `cursor_y`. Inline runs are flushed
/// before each block child. List items receive a marker: `N. ` under `ol`,
/// otherwise `- `. The marker's node is the `li`.
fn layout_block(
    doc: &Document,
    styles: &StyleMap,
    images: &HashMap<NodeId, Image>,
    focused_input: Option<NodeId>,
    id: NodeId,
    x: f32,
    y: f32,
    containing_width: f32,
    containing_height: Option<f32>,
    list_marker: Option<String>,
) -> LayoutBox {
    let style = *styles.get(id);
    let padding = style.padding;
    let margin = style.margin;
    let border = style.border;
    let child_cb_height = used_length(style.height, containing_height, &style, styles);

    let mut border_width = resolve_border_width(&style, containing_width, styles);
    let data = doc.get(id).data();
    let tag = data.tag().unwrap_or("");
    let is_img = tag == "img" || tag == "video";
    let is_svg = tag == "svg";
    let is_canvas = tag == "canvas";
    let is_checkbox = doc.is_checkbox_input(id);
    let is_radio = doc.is_radio_input(id);
    let is_range = doc.is_range_input(id);
    let is_bar_ctrl = matches!(tag, "progress" | "meter");
    let is_text_ctrl = doc.is_text_input(id);
    let is_textarea = tag == "textarea";
    let is_select = doc.is_select_control(id);
    let is_submit_input = tag == "input" && doc.is_submit_control(id);
    let mut decoded_image = if is_img {
        images.get(&id).cloned()
    } else if is_canvas {
        doc.canvas_surface(id)
            .and_then(|s| Image::from_rgba(s.width, s.height, s.rgba.clone()).ok())
            .or_else(|| images.get(&id).cloned())
    } else {
        None
    };

    if matches!(style.width, Size::Auto) {
        if let (Some(ratio), Some(def_h)) = (style.aspect_ratio, child_cb_height)
            && ratio > 0.0
        {
            let chrome_v = padding.top + padding.bottom + border.top + border.bottom;
            let content_h = match style.box_sizing {
                BoxSizing::ContentBox => def_h.max(0.0),
                BoxSizing::BorderBox => (def_h - chrome_v).max(0.0),
            };
            border_width =
                content_h * ratio + padding.left + padding.right + border.left + border.right;
        } else if is_img {
            let attr_w = data
                .attr("width")
                .and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
                .or_else(|| decoded_image.as_ref().map(|img| img.width as f32))
                .unwrap_or(120.0);
            border_width = attr_w + padding.left + padding.right + border.left + border.right;
        } else if is_svg {
            let (sw, _) = svg::intrinsic_svg_size(doc, id);
            border_width = sw + padding.left + padding.right + border.left + border.right;
        } else if is_canvas {
            let attr_w = data
                .attr("width")
                .and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
                .or_else(|| decoded_image.as_ref().map(|img| img.width as f32))
                .unwrap_or(300.0);
            border_width = attr_w + padding.left + padding.right + border.left + border.right;
        } else if is_checkbox {
            border_width = 20.0;
        } else if is_radio {
            border_width = 32.0;
        } else if is_range || is_bar_ctrl {
            border_width = 160.0_f32.min(containing_width.max(60.0));
        } else if is_submit_input {
            let label = data.attr("value").unwrap_or("Submit");
            border_width = measure(label, style.font_size)
                + padding.left
                + padding.right
                + border.left
                + border.right
                + 8.0;
        } else if is_textarea {
            let cols = data
                .attr("cols")
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(32.0)
                .clamp(8.0, 120.0);
            border_width = (cols * 8.0 + padding.left + padding.right + border.left + border.right)
                .min(containing_width.max(80.0));
        } else if is_select {
            let label = doc.select_display_label(id);
            border_width = (measure(&label, style.font_size)
                + 32.0
                + padding.left
                + padding.right
                + border.left
                + border.right)
                .max(140.0)
                .min(containing_width.max(60.0));
        } else if is_text_ctrl {
            border_width = 360.0_f32.min(containing_width.max(60.0));
        } else if tag == "button" {
            let label = doc.text_content(id);
            border_width = (measure(&label, style.font_size)
                + padding.left
                + padding.right
                + border.left
                + border.right
                + 8.0)
                .min(containing_width.max(48.0));
        } else if (style.inline_level && style.display != Display::Inline)
            || is_decorated_inline(&style)
        {
            border_width = intrinsic_box_width(doc, styles, images, id, containing_width);
        }
        border_width = clamp_border_width(&style, containing_width, styles, border_width);
    }

    let content_width =
        (border_width - padding.left - padding.right - border.left - border.right).max(0.0);
    let border_x = resolve_border_x(&style, x, containing_width, border_width);
    let border_y = y + margin.top;
    let content_x = border_x + border.left + padding.left;
    let mut cursor_y = border_y + border.top + padding.top;
    let mut children = Vec::new();
    let mut text = Vec::new();
    let mut inline_ids = Vec::new();
    let mut pending_inline_blocks: Vec<LayoutBox> = Vec::new();
    let mut pending_marker = list_marker;
    if tag == "summary"
        && pending_marker.is_none()
        && let Some(parent) = doc.parent_element(id)
        && doc.get(parent).data().tag() == Some("details")
    {
        pending_marker = Some(if doc.is_details_open(parent) {
            "v ".to_string()
        } else {
            "> ".to_string()
        });
    }

    if is_img && decoded_image.is_none() {
        if let Some(alt) = data.attr("alt")
            && !alt.trim().is_empty()
        {
            let segments = vec![Segment::plain(
                id,
                format!("[{alt}]"),
                style.font_size,
                style.line_height,
                style.color,
                false,
            )];
            let (runs, h) = place_lines(&segments, content_x, cursor_y, content_width, false);
            text.extend(runs);
            cursor_y += h;
        }
    } else if is_checkbox {
        let mark = if doc.is_checked(id) { "X" } else { " " };
        let segments = vec![Segment::plain(
            id,
            mark.to_string(),
            style.font_size,
            style.line_height,
            style.color,
            false,
        )];
        let (runs, h) = place_lines(
            &segments,
            content_x,
            cursor_y,
            content_width.max(16.0),
            true,
        );
        text.extend(runs);
        cursor_y += h;
    } else if is_radio {
        let mark = if doc.is_checked(id) { "(o)" } else { "( )" };
        let segments = vec![Segment::plain(
            id,
            mark.to_string(),
            style.font_size,
            style.line_height,
            style.color,
            false,
        )];
        let (runs, h) = place_lines(
            &segments,
            content_x,
            cursor_y,
            content_width.max(24.0),
            true,
        );
        text.extend(runs);
        cursor_y += h;
    } else if is_submit_input {
        let label = data.attr("value").unwrap_or("Submit");
        let segments = vec![Segment::plain(
            id,
            label.to_string(),
            style.font_size,
            style.line_height,
            style.color,
            false,
        )];
        let (runs, h) = place_lines(
            &segments,
            content_x,
            cursor_y,
            content_width.max(24.0),
            false,
        );
        text.extend(runs);
        cursor_y += h;
    } else if is_select {
        let label = format!("{}  v", doc.select_display_label(id));
        let segments = vec![Segment::plain(
            id,
            label,
            style.font_size,
            style.line_height,
            style.color,
            false,
        )];
        let (runs, h) = place_lines(
            &segments,
            content_x,
            cursor_y,
            content_width.max(24.0),
            false,
        );
        text.extend(runs);
        cursor_y += h;
    } else if (tag == "input" || is_textarea) && is_text_ctrl {
        let raw_val = doc.input_value(id);
        let is_pw = data
            .attr("type")
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("password"));
        let val = if is_pw {
            "*".repeat(raw_val.chars().count())
        } else {
            raw_val
        };
        let is_focused = focused_input == Some(id);
        let cur_idx = doc.input_cursor(id);
        let (display_str, text_color) = if is_focused {
            let mut vchars: Vec<char> = val.chars().collect();
            let pos = cur_idx.min(vchars.len());
            vchars.insert(pos, '|');
            (vchars.into_iter().collect::<String>(), style.color)
        } else if !val.is_empty() {
            (val, style.color)
        } else if let Some(ph) = data.attr("placeholder") {
            (
                ph.to_string(),
                Color {
                    r: 138,
                    g: 148,
                    b: 166,
                    a: 255,
                },
            )
        } else {
            (" ".to_string(), style.color)
        };
        let single_line_str = if !is_textarea {
            let max_chars = ((content_width.max(24.0) / (style.font_size * 0.5).max(1.0)).floor()
                as usize)
                .max(4);
            let chars: Vec<char> = display_str.chars().collect();
            if chars.len() > max_chars {
                if is_focused {
                    let end = (cur_idx + 2).min(chars.len()).max(max_chars);
                    let start = end.saturating_sub(max_chars);
                    chars[start..end].iter().collect()
                } else {
                    chars[..max_chars].iter().collect()
                }
            } else {
                display_str
            }
        } else {
            display_str
        };
        let segments = vec![Segment::plain(
            id,
            single_line_str,
            style.font_size,
            style.line_height,
            text_color,
            false,
        )];
        // Single-line controls do not wrap. Overflow keeps the tail while focused
        // and the head otherwise. Textareas wrap inside the content box.
        let wrap_w = if !is_textarea {
            content_width.max(4096.0)
        } else {
            content_width.max(16.0)
        };
        let (runs, h) = place_lines(&segments, content_x, cursor_y, wrap_w, is_textarea);
        text.extend(runs);
        cursor_y += h;
    }

    let parent_tag = tag;
    let mut li_index = 0_usize;
    let mut out_of_flow = Vec::new();
    let mut pending_before_inline: Option<(String, ComputedStyle)> = None;

    if tag != "input" && !is_textarea && !is_select && !is_svg && !is_bar_ctrl {
        if let Some((b_txt, b_style)) = styles.before_style(id)
            && b_style.display != Display::None
        {
            if b_style.display == Display::Inline && !is_decorated_inline(b_style) {
                if !b_txt.is_empty() {
                    pending_before_inline = Some((b_txt.clone(), *b_style));
                }
            } else {
                let pbox = layout_pseudo_box(
                    id,
                    b_txt,
                    b_style,
                    styles,
                    content_x,
                    cursor_y,
                    content_width,
                    child_cb_height,
                );
                if (b_style.inline_level && b_style.display != Display::Inline)
                    || is_decorated_inline(b_style)
                {
                    pending_inline_blocks.push(pbox);
                } else {
                    cursor_y += b_style.margin.top + pbox.rect.height + b_style.margin.bottom;
                    children.push(pbox);
                }
            }
        }
        for child in doc.composed_children(id).iter() {
            if is_skipped(doc, styles, *child) {
                continue;
            }
            let child_style = styles.get(*child);
            if matches!(child_style.position, Position::Absolute | Position::Fixed) {
                out_of_flow.push(*child);
                continue;
            }
            if is_block_box(doc, styles, *child) {
                place_inlines(
                    doc,
                    styles,
                    id,
                    &mut pending_marker,
                    &mut pending_before_inline,
                    None,
                    &mut inline_ids,
                    content_x,
                    &mut cursor_y,
                    content_width,
                    &mut text,
                );
                let child_tag = doc.get(*child).data().tag().unwrap_or("");
                let child_is_inline_block = (child_style.inline_level
                    && child_style.display != Display::Inline)
                    || is_decorated_inline(child_style)
                    || (matches!(
                        child_tag,
                        "img" | "video" | "canvas" | "svg" | "progress" | "meter"
                    ) && child_style.inline_level);
                if child_is_inline_block {
                    let ib_box = layout_node(
                        doc,
                        styles,
                        images,
                        focused_input,
                        *child,
                        0.0,
                        0.0,
                        content_width,
                        child_cb_height,
                        None,
                    );
                    pending_inline_blocks.push(ib_box);
                    continue;
                }
                flush_inline_blocks(
                    &mut pending_inline_blocks,
                    styles,
                    content_x,
                    &mut cursor_y,
                    content_width,
                    style.text_align,
                    &mut children,
                );
                let child_marker = if child_tag == "li" {
                    li_index += 1;
                    if parent_tag == "ol" {
                        Some(format!("{li_index}. "))
                    } else {
                        Some("- ".to_string())
                    }
                } else {
                    None
                };
                let mut child_box = layout_node(
                    doc,
                    styles,
                    images,
                    focused_input,
                    *child,
                    content_x,
                    cursor_y,
                    content_width,
                    child_cb_height,
                    child_marker,
                );
                if parent_tag == "center"
                    && !child_style.margin_left_auto
                    && !child_style.margin_right_auto
                {
                    let outer_w =
                        child_style.margin.left + child_box.rect.width + child_style.margin.right;
                    if outer_w < content_width {
                        offset_box(&mut child_box, (content_width - outer_w) * 0.5, 0.0);
                    }
                }
                cursor_y +=
                    child_style.margin.top + child_box.rect.height + child_style.margin.bottom;
                children.push(child_box);
            } else {
                flush_inline_blocks(
                    &mut pending_inline_blocks,
                    styles,
                    content_x,
                    &mut cursor_y,
                    content_width,
                    style.text_align,
                    &mut children,
                );
                inline_ids.push(*child);
            }
        }
        flush_inline_blocks(
            &mut pending_inline_blocks,
            styles,
            content_x,
            &mut cursor_y,
            content_width,
            style.text_align,
            &mut children,
        );
        let after_entry = styles
            .after_style(id)
            .filter(|(_, a_style)| a_style.display != Display::None);
        let after_is_inline_text = after_entry.is_some_and(|(_, a_style)| {
            a_style.display == Display::Inline && !is_decorated_inline(a_style)
        });
        place_inlines(
            doc,
            styles,
            id,
            &mut pending_marker,
            &mut pending_before_inline,
            if after_is_inline_text {
                after_entry.and_then(|(txt, st)| (!txt.is_empty()).then_some((txt.as_str(), st)))
            } else {
                None
            },
            &mut inline_ids,
            content_x,
            &mut cursor_y,
            content_width,
            &mut text,
        );
        if let Some((a_txt, a_style)) = after_entry
            && !after_is_inline_text
        {
            let pbox = layout_pseudo_box(
                id,
                a_txt,
                a_style,
                styles,
                content_x,
                cursor_y,
                content_width,
                child_cb_height,
            );
            if (a_style.inline_level && a_style.display != Display::Inline)
                || is_decorated_inline(a_style)
            {
                pending_inline_blocks.push(pbox);
                flush_inline_blocks(
                    &mut pending_inline_blocks,
                    styles,
                    content_x,
                    &mut cursor_y,
                    content_width,
                    style.text_align,
                    &mut children,
                );
            } else {
                cursor_y += a_style.margin.top + pbox.rect.height + a_style.margin.bottom;
                children.push(pbox);
            }
        }
        stretch_table_rowspan_cells(doc, &mut children);
    }

    let chrome_v = padding.top + padding.bottom + border.top + border.bottom;
    let flow_height = cursor_y - (border_y + border.top + padding.top);
    let content_height = match used_length(style.height, containing_height, &style, styles) {
        Some(height) => match style.box_sizing {
            BoxSizing::ContentBox => height.max(0.0),
            BoxSizing::BorderBox => (height - chrome_v).max(0.0),
        },
        None if is_img => data
            .attr("height")
            .and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
            .or_else(|| decoded_image.as_ref().map(|img| img.height as f32))
            .unwrap_or(36.0)
            .max(flow_height),
        None if is_svg => {
            let (sw, sh) = svg::intrinsic_svg_size(doc, id);
            let scaled_h = if !matches!(style.width, Size::Auto) && sw > 0.0 {
                (sh * (content_width / sw)).max(1.0)
            } else {
                sh
            };
            scaled_h.max(flow_height)
        }
        None if is_canvas => data
            .attr("height")
            .and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
            .or_else(|| decoded_image.as_ref().map(|img| img.height as f32))
            .unwrap_or(150.0)
            .max(flow_height),
        None if is_checkbox || is_radio => 18.0,
        None if is_range => 20.0,
        None if is_bar_ctrl => 14.0,
        None if is_textarea => {
            let rows = data
                .attr("rows")
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(3.0)
                .clamp(1.0, 24.0);
            flow_height.max(line_height(style.font_size) * rows)
        }
        None if is_text_ctrl || is_select => flow_height.max(line_height(style.font_size)),
        None if style.aspect_ratio.is_some_and(|r| r > 0.0) && content_width > 0.0 => {
            let ratio = style.aspect_ratio.unwrap_or(1.0);
            (content_width / ratio).max(flow_height)
        }
        None => flow_height,
    };
    let mut border_height = content_height + chrome_v;
    if let Some(max_h) = used_length(style.max_height, containing_height, &style, styles) {
        let max_border_h = match style.box_sizing {
            BoxSizing::ContentBox => (max_h + chrome_v).max(0.0),
            BoxSizing::BorderBox => max_h.max(chrome_v),
        };
        border_height = border_height.min(max_border_h);
    }
    if let Some(min_h) = used_length(style.min_height, containing_height, &style, styles) {
        let min_border_h = match style.box_sizing {
            BoxSizing::ContentBox => (min_h + chrome_v).max(0.0),
            BoxSizing::BorderBox => min_h.max(chrome_v),
        };
        border_height = border_height.max(min_border_h);
    }
    let parent_rect = Rect {
        x: border_x,
        y: border_y,
        width: border_width,
        height: border_height,
    };
    if is_svg && decoded_image.is_none() {
        let svg_w = content_width.round().clamp(1.0, 1024.0) as u32;
        let svg_h = content_height.round().clamp(1.0, 1024.0) as u32;
        decoded_image = svg::render_svg_node(doc, styles, id, svg_w, svg_h);
    } else if (is_range || is_bar_ctrl) && decoded_image.is_none() {
        let ctrl_w = content_width.round().clamp(24.0, 1024.0) as u32;
        let ctrl_h = content_height.round().clamp(8.0, 128.0) as u32;
        let ratio = doc.control_fill_ratio(id).unwrap_or(0.5);
        decoded_image = render_control_bar_image(ctrl_w, ctrl_h, ratio, is_range, tag == "meter");
    }

    for oof_id in out_of_flow {
        let oof_box = layout_out_of_flow(
            doc,
            styles,
            images,
            focused_input,
            oof_id,
            parent_rect,
            containing_width,
        );
        children.push(oof_box);
    }

    let border_color = if focused_input == Some(id) {
        Color {
            r: 232,
            g: 195,
            b: 122,
            a: 255,
        }
    } else {
        style.border_color
    };
    let border_colors = if focused_input == Some(id) {
        [border_color; 4]
    } else {
        style.border_colors
    };

    LayoutBox {
        node: id,
        rect: parent_rect,
        position: style.position,
        z_index: style.z_index,
        overflow: style.overflow,
        background: style.background,
        background_gradient: style.background_gradient,
        box_shadow: style.box_shadow,
        border,
        border_color,
        border_colors,
        border_radius: style.border_radius,
        outline_width: style.outline_width,
        outline_color: style.outline_color,
        outline_offset: style.outline_offset,
        scrollable_overflow: style.scrollable_overflow,
        scroll_offset: (0.0, 0.0),
        scroll_size: (0.0, 0.0),
        object_fit: style.object_fit,
        image: decoded_image,
        sticky: None,
        transform: None,
        opacity: 1.0,
        text,
        children,
    }
}

/// Place an absolute or fixed box after its static position was computed at the origin.
///
/// Fixed uses a reference box at `(0, 0)` whose width is `viewport_width` (the
/// containing width of the parent that pulled this box out of flow, not a
/// separate viewport) and whose height is the parent's border-box height.
/// Absolute uses the parent's border box. `left` wins over `right` and `top`
/// wins over `bottom`. Missing offsets sit on the reference origin. The shift
/// moves the whole subtree, including text runs.
fn layout_out_of_flow(
    doc: &Document,
    styles: &StyleMap,
    images: &HashMap<NodeId, Image>,
    focused_input: Option<NodeId>,
    id: NodeId,
    parent_rect: Rect,
    viewport_width: f32,
) -> LayoutBox {
    let style = *styles.get(id);
    // Fixed origin is the top-left of the initial coordinates, y downward.
    // The height is the parent's border box, not the viewport height.
    let ref_rect = if style.position == Position::Fixed {
        Rect {
            x: 0.0,
            y: 0.0,
            width: viewport_width,
            height: parent_rect.height,
        }
    } else {
        parent_rect
    };
    let mut b = layout_node(
        doc,
        styles,
        images,
        focused_input,
        id,
        0.0,
        0.0,
        ref_rect.width,
        Some(ref_rect.height),
        None,
    );
    let target_x = match used_length(style.left, Some(ref_rect.width), &style, styles) {
        Some(l) => ref_rect.x + l,
        None => match used_length(style.right, Some(ref_rect.width), &style, styles) {
            Some(r) => ref_rect.x + ref_rect.width - b.rect.width - r,
            None => ref_rect.x,
        },
    };
    let target_y = match used_length(style.top, Some(ref_rect.height), &style, styles) {
        Some(t) => ref_rect.y + t,
        None => match used_length(style.bottom, Some(ref_rect.height), &style, styles) {
            Some(bt) => ref_rect.y + ref_rect.height - b.rect.height - bt,
            None => ref_rect.y,
        },
    };
    let dx = target_x - b.rect.x;
    let dy = target_y - b.rect.y;
    offset_box(&mut b, dx, dy);
    b
}

#[allow(clippy::too_many_arguments)]
fn layout_pseudo_box(
    owner_id: NodeId,
    pseudo_text: &str,
    style: &ComputedStyle,
    styles: &StyleMap,
    x: f32,
    y: f32,
    containing_width: f32,
    containing_height: Option<f32>,
) -> LayoutBox {
    let padding = style.padding;
    let border = style.border;
    let chrome_h = padding.left + padding.right + border.left + border.right;
    let chrome_v = padding.top + padding.bottom + border.top + border.bottom;
    let mut border_width =
        if let Some(w) = used_length(style.width, Some(containing_width), style, styles) {
            match style.box_sizing {
                BoxSizing::ContentBox => (w + chrome_h).max(0.0),
                BoxSizing::BorderBox => w.max(chrome_h),
            }
        } else if style.inline_level || is_decorated_inline(style) {
            (measure(pseudo_text, style.font_size) + chrome_h).min(containing_width.max(chrome_h))
        } else {
            resolve_border_width(style, containing_width, styles)
        };
    border_width = clamp_border_width(style, containing_width, styles, border_width);
    let content_width = (border_width - chrome_h).max(0.0);
    let border_x = resolve_border_x(style, x, containing_width, border_width);
    let border_y = y + style.margin.top;
    let content_x = border_x + border.left + padding.left;
    let content_y = border_y + border.top + padding.top;

    let mut text_runs = Vec::new();
    let mut flow_h = 0.0_f32;
    if !pseudo_text.is_empty() {
        let segs = vec![Segment {
            node: owner_id,
            text: apply_text_transform(pseudo_text, style.text_transform),
            font_size: style.font_size,
            line_height: style.line_height,
            color: style.color,
            underline: style.underline,
            line_through: style.line_through,
            letter_spacing: style.letter_spacing,
            word_spacing: style.word_spacing,
        }];
        let (runs, h) = place_lines(
            &segs,
            content_x,
            content_y,
            content_width.max(16.0),
            matches!(style.white_space, WhiteSpace::Pre | WhiteSpace::PreWrap),
        );
        text_runs = runs;
        flow_h = h;
    }
    let content_height = match used_length(style.height, containing_height, style, styles) {
        Some(h) => match style.box_sizing {
            BoxSizing::ContentBox => h.max(0.0),
            BoxSizing::BorderBox => (h - chrome_v).max(0.0),
        },
        None => flow_h,
    };
    let border_height = content_height + chrome_v;
    LayoutBox {
        node: owner_id,
        rect: Rect {
            x: border_x,
            y: border_y,
            width: border_width,
            height: border_height,
        },
        position: style.position,
        z_index: style.z_index,
        overflow: style.overflow,
        background: style.background,
        background_gradient: style.background_gradient,
        box_shadow: style.box_shadow,
        border,
        border_color: style.border_color,
        border_colors: style.border_colors,
        border_radius: style.border_radius,
        outline_width: style.outline_width,
        outline_color: style.outline_color,
        outline_offset: style.outline_offset,
        scrollable_overflow: style.scrollable_overflow,
        scroll_offset: (0.0, 0.0),
        scroll_size: (0.0, 0.0),
        object_fit: style.object_fit,
        image: None,
        sticky: None,
        transform: None,
        opacity: style.opacity.clamp(0.0, 1.0),
        text: text_runs,
        children: Vec::new(),
    }
}

fn stretch_table_rowspan_cells(doc: &Document, children: &mut [LayoutBox]) {
    let tr_indices: Vec<usize> = children
        .iter()
        .enumerate()
        .filter_map(|(idx, b)| (doc.get(b.node).data().tag() == Some("tr")).then_some(idx))
        .collect();
    if tr_indices.len() < 2 {
        return;
    }
    let row_heights: Vec<f32> = tr_indices
        .iter()
        .map(|&idx| children[idx].rect.height)
        .collect();
    for (r_pos, &tr_idx) in tr_indices.iter().enumerate() {
        for cell in &mut children[tr_idx].children {
            let rspan = doc
                .get(cell.node)
                .data()
                .attr("rowspan")
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(1)
                .clamp(1, 12);
            if rspan > 1 {
                let end_r = (r_pos + rspan).min(row_heights.len());
                let spanned_h: f32 = row_heights[r_pos..end_r].iter().sum();
                cell.rect.height = cell.rect.height.max(spanned_h);
            }
        }
    }
}

fn collect_table_rows(doc: &Document, styles: &StyleMap, tr_id: NodeId) -> Vec<NodeId> {
    let mut table_root = doc.parent_element(tr_id);
    if let Some(p) = table_root
        && matches!(doc.get(p).data().tag(), Some("thead" | "tbody" | "tfoot"))
        && let Some(gp) = doc.parent_element(p)
        && doc.get(gp).data().tag() == Some("table")
    {
        table_root = Some(gp);
    }
    let Some(root) = table_root else {
        return vec![tr_id];
    };
    let mut rows = Vec::new();
    for &child in doc.children(root) {
        if is_skipped(doc, styles, child) {
            continue;
        }
        match doc.get(child).data().tag() {
            Some("tr") => rows.push(child),
            Some("thead" | "tbody" | "tfoot") => {
                for &sub in doc.children(child) {
                    if !is_skipped(doc, styles, sub) && doc.get(sub).data().tag() == Some("tr") {
                        rows.push(sub);
                    }
                }
            }
            _ => {}
        }
    }
    if rows.is_empty() {
        rows.push(tr_id);
    }
    rows
}

#[allow(clippy::too_many_arguments)]
/// Lay out a `tr` with cross-row `colspan`, `rowspan`, and explicit column widths.
fn layout_table_row(
    doc: &Document,
    styles: &StyleMap,
    images: &HashMap<NodeId, Image>,
    focused_input: Option<NodeId>,
    id: NodeId,
    x: f32,
    y: f32,
    containing_width: f32,
    containing_height: Option<f32>,
) -> LayoutBox {
    let style = *styles.get(id);
    let padding = style.padding;
    let margin = style.margin;
    let border = style.border;
    let border_width = resolve_border_width(&style, containing_width, styles);
    let content_width =
        (border_width - padding.left - padding.right - border.left - border.right).max(0.0);
    let border_x = x + margin.left;
    let border_y = y + margin.top;
    let content_x = border_x + border.left + padding.left;
    let content_y = border_y + border.top + padding.top;

    let cell_colspan = |cid: NodeId| -> usize {
        doc.get(cid)
            .data()
            .attr("colspan")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 12)
    };
    let cell_rowspan = |cid: NodeId| -> usize {
        doc.get(cid)
            .data()
            .attr("rowspan")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 12)
    };

    let table_rows = collect_table_rows(doc, styles, id);
    let mut occ = Occupancy {
        cols: 1,
        cells: Vec::new(),
    };
    type TableCellPlacement = (NodeId, usize, usize);
    let mut row_placements: Vec<(NodeId, Vec<TableCellPlacement>)> =
        Vec::with_capacity(table_rows.len());
    let mut max_cols = 1usize;

    for (r_idx, &row_id) in table_rows.iter().enumerate() {
        let r_cells: Vec<NodeId> = doc
            .children(row_id)
            .iter()
            .copied()
            .filter(|cid| !is_skipped(doc, styles, *cid))
            .collect();
        let mut placed = Vec::with_capacity(r_cells.len());
        let mut col_cursor = 0usize;
        for cid in r_cells {
            let cspan = cell_colspan(cid);
            let rspan = cell_rowspan(cid);
            while !occ.fits_in_rows(r_idx, col_cursor, 1, cspan, usize::MAX) {
                col_cursor += 1;
            }
            occ.take(GridArea {
                row: r_idx,
                col: col_cursor,
                row_span: rspan,
                col_span: cspan,
            });
            placed.push((cid, col_cursor, cspan));
            col_cursor += cspan;
            max_cols = max_cols.max(col_cursor);
        }
        row_placements.push((row_id, placed));
    }
    max_cols = max_cols.max(occ.cols).max(1);

    let mut explicit_col_widths: Vec<Option<f32>> = vec![None; max_cols];
    for (_, placed) in &row_placements {
        for &(cid, col_start, cspan) in placed {
            if cspan == 1 && col_start < max_cols {
                let cstyle = styles.get(cid);
                let explicit_w = used_length(cstyle.width, Some(content_width), cstyle, styles)
                    .map(|w| w.max(24.0))
                    .or_else(|| {
                        doc.get(cid)
                            .data()
                            .attr("width")
                            .and_then(|s| s.trim().trim_end_matches("px").parse::<f32>().ok())
                            .map(|w| w.max(24.0))
                    });
                if let Some(ew) = explicit_w {
                    explicit_col_widths[col_start] =
                        Some(explicit_col_widths[col_start].map_or(ew, |prev: f32| prev.max(ew)));
                }
            }
        }
    }

    let fixed_sum: f32 = explicit_col_widths.iter().flatten().sum();
    let auto_cols = explicit_col_widths.iter().filter(|w| w.is_none()).count();
    let auto_col_w = if auto_cols > 0 {
        ((content_width - fixed_sum).max(0.0) / (auto_cols as f32)).max(24.0)
    } else {
        (content_width / (max_cols as f32)).max(24.0)
    };
    let col_widths: Vec<f32> = explicit_col_widths
        .into_iter()
        .map(|w| w.unwrap_or(auto_col_w))
        .collect();
    let mut col_offsets = Vec::with_capacity(max_cols + 1);
    let mut acc_x = 0.0_f32;
    for &cw in &col_widths {
        col_offsets.push(acc_x);
        acc_x += cw;
    }
    col_offsets.push(acc_x);

    let active_placed = row_placements
        .into_iter()
        .find(|(rid, _)| *rid == id)
        .map(|(_, p)| p)
        .unwrap_or_default();

    let mut children = Vec::with_capacity(active_placed.len());
    let mut row_height = 0.0_f32;

    for (cid, col_start, cspan) in active_placed {
        let col_end = (col_start + cspan).min(col_widths.len());
        let cell_x = content_x + col_offsets.get(col_start).copied().unwrap_or(0.0);
        let span_width = (col_offsets.get(col_end).copied().unwrap_or(acc_x)
            - col_offsets.get(col_start).copied().unwrap_or(0.0))
        .max(24.0);
        let mut cell_box = layout_node(
            doc,
            styles,
            images,
            focused_input,
            cid,
            cell_x,
            content_y,
            span_width,
            containing_height,
            None,
        );
        cell_box.rect.width = span_width;
        if cell_rowspan(cid) == 1 {
            row_height = row_height.max(cell_box.rect.height);
        } else {
            row_height = row_height.max(line_height(styles.get(cid).font_size));
        }
        children.push(cell_box);
    }

    for cell in &mut children {
        if cell_rowspan(cell.node) == 1 {
            cell.rect.height = row_height;
        } else {
            cell.rect.height = cell.rect.height.max(row_height);
        }
    }

    let content_height = match used_length(style.height, containing_height, &style, styles) {
        Some(h) => h.max(row_height),
        None => row_height,
    };
    let border_height = content_height + padding.top + padding.bottom + border.top + border.bottom;

    LayoutBox {
        node: id,
        rect: Rect {
            x: border_x,
            y: border_y,
            width: border_width,
            height: border_height,
        },
        position: style.position,
        z_index: style.z_index,
        overflow: style.overflow,
        background: style.background,
        background_gradient: style.background_gradient,
        box_shadow: style.box_shadow,
        border,
        border_color: style.border_color,
        border_colors: style.border_colors,
        border_radius: style.border_radius,
        outline_width: style.outline_width,
        outline_color: style.outline_color,
        outline_offset: style.outline_offset,
        scrollable_overflow: style.scrollable_overflow,
        scroll_offset: (0.0, 0.0),
        scroll_size: (0.0, 0.0),
        object_fit: style.object_fit,
        image: None,
        sticky: None,
        transform: None,
        opacity: 1.0,
        text: Vec::new(),
        children,
    }
}

/// One grid item's area in track indices. `col` and `row` are zero-based
/// starts. Spans are at least 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GridArea {
    row: usize,
    col: usize,
    row_span: usize,
    col_span: usize,
}

/// How one track is sized once the axis is known.
#[derive(Clone, Copy, Debug)]
enum TrackPlan {
    /// A definite size.
    Fixed(f32),
    /// Sized to content, then clamped to `[min, max]`.
    Content { min: f32, max: f32 },
    /// Starts at `min` and grows into free space up to `max`.
    Grow { min: f32, max: f32 },
    /// A flexible share with a floor.
    Flex { fr: f32, min: f32 },
}

/// Turn a track into a plan. `basis` is the container content size on this
/// axis, or `None` when it is not definite. `auto_is_flex` is true for
/// columns, where `auto` behaves like `1fr`.
fn plan_track(
    track: GridTrack,
    basis: Option<f32>,
    auto_is_flex: bool,
    min_content_px: f32,
    max_content_px: f32,
) -> TrackPlan {
    let min_c = min_content_px.max(0.0);
    let max_c = max_content_px.max(min_c);
    let breadth = |b: TrackBreadth| -> Option<f32> {
        match b {
            TrackBreadth::Px(px) => Some(px.max(0.0)),
            TrackBreadth::Percent(pct) => basis.map(|size| (size * pct / 100.0).max(0.0)),
            TrackBreadth::MinContent => Some(min_c),
            TrackBreadth::MaxContent => Some(max_c),
            TrackBreadth::FitContent(limit) => Some(max_c.min(limit.max(min_c))),
            TrackBreadth::Fr(_) | TrackBreadth::Auto => None,
        }
    };
    let auto = if auto_is_flex {
        TrackPlan::Flex { fr: 1.0, min: 0.0 }
    } else {
        TrackPlan::Content {
            min: 0.0,
            max: f32::INFINITY,
        }
    };
    match track {
        GridTrack::Px(px) => TrackPlan::Fixed(px.max(0.0)),
        GridTrack::Percent(pct) => match basis {
            Some(size) => TrackPlan::Fixed((size * pct / 100.0).max(0.0)),
            None => auto,
        },
        GridTrack::Fr(fr) => TrackPlan::Flex {
            fr: fr.max(0.0),
            min: 0.0,
        },
        GridTrack::Auto => auto,
        GridTrack::MinContent => {
            if auto_is_flex {
                TrackPlan::Content {
                    min: min_c,
                    max: min_c,
                }
            } else {
                TrackPlan::Content {
                    min: 0.0,
                    max: f32::INFINITY,
                }
            }
        }
        GridTrack::MaxContent => {
            if auto_is_flex {
                TrackPlan::Content {
                    min: max_c,
                    max: max_c,
                }
            } else {
                TrackPlan::Content {
                    min: 0.0,
                    max: f32::INFINITY,
                }
            }
        }
        GridTrack::FitContent(limit) => {
            if auto_is_flex {
                let cap = max_c.min(limit.max(min_c));
                TrackPlan::Grow {
                    min: min_c,
                    max: cap.max(min_c),
                }
            } else {
                TrackPlan::Content {
                    min: 0.0,
                    max: limit.max(0.0),
                }
            }
        }
        GridTrack::MinMax(min, max) => {
            let min_px = breadth(min);
            match max {
                TrackBreadth::Fr(fr) => TrackPlan::Flex {
                    fr: fr.max(0.0),
                    min: min_px.unwrap_or(0.0),
                },
                TrackBreadth::Auto if auto_is_flex => TrackPlan::Flex {
                    fr: 1.0,
                    min: min_px.unwrap_or(0.0),
                },
                TrackBreadth::Auto => TrackPlan::Content {
                    min: min_px.unwrap_or(0.0),
                    max: f32::INFINITY,
                },
                TrackBreadth::Px(_)
                | TrackBreadth::Percent(_)
                | TrackBreadth::MinContent
                | TrackBreadth::MaxContent
                | TrackBreadth::FitContent(_) => {
                    let max_px = breadth(max);
                    match (min_px, max_px) {
                        (Some(lo), Some(hi)) => TrackPlan::Grow {
                            min: lo,
                            max: hi.max(lo),
                        },
                        (None, Some(hi)) => TrackPlan::Content { min: 0.0, max: hi },
                        (Some(lo), None) => TrackPlan::Content {
                            min: lo,
                            max: f32::INFINITY,
                        },
                        (None, None) => auto,
                    }
                }
            }
        }
    }
}

/// Size the tracks on one axis.
///
/// `content[i]` is the largest outer size of an item that sits only in
/// track `i` (0 when none). `spanning` lists `(start, span, size)` for items
/// that cross more than one track. `available` is the container content size,
/// or `None` when it is not definite. Steps: fixed and content sizes, then
/// spanning items grow the content tracks they cross, then growable tracks
/// take free space up to their maximum, then flexible tracks share what is
/// left. With no definite size, a flexible track is sized so the largest
/// item per `fr` fits.
fn size_tracks(
    plans: &[TrackPlan],
    content: &[f32],
    spanning: &[(usize, usize, f32)],
    available: Option<f32>,
    gap: f32,
) -> Vec<f32> {
    let count = plans.len();
    let mut sizes: Vec<f32> = plans
        .iter()
        .enumerate()
        .map(|(i, plan)| match *plan {
            TrackPlan::Fixed(px) => px,
            TrackPlan::Content { min, max } => content
                .get(i)
                .copied()
                .unwrap_or(0.0)
                .max(min)
                .min(max.max(min)),
            TrackPlan::Grow { min, .. } => min,
            TrackPlan::Flex { min, .. } => min,
        })
        .collect();

    for &(start, span, size) in spanning {
        let end = (start + span).min(count);
        if start >= end {
            continue;
        }
        if plans[start..end]
            .iter()
            .any(|plan| matches!(plan, TrackPlan::Flex { .. }))
        {
            continue;
        }
        let current: f32 = sizes[start..end].iter().sum::<f32>() + gap * (end - start - 1) as f32;
        let extra = size - current;
        if extra <= 0.0 {
            continue;
        }
        let growable: Vec<usize> = (start..end)
            .filter(|&i| matches!(plans[i], TrackPlan::Content { .. }))
            .collect();
        if growable.is_empty() {
            continue;
        }
        let share = extra / growable.len() as f32;
        for i in growable {
            if let TrackPlan::Content { max, .. } = plans[i] {
                sizes[i] = (sizes[i] + share).min(max.max(sizes[i]));
            }
        }
    }

    let gaps = gap * count.saturating_sub(1) as f32;
    let used = |sizes: &[f32]| -> f32 {
        sizes
            .iter()
            .zip(plans)
            .filter(|(_, plan)| !matches!(plan, TrackPlan::Flex { .. }))
            .map(|(size, _)| *size)
            .sum::<f32>()
    };

    // Growable tracks take free space in equal steps, each up to its maximum.
    let mut free = match available {
        Some(size) => size - gaps - used(&sizes) - flex_floor_sum(plans),
        None => f32::INFINITY,
    };
    let mut open: Vec<usize> = (0..count)
        .filter(|&i| matches!(plans[i], TrackPlan::Grow { .. }))
        .collect();
    while free > 0.0 && !open.is_empty() {
        let share = if free.is_finite() {
            free / open.len() as f32
        } else {
            f32::INFINITY
        };
        let mut next_open = Vec::new();
        let mut spent = 0.0;
        for &i in &open {
            if let TrackPlan::Grow { max, .. } = plans[i] {
                let room = max - sizes[i];
                if room <= share {
                    sizes[i] = max;
                    spent += room;
                } else {
                    sizes[i] += share;
                    spent += share;
                    next_open.push(i);
                }
            }
        }
        if !free.is_finite() || next_open.len() == open.len() {
            break;
        }
        free -= spent;
        open = next_open;
    }

    let flex: Vec<usize> = (0..count)
        .filter(|&i| matches!(plans[i], TrackPlan::Flex { .. }))
        .collect();
    if flex.is_empty() {
        return sizes;
    }
    let fr_of = |i: usize| match plans[i] {
        TrackPlan::Flex { fr, .. } => fr,
        _ => 0.0,
    };
    let floor_of = |i: usize| match plans[i] {
        TrackPlan::Flex { min, .. } => min,
        _ => 0.0,
    };
    match available {
        Some(size) => {
            // Flexible tracks share the leftover space. A track whose share is
            // below its floor keeps the floor and leaves the shared pool.
            let leftover = (size - gaps - used(&sizes)).max(0.0);
            let mut active = flex.clone();
            let mut pool = leftover;
            loop {
                let fr_sum: f32 = active.iter().map(|&i| fr_of(i)).sum();
                let unit = if fr_sum > 0.0 { pool / fr_sum } else { 0.0 };
                let frozen: Vec<usize> = active
                    .iter()
                    .copied()
                    .filter(|&i| fr_of(i) * unit < floor_of(i))
                    .collect();
                if frozen.is_empty() {
                    for &i in &active {
                        sizes[i] = fr_of(i) * unit;
                    }
                    break;
                }
                for &i in &frozen {
                    sizes[i] = floor_of(i);
                    pool = (pool - floor_of(i)).max(0.0);
                }
                active.retain(|i| !frozen.contains(i));
                if active.is_empty() {
                    break;
                }
            }
        }
        None => {
            let mut unit = 0.0_f32;
            for &i in &flex {
                let fr = fr_of(i);
                let need = content.get(i).copied().unwrap_or(0.0);
                // A factor below 1 divides by 1, so a small `fr` does not blow up.
                if fr > 0.0 {
                    unit = unit.max(need / fr.max(1.0));
                }
            }
            for &i in &flex {
                sizes[i] = (fr_of(i) * unit).max(floor_of(i));
            }
        }
    }
    sizes
}

/// Sum of the floors of flexible tracks. Growable tracks leave this room.
fn flex_floor_sum(plans: &[TrackPlan]) -> f32 {
    plans
        .iter()
        .map(|plan| match plan {
            TrackPlan::Flex { min, .. } => *min,
            _ => 0.0,
        })
        .sum()
}

/// Start offsets of each track, from `origin`, with `gap` between tracks.
fn track_offsets(sizes: &[f32], origin: f32, gap: f32) -> Vec<f32> {
    let mut out = Vec::with_capacity(sizes.len() + 1);
    let mut run = origin;
    for size in sizes {
        out.push(run);
        run += size + gap;
    }
    out.push(run - if sizes.is_empty() { 0.0 } else { gap });
    out
}

/// Size of an area that spans `span` tracks from `start`, including the gaps inside it.
fn span_size(sizes: &[f32], start: usize, span: usize, gap: f32) -> f32 {
    let end = (start + span).min(sizes.len());
    if start >= end {
        return 0.0;
    }
    sizes[start..end].iter().sum::<f32>() + gap * (end - start - 1) as f32
}

/// One axis of an item's placement after line numbers are resolved:
/// a definite zero-based start, or auto, plus a span of at least 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AxisSpan {
    start: Option<usize>,
    span: usize,
}

/// Resolve `placement` against `explicit` tracks on this axis.
///
/// Positive lines count from 1. Negative lines count back from the last
/// explicit line. A line before the first line is clamped to the first.
/// `a / b` with `b` before `a` is swapped; equal lines span one track.
fn resolve_axis(placement: GridPlacement, explicit: usize) -> AxisSpan {
    let max = MAX_GRID_LINE as usize;
    let line_index = |n: i16| -> usize {
        let last_line = explicit as i64 + 1;
        let line = if n > 0 {
            i64::from(n)
        } else {
            last_line + 1 + i64::from(n)
        };
        (line.max(1) - 1) as usize
    };
    let span_of = |n: i16| (n.max(1) as usize).min(max);
    match (placement.start, placement.end) {
        (GridLine::Line(s), GridLine::Line(e)) => {
            let (a, b) = (line_index(s), line_index(e));
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            AxisSpan {
                start: Some(lo),
                span: (hi - lo).max(1),
            }
        }
        (GridLine::Line(s), GridLine::Span(n)) => AxisSpan {
            start: Some(line_index(s)),
            span: span_of(n),
        },
        (GridLine::Line(s), GridLine::Auto) => AxisSpan {
            start: Some(line_index(s)),
            span: 1,
        },
        (GridLine::Span(n), GridLine::Line(e)) => {
            let end = line_index(e);
            let span = span_of(n);
            AxisSpan {
                start: Some(end.saturating_sub(span)),
                span: span.min(end.max(1)),
            }
        }
        (GridLine::Auto, GridLine::Line(e)) => AxisSpan {
            start: Some(line_index(e).saturating_sub(1)),
            span: 1,
        },
        (GridLine::Span(n), _) | (GridLine::Auto, GridLine::Span(n)) => AxisSpan {
            start: None,
            span: span_of(n),
        },
        (GridLine::Auto, GridLine::Auto) => AxisSpan {
            start: None,
            span: 1,
        },
    }
}

/// Cells already taken by placed items. Rows grow on demand.
struct Occupancy {
    cols: usize,
    cells: Vec<Vec<bool>>,
}

impl Occupancy {
    fn fits(&self, row: usize, col: usize, row_span: usize, col_span: usize) -> bool {
        if col + col_span > self.cols {
            return false;
        }
        (row..row + row_span).all(|r| {
            self.cells
                .get(r)
                .is_none_or(|cells| cells[col..col + col_span].iter().all(|taken| !taken))
        })
    }

    fn fits_in_rows(
        &self,
        row: usize,
        col: usize,
        row_span: usize,
        col_span: usize,
        max_rows: usize,
    ) -> bool {
        if row + row_span > max_rows {
            return false;
        }
        (row..row + row_span).all(|r| {
            self.cells.get(r).is_none_or(|cells| {
                let end = (col + col_span).min(cells.len());
                if col >= end {
                    true
                } else {
                    cells[col..end].iter().all(|taken| !taken)
                }
            })
        })
    }

    fn take(&mut self, area: GridArea) {
        if area.col + area.col_span > self.cols {
            let cols = area.col + area.col_span;
            for row in &mut self.cells {
                row.resize(cols, false);
            }
            self.cols = cols;
        }
        while self.cells.len() < area.row + area.row_span {
            self.cells.push(vec![false; self.cols]);
        }
        for row in &mut self.cells[area.row..area.row + area.row_span] {
            for cell in &mut row[area.col..area.col + area.col_span] {
                *cell = true;
            }
        }
    }
}

/// Place grid items according to `grid-auto-flow` (`row`, `column`, `row dense`, `column dense`).
fn place_grid_items(
    items: &[(AxisSpan, AxisSpan)],
    explicit_cols: usize,
    explicit_rows: usize,
    flow: GridAutoFlow,
) -> Vec<GridArea> {
    let mut cols = explicit_cols.max(1);
    let mut rows = explicit_rows.max(1);
    for (row, col) in items {
        cols = cols.max(col.start.unwrap_or(0) + col.span);
        rows = rows.max(row.start.unwrap_or(0) + row.span);
    }
    let mut grid = Occupancy {
        cols,
        cells: Vec::new(),
    };
    let mut areas: Vec<Option<GridArea>> = vec![None; items.len()];

    for (i, (row, col)) in items.iter().enumerate() {
        if let (Some(r), Some(c)) = (row.start, col.start) {
            let area = GridArea {
                row: r,
                col: c,
                row_span: row.span,
                col_span: col.span,
            };
            grid.take(area);
            areas[i] = Some(area);
        }
    }

    let dense = matches!(flow, GridAutoFlow::RowDense | GridAutoFlow::ColumnDense);
    let col_major = matches!(flow, GridAutoFlow::Column | GridAutoFlow::ColumnDense);

    if col_major {
        let mut col_cursor: HashMap<usize, usize> = HashMap::new();
        for (i, (row, col)) in items.iter().enumerate() {
            let (None, Some(c)) = (row.start, col.start) else {
                continue;
            };
            let mut r = if dense {
                0
            } else {
                col_cursor.get(&c).copied().unwrap_or(0)
            };
            while !grid.fits_in_rows(r, c, row.span, col.span, rows) && r + row.span <= rows {
                r += 1;
            }
            let area = GridArea {
                row: r,
                col: c,
                row_span: row.span,
                col_span: col.span,
            };
            grid.take(area);
            col_cursor.insert(c, r + row.span);
            areas[i] = Some(area);
        }

        let (mut cur_row, mut cur_col) = (0usize, 0usize);
        for (i, (row, col)) in items.iter().enumerate() {
            if col.start.is_some() {
                continue;
            }
            let area = if let Some(r) = row.start {
                if dense {
                    let mut c = 0usize;
                    while !grid.fits_in_rows(r, c, row.span, col.span, rows) {
                        c += 1;
                    }
                    GridArea {
                        row: r,
                        col: c,
                        row_span: row.span,
                        col_span: col.span,
                    }
                } else {
                    if r < cur_row {
                        cur_col += 1;
                    }
                    cur_row = r;
                    while !grid.fits_in_rows(r, cur_col, row.span, col.span, rows) {
                        cur_col += 1;
                    }
                    GridArea {
                        row: r,
                        col: cur_col,
                        row_span: row.span,
                        col_span: col.span,
                    }
                }
            } else {
                let (mut r, mut c) = if dense {
                    (0usize, 0usize)
                } else {
                    (cur_row, cur_col)
                };
                loop {
                    if r + row.span > rows {
                        c += 1;
                        r = 0;
                        continue;
                    }
                    if grid.fits_in_rows(r, c, row.span, col.span, rows) {
                        break;
                    }
                    r += 1;
                }
                if !dense {
                    cur_row = r;
                    cur_col = c;
                }
                GridArea {
                    row: r,
                    col: c,
                    row_span: row.span,
                    col_span: col.span,
                }
            };
            grid.take(area);
            areas[i] = Some(area);
        }
    } else {
        let mut row_cursor: HashMap<usize, usize> = HashMap::new();
        for (i, (row, col)) in items.iter().enumerate() {
            let (Some(r), None) = (row.start, col.start) else {
                continue;
            };
            let mut c = if dense {
                0
            } else {
                row_cursor.get(&r).copied().unwrap_or(0)
            };
            while !grid.fits(r, c, row.span, col.span) && c + col.span <= grid.cols {
                c += 1;
            }
            let area = GridArea {
                row: r,
                col: c,
                row_span: row.span,
                col_span: col.span,
            };
            grid.take(area);
            row_cursor.insert(r, c + col.span);
            areas[i] = Some(area);
        }

        let (mut cur_row, mut cur_col) = (0usize, 0usize);
        for (i, (row, col)) in items.iter().enumerate() {
            if row.start.is_some() {
                continue;
            }
            let area = if let Some(c) = col.start {
                if dense {
                    let mut r = 0usize;
                    while !grid.fits(r, c, row.span, col.span) {
                        r += 1;
                    }
                    GridArea {
                        row: r,
                        col: c,
                        row_span: row.span,
                        col_span: col.span,
                    }
                } else {
                    if c < cur_col {
                        cur_row += 1;
                    }
                    cur_col = c;
                    while !grid.fits(cur_row, c, row.span, col.span) {
                        cur_row += 1;
                    }
                    GridArea {
                        row: cur_row,
                        col: c,
                        row_span: row.span,
                        col_span: col.span,
                    }
                }
            } else {
                let (mut r, mut c) = if dense {
                    (0usize, 0usize)
                } else {
                    (cur_row, cur_col)
                };
                loop {
                    if c + col.span > grid.cols {
                        r += 1;
                        c = 0;
                        continue;
                    }
                    if grid.fits(r, c, row.span, col.span) {
                        break;
                    }
                    c += 1;
                }
                if !dense {
                    cur_row = r;
                    cur_col = c;
                }
                GridArea {
                    row: r,
                    col: c,
                    row_span: row.span,
                    col_span: col.span,
                }
            };
            grid.take(area);
            areas[i] = Some(area);
        }
    }
    areas
        .into_iter()
        .map(|area| {
            area.unwrap_or(GridArea {
                row: 0,
                col: 0,
                row_span: 1,
                col_span: 1,
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
/// Grid layout.
///
/// Columns come from `grid-template-columns`; with none set there is one
/// `1fr` column. Rows come from `grid-template-rows`, then `grid-auto-rows`
/// for every row past the explicit list. Items are laid out at their column
/// width to measure their height, then rows are sized and items are moved
/// into their rows. `column-gap` goes between columns and `row-gap` between
/// rows.
/// `align-items: stretch` grows an `auto`-height item to its area height;
/// `center` and `end` move it inside the area.
fn layout_grid(
    doc: &Document,
    styles: &StyleMap,
    images: &HashMap<NodeId, Image>,
    focused_input: Option<NodeId>,
    id: NodeId,
    x: f32,
    y: f32,
    containing_width: f32,
    containing_height: Option<f32>,
) -> LayoutBox {
    let style = *styles.get(id);
    let padding = style.padding;
    let margin = style.margin;
    let border = style.border;
    let border_width = resolve_border_width(&style, containing_width, styles);
    let content_width =
        (border_width - padding.left - padding.right - border.left - border.right).max(0.0);
    let border_x = resolve_border_x(&style, x, containing_width, border_width);
    let border_y = y + margin.top;
    let content_x = border_x + border.left + padding.left;
    let content_y = border_y + border.top + padding.top;
    let specified_height = used_length(style.height, containing_height, &style, styles);

    let mut in_flow = Vec::new();
    let mut out_of_flow = Vec::new();
    for &cid in doc.composed_children(id).iter() {
        if is_skipped(doc, styles, cid) {
            continue;
        }
        if let NodeData::Text(t) = doc.get(cid).data()
            && t.trim().is_empty()
        {
            continue;
        }
        let cstyle = styles.get(cid);
        if matches!(cstyle.position, Position::Absolute | Position::Fixed) {
            out_of_flow.push(cid);
        } else {
            in_flow.push(cid);
        }
    }
    in_flow.sort_by_key(|&cid| {
        if matches!(doc.get(cid).data(), NodeData::Text(_)) {
            0
        } else {
            styles.get(cid).order
        }
    });

    let (row_gap, column_gap) = used_gaps(&style, content_width, specified_height, styles);
    let raw_cols = style.grid_tracks();
    let explicit_cols: Vec<GridTrack> =
        if style.grid_auto_repeat != GridAutoRepeat::None && !raw_cols.is_empty() {
            let pattern_min: f32 = raw_cols
                .iter()
                .map(|t| match t {
                    GridTrack::Px(px) => (*px).max(1.0),
                    GridTrack::Percent(pct) => (content_width * pct / 100.0).max(1.0),
                    GridTrack::MinMax(min_b, _) => match min_b {
                        TrackBreadth::Px(px) => (*px).max(1.0),
                        TrackBreadth::Percent(pct) => (content_width * pct / 100.0).max(1.0),
                        _ => 100.0,
                    },
                    _ => 100.0,
                })
                .sum();
            let pattern_gaps = column_gap * (raw_cols.len().saturating_sub(1) as f32);
            let unit_w = (pattern_min + pattern_gaps).max(1.0);
            let mut reps = ((content_width + column_gap) / (unit_w + column_gap))
                .floor()
                .clamp(1.0, 12.0) as usize;
            if style.grid_auto_repeat == GridAutoRepeat::AutoFit {
                let needed_reps = (in_flow.len().max(1)).div_ceil(raw_cols.len());
                reps = reps.min(needed_reps.max(1));
            }
            let mut expanded = Vec::with_capacity(raw_cols.len() * reps);
            for _ in 0..reps {
                expanded.extend_from_slice(raw_cols);
            }
            expanded
        } else {
            raw_cols.to_vec()
        };
    let explicit_rows = style.grid_row_tracks();
    let area_cols = style.grid_template_areas.cols as usize;
    let area_rows = style.grid_template_areas.rows as usize;
    let explicit_col_count = explicit_cols.len().max(area_cols).max(1);
    let explicit_row_count = explicit_rows.len().max(area_rows);
    let spans: Vec<(AxisSpan, AxisSpan)> = in_flow
        .iter()
        .map(|&cid| {
            // Text runs are anonymous items. They copy the parent's style, so
            // they must not pick up the container's own placement.
            if matches!(doc.get(cid).data(), NodeData::Text(_)) {
                let auto = AxisSpan {
                    start: None,
                    span: 1,
                };
                return (auto, auto);
            }
            let cstyle = styles.get(cid);
            if cstyle.grid_area_name != 0
                && let Some((rs, cs, re, ce)) =
                    style.grid_template_areas.find_area(cstyle.grid_area_name)
            {
                return (
                    AxisSpan {
                        start: Some((rs - 1) as usize),
                        span: (re - rs).max(1) as usize,
                    },
                    AxisSpan {
                        start: Some((cs - 1) as usize),
                        span: (ce - cs).max(1) as usize,
                    },
                );
            }
            (
                resolve_axis(cstyle.grid_row, explicit_row_count),
                resolve_axis(cstyle.grid_column, explicit_cols.len().max(area_cols)),
            )
        })
        .collect();
    let areas = place_grid_items(
        &spans,
        explicit_col_count,
        explicit_row_count.max(1),
        style.grid_auto_flow,
    );
    let col_count = areas
        .iter()
        .map(|a| a.col + a.col_span)
        .max()
        .unwrap_or(0)
        .max(explicit_col_count);
    let row_count = areas
        .iter()
        .map(|a| a.row + a.row_span)
        .max()
        .unwrap_or(0)
        .max(explicit_row_count);

    let auto_cols = style.grid_auto_column_tracks();
    let col_track_at = |i: usize| -> GridTrack {
        if let Some(&t) = explicit_cols.get(i) {
            t
        } else if !auto_cols.is_empty() {
            auto_cols[(i - explicit_cols.len()) % auto_cols.len()]
        } else {
            GridTrack::Auto
        }
    };
    let breadth_is_intrinsic = |b: TrackBreadth| {
        matches!(
            b,
            TrackBreadth::MinContent | TrackBreadth::MaxContent | TrackBreadth::FitContent(_)
        )
    };
    let track_is_intrinsic = |t: GridTrack| match t {
        GridTrack::MinContent | GridTrack::MaxContent | GridTrack::FitContent(_) => true,
        GridTrack::MinMax(a, b) => breadth_is_intrinsic(a) || breadth_is_intrinsic(b),
        _ => false,
    };
    let mut col_min_content = vec![0.0_f32; col_count];
    let mut col_max_content = vec![0.0_f32; col_count];
    for (&cid, area) in in_flow.iter().zip(&areas) {
        if area.col_span == 1 && area.col < col_count && track_is_intrinsic(col_track_at(area.col))
        {
            let cstyle = styles.get(cid);
            let h_margin = cstyle.margin.left + cstyle.margin.right;
            let min_w = min_content_box_width(doc, styles, images, cid, content_width) + h_margin;
            let max_w = max_content_box_width(doc, styles, images, cid, content_width) + h_margin;
            col_min_content[area.col] = col_min_content[area.col].max(min_w);
            col_max_content[area.col] = col_max_content[area.col].max(max_w);
        }
    }

    let col_plans: Vec<TrackPlan> = (0..col_count)
        .map(|i| {
            let track = col_track_at(i);
            plan_track(
                track,
                Some(content_width),
                true,
                col_min_content[i],
                col_max_content[i],
            )
        })
        .collect();
    let col_widths = size_tracks(
        &col_plans,
        &col_max_content,
        &[],
        Some(content_width),
        column_gap,
    );
    let base_col_offsets = track_offsets(&col_widths, content_x, column_gap);
    let used_cols_w = if col_count == 0 {
        0.0
    } else {
        (base_col_offsets[col_count] - content_x).max(0.0)
    };
    let free_cols_w = (content_width - used_cols_w).max(0.0);
    let (col_start_dx, col_step_dx) = if free_cols_w > 0.0 && col_count > 0 {
        match style.justify_content {
            JustifyContent::Start => (0.0, 0.0),
            JustifyContent::Center => (free_cols_w * 0.5, 0.0),
            JustifyContent::End => (free_cols_w, 0.0),
            JustifyContent::SpaceBetween if col_count > 1 => {
                (0.0, free_cols_w / ((col_count - 1) as f32))
            }
            JustifyContent::SpaceBetween => (0.0, 0.0),
            JustifyContent::SpaceAround => {
                let u = free_cols_w / (col_count as f32);
                (u * 0.5, u)
            }
            JustifyContent::SpaceEvenly => {
                let u = free_cols_w / ((col_count + 1) as f32);
                (u, u)
            }
        }
    } else {
        (0.0, 0.0)
    };
    let effective_col_gap = column_gap + col_step_dx;
    let col_offsets = track_offsets(&col_widths, content_x + col_start_dx, effective_col_gap);

    // Lay each item out at its area width (or shrink-to-fit width when aligned) to learn its height.
    let mut boxes = Vec::with_capacity(in_flow.len());
    let mut item_cb_widths = Vec::with_capacity(in_flow.len());
    for (&cid, area) in in_flow.iter().zip(&areas) {
        let cell_w = span_size(&col_widths, area.col, area.col_span, effective_col_gap);
        let is_text = matches!(doc.get(cid).data(), NodeData::Text(_));
        let cstyle = styles.get(cid);
        let item_justify = cstyle.justify_self.unwrap_or(style.justify_items);
        let has_h_auto = cstyle.margin_left_auto || cstyle.margin_right_auto;
        let item_cb_w = if !is_text
            && matches!(cstyle.width, Size::Auto)
            && (item_justify != AlignItems::Stretch || has_h_auto)
        {
            (intrinsic_box_width(doc, styles, images, cid, cell_w)
                + cstyle.margin.left
                + cstyle.margin.right)
                .min(cell_w)
        } else {
            cell_w
        };
        let child_box = layout_node(
            doc,
            styles,
            images,
            focused_input,
            cid,
            col_offsets[area.col],
            content_y,
            item_cb_w,
            specified_height,
            None,
        );
        boxes.push(child_box);
        item_cb_widths.push(item_cb_w);
    }

    let auto_rows = style.grid_auto_row_tracks();
    let row_track_at = |i: usize| -> GridTrack {
        if let Some(&t) = explicit_rows.get(i) {
            t
        } else if !auto_rows.is_empty() {
            auto_rows[(i - explicit_rows.len()) % auto_rows.len()]
        } else {
            style.grid_auto_rows
        }
    };
    let mut row_content = vec![0.0_f32; row_count];
    let mut row_spanning = Vec::new();
    for (child_box, area) in boxes.iter().zip(&areas) {
        let cstyle = styles.get(child_box.node);
        let outer_h = cstyle.margin.top + child_box.rect.height + cstyle.margin.bottom;
        if area.row_span == 1 {
            row_content[area.row] = row_content[area.row].max(outer_h);
        } else {
            row_spanning.push((area.row, area.row_span, outer_h));
        }
    }
    let row_plans: Vec<TrackPlan> = (0..row_count)
        .map(|i| {
            let track = row_track_at(i);
            plan_track(
                track,
                specified_height,
                false,
                row_content[i],
                row_content[i],
            )
        })
        .collect();
    row_spanning.sort_by_key(|(_, span, _)| *span);
    let row_heights = size_tracks(
        &row_plans,
        &row_content,
        &row_spanning,
        specified_height,
        row_gap,
    );
    let base_row_offsets = track_offsets(&row_heights, content_y, row_gap);
    let base_flow_h = if row_count == 0 {
        0.0
    } else {
        (base_row_offsets[row_count] - content_y).max(0.0)
    };
    let free_rows_h = specified_height
        .map(|sh| (sh - base_flow_h).max(0.0))
        .unwrap_or(0.0);
    let (row_start_dy, row_step_dy) = if free_rows_h > 0.0 && row_count > 0 {
        match style.align_content {
            AlignContent::Stretch | AlignContent::Start => (0.0, 0.0),
            AlignContent::Center => (free_rows_h * 0.5, 0.0),
            AlignContent::End => (free_rows_h, 0.0),
            AlignContent::SpaceBetween if row_count > 1 => {
                (0.0, free_rows_h / ((row_count - 1) as f32))
            }
            AlignContent::SpaceBetween => (0.0, 0.0),
            AlignContent::SpaceAround => {
                let u = free_rows_h / (row_count as f32);
                (u * 0.5, u)
            }
            AlignContent::SpaceEvenly => {
                let u = free_rows_h / ((row_count + 1) as f32);
                (u, u)
            }
        }
    } else {
        (0.0, 0.0)
    };
    let effective_row_gap = row_gap + row_step_dy;
    let row_offsets = track_offsets(&row_heights, content_y + row_start_dy, effective_row_gap);

    let mut children = Vec::with_capacity(in_flow.len() + out_of_flow.len());
    for ((mut item_box, area), item_cb_w) in boxes.into_iter().zip(&areas).zip(item_cb_widths) {
        let cstyle = styles.get(item_box.node);
        let cell_w = span_size(&col_widths, area.col, area.col_span, effective_col_gap);
        let area_h = span_size(&row_heights, area.row, area.row_span, effective_row_gap);
        let outer_w = cstyle.margin.left + item_box.rect.width + cstyle.margin.right;
        let outer_h = cstyle.margin.top + item_box.rect.height + cstyle.margin.bottom;
        let free_x = (cell_w - outer_w).max(0.0);
        let free_y = (area_h - outer_h).max(0.0);
        let item_justify = cstyle.justify_self.unwrap_or(style.justify_items);
        let item_align = cstyle.align_self.unwrap_or(style.align_items);

        let already_dx =
            resolve_border_x(cstyle, 0.0, item_cb_w, item_box.rect.width) - cstyle.margin.left;
        let dx_in_area = if cstyle.margin_left_auto && cstyle.margin_right_auto {
            free_x * 0.5
        } else if cstyle.margin_left_auto {
            free_x
        } else if cstyle.margin_right_auto {
            0.0
        } else {
            match item_justify {
                AlignItems::Center => free_x * 0.5,
                AlignItems::End => free_x,
                AlignItems::Start | AlignItems::Stretch | AlignItems::Baseline => 0.0,
            }
        };
        let dy_in_area = if cstyle.margin_top_auto && cstyle.margin_bottom_auto {
            free_y * 0.5
        } else if cstyle.margin_top_auto {
            free_y
        } else if cstyle.margin_bottom_auto {
            0.0
        } else {
            match item_align {
                AlignItems::Start | AlignItems::Stretch | AlignItems::Baseline => 0.0,
                AlignItems::Center => free_y * 0.5,
                AlignItems::End => free_y,
            }
        };
        let dx = dx_in_area - already_dx;
        let dy = row_offsets[area.row] + dy_in_area - content_y;
        offset_box(&mut item_box, dx, dy);
        let has_v_auto = cstyle.margin_top_auto || cstyle.margin_bottom_auto;
        if !has_v_auto && item_align == AlignItems::Stretch && matches!(cstyle.height, Size::Auto) {
            item_box.rect.height =
                (area_h - cstyle.margin.top - cstyle.margin.bottom).max(item_box.rect.height);
        }
        children.push(item_box);
    }

    let flow_height = if row_count == 0 {
        0.0
    } else {
        (row_offsets[row_count] - content_y).max(0.0)
    };
    let content_height = specified_height.unwrap_or(flow_height);
    let parent_rect = Rect {
        x: border_x,
        y: border_y,
        width: border_width,
        height: content_height + padding.top + padding.bottom + border.top + border.bottom,
    };

    for oof_id in out_of_flow {
        children.push(layout_out_of_flow(
            doc,
            styles,
            images,
            focused_input,
            oof_id,
            parent_rect,
            containing_width,
        ));
    }

    LayoutBox {
        node: id,
        rect: parent_rect,
        position: style.position,
        z_index: style.z_index,
        overflow: style.overflow,
        background: style.background,
        background_gradient: style.background_gradient,
        box_shadow: style.box_shadow,
        border,
        border_color: style.border_color,
        border_colors: style.border_colors,
        border_radius: style.border_radius,
        outline_width: style.outline_width,
        outline_color: style.outline_color,
        outline_offset: style.outline_offset,
        scrollable_overflow: style.scrollable_overflow,
        scroll_offset: (0.0, 0.0),
        scroll_size: (0.0, 0.0),
        object_fit: style.object_fit,
        image: None,
        sticky: None,
        transform: None,
        opacity: 1.0,
        text: Vec::new(),
        children,
    }
}

/// One flex item after its base border box has been measured.
struct FlexItem {
    node_box: LayoutBox,
    outer_w: f32,
    outer_h: f32,
    grow: f32,
    shrink: f32,
    min_main: f32,
    max_main: f32,
    item_cb_w: f32,
}

/// First baseline offset of a box relative to its border-box top (`rect.y`).
fn box_first_baseline(b: &LayoutBox, styles: &StyleMap) -> f32 {
    if let Some(run) = b.text.first() {
        return (run.y - b.rect.y) + run.font_size * 0.8;
    }
    for child in &b.children {
        if !child.text.is_empty() || !child.children.is_empty() {
            return (child.rect.y - b.rect.y) + box_first_baseline(child, styles);
        }
    }
    let cs = styles.get(b.node);
    let synth = cs.border.top + cs.padding.top + cs.font_size * 0.8;
    if b.rect.height > 0.0 {
        synth.min(b.rect.height)
    } else {
        synth
    }
}

/// Iterative CSS Flexbox flexible length resolution with `min`/`max` clamping freeze loop.
fn resolve_flexible_lengths(
    line: &mut [FlexItem],
    available_main: f32,
    main_gap: f32,
    can_shrink: bool,
    is_row: bool,
) {
    if line.is_empty() {
        return;
    }
    let count = line.len();
    let gaps = if count > 1 {
        main_gap * ((count - 1) as f32)
    } else {
        0.0
    };
    let main_size = |it: &FlexItem| {
        if is_row {
            it.node_box.rect.width
        } else {
            it.node_box.rect.height
        }
    };
    let outer_main = |it: &FlexItem| if is_row { it.outer_w } else { it.outer_h };
    let set_main = |it: &mut FlexItem, new_main: f32| {
        if is_row {
            let margin_h = it.outer_w - it.node_box.rect.width;
            it.node_box.rect.width = new_main;
            it.outer_w = new_main + margin_h;
        } else {
            let margin_v = it.outer_h - it.node_box.rect.height;
            it.node_box.rect.height = new_main;
            it.outer_h = new_main + margin_v;
        }
    };

    let base_sizes: Vec<f32> = line.iter().map(main_size).collect();
    let margins: Vec<f32> = line
        .iter()
        .map(|it| outer_main(it) - main_size(it))
        .collect();
    let initial_used: f32 = line.iter().map(outer_main).sum::<f32>() + gaps;
    let growing = available_main > initial_used;
    let shrinking = can_shrink && available_main < initial_used;
    if !growing && !shrinking {
        return;
    }

    let mut frozen = vec![false; count];
    for (i, it) in line.iter_mut().enumerate() {
        if (growing && it.grow <= 0.0) || (shrinking && it.shrink <= 0.0) {
            let clamped = base_sizes[i].clamp(it.min_main, it.max_main);
            set_main(it, clamped);
            frozen[i] = true;
        }
    }

    let mut targets = base_sizes.clone();
    for _ in 0..=count {
        if frozen.iter().all(|&f| f) {
            break;
        }
        let used_frozen: f32 = line
            .iter()
            .enumerate()
            .filter(|(i, _)| frozen[*i])
            .map(|(_, it)| outer_main(it))
            .sum();
        let unfrozen_base: f32 = (0..count)
            .filter(|&i| !frozen[i])
            .map(|i| base_sizes[i] + margins[i])
            .sum();
        let remaining_free = available_main - gaps - used_frozen - unfrozen_base;

        if growing {
            let grow_sum: f32 = line
                .iter()
                .enumerate()
                .filter(|(i, _)| !frozen[*i])
                .map(|(_, it)| it.grow)
                .sum();
            if grow_sum <= 0.0 {
                break;
            }
            for i in 0..count {
                if !frozen[i] {
                    targets[i] = if remaining_free > 0.0 {
                        base_sizes[i] + remaining_free * line[i].grow / grow_sum
                    } else {
                        base_sizes[i]
                    };
                }
            }
        } else {
            let shrink_sum: f32 = line
                .iter()
                .enumerate()
                .filter(|(i, _)| !frozen[*i])
                .map(|(i, it)| it.shrink * base_sizes[i].max(1.0))
                .sum();
            if shrink_sum <= 0.0 {
                break;
            }
            let overflow = (-remaining_free).max(0.0);
            for i in 0..count {
                if !frozen[i] {
                    let scaled = line[i].shrink * base_sizes[i].max(1.0);
                    targets[i] = (base_sizes[i] - overflow * scaled / shrink_sum).max(0.0);
                }
            }
        }

        let mut any_violation = false;
        for i in 0..count {
            if !frozen[i]
                && (targets[i] < line[i].min_main - 0.01 || targets[i] > line[i].max_main + 0.01)
            {
                let clamped = targets[i].clamp(line[i].min_main, line[i].max_main);
                set_main(&mut line[i], clamped);
                frozen[i] = true;
                any_violation = true;
            }
        }
        if !any_violation {
            for i in 0..count {
                if !frozen[i] {
                    let clamped = targets[i].clamp(line[i].min_main, line[i].max_main);
                    set_main(&mut line[i], clamped);
                    frozen[i] = true;
                }
            }
            break;
        }
    }
}

/// Compute `(start_offset, step_gap)` for cross-axis multi-line distribution (`align-content`).
fn distribute_align_content(
    align: AlignContent,
    free_cross: f32,
    base_gap: f32,
    line_count: usize,
    wrap_reverse: bool,
) -> (f32, f32, f32) {
    if line_count == 0 || free_cross <= 0.0 {
        return (0.0, base_gap, 0.0);
    }
    let effective_align = if wrap_reverse {
        match align {
            AlignContent::Start => AlignContent::End,
            AlignContent::End => AlignContent::Start,
            other => other,
        }
    } else {
        align
    };
    match effective_align {
        AlignContent::Stretch => (0.0, base_gap, free_cross / (line_count as f32)),
        AlignContent::Start => (0.0, base_gap, 0.0),
        AlignContent::Center => (free_cross * 0.5, base_gap, 0.0),
        AlignContent::End => (free_cross, base_gap, 0.0),
        AlignContent::SpaceBetween if line_count > 1 => {
            (0.0, base_gap + free_cross / ((line_count - 1) as f32), 0.0)
        }
        AlignContent::SpaceBetween => (0.0, base_gap, 0.0),
        AlignContent::SpaceAround => {
            let u = free_cross / (line_count as f32);
            (u * 0.5, base_gap + u, 0.0)
        }
        AlignContent::SpaceEvenly => {
            let u = free_cross / ((line_count + 1) as f32);
            (u, base_gap + u, 0.0)
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// Flex layout.
fn layout_flex(
    doc: &Document,
    styles: &StyleMap,
    images: &HashMap<NodeId, Image>,
    focused_input: Option<NodeId>,
    id: NodeId,
    x: f32,
    y: f32,
    containing_width: f32,
    containing_height: Option<f32>,
) -> LayoutBox {
    let style = *styles.get(id);
    let padding = style.padding;
    let margin = style.margin;
    let border = style.border;
    let mut border_width = resolve_border_width(&style, containing_width, styles);
    if matches!(style.width, Size::Auto) && style.inline_level {
        border_width = intrinsic_box_width(doc, styles, images, id, containing_width);
    }
    let content_width =
        (border_width - padding.left - padding.right - border.left - border.right).max(0.0);
    let border_x = resolve_border_x(&style, x, containing_width, border_width);
    let border_y = y + margin.top;
    let content_x = border_x + border.left + padding.left;
    let content_y = border_y + border.top + padding.top;
    let chrome_v = padding.top + padding.bottom + border.top + border.bottom;
    let specified_height = used_length(style.height, containing_height, &style, styles).map(|h| {
        match style.box_sizing {
            BoxSizing::ContentBox => h.max(0.0),
            BoxSizing::BorderBox => (h - chrome_v).max(0.0),
        }
    });

    let mut visible_children = Vec::new();
    let mut out_of_flow = Vec::new();
    for &child in doc.composed_children(id).iter() {
        if is_skipped(doc, styles, child) {
            continue;
        }
        if let NodeData::Text(t) = doc.get(child).data()
            && t.trim().is_empty()
        {
            continue;
        }
        let cstyle = styles.get(child);
        if matches!(cstyle.position, Position::Absolute | Position::Fixed) {
            out_of_flow.push(child);
        } else {
            visible_children.push(child);
        }
    }
    visible_children.sort_by_key(|&cid| {
        if matches!(doc.get(cid).data(), NodeData::Text(_)) {
            0
        } else {
            styles.get(cid).order
        }
    });
    let (row_gap, column_gap) = used_gaps(&style, content_width, specified_height, styles);
    let wrap = matches!(style.flex_wrap, FlexWrap::Wrap | FlexWrap::WrapReverse);
    let wrap_reverse = style.flex_wrap == FlexWrap::WrapReverse;

    if matches!(
        style.flex_direction,
        FlexDirection::Column | FlexDirection::ColumnReverse
    ) {
        let is_reverse = style.flex_direction == FlexDirection::ColumnReverse;
        let mut items = Vec::with_capacity(visible_children.len());
        for &child in &visible_children {
            let child_style = styles.get(child);
            let item_align = child_style.align_self.unwrap_or(style.align_items);
            let has_h_auto = child_style.margin_left_auto || child_style.margin_right_auto;
            let item_cb_w = if matches!(child_style.width, Size::Auto)
                && (item_align != AlignItems::Stretch || has_h_auto)
            {
                (intrinsic_box_width(doc, styles, images, child, content_width)
                    + child_style.margin.left
                    + child_style.margin.right)
                    .min(content_width)
            } else {
                content_width
            };
            let mut child_box = layout_node(
                doc,
                styles,
                images,
                focused_input,
                child,
                content_x,
                content_y,
                item_cb_w,
                specified_height,
                None,
            );
            let child_chrome_v = child_style.padding.top
                + child_style.padding.bottom
                + child_style.border.top
                + child_style.border.bottom;
            let to_border_v = |len: f32| match child_style.box_sizing {
                BoxSizing::ContentBox => (len + child_chrome_v).max(0.0),
                BoxSizing::BorderBox => len.max(child_chrome_v),
            };
            let min_main = used_length(
                child_style.min_height,
                specified_height,
                child_style,
                styles,
            )
            .map(to_border_v)
            .unwrap_or(0.0);
            let max_main = used_length(
                child_style.max_height,
                specified_height,
                child_style,
                styles,
            )
            .map(to_border_v)
            .unwrap_or(f32::INFINITY)
            .max(min_main);
            if !matches!(child_style.flex_basis, Size::Auto)
                && let Some(basis_px) = used_length(
                    child_style.flex_basis,
                    specified_height,
                    child_style,
                    styles,
                )
            {
                child_box.rect.height = to_border_v(basis_px).clamp(min_main, max_main);
            } else {
                child_box.rect.height = child_box.rect.height.clamp(min_main, max_main);
            }
            let outer_w = child_style.margin.left + child_box.rect.width + child_style.margin.right;
            let outer_h =
                child_style.margin.top + child_box.rect.height + child_style.margin.bottom;
            items.push(FlexItem {
                node_box: child_box,
                outer_w,
                outer_h,
                grow: child_style.flex_grow,
                shrink: child_style.flex_shrink,
                min_main,
                max_main,
                item_cb_w,
            });
        }

        let max_col_h = specified_height.or_else(|| {
            used_length(style.max_height, containing_height, &style, styles).map(|mh| {
                match style.box_sizing {
                    BoxSizing::ContentBox => mh.max(0.0),
                    BoxSizing::BorderBox => (mh - chrome_v).max(0.0),
                }
            })
        });
        let mut lines: Vec<Vec<FlexItem>> = Vec::new();
        let mut current: Vec<FlexItem> = Vec::new();
        let mut col_h = 0.0_f32;
        for item in items {
            let gap = if current.is_empty() { 0.0 } else { row_gap };
            if wrap
                && let Some(limit_h) = max_col_h
                && !current.is_empty()
                && col_h + gap + item.outer_h > limit_h
            {
                lines.push(std::mem::take(&mut current));
                col_h = 0.0;
            }
            let gap = if current.is_empty() { 0.0 } else { row_gap };
            col_h += gap + item.outer_h;
            current.push(item);
        }
        if !current.is_empty() {
            lines.push(current);
        }
        if lines.is_empty() {
            lines.push(Vec::new());
        }
        if wrap_reverse && lines.len() > 1 {
            lines.reverse();
        }

        let line_count = lines.len();
        let mut line_widths: Vec<f32> = if line_count == 1 {
            vec![content_width]
        } else {
            lines
                .iter()
                .map(|line| line.iter().map(|it| it.outer_w).fold(0.0_f32, f32::max))
                .collect()
        };
        let cross_gaps = if line_count > 1 {
            column_gap * ((line_count - 1) as f32)
        } else {
            0.0
        };
        let total_line_w: f32 = line_widths.iter().sum::<f32>() + cross_gaps;
        let free_cross_x = (content_width - total_line_w).max(0.0);
        let (start_cross_x, step_cross_gap, stretch_extra_w) = distribute_align_content(
            style.align_content,
            free_cross_x,
            column_gap,
            line_count,
            wrap_reverse,
        );
        if stretch_extra_w > 0.0 && line_count > 1 {
            for lw in &mut line_widths {
                *lw += stretch_extra_w;
            }
        }

        let mut max_flow_height = 0.0_f32;
        for line in &lines {
            let gaps = if line.len() > 1 {
                row_gap * ((line.len() - 1) as f32)
            } else {
                0.0
            };
            let h: f32 = line.iter().map(|it| it.outer_h).sum::<f32>() + gaps;
            max_flow_height = max_flow_height.max(h);
        }
        let mut target_h = specified_height.unwrap_or(max_flow_height);
        if let Some(min_h) = used_length(style.min_height, containing_height, &style, styles) {
            let min_content_h = match style.box_sizing {
                BoxSizing::ContentBox => min_h.max(0.0),
                BoxSizing::BorderBox => (min_h - chrome_v).max(0.0),
            };
            target_h = target_h.max(min_content_h);
        }
        if let Some(max_h) = used_length(style.max_height, containing_height, &style, styles) {
            let max_content_h = match style.box_sizing {
                BoxSizing::ContentBox => max_h.max(0.0),
                BoxSizing::BorderBox => (max_h - chrome_v).max(0.0),
            };
            target_h = target_h.min(max_content_h);
        }

        let can_shrink_v = specified_height.is_some() || !matches!(style.max_height, Size::Auto);
        let mut children = Vec::new();
        let mut col_x = content_x + start_cross_x;
        let mut actual_max_h = 0.0_f32;

        for (line_idx, line) in lines.iter_mut().enumerate() {
            let col_w = line_widths[line_idx];
            resolve_flexible_lengths(line, target_h, row_gap, can_shrink_v, false);
            if is_reverse {
                line.reverse();
            }
            let count = line.len();
            let gaps = if count > 1 {
                row_gap * ((count - 1) as f32)
            } else {
                0.0
            };
            let used_h: f32 = line.iter().map(|it| it.outer_h).sum();
            let free_y = (target_h - used_h - gaps).max(0.0);
            let auto_v_margins: usize = line
                .iter()
                .map(|it| {
                    let cs = styles.get(it.node_box.node);
                    usize::from(cs.margin_top_auto) + usize::from(cs.margin_bottom_auto)
                })
                .sum();
            let auto_v_px = if auto_v_margins > 0 {
                free_y / (auto_v_margins as f32)
            } else {
                0.0
            };
            let effective_justify = if is_reverse {
                match style.justify_content {
                    JustifyContent::Start => JustifyContent::End,
                    JustifyContent::End => JustifyContent::Start,
                    other => other,
                }
            } else {
                style.justify_content
            };
            let (mut cursor_y, step_y) = if auto_v_margins > 0 {
                (content_y, row_gap)
            } else {
                match effective_justify {
                    JustifyContent::Start => (content_y, row_gap),
                    JustifyContent::Center => (content_y + free_y * 0.5, row_gap),
                    JustifyContent::End => (content_y + free_y, row_gap),
                    JustifyContent::SpaceBetween if count > 1 => {
                        (content_y, row_gap + free_y / ((count - 1) as f32))
                    }
                    JustifyContent::SpaceBetween => (content_y, row_gap),
                    JustifyContent::SpaceAround if count > 0 => {
                        let u = free_y / (count as f32);
                        (content_y + u * 0.5, row_gap + u)
                    }
                    JustifyContent::SpaceAround => (content_y, row_gap),
                    JustifyContent::SpaceEvenly if count > 0 => {
                        let u = free_y / ((count + 1) as f32);
                        (content_y + u, row_gap + u)
                    }
                    JustifyContent::SpaceEvenly => (content_y, row_gap),
                }
            };

            for item in line.iter_mut() {
                let child_style = styles.get(item.node_box.node);
                let item_align = child_style.align_self.unwrap_or(style.align_items);
                let has_h_auto = child_style.margin_left_auto || child_style.margin_right_auto;
                if !has_h_auto
                    && item_align == AlignItems::Stretch
                    && matches!(child_style.width, Size::Auto)
                {
                    item.node_box.rect.width =
                        (col_w - child_style.margin.left - child_style.margin.right).max(0.0);
                    item.outer_w = child_style.margin.left
                        + item.node_box.rect.width
                        + child_style.margin.right;
                }
                let free_x = (col_w - item.outer_w).max(0.0);
                let dx_in_col = if child_style.margin_left_auto && child_style.margin_right_auto {
                    free_x * 0.5
                } else if child_style.margin_left_auto {
                    free_x
                } else if child_style.margin_right_auto {
                    0.0
                } else {
                    match item_align {
                        AlignItems::Center => free_x * 0.5,
                        AlignItems::End => free_x,
                        AlignItems::Start | AlignItems::Stretch | AlignItems::Baseline => 0.0,
                    }
                };
                let already_dx =
                    resolve_border_x(child_style, 0.0, item.item_cb_w, item.node_box.rect.width)
                        - child_style.margin.left;
                let mt = child_style.margin.top
                    + if child_style.margin_top_auto {
                        auto_v_px
                    } else {
                        0.0
                    };
                let mb = child_style.margin.bottom
                    + if child_style.margin_bottom_auto {
                        auto_v_px
                    } else {
                        0.0
                    };
                let target_x = col_x + child_style.margin.left + dx_in_col;
                let target_y = cursor_y + mt;
                let dx = target_x - (item.node_box.rect.x - already_dx);
                let dy = target_y - item.node_box.rect.y;
                offset_box(&mut item.node_box, dx, dy);
                cursor_y += mt + item.node_box.rect.height + mb + step_y;
            }
            actual_max_h = actual_max_h.max(used_h + gaps);
            col_x += col_w + step_cross_gap;
            for item in line.drain(..) {
                children.push(item.node_box);
            }
        }

        let content_height = if specified_height.is_some() {
            target_h
        } else {
            target_h.max(actual_max_h)
        };
        let parent_rect = Rect {
            x: border_x,
            y: border_y,
            width: border_width,
            height: content_height + chrome_v,
        };
        for oof_id in out_of_flow {
            children.push(layout_out_of_flow(
                doc,
                styles,
                images,
                focused_input,
                oof_id,
                parent_rect,
                containing_width,
            ));
        }
        return LayoutBox {
            node: id,
            rect: parent_rect,
            position: style.position,
            z_index: style.z_index,
            overflow: style.overflow,
            background: style.background,
            background_gradient: style.background_gradient,
            box_shadow: style.box_shadow,
            border,
            border_color: style.border_color,
            border_colors: style.border_colors,
            border_radius: style.border_radius,
            outline_width: style.outline_width,
            outline_color: style.outline_color,
            outline_offset: style.outline_offset,
            scrollable_overflow: style.scrollable_overflow,
            scroll_offset: (0.0, 0.0),
            scroll_size: (0.0, 0.0),
            object_fit: style.object_fit,
            image: None,
            sticky: None,
            transform: None,
            opacity: 1.0,
            text: Vec::new(),
            children,
        };
    }

    let is_reverse = style.flex_direction == FlexDirection::RowReverse;
    let mut items = Vec::new();
    for child in &visible_children {
        let child_style = styles.get(*child);
        let explicit_basis = if !matches!(child_style.flex_basis, Size::Auto) {
            used_length(
                child_style.flex_basis,
                Some(content_width),
                child_style,
                styles,
            )
        } else {
            used_length(child_style.width, Some(content_width), child_style, styles)
        };
        let basis_width = if let Some(w) = explicit_basis {
            let bw = match child_style.box_sizing {
                BoxSizing::ContentBox => {
                    w + child_style.padding.left
                        + child_style.padding.right
                        + child_style.border.left
                        + child_style.border.right
                }
                BoxSizing::BorderBox => w.max(
                    child_style.padding.left
                        + child_style.padding.right
                        + child_style.border.left
                        + child_style.border.right,
                ),
            };
            bw + child_style.margin.left + child_style.margin.right
        } else {
            let child_data = doc.get(*child).data();
            let child_tag = child_data.tag().unwrap_or("");
            let intrinsic_content_w = if child_tag == "img" || child_tag == "video" {
                child_data
                    .attr("width")
                    .and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
                    .or_else(|| images.get(child).map(|img| img.width as f32))
                    .unwrap_or(120.0)
            } else if child_tag == "svg" {
                svg::intrinsic_svg_size(doc, *child).0
            } else if child_tag == "input" && doc.is_text_input(*child) {
                (content_width - 136.0).clamp(160.0, 560.0)
            } else if child_tag == "input" && doc.is_submit_control(*child) {
                let lbl = child_data.attr("value").unwrap_or("Submit");
                measure(lbl, child_style.font_size) + 12.0
            } else if child_tag == "button" {
                measure(&doc.text_content(*child), child_style.font_size) + 12.0
            } else {
                measure(&doc.text_content(*child), child_style.font_size)
            };
            (intrinsic_content_w
                + child_style.padding.left
                + child_style.padding.right
                + child_style.border.left
                + child_style.border.right
                + child_style.margin.left
                + child_style.margin.right)
                .min(content_width)
        };
        let item_containing = if explicit_basis.is_some() {
            content_width
        } else {
            basis_width
        };
        let mut child_box = layout_node(
            doc,
            styles,
            images,
            focused_input,
            *child,
            0.0,
            0.0,
            item_containing,
            specified_height,
            None,
        );
        let chrome_h = child_style.padding.left
            + child_style.padding.right
            + child_style.border.left
            + child_style.border.right;
        let to_border = |len: f32| match child_style.box_sizing {
            BoxSizing::ContentBox => (len + chrome_h).max(0.0),
            BoxSizing::BorderBox => len.max(chrome_h),
        };
        let min_main = used_length(
            child_style.min_width,
            Some(content_width),
            child_style,
            styles,
        )
        .map(to_border)
        .unwrap_or(0.0);
        let max_main = used_length(
            child_style.max_width,
            Some(content_width),
            child_style,
            styles,
        )
        .map(to_border)
        .unwrap_or(f32::INFINITY)
        .max(min_main);
        if let Some(basis_px) = explicit_basis
            && matches!(child_style.width, Size::Auto)
        {
            let bw = to_border(basis_px);
            child_box.rect.width = clamp_border_width(child_style, content_width, styles, bw);
        }
        let outer_w = child_style.margin.left + child_box.rect.width + child_style.margin.right;
        let outer_h = child_style.margin.top + child_box.rect.height + child_style.margin.bottom;
        items.push(FlexItem {
            node_box: child_box,
            outer_w,
            outer_h,
            grow: child_style.flex_grow,
            shrink: child_style.flex_shrink,
            min_main,
            max_main,
            item_cb_w: item_containing,
        });
    }

    let mut lines: Vec<Vec<FlexItem>> = Vec::new();
    let mut current: Vec<FlexItem> = Vec::new();
    let mut line_width = 0.0_f32;
    for item in items {
        let gap = if current.is_empty() { 0.0 } else { column_gap };
        if wrap && !current.is_empty() && line_width + gap + item.outer_w > content_width {
            lines.push(std::mem::take(&mut current));
            line_width = 0.0;
        }
        let gap = if current.is_empty() { 0.0 } else { column_gap };
        line_width += gap + item.outer_w;
        current.push(item);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    if wrap_reverse && lines.len() > 1 {
        lines.reverse();
    }

    let line_count = lines.len();
    let mut line_heights = Vec::with_capacity(line_count);
    let mut line_baselines = Vec::with_capacity(line_count);
    for line in &mut lines {
        let can_shrink = !wrap || line.len() == 1;
        resolve_flexible_lengths(line, content_width, column_gap, can_shrink, true);
        if is_reverse {
            line.reverse();
        }
        let mut max_asc = 0.0_f32;
        let mut max_desc = 0.0_f32;
        let mut has_baseline = false;
        for item in line.iter() {
            let cs = styles.get(item.node_box.node);
            let item_align = cs.align_self.unwrap_or(style.align_items);
            if item_align == AlignItems::Baseline && !cs.margin_top_auto && !cs.margin_bottom_auto {
                let asc = cs.margin.top + box_first_baseline(&item.node_box, styles);
                let desc = (item.outer_h - asc).max(0.0);
                max_asc = max_asc.max(asc);
                max_desc = max_desc.max(desc);
                has_baseline = true;
            }
        }
        let base_h = if has_baseline {
            max_asc + max_desc
        } else {
            0.0
        };
        let lh = line.iter().map(|item| item.outer_h).fold(base_h, f32::max);
        line_heights.push(lh);
        line_baselines.push(max_asc);
    }

    let cross_gaps = if line_count > 1 {
        row_gap * ((line_count - 1) as f32)
    } else {
        0.0
    };
    let base_lines_cross: f32 = line_heights.iter().sum::<f32>() + cross_gaps;
    let mut target_cross_h = specified_height.unwrap_or(base_lines_cross);
    if let Some(min_h) = used_length(style.min_height, containing_height, &style, styles) {
        let min_content_h = match style.box_sizing {
            BoxSizing::ContentBox => min_h.max(0.0),
            BoxSizing::BorderBox => (min_h - chrome_v).max(0.0),
        };
        target_cross_h = target_cross_h.max(min_content_h);
    }
    if let Some(max_h) = used_length(style.max_height, containing_height, &style, styles) {
        let max_content_h = match style.box_sizing {
            BoxSizing::ContentBox => max_h.max(0.0),
            BoxSizing::BorderBox => (max_h - chrome_v).max(0.0),
        };
        target_cross_h = target_cross_h.min(max_content_h);
    }
    let free_cross_y = (target_cross_h - base_lines_cross).max(0.0);
    let (start_cross_y, step_row_gap, stretch_extra_h) = distribute_align_content(
        style.align_content,
        free_cross_y,
        row_gap,
        line_count,
        wrap_reverse,
    );
    if stretch_extra_h > 0.0 {
        for lh in &mut line_heights {
            *lh += stretch_extra_h;
        }
    }

    let mut children = Vec::new();
    let mut cursor_y = content_y + start_cross_y;
    for (line_index, line) in lines.iter_mut().enumerate() {
        if line_index > 0 {
            cursor_y += step_row_gap;
        }
        let count = line.len();
        let gaps = if count > 1 {
            column_gap * ((count - 1) as f32)
        } else {
            0.0
        };
        let used_after: f32 = line.iter().map(|item| item.outer_w).sum();
        let free_after = (content_width - used_after - gaps).max(0.0);
        let auto_margins: usize = line
            .iter()
            .map(|item| {
                let cs = styles.get(item.node_box.node);
                usize::from(cs.margin_left_auto) + usize::from(cs.margin_right_auto)
            })
            .sum();
        let auto_margin_px = if auto_margins > 0 {
            free_after / (auto_margins as f32)
        } else {
            0.0
        };
        let effective_justify = if is_reverse {
            match style.justify_content {
                JustifyContent::Start => JustifyContent::End,
                JustifyContent::End => JustifyContent::Start,
                other => other,
            }
        } else {
            style.justify_content
        };
        let (mut cursor_x, step_gap) = if auto_margins > 0 {
            (content_x, column_gap)
        } else {
            match effective_justify {
                JustifyContent::Start => (content_x, column_gap),
                JustifyContent::Center => (content_x + free_after * 0.5, column_gap),
                JustifyContent::End => (content_x + free_after, column_gap),
                JustifyContent::SpaceBetween if count > 1 => {
                    (content_x, column_gap + free_after / ((count - 1) as f32))
                }
                JustifyContent::SpaceBetween => (content_x, column_gap),
                JustifyContent::SpaceAround if count > 0 => {
                    let u = free_after / (count as f32);
                    (content_x + u * 0.5, column_gap + u)
                }
                JustifyContent::SpaceAround => (content_x, column_gap),
                JustifyContent::SpaceEvenly if count > 0 => {
                    let u = free_after / ((count + 1) as f32);
                    (content_x + u, column_gap + u)
                }
                JustifyContent::SpaceEvenly => (content_x, column_gap),
            }
        };
        let line_height = line_heights[line_index];
        let line_baseline = line_baselines[line_index];
        for item in line.iter_mut() {
            let child_style = styles.get(item.node_box.node);
            let item_align = child_style.align_self.unwrap_or(style.align_items);
            let free_item_y = (line_height - item.outer_h).max(0.0);
            let has_v_auto = child_style.margin_top_auto || child_style.margin_bottom_auto;
            let y_offset = if child_style.margin_top_auto && child_style.margin_bottom_auto {
                free_item_y * 0.5
            } else if child_style.margin_top_auto {
                free_item_y
            } else if child_style.margin_bottom_auto {
                0.0
            } else {
                match item_align {
                    AlignItems::Start | AlignItems::Stretch => 0.0,
                    AlignItems::Center => free_item_y * 0.5,
                    AlignItems::End => free_item_y,
                    AlignItems::Baseline => {
                        let asc =
                            child_style.margin.top + box_first_baseline(&item.node_box, styles);
                        (line_baseline - asc).max(0.0)
                    }
                }
            };
            let ml = child_style.margin.left
                + if child_style.margin_left_auto {
                    auto_margin_px
                } else {
                    0.0
                };
            let mr = child_style.margin.right
                + if child_style.margin_right_auto {
                    auto_margin_px
                } else {
                    0.0
                };
            let dx = (cursor_x + ml) - item.node_box.rect.x;
            let dy = (cursor_y + y_offset + child_style.margin.top) - item.node_box.rect.y;
            offset_box(&mut item.node_box, dx, dy);
            if !has_v_auto
                && item_align == AlignItems::Stretch
                && matches!(child_style.height, Size::Auto)
            {
                item.node_box.rect.height =
                    (line_height - child_style.margin.top - child_style.margin.bottom)
                        .max(item.node_box.rect.height);
            }
            cursor_x += ml + item.node_box.rect.width + mr + step_gap;
        }
        cursor_y += line_height;
        for item in line.drain(..) {
            children.push(item.node_box);
        }
    }

    let content_height = target_cross_h;
    let parent_rect = Rect {
        x: border_x,
        y: border_y,
        width: border_width,
        height: content_height + chrome_v,
    };
    for oof_id in out_of_flow {
        children.push(layout_out_of_flow(
            doc,
            styles,
            images,
            focused_input,
            oof_id,
            parent_rect,
            containing_width,
        ));
    }
    LayoutBox {
        node: id,
        rect: parent_rect,
        position: style.position,
        z_index: style.z_index,
        overflow: style.overflow,
        background: style.background,
        background_gradient: style.background_gradient,
        box_shadow: style.box_shadow,
        border,
        border_color: style.border_color,
        border_colors: style.border_colors,
        border_radius: style.border_radius,
        outline_width: style.outline_width,
        outline_color: style.outline_color,
        outline_offset: style.outline_offset,
        scrollable_overflow: style.scrollable_overflow,
        scroll_offset: (0.0, 0.0),
        scroll_size: (0.0, 0.0),
        object_fit: style.object_fit,
        image: None,
        sticky: None,
        transform: None,
        opacity: 1.0,
        text: Vec::new(),
        children,
    }
}

/// Add `dx` and `dy` to this box, its text runs, and every descendant.
/// Positive `dy` moves the subtree downward.
fn offset_box(tree: &mut LayoutBox, dx: f32, dy: f32) {
    tree.rect.x += dx;
    tree.rect.y += dy;
    if let Some(sticky) = &mut tree.sticky {
        *sticky = sticky.translated(dx, dy);
    }
    if let Some(transform) = &mut tree.transform {
        *transform = transform.translated(dx, dy);
    }
    for run in &mut tree.text {
        run.x += dx;
        run.y += dy;
    }
    for child in &mut tree.children {
        offset_box(child, dx, dy);
    }
}

fn align_runs(runs: &mut [TextRun], origin_x: f32, max_width: f32, align: TextAlign) {
    if matches!(align, TextAlign::Left | TextAlign::Justify) || runs.is_empty() || max_width <= 0.0
    {
        return;
    }
    let mut start = 0usize;
    while start < runs.len() {
        let line_y = runs[start].y;
        let mut end = start + 1;
        while end < runs.len() && (runs[end].y - line_y).abs() < 0.01 {
            end += 1;
        }
        let line_end_x = runs[end - 1].x + runs[end - 1].width;
        let line_w = (line_end_x - origin_x).max(0.0);
        let shift = match align {
            TextAlign::Center => ((max_width - line_w) * 0.5).max(0.0),
            TextAlign::Right => (max_width - line_w).max(0.0),
            TextAlign::Left | TextAlign::Justify => 0.0,
        };
        if shift > 0.0 {
            for run in &mut runs[start..end] {
                run.x += shift;
            }
        }
        start = end;
    }
}

fn apply_text_overflow_ellipsis(runs: &mut Vec<TextRun>, origin_x: f32, max_width: f32) {
    if runs.is_empty() || max_width <= 0.0 {
        return;
    }
    let limit_x = origin_x + max_width;
    let mut out = Vec::with_capacity(runs.len());
    let mut idx = 0usize;
    while idx < runs.len() {
        let line_y = runs[idx].y;
        let mut end = idx + 1;
        while end < runs.len() && (runs[end].y - line_y).abs() < 0.01 {
            end += 1;
        }
        let line_overflows = runs[idx..end]
            .last()
            .is_some_and(|r| r.x + r.width > limit_x + 0.01);
        if !line_overflows {
            out.extend_from_slice(&runs[idx..end]);
            idx = end;
            continue;
        }
        for run in &runs[idx..end] {
            if run.x + run.width <= limit_x + 0.01 {
                out.push(run.clone());
                continue;
            }
            let avail = (limit_x - run.x).max(0.0);
            let ellipsis = "…";
            let ellipsis_w = measure(ellipsis, run.font_size);
            let mut truncated = String::new();
            for ch in run.text.chars() {
                let next_w = measure(&truncated, run.font_size)
                    + measure(&ch.to_string(), run.font_size)
                    + ellipsis_w;
                if next_w <= avail + 0.01 {
                    truncated.push(ch);
                } else {
                    break;
                }
            }
            truncated.push_str(ellipsis);
            let mut new_run = run.clone();
            new_run.width = measure(&truncated, new_run.font_size);
            new_run.text = truncated;
            out.push(new_run);
            break;
        }
        idx = end;
    }
    *runs = out;
}

#[allow(clippy::too_many_arguments)]
/// Flush queued inline children into line boxes and advance `cursor_y`.
///
/// A pending list marker is inserted as the first segment. An empty queue
/// with no marker does nothing. `cursor_y` is the top of the next line, y downward.
fn place_inlines(
    doc: &Document,
    styles: &StyleMap,
    block_id: NodeId,
    pending_marker: &mut Option<String>,
    pending_before: &mut Option<(String, ComputedStyle)>,
    after_inline: Option<(&str, &ComputedStyle)>,
    inline_ids: &mut Vec<NodeId>,
    content_x: f32,
    cursor_y: &mut f32,
    content_width: f32,
    text: &mut Vec<TextRun>,
) {
    if inline_ids.is_empty()
        && pending_marker.is_none()
        && pending_before.is_none()
        && after_inline.is_none()
    {
        return;
    }
    let block_style = styles.get(block_id);
    let mut segments = Vec::new();
    if let Some(marker) = pending_marker.take() {
        segments.push(Segment::plain(
            block_id,
            marker,
            block_style.font_size,
            block_style.line_height,
            block_style.color,
            false,
        ));
    }
    if let Some((b_txt, b_style)) = pending_before.take() {
        segments.push(Segment {
            node: block_id,
            text: apply_text_transform(&b_txt, b_style.text_transform),
            font_size: b_style.font_size,
            line_height: b_style.line_height,
            color: b_style.color,
            underline: b_style.underline,
            line_through: b_style.line_through,
            letter_spacing: b_style.letter_spacing,
            word_spacing: b_style.word_spacing,
        });
    }
    for id in inline_ids.drain(..) {
        gather(doc, styles, id, styles.get(id), &mut segments);
    }
    if let Some((a_txt, a_style)) = after_inline {
        segments.push(Segment {
            node: block_id,
            text: apply_text_transform(a_txt, a_style.text_transform),
            font_size: a_style.font_size,
            line_height: a_style.line_height,
            color: a_style.color,
            underline: a_style.underline,
            line_through: a_style.line_through,
            letter_spacing: a_style.letter_spacing,
            word_spacing: a_style.word_spacing,
        });
    }
    let wrap_width = if block_style.white_space == WhiteSpace::Nowrap
        || block_style.white_space == WhiteSpace::Pre
    {
        content_width.max(16384.0)
    } else {
        content_width
    };
    let preserve_spaces = matches!(
        block_style.white_space,
        WhiteSpace::Pre | WhiteSpace::PreWrap
    );
    let (mut runs, height) =
        place_lines(&segments, content_x, *cursor_y, wrap_width, preserve_spaces);
    if block_style.text_overflow_ellipsis && block_style.overflow == Overflow::Hidden {
        apply_text_overflow_ellipsis(&mut runs, content_x, content_width);
    }
    align_runs(&mut runs, content_x, content_width, block_style.text_align);
    text.extend(runs);
    *cursor_y += height;
}

/// Append inline text for `id` and its descendants. `display: none` stops the walk.
/// A `br` becomes a segment whose text is a single newline.
fn gather(
    doc: &Document,
    styles: &StyleMap,
    id: NodeId,
    style: &ComputedStyle,
    out: &mut Vec<Segment>,
) {
    match doc.get(id).data() {
        NodeData::Text(text) => out.push(Segment {
            node: id,
            text: apply_text_transform(text, style.text_transform),
            font_size: style.font_size,
            line_height: style.line_height,
            color: style.color,
            underline: style.underline,
            line_through: style.line_through,
            letter_spacing: style.letter_spacing,
            word_spacing: style.word_spacing,
        }),
        NodeData::Element { tag, .. } => {
            if styles.get(id).display == Display::None || tag == "svg" {
                return;
            }
            if tag == "br" {
                out.push(Segment::plain(
                    id,
                    "\n".to_string(),
                    style.font_size,
                    style.line_height,
                    style.color,
                    false,
                ));
                return;
            }
            if let Some((b_txt, b_style)) = styles.before_style(id)
                && b_style.display != Display::None
                && !b_txt.is_empty()
            {
                out.push(Segment {
                    node: id,
                    text: apply_text_transform(b_txt, b_style.text_transform),
                    font_size: b_style.font_size,
                    line_height: b_style.line_height,
                    color: b_style.color,
                    underline: b_style.underline,
                    line_through: b_style.line_through,
                    letter_spacing: b_style.letter_spacing,
                    word_spacing: b_style.word_spacing,
                });
            }
            let style = *styles.get(id);
            for child in doc.composed_children(id).iter() {
                gather(doc, styles, *child, &style, out);
            }
            if let Some((a_txt, a_style)) = styles.after_style(id)
                && a_style.display != Display::None
                && !a_txt.is_empty()
            {
                out.push(Segment {
                    node: id,
                    text: apply_text_transform(a_txt, a_style.text_transform),
                    font_size: a_style.font_size,
                    line_height: a_style.line_height,
                    color: a_style.color,
                    underline: a_style.underline,
                    line_through: a_style.line_through,
                    letter_spacing: a_style.letter_spacing,
                    word_spacing: a_style.word_spacing,
                });
            }
        }
        NodeData::Document => {}
    }
}

/// True for an element whose used display is `none`. Text nodes are not skipped.
fn is_skipped(doc: &Document, styles: &StyleMap, id: NodeId) -> bool {
    matches!(doc.get(id).data(), NodeData::Element { .. })
        && styles.get(id).display == Display::None
}

/// True when an inline element has background, border, padding, or explicit
/// width/height that requires its own box to paint.
fn is_decorated_inline(style: &ComputedStyle) -> bool {
    style.display == Display::Inline
        && (style.background.is_some()
            || style.background_gradient.is_some()
            || style.border.top > 0.0
            || style.border.right > 0.0
            || style.border.bottom > 0.0
            || style.border.left > 0.0
            || style.padding.top > 0.0
            || style.padding.right > 0.0
            || style.padding.bottom > 0.0
            || style.padding.left > 0.0
            || !matches!(style.width, Size::Auto)
            || !matches!(style.height, Size::Auto))
}

/// True for `img`, `video`, `svg`, `progress`, `meter`, a block or flex element,
/// a decorated inline element, or an element that contains a block/replaced descendant.
fn is_block_box(doc: &Document, styles: &StyleMap, id: NodeId) -> bool {
    match doc.get(id).data() {
        NodeData::Element { tag, .. } => {
            let style = styles.get(id);
            tag == "img"
                || tag == "video"
                || tag == "canvas"
                || tag == "svg"
                || tag == "progress"
                || tag == "meter"
                || matches!(
                    style.display,
                    Display::Block | Display::Flex | Display::Grid
                )
                || is_decorated_inline(style)
                || contains_block(doc, styles, id)
        }
        _ => false,
    }
}

/// True when any descendant is `img`, `video`, `canvas`, `svg`, `progress`, `meter`, block, or flex.
fn contains_block(doc: &Document, styles: &StyleMap, id: NodeId) -> bool {
    if matches!(doc.get(id).data().tag(), Some("svg" | "progress" | "meter")) {
        return false;
    }
    for child in doc.composed_children(id).iter() {
        if let NodeData::Element { tag, .. } = doc.get(*child).data()
            && (tag == "img"
                || tag == "video"
                || tag == "canvas"
                || tag == "svg"
                || tag == "progress"
                || tag == "meter"
                || matches!(
                    styles.get(*child).display,
                    Display::Block | Display::Flex | Display::Grid
                ))
        {
            return true;
        }
        if contains_block(doc, styles, *child) {
            return true;
        }
    }
    false
}

/// Render a procedural RGBA bitmap for `<input type="range">`, `<progress>`, or `<meter>`.
fn render_control_bar_image(
    width: u32,
    height: u32,
    ratio: f32,
    is_range: bool,
    is_meter: bool,
) -> Option<Image> {
    let w = width.clamp(24, 1024);
    let h = height.clamp(8, 128);
    let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
    let clamped = ratio.clamp(0.0, 1.0);
    let track_h = if is_range {
        6u32.min(h)
    } else {
        (h.saturating_sub(4)).max(6)
    };
    let track_y0 = (h.saturating_sub(track_h)) / 2;
    let track_y1 = track_y0 + track_h;
    let pad_x = if is_range { 6u32 } else { 1u32 };
    let track_x0 = pad_x.min(w / 4);
    let track_x1 = w.saturating_sub(track_x0).max(track_x0 + 1);
    let fill_end = track_x0 + (((track_x1 - track_x0) as f32) * clamped).round() as u32;
    let fill_rgb = if is_meter {
        if clamped < 0.25 {
            [230u8, 162, 60]
        } else {
            [68u8, 188, 130]
        }
    } else {
        [98u8, 156, 236]
    };
    let track_rgb = [42u8, 49, 64];
    let border_rgb = [76u8, 86, 106];

    for y in track_y0..track_y1 {
        for x in track_x0..track_x1 {
            let idx = ((y * w + x) as usize) * 4;
            let on_edge = y == track_y0 || y + 1 == track_y1 || x == track_x0 || x + 1 == track_x1;
            let rgb = if on_edge {
                border_rgb
            } else if x <= fill_end {
                fill_rgb
            } else {
                track_rgb
            };
            rgba[idx] = rgb[0];
            rgba[idx + 1] = rgb[1];
            rgba[idx + 2] = rgb[2];
            rgba[idx + 3] = 255;
        }
    }

    if is_range {
        let thumb_cx = fill_end.clamp(track_x0, track_x1.saturating_sub(1)) as i32;
        let thumb_cy = (h / 2) as i32;
        let radius = ((h.min(18) / 2).max(4)) as i32;
        let r2 = radius * radius;
        let inner_r2 = (radius - 2).max(1) * (radius - 2).max(1);
        for y in 0..(h as i32) {
            for x in 0..(w as i32) {
                let dx = x - thumb_cx;
                let dy = y - thumb_cy;
                let d2 = dx * dx + dy * dy;
                if d2 <= r2 {
                    let idx = ((y as u32 * w + x as u32) as usize) * 4;
                    let rgb = if d2 <= inner_r2 {
                        [232u8, 195, 122]
                    } else {
                        [28u8, 34, 46]
                    };
                    rgba[idx] = rgb[0];
                    rgba[idx + 1] = rgb[1];
                    rgba[idx + 2] = rgb[2];
                    rgba[idx + 3] = 255;
                }
            }
        }
    }

    Image::from_rgba(w, h, rgba).ok()
}

fn measure_token(text: &str, font_size: f32, letter_spacing: f32, word_spacing: f32) -> f32 {
    let base = measure(text, font_size);
    if text.is_empty() || text == "\n" {
        return 0.0;
    }
    if letter_spacing == 0.0 && word_spacing == 0.0 {
        return base;
    }
    let chars = text.chars().count() as f32;
    let spaces = text.chars().filter(|&c| c == ' ').count() as f32;
    (base + chars * letter_spacing + spaces * word_spacing).max(0.0)
}

/// Break `segments` into line boxes inside `max_width`, starting at
/// `(origin_x, origin_y)` with y downward.
///
/// Returns the owned runs and the total height. A line that would pass
/// `max_width` starts a new line. A space at the start of a line is dropped.
/// An empty line from a newline advances 16px. Other lines use the maximum
/// [`line_height`] on that line. Adjacent tokens with the same node, size,
/// color, and underline are merged into one run.
fn place_lines(
    segments: &[Segment],
    origin_x: f32,
    origin_y: f32,
    max_width: f32,
    preserve_spaces: bool,
) -> (Vec<TextRun>, f32) {
    let tokens = explode_wide(tokenize(segments), max_width);
    let mut lines: Vec<Vec<Token>> = Vec::new();
    let mut current: Vec<Token> = Vec::new();
    let mut line_width = 0.0;

    for token in tokens {
        if token.text == "\n" {
            trim_trailing_space(&mut current);
            lines.push(std::mem::take(&mut current));
            line_width = 0.0;
            continue;
        }
        if !preserve_spaces && token.text == " " && (current.is_empty() || line_width == 0.0) {
            continue;
        }
        let width = measure_token(
            &token.text,
            token.font_size,
            token.letter_spacing,
            token.word_spacing,
        );
        // Overflow wraps before this token. A wrapping space is discarded.
        if line_width > 0.0 && line_width + width > max_width {
            trim_trailing_space(&mut current);
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            line_width = 0.0;
            if !preserve_spaces && token.text == " " {
                continue;
            }
        }
        line_width += width;
        current.push(token);
    }
    trim_trailing_space(&mut current);
    if !current.is_empty() {
        lines.push(current);
    }

    let mut runs = Vec::new();
    let mut y = origin_y;
    for line in lines {
        if line.is_empty() {
            y += 16.0;
            continue;
        }
        let height = line
            .iter()
            .map(|token| {
                token
                    .line_height
                    .unwrap_or_else(|| line_height(token.font_size))
            })
            .fold(0.0_f32, f32::max);
        let mut x = origin_x;
        let mut combined: Option<TextRun> = None;
        for token in line {
            let same = combined.as_ref().is_some_and(|run| {
                run.node == token.node
                    && run.font_size == token.font_size
                    && run.color == token.color
                    && run.underline == token.underline
                    && run.line_through == token.line_through
            });
            if same {
                let run = combined.as_mut().expect("checked");
                run.text.push_str(&token.text);
                run.width = measure_token(
                    &run.text,
                    run.font_size,
                    token.letter_spacing,
                    token.word_spacing,
                );
            } else {
                if let Some(run) = combined.take() {
                    x += run.width;
                    runs.push(run);
                }
                let width = measure_token(
                    &token.text,
                    token.font_size,
                    token.letter_spacing,
                    token.word_spacing,
                );
                combined = Some(TextRun {
                    node: token.node,
                    text: token.text,
                    x,
                    y,
                    width,
                    font_size: token.font_size,
                    color: token.color,
                    underline: token.underline,
                    line_through: token.line_through,
                });
            }
        }
        if let Some(run) = combined {
            runs.push(run);
        }
        y += height;
    }
    (runs, y - origin_y)
}

/// Split segments into words and separate space and newline tokens.
fn tokenize(segments: &[Segment]) -> Vec<Token> {
    let mut tokens = Vec::new();
    for segment in segments {
        if segment.text == "\n" {
            tokens.push(Token {
                node: segment.node,
                text: "\n".to_string(),
                font_size: segment.font_size,
                line_height: segment.line_height,
                color: segment.color,
                underline: segment.underline,
                line_through: segment.line_through,
                letter_spacing: segment.letter_spacing,
                word_spacing: segment.word_spacing,
            });
            continue;
        }
        let mut word = String::new();
        for ch in segment.text.chars() {
            if ch == '\r' {
                continue;
            }
            if ch == '\n' {
                if !word.is_empty() {
                    tokens.push(Token {
                        node: segment.node,
                        text: std::mem::take(&mut word),
                        font_size: segment.font_size,
                        line_height: segment.line_height,
                        color: segment.color,
                        underline: segment.underline,
                        line_through: segment.line_through,
                        letter_spacing: segment.letter_spacing,
                        word_spacing: segment.word_spacing,
                    });
                }
                tokens.push(Token {
                    node: segment.node,
                    text: "\n".to_string(),
                    font_size: segment.font_size,
                    line_height: segment.line_height,
                    color: segment.color,
                    underline: segment.underline,
                    line_through: segment.line_through,
                    letter_spacing: segment.letter_spacing,
                    word_spacing: segment.word_spacing,
                });
            } else if ch == ' ' {
                if !word.is_empty() {
                    tokens.push(Token {
                        node: segment.node,
                        text: std::mem::take(&mut word),
                        font_size: segment.font_size,
                        line_height: segment.line_height,
                        color: segment.color,
                        underline: segment.underline,
                        line_through: segment.line_through,
                        letter_spacing: segment.letter_spacing,
                        word_spacing: segment.word_spacing,
                    });
                }
                tokens.push(Token {
                    node: segment.node,
                    text: " ".to_string(),
                    font_size: segment.font_size,
                    line_height: segment.line_height,
                    color: segment.color,
                    underline: segment.underline,
                    line_through: segment.line_through,
                    letter_spacing: segment.letter_spacing,
                    word_spacing: segment.word_spacing,
                });
            } else {
                word.push(ch);
            }
        }
        if !word.is_empty() {
            tokens.push(Token {
                node: segment.node,
                text: word,
                font_size: segment.font_size,
                line_height: segment.line_height,
                color: segment.color,
                underline: segment.underline,
                line_through: segment.line_through,
                letter_spacing: segment.letter_spacing,
                word_spacing: segment.word_spacing,
            });
        }
    }
    tokens
}

/// Split any word wider than `max_width` into one token per character so
/// [`place_lines`] can wrap it. Spaces and newlines are left whole.
/// A non-positive `max_width` returns the tokens unchanged, so nothing wraps.
fn explode_wide(tokens: Vec<Token>, max_width: f32) -> Vec<Token> {
    if max_width <= 0.0 {
        return tokens;
    }
    let mut out = Vec::new();
    for token in tokens {
        if token.text == " "
            || token.text == "\n"
            || measure_token(
                &token.text,
                token.font_size,
                token.letter_spacing,
                token.word_spacing,
            ) <= max_width
        {
            out.push(token);
            continue;
        }
        for ch in token.text.chars() {
            out.push(Token {
                node: token.node,
                text: ch.to_string(),
                font_size: token.font_size,
                line_height: token.line_height,
                color: token.color,
                underline: token.underline,
                line_through: token.line_through,
                letter_spacing: token.letter_spacing,
                word_spacing: token.word_spacing,
            });
        }
    }
    out
}

/// Drop space tokens at the end of a line before it is committed.
fn trim_trailing_space(line: &mut Vec<Token>) {
    while line.last().is_some_and(|token| token.text == " ") {
        line.pop();
    }
}

/// Width used by line breaking: half of `font_size` per Unicode scalar.
/// This is not the proportional width from anvil-ttf.
pub fn measure(text: &str, font_size: f32) -> f32 {
    text.chars().count() as f32 * font_size * 0.5
}

/// Proportional width from the cached system font in anvil-ttf.
///
/// The font is loaded once. If that load returns nothing, the anvil-ttf
/// fallback width is used. Layout does not call this; callers that want a
/// measured string do. `text` is borrowed.
pub fn measure_proportional(text: &str, font_size: f32) -> f32 {
    if let Some(ttf) = system_font() {
        ttf.measure_text(text, font_size)
    } else {
        anvil_ttf::measure_proportional_fallback(text, font_size)
    }
}

/// The system font from anvil-ttf, loaded once. `None` when it did not load.
fn system_font() -> Option<&'static TtfFont> {
    static SYSTEM_FONT: OnceLock<Option<TtfFont>> = OnceLock::new();
    SYSTEM_FONT
        .get_or_init(|| TtfFont::load_system_font(false, false))
        .as_ref()
}

/// Width of one `ch` at `font_size`: the advance of the `0` glyph.
///
/// The engine has one text font, so "the element's font" is that font at the
/// element's size. When the anvil-ttf system font loads and maps `0`, its
/// advance is used. Otherwise the fallback is the fixed advance [`measure`]
/// gives every character, half the font size, so `20ch` then fits exactly 20
/// characters of laid-out text.
pub fn ch_width(font_size: f32) -> f32 {
    ch_width_with(system_font(), font_size)
}

/// [`ch_width`] with an explicit font, or the fallback when `font` is `None`.
fn ch_width_with(font: Option<&TtfFont>, font_size: f32) -> f32 {
    match font {
        Some(ttf) if ttf.glyph_id('0') != 0 => ttf.glyph_advance_px('0', font_size),
        _ => measure("0", font_size),
    }
}

/// Line box height: `font_size` times 1.25. The extra quarter is leading,
/// not a separate font metric.
pub fn line_height(font_size: f32) -> f32 {
    font_size * 1.25
}

#[cfg(test)]
mod tests {
    //! Line wrapping, hit testing, flex, images, and table columns.

    use super::*;
    use anvil_dom::parse_html;
    use anvil_style::{compute_styles, compute_styles_for_viewport, parse_css};

    fn render(html: &str, width: f32) -> (Document, LayoutBox) {
        let doc = parse_html(html);
        let styles = compute_styles(&doc, &parse_css(&doc.embedded_css()));
        let tree = layout_document(&doc, &styles, width);
        (doc, tree)
    }

    fn collect_tag<'a>(
        doc: &Document,
        tree: &'a LayoutBox,
        tag: &str,
        out: &mut Vec<&'a LayoutBox>,
    ) {
        if doc.get(tree.node).data().tag() == Some(tag) {
            out.push(tree);
        }
        for child in &tree.children {
            collect_tag(doc, child, tag, out);
        }
    }

    #[test]
    fn wraps_a_line_on_the_width() {
        let segments = vec![Segment::plain(
            NodeId::from_raw(1),
            "hello hello".to_string(),
            10.0,
            None,
            Color::BLACK,
            false,
        )];
        let (runs, height) = place_lines(&segments, 0.0, 0.0, 30.0, false);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "hello");
        assert_eq!(runs[1].text, "hello");
        assert_eq!(runs[1].y, 12.5);
        assert_eq!(height, 25.0);
    }

    #[test]
    fn hits_an_inline_text_node_before_its_block() {
        let (doc, tree) = render(
            "<style>html, body, p { margin: 0; padding: 0; font-size: 10px; }</style>
             <p><a href=\"next.html\">go</a></p>",
            100.0,
        );
        let hit = hit_test(&tree, 2.0, 2.0).unwrap();
        assert_eq!(doc.get(hit).data().text(), Some("go"));
        let parent = doc.get(hit).parent().unwrap();
        assert_eq!(doc.get(parent).data().tag(), Some("a"));
    }

    #[test]
    fn stacks_blocks_and_pads_text() {
        let (doc, tree) = render(
            "<style>
                html, body { margin: 0; padding: 0; }
                div { margin: 0; padding: 10px; font-size: 10px; background: #ff0000; }
             </style>
             <div>a</div><div>b</div>",
            100.0,
        );
        let mut divs = Vec::new();
        collect_tag(&doc, &tree, "div", &mut divs);
        assert_eq!(divs.len(), 2);
        assert_eq!(divs[0].rect.y, 0.0);
        assert_eq!(divs[1].rect.y, 32.5);
        assert_eq!(divs[0].text[0].x, 10.0);
        assert_eq!(divs[0].text[0].y, 10.0);
        assert_eq!(divs[0].background, Color::parse("#ff0000"));
    }

    #[test]
    fn viewport_lengths_follow_the_window() {
        let doc = parse_html(
            "<style>html, body, div { margin: 0; padding: 0; }</style>
             <div id=\"box\" style=\"width: 50vw; height: 25vh;\"></div>",
        );
        let styles =
            compute_styles_for_viewport(&doc, &parse_css(&doc.embedded_css()), 200.0, 80.0);
        let tree = layout_document(&doc, &styles, 200.0);
        let mut divs = Vec::new();
        collect_tag(&doc, &tree, "div", &mut divs);
        assert_eq!(divs.len(), 1);
        assert_eq!(divs[0].rect.width, 100.0);
        assert_eq!(divs[0].rect.height, 20.0);
    }

    #[test]
    fn lays_out_flex_row_children_with_gap() {
        let (doc, tree) = render(
            "<style>
                html, body { margin: 0; padding: 0; }
                .row { display: flex; gap: 10px; }
                .item { width: 40px; height: 20px; }
             </style>
             <div class=\"row\"><div class=\"item\">A</div><div class=\"item\">B</div></div>",
            200.0,
        );
        let mut divs = Vec::new();
        collect_tag(&doc, &tree, "div", &mut divs);
        assert_eq!(divs.len(), 3);
        assert_eq!(divs[1].rect.x, 0.0);
        assert_eq!(divs[2].rect.x, 50.0);
    }

    #[test]
    fn sizes_decoded_image_and_measures_proportional_text() {
        let doc = parse_html("<img id=\"logo\" src=\"logo.png\" alt=\"Logo\">");
        let styles = compute_styles(&doc, &parse_css("html, body { margin: 0; padding: 0; }"));
        let logo_id = doc.get_element_by_id("logo").unwrap();
        let mut images = HashMap::new();
        images.insert(logo_id, Image::new(64, 48));

        let tree = layout_document_with_images(&doc, &styles, 300.0, &images);
        let mut imgs = Vec::new();
        collect_tag(&doc, &tree, "img", &mut imgs);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].rect.width, 64.0);
        assert_eq!(imgs[0].rect.height, 48.0);
        assert!(imgs[0].image.is_some());

        assert!(measure_proportional("MMMM", 16.0) > measure_proportional("iiii", 16.0));
    }

    #[test]
    fn positions_out_of_flow_boxes_and_respects_z_index_and_overflow_in_hit_test() {
        let doc = parse_html(
            "<style>
                html, body { margin: 0; padding: 0; }
                #under { position: absolute; top: 10px; left: 10px; width: 60px; height: 60px; z-index: 1; }
                #over  { position: absolute; top: 10px; left: 10px; width: 60px; height: 60px; z-index: 5; }
                #clip  { width: 40px; height: 40px; overflow: hidden; }
             </style>
             <div id=\"over\"></div>
             <div id=\"under\"></div>
             <div id=\"clip\"><input id=\"field\" type=\"text\" value=\"abc\"></div>",
        );
        let styles = compute_styles(&doc, &parse_css(&doc.embedded_css()));
        let field = doc.get_element_by_id("field").unwrap();
        let over = doc.get_element_by_id("over").unwrap();
        let tree = layout_document_interactive(&doc, &styles, 300.0, &HashMap::new(), Some(field));
        assert_eq!(hit_test(&tree, 20.0, 20.0), Some(over));
    }

    #[test]
    fn lays_out_html_tables_textareas_and_select_controls() {
        let (doc, tree) = render(
            "<style>html, body, table, tr, td { margin: 0; }</style>
             <table>
               <tr><td>Cell 1</td><td>Cell 2</td></tr>
             </table>
             <textarea id=\"notes\" rows=\"4\">Hello</textarea>
             <select id=\"mode\"><option selected>Dark Mode</option></select>",
            400.0,
        );
        let mut tds = Vec::new();
        collect_tag(&doc, &tree, "td", &mut tds);
        assert_eq!(tds.len(), 2);
        assert_eq!(tds[0].rect.x, 0.0);
        assert_eq!(tds[1].rect.x, 200.0);
        assert_eq!(tds[0].rect.height, tds[1].rect.height);

        let mut textareas = Vec::new();
        collect_tag(&doc, &tree, "textarea", &mut textareas);
        assert_eq!(textareas.len(), 1);
        assert!(textareas[0].rect.height >= 4.0 * 16.0);

        let mut selects = Vec::new();
        collect_tag(&doc, &tree, "select", &mut selects);
        assert_eq!(selects.len(), 1);
        assert!(selects[0].text[0].text.contains("Dark Mode"));
    }

    #[test]
    fn resolves_percent_em_rem_and_wraps_a_flex_row_that_grows() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                html { font-size: 20px; }
                #host { width: 200px; height: 100px; background: #111111; }
                #pct { width: 50%; height: 40%; background: #ff0000; }
                #em { font-size: 1.5rem; width: 2em; height: 10px; background: #00ff00; }
                #row { display: flex; flex-wrap: wrap; width: 100px; background: #0000ff; }
                #row div { width: 40px; height: 10px; }
                #grow { display: flex; width: 100px; height: 10px; }
                #g1 { width: 20px; height: 10px; flex-grow: 1; background: #abcdef; }
                #g2 { width: 20px; height: 10px; flex-grow: 2; background: #fedcba; }
             </style>
             <div id=\"host\"><div id=\"pct\"></div><div id=\"em\"></div></div>
             <div id=\"row\"><div id=\"a\"></div><div id=\"b\"></div><div id=\"c\"></div></div>
             <div id=\"grow\"><div id=\"g1\"></div><div id=\"g2\"></div></div>",
            200.0,
        );
        let rect = |id: &str| {
            fn find<'a>(doc: &Document, tree: &'a LayoutBox, id: &str) -> Option<&'a LayoutBox> {
                if doc.get(tree.node).data().attr("id") == Some(id) {
                    return Some(tree);
                }
                for child in &tree.children {
                    if let Some(found) = find(doc, child, id) {
                        return Some(found);
                    }
                }
                None
            }
            find(&doc, &tree, id).unwrap().rect
        };
        assert_eq!(rect("pct").width, 100.0);
        assert_eq!(rect("pct").height, 40.0);
        assert_eq!(rect("pct").x, 0.0);
        assert_eq!(rect("pct").y, 0.0);
        assert_eq!(rect("em").width, 60.0);
        assert_eq!(rect("em").height, 10.0);
        assert_eq!(rect("em").y, 40.0);
        assert_eq!(rect("a").x, 0.0);
        assert_eq!(rect("b").x, 40.0);
        assert_eq!(rect("c").x, 0.0);
        assert_eq!(rect("c").y, rect("a").y + 10.0);
        assert_eq!(rect("g1").width, 40.0);
        assert_eq!(rect("g2").width, 60.0);
        assert_eq!(rect("g2").x, rect("g1").x + 40.0);
    }

    #[test]
    fn lays_out_css_grid_tracks_and_calc_lengths() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                :root { --grid-gap: 10px; }
                #grid { display: grid; grid-template-columns: 100px 1fr 2fr; gap: var(--grid-gap); width: 420px; }
                .cell { height: 30px; }
                #wide { width: calc(100% - 20px); height: 24px; }
             </style>
             <div id=\"grid\">
               <div id=\"c1\" class=\"cell\"></div>
               <div id=\"c2\" class=\"cell\"></div>
               <div id=\"c3\" class=\"cell\"></div>
               <div id=\"c4\" class=\"cell\"></div>
             </div>
             <div id=\"wide\"></div>",
            500.0,
        );
        let rect = |id: &str| {
            fn find<'a>(doc: &Document, tree: &'a LayoutBox, id: &str) -> Option<&'a LayoutBox> {
                if doc.get(tree.node).data().attr("id") == Some(id) {
                    return Some(tree);
                }
                for child in &tree.children {
                    if let Some(found) = find(doc, child, id) {
                        return Some(found);
                    }
                }
                None
            }
            find(&doc, &tree, id).unwrap().rect
        };
        // 420px total - 20px (two 10px gaps) - 100px fixed = 300px for 3fr -> 1fr = 100px, 2fr = 200px
        assert_eq!(rect("c1").x, 0.0);
        assert_eq!(rect("c1").width, 100.0);
        assert_eq!(rect("c2").x, 110.0);
        assert_eq!(rect("c2").width, 100.0);
        assert_eq!(rect("c3").x, 220.0);
        assert_eq!(rect("c3").width, 200.0);
        assert_eq!(rect("c4").x, 0.0);
        assert_eq!(rect("c4").y, 40.0);
        assert_eq!(rect("wide").width, 480.0);
    }

    /// Border box of the element whose `id` attribute is `id`.
    fn rect_of(doc: &Document, tree: &LayoutBox, id: &str) -> Rect {
        fn find<'a>(doc: &Document, tree: &'a LayoutBox, id: &str) -> Option<&'a LayoutBox> {
            if doc.get(tree.node).data().attr("id") == Some(id) {
                return Some(tree);
            }
            tree.children.iter().find_map(|child| find(doc, child, id))
        }
        find(doc, tree, id)
            .unwrap_or_else(|| panic!("no box for #{id}"))
            .rect
    }

    #[test]
    fn sizes_fixed_auto_and_flexible_grid_rows() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                #grid { display: grid; width: 110px; height: 200px; gap: 10px;
                        grid-template-columns: 50px 50px; grid-template-rows: 20px auto 1fr; }
             </style>
             <div id=\"grid\">
               <div id=\"a\" style=\"height: 5px\"></div><div id=\"b\"></div>
               <div id=\"c\" style=\"height: 30px\"></div><div id=\"d\"></div>
               <div id=\"e\" style=\"height: 12px\"></div><div id=\"f\"></div>
             </div>",
            300.0,
        );
        let r = |id: &str| rect_of(&doc, &tree, id);
        assert_eq!((r("a").y, r("a").height), (0.0, 5.0));
        assert_eq!((r("b").x, r("b").y, r("b").height), (60.0, 0.0, 20.0));
        assert_eq!((r("c").y, r("c").height), (30.0, 30.0));
        assert_eq!((r("d").y, r("d").height), (30.0, 30.0));
        // 200 - 2 gaps - 20 - 30 leaves 130 for the 1fr row.
        assert_eq!((r("e").y, r("e").height), (70.0, 12.0));
        assert_eq!((r("f").y, r("f").height), (70.0, 130.0));
        assert_eq!(r("grid").height, 200.0);
    }

    #[test]
    fn sizes_implicit_minmax_percent_and_repeated_rows() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                .g { display: grid; width: 90px; grid-template-columns: repeat(3, 30px); }
                #auto { grid-template-rows: 40px; grid-auto-rows: 25px; }
                #mm { grid-template-rows: minmax(30px, auto) minmax(30px, auto) minmax(10px, 40px); grid-template-columns: 30px; }
                #pct { height: 200px; grid-template-rows: 25% repeat(2, 15px); grid-template-columns: 30px; }
                #fr { grid-template-rows: 1fr 2fr; grid-template-columns: 30px; }
             </style>
             <div id=\"auto\" class=\"g\">
               <div id=\"a1\"></div><div></div><div></div><div id=\"a4\"></div><div></div><div></div><div id=\"a7\"></div>
             </div>
             <div id=\"mm\" class=\"g\">
               <div id=\"m1\" style=\"height: 10px\"></div><div id=\"m2\" style=\"height: 50px\"></div><div id=\"m3\"></div>
             </div>
             <div id=\"pct\" class=\"g\"><div id=\"p1\"></div><div id=\"p2\"></div><div id=\"p3\"></div></div>
             <div id=\"fr\" class=\"g\">
               <div id=\"f1\" style=\"height: 30px\"></div><div id=\"f2\" style=\"height: 20px\"></div>
             </div>",
            300.0,
        );
        let r = |id: &str| rect_of(&doc, &tree, id);
        assert_eq!((r("a1").y, r("a1").height), (0.0, 40.0));
        assert_eq!((r("a4").y, r("a4").height), (40.0, 25.0));
        assert_eq!((r("a7").y, r("a7").height), (65.0, 25.0));
        assert_eq!(r("auto").height, 90.0);

        let top = r("mm").y;
        assert_eq!((r("m1").y - top, r("m1").height), (0.0, 10.0));
        assert_eq!((r("m2").y - top, r("m2").height), (30.0, 50.0));
        // A fixed minmax with no definite height grows to its maximum.
        assert_eq!((r("m3").y - top, r("m3").height), (80.0, 40.0));
        assert_eq!(r("mm").height, 120.0);

        let top = r("pct").y;
        assert_eq!((r("p1").y - top, r("p1").height), (0.0, 50.0));
        assert_eq!((r("p2").y - top, r("p2").height), (50.0, 15.0));
        assert_eq!((r("p3").y - top, r("p3").height), (65.0, 15.0));

        // No definite height: one fr unit fits the largest item per fr.
        let top = r("fr").y;
        assert_eq!((r("f1").y - top, r("f1").height), (0.0, 30.0));
        assert_eq!((r("f2").y - top, r("f2").height), (30.0, 20.0));
        assert_eq!(r("fr").height, 90.0);
    }

    #[test]
    fn grid_columns_share_space_with_minmax_floors() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                #g { display: grid; width: 200px; grid-template-columns: minmax(120px, 1fr) 1fr; }
                #h { display: grid; width: 200px; grid-template-columns: minmax(20px, 60px) auto; }
             </style>
             <div id=\"g\"><div id=\"g1\"></div><div id=\"g2\"></div></div>
             <div id=\"h\"><div id=\"h1\"></div><div id=\"h2\"></div></div>",
            300.0,
        );
        let r = |id: &str| rect_of(&doc, &tree, id);
        // 1fr would be 100px, below the 120px floor, so the floor holds and
        // the other column takes the rest.
        assert_eq!((r("g1").x, r("g1").width), (0.0, 120.0));
        assert_eq!((r("g2").x, r("g2").width), (120.0, 80.0));
        // The fixed minmax grows to 60px; `auto` acts as 1fr on columns.
        assert_eq!((r("h1").x, r("h1").width), (0.0, 60.0));
        assert_eq!((r("h2").x, r("h2").width), (60.0, 140.0));
    }

    #[test]
    fn places_spanning_grid_items_and_auto_flows_around_them() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                #g { display: grid; width: 320px; gap: 10px;
                     grid-template-columns: repeat(4, 1fr); grid-auto-rows: 20px; }
                #wide { grid-column: span 2; }
                #tall { grid-row: span 2; }
                #pin { grid-area: 3 / 1 / 4 / -1; }
                #late { grid-column: 2; }
                #lines { grid-row: 5; grid-column: 4 / 2; }
             </style>
             <div id=\"g\">
               <div id=\"wide\"></div><div id=\"tall\"></div><div id=\"a\"></div>
               <div id=\"b\"></div><div id=\"c\"></div>
               <div id=\"pin\"></div><div id=\"late\"></div><div id=\"lines\"></div>
               <div id=\"d\"></div>
             </div>",
            400.0,
        );
        let r = |id: &str| rect_of(&doc, &tree, id);
        // Four 1fr columns of 72.5px with 10px gaps: lines at 0, 82.5, 165, 247.5.
        assert_eq!(
            (r("wide").x, r("wide").y, r("wide").width),
            (0.0, 0.0, 155.0)
        );
        assert_eq!(
            (r("tall").x, r("tall").y, r("tall").height),
            (165.0, 0.0, 50.0)
        );
        assert_eq!((r("a").x, r("a").y), (247.5, 0.0));
        // Row two has the tall item in column three, so b and c take 1, 2, then 4.
        assert_eq!((r("b").x, r("b").y), (0.0, 30.0));
        assert_eq!((r("c").x, r("c").y), (82.5, 30.0));
        assert_eq!((r("pin").x, r("pin").y, r("pin").width), (0.0, 60.0, 320.0));
        // Column 2 is behind the cursor after row two, so it drops to row four.
        assert_eq!((r("late").x, r("late").y), (82.5, 90.0));
        assert_eq!(
            (r("lines").x, r("lines").y, r("lines").width),
            (82.5, 120.0, 155.0)
        );
        // `d` continues after `late` on row four.
        assert_eq!((r("d").x, r("d").y), (165.0, 90.0));
        assert_eq!(r("g").height, 140.0);
    }

    #[test]
    fn spanning_items_grow_auto_rows_and_add_implicit_columns() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                #g { display: grid; width: 100px; grid-template-columns: 50px 50px; }
                #big { grid-row: 1 / span 2; height: 60px; }
                #s1 { height: 10px; }
                #s2 { height: 10px; }
                #h { display: grid; width: 90px; grid-template-columns: 30px; }
                #span3 { grid-column: span 3; }
             </style>
             <div id=\"g\"><div id=\"big\"></div><div id=\"s1\"></div><div id=\"s2\"></div></div>
             <div id=\"h\"><div id=\"span3\"></div><div id=\"after\"></div></div>",
            200.0,
        );
        let r = |id: &str| rect_of(&doc, &tree, id);
        // Rows start at 10px each; the 60px item adds 20px to each auto row it spans.
        assert_eq!((r("s1").x, r("s1").y), (50.0, 0.0));
        assert_eq!((r("s2").x, r("s2").y), (50.0, 30.0));
        assert_eq!(r("g").height, 60.0);
        // Two implicit auto columns share the 60px left after the 30px column.
        assert_eq!((r("span3").x, r("span3").width), (0.0, 90.0));
        // The empty first row is 0px tall, so the next item starts at the grid top.
        assert_eq!((r("after").x, r("after").y), (0.0, r("h").y));
        assert_eq!(r("after").width, 30.0);
    }

    #[test]
    fn resolves_ch_units_from_the_zero_glyph() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                #box { font-size: 20px; width: 10ch; height: 2ch; }
                #cap { font-size: 10px; width: 300px; max-width: 12ch; padding-left: 4px; }
                #rel { position: relative; left: 3ch; height: 1px; }
             </style>
             <div id=\"box\"></div><div id=\"cap\"></div><div id=\"rel\"></div>",
            400.0,
        );
        let r = |id: &str| rect_of(&doc, &tree, id);
        let ch20 = ch_width(20.0);
        let ch16 = ch_width(16.0);
        assert_eq!((r("box").width, r("box").height), (10.0 * ch20, 2.0 * ch20));
        assert_eq!(r("cap").width, 12.0 * ch_width(10.0) + 4.0);
        assert_eq!(r("rel").x, 3.0 * ch16);
        // Without font metrics, one ch is the layout advance: half the font size.
        assert_eq!(ch_width_with(None, 20.0), 10.0);
        assert_eq!(ch_width_with(None, 20.0), measure("0", 20.0));
        if system_font().is_none() {
            assert_eq!(ch20, 10.0);
        }
    }

    fn find_box<'a>(doc: &Document, tree: &'a LayoutBox, id: &str) -> &'a LayoutBox {
        fn find<'a>(doc: &Document, tree: &'a LayoutBox, id: &str) -> Option<&'a LayoutBox> {
            if doc.get(tree.node).data().attr("id") == Some(id) {
                return Some(tree);
            }
            tree.children.iter().find_map(|child| find(doc, child, id))
        }
        find(doc, tree, id).unwrap_or_else(|| panic!("no box for #{id}"))
    }

    #[test]
    fn sticky_boxes_follow_the_viewport_inside_their_containing_block() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                #wrap { height: 300px; }
                #bar { position: sticky; top: 10px; height: 20px; }
                #low { position: sticky; bottom: 0; height: 20px; }
             </style>
             <div id=\"wrap\">
               <div id=\"pre\" style=\"height: 50px\"></div>
               <div id=\"bar\"></div>
               <div id=\"filler\" style=\"height: 100px\"></div>
               <div id=\"low\"></div>
             </div>
             <div style=\"height: 400px\"></div>",
            200.0,
        );
        let bar = find_box(&doc, &tree, "bar");
        assert_eq!(bar.rect.y, 50.0);
        let sticky = bar.sticky.expect("bar is sticky against the viewport");
        assert_eq!(sticky.top, Some(10.0));
        assert_eq!(sticky.bottom, None);
        assert_eq!(sticky.containing.height, 300.0);
        let port = |scroll: f32| Rect {
            x: 0.0,
            y: scroll,
            width: 200.0,
            height: 100.0,
        };
        assert_eq!(sticky.offset(port(0.0)), (0.0, 0.0));
        assert_eq!(sticky.offset(port(100.0)), (0.0, 60.0));
        // The bar stops at the bottom of #wrap: 300 - 20 - 50.
        assert_eq!(sticky.offset(port(400.0)), (0.0, 230.0));

        // #low sits at 170..190 and sticks to the viewport bottom.
        let low = find_box(&doc, &tree, "low").sticky.unwrap();
        assert_eq!(low.offset(port(0.0)), (0.0, -90.0));
        assert_eq!(low.offset(port(100.0)), (0.0, 0.0));

        let filler = doc.get_element_by_id("filler").unwrap();
        let bar_id = doc.get_element_by_id("bar").unwrap();
        // At scroll 100 the bar is drawn at 110..130, over the filler.
        assert_eq!(hit_test(&tree, 5.0, 115.0), Some(filler));
        assert_eq!(
            hit_test_scrolled(&tree, 5.0, 115.0, port(100.0)),
            Some(bar_id)
        );
        assert_eq!(
            hit_test_scrolled(&tree, 5.0, 55.0, port(100.0)),
            Some(doc.get_element_by_id("wrap").unwrap())
        );
    }

    #[test]
    fn sticky_inside_a_clip_is_resolved_in_layout() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                #clip { overflow: hidden; height: 100px; padding: 4px; }
                #s { position: sticky; top: 15px; height: 10px; }
                #t { position: sticky; top: 300px; height: 10px; }
             </style>
             <div id=\"clip\"><div id=\"s\"></div><div id=\"t\"></div></div>",
            200.0,
        );
        let s = find_box(&doc, &tree, "s");
        assert!(s.sticky.is_none());
        // The clip's padding box starts at 0, so top: 15px moves s from 4 to 15.
        assert_eq!(s.rect.y, 15.0);
        // t would need 300px but stops at the bottom of the 100px content box.
        let t = find_box(&doc, &tree, "t");
        assert_eq!(t.rect.y, 94.0);
    }

    #[test]
    fn sticky_offset_handles_left_right_and_conflicts() {
        let c = StickyConstraint {
            rect: Rect {
                x: 50.0,
                y: 0.0,
                width: 20.0,
                height: 10.0,
            },
            margin: Edge::all(2.0),
            containing: Rect {
                x: 0.0,
                y: 0.0,
                width: 200.0,
                height: 10.0,
            },
            top: None,
            right: Some(5.0),
            bottom: None,
            left: Some(5.0),
        };
        let port = |x: f32, w: f32| Rect {
            x,
            y: 0.0,
            width: w,
            height: 50.0,
        };
        assert_eq!(c.offset(port(100.0, 100.0)), (55.0, 0.0));
        // Right edge 70 is past 60 - 5, so the box moves left by 15.
        assert_eq!(c.offset(port(0.0, 60.0)), (-15.0, 0.0));
        // A port too narrow for both insets: left wins.
        assert_eq!(c.offset(port(48.0, 10.0)), (3.0, 0.0));
        // Never past the containing block, margins included.
        assert_eq!(c.offset(port(500.0, 100.0)), (128.0, 0.0));
    }

    #[test]
    fn builds_box_transforms_around_the_origin() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                #move { width: 100px; height: 50px; transform: translate(10px, 50%) scale(2, 0.5); }
                #corner { width: 40px; height: 20px; transform-origin: left top;
                          transform: matrix(1, 0, 0, 1, 3, 4) scaleX(-1); }
                #inline { transform: scale(3); }
                #rel { position: relative; top: 10px; }
                #inner { width: 20px; height: 20px; transform: scale(2); }
             </style>
             <div id=\"move\"></div><div id=\"corner\"></div><span id=\"inline\">x</span>
             <div id=\"rel\"><div id=\"inner\"></div></div>",
            200.0,
        );
        // Origin (50, 25): move by (10, 25), then scale about the origin.
        assert_eq!(
            find_box(&doc, &tree, "move").transform,
            Some(Transform2D {
                a: 2.0,
                b: 0.0,
                c: 0.0,
                d: 0.5,
                e: -40.0,
                f: 37.5,
            })
        );
        // Origin (0, 50): mirror, then move by (3, 4).
        assert_eq!(
            find_box(&doc, &tree, "corner").transform,
            Some(Transform2D {
                a: -1.0,
                b: 0.0,
                c: 0.0,
                d: 1.0,
                e: 3.0,
                f: 4.0,
            })
        );
        // Inline boxes and text are not transformed.
        let inline = doc.get_element_by_id("inline").unwrap();
        let mut stack = vec![&tree];
        while let Some(b) = stack.pop() {
            assert!(b.node != inline || b.transform.is_none());
            if doc.get(b.node).data().tag().is_none() {
                assert!(b.transform.is_none());
            }
            stack.extend(b.children.iter());
        }
        // The relative offset moved the box after layout; the origin moved too.
        let inner = find_box(&doc, &tree, "inner");
        let center = (inner.rect.x + 10.0, inner.rect.y + 10.0);
        let m = inner.transform.unwrap();
        assert_eq!(m.apply(center.0, center.1), center);
        assert_eq!(
            m.apply(inner.rect.x, inner.rect.y),
            (inner.rect.x - 10.0, inner.rect.y - 10.0)
        );
    }

    #[test]
    fn hit_tests_transformed_boxes_where_they_are_drawn() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                #pad { height: 50px; }
                #bar { width: 100px; height: 20px; transform: rotate(90deg); }
                #flat { width: 30px; height: 30px; transform: scale(0); }
                #ring { width: 40px; height: 40px; overflow: hidden;
                        transform: translate(100px, 0); }
                #in { width: 80px; height: 80px; }
             </style>
             <div id=\"pad\"></div><div id=\"bar\"></div><div id=\"flat\"></div>
             <div id=\"ring\"><div id=\"in\"></div></div>",
            200.0,
        );
        let id = |name: &str| doc.get_element_by_id(name);
        // #bar is 0..100 x 50..70 turned about (50, 60): now 40..60 x 10..110.
        assert_eq!(hit_test(&tree, 50.0, 15.0), id("bar"));
        assert_eq!(hit_test(&tree, 50.0, 105.0), id("bar"));
        assert_ne!(hit_test(&tree, 5.0, 60.0), id("bar"));
        assert_ne!(hit_test(&tree, 95.0, 60.0), id("bar"));
        // A flattened box is never hit.
        let flat = find_box(&doc, &tree, "flat");
        assert_ne!(
            hit_test(&tree, flat.rect.x + 15.0, flat.rect.y + 15.0),
            id("flat")
        );
        // #ring moved 100px right; its own clip moved with it.
        let ring = find_box(&doc, &tree, "ring");
        assert_eq!(hit_test(&tree, 110.0, ring.rect.y + 5.0), id("in"));
        assert_ne!(hit_test(&tree, 10.0, ring.rect.y + 5.0), id("in"));
        assert_ne!(hit_test(&tree, 150.0, ring.rect.y + 5.0), id("in"));
    }

    #[test]
    fn grid_auto_fill_and_canvas_surface_layout() {
        let (mut doc, tree) = render(
            "<style>
                html, body { margin: 0; padding: 0; }
                #grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(100px, 1fr)); width: 320px; }
                .cell { height: 24px; }
             </style>
             <div id=\"grid\">
                <div id=\"c1\" class=\"cell\"></div>
                <div id=\"c2\" class=\"cell\"></div>
                <div id=\"c3\" class=\"cell\"></div>
             </div>
             <canvas id=\"cv\" width=\"64\" height=\"32\"></canvas>",
            400.0,
        );
        let c1 = find_box(&doc, &tree, "c1");
        let c2 = find_box(&doc, &tree, "c2");
        let c3 = find_box(&doc, &tree, "c3");
        // 320px fits 3 columns of minmax(100px, 1fr) on the same row.
        assert_eq!(c1.rect.y, c2.rect.y);
        assert_eq!(c2.rect.y, c3.rect.y);
        assert!(c2.rect.x > c1.rect.x);
        assert!(c3.rect.x > c2.rect.x);

        let cv_id = doc.get_element_by_id("cv").unwrap();
        doc.canvas_fill_rect(cv_id, 0, 0, 64, 32, [255, 0, 0, 255]);
        let styles = compute_styles(&doc, &parse_css(&doc.embedded_css()));
        let tree2 = layout_document(&doc, &styles, 400.0);
        let cv_box = find_box(&doc, &tree2, "cv");
        assert_eq!(cv_box.rect.width, 64.0);
        assert_eq!(cv_box.rect.height, 32.0);
        let img = cv_box
            .image
            .as_ref()
            .expect("canvas surface image attached");
        assert_eq!(img.width, 64);
        assert_eq!(img.height, 32);
    }

    #[test]
    fn auto_margins_inline_blocks_border_box_and_column_flex_centering() {
        let (doc, tree) = render(
            "<style>
                html, body { margin: 0; padding: 0; }
                #centered { width: 200px; margin: 0 auto; height: 24px; }
                #row { text-align: center; width: 400px; }
                .ib { display: inline-block; width: 60px; height: 20px; margin: 0 5px; }
                #bbox { box-sizing: border-box; width: 180px; min-height: 40px; padding: 10px; border: 2px solid #111; }
                #col { display: flex; flex-direction: column; align-items: center; width: 400px; }
                #col_child { width: 120px; height: 20px; }
                #hidden-box { visibility: hidden; width: 100px; height: 30px; background: #ff0000; }
             </style>
             <div id=\"centered\"></div>
             <div id=\"row\"><span id=\"b1\" class=\"ib\"></span><span id=\"b2\" class=\"ib\"></span></div>
             <div id=\"bbox\"></div>
             <div id=\"col\"><div id=\"col_child\"></div></div>
             <div id=\"hidden-box\">Hidden text</div>",
            400.0,
        );
        let centered = find_box(&doc, &tree, "centered");
        assert_eq!(centered.rect.x, 100.0);
        assert_eq!(centered.rect.width, 200.0);

        let b1 = find_box(&doc, &tree, "b1");
        let b2 = find_box(&doc, &tree, "b2");
        assert_eq!(b1.rect.y, b2.rect.y);
        assert!(b2.rect.x > b1.rect.x);
        // Total row width = (5 + 60 + 5) * 2 = 140; centered in 400 -> starts at 130 + 5 = 135.
        assert_eq!(b1.rect.x, 135.0);

        let bbox = find_box(&doc, &tree, "bbox");
        assert_eq!(bbox.rect.width, 180.0);
        assert_eq!(bbox.rect.height, 40.0);

        let col_child = find_box(&doc, &tree, "col_child");
        assert_eq!(col_child.rect.x, 140.0);

        let hidden_box = find_box(&doc, &tree, "hidden-box");
        assert_eq!(hidden_box.rect.width, 100.0);
        assert_eq!(hidden_box.rect.height, 30.0);
        assert!(hidden_box.background.is_none());
        assert!(hidden_box.text.is_empty());
    }

    #[test]
    fn rasterizes_inline_svg_icons_decorated_inlines_line_height_and_table_colspan() {
        let (doc, tree) = render(
            "<style>
                html, body, table, tr, td { margin: 0; padding: 0; border: none; }
                #tall_p { font-size: 16px; line-height: 30px; margin: 0; }
                .pill { background: #1e40af; color: #ffffff; padding: 4px 10px; border-radius: 6px; }
                #icon { color: #dc2626; }
             </style>
             <svg id=\"icon\" viewBox=\"0 0 24 24\" width=\"24\" height=\"24\">
               <title>Hidden SVG Title</title>
               <circle cx=\"12\" cy=\"12\" r=\"9\" fill=\"currentColor\"></circle>
               <path d=\"M8 12 L11 15 L16 9\" stroke=\"#ffffff\" stroke-width=\"2\" fill=\"none\"></path>
             </svg>
             <p id=\"tall_p\">Line one<br>Line two</p>
             <div><span id=\"badge\" class=\"pill\">Badge</span><code id=\"code_el\">fn main()</code></div>
             <table>
               <tr><td id=\"span2\" colspan=\"2\">Wide</td><td id=\"col3\">Narrow</td></tr>
             </table>",
            300.0,
        );
        let icon = find_box(&doc, &tree, "icon");
        assert_eq!(icon.rect.width, 24.0);
        assert_eq!(icon.rect.height, 24.0);
        assert!(icon.text.is_empty());
        let svg_img = icon.image.as_ref().expect("svg rasterized to Image");
        assert_eq!((svg_img.width, svg_img.height), (24, 24));
        assert!(svg_img.pixels.as_chunks::<4>().0.iter().any(|px| px[3] > 0));

        let tall_p = find_box(&doc, &tree, "tall_p");
        assert_eq!(tall_p.rect.height, 60.0);

        let badge = find_box(&doc, &tree, "badge");
        assert!(badge.background.is_some());
        assert!(badge.rect.width > 20.0);
        let code_el = find_box(&doc, &tree, "code_el");
        assert!(code_el.background.is_some());

        let span2 = find_box(&doc, &tree, "span2");
        let col3 = find_box(&doc, &tree, "col3");
        assert_eq!(span2.rect.width, 200.0);
        assert_eq!(col3.rect.x, 200.0);
        assert_eq!(col3.rect.width, 100.0);
    }

    #[test]
    fn preserves_multiline_pre_indentation_textarea_summary_markers_and_radio_buttons() {
        let (doc, tree) = render(
            "<pre id=\"code_block\">fn main() {\n    let x = 1;\n}</pre>
             <textarea id=\"ta\">line 1\n  line 2</textarea>
             <details id=\"det_closed\"><summary id=\"sum_closed\">More Info</summary><p>Hidden</p></details>
             <details id=\"det_open\" open><summary id=\"sum_open\">Expanded</summary><p>Shown</p></details>
             <input id=\"rad_on\" type=\"radio\" checked>
             <input id=\"rad_off\" type=\"radio\">",
            400.0,
        );
        let code_block = find_box(&doc, &tree, "code_block");
        assert_eq!(code_block.text.len(), 3);
        assert_eq!(code_block.text[0].text, "fn main() {");
        assert_eq!(code_block.text[1].text, "    let x = 1;");
        assert_eq!(code_block.text[2].text, "}");
        assert!(code_block.text[1].y > code_block.text[0].y);
        assert!(code_block.text[2].y > code_block.text[1].y);

        let ta = find_box(&doc, &tree, "ta");
        assert_eq!(ta.text.len(), 2);
        assert_eq!(ta.text[0].text, "line 1");
        assert_eq!(ta.text[1].text, "  line 2");

        let sum_closed = find_box(&doc, &tree, "sum_closed");
        let closed_str: String = sum_closed.text.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(closed_str, "> More Info");

        let sum_open = find_box(&doc, &tree, "sum_open");
        let open_str: String = sum_open.text.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(open_str, "v Expanded");

        let rad_on = find_box(&doc, &tree, "rad_on");
        assert_eq!(rad_on.text[0].text, "(o)");
        let rad_off = find_box(&doc, &tree, "rad_off");
        assert_eq!(rad_off.text[0].text, "( )");
    }

    #[test]
    fn renders_mid_string_input_caret_and_range_progress_meter_bitmaps() {
        let mut doc = parse_html(
            "<html><body>
               <input id=\"txt\" type=\"text\" value=\"abcdef\">
               <input id=\"rng\" type=\"range\" min=\"0\" max=\"100\" value=\"75\">
               <progress id=\"prog\" value=\"40\" max=\"100\">40%</progress>
               <meter id=\"mtr\" min=\"0\" max=\"10\" value=\"8\">8/10</meter>
             </body></html>",
        );
        let txt_id = doc.get_element_by_id("txt").unwrap();
        doc.set_input_cursor(txt_id, 3);
        let styles = compute_styles(&doc, &parse_css(&doc.embedded_css()));
        let tree = layout_document_interactive(&doc, &styles, 400.0, &HashMap::new(), Some(txt_id));

        let txt_box = find_box(&doc, &tree, "txt");
        assert_eq!(txt_box.text[0].text, "abc|def");
        assert_eq!(tree.find_node_rect(txt_id), Some(txt_box.rect));

        let rng_box = find_box(&doc, &tree, "rng");
        assert!(rng_box.image.is_some());
        assert!(rng_box.rect.width >= 60.0);

        let prog_box = find_box(&doc, &tree, "prog");
        assert!(prog_box.image.is_some());
        assert!(prog_box.text.is_empty());

        let mtr_box = find_box(&doc, &tree, "mtr");
        assert!(mtr_box.image.is_some());
        assert!(mtr_box.text.is_empty());
    }

    #[test]
    fn resolves_aspect_ratio_dimensions_and_collects_text_runs_in_rect() {
        let (doc, tree) = render(
            "<style>
                html, body { margin: 0; padding: 0; }
                #video_card { width: 320px; aspect-ratio: 16 / 9; }
                #avatar { height: 64px; aspect-ratio: 1 / 1; }
                p { margin: 0; font-size: 16px; }
             </style>
             <div id=\"video_card\"></div>
             <div id=\"avatar\"></div>
             <p id=\"p1\">Alpha line</p>
             <p id=\"p2\">Beta line</p>",
            400.0,
        );
        let vc = find_box(&doc, &tree, "video_card");
        assert_eq!(vc.rect.width, 320.0);
        assert!((vc.rect.height - 180.0).abs() < 0.5);

        let av = find_box(&doc, &tree, "avatar");
        assert_eq!(av.rect.width, 64.0);
        assert_eq!(av.rect.height, 64.0);

        let p1 = find_box(&doc, &tree, "p1");
        let p2 = find_box(&doc, &tree, "p2");
        let mut runs = Vec::new();
        tree.collect_text_runs_in_rect(
            Rect {
                x: 0.0,
                y: p1.rect.y,
                width: 200.0,
                height: (p2.rect.y + p2.rect.height) - p1.rect.y,
            },
            &mut runs,
        );
        let joined: Vec<&str> = runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(joined, vec!["Alpha line", "Beta line"]);
    }

    #[test]
    fn lays_out_flex_directions_wrap_reverse_order_freeze_loop_align_content_and_baseline() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                #rr { display: flex; flex-direction: row-reverse; width: 200px; }
                #rr div { height: 20px; }
                #r1 { width: 40px; order: 2; }
                #r2 { width: 60px; order: 1; }
                #freeze { display: flex; width: 300px; }
                #f1 { width: 200px; min-width: 170px; flex-shrink: 1; height: 20px; }
                #f2 { width: 200px; flex-shrink: 1; height: 20px; }
                #wraprev { display: flex; flex-flow: row wrap-reverse; align-content: space-between; width: 100px; height: 100px; }
                #wraprev div { width: 60px; height: 20px; }
                #base { display: flex; align-items: baseline; width: 200px; }
                #b_big { font-size: 30px; width: 60px; height: 40px; }
                #b_small { font-size: 10px; width: 60px; height: 20px; }
                #colrev { display: flex; flex-direction: column-reverse; width: 100px; height: 100px; }
                #c1 { width: 50px; height: 20px; }
                #c2 { width: 50px; height: 30px; margin-top: auto; }
             </style>
             <div id=\"rr\"><div id=\"r1\"></div><div id=\"r2\"></div></div>
             <div id=\"freeze\"><div id=\"f1\"></div><div id=\"f2\"></div></div>
             <div id=\"wraprev\"><div id=\"w1\"></div><div id=\"w2\"></div></div>
             <div id=\"base\"><div id=\"b_big\">Big</div><div id=\"b_small\">Sm</div></div>
             <div id=\"colrev\"><div id=\"c1\"></div><div id=\"c2\"></div></div>",
            400.0,
        );
        let r = |id: &str| rect_of(&doc, &tree, id);

        // #r2 has order 1, #r1 has order 2; in row-reverse, #r2 is at the right edge (140..200) and #r1 is left of it (100..140).
        assert_eq!((r("r2").x, r("r2").width), (140.0, 60.0));
        assert_eq!((r("r1").x, r("r1").width), (100.0, 40.0));

        // Freeze loop: #f1 clamps at min-width 170px, #f2 shrinks from 200px to 130px.
        assert_eq!(r("f1").width, 170.0);
        assert_eq!(r("f2").width, 130.0);
        assert_eq!(r("f2").x, 170.0);

        // wrap-reverse + align-content: space-between in 100px height -> line 1 (w1) at bottom (+80), line 2 (w2) at top (+0).
        let wr_top = r("wraprev").y;
        assert_eq!(r("w2").y - wr_top, 0.0);
        assert_eq!(r("w1").y - wr_top, 80.0);

        // Baseline alignment: #b_big has 30px font (baseline 24px), #b_small has 10px font (baseline 8px) -> shifted down by 16px.
        let base_top = r("base").y;
        assert_eq!(r("b_big").y - base_top, 0.0);
        assert!((r("b_small").y - base_top - 16.0).abs() < 0.5);

        // column-reverse with margin-top: auto on #c2: #c1 sits at bottom (y + 80), #c2 sits at top (y + 50 after 50px auto margin? Wait: c2 has margin-top: auto, so auto margin is above c2 pushing c2 down to 50..80!).
        let cr_top = r("colrev").y;
        assert_eq!(r("c1").y - cr_top, 80.0);
        assert_eq!(r("c2").y - cr_top, 50.0);
    }

    #[test]
    fn lays_out_named_grid_areas_dense_auto_flow_intrinsic_tracks_and_box_alignment() {
        let (doc, tree) = render(
            "<style>
                html, body, div { margin: 0; padding: 0; }
                #areas {
                    display: grid;
                    width: 300px;
                    grid-template-columns: 100px 200px;
                    grid-template-rows: 40px 80px;
                    grid-template-areas: \"hdr hdr\" \"nav main\";
                }
                #hdr { grid-area: hdr; }
                #nav { grid-area: nav; }
                #main { grid-area: main; }
                #dense {
                    display: grid;
                    width: 200px;
                    grid-template-columns: 100px 100px;
                    grid-auto-rows: 20px;
                    grid-auto-flow: row dense;
                }
                #d1 { grid-column: 2; }
                #d2 { grid-column: span 2; }
                #d3 { }
                #colflow {
                    display: grid;
                    width: 300px;
                    grid-template-rows: 30px 30px;
                    grid-template-columns: 50px;
                    grid-auto-columns: 40px 80px;
                    grid-auto-flow: column;
                }
                #intr {
                    display: grid;
                    width: 300px;
                    grid-template-columns: min-content max-content fit-content(70px);
                }
                #i1 { width: 35px; height: 20px; }
                #i2 { width: 65px; height: 20px; }
                #i3 { width: 120px; height: 20px; }
                #align_grid {
                    display: grid;
                    width: 200px;
                    height: 100px;
                    grid-template-columns: 100px 100px;
                    grid-template-rows: 60px;
                    place-items: center end;
                    align-content: end;
                }
                #ag1 { width: 40px; height: 20px; }
                #ag2 { width: 40px; height: 20px; margin: auto; }
             </style>
             <div id=\"areas\"><div id=\"main\"></div><div id=\"hdr\"></div><div id=\"nav\"></div></div>
             <div id=\"dense\"><div id=\"d1\"></div><div id=\"d2\"></div><div id=\"d3\"></div></div>
             <div id=\"colflow\"><div id=\"cf1\"></div><div id=\"cf2\"></div><div id=\"cf3\"></div><div id=\"cf4\"></div><div id=\"cf5\"></div></div>
             <div id=\"intr\"><div id=\"i1\"></div><div id=\"i2\"></div><div id=\"i3\"></div></div>
             <div id=\"align_grid\"><div id=\"ag1\"></div><div id=\"ag2\"></div></div>",
            400.0,
        );
        let r = |id: &str| rect_of(&doc, &tree, id);

        // Named grid areas: hdr spans both columns on row 0 (300x40), nav is (0, 40, 100x80), main is (100, 40, 200x80).
        let ar_top = r("areas").y;
        assert_eq!(
            (
                r("hdr").x,
                r("hdr").y - ar_top,
                r("hdr").width,
                r("hdr").height
            ),
            (0.0, 0.0, 300.0, 40.0)
        );
        assert_eq!(
            (
                r("nav").x,
                r("nav").y - ar_top,
                r("nav").width,
                r("nav").height
            ),
            (0.0, 40.0, 100.0, 80.0)
        );
        assert_eq!(
            (
                r("main").x,
                r("main").y - ar_top,
                r("main").width,
                r("main").height
            ),
            (100.0, 40.0, 200.0, 80.0)
        );

        // Dense row auto-flow: #d1 takes (row 0, col 1), #d2 spans 2 cols on row 1, and #d3 backfills (row 0, col 0)!
        let dn_top = r("dense").y;
        assert_eq!((r("d1").x, r("d1").y - dn_top), (100.0, 0.0));
        assert_eq!(
            (r("d2").x, r("d2").y - dn_top, r("d2").width),
            (0.0, 20.0, 200.0)
        );
        assert_eq!((r("d3").x, r("d3").y - dn_top), (0.0, 0.0));

        // Column auto-flow + grid-auto-columns: 40px 80px cycling after 50px explicit column.
        let cf_top = r("colflow").y;
        assert_eq!(
            (r("cf1").x, r("cf1").y - cf_top, r("cf1").width),
            (0.0, 0.0, 50.0)
        );
        assert_eq!(
            (r("cf2").x, r("cf2").y - cf_top, r("cf2").width),
            (0.0, 30.0, 50.0)
        );
        assert_eq!(
            (r("cf3").x, r("cf3").y - cf_top, r("cf3").width),
            (50.0, 0.0, 40.0)
        );
        assert_eq!(
            (r("cf4").x, r("cf4").y - cf_top, r("cf4").width),
            (50.0, 30.0, 40.0)
        );
        assert_eq!(
            (r("cf5").x, r("cf5").y - cf_top, r("cf5").width),
            (90.0, 0.0, 80.0)
        );

        // Intrinsic tracks: min-content (35px), max-content (65px), fit-content(70px).
        assert_eq!(r("i1").width, 35.0);
        assert_eq!((r("i2").x, r("i2").width), (35.0, 65.0));
        assert_eq!(r("i3").x, 100.0);

        // Grid box alignment: align-content: end pushes the 60px row to y + 40 in 100px grid.
        // place-items: center end -> align-items: center (dy = +20), justify-items: end (dx = +60).
        // #ag2 has margin: auto -> centered both horizontally (x = 100 + 30 = 130) and vertically (dy = +20).
        let ag_top = r("align_grid").y;
        assert_eq!((r("ag1").x, r("ag1").y - ag_top), (60.0, 60.0));
        assert_eq!((r("ag2").x, r("ag2").y - ag_top), (130.0, 60.0));
    }

    #[test]
    fn lays_out_pseudo_elements_ellipsis_radial_gradient_universal_units_and_table_rowspan() {
        let (doc, tree) = render(
            "<style>
                html, body { margin: 0; padding: 0; font-size: 16px; }
                #badge {
                    font-size: 125%;
                    padding: 1rem 2em;
                    margin: calc(10px + 6px);
                    border: medium solid #112233;
                    background: radial-gradient(circle, #ff0000, #0000ff);
                }
                #badge::before { content: \"[\"; }
                #badge::after { content: \"] (\" attr(data-count) \")\"; }
                #card::after {
                    content: \"\";
                    display: block;
                    width: 40px;
                    height: 8px;
                    background: #00ff00;
                }
                #clip {
                    width: 40px;
                    white-space: nowrap;
                    overflow: hidden;
                    text-overflow: ellipsis;
                    font-size: 10px;
                }
                table { width: 200px; }
             </style>
             <div id=\"badge\" data-count=\"7\">Alerts</div>
             <div id=\"card\">Header</div>
             <div id=\"clip\">abcdefghijklmnop</div>
             <table>
                <tr><td id=\"r0c0\" rowspan=\"2\" style=\"width: 80px\">Tall</td><td id=\"r0c1\">Top</td></tr>
                <tr><td id=\"r1c1\">Bottom</td></tr>
             </table>",
            400.0,
        );
        let badge_box = find_box(&doc, &tree, "badge");
        // font-size: 125% of 16px = 20px; padding: 1rem (16px) top/bottom, 2em (40px) left/right; border: medium (3px).
        assert_eq!(badge_box.border.left, 3.0);
        assert!(badge_box.background_gradient.is_some());
        let badge_text: String = badge_box.text.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(badge_text, "[Alerts] (7)");

        let card_box = find_box(&doc, &tree, "card");
        assert_eq!(card_box.children.len(), 1);
        assert_eq!(card_box.children[0].rect.width, 40.0);
        assert_eq!(card_box.children[0].rect.height, 8.0);

        let clip_box = find_box(&doc, &tree, "clip");
        assert_eq!(clip_box.text.len(), 1);
        assert!(clip_box.text[0].text.ends_with('…'));
        assert!(clip_box.text[0].width <= 40.01);

        let r0c0 = find_box(&doc, &tree, "r0c0");
        let r0c1 = find_box(&doc, &tree, "r0c1");
        let r1c1 = find_box(&doc, &tree, "r1c1");
        assert_eq!(r0c0.rect.width, 80.0);
        assert_eq!(r0c1.rect.width, 120.0);
        assert_eq!(r1c1.rect.x, r0c1.rect.x);
        assert_eq!(r1c1.rect.width, 120.0);
        assert!(r0c0.rect.height >= r0c1.rect.height + r1c1.rect.height);
    }

    #[test]
    fn lays_out_text_transform_letter_spacing_line_through_and_scrollable_overflow() {
        let mut doc = parse_html(
            "<style>
                html, body { margin: 0; padding: 0; }
                #caps { text-transform: capitalize; letter-spacing: 2px; font-size: 10px; }
                #scroll { overflow: auto; width: 120px; height: 50px; border: 2px solid #222; }
                #inner { height: 140px; }
             </style>
             <p id=\"caps\">hello world <del id=\"del\">old</del></p>
             <div id=\"scroll\"><div id=\"inner\">Content</div></div>",
        );
        let scroll_id = doc.get_element_by_id("scroll").unwrap();
        doc.set_node_scroll_offset(scroll_id, 0.0, 30.0);
        let styles = compute_styles(&doc, &parse_css(&doc.embedded_css()));
        let tree = layout_document(&doc, &styles, 300.0);

        let caps = find_box(&doc, &tree, "caps");
        let combined: String = caps.text.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(combined, "Hello World Old");
        assert!(caps.text.iter().any(|r| r.line_through));

        let scroll_box = find_box(&doc, &tree, "scroll");
        assert!(scroll_box.scrollable_overflow);
        assert_eq!(scroll_box.scroll_offset, (0.0, 30.0));
        assert!(scroll_box.scroll_size.1 >= 140.0);
        let inner_box = find_box(&doc, &tree, "inner");
        assert!((inner_box.rect.y - (scroll_box.rect.y + 2.0 - 30.0)).abs() < 0.5);
        assert!(
            tree.find_scrollable_container(10.0, scroll_box.rect.y + 10.0)
                .is_some()
        );
    }

    #[test]
    fn lays_out_shadow_dom_composed_tree_and_slotted_projection() {
        let mut doc = parse_html(
            "<style>html, body { margin: 0; padding: 0; }</style>\
             <div id=\"host\" style=\"display: flex; gap: 10px; width: 300px;\">\
               <div id=\"light-b\" slot=\"second\" style=\"width: 80px; height: 30px;\">Second</div>\
               <div id=\"light-a\" slot=\"first\" style=\"width: 60px; height: 30px;\">First</div>\
               <div id=\"unslotted\" style=\"width: 90px; height: 30px;\">Hidden</div>\
             </div>",
        );
        let host = doc.get_element_by_id("host").unwrap();
        let sr = doc.attach_shadow(host, true);
        doc.set_inner_html(
            sr,
            "<slot name=\"first\"></slot>\
             <slot name=\"second\"></slot>\
             <slot name=\"fallback\"><div id=\"fb\" style=\"width: 40px; height: 30px;\">Fallback</div></slot>",
        );
        let styles = compute_styles(&doc, &parse_css(&doc.embedded_css()));
        let tree = layout_document(&doc, &styles, 400.0);

        // `#light-a` is projected into `<slot name="first">` at x = 0
        let box_a = find_box(&doc, &tree, "light-a");
        // `#light-b` is projected into `<slot name="second">` after `#light-a` + 10px gap
        let box_b = find_box(&doc, &tree, "light-b");
        assert_eq!(box_a.rect.x, 0.0);
        assert_eq!(box_a.rect.width, 60.0);
        assert_eq!(box_b.rect.x, 70.0);
        assert_eq!(box_b.rect.width, 80.0);
        // `#unslotted` has no matching `<slot>` and is not laid out in the composed tree
        let host_box = find_box(&doc, &tree, "host");
        assert_eq!(host_box.children.len(), 3);
    }

    #[test]
    fn incremental_layout_cache_reuses_unchanged_tree_and_invalidates_on_mutation() {
        let mut doc = parse_html(
            "<style>html, body { margin: 0; padding: 0; }</style>\
             <div id=\"card\" style=\"width: 150px; height: 60px;\">Hello</div>",
        );
        let styles = compute_styles(&doc, &parse_css(&doc.embedded_css()));
        let images = HashMap::new();
        let mut cache = LayoutCache::new();

        let first = layout_document_incremental(&doc, &styles, 400.0, &images, None, &mut cache);
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 0);
        let card_id = doc.get_element_by_id("card").unwrap();
        assert_eq!(cache.cached_box(card_id).unwrap().rect.width, 150.0);

        // Identical pass hits the cache
        let second = layout_document_incremental(&doc, &styles, 400.0, &images, None, &mut cache);
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 1);
        assert_eq!(first, second);

        // Mutating DOM attribute invalidates fingerprint and recomputes
        doc.set_attribute(card_id, "style", "width: 240px; height: 80px;");
        let styles2 = compute_styles(&doc, &parse_css(&doc.embedded_css()));
        let third = layout_document_incremental(&doc, &styles2, 400.0, &images, None, &mut cache);
        assert_eq!(cache.misses(), 2);
        assert_eq!(cache.hits(), 1);
        assert_eq!(find_box(&doc, &third, "card").rect.width, 240.0);
    }
}
