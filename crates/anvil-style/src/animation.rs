// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato
//! Transitions and `@keyframes` animations: timing functions,
//! interpolation, and the [`Animator`] that runs them against a clock.
//!
//! See [`Animator`] for how a frame loop drives them.

use crate::{
    Color, ComputedStyle, Declaration, Display, Edge, Size, TransformFn, TransformList,
    apply_declaration, find_matching_brace, find_unquoted, parse_declarations,
};
use anvil_dom::NodeId;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

/// Most entries kept in each `transition-*` list. Longer lists are ignored.
pub const MAX_TRANSITIONS: usize = 16;

/// Where the jumps of `steps()` fall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepPosition {
    /// `jump-start` / `start`: the first jump happens at the start.
    Start,
    /// `jump-end` / `end`: the last jump happens at the end.
    End,
    /// `jump-none`: no jump at either end.
    None,
    /// `jump-both`: a jump at both ends.
    Both,
}

/// An easing function: maps progress in `0..=1` to output progress.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TimingFunction {
    Linear,
    /// `cubic-bezier(x1, y1, x2, y2)`. `ease`, `ease-in`, `ease-out`, and
    /// `ease-in-out` are stored as their curves.
    CubicBezier(f32, f32, f32, f32),
    /// `steps(n, position)`. `step-start` and `step-end` are stored as one step.
    Steps(u32, StepPosition),
}

impl TimingFunction {
    /// `ease`, the initial value.
    pub const EASE: Self = Self::CubicBezier(0.25, 0.1, 0.25, 1.0);

    /// Parse one timing function. Bezier `x` values must be in `0..=1`;
    /// `steps()` needs a positive count, and at least 2 for `jump-none`.
    pub fn parse(value: &str) -> Option<Self> {
        let lower = value.trim().to_ascii_lowercase();
        Some(match lower.as_str() {
            "linear" => Self::Linear,
            "ease" => Self::EASE,
            "ease-in" => Self::CubicBezier(0.42, 0.0, 1.0, 1.0),
            "ease-out" => Self::CubicBezier(0.0, 0.0, 0.58, 1.0),
            "ease-in-out" => Self::CubicBezier(0.42, 0.0, 0.58, 1.0),
            "step-start" => Self::Steps(1, StepPosition::Start),
            "step-end" => Self::Steps(1, StepPosition::End),
            _ => {
                let (name, args) = lower.strip_suffix(')')?.split_once('(')?;
                let args: Vec<&str> = args.split(',').map(str::trim).collect();
                match (name.trim(), args.as_slice()) {
                    ("cubic-bezier", [x1, y1, x2, y2]) => {
                        let n = |s: &str| s.parse::<f32>().ok().filter(|v| v.is_finite());
                        let (x1, y1, x2, y2) = (n(x1)?, n(y1)?, n(x2)?, n(y2)?);
                        if !(0.0..=1.0).contains(&x1) || !(0.0..=1.0).contains(&x2) {
                            return None;
                        }
                        Self::CubicBezier(x1, y1, x2, y2)
                    }
                    ("steps", [count, rest @ ..]) if rest.len() <= 1 => {
                        let count = count.parse::<u32>().ok().filter(|&c| c > 0)?;
                        let position = match rest.first().copied() {
                            None | Some("jump-end") | Some("end") => StepPosition::End,
                            Some("jump-start") | Some("start") => StepPosition::Start,
                            Some("jump-none") => StepPosition::None,
                            Some("jump-both") => StepPosition::Both,
                            Some(_) => return None,
                        };
                        if position == StepPosition::None && count < 2 {
                            return None;
                        }
                        Self::Steps(count, position)
                    }
                    _ => return None,
                }
            }
        })
    }

    /// Output progress for input progress `p`, which is clamped to `0..=1`.
    pub fn eval(&self, p: f64) -> f64 {
        let p = if p.is_nan() { 0.0 } else { p.clamp(0.0, 1.0) };
        match *self {
            Self::Linear => p,
            Self::CubicBezier(x1, y1, x2, y2) => bezier_y_for_x(
                f64::from(x1),
                f64::from(y1),
                f64::from(x2),
                f64::from(y2),
                p,
            ),
            Self::Steps(count, position) => {
                let count = f64::from(count);
                let mut step = (p * count).floor();
                if matches!(position, StepPosition::Start | StepPosition::Both) {
                    step += 1.0;
                }
                let jumps = match position {
                    StepPosition::Start | StepPosition::End => count,
                    StepPosition::None => count - 1.0,
                    StepPosition::Both => count + 1.0,
                };
                step.min(jumps) / jumps
            }
        }
    }
}

/// One coordinate of a cubic Bezier from `(0, 0)` to `(1, 1)` with control
/// values `c1` and `c2`, at parameter `t`.
fn bezier(c1: f64, c2: f64, t: f64) -> f64 {
    let u = 1.0 - t;
    3.0 * u * u * t * c1 + 3.0 * u * t * t * c2 + t * t * t
}

/// The curve's `y` where its `x` equals `x`. Newton steps from a linear
/// guess, then bisection when they stall; `x` is monotonic because both
/// control `x` values are in `0..=1`.
fn bezier_y_for_x(x1: f64, y1: f64, x2: f64, y2: f64, x: f64) -> f64 {
    if x <= 0.0 || x >= 1.0 {
        return x;
    }
    let mut t = x;
    for _ in 0..8 {
        let err = bezier(x1, x2, t) - x;
        if err.abs() < 1e-9 {
            return bezier(y1, y2, t);
        }
        let u = 1.0 - t;
        let slope = 3.0 * u * u * x1 + 6.0 * u * t * (x2 - x1) + 3.0 * t * t * (1.0 - x2);
        if slope.abs() < 1e-9 {
            break;
        }
        t = (t - err / slope).clamp(0.0, 1.0);
    }
    let (mut lo, mut hi) = (0.0, 1.0);
    t = x;
    for _ in 0..60 {
        let value = bezier(x1, x2, t);
        if (value - x).abs() < 1e-9 {
            break;
        }
        if value < x {
            lo = t;
        } else {
            hi = t;
        }
        t = (lo + hi) / 2.0;
    }
    bezier(y1, y2, t)
}

/// A property that transitions can move. `All` stands for every one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AnimProperty {
    All,
    Opacity,
    Transform,
    Color,
    BackgroundColor,
    BorderColor,
    Width,
    Height,
    MaxWidth,
    Top,
    Right,
    Bottom,
    Left,
    MarginTop,
    MarginRight,
    MarginBottom,
    MarginLeft,
    PaddingTop,
    PaddingRight,
    PaddingBottom,
    PaddingLeft,
    BorderRadius,
    FontSize,
    RowGap,
    ColumnGap,
    /// The `gap` shorthand in a `transition` list: it stands for
    /// [`Self::RowGap`] and [`Self::ColumnGap`] and has no value of its own.
    Gap,
    /// A name this engine does not animate. It still takes its place in
    /// the list so durations line up.
    Other,
}

