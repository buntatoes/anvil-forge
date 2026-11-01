// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato
//! Selector parsing and matching.
//!
//! One parser answers both questions the cascade asks about a selector list:
//! is it valid, and which elements does it match. A list with one invalid
//! selector is dropped whole by the caller.
//!
//! Matching runs right to left. A descendant or child walk stops as soon as
//! no ancestor further up can match the rest of the selector, and in the
//! cascade a filter of the names above each element rules most walks out
//! before they start. The subsequent-sibling combinator remembers what it
//! found per element, and `:has()` and `nth-child(... of S)` cache their
//! answers the same way. Nothing here
//! recurses on the depth of the document: walks up, sideways, and into a
//! subtree use loops and explicit stacks. Recursion follows the selector only,
//! and that is bounded by [`MAX_SELECTOR_NESTING`] and
//! [`MAX_SELECTOR_COMPOUNDS`].
//!
//! The page is static. Pseudo-classes about user interaction, time, media
//! playback, URL fragments, and validation (`:hover`, `:focus`, `:visited`,
//! `:target`, `:valid`, `:playing`, and the rest listed in [`State::Never`])
//! are valid and never match.

use anvil_dom::{Document, NodeData, NodeId};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// How deep `:is()`, `:where()`, `:not()`, `:has()`, and `nth-*(... of S)`
/// may nest inside each other. A deeper selector makes its whole list invalid.
pub const MAX_SELECTOR_NESTING: usize = 64;

/// Most compounds in one complex selector. A longer selector makes its whole
/// list invalid.
pub const MAX_SELECTOR_COMPOUNDS: usize = 640;

/// Pseudo-classes written without arguments.
const PSEUDO_CLASSES: &[&str] = &[
    "active",
    "any-link",
    "autofill",
    "blank",
    "buffering",
    "checked",
    "closed",
    "current",
    "default",
    "defined",
    "disabled",
    "empty",
    "enabled",
    "first",
    "first-child",
    "first-of-type",
    "focus",
    "focus-visible",
    "focus-within",
    "fullscreen",
    "future",
    "host",
    "hover",
    "in-range",
    "indeterminate",
    "invalid",
    "last-child",
    "last-of-type",
    "left",
    "link",
    "local-link",
    "modal",
    "muted",
    "only-child",
    "only-of-type",
    "open",
    "optional",
    "out-of-range",
    "past",
    "paused",
    "picture-in-picture",
    "placeholder-shown",
    "playing",
    "popover-open",
    "read-only",
    "read-write",
    "required",
    "right",
    "root",
    "scope",
    "seeking",
    "stalled",
    "target",
    "target-within",
    "user-invalid",
    "user-valid",
    "valid",
    "visited",
    "volume-locked",
];

/// Pseudo-classes that take an argument in parentheses.
const PSEUDO_CLASS_FUNCTIONS: &[&str] = &[
    "current",
    "dir",
    "has",
    "host",
    "host-context",
    "is",
    "lang",
    "not",
    "nth-child",
    "nth-last-child",
    "nth-last-of-type",
    "nth-of-type",
    "state",
    "where",
];

/// Pseudo-elements written after `::` without arguments.
const PSEUDO_ELEMENTS: &[&str] = &[
    "after",
    "backdrop",
    "before",
    "cue",
    "cue-region",
    "details-content",
    "file-selector-button",
    "first-letter",
    "first-line",
    "grammar-error",
    "marker",
    "placeholder",
    "selection",
    "spelling-error",
    "target-text",
];

/// Pseudo-elements written after `::` with an argument.
const PSEUDO_ELEMENT_FUNCTIONS: &[&str] = &["cue", "cue-region", "highlight", "part", "slotted"];

/// Pseudo-elements that may also be written with a single colon.
const LEGACY_PSEUDO_ELEMENTS: &[&str] = &["after", "before", "first-letter", "first-line"];

/// Attributes whose values compare without ASCII case in an HTML document,
/// unless the selector says `s`.
const CASE_INSENSITIVE_VALUES: &[&str] = &[
    "accept",
    "accept-charset",
    "align",
    "alink",
    "axis",
    "bgcolor",
    "charset",
    "checked",
    "clear",
    "codetype",
    "color",
    "compact",
    "declare",
    "defer",
    "dir",
    "direction",
    "disabled",
    "enctype",
    "face",
    "frame",
    "hreflang",
    "http-equiv",
    "lang",
    "language",
    "link",
    "media",
    "method",
    "multiple",
    "nohref",
    "noresize",
    "noshade",
    "nowrap",
    "readonly",
    "rel",
    "rev",
    "rules",
    "scope",
    "scrolling",
    "selected",
    "shape",
    "target",
    "text",
    "type",
    "valign",
    "valuetype",
    "vlink",
];

/// How two compounds relate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Combinator {
    /// Whitespace.
    Descendant,
    /// `>`
    Child,
    /// `+`
    Next,
    /// `~`
    Subsequent,
}

/// One parsed complex selector.
///
/// Built only by the style sheet parser. The compounds read left to right;
/// the last one is the subject. Inside `:has()` the selector is relative and
/// starts with a combinator from the `:has()` element.
///
/// What matches:
///
/// - type, `*`, `#id`, and `.class` (names keep their case; element names
///   do not), with CSS escapes;
/// - the descendant, `>`, `+`, and `~` combinators;
/// - `[attr]` and `=`, `~=`, `|=`, `^=`, `$=`, `*=` with the `i` and `s`
///   flags. Without a flag, values of the attributes HTML lists as
///   case-insensitive (`type`, `lang`, `dir`, and so on) ignore ASCII case;
/// - `:is()` and `:where()` (forgiving), `:not()`, and `:has()` with any
///   relative selector except a nested `:has()`;
/// - `:root`, `:scope` (the root), `:empty`, `:first-child`, `:last-child`,
///   `:only-child`, `:first-of-type`, `:last-of-type`, `:only-of-type`,
///   `:nth-child()` and `:nth-last-child()` with `of S`, `:nth-of-type()`,
///   and `:nth-last-of-type()`;
/// - `:link` and `:any-link`, `:checked`, `:default`, `:disabled`,
///   `:enabled`, `:required`, `:optional`, `:read-only`, `:read-write`,
///   `:placeholder-shown`, `:indeterminate` (`progress` only), `:open`,
///   `:closed`, `:defined`, `:lang()`, and `:dir()` (from `dir` attributes;
///   `auto` reads as left to right).
///
/// What parses but never matches: pseudo-elements, since they style no
/// element of the tree; namespace prefixes other than `*|`, because
/// `@namespace` is not read; and the states a static page never has:
/// `:active`, `:autofill`, `:blank`, `:buffering`, `:current`, `:first`,
/// `:focus`, `:focus-visible`, `:focus-within`, `:fullscreen`, `:future`,
/// `:host`, `:hover`, `:in-range`, `:invalid`, `:left`, `:local-link`,
/// `:modal`, `:muted`, `:out-of-range`, `:past`, `:paused`,
/// `:picture-in-picture`, `:playing`, `:popover-open`, `:right`,
/// `:seeking`, `:stalled`, `:target`, `:target-within`, `:user-invalid`,
/// `:user-valid`, `:valid`, `:visited`, `:volume-locked`, `:current()`,
/// `:host()`, `:host-context()`, and `:state()`. `:indeterminate` on a
/// radio button never matches either. `&` (nesting) is not valid here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selector {
    leading: Option<Combinator>,
    compounds: Vec<Compound>,
    /// `combinators[i]` sits between `compounds[i]` and `compounds[i + 1]`.
    combinators: Vec<Combinator>,
    specificity: (u16, u16, u16),
    /// The subject compound's bits, kept here so most elements are turned
    /// away without leaving the rule list.
    subject_bits: u64,
}

/// Simple selectors that must all match one element. An empty list is `*`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Compound {
    parts: Vec<Simple>,
    /// [`name_bit`]s of its type, id, and classes.
    bits: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Simple {
    /// Lowercase element name.
    Type(String),
    /// A type or universal selector with a namespace this crate cannot
    /// resolve. `types` is 1 for a name and 0 for `*`. Never matches.
    Unresolved {
        types: u16,
    },
    Id(String),
    Class(String),
    Attr(Box<AttrSelector>),
    State(State),
    Nth(Box<Nth>),
    Is(Vec<Selector>),
    Where(Vec<Selector>),
    Not(Vec<Selector>),
    Has(Vec<Selector>),
    Host(Vec<Selector>),
    HostContext(Vec<Selector>),
    /// `::slotted(<compound-selector-list>)` matching slotted light-DOM children.
    Slotted(Vec<Selector>),
    /// `::part(<ident>+)` matching shadow tree elements with `part="..."`.
    Part(Vec<String>),
    /// Language ranges from `:lang()`, as written.
    Lang(Vec<String>),
    /// `:dir()`. `None` for a direction other than `ltr` or `rtl`.
    Dir(Option<bool>),
    /// Any pseudo-element (`Some(false)` for `::before`, `Some(true)` for `::after`,
    /// `None` for any other pseudo-element). Selects no element of the tree itself.
    PseudoElement(Option<bool>),
}

impl Simple {
    /// Cheap tests first; the order of simple selectors does not change
    /// what a compound matches.
    fn cost(&self) -> u8 {
        match self {
            Simple::Type(_)
            | Simple::Unresolved { .. }
            | Simple::Id(_)
            | Simple::Class(_)
            | Simple::PseudoElement(_) => 0,
            Simple::Attr(_)
            | Simple::State(_)
            | Simple::Nth(_)
            | Simple::Dir(_)
            | Simple::Part(_) => 1,
            Simple::Lang(_)
            | Simple::Is(_)
            | Simple::Where(_)
            | Simple::Not(_)
            | Simple::Host(_)
            | Simple::HostContext(_)
            | Simple::Slotted(_) => 2,
            Simple::Has(_) => 3,
        }
    }
}

/// Element states a pseudo-class names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Root,
    Scope,
    Empty,
    FirstChild,
    LastChild,
    OnlyChild,
    FirstOfType,
    LastOfType,
    OnlyOfType,
    /// `:link` and `:any-link`: `a` and `area` with `href`. No link counts as
    /// visited.
    Link,
    Checked,
    Default,
    Disabled,
    Enabled,
    Required,
    Optional,
    ReadOnly,
    ReadWrite,
    PlaceholderShown,
    Defined,
    Host,
    Open,
    Closed,
    /// A `progress` without `value`. Radio groups are not tracked.
    Indeterminate,
    Hover,
    Active,
    Focus,
    FocusVisible,
    FocusWithin,
    Target,
    TargetWithin,
    Valid,
    Invalid,
    InRange,
    OutOfRange,
    /// Valid, and never true for a static page.
    Never,
}

