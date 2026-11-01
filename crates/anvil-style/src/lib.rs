// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato
//! Style sheets, selector matching, and the cascade.
//!
//! Second stage of the pipeline. anvil-dom has already built the node list.
//! This crate borrows that [`anvil_dom::Document`], parses CSS into an owned [`Stylesheet`],
//! and writes one [`ComputedStyle`] per reachable node. anvil-layout reads the
//! [`StyleMap`] and places boxes. This crate does not lay out or paint.
//!
//! `width`, `height`, `min-width`, `min-height`, `max-width`, `max-height`, and
//! the offset properties (`top`, `right`, `bottom`, `left`, `inset`) keep `px`,
//! `%`, `em`, `rem`, `vw`, `vh`, `vmin`, `vmax`, `ch`, and `calc()` / `min()` /
//! `max()` / `clamp()`. `font-size`, `line-height`, `margin`, `padding`,
//! `border-width`, and `border-radius` resolve `px`, `em`, `rem`, `%`, viewport
//! units, `ch`, and math functions (`calc()`, `min()`, `max()`, `clamp()`) against
//! the parent/root font size and viewport dimensions. `row-gap`, `column-gap`, and
//! the `gap` shorthand keep lengths, percentages, `calc()`, and `normal`;
//! flex and grid layout use them, and multi-column layout does not exist. `display` accepts `block`, `inline`, `inline-block`,
//! `list-item`, `flow-root`, `flex`, `inline-flex`, `grid`, `inline-grid`,
//! `table`, `table-row-group`, `table-header-group`, `table-footer-group`,
//! `table-row`, `table-cell`, `contents`, and `none`. Selectors take all four combinators, attribute selectors with
//! every operator and the `i` and `s` flags, `:is()`, `:where()`, `:not()`,
//! `:has()`, `:host`, the `nth-*()` family with `of S`, and the structural, link,
//! form, and language pseudo-classes the document can answer; see
//! [`Selector`]. A rule whose selector list holds an invalid selector is
//! dropped whole.
//!
//! `transform` keeps up to [`MAX_TRANSFORM_FNS`] 2D functions:
//! `translate()`, `translateX()`, `translateY()`, `scale()`, `scaleX()`,
//! `scaleY()`, `rotate()`, `skew()`, `skewX()`, `skewY()`, and `matrix()`,
//! plus the standalone `translate`, `scale`, and `rotate` properties.
//! Angles take `deg`, `rad`, `grad`, `turn`, or `0`. 3D functions are not parsed.
//! `transform-origin` takes one or two keywords, lengths, or
//! percentages; a third (z) length is accepted and ignored.
//!
//! `opacity` takes a number or a percentage, clamped to `0..=1`.
//! `transition` and its `-property`, `-duration`, `-delay`, and
//! `-timing-function` longhands keep up to [`MAX_TRANSITIONS`] entries.
//! `animation` and its `-name`, `-duration`, `-delay`, `-iteration-count`,
//! `-direction`, `-fill-mode`, and `-timing-function` longhands keep up to
//! [`MAX_ANIMATIONS`] entries and play `@keyframes` blocks;
//! `animation-play-state` is accepted and ignored.
//! [`compute_styles_animated`] runs both through an [`Animator`]; the plain
//! `compute_styles` functions ignore them. See [`Animator`] for what moves.
//!
//! `@media` reads `min-width`, `max-width`, `min-height`, `max-height`, and `prefers-color-scheme`.
//! Both `dark` and `light` match the active color scheme.
//! The page is a `screen`: `all` and `screen` match, `print` and other media
//! types do not. `not`, `only`, `and`, and comma lists work, and an inner
//! `@media` is combined with the outer one. See [`MediaQuery`].
//! `@supports`, `@layer`, `@scope`, `@container`, `@property`, `@font-face`,
//! `@starting-style`, and `@keyframes` blocks are parsed and retained on the
//! [`Stylesheet`]. The cascade applies user-agent defaults, then author
//! rules ordered by `@layer`, specificity `(ids, classes, types)`, and source order, then the
//! `style` attribute, then `!important` author rules, then
//! `!important` declarations in the `style` attribute.

use anvil_dom::{Document, NodeData, NodeId};
use std::collections::HashMap;

/// Direct linkage to the in-tree `Prism` engine (`engines/prism`).
#[path = "../../../../anvil-browser/engines/prism/src/lib.rs"]
pub mod prism;

mod animation;
mod selectors;
pub use animation::{
    ANIMATABLE, AnimProperty, AnimValue, AnimationDirection, AnimationSpec, Animations, Animator,
    FillMode, FrameClock, Keyframe, KeyframesRule, MAX_ANIMATIONS, MAX_TRANSITIONS, ManualClock,
    MonotonicClock, StepPosition, TimingFunction, TransitionSpec, Transitions, animation_name_hash,
    interpolate, read_value, write_value,
};
pub use selectors::{MAX_SELECTOR_COMPOUNDS, MAX_SELECTOR_NESTING, Selector};

/// A custom property registered via `@property --name { ... }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredProperty {
    pub name: String,
    pub syntax: String,
    pub inherits: bool,
    pub initial_value: Option<String>,
}

/// A parsed style sheet. Rules are owned, in source order, including rules
/// that came from inside `@media`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
    /// `@keyframes` blocks in source order, including any inside `@media`.
    pub keyframes: Vec<KeyframesRule>,
    /// Cascade layers (`@layer`) in first-declared order.
    pub layers: Vec<String>,
    /// Custom properties registered with `@property`.
    pub properties: Vec<RegisteredProperty>,
    /// `@font-face` rules declared in the stylesheet.
    pub font_faces: Vec<FontFaceRule>,
}

/// An `@font-face` rule parsed from CSS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFaceRule {
    pub family: String,
    pub src_url: String,
    pub weight: u16,
    pub italic: bool,
}

/// Resolved `font-family` stack slot. Inherited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FontFamily {
    /// Proportional sans-serif (`sans-serif`, `system-ui`, `ui-sans-serif`). Initial value.
    #[default]
    SansSerif,
    /// Proportional serif (`serif`, `ui-serif`, `georgia`, `times`).
    Serif,
    /// Fixed-width monospace (`monospace`, `ui-monospace`, `consolas`, `courier`).
    Monospace,
    /// Custom `@font-face` family hash.
    Custom(u16),
}

/// `text-transform` case conversion. Inherited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextTransform {
    /// Leave character case unchanged. Initial value.
    #[default]
    None,
    /// Convert all characters to uppercase (`uppercase`).
    Uppercase,
    /// Convert all characters to lowercase (`lowercase`).
    Lowercase,
    /// Capitalize the first letter of each word (`capitalize`).
    Capitalize,
}

/// `object-fit` sizing mode for replaced elements (`<img>`, `<video>`, `<canvas>`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ObjectFit {
    /// Stretch to fill the content box (`fill`). Initial value.
    #[default]
    Fill,
    /// Scale to fit inside the content box preserving aspect ratio (`contain`).
    Contain,
    /// Scale to cover the content box preserving aspect ratio (`cover`).
    Cover,
    /// Do not scale the replaced content (`none`).
    None,
    /// Like `none` or `contain`, whichever produces a smaller concrete object size (`scale-down`).
    ScaleDown,
}

impl Stylesheet {
    /// Move the rules and keyframes of `later` after this sheet's own, as if
    /// its text came after this one in the same sheet.
    pub fn append(&mut self, later: Stylesheet) {
        let mut layer_remap = Vec::with_capacity(later.layers.len());
        for name in later.layers {
            let mapped = if name.starts_with("<anon:") || name.contains(".<anon:") {
                let fresh = format!("<anon:{}>", self.layers.len());
                intern_layer(&mut self.layers, &fresh)
            } else {
                intern_layer(&mut self.layers, &name)
            };
            layer_remap.push(mapped);
        }
        for mut rule in later.rules {
            if let Some(old_idx) = rule.layer {
                rule.layer = layer_remap.get(usize::from(old_idx)).copied();
            }
            self.rules.push(rule);
        }
        self.keyframes.extend(later.keyframes);
        self.font_faces.extend(later.font_faces);
        for prop in later.properties {
            if let Some(existing) = self.properties.iter_mut().find(|p| p.name == prop.name) {
                *existing = prop;
            } else {
                self.properties.push(prop);
            }
        }
    }
}

fn intern_layer(layers: &mut Vec<String>, name: &str) -> u16 {
    if let Some(pos) = layers.iter().position(|existing| existing == name) {
        return pos.min(usize::from(u16::MAX - 1)) as u16;
    }
    let idx = layers.len().min(usize::from(u16::MAX - 1)) as u16;
    layers.push(name.to_string());
    idx
}

fn qualify_layer_name(parent: Option<&str>, child: &str) -> String {
    match parent {
        Some(p) if !p.is_empty() => format!("{p}.{child}"),
        _ => child.to_string(),
    }
}

/// Media queries attached to a rule.
///
/// Holds one or more media query lists. Each list matches when any query in
/// it matches (a comma is OR), and the whole value matches only when every
/// list does. A rule inside nested `@media` blocks gets one list per level,
/// so the levels combine with AND.
///
/// The page is always treated as a `screen`. Media types `all` and `screen`
/// match, `print` and every other type do not. `not` negates one query and
/// `only` is accepted and ignored. Features supported: `min-width` and
/// `max-width` in CSS pixels (rounded to whole pixels), and
/// `prefers-color-scheme`, which matches `dark` only. Other features, and
/// feature values that do not parse, are ignored and do not stop a query from
/// matching. A query with a syntax error matches nothing, while the other
/// queries in its list still count. An empty query text matches everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaQuery {
    lists: Vec<Vec<SingleMediaQuery>>,
}

/// One query from a comma-separated list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SingleMediaQuery {
    /// Did not parse. Matches nothing, even with `not`.
    Invalid,
    Query {
        negated: bool,
        /// `None` when the query is only a condition, which means `all`.
        media_type: Option<MediaType>,
        condition: Option<MediaCondition>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaType {
    All,
    Screen,
    /// `print` and any other type name. Never matches a page on screen.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MediaCondition {
    MinWidth(u32),
    MaxWidth(u32),
    ColorScheme(String),
    /// A feature that is not supported here. Treated as true.
    Ignored,
    Not(Box<MediaCondition>),
    And(Vec<MediaCondition>),
    Or(Vec<MediaCondition>),
}

impl MediaCondition {
    fn matches(&self, vw: u32) -> bool {
        match self {
            MediaCondition::MinWidth(min_w) => vw >= *min_w,
            MediaCondition::MaxWidth(max_w) => vw <= *max_w,
            MediaCondition::ColorScheme(scheme) => scheme.eq_ignore_ascii_case("dark"),
            MediaCondition::Ignored => true,
            MediaCondition::Not(inner) => !inner.matches(vw),
            MediaCondition::And(parts) => parts.iter().all(|part| part.matches(vw)),
            MediaCondition::Or(parts) => parts.iter().any(|part| part.matches(vw)),
        }
    }
}

impl SingleMediaQuery {
    fn matches(&self, vw: u32) -> bool {
        let SingleMediaQuery::Query {
            negated,
            media_type,
            condition,
        } = self
        else {
            return false;
        };
        let type_matches = !matches!(media_type, Some(MediaType::Other));
        let result = type_matches && condition.as_ref().is_none_or(|c| c.matches(vw));
        result != *negated
    }

    /// Parse one query. `src` is lowercase and trimmed.
    fn parse(src: &str) -> Self {
        let Some(tokens) = media_tokens(src) else {
            return SingleMediaQuery::Invalid;
        };
        let mut pos = 0;
        let mut negated = false;
        let mut media_type = None;
        if let Some(MediaToken::Word(word)) = tokens.first() {
            let mut type_at = 0;
            if word == "not" || word == "only" {
                negated = word == "not";
                type_at = 1;
            }
            match tokens.get(type_at) {
                Some(MediaToken::Word(name)) => {
                    media_type = Some(match name.as_str() {
                        "all" => MediaType::All,
                        "screen" => MediaType::Screen,
                        "and" | "or" | "not" | "only" | "layer" => {
                            return SingleMediaQuery::Invalid;
                        }
                        _ => MediaType::Other,
                    });
                    pos = type_at + 1;
                }
                // `not (...)` with no type is a negated condition.
                Some(MediaToken::Group(_)) if negated => {
                    negated = false;
                    pos = 0;
                }
                _ => return SingleMediaQuery::Invalid,
            }
        }
        let condition = if pos == tokens.len() {
            None
        } else {
            if media_type.is_some() {
                // A type can only be followed by `and` and a condition
                // without `or`.
                if !matches!(tokens.get(pos), Some(MediaToken::Word(word)) if word == "and") {
                    return SingleMediaQuery::Invalid;
                }
                pos += 1;
            }
            match parse_media_condition(&tokens[pos..], media_type.is_none(), 0) {
                Some(c) => Some(c),
                None => return SingleMediaQuery::Invalid,
            }
        };
        if media_type.is_some() && condition.is_none() && pos != tokens.len() {
            return SingleMediaQuery::Invalid;
        }
        SingleMediaQuery::Query {
            negated,
            media_type,
            condition,
        }
    }
}

/// A word or a parenthesized group in a media query.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MediaToken {
    Word(String),
    /// Text inside the parentheses, without them.
    Group(String),
}

/// Split a query into words and top-level parenthesized groups.
/// `None` when the parentheses do not balance or a stray character appears.
fn media_tokens(src: &str) -> Option<Vec<MediaToken>> {
    let mut tokens = Vec::new();
    let mut chars = src.char_indices().peekable();
    while let Some(&(idx, ch)) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
        } else if ch == '(' {
            let mut depth = 0usize;
            let mut end = None;
            for (i, c) in chars.by_ref() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let end = end?;
            tokens.push(MediaToken::Group(src[idx + 1..end].trim().to_string()));
        } else if ch.is_alphanumeric() || ch == '-' || ch == '_' {
            let mut word = String::new();
            while let Some(&(_, c)) = chars.peek() {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    word.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            tokens.push(MediaToken::Word(word));
        } else {
            return None;
        }
    }
    Some(tokens)
}

/// Deepest nesting of parenthesized conditions that is read. Deeper input is
/// treated as a syntax error, which keeps hostile sheets from using the stack.
const MAX_MEDIA_NESTING: usize = 64;

/// Parse `not (..)`, or groups joined by only `and` or only `or`.
/// `allow_or` is false right after a media type. `depth` counts the
/// parentheses already entered.
fn parse_media_condition(
    tokens: &[MediaToken],
    allow_or: bool,
    depth: usize,
) -> Option<MediaCondition> {
    if depth > MAX_MEDIA_NESTING {
        return None;
    }
    match tokens {
        [MediaToken::Word(word), MediaToken::Group(inner)] if word == "not" => Some(
            MediaCondition::Not(Box::new(parse_media_in_parens(inner, depth)?)),
        ),
        [MediaToken::Group(first), rest @ ..] => {
            let mut parts = vec![parse_media_in_parens(first, depth)?];
            let mut joiner: Option<&str> = None;
            let mut iter = rest.iter();
            while let Some(token) = iter.next() {
                let MediaToken::Word(word) = token else {
                    return None;
                };
                if word != "and" && !(word == "or" && allow_or) {
                    return None;
                }
                if joiner.is_some_and(|j| j != word) {
                    return None;
                }
                joiner = Some(word);
                let Some(MediaToken::Group(next)) = iter.next() else {
                    return None;
                };
                parts.push(parse_media_in_parens(next, depth)?);
            }
            Some(match (joiner, parts.len()) {
                (_, 1) => parts.remove(0),
                (Some("or"), _) => MediaCondition::Or(parts),
                _ => MediaCondition::And(parts),
            })
        }
        _ => None,
    }
}

/// Parse the inside of one `( ... )`: a nested condition or a feature.
fn parse_media_in_parens(inner: &str, depth: usize) -> Option<MediaCondition> {
    if inner.starts_with('(') || inner.starts_with("not ") || inner.starts_with("not(") {
        if depth >= MAX_MEDIA_NESTING {
            return None;
        }
        let tokens = media_tokens(inner)?;
        return parse_media_condition(&tokens, true, depth + 1);
    }
    if inner.is_empty() {
        return None;
    }
    if let Some((lhs, rhs)) = inner.split_once(">=") {
        let key = lhs.trim();
        let val = rhs.trim().trim_end_matches("px").trim();
        if matches!(key, "width" | "inline-size")
            && let Ok(f) = val.parse::<f32>()
        {
            return Some(MediaCondition::MinWidth(f.max(0.0).round() as u32));
        }
        return Some(MediaCondition::Ignored);
    }
    if let Some((lhs, rhs)) = inner.split_once("<=") {
        let key = lhs.trim();
        let val = rhs.trim().trim_end_matches("px").trim();
        if matches!(key, "width" | "inline-size")
            && let Ok(f) = val.parse::<f32>()
        {
            return Some(MediaCondition::MaxWidth(f.max(0.0).round() as u32));
        }
        return Some(MediaCondition::Ignored);
    }
    if let Some((lhs, rhs)) = inner.split_once('>') {
        let key = lhs.trim();
        let val = rhs.trim().trim_end_matches("px").trim();
        if matches!(key, "width" | "inline-size")
            && let Ok(f) = val.parse::<f32>()
        {
            return Some(MediaCondition::MinWidth(
                (f.max(0.0).floor() as u32).saturating_add(1),
            ));
        }
        return Some(MediaCondition::Ignored);
    }
    if let Some((lhs, rhs)) = inner.split_once('<') {
        let key = lhs.trim();
        let val = rhs.trim().trim_end_matches("px").trim();
        if matches!(key, "width" | "inline-size")
            && let Ok(f) = val.parse::<f32>()
        {
            return Some(MediaCondition::MaxWidth(
                (f.max(0.0).ceil() as u32).saturating_sub(1),
            ));
        }
        return Some(MediaCondition::Ignored);
    }
    let Some((k, v)) = inner.split_once(':') else {
        return Some(MediaCondition::Ignored);
    };
    let key = k.trim();
    let val = v.trim().trim_end_matches("px").trim();
    let px = || val.parse::<f32>().ok().map(|f| f.round() as u32);
    Some(match key {
        "min-width" | "min-inline-size" => {
            px().map_or(MediaCondition::Ignored, MediaCondition::MinWidth)
        }
        "max-width" | "max-inline-size" => {
            px().map_or(MediaCondition::Ignored, MediaCondition::MaxWidth)
        }
        "prefers-color-scheme" => MediaCondition::ColorScheme(val.to_string()),
        _ => MediaCondition::Ignored,
    })
}

/// `@scope (<root>)` or `@scope (<root>) to (<limit>)` boundary condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeCondition {
    /// Optional `<scope-start>` selector list. When `None`, the scope root is `html`.
    pub root: Option<Vec<Selector>>,
    /// Optional `<scope-end>` selector list (`to (...)`). Elements at or inside a limit are excluded.
    pub limit: Option<Vec<Selector>>,
}

impl ScopeCondition {
    /// Parse an `@scope` prelude such as `(.card)`, `(.card) to (.slot)`, or `to (.slot)`.
    pub fn parse(prelude: &str) -> Option<Self> {
        let trimmed = prelude.trim();
        if trimmed.is_empty() {
            return Some(Self {
                root: None,
                limit: None,
            });
        }
        fn extract_paren(s: &str) -> Option<(&str, &str)> {
            let s = s.trim_start();
            if !s.starts_with('(') {
                return None;
            }
            let mut depth = 0usize;
            for (idx, ch) in s.char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some((s[1..idx].trim(), s[idx + 1..].trim()));
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        if let Some(after_to) = trimmed
            .strip_prefix("to")
            .or_else(|| trimmed.strip_prefix("TO"))
            && (after_to.starts_with('(') || after_to.starts_with(char::is_whitespace))
        {
            let (limit_src, rest) = extract_paren(after_to)?;
            if !rest.is_empty() {
                return None;
            }
            let limit = parse_selectors(limit_src)?;
            return Some(Self {
                root: None,
                limit: Some(limit),
            });
        }
        let (root_src, after_root) = extract_paren(trimmed)?;
        let root = parse_selectors(root_src)?;
        if after_root.is_empty() {
            return Some(Self {
                root: Some(root),
                limit: None,
            });
        }
        let after_to = after_root
            .strip_prefix("to")
            .or_else(|| after_root.strip_prefix("TO"))?;
        let (limit_src, rest) = extract_paren(after_to)?;
        if !rest.is_empty() {
            return None;
        }
        let limit = parse_selectors(limit_src)?;
        Some(Self {
            root: Some(root),
            limit: Some(limit),
        })
    }
}

fn parse_container_query(cond: &str) -> MediaQuery {
    let trimmed = cond.trim();
    if let Some(open_idx) = trimmed.find('(') {
        let prefix = trimmed[..open_idx].trim();
        if !prefix.is_empty() && !prefix.eq_ignore_ascii_case("not") {
            return MediaQuery::parse(&trimmed[open_idx..]);
        }
    }
    MediaQuery::parse(trimmed)
}

impl MediaQuery {
    /// Parse the text of an `@media` prelude or a `media` attribute.
    ///
    /// `cond` is borrowed and read without regard to case. Commas split it
    /// into queries; see [`MediaQuery`] for what each one can say. Commas
    /// inside parentheses do not split.
    pub fn parse(cond: &str) -> Self {
        let lower = cond.to_ascii_lowercase();
        let trimmed = lower.trim();
        if trimmed.is_empty() {
            return Self::default();
        }
        let mut list = Vec::new();
        let mut depth = 0usize;
        let mut start = 0;
        for (idx, ch) in trimmed.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    list.push(SingleMediaQuery::parse(trimmed[start..idx].trim()));
                    start = idx + 1;
                }
                _ => {}
            }
        }
        list.push(SingleMediaQuery::parse(trimmed[start..].trim()));
        Self { lists: vec![list] }
    }

    /// Both `self` and `inner` must match, as for `@media` inside `@media`.
    pub fn and(mut self, inner: &MediaQuery) -> Self {
        self.lists.extend(inner.lists.iter().cloned());
        self
    }

    /// True when every list has a matching query for a screen
    /// `viewport_width` CSS pixels wide.
    ///
    /// Width is rounded to a `u32` after clamping at 0. The query is
    /// borrowed.
    pub fn matches(&self, viewport_width: f32) -> bool {
        let vw = viewport_width.max(0.0).round() as u32;
        self.lists
            .iter()
            .all(|list| list.iter().any(|query| query.matches(vw)))
    }
}

/// One style rule. Selectors and declarations are owned.
/// `media`, `container`, `scope`, and `layer` are `None` when the rule was not
/// inside `@media`, `@container`, `@scope`, or `@layer`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
    pub media: Option<MediaQuery>,
    pub container: Option<MediaQuery>,
    pub scope: Option<ScopeCondition>,
    pub layer: Option<u16>,
}

/// One CSS declaration. `name` is owned and lowercase. `value` is owned
/// and still unparsed, with any trailing `!important` taken off.
///
/// `important` is true when the value ended in `!important`. Important
/// declarations win over normal ones in the cascade, whatever their
/// specificity or order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    pub name: String,
    pub value: String,
    pub important: bool,
}

/// sRGB color. Channels are 0–255. `a` is 255 when the source omitted alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    /// Opaque black.
    pub const BLACK: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };

    /// Opaque white.
    pub const WHITE: Self = Self {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };

    /// Parse `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`, `rgb()`, `rgba()`, or a
    /// named color: black, white, red, green, blue, gray, grey, silver, yellow,
    /// orange, transparent.
    ///
    /// `value` is borrowed. `None` if the text is not one of those forms.
    /// `rgba` alpha with a decimal point and a value of at most 1 is a fraction
    /// of 255. Any other alpha is a 0–255 channel.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if let Some(hex) = value.strip_prefix('#') {
            return parse_hex(hex);
        }
        let lower = value.to_ascii_lowercase();
        if let Some(inner) = lower.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
            return parse_rgb(inner, false)
                .or_else(|| prism::parse_css_rgba8(value).map(|[r, g, b, a]| Self { r, g, b, a }));
        }
        if let Some(inner) = lower
            .strip_prefix("rgba(")
            .and_then(|s| s.strip_suffix(')'))
        {
            return parse_rgb(inner, true)
                .or_else(|| prism::parse_css_rgba8(value).map(|[r, g, b, a]| Self { r, g, b, a }));
        }
        Some(match lower.as_str() {
            "black" => Self::BLACK,
            "white" => Self {
                r: 255,
                g: 255,
                b: 255,
                a: 255,
            },
            "red" => Self {
                r: 255,
                g: 0,
                b: 0,
                a: 255,
            },
            "green" => Self {
                r: 0,
                g: 128,
                b: 0,
                a: 255,
            },
            "blue" => Self {
                r: 0,
                g: 0,
                b: 255,
                a: 255,
            },
            "gray" | "grey" => Self {
                r: 128,
                g: 128,
                b: 128,
                a: 255,
            },
            "silver" => Self {
                r: 192,
                g: 192,
                b: 192,
                a: 255,
            },
            "yellow" => Self {
                r: 255,
                g: 255,
                b: 0,
                a: 255,
            },
            "orange" => Self {
                r: 255,
                g: 165,
                b: 0,
                a: 255,
            },
            "transparent" => Self {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            },
            _ => return prism::parse_css_rgba8(value).map(|[r, g, b, a]| Self { r, g, b, a }),
        })
    }

    /// Owned `#rrggbb`. Alpha is omitted.
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// `display` as stored after the cascade. Unknown values are left unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Display {
    /// Participate in a line. This is the initial value.
    Inline,
    /// A block box.
    Block,
    /// A grid container (`display: grid`).
    Grid,
    /// A flex container. The items are the element's children.
    Flex,
    /// The element and its descendants generate no box.
    None,
}

/// Main axis of a flex container. The initial value is [`Self::Row`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlexDirection {
    /// Lay items out on horizontal lines, start to end.
    Row,
    /// Lay items out on horizontal lines, end to start (`row-reverse`).
    RowReverse,
    /// Lay items out on vertical lines, top to bottom.
    Column,
    /// Lay items out on vertical lines, bottom to top (`column-reverse`).
    ColumnReverse,
}

/// Whether a flex container breaks items onto more than one line.
/// The initial value is [`Self::NoWrap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlexWrap {
    /// Keep every item on one line, even when the line is wider than the container.
    NoWrap,
    /// Start a new line when the next item does not fit. A single item that is
    /// wider than the container stays on its own line.
    Wrap,
    /// Like [`Self::Wrap`], with cross-axis line order reversed (`wrap-reverse`).
    WrapReverse,
}

/// Multi-line cross-axis distribution (`align-content`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AlignContent {
    /// Stretch lines to fill remaining cross-axis space. Initial value.
    #[default]
    Stretch,
    /// Pack lines against the cross start (`flex-start` / `start`).
    Start,
    /// Center lines on the cross axis (`center`).
    Center,
    /// Pack lines against the cross end (`flex-end` / `end`).
    End,
    /// Equal space between lines (`space-between`).
    SpaceBetween,
    /// Equal space around each line (`space-around`).
    SpaceAround,
    /// Equal space between lines and edges (`space-evenly`).
    SpaceEvenly,
}

/// Main-axis packing for a flex line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JustifyContent {
    /// Pack against the start. `flex-start` and `start` both map here.
    Start,
    /// Center the free space before and after the items.
    Center,
    /// Pack against the end. `flex-end` and `end` both map here.
    End,
    /// First item at the start, last at the end, equal gaps between.
    /// A single item is placed at the start.
    SpaceBetween,
    /// Equal space around each item (`space-around`).
    SpaceAround,
    /// Equal space between items and edges (`space-evenly`).
    SpaceEvenly,
}

/// `box-sizing` as stored for layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BoxSizing {
    /// `width` and `height` size the content box. Initial value.
    #[default]
    ContentBox,
    /// `width` and `height` include padding and border.
    BorderBox,
}

/// `visibility` as stored for layout and paint. Inherited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Visibility {
    /// Box is drawn normally. Initial value.
    #[default]
    Visible,
    /// Box takes up layout space, but its own background, border, and text are not drawn.
    Hidden,
    /// Treated like `Hidden` on non-table boxes.
    Collapse,
}

/// Horizontal alignment of inline content inside a block container. Inherited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextAlign {
    /// Align inline runs to the left edge (`left` or `start`). Initial value.
    #[default]
    Left,
    /// Center inline runs within the line (`center`).
    Center,
    /// Align inline runs to the right edge (`right` or `end`).
    Right,
    /// Justify inline runs (`justify`).
    Justify,
}

/// `white-space` handling for inline text runs. Inherited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WhiteSpace {
    /// Collapse whitespace and wrap lines. Initial value.
    #[default]
    Normal,
    /// Collapse whitespace and do not wrap lines (`nowrap`).
    Nowrap,
    /// Preserve whitespace and newlines (`pre`).
    Pre,
    /// Preserve whitespace and wrap lines (`pre-wrap` / `pre-line`).
    PreWrap,
}

/// Cross-axis alignment for flex or grid items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlignItems {
    /// Align to the start edge. `flex-start`, `self-start`, and `start` map here.
    Start,
    /// Center on the axis.
    Center,
    /// Align to the end edge. `flex-end`, `self-end`, and `end` map here.
    End,
    /// Stretch items whose size on the axis is `auto`. This is the initial value.
    Stretch,
    /// Align items along their first text baseline (`baseline`).
    Baseline,
}

/// `position` as stored for anvil-layout. Unknown values are left unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    /// In flow. Offsets are ignored. This is the initial value.
    Static,
    /// In flow, then shifted by `top`/`left` (or `bottom`/`right`).
    Relative,
    /// Out of flow. The containing block is the parent's border box.
    Absolute,
    /// Out of flow. Laid out against the origin, not a separate viewport.
    Fixed,
    /// In flow, then shifted while scrolling so that the box keeps the
    /// `top` / `right` / `bottom` / `left` distance from the edge of its
    /// scrollport, without leaving its containing block.
    Sticky,
}

/// `overflow` as stored for layout and paint. `scroll` and `auto` become [`Self::Hidden`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overflow {
    /// Descendants may paint outside the box. This is the initial value.
    Visible,
    /// Clip descendants to the padding edge. Also used for `scroll` and `auto`.
    Hidden,
}

/// A length stored after parsing. [`Size::used_px`] turns `px`, `%`, `em`,
/// and `rem` into CSS pixels. `vw`, `vh`, `vmin`, and `vmax` stay here until
/// layout, which uses the viewport stored on [`StyleMap`].
/// Units other than those are not represented; they parse as [`Self::Auto`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Size {
    /// The layout stage chooses the size.
    Auto,
    /// A CSS pixel length. Zero is allowed. Negative values are not rejected here.
    Px(f32),
    /// A percentage of the containing block on the axis layout is resolving.
    /// `50.0` means `50%`.
    Percent(f32),
    /// A multiple of the element's own font size. `2.0` means `2em`.
    Em(f32),
    /// A multiple of the root font size. `2.0` means `2rem`.
    Rem(f32),
    /// A percentage of the viewport width. `50.0` means `50vw`.
    Vw(f32),
    /// A percentage of the viewport height. `50.0` means `50vh`.
    Vh(f32),
    /// A percentage of the smaller viewport side. `50.0` means `50vmin`.
    Vmin(f32),
    /// A percentage of the larger viewport side. `50.0` means `50vmax`.
    Vmax(f32),
    /// A multiple of the width of the `0` glyph in the element's font.
    /// `20.0` means `20ch`. Layout resolves it; see `anvil_layout::ch_width`.
    Ch(f32),
    /// A combined `calc(...)` expression across `px`, `%`, `em`, and `rem`.
    Calc {
        px: f32,
        percent: f32,
        em: f32,
        rem: f32,
    },
}

impl Size {
    /// Used CSS pixels, or `None` when this length is [`Size::Auto`].
    ///
    /// Viewport units and `ch` also return `None` here, because they need
    /// the viewport or font metrics that only layout has.
    ///
    /// `percent_basis` is the containing-block size on this axis, in CSS pixels.
    /// A percentage needs a finite basis. `em_px` is this element's font size.
    /// `rem_px` is the root font size from [`StyleMap::root_font_px`].
    pub fn used_px(self, percent_basis: f32, em_px: f32, rem_px: f32) -> Option<f32> {
        match self {
            Size::Auto => None,
            Size::Px(value) => Some(value),
            Size::Percent(value) => {
                if percent_basis.is_finite() {
                    Some(percent_basis * value / 100.0)
                } else {
                    None
                }
            }
            Size::Em(value) => Some(em_px * value),
            Size::Rem(value) => Some(rem_px * value),
            Size::Vw(_) | Size::Vh(_) | Size::Vmin(_) | Size::Vmax(_) | Size::Ch(_) => None,
            Size::Calc {
                px,
                percent,
                em,
                rem,
            } => {
                if percent != 0.0 && !percent_basis.is_finite() {
                    return None;
                }
                let pct_px = if percent != 0.0 {
                    percent_basis * percent / 100.0
                } else {
                    0.0
                };
                Some(px + pct_px + em * em_px + rem * rem_px)
            }
        }
    }
}

/// Four CSS pixel edges, clockwise from the top. Not a percentage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edge {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Edge {
    /// All four edges zero.
    pub const fn zero() -> Self {
        Self {
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        }
    }

    /// The same pixel length on every edge.
    pub const fn all(value: f32) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }
}

/// One `box-shadow` layer. Offsets are CSS pixels, y downward (positive `offset_y`
/// moves the shadow down). Blur is floored at 0 when parsed. The default color,
/// if the declaration omits one, is black at alpha 128. Inset shadows are not parsed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxShadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur_radius: f32,
    pub spread_radius: f32,
    pub color: Color,
}

/// A two-stop `linear-gradient`. `angle_deg` follows CSS: 0 is upward, 90 is
/// to the right. Extra color stops are dropped; only the first and last are kept.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinearGradient {
    pub angle_deg: f32,
    pub start_color: Color,
    pub end_color: Color,
}