/// Every property [`AnimProperty::All`] covers, in a fixed order.
pub const ANIMATABLE: [AnimProperty; 24] = [
    AnimProperty::Opacity,
    AnimProperty::Transform,
    AnimProperty::Color,
    AnimProperty::BackgroundColor,
    AnimProperty::BorderColor,
    AnimProperty::Width,
    AnimProperty::Height,
    AnimProperty::MaxWidth,
    AnimProperty::Top,
    AnimProperty::Right,
    AnimProperty::Bottom,
    AnimProperty::Left,
    AnimProperty::MarginTop,
    AnimProperty::MarginRight,
    AnimProperty::MarginBottom,
    AnimProperty::MarginLeft,
    AnimProperty::PaddingTop,
    AnimProperty::PaddingRight,
    AnimProperty::PaddingBottom,
    AnimProperty::PaddingLeft,
    AnimProperty::BorderRadius,
    AnimProperty::FontSize,
    AnimProperty::RowGap,
    AnimProperty::ColumnGap,
];

impl AnimProperty {
    /// The property for a CSS name. Shorthands are not expanded here.
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "all" => Self::All,
            "opacity" => Self::Opacity,
            "transform" => Self::Transform,
            "color" => Self::Color,
            "background-color" | "background" => Self::BackgroundColor,
            "border-color" => Self::BorderColor,
            "width" => Self::Width,
            "height" => Self::Height,
            "max-width" => Self::MaxWidth,
            "top" => Self::Top,
            "right" => Self::Right,
            "bottom" => Self::Bottom,
            "left" => Self::Left,
            "margin-top" => Self::MarginTop,
            "margin-right" => Self::MarginRight,
            "margin-bottom" => Self::MarginBottom,
            "margin-left" => Self::MarginLeft,
            "padding-top" => Self::PaddingTop,
            "padding-right" => Self::PaddingRight,
            "padding-bottom" => Self::PaddingBottom,
            "padding-left" => Self::PaddingLeft,
            "border-radius" => Self::BorderRadius,
            "font-size" => Self::FontSize,
            "row-gap" | "grid-row-gap" => Self::RowGap,
            "column-gap" | "grid-column-gap" => Self::ColumnGap,
            "gap" | "grid-gap" => Self::Gap,
            _ => Self::Other,
        }
    }
}

/// The `transition-*` longhands as parsed lists. Lists shorter than
/// `transition-property` repeat, as in CSS.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transitions {
    properties: [AnimProperty; MAX_TRANSITIONS],
    property_count: u8,
    durations_ms: [f32; MAX_TRANSITIONS],
    duration_count: u8,
    delays_ms: [f32; MAX_TRANSITIONS],
    delay_count: u8,
    timings: [TimingFunction; MAX_TRANSITIONS],
    timing_count: u8,
}

/// How one property transitions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransitionSpec {
    pub duration_ms: f32,
    pub delay_ms: f32,
    pub timing: TimingFunction,
}

impl Transitions {
    /// `all 0s ease 0s`: nothing transitions.
    pub const INITIAL: Self = Self {
        properties: [AnimProperty::All; MAX_TRANSITIONS],
        property_count: 1,
        durations_ms: [0.0; MAX_TRANSITIONS],
        duration_count: 1,
        delays_ms: [0.0; MAX_TRANSITIONS],
        delay_count: 1,
        timings: [TimingFunction::EASE; MAX_TRANSITIONS],
        timing_count: 1,
    };

    /// The listed properties; empty for `none`.
    pub fn properties(&self) -> &[AnimProperty] {
        &self.properties[..usize::from(self.property_count).min(MAX_TRANSITIONS)]
    }

    /// How `property` transitions, from the last list entry naming it or
    /// `all`. `None` when no entry applies or the duration is 0.
    pub fn spec_for(&self, property: AnimProperty) -> Option<TransitionSpec> {
        let index = self.properties().iter().rposition(|p| {
            *p == property
                || *p == AnimProperty::All
                || (*p == AnimProperty::Gap
                    && matches!(property, AnimProperty::RowGap | AnimProperty::ColumnGap))
        })?;
        let pick = |count: u8| index % usize::from(count.max(1));
        let spec = TransitionSpec {
            duration_ms: self.durations_ms[pick(self.duration_count)],
            delay_ms: self.delays_ms[pick(self.delay_count)],
            timing: self.timings[pick(self.timing_count)],
        };
        (spec.duration_ms > 0.0).then_some(spec)
    }
}