fn is_known_vendor_pseudo(lower: &str) -> bool {
    let Some(rest) = lower.strip_prefix('-') else {
        return false;
    };
    let Some((prefix, suffix)) = rest.split_once('-') else {
        return false;
    };
    if prefix.len() < 2 {
        return false;
    }
    matches!(
        suffix,
        "input-placeholder"
            | "placeholder"
            | "search-cancel-button"
            | "search-decoration"
            | "search-results-button"
            | "search-results-decoration"
            | "scrollbar"
            | "scrollbar-thumb"
            | "scrollbar-track"
            | "scrollbar-button"
            | "scrollbar-corner"
            | "autofill"
            | "focus-inner"
            | "focusring"
            | "clear"
            | "reveal"
            | "file-upload-button"
            | "slider-thumb"
            | "slider-runnable-track"
            | "progress-bar"
            | "progress-value"
            | "progress-inner-element"
            | "meter-bar"
            | "details-marker"
            | "calendar-picker-indicator"
            | "inner-spin-button"
            | "outer-spin-button"
            | "color-swatch"
            | "color-swatch-wrapper"
            | "any"
            | "any-link"
            | "full-screen"
            | "read-only"
            | "read-write"
            | "placeholder-shown"
            | "user-invalid"
    )
}

fn state_for(name: &str) -> Option<State> {
    let state = match name {
        "root" => State::Root,
        "scope" => State::Scope,
        "empty" => State::Empty,
        "first-child" => State::FirstChild,
        "last-child" => State::LastChild,
        "only-child" => State::OnlyChild,
        "first-of-type" => State::FirstOfType,
        "last-of-type" => State::LastOfType,
        "only-of-type" => State::OnlyOfType,
        "link" | "any-link" => State::Link,
        "checked" => State::Checked,
        "default" => State::Default,
        "disabled" => State::Disabled,
        "enabled" => State::Enabled,
        "required" => State::Required,
        "optional" => State::Optional,
        "read-only" => State::ReadOnly,
        "read-write" => State::ReadWrite,
        "placeholder-shown" => State::PlaceholderShown,
        "defined" => State::Defined,
        "host" => State::Host,
        "open" => State::Open,
        "closed" => State::Closed,
        "indeterminate" => State::Indeterminate,
        "hover" => State::Hover,
        "active" => State::Active,
        "focus" => State::Focus,
        "focus-visible" => State::FocusVisible,
        "focus-within" => State::FocusWithin,
        "target" => State::Target,
        "target-within" => State::TargetWithin,
        "valid" | "user-valid" => State::Valid,
        "invalid" | "user-invalid" => State::Invalid,
        "in-range" => State::InRange,
        "out-of-range" => State::OutOfRange,
        _ if PSEUDO_CLASSES.contains(&name) || is_known_vendor_pseudo(name) => State::Never,
        _ => return None,
    };
    Some(state)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttrOp {
    Exists,
    Equals,
    Includes,
    DashMatch,
    Prefix,
    Suffix,
    Substring,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaseFlag {
    Default,
    Insensitive,
    Sensitive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AttrSelector {
    /// Lowercase name.
    name: String,
    /// False for a namespace prefix this crate cannot resolve.
    resolvable: bool,
    op: AttrOp,
    value: String,
    case: CaseFlag,
}

/// `:nth-child()` and its relatives: elements at `a*n + b`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Nth {
    a: i64,
    b: i64,
    from_end: bool,
    of_type: bool,
    of: Option<Vec<Selector>>,
}

/// Why a selector did not parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fail {
    /// Bad syntax. A forgiving list skips the entry.
    Invalid,
    /// Past a limit. Always fails the whole list.
    TooBig,
}

type Parsed<T> = Result<T, Fail>;

/// What the surrounding selector allows.
#[derive(Clone, Copy)]
struct Ctx {
    depth: usize,
    in_has: bool,
    nested: bool,
}

impl Ctx {
    fn inner(self) -> Parsed<Ctx> {
        if self.depth + 1 > MAX_SELECTOR_NESTING {
            return Err(Fail::TooBig);
        }
        Ok(Ctx {
            depth: self.depth + 1,
            in_has: self.in_has,
            nested: true,
        })
    }
}

/// Parse a selector list. `None` when any entry is invalid or past a limit.
pub(crate) fn parse_selector_list(source: &str) -> Option<Vec<Selector>> {
    let chars: Vec<char> = source.chars().collect();
    let cx = Ctx {
        depth: 0,
        in_has: false,
        nested: false,
    };
    parse_list(&chars, cx, false).ok()
}

/// Every entry must parse.
fn parse_list(src: &[char], cx: Ctx, relative: bool) -> Parsed<Vec<Selector>> {
    split_list(src)
        .into_iter()
        .map(|part| parse_complex(part, cx, relative))
        .collect()
}

/// Entries that do not parse are left out. Limits still fail the list.
fn parse_forgiving(src: &[char], cx: Ctx) -> Parsed<Vec<Selector>> {
    let mut out = Vec::new();
    for part in split_list(src) {
        match parse_complex(part, cx, false) {
            Ok(sel) => out.push(sel),
            Err(Fail::Invalid) => {}
            Err(Fail::TooBig) => return Err(Fail::TooBig),
        }
    }
    Ok(out)
}

/// Split on commas outside parentheses, brackets, strings, and escapes.
fn split_list(src: &[char]) -> Vec<&[char]> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quote = None;
    let mut start = 0;
    let mut i = 0;
    while i < src.len() {
        let ch = src[i];
        if let Some(q) = quote {
            if ch == '\\' {
                i += 1;
            } else if ch == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match ch {
            '\\' => i += 1,
            '"' | '\'' => quote = Some(ch),
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&src[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&src[start.min(src.len())..]);
    parts
}

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}')
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

fn is_name_char(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == '-'
}

/// Character cursor over one piece of selector text.
struct Cursor<'s> {
    chars: &'s [char],
    pos: usize,
}

impl<'s> Cursor<'s> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn at_end(&self) -> bool {
        self.pos >= self.chars.len()
    }

    fn skip_ws(&mut self) -> bool {
        let start = self.pos;
        while self.peek().is_some_and(is_ws) {
            self.pos += 1;
        }
        self.pos > start
    }

    /// True at `\` that starts an escape, at `offset`. A backslash at the
    /// very end is an escape too, for U+FFFD.
    fn at_escape(&self, offset: usize) -> bool {
        self.peek_at(offset) == Some('\\') && self.peek_at(offset + 1) != Some('\n')
    }

    /// Read one escape; the cursor is on the backslash.
    fn read_escape(&mut self) -> char {
        self.pos += 1;
        let mut value = 0u32;
        let mut digits = 0;
        while digits < 6 {
            match self.peek().and_then(|c| c.to_digit(16)) {
                Some(d) => {
                    value = value * 16 + d;
                    digits += 1;
                    self.pos += 1;
                }
                None => break,
            }
        }
        if digits == 0 {
            let Some(c) = self.peek() else {
                return '\u{fffd}';
            };
            self.pos += 1;
            return c;
        }
        if self.peek() == Some('\r') && self.peek_at(1) == Some('\n') {
            self.pos += 2;
        } else if self.peek().is_some_and(is_ws) {
            self.pos += 1;
        }
        match char::from_u32(value) {
            Some(c) if value != 0 => c,
            _ => '\u{fffd}',
        }
    }

    fn at_ident_start(&self) -> bool {
        match self.peek() {
            Some('-') => match self.peek_at(1) {
                Some(c) if is_name_start(c) || c == '-' => true,
                _ => self.at_escape(1),
            },
            Some(c) if is_name_start(c) => true,
            _ => self.at_escape(0),
        }
    }

    /// Read an identifier with escapes decoded and case kept.
    fn read_ident(&mut self) -> Option<String> {
        if !self.at_ident_start() {
            return None;
        }
        let mut out = String::new();
        loop {
            match self.peek() {
                Some('\\') if self.at_escape(0) => out.push(self.read_escape()),
                Some(c) if is_name_char(c) => {
                    out.push(c);
                    self.pos += 1;
                }
                _ => break,
            }
        }
        Some(out)
    }

    /// Read a quoted string; the cursor is on the quote. `None` when it runs
    /// into a raw newline or the end.
    fn read_string(&mut self) -> Option<String> {
        let quote = self.peek()?;
        self.pos += 1;
        let mut out = String::new();
        loop {
            let c = self.peek()?;
            match c {
                '\n' => return None,
                '\\' => match self.peek_at(1) {
                    None => {
                        self.pos += 1;
                    }
                    Some('\n') => self.pos += 2,
                    Some(_) => out.push(self.read_escape()),
                },
                _ if c == quote => {
                    self.pos += 1;
                    return Some(out);
                }
                _ => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// The text inside `( ... )`; the cursor is just past `(`. `None` when
    /// it never closes.
    fn read_argument(&mut self) -> Option<&'s [char]> {
        let start = self.pos;
        let mut depth = 1usize;
        while let Some(c) = self.peek() {
            match c {
                '"' | '\'' => {
                    self.read_string()?;
                    continue;
                }
                '\\' => self.pos += 1,
                '(' | '[' => depth += 1,
                ')' | ']' => {
                    depth -= 1;
                    if depth == 0 {
                        let end = self.pos;
                        self.pos += 1;
                        return Some(&self.chars[start..end]);
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
        None
    }

    fn combinator(&self) -> Option<Combinator> {
        match self.peek()? {
            '>' => Some(Combinator::Child),
            '+' => Some(Combinator::Next),
            '~' => Some(Combinator::Subsequent),
            _ => None,
        }
    }
}

/// One complex selector. With `relative`, it may start with a combinator,
/// and without one it starts with the descendant combinator.
fn parse_complex(src: &[char], cx: Ctx, relative: bool) -> Parsed<Selector> {
    let mut cur = Cursor { chars: src, pos: 0 };
    cur.skip_ws();
    if cur.at_end() {
        return Err(Fail::Invalid);
    }
    let mut leading = None;
    if relative {
        let comb = cur.combinator();
        if comb.is_some() {
            cur.pos += 1;
            cur.skip_ws();
        }
        leading = Some(comb.unwrap_or(Combinator::Descendant));
    }
    let mut compounds = Vec::new();
    let mut combinators = Vec::new();
    loop {
        if compounds.len() >= MAX_SELECTOR_COMPOUNDS {
            return Err(Fail::TooBig);
        }
        compounds.push(parse_compound(&mut cur, cx)?);
        let spaced = cur.skip_ws();
        if cur.at_end() {
            break;
        }
        let comb = match cur.combinator() {
            Some(comb) => {
                cur.pos += 1;
                cur.skip_ws();
                comb
            }
            None if spaced => Combinator::Descendant,
            None => return Err(Fail::Invalid),
        };
        if cur.at_end() {
            return Err(Fail::Invalid);
        }
        combinators.push(comb);
    }
    let specificity = clamp_specificity(
        compounds
            .iter()
            .flat_map(|compound| compound.parts.iter())
            .fold((0, 0, 0), |sum, part| add(sum, simple_specificity(part))),
    );
    let subject_bits = compounds.last().map_or(0, |subject| subject.bits);
    Ok(Selector {
        leading,
        compounds,
        combinators,
        specificity,
        subject_bits,
    })
}

fn parse_compound(cur: &mut Cursor, cx: Ctx) -> Parsed<Compound> {
    let mut parts = Vec::new();
    let mut any = false;
    // Type or universal, with an optional namespace prefix.
    let starts_type = cur.peek() == Some('*')
        || cur.at_ident_start()
        || (cur.peek() == Some('|') && cur.peek_at(1) != Some('='));
    if starts_type {
        // `Some(None)` is `*`, `Some(Some(name))` a name, `None` nothing
        // before `|`.
        let first: Option<Option<String>> = if cur.peek() == Some('*') {
            cur.pos += 1;
            Some(None)
        } else if cur.peek() == Some('|') {
            None
        } else {
            Some(cur.read_ident())
        };
        if cur.peek() == Some('|') && cur.peek_at(1) != Some('=') {
            cur.pos += 1;
            let local = if cur.peek() == Some('*') {
                cur.pos += 1;
                None
            } else {
                Some(cur.read_ident().ok_or(Fail::Invalid)?)
            };
            match first {
                // `*|name`: any namespace.
                Some(None) => {
                    if let Some(name) = local {
                        parts.push(Simple::Type(name.to_ascii_lowercase()));
                    }
                }
                // `|name` asks for no namespace, and `ns|name` for a prefix
                // that `@namespace` would declare. Elements here are all in
                // the HTML namespace and `@namespace` is not read.
                _ => parts.push(Simple::Unresolved {
                    types: u16::from(local.is_some()),
                }),
            }
        } else {
            match first {
                Some(Some(name)) => parts.push(Simple::Type(name.to_ascii_lowercase())),
                Some(None) => {}
                None => return Err(Fail::Invalid),
            }
        }
        any = true;
    }
    let mut after_pseudo_element = false;
    loop {
        match cur.peek() {
            Some('#') => {
                cur.pos += 1;
                let name = cur.read_ident().ok_or(Fail::Invalid)?;
                if after_pseudo_element {
                    return Err(Fail::Invalid);
                }
                parts.push(Simple::Id(name));
            }
            Some('.') => {
                cur.pos += 1;
                let name = cur.read_ident().ok_or(Fail::Invalid)?;
                if after_pseudo_element {
                    return Err(Fail::Invalid);
                }
                parts.push(Simple::Class(name));
            }
            Some('[') => {
                cur.pos += 1;
                if after_pseudo_element {
                    return Err(Fail::Invalid);
                }
                parts.push(Simple::Attr(Box::new(parse_attribute(cur)?)));
            }
            Some(':') => {
                cur.pos += 1;
                let element = cur.peek() == Some(':');
                if element {
                    cur.pos += 1;
                }
                let name = cur.read_ident().ok_or(Fail::Invalid)?.to_ascii_lowercase();
                let arg = if cur.peek() == Some('(') {
                    cur.pos += 1;
                    Some(cur.read_argument().ok_or(Fail::Invalid)?)
                } else {
                    None
                };
                let legacy =
                    !element && arg.is_none() && LEGACY_PSEUDO_ELEMENTS.contains(&name.as_str());
                if element || legacy {
                    let known = is_known_vendor_pseudo(&name)
                        || match arg {
                            Some(arg) => {
                                PSEUDO_ELEMENT_FUNCTIONS.contains(&name.as_str())
                                    && arg.iter().any(|c| !is_ws(*c))
                            }
                            None => PSEUDO_ELEMENTS.contains(&name.as_str()),
                        };
                    // Pseudo-elements are not allowed inside `:is()`,
                    // `:not()`, `:has()`, and the other selector arguments.
                    if !known || cx.nested {
                        return Err(Fail::Invalid);
                    }
                    if element && name == "slotted" {
                        if let Some(arg) = arg {
                            match parse_list(arg, cx.inner()?, false) {
                                Ok(list) => parts.push(Simple::Slotted(list)),
                                Err(Fail::TooBig) => return Err(Fail::TooBig),
                                Err(Fail::Invalid) => parts.push(Simple::PseudoElement(None)),
                            }
                        } else {
                            parts.push(Simple::PseudoElement(None));
                        }
                    } else if element && name == "part" {
                        if let Some(arg) = arg {
                            let s: String = arg.iter().collect();
                            let tokens: Vec<String> =
                                s.split_whitespace().map(|t| t.to_string()).collect();
                            if tokens.is_empty() {
                                parts.push(Simple::PseudoElement(None));
                            } else {
                                parts.push(Simple::Part(tokens));
                            }
                        } else {
                            parts.push(Simple::PseudoElement(None));
                        }
                    } else {
                        let pseudo_kind = if arg.is_none() && name == "before" {
                            Some(false)
                        } else if arg.is_none() && name == "after" {
                            Some(true)
                        } else {
                            None
                        };
                        parts.push(Simple::PseudoElement(pseudo_kind));
                    }
                    after_pseudo_element = true;
                } else {
                    let part = match arg {
                        None => Simple::State(state_for(&name).ok_or(Fail::Invalid)?),
                        Some(arg) => parse_pseudo_function(&name, arg, cx)?,
                    };
                    parts.push(part);
                }
            }
            _ => break,
        }
        any = true;
    }
    if !any {
        return Err(Fail::Invalid);
    }
    parts.sort_by_key(Simple::cost);
    let has_shadow_pseudo = parts
        .iter()
        .any(|p| matches!(p, Simple::Slotted(_) | Simple::Part(_)));
    let bits = if has_shadow_pseudo {
        0
    } else {
        parts.iter().fold(0, |bits, part| {
            bits | match part {
                Simple::Type(name) => name_bit(b't', name),
                Simple::Id(id) => name_bit(b'#', id),
                Simple::Class(class) => name_bit(b'.', class),
                _ => 0,
            }
        })
    };
    Ok(Compound { parts, bits })
}

/// The inside of `[...]`; the cursor is just past `[`.
fn parse_attribute(cur: &mut Cursor) -> Parsed<AttrSelector> {
    cur.skip_ws();
    let mut resolvable = true;
    if cur.peek() == Some('*') && cur.peek_at(1) == Some('|') {
        cur.pos += 2;
    } else if cur.peek() == Some('|') {
        // No namespace, which every attribute here is in.
        cur.pos += 1;
    }
    let mut name = cur.read_ident().ok_or(Fail::Invalid)?;
    if cur.peek() == Some('|') && cur.peek_at(1) != Some('=') {
        cur.pos += 1;
        name = cur.read_ident().ok_or(Fail::Invalid)?;
        resolvable = false;
    }
    cur.skip_ws();
    let mut out = AttrSelector {
        name: name.to_ascii_lowercase(),
        resolvable,
        op: AttrOp::Exists,
        value: String::new(),
        case: CaseFlag::Default,
    };
    if cur.peek() == Some(']') {
        cur.pos += 1;
        return Ok(out);
    }
    out.op = match (cur.peek(), cur.peek_at(1)) {
        (Some('='), _) => AttrOp::Equals,
        (Some('~'), Some('=')) => AttrOp::Includes,
        (Some('|'), Some('=')) => AttrOp::DashMatch,
        (Some('^'), Some('=')) => AttrOp::Prefix,
        (Some('$'), Some('=')) => AttrOp::Suffix,
        (Some('*'), Some('=')) => AttrOp::Substring,
        _ => return Err(Fail::Invalid),
    };
    cur.pos += if out.op == AttrOp::Equals { 1 } else { 2 };
    cur.skip_ws();
    out.value = match cur.peek() {
        Some('"' | '\'') => cur.read_string().ok_or(Fail::Invalid)?,
        _ => cur.read_ident().ok_or(Fail::Invalid)?,
    };
    let spaced = cur.skip_ws();
    if cur.peek() != Some(']') {
        let flag = if spaced { cur.read_ident() } else { None };
        out.case = match flag.as_deref() {
            Some(f) if f.eq_ignore_ascii_case("i") => CaseFlag::Insensitive,
            Some(f) if f.eq_ignore_ascii_case("s") => CaseFlag::Sensitive,
            _ => return Err(Fail::Invalid),
        };
        cur.skip_ws();
    }
    if cur.peek() != Some(']') {
        return Err(Fail::Invalid);
    }
    cur.pos += 1;
    Ok(out)
}

fn parse_pseudo_function(name: &str, arg: &[char], cx: Ctx) -> Parsed<Simple> {
    if is_known_vendor_pseudo(name) {
        return Ok(Simple::State(State::Never));
    }
    if !PSEUDO_CLASS_FUNCTIONS.contains(&name) {
        return Err(Fail::Invalid);
    }
    let blank = arg.iter().all(|c| is_ws(*c));
    match name {
        "is" => Ok(Simple::Is(parse_forgiving(arg, cx.inner()?)?)),
        "where" => Ok(Simple::Where(parse_forgiving(arg, cx.inner()?)?)),
        "not" => Ok(Simple::Not(parse_list(arg, cx.inner()?, false)?)),
        "has" => {
            if cx.in_has {
                return Err(Fail::Invalid);
            }
            let mut inner = cx.inner()?;
            inner.in_has = true;
            Ok(Simple::Has(parse_list(arg, inner, true)?))
        }
        "nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type" => {
            parse_nth(name, arg, cx).map(|nth| Simple::Nth(Box::new(nth)))
        }
        "lang" => parse_lang(arg).map(Simple::Lang),
        "dir" => {
            let mut cur = Cursor { chars: arg, pos: 0 };
            cur.skip_ws();
            let dir = cur.read_ident().ok_or(Fail::Invalid)?;
            cur.skip_ws();
            if !cur.at_end() {
                return Err(Fail::Invalid);
            }
            let dir = dir.to_ascii_lowercase();
            Ok(Simple::Dir(match dir.as_str() {
                "ltr" => Some(false),
                "rtl" => Some(true),
                _ => None,
            }))
        }
        "host" if !blank => match parse_list(arg, cx.inner()?, false) {
            Ok(list) => Ok(Simple::Host(list)),
            Err(Fail::TooBig) => Err(Fail::TooBig),
            Err(Fail::Invalid) => Ok(Simple::State(State::Never)),
        },
        "host-context" if !blank => match parse_list(arg, cx.inner()?, false) {
            Ok(list) => Ok(Simple::HostContext(list)),
            Err(Fail::TooBig) => Err(Fail::TooBig),
            Err(Fail::Invalid) => Ok(Simple::State(State::Never)),
        },
        _ if blank => Err(Fail::Invalid),
        _ => Ok(Simple::State(State::Never)),
    }
}

/// `:lang()` takes identifiers or strings separated by commas.
fn parse_lang(arg: &[char]) -> Parsed<Vec<String>> {
    let mut out = Vec::new();
    for part in split_list(arg) {
        let mut cur = Cursor {
            chars: part,
            pos: 0,
        };
        cur.skip_ws();
        let range = match cur.peek() {
            Some('"' | '\'') => cur.read_string(),
            _ => cur.read_ident(),
        }
        .ok_or(Fail::Invalid)?;
        cur.skip_ws();
        if !cur.at_end() {
            return Err(Fail::Invalid);
        }
        out.push(range);
    }
    Ok(out)
}

fn parse_nth(name: &str, arg: &[char], cx: Ctx) -> Parsed<Nth> {
    let of_type = name.ends_with("of-type");
    // `of` can only follow white space, and An+B never holds the letters
    // `o` then `f`, so the first such `of` ends the An+B part.
    let mut split = None;
    for i in 1..arg.len().saturating_sub(1) {
        if is_ws(arg[i - 1])
            && arg[i].eq_ignore_ascii_case(&'o')
            && arg[i + 1].eq_ignore_ascii_case(&'f')
            && arg
                .get(i + 2)
                .is_none_or(|c| !is_name_char(*c) && *c != '\\')
        {
            split = Some(i);
            break;
        }
    }
    let (an_b, of) = match split {
        Some(i) => (&arg[..i], Some(&arg[i + 2..])),
        None => (arg, None),
    };
    let text: String = an_b.iter().collect();
    let (a, b) = parse_an_plus_b(&text).ok_or(Fail::Invalid)?;
    let of = match of {
        Some(_) if of_type => return Err(Fail::Invalid),
        Some(list) => Some(parse_list(list, cx.inner()?, false)?),
        None => None,
    };
    Ok(Nth {
        a,
        b,
        from_end: name.starts_with("nth-last"),
        of_type,
        of,
    })
}

/// Read `An+B`, `odd`, or `even`. White space may sit around the whole and
/// around the sign before `B`, and nowhere else.
fn parse_an_plus_b(text: &str) -> Option<(i64, i64)> {
    let text = text.trim_matches(is_ws).to_ascii_lowercase();
    match text.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    let Some(n_at) = text.find('n') else {
        return parse_signed(&text).map(|b| (0, b));
    };
    let (head, tail) = (&text[..n_at], &text[n_at + 1..]);
    let a = match head {
        "" | "+" => 1,
        "-" => -1,
        _ => parse_signed(head)?,
    };
    let tail = tail.trim_start_matches(is_ws);
    if tail.is_empty() {
        return Some((a, 0));
    }
    let negative = match tail.as_bytes()[0] {
        b'+' => false,
        b'-' => true,
        _ => return None,
    };
    let digits = tail[1..].trim_start_matches(is_ws);
    let b = parse_digits(digits)?;
    Some((a, if negative { -b } else { b }))
}

/// An optional sign directly followed by digits.
fn parse_signed(text: &str) -> Option<i64> {
    let (negative, digits) = match text.as_bytes().first()? {
        b'+' => (false, &text[1..]),
        b'-' => (true, &text[1..]),
        _ => (false, text),
    };
    let value = parse_digits(digits)?;
    Some(if negative { -value } else { value })
}

/// ASCII digits only, saturating far beyond any real sibling count.
fn parse_digits(digits: &str) -> Option<i64> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(digits.bytes().fold(0i64, |acc, b| {
        acc.saturating_mul(10)
            .saturating_add(i64::from(b - b'0'))
            .min(1 << 40)
    }))
}

/// One of 64 bits for a type, id, or class name, so a whole compound's
/// names can be checked against an element with one mask.
fn name_bit(kind: u8, name: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ u64::from(kind);
    for b in name.bytes() {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    1 << (hash >> 58)
}

/// The [`name_bit`]s of an element's own type, id, and classes.
fn element_bits(data: &NodeData) -> u64 {
    let Some(tag) = data.tag() else {
        return 0;
    };
    let mut bits = name_bit(b't', tag);
    if let Some(id) = data.id() {
        bits |= name_bit(b'#', id);
    }
    if let Some(classes) = data.attr("class") {
        for class in classes.split_ascii_whitespace() {
            bits |= name_bit(b'.', class);
        }
    }
    bits
}

type Spec = (u32, u32, u32);

fn add(x: Spec, y: Spec) -> Spec {
    (
        x.0.saturating_add(y.0),
        x.1.saturating_add(y.1),
        x.2.saturating_add(y.2),
    )
}

fn clamp_specificity(spec: Spec) -> (u16, u16, u16) {
    let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
    (clamp(spec.0), clamp(spec.1), clamp(spec.2))
}

/// Largest specificity in a list, or zero for an empty one.
fn max_specificity(list: &[Selector]) -> Spec {
    list.iter()
        .map(|sel| {
            let (a, b, c) = sel.specificity;
            (u32::from(a), u32::from(b), u32::from(c))
        })
        .max()
        .unwrap_or((0, 0, 0))
}

fn simple_specificity(part: &Simple) -> Spec {
    match part {
        Simple::Type(_) | Simple::PseudoElement(_) => (0, 0, 1),
        Simple::Unresolved { types } => (0, 0, u32::from(*types)),
        Simple::Id(_) => (1, 0, 0),
        Simple::Class(_)
        | Simple::Attr(_)
        | Simple::State(_)
        | Simple::Lang(_)
        | Simple::Dir(_) => (0, 1, 0),
        Simple::Nth(nth) => add(
            (0, 1, 0),
            nth.of.as_deref().map(max_specificity).unwrap_or((0, 0, 0)),
        ),
        Simple::Host(list) | Simple::HostContext(list) => add((0, 1, 0), max_specificity(list)),
        Simple::Slotted(list) => add((0, 0, 1), max_specificity(list)),
        Simple::Part(_) => (0, 0, 1),
        Simple::Is(list) | Simple::Not(list) | Simple::Has(list) => max_specificity(list),
        Simple::Where(_) => (0, 0, 0),
    }
}

impl Selector {
    /// True when `node_id` is an element of `doc` that this selector
    /// matches. Pseudo-element selectors match no element.
    ///
    /// Each call starts with empty caches and reads sibling positions from
    /// the tree, so it costs more per element than the cascade, which builds
    /// its own index once per pass.
    pub fn matches(&self, doc: &Document, node_id: NodeId) -> bool {
        if node_id.index() >= doc.node_count() {
            return false;
        }
        Matcher::new(doc, None).matches(self, node_id)
    }

    /// Specificity as `(ids, classes, types)`. `:is()`, `:not()`, and
    /// `:has()` count their most specific argument, `:where()` counts
    /// nothing, and `nth-child(An+B of S)` counts one class plus the most
    /// specific selector in `S`. Each part stops at `u16::MAX`.
    pub fn specificity(&self) -> (u16, u16, u16) {
        self.specificity
    }

    /// Returns `Some(false)` when this selector targets `::before`, `Some(true)`
    /// when it targets `::after`, and `None` otherwise.
    pub(crate) fn pseudo_before_or_after(&self) -> Option<bool> {
        let subject = self.compounds.last()?;
        let mut target = None;
        for part in &subject.parts {
            if let Simple::PseudoElement(kind) = part {
                if target.is_some() {
                    return None;
                }
                target = Some(*kind);
            }
        }
        target.flatten()
    }

    pub(crate) fn has_part(&self) -> bool {
        self.compounds
            .iter()
            .any(|c| c.parts.iter().any(|p| matches!(p, Simple::Part(_))))
    }

    pub(crate) fn has_slotted(&self) -> bool {
        self.compounds
            .iter()
            .any(|c| c.parts.iter().any(|p| matches!(p, Simple::Slotted(_))))
    }

    pub(crate) fn has_host(&self) -> bool {
        self.compounds.iter().any(|c| {
            c.parts.iter().any(|p| {
                matches!(
                    p,
                    Simple::Host(_) | Simple::HostContext(_) | Simple::State(State::Host)
                )
            })
        })
    }

    fn last(&self) -> usize {
        self.compounds.len().saturating_sub(1)
    }

    fn key(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }
}

/// No node.
const NONE: u32 = u32::MAX;

/// Tree positions for every node reachable from the document node, built in
/// one pass so sibling and ancestor questions take constant time.
pub(crate) struct DocIndex {
    slots: Vec<Slot>,
}

#[derive(Clone, Copy)]
struct Slot {
    /// Parent element, or [`NONE`].
    parent: u32,
    prev: u32,
    next: u32,
    /// 1-based place among element siblings, and their number.
    pos: u32,
    count: u32,
    /// The same among siblings with the same name.
    type_pos: u32,
    type_count: u32,
    /// Pre-order number, and the largest one inside this subtree.
    enter: u32,
    exit: u32,
    /// [`element_bits`] of this element, and of all its ancestors together.
    bits: u64,
    above: u64,
}

const EMPTY_SLOT: Slot = Slot {
    parent: NONE,
    prev: NONE,
    next: NONE,
    pos: 0,
    count: 0,
    type_pos: 0,
    type_count: 0,
    enter: NONE,
    exit: NONE,
    bits: 0,
    above: 0,
};

impl DocIndex {
    pub(crate) fn build(doc: &Document) -> DocIndex {
        let mut slots = vec![EMPTY_SLOT; doc.node_count()];
        let mut order = 0u32;
        let mut counts: HashMap<&str, u32> = HashMap::new();
        // (node, next child to visit)
        let mut stack: Vec<(NodeId, usize)> = vec![(doc.root(), 0)];
        slots[doc.root().index()].enter = 0;
        order += 1;
        while let Some(top) = stack.last_mut() {
            let (node, child_at) = *top;
            let kids = doc.children(node);
            if child_at == 0 {
                link_siblings(doc, node, kids, &mut slots, &mut counts);
            }
            if let Some(&child) = kids.get(child_at) {
                top.1 += 1;
                let (bits, above) = slots
                    .get(node.index())
                    .map_or((0, 0), |slot| (slot.bits, slot.above));
                if let Some(slot) = slots.get_mut(child.index())
                    && slot.enter == NONE
                {
                    slot.enter = order;
                    slot.bits = element_bits(doc.get(child).data());
                    slot.above = above | bits;
                    order += 1;
                    stack.push((child, 0));
                }
            } else {
                if let Some(slot) = slots.get_mut(node.index()) {
                    slot.exit = order - 1;
                }
                stack.pop();
            }
        }
        DocIndex { slots }
    }
}

/// Fill sibling links and positions for the element children of `parent`.
fn link_siblings<'d>(
    doc: &'d Document,
    parent: NodeId,
    kids: &[NodeId],
    slots: &mut [Slot],
    counts: &mut HashMap<&'d str, u32>,
) {
    let parent_el = if doc.get(parent).data().tag().is_some() {
        parent.index() as u32
    } else {
        NONE
    };
    counts.clear();
    let mut total = 0u32;
    for &kid in kids {
        if let Some(tag) = doc.get(kid).data().tag() {
            total += 1;
            *counts.entry(tag).or_insert(0) += 1;
        }
    }
    let mut seen: HashMap<&str, u32> = HashMap::with_capacity(counts.len());
    let mut pos = 0u32;
    let mut prev = NONE;
    for &kid in kids {
        let Some(tag) = doc.get(kid).data().tag() else {
            continue;
        };
        pos += 1;
        let type_pos = {
            let n = seen.entry(tag).or_insert(0);
            *n += 1;
            *n
        };
        let k = kid.index() as u32;
        if let Some(slot) = slots.get_mut(kid.index()) {
            slot.parent = parent_el;
            slot.prev = prev;
            slot.pos = pos;
            slot.count = total;
            slot.type_pos = type_pos;
            slot.type_count = counts.get(tag).copied().unwrap_or(1);
        }
        if let Some(prev_slot) = slots.get_mut(prev as usize) {
            prev_slot.next = k;
        }
        prev = k;
    }
}

/// Small, fast hasher for the integer keys of the match caches.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_u8(&mut self, n: u8) {
        self.write_u64(u64::from(n));
    }

    fn write_u32(&mut self, n: u32) {
        self.write_u64(u64::from(n));
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<KeyHasher>>;

/// Which way a cached walk goes.
#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Walk {
    /// Earlier siblings, for `~`.
    Back,
    /// Later siblings, for `:has(~ ...)`.
    Forward,
    /// Descendants, for `:has(...)`.
    Down,
    /// The whole `:has()` answer for one relative selector.
    Has,
}

/// How a failed match leaves the search around it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Matched,
    /// This element does not match; another candidate might.
    TryNext,
    /// No earlier sibling can match; an element under another ancestor
    /// might.
    TryAncestor,
    /// Nothing further up the tree can match either.
    Nowhere,
}

/// Cache key: selector, compound, node, `:has()` element, scope root, walk.
type MemoKey = (usize, u32, u32, u32, u32, Walk);

/// Matches selectors against one document, caching as it goes.
///
/// The caches assume the document and the selectors do not change while
/// the matcher lives. One matcher serves one cascade pass on one thread.
pub(crate) struct Matcher<'d> {
    doc: &'d Document,
    index: Option<&'d DocIndex>,
    memo: RefCell<FastMap<MemoKey, bool>>,
    /// (`of` list, element) to (place among matching siblings, how many);
    /// place 0 when the element does not match the list.
    of_places: RefCell<FastMap<(usize, u32), (u32, u32)>>,
    /// `select` element to its selected `option`, or [`NONE`].
    selected: RefCell<FastMap<u32, u32>>,
    /// `form` element to its default button, or [`NONE`].
    default_button: RefCell<FastMap<u32, u32>>,
    /// The last element asked about and its [`element_bits`]. The cascade
    /// asks about one element for every rule in a row.
    last_bits: Cell<(u32, u64)>,
    /// Active `@scope` root element, or [`NONE`] when outside `@scope`.
    scope_root: Cell<u32>,
}

impl<'d> Matcher<'d> {
    pub(crate) fn new(doc: &'d Document, index: Option<&'d DocIndex>) -> Matcher<'d> {
        Matcher {
            doc,
            index,
            memo: RefCell::default(),
            of_places: RefCell::default(),
            selected: RefCell::default(),
            default_button: RefCell::default(),
            last_bits: Cell::new((NONE, 0)),
            scope_root: Cell::new(NONE),
        }
    }

    pub(crate) fn matches_with_scope(
        &self,
        sel: &Selector,
        node: NodeId,
        scope_root: Option<NodeId>,
    ) -> bool {
        let prev = self.scope_root.get();
        self.scope_root
            .set(scope_root.map_or(NONE, |id| id.index() as u32));
        let res = self.matches(sel, node);
        self.scope_root.set(prev);
        res
    }

    pub(crate) fn matches_pseudo_with_scope(
        &self,
        sel: &Selector,
        node: NodeId,
        is_after: bool,
        scope_root: Option<NodeId>,
    ) -> bool {
        if sel.pseudo_before_or_after() != Some(is_after) {
            return false;
        }
        let n = node.index() as u32;
        if let Some(slot) = self.slot(n)
            && sel.subject_bits & !slot.bits != 0
        {
            return false;
        }
        let Some(subject) = sel.compounds.last() else {
            return false;
        };
        let prev = self.scope_root.get();
        self.scope_root
            .set(scope_root.map_or(NONE, |id| id.index() as u32));
        let data = self.data(n);
        let res = if let Some(tag) = data.tag() {
            let subject_ok = subject.parts.iter().all(|part| match part {
                Simple::PseudoElement(Some(p)) => *p == is_after,
                Simple::Type(name) => tag == name,
                Simple::Class(class) => data.has_class(class),
                Simple::Id(id) => data.id() == Some(id.as_str()),
                _ => self.simple(part, n, data, tag),
            });
            if !subject_ok {
                false
            } else if sel.compounds.len() == 1 {
                true
            } else {
                let idx = sel.last();
                let limit = self.walk_limit(sel, NONE);
                match sel.combinators[idx - 1] {
                    Combinator::Child => {
                        let p = self.parent_el(n);
                        p != NONE
                            && p != limit
                            && self.match_from(sel, idx - 1, p, NONE) == Outcome::Matched
                    }
                    Combinator::Next => {
                        let p = self.prev(n);
                        p != NONE && self.match_from(sel, idx - 1, p, NONE) == Outcome::Matched
                    }
                    Combinator::Descendant => {
                        let mut p = self.parent_el(n);
                        let mut matched = false;
                        while p != NONE && p != limit {
                            match self.match_from(sel, idx - 1, p, NONE) {
                                Outcome::Matched => {
                                    matched = true;
                                    break;
                                }
                                Outcome::Nowhere => break,
                                Outcome::TryNext | Outcome::TryAncestor => p = self.parent_el(p),
                            }
                        }
                        matched
                    }
                    Combinator::Subsequent => {
                        self.any_earlier(sel, idx - 1, n, NONE) == Outcome::Matched
                    }
                }
            }
        } else {
            false
        };
        self.scope_root.set(prev);
        res
    }

    pub(crate) fn matches(&self, sel: &Selector, node: NodeId) -> bool {
        let n = node.index() as u32;
        if let Some(slot) = self.slot(n) {
            if sel.subject_bits & !slot.bits != 0 {
                return false;
            }
        } else if sel.subject_bits != 0 {
            let (last, cached) = self.last_bits.get();
            let bits = if last == n {
                cached
            } else {
                let bits = element_bits(self.data(n));
                self.last_bits.set((n, bits));
                bits
            };
            if sel.subject_bits & !bits != 0 {
                return false;
            }
        }
        self.match_at(sel, sel.last(), n, NONE)
    }

    fn any_matches(&self, list: &[Selector], node: u32) -> bool {
        list.iter()
            .any(|sel| self.match_at(sel, sel.last(), node, NONE))
    }

    // ----- tree navigation -----

    fn slot(&self, n: u32) -> Option<&Slot> {
        let slot = self.index?.slots.get(n as usize)?;
        (slot.enter != NONE).then_some(slot)
    }

    fn node_id(n: u32) -> NodeId {
        NodeId::from_raw(n)
    }

    fn data(&self, n: u32) -> &'d NodeData {
        self.doc.get(Self::node_id(n)).data()
    }

    fn tag(&self, n: u32) -> Option<&'d str> {
        self.data(n).tag()
    }

    fn parent_el(&self, n: u32) -> u32 {
        if let Some(slot) = self.slot(n) {
            return slot.parent;
        }
        self.doc
            .parent_element(Self::node_id(n))
            .map_or(NONE, |p| p.index() as u32)
    }

    fn prev(&self, n: u32) -> u32 {
        if let Some(slot) = self.slot(n) {
            return slot.prev;
        }
        self.doc
            .previous_element_sibling(Self::node_id(n))
            .map_or(NONE, |p| p.index() as u32)
    }

    fn next(&self, n: u32) -> u32 {
        if let Some(slot) = self.slot(n) {
            return slot.next;
        }
        self.doc
            .next_element_sibling(Self::node_id(n))
            .map_or(NONE, |p| p.index() as u32)
    }

    /// Element children, in order.
    fn element_children(&self, n: u32) -> impl Iterator<Item = u32> + 'd {
        let doc = self.doc;
        doc.children(Self::node_id(n))
            .iter()
            .filter(move |&&kid| doc.get(kid).data().tag().is_some())
            .map(|kid| kid.index() as u32)
    }

    /// (place, count) among element siblings, all or of the same name.
    fn place(&self, n: u32, of_type: bool) -> (u32, u32) {
        if let Some(slot) = self.slot(n) {
            return if of_type {
                (slot.type_pos, slot.type_count)
            } else {
                (slot.pos, slot.count)
            };
        }
        let Some(parent) = self.doc.get(Self::node_id(n)).parent() else {
            return (1, 1);
        };
        let tag = self.tag(n);
        let mut place = 0;
        let mut count = 0;
        for kid in self.element_children(parent.index() as u32) {
            if of_type && self.tag(kid) != tag {
                continue;
            }
            count += 1;
            if kid == n {
                place = count;
            }
        }
        (place, count)
    }

    /// True when `a` is a proper ancestor of `n`.
    fn is_ancestor(&self, a: u32, n: u32) -> bool {
        if let (Some(sa), Some(sn)) = (self.slot(a), self.slot(n)) {
            return sa.enter < sn.enter && sn.enter <= sa.exit;
        }
        let mut cur = self.doc.get(Self::node_id(n)).parent();
        while let Some(p) = cur {
            if p.index() as u32 == a {
                return true;
            }
            cur = self.doc.get(p).parent();
        }
        false
    }

    // ----- complex selectors -----

    /// Does element `n` match compound `idx` of `sel`, with everything to
    /// its left matching too? `anchor` is the `:has()` element for a
    /// relative selector and [`NONE`] otherwise.
    fn match_at(&self, sel: &Selector, idx: usize, n: u32, anchor: u32) -> bool {
        self.match_from(sel, idx, n, anchor) == Outcome::Matched
    }

    /// [`Self::match_at`], saying on a miss how far out a caller still needs
    /// to look. That lets a descendant walk stop as soon as nothing above can
    /// match, which keeps chains of descendant compounds from going
    /// exponential.
    fn match_from(&self, sel: &Selector, idx: usize, n: u32, anchor: u32) -> Outcome {
        let Some(compound) = sel.compounds.get(idx) else {
            return Outcome::Nowhere;
        };
        let n = if compound
            .parts
            .iter()
            .any(|p| matches!(p, Simple::Slotted(_) | Simple::Part(_)))
        {
            let Some(mapped_n) = self.compound_shadow_pseudo(compound, n) else {
                return Outcome::TryNext;
            };
            mapped_n
        } else {
            if !self.compound(compound, n) {
                return Outcome::TryNext;
            }
            n
        };
        if idx == 0 {
            return match sel.leading {
                None => Outcome::Matched,
                Some(comb) if self.related(comb, anchor, n) => Outcome::Matched,
                Some(_) => Outcome::TryNext,
            };
        }
        let limit = self.walk_limit(sel, anchor);
        match sel.combinators[idx - 1] {
            Combinator::Child => {
                let p = self.parent_el(n);
                if p == NONE || p == limit {
                    return Outcome::Nowhere;
                }
                self.match_from(sel, idx - 1, p, anchor)
            }
            Combinator::Next => {
                let p = self.prev(n);
                if p == NONE {
                    return Outcome::TryAncestor;
                }
                self.match_from(sel, idx - 1, p, anchor)
            }
            Combinator::Descendant => {
                // Names the next compound needs that no ancestor has.
                let wanted = sel.compounds[idx - 1].bits;
                if self.slot(n).is_some_and(|slot| wanted & !slot.above != 0) {
                    return Outcome::Nowhere;
                }
                let mut p = self.parent_el(n);
                loop {
                    if p == NONE || p == limit {
                        return Outcome::Nowhere;
                    }
                    match self.match_from(sel, idx - 1, p, anchor) {
                        Outcome::TryNext | Outcome::TryAncestor => p = self.parent_el(p),
                        done => return done,
                    }
                }
            }
            Combinator::Subsequent => self.any_earlier(sel, idx - 1, n, anchor),
        }
    }

    /// Where a walk up from a subject stops for a relative selector: every
    /// compound sits strictly inside the `:has()` element, or inside its
    /// parent when the selector starts with `+` or `~`.
    fn walk_limit(&self, sel: &Selector, anchor: u32) -> u32 {
        match sel.leading {
            Some(Combinator::Child | Combinator::Descendant) => anchor,
            Some(Combinator::Next | Combinator::Subsequent) => self.parent_el(anchor),
            None => NONE,
        }
    }

    /// Does an earlier sibling of `start` match compound `idx`? Answers are
    /// kept per element, so a long run of siblings is walked once per
    /// selector instead of once per element.
    fn any_earlier(&self, sel: &Selector, idx: usize, start: u32, anchor: u32) -> Outcome {
        let sr = self.scope_root.get();
        let key = |n: u32| (sel.key(), idx as u32, n, anchor, sr, Walk::Back);
        let mut pending = Vec::new();
        let mut cur = start;
        let mut outcome = Outcome::TryAncestor;
        loop {
            if let Some(&known) = self.memo.borrow().get(&key(cur)) {
                if known {
                    outcome = Outcome::Matched;
                }
                break;
            }
            pending.push(cur);
            let step = self.prev(cur);
            if step == NONE {
                break;
            }
            match self.match_from(sel, idx, step, anchor) {
                Outcome::Matched => {
                    outcome = Outcome::Matched;
                    break;
                }
                // Siblings share their ancestors: none of them can match.
                Outcome::Nowhere => {
                    outcome = Outcome::Nowhere;
                    break;
                }
                Outcome::TryAncestor | Outcome::TryNext => cur = step,
            }
        }
        let found = outcome == Outcome::Matched;
        let mut memo = self.memo.borrow_mut();
        for n in pending {
            memo.insert(key(n), found);
        }
        outcome
    }

    /// The leading combinator of a relative selector, from `anchor` to `n`.
    fn related(&self, comb: Combinator, anchor: u32, n: u32) -> bool {
        if anchor == NONE {
            return false;
        }
        match comb {
            Combinator::Child => self.parent_el(n) == anchor,
            Combinator::Descendant => self.is_ancestor(anchor, n),
            Combinator::Next => self.prev(n) == anchor,
            Combinator::Subsequent => {
                if self.parent_el(n) != self.parent_el(anchor)
                    || self.doc.get(Self::node_id(n)).parent()
                        != self.doc.get(Self::node_id(anchor)).parent()
                {
                    return false;
                }
                self.place(anchor, false).0 < self.place(n, false).0
            }
        }
    }

    fn compound(&self, compound: &Compound, n: u32) -> bool {
        if self
            .slot(n)
            .is_some_and(|slot| compound.bits & !slot.bits != 0)
        {
            return false;
        }
        let data = self.data(n);
        let Some(tag) = data.tag() else {
            return false;
        };
        for part in &compound.parts {
            // The common simple selectors inline; the rest go through
            // `simple`.
            let hit = match part {
                Simple::Type(name) => tag == name,
                Simple::Class(class) => data.has_class(class),
                Simple::Id(id) => data.id() == Some(id.as_str()),
                _ => self.simple(part, n, data, tag),
            };
            if !hit {
                return false;
            }
        }
        true
    }

    fn compound_shadow_pseudo(&self, compound: &Compound, n: u32) -> Option<u32> {
        let data_n = self.data(n);
        let tag_n = data_n.tag()?;
        let nid = Self::node_id(n);
        let mut target_n = n;
        for part in &compound.parts {
            match part {
                Simple::Slotted(list) => {
                    let slot_id = self.doc.assigned_slot(nid)?;
                    if !self.any_matches(list, n) {
                        return None;
                    }
                    target_n = slot_id.index() as u32;
                }
                Simple::Part(tokens) => {
                    let host_id = self.doc.enclosing_shadow_host(nid)?;
                    if host_id == nid {
                        return None;
                    }
                    let part_attr = data_n.attr("part")?;
                    if !tokens
                        .iter()
                        .all(|w| part_attr.split_whitespace().any(|t| t == w))
                    {
                        return None;
                    }
                    target_n = host_id.index() as u32;
                }
                _ => {}
            }
        }
        let data_t = self.data(target_n);
        let tag_t = data_t.tag()?;
        for part in &compound.parts {
            match part {
                Simple::Slotted(_) | Simple::Part(_) => continue,
                Simple::Type(name) => {
                    if tag_t != name {
                        return None;
                    }
                }
                Simple::Class(class) => {
                    if !data_t.has_class(class) {
                        return None;
                    }
                }
                Simple::Id(id) => {
                    if data_t.id() != Some(id.as_str()) {
                        return None;
                    }
                }
                Simple::State(_) => {
                    if !self.simple(part, target_n, data_t, tag_t)
                        && !self.simple(part, n, data_n, tag_n)
                    {
                        return None;
                    }
                }
                _ => {
                    if !self.simple(part, target_n, data_t, tag_t) {
                        return None;
                    }
                }
            }
        }
        Some(target_n)
    }

    fn simple(&self, part: &Simple, n: u32, data: &NodeData, tag: &str) -> bool {
        match part {
            Simple::Type(name) => tag == name,
            Simple::Unresolved { .. }
            | Simple::PseudoElement(_)
            | Simple::Slotted(_)
            | Simple::Part(_) => false,
            Simple::Id(id) => data.id() == Some(id.as_str()),
            Simple::Class(class) => data.has_class(class),
            Simple::Attr(attr) => attr_matches(attr, data),
            Simple::State(state) => self.state(*state, n, data, tag),
            Simple::Nth(nth) => self.nth(nth, n),
            Simple::Is(list) | Simple::Where(list) => self.any_matches(list, n),
            Simple::Not(list) => !self.any_matches(list, n),
            Simple::Has(list) => list.iter().any(|rel| self.has(rel, n)),
            Simple::Host(list) => {
                let nid = Self::node_id(n);
                if !self.doc.is_shadow_host(nid) {
                    return false;
                }
                let host = self.doc.shadow_host(nid).unwrap_or(nid);
                self.any_matches(list, host.index() as u32)
            }
            Simple::HostContext(list) => {
                let nid = Self::node_id(n);
                if !self.doc.is_shadow_host(nid) {
                    return false;
                }
                let host = self.doc.shadow_host(nid).unwrap_or(nid);
                let mut cur = host.index() as u32;
                while cur != NONE {
                    if self.any_matches(list, cur) {
                        return true;
                    }
                    cur = self.parent_el(cur);
                }
                false
            }
            Simple::Lang(ranges) => self.lang(ranges, n),
            Simple::Dir(dir) => dir.is_some_and(|rtl| self.is_rtl(n) == rtl),
        }
    }

    // ----- :has() -----

    fn has(&self, rel: &Selector, anchor: u32) -> bool {
        let key = (rel.key(), 0, anchor, NONE, self.scope_root.get(), Walk::Has);
        if let Some(&known) = self.memo.borrow().get(&key) {
            return known;
        }
        let found = self.has_uncached(rel, anchor);
        self.memo.borrow_mut().insert(key, found);
        found
    }

    fn has_uncached(&self, rel: &Selector, anchor: u32) -> bool {
        let leading = rel.leading.unwrap_or(Combinator::Descendant);
        if let [only] = rel.compounds.as_slice() {
            return match leading {
                Combinator::Child => self
                    .element_children(anchor)
                    .any(|kid| self.compound(only, kid)),
                Combinator::Next => {
                    let next = self.next(anchor);
                    next != NONE && self.compound(only, next)
                }
                Combinator::Subsequent => self.any_after(rel, only, anchor, Walk::Forward),
                Combinator::Descendant => self.any_after(rel, only, anchor, Walk::Down),
            };
        }
        // Where the subject can be: among the children or later siblings of
        // the anchor, or anywhere below those when a child or descendant
        // combinator follows.
        let deep = leading == Combinator::Descendant
            || rel
                .combinators
                .iter()
                .any(|c| matches!(c, Combinator::Child | Combinator::Descendant));
        let mut roots = Vec::new();
        match leading {
            Combinator::Child | Combinator::Descendant => {
                roots.extend(self.element_children(anchor));
            }
            Combinator::Next | Combinator::Subsequent => {
                let mut sib = self.next(anchor);
                while sib != NONE {
                    roots.push(sib);
                    sib = self.next(sib);
                }
            }
        }
        let last = rel.last();
        let mut stack: Vec<u32> = roots.into_iter().rev().collect();
        while let Some(n) = stack.pop() {
            if self.match_at(rel, last, n, anchor) {
                return true;
            }
            if deep {
                let before = stack.len();
                stack.extend(self.element_children(n));
                stack[before..].reverse();
            }
        }
        false
    }

    /// For a one-compound relative selector: does any node after `start`
    /// (later siblings for [`Walk::Forward`], descendants for
    /// [`Walk::Down`]) match `only`? Each node's answer is kept, so asking
    /// for every element of the tree costs one pass over it.
    fn any_after(&self, rel: &Selector, only: &Compound, start: u32, walk: Walk) -> bool {
        let sr = self.scope_root.get();
        let key = |n: u32| (rel.key(), 0, n, NONE, sr, walk);
        if walk == Walk::Forward {
            let mut pending = Vec::new();
            let mut cur = start;
            let found = loop {
                if let Some(&known) = self.memo.borrow().get(&key(cur)) {
                    break known;
                }
                pending.push(cur);
                let step = self.next(cur);
                if step == NONE {
                    break false;
                }
                if self.compound(only, step) {
                    break true;
                }
                cur = step;
            };
            let mut memo = self.memo.borrow_mut();
            for n in pending {
                memo.insert(key(n), found);
            }
            return found;
        }
        if let Some(&known) = self.memo.borrow().get(&key(start)) {
            return known;
        }
        // Post-order walk: (node, its element children, next child, found).
        let mut stack: Vec<(u32, Vec<u32>, usize, bool)> =
            vec![(start, self.element_children(start).collect(), 0, false)];
        let mut result = false;
        while let Some(top) = stack.last_mut() {
            if !top.3 && top.2 < top.1.len() {
                let kid = top.1[top.2];
                top.2 += 1;
                if self.compound(only, kid) {
                    top.3 = true;
                    continue;
                }
                let known = self.memo.borrow().get(&key(kid)).copied();
                match known {
                    Some(true) => top.3 = true,
                    Some(false) => {}
                    None => {
                        let kids = self.element_children(kid).collect();
                        stack.push((kid, kids, 0, false));
                    }
                }
                continue;
            }
            let (node, _, _, found) = stack.pop().unwrap_or((NONE, Vec::new(), 0, false));
            self.memo.borrow_mut().insert(key(node), found);
            match stack.last_mut() {
                Some(parent) => parent.3 |= found,
                None => result = found,
            }
        }
        result
    }

    // ----- :nth-*() -----

    fn nth(&self, nth: &Nth, n: u32) -> bool {
        let (place, count) = match &nth.of {
            Some(list) => self.of_place(list, n),
            None => self.place(n, nth.of_type),
        };
        if place == 0 {
            return false;
        }
        let index = if nth.from_end {
            i64::from(count) - i64::from(place) + 1
        } else {
            i64::from(place)
        };
        nth_hits(nth.a, nth.b, index)
    }

    /// Place among the siblings that match `list`, worked out once for all
    /// the siblings together.
    fn of_place(&self, list: &[Selector], n: u32) -> (u32, u32) {
        let list_key = list.as_ptr() as usize;
        if let Some(&known) = self.of_places.borrow().get(&(list_key, n)) {
            return known;
        }
        let siblings: Vec<u32> = match self.doc.get(Self::node_id(n)).parent() {
            Some(parent) => self.element_children(parent.index() as u32).collect(),
            None => vec![n],
        };
        let hits: Vec<bool> = siblings
            .iter()
            .map(|&sib| self.any_matches(list, sib))
            .collect();
        let total = hits.iter().filter(|hit| **hit).count() as u32;
        let mut place = 0;
        let mut cache = self.of_places.borrow_mut();
        for (&sib, &hit) in siblings.iter().zip(&hits) {
            if hit {
                place += 1;
            }
            cache.insert((list_key, sib), (if hit { place } else { 0 }, total));
        }
        cache.get(&(list_key, n)).copied().unwrap_or((0, total))
    }

    // ----- states -----

    fn state(&self, state: State, n: u32, data: &NodeData, tag: &str) -> bool {
        match state {
            State::Root => self
                .doc
                .get(Self::node_id(n))
                .parent()
                .is_some_and(|p| matches!(self.doc.get(p).data(), NodeData::Document)),
            State::Scope => {
                let sr = self.scope_root.get();
                if sr != NONE {
                    n == sr
                } else {
                    self.doc
                        .get(Self::node_id(n))
                        .parent()
                        .is_some_and(|p| matches!(self.doc.get(p).data(), NodeData::Document))
                }
            }
            State::Empty => self.doc.children(Self::node_id(n)).iter().all(|&kid| {
                match self.doc.get(kid).data() {
                    NodeData::Text(text) => text.is_empty(),
                    NodeData::Element { .. } => false,
                    NodeData::Document => true,
                }
            }),
            State::FirstChild => self.place(n, false).0 == 1,
            State::LastChild => {
                let (place, count) = self.place(n, false);
                place == count
            }
            State::OnlyChild => self.place(n, false).1 == 1,
            State::FirstOfType => self.place(n, true).0 == 1,
            State::LastOfType => {
                let (place, count) = self.place(n, true);
                place == count
            }
            State::OnlyOfType => self.place(n, true).1 == 1,
            State::Link => matches!(tag, "a" | "area") && data.attr("href").is_some(),
            State::Checked => self.is_checked(n, data, tag),
            State::Default => self.is_default(n, data, tag),
            State::Disabled => is_form_control(tag) && self.is_disabled(n, data, tag),
            State::Enabled => is_form_control(tag) && !self.is_disabled(n, data, tag),
            State::Required => can_be_required(data, tag) && data.attr("required").is_some(),
            State::Optional => can_be_required(data, tag) && data.attr("required").is_none(),
            State::ReadWrite => self.is_read_write(n, data, tag),
            State::ReadOnly => !self.is_read_write(n, data, tag),
            State::PlaceholderShown => {
                let takes_placeholder = match tag {
                    "textarea" => true,
                    "input" => matches!(
                        input_type(data),
                        "text" | "search" | "url" | "tel" | "email" | "password" | "number"
                    ),
                    _ => false,
                };
                takes_placeholder
                    && data.attr("placeholder").is_some()
                    && self.doc.input_value(Self::node_id(n)).is_empty()
            }
            State::Defined => !tag.contains('-') || self.doc.is_custom_element_defined(tag),
            State::Host => self.doc.is_shadow_host(Self::node_id(n)),
            State::Open => matches!(tag, "details" | "dialog") && data.attr("open").is_some(),
            State::Closed => matches!(tag, "details" | "dialog") && data.attr("open").is_none(),
            State::Indeterminate => tag == "progress" && data.attr("value").is_none(),
            State::Hover => self.doc.is_hovered(Self::node_id(n)),
            State::Active => self.doc.is_active(Self::node_id(n)),
            State::Focus | State::FocusVisible => self.doc.is_focused(Self::node_id(n)),
            State::FocusWithin => self.doc.is_focus_within(Self::node_id(n)),
            State::Target => self.doc.is_target(Self::node_id(n)),
            State::TargetWithin => self.doc.is_target_within(Self::node_id(n)),
            State::Valid => self.doc.is_control_valid(Self::node_id(n)) == Some(true),
            State::Invalid => self.doc.is_control_valid(Self::node_id(n)) == Some(false),
            State::InRange => self.doc.is_control_in_range(Self::node_id(n)) == Some(true),
            State::OutOfRange => self.doc.is_control_in_range(Self::node_id(n)) == Some(false),
            State::Never => false,
        }
    }

    fn is_checked(&self, n: u32, data: &NodeData, tag: &str) -> bool {
        match tag {
            "input" => {
                matches!(input_type(data), "checkbox" | "radio")
                    && self.doc.is_checked(Self::node_id(n))
            }
            "option" => {
                if data.attr("selected").is_some() {
                    return true;
                }
                let select = self.owning_select(n);
                select != NONE && self.selected_option(select) == n
            }
            _ => false,
        }
    }

    /// The `select` an `option` belongs to, directly or through `optgroup`.
    fn owning_select(&self, option: u32) -> u32 {
        let mut p = self.parent_el(option);
        if p != NONE && self.tag(p) == Some("optgroup") {
            p = self.parent_el(p);
        }
        if p != NONE && self.tag(p) == Some("select") {
            p
        } else {
            NONE
        }
    }

    fn selected_option(&self, select: u32) -> u32 {
        if let Some(&known) = self.selected.borrow().get(&select) {
            return known;
        }
        let multiple = self.data(select).attr("multiple").is_some();
        let chosen = if multiple {
            NONE
        } else {
            self.doc
                .selected_option(Self::node_id(select))
                .map_or(NONE, |o| o.index() as u32)
        };
        self.selected.borrow_mut().insert(select, chosen);
        chosen
    }

    fn is_default(&self, n: u32, data: &NodeData, tag: &str) -> bool {
        match tag {
            "input" if matches!(input_type(data), "checkbox" | "radio") => {
                self.doc.is_checked(Self::node_id(n))
            }
            "option" => data.attr("selected").is_some(),
            "input" | "button" if self.is_submit(n, data, tag) => {
                let Some(form) = self.doc.enclosing_form(Self::node_id(n)) else {
                    return false;
                };
                self.default_button(form.index() as u32) == n
            }
            _ => false,
        }
    }

    fn is_submit(&self, n: u32, data: &NodeData, tag: &str) -> bool {
        (tag == "input" && input_type(data) == "image")
            || self.doc.is_submit_control(Self::node_id(n))
    }

    /// The first submit button inside `form`, in tree order.
    fn default_button(&self, form: u32) -> u32 {
        if let Some(&known) = self.default_button.borrow().get(&form) {
            return known;
        }
        let mut found = NONE;
        let mut stack: Vec<u32> = self.element_children(form).collect();
        stack.reverse();
        while let Some(n) = stack.pop() {
            let data = self.data(n);
            let tag = data.tag().unwrap_or("");
            if matches!(tag, "input" | "button") && self.is_submit(n, data, tag) {
                found = n;
                break;
            }
            let before = stack.len();
            stack.extend(self.element_children(n));
            stack[before..].reverse();
        }
        self.default_button.borrow_mut().insert(form, found);
        found
    }

    /// Disabled by its own attribute, by a disabled `optgroup` for an
    /// `option`, or by a disabled `fieldset` it is not in the first `legend`
    /// of.
    fn is_disabled(&self, n: u32, data: &NodeData, tag: &str) -> bool {
        if data.attr("disabled").is_some() {
            return true;
        }
        if tag == "option" {
            let p = self.parent_el(n);
            return p != NONE
                && self.tag(p) == Some("optgroup")
                && self.data(p).attr("disabled").is_some();
        }
        if tag == "optgroup" {
            return false;
        }
        let mut child = n;
        let mut cur = self.parent_el(n);
        while cur != NONE {
            if self.tag(cur) == Some("fieldset") && self.data(cur).attr("disabled").is_some() {
                let first_legend = self
                    .element_children(cur)
                    .find(|&kid| self.tag(kid) == Some("legend"));
                if first_legend != Some(child) {
                    return true;
                }
            }
            child = cur;
            cur = self.parent_el(cur);
        }
        false
    }

    fn is_read_write(&self, n: u32, data: &NodeData, tag: &str) -> bool {
        match tag {
            "input" => {
                matches!(
                    input_type(data),
                    "text"
                        | "search"
                        | "url"
                        | "tel"
                        | "email"
                        | "password"
                        | "date"
                        | "month"
                        | "week"
                        | "time"
                        | "datetime-local"
                        | "number"
                ) && data.attr("readonly").is_none()
                    && !self.is_disabled(n, data, tag)
            }
            "textarea" => data.attr("readonly").is_none() && !self.is_disabled(n, data, tag),
            _ => self.is_editable(n),
        }
    }

    /// `contenteditable` on the element or the nearest ancestor that sets it.
    fn is_editable(&self, n: u32) -> bool {
        let mut cur = n;
        while cur != NONE {
            if let Some(value) = self.data(cur).attr("contenteditable") {
                let value = value.trim();
                if value.is_empty()
                    || value.eq_ignore_ascii_case("true")
                    || value.eq_ignore_ascii_case("plaintext-only")
                {
                    return true;
                }
                if value.eq_ignore_ascii_case("false") {
                    return false;
                }
            }
            cur = self.parent_el(cur);
        }
        false
    }

    // ----- :lang() and :dir() -----

    /// The language from the nearest `xml:lang` or `lang`, if any.
    fn language(&self, n: u32) -> Option<&'d str> {
        let mut cur = n;
        while cur != NONE {
            let data = self.data(cur);
            if let Some(lang) = data.attr("xml:lang").or_else(|| data.attr("lang")) {
                return Some(lang);
            }
            cur = self.parent_el(cur);
        }
        None
    }

    fn lang(&self, ranges: &[String], n: u32) -> bool {
        let Some(lang) = self.language(n) else {
            return false;
        };
        ranges.iter().any(|range| lang_matches(range, lang))
    }

    /// Direction from the nearest `dir` of `ltr` or `rtl`. `auto` and a
    /// missing attribute read as left to right.
    fn is_rtl(&self, n: u32) -> bool {
        let mut cur = n;
        while cur != NONE {
            if let Some(dir) = self.data(cur).attr("dir") {
                let dir = dir.trim();
                if dir.eq_ignore_ascii_case("rtl") {
                    return true;
                }
                if dir.eq_ignore_ascii_case("ltr") || dir.eq_ignore_ascii_case("auto") {
                    return false;
                }
            }
            cur = self.parent_el(cur);
        }
        false
    }
}