/// One track in `grid-template-columns`, `grid-template-rows`, `grid-auto-rows`, or `grid-auto-columns`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GridTrack {
    /// Fixed pixel size (`120px`).
    Px(f32),
    /// Percentage of the grid container content size on that axis (`25%`).
    /// A row percentage with no definite container height acts as `auto`.
    Percent(f32),
    /// Share of remaining free space (`1fr`, `2fr`).
    Fr(f32),
    /// `auto`.
    Auto,
    /// `min-content` intrinsic track sizing.
    MinContent,
    /// `max-content` intrinsic track sizing.
    MaxContent,
    /// `fit-content(<length>)` clamped intrinsic track sizing in CSS pixels.
    FitContent(f32),
    /// `minmax(min, max)`. The minimum is never a flexible size.
    MinMax(TrackBreadth, TrackBreadth),
}

/// Auto-placement algorithm direction and packing mode (`grid-auto-flow`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GridAutoFlow {
    /// Fill row by row without backtracking. Initial value.
    #[default]
    Row,
    /// Fill column by column without backtracking.
    Column,
    /// Fill row by row, backtracking into earlier empty cells (`row dense`).
    RowDense,
    /// Fill column by column, backtracking into earlier empty cells (`column dense`).
    ColumnDense,
}

/// Parsed `grid-template-areas` cell matrix (up to 6 rows by 6 columns).
/// Each cell stores `0` for an empty `.` token or [`grid_area_hash`] of the area name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridTemplateAreas {
    pub rows: u8,
    pub cols: u8,
    pub cells: [[u32; 6]; 6],
}

impl GridTemplateAreas {
    pub const NONE: Self = Self {
        rows: 0,
        cols: 0,
        cells: [[0; 6]; 6],
    };

    /// Look up the `(row_start, col_start, row_end, col_end)` 1-based inclusive-start /
    /// exclusive-end grid lines for an area hash, or `None` if not present.
    pub fn find_area(&self, hash: u32) -> Option<(i16, i16, i16, i16)> {
        if hash == 0 || self.rows == 0 || self.cols == 0 {
            return None;
        }
        let mut r_min = i16::MAX;
        let mut r_max = 0i16;
        let mut c_min = i16::MAX;
        let mut c_max = 0i16;
        for r in 0..usize::from(self.rows).min(6) {
            for c in 0..usize::from(self.cols).min(6) {
                if self.cells[r][c] == hash {
                    let ri = (r as i16) + 1;
                    let ci = (c as i16) + 1;
                    r_min = r_min.min(ri);
                    r_max = r_max.max(ri + 1);
                    c_min = c_min.min(ci);
                    c_max = c_max.max(ci + 1);
                }
            }
        }
        (r_min <= r_max && c_min <= c_max).then_some((r_min, c_min, r_max, c_max))
    }
}

/// FNV-1a 32-bit hash for a named grid area identifier (never returns 0 for non-empty names).
pub fn grid_area_hash(name: &str) -> u32 {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed == "." {
        return 0;
    }
    let mut h = 0x811c_9dc5u32;
    for b in trimmed.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    if h == 0 { 1 } else { h }
}

/// Auto-repeating track mode for `repeat(auto-fill, ...)` and `repeat(auto-fit, ...)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GridAutoRepeat {
    #[default]
    None,
    AutoFill,
    AutoFit,
}

/// Largest grid line number or span kept by the parser. Bigger values are
/// clamped, so a typo such as `span 99999` does not build a huge grid.
pub const MAX_GRID_LINE: i16 = 1000;

/// One end of a grid item's placement on an axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GridLine {
    /// Auto-placed. This is the initial value.
    #[default]
    Auto,
    /// A numbered line. Positive counts from the start (1 is the first
    /// line), negative from the end of the explicit grid (-1 is the last).
    /// Never 0.
    Line(i16),
    /// `span N`. Always at least 1.
    Span(i16),
}

/// `grid-column` or `grid-row` after parsing: a start and an end line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GridPlacement {
    pub start: GridLine,
    pub end: GridLine,
}

/// One side of a `minmax()` track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TrackBreadth {
    /// Fixed pixel size.
    Px(f32),
    /// Percentage of the grid container content size on that axis.
    Percent(f32),
    /// Flexible share. Only valid as the maximum.
    Fr(f32),
    /// Content size on rows, `1fr` behavior on columns when it is the maximum.
    Auto,
    /// `min-content` intrinsic sizing.
    MinContent,
    /// `max-content` intrinsic sizing.
    MaxContent,
    /// `fit-content(<length>)` clamped intrinsic sizing.
    FitContent(f32),
}

/// Most functions one `transform` list keeps. A longer list is ignored.
pub const MAX_TRANSFORM_FNS: usize = 16;

/// One 2D transform function. Angles are radians. Translations keep their
/// unit; layout resolves percentages against the border box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TransformFn {
    /// `translate()`, `translateX()`, `translateY()`.
    Translate(Size, Size),
    /// `scale()`, `scaleX()`, `scaleY()`.
    Scale(f32, f32),
    /// `rotate()`. Positive turns clockwise on screen.
    Rotate(f32),
    /// `skew()`, `skewX()`, `skewY()`.
    Skew(f32, f32),
    /// `matrix(a, b, c, d, e, f)`, with `e` and `f` in CSS pixels.
    Matrix([f32; 6]),
}

/// A parsed `transform` list in source order. Empty is `none`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransformList {
    fns: [TransformFn; MAX_TRANSFORM_FNS],
    len: u8,
}

impl TransformList {
    /// The empty list, `transform: none`.
    pub const NONE: Self = Self {
        fns: [TransformFn::Scale(1.0, 1.0); MAX_TRANSFORM_FNS],
        len: 0,
    };

    /// A list holding `fns`, or `None` when there are more than
    /// [`MAX_TRANSFORM_FNS`].
    pub fn new(fns: &[TransformFn]) -> Option<Self> {
        if fns.len() > MAX_TRANSFORM_FNS {
            return None;
        }
        let mut list = Self::NONE;
        list.fns[..fns.len()].copy_from_slice(fns);
        list.len = fns.len() as u8;
        Some(list)
    }

    /// The functions in source order.
    pub fn functions(&self) -> &[TransformFn] {
        &self.fns[..usize::from(self.len).min(MAX_TRANSFORM_FNS)]
    }

    /// True for `none`.
    pub fn is_none(&self) -> bool {
        self.len == 0
    }
}

/// `transform-origin` as a horizontal and a vertical length. Percentages
/// are of the border box. The initial value is `50% 50%`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransformOrigin {
    pub x: Size,
    pub y: Size,
}

impl TransformOrigin {
    /// `50% 50%`, the center of the border box.
    pub const CENTER: Self = Self {
        x: Size::Percent(50.0),
        y: Size::Percent(50.0),
    };
}

/// Used values for one element after the user-agent defaults, the cascade,
/// and the inline `style` attribute.
///
/// `width`, `height`, `max-width`, and offsets may still be `%`, `em`, or `rem`.
/// `font-size` is already in CSS pixels. `background` is `None` when no solid color was set.
/// `background_gradient` is independent and, when set, also stores its first
/// stop in `background`. `box_shadow` is `None` when unset or `none`.
/// Inherited on elements: `color`, `font_size`, and `underline`. A child's
/// `border_color` starts as the inherited `color`, not the parent's border color.
/// Text nodes keep the parent style unchanged. This value does not include
/// a used position; anvil-layout resolves `auto` sizes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ComputedStyle {
    pub display: Display,
    /// True when the box participates in inline horizontal flow (`inline`,
    /// `inline-block`, `inline-flex`, `inline-grid`).
    pub inline_level: bool,
    pub position: Position,
    pub top: Size,
    pub right: Size,
    pub bottom: Size,
    pub left: Size,
    pub z_index: i32,
    pub overflow: Overflow,
    pub visibility: Visibility,
    pub box_sizing: BoxSizing,
    pub color: Color,
    pub background: Option<Color>,
    pub background_gradient: Option<LinearGradient>,
    pub box_shadow: Option<BoxShadow>,
    pub font_size: f32,
    pub font_weight: u16,
    pub italic: bool,
    pub line_height: Option<f32>,
    pub text_align: TextAlign,
    pub white_space: WhiteSpace,
    pub margin: Edge,
    pub margin_top_auto: bool,
    pub margin_right_auto: bool,
    pub margin_bottom_auto: bool,
    pub margin_left_auto: bool,
    pub padding: Edge,
    pub border: Edge,
    pub border_color: Color,
    pub border_radius: f32,
    pub width: Size,
    pub height: Size,
    pub min_width: Size,
    pub max_width: Size,
    pub min_height: Size,
    pub max_height: Size,
    /// Preferred `width / height` ratio from CSS `aspect-ratio` (`None` when `auto`).
    pub aspect_ratio: Option<f32>,
    pub underline: bool,
    /// Truncate overflowing single-line inline text with `…` (`text-overflow: ellipsis`).
    pub text_overflow_ellipsis: bool,
    /// Break long unbreakable words at arbitrary character boundaries (`overflow-wrap: break-word` / `word-break: break-all`).
    pub word_break_any: bool,
    pub flex_direction: FlexDirection,
    pub flex_wrap: FlexWrap,
    /// Visual order of a flex or grid item (`order`). Initial value is 0.
    pub order: i32,
    /// Main-size growth factor for a flex item. The initial value is 0.
    /// This is not inherited. Negative values are stored as 0.
    pub flex_grow: f32,
    /// Main-size shrink factor for a flex item. The initial value is 1.
    pub flex_shrink: f32,
    /// Initial main size of a flex item (`flex-basis`). The initial value is `auto`.
    pub flex_basis: Size,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    pub align_self: Option<AlignItems>,
    pub align_content: AlignContent,
    pub justify_items: AlignItems,
    pub justify_self: Option<AlignItems>,
    pub grid_template_columns: [GridTrack; 12],
    pub grid_column_count: u8,
    pub grid_auto_repeat: GridAutoRepeat,
    /// Explicit row tracks from `grid-template-rows`. See [`Self::grid_row_tracks`].
    pub grid_template_rows: [GridTrack; 12],
    pub grid_row_count: u8,
    /// Size of rows past the explicit grid (`grid-auto-rows`). The initial value is `auto`.
    pub grid_auto_rows: GridTrack,
    /// Multi-track list for `grid-auto-rows` cycling.
    pub grid_auto_row_list: [GridTrack; 8],
    pub grid_auto_row_count: u8,
    /// Multi-track list for `grid-auto-columns` cycling.
    pub grid_auto_columns: [GridTrack; 8],
    pub grid_auto_col_count: u8,
    /// Auto-placement mode (`grid-auto-flow`).
    pub grid_auto_flow: GridAutoFlow,
    /// Named grid template areas (`grid-template-areas`).
    pub grid_template_areas: GridTemplateAreas,
    /// Hash of a named area from `grid-area: <ident>` (`0` when unset).
    pub grid_area_name: u32,
    /// Placement from `grid-column` and its longhands. Not inherited.
    pub grid_column: GridPlacement,
    /// Placement from `grid-row` and its longhands. Not inherited.
    pub grid_row: GridPlacement,
    /// `row-gap`: space between grid rows and between flex lines (between
    /// items in a column flex container). `normal` is stored as `0px`. A
    /// percentage is of the container's content height and counts as 0 when
    /// that height is not definite. Not inherited.
    pub row_gap: Size,
    /// `column-gap`: space between grid columns and between items in a flex
    /// row. `normal` is stored as `0px`. A percentage is of the container's
    /// content width. Multi-column layout does not exist here, so its
    /// `normal` (1em) never applies. Not inherited.
    pub column_gap: Size,
    /// `transform`. Not inherited. See [`TransformList`].
    pub transform: TransformList,
    /// `transform-origin`. Not inherited.
    pub transform_origin: TransformOrigin,
    /// `opacity` in `0..=1`. Not inherited; it applies to the box and
    /// everything in it as one group.
    pub opacity: f32,
    /// `transition` and its longhands. Not inherited.
    pub transitions: Transitions,
    /// `animation` and its longhands. Not inherited.
    pub animations: Animations,
    pub font_family: FontFamily,
    pub text_transform: TextTransform,
    pub line_through: bool,
    pub letter_spacing: f32,
    pub word_spacing: f32,
    pub scrollable_overflow: bool,
    pub object_fit: ObjectFit,
    pub border_colors: [Color; 4],
    pub outline_width: f32,
    pub outline_color: Color,
    pub outline_offset: f32,
    pub backdrop_blur: f32,
    pub filter_blur: f32,
    pub filter_brightness: f32,
    pub filter_contrast: f32,
    pub filter_grayscale: f32,
    pub filter_invert: f32,
    pub filter_sepia: f32,
}

impl ComputedStyle {
    /// Initial values: inline, static, visible, black text, 16px, no margin,
    /// padding, border, or background, row flex, start justification, stretch
    /// alignment, and zero gap.
    pub const fn initial() -> Self {
        Self {
            display: Display::Inline,
            inline_level: true,
            position: Position::Static,
            top: Size::Auto,
            right: Size::Auto,
            bottom: Size::Auto,
            left: Size::Auto,
            z_index: 0,
            overflow: Overflow::Visible,
            visibility: Visibility::Visible,
            box_sizing: BoxSizing::ContentBox,
            color: Color::BLACK,
            background: None,
            background_gradient: None,
            box_shadow: None,
            font_size: 16.0,
            font_weight: 400,
            italic: false,
            line_height: None,
            text_align: TextAlign::Left,
            white_space: WhiteSpace::Normal,
            margin: Edge::zero(),
            margin_top_auto: false,
            margin_right_auto: false,
            margin_bottom_auto: false,
            margin_left_auto: false,
            padding: Edge::zero(),
            border: Edge::zero(),
            border_color: Color::BLACK,
            border_radius: 0.0,
            width: Size::Auto,
            height: Size::Auto,
            min_width: Size::Auto,
            max_width: Size::Auto,
            min_height: Size::Auto,
            max_height: Size::Auto,
            aspect_ratio: None,
            underline: false,
            text_overflow_ellipsis: false,
            word_break_any: false,
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::NoWrap,
            order: 0,
            flex_grow: 0.0,
            flex_shrink: 0.0,
            flex_basis: Size::Auto,
            justify_content: JustifyContent::Start,
            align_items: AlignItems::Stretch,
            align_self: None,
            align_content: AlignContent::Stretch,
            justify_items: AlignItems::Stretch,
            justify_self: None,
            grid_template_columns: [GridTrack::Fr(1.0); 12],
            grid_column_count: 0,
            grid_auto_repeat: GridAutoRepeat::None,
            grid_template_rows: [GridTrack::Auto; 12],
            grid_row_count: 0,
            grid_auto_rows: GridTrack::Auto,
            grid_auto_row_list: [GridTrack::Auto; 8],
            grid_auto_row_count: 0,
            grid_auto_columns: [GridTrack::Auto; 8],
            grid_auto_col_count: 0,
            grid_auto_flow: GridAutoFlow::Row,
            grid_template_areas: GridTemplateAreas::NONE,
            grid_area_name: 0,
            grid_column: GridPlacement {
                start: GridLine::Auto,
                end: GridLine::Auto,
            },
            grid_row: GridPlacement {
                start: GridLine::Auto,
                end: GridLine::Auto,
            },
            row_gap: Size::Px(0.0),
            column_gap: Size::Px(0.0),
            transform: TransformList::NONE,
            transform_origin: TransformOrigin::CENTER,
            opacity: 1.0,
            transitions: Transitions::INITIAL,
            animations: Animations::INITIAL,
            font_family: FontFamily::SansSerif,
            text_transform: TextTransform::None,
            line_through: false,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            scrollable_overflow: false,
            object_fit: ObjectFit::Fill,
            border_colors: [Color::BLACK; 4],
            outline_width: 0.0,
            outline_color: Color::BLACK,
            outline_offset: 0.0,
            backdrop_blur: 0.0,
            filter_blur: 0.0,
            filter_brightness: 1.0,
            filter_contrast: 1.0,
            filter_grayscale: 0.0,
            filter_invert: 0.0,
            filter_sepia: 0.0,
        }
    }

    /// Active column tracks parsed from `grid-template-columns`.
    pub fn grid_tracks(&self) -> &[GridTrack] {
        &self.grid_template_columns[..(self.grid_column_count as usize).min(12)]
    }

    /// Active explicit row tracks parsed from `grid-template-rows`.
    /// Empty when the property is unset; every row is then a `grid-auto-rows` row.
    pub fn grid_row_tracks(&self) -> &[GridTrack] {
        &self.grid_template_rows[..(self.grid_row_count as usize).min(12)]
    }

    /// Active implicit row tracks parsed from `grid-auto-rows`.
    pub fn grid_auto_row_tracks(&self) -> &[GridTrack] {
        &self.grid_auto_row_list[..(self.grid_auto_row_count as usize).min(8)]
    }

    /// Active implicit column tracks parsed from `grid-auto-columns`.
    pub fn grid_auto_column_tracks(&self) -> &[GridTrack] {
        &self.grid_auto_columns[..(self.grid_auto_col_count as usize).min(8)]
    }

    /// Style a child element starts from. Copies inherited properties, sets
    /// `border_color` to `color`, and resets everything else.
    fn inherited_child(self) -> Self {
        Self {
            color: self.color,
            font_size: self.font_size,
            font_weight: self.font_weight,
            italic: self.italic,
            line_height: self.line_height,
            text_align: self.text_align,
            white_space: self.white_space,
            visibility: self.visibility,
            underline: self.underline,
            word_break_any: self.word_break_any,
            border_color: self.color,
            border_colors: [self.color; 4],
            outline_color: self.color,
            font_family: self.font_family,
            text_transform: self.text_transform,
            line_through: self.line_through,
            letter_spacing: self.letter_spacing,
            word_spacing: self.word_spacing,
            ..Self::initial()
        }
    }
}

/// Computed styles indexed by [`NodeId`]. The vector is owned.
/// Slots that the walk never wrote stay at [`ComputedStyle::initial`].
#[derive(Clone, Debug, PartialEq)]
pub struct StyleMap {
    styles: Vec<ComputedStyle>,
    before_styles: HashMap<NodeId, (String, ComputedStyle)>,
    after_styles: HashMap<NodeId, (String, ComputedStyle)>,
    root_font_px: f32,
    viewport_width: f32,
    viewport_height: f32,
}

impl StyleMap {
    /// Borrow the style for `id`. An id past the map returns a static initial
    /// style, not an error. The reference lives as long as `self`.
    pub fn get(&self, id: NodeId) -> &ComputedStyle {
        self.styles.get(id.index()).unwrap_or(&FALLBACK)
    }

    /// Generated `(content, style)` for `id::before`, when present and not `display: none`.
    pub fn before_style(&self, id: NodeId) -> Option<&(String, ComputedStyle)> {
        self.before_styles.get(&id)
    }

    /// Generated `(content, style)` for `id::after`, when present and not `display: none`.
    pub fn after_style(&self, id: NodeId) -> Option<&(String, ComputedStyle)> {
        self.after_styles.get(&id)
    }

    /// Computed `font-size` of the `html` element, in CSS pixels.
    /// `rem` uses this value. It stays 16 when `html` does not set a font size.
    pub fn root_font_px(&self) -> f32 {
        self.root_font_px
    }

    /// Viewport width in CSS pixels, used for `vw` and `vmin` / `vmax`.
    pub fn viewport_width(&self) -> f32 {
        self.viewport_width
    }

    /// Viewport height in CSS pixels, used for `vh` and `vmin` / `vmax`.
    pub fn viewport_height(&self) -> f32 {
        self.viewport_height
    }
}

const FALLBACK: ComputedStyle = ComputedStyle::initial();

/// Parse `input` into an owned style sheet.
///
/// Comments (`/* */`) are replaced with a space. Rules inside `@media` keep
/// that query. An inner `@media` is combined with the outer one.
/// `@keyframes` blocks go to [`Stylesheet::keyframes`]. Other at-rules are
/// skipped. The input is borrowed.
pub fn parse_css(input: &str) -> Stylesheet {
    parse_css_with_media(input, None)
}

/// Parse `input` as a sheet that only applies under `media`, the text of a
/// `media` attribute on `<link>` or `<style>`.
///
/// Every rule gets that query, combined with any `@media` around it. `None`
/// is the same as [`parse_css`]. `@keyframes` blocks are kept either way,
/// as they are inside `@media`.
pub fn parse_css_with_media(input: &str, media: Option<&str>) -> Stylesheet {
    let input = strip_comments(input);
    let mut sheet = Stylesheet::default();
    parse_css_rules(&input, media.map(MediaQuery::parse), &mut sheet);
    sheet
}

/// Characters of a style sheet that are outside quoted strings, with
/// their byte offsets.
///
/// Quotes and everything inside them are skipped, and so is a backslash
/// with the character it escapes, inside or outside a string. The body of
/// an unquoted `url(...)` is skipped too; its parentheses are kept. A string
/// ends at its closing quote, at an unescaped newline, or at the end of the
/// input, as in CSS syntax. [`Unquoted::in_string`] says whether the input
/// ended inside a string.
pub(crate) struct Unquoted<'a> {
    src: &'a str,
    chars: std::str::CharIndices<'a>,
    quote: Option<char>,
    in_url: bool,
}

impl<'a> Unquoted<'a> {
    pub(crate) fn new(src: &'a str) -> Self {
        Unquoted {
            src,
            chars: src.char_indices(),
            quote: None,
            in_url: false,
        }
    }

    /// True when the `(` at byte `open` starts an unquoted `url(`, whose
    /// body runs to the next unescaped `)` and may hold `;`, `{` or `}`.
    fn opens_unquoted_url(&self, open: usize) -> bool {
        let before = &self.src[..open];
        let Some(head) = before.len().checked_sub(3).and_then(|i| before.get(i..)) else {
            return false;
        };
        if !head.eq_ignore_ascii_case("url") {
            return false;
        }
        let word_start = before[..before.len() - 3]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_'));
        let body = self.src[open + 1..].trim_start();
        word_start && !body.starts_with(['"', '\''])
    }

    /// True while the scan is inside a string.
    pub(crate) fn in_string(&self) -> bool {
        self.quote.is_some()
    }
}

impl Iterator for Unquoted<'_> {
    type Item = (usize, char);

    fn next(&mut self) -> Option<(usize, char)> {
        loop {
            let (idx, ch) = self.chars.next()?;
            if self.in_url {
                match ch {
                    '\\' => {
                        self.chars.next();
                    }
                    ')' => {
                        self.in_url = false;
                        return Some((idx, ch));
                    }
                    _ => {}
                }
                continue;
            }
            match self.quote {
                Some(quote) => {
                    if ch == '\\' {
                        self.chars.next();
                    } else if ch == quote || ch == '\n' {
                        self.quote = None;
                    }
                }
                None => match ch {
                    '\\' => {
                        self.chars.next();
                    }
                    '"' | '\'' => self.quote = Some(ch),
                    '(' if self.opens_unquoted_url(idx) => {
                        self.in_url = true;
                        return Some((idx, ch));
                    }
                    _ => return Some((idx, ch)),
                },
            }
        }
    }
}

/// Byte index of the first `wanted` outside strings, or `None`.
pub(crate) fn find_unquoted(src: &str, wanted: char) -> Option<usize> {
    Unquoted::new(src)
        .find(|&(_, ch)| ch == wanted)
        .map(|(idx, _)| idx)
}

/// Index of the `}` that closes a body whose opening `{` was already consumed.
///
/// Nested braces are counted. Braces inside strings or escaped with a
/// backslash do not count. If the body never closes, the length is returned
/// and the caller treats the rest as the body.
pub(crate) fn find_matching_brace(src: &str) -> usize {
    let mut depth = 1usize;
    for (idx, ch) in Unquoted::new(src) {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return idx;
                }
            }
            _ => {}
        }
    }
    src.len()
}

/// Split `rest` into rules and keyframes, appending to `sheet`.
///
/// `active_media`, `active_container`, and `active_scope` are cloned onto each
/// rule. A nested `@media` or `@container` is combined with its outer condition,
/// so both must match. A prelude that starts with `@` and is not recognized is
/// dropped, body included. A rule whose selector list has an invalid selector is
/// dropped too.
fn parse_css_rules(rest: &str, active_media: Option<MediaQuery>, sheet: &mut Stylesheet) {
    parse_css_rules_scoped(rest, active_media, None, None, None, sheet);
}

fn parse_statement_at_rule(stmt: &str, active_layer: Option<&str>, sheet: &mut Stylesheet) {
    let trimmed = stmt.trim();
    if let Some(names) = trimmed
        .strip_prefix("@layer")
        .or_else(|| trimmed.strip_prefix("@LAYER"))
    {
        for raw_name in names.split(',') {
            let name = raw_name.trim();
            if !name.is_empty() {
                let full = qualify_layer_name(active_layer, name);
                intern_layer(&mut sheet.layers, &full);
            }
        }
    }
}

fn parse_css_rules_scoped(
    mut rest: &str,
    active_media: Option<MediaQuery>,
    active_container: Option<MediaQuery>,
    active_scope: Option<ScopeCondition>,
    active_layer: Option<String>,
    sheet: &mut Stylesheet,
) {
    while let Some(mut open) = find_unquoted(rest, '{') {
        // Statement at-rules such as `@layer a, b;`, `@import`, or `@charset`
        // end at `;` with no block.
        while rest.trim_start().starts_with('@')
            && let Some(semi) = find_unquoted(&rest[..open], ';')
        {
            parse_statement_at_rule(&rest[..semi], active_layer.as_deref(), sheet);
            rest = &rest[semi + 1..];
            let Some(next_open) = find_unquoted(rest, '{') else {
                while rest.trim_start().starts_with('@')
                    && let Some(tail_semi) = find_unquoted(rest, ';')
                {
                    parse_statement_at_rule(&rest[..tail_semi], active_layer.as_deref(), sheet);
                    rest = &rest[tail_semi + 1..];
                }
                return;
            };
            open = next_open;
        }
        let selector_src = rest[..open].trim();
        rest = &rest[open + 1..];
        let close = find_matching_brace(rest);
        let body_src = &rest[..close];
        rest = if close < rest.len() {
            &rest[close + 1..]
        } else {
            ""
        };
        if let Some(cond) = selector_src
            .strip_prefix("@media")
            .or_else(|| selector_src.strip_prefix("@MEDIA"))
        {
            let mq = MediaQuery::parse(cond);
            let combined = match &active_media {
                Some(outer) => outer.clone().and(&mq),
                None => mq,
            };
            parse_css_rules_scoped(
                body_src,
                Some(combined),
                active_container.clone(),
                active_scope.clone(),
                active_layer.clone(),
                sheet,
            );
            continue;
        }
        if let Some(cond) = selector_src
            .strip_prefix("@container")
            .or_else(|| selector_src.strip_prefix("@CONTAINER"))
        {
            let cq = parse_container_query(cond);
            let combined = match &active_container {
                Some(outer) => outer.clone().and(&cq),
                None => cq,
            };
            parse_css_rules_scoped(
                body_src,
                active_media.clone(),
                Some(combined),
                active_scope.clone(),
                active_layer.clone(),
                sheet,
            );
            continue;
        }
        if let Some(cond) = selector_src
            .strip_prefix("@scope")
            .or_else(|| selector_src.strip_prefix("@SCOPE"))
        {
            if let Some(scope_cond) = ScopeCondition::parse(cond) {
                parse_css_rules_scoped(
                    body_src,
                    active_media.clone(),
                    active_container.clone(),
                    Some(scope_cond),
                    active_layer.clone(),
                    sheet,
                );
            }
            continue;
        }
        if let Some(cond) = selector_src
            .strip_prefix("@supports")
            .or_else(|| selector_src.strip_prefix("@SUPPORTS"))
        {
            let opts = prism::Options::default();
            if let Ok(cvs) = prism::parse_component_values(cond.trim(), &opts)
                && prism::media::parse_supports_condition(&cvs, &opts.limits)
                    .is_ok_and(|c| c.evaluate())
            {
                parse_css_rules_scoped(
                    body_src,
                    active_media.clone(),
                    active_container.clone(),
                    active_scope.clone(),
                    active_layer.clone(),
                    sheet,
                );
            }
            continue;
        }
        if let Some(name) = selector_src.strip_prefix("@keyframes") {
            sheet
                .keyframes
                .extend(animation::parse_keyframes(name, body_src));
            continue;
        }
        if let Some(cond) = selector_src
            .strip_prefix("@layer")
            .or_else(|| selector_src.strip_prefix("@LAYER"))
        {
            let name = cond.trim();
            let full = if name.is_empty() {
                let anon = format!("<anon:{}>", sheet.layers.len());
                qualify_layer_name(active_layer.as_deref(), &anon)
            } else {
                qualify_layer_name(active_layer.as_deref(), name)
            };
            intern_layer(&mut sheet.layers, &full);
            parse_css_rules_scoped(
                body_src,
                active_media.clone(),
                active_container.clone(),
                active_scope.clone(),
                Some(full),
                sheet,
            );
            continue;
        }
        if let Some(prop_name) = selector_src
            .strip_prefix("@property")
            .or_else(|| selector_src.strip_prefix("@PROPERTY"))
        {
            let name = prop_name.trim();
            if name.starts_with("--") && name.len() > 2 && !name.chars().any(char::is_whitespace) {
                let decls = parse_declarations(body_src);
                let mut syntax = "*".to_string();
                let mut inherits = None;
                let mut initial_value = None;
                for d in decls {
                    match d.name.as_str() {
                        "syntax" => {
                            let v = d.value.trim().trim_matches(['"', '\'']).trim();
                            if !v.is_empty() {
                                syntax = v.to_string();
                            }
                        }
                        "inherits" => {
                            if d.value.eq_ignore_ascii_case("true") {
                                inherits = Some(true);
                            } else if d.value.eq_ignore_ascii_case("false") {
                                inherits = Some(false);
                            }
                        }
                        "initial-value" => {
                            initial_value = Some(d.value.trim().to_string());
                        }
                        _ => {}
                    }
                }
                if let Some(inh) = inherits {
                    let prop = RegisteredProperty {
                        name: name.to_ascii_lowercase(),
                        syntax,
                        inherits: inh,
                        initial_value,
                    };
                    if let Some(existing) =
                        sheet.properties.iter_mut().find(|p| p.name == prop.name)
                    {
                        *existing = prop;
                    } else {
                        sheet.properties.push(prop);
                    }
                }
            }
            continue;
        }
        if selector_src.starts_with("@starting-style")
            || selector_src.starts_with("@STARTING-STYLE")
        {
            parse_css_rules_scoped(
                body_src,
                active_media.clone(),
                active_container.clone(),
                active_scope.clone(),
                active_layer.clone(),
                sheet,
            );
            continue;
        }
        if selector_src.eq_ignore_ascii_case("@font-face") {
            let decls = parse_declarations(body_src);
            let mut family = None;
            let mut src_url = String::new();
            let mut weight = 400u16;
            let mut italic = false;
            for d in decls {
                match d.name.as_str() {
                    "font-family" => {
                        let f = d.value.trim().trim_matches(['"', '\'']).trim();
                        if !f.is_empty() {
                            family = Some(f.to_string());
                        }
                    }
                    "src" => {
                        let lower = d.value.to_ascii_lowercase();
                        if let Some(u_idx) = lower.find("url(") {
                            let after = &d.value[u_idx + 4..];
                            if let Some(close_idx) = after.find(')') {
                                src_url = after[..close_idx]
                                    .trim()
                                    .trim_matches(['"', '\''])
                                    .trim()
                                    .to_string();
                            }
                        } else {
                            src_url = d.value.trim().to_string();
                        }
                    }
                    "font-weight" => {
                        let lw = d.value.trim().to_ascii_lowercase();
                        if lw == "bold" {
                            weight = 700;
                        } else if let Ok(w) = lw.parse::<u16>() {
                            weight = w.clamp(1, 1000);
                        }
                    }
                    "font-style" => {
                        let ls = d.value.trim().to_ascii_lowercase();
                        italic = matches!(ls.as_str(), "italic" | "oblique");
                    }
                    _ => {}
                }
            }
            if let Some(fam) = family {
                sheet.font_faces.push(FontFaceRule {
                    family: fam,
                    src_url,
                    weight,
                    italic,
                });
            }
            continue;
        }
        if selector_src.starts_with('@') {
            continue;
        }
        emit_style_rule_with_nesting(
            selector_src,
            body_src,
            &active_media,
            &active_container,
            &active_scope,
            &active_layer,
            sheet,
            0,
        );
    }
    while rest.trim_start().starts_with('@')
        && let Some(semi) = find_unquoted(rest, ';')
    {
        parse_statement_at_rule(&rest[..semi], active_layer.as_deref(), sheet);
        rest = &rest[semi + 1..];
    }
}

const MAX_CSS_NESTING_DEPTH: usize = 16;

#[allow(clippy::too_many_arguments)]
fn emit_style_rule_with_nesting(
    selector_src: &str,
    body_src: &str,
    active_media: &Option<MediaQuery>,
    active_container: &Option<MediaQuery>,
    active_scope: &Option<ScopeCondition>,
    active_layer: &Option<String>,
    sheet: &mut Stylesheet,
    depth: usize,
) {
    if depth > MAX_CSS_NESTING_DEPTH {
        return;
    }
    let Some(selectors) = parse_selectors(selector_src) else {
        return;
    };
    if selectors.is_empty() {
        return;
    }
    if find_unquoted(body_src, '{').is_none() {
        let layer = active_layer
            .as_deref()
            .map(|name| intern_layer(&mut sheet.layers, name));
        sheet.rules.push(Rule {
            selectors,
            declarations: parse_declarations(body_src),
            media: active_media.clone(),
            container: active_container.clone(),
            scope: active_scope.clone(),
            layer,
        });
        return;
    }

    let (own_decls_src, nested_blocks) = split_rule_body_and_nested(body_src);
    let declarations = parse_declarations(&own_decls_src);
    if !declarations.is_empty() || nested_blocks.is_empty() {
        let layer = active_layer
            .as_deref()
            .map(|name| intern_layer(&mut sheet.layers, name));
        sheet.rules.push(Rule {
            selectors,
            declarations,
            media: active_media.clone(),
            container: active_container.clone(),
            scope: active_scope.clone(),
            layer,
        });
    }

    for (child_prelude, child_body) in nested_blocks {
        let trimmed = child_prelude.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('@') {
            let wrapped = format!("{trimmed} {{ {selector_src} {{ {child_body} }} }}");
            parse_css_rules_scoped(
                &wrapped,
                active_media.clone(),
                active_container.clone(),
                active_scope.clone(),
                active_layer.clone(),
                sheet,
            );
        } else {
            let combined = combine_nested_selectors(selector_src, trimmed);
            if !combined.is_empty() {
                emit_style_rule_with_nesting(
                    &combined,
                    &child_body,
                    active_media,
                    active_container,
                    active_scope,
                    active_layer,
                    sheet,
                    depth + 1,
                );
            }
        }
    }
}