/// Split on commas outside parentheses.
fn split_top_level(value: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in value.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 && (c == separator || (separator == ' ' && c.is_whitespace())) => {
                parts.push(&value[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
        .into_iter()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect()
}

/// A CSS time in milliseconds: `1.5s` or `200ms`. Non-finite values fail.
pub(crate) fn parse_time_ms(value: &str) -> Option<f32> {
    let lower = value.trim().to_ascii_lowercase();
    let ms = if let Some(n) = lower.strip_suffix("ms") {
        n.parse::<f32>().ok()?
    } else {
        lower.strip_suffix('s')?.parse::<f32>().ok()? * 1000.0
    };
    ms.is_finite().then_some(ms)
}

/// Fill a fixed list from comma-separated `value`. Fails on an empty list,
/// a list longer than `out`, or any bad entry.
fn parse_list<T: Copy, const N: usize>(
    value: &str,
    out: &mut [T; N],
    parse: impl Fn(&str) -> Option<T>,
) -> Option<u8> {
    let parts = split_top_level(value, ',');
    if parts.is_empty() || parts.len() > N {
        return None;
    }
    let mut parsed = *out;
    for (slot, part) in parsed.iter_mut().zip(&parts) {
        *slot = parse(part)?;
    }
    *out = parsed;
    Some(parts.len() as u8)
}

/// Apply one `transition` or `transition-*` declaration. Returns false
/// when `name` is not one of them; a bad value leaves the lists alone.
pub(crate) fn apply_transition_declaration(t: &mut Transitions, name: &str, value: &str) -> bool {
    match name {
        "transition-property" => {
            if value.trim().eq_ignore_ascii_case("none") {
                t.property_count = 0;
            } else if let Some(n) = parse_list(value, &mut t.properties, |p| {
                let p = p.trim();
                let ok = !p.is_empty() && !p.contains(char::is_whitespace);
                ok.then(|| AnimProperty::from_name(p))
            }) {
                t.property_count = n;
            }
        }
        "transition-duration" => {
            if let Some(n) = parse_list(value, &mut t.durations_ms, |v| {
                parse_time_ms(v).filter(|ms| *ms >= 0.0)
            }) {
                t.duration_count = n;
            }
        }
        "transition-delay" => {
            if let Some(n) = parse_list(value, &mut t.delays_ms, parse_time_ms) {
                t.delay_count = n;
            }
        }
        "transition-timing-function" => {
            if let Some(n) = parse_list(value, &mut t.timings, TimingFunction::parse) {
                t.timing_count = n;
            }
        }
        "transition" => {
            if let Some(parsed) = parse_transition_shorthand(value) {
                *t = parsed;
            }
        }
        _ => return false,
    }
    true
}

/// `transition: <property> <duration> <timing> <delay>, ...` in any order
/// within an entry. The first time is the duration, the second the delay.
fn parse_transition_shorthand(value: &str) -> Option<Transitions> {
    let entries = split_top_level(value, ',');
    if entries.is_empty() || entries.len() > MAX_TRANSITIONS {
        return None;
    }
    let mut t = Transitions::INITIAL;
    let count = entries.len() as u8;
    t.property_count = count;
    t.duration_count = count;
    t.delay_count = count;
    t.timing_count = count;
    for (i, entry) in entries.iter().enumerate() {
        let (mut property, mut duration, mut delay, mut timing) = (None, None, None, None);
        for token in split_top_level(entry, ' ') {
            if let Some(ms) = parse_time_ms(token) {
                if duration.is_none() {
                    if ms < 0.0 {
                        return None;
                    }
                    duration = Some(ms);
                } else if delay.is_none() {
                    delay = Some(ms);
                } else {
                    return None;
                }
            } else if let Some(f) = TimingFunction::parse(token).filter(|_| timing.is_none()) {
                timing = Some(f);
            } else if property.is_none() && !token.contains('(') {
                if token.eq_ignore_ascii_case("none") {
                    if entries.len() > 1 {
                        return None;
                    }
                    t.property_count = 0;
                }
                property = Some(AnimProperty::from_name(token));
            } else {
                return None;
            }
        }
        t.properties[i] = property.unwrap_or(AnimProperty::All);
        t.durations_ms[i] = duration.unwrap_or(0.0);
        t.delays_ms[i] = delay.unwrap_or(0.0);
        t.timings[i] = timing.unwrap_or(TimingFunction::EASE);
    }
    Some(t)
}

/// One property's value, in a form that can be blended.
///
/// The transform variant is much larger than the rest; values stay inline
/// and `Copy` like the [`ComputedStyle`] fields they come from.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AnimValue {
    Number(f32),
    Length(Size),
    Color(Option<Color>),
    Transform(TransformList),
}

/// Read `property` from `style`. `All` and `Other` read as `None`.
pub fn read_value(style: &ComputedStyle, property: AnimProperty) -> Option<AnimValue> {
    use AnimProperty as P;
    use AnimValue as V;
    Some(match property {
        P::All | P::Other | P::Gap => return None,
        P::Opacity => V::Number(style.opacity),
        P::Transform => V::Transform(style.transform),
        P::Color => V::Color(Some(style.color)),
        P::BackgroundColor => {
            if style.background_gradient.is_some() {
                return None;
            }
            V::Color(style.background)
        }
        P::BorderColor => V::Color(Some(style.border_color)),
        P::Width => V::Length(style.width),
        P::Height => V::Length(style.height),
        P::MaxWidth => V::Length(style.max_width),
        P::Top => V::Length(style.top),
        P::Right => V::Length(style.right),
        P::Bottom => V::Length(style.bottom),
        P::Left => V::Length(style.left),
        P::MarginTop => V::Number(style.margin.top),
        P::MarginRight => V::Number(style.margin.right),
        P::MarginBottom => V::Number(style.margin.bottom),
        P::MarginLeft => V::Number(style.margin.left),
        P::PaddingTop => V::Number(style.padding.top),
        P::PaddingRight => V::Number(style.padding.right),
        P::PaddingBottom => V::Number(style.padding.bottom),
        P::PaddingLeft => V::Number(style.padding.left),
        P::BorderRadius => V::Number(style.border_radius),
        P::FontSize => V::Number(style.font_size),
        P::RowGap => V::Length(style.row_gap),
        P::ColumnGap => V::Length(style.column_gap),
    })
}

/// Write `value` to `property` on `style`. A value of the wrong kind is
/// ignored. Opacity is clamped to `0..=1`; sizes that cannot be negative
/// are floored at 0.
pub fn write_value(style: &mut ComputedStyle, property: AnimProperty, value: AnimValue) {
    use AnimProperty as P;
    use AnimValue as V;
    let edge = |edge: &mut Edge, side: P, v: f32| match side {
        P::MarginTop | P::PaddingTop => edge.top = v,
        P::MarginRight | P::PaddingRight => edge.right = v,
        P::MarginBottom | P::PaddingBottom => edge.bottom = v,
        _ => edge.left = v,
    };
    match (property, value) {
        (P::Opacity, V::Number(v)) => style.opacity = v.clamp(0.0, 1.0),
        (P::Transform, V::Transform(list)) => style.transform = list,
        (P::Color, V::Color(Some(c))) => style.color = c,
        (P::BackgroundColor, V::Color(c)) => style.background = c,
        (P::BorderColor, V::Color(Some(c))) => style.border_color = c,
        (P::Width, V::Length(s)) => style.width = s,
        (P::Height, V::Length(s)) => style.height = s,
        (P::MaxWidth, V::Length(s)) => style.max_width = s,
        (P::Top, V::Length(s)) => style.top = s,
        (P::Right, V::Length(s)) => style.right = s,
        (P::Bottom, V::Length(s)) => style.bottom = s,
        (P::Left, V::Length(s)) => style.left = s,
        (P::MarginTop | P::MarginRight | P::MarginBottom | P::MarginLeft, V::Number(v)) => {
            edge(&mut style.margin, property, v);
        }
        (P::PaddingTop | P::PaddingRight | P::PaddingBottom | P::PaddingLeft, V::Number(v)) => {
            edge(&mut style.padding, property, v.max(0.0));
        }
        (P::BorderRadius, V::Number(v)) => style.border_radius = v.max(0.0),
        (P::FontSize, V::Number(v)) => style.font_size = v.max(0.0),
        (P::RowGap, V::Length(s)) => style.row_gap = s,
        (P::ColumnGap, V::Length(s)) => style.column_gap = s,
        _ => {}
    }
}

/// `a` at 0 and `b` at 1. Weighting both ends keeps each one exact, even
/// when they are so far apart that `b - a` loses the smaller one.
fn mix(a: f64, b: f64, t: f64) -> f64 {
    a * (1.0 - t) + b * t
}

fn lerp(a: f32, b: f32, t: f64) -> f32 {
    mix(f64::from(a), f64::from(b), t) as f32
}

/// Blend `from` toward `to` by `t` (0 is `from`, 1 is `to`; values past
/// either end extrapolate, as easing curves can). `None` when the two
/// cannot be blended, such as `auto` and a length.
pub fn interpolate(from: AnimValue, to: AnimValue, t: f64) -> Option<AnimValue> {
    use AnimValue as V;
    Some(match (from, to) {
        (V::Number(a), V::Number(b)) => V::Number(lerp(a, b, t)),
        (V::Length(a), V::Length(b)) => V::Length(interpolate_size(a, b, t)?),
        (V::Color(a), V::Color(b)) => V::Color(Some(interpolate_color(a, b, t))),
        (V::Transform(a), V::Transform(b)) => V::Transform(interpolate_transform(&a, &b, t)?),
        _ => return None,
    })
}

/// Blend two lengths of the same unit, or two from `px`, `%`, `em`, and
/// `rem` through a `calc()` mix. `auto` and mixed viewport or `ch` units
/// do not blend.
fn interpolate_size(a: Size, b: Size, t: f64) -> Option<Size> {
    let same = |x: f32, y: f32| lerp(x, y, t);
    Some(match (a, b) {
        (Size::Px(x), Size::Px(y)) => Size::Px(same(x, y)),
        (Size::Percent(x), Size::Percent(y)) => Size::Percent(same(x, y)),
        (Size::Em(x), Size::Em(y)) => Size::Em(same(x, y)),
        (Size::Rem(x), Size::Rem(y)) => Size::Rem(same(x, y)),
        (Size::Vw(x), Size::Vw(y)) => Size::Vw(same(x, y)),
        (Size::Vh(x), Size::Vh(y)) => Size::Vh(same(x, y)),
        (Size::Vmin(x), Size::Vmin(y)) => Size::Vmin(same(x, y)),
        (Size::Vmax(x), Size::Vmax(y)) => Size::Vmax(same(x, y)),
        (Size::Ch(x), Size::Ch(y)) => Size::Ch(same(x, y)),
        _ => {
            let (a, b) = (calc_parts(a)?, calc_parts(b)?);
            Size::Calc {
                px: same(a[0], b[0]),
                percent: same(a[1], b[1]),
                em: same(a[2], b[2]),
                rem: same(a[3], b[3]),
            }
        }
    })
}

/// `[px, percent, em, rem]` for a size that `calc()` can hold.
fn calc_parts(size: Size) -> Option<[f32; 4]> {
    Some(match size {
        Size::Px(v) => [v, 0.0, 0.0, 0.0],
        Size::Percent(v) => [0.0, v, 0.0, 0.0],
        Size::Em(v) => [0.0, 0.0, v, 0.0],
        Size::Rem(v) => [0.0, 0.0, 0.0, v],
        Size::Calc {
            px,
            percent,
            em,
            rem,
        } => [px, percent, em, rem],
        _ => return None,
    })
}

/// Blend colors with premultiplied alpha, so a fade from transparent does
/// not pass through black. No color is transparent black.
fn interpolate_color(a: Option<Color>, b: Option<Color>, t: f64) -> Color {
    let clear = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };
    let (a, b) = (a.unwrap_or(clear), b.unwrap_or(clear));
    let pre = |c: Color| {
        let alpha = f64::from(c.a) / 255.0;
        [
            f64::from(c.r) * alpha,
            f64::from(c.g) * alpha,
            f64::from(c.b) * alpha,
            alpha,
        ]
    };
    let (pa, pb) = (pre(a), pre(b));
    let blend = |i: usize| mix(pa[i], pb[i], t);
    let alpha = blend(3).clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return clear;
    }
    let channel = |i: usize| (blend(i) / alpha).round().clamp(0.0, 255.0) as u8;
    Color {
        r: channel(0),
        g: channel(1),
        b: channel(2),
        a: (alpha * 255.0).round() as u8,
    }
}