/// True when `index` (1-based) is `a*n + b` for some `n >= 0`.
fn nth_hits(a: i64, b: i64, index: i64) -> bool {
    let diff = index - b;
    if a == 0 {
        return diff == 0;
    }
    diff % a == 0 && diff / a >= 0
}

fn is_form_control(tag: &str) -> bool {
    matches!(
        tag,
        "button" | "input" | "select" | "textarea" | "optgroup" | "option" | "fieldset"
    )
}

/// The `type` of an `input`, lowercase; an unknown or missing type is `text`.
fn input_type(data: &NodeData) -> &'static str {
    const TYPES: &[&str] = &[
        "hidden",
        "text",
        "search",
        "tel",
        "url",
        "email",
        "password",
        "date",
        "month",
        "week",
        "time",
        "datetime-local",
        "number",
        "range",
        "color",
        "checkbox",
        "radio",
        "file",
        "submit",
        "image",
        "reset",
        "button",
    ];
    let Some(kind) = data.attr("type") else {
        return "text";
    };
    let kind = kind.trim();
    TYPES
        .iter()
        .find(|t| t.eq_ignore_ascii_case(kind))
        .copied()
        .unwrap_or("text")
}

/// `input`, `select`, and `textarea` that `required` applies to.
fn can_be_required(data: &NodeData, tag: &str) -> bool {
    match tag {
        "select" | "textarea" => true,
        "input" => !matches!(
            input_type(data),
            "hidden" | "range" | "color" | "submit" | "image" | "reset" | "button"
        ),
        _ => false,
    }
}