fn split_rule_body_and_nested(body_src: &str) -> (String, Vec<(String, String)>) {
    let mut own_decls = String::new();
    let mut nested = Vec::new();
    let mut rest = body_src;
    while !rest.is_empty() {
        let next_open = find_unquoted(rest, '{');
        let next_semi = find_unquoted(rest, ';');
        match (next_semi, next_open) {
            (Some(semi), Some(open)) if semi < open => {
                own_decls.push_str(&rest[..=semi]);
                rest = &rest[semi + 1..];
            }
            (Some(semi), None) => {
                own_decls.push_str(&rest[..=semi]);
                rest = &rest[semi + 1..];
            }
            (_, Some(open)) => {
                let head = rest[..open].trim();
                let body_rest = &rest[open + 1..];
                let close = find_matching_brace(body_rest);
                let inner_body = &body_rest[..close];
                let after_close = if close < body_rest.len() {
                    &body_rest[close + 1..]
                } else {
                    ""
                };
                if head.starts_with("--") && head.contains(':') {
                    own_decls.push_str(&rest[..open + 1 + close.min(body_rest.len())]);
                    if close < body_rest.len() {
                        own_decls.push('}');
                    }
                    rest = after_close;
                } else {
                    nested.push((head.to_string(), inner_body.to_string()));
                    rest = after_close;
                }
            }
            (None, None) => {
                own_decls.push_str(rest);
                break;
            }
        }
    }
    (own_decls, nested)
}

fn split_selector_commas(src: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in Unquoted::new(src) {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let part = src[start..idx].trim();
                if !part.is_empty() {
                    out.push(part);
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    let tail = src[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn combine_nested_selectors(parent_src: &str, child_src: &str) -> String {
    let parents = split_selector_commas(parent_src);
    let children = split_selector_commas(child_src);
    let mut combined = Vec::new();
    for p in &parents {
        for c in &children {
            if c.contains('&') {
                combined.push(c.replace('&', p));
            } else {
                combined.push(format!("{p} {c}"));
            }
        }
    }
    combined.join(", ")
}

/// Cascade `sheet` onto `doc` at a viewport width of 960 CSS pixels.
///
/// Both inputs are borrowed. The returned map is owned. Equivalent to
/// [`compute_styles_with_viewport`] with `960.0`.
pub fn compute_styles(doc: &Document, sheet: &Stylesheet) -> StyleMap {
    compute_styles_with_viewport(doc, sheet, 960.0)
}

/// Cascade `sheet` onto `doc` for `viewport_width` in CSS pixels.
///
/// The viewport height stored for `vh` is the same number as the width.
/// Call [`compute_styles_for_viewport`] when the window height is known.
pub fn compute_styles_with_viewport(
    doc: &Document,
    sheet: &Stylesheet,
    viewport_width: f32,
) -> StyleMap {
    compute_styles_for_viewport(doc, sheet, viewport_width, viewport_width)
}

/// Cascade `sheet` onto `doc` for a window `viewport_width` by `viewport_height`.
///
/// `vw` is a percentage of the width. `vh` is a percentage of the height.
/// `vmin` uses the smaller side and `vmax` the larger. A negative side is
/// treated as 0. `@media` still tests the width only.
pub fn compute_styles_for_viewport(
    doc: &Document,
    sheet: &Stylesheet,
    viewport_width: f32,
    viewport_height: f32,
) -> StyleMap {
    cascade_document(doc, sheet, viewport_width, viewport_height, None)
}

/// Cascade `sheet` onto `doc` across multiple CPU cores when `body` has independent child subtrees.
///
/// Produces bit-exact output matching [`compute_styles_for_viewport`].
pub fn compute_styles_parallel(
    doc: &Document,
    sheet: &Stylesheet,
    viewport_width: f32,
    viewport_height: f32,
) -> StyleMap {
    let viewport_width = viewport_width.max(0.0);
    let viewport_height = viewport_height.max(0.0);
    let total_nodes = doc_len(doc);
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(32);
    let body_id = doc.body();
    let body_children: Vec<NodeId> = doc.children(body_id).to_vec();
    if workers <= 1 || body_children.len() < 2 {
        return compute_styles_for_viewport(doc, sheet, viewport_width, viewport_height);
    }
    let mut styles = vec![ComputedStyle::initial(); total_nodes];
    let html_id = doc.html();
    let mut root_vars = inherit_custom_properties(&HashMap::new(), &sheet.properties);
    let shadow_sheets = build_shadow_sheets(doc);
    let index = selectors::DocIndex::build(doc);
    let matcher = selectors::Matcher::new(doc, Some(&index));
    let mut cx = Cascade {
        sheet,
        shadow_sheets: &shadow_sheets,
        viewport_width,
        root_font_px: 16.0,
        animator: None,
        matcher: &matcher,
    };
    if let NodeData::Element { tag, .. } = doc.get(html_id).data() {
        let html_style = cascade_element(
            doc,
            html_id,
            tag,
            &ComputedStyle::initial(),
            &mut root_vars,
            &cx,
            viewport_width,
        );
        if tag == "html" {
            cx.root_font_px = html_style.font_size;
        }
        if let Some(slot) = styles.get_mut(html_id.index()) {
            *slot = html_style;
        }
        let html_container_w = resolve_child_container_width(
            &html_style,
            viewport_width,
            cx.root_font_px,
            viewport_width,
        );
        for &child in doc.children(html_id) {
            if child != body_id {
                walk(
                    doc,
                    child,
                    &html_style,
                    &html_style,
                    &root_vars,
                    &mut cx,
                    html_container_w,
                    &mut styles,
                );
            }
        }
        let mut body_vars = inherit_custom_properties(&root_vars, &sheet.properties);
        let body_style = match doc.get(body_id).data() {
            NodeData::Element { tag: btag, .. } => cascade_element(
                doc,
                body_id,
                btag,
                &html_style,
                &mut body_vars,
                &cx,
                html_container_w,
            ),
            _ => html_style,
        };
        if let Some(slot) = styles.get_mut(body_id.index()) {
            *slot = body_style;
        }
        let root_font_px = cx.root_font_px;
        let body_container_w = resolve_child_container_width(
            &body_style,
            html_container_w,
            root_font_px,
            viewport_width,
        );
        let index = &index;
        let shadow_sheets_ref = &shadow_sheets;
        let chunk_size = body_children.len().div_ceil(workers);
        let branch_results: Vec<Vec<(usize, ComputedStyle)>> = std::thread::scope(|s| {
            let mut handles = Vec::new();
            for chunk in body_children.chunks(chunk_size) {
                let body_vars_ref = &body_vars;
                handles.push(s.spawn(move || {
                    let mut local = Vec::new();
                    let matcher = selectors::Matcher::new(doc, Some(index));
                    let worker_cx = Cascade {
                        sheet,
                        shadow_sheets: shadow_sheets_ref,
                        viewport_width,
                        root_font_px,
                        animator: None,
                        matcher: &matcher,
                    };
                    for &child_id in chunk {
                        walk_subtree_into(
                            doc,
                            child_id,
                            &body_style,
                            body_vars_ref,
                            &worker_cx,
                            body_container_w,
                            &mut local,
                        );
                    }
                    local
                }));
            }
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_default())
                .collect()
        });
        for group in branch_results {
            for (idx, st) in group {
                if let Some(slot) = styles.get_mut(idx) {
                    *slot = st;
                }
            }
        }
        let (before_styles, after_styles) =
            cascade_pseudo_elements(doc, sheet, viewport_width, root_font_px, &styles, &matcher);
        return StyleMap {
            styles,
            before_styles,
            after_styles,
            root_font_px,
            viewport_width,
            viewport_height,
        };
    }
    compute_styles_for_viewport(doc, sheet, viewport_width, viewport_height)
}

fn inherit_custom_properties(
    parent_vars: &HashMap<String, String>,
    registered: &[RegisteredProperty],
) -> HashMap<String, String> {
    let mut vars = parent_vars.clone();
    for prop in registered {
        if !prop.inherits {
            vars.remove(&prop.name);
            if let Some(init) = &prop.initial_value {
                vars.insert(prop.name.clone(), init.clone());
            }
        } else if !vars.contains_key(&prop.name)
            && let Some(init) = &prop.initial_value
        {
            vars.insert(prop.name.clone(), init.clone());
        }
    }
    vars
}

fn walk_subtree_into(
    doc: &Document,
    id: NodeId,
    parent: &ComputedStyle,
    parent_vars: &HashMap<String, String>,
    cx: &Cascade,
    container_width: f32,
    out: &mut Vec<(usize, ComputedStyle)>,
) {
    let mut vars = inherit_custom_properties(parent_vars, &cx.sheet.properties);
    let style = match doc.get(id).data() {
        NodeData::Element { tag, .. } => {
            cascade_element(doc, id, tag, parent, &mut vars, cx, container_width)
        }
        _ => *parent,
    };
    out.push((id.index(), style));
    let child_container_w =
        resolve_child_container_width(&style, container_width, cx.root_font_px, cx.viewport_width);
    for &child in doc.children(id) {
        walk_subtree_into(doc, child, &style, &vars, cx, child_container_w, out);
    }
}

/// Like [`compute_styles_for_viewport`], with transitions run by `animator`
/// at its current time.
///
/// Call [`Animator::tick`] first to move the time. Each element's freshly
/// cascaded style is compared with the one from the animator's previous
/// pass; changed properties with a `transition` start moving, and running
/// transitions are written into the result before children inherit from
/// it. The first pass for an element starts nothing. See [`Animator`] for
/// a frame loop.
pub fn compute_styles_animated(
    doc: &Document,
    sheet: &Stylesheet,
    viewport_width: f32,
    viewport_height: f32,
    animator: &mut Animator,
) -> StyleMap {
    animator.begin_pass();
    let styles = cascade_document(doc, sheet, viewport_width, viewport_height, Some(animator));
    animator.end_pass();
    styles
}

/// Incrementally re-cascade dirty subtrees recorded on `doc` (plus any elements
/// with active transitions or keyframe animations in `animator`) into `styles`.
///
/// Consumes `doc.take_dirty_style_nodes()` and re-cascades only the affected
/// subtrees, extending `styles` if new nodes were allocated. When an [`Animator`]
/// is provided, any property change on a re-cascaded element with a `transition`
/// starts a transition from its previous computed value.
/// Returns the number of element nodes re-cascaded in this pass.
pub fn compute_styles_incremental(
    doc: &mut Document,
    sheet: &Stylesheet,
    styles: &mut StyleMap,
    viewport_width: f32,
    viewport_height: f32,
    mut animator: Option<&mut Animator>,
) -> usize {
    let viewport_width = viewport_width.max(0.0);
    let viewport_height = viewport_height.max(0.0);
    let mut dirty = doc.take_dirty_style_nodes();
    if let Some(anim) = animator.as_deref() {
        for active_id in anim.active_nodes() {
            if !dirty.contains(&active_id) {
                dirty.push(active_id);
            }
        }
    }
    let total_nodes = doc_len(doc);
    if dirty.is_empty()
        && styles.styles.len() >= total_nodes
        && (styles.viewport_width - viewport_width).abs() < f32::EPSILON
        && (styles.viewport_height - viewport_height).abs() < f32::EPSILON
    {
        return 0;
    }
    if styles.styles.is_empty()
        || dirty.contains(&doc.root())
        || dirty.contains(&doc.html())
        || (styles.viewport_width - viewport_width).abs() >= f32::EPSILON
        || (styles.viewport_height - viewport_height).abs() >= f32::EPSILON
    {
        if let Some(anim) = animator.as_deref_mut() {
            anim.begin_pass();
            for (idx, existing) in styles.styles.iter().enumerate() {
                anim.seed_if_absent(NodeId::from_raw(idx as u32), existing);
            }
        }
        *styles = cascade_document(doc, sheet, viewport_width, viewport_height, animator);
        return total_nodes;
    }

    if styles.styles.len() < total_nodes {
        styles.styles.resize(total_nodes, ComputedStyle::initial());
    }
    if let Some(anim) = animator.as_deref_mut() {
        anim.begin_pass();
        for &d in &dirty {
            if let Some(existing) = styles.styles.get(d.index()) {
                anim.seed_if_absent(d, existing);
            }
        }
    }

    // Normalize dirty nodes: promote text nodes to their parent element and keep
    // only top-most dirty roots so each affected subtree is walked once.
    let mut roots: Vec<NodeId> = Vec::new();
    for d in dirty {
        if d.index() >= total_nodes {
            continue;
        }
        let elem_id = if doc.get(d).data().tag().is_some() {
            d
        } else {
            doc.parent_element(d).unwrap_or(doc.body())
        };
        if !roots.contains(&elem_id) {
            roots.push(elem_id);
        }
    }
    let roots_snapshot = roots.clone();
    roots.retain(|&cand| {
        !roots_snapshot
            .iter()
            .any(|&anc| anc != cand && doc.contains_node(anc, cand))
    });

    let shadow_sheets = build_shadow_sheets(doc);
    let index = selectors::DocIndex::build(doc);
    let matcher = selectors::Matcher::new(doc, Some(&index));
    let root_font_px = styles
        .styles
        .get(doc.html().index())
        .map(|s| s.font_size)
        .unwrap_or(16.0);
    let mut cx = Cascade {
        sheet,
        shadow_sheets: &shadow_sheets,
        viewport_width,
        root_font_px,
        animator,
        matcher: &matcher,
    };

    let mut updated_count = 0usize;
    for sub_root in roots {
        let mut ancestors = Vec::new();
        let mut cursor = doc.parent_element(sub_root);
        while let Some(anc) = cursor {
            ancestors.push(anc);
            cursor = doc.parent_element(anc);
        }
        ancestors.reverse();
        let mut parent_vars = inherit_custom_properties(&HashMap::new(), &sheet.properties);
        let mut container_w = viewport_width;
        let mut parent_style = ComputedStyle::initial();
        let mut parent_base = ComputedStyle::initial();
        for anc in ancestors {
            if let Some(anim) = cx.animator.as_deref_mut()
                && let Some(existing) = styles.styles.get(anc.index())
            {
                anim.seed_if_absent(anc, existing);
            }
            let (st, base, vars, next_w) = walk_node_step(
                doc,
                anc,
                &parent_style,
                &parent_base,
                &parent_vars,
                &mut cx,
                container_w,
                &mut styles.styles,
            );
            parent_style = st;
            parent_base = base;
            parent_vars = vars;
            container_w = next_w;
        }
        updated_count += walk_incremental(
            doc,
            sub_root,
            &parent_style,
            &parent_base,
            &parent_vars,
            &mut cx,
            container_w,
            &mut styles.styles,
        );
    }
    styles.root_font_px = cx.root_font_px;
    let (before_styles, after_styles) = cascade_pseudo_elements(
        doc,
        sheet,
        viewport_width,
        cx.root_font_px,
        &styles.styles,
        &matcher,
    );
    styles.before_styles = before_styles;
    styles.after_styles = after_styles;
    updated_count
}

#[allow(clippy::too_many_arguments)]
fn walk_incremental(
    doc: &Document,
    id: NodeId,
    parent: &ComputedStyle,
    parent_base: &ComputedStyle,
    parent_vars: &HashMap<String, String>,
    cx: &mut Cascade,
    container_width: f32,
    out: &mut [ComputedStyle],
) -> usize {
    if let Some(anim) = cx.animator.as_deref_mut()
        && let Some(existing) = out.get(id.index())
    {
        anim.seed_if_absent(id, existing);
    }
    let (style, base, vars, child_container_w) = walk_node_step(
        doc,
        id,
        parent,
        parent_base,
        parent_vars,
        cx,
        container_width,
        out,
    );
    let mut count = usize::from(doc.get(id).data().tag().is_some());
    let children: Vec<NodeId> = doc.children(id).to_vec();
    for child in children {
        count += walk_incremental(doc, child, &style, &base, &vars, cx, child_container_w, out);
    }
    count
}

fn build_shadow_sheets(doc: &Document) -> Vec<(NodeId, NodeId, Stylesheet)> {
    let mut out = Vec::new();
    for (host, sr, _) in doc.shadow_root_entries() {
        let css = doc.shadow_css(sr);
        if !css.trim().is_empty() {
            out.push((host, sr, parse_css(&css)));
        }
    }
    out
}

/// Shared body of the `compute_styles` entry points.
fn cascade_document(
    doc: &Document,
    sheet: &Stylesheet,
    viewport_width: f32,
    viewport_height: f32,
    animator: Option<&mut Animator>,
) -> StyleMap {
    let viewport_width = viewport_width.max(0.0);
    let viewport_height = viewport_height.max(0.0);
    let mut styles = vec![ComputedStyle::initial(); doc_len(doc)];
    let shadow_sheets = build_shadow_sheets(doc);
    let index = selectors::DocIndex::build(doc);
    let matcher = selectors::Matcher::new(doc, Some(&index));
    let mut cx = Cascade {
        sheet,
        shadow_sheets: &shadow_sheets,
        viewport_width,
        root_font_px: 16.0,
        animator,
        matcher: &matcher,
    };
    walk(
        doc,
        doc.root(),
        &ComputedStyle::initial(),
        &ComputedStyle::initial(),
        &HashMap::new(),
        &mut cx,
        viewport_width,
        &mut styles,
    );
    let mut root_font_px = cx.root_font_px;
    if let Some(html_style) = styles.get(doc.html().index()) {
        root_font_px = html_style.font_size;
    }
    let (before_styles, after_styles) =
        cascade_pseudo_elements(doc, sheet, viewport_width, root_font_px, &styles, &matcher);
    StyleMap {
        styles,
        before_styles,
        after_styles,
        root_font_px,
        viewport_width,
        viewport_height,
    }
}

#[allow(clippy::type_complexity)]
fn cascade_pseudo_elements(
    doc: &Document,
    sheet: &Stylesheet,
    viewport_width: f32,
    root_font_px: f32,
    styles: &[ComputedStyle],
    matcher: &selectors::Matcher,
) -> (
    HashMap<NodeId, (String, ComputedStyle)>,
    HashMap<NodeId, (String, ComputedStyle)>,
) {
    let mut before_styles = HashMap::new();
    let mut after_styles = HashMap::new();
    let has_pseudo_rules = sheet.rules.iter().any(|rule| {
        rule.media
            .as_ref()
            .is_none_or(|mq| mq.matches(viewport_width))
            && rule
                .selectors
                .iter()
                .any(|s| s.pseudo_before_or_after().is_some())
    });
    if !has_pseudo_rules {
        return (before_styles, after_styles);
    }

    let mut stack = vec![doc.root()];
    while let Some(id) = stack.pop() {
        for &child in doc.children(id).iter().rev() {
            stack.push(child);
        }
        let NodeData::Element { .. } = doc.get(id).data() else {
            continue;
        };
        let Some(host_style) = styles.get(id.index()) else {
            continue;
        };
        if host_style.display == Display::None {
            continue;
        }
        for is_after in [false, true] {
            let mut matched: Vec<((bool, (u16, u16, u16), usize, usize), &Declaration)> =
                Vec::new();
            for (rule_idx, rule) in sheet.rules.iter().enumerate() {
                if let Some(mq) = &rule.media
                    && !mq.matches(viewport_width)
                {
                    continue;
                }
                let scope_root = match &rule.scope {
                    Some(scope) => match scope_proximity(doc, id, scope, matcher) {
                        Some((_, root_id)) => Some(root_id),
                        None => continue,
                    },
                    None => None,
                };
                let spec = rule
                    .selectors
                    .iter()
                    .filter(|sel| matcher.matches_pseudo_with_scope(sel, id, is_after, scope_root))
                    .map(Selector::specificity)
                    .max();
                if let Some(specificity) = spec {
                    for (decl_idx, decl) in rule.declarations.iter().enumerate() {
                        matched.push(((decl.important, specificity, rule_idx, decl_idx), decl));
                    }
                }
            }
            if matched.is_empty() {
                continue;
            }
            matched.sort_by_key(|(k, _)| *k);
            let mut pseudo_style = host_style.inherited_child();
            let mut vars = HashMap::new();
            let mut content: Option<String> = None;
            for (_, decl) in matched {
                if decl.name == "content" {
                    content = resolve_pseudo_content(&decl.value, doc, id);
                } else {
                    apply_declaration(
                        &mut pseudo_style,
                        &mut vars,
                        decl,
                        host_style.font_size,
                        root_font_px,
                    );
                }
            }
            if let Some(text) = content
                && pseudo_style.display != Display::None
            {
                if is_after {
                    after_styles.insert(id, (text, pseudo_style));
                } else {
                    before_styles.insert(id, (text, pseudo_style));
                }
            }
        }
    }
    (before_styles, after_styles)
}

fn resolve_pseudo_content(raw: &str, doc: &Document, id: NodeId) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("none")
        || trimmed.eq_ignore_ascii_case("normal")
    {
        return None;
    }
    let mut out = String::new();
    let mut matched_token = false;
    let mut chars = trimmed.chars().peekable();
    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }
        if ch == '"' || ch == '\'' {
            let quote = ch;
            chars.next();
            matched_token = true;
            while let Some(c) = chars.next() {
                if c == '\\' {
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                } else if c == quote {
                    break;
                } else {
                    out.push(c);
                }
            }
            continue;
        }
        let mut word = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() || c == '"' || c == '\'' || c == '(' {
                break;
            }
            word.push(c);
            chars.next();
        }
        if word.eq_ignore_ascii_case("attr") && chars.peek() == Some(&'(') {
            chars.next();
            let mut attr_name = String::new();
            for c in chars.by_ref() {
                if c == ')' {
                    break;
                }
                attr_name.push(c);
            }
            let attr_key = attr_name.trim();
            if !attr_key.is_empty() {
                matched_token = true;
                if let Some(val) = doc.get(id).data().attr(attr_key) {
                    out.push_str(val);
                }
            }
        } else if word.eq_ignore_ascii_case("open-quote") {
            matched_token = true;
            out.push('\u{201C}');
        } else if word.eq_ignore_ascii_case("close-quote") {
            matched_token = true;
            out.push('\u{201D}');
        } else {
            break;
        }
    }
    matched_token.then_some(out)
}

/// What stays the same across one cascade walk.
struct Cascade<'a> {
    sheet: &'a Stylesheet,
    shadow_sheets: &'a [(NodeId, NodeId, Stylesheet)],
    viewport_width: f32,
    /// Font size of `html` once it is styled; 16 before that.
    root_font_px: f32,
    animator: Option<&'a mut Animator>,
    /// Selector matching with caches for this pass, on this thread.
    matcher: &'a selectors::Matcher<'a>,
}

/// One past the largest node index reachable from the root.
/// Unreachable detached nodes do not count. `doc` is borrowed.
fn doc_len(doc: &Document) -> usize {
    let mut count = 0;
    let mut stack = vec![doc.root()];
    while let Some(id) = stack.pop() {
        count = count.max(id.index() + 1);
        stack.extend(doc.children(id).iter().copied());
    }
    count
}

fn resolve_child_container_width(
    style: &ComputedStyle,
    parent_container_w: f32,
    root_font_px: f32,
    viewport_width: f32,
) -> f32 {
    let resolve_sz = |sz: Size| -> Option<f32> {
        match sz {
            Size::Px(px) if px.is_finite() => Some(px.max(0.0)),
            Size::Percent(pct) if pct.is_finite() => {
                Some((parent_container_w * pct / 100.0).max(0.0))
            }
            Size::Em(em) if em.is_finite() => Some((style.font_size * em).max(0.0)),
            Size::Rem(rem) if rem.is_finite() => Some((root_font_px * rem).max(0.0)),
            Size::Vw(vw) if vw.is_finite() => Some((viewport_width * vw / 100.0).max(0.0)),
            Size::Calc {
                px,
                percent,
                em,
                rem,
            } => Some(
                (px + parent_container_w * percent / 100.0
                    + style.font_size * em
                    + root_font_px * rem)
                    .max(0.0),
            ),
            _ => None,
        }
    };
    let mut w = resolve_sz(style.width).unwrap_or(parent_container_w);
    if let Some(max_w) = resolve_sz(style.max_width) {
        w = w.min(max_w);
    }
    if let Some(min_w) = resolve_sz(style.min_width) {
        w = w.max(min_w);
    }
    w
}

fn scope_proximity(
    doc: &Document,
    id: NodeId,
    scope: &ScopeCondition,
    matcher: &selectors::Matcher,
) -> Option<(u16, NodeId)> {
    let mut cur = Some(id);
    let mut hops = 0u16;
    while let Some(node_id) = cur {
        let is_root = match &scope.root {
            Some(roots) => roots
                .iter()
                .any(|s| matcher.matches_with_scope(s, node_id, Some(node_id))),
            None => node_id == doc.html(),
        };
        if is_root {
            return Some((hops, node_id));
        }
        if let Some(limits) = &scope.limit
            && limits.iter().any(|s| matcher.matches(s, node_id))
        {
            return None;
        }
        hops = hops.saturating_add(1);
        cur = doc.parent_element(node_id);
    }
    None
}

/// Write `id`'s used style and recurse to its children.
///
/// Elements start from [`ComputedStyle::inherited_child`], then user-agent
/// defaults, then author rules, then the inline `style` attribute. Text and
/// the document node copy `parent` unchanged. `out` must be long enough for
/// every reachable index; a short slot is skipped.
///
/// `parent_base` is the parent's style before animation. With an animator,
/// each element is cascaded from it for change detection, and again from
/// the animated `parent` when the two differ, so children inherit animated
/// values without that counting as a change of their own.
#[allow(clippy::too_many_arguments)]
fn walk(
    doc: &Document,
    id: NodeId,
    parent: &ComputedStyle,
    parent_base: &ComputedStyle,
    parent_vars: &HashMap<String, String>,
    cx: &mut Cascade,
    container_width: f32,
    out: &mut [ComputedStyle],
) {
    let (style, base, vars, child_container_w) = walk_node_step(
        doc,
        id,
        parent,
        parent_base,
        parent_vars,
        cx,
        container_width,
        out,
    );
    let children: Vec<NodeId> = doc.children(id).to_vec();
    for child in children {
        walk(doc, child, &style, &base, &vars, cx, child_container_w, out);
    }
}

#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn walk_node_step(
    doc: &Document,
    id: NodeId,
    parent: &ComputedStyle,
    parent_base: &ComputedStyle,
    parent_vars: &HashMap<String, String>,
    cx: &mut Cascade,
    container_width: f32,
    out: &mut [ComputedStyle],
) -> (ComputedStyle, ComputedStyle, HashMap<String, String>, f32) {
    let mut vars = inherit_custom_properties(parent_vars, &cx.sheet.properties);
    let (style, base) = match doc.get(id).data() {
        NodeData::Element { tag, .. } => {
            let base = cascade_element(doc, id, tag, parent_base, &mut vars, cx, container_width);
            if tag == "html" {
                cx.root_font_px = base.font_size;
            }
            let style = if cx.animator.is_none() {
                base
            } else {
                let mut style = if parent == parent_base {
                    base
                } else {
                    let mut scratch = inherit_custom_properties(parent_vars, &cx.sheet.properties);
                    cascade_element(doc, id, tag, parent, &mut scratch, cx, container_width)
                };
                let input = animation::AnimInput {
                    keyframes: &cx.sheet.keyframes,
                    vars: &vars,
                    parent_font_px: parent.font_size,
                    root_font_px: cx.root_font_px,
                };
                if let Some(animator) = cx.animator.as_deref_mut() {
                    animator.apply(id, &base, &mut style, &input);
                }
                style
            };
            (style, base)
        }
        _ => (*parent, *parent_base),
    };
    if let Some(slot) = out.get_mut(id.index()) {
        *slot = style;
    }
    let child_container_w =
        resolve_child_container_width(&style, container_width, cx.root_font_px, cx.viewport_width);
    (style, base, vars, child_container_w)
}

/// Cascade one element from `parent`: inherited values, user-agent
/// defaults, author rules, then the inline `style` attribute.
#[inline(never)]
fn cascade_element(
    doc: &Document,
    id: NodeId,
    tag: &str,
    parent: &ComputedStyle,
    vars: &mut HashMap<String, String>,
    cx: &Cascade,
    container_width: f32,
) -> ComputedStyle {
    let parent_font_px = parent.font_size;
    let inherited_vars = vars.clone();
    let inherited_style = parent.inherited_child();
    let mut style = inherited_style;
    apply_user_agent(&mut style, tag);
    let data = doc.get(id).data();
    let in_closed_details = tag != "summary"
        && doc
            .parent_element(id)
            .is_some_and(|p| doc.get(p).data().tag() == Some("details") && !doc.is_details_open(p));
    let is_closed_dialog = tag == "dialog"
        && !data
            .attr("open")
            .is_some_and(|v| !v.eq_ignore_ascii_case("false"));
    if data.attr("hidden").is_some()
        || in_closed_details
        || is_closed_dialog
        || (tag == "input"
            && data
                .attr("type")
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("hidden")))
    {
        style.display = Display::None;
        style.inline_level = false;
    }
    if let Some(align) = data.attr("align") {
        match align.trim().to_ascii_lowercase().as_str() {
            "center" | "middle" => {
                style.text_align = TextAlign::Center;
                if matches!(tag, "table" | "div" | "p" | "form") {
                    style.margin_left_auto = true;
                    style.margin_right_auto = true;
                }
            }
            "right" => style.text_align = TextAlign::Right,
            "left" => style.text_align = TextAlign::Left,
            "justify" => style.text_align = TextAlign::Justify,
            _ => {}
        }
    }
    let ua_style = style;
    apply_author(
        &mut style,
        vars,
        &inherited_vars,
        parent,
        &inherited_style,
        &ua_style,
        doc,
        id,
        cx,
        parent_font_px,
        container_width,
    );
    style
}