/// The identity function of the same kind as `f`, for blending with `none`.
fn neutral(f: TransformFn) -> TransformFn {
    match f {
        TransformFn::Translate(..) => TransformFn::Translate(Size::Px(0.0), Size::Px(0.0)),
        TransformFn::Scale(..) => TransformFn::Scale(1.0, 1.0),
        TransformFn::Rotate(_) => TransformFn::Rotate(0.0),
        TransformFn::Skew(..) => TransformFn::Skew(0.0, 0.0),
        TransformFn::Matrix(_) => TransformFn::Matrix([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
    }
}

/// Blend two transform lists. Lists whose functions match kind by kind
/// (with `none` standing for identities) blend function by function.
/// Anything else blends as one matrix each, split into move, turn, skew,
/// and scale; that needs both lists in plain pixels, so a list with a
/// percentage or font-relative move against a different list does not blend.
fn interpolate_transform(a: &TransformList, b: &TransformList, t: f64) -> Option<TransformList> {
    let (fa, fb) = (a.functions(), b.functions());
    let fa: Vec<TransformFn> = if fa.is_empty() {
        fb.iter().map(|f| neutral(*f)).collect()
    } else {
        fa.to_vec()
    };
    let fb: Vec<TransformFn> = if fb.is_empty() {
        fa.iter().map(|f| neutral(*f)).collect()
    } else {
        fb.to_vec()
    };
    let pairwise = fa.len() == fb.len()
        && fa.iter().zip(&fb).all(|(x, y)| {
            std::mem::discriminant(x) == std::mem::discriminant(y)
                && !matches!(x, TransformFn::Matrix(_))
        });
    if pairwise {
        let mut out = Vec::with_capacity(fa.len());
        for (x, y) in fa.iter().zip(&fb) {
            out.push(match (*x, *y) {
                (TransformFn::Translate(x1, y1), TransformFn::Translate(x2, y2)) => {
                    TransformFn::Translate(
                        interpolate_size(x1, x2, t)?,
                        interpolate_size(y1, y2, t)?,
                    )
                }
                (TransformFn::Scale(x1, y1), TransformFn::Scale(x2, y2)) => {
                    TransformFn::Scale(lerp(x1, x2, t), lerp(y1, y2, t))
                }
                (TransformFn::Rotate(r1), TransformFn::Rotate(r2)) => {
                    TransformFn::Rotate(lerp(r1, r2, t))
                }
                (TransformFn::Skew(x1, y1), TransformFn::Skew(x2, y2)) => {
                    TransformFn::Skew(lerp(x1, x2, t), lerp(y1, y2, t))
                }
                _ => return None,
            });
        }
        return TransformList::new(&out);
    }
    let (ma, mb) = (pixel_matrix(&fa)?, pixel_matrix(&fb)?);
    let (da, db) = (Decomposed::from_matrix(ma)?, Decomposed::from_matrix(mb)?);
    TransformList::new(&[TransformFn::Matrix(da.blend(&db, t).to_matrix())])
}

/// `[a, b, c, d, e, f]` composition: `outer` after `inner`.
fn mat_mul(outer: [f32; 6], inner: [f32; 6]) -> [f32; 6] {
    let [a, b, c, d, e, f] = outer;
    let [a2, b2, c2, d2, e2, f2] = inner;
    [
        a * a2 + c * b2,
        b * a2 + d * b2,
        a * c2 + c * d2,
        b * c2 + d * d2,
        a * e2 + c * f2 + e,
        b * e2 + d * f2 + f,
    ]
}

/// The list as one matrix, or `None` when a move is not in pixels.
fn pixel_matrix(fns: &[TransformFn]) -> Option<[f32; 6]> {
    let mut m = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    for f in fns {
        let step = match *f {
            TransformFn::Translate(Size::Px(x), Size::Px(y)) => [1.0, 0.0, 0.0, 1.0, x, y],
            TransformFn::Translate(..) => return None,
            TransformFn::Scale(x, y) => [x, 0.0, 0.0, y, 0.0, 0.0],
            TransformFn::Rotate(r) => {
                let (s, c) = r.sin_cos();
                [c, s, -s, c, 0.0, 0.0]
            }
            TransformFn::Skew(x, y) => [1.0, y.tan(), x.tan(), 1.0, 0.0, 0.0],
            TransformFn::Matrix(m) => m,
        };
        m = mat_mul(m, step);
    }
    m.iter().all(|v| v.is_finite()).then_some(m)
}

/// A 2D matrix split into move, turn, shear, and scale, applied as
/// `translate * rotate * shear * scale`.
#[derive(Clone, Copy, Debug)]
struct Decomposed {
    translate: [f64; 2],
    angle: f64,
    shear: f64,
    scale: [f64; 2],
}

impl Decomposed {
    /// Split `m`. `None` for a matrix that flattens the plane.
    fn from_matrix(m: [f32; 6]) -> Option<Self> {
        let [a, b, c, d, e, f] = m.map(f64::from);
        let mut col0 = [a, b];
        let mut col1 = [c, d];
        let mut sx = a.hypot(b);
        let det = a * d - b * c;
        if sx < 1e-12 || det.abs() < 1e-12 {
            return None;
        }
        if det < 0.0 {
            // A mirror: flip the x axis so the turn stays continuous.
            sx = -sx;
            col0 = [-a, -b];
        }
        let n0 = [col0[0] / sx.abs(), col0[1] / sx.abs()];
        let mut shear = n0[0] * col1[0] + n0[1] * col1[1];
        col1 = [col1[0] - n0[0] * shear, col1[1] - n0[1] * shear];
        let sy = col1[0].hypot(col1[1]);
        if sy < 1e-12 {
            return None;
        }
        shear /= sy;
        Some(Self {
            translate: [e, f],
            angle: n0[1].atan2(n0[0]),
            shear,
            scale: [sx, sy],
        })
    }

    /// Blend toward `other`, turning the short way round.
    fn blend(&self, other: &Self, t: f64) -> Self {
        let mut target = other.angle;
        let tau = std::f64::consts::TAU;
        if target - self.angle > std::f64::consts::PI {
            target -= tau;
        } else if self.angle - target > std::f64::consts::PI {
            target += tau;
        }
        Self {
            translate: [
                mix(self.translate[0], other.translate[0], t),
                mix(self.translate[1], other.translate[1], t),
            ],
            angle: mix(self.angle, target, t),
            shear: mix(self.shear, other.shear, t),
            scale: [
                mix(self.scale[0], other.scale[0], t),
                mix(self.scale[1], other.scale[1], t),
            ],
        }
    }

    /// Put the parts back together.
    fn to_matrix(self) -> [f32; 6] {
        let (s, c) = self.angle.sin_cos();
        let [sx, sy] = self.scale;
        // rotate * [1 shear; 0 1] * scale, by columns.
        let col0 = [c * sx, s * sx];
        let col1 = [(c * self.shear - s) * sy, (s * self.shear + c) * sy];
        [
            col0[0] as f32,
            col0[1] as f32,
            col1[0] as f32,
            col1[1] as f32,
            self.translate[0] as f32,
            self.translate[1] as f32,
        ]
    }
}

/// Most entries kept in each `animation-*` list. Longer lists are ignored.
pub const MAX_ANIMATIONS: usize = 16;

/// One `@keyframes` block. The name is as written, without quotes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyframesRule {
    pub name: String,
    pub frames: Vec<Keyframe>,
}

/// One keyframe. `offset_millionths` is the position from `0` (`from`) to
/// `1_000_000` (`to`). A selector list like `from, 50%` gives one keyframe
/// per offset, each with the same declarations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keyframe {
    pub offset_millionths: u32,
    pub declarations: Vec<Declaration>,
}