fn attr_matches(sel: &AttrSelector, data: &NodeData) -> bool {
    if !sel.resolvable {
        return false;
    }
    let Some(value) = data.attr(&sel.name) else {
        return false;
    };
    let fold = match sel.case {
        CaseFlag::Insensitive => true,
        CaseFlag::Sensitive => false,
        CaseFlag::Default => CASE_INSENSITIVE_VALUES.contains(&sel.name.as_str()),
    };
    let want = sel.value.as_str();
    let eq = |x: &str, y: &str| {
        if fold {
            x.eq_ignore_ascii_case(y)
        } else {
            x == y
        }
    };
    match sel.op {
        AttrOp::Exists => true,
        AttrOp::Equals => eq(value, want),
        AttrOp::Includes => {
            !want.is_empty()
                && !want.contains(is_ws)
                && value.split(is_ws).any(|word| eq(word, want))
        }
        AttrOp::DashMatch => {
            eq(value, want)
                || (value.len() > want.len()
                    && value.as_bytes()[want.len()] == b'-'
                    && value.get(..want.len()).is_some_and(|head| eq(head, want)))
        }
        AttrOp::Prefix => {
            !want.is_empty() && value.get(..want.len()).is_some_and(|head| eq(head, want))
        }
        AttrOp::Suffix => {
            !want.is_empty()
                && value.len() >= want.len()
                && value
                    .get(value.len() - want.len()..)
                    .is_some_and(|tail| eq(tail, want))
        }
        AttrOp::Substring => {
            if want.is_empty() {
                false
            } else if fold {
                value
                    .to_ascii_lowercase()
                    .contains(&want.to_ascii_lowercase())
            } else {
                value.contains(want)
            }
        }
    }
}