/// Apply author rules that match `id` and then its inline `style` attribute.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn apply_author(
    style: &mut ComputedStyle,
    vars: &mut HashMap<String, String>,
    inherited_vars: &HashMap<String, String>,
    parent: &ComputedStyle,
    inherited_style: &ComputedStyle,
    ua_style: &ComputedStyle,
    doc: &Document,
    id: NodeId,
    cx: &Cascade,
    parent_font_px: f32,
    container_width: f32,
) {
    let (sheet, viewport_width, root_font_px) = (cx.sheet, cx.viewport_width, cx.root_font_px);
    struct MatchedDecl<'a> {
        important: bool,
        layer_rank: u32,
        specificity: (u16, u16, u16),
        inv_scope_hops: u16,
        rule_order: usize,
        decl_order: usize,
        decl: &'a Declaration,
    }
    let normal_layer_rank = |layer: Option<u16>| -> u32 {
        match layer {
            Some(l) => u32::from(l),
            None => u32::from(u16::MAX),
        }
    };
    let important_layer_rank = |layer: Option<u16>| -> u32 {
        match layer {
            Some(l) => u32::from(u16::MAX).saturating_sub(u32::from(l)),
            None => 0,
        }
    };
    let inline_rank = u32::from(u16::MAX) + 1;
    let enclosing_host = if cx.shadow_sheets.is_empty() {
        None
    } else {
        doc.enclosing_shadow_host(id)
    };
    let in_scoped_shadow_tree = enclosing_host
        .is_some_and(|h| h != id && cx.shadow_sheets.iter().any(|(sh, _, _)| *sh == h));

    let mut entries: Vec<MatchedDecl> = Vec::new();
    for (order, rule) in sheet.rules.iter().enumerate() {
        if let Some(mq) = &rule.media
            && !mq.matches(viewport_width)
        {
            continue;
        }
        if let Some(cq) = &rule.container
            && !cq.matches(container_width)
        {
            continue;
        }
        let (scope_hops, scope_root) = match &rule.scope {
            Some(scope) => match scope_proximity(doc, id, scope, cx.matcher) {
                Some((hops, root_id)) => (hops, Some(root_id)),
                None => continue,
            },
            None => (u16::MAX, None),
        };
        let specificity = rule
            .selectors
            .iter()
            .filter(|selector| {
                if in_scoped_shadow_tree && !selector.has_part() {
                    return false;
                }
                cx.matcher.matches_with_scope(selector, id, scope_root)
            })
            .map(Selector::specificity)
            .max();
        if let Some(specificity) = specificity {
            let inv_hops = u16::MAX - scope_hops;
            for (decl_idx, decl) in rule.declarations.iter().enumerate() {
                let layer_rank = if decl.important {
                    important_layer_rank(rule.layer)
                } else {
                    normal_layer_rank(rule.layer)
                };
                entries.push(MatchedDecl {
                    important: decl.important,
                    layer_rank,
                    specificity,
                    inv_scope_hops: inv_hops,
                    rule_order: order,
                    decl_order: decl_idx,
                    decl,
                });
            }
        }
    }
    if !cx.shadow_sheets.is_empty() {
        let slotted_host = doc
            .assigned_slot(id)
            .and_then(|slot_id| doc.enclosing_shadow_host(slot_id));
        let mut shadow_order_base = sheet.rules.len();
        for (sh_host, sh_root, sh_sheet) in cx.shadow_sheets {
            let is_host = id == *sh_host;
            let is_inner = enclosing_host == Some(*sh_host) && id != *sh_host;
            let is_slotted = slotted_host == Some(*sh_host);
            if !is_host && !is_inner && !is_slotted {
                shadow_order_base += sh_sheet.rules.len();
                continue;
            }
            for (order, rule) in sh_sheet.rules.iter().enumerate() {
                if let Some(mq) = &rule.media
                    && !mq.matches(viewport_width)
                {
                    continue;
                }
                if let Some(cq) = &rule.container
                    && !cq.matches(container_width)
                {
                    continue;
                }
                let specificity = rule
                    .selectors
                    .iter()
                    .filter(|selector| {
                        if is_host && !selector.has_host() {
                            return false;
                        }
                        if is_slotted && !selector.has_slotted() {
                            return false;
                        }
                        if is_inner && selector.has_slotted() {
                            return false;
                        }
                        cx.matcher.matches_with_scope(selector, id, Some(*sh_root))
                    })
                    .map(Selector::specificity)
                    .max();
                if let Some(specificity) = specificity {
                    for (decl_idx, decl) in rule.declarations.iter().enumerate() {
                        let layer_rank = if decl.important {
                            important_layer_rank(rule.layer)
                        } else {
                            normal_layer_rank(rule.layer)
                        };
                        entries.push(MatchedDecl {
                            important: decl.important,
                            layer_rank,
                            specificity,
                            inv_scope_hops: u16::MAX,
                            rule_order: shadow_order_base + order,
                            decl_order: decl_idx,
                            decl,
                        });
                    }
                }
            }
            shadow_order_base += sh_sheet.rules.len();
        }
    }
    let inline = doc
        .get(id)
        .data()
        .attr("style")
        .map(parse_declarations)
        .unwrap_or_default();
    let inline_rule_order = sheet.rules.len()
        + cx.shadow_sheets
            .iter()
            .map(|(_, _, s)| s.rules.len())
            .sum::<usize>();
    for (decl_idx, decl) in inline.iter().enumerate() {
        entries.push(MatchedDecl {
            important: decl.important,
            layer_rank: inline_rank,
            specificity: (u16::MAX, u16::MAX, u16::MAX),
            inv_scope_hops: u16::MAX,
            rule_order: inline_rule_order,
            decl_order: decl_idx,
            decl,
        });
    }
    entries.sort_by_key(|e| {
        (
            e.important,
            e.layer_rank,
            e.specificity,
            e.inv_scope_hops,
            e.rule_order,
            e.decl_order,
        )
    });

    let mut prev_layer_vars = inherited_vars.clone();
    let mut current_var_layer: Option<(bool, u32)> = None;
    for entry in &entries {
        let declaration = entry.decl;
        if declaration.name.starts_with("--") {
            let key = (entry.important, entry.layer_rank);
            if current_var_layer != Some(key) {
                if current_var_layer.is_some() {
                    prev_layer_vars = vars.clone();
                }
                current_var_layer = Some(key);
            }
            let raw = declaration.value.trim();
            let reg = sheet.properties.iter().find(|p| p.name == declaration.name);
            let apply_initial_var = |vars: &mut HashMap<String, String>| {
                if let Some(r) = reg
                    && let Some(init) = &r.initial_value
                {
                    vars.insert(declaration.name.clone(), init.clone());
                } else {
                    vars.remove(&declaration.name);
                }
            };
            let apply_inherit_var = |vars: &mut HashMap<String, String>| {
                if let Some(pv) = inherited_vars.get(&declaration.name) {
                    vars.insert(declaration.name.clone(), pv.clone());
                } else {
                    apply_initial_var(vars);
                }
            };
            if raw.eq_ignore_ascii_case("initial") || raw.eq_ignore_ascii_case("revert") {
                apply_initial_var(vars);
            } else if raw.eq_ignore_ascii_case("inherit") {
                apply_inherit_var(vars);
            } else if raw.eq_ignore_ascii_case("unset") {
                if reg.is_none_or(|r| r.inherits) {
                    apply_inherit_var(vars);
                } else {
                    apply_initial_var(vars);
                }
            } else if raw.eq_ignore_ascii_case("revert-layer") {
                if let Some(plv) = prev_layer_vars.get(&declaration.name) {
                    vars.insert(declaration.name.clone(), plv.clone());
                } else {
                    vars.remove(&declaration.name);
                }
            } else {
                let resolved = resolve_css_vars(raw, vars);
                if !resolved.is_empty() {
                    vars.insert(declaration.name.clone(), resolved);
                }
            }
        }
    }

    let initial_style = ComputedStyle::initial();
    let mut prev_layer_style = *ua_style;
    let mut current_layer: Option<(bool, u32)> = None;
    for entry in &entries {
        let declaration = entry.decl;
        if declaration.name.starts_with("--") {
            continue;
        }
        let key = (entry.important, entry.layer_rank);
        if current_layer != Some(key) {
            if current_layer.is_some() {
                prev_layer_style = *style;
            }
            current_layer = Some(key);
        }
        let resolved = resolve_css_vars(declaration.value.trim(), vars);
        let val = resolved.trim();
        let kw_source = if val.eq_ignore_ascii_case("initial") {
            Some(&initial_style)
        } else if val.eq_ignore_ascii_case("inherit") {
            Some(parent)
        } else if val.eq_ignore_ascii_case("unset") {
            Some(inherited_style)
        } else if val.eq_ignore_ascii_case("revert") {
            Some(ua_style)
        } else if val.eq_ignore_ascii_case("revert-layer") {
            Some(&prev_layer_style)
        } else {
            None
        };
        if let Some(src) = kw_source {
            copy_property(style, src, declaration.name.as_str());
            continue;
        }
        apply_declaration(style, vars, declaration, parent_font_px, root_font_px);
    }
}

fn copy_property(dst: &mut ComputedStyle, src: &ComputedStyle, name: &str) {
    match name {
        "all" => *dst = *src,
        "display" => {
            dst.display = src.display;
            dst.inline_level = src.inline_level;
        }
        "position" => dst.position = src.position,
        "top" | "inset-block-start" => dst.top = src.top,
        "right" | "inset-inline-end" => dst.right = src.right,
        "bottom" | "inset-block-end" => dst.bottom = src.bottom,
        "left" | "inset-inline-start" => dst.left = src.left,
        "inset" => {
            dst.top = src.top;
            dst.right = src.right;
            dst.bottom = src.bottom;
            dst.left = src.left;
        }
        "inset-inline" => {
            dst.left = src.left;
            dst.right = src.right;
        }
        "inset-block" => {
            dst.top = src.top;
            dst.bottom = src.bottom;
        }
        "z-index" => dst.z_index = src.z_index,
        "overflow" | "overflow-x" | "overflow-y" => {
            dst.overflow = src.overflow;
            dst.scrollable_overflow = src.scrollable_overflow;
        }
        "visibility" => dst.visibility = src.visibility,
        "box-sizing" => dst.box_sizing = src.box_sizing,
        "color" => dst.color = src.color,
        "background" | "background-color" | "background-image" => {
            dst.background = src.background;
            dst.background_gradient = src.background_gradient;
        }
        "box-shadow" => dst.box_shadow = src.box_shadow,
        "font-size" => dst.font_size = src.font_size,
        "font-weight" => dst.font_weight = src.font_weight,
        "font-style" => dst.italic = src.italic,
        "line-height" => dst.line_height = src.line_height,
        "text-align" => dst.text_align = src.text_align,
        "white-space" => dst.white_space = src.white_space,
        "text-decoration" | "text-decoration-line" => {
            dst.underline = src.underline;
            dst.line_through = src.line_through;
        }
        "font-family" => dst.font_family = src.font_family,
        "text-transform" => dst.text_transform = src.text_transform,
        "letter-spacing" => dst.letter_spacing = src.letter_spacing,
        "word-spacing" => dst.word_spacing = src.word_spacing,
        "object-fit" => dst.object_fit = src.object_fit,
        "outline" => {
            dst.outline_width = src.outline_width;
            dst.outline_color = src.outline_color;
            dst.outline_offset = src.outline_offset;
        }
        "outline-width" => dst.outline_width = src.outline_width,
        "outline-color" => dst.outline_color = src.outline_color,
        "outline-offset" => dst.outline_offset = src.outline_offset,
        "backdrop-filter" => dst.backdrop_blur = src.backdrop_blur,
        "filter" => {
            dst.filter_blur = src.filter_blur;
            dst.filter_brightness = src.filter_brightness;
            dst.filter_contrast = src.filter_contrast;
            dst.filter_grayscale = src.filter_grayscale;
            dst.filter_invert = src.filter_invert;
            dst.filter_sepia = src.filter_sepia;
        }
        "text-overflow" => dst.text_overflow_ellipsis = src.text_overflow_ellipsis,
        "overflow-wrap" | "word-wrap" | "word-break" => dst.word_break_any = src.word_break_any,
        "margin" => {
            dst.margin = src.margin;
            dst.margin_top_auto = src.margin_top_auto;
            dst.margin_right_auto = src.margin_right_auto;
            dst.margin_bottom_auto = src.margin_bottom_auto;
            dst.margin_left_auto = src.margin_left_auto;
        }
        "margin-top" | "margin-block-start" => {
            dst.margin.top = src.margin.top;
            dst.margin_top_auto = src.margin_top_auto;
        }
        "margin-right" | "margin-inline-end" => {
            dst.margin.right = src.margin.right;
            dst.margin_right_auto = src.margin_right_auto;
        }
        "margin-bottom" | "margin-block-end" => {
            dst.margin.bottom = src.margin.bottom;
            dst.margin_bottom_auto = src.margin_bottom_auto;
        }
        "margin-left" | "margin-inline-start" => {
            dst.margin.left = src.margin.left;
            dst.margin_left_auto = src.margin_left_auto;
        }
        "margin-inline" => {
            dst.margin.left = src.margin.left;
            dst.margin.right = src.margin.right;
            dst.margin_left_auto = src.margin_left_auto;
            dst.margin_right_auto = src.margin_right_auto;
        }
        "margin-block" => {
            dst.margin.top = src.margin.top;
            dst.margin.bottom = src.margin.bottom;
            dst.margin_top_auto = src.margin_top_auto;
            dst.margin_bottom_auto = src.margin_bottom_auto;
        }
        "padding" => dst.padding = src.padding,
        "padding-top" | "padding-block-start" => dst.padding.top = src.padding.top,
        "padding-right" | "padding-inline-end" => dst.padding.right = src.padding.right,
        "padding-bottom" | "padding-block-end" => dst.padding.bottom = src.padding.bottom,
        "padding-left" | "padding-inline-start" => dst.padding.left = src.padding.left,
        "padding-inline" => {
            dst.padding.left = src.padding.left;
            dst.padding.right = src.padding.right;
        }
        "padding-block" => {
            dst.padding.top = src.padding.top;
            dst.padding.bottom = src.padding.bottom;
        }
        "border" => {
            dst.border = src.border;
            dst.border_color = src.border_color;
        }
        "border-width" => dst.border = src.border,
        "border-color" => {
            dst.border_color = src.border_color;
            dst.border_colors = src.border_colors;
        }
        "border-top-color" => dst.border_colors[0] = src.border_colors[0],
        "border-right-color" => dst.border_colors[1] = src.border_colors[1],
        "border-bottom-color" => dst.border_colors[2] = src.border_colors[2],
        "border-left-color" => dst.border_colors[3] = src.border_colors[3],
        "border-top" | "border-block-start" => dst.border.top = src.border.top,
        "border-right" | "border-inline-end" => dst.border.right = src.border.right,
        "border-bottom" | "border-block-end" => dst.border.bottom = src.border.bottom,
        "border-left" | "border-inline-start" => dst.border.left = src.border.left,
        "border-radius" => dst.border_radius = src.border_radius,
        "width" | "inline-size" => dst.width = src.width,
        "height" | "block-size" => dst.height = src.height,
        "min-width" | "min-inline-size" => dst.min_width = src.min_width,
        "max-width" | "max-inline-size" => dst.max_width = src.max_width,
        "min-height" | "min-block-size" => dst.min_height = src.min_height,
        "max-height" | "max-block-size" => dst.max_height = src.max_height,
        "aspect-ratio" => dst.aspect_ratio = src.aspect_ratio,
        "flex" => {
            dst.flex_grow = src.flex_grow;
            dst.flex_shrink = src.flex_shrink;
            dst.flex_basis = src.flex_basis;
        }
        "flex-direction" => dst.flex_direction = src.flex_direction,
        "flex-wrap" => dst.flex_wrap = src.flex_wrap,
        "flex-flow" => {
            dst.flex_direction = src.flex_direction;
            dst.flex_wrap = src.flex_wrap;
        }
        "order" => dst.order = src.order,
        "flex-grow" => dst.flex_grow = src.flex_grow,
        "flex-shrink" => dst.flex_shrink = src.flex_shrink,
        "flex-basis" => dst.flex_basis = src.flex_basis,
        "justify-content" => dst.justify_content = src.justify_content,
        "align-items" => dst.align_items = src.align_items,
        "align-self" => dst.align_self = src.align_self,
        "align-content" => dst.align_content = src.align_content,
        "justify-items" => dst.justify_items = src.justify_items,
        "justify-self" => dst.justify_self = src.justify_self,
        "place-items" => {
            dst.align_items = src.align_items;
            dst.justify_items = src.justify_items;
        }
        "place-self" => {
            dst.align_self = src.align_self;
            dst.justify_self = src.justify_self;
        }
        "place-content" => {
            dst.align_content = src.align_content;
            dst.justify_content = src.justify_content;
        }
        "grid-template-columns" => {
            dst.grid_template_columns = src.grid_template_columns;
            dst.grid_column_count = src.grid_column_count;
            dst.grid_auto_repeat = src.grid_auto_repeat;
        }
        "grid-template-rows" => {
            dst.grid_template_rows = src.grid_template_rows;
            dst.grid_row_count = src.grid_row_count;
        }
        "grid-template-areas" => dst.grid_template_areas = src.grid_template_areas,
        "grid-auto-rows" => {
            dst.grid_auto_rows = src.grid_auto_rows;
            dst.grid_auto_row_list = src.grid_auto_row_list;
            dst.grid_auto_row_count = src.grid_auto_row_count;
        }
        "grid-auto-columns" => {
            dst.grid_auto_columns = src.grid_auto_columns;
            dst.grid_auto_col_count = src.grid_auto_col_count;
        }
        "grid-auto-flow" => dst.grid_auto_flow = src.grid_auto_flow,
        "grid-column" => dst.grid_column = src.grid_column,
        "grid-column-start" => dst.grid_column.start = src.grid_column.start,
        "grid-column-end" => dst.grid_column.end = src.grid_column.end,
        "grid-row" => dst.grid_row = src.grid_row,
        "grid-row-start" => dst.grid_row.start = src.grid_row.start,
        "grid-row-end" => dst.grid_row.end = src.grid_row.end,
        "grid-area" => {
            dst.grid_row = src.grid_row;
            dst.grid_column = src.grid_column;
            dst.grid_area_name = src.grid_area_name;
        }
        "row-gap" | "grid-row-gap" => dst.row_gap = src.row_gap,
        "column-gap" | "grid-column-gap" => dst.column_gap = src.column_gap,
        "gap" | "grid-gap" => {
            dst.row_gap = src.row_gap;
            dst.column_gap = src.column_gap;
        }
        "transform" | "translate" | "scale" | "rotate" => dst.transform = src.transform,
        "transform-origin" => dst.transform_origin = src.transform_origin,
        "opacity" => dst.opacity = src.opacity,
        "transition"
        | "transition-property"
        | "transition-duration"
        | "transition-delay"
        | "transition-timing-function" => dst.transitions = src.transitions,
        "animation"
        | "animation-name"
        | "animation-duration"
        | "animation-delay"
        | "animation-iteration-count"
        | "animation-direction"
        | "animation-fill-mode"
        | "animation-timing-function"
        | "animation-play-state" => dst.animations = src.animations,
        _ => {}
    }
}

/// Tag defaults applied before author CSS. `tag` is borrowed and already lowercase.
///
/// Block tags become `display: block`. `base`, `head`, `link`, `meta`,
/// `option`, `script`, `style`, and `title` become `display: none`. `body`
/// gets an 8px margin. Tables, lists, rules, links, forms, and form controls
/// get the fixed padding, borders, and colors written below. Heading sizes
/// replace the inherited font size for `h1` through `h4` and `small`.
fn apply_user_agent(style: &mut ComputedStyle, tag: &str) {
    if is_block_tag(tag) {
        style.display = Display::Block;
        style.inline_level = false;
    }
    if matches!(
        tag,
        "base"
            | "head"
            | "link"
            | "meta"
            | "option"
            | "script"
            | "style"
            | "title"
            | "noscript"
            | "template"
            | "datalist"
            | "rp"
            | "audio"
            | "source"
            | "track"
            | "param"
            | "defs"
            | "symbol"
            | "clippath"
            | "mask"
            | "lineargradient"
            | "radialgradient"
            | "pattern"
            | "filter"
            | "marker"
            | "metadata"
            | "desc"
    ) {
        style.display = Display::None;
        style.inline_level = false;
    }
    if tag == "center" {
        style.text_align = TextAlign::Center;
    }
    if matches!(
        tag,
        "b" | "strong" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "th" | "summary"
    ) {
        style.font_weight = 700;
    }
    if matches!(tag, "i" | "em" | "cite" | "var" | "dfn") {
        style.italic = true;
    }
    if tag == "pre" {
        style.white_space = WhiteSpace::Pre;
        style.padding = Edge {
            top: 8.0,
            right: 10.0,
            bottom: 8.0,
            left: 10.0,
        };
        style.border = Edge::all(1.0);
        style.border_color = Color {
            r: 68,
            g: 76,
            b: 92,
            a: 255,
        };
        style.border_radius = 4.0;
        style.margin.top = 6.0;
        style.margin.bottom = 6.0;
    }
    if tag == "body" {
        style.margin = Edge::all(8.0);
    }
    if tag == "table" {
        style.margin.top = 6.0;
        style.margin.bottom = 6.0;
    }
    if tag == "th" || tag == "td" {
        style.padding = Edge::all(6.0);
        style.border = Edge::all(1.0);
        style.border_color = Color {
            r: 68,
            g: 60,
            b: 52,
            a: 255,
        };
    }
    if tag == "ul" || tag == "ol" {
        style.padding.left = 24.0;
        style.margin.top = 6.0;
        style.margin.bottom = 6.0;
    }
    if tag == "hr" {
        style.height = Size::Px(1.0);
        style.background = Some(Color {
            r: 128,
            g: 128,
            b: 128,
            a: 255,
        });
        style.margin.top = 8.0;
        style.margin.bottom = 8.0;
    }
    if tag == "a" {
        style.color = Color {
            r: 0,
            g: 102,
            b: 204,
            a: 255,
        };
        style.underline = true;
    }
    if tag == "form" {
        style.margin.top = 6.0;
        style.margin.bottom = 10.0;
    }
    if tag == "input" || tag == "textarea" || tag == "select" {
        style.display = Display::Block;
        style.inline_level = false;
        style.background = Some(Color::WHITE);
        style.color = Color {
            r: 24,
            g: 28,
            b: 36,
            a: 255,
        };
        style.border = Edge::all(1.0);
        style.border_color = Color {
            r: 138,
            g: 148,
            b: 166,
            a: 255,
        };
        style.border_radius = 4.0;
        style.padding = Edge {
            top: 6.0,
            right: 8.0,
            bottom: 6.0,
            left: 8.0,
        };
        style.margin.top = 4.0;
        style.margin.bottom = 4.0;
    }
    if tag == "button" {
        style.display = Display::Block;
        style.inline_level = false;
        style.background = Some(Color {
            r: 43,
            g: 53,
            b: 72,
            a: 255,
        });
        style.color = Color {
            r: 244,
            g: 241,
            b: 234,
            a: 255,
        };
        style.border = Edge::all(1.0);
        style.border_color = Color {
            r: 68,
            g: 82,
            b: 108,
            a: 255,
        };
        style.border_radius = 4.0;
        style.padding = Edge {
            top: 6.0,
            right: 12.0,
            bottom: 6.0,
            left: 12.0,
        };
        style.margin.top = 4.0;
        style.margin.bottom = 4.0;
    }
    if tag == "svg" {
        style.display = Display::Block;
        style.inline_level = true;
    }
    if matches!(tag, "code" | "kbd" | "samp") {
        style.background = Some(Color {
            r: 236,
            g: 239,
            b: 244,
            a: 255,
        });
        style.padding = Edge {
            top: 1.0,
            right: 4.0,
            bottom: 1.0,
            left: 4.0,
        };
        style.border_radius = 3.0;
    }
    if tag == "mark" {
        style.background = Some(Color {
            r: 255,
            g: 243,
            b: 163,
            a: 255,
        });
        style.color = Color {
            r: 24,
            g: 28,
            b: 36,
            a: 255,
        };
        style.padding = Edge {
            top: 1.0,
            right: 3.0,
            bottom: 1.0,
            left: 3.0,
        };
        style.border_radius = 2.0;
    }
    if tag == "blockquote" {
        style.margin = Edge {
            top: 8.0,
            right: 16.0,
            bottom: 8.0,
            left: 16.0,
        };
        style.padding.left = 12.0;
        style.border.left = 3.0;
        style.border_color = Color {
            r: 160,
            g: 168,
            b: 184,
            a: 255,
        };
    }
    if matches!(tag, "code" | "pre" | "kbd" | "samp" | "tt") {
        style.font_family = FontFamily::Monospace;
    }
    if matches!(tag, "s" | "del" | "strike") {
        style.line_through = true;
    }
    style.border_colors = [style.border_color; 4];
    style.font_size = match tag {
        "h1" => 32.0,
        "h2" => 24.0,
        "h3" => 20.0,
        "h4" => 18.0,
        "small" => 13.0,
        _ => style.font_size,
    };
}

/// Tags that the user agent displays as blocks. Not a CSS parser.
fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "body"
            | "canvas"
            | "caption"
            | "center"
            | "details"
            | "dialog"
            | "div"
            | "dl"
            | "dt"
            | "dd"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hgroup"
            | "hr"
            | "html"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "picture"
            | "pre"
            | "section"
            | "summary"
            | "table"
            | "tbody"
            | "td"
            | "tfoot"
            | "th"
            | "thead"
            | "tr"
            | "ul"
            | "video"
    )
}