/// Parse the body of `@keyframes <name> { ... }`. `None` when the name is
/// missing or not one word. Keyframes with a bad selector are dropped.
pub(crate) fn parse_keyframes(prelude: &str, body: &str) -> Option<KeyframesRule> {
    let name = prelude
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim();
    if name.is_empty() || name.contains(char::is_whitespace) || name.eq_ignore_ascii_case("none") {
        return None;
    }
    let mut frames = Vec::new();
    let mut rest = body;
    while let Some(open) = find_unquoted(rest, '{') {
        let selector = &rest[..open];
        let after = &rest[open + 1..];
        let close = find_matching_brace(after);
        // Important declarations inside a keyframe are ignored.
        let declarations: Vec<_> = parse_declarations(&after[..close])
            .into_iter()
            .filter(|declaration| !declaration.important)
            .collect();
        rest = after.get(close + 1..).unwrap_or("");
        let offsets: Option<Vec<u32>> = selector.split(',').map(parse_keyframe_offset).collect();
        for offset in offsets.unwrap_or_default() {
            frames.push(Keyframe {
                offset_millionths: offset,
                declarations: declarations.clone(),
            });
        }
    }
    Some(KeyframesRule {
        name: name.to_string(),
        frames,
    })
}

/// `from`, `to`, or a percentage in `0..=100`, in millionths.
fn parse_keyframe_offset(selector: &str) -> Option<u32> {
    let selector = selector.trim().to_ascii_lowercase();
    match selector.as_str() {
        "from" => Some(0),
        "to" => Some(1_000_000),
        _ => {
            let pct = selector.strip_suffix('%')?.trim().parse::<f64>().ok()?;
            (0.0..=100.0)
                .contains(&pct)
                .then(|| (pct * 10_000.0).round() as u32)
        }
    }
}

/// A stable 64-bit hash of an animation name, so [`Animations`] can stay
/// `Copy`. Never 0, which stands for `none`.
pub fn animation_name_hash(name: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash.max(1)
}

/// `animation-direction`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnimationDirection {
    Normal,
    Reverse,
    Alternate,
    AlternateReverse,
}

/// `animation-fill-mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillMode {
    None,
    Forwards,
    Backwards,
    Both,
}

impl FillMode {
    fn backwards(self) -> bool {
        matches!(self, Self::Backwards | Self::Both)
    }

    fn forwards(self) -> bool {
        matches!(self, Self::Forwards | Self::Both)
    }
}

/// The `animation-*` longhands as parsed lists. Lists shorter than
/// `animation-name` repeat, as in CSS.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Animations {
    names: [u64; MAX_ANIMATIONS],
    name_count: u8,
    durations_ms: [f32; MAX_ANIMATIONS],
    duration_count: u8,
    delays_ms: [f32; MAX_ANIMATIONS],
    delay_count: u8,
    iterations: [f32; MAX_ANIMATIONS],
    iteration_count: u8,
    directions: [AnimationDirection; MAX_ANIMATIONS],
    direction_count: u8,
    fills: [FillMode; MAX_ANIMATIONS],
    fill_count: u8,
    timings: [TimingFunction; MAX_ANIMATIONS],
    timing_count: u8,
}

/// One entry of [`Animations`], with its repeated longhands filled in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimationSpec {
    /// [`animation_name_hash`] of the name.
    pub name_hash: u64,
    pub duration_ms: f32,
    pub delay_ms: f32,
    /// `infinite` is `f32::INFINITY`.
    pub iterations: f32,
    pub direction: AnimationDirection,
    pub fill: FillMode,
    pub timing: TimingFunction,
}

impl Animations {
    /// `animation: none`.
    pub const INITIAL: Self = Self {
        names: [0; MAX_ANIMATIONS],
        name_count: 0,
        durations_ms: [0.0; MAX_ANIMATIONS],
        duration_count: 1,
        delays_ms: [0.0; MAX_ANIMATIONS],
        delay_count: 1,
        iterations: [1.0; MAX_ANIMATIONS],
        iteration_count: 1,
        directions: [AnimationDirection::Normal; MAX_ANIMATIONS],
        direction_count: 1,
        fills: [FillMode::None; MAX_ANIMATIONS],
        fill_count: 1,
        timings: [TimingFunction::EASE; MAX_ANIMATIONS],
        timing_count: 1,
    };

    /// Every named entry in list order. `none` entries are skipped.
    pub fn specs(&self) -> Vec<AnimationSpec> {
        let count = usize::from(self.name_count).min(MAX_ANIMATIONS);
        (0..count)
            .filter(|&i| self.names[i] != 0)
            .map(|i| {
                let pick = |n: u8| i % usize::from(n.max(1));
                AnimationSpec {
                    name_hash: self.names[i],
                    duration_ms: self.durations_ms[pick(self.duration_count)],
                    delay_ms: self.delays_ms[pick(self.delay_count)],
                    iterations: self.iterations[pick(self.iteration_count)],
                    direction: self.directions[pick(self.direction_count)],
                    fill: self.fills[pick(self.fill_count)],
                    timing: self.timings[pick(self.timing_count)],
                }
            })
            .collect()
    }
}

/// `infinite` or a non-negative finite number.
fn parse_iterations(value: &str) -> Option<f32> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("infinite") {
        return Some(f32::INFINITY);
    }
    value
        .parse::<f32>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
}