/// Extended language-range matching, where a range also matches longer
/// tags (`en` matches `en-US`). `*` matches any language.
fn lang_matches(range: &str, lang: &str) -> bool {
    if range.is_empty() {
        return lang.is_empty();
    }
    if lang.is_empty() {
        return false;
    }
    let mut want = range.split('-');
    let mut have = lang.split('-');
    let (Some(first_want), Some(first_have)) = (want.next(), have.next()) else {
        return false;
    };
    if first_want != "*" && !first_want.eq_ignore_ascii_case(first_have) {
        return false;
    }
    let mut have = have.peekable();
    for sub in want {
        if sub == "*" {
            continue;
        }
        loop {
            let Some(next) = have.next() else {
                return false;
            };
            if next.eq_ignore_ascii_case(sub) {
                break;
            }
            if next.len() == 1 {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_plus_b_forms() {
        let ok = [
            ("odd", (2, 1)),
            ("EVEN", (2, 0)),
            ("7", (0, 7)),
            ("-7", (0, -7)),
            ("+7", (0, 7)),
            ("n", (1, 0)),
            ("-n", (-1, 0)),
            ("+n", (1, 0)),
            ("2n", (2, 0)),
            ("2n+1", (2, 1)),
            ("2n + 1", (2, 1)),
            ("2n- 1", (2, -1)),
            ("2n -1", (2, -1)),
            ("-n+3", (-1, 3)),
            (" 3N-2 ", (3, -2)),
            ("0n+0", (0, 0)),
        ];
        for (text, want) in ok {
            assert_eq!(parse_an_plus_b(text), Some(want), "{text}");
        }
        for bad in [
            "", "+ 2", "- n", "2 n", "n 2", "2n+", "2n + + 1", "2n+-1", "2n1", "1.5", "nn", "o",
            "2n of", "n-",
        ] {
            assert_eq!(parse_an_plus_b(bad), None, "{bad}");
        }
        assert_eq!(
            parse_an_plus_b("99999999999999999999999n"),
            Some((1 << 40, 0))
        );
    }

    #[test]
    fn nth_positions() {
        assert!(nth_hits(2, 1, 1));
        assert!(!nth_hits(2, 1, 2));
        assert!(nth_hits(-1, 3, 3));
        assert!(!nth_hits(-1, 3, 4));
        assert!(nth_hits(0, 4, 4));
        assert!(!nth_hits(0, 4, 5));
        assert!(!nth_hits(3, 5, 2));
    }

    #[test]
    fn language_ranges() {
        assert!(lang_matches("en", "en"));
        assert!(lang_matches("en", "EN-us"));
        assert!(!lang_matches("en", "eng"));
        assert!(lang_matches("*", "fr"));
        assert!(lang_matches("*-CH", "de-CH"));
        assert!(lang_matches("de-DE", "de-Latn-DE"));
        assert!(!lang_matches("de-DE", "de-x-DE"));
        assert!(!lang_matches("de-DE", "de"));
        assert!(!lang_matches("fr", ""));
        assert!(lang_matches("", ""));
    }

    #[test]
    fn escapes_decode() {
        let chars: Vec<char> = "\\31 0x \\0 \\110000 \\41\r\nB\\".chars().collect();
        let mut cur = Cursor {
            chars: &chars,
            pos: 0,
        };
        assert_eq!(cur.read_ident().as_deref(), Some("10x"));
        cur.skip_ws();
        // One white space after hex digits belongs to the escape, so these
        // all run together. Zero and values past U+10FFFF become U+FFFD.
        assert_eq!(
            cur.read_ident().as_deref(),
            Some("\u{fffd}\u{fffd}AB\u{fffd}")
        );
        assert!(cur.at_end());
    }

    #[test]
    fn list_split_respects_nesting() {
        let chars: Vec<char> = "a, :is(b, c), [x=','], 'd,e', \\,f".chars().collect();
        let parts: Vec<String> = split_list(&chars)
            .into_iter()
            .map(|p| p.iter().collect::<String>().trim().to_string())
            .collect();
        assert_eq!(parts, vec!["a", ":is(b, c)", "[x=',']", "'d,e'", "\\,f"]);
    }
}