/// Apply one declaration. Unknown properties and unparsed values are ignored.
///
/// `display: grid` and `var(--token)` are applied when the value parses. On
/// `width`, `height`, `max-width`, and the offsets, `px`, `%`, `em`, `rem`,
/// `vw`, `vh`, `vmin`, and `vmax` are kept. Another unit becomes
/// [`Size::Auto`]. `ch` is kept for layout. `font-size` resolves `%` and `em` against `parent_font_px`,
/// and `rem` against `root_font_px`. Margin, padding, and border still
/// accept only `px` and `0`. `row-gap`, `column-gap`, and `gap` keep any
/// length, a percentage, or `calc()`. `overflow` values `scroll` and
/// `auto` are stored as [`Overflow::Hidden`].
fn apply_declaration(
    style: &mut ComputedStyle,
    vars: &mut HashMap<String, String>,
    declaration: &Declaration,
    parent_font_px: f32,
    root_font_px: f32,
) {
    if declaration.name.starts_with("--") {
        return;
    }
    let resolved = resolve_css_vars(declaration.value.trim(), vars);
    let value = resolved.trim();
    if value.is_empty() {
        return;
    }
    match declaration.name.as_str() {
        "display" => {
            let lower = value.to_ascii_lowercase();
            style.display = match lower.as_str() {
                "block" | "inline-block" | "list-item" | "flow-root" | "table"
                | "table-row-group" | "table-header-group" | "table-footer-group" | "table-row"
                | "table-cell" => Display::Block,
                "inline" | "contents" => Display::Inline,
                "flex" | "inline-flex" => Display::Flex,
                "grid" | "inline-grid" => Display::Grid,
                "none" => Display::None,
                _ => {
                    if lower.starts_with('-') && (lower.ends_with("flex") || lower.ends_with("box"))
                    {
                        Display::Flex
                    } else {
                        return;
                    }
                }
            };
            style.inline_level = matches!(
                lower.as_str(),
                "inline" | "inline-block" | "inline-flex" | "inline-grid" | "contents"
            ) || (lower.starts_with('-')
                && (lower.ends_with("inline-flex") || lower.ends_with("inline-box")));
        }
        "box-sizing" => {
            style.box_sizing = match value.to_ascii_lowercase().as_str() {
                "border-box" => BoxSizing::BorderBox,
                "content-box" => BoxSizing::ContentBox,
                _ => return,
            };
        }
        "visibility" => {
            style.visibility = match value.to_ascii_lowercase().as_str() {
                "visible" => Visibility::Visible,
                "hidden" => Visibility::Hidden,
                "collapse" => Visibility::Collapse,
                _ => return,
            };
        }
        "text-align" => {
            let lower = value.to_ascii_lowercase();
            style.text_align = match lower.as_str() {
                "left" | "start" => TextAlign::Left,
                "center" => TextAlign::Center,
                "right" | "end" => TextAlign::Right,
                "justify" => TextAlign::Justify,
                _ if lower.ends_with("-center") => TextAlign::Center,
                _ if lower.ends_with("-right") => TextAlign::Right,
                _ if lower.ends_with("-left") => TextAlign::Left,
                _ => return,
            };
        }
        "white-space" => {
            style.white_space = match value.to_ascii_lowercase().as_str() {
                "normal" => WhiteSpace::Normal,
                "nowrap" => WhiteSpace::Nowrap,
                "pre" => WhiteSpace::Pre,
                "pre-wrap" | "pre-line" | "break-spaces" => WhiteSpace::PreWrap,
                _ => return,
            };
        }
        "font-weight" => {
            let lower = value.to_ascii_lowercase();
            match lower.as_str() {
                "normal" => style.font_weight = 400,
                "bold" | "bolder" => style.font_weight = 700,
                "lighter" => style.font_weight = 300,
                _ => {
                    if let Ok(w) = lower.parse::<u16>()
                        && (1..=1000).contains(&w)
                    {
                        style.font_weight = w;
                    }
                }
            }
        }
        "font-style" => match value.to_ascii_lowercase().as_str() {
            "italic" | "oblique" => style.italic = true,
            "normal" => style.italic = false,
            _ => {}
        },
        "line-height" => {
            if value.eq_ignore_ascii_case("normal") {
                style.line_height = None;
            } else if let Ok(mult) = value.parse::<f32>() {
                if mult.is_finite() && mult >= 0.0 {
                    style.line_height = Some(style.font_size * mult);
                }
            } else if let Some(px) = parse_font_size(value, style.font_size, root_font_px) {
                style.line_height = Some(px.max(0.0));
            }
        }
        "color" => {
            if let Some(color) = Color::parse(value) {
                style.color = color;
            }
        }
        "background" | "background-color" | "background-image" => {
            if value.eq_ignore_ascii_case("transparent") || value.eq_ignore_ascii_case("none") {
                style.background = None;
                style.background_gradient = None;
            } else if let Some(grad) = parse_linear_gradient(value) {
                style.background_gradient = Some(grad);
                style.background = Some(grad.start_color);
            } else if let Some(color) = Color::parse(value) {
                style.background = Some(color);
                style.background_gradient = None;
            } else {
                for tok in split_css_tokens(value) {
                    if tok.eq_ignore_ascii_case("transparent") {
                        style.background = None;
                        style.background_gradient = None;
                        break;
                    }
                    if let Some(color) = Color::parse(tok) {
                        style.background = Some(color);
                        style.background_gradient = None;
                        break;
                    }
                }
            }
        }
        "box-shadow" => {
            if value.eq_ignore_ascii_case("none") {
                style.box_shadow = None;
            } else if let Some(shadow) = parse_box_shadow(value) {
                style.box_shadow = Some(shadow);
            }
        }
        "font-size" => {
            if let Some(size) = parse_font_size(value, parent_font_px, root_font_px) {
                style.font_size = size.max(0.0);
            }
        }
        "text-decoration" | "text-decoration-line" => {
            for tok in split_css_tokens(value) {
                if tok.eq_ignore_ascii_case("underline") {
                    style.underline = true;
                } else if tok.eq_ignore_ascii_case("line-through") {
                    style.line_through = true;
                } else if tok.eq_ignore_ascii_case("none") {
                    style.underline = false;
                    style.line_through = false;
                }
            }
        }
        "font-family" => {
            if let Some(fam) = parse_font_family(value) {
                style.font_family = fam;
            }
        }
        "text-transform" => {
            style.text_transform = match value.to_ascii_lowercase().as_str() {
                "none" => TextTransform::None,
                "uppercase" => TextTransform::Uppercase,
                "lowercase" => TextTransform::Lowercase,
                "capitalize" => TextTransform::Capitalize,
                _ => return,
            };
        }
        "letter-spacing" => {
            if value.eq_ignore_ascii_case("normal") {
                style.letter_spacing = 0.0;
            } else if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.letter_spacing = px.clamp(-64.0, 256.0);
            }
        }
        "word-spacing" => {
            if value.eq_ignore_ascii_case("normal") {
                style.word_spacing = 0.0;
            } else if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.word_spacing = px.clamp(-64.0, 256.0);
            }
        }
        "object-fit" => {
            style.object_fit = match value.to_ascii_lowercase().as_str() {
                "fill" => ObjectFit::Fill,
                "contain" => ObjectFit::Contain,
                "cover" => ObjectFit::Cover,
                "none" => ObjectFit::None,
                "scale-down" => ObjectFit::ScaleDown,
                _ => return,
            };
        }
        "outline" => {
            if value.eq_ignore_ascii_case("none") || value == "0" {
                style.outline_width = 0.0;
            } else if let Some((w, c)) = parse_border_side(value) {
                style.outline_width = w;
                if let Some(color) = c {
                    style.outline_color = color;
                }
            }
        }
        "outline-width" => {
            if let Some(w) = parse_border_width_token(value) {
                style.outline_width = w;
            }
        }
        "outline-color" => {
            if value.eq_ignore_ascii_case("currentcolor") {
                style.outline_color = style.color;
            } else if let Some(c) = Color::parse(value) {
                style.outline_color = c;
            }
        }
        "outline-offset" => {
            if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.outline_offset = px.clamp(-128.0, 128.0);
            }
        }
        p if p == "backdrop-filter" || p.ends_with("-backdrop-filter") => {
            if value.eq_ignore_ascii_case("none") {
                style.backdrop_blur = 0.0;
            } else {
                for tok in split_css_tokens(value) {
                    let lower = tok.to_ascii_lowercase();
                    if let Some(inner) = lower
                        .strip_prefix("blur(")
                        .and_then(|s| s.strip_suffix(')'))
                        && let Some(px) = parse_box_px(inner.trim(), style.font_size, root_font_px)
                    {
                        style.backdrop_blur = px.clamp(0.0, 64.0);
                    }
                }
            }
        }
        "filter" => {
            apply_filter_declaration(style, value, root_font_px);
        }
        "text-overflow" => {
            if value.eq_ignore_ascii_case("ellipsis") {
                style.text_overflow_ellipsis = true;
            } else if value.eq_ignore_ascii_case("clip") {
                style.text_overflow_ellipsis = false;
            }
        }
        "overflow-wrap" | "word-wrap" => match value.to_ascii_lowercase().as_str() {
            "break-word" | "anywhere" => style.word_break_any = true,
            "normal" => style.word_break_any = false,
            _ => {}
        },
        "word-break" => match value.to_ascii_lowercase().as_str() {
            "break-all" | "break-word" => style.word_break_any = true,
            "normal" | "keep-all" => style.word_break_any = false,
            _ => {}
        },
        "margin" => {
            apply_margin_shorthand(style, value, root_font_px);
        }
        "margin-top" => {
            if value.eq_ignore_ascii_case("auto") {
                style.margin.top = 0.0;
                style.margin_top_auto = true;
            } else if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.margin.top = px;
                style.margin_top_auto = false;
            }
        }
        "margin-right" => {
            if value.eq_ignore_ascii_case("auto") {
                style.margin.right = 0.0;
                style.margin_right_auto = true;
            } else if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.margin.right = px;
                style.margin_right_auto = false;
            }
        }
        "margin-bottom" => {
            if value.eq_ignore_ascii_case("auto") {
                style.margin.bottom = 0.0;
                style.margin_bottom_auto = true;
            } else if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.margin.bottom = px;
                style.margin_bottom_auto = false;
            }
        }
        "margin-left" => {
            if value.eq_ignore_ascii_case("auto") {
                style.margin.left = 0.0;
                style.margin_left_auto = true;
            } else if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.margin.left = px;
                style.margin_left_auto = false;
            }
        }
        "padding" => {
            if let Some(edge) = parse_box_edge(value, style.font_size, root_font_px) {
                style.padding = edge;
            }
        }
        "padding-top" => {
            if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.padding.top = px;
            }
        }
        "padding-right" => {
            if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.padding.right = px;
            }
        }
        "padding-bottom" => {
            if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.padding.bottom = px;
            }
        }
        "padding-left" => {
            if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.padding.left = px;
            }
        }
        "border" => {
            apply_border_shorthand(style, value);
        }
        "border-width" => {
            if let Some(edge) = parse_edge(value) {
                style.border = edge;
            }
        }
        "border-color" => {
            if value.eq_ignore_ascii_case("currentcolor") {
                style.border_color = style.color;
                style.border_colors = [style.color; 4];
            } else if let Some(color) = Color::parse(value) {
                style.border_color = color;
                style.border_colors = [color; 4];
            } else {
                let mut parsed_colors = Vec::new();
                for tok in split_css_tokens(value) {
                    let c = if tok.eq_ignore_ascii_case("currentcolor") {
                        Some(style.color)
                    } else {
                        Color::parse(tok)
                    };
                    if let Some(col) = c {
                        parsed_colors.push(col);
                    } else {
                        parsed_colors.clear();
                        break;
                    }
                }
                match *parsed_colors.as_slice() {
                    [all] => {
                        style.border_color = all;
                        style.border_colors = [all; 4];
                    }
                    [v, h] => {
                        style.border_color = v;
                        style.border_colors = [v, h, v, h];
                    }
                    [t, h, b] => {
                        style.border_color = t;
                        style.border_colors = [t, h, b, h];
                    }
                    [t, r, b, l] => {
                        style.border_color = t;
                        style.border_colors = [t, r, b, l];
                    }
                    _ => {}
                }
            }
        }
        "border-top-color" | "border-block-start-color" => {
            if value.eq_ignore_ascii_case("currentcolor") {
                style.border_colors[0] = style.color;
            } else if let Some(c) = Color::parse(value) {
                style.border_colors[0] = c;
            }
        }
        "border-right-color" | "border-inline-end-color" => {
            if value.eq_ignore_ascii_case("currentcolor") {
                style.border_colors[1] = style.color;
            } else if let Some(c) = Color::parse(value) {
                style.border_colors[1] = c;
            }
        }
        "border-bottom-color" | "border-block-end-color" => {
            if value.eq_ignore_ascii_case("currentcolor") {
                style.border_colors[2] = style.color;
            } else if let Some(c) = Color::parse(value) {
                style.border_colors[2] = c;
            }
        }
        "border-left-color" | "border-inline-start-color" => {
            if value.eq_ignore_ascii_case("currentcolor") {
                style.border_colors[3] = style.color;
            } else if let Some(c) = Color::parse(value) {
                style.border_colors[3] = c;
            }
        }
        "border-top" => {
            if value.eq_ignore_ascii_case("none") || value == "0" {
                style.border.top = 0.0;
            } else if let Some((w, c)) = parse_border_side(value) {
                style.border.top = w;
                if let Some(color) = c {
                    style.border_color = color;
                    style.border_colors[0] = color;
                }
            }
        }
        "border-right" => {
            if value.eq_ignore_ascii_case("none") || value == "0" {
                style.border.right = 0.0;
            } else if let Some((w, c)) = parse_border_side(value) {
                style.border.right = w;
                if let Some(color) = c {
                    style.border_color = color;
                    style.border_colors[1] = color;
                }
            }
        }
        "border-bottom" => {
            if value.eq_ignore_ascii_case("none") || value == "0" {
                style.border.bottom = 0.0;
            } else if let Some((w, c)) = parse_border_side(value) {
                style.border.bottom = w;
                if let Some(color) = c {
                    style.border_color = color;
                    style.border_colors[2] = color;
                }
            }
        }
        "border-left" => {
            if value.eq_ignore_ascii_case("none") || value == "0" {
                style.border.left = 0.0;
            } else if let Some((w, c)) = parse_border_side(value) {
                style.border.left = w;
                if let Some(color) = c {
                    style.border_color = color;
                    style.border_colors[3] = color;
                }
            }
        }
        "border-radius" => {
            if let Some(first) = split_css_tokens(value).first().copied()
                && let Some(px) = parse_box_px(first, style.font_size, root_font_px)
            {
                style.border_radius = px.max(0.0);
            }
        }
        "width" | "inline-size" => style.width = parse_size(value),
        "height" | "block-size" => style.height = parse_size(value),
        "min-width" | "min-inline-size" => style.min_width = parse_size(value),
        "max-width" | "max-inline-size" => style.max_width = parse_size(value),
        "min-height" | "min-block-size" => style.min_height = parse_size(value),
        "max-height" | "max-block-size" => style.max_height = parse_size(value),
        "aspect-ratio" => {
            let lower = value.to_ascii_lowercase();
            let trimmed = lower.trim();
            if trimmed == "auto" || trimmed == "initial" || trimmed == "unset" || trimmed == "none"
            {
                style.aspect_ratio = None;
            } else if let Some((w_str, h_str)) = trimmed.split_once('/') {
                let w_clean = w_str.trim().trim_start_matches("auto").trim();
                let h_clean = h_str.trim().trim_end_matches("auto").trim();
                if let (Ok(w), Ok(h)) = (w_clean.parse::<f32>(), h_clean.parse::<f32>())
                    && w.is_finite()
                    && h.is_finite()
                    && w > 0.0
                    && h > 0.0
                {
                    style.aspect_ratio = Some(w / h);
                }
            } else {
                let num_clean = trimmed
                    .trim_start_matches("auto")
                    .trim_end_matches("auto")
                    .trim();
                if let Ok(r) = num_clean.parse::<f32>()
                    && r.is_finite()
                    && r > 0.0
                {
                    style.aspect_ratio = Some(r);
                }
            }
        }
        "position" => {
            style.position = match value.to_ascii_lowercase().as_str() {
                "static" => Position::Static,
                "relative" => Position::Relative,
                "absolute" => Position::Absolute,
                "fixed" => Position::Fixed,
                "sticky" => Position::Sticky,
                _ => return,
            };
        }
        "top" | "inset-block-start" => style.top = parse_size(value),
        "right" | "inset-inline-end" => style.right = parse_size(value),
        "bottom" | "inset-block-end" => style.bottom = parse_size(value),
        "left" | "inset-inline-start" => style.left = parse_size(value),
        "inset-inline" => {
            let parts = split_css_tokens(value);
            match parts.as_slice() {
                [both] => {
                    let s = parse_size(both);
                    style.left = s;
                    style.right = s;
                }
                [start, end] => {
                    style.left = parse_size(start);
                    style.right = parse_size(end);
                }
                _ => {}
            }
        }
        "inset-block" => {
            let parts = split_css_tokens(value);
            match parts.as_slice() {
                [both] => {
                    let s = parse_size(both);
                    style.top = s;
                    style.bottom = s;
                }
                [start, end] => {
                    style.top = parse_size(start);
                    style.bottom = parse_size(end);
                }
                _ => {}
            }
        }
        "margin-inline" => {
            let parts = split_css_tokens(value);
            let apply_side = |tok: &str| -> Option<(f32, bool)> {
                if tok.eq_ignore_ascii_case("auto") {
                    Some((0.0, true))
                } else {
                    parse_box_px(tok, style.font_size, root_font_px).map(|px| (px, false))
                }
            };
            match parts.as_slice() {
                [both] => {
                    if let Some((px, auto)) = apply_side(both) {
                        style.margin.left = px;
                        style.margin.right = px;
                        style.margin_left_auto = auto;
                        style.margin_right_auto = auto;
                    }
                }
                [start, end] => {
                    if let (Some((l_px, l_auto)), Some((r_px, r_auto))) =
                        (apply_side(start), apply_side(end))
                    {
                        style.margin.left = l_px;
                        style.margin.right = r_px;
                        style.margin_left_auto = l_auto;
                        style.margin_right_auto = r_auto;
                    }
                }
                _ => {}
            }
        }
        "margin-inline-start" => {
            if value.eq_ignore_ascii_case("auto") {
                style.margin.left = 0.0;
                style.margin_left_auto = true;
            } else if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.margin.left = px;
                style.margin_left_auto = false;
            }
        }
        "margin-inline-end" => {
            if value.eq_ignore_ascii_case("auto") {
                style.margin.right = 0.0;
                style.margin_right_auto = true;
            } else if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.margin.right = px;
                style.margin_right_auto = false;
            }
        }
        "margin-block" => {
            let parts = split_css_tokens(value);
            let apply_side = |tok: &str| -> Option<(f32, bool)> {
                if tok.eq_ignore_ascii_case("auto") {
                    Some((0.0, true))
                } else {
                    parse_box_px(tok, style.font_size, root_font_px).map(|px| (px, false))
                }
            };
            match parts.as_slice() {
                [both] => {
                    if let Some((px, auto)) = apply_side(both) {
                        style.margin.top = px;
                        style.margin.bottom = px;
                        style.margin_top_auto = auto;
                        style.margin_bottom_auto = auto;
                    }
                }
                [start, end] => {
                    if let (Some((t_px, t_auto)), Some((b_px, b_auto))) =
                        (apply_side(start), apply_side(end))
                    {
                        style.margin.top = t_px;
                        style.margin.bottom = b_px;
                        style.margin_top_auto = t_auto;
                        style.margin_bottom_auto = b_auto;
                    }
                }
                _ => {}
            }
        }
        "margin-block-start" => {
            if value.eq_ignore_ascii_case("auto") {
                style.margin.top = 0.0;
                style.margin_top_auto = true;
            } else if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.margin.top = px;
                style.margin_top_auto = false;
            }
        }
        "margin-block-end" => {
            if value.eq_ignore_ascii_case("auto") {
                style.margin.bottom = 0.0;
                style.margin_bottom_auto = true;
            } else if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.margin.bottom = px;
                style.margin_bottom_auto = false;
            }
        }
        "padding-inline" => {
            let parts = split_css_tokens(value);
            match parts.as_slice() {
                [both] => {
                    if let Some(px) = parse_box_px(both, style.font_size, root_font_px) {
                        style.padding.left = px;
                        style.padding.right = px;
                    }
                }
                [start, end] => {
                    if let (Some(l), Some(r)) = (
                        parse_box_px(start, style.font_size, root_font_px),
                        parse_box_px(end, style.font_size, root_font_px),
                    ) {
                        style.padding.left = l;
                        style.padding.right = r;
                    }
                }
                _ => {}
            }
        }
        "padding-inline-start" => {
            if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.padding.left = px;
            }
        }
        "padding-inline-end" => {
            if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.padding.right = px;
            }
        }
        "padding-block" => {
            let parts = split_css_tokens(value);
            match parts.as_slice() {
                [both] => {
                    if let Some(px) = parse_box_px(both, style.font_size, root_font_px) {
                        style.padding.top = px;
                        style.padding.bottom = px;
                    }
                }
                [start, end] => {
                    if let (Some(t), Some(b)) = (
                        parse_box_px(start, style.font_size, root_font_px),
                        parse_box_px(end, style.font_size, root_font_px),
                    ) {
                        style.padding.top = t;
                        style.padding.bottom = b;
                    }
                }
                _ => {}
            }
        }
        "padding-block-start" => {
            if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.padding.top = px;
            }
        }
        "padding-block-end" => {
            if let Some(px) = parse_box_px(value, style.font_size, root_font_px) {
                style.padding.bottom = px;
            }
        }
        "border-inline" => {
            if value.eq_ignore_ascii_case("none") || value == "0" {
                style.border.left = 0.0;
                style.border.right = 0.0;
            } else if let Some((w, c)) = parse_border_side(value) {
                style.border.left = w;
                style.border.right = w;
                if let Some(color) = c {
                    style.border_color = color;
                }
            }
        }
        "border-inline-start" => {
            if value.eq_ignore_ascii_case("none") || value == "0" {
                style.border.left = 0.0;
            } else if let Some((w, c)) = parse_border_side(value) {
                style.border.left = w;
                if let Some(color) = c {
                    style.border_color = color;
                }
            }
        }
        "border-inline-end" => {
            if value.eq_ignore_ascii_case("none") || value == "0" {
                style.border.right = 0.0;
            } else if let Some((w, c)) = parse_border_side(value) {
                style.border.right = w;
                if let Some(color) = c {
                    style.border_color = color;
                }
            }
        }
        "border-block" => {
            if value.eq_ignore_ascii_case("none") || value == "0" {
                style.border.top = 0.0;
                style.border.bottom = 0.0;
            } else if let Some((w, c)) = parse_border_side(value) {
                style.border.top = w;
                style.border.bottom = w;
                if let Some(color) = c {
                    style.border_color = color;
                }
            }
        }
        "border-block-start" => {
            if value.eq_ignore_ascii_case("none") || value == "0" {
                style.border.top = 0.0;
            } else if let Some((w, c)) = parse_border_side(value) {
                style.border.top = w;
                if let Some(color) = c {
                    style.border_color = color;
                }
            }
        }
        "border-block-end" => {
            if value.eq_ignore_ascii_case("none") || value == "0" {
                style.border.bottom = 0.0;
            } else if let Some((w, c)) = parse_border_side(value) {
                style.border.bottom = w;
                if let Some(color) = c {
                    style.border_color = color;
                }
            }
        }
        "translate" => {
            if value.eq_ignore_ascii_case("none") {
                style.transform = TransformList::NONE;
            } else {
                let parts = split_css_tokens(value);
                if let Some(tf) = parse_transform_fn("translate", &parts)
                    && let Some(list) = TransformList::new(&[tf])
                {
                    style.transform = list;
                }
            }
        }
        "scale" => {
            if value.eq_ignore_ascii_case("none") {
                style.transform = TransformList::NONE;
            } else {
                let parts = split_css_tokens(value);
                if let Some(tf) = parse_transform_fn("scale", &parts)
                    && let Some(list) = TransformList::new(&[tf])
                {
                    style.transform = list;
                }
            }
        }
        "rotate" => {
            if value.eq_ignore_ascii_case("none") {
                style.transform = TransformList::NONE;
            } else {
                let parts = split_css_tokens(value);
                if let Some(tf) = parse_transform_fn("rotate", &parts)
                    && let Some(list) = TransformList::new(&[tf])
                {
                    style.transform = list;
                }
            }
        }
        "inset" => {
            let parts = split_css_tokens(value);
            match parts.as_slice() {
                [all] => {
                    let s = parse_size(all);
                    style.top = s;
                    style.right = s;
                    style.bottom = s;
                    style.left = s;
                }
                [v, h] => {
                    let vs = parse_size(v);
                    let hs = parse_size(h);
                    style.top = vs;
                    style.bottom = vs;
                    style.left = hs;
                    style.right = hs;
                }
                [t, h, b] => {
                    style.top = parse_size(t);
                    let hs = parse_size(h);
                    style.left = hs;
                    style.right = hs;
                    style.bottom = parse_size(b);
                }
                [t, r, b, l] => {
                    style.top = parse_size(t);
                    style.right = parse_size(r);
                    style.bottom = parse_size(b);
                    style.left = parse_size(l);
                }
                _ => {}
            }
        }
        "transform" => {
            if let Some(list) = parse_transform(value) {
                style.transform = list;
            }
        }
        "opacity" => {
            let parsed = match value.strip_suffix('%') {
                Some(pct) => pct.trim().parse::<f32>().ok().map(|p| p / 100.0),
                None => value.parse::<f32>().ok(),
            };
            if let Some(opacity) = parsed.filter(|o| !o.is_nan()) {
                style.opacity = opacity.clamp(0.0, 1.0);
            }
        }
        "transition"
        | "transition-property"
        | "transition-duration"
        | "transition-delay"
        | "transition-timing-function" => {
            let name = declaration.name.as_str();
            animation::apply_transition_declaration(&mut style.transitions, name, value);
        }
        "animation"
        | "animation-name"
        | "animation-duration"
        | "animation-delay"
        | "animation-iteration-count"
        | "animation-direction"
        | "animation-fill-mode"
        | "animation-timing-function"
        | "animation-play-state" => {
            let name = declaration.name.as_str();
            animation::apply_animation_declaration(&mut style.animations, name, value);
        }
        "transform-origin" => {
            if let Some(origin) = parse_transform_origin(value) {
                style.transform_origin = origin;
            }
        }
        "z-index" => {
            if let Ok(z) = value.parse::<i32>() {
                style.z_index = z;
            }
        }
        "overflow" | "overflow-x" | "overflow-y" => {
            let first = split_css_tokens(value).first().copied().unwrap_or(value);
            let lower_ov = first.to_ascii_lowercase();
            style.overflow = match lower_ov.as_str() {
                "hidden" | "scroll" | "auto" | "clip" => Overflow::Hidden,
                "visible" => Overflow::Visible,
                _ => return,
            };
            style.scrollable_overflow = matches!(lower_ov.as_str(), "scroll" | "auto");
        }
        "flex" => {
            apply_flex_shorthand(style, value);
        }
        "flex-direction" => {
            style.flex_direction = match value.to_ascii_lowercase().as_str() {
                "row" => FlexDirection::Row,
                "row-reverse" => FlexDirection::RowReverse,
                "column" => FlexDirection::Column,
                "column-reverse" => FlexDirection::ColumnReverse,
                _ => return,
            };
        }
        "flex-wrap" => {
            style.flex_wrap = match value.to_ascii_lowercase().as_str() {
                "nowrap" => FlexWrap::NoWrap,
                "wrap" => FlexWrap::Wrap,
                "wrap-reverse" => FlexWrap::WrapReverse,
                _ => return,
            };
        }
        "flex-flow" => {
            let mut dir = None;
            let mut wrap = None;
            for tok in split_css_tokens(value) {
                match tok.to_ascii_lowercase().as_str() {
                    "row" if dir.is_none() => dir = Some(FlexDirection::Row),
                    "row-reverse" if dir.is_none() => dir = Some(FlexDirection::RowReverse),
                    "column" if dir.is_none() => dir = Some(FlexDirection::Column),
                    "column-reverse" if dir.is_none() => dir = Some(FlexDirection::ColumnReverse),
                    "nowrap" if wrap.is_none() => wrap = Some(FlexWrap::NoWrap),
                    "wrap" if wrap.is_none() => wrap = Some(FlexWrap::Wrap),
                    "wrap-reverse" if wrap.is_none() => wrap = Some(FlexWrap::WrapReverse),
                    _ => return,
                }
            }
            if let Some(d) = dir {
                style.flex_direction = d;
            }
            if let Some(w) = wrap {
                style.flex_wrap = w;
            }
        }
        "order" => {
            if let Ok(ord) = value.parse::<i32>() {
                style.order = ord;
            }
        }
        "flex-grow" => {
            if let Ok(grow) = value.parse::<f32>()
                && grow.is_finite()
            {
                style.flex_grow = grow.max(0.0);
            }
        }
        "flex-shrink" => {
            if let Ok(shrink) = value.parse::<f32>()
                && shrink.is_finite()
            {
                style.flex_shrink = shrink.max(0.0);
            }
        }
        "flex-basis" => {
            style.flex_basis = parse_size(value);
        }
        "justify-content" => {
            if let Some(jc) = parse_justify_content_keyword(value) {
                style.justify_content = jc;
            }
        }
        "align-content" => {
            if let Some(ac) = parse_align_content_keyword(value) {
                style.align_content = ac;
            }
        }
        "align-items" => {
            if let Some(ai) = parse_align_items_keyword(value) {
                style.align_items = ai;
            }
        }
        "align-self" => {
            if value.eq_ignore_ascii_case("auto") || value.eq_ignore_ascii_case("normal") {
                style.align_self = None;
            } else if let Some(ai) = parse_align_items_keyword(value) {
                style.align_self = Some(ai);
            }
        }
        "justify-items" => {
            if let Some(ji) = parse_align_items_keyword(value) {
                style.justify_items = ji;
            }
        }
        "justify-self" => {
            if value.eq_ignore_ascii_case("auto") || value.eq_ignore_ascii_case("normal") {
                style.justify_self = None;
            } else if let Some(js) = parse_align_items_keyword(value) {
                style.justify_self = Some(js);
            }
        }
        "place-items" => {
            let parts = split_css_tokens(value);
            match parts.as_slice() {
                [both] => {
                    if let Some(ai) = parse_align_items_keyword(both) {
                        style.align_items = ai;
                        style.justify_items = ai;
                    }
                }
                [align, justify] => {
                    if let (Some(ai), Some(ji)) = (
                        parse_align_items_keyword(align),
                        parse_align_items_keyword(justify),
                    ) {
                        style.align_items = ai;
                        style.justify_items = ji;
                    }
                }
                _ => {}
            }
        }
        "place-self" => {
            let parts = split_css_tokens(value);
            let parse_self = |tok: &str| -> Result<Option<AlignItems>, ()> {
                if tok.eq_ignore_ascii_case("auto") || tok.eq_ignore_ascii_case("normal") {
                    Ok(None)
                } else {
                    parse_align_items_keyword(tok).map(Some).ok_or(())
                }
            };
            match parts.as_slice() {
                [both] => {
                    if let Ok(val) = parse_self(both) {
                        style.align_self = val;
                        style.justify_self = val;
                    }
                }
                [align, justify] => {
                    if let (Ok(a), Ok(j)) = (parse_self(align), parse_self(justify)) {
                        style.align_self = a;
                        style.justify_self = j;
                    }
                }
                _ => {}
            }
        }
        "place-content" => {
            let parts = split_css_tokens(value);
            match parts.as_slice() {
                [both] => {
                    if let Some(ac) = parse_align_content_keyword(both) {
                        style.align_content = ac;
                        if let Some(jc) = parse_justify_content_keyword(both) {
                            style.justify_content = jc;
                        } else if matches!(ac, AlignContent::Stretch) {
                            style.justify_content = JustifyContent::Start;
                        }
                    }
                }
                [align, justify] => {
                    if let (Some(ac), Some(jc)) = (
                        parse_align_content_keyword(align),
                        parse_justify_content_keyword(justify),
                    ) {
                        style.align_content = ac;
                        style.justify_content = jc;
                    }
                }
                _ => {}
            }
        }
        "grid-template-columns" => {
            if value.eq_ignore_ascii_case("none") {
                style.grid_column_count = 0;
                style.grid_auto_repeat = GridAutoRepeat::None;
            } else if let Some((tracks, count, auto_rep)) = parse_grid_template(value) {
                style.grid_template_columns = tracks;
                style.grid_column_count = count;
                style.grid_auto_repeat = auto_rep;
            }
        }
        "grid-template-rows" => {
            if value.eq_ignore_ascii_case("none") {
                style.grid_row_count = 0;
            } else if let Some((tracks, count, _)) = parse_grid_template(value) {
                style.grid_template_rows = tracks;
                style.grid_row_count = count;
            }
        }
        "grid-template-areas" => {
            if value.eq_ignore_ascii_case("none") {
                style.grid_template_areas = GridTemplateAreas::NONE;
            } else if let Some(areas) = parse_grid_template_areas(value) {
                style.grid_template_areas = areas;
            }
        }
        "grid-column" => {
            if let Some(placement) = parse_grid_placement(value) {
                style.grid_column = placement;
            }
        }
        "grid-row" => {
            if let Some(placement) = parse_grid_placement(value) {
                style.grid_row = placement;
            }
        }
        "grid-column-start" => {
            if let Some(line) = parse_grid_line(value) {
                style.grid_column.start = line;
            }
        }
        "grid-column-end" => {
            if let Some(line) = parse_grid_line(value) {
                style.grid_column.end = line;
            }
        }
        "grid-row-start" => {
            if let Some(line) = parse_grid_line(value) {
                style.grid_row.start = line;
            }
        }
        "grid-row-end" => {
            if let Some(line) = parse_grid_line(value) {
                style.grid_row.end = line;
            }
        }
        "grid-area" => {
            if let Some((row, column)) = parse_grid_area(value) {
                style.grid_row = row;
                style.grid_column = column;
                style.grid_area_name = 0;
            } else {
                let trimmed = value.trim();
                if !trimmed.is_empty()
                    && !trimmed.contains('/')
                    && !trimmed.contains(char::is_whitespace)
                    && !trimmed.eq_ignore_ascii_case("none")
                    && !trimmed.eq_ignore_ascii_case("span")
                    && trimmed.parse::<i64>().is_err()
                {
                    style.grid_area_name = grid_area_hash(trimmed);
                    style.grid_row = GridPlacement::default();
                    style.grid_column = GridPlacement::default();
                }
            }
        }
        "grid-auto-rows" => {
            if let Some((tracks, count)) = parse_grid_auto_tracks(value) {
                style.grid_auto_rows = tracks[0];
                style.grid_auto_row_list = tracks;
                style.grid_auto_row_count = count;
            }
        }
        "grid-auto-columns" => {
            if let Some((tracks, count)) = parse_grid_auto_tracks(value) {
                style.grid_auto_columns = tracks;
                style.grid_auto_col_count = count;
            }
        }
        "grid-auto-flow" => {
            if let Some(flow) = parse_grid_auto_flow(value) {
                style.grid_auto_flow = flow;
            }
        }
        "row-gap" | "grid-row-gap" => {
            if let Some(gap) = parse_gap(value) {
                style.row_gap = gap;
            }
        }
        "column-gap" | "grid-column-gap" => {
            if let Some(gap) = parse_gap(value) {
                style.column_gap = gap;
            }
        }
        "gap" | "grid-gap" => {
            let parts = split_css_tokens(value);
            let (row, column) = match parts.as_slice() {
                [both] => (parse_gap(both), parse_gap(both)),
                [row, column] => (parse_gap(row), parse_gap(column)),
                _ => (None, None),
            };
            if let (Some(row), Some(column)) = (row, column) {
                style.row_gap = row;
                style.column_gap = column;
            }
        }
        _ => {}
    }
}

fn parse_justify_content_keyword(value: &str) -> Option<JustifyContent> {
    match value.trim().to_ascii_lowercase().as_str() {
        "flex-start" | "start" | "left" | "normal" => Some(JustifyContent::Start),
        "center" => Some(JustifyContent::Center),
        "flex-end" | "end" | "right" => Some(JustifyContent::End),
        "space-between" => Some(JustifyContent::SpaceBetween),
        "space-around" => Some(JustifyContent::SpaceAround),
        "space-evenly" => Some(JustifyContent::SpaceEvenly),
        _ => None,
    }
}

fn parse_align_content_keyword(value: &str) -> Option<AlignContent> {
    match value.trim().to_ascii_lowercase().as_str() {
        "stretch" | "normal" => Some(AlignContent::Stretch),
        "flex-start" | "start" => Some(AlignContent::Start),
        "center" => Some(AlignContent::Center),
        "flex-end" | "end" => Some(AlignContent::End),
        "space-between" => Some(AlignContent::SpaceBetween),
        "space-around" => Some(AlignContent::SpaceAround),
        "space-evenly" => Some(AlignContent::SpaceEvenly),
        _ => None,
    }
}

fn parse_align_items_keyword(value: &str) -> Option<AlignItems> {
    match value.trim().to_ascii_lowercase().as_str() {
        "flex-start" | "start" | "self-start" | "left" => Some(AlignItems::Start),
        "center" => Some(AlignItems::Center),
        "flex-end" | "end" | "self-end" | "right" => Some(AlignItems::End),
        "stretch" | "normal" => Some(AlignItems::Stretch),
        "baseline" | "first baseline" | "last baseline" => Some(AlignItems::Baseline),
        _ => None,
    }
}

fn parse_grid_auto_flow(value: &str) -> Option<GridAutoFlow> {
    let tokens: Vec<String> = value
        .split_whitespace()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    match tokens.as_slice() {
        [one] if one == "row" => Some(GridAutoFlow::Row),
        [one] if one == "column" => Some(GridAutoFlow::Column),
        [one] if one == "dense" => Some(GridAutoFlow::RowDense),
        [a, b] if (a == "row" && b == "dense") || (a == "dense" && b == "row") => {
            Some(GridAutoFlow::RowDense)
        }
        [a, b] if (a == "column" && b == "dense") || (a == "dense" && b == "column") => {
            Some(GridAutoFlow::ColumnDense)
        }
        _ => None,
    }
}

fn parse_grid_auto_tracks(value: &str) -> Option<([GridTrack; 8], u8)> {
    let mut tracks = [GridTrack::Auto; 8];
    let mut count = 0usize;
    for tok in split_css_tokens(value) {
        let track = parse_single_grid_track(tok)?;
        if count < 8 {
            tracks[count] = track;
            count += 1;
        }
    }
    (count > 0).then_some((tracks, count as u8))
}

fn parse_grid_template_areas(value: &str) -> Option<GridTemplateAreas> {
    let mut rows_vec: Vec<Vec<u32>> = Vec::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '"' || ch == '\'' {
            let quote = ch;
            let mut row_str = String::new();
            let mut closed = false;
            for inner in chars.by_ref() {
                if inner == quote {
                    closed = true;
                    break;
                }
                row_str.push(inner);
            }
            if !closed {
                return None;
            }
            let mut row_cells = Vec::new();
            for cell in row_str.split_whitespace() {
                if cell.chars().all(|c| c == '.') {
                    row_cells.push(0);
                } else {
                    row_cells.push(grid_area_hash(cell));
                }
            }
            if row_cells.is_empty() || row_cells.len() > 6 {
                return None;
            }
            rows_vec.push(row_cells);
            if rows_vec.len() > 6 {
                return None;
            }
        } else if !ch.is_whitespace() {
            return None;
        }
    }
    if rows_vec.is_empty() {
        return None;
    }
    let cols = rows_vec[0].len();
    if rows_vec.iter().any(|r| r.len() != cols) {
        return None;
    }
    let mut cells = [[0u32; 6]; 6];
    for (r_idx, row) in rows_vec.iter().enumerate() {
        for (c_idx, &hash) in row.iter().enumerate() {
            cells[r_idx][c_idx] = hash;
        }
    }
    let areas = GridTemplateAreas {
        rows: rows_vec.len() as u8,
        cols: cols as u8,
        cells,
    };
    // Validate that every named area forms a single filled rectangle.
    for r in 0..areas.rows as usize {
        for c in 0..areas.cols as usize {
            let h = areas.cells[r][c];
            if h == 0 {
                continue;
            }
            let (r0, c0, r1, c1) = areas.find_area(h)?;
            for rr in r0..r1 {
                for cc in c0..c1 {
                    if areas.cells[(rr - 1) as usize][(cc - 1) as usize] != h {
                        return None;
                    }
                }
            }
        }
    }
    Some(areas)
}

/// `border: none` and `border: 0` clear the width. Otherwise the first parsed
/// length and color in the shorthand set every side. Style keywords such as
/// `solid` are ignored by [`parse_border_side`].
fn resolve_css_vars(input: &str, vars: &HashMap<String, String>) -> String {
    let mut out = input.to_string();
    for _ in 0..16 {
        let lower = out.to_ascii_lowercase();
        let Some(pos) = lower.find("var(") else {
            break;
        };
        let after = &out[pos + 4..];
        let mut depth = 1usize;
        let mut close_rel = None;
        let mut comma_rel = None;
        for (idx, ch) in after.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close_rel = Some(idx);
                        break;
                    }
                }
                ',' if depth == 1 && comma_rel.is_none() => {
                    comma_rel = Some(idx);
                }
                _ => {}
            }
        }
        let Some(close_idx) = close_rel else {
            return String::new();
        };
        let (var_name, fallback) = match comma_rel {
            Some(c) => (after[..c].trim(), Some(after[c + 1..close_idx].trim())),
            None => (after[..close_idx].trim(), None),
        };
        let replacement = if let Some(val) = vars.get(&var_name.to_ascii_lowercase()) {
            val.clone()
        } else if let Some(fb) = fallback {
            fb.to_string()
        } else {
            return String::new();
        };
        out.replace_range(pos..=(pos + 4 + close_idx), &replacement);
    }
    out
}

/// One grid line: `auto`, a non-zero integer, `span N`, `N span`, or `span`
/// (span 1). Line names are not supported and return `None`, as do 0 and a
/// span below 1. Numbers past [`MAX_GRID_LINE`] are clamped.
fn parse_grid_line(value: &str) -> Option<GridLine> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    let clamp = |n: i64| n.clamp(-i64::from(MAX_GRID_LINE), i64::from(MAX_GRID_LINE)) as i16;
    let int = |t: &str| t.parse::<i64>().ok();
    match tokens.as_slice() {
        [one] if one.eq_ignore_ascii_case("auto") => Some(GridLine::Auto),
        [one] if one.eq_ignore_ascii_case("span") => Some(GridLine::Span(1)),
        [one] => {
            let n = int(one)?;
            (n != 0).then(|| GridLine::Line(clamp(n)))
        }
        [a, b] if a.eq_ignore_ascii_case("span") || b.eq_ignore_ascii_case("span") => {
            let num = if a.eq_ignore_ascii_case("span") { b } else { a };
            let n = int(num)?;
            (n > 0).then(|| GridLine::Span(clamp(n)))
        }
        _ => None,
    }
}