fn parse_direction(value: &str) -> Option<AnimationDirection> {
    Some(match value.trim().to_ascii_lowercase().as_str() {
        "normal" => AnimationDirection::Normal,
        "reverse" => AnimationDirection::Reverse,
        "alternate" => AnimationDirection::Alternate,
        "alternate-reverse" => AnimationDirection::AlternateReverse,
        _ => return None,
    })
}

fn parse_fill(value: &str) -> Option<FillMode> {
    Some(match value.trim().to_ascii_lowercase().as_str() {
        "none" => FillMode::None,
        "forwards" => FillMode::Forwards,
        "backwards" => FillMode::Backwards,
        "both" => FillMode::Both,
        _ => return None,
    })
}

/// A name: one word, optionally quoted, or `none` (hash 0).
fn parse_animation_name(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("none") {
        return Some(0);
    }
    let name = value.trim_matches(|c| c == '"' || c == '\'');
    let ok = !name.is_empty() && !name.contains(|c: char| c.is_whitespace() || c == '(');
    ok.then(|| animation_name_hash(name))
}

/// Apply one `animation` or `animation-*` declaration. Returns false when
/// `name` is not one of them; a bad value leaves the lists alone.
/// `animation-play-state` is accepted and ignored.
pub(crate) fn apply_animation_declaration(a: &mut Animations, name: &str, value: &str) -> bool {
    match name {
        "animation-name" => {
            if let Some(n) = parse_list(value, &mut a.names, parse_animation_name) {
                a.name_count = n;
            }
        }
        "animation-duration" => {
            if let Some(n) = parse_list(value, &mut a.durations_ms, |v| {
                parse_time_ms(v).filter(|ms| *ms >= 0.0)
            }) {
                a.duration_count = n;
            }
        }
        "animation-delay" => {
            if let Some(n) = parse_list(value, &mut a.delays_ms, parse_time_ms) {
                a.delay_count = n;
            }
        }
        "animation-iteration-count" => {
            if let Some(n) = parse_list(value, &mut a.iterations, parse_iterations) {
                a.iteration_count = n;
            }
        }
        "animation-direction" => {
            if let Some(n) = parse_list(value, &mut a.directions, parse_direction) {
                a.direction_count = n;
            }
        }
        "animation-fill-mode" => {
            if let Some(n) = parse_list(value, &mut a.fills, parse_fill) {
                a.fill_count = n;
            }
        }
        "animation-timing-function" => {
            if let Some(n) = parse_list(value, &mut a.timings, TimingFunction::parse) {
                a.timing_count = n;
            }
        }
        "animation-play-state" => {}
        "animation" => {
            if let Some(parsed) = parse_animation_shorthand(value) {
                *a = parsed;
            }
        }
        _ => return false,
    }
    true
}

/// `animation: <name> <duration> <timing> <delay> <count> <direction>
/// <fill>, ...` in any order within an entry. The first time is the
/// duration, the second the delay. A word that is not a keyword is the name.
fn parse_animation_shorthand(value: &str) -> Option<Animations> {
    let entries = split_top_level(value, ',');
    if entries.is_empty() || entries.len() > MAX_ANIMATIONS {
        return None;
    }
    let mut a = Animations::INITIAL;
    let count = entries.len() as u8;
    a.name_count = count;
    a.duration_count = count;
    a.delay_count = count;
    a.iteration_count = count;
    a.direction_count = count;
    a.fill_count = count;
    a.timing_count = count;
    for (i, entry) in entries.iter().enumerate() {
        let mut name = None;
        let mut duration = None;
        let mut delay = None;
        let mut iterations = None;
        let mut direction = None;
        let mut fill = None;
        let mut timing = None;
        for token in split_top_level(entry, ' ') {
            if let Some(ms) = parse_time_ms(token) {
                if duration.is_none() {
                    if ms < 0.0 {
                        return None;
                    }
                    duration = Some(ms);
                } else if delay.is_none() {
                    delay = Some(ms);
                } else {
                    return None;
                }
            } else if let Some(f) = TimingFunction::parse(token).filter(|_| timing.is_none()) {
                timing = Some(f);
            } else if let Some(n) = parse_iterations(token).filter(|_| iterations.is_none()) {
                iterations = Some(n);
            } else if let Some(d) = parse_direction(token).filter(|_| direction.is_none()) {
                direction = Some(d);
            } else if let Some(f) = parse_fill(token).filter(|_| fill.is_none()) {
                fill = Some(f);
            } else if matches!(token.to_ascii_lowercase().as_str(), "running" | "paused") {
                continue;
            } else if name.is_none() {
                name = Some(parse_animation_name(token)?);
            } else {
                return None;
            }
        }
        a.names[i] = name.unwrap_or(0);
        a.durations_ms[i] = duration.unwrap_or(0.0);
        a.delays_ms[i] = delay.unwrap_or(0.0);
        a.iterations[i] = iterations.unwrap_or(1.0);
        a.directions[i] = direction.unwrap_or(AnimationDirection::Normal);
        a.fills[i] = fill.unwrap_or(FillMode::None);
        a.timings[i] = timing.unwrap_or(TimingFunction::EASE);
    }
    Some(a)
}

/// The animatable properties one declaration sets, shorthands expanded.
fn declared_properties(name: &str) -> Vec<AnimProperty> {
    use AnimProperty as P;
    match name {
        "margin" => vec![P::MarginTop, P::MarginRight, P::MarginBottom, P::MarginLeft],
        "padding" => vec![
            P::PaddingTop,
            P::PaddingRight,
            P::PaddingBottom,
            P::PaddingLeft,
        ],
        "border" | "border-top" | "border-right" | "border-bottom" | "border-left" => {
            vec![P::BorderColor]
        }
        "background-image" => vec![P::BackgroundColor],
        "gap" | "grid-gap" => vec![P::RowGap, P::ColumnGap],
        _ => match AnimProperty::from_name(name) {
            P::All | P::Other => Vec::new(),
            property => vec![property],
        },
    }
}

/// Phase and progress of an animation `local_ms` after its start, before
/// keyframe easing. `None` when it has no effect at that time.
fn animation_progress(spec: &AnimationSpec, local_ms: f64) -> Option<f64> {
    let duration = f64::from(spec.duration_ms.max(0.0));
    let iterations = f64::from(spec.iterations.max(0.0));
    let active = if duration == 0.0 || iterations == 0.0 {
        0.0
    } else {
        duration * iterations
    };
    let (iteration, progress) = if local_ms < 0.0 {
        if !spec.fill.backwards() {
            return None;
        }
        (0.0, 0.0)
    } else if local_ms < active {
        let overall = local_ms / duration;
        let iteration = overall.floor();
        (iteration, overall - iteration)
    } else {
        if !spec.fill.forwards() {
            return None;
        }
        if iterations.is_infinite() {
            (0.0, 1.0)
        } else if iterations > 0.0 && iterations.fract() == 0.0 {
            (iterations - 1.0, 1.0)
        } else {
            (iterations.floor(), iterations.fract())
        }
    };
    let odd = iteration % 2.0 == 1.0;
    let reversed = match spec.direction {
        AnimationDirection::Normal => false,
        AnimationDirection::Reverse => true,
        AnimationDirection::Alternate => odd,
        AnimationDirection::AlternateReverse => !odd,
    };
    Some(if reversed { 1.0 - progress } else { progress })
}

/// True while the animation can still change: waiting on its delay or in
/// its active time.
fn animation_is_live(spec: &AnimationSpec, local_ms: f64) -> bool {
    let duration = f64::from(spec.duration_ms.max(0.0));
    let iterations = f64::from(spec.iterations.max(0.0));
    let active = if duration == 0.0 || iterations == 0.0 {
        0.0
    } else {
        duration * iterations
    };
    local_ms < active
}