/// `start` or `start / end` for `grid-column` and `grid-row`. A missing end is `auto`.
fn parse_grid_placement(value: &str) -> Option<GridPlacement> {
    let mut parts = value.split('/');
    let start = parse_grid_line(parts.next()?)?;
    let end = match parts.next() {
        Some(end) => parse_grid_line(end)?,
        None => GridLine::Auto,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(GridPlacement { start, end })
}

/// `grid-area: row-start / column-start / row-end / column-end` with numeric
/// lines or spans. Missing parts are `auto`. Area names are not supported.
/// Returns the row placement, then the column placement.
fn parse_grid_area(value: &str) -> Option<(GridPlacement, GridPlacement)> {
    let parts: Vec<&str> = value.split('/').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut lines = [GridLine::Auto; 4];
    for (slot, part) in lines.iter_mut().zip(&parts) {
        *slot = parse_grid_line(part)?;
    }
    Some((
        GridPlacement {
            start: lines[0],
            end: lines[2],
        },
        GridPlacement {
            start: lines[1],
            end: lines[3],
        },
    ))
}

/// One track: a length, a percentage, `Nfr`, `auto`, `min-content`,
/// `max-content`, `fit-content(<length>)`, or `minmax(min, max)`.
/// `None` for anything else, including a flexible minimum such as `minmax(1fr, 2fr)`.
fn parse_single_grid_track(token: &str) -> Option<GridTrack> {
    let t = token.trim();
    let is_minmax = t
        .get(..7)
        .is_some_and(|head| head.eq_ignore_ascii_case("minmax("));
    if is_minmax && t.ends_with(')') && t.len() > 7 {
        let inner = &t[7..t.len() - 1];
        let (min_src, max_src) = inner.split_once(',')?;
        let min = parse_track_breadth(min_src)?;
        let max = parse_track_breadth(max_src)?;
        if matches!(min, TrackBreadth::Fr(_)) {
            return None;
        }
        return Some(GridTrack::MinMax(min, max));
    }
    Some(match parse_track_breadth(t)? {
        TrackBreadth::Px(px) => GridTrack::Px(px),
        TrackBreadth::Percent(pct) => GridTrack::Percent(pct),
        TrackBreadth::Fr(fr) => GridTrack::Fr(fr),
        TrackBreadth::Auto => GridTrack::Auto,
        TrackBreadth::MinContent => GridTrack::MinContent,
        TrackBreadth::MaxContent => GridTrack::MaxContent,
        TrackBreadth::FitContent(px) => GridTrack::FitContent(px),
    })
}

/// A single track size: `auto`, `min-content`, `max-content`,
/// `fit-content(<length>)`, `Nfr`, a pixel length, or a percentage.
/// Negative sizes are clamped to 0. A flexible size is at least `0.01fr`.
fn parse_track_breadth(token: &str) -> Option<TrackBreadth> {
    let t = token.trim();
    if t.eq_ignore_ascii_case("auto") {
        return Some(TrackBreadth::Auto);
    }
    if t.eq_ignore_ascii_case("min-content") {
        return Some(TrackBreadth::MinContent);
    }
    if t.eq_ignore_ascii_case("max-content") {
        return Some(TrackBreadth::MaxContent);
    }
    let is_fit_content = t
        .get(..12)
        .is_some_and(|head| head.eq_ignore_ascii_case("fit-content("));
    if is_fit_content && t.ends_with(')') && t.len() > 12 {
        let inner = t[12..t.len() - 1].trim();
        let px = match parse_length(inner)? {
            Size::Px(v) if v.is_finite() => v.max(0.0),
            Size::Em(v) if v.is_finite() => (v * 16.0).max(0.0),
            Size::Rem(v) if v.is_finite() => (v * 16.0).max(0.0),
            _ => return None,
        };
        return Some(TrackBreadth::FitContent(px));
    }
    if let Some(fr_str) = strip_unit(t, "fr") {
        let fr = fr_str.parse::<f32>().ok()?;
        return (fr >= 0.0 && fr.is_finite()).then_some(TrackBreadth::Fr(fr.max(0.01)));
    }
    match parse_length(t)? {
        Size::Px(px) if px.is_finite() => Some(TrackBreadth::Px(px.max(0.0))),
        Size::Percent(pct) if pct.is_finite() => Some(TrackBreadth::Percent(pct.max(0.0))),
        _ => None,
    }
}

/// Track list for `grid-template-columns` or `grid-template-rows`.
///
/// Tokens are tracks or `repeat(N | auto-fill | auto-fit, tracks...)`. At most 12 tracks are kept;
/// later ones are dropped. `None` when a token does not parse or the list is empty.
fn parse_grid_template(value: &str) -> Option<([GridTrack; 12], u8, GridAutoRepeat)> {
    let mut tracks = [GridTrack::Fr(1.0); 12];
    let mut count = 0usize;
    let mut auto_repeat = GridAutoRepeat::None;
    for token in split_css_tokens(value) {
        let lower = token.to_ascii_lowercase();
        if let Some(inner) = lower
            .strip_prefix("repeat(")
            .and_then(|s| s.strip_suffix(')'))
        {
            let (rep_str, body) = inner.split_once(',')?;
            let sub_tokens = split_css_tokens(body.trim());
            if sub_tokens.is_empty() {
                return None;
            }
            let rep_trimmed = rep_str.trim();
            let reps = if rep_trimmed == "auto-fill" {
                auto_repeat = GridAutoRepeat::AutoFill;
                1usize
            } else if rep_trimmed == "auto-fit" {
                auto_repeat = GridAutoRepeat::AutoFit;
                1usize
            } else {
                rep_trimmed.parse::<usize>().ok()?.min(12)
            };
            for _ in 0..reps {
                for &sub in &sub_tokens {
                    if count < 12 {
                        tracks[count] = parse_single_grid_track(sub)?;
                        count += 1;
                    }
                }
            }
        } else if count < 12 {
            tracks[count] = parse_single_grid_track(token)?;
            count += 1;
        }
    }
    (count > 0).then_some((tracks, count as u8, auto_repeat))
}

fn parse_calc(expr: &str) -> Option<Size> {
    let mut normalized = String::with_capacity(expr.len() + 8);
    let bytes = expr.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let ch = b as char;
        if (ch == '+' || ch == '-') && i > 0 {
            let prev = bytes[i - 1] as char;
            let next = bytes.get(i + 1).copied().unwrap_or(b' ') as char;
            if prev.is_whitespace() && next.is_whitespace() {
                normalized.push(' ');
                normalized.push(ch);
                normalized.push(' ');
                continue;
            }
        }
        normalized.push(ch);
    }
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    let mut px = 0.0f32;
    let mut percent = 0.0f32;
    let mut em = 0.0f32;
    let mut rem = 0.0f32;
    let mut sign = 1.0f32;
    let mut idx = 0usize;
    while idx < tokens.len() {
        let tok = tokens[idx];
        idx += 1;
        if tok == "+" {
            sign = 1.0;
            continue;
        }
        if tok == "-" {
            sign = -1.0;
            continue;
        }
        let mut scale = sign;
        let mut term_str = tok;
        if idx + 1 < tokens.len() && (tokens[idx] == "*" || tokens[idx] == "/") {
            let op = tokens[idx];
            let rhs = tokens[idx + 1];
            idx += 2;
            if let Ok(num) = term_str.parse::<f32>() {
                if op == "*" {
                    scale *= num;
                    term_str = rhs;
                } else {
                    return None;
                }
            } else if let Ok(num) = rhs.parse::<f32>() {
                if op == "*" {
                    scale *= num;
                } else if num != 0.0 {
                    scale /= num;
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }
        let term = if term_str == "0" {
            Size::Px(0.0)
        } else if let Some(n) = term_str.strip_suffix("rem") {
            Size::Rem(n.trim().parse::<f32>().ok()?)
        } else if let Some(n) = term_str.strip_suffix("em") {
            Size::Em(n.trim().parse::<f32>().ok()?)
        } else if let Some(n) = term_str.strip_suffix("px") {
            Size::Px(n.trim().parse::<f32>().ok()?)
        } else {
            let n = term_str.strip_suffix('%')?;
            Size::Percent(n.trim().parse::<f32>().ok()?)
        };
        match term {
            Size::Px(v) => px += v * scale,
            Size::Percent(v) => percent += v * scale,
            Size::Em(v) => em += v * scale,
            Size::Rem(v) => rem += v * scale,
            _ => return None,
        }
        sign = 1.0;
    }
    if percent == 0.0 && em == 0.0 && rem == 0.0 {
        Some(Size::Px(px))
    } else {
        Some(Size::Calc {
            px,
            percent,
            em,
            rem,
        })
    }
}

fn apply_border_shorthand(style: &mut ComputedStyle, value: &str) {
    if value.eq_ignore_ascii_case("none") || value == "0" {
        style.border = Edge::zero();
        return;
    }
    if let Some((width, color)) = parse_border_side(value) {
        style.border = Edge::all(width);
        if let Some(color) = color {
            style.border_color = color;
            style.border_colors = [color; 4];
        }
    }
}

fn parse_font_family(value: &str) -> Option<FontFamily> {
    for raw in value.split(',') {
        let name = raw
            .trim()
            .trim_matches(['"', '\''])
            .trim()
            .to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        if matches!(
            name.as_str(),
            "monospace"
                | "ui-monospace"
                | "consolas"
                | "courier"
                | "courier new"
                | "cascadia mono"
                | "cascadia code"
                | "fira code"
                | "jetbrains mono"
                | "menlo"
                | "monaco"
                | "source code pro"
        ) || name.contains("mono")
        {
            return Some(FontFamily::Monospace);
        }
        if matches!(
            name.as_str(),
            "serif"
                | "ui-serif"
                | "georgia"
                | "times"
                | "times new roman"
                | "cambria"
                | "palatino"
                | "garamond"
        ) {
            return Some(FontFamily::Serif);
        }
        if matches!(
            name.as_str(),
            "sans-serif"
                | "system-ui"
                | "ui-sans-serif"
                | "-apple-system"
                | "blinkmacsystemfont"
                | "segoe ui"
                | "arial"
                | "helvetica"
                | "inter"
                | "roboto"
                | "verdana"
                | "tahoma"
        ) {
            return Some(FontFamily::SansSerif);
        }
    }
    let first = value
        .split(',')
        .next()?
        .trim()
        .trim_matches(['"', '\''])
        .trim();
    if first.is_empty() {
        return None;
    }
    let mut h = 0x811c_9dc5u32;
    for b in first.to_ascii_lowercase().bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    Some(FontFamily::Custom((h as u16).max(1)))
}

fn apply_filter_declaration(style: &mut ComputedStyle, value: &str, root_font_px: f32) {
    if value.eq_ignore_ascii_case("none") {
        style.filter_blur = 0.0;
        style.filter_brightness = 1.0;
        style.filter_contrast = 1.0;
        style.filter_grayscale = 0.0;
        style.filter_invert = 0.0;
        style.filter_sepia = 0.0;
        return;
    }
    let parse_amount = |arg: &str| -> Option<f32> {
        let trimmed = arg.trim();
        if let Some(pct) = trimmed.strip_suffix('%') {
            pct.trim()
                .parse::<f32>()
                .ok()
                .filter(|v| v.is_finite())
                .map(|v| (v / 100.0).max(0.0))
        } else {
            trimmed
                .parse::<f32>()
                .ok()
                .filter(|v| v.is_finite())
                .map(|v| v.max(0.0))
        }
    };
    for tok in split_css_tokens(value) {
        let lower = tok.to_ascii_lowercase();
        if let Some(inner) = lower
            .strip_prefix("blur(")
            .and_then(|s| s.strip_suffix(')'))
        {
            if let Some(px) = parse_box_px(inner.trim(), style.font_size, root_font_px) {
                style.filter_blur = px.clamp(0.0, 64.0);
            }
        } else if let Some(inner) = lower
            .strip_prefix("brightness(")
            .and_then(|s| s.strip_suffix(')'))
        {
            if let Some(v) = parse_amount(inner) {
                style.filter_brightness = v.clamp(0.0, 8.0);
            }
        } else if let Some(inner) = lower
            .strip_prefix("contrast(")
            .and_then(|s| s.strip_suffix(')'))
        {
            if let Some(v) = parse_amount(inner) {
                style.filter_contrast = v.clamp(0.0, 8.0);
            }
        } else if let Some(inner) = lower
            .strip_prefix("grayscale(")
            .and_then(|s| s.strip_suffix(')'))
        {
            if let Some(v) = parse_amount(inner) {
                style.filter_grayscale = v.clamp(0.0, 1.0);
            }
        } else if let Some(inner) = lower
            .strip_prefix("invert(")
            .and_then(|s| s.strip_suffix(')'))
        {
            if let Some(v) = parse_amount(inner) {
                style.filter_invert = v.clamp(0.0, 1.0);
            }
        } else if let Some(inner) = lower
            .strip_prefix("sepia(")
            .and_then(|s| s.strip_suffix(')'))
        {
            if let Some(v) = parse_amount(inner) {
                style.filter_sepia = v.clamp(0.0, 1.0);
            }
        } else if let Some(inner) = lower
            .strip_prefix("opacity(")
            .and_then(|s| s.strip_suffix(')'))
            && let Some(v) = parse_amount(inner)
        {
            style.opacity = (style.opacity * v.clamp(0.0, 1.0)).clamp(0.0, 1.0);
        }
    }
}

/// Split a border side into a pixel width and an optional color.
///
/// `None` when no length token parses. `solid`, `dashed`, `dotted`, and
/// `double` are skipped. The color is `None` when no color token parses.
fn parse_border_side(value: &str) -> Option<(f32, Option<Color>)> {
    let mut width = None;
    let mut color = None;
    for token in split_css_tokens(value) {
        if let Some(px) = parse_border_width_token(token) {
            width = Some(px);
        } else if !matches!(
            token.to_ascii_lowercase().as_str(),
            "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset"
        ) && let Some(c) = Color::parse(token)
        {
            color = Some(c);
        }
    }
    Some((width?, color))
}

fn parse_border_width_token(token: &str) -> Option<f32> {
    match token.trim().to_ascii_lowercase().as_str() {
        "thin" => Some(1.0),
        "medium" => Some(3.0),
        "thick" => Some(5.0),
        _ => parse_box_px(token, 16.0, 16.0).map(|v| v.max(0.0)),
    }
}

/// Whitespace-separated tokens. Parentheses keep internal spaces, so
/// `rgb(0, 0, 0)` stays one token. The slices borrow `value`.
fn split_css_tokens(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    for (idx, ch) in value.char_indices() {
        match ch {
            '(' => {
                if start.is_none() {
                    start = Some(idx);
                }
                depth += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
            }
            c if c.is_whitespace() && depth == 0 => {
                if let Some(s) = start.take() {
                    out.push(&value[s..idx]);
                }
            }
            _ => {
                if start.is_none() {
                    start = Some(idx);
                }
            }
        }
    }
    if let Some(s) = start {
        out.push(&value[s..]);
    }
    out
}

/// `auto` and `none` become [`Size::Auto`]. `px`, `%`, `em`, `rem`, `vw`,
/// `vh`, `vmin`, `vmax`, `ch`, and `0` keep their unit. Any other unit becomes
/// [`Size::Auto`].
fn parse_size(value: &str) -> Size {
    let value = value.trim();
    if value.eq_ignore_ascii_case("auto") || value.eq_ignore_ascii_case("none") {
        Size::Auto
    } else {
        parse_length(value).unwrap_or(Size::Auto)
    }
}

/// `0`, or a number with a `px`, `%`, `em`, `rem`, `vw`, `vh`, `vmin`,
/// `vmax`, or `ch` suffix. `rem` is matched before `em`, and `vmin` / `vmax` before
/// `vw` / `vh`. Other units, including a bare number other than `0`, return
/// `None`.
fn parse_length(value: &str) -> Option<Size> {
    let value = value.trim();
    // `get` rather than an index: byte 5 can fall inside a multi-byte character.
    let is_calc = value
        .get(..5)
        .is_some_and(|head| head.eq_ignore_ascii_case("calc("));
    if value.len() > 5 && is_calc && value.ends_with(')') {
        if let Some(sz) = parse_calc(&value[5..value.len() - 1]) {
            return Some(sz);
        }
        return parse_css_math_size(value);
    }
    if value.ends_with(')')
        && let Some(open) = value.find('(')
    {
        let fn_name = value[..open].trim();
        if prism::values::is_math_function(fn_name) {
            return parse_css_math_size(value);
        }
    }
    let value = value.trim();
    if value == "0" {
        return Some(Size::Px(0.0));
    }
    if let Some(num) = value.strip_suffix("rem") {
        return num.trim().parse::<f32>().ok().map(Size::Rem);
    }
    if let Some(num) = value.strip_suffix("em") {
        return num.trim().parse::<f32>().ok().map(Size::Em);
    }
    if let Some(num) = value.strip_suffix("px") {
        return num.trim().parse::<f32>().ok().map(Size::Px);
    }
    if let Some(num) = strip_unit(value, "vmin") {
        return num.parse::<f32>().ok().map(Size::Vmin);
    }
    if let Some(num) = strip_unit(value, "vmax") {
        return num.parse::<f32>().ok().map(Size::Vmax);
    }
    if let Some(num) = strip_unit(value, "vh") {
        return num.parse::<f32>().ok().map(Size::Vh);
    }
    if let Some(num) = strip_unit(value, "vw") {
        return num.parse::<f32>().ok().map(Size::Vw);
    }
    if let Some(num) = strip_unit(value, "ch") {
        return num.parse::<f32>().ok().map(Size::Ch);
    }
    if let Some(num) = value.strip_suffix('%') {
        return num.trim().parse::<f32>().ok().map(Size::Percent);
    }
    None
}

fn parse_css_math_size(value: &str) -> Option<Size> {
    use prism::values::{CalcLeaf, CalcNode, CalcType, LengthUnit};
    let opts = prism::Options::default();
    let cvs = prism::parse_component_values(value, &opts).ok()?;
    let non_ws: Vec<_> = cvs.iter().filter(|c| !c.is_whitespace()).collect();
    let [cv] = non_ws.as_slice() else {
        return None;
    };
    let calc = prism::values::parse_math(cv, &opts.limits).ok()?;
    let leaf_to_size = |leaf: CalcLeaf| -> Option<Size> {
        match leaf {
            CalcLeaf::Length(len) => {
                let v = len.value as f32;
                if !v.is_finite() {
                    return None;
                }
                Some(match len.unit {
                    LengthUnit::Px => Size::Px(v),
                    LengthUnit::Em => Size::Em(v),
                    LengthUnit::Rem => Size::Rem(v),
                    LengthUnit::Vw | LengthUnit::Svw | LengthUnit::Lvw | LengthUnit::Dvw => {
                        Size::Vw(v)
                    }
                    LengthUnit::Vh | LengthUnit::Svh | LengthUnit::Lvh | LengthUnit::Dvh => {
                        Size::Vh(v)
                    }
                    LengthUnit::Vmin
                    | LengthUnit::Svmin
                    | LengthUnit::Lvmin
                    | LengthUnit::Dvmin => Size::Vmin(v),
                    LengthUnit::Vmax
                    | LengthUnit::Svmax
                    | LengthUnit::Lvmax
                    | LengthUnit::Dvmax => Size::Vmax(v),
                    LengthUnit::Ch | LengthUnit::Rch => Size::Ch(v),
                    _ => Size::Px(len.to_px(&prism::ResolveContext::default()) as f32),
                })
            }
            CalcLeaf::Percentage(p) => {
                let v = p as f32;
                v.is_finite().then_some(Size::Percent(v))
            }
            CalcLeaf::Number(0.0) => Some(Size::Px(0.0)),
            _ => None,
        }
    };
    if let Some(leaf) = calc.as_leaf() {
        return leaf_to_size(leaf);
    }
    if let CalcNode::Sum(items) = &calc.node {
        let mut px = 0.0f32;
        let mut percent = 0.0f32;
        let mut em = 0.0f32;
        let mut rem = 0.0f32;
        let mut all_convertible = true;
        for item in items {
            let CalcNode::Leaf(leaf) = item else {
                all_convertible = false;
                break;
            };
            match leaf_to_size(*leaf) {
                Some(Size::Px(v)) => px += v,
                Some(Size::Percent(v)) => percent += v,
                Some(Size::Em(v)) => em += v,
                Some(Size::Rem(v)) => rem += v,
                _ => {
                    all_convertible = false;
                    break;
                }
            }
        }
        if all_convertible {
            return Some(if percent == 0.0 && em == 0.0 && rem == 0.0 {
                Size::Px(px)
            } else {
                Size::Calc {
                    px,
                    percent,
                    em,
                    rem,
                }
            });
        }
    }
    if matches!(calc.ty, CalcType::Length | CalcType::LengthPercentage) {
        let resolved = calc.resolve(&prism::ResolveContext::default()) as f32;
        if resolved.is_finite() {
            return Some(Size::Px(resolved));
        }
    }
    None
}

fn strip_unit<'a>(value: &'a str, unit: &str) -> Option<&'a str> {
    if value.len() <= unit.len() {
        return None;
    }
    let split = value.len() - unit.len();
    if value.is_char_boundary(split) && value[split..].eq_ignore_ascii_case(unit) {
        Some(value[..split].trim())
    } else {
        None
    }
}

/// Resolve `font-size`. `%` and `em` use the parent font size. `rem` uses the
/// root font size. `px`, `vw`, `vh`, `vmin`, `vmax`, `ch`, keywords, and `calc()`
/// resolve to CSS pixels.
fn parse_font_size(value: &str, parent_px: f32, root_px: f32) -> Option<f32> {
    match value.trim().to_ascii_lowercase().as_str() {
        "xx-small" => return Some(9.0),
        "x-small" => return Some(10.0),
        "small" => return Some(13.0),
        "medium" => return Some(16.0),
        "large" => return Some(18.0),
        "x-large" => return Some(24.0),
        "xx-large" => return Some(32.0),
        "xxx-large" => return Some(48.0),
        "smaller" => return Some(parent_px * (5.0 / 6.0)),
        "larger" => return Some(parent_px * 1.2),
        _ => {}
    }
    let raw = match parse_length(value)? {
        Size::Px(value) if value.is_finite() => value,
        Size::Em(value) if value.is_finite() => parent_px * value,
        Size::Rem(value) if value.is_finite() => root_px * value,
        Size::Percent(value) if value.is_finite() => parent_px * value / 100.0,
        Size::Vw(value) | Size::Vh(value) | Size::Vmin(value) | Size::Vmax(value)
            if value.is_finite() =>
        {
            960.0 * value / 100.0
        }
        Size::Ch(value) if value.is_finite() => parent_px * 0.5 * value,
        Size::Calc {
            px,
            percent,
            em,
            rem,
        } => px + (parent_px * percent / 100.0) + (parent_px * em) + (root_px * rem),
        _ => return None,
    };
    (raw.is_finite() && raw >= 0.0).then(|| raw.min(4096.0))
}

/// One to four border widths into an [`Edge`], using the CSS shorthand cycle.
fn parse_edge(value: &str) -> Option<Edge> {
    let parts: Vec<f32> = split_css_tokens(value)
        .into_iter()
        .map(parse_border_width_token)
        .collect::<Option<_>>()?;
    Some(match parts.as_slice() {
        [all] => Edge::all(*all),
        [vertical, horizontal] => Edge {
            top: *vertical,
            right: *horizontal,
            bottom: *vertical,
            left: *horizontal,
        },
        [top, horizontal, bottom] => Edge {
            top: *top,
            right: *horizontal,
            bottom: *bottom,
            left: *horizontal,
        },
        [top, right, bottom, left] => Edge {
            top: *top,
            right: *right,
            bottom: *bottom,
            left: *left,
        },
        _ => return None,
    })
}

fn parse_box_px(value: &str, em_px: f32, rem_px: f32) -> Option<f32> {
    let raw = match parse_length(value)? {
        Size::Px(v) if v.is_finite() => v,
        Size::Em(v) if v.is_finite() => v * em_px,
        Size::Rem(v) if v.is_finite() => v * rem_px,
        Size::Percent(v) if v.is_finite() => 960.0 * v / 100.0,
        Size::Vw(v) | Size::Vh(v) | Size::Vmin(v) | Size::Vmax(v) if v.is_finite() => {
            960.0 * v / 100.0
        }
        Size::Ch(v) if v.is_finite() => v * em_px * 0.5,
        Size::Calc {
            px,
            percent,
            em,
            rem,
        } => px + (960.0 * percent / 100.0) + em * em_px + rem * rem_px,
        _ => return None,
    };
    raw.is_finite().then_some(raw)
}

fn parse_box_edge(value: &str, em_px: f32, rem_px: f32) -> Option<Edge> {
    let parts: Vec<f32> = split_css_tokens(value)
        .into_iter()
        .map(|tok| parse_box_px(tok, em_px, rem_px))
        .collect::<Option<_>>()?;
    Some(match parts.as_slice() {
        [all] => Edge::all(*all),
        [vertical, horizontal] => Edge {
            top: *vertical,
            right: *horizontal,
            bottom: *vertical,
            left: *horizontal,
        },
        [top, horizontal, bottom] => Edge {
            top: *top,
            right: *horizontal,
            bottom: *bottom,
            left: *horizontal,
        },
        [top, right, bottom, left] => Edge {
            top: *top,
            right: *right,
            bottom: *bottom,
            left: *left,
        },
        _ => return None,
    })
}

fn apply_margin_shorthand(style: &mut ComputedStyle, value: &str, root_font_px: f32) {
    let em_px = style.font_size;
    let parse_term = |tok: &str| -> Option<(f32, bool)> {
        if tok.eq_ignore_ascii_case("auto") {
            Some((0.0, true))
        } else {
            parse_box_px(tok, em_px, root_font_px).map(|px| (px, false))
        }
    };
    let Some(parts): Option<Vec<(f32, bool)>> = split_css_tokens(value)
        .into_iter()
        .map(parse_term)
        .collect()
    else {
        return;
    };
    let ((t, t_auto), (r, r_auto), (b, b_auto), (l, l_auto)) = match *parts.as_slice() {
        [all] => (all, all, all, all),
        [v, h] => (v, h, v, h),
        [t, h, b] => (t, h, b, h),
        [t, r, b, l] => (t, r, b, l),
        _ => return,
    };
    style.margin = Edge {
        top: t,
        right: r,
        bottom: b,
        left: l,
    };
    style.margin_top_auto = t_auto;
    style.margin_right_auto = r_auto;
    style.margin_bottom_auto = b_auto;
    style.margin_left_auto = l_auto;
}

fn apply_flex_shorthand(style: &mut ComputedStyle, value: &str) {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("none") {
        style.flex_grow = 0.0;
        style.flex_shrink = 0.0;
        style.flex_basis = Size::Auto;
        return;
    }
    if trimmed.eq_ignore_ascii_case("auto") {
        style.flex_grow = 1.0;
        style.flex_shrink = 1.0;
        style.flex_basis = Size::Auto;
        return;
    }
    if trimmed.eq_ignore_ascii_case("initial") {
        style.flex_grow = 0.0;
        style.flex_shrink = 1.0;
        style.flex_basis = Size::Auto;
        return;
    }
    let parts = split_css_tokens(trimmed);
    let non_neg = |s: &str| s.parse::<f32>().ok().filter(|v| v.is_finite() && *v >= 0.0);
    match parts.as_slice() {
        [one] => {
            if let Some(grow) = non_neg(one) {
                style.flex_grow = grow;
                style.flex_shrink = 1.0;
                style.flex_basis = Size::Px(0.0);
            } else {
                style.flex_grow = 1.0;
                style.flex_shrink = 1.0;
                style.flex_basis = parse_size(one);
            }
        }
        [a, b] => {
            if let Some(grow) = non_neg(a) {
                style.flex_grow = grow;
                if let Some(shrink) = non_neg(b) {
                    style.flex_shrink = shrink;
                    style.flex_basis = Size::Px(0.0);
                } else {
                    style.flex_shrink = 1.0;
                    style.flex_basis = parse_size(b);
                }
            }
        }
        [a, b, c] => {
            if let (Some(grow), Some(shrink)) = (non_neg(a), non_neg(b)) {
                style.flex_grow = grow;
                style.flex_shrink = shrink;
                style.flex_basis = parse_size(c);
            }
        }
        _ => {}
    }
}

/// Parse a `transform` value: `none`, or up to [`MAX_TRANSFORM_FNS`]
/// functions separated by whitespace. Any bad function, unit, or argument
/// count rejects the whole value.
fn parse_transform(value: &str) -> Option<TransformList> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("none") {
        return Some(TransformList::NONE);
    }
    let mut fns = Vec::new();
    let mut rest = value;
    while !rest.is_empty() {
        let open = rest.find('(')?;
        let name = rest[..open].trim().to_ascii_lowercase();
        let close = open + rest[open..].find(')')?;
        let args: Vec<&str> = rest[open + 1..close]
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|arg| !arg.is_empty())
            .collect();
        fns.push(parse_transform_fn(&name, &args)?);
        if fns.len() > MAX_TRANSFORM_FNS {
            return None;
        }
        rest = rest[close + 1..].trim_start();
    }
    TransformList::new(&fns).filter(|list| !list.is_none())
}

/// One transform function from its lowercase name and split arguments.
fn parse_transform_fn<'a>(name: &str, args: &[&'a str]) -> Option<TransformFn> {
    let one = |args: &[&'a str]| (args.len() == 1).then(|| args[0]);
    let length = |arg: &str| {
        parse_length(arg)
            .filter(|size| !matches!(size, Size::Calc { .. }) && size_number(*size).is_finite())
    };
    let zero = Size::Px(0.0);
    Some(match name {
        "translate" => match args {
            [x] => TransformFn::Translate(length(x)?, zero),
            [x, y] => TransformFn::Translate(length(x)?, length(y)?),
            _ => return None,
        },
        "translatex" => TransformFn::Translate(length(one(args)?)?, zero),
        "translatey" => TransformFn::Translate(zero, length(one(args)?)?),
        "scale" => match args {
            [s] => {
                let s = parse_scale_number(s)?;
                TransformFn::Scale(s, s)
            }
            [x, y] => TransformFn::Scale(parse_scale_number(x)?, parse_scale_number(y)?),
            _ => return None,
        },
        "scalex" => TransformFn::Scale(parse_scale_number(one(args)?)?, 1.0),
        "scaley" => TransformFn::Scale(1.0, parse_scale_number(one(args)?)?),
        "rotate" => TransformFn::Rotate(parse_angle(one(args)?)?),
        "skew" => match args {
            [x] => TransformFn::Skew(parse_angle(x)?, 0.0),
            [x, y] => TransformFn::Skew(parse_angle(x)?, parse_angle(y)?),
            _ => return None,
        },
        "skewx" => TransformFn::Skew(parse_angle(one(args)?)?, 0.0),
        "skewy" => TransformFn::Skew(0.0, parse_angle(one(args)?)?),
        "matrix" => {
            if args.len() != 6 {
                return None;
            }
            let mut m = [0.0f32; 6];
            for (slot, arg) in m.iter_mut().zip(args) {
                *slot = parse_finite(arg)?;
            }
            TransformFn::Matrix(m)
        }
        _ => return None,
    })
}

/// The number inside a single-unit [`Size`], for finiteness checks.
fn size_number(size: Size) -> f32 {
    match size {
        Size::Auto => 0.0,
        Size::Px(v)
        | Size::Percent(v)
        | Size::Em(v)
        | Size::Rem(v)
        | Size::Vw(v)
        | Size::Vh(v)
        | Size::Vmin(v)
        | Size::Vmax(v)
        | Size::Ch(v) => v,
        Size::Calc {
            px,
            percent,
            em,
            rem,
        } => px + percent + em + rem,
    }
}

/// A finite plain number.
fn parse_finite(value: &str) -> Option<f32> {
    value.trim().parse::<f32>().ok().filter(|v| v.is_finite())
}

/// A scale factor: a plain number, or a percentage where `50%` is `0.5`.
fn parse_scale_number(value: &str) -> Option<f32> {
    match value.trim().strip_suffix('%') {
        Some(pct) => parse_finite(pct).map(|p| p / 100.0),
        None => parse_finite(value),
    }
}

/// An angle in radians from `deg`, `rad`, `grad`, `turn`, or a bare `0`.
fn parse_angle(value: &str) -> Option<f32> {
    let value = value.trim();
    if value == "0" {
        return Some(0.0);
    }
    let lower = value.to_ascii_lowercase();
    // `grad` before `rad`, which it ends with.
    let units = [
        ("deg", std::f32::consts::PI / 180.0),
        ("grad", std::f32::consts::PI / 200.0),
        ("rad", 1.0),
        ("turn", std::f32::consts::TAU),
    ];
    let (num, per_unit) = units
        .iter()
        .find_map(|&(unit, per_unit)| lower.strip_suffix(unit).map(|n| (n, per_unit)))?;
    parse_finite(num)
        .map(|n| n * per_unit)
        .filter(|a| a.is_finite())
}

/// Parse `transform-origin`: one or two of a length, a percentage, or
/// `left`, `center`, `right`, `top`, `bottom`. A third value (the z
/// offset) must be a length and is ignored, since only 2D is drawn.
fn parse_transform_origin(value: &str) -> Option<TransformOrigin> {
    let parts: Vec<&str> = value.split_ascii_whitespace().collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    if let Some(z) = parts.get(2) {
        parse_length(z).filter(|size| matches!(size, Size::Px(_)))?;
    }
    #[derive(Clone, Copy, PartialEq)]
    enum Axis {
        X,
        Y,
        Center,
        Length,
    }
    let term = |part: &str| -> Option<(Axis, Size)> {
        Some(match part.to_ascii_lowercase().as_str() {
            "left" => (Axis::X, Size::Percent(0.0)),
            "right" => (Axis::X, Size::Percent(100.0)),
            "top" => (Axis::Y, Size::Percent(0.0)),
            "bottom" => (Axis::Y, Size::Percent(100.0)),
            "center" => (Axis::Center, Size::Percent(50.0)),
            _ => {
                let size = parse_length(part)
                    .filter(|s| !matches!(s, Size::Calc { .. }) && size_number(*s).is_finite())?;
                (Axis::Length, size)
            }
        })
    };
    let (a_axis, a) = term(parts[0])?;
    let Some(second) = parts.get(1) else {
        return Some(match a_axis {
            Axis::Y => TransformOrigin {
                x: Size::Percent(50.0),
                y: a,
            },
            _ => TransformOrigin {
                x: a,
                y: Size::Percent(50.0),
            },
        });
    };
    let (b_axis, b) = term(second)?;
    // Keywords may name y first (`top left`); a length always pairs with
    // the axis its position implies.
    match (a_axis, b_axis) {
        (Axis::Y, Axis::Y) | (Axis::X, Axis::X) => None,
        (Axis::Y, Axis::X | Axis::Center) | (Axis::Center, Axis::X) => {
            Some(TransformOrigin { x: b, y: a })
        }
        (Axis::Y, Axis::Length) | (Axis::Length, Axis::X) => None,
        _ => Some(TransformOrigin { x: a, y: b }),
    }
}

/// One `row-gap` or `column-gap` value: `normal` (stored as `0px`), a
/// length, a percentage, or a `calc()` of them. `None` for anything else
/// and for a negative value, so the declaration is ignored.
fn parse_gap(value: &str) -> Option<Size> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("normal") {
        return Some(Size::Px(0.0));
    }
    let size = parse_length(value)?;
    let negative = match size {
        Size::Auto => true,
        Size::Px(v)
        | Size::Percent(v)
        | Size::Em(v)
        | Size::Rem(v)
        | Size::Vw(v)
        | Size::Vh(v)
        | Size::Vmin(v)
        | Size::Vmax(v)
        | Size::Ch(v) => v < 0.0 || !v.is_finite(),
        Size::Calc { .. } => false,
    };
    (!negative).then_some(size)
}

/// Parse `0` or a number with a `px` suffix. `%`, `em`, `rem`, and other units
/// return `None`, so margin, padding, and border leave the previous value.
fn parse_px(value: &str) -> Option<f32> {
    match parse_length(value) {
        Some(Size::Px(value)) => Some(value),
        _ => None,
    }
}

/// Parse `offset-x offset-y blur spread color`. At least two lengths are required.
/// Missing blur and spread are 0. A missing color is black at alpha 128.
/// `None` when fewer than two lengths parse. `inset` is not recognized.
fn parse_box_shadow(value: &str) -> Option<BoxShadow> {
    let mut lengths = Vec::new();
    let mut color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 128,
    };
    for token in split_css_tokens(value) {
        if let Some(px) = parse_px(token) {
            lengths.push(px);
        } else if let Some(c) = Color::parse(token) {
            color = c;
        }
    }
    if lengths.len() < 2 {
        return None;
    }
    Some(BoxShadow {
        offset_x: lengths[0],
        offset_y: lengths[1],
        blur_radius: lengths.get(2).copied().unwrap_or(0.0).max(0.0),
        spread_radius: lengths.get(3).copied().unwrap_or(0.0),
        color,
    })
}

/// Parse `linear-gradient(...)`. `None` if the function is not that name or
/// fewer than two color stops remain after the direction.
///
/// A leading `<angle>deg` sets the angle. `to right`, `to left`, `to top`,
/// and `to bottom` map to 90, 270, 0, and 180. Any other `to ` direction
/// becomes 135. With no direction the angle is 180 (downward). Only the first
/// and last color stops are kept. Stop positions are ignored.
fn parse_linear_gradient(value: &str) -> Option<LinearGradient> {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();
    let mut is_radial = false;
    let mut is_conic = false;
    let inner = if let Some(s) = lower
        .strip_prefix("linear-gradient(")
        .or_else(|| lower.strip_prefix("repeating-linear-gradient("))
        .and_then(|s| s.strip_suffix(')'))
    {
        s
    } else if let Some(s) = lower
        .strip_prefix("radial-gradient(")
        .or_else(|| lower.strip_prefix("repeating-radial-gradient("))
        .and_then(|s| s.strip_suffix(')'))
    {
        is_radial = true;
        s
    } else {
        let s = lower
            .strip_prefix("conic-gradient(")
            .or_else(|| lower.strip_prefix("repeating-conic-gradient("))
            .and_then(|s| s.strip_suffix(')'))?;
        is_conic = true;
        s
    };

    let mut args = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    for (idx, ch) in inner.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                args.push(inner[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    args.push(inner[start..].trim());
    if args.len() < 2 {
        return None;
    }

    let parse_stop_color = |stop: &str| -> Option<Color> {
        let tokens = split_css_tokens(stop);
        Color::parse(tokens.first().copied()?)
    };

    let mut angle_deg = 180.0f32;
    let mut color_start_idx = 0usize;
    let first = args[0];
    if is_radial {
        if parse_stop_color(first).is_none()
            && (first.starts_with("circle")
                || first.starts_with("ellipse")
                || first.starts_with("closest-")
                || first.starts_with("farthest-")
                || first.starts_with("at "))
        {
            color_start_idx = 1;
        }
    } else if is_conic {
        angle_deg = 0.0;
        if parse_stop_color(first).is_none()
            && (first.starts_with("from ") || first.starts_with("at "))
        {
            if let Some(after_from) = first.strip_prefix("from ") {
                let deg_tok = after_from.split_whitespace().next().unwrap_or("");
                if let Some(deg_str) = deg_tok.strip_suffix("deg")
                    && let Ok(deg) = deg_str.trim().parse::<f32>()
                {
                    angle_deg = deg;
                }
            }
            color_start_idx = 1;
        }
    } else if let Some(deg_str) = first.strip_suffix("deg") {
        if let Ok(deg) = deg_str.trim().parse::<f32>() {
            angle_deg = deg;
            color_start_idx = 1;
        }
    } else if first.starts_with("to ") {
        angle_deg = match first {
            "to right" => 90.0,
            "to left" => 270.0,
            "to top" => 0.0,
            "to bottom" => 180.0,
            _ => 135.0,
        };
        color_start_idx = 1;
    }

    if args.len() < color_start_idx + 2 {
        return None;
    }
    let start_color = parse_stop_color(args[color_start_idx])?;
    let end_color = parse_stop_color(args[args.len() - 1])?;
    Some(LinearGradient {
        angle_deg,
        start_color,
        end_color,
    })
}

/// Parse a selector list for a style rule.
///
/// `None` when any selector in the list is invalid, so the caller drops the
/// whole rule. See [`Selector`] for what matches.
fn parse_selectors(source: &str) -> Option<Vec<Selector>> {
    selectors::parse_selector_list(source)
}

/// Parse `name: value` pairs separated by `;`.
///
/// Comments are ignored. A `;` inside a string or inside parentheses, as in
/// `url(data:image/png;base64,...)`, does not end the declaration.
/// Names are owned and lowercased. Values are owned and trimmed. A trailing
/// `!important` (any case, with optional space after the `!`) is taken off
/// the value and sets [`Declaration::important`]. A pair with an empty name,
/// or with an empty value once the flag is gone, is dropped. `source` is
/// borrowed.
pub fn parse_declarations(source: &str) -> Vec<Declaration> {
    let source = strip_comments(source);
    split_declarations(&source)
        .into_iter()
        .filter_map(|part| {
            let (name, value) = part.split_once(':')?;
            let name = name.trim().to_ascii_lowercase();
            let (value, important) = split_important(value.trim());
            if name.is_empty() || value.is_empty() {
                None
            } else {
                Some(Declaration {
                    name,
                    value: value.to_string(),
                    important,
                })
            }
        })
        .collect()
}

/// Split `source` on `;` outside strings, parentheses, and brackets.
fn split_declarations(source: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (idx, ch) in Unquoted::new(source) {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => {
                parts.push(&source[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(&source[start..]);
    parts
}

/// Split a trailing `!important` off `value`.
///
/// Only a `!` outside quotes and parentheses counts, so text such as
/// `"a !important"` or `url(x!important)` stays in the value. The flag
/// must be the last thing in the value: `red !important x` is returned
/// unchanged and not important, and fails later as an unknown value. The
/// returned value is trimmed.
fn split_important(value: &str) -> (&str, bool) {
    let mut bang = None;
    let mut depth = 0usize;
    let mut scan = Unquoted::new(value);
    for (idx, ch) in scan.by_ref() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '!' if depth == 0 => bang = Some(idx),
            _ => {}
        }
    }
    let Some(bang) = bang else {
        return (value, false);
    };
    if scan.in_string() || !value[bang + 1..].trim().eq_ignore_ascii_case("important") {
        return (value, false);
    }
    (value[..bang].trim(), true)
}

/// Copy `input` with `/* ... */` comments replaced by one space.
///
/// `/*` inside a quoted string is kept as text. Strings follow the same
/// rules as [`Unquoted`]. An unclosed comment runs to the end of the string.
/// The result is owned.
fn strip_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            out.push(ch);
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if ch == q || ch == '\n' {
                quote = None;
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
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '"' | '\'' => quote = Some(ch),
            _ => {}
        }
    }
    out
}

/// Parse the inside of `rgb()` or `rgba()`. Channels are integers 0–255.
/// When `has_alpha` is set, a dotted alpha of at most 1 is a fraction.
/// `None` when the channel count or a parse fails.
fn parse_rgb(inner: &str, has_alpha: bool) -> Option<Color> {
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if (!has_alpha && parts.len() != 3) || (has_alpha && parts.len() != 4) {
        return None;
    }
    let r: u8 = parts[0].parse().ok()?;
    let g: u8 = parts[1].parse().ok()?;
    let b: u8 = parts[2].parse().ok()?;
    let a = if has_alpha {
        let raw: f32 = parts[3].parse().ok()?;
        if raw <= 1.0 && parts[3].contains('.') {
            (raw.clamp(0.0, 1.0) * 255.0).round() as u8
        } else {
            raw.clamp(0.0, 255.0).round() as u8
        }
    } else {
        255
    };
    Some(Color { r, g, b, a })
}

/// Parse 3, 4, 6, or 8 hex digits. Short digits are expanded. `None` otherwise.
fn parse_hex(hex: &str) -> Option<Color> {
    let chars: Vec<char> = hex.chars().collect();
    let component = |text: &[char]| -> Option<u8> {
        let value = u8::from_str_radix(&text.iter().collect::<String>(), 16).ok()?;
        Some(value)
    };
    let expand = |ch: char| -> Option<u8> {
        let nibble = ch.to_digit(16)? as u8;
        Some(nibble * 16 + nibble)
    };
    match chars.as_slice() {
        [r, g, b] => Some(Color {
            r: expand(*r)?,
            g: expand(*g)?,
            b: expand(*b)?,
            a: 255,
        }),
        [r, g, b, a] => Some(Color {
            r: expand(*r)?,
            g: expand(*g)?,
            b: expand(*b)?,
            a: expand(*a)?,
        }),
        [r1, r2, g1, g2, b1, b2] => Some(Color {
            r: component(&[*r1, *r2])?,
            g: component(&[*g1, *g2])?,
            b: component(&[*b1, *b2])?,
            a: 255,
        }),
        [r1, r2, g1, g2, b1, b2, a1, a2] => Some(Color {
            r: component(&[*r1, *r2])?,
            g: component(&[*g1, *g2])?,
            b: component(&[*b1, *b2])?,
            a: component(&[*a1, *a2])?,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    //! Cascade order, inheritance, media queries, and a few declaration parsers.

    use super::*;
    use anvil_dom::parse_html;

    #[test]
    fn later_type_rule_beats_an_earlier_universal() {
        let doc = parse_html("<p>Hi</p>");
        let sheet = parse_css("* { color: red } p { color: #00ff00; font-size: 20px; }");
        let styles = compute_styles(&doc, &sheet);
        let paragraph = doc
            .children(doc.body())
            .iter()
            .copied()
            .find(|id| doc.get(*id).data().tag() == Some("p"))
            .unwrap();
        let style = styles.get(paragraph);
        assert_eq!(
            style.color,
            Color {
                r: 0,
                g: 255,
                b: 0,
                a: 255
            }
        );
        assert_eq!(style.font_size, 20.0);
        assert_eq!(style.display, Display::Block);
    }

    #[test]
    fn color_inherits_and_head_stays_hidden() {
        let doc = parse_html("<style>div { color: #ff0000 }</style><div><span>Hi</span></div>");
        let styles = compute_styles(&doc, &parse_css(&doc.embedded_css()));
        let style_node = doc
            .children(doc.head())
            .iter()
            .copied()
            .find(|id| doc.get(*id).data().tag() == Some("style"))
            .unwrap();
        assert_eq!(styles.get(style_node).display, Display::None);
        let div = doc.children(doc.body())[0];
        let span = doc.children(div)[0];
        assert_eq!(doc.get(span).data().tag(), Some("span"));
        assert_eq!(styles.get(span).color, Color::parse("#ff0000").unwrap());
    }

    #[test]
    fn links_get_a_default_color_that_author_rules_can_override() {
        let doc = parse_html("<p><a href=\"next.html\">Go</a></p>");
        let default_styles = compute_styles(&doc, &parse_css("p { color: #111111 }"));
        let p = doc.children(doc.body())[0];
        let a = doc.children(p)[0];
        assert_eq!(
            default_styles.get(a).color,
            Color {
                r: 0,
                g: 102,
                b: 204,
                a: 255
            }
        );
        assert!(default_styles.get(a).underline);

        let custom_styles = compute_styles(
            &doc,
            &parse_css("a { color: #e8c37a; text-decoration: none; }"),
        );
        assert_eq!(custom_styles.get(a).color, Color::parse("#e8c37a").unwrap());
        assert!(!custom_styles.get(a).underline);
    }

    #[test]
    fn matches_id_class_descendant_and_inline_styles() {
        let doc = parse_html(
            "<div id=\"card\" class=\"panel\"><p class=\"lead\" style=\"padding-left: 14px\">Hi</p></div>",
        );
        let sheet = parse_css(
            ".panel { border: 2px solid rgb(10, 20, 30); display: flex; gap: 12px; }
             #card .lead { color: #123456; }
             p { color: #ffffff; }",
        );
        let styles = compute_styles(&doc, &sheet);
        let card = doc.get_element_by_id("card").unwrap();
        let lead = doc.query_selector(".lead").unwrap();
        assert_eq!(styles.get(card).display, Display::Flex);
        assert_eq!(styles.get(card).row_gap, Size::Px(12.0));
        assert_eq!(styles.get(card).column_gap, Size::Px(12.0));
        assert_eq!(styles.get(card).border, Edge::all(2.0));
        assert_eq!(
            styles.get(card).border_color,
            Color {
                r: 10,
                g: 20,
                b: 30,
                a: 255
            }
        );
        assert_eq!(styles.get(lead).color, Color::parse("#123456").unwrap());
        assert_eq!(styles.get(lead).padding.left, 14.0);
    }

    #[test]
    fn parses_positioned_layout_z_index_and_overflow() {
        let doc = parse_html("<div id=\"modal\"><input id=\"q\" type=\"text\"></div>");
        let sheet = parse_css(
            "#modal { position: fixed; top: 12px; left: 24px; z-index: 10; overflow: hidden; }",
        );
        let styles = compute_styles(&doc, &sheet);
        let modal = doc.get_element_by_id("modal").unwrap();
        let q = doc.get_element_by_id("q").unwrap();
        assert_eq!(styles.get(modal).position, Position::Fixed);
        assert_eq!(styles.get(modal).top, Size::Px(12.0));
        assert_eq!(styles.get(modal).left, Size::Px(24.0));
        assert_eq!(styles.get(modal).z_index, 10);
        assert_eq!(styles.get(modal).overflow, Overflow::Hidden);
        assert_eq!(styles.get(q).border_radius, 4.0);
    }

    #[test]
    fn parses_box_shadow_and_linear_gradient() {
        let doc = parse_html("<div id=\"card\"></div>");
        let sheet = parse_css(
            "#card { background: linear-gradient(135deg, #3a342c, #25221e); box-shadow: 0px 4px 12px rgba(0, 0, 0, 160); }",
        );
        let styles = compute_styles(&doc, &sheet);
        let card = doc.get_element_by_id("card").unwrap();
        let grad = styles.get(card).background_gradient.expect("gradient");
        assert_eq!(grad.angle_deg, 135.0);
        let shadow = styles.get(card).box_shadow.expect("box-shadow");
        assert_eq!(shadow.offset_y, 4.0);
        assert_eq!(shadow.blur_radius, 12.0);
    }

    #[test]
    fn evaluates_media_queries_across_viewport_widths_and_color_schemes() {
        let doc = parse_html("<div id=\"box\" class=\"responsive\">Text</div>");
        let sheet = parse_css(
            ".responsive { width: 800px; color: #111111; }
             @media (max-width: 600px) {
               .responsive { width: 320px; }
             }
             @media (prefers-color-scheme: dark) {
               .responsive { color: #e8c37a; }
             }
             @media (prefers-color-scheme: light) {
               .responsive { color: #ff0000; }
             }",
        );
        let box_id = doc.get_element_by_id("box").unwrap();

        let desktop = compute_styles_with_viewport(&doc, &sheet, 960.0);
        assert_eq!(desktop.get(box_id).width, Size::Px(800.0));
        assert_eq!(desktop.get(box_id).color, Color::parse("#e8c37a").unwrap());

        let mobile = compute_styles_with_viewport(&doc, &sheet, 480.0);
        assert_eq!(mobile.get(box_id).width, Size::Px(320.0));
        assert_eq!(mobile.get(box_id).color, Color::parse("#e8c37a").unwrap());
    }

    #[test]
    fn keeps_px_percent_em_rem_and_viewport_units() {
        let doc = parse_html(
            "<div id=\"box\"><div id=\"child\"></div><p hidden>No</p><p id=\"shown\" hidden style=\"display:block\">Yes</p></div>",
        );
        let sheet = parse_css(
            "html { font-size: 20px; }
             #box { font-size: 2rem; width: 50%; height: 1.5em; flex-wrap: wrap; }
             #child { width: 10vw; height: 20vh; max-width: 4rem; top: 25%; left: 20ch; font-size: 50%; flex-grow: 2; display: inline-block; }",
        );
        let styles = compute_styles_for_viewport(&doc, &sheet, 400.0, 200.0);
        let box_id = doc.get_element_by_id("box").unwrap();
        let child = doc.get_element_by_id("child").unwrap();
        assert_eq!(styles.root_font_px(), 20.0);
        assert_eq!(styles.viewport_width(), 400.0);
        assert_eq!(styles.viewport_height(), 200.0);
        assert_eq!(styles.get(box_id).font_size, 40.0);
        assert_eq!(styles.get(box_id).width, Size::Percent(50.0));
        assert_eq!(styles.get(box_id).height, Size::Em(1.5));
        assert_eq!(styles.get(box_id).flex_wrap, FlexWrap::Wrap);
        assert_eq!(styles.get(child).width, Size::Vw(10.0));
        assert_eq!(styles.get(child).height, Size::Vh(20.0));
        assert_eq!(styles.get(child).max_width, Size::Rem(4.0));
        assert_eq!(styles.get(child).top, Size::Percent(25.0));
        assert_eq!(styles.get(child).left, Size::Ch(20.0));
        assert_eq!(styles.get(child).font_size, 20.0);
        assert_eq!(styles.get(child).flex_grow, 2.0);
        assert_eq!(styles.get(child).display, Display::Block);
        let hidden = doc
            .children(box_id)
            .iter()
            .copied()
            .find(|id| {
                doc.get(*id).data().tag() == Some("p") && doc.text_content(*id).contains("No")
            })
            .unwrap();
        assert_eq!(styles.get(hidden).display, Display::None);
        let shown = doc.get_element_by_id("shown").unwrap();
        assert_eq!(styles.get(shown).display, Display::Block);
        assert_eq!(Size::Percent(50.0).used_px(200.0, 16.0, 16.0), Some(100.0));
        assert_eq!(Size::Em(2.0).used_px(0.0, 30.0, 16.0), Some(60.0));
        assert_eq!(Size::Rem(1.5).used_px(0.0, 30.0, 20.0), Some(30.0));
        assert_eq!(Size::Vw(50.0).used_px(200.0, 16.0, 16.0), None);
    }

    #[test]
    fn resolves_custom_properties_calc_lengths_and_grid_tracks() {
        let doc = parse_html("<div id=\"grid\"><div id=\"card\"></div></div>");
        let sheet = parse_css(
            ":root { --accent: #ff6b24; --gap-px: 16px; }
             #grid { display: grid; grid-template-columns: 120px repeat(2, 1fr); gap: var(--gap-px); }
             #card { color: var(--accent); background: var(--missing, #112233); width: calc(100% - 24px); padding: calc(10px + 6px); }",
        );
        let styles = compute_styles(&doc, &sheet);
        let grid_id = doc.get_element_by_id("grid").unwrap();
        let card_id = doc.get_element_by_id("card").unwrap();

        assert_eq!(styles.get(grid_id).display, Display::Grid);
        assert_eq!(styles.get(grid_id).row_gap, Size::Px(16.0));
        assert_eq!(styles.get(grid_id).column_gap, Size::Px(16.0));
        assert_eq!(
            styles.get(grid_id).grid_tracks(),
            &[GridTrack::Px(120.0), GridTrack::Fr(1.0), GridTrack::Fr(1.0)]
        );
        assert_eq!(styles.get(card_id).color, Color::parse("#ff6b24").unwrap());
        assert_eq!(
            styles.get(card_id).background,
            Some(Color::parse("#112233").unwrap())
        );
        assert_eq!(styles.get(card_id).padding, Edge::all(16.0));
        assert_eq!(
            styles.get(card_id).width.used_px(300.0, 16.0, 16.0),
            Some(276.0)
        );
    }

    #[test]
    fn parses_grid_row_tracks_minmax_and_auto_rows() {
        let doc = parse_html("<div id=\"g\"></div><div id=\"h\"></div>");
        let sheet = parse_css(
            "#g { grid-template-rows: 20px auto 1fr repeat(2, minmax(10px, 2fr)) 25%; grid-auto-rows: minmax(5px, auto); }
             #h { grid-template-rows: minmax(1fr, 2fr); grid-template-columns: auto 10px; grid-auto-rows: bogus; }",
        );
        let styles = compute_styles(&doc, &sheet);
        let g = doc.get_element_by_id("g").unwrap();
        let h = doc.get_element_by_id("h").unwrap();
        assert_eq!(
            styles.get(g).grid_row_tracks(),
            &[
                GridTrack::Px(20.0),
                GridTrack::Auto,
                GridTrack::Fr(1.0),
                GridTrack::MinMax(TrackBreadth::Px(10.0), TrackBreadth::Fr(2.0)),
                GridTrack::MinMax(TrackBreadth::Px(10.0), TrackBreadth::Fr(2.0)),
                GridTrack::Percent(25.0),
            ]
        );
        assert_eq!(
            styles.get(g).grid_auto_rows,
            GridTrack::MinMax(TrackBreadth::Px(5.0), TrackBreadth::Auto)
        );
        // A flexible minimum is invalid, so the declaration is dropped.
        assert!(styles.get(h).grid_row_tracks().is_empty());
        assert_eq!(styles.get(h).grid_auto_rows, GridTrack::Auto);
        assert_eq!(
            styles.get(h).grid_tracks(),
            &[GridTrack::Auto, GridTrack::Px(10.0)]
        );
    }

    #[test]
    fn parses_grid_lines_spans_and_areas() {
        let doc = parse_html(
            "<div id=\"a\"></div><div id=\"b\"></div><div id=\"c\"></div><div id=\"d\"></div>",
        );
        let sheet = parse_css(
            "#a { grid-column: 2 / span 3; grid-row: span 2; }
             #b { grid-area: 1 / -1 / 3; grid-column-end: 4 span; }
             #c { grid-column: 0; grid-row: name / 2; grid-row-start: 5000; }
             #d { grid-column: span 0; grid-row: 1 / 2 / 3; grid-area: 2 / 3 / 4 / 5 / 6; }",
        );
        let styles = compute_styles(&doc, &sheet);
        let get = |id: &str| *styles.get(doc.get_element_by_id(id).unwrap());
        let place = |start, end| GridPlacement { start, end };
        assert_eq!(
            get("a").grid_column,
            place(GridLine::Line(2), GridLine::Span(3))
        );
        assert_eq!(get("a").grid_row, place(GridLine::Span(2), GridLine::Auto));
        assert_eq!(
            get("b").grid_row,
            place(GridLine::Line(1), GridLine::Line(3))
        );
        assert_eq!(
            get("b").grid_column,
            place(GridLine::Line(-1), GridLine::Span(4))
        );
        // Line 0 and named lines are invalid; a huge line is clamped.
        assert_eq!(get("c").grid_column, GridPlacement::default());
        assert_eq!(
            get("c").grid_row,
            place(GridLine::Line(MAX_GRID_LINE), GridLine::Auto)
        );
        assert_eq!(get("d").grid_column, GridPlacement::default());
        assert_eq!(get("d").grid_row, GridPlacement::default());
    }

    #[test]
    fn parses_transform_lists_and_origins() {
        let doc = parse_html(
            "<div id=\"a\"></div><div id=\"b\"></div><div id=\"c\"></div><div id=\"d\"></div>",
        );
        let sheet = parse_css(
            "#a { transform: translate(10px, 50%) scale(2) rotate(0.5turn) skewY(0);
                  transform-origin: right top; }
             #b { transform: translateX(1em) translateY(-3px) scaleX(150%) scaleY(-1)
                  matrix(1, 0, 0, 1, 5, 6) skew(90deg, 1rad) rotate(200grad);
                  transform-origin: 10px; }
             #c { transform: rotate(45deg); transform: rotate(45); transform-origin: 0 bottom 0;
                  transform-origin: left 5px left; }
             #d { transform: scale(1) scale(1) scale(1) scale(1) scale(1) scale(1) scale(1);
                  transform: none; transform-origin: center left; }",
        );
        let styles = compute_styles(&doc, &sheet);
        let get = |id: &str| *styles.get(doc.get_element_by_id(id).unwrap());
        use std::f32::consts::PI;
        assert_eq!(
            get("a").transform.functions(),
            [
                TransformFn::Translate(Size::Px(10.0), Size::Percent(50.0)),
                TransformFn::Scale(2.0, 2.0),
                TransformFn::Rotate(PI),
                TransformFn::Skew(0.0, 0.0),
            ]
        );
        assert_eq!(
            get("a").transform_origin,
            TransformOrigin {
                x: Size::Percent(100.0),
                y: Size::Percent(0.0)
            }
        );
        assert_eq!(
            get("b").transform.functions(),
            [
                TransformFn::Translate(Size::Em(1.0), Size::Px(0.0)),
                TransformFn::Translate(Size::Px(0.0), Size::Px(-3.0)),
                TransformFn::Scale(1.5, 1.0),
                TransformFn::Scale(1.0, -1.0),
                TransformFn::Matrix([1.0, 0.0, 0.0, 1.0, 5.0, 6.0]),
                TransformFn::Skew(PI / 2.0, 1.0),
                TransformFn::Rotate(PI),
            ]
        );
        assert_eq!(
            get("b").transform_origin,
            TransformOrigin {
                x: Size::Px(10.0),
                y: Size::Percent(50.0)
            }
        );
        // A bare number is not an angle, so the second rotate is dropped;
        // two horizontal keywords are invalid, so the 3-value origin stays.
        assert_eq!(
            get("c").transform.functions(),
            [TransformFn::Rotate(PI / 4.0)]
        );
        assert_eq!(
            get("c").transform_origin,
            TransformOrigin {
                x: Size::Px(0.0),
                y: Size::Percent(100.0)
            }
        );
        // `center left` puts the keyword on its own axis.
        assert!(get("d").transform.is_none());
        assert_eq!(
            get("d").transform_origin,
            TransformOrigin {
                x: Size::Percent(0.0),
                y: Size::Percent(50.0)
            }
        );
        // Lists longer than the limit, unknown functions, and broken syntax
        // are rejected whole.
        let ten = "scale(1) ".repeat(MAX_TRANSFORM_FNS + 1);
        for bad in [
            ten.as_str(),
            "rotate(10deg) spin(3)",
            "translate(1px, 2px, 3px)",
            "matrix(1, 0, 0, 1, 0)",
            "translate(calc(1px + 2px))",
            "scale(inf)",
            "rotate(10deg",
            "(1)",
            "translate(1)",
        ] {
            assert_eq!(parse_transform(bad), None, "{bad}");
        }
        assert_eq!(parse_transform_origin("top 10px"), None);
        assert_eq!(parse_transform_origin("10px left"), None);
        assert_eq!(parse_transform_origin("top bottom"), None);
        assert_eq!(parse_transform_origin("1px 2px 3%"), None);
    }

    #[test]
    fn parses_opacity_and_transition_lists() {
        let doc = parse_html("<div id=\"a\"></div><div id=\"b\"></div><div id=\"c\"></div>");
        let sheet = parse_css(
            "#a { opacity: 40%; transition: opacity 1s ease-in, transform 250ms steps(4, jump-none) -50ms; }
             #b { opacity: 2; transition-property: color, width; transition-duration: 2s;
                  transition-timing-function: cubic-bezier(0.1, -1, 0.9, 2), linear;
                  transition-delay: 10ms, 20ms, 30ms; transition: bogus 1s 2s 3s; }
             #c { opacity: -1; opacity: nope; transition: 1s; transition-property: none;
                  transition-duration: -1s; transition-timing-function: cubic-bezier(2, 0, 0, 1); }",
        );
        let styles = compute_styles(&doc, &sheet);
        let get = |id: &str| *styles.get(doc.get_element_by_id(id).unwrap());
        assert_eq!(get("a").opacity, 0.4);
        assert_eq!(get("b").opacity, 1.0);
        assert_eq!(get("c").opacity, 0.0);
        let a = get("a").transitions;
        assert_eq!(
            a.properties(),
            [AnimProperty::Opacity, AnimProperty::Transform]
        );
        assert_eq!(
            a.spec_for(AnimProperty::Opacity),
            Some(TransitionSpec {
                duration_ms: 1000.0,
                delay_ms: 0.0,
                timing: TimingFunction::CubicBezier(0.42, 0.0, 1.0, 1.0),
            })
        );
        assert_eq!(
            a.spec_for(AnimProperty::Transform),
            Some(TransitionSpec {
                duration_ms: 250.0,
                delay_ms: -50.0,
                timing: TimingFunction::Steps(4, StepPosition::None),
            })
        );
        assert_eq!(a.spec_for(AnimProperty::Color), None);
        // Short lists repeat; the bad shorthand (three times) is ignored.
        let b = get("b").transitions;
        assert_eq!(
            b.spec_for(AnimProperty::Width),
            Some(TransitionSpec {
                duration_ms: 2000.0,
                delay_ms: 20.0,
                timing: TimingFunction::Linear,
            })
        );
        assert_eq!(
            b.spec_for(AnimProperty::Color).map(|s| s.timing),
            Some(TimingFunction::CubicBezier(0.1, -1.0, 0.9, 2.0))
        );
        // `none` turns everything off; the bad duration and curve are ignored.
        let c = get("c").transitions;
        assert!(c.properties().is_empty());
        assert_eq!(c.spec_for(AnimProperty::Opacity), None);
    }

    #[test]
    fn timing_functions_follow_their_curves() {
        let f = |s: &str| TimingFunction::parse(s).unwrap();
        assert_eq!(f("linear").eval(0.25), 0.25);
        // The curve constants are f32, so symmetric curves are off by ~1e-8.
        assert!((f("ease-in-out").eval(0.5) - 0.5).abs() < 1e-6);
        assert!((f("ease").eval(0.5) - 0.802_403_4).abs() < 1e-5);
        assert!((f("ease-in").eval(0.5) - 0.315_356_2).abs() < 1e-5);
        assert!((f("ease-out").eval(0.5) - 0.684_643_8).abs() < 1e-5);
        assert!((f("cubic-bezier(0, 0, 1, 1)").eval(0.3) - 0.3).abs() < 1e-6);
        assert_eq!(f("ease").eval(-1.0), 0.0);
        assert_eq!(f("ease").eval(f64::NAN), 0.0);
        assert_eq!(f("ease").eval(2.0), 1.0);
        assert_eq!(f("steps(4)").eval(0.3), 0.25);
        assert_eq!(f("steps(4, start)").eval(0.3), 0.5);
        assert_eq!(f("steps(5, jump-none)").eval(0.5), 0.5);
        assert_eq!(f("steps(3, jump-both)").eval(0.5), 0.5);
        assert_eq!(f("step-end").eval(0.99), 0.0);
        assert_eq!(f("step-start").eval(0.0), 1.0);
        assert_eq!(f("steps(2)").eval(1.0), 1.0);
        for bad in [
            "steps(0)",
            "steps(1, jump-none)",
            "steps(2, middle)",
            "cubic-bezier(1.5, 0, 0, 1)",
            "cubic-bezier(0, 0, 1)",
            "bounce",
        ] {
            assert_eq!(TimingFunction::parse(bad), None, "{bad}");
        }
    }

    /// Restyle `doc` at `now` and return `id`'s style.
    fn frame(
        animator: &mut Animator,
        doc: &Document,
        sheet: &Stylesheet,
        now: f64,
        id: &str,
    ) -> ComputedStyle {
        animator.tick(now);
        let styles = compute_styles_animated(doc, sheet, 800.0, 600.0, animator);
        *styles.get(doc.get_element_by_id(id).unwrap())
    }

    #[test]
    fn runs_transitions_on_an_injected_clock() {
        let mut doc = parse_html("<div id=\"a\"><p id=\"kid\">x</p></div>");
        let sheet = parse_css(
            "#a { opacity: 1; color: #ff0000; width: 10px;
                  transition: opacity 100ms linear, color 100ms linear 50ms, width 100ms linear; }
             #kid { transition: opacity 1s linear; }",
        );
        let clock = ManualClock::new();
        let mut animator = Animator::new();
        let a = doc.get_element_by_id("a").unwrap();
        // The first pass only records the style.
        let style = frame(&mut animator, &doc, &sheet, clock.now_ms(), "a");
        assert_eq!(style.opacity, 1.0);
        assert!(!animator.is_animating());

        doc.set_attribute(a, "style", "opacity: 0; color: #0000ff; width: 50%");
        clock.set(1000.0);
        let style = frame(&mut animator, &doc, &sheet, clock.now_ms(), "a");
        assert_eq!(style.opacity, 1.0);
        assert_eq!(
            style.width,
            Size::Calc {
                px: 10.0,
                percent: 0.0,
                em: 0.0,
                rem: 0.0
            }
        );
        assert!(animator.is_animating());

        clock.advance(25.0);
        assert!(animator.tick_clock(&clock));
        let style = frame(&mut animator, &doc, &sheet, clock.now_ms(), "a");
        assert_eq!(style.opacity, 0.75);
        assert_eq!(
            style.width,
            Size::Calc {
                px: 7.5,
                percent: 12.5,
                em: 0.0,
                rem: 0.0
            }
        );
        // Color waits out its 50ms delay at the old value.
        assert_eq!(
            style.color,
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );

        clock.advance(75.0);
        let style = frame(&mut animator, &doc, &sheet, clock.now_ms(), "a");
        assert_eq!(style.opacity, 0.0);
        assert_eq!(style.width, Size::Percent(50.0));
        assert_eq!(
            style.color,
            Color {
                r: 128,
                g: 0,
                b: 128,
                a: 255
            }
        );
        // The child inherits the moving color.
        let kid = frame(&mut animator, &doc, &sheet, clock.now_ms(), "kid");
        assert_eq!(kid.color, style.color);

        assert!(!animator.tick(1150.0));
        let style = frame(&mut animator, &doc, &sheet, 1150.0, "a");
        assert_eq!(
            style.color,
            Color {
                r: 0,
                g: 0,
                b: 255,
                a: 255
            }
        );
        assert!(!animator.is_animating());
        // Time does not run backward.
        animator.tick(5.0);
        assert_eq!(animator.now_ms(), 1150.0);
    }

    #[test]
    fn interrupted_transitions_restart_from_the_current_value() {
        let mut doc = parse_html("<div id=\"a\"></div>");
        let sheet = parse_css("#a { transition: transform 100ms linear, height 100ms linear; }");
        let a = doc.get_element_by_id("a").unwrap();
        let mut animator = Animator::new();
        frame(&mut animator, &doc, &sheet, 0.0, "a");
        doc.set_attribute(a, "style", "transform: translateX(100px); height: 20px");
        frame(&mut animator, &doc, &sheet, 0.0, "a");
        let style = frame(&mut animator, &doc, &sheet, 50.0, "a");
        assert_eq!(
            style.transform.functions(),
            [TransformFn::Translate(Size::Px(50.0), Size::Px(0.0))]
        );
        // `auto` to a length does not blend, so height jumps.
        assert_eq!(style.height, Size::Px(20.0));
        // Going back starts at 50px and takes the full duration.
        doc.set_attribute(a, "style", "transform: none");
        frame(&mut animator, &doc, &sheet, 50.0, "a");
        let style = frame(&mut animator, &doc, &sheet, 100.0, "a");
        assert_eq!(
            style.transform.functions(),
            [TransformFn::Translate(Size::Px(25.0), Size::Px(0.0))]
        );
        // Lists of different shapes blend as matrices.
        doc.set_attribute(a, "style", "transform: rotate(0deg); transition: none");
        frame(&mut animator, &doc, &sheet, 300.0, "a");
        doc.set_attribute(a, "style", "transform: scale(2)");
        frame(&mut animator, &doc, &sheet, 300.0, "a");
        let style = frame(&mut animator, &doc, &sheet, 350.0, "a");
        assert_eq!(
            style.transform.functions(),
            [TransformFn::Matrix([1.5, 0.0, 0.0, 1.5, 0.0, 0.0])]
        );
        // A hidden element starts nothing, and removed elements are forgotten.
        doc.set_attribute(a, "style", "display: none; transform: scale(3)");
        frame(&mut animator, &doc, &sheet, 500.0, "a");
        assert!(!animator.is_animating());
        doc.set_attribute(a, "style", "transform: scale(4)");
        frame(&mut animator, &doc, &sheet, 500.0, "a");
        assert!(!animator.is_animating());
        let root = doc.body();
        doc.set_attribute(a, "style", "transform: scale(1)");
        doc.remove_child(root, a).unwrap();
        frame_all(&mut animator, &doc, &sheet, 600.0);
        assert!(!animator.is_animating());
    }

    #[test]
    fn far_apart_ends_blend_exactly_at_both_ends() {
        let (a, b) = (AnimValue::Number(1e30), AnimValue::Number(-370.0));
        assert_eq!(interpolate(a, b, 0.0), Some(a));
        assert_eq!(interpolate(a, b, 1.0), Some(b));
        assert_eq!(interpolate(b, a, 0.0), Some(b));
        assert_eq!(interpolate(b, a, 1.0), Some(a));
        let (a, b) = (
            AnimValue::Length(Size::Px(1e30)),
            AnimValue::Length(Size::Px(-370.0)),
        );
        assert_eq!(interpolate(a, b, 0.0), Some(a));
        assert_eq!(interpolate(a, b, 1.0), Some(b));

        // A running transition starts on the old value and reaches the new one.
        let mut doc = parse_html("<div id=\"a\"></div>");
        let sheet = parse_css("#a { margin-left: 1e30px; transition: margin-left 100ms linear; }");
        let a = doc.get_element_by_id("a").unwrap();
        let mut animator = Animator::new();
        let style = frame(&mut animator, &doc, &sheet, 0.0, "a");
        assert_eq!(style.margin.left, 1e30);
        doc.set_attribute(a, "style", "margin-left: -370px");
        let style = frame(&mut animator, &doc, &sheet, 0.0, "a");
        assert_eq!(style.margin.left, 1e30);
        assert!(animator.is_animating());
        let style = frame(&mut animator, &doc, &sheet, 100.0, "a");
        assert_eq!(style.margin.left, -370.0);
        assert!(!animator.is_animating());
    }

    /// Restyle `doc` at `now` without reading any element.
    fn frame_all(animator: &mut Animator, doc: &Document, sheet: &Stylesheet, now: f64) {
        animator.tick(now);
        compute_styles_animated(doc, sheet, 800.0, 600.0, animator);
    }

    #[test]
    fn parses_keyframes_and_animation_lists() {
        let sheet = parse_css(
            "@keyframes slide { from { margin-left: 0 } 50%, 75% { margin-left: 10px }
                                to { opacity: 0.5 } 120% { color: red } bogus { color: red } }
             @media (min-width: 1px) { @keyframes 'quoted' { 0% { opacity: 0 } } }
             @keyframes none { from { opacity: 0 } }
             @keyframes two words { from { opacity: 0 } }
             div { color: blue; }",
        );
        assert_eq!(sheet.rules.len(), 1);
        let names: Vec<&str> = sheet.keyframes.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, ["slide", "quoted"]);
        let offsets: Vec<u32> = sheet.keyframes[0]
            .frames
            .iter()
            .map(|f| f.offset_millionths)
            .collect();
        assert_eq!(offsets, [0, 500_000, 750_000, 1_000_000]);
        assert_eq!(
            sheet.keyframes[0].frames[1].declarations,
            [Declaration {
                name: "margin-left".to_string(),
                value: "10px".to_string(),
                important: false,
            }]
        );

        let doc = parse_html("<div id=\"a\"></div><div id=\"b\"></div>");
        let sheet = parse_css(
            "#a { animation: slide 2s ease-out 100ms infinite alternate-reverse both paused,
                             'fade' 50ms; }
             #b { animation-name: x, y, none; animation-duration: 1s, 2s; animation-delay: -1s;
                  animation-iteration-count: 2.5; animation-direction: reverse, normal;
                  animation-fill-mode: forwards; animation-timing-function: steps(2);
                  animation: slide 1s 2s 3s; animation-duration: -1s; }",
        );
        let styles = compute_styles(&doc, &sheet);
        let get = |id: &str| *styles.get(doc.get_element_by_id(id).unwrap());
        assert_eq!(
            get("a").animations.specs(),
            [
                AnimationSpec {
                    name_hash: animation_name_hash("slide"),
                    duration_ms: 2000.0,
                    delay_ms: 100.0,
                    iterations: f32::INFINITY,
                    direction: AnimationDirection::AlternateReverse,
                    fill: FillMode::Both,
                    timing: TimingFunction::CubicBezier(0.0, 0.0, 0.58, 1.0),
                },
                AnimationSpec {
                    name_hash: animation_name_hash("fade"),
                    duration_ms: 50.0,
                    delay_ms: 0.0,
                    iterations: 1.0,
                    direction: AnimationDirection::Normal,
                    fill: FillMode::None,
                    timing: TimingFunction::EASE,
                },
            ]
        );
        // `none` entries are skipped; shorter lists repeat; the bad
        // shorthand and the negative duration are ignored.
        let b = get("b").animations.specs();
        assert_eq!(b.len(), 2);
        assert_eq!(
            b[1],
            AnimationSpec {
                name_hash: animation_name_hash("y"),
                duration_ms: 2000.0,
                delay_ms: -1000.0,
                iterations: 2.5,
                direction: AnimationDirection::Normal,
                fill: FillMode::Forwards,
                timing: TimingFunction::Steps(2, StepPosition::End),
            }
        );
        assert_eq!(b[0].direction, AnimationDirection::Reverse);
    }

    #[test]
    fn runs_keyframe_animations_on_an_injected_clock() {
        let mut doc = parse_html("<div id=\"a\"></div>");
        let sheet = parse_css(
            "@keyframes slide {
                from { margin-left: 0; width: auto }
                50% { margin-left: 100px; animation-timing-function: linear }
                to { margin-left: 50px; width: 100px }
             }
             @keyframes fade { from { opacity: 0 } }
             #a { animation: slide 1s linear 100ms 2 alternate both, fade 100ms linear; }",
        );
        let a = doc.get_element_by_id("a").unwrap();
        let mut animator = Animator::new();
        let at = |animator: &mut Animator, doc: &Document, now: f64| {
            let style = frame(animator, doc, &sheet, now, "a");
            (style.margin.left, style.opacity, style.width)
        };
        // Waiting on the delay, filled backwards from the first keyframe.
        assert_eq!(at(&mut animator, &doc, 0.0), (0.0, 0.0, Size::Auto));
        assert!(animator.is_animating());
        assert_eq!(at(&mut animator, &doc, 25.0), (0.0, 0.25, Size::Auto));
        // 250ms in: halfway to the 50% keyframe. `auto` does not blend.
        assert_eq!(at(&mut animator, &doc, 350.0), (50.0, 1.0, Size::Auto));
        assert_eq!(at(&mut animator, &doc, 850.0), (75.0, 1.0, Size::Px(100.0)));
        // The second run plays backward.
        assert_eq!(
            at(&mut animator, &doc, 1350.0),
            (75.0, 1.0, Size::Px(100.0))
        );
        // Done after two runs; filled forwards at the start of the curve.
        assert!(animator.tick(2099.0));
        assert_eq!(at(&mut animator, &doc, 2200.0), (0.0, 1.0, Size::Auto));
        assert!(!animator.is_animating());

        // Keeping the name keeps the start time: 2100ms into a 4s run is
        // 52.5%, just past the middle keyframe. A new name starts now.
        doc.set_attribute(a, "style", "animation-duration: 4s, 100ms");
        let (left, _, _) = at(&mut animator, &doc, 2200.0);
        assert_eq!(left, 97.5);
        doc.set_attribute(a, "style", "animation: fade 100ms linear infinite");
        assert_eq!(at(&mut animator, &doc, 3000.0).1, 0.0);
        assert_eq!(at(&mut animator, &doc, 3150.0).1, 0.5);
        assert!(animator.tick(1e12));
        // Removing the animation stops it.
        doc.set_attribute(a, "style", "animation: none");
        assert_eq!(at(&mut animator, &doc, 1e12), (0.0, 1.0, Size::Auto));
        assert!(!animator.is_animating());
    }

    #[test]
    fn far_apart_keyframes_fill_forwards_with_their_last_value() {
        let doc = parse_html("<div id=\"a\"></div><div id=\"b\"></div>");
        let sheet = parse_css(
            "@keyframes far { from { margin-left: 1e30px } to { margin-left: -370px } }
             #a { animation: far 100ms linear forwards; }
             #b { animation: far 100ms linear reverse forwards; }",
        );
        let mut animator = Animator::new();
        let mut left = |now: f64, id: &str| frame(&mut animator, &doc, &sheet, now, id).margin.left;
        assert_eq!(left(0.0, "a"), 1e30);
        assert_eq!(left(0.0, "b"), -370.0);
        assert_eq!(left(100.0, "a"), -370.0);
        assert_eq!(left(5000.0, "a"), -370.0);
        // Played backward, the fill keeps the `from` value.
        assert_eq!(left(5000.0, "b"), 1e30);
        assert!(!animator.is_animating());

        let html = "<style>
            :root { --accent: #2266aa; }
            .card { width: 120px; padding: 8px; color: var(--accent); }
            .card p { font-size: 14px; margin: 4px; }
        </style>
        <div class=\"card\" id=\"c1\"><p>One</p></div>
        <div class=\"card\" id=\"c2\"><p>Two</p></div>
        <div class=\"card\" id=\"c3\"><p>Three</p></div>
        <div class=\"card\" id=\"c4\"><p>Four</p></div>";
        let parallel_doc = parse_html(html);
        let parallel_sheet = parse_css(&parallel_doc.embedded_css());
        let sequential = compute_styles_for_viewport(&parallel_doc, &parallel_sheet, 800.0, 600.0);
        let parallel = compute_styles_parallel(&parallel_doc, &parallel_sheet, 800.0, 600.0);
        assert_eq!(parallel, sequential);
        assert!(!animator.is_animating());
    }

    #[test]
    fn parses_modern_colors_box_sizing_auto_margins_and_flex_shorthands() {
        let doc = parse_html(
            "<center><div id=\"box\"><b>Bold</b><input id=\"hid\" type=\"hidden\"><noscript><div>Hidden</div></noscript></div></center>",
        );
        let sheet = parse_css(
            "#box, input::-vendor-input-placeholder {
                margin: 12px auto;
                box-sizing: border-box;
                min-width: 240px;
                min-height: 40px;
                max-height: 120px;
                visibility: hidden;
                white-space: nowrap;
                flex: 2 0 180px;
                justify-content: space-around;
                align-self: center;
                color: cornflowerblue;
                background-color: hsl(120, 100%, 50%);
                border-color: currentColor;
            }",
        );
        let styles = compute_styles(&doc, &sheet);
        let box_id = doc.get_element_by_id("box").unwrap();
        let hid_id = doc.get_element_by_id("hid").unwrap();
        let st = styles.get(box_id);
        assert_eq!(st.margin.top, 12.0);
        assert_eq!(st.margin.bottom, 12.0);
        assert!(st.margin_left_auto);
        assert!(st.margin_right_auto);
        assert_eq!(st.box_sizing, BoxSizing::BorderBox);
        assert_eq!(st.min_width, Size::Px(240.0));
        assert_eq!(st.min_height, Size::Px(40.0));
        assert_eq!(st.max_height, Size::Px(120.0));
        assert_eq!(st.visibility, Visibility::Hidden);
        assert_eq!(st.text_align, TextAlign::Center);
        assert_eq!(st.white_space, WhiteSpace::Nowrap);
        assert_eq!(st.flex_grow, 2.0);
        assert_eq!(st.flex_shrink, 0.0);
        assert_eq!(st.flex_basis, Size::Px(180.0));
        assert_eq!(st.justify_content, JustifyContent::SpaceAround);
        assert_eq!(st.align_self, Some(AlignItems::Center));
        assert_eq!(
            st.color,
            Color {
                r: 100,
                g: 149,
                b: 237,
                a: 255
            }
        );
        assert_eq!(
            st.background,
            Some(Color {
                r: 0,
                g: 255,
                b: 0,
                a: 255
            })
        );
        assert_eq!(st.border_color, st.color);
        assert_eq!(styles.get(hid_id).display, Display::None);
    }

    #[test]
    fn hides_closed_dialog_and_closed_details_body_until_opened() {
        let mut doc = parse_html(
            "<dialog id=\"dlg_closed\">Secret</dialog>
             <dialog id=\"dlg_open\" open>Visible</dialog>
             <details id=\"det\">
               <summary id=\"sum\">Header</summary>
               <p id=\"det_body\">Body text</p>
             </details>",
        );
        let sheet = parse_css("");
        let styles = compute_styles(&doc, &sheet);
        let dlg_closed = doc.get_element_by_id("dlg_closed").unwrap();
        let dlg_open = doc.get_element_by_id("dlg_open").unwrap();
        let det = doc.get_element_by_id("det").unwrap();
        let sum = doc.get_element_by_id("sum").unwrap();
        let det_body = doc.get_element_by_id("det_body").unwrap();

        assert_eq!(styles.get(dlg_closed).display, Display::None);
        assert_eq!(styles.get(dlg_open).display, Display::Block);
        assert_eq!(styles.get(sum).display, Display::Block);
        assert_eq!(styles.get(det_body).display, Display::None);

        doc.toggle_details(det);
        let styles_open = compute_styles(&doc, &sheet);
        assert_eq!(styles_open.get(det_body).display, Display::Block);
    }

    #[test]
    fn parses_aspect_ratio_fractions_numbers_and_auto() {
        let doc = parse_html(
            "<div id=\"video\" style=\"aspect-ratio: 16 / 9\"></div>
             <div id=\"square\" style=\"aspect-ratio: 1\"></div>
             <div id=\"auto_r\" style=\"aspect-ratio: 4/3; aspect-ratio: auto\"></div>",
        );
        let sheet = parse_css("");
        let styles = compute_styles(&doc, &sheet);
        let video = doc.get_element_by_id("video").unwrap();
        let square = doc.get_element_by_id("square").unwrap();
        let auto_r = doc.get_element_by_id("auto_r").unwrap();

        let vr = styles.get(video).aspect_ratio.expect("16/9 ratio");
        assert!((vr - (16.0 / 9.0)).abs() < 1e-4);
        assert_eq!(styles.get(square).aspect_ratio, Some(1.0));
        assert_eq!(styles.get(auto_r).aspect_ratio, None);
    }

    #[test]
    fn evaluates_scope_boundaries_container_size_queries_and_css_values_4_math() {
        let doc = parse_html(
            "<div id=\"wide_wrap\" style=\"width: 500px\">
               <div class=\"card\">
                 <p id=\"in_scope\">Scoped</p>
                 <div class=\"slot\">
                   <p id=\"out_scope\">Slotted</p>
                 </div>
               </div>
             </div>
             <div id=\"narrow_wrap\" style=\"width: 240px\">
               <div class=\"card\">
                 <p id=\"narrow_in_scope\">Narrow Scoped</p>
               </div>
             </div>",
        );
        let sheet = parse_css(
            "@scope (.card) to (.slot) {
               p { color: rgb(20, 40, 60); width: round(230px, 50px); padding-left: mod(140px, 50px); }
             }
             @container (min-width: 400px) {
               p { height: hypot(30px, 40px); }
             }
             @container card (width <= 300px) {
               p { height: calc(sin(30deg) * 100px); }
             }",
        );
        let styles = compute_styles(&doc, &sheet);
        let in_scope = doc.get_element_by_id("in_scope").unwrap();
        let out_scope = doc.get_element_by_id("out_scope").unwrap();
        let narrow_in_scope = doc.get_element_by_id("narrow_in_scope").unwrap();

        assert_eq!(
            styles.get(in_scope).color,
            Color {
                r: 20,
                g: 40,
                b: 60,
                a: 255
            }
        );
        assert_eq!(styles.get(in_scope).width, Size::Px(250.0));
        assert_eq!(styles.get(in_scope).padding.left, 40.0);
        assert_eq!(styles.get(in_scope).height, Size::Px(50.0));

        assert_eq!(styles.get(out_scope).color, Color::BLACK);
        assert_eq!(styles.get(out_scope).width, Size::Auto);

        assert_eq!(
            styles.get(narrow_in_scope).color,
            Color {
                r: 20,
                g: 40,
                b: 60,
                a: 255
            }
        );
        assert_eq!(styles.get(narrow_in_scope).height, Size::Px(50.0));
    }

    #[test]
    fn resolves_cascade_layers_important_inversion_and_revert_layer() {
        let doc = parse_html(
            "<div id=\"normal\" class=\"btn\"></div>
             <div id=\"imp\" class=\"btn\"></div>
             <div id=\"rev\" class=\"btn\"></div>",
        );
        let sheet = parse_css(
            "@layer base, theme, utilities;
             @layer utilities {
               #normal { width: 100px; }
               #imp { height: 90px !important; }
               #rev { width: 300px; width: revert-layer; }
             }
             @layer base {
               .btn { width: 40px; height: 25px !important; }
             }
             @layer theme {
               #normal { width: 80px; }
               #imp { height: 50px !important; }
               #rev { width: 140px; }
             }",
        );
        let styles = compute_styles(&doc, &sheet);
        let normal = doc.get_element_by_id("normal").unwrap();
        let imp = doc.get_element_by_id("imp").unwrap();
        let rev = doc.get_element_by_id("rev").unwrap();

        // Normal rules: later declared layer (`utilities`) beats earlier (`theme`, `base`).
        assert_eq!(styles.get(normal).width, Size::Px(100.0));
        // `!important` rules: earlier declared layer (`base`) beats later (`theme`, `utilities`)
        // even when `utilities` has higher selector specificity (`#imp` vs `.btn`).
        assert_eq!(styles.get(imp).height, Size::Px(25.0));
        // `revert-layer` in `utilities` rolls back to the value from `theme` (140px).
        assert_eq!(styles.get(rev).width, Size::Px(140.0));
    }

    #[test]
    fn resolves_scope_proximity_scope_pseudo_and_registered_properties() {
        let doc = parse_html(
            "<div class=\"outer\" id=\"outer_root\">
               <div class=\"inner\" id=\"inner_root\">
                 <p id=\"target\">Text</p>
               </div>
             </div>",
        );
        let sheet = parse_css(
            "@property --box-gap {
               syntax: \"<length>\";
               inherits: false;
               initial-value: 18px;
             }
             @scope (.outer) {
               :scope { --box-gap: 44px; padding-top: var(--box-gap); }
               p { width: 100px; }
             }
             @scope (.inner) {
               :scope { padding-bottom: 12px; }
               p { width: 220px; }
             }
             @scope (.outer) {
               p { width: 999px; margin-top: var(--box-gap); }
             }",
        );
        let styles = compute_styles(&doc, &sheet);
        let outer_root = doc.get_element_by_id("outer_root").unwrap();
        let inner_root = doc.get_element_by_id("inner_root").unwrap();
        let target = doc.get_element_by_id("target").unwrap();

        // `:scope` matches `.outer` and `.inner` roots respectively.
        assert_eq!(styles.get(outer_root).padding.top, 44.0);
        assert_eq!(styles.get(inner_root).padding.bottom, 12.0);
        // `@scope (.inner)` has 1 ancestor hop to `#target`, beating `@scope (.outer)` (2 hops)
        // even though the second `@scope (.outer)` rule appears later in source order.
        assert_eq!(styles.get(target).width, Size::Px(220.0));
        // `--box-gap` is registered with `inherits: false` and `initial-value: 18px`,
        // so `#target` does not inherit `44px` from `#outer_root` and falls back to `18px`.
        assert_eq!(styles.get(target).margin.top, 18.0);
    }

    #[test]
    fn resolves_css_wide_keywords_and_all_shorthand() {
        let doc = parse_html(
            "<div id=\"parent\" style=\"color: rgb(10, 20, 30); width: 240px\">
               <h1 id=\"heading\">Title</h1>
               <p id=\"child\">Body</p>
               <div id=\"reset_box\">Reset</div>
             </div>",
        );
        let sheet = parse_css(
            "#heading { font-size: 40px; font-weight: 400; font-size: revert; font-weight: revert; }
             #child { width: inherit; color: initial; }
             #reset_box { display: flex; width: 180px; padding: 12px; all: initial; }",
        );
        let styles = compute_styles(&doc, &sheet);
        let heading = doc.get_element_by_id("heading").unwrap();
        let child = doc.get_element_by_id("child").unwrap();
        let reset_box = doc.get_element_by_id("reset_box").unwrap();

        // `revert` restores UA `<h1>` defaults (32px, bold 700).
        assert_eq!(styles.get(heading).font_size, 32.0);
        assert_eq!(styles.get(heading).font_weight, 700);
        // `inherit` on non-inherited `width` copies parent's 240px; `initial` on `color` resets to black.
        assert_eq!(styles.get(child).width, Size::Px(240.0));
        assert_eq!(styles.get(child).color, Color::BLACK);
        // `all: initial` resets `display`, `width`, and `padding` to initial values.
        assert_eq!(styles.get(reset_box).display, Display::Inline);
        assert_eq!(styles.get(reset_box).width, Size::Auto);
        assert_eq!(styles.get(reset_box).padding, Edge::zero());
    }

    #[test]
    fn parses_complete_flexbox_and_grid_declarations() {
        let doc =
            parse_html("<div id=\"flex\"></div><div id=\"grid\"></div><div id=\"item\"></div>");
        let sheet = parse_css(
            "#flex {
               display: flex;
               flex-flow: column-reverse wrap-reverse;
               align-content: space-between;
               align-items: baseline;
             }
             #grid {
               display: grid;
               grid-template-columns: min-content max-content fit-content(160px);
               grid-template-areas: \"header header\" \"nav main\";
               grid-auto-rows: 30px 50px;
               grid-auto-columns: 70px 90px;
               grid-auto-flow: row dense;
               place-items: center end;
               place-content: space-evenly center;
             }
             #item {
               order: -2;
               grid-area: main;
               place-self: end start;
               margin: auto;
             }",
        );
        let styles = compute_styles(&doc, &sheet);
        let flex = styles.get(doc.get_element_by_id("flex").unwrap());
        let grid = styles.get(doc.get_element_by_id("grid").unwrap());
        let item = styles.get(doc.get_element_by_id("item").unwrap());

        assert_eq!(flex.flex_direction, FlexDirection::ColumnReverse);
        assert_eq!(flex.flex_wrap, FlexWrap::WrapReverse);
        assert_eq!(flex.align_content, AlignContent::SpaceBetween);
        assert_eq!(flex.align_items, AlignItems::Baseline);

        assert_eq!(
            grid.grid_tracks(),
            &[
                GridTrack::MinContent,
                GridTrack::MaxContent,
                GridTrack::FitContent(160.0),
            ]
        );
        assert_eq!(
            grid.grid_template_areas.find_area(grid_area_hash("header")),
            Some((1, 1, 2, 3))
        );
        assert_eq!(
            grid.grid_template_areas.find_area(grid_area_hash("main")),
            Some((2, 2, 3, 3))
        );
        assert_eq!(
            grid.grid_auto_row_tracks(),
            &[GridTrack::Px(30.0), GridTrack::Px(50.0)]
        );
        assert_eq!(
            grid.grid_auto_column_tracks(),
            &[GridTrack::Px(70.0), GridTrack::Px(90.0)]
        );
        assert_eq!(grid.grid_auto_flow, GridAutoFlow::RowDense);
        assert_eq!(grid.align_items, AlignItems::Center);
        assert_eq!(grid.justify_items, AlignItems::End);
        assert_eq!(grid.align_content, AlignContent::SpaceEvenly);
        assert_eq!(grid.justify_content, JustifyContent::Center);

        assert_eq!(item.order, -2);
        assert_eq!(item.grid_area_name, grid_area_hash("main"));
        assert_eq!(item.align_self, Some(AlignItems::End));
        assert_eq!(item.justify_self, Some(AlignItems::Start));
        assert!(item.margin_top_auto);
        assert!(item.margin_right_auto);
        assert!(item.margin_bottom_auto);
        assert!(item.margin_left_auto);
    }

    #[test]
    fn evaluates_interactive_pseudo_classes_and_css_nesting() {
        let mut doc = parse_html(
            "<div id=\"card\" class=\"card\">
               <a id=\"cta\" href=\"#sec\">Action</a>
               <input id=\"email\" type=\"email\" required value=\"bad-email\" />
               <input id=\"qty\" type=\"number\" min=\"1\" max=\"10\" value=\"25\" />
             </div>
             <section id=\"sec\"><p id=\"sec_p\">Target Section</p></section>",
        );
        let sheet = parse_css(
            ".card {
               padding: 10px;
               &:hover {
                 padding: 20px;
               }
               &:focus-within {
                 border: 3px solid rgb(10, 20, 30);
               }
               > a {
                 color: rgb(1, 2, 3);
                 &:active {
                   color: rgb(200, 10, 20);
                 }
               }
               input:invalid {
                 background-color: rgb(255, 0, 0);
               }
               input:valid {
                 background-color: rgb(0, 255, 0);
               }
               input:out-of-range {
                 width: 80px;
               }
               input:in-range {
                 width: 160px;
               }
             }
             #sec:target {
               margin-top: 32px;
               p {
                 font-weight: 700;
               }
             }",
        );

        let card = doc.get_element_by_id("card").unwrap();
        let cta = doc.get_element_by_id("cta").unwrap();
        let email = doc.get_element_by_id("email").unwrap();
        let qty = doc.get_element_by_id("qty").unwrap();
        let sec = doc.get_element_by_id("sec").unwrap();
        let sec_p = doc.get_element_by_id("sec_p").unwrap();

        let initial = compute_styles(&doc, &sheet);
        assert_eq!(initial.get(card).padding.top, 10.0);
        assert_eq!(initial.get(card).border.top, 0.0);
        assert_eq!(
            initial.get(cta).color,
            Color {
                r: 1,
                g: 2,
                b: 3,
                a: 255
            }
        );
        assert_eq!(
            initial.get(email).background,
            Some(Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            })
        );
        assert_eq!(initial.get(qty).width, Size::Px(80.0));
        assert_eq!(initial.get(sec).margin.top, 0.0);

        doc.set_hovered_node(Some(cta));
        doc.set_active_node(Some(cta));
        doc.set_focused_node(Some(email));
        doc.set_target_fragment(Some("sec"));
        doc.set_input_value(email, "user@example.com");
        doc.set_input_value(qty, "5");

        let updated = compute_styles(&doc, &sheet);
        assert_eq!(updated.get(card).padding.top, 20.0);
        assert_eq!(updated.get(card).border.top, 3.0);
        assert_eq!(
            updated.get(cta).color,
            Color {
                r: 200,
                g: 10,
                b: 20,
                a: 255
            }
        );
        assert_eq!(
            updated.get(email).background,
            Some(Color {
                r: 0,
                g: 255,
                b: 0,
                a: 255
            })
        );
        assert_eq!(updated.get(qty).width, Size::Px(160.0));
        assert_eq!(updated.get(sec).margin.top, 32.0);
        assert_eq!(updated.get(sec_p).font_weight, 700);
    }

    #[test]
    fn parses_builtin_ua_stylesheets_and_executes_cpp_css_engine_kernels() {
        let sheets = prism::builtin_user_agent_stylesheets();
        assert_eq!(sheets.len(), 4);
        for css_src in sheets {
            let parsed = prism::parse_stylesheet(css_src, &prism::Options::default())
                .expect("built-in UA stylesheet should parse within limits");
            assert!(!parsed.rules.is_empty());
            let cascade_sheet = parse_css(css_src);
            assert!(!cascade_sheet.rules.is_empty());
        }

        assert_eq!(prism::classify_pseudo_element_fast(".badge::before"), 1);
        assert_eq!(prism::classify_pseudo_element_fast(".badge::after"), 2);
        assert_eq!(prism::classify_pseudo_element_fast(".badge:hover"), 0);
        assert_eq!(
            prism::resolve_length_px_fast(2.0, 2, 14.0, 16.0, 1280.0, 720.0, 400.0),
            32.0
        );
        assert_eq!(prism::eval_clamp_fast(12.0, 40.0, 24.0), 24.0);
        let center_px = prism::sample_radial_gradient_fast(0.5, 0.5, 0xFF00_00FF, 0x0000_FFFF);
        assert_eq!(center_px, 0xFF00_00FF);
        assert_eq!(
            prism::pack_specificity_fast(1, 2, 3),
            (1 << 20) | (2 << 10) | 3
        );
    }

    #[test]
    fn parses_font_face_typography_overflow_scroll_object_fit_outline_and_filters() {
        let doc = parse_html(
            "<div id=\"card\">
               <code id=\"code_el\">const x = 1;</code>
               <del id=\"del_el\">obsolete</del>
               <img id=\"hero_img\" src=\"hero.png\" />
             </div>",
        );
        let sheet = parse_css(
            "@font-face {
               font-family: \"InterVariable\";
               src: url(\"fonts/inter.woff2\") format(\"woff2\");
               font-weight: 700;
               font-style: italic;
             }
             #card {
               font-family: Georgia, serif;
               text-transform: uppercase;
               letter-spacing: 2px;
               word-spacing: 4px;
               overflow: auto;
               border: 2px solid #112233;
               border-top-color: #ff0000;
               border-bottom-color: #00ff00;
               outline: 3px solid #336699;
               outline-offset: 4px;
               background-image: conic-gradient(from 45deg, #102030, #405060);
               backdrop-filter: blur(8px);
               filter: blur(2px) brightness(120%) contrast(1.5) grayscale(50%);
             }
             #hero_img {
               object-fit: cover;
             }",
        );
        assert_eq!(sheet.font_faces.len(), 1);
        assert_eq!(sheet.font_faces[0].family, "InterVariable");
        assert_eq!(sheet.font_faces[0].src_url, "fonts/inter.woff2");
        assert_eq!(sheet.font_faces[0].weight, 700);
        assert!(sheet.font_faces[0].italic);

        let styles = compute_styles(&doc, &sheet);
        let card = styles.get(doc.get_element_by_id("card").unwrap());
        let code_el = styles.get(doc.get_element_by_id("code_el").unwrap());
        let del_el = styles.get(doc.get_element_by_id("del_el").unwrap());
        let hero_img = styles.get(doc.get_element_by_id("hero_img").unwrap());

        assert_eq!(card.font_family, FontFamily::Serif);
        assert_eq!(card.text_transform, TextTransform::Uppercase);
        assert_eq!(card.letter_spacing, 2.0);
        assert_eq!(card.word_spacing, 4.0);
        assert_eq!(card.overflow, Overflow::Hidden);
        assert!(card.scrollable_overflow);
        assert_eq!(
            card.border_colors[0],
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
        assert_eq!(
            card.border_colors[2],
            Color {
                r: 0,
                g: 255,
                b: 0,
                a: 255
            }
        );
        assert_eq!(card.outline_width, 3.0);
        assert_eq!(card.outline_offset, 4.0);
        assert!(card.background_gradient.is_some());
        assert_eq!(card.backdrop_blur, 8.0);
        assert_eq!(card.filter_blur, 2.0);
        assert!((card.filter_brightness - 1.2).abs() < 1e-4);
        assert!((card.filter_contrast - 1.5).abs() < 1e-4);
        assert!((card.filter_grayscale - 0.5).abs() < 1e-4);

        assert_eq!(code_el.font_family, FontFamily::Monospace);
        assert_eq!(code_el.text_transform, TextTransform::Uppercase);
        assert!(del_el.line_through);
        assert_eq!(hero_img.object_fit, ObjectFit::Cover);
    }

    #[test]
    fn shadow_dom_scoped_styles_slotted_part_and_incremental_cascade_work() {
        let mut doc = anvil_dom::parse_html(
            "<style>\
               .inner { color: #112233; }\
               #host::part(banner) { background-color: #445566; }\
               #box { opacity: 1; width: 100px; transition: opacity 200ms linear, width 200ms linear; }\
               #box.active { opacity: 0.2; width: 300px; }\
             </style>\
             <div id=\"outer\" class=\"inner\">Outer</div>\
             <div id=\"host\" class=\"card\">\
               <span id=\"slotted-title\" slot=\"title\" class=\"hl\">Slotted Title</span>\
             </div>\
             <div id=\"box\">Box</div>",
        );
        let host = doc.get_element_by_id("host").unwrap();
        let outer = doc.get_element_by_id("outer").unwrap();
        let slotted_title = doc.get_element_by_id("slotted-title").unwrap();
        let box_id = doc.get_element_by_id("box").unwrap();

        let sr = doc.attach_shadow(host, true);
        doc.set_inner_html(
            sr,
            "<style>\
               :host(.card) { border: 2px solid #abcdef; }\
               .inner { color: #ff00aa; }\
               slot[name=\"title\"]::slotted(.hl) { color: #00cc88; }\
             </style>\
             <header id=\"hdr\" class=\"inner\" part=\"banner\">\
               <slot name=\"title\"></slot>\
             </header>",
        );
        doc.clear_dirty_styles();

        let sheet = parse_css(&doc.embedded_css());
        let mut styles = compute_styles_for_viewport(&doc, &sheet, 800.0, 600.0);
        let hdr = doc.query_selector_within(sr, "#hdr").unwrap();

        // Shadow `:host(.card)` styled `#host`
        assert_eq!(styles.get(host).border.top, 2.0);
        assert_eq!(
            styles.get(host).border_color,
            Color::parse("#abcdef").unwrap()
        );
        // Shadow `.inner` styled `#hdr` without leaking to `#outer`
        assert_eq!(styles.get(hdr).color, Color::parse("#ff00aa").unwrap());
        assert_eq!(styles.get(outer).color, Color::parse("#112233").unwrap());
        // Document `#host::part(banner)` styled `#hdr`'s background
        assert_eq!(
            styles.get(hdr).background,
            Some(Color::parse("#445566").unwrap())
        );
        // Shadow `slot[name="title"]::slotted(.hl)` styled `#slotted-title`
        assert_eq!(
            styles.get(slotted_title).color,
            Color::parse("#00cc88").unwrap()
        );

        // Incremental cascade + CSS transition on `#box` class mutation
        let mut animator = Animator::new();
        animator.tick(0.0);
        assert_eq!(
            compute_styles_incremental(
                &mut doc,
                &sheet,
                &mut styles,
                800.0,
                600.0,
                Some(&mut animator)
            ),
            0
        );

        doc.add_class(box_id, "active");
        let updated = compute_styles_incremental(
            &mut doc,
            &sheet,
            &mut styles,
            800.0,
            600.0,
            Some(&mut animator),
        );
        assert!(updated >= 1);
        assert!(animator.is_animating());

        // Advance clock to 100ms (50% of 200ms linear transition)
        assert!(animator.tick(100.0));
        let _ = compute_styles_incremental(
            &mut doc,
            &sheet,
            &mut styles,
            800.0,
            600.0,
            Some(&mut animator),
        );
        assert!((styles.get(box_id).opacity - 0.6).abs() < 1e-4);
        assert_eq!(styles.get(box_id).width, Size::Px(200.0));
    }
}