/// Write one animation's values for directed progress `p` onto `out`.
///
/// Each property the keyframes name blends between the two keyframes
/// around `p`, eased by the earlier keyframe's `animation-timing-function`
/// or the animation's. A missing `0%` or `100%` uses `underlying`. Values
/// that cannot blend switch halfway through the eased interval.
fn apply_keyframes(
    rule: &KeyframesRule,
    spec: &AnimationSpec,
    p: f64,
    underlying: &ComputedStyle,
    input: &AnimInput,
    out: &mut ComputedStyle,
) {
    // Each keyframe's style, from the underlying one plus its declarations.
    let mut frames: Vec<(f64, ComputedStyle, Vec<AnimProperty>, TimingFunction)> = Vec::new();
    for frame in &rule.frames {
        let mut style = *underlying;
        let mut vars = input.vars.clone();
        let mut declared = Vec::new();
        let mut timing = spec.timing;
        for declaration in &frame.declarations {
            if declaration.name == "animation-timing-function" {
                if let Some(f) = TimingFunction::parse(&declaration.value) {
                    timing = f;
                }
                continue;
            }
            declared.extend(declared_properties(&declaration.name));
            apply_declaration(
                &mut style,
                &mut vars,
                declaration,
                input.parent_font_px,
                input.root_font_px,
            );
        }
        let offset = f64::from(frame.offset_millionths) / 1_000_000.0;
        frames.push((offset, style, declared, timing));
    }
    let mut properties: Vec<AnimProperty> = Vec::new();
    for (_, _, declared, _) in &frames {
        for property in declared {
            if !properties.contains(property) {
                properties.push(*property);
            }
        }
    }
    for property in properties {
        let Some(base) = read_value(underlying, property) else {
            continue;
        };
        // (offset, value, easing) for keyframes naming this property; at a
        // repeated offset the later keyframe wins.
        let mut stops: Vec<(f64, AnimValue, TimingFunction)> = Vec::new();
        for (offset, style, declared, timing) in &frames {
            if !declared.contains(&property) {
                continue;
            }
            let Some(value) = read_value(style, property) else {
                continue;
            };
            stops.retain(|(o, _, _)| o != offset);
            stops.push((*offset, value, *timing));
        }
        stops.sort_by(|a, b| a.0.total_cmp(&b.0));
        if stops.first().is_none_or(|s| s.0 > 0.0) {
            stops.insert(0, (0.0, base, spec.timing));
        }
        if stops.last().is_none_or(|s| s.0 < 1.0) {
            stops.push((1.0, base, spec.timing));
        }
        let after = stops
            .iter()
            .position(|s| s.0 > p)
            .unwrap_or(stops.len() - 1)
            .max(1);
        let (from, to) = (stops[after - 1], stops[after]);
        let span = to.0 - from.0;
        let local = if span > 0.0 {
            ((p - from.0) / span).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let eased = from.2.eval(local);
        let value =
            interpolate(from.1, to.1, eased).unwrap_or(if eased < 0.5 { from.1 } else { to.1 });
        write_value(out, property, value);
    }
}

/// What applying keyframes needs from the cascade.
pub(crate) struct AnimInput<'a> {
    pub keyframes: &'a [KeyframesRule],
    pub vars: &'a HashMap<String, String>,
    pub parent_font_px: f32,
    pub root_font_px: f32,
}

impl AnimInput<'_> {
    /// The last `@keyframes` with this name hash.
    fn rule(&self, name_hash: u64) -> Option<&KeyframesRule> {
        self.keyframes
            .iter()
            .rev()
            .find(|rule| animation_name_hash(&rule.name) == name_hash)
    }
}

/// A source of frame times in milliseconds.
pub trait FrameClock {
    fn now_ms(&self) -> f64;
}

/// A clock that only moves when told to. Clones share the same time, so a
/// test can keep one and hand another to the code under test.
#[derive(Clone, Debug, Default)]
pub struct ManualClock {
    now: Rc<Cell<f64>>,
}

impl ManualClock {
    /// A clock at 0 ms.
    pub fn new() -> Self {
        Self::default()
    }

    /// Jump to `ms`.
    pub fn set(&self, ms: f64) {
        self.now.set(ms);
    }

    /// Move forward by `ms`.
    pub fn advance(&self, ms: f64) {
        self.now.set(self.now.get() + ms);
    }
}

impl FrameClock for ManualClock {
    fn now_ms(&self) -> f64 {
        self.now.get()
    }
}

/// Wall time since the clock was made, from the system's monotonic timer.
#[derive(Clone, Copy, Debug)]
pub struct MonotonicClock {
    start: Instant,
}

impl MonotonicClock {
    /// A clock reading 0 now.
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameClock for MonotonicClock {
    fn now_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }
}

/// One property moving from one value to another.
#[derive(Clone, Copy, Debug)]
struct RunningTransition {
    property: AnimProperty,
    from: AnimValue,
    to: AnimValue,
    start_ms: f64,
    duration_ms: f64,
    timing: TimingFunction,
}

impl RunningTransition {
    /// The value at `now`. Before the start (during the delay) it is `from`.
    fn value_at(&self, now: f64) -> AnimValue {
        let p = ((now - self.start_ms) / self.duration_ms).clamp(0.0, 1.0);
        interpolate(self.from, self.to, self.timing.eval(p)).unwrap_or(self.to)
    }

    fn is_done(&self, now: f64) -> bool {
        now >= self.start_ms + self.duration_ms
    }
}

/// What the animator remembers about one element between passes.
#[derive(Clone, Debug)]
struct NodeState {
    /// The style the cascade produced last time, before any animation.
    base: ComputedStyle,
    transitions: Vec<RunningTransition>,
    /// Named animations in list order, with the time each name first
    /// appeared and whether a `@keyframes` rule matched it.
    animations: Vec<(AnimationSpec, f64, bool)>,
    /// Pass number when this element was last styled.
    seen: u64,
}

/// Runs transitions for [`crate::compute_styles_animated`].
///
/// Keeps per-element state keyed by node id. Elements missing from a pass
/// (removed, or no longer reachable) are forgotten at the end of it.
///
/// The cascade stays a pure function of the document and style sheet.
/// [`crate::compute_styles_animated`] runs the same cascade and lets an
/// [`Animator`] compare each element's new style with the one from the
/// previous pass. A changed property with a `transition` starts moving from
/// its current value to the new one, and every running transition writes
/// its value for the animator's current time into the result. Inherited
/// properties reach children with their animated value.
///
/// Time only moves when the caller says so: [`Animator::tick`] takes a
/// time in milliseconds from any [`FrameClock`], so tests drive it with a
/// [`ManualClock`] and a host uses [`MonotonicClock`]. A frame loop looks
/// like this:
///
/// ```
/// use anvil_dom::parse_html;
/// use anvil_style::{Animator, compute_styles_animated, parse_css};
///
/// let mut doc = parse_html("<div id=\"a\" style=\"opacity: 1\"></div>");
/// let sheet = parse_css("div { transition: opacity 100ms linear; }");
/// let mut animator = Animator::new();
/// animator.tick(0.0);
/// compute_styles_animated(&doc, &sheet, 800.0, 600.0, &mut animator);
///
/// // Something changes the style; the next pass starts a transition.
/// let id = doc.get_element_by_id("a").unwrap();
/// doc.set_attribute(id, "style", "opacity: 0");
/// animator.tick(0.0);
/// compute_styles_animated(&doc, &sheet, 800.0, 600.0, &mut animator);
///
/// // Each frame: tick, restyle, lay out, paint. Stop when tick says so.
/// assert!(animator.tick(25.0));
/// let styles = compute_styles_animated(&doc, &sheet, 800.0, 600.0, &mut animator);
/// assert_eq!(styles.get(id).opacity, 0.75);
/// assert!(!animator.tick(100.0));
/// ```
///
/// Moving properties: `opacity`, `transform`, `color`, `background-color`
/// (not gradients), `border-color`, `width`, `height`, `max-width`, the
/// offsets, the margin and padding sides, `border-radius`, `font-size`,
/// and `gap`. Lengths blend within one unit, or across `px`, `%`, `em`, and
/// `rem` as a `calc()` mix; `auto` and other unit pairs jump. Colors blend
/// with premultiplied alpha. Not supported: transition events, the
/// shortened reverse of an interrupted transition (a reverse takes the full
/// duration from the current value), and starting on an element's first
/// styled pass or while it is `display: none`.
///
/// Animations play the `@keyframes` block named by `animation-name` (the
/// last one with that name), from the time the name first appears on the
/// element; changing other `animation-*` values keeps that start time.
/// Keyframes blend the same properties as transitions, with values that
/// cannot blend switching halfway; a keyframe's own
/// `animation-timing-function` eases the interval after it. A missing
/// `0%` or `100%` keyframe uses the element's value without the animation.
/// Animations apply under transitions, and animated values do not start
/// transitions. Not supported: animation events, `animation-play-state`,
/// `animation-composition`, and `!important` inside keyframes.
#[derive(Clone, Debug, Default)]
pub struct Animator {
    nodes: HashMap<NodeId, NodeState>,
    now_ms: f64,
    pass: u64,
}

impl Animator {
    /// An animator at time 0 with nothing running.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the current time to `now_ms` and drop finished transitions.
    /// Returns true when something is still running or waiting on a delay,
    /// so the caller should draw another frame (see [`Self::is_animating`]). Time never runs backward;
    /// an earlier time is treated as the current one.
    pub fn tick(&mut self, now_ms: f64) -> bool {
        if now_ms.is_finite() && now_ms > self.now_ms {
            self.now_ms = now_ms;
        }
        let now = self.now_ms;
        for state in self.nodes.values_mut() {
            state.transitions.retain(|t| !t.is_done(now));
        }
        self.is_animating()
    }

    /// [`Self::tick`] with the time read from `clock`.
    pub fn tick_clock(&mut self, clock: &dyn FrameClock) -> bool {
        self.tick(clock.now_ms())
    }

    /// The time set by the last [`Self::tick`].
    pub fn now_ms(&self) -> f64 {
        self.now_ms
    }

    /// True when any transition has not finished at the current time, or
    /// any animation is waiting on its delay or still in its active time.
    /// A pass that starts one makes this true at once.
    pub fn is_animating(&self) -> bool {
        let now = self.now_ms;
        self.nodes.values().any(|state| {
            state.transitions.iter().any(|t| !t.is_done(now))
                || state.animations.iter().any(|(spec, start, has_rule)| {
                    *has_rule && animation_is_live(spec, now - start - f64::from(spec.delay_ms))
                })
        })
    }

    /// Start a restyle pass.
    pub(crate) fn begin_pass(&mut self) {
        self.pass += 1;
    }

    /// Forget elements the pass did not reach.
    pub(crate) fn end_pass(&mut self) {
        let pass = self.pass;
        self.nodes.retain(|_, state| state.seen == pass);
    }

    /// Seed `id` with `existing_base` if `id` has not been tracked by this animator yet.
    pub(crate) fn seed_if_absent(&mut self, id: NodeId, existing_base: &ComputedStyle) {
        let pass = self.pass;
        self.nodes.entry(id).or_insert_with(|| NodeState {
            base: *existing_base,
            transitions: Vec::new(),
            animations: Vec::new(),
            seen: pass,
        });
    }

    /// Nodes that currently have a live transition or keyframe animation at `now_ms`.
    pub(crate) fn active_nodes(&self) -> Vec<NodeId> {
        let now = self.now_ms;
        let mut out: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|(_, state)| {
                state.transitions.iter().any(|t| !t.is_done(now))
                    || state.animations.iter().any(|(spec, start, has_rule)| {
                        *has_rule && animation_is_live(spec, now - start - f64::from(spec.delay_ms))
                    })
            })
            .map(|(&id, _)| id)
            .collect();
        out.sort_by_key(|id| id.index());
        out
    }

    /// Note `base`, the element's freshly cascaded style, start transitions
    /// for properties that changed since the last pass, and write running
    /// values onto `out`, the style the page will use.
    pub(crate) fn apply(
        &mut self,
        id: NodeId,
        base: &ComputedStyle,
        out: &mut ComputedStyle,
        input: &AnimInput,
    ) {
        let now = self.now_ms;
        let pass = self.pass;
        let state = self.nodes.entry(id).or_insert_with(|| NodeState {
            base: *base,
            transitions: Vec::new(),
            animations: Vec::new(),
            seen: pass,
        });
        state.seen = pass;
        // Nothing starts on an element that is or was `display: none`.
        let hidden = base.display == Display::None || state.base.display == Display::None;
        if hidden {
            state.transitions.clear();
        }
        if state.base != *base && !hidden {
            let old = state.base;
            for property in ANIMATABLE {
                let (Some(before), Some(after)) =
                    (read_value(&old, property), read_value(base, property))
                else {
                    continue;
                };
                if before == after {
                    continue;
                }
                let running = state
                    .transitions
                    .iter()
                    .position(|t| t.property == property);
                let current = running
                    .map(|i| state.transitions[i].value_at(now))
                    .unwrap_or(before);
                if let Some(i) = running {
                    state.transitions.remove(i);
                }
                let Some(spec) = base.transitions.spec_for(property) else {
                    continue;
                };
                if interpolate(current, after, 0.5).is_none() {
                    continue;
                }
                state.transitions.push(RunningTransition {
                    property,
                    from: current,
                    to: after,
                    start_ms: now + f64::from(spec.delay_ms),
                    duration_ms: f64::from(spec.duration_ms),
                    timing: spec.timing,
                });
            }
        }
        state.base = *base;
        // Animations keep their start time while their name stays listed;
        // a new name starts now. Hidden elements run none.
        let specs = if hidden {
            Vec::new()
        } else {
            base.animations.specs()
        };
        let animations: Vec<(AnimationSpec, f64, bool)> = specs
            .iter()
            .map(|spec| {
                let start = state
                    .animations
                    .iter()
                    .find(|(old, _, _)| old.name_hash == spec.name_hash)
                    .map_or(now, |(_, start, _)| *start);
                (*spec, start, input.rule(spec.name_hash).is_some())
            })
            .collect();
        state.animations = animations;
        let underlying = *out;
        for (spec, start, _) in &state.animations {
            let Some(rule) = input.rule(spec.name_hash) else {
                continue;
            };
            let local = now - start - f64::from(spec.delay_ms);
            if let Some(p) = animation_progress(spec, local) {
                apply_keyframes(rule, spec, p, &underlying, input, out);
            }
        }
        for t in &state.transitions {
            if !t.is_done(now) {
                write_value(out, t.property, t.value_at(now));
            }
        }
    }
}
