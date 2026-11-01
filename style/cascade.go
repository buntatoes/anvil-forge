// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package style

import (
	"math"
	"sort"
	"strconv"
	"strings"
	"sync"

	"github.com/buntatoes/anvil-engine/dom"
)

type ComputedStyle struct {
	Display            Display
	Position           Position
	Top                Size
	Right              Size
	Bottom             Size
	Left               Size
	Width              Size
	Height             Size
	MinWidth           Size
	MinHeight          Size
	MaxWidth           Size
	MaxHeight          Size
	Margin             Edge
	MarginAuto         [4]bool
	Padding            Edge
	Border             Edge
	BorderColor        Color
	BorderColors       [4]Color
	BorderRadius       float32
	OutlineWidth       float32
	OutlineColor       Color
	OutlineOffset      float32
	BoxSizing          BoxSizing
	Overflow           Overflow
	ScrollableOverflow bool
	ObjectFit          ObjectFit
	Visibility         Visibility
	ZIndex             int32
	Opacity            float32
	Color              Color
	Background         *Color
	BackgroundGradient *LinearGradient
	BoxShadow          *BoxShadow
	FontSize           float32
	LineHeight         float32
	FontWeight         uint16
	Italic             bool
	FontFamily         FontFamily
	TextAlign          TextAlign
	TextTransform      TextTransform
	WhiteSpace         WhiteSpace
	Underline          bool
	LineThrough        bool
	LetterSpacing      float32
	WordSpacing        float32
	TextOverflowEllip  bool
	BreakWord          bool
	AspectRatio        float32
	FlexDirection      FlexDirection
	FlexWrap           FlexWrap
	FlexGrow           float32
	FlexShrink         float32
	FlexBasis          Size
	Order              int32
	JustifyContent     JustifyContent
	AlignItems         AlignItems
	AlignSelf          *AlignItems
	AlignContent       AlignContent
	RowGap             float32
	ColumnGap          float32
	GridTemplateCols   []GridTrack
	GridTemplateRows   []GridTrack
	GridAutoFlow       GridAutoFlow
	Transforms         []TransformFn
	BeforeContent      string
	AfterContent       string
	CustomProps        map[string]string
}

func DefaultComputedStyle() ComputedStyle {
	black := RGBA(0, 0, 0, 255)
	return ComputedStyle{
		Display:      DisplayBlock,
		Position:     PositionStatic,
		Top:          Size{Kind: SizeAuto},
		Right:        Size{Kind: SizeAuto},
		Bottom:       Size{Kind: SizeAuto},
		Left:         Size{Kind: SizeAuto},
		Width:        Size{Kind: SizeAuto},
		Height:       Size{Kind: SizeAuto},
		MinWidth:     Size{Kind: SizeAuto},
		MinHeight:    Size{Kind: SizeAuto},
		MaxWidth:     Size{Kind: SizeAuto},
		MaxHeight:    Size{Kind: SizeAuto},
		BorderColor:  black,
		BorderColors: [4]Color{black, black, black, black},
		OutlineColor: black,
		BoxSizing:    BoxSizingContentBox,
		Overflow:     OverflowVisible,
		ObjectFit:    ObjectFitFill,
		Visibility:   VisibilityVisible,
		Opacity:      1.0,
		Color:        RGBA(17, 17, 17, 255),
		FontSize:     16.0,
		LineHeight:   20.0,
		FontWeight:   400,
		FontFamily:   FontSansSerif,
		TextAlign:    TextAlignLeft,
		WhiteSpace:   WhiteSpaceNormal,
		FlexShrink:   1.0,
		FlexBasis:    Size{Kind: SizeAuto},
		AlignItems:   AlignStretch,
		AlignContent: AlignContentStretch,
		CustomProps:  make(map[string]string),
	}
}

type StyleMap struct {
	Styles     map[dom.NodeId]ComputedStyle
	RootFontPx float32
	ViewportW  uint32
	ViewportH  uint32
}

func (sm *StyleMap) Get(id dom.NodeId) (ComputedStyle, bool) {
	st, ok := sm.Styles[id]
	return st, ok
}

type Animator struct {
	NowMs uint64
}

func NewAnimator() *Animator {
	return &Animator{}
}

func (a *Animator) Tick(nowMs uint64) bool {
	a.NowMs = nowMs
	return false
}

func ComputeStyles(doc *dom.Document, sheet *Stylesheet) *StyleMap {
	return ComputeStylesWithViewport(doc, sheet, 1280, 720)
}

func ComputeStylesWithViewport(doc *dom.Document, sheet *Stylesheet, vw, vh uint32) *StyleMap {
	sm := &StyleMap{
		Styles:     make(map[dom.NodeId]ComputedStyle, doc.NodeCount()),
		RootFontPx: 16.0,
		ViewportW:  vw,
		ViewportH:  vh,
	}
	var walk func(dom.NodeId, *ComputedStyle)
	walk = func(id dom.NodeId, parent *ComputedStyle) {
		n := doc.Node(id)
		if n == nil {
			return
		}
		if n.Data.Kind == dom.NodeKindElement {
			st := resolveElementStyle(doc, id, parent, sheet, sm.RootFontPx, float32(vw), float32(vh))
			if id == doc.HTML() {
				sm.RootFontPx = st.FontSize
			}
			sm.Styles[id] = st
			for _, c := range doc.ComposedChildren(id) {
				walk(c, &st)
			}
		} else {
			for _, c := range n.Children {
				walk(c, parent)
			}
		}
	}
	walk(doc.Root(), nil)
	return sm
}

func ComputeStylesIncremental(doc *dom.Document, sheet *Stylesheet, prev *StyleMap, vw, vh uint32) *StyleMap {
	dirty := doc.TakeDirtyStyleNodes()
	if prev == nil || len(dirty) == 0 {
		return ComputeStylesWithViewport(doc, sheet, vw, vh)
	}
	return ComputeStylesWithViewport(doc, sheet, vw, vh)
}

func ComputeStylesParallel(doc *dom.Document, sheet *Stylesheet, vw, vh uint32) *StyleMap {
	var wg sync.WaitGroup
	var out *StyleMap
	wg.Add(1)
	go func() {
		defer wg.Done()
		out = ComputeStylesWithViewport(doc, sheet, vw, vh)
	}()
	wg.Wait()
	return out
}

func ComputeStylesAnimated(doc *dom.Document, sheet *Stylesheet, anim *Animator, vw, vh uint32) *StyleMap {
	if anim != nil {
		_ = anim.NowMs
	}
	return ComputeStylesWithViewport(doc, sheet, vw, vh)
}

type matchedDecl struct {
	decl        Declaration
	layerOrder  int
	scopeHops   int
	specificity Specificity
	sourceOrder int
	isInline    bool
}

func resolveElementStyle(doc *dom.Document, id dom.NodeId, parent *ComputedStyle, sheet *Stylesheet, rootFont, vw, vh float32) ComputedStyle {
	st := DefaultComputedStyle()
	if parent != nil {
		st.Color = parent.Color
		st.FontSize = parent.FontSize
		st.LineHeight = parent.LineHeight
		st.FontWeight = parent.FontWeight
		st.Italic = parent.Italic
		st.FontFamily = parent.FontFamily
		st.TextAlign = parent.TextAlign
		st.TextTransform = parent.TextTransform
		st.WhiteSpace = parent.WhiteSpace
		st.Visibility = parent.Visibility
		st.LetterSpacing = parent.LetterSpacing
		st.WordSpacing = parent.WordSpacing
		for k, v := range parent.CustomProps {
			st.CustomProps[k] = v
		}
	}

	n := doc.Node(id)
	if n == nil {
		return st
	}
	applyUserAgentDefaults(&st, n)

	var matches []matchedDecl
	orderCounter := 0
	if sheet != nil {
		layerIdxMap := make(map[string]int, len(sheet.Layers))
		for idx, l := range sheet.Layers {
			layerIdxMap[l] = idx + 1
		}
		for _, rule := range sheet.Rules {
			if rule.Media != "" && !matchesMediaQuery(rule.Media, uint32(vw), uint32(vh)) {
				continue
			}
			hops, inScope := checkScopeProximity(doc, id, rule.ScopeRoot, rule.ScopeLimit)
			if !inScope {
				continue
			}
			var bestSpec Specificity
			matched := false
			for _, sel := range rule.Selectors {
				// Handle ::before and ::after rules separately
				lowerSel := strings.ToLower(sel)
				if strings.HasSuffix(lowerSel, "::before") {
					baseSel := sel[:len(sel)-8]
					if baseSel == "" || doc.MatchesSelector(id, baseSel) {
						for _, d := range rule.Declarations {
							if d.Property == "content" {
								st.BeforeContent = evalContentValue(&n.Data, d.Value)
							}
						}
					}
					continue
				}
				if strings.HasSuffix(lowerSel, "::after") {
					baseSel := sel[:len(sel)-7]
					if baseSel == "" || doc.MatchesSelector(id, baseSel) {
						for _, d := range rule.Declarations {
							if d.Property == "content" {
								st.AfterContent = evalContentValue(&n.Data, d.Value)
							}
						}
					}
					continue
				}
				if doc.MatchesSelector(id, sel) {
					sp := ComputeSpecificity(sel)
					if !matched || bestSpec.Less(sp) {
						bestSpec = sp
						matched = true
					}
				}
			}
			if !matched {
				continue
			}
			lOrder := len(sheet.Layers) + 1 // unlayered beats layered for normal rules
			if rule.Layer != "" {
				if idx, ok := layerIdxMap[rule.Layer]; ok {
					lOrder = idx
				}
			}
			for _, d := range rule.Declarations {
				orderCounter++
				matches = append(matches, matchedDecl{
					decl:        d,
					layerOrder:  lOrder,
					scopeHops:   hops,
					specificity: bestSpec,
					sourceOrder: orderCounter,
				})
			}
		}
	}

	if inlineStyle, ok := n.Data.Attr("style"); ok && strings.TrimSpace(inlineStyle) != "" {
		for _, d := range parseDeclarationsOnly(inlineStyle) {
			orderCounter++
			matches = append(matches, matchedDecl{
				decl:        d,
				layerOrder:  999999,
				scopeHops:   0,
				specificity: Specificity{IDs: 1000},
				sourceOrder: orderCounter,
				isInline:    true,
			})
		}
	}

	sort.SliceStable(matches, func(i, j int) bool {
		a, b := matches[i], matches[j]
		if a.decl.Important != b.decl.Important {
			return !a.decl.Important && b.decl.Important
		}
		if a.isInline != b.isInline {
			return !a.isInline && b.isInline
		}
		if a.layerOrder != b.layerOrder {
			if a.decl.Important {
				return a.layerOrder > b.layerOrder
			}
			return a.layerOrder < b.layerOrder
		}
		if a.scopeHops != b.scopeHops {
			return a.scopeHops > b.scopeHops
		}
		if a.specificity != b.specificity {
			return a.specificity.Less(b.specificity)
		}
		return a.sourceOrder < b.sourceOrder
	})

	// Pass 1: custom properties (--*)
	for _, m := range matches {
		if strings.HasPrefix(m.decl.Property, "--") {
			st.CustomProps[m.decl.Property] = m.decl.Value
		}
	}
	// Pass 2: font-size first so em units resolve against updated font-size
	parentFont := float32(16.0)
	if parent != nil {
		parentFont = parent.FontSize
	}
	for _, m := range matches {
		if m.decl.Property == "font-size" {
			val := resolveCustomVars(m.decl.Value, st.CustomProps, 0)
			if sz := parseSizeValue(val, parentFont, rootFont, vw, vh); sz.Kind != SizeAuto {
				if px, ok := sz.ResolvePx(parentFont, parentFont, rootFont, vw, vh); ok && px > 0 {
					st.FontSize = px
					st.LineHeight = px * 1.25
				}
			}
		}
	}
	// Pass 3: all other declarations
	for _, m := range matches {
		if strings.HasPrefix(m.decl.Property, "--") || m.decl.Property == "font-size" {
			continue
		}
		val := resolveCustomVars(m.decl.Value, st.CustomProps, 0)
		applyDeclaration(&st, parent, m.decl.Property, val, st.FontSize, rootFont, vw, vh)
	}
	return st
}

func applyUserAgentDefaults(st *ComputedStyle, n *dom.Node) {
	switch n.Data.Tag {
	case "head", "style", "script", "title", "meta", "link", "template", "noscript":
		st.Display = DisplayNone
	case "span", "a", "b", "strong", "i", "em", "u", "s", "code", "small", "label":
		st.Display = DisplayInline
		if n.Data.Tag == "a" {
			st.Color = RGBA(0, 102, 204, 255)
			st.Underline = true
		} else if n.Data.Tag == "b" || n.Data.Tag == "strong" {
			st.FontWeight = 700
		} else if n.Data.Tag == "i" || n.Data.Tag == "em" {
			st.Italic = true
		} else if n.Data.Tag == "u" {
			st.Underline = true
		} else if n.Data.Tag == "s" {
			st.LineThrough = true
		} else if n.Data.Tag == "code" {
			st.FontFamily = FontMonospace
		}
	case "button", "input", "select", "textarea", "img", "video", "canvas", "progress", "meter":
		st.Display = DisplayInlineBlock
	case "h1":
		st.FontSize = 32
		st.LineHeight = 40
		st.FontWeight = 700
	case "h2":
		st.FontSize = 24
		st.LineHeight = 30
		st.FontWeight = 700
	case "h3":
		st.FontSize = 19
		st.LineHeight = 24
		st.FontWeight = 700
	case "pre":
		st.FontFamily = FontMonospace
		st.WhiteSpace = WhiteSpacePre
	case "ul", "ol":
		st.Padding.Left = 24
	case "li":
		st.Display = DisplayListItem
	case "table":
		st.Display = DisplayTable
	case "thead":
		st.Display = DisplayTableHeaderGroup
	case "tbody":
		st.Display = DisplayTableRowGroup
	case "tfoot":
		st.Display = DisplayTableFooterGroup
	case "tr":
		st.Display = DisplayTableRow
	case "td", "th":
		st.Display = DisplayTableCell
		if n.Data.Tag == "th" {
			st.FontWeight = 700
		}
	case "dialog":
		if _, open := n.Data.Attr("open"); !open {
			st.Display = DisplayNone
		}
	}
	if _, hidden := n.Data.Attr("hidden"); hidden {
		st.Display = DisplayNone
	}
}

func applyDeclaration(st *ComputedStyle, parent *ComputedStyle, prop, rawVal string, fontSize, rootFont, vw, vh float32) {
	val := strings.TrimSpace(rawVal)
	lower := strings.ToLower(val)
	if lower == "initial" || lower == "unset" || lower == "revert" || lower == "revert-layer" {
		def := DefaultComputedStyle()
		switch prop {
		case "color":
			if lower == "unset" && parent != nil {
				st.Color = parent.Color
			} else {
				st.Color = def.Color
			}
		case "background", "background-color":
			st.Background = nil
			st.BackgroundGradient = nil
		case "display":
			st.Display = def.Display
		}
		return
	}
	if lower == "inherit" && parent != nil {
		switch prop {
		case "color":
			st.Color = parent.Color
		case "background-color":
			st.Background = parent.Background
		case "font-size":
			st.FontSize = parent.FontSize
		}
		return
	}

	switch prop {
	case "display":
		switch lower {
		case "none":
			st.Display = DisplayNone
		case "block":
			st.Display = DisplayBlock
		case "inline":
			st.Display = DisplayInline
		case "inline-block":
			st.Display = DisplayInlineBlock
		case "list-item":
			st.Display = DisplayListItem
		case "flow-root":
			st.Display = DisplayFlowRoot
		case "flex":
			st.Display = DisplayFlex
		case "inline-flex":
			st.Display = DisplayInlineFlex
		case "grid":
			st.Display = DisplayGrid
		case "inline-grid":
			st.Display = DisplayInlineGrid
		case "table":
			st.Display = DisplayTable
		case "table-row":
			st.Display = DisplayTableRow
		case "table-cell":
			st.Display = DisplayTableCell
		case "contents":
			st.Display = DisplayContents
		}
	case "position":
		switch lower {
		case "static":
			st.Position = PositionStatic
		case "relative":
			st.Position = PositionRelative
		case "absolute":
			st.Position = PositionAbsolute
		case "fixed":
			st.Position = PositionFixed
		case "sticky":
			st.Position = PositionSticky
		}
	case "top":
		st.Top = parseSizeValue(val, fontSize, rootFont, vw, vh)
	case "right":
		st.Right = parseSizeValue(val, fontSize, rootFont, vw, vh)
	case "bottom":
		st.Bottom = parseSizeValue(val, fontSize, rootFont, vw, vh)
	case "left":
		st.Left = parseSizeValue(val, fontSize, rootFont, vw, vh)
	case "inset":
		sz := parseSizeValue(val, fontSize, rootFont, vw, vh)
		st.Top, st.Right, st.Bottom, st.Left = sz, sz, sz, sz
	case "width", "inline-size":
		st.Width = parseSizeValue(val, fontSize, rootFont, vw, vh)
	case "height", "block-size":
		st.Height = parseSizeValue(val, fontSize, rootFont, vw, vh)
	case "min-width", "min-inline-size":
		st.MinWidth = parseSizeValue(val, fontSize, rootFont, vw, vh)
	case "min-height", "min-block-size":
		st.MinHeight = parseSizeValue(val, fontSize, rootFont, vw, vh)
	case "max-width", "max-inline-size":
		st.MaxWidth = parseSizeValue(val, fontSize, rootFont, vw, vh)
	case "max-height", "max-block-size":
		st.MaxHeight = parseSizeValue(val, fontSize, rootFont, vw, vh)
	case "box-sizing":
		if lower == "border-box" {
			st.BoxSizing = BoxSizingBorderBox
		} else if lower == "content-box" {
			st.BoxSizing = BoxSizingContentBox
		}
	case "margin":
		e, autos := parseBoxEdgeShorthand(val, fontSize, rootFont, vw, vh)
		st.Margin = e
		st.MarginAuto = autos
	case "margin-top", "margin-block-start":
		if lower == "auto" {
			st.MarginAuto[0] = true
		} else {
			st.Margin.Top = EvalMathPx(val, fontSize, rootFont, vw, vh)
		}
	case "margin-right", "margin-inline-end":
		if lower == "auto" {
			st.MarginAuto[1] = true
		} else {
			st.Margin.Right = EvalMathPx(val, fontSize, rootFont, vw, vh)
		}
	case "margin-bottom", "margin-block-end":
		if lower == "auto" {
			st.MarginAuto[2] = true
		} else {
			st.Margin.Bottom = EvalMathPx(val, fontSize, rootFont, vw, vh)
		}
	case "margin-left", "margin-inline-start":
		if lower == "auto" {
			st.MarginAuto[3] = true
		} else {
			st.Margin.Left = EvalMathPx(val, fontSize, rootFont, vw, vh)
		}
	case "padding":
		e, _ := parseBoxEdgeShorthand(val, fontSize, rootFont, vw, vh)
		st.Padding = e
	case "padding-top", "padding-block-start":
		st.Padding.Top = EvalMathPx(val, fontSize, rootFont, vw, vh)
	case "padding-right", "padding-inline-end":
		st.Padding.Right = EvalMathPx(val, fontSize, rootFont, vw, vh)
	case "padding-bottom", "padding-block-end":
		st.Padding.Bottom = EvalMathPx(val, fontSize, rootFont, vw, vh)
	case "padding-left", "padding-inline-start":
		st.Padding.Left = EvalMathPx(val, fontSize, rootFont, vw, vh)
	case "border", "border-width":
		w, col, hasCol := parseBorderShorthand(val, fontSize, rootFont, vw, vh)
		st.Border = Edge{Top: w, Right: w, Bottom: w, Left: w}
		if hasCol {
			st.BorderColor = col
			st.BorderColors = [4]Color{col, col, col, col}
		}
	case "border-color":
		if c, ok := ParseColor(val); ok {
			st.BorderColor = c
			st.BorderColors = [4]Color{c, c, c, c}
		}
	case "border-top-color":
		if c, ok := ParseColor(val); ok {
			st.BorderColors[0] = c
		}
	case "border-right-color":
		if c, ok := ParseColor(val); ok {
			st.BorderColors[1] = c
		}
	case "border-bottom-color":
		if c, ok := ParseColor(val); ok {
			st.BorderColors[2] = c
		}
	case "border-left-color":
		if c, ok := ParseColor(val); ok {
			st.BorderColors[3] = c
		}
	case "border-radius":
		st.BorderRadius = EvalMathPx(val, fontSize, rootFont, vw, vh)
	case "outline":
		w, col, hasCol := parseBorderShorthand(val, fontSize, rootFont, vw, vh)
		st.OutlineWidth = w
		if hasCol {
			st.OutlineColor = col
		}
	case "outline-offset":
		st.OutlineOffset = EvalMathPx(val, fontSize, rootFont, vw, vh)
	case "color":
		if c, ok := ParseColor(val); ok {
			st.Color = c
		}
	case "background", "background-color", "background-image":
		if strings.Contains(lower, "linear-gradient(") {
			if lg, ok := parseLinearGradient(val); ok {
				st.BackgroundGradient = &lg
			}
		} else if c, ok := ParseColor(val); ok {
			st.Background = &c
		}
	case "box-shadow":
		if bs, ok := parseBoxShadow(val, fontSize, rootFont, vw, vh); ok {
			st.BoxShadow = &bs
		}
	case "opacity":
		if v, err := strconv.ParseFloat(strings.TrimSuffix(lower, "%"), 32); err == nil {
			if strings.HasSuffix(lower, "%") {
				v /= 100.0
			}
			if v < 0 {
				v = 0
			} else if v > 1 {
				v = 1
			}
			st.Opacity = float32(v)
		}
	case "z-index":
		if v, err := strconv.Atoi(lower); err == nil {
			st.ZIndex = int32(v)
		}
	case "overflow", "overflow-y", "overflow-x":
		switch lower {
		case "hidden", "clip":
			st.Overflow = OverflowHidden
		case "auto":
			st.Overflow = OverflowAuto
			st.ScrollableOverflow = true
		case "scroll":
			st.Overflow = OverflowScroll
			st.ScrollableOverflow = true
		case "visible":
			st.Overflow = OverflowVisible
		}
	case "visibility":
		if lower == "hidden" || lower == "collapse" {
			st.Visibility = VisibilityHidden
		} else {
			st.Visibility = VisibilityVisible
		}
	case "object-fit":
		switch lower {
		case "contain":
			st.ObjectFit = ObjectFitContain
		case "cover":
			st.ObjectFit = ObjectFitCover
		case "none":
			st.ObjectFit = ObjectFitNone
		case "scale-down":
			st.ObjectFit = ObjectFitScaleDown
		default:
			st.ObjectFit = ObjectFitFill
		}
	case "line-height":
		if v, err := strconv.ParseFloat(lower, 32); err == nil {
			st.LineHeight = st.FontSize * float32(v)
		} else {
			st.LineHeight = EvalMathPx(val, fontSize, rootFont, vw, vh)
		}
	case "font-weight":
		if lower == "bold" || lower == "bolder" {
			st.FontWeight = 700
		} else if lower == "normal" || lower == "lighter" {
			st.FontWeight = 400
		} else if v, err := strconv.Atoi(lower); err == nil {
			st.FontWeight = uint16(v)
		}
	case "font-style":
		st.Italic = strings.Contains(lower, "italic") || strings.Contains(lower, "oblique")
	case "font-family":
		if strings.Contains(lower, "mono") || strings.Contains(lower, "consolas") || strings.Contains(lower, "courier") {
			st.FontFamily = FontMonospace
		} else if strings.Contains(lower, "serif") && !strings.Contains(lower, "sans-serif") {
			st.FontFamily = FontSerif
		} else {
			st.FontFamily = FontSansSerif
		}
	case "text-align":
		switch lower {
		case "center":
			st.TextAlign = TextAlignCenter
		case "right", "end":
			st.TextAlign = TextAlignRight
		case "justify":
			st.TextAlign = TextAlignJustify
		default:
			st.TextAlign = TextAlignLeft
		}
	case "text-transform":
		switch lower {
		case "uppercase":
			st.TextTransform = TextTransformUppercase
		case "lowercase":
			st.TextTransform = TextTransformLowercase
		case "capitalize":
			st.TextTransform = TextTransformCapitalize
		default:
			st.TextTransform = TextTransformNone
		}
	case "text-decoration", "text-decoration-line":
		st.Underline = strings.Contains(lower, "underline")
		st.LineThrough = strings.Contains(lower, "line-through")
	case "letter-spacing":
		st.LetterSpacing = EvalMathPx(val, fontSize, rootFont, vw, vh)
	case "word-spacing":
		st.WordSpacing = EvalMathPx(val, fontSize, rootFont, vw, vh)
	case "white-space":
		switch lower {
		case "pre":
			st.WhiteSpace = WhiteSpacePre
		case "nowrap":
			st.WhiteSpace = WhiteSpaceNoWrap
		case "pre-wrap":
			st.WhiteSpace = WhiteSpacePreWrap
		default:
			st.WhiteSpace = WhiteSpaceNormal
		}
	case "text-overflow":
		st.TextOverflowEllip = lower == "ellipsis"
	case "overflow-wrap", "word-break":
		st.BreakWord = strings.Contains(lower, "break")
	case "flex-direction":
		switch lower {
		case "row-reverse":
			st.FlexDirection = FlexRowReverse
		case "column":
			st.FlexDirection = FlexColumn
		case "column-reverse":
			st.FlexDirection = FlexColumnReverse
		default:
			st.FlexDirection = FlexRow
		}
	case "flex-wrap":
		switch lower {
		case "wrap":
			st.FlexWrap = FlexWrapNormal
		case "wrap-reverse":
			st.FlexWrap = FlexWrapReverse
		default:
			st.FlexWrap = FlexNoWrap
		}
	case "flex-grow":
		if v, err := strconv.ParseFloat(lower, 32); err == nil {
			st.FlexGrow = float32(v)
		}
	case "flex-shrink":
		if v, err := strconv.ParseFloat(lower, 32); err == nil {
			st.FlexShrink = float32(v)
		}
	case "flex-basis":
		st.FlexBasis = parseSizeValue(val, fontSize, rootFont, vw, vh)
	case "flex":
		fields := strings.Fields(lower)
		if len(fields) >= 1 {
			if fields[0] == "none" {
				st.FlexGrow = 0
				st.FlexShrink = 0
			} else if v, err := strconv.ParseFloat(fields[0], 32); err == nil {
				st.FlexGrow = float32(v)
				st.FlexShrink = 1
				st.FlexBasis = Size{Kind: SizePx, Value: 0}
			}
		}
		if len(fields) >= 2 {
			if v, err := strconv.ParseFloat(fields[1], 32); err == nil {
				st.FlexShrink = float32(v)
			}
		}
		if len(fields) >= 3 {
			st.FlexBasis = parseSizeValue(fields[2], fontSize, rootFont, vw, vh)
		}
	case "justify-content":
		switch lower {
		case "center":
			st.JustifyContent = JustifyCenter
		case "flex-end", "end", "right":
			st.JustifyContent = JustifyFlexEnd
		case "space-between":
			st.JustifyContent = JustifySpaceBetween
		case "space-around":
			st.JustifyContent = JustifySpaceAround
		case "space-evenly":
			st.JustifyContent = JustifySpaceEvenly
		default:
			st.JustifyContent = JustifyFlexStart
		}
	case "align-items":
		st.AlignItems = parseAlignItems(lower)
	case "align-self":
		ai := parseAlignItems(lower)
		st.AlignSelf = &ai
	case "gap", "grid-gap":
		fields := strings.Fields(val)
		if len(fields) == 1 {
			g := EvalMathPx(fields[0], fontSize, rootFont, vw, vh)
			st.RowGap = g
			st.ColumnGap = g
		} else if len(fields) >= 2 {
			st.RowGap = EvalMathPx(fields[0], fontSize, rootFont, vw, vh)
			st.ColumnGap = EvalMathPx(fields[1], fontSize, rootFont, vw, vh)
		}
	case "row-gap", "grid-row-gap":
		st.RowGap = EvalMathPx(val, fontSize, rootFont, vw, vh)
	case "column-gap", "grid-column-gap":
		st.ColumnGap = EvalMathPx(val, fontSize, rootFont, vw, vh)
	case "grid-template-columns":
		st.GridTemplateCols = parseGridTracks(val, fontSize, rootFont, vw, vh)
	case "grid-template-rows":
		st.GridTemplateRows = parseGridTracks(val, fontSize, rootFont, vw, vh)
	}
}

func parseAlignItems(lower string) AlignItems {
	switch lower {
	case "flex-start", "start":
		return AlignFlexStart
	case "center":
		return AlignCenter
	case "flex-end", "end":
		return AlignFlexEnd
	case "baseline":
		return AlignBaseline
	default:
		return AlignStretch
	}
}

func parseGridTracks(raw string, fontSize, rootFont, vw, vh float32) []GridTrack {
	var out []GridTrack
	for _, tok := range strings.Fields(strings.ToLower(raw)) {
		if strings.HasSuffix(tok, "fr") {
			v, _ := strconv.ParseFloat(strings.TrimSuffix(tok, "fr"), 32)
			if v <= 0 {
				v = 1
			}
			out = append(out, GridTrack{Kind: SizeAuto, Fr: float32(v)})
		} else {
			sz := parseSizeValue(tok, fontSize, rootFont, vw, vh)
			if sz.Kind == SizeAuto {
				out = append(out, GridTrack{Kind: SizeAuto, Fr: 1})
			} else {
				out = append(out, GridTrack{Kind: sz.Kind, Value: sz.Value})
			}
		}
	}
	return out
}

func parseBoxEdgeShorthand(raw string, fontSize, rootFont, vw, vh float32) (Edge, [4]bool) {
	fields := strings.Fields(raw)
	var vals [4]float32
	var autos [4]bool
	parseOne := func(tok string) (float32, bool) {
		if strings.EqualFold(tok, "auto") {
			return 0, true
		}
		return EvalMathPx(tok, fontSize, rootFont, vw, vh), false
	}
	switch len(fields) {
	case 1:
		v, a := parseOne(fields[0])
		vals = [4]float32{v, v, v, v}
		autos = [4]bool{a, a, a, a}
	case 2:
		v0, a0 := parseOne(fields[0])
		v1, a1 := parseOne(fields[1])
		vals = [4]float32{v0, v1, v0, v1}
		autos = [4]bool{a0, a1, a0, a1}
	case 3:
		v0, a0 := parseOne(fields[0])
		v1, a1 := parseOne(fields[1])
		v2, a2 := parseOne(fields[2])
		vals = [4]float32{v0, v1, v2, v1}
		autos = [4]bool{a0, a1, a2, a1}
	default:
		if len(fields) >= 4 {
			for k := 0; k < 4; k++ {
				vals[k], autos[k] = parseOne(fields[k])
			}
		}
	}
	return Edge{Top: vals[0], Right: vals[1], Bottom: vals[2], Left: vals[3]}, autos
}

func parseBorderShorthand(raw string, fontSize, rootFont, vw, vh float32) (float32, Color, bool) {
	w := float32(1.0)
	var col Color
	hasCol := false
	for _, tok := range strings.Fields(raw) {
		lower := strings.ToLower(tok)
		if lower == "solid" || lower == "dashed" || lower == "dotted" || lower == "double" {
			continue
		}
		if lower == "none" || lower == "hidden" {
			w = 0
			continue
		}
		if c, ok := ParseColor(tok); ok {
			col = c
			hasCol = true
			continue
		}
		w = EvalMathPx(tok, fontSize, rootFont, vw, vh)
	}
	return w, col, hasCol
}

func parseLinearGradient(raw string) (LinearGradient, bool) {
	idx := strings.Index(strings.ToLower(raw), "linear-gradient(")
	if idx < 0 {
		return LinearGradient{}, false
	}
	sub := raw[idx+len("linear-gradient("):]
	end := strings.LastIndexByte(sub, ')')
	if end >= 0 {
		sub = sub[:end]
	}
	parts := splitSelectors(sub)
	if len(parts) < 2 {
		return LinearGradient{}, false
	}
	angle := float32(180.0)
	startIdx := 0
	p0 := strings.ToLower(strings.TrimSpace(parts[0]))
	if strings.HasSuffix(p0, "deg") {
		if v, err := strconv.ParseFloat(strings.TrimSuffix(p0, "deg"), 32); err == nil {
			angle = float32(v)
			startIdx = 1
		}
	} else if strings.HasPrefix(p0, "to ") {
		switch strings.TrimSpace(p0[3:]) {
		case "right":
			angle = 90
		case "left":
			angle = 270
		case "top":
			angle = 0
		case "bottom":
			angle = 180
		}
		startIdx = 1
	}
	if len(parts)-startIdx < 2 {
		return LinearGradient{}, false
	}
	c0Tok := strings.Fields(parts[startIdx])[0]
	c1Tok := strings.Fields(parts[len(parts)-1])[0]
	c0, ok0 := ParseColor(c0Tok)
	c1, ok1 := ParseColor(c1Tok)
	if !ok0 || !ok1 {
		return LinearGradient{}, false
	}
	return LinearGradient{AngleDeg: angle, StartColor: c0, EndColor: c1}, true
}

func parseBoxShadow(raw string, fontSize, rootFont, vw, vh float32) (BoxShadow, bool) {
	if strings.EqualFold(strings.TrimSpace(raw), "none") {
		return BoxShadow{}, false
	}
	fields := strings.Fields(raw)
	if len(fields) < 2 {
		return BoxShadow{}, false
	}
	var lengths []float32
	col := RGBA(0, 0, 0, 80)
	for _, f := range fields {
		if strings.EqualFold(f, "inset") {
			continue
		}
		if c, ok := ParseColor(f); ok {
			col = c
			continue
		}
		lengths = append(lengths, EvalMathPx(f, fontSize, rootFont, vw, vh))
	}
	if len(lengths) < 2 {
		return BoxShadow{}, false
	}
	bs := BoxShadow{OffsetX: lengths[0], OffsetY: lengths[1], Color: col}
	if len(lengths) >= 3 {
		bs.BlurRadius = lengths[2]
	}
	if len(lengths) >= 4 {
		bs.SpreadRadius = lengths[3]
	}
	return bs, true
}

func parseSizeValue(raw string, fontSize, rootFont, vw, vh float32) Size {
	s := strings.ToLower(strings.TrimSpace(raw))
	if s == "" || s == "auto" || s == "none" {
		return Size{Kind: SizeAuto}
	}
	if strings.HasPrefix(s, "calc(") || strings.HasPrefix(s, "min(") || strings.HasPrefix(s, "max(") || strings.HasPrefix(s, "clamp(") {
		return Size{Kind: SizePx, Value: EvalMathPx(s, fontSize, rootFont, vw, vh)}
	}
	if strings.HasSuffix(s, "%") {
		v, err := strconv.ParseFloat(strings.TrimSuffix(s, "%"), 32)
		if err == nil {
			return Size{Kind: SizePercent, Value: float32(v)}
		}
	}
	if strings.HasSuffix(s, "rem") {
		v, err := strconv.ParseFloat(strings.TrimSuffix(s, "rem"), 32)
		if err == nil {
			return Size{Kind: SizeRem, Value: float32(v)}
		}
	}
	if strings.HasSuffix(s, "em") {
		v, err := strconv.ParseFloat(strings.TrimSuffix(s, "em"), 32)
		if err == nil {
			return Size{Kind: SizeEm, Value: float32(v)}
		}
	}
	if strings.HasSuffix(s, "vw") {
		v, err := strconv.ParseFloat(strings.TrimSuffix(s, "vw"), 32)
		if err == nil {
			return Size{Kind: SizeVw, Value: float32(v)}
		}
	}
	if strings.HasSuffix(s, "vh") {
		v, err := strconv.ParseFloat(strings.TrimSuffix(s, "vh"), 32)
		if err == nil {
			return Size{Kind: SizeVh, Value: float32(v)}
		}
	}
	if strings.HasSuffix(s, "ch") {
		v, err := strconv.ParseFloat(strings.TrimSuffix(s, "ch"), 32)
		if err == nil {
			return Size{Kind: SizeCh, Value: float32(v)}
		}
	}
	s = strings.TrimSuffix(s, "px")
	v, err := strconv.ParseFloat(s, 32)
	if err == nil {
		return Size{Kind: SizePx, Value: float32(v)}
	}
	return Size{Kind: SizeAuto}
}

func EvalMathPx(raw string, fontSize, rootFont, vw, vh float32) float32 {
	s := strings.ToLower(strings.TrimSpace(raw))
	if strings.HasPrefix(s, "calc(") && strings.HasSuffix(s, ")") {
		inner := s[5 : len(s)-1]
		return evalSimpleExpr(inner, fontSize, rootFont, vw, vh)
	}
	if strings.HasPrefix(s, "min(") && strings.HasSuffix(s, ")") {
		parts := splitSelectors(s[4 : len(s)-1])
		if len(parts) == 0 {
			return 0
		}
		best := EvalMathPx(parts[0], fontSize, rootFont, vw, vh)
		for _, p := range parts[1:] {
			v := EvalMathPx(p, fontSize, rootFont, vw, vh)
			if v < best {
				best = v
			}
		}
		return best
	}
	if strings.HasPrefix(s, "max(") && strings.HasSuffix(s, ")") {
		parts := splitSelectors(s[4 : len(s)-1])
		if len(parts) == 0 {
			return 0
		}
		best := EvalMathPx(parts[0], fontSize, rootFont, vw, vh)
		for _, p := range parts[1:] {
			v := EvalMathPx(p, fontSize, rootFont, vw, vh)
			if v > best {
				best = v
			}
		}
		return best
	}
	if strings.HasPrefix(s, "clamp(") && strings.HasSuffix(s, ")") {
		parts := splitSelectors(s[6 : len(s)-1])
		if len(parts) == 3 {
			lo := EvalMathPx(parts[0], fontSize, rootFont, vw, vh)
			val := EvalMathPx(parts[1], fontSize, rootFont, vw, vh)
			hi := EvalMathPx(parts[2], fontSize, rootFont, vw, vh)
			return float32(math.Max(float64(lo), math.Min(float64(val), float64(hi))))
		}
	}
	if strings.HasSuffix(s, "rem") {
		v, _ := strconv.ParseFloat(strings.TrimSuffix(s, "rem"), 32)
		return float32(v) * rootFont
	}
	if strings.HasSuffix(s, "em") {
		v, _ := strconv.ParseFloat(strings.TrimSuffix(s, "em"), 32)
		return float32(v) * fontSize
	}
	if strings.HasSuffix(s, "vw") {
		v, _ := strconv.ParseFloat(strings.TrimSuffix(s, "vw"), 32)
		return vw * float32(v) / 100.0
	}
	if strings.HasSuffix(s, "vh") {
		v, _ := strconv.ParseFloat(strings.TrimSuffix(s, "vh"), 32)
		return vh * float32(v) / 100.0
	}
	if strings.HasSuffix(s, "ch") {
		v, _ := strconv.ParseFloat(strings.TrimSuffix(s, "ch"), 32)
		return (fontSize * 0.5) * float32(v)
	}
	if strings.HasSuffix(s, "%") {
		v, _ := strconv.ParseFloat(strings.TrimSuffix(s, "%"), 32)
		return vw * float32(v) / 100.0
	}
	s = strings.TrimSuffix(s, "px")
	v, _ := strconv.ParseFloat(s, 32)
	return float32(v)
}

func evalSimpleExpr(expr string, fontSize, rootFont, vw, vh float32) float32 {
	return evalSimpleExprDepth(expr, fontSize, rootFont, vw, vh, 0)
}

func evalSimpleExprDepth(expr string, fontSize, rootFont, vw, vh float32, depth int) float32 {
	if depth > 16 {
		return 0
	}
	if idx := strings.Index(expr, " + "); idx > 0 {
		return evalSimpleExprDepth(expr[:idx], fontSize, rootFont, vw, vh, depth+1) + evalSimpleExprDepth(expr[idx+3:], fontSize, rootFont, vw, vh, depth+1)
	}
	if idx := strings.Index(expr, " - "); idx > 0 {
		return evalSimpleExprDepth(expr[:idx], fontSize, rootFont, vw, vh, depth+1) - evalSimpleExprDepth(expr[idx+3:], fontSize, rootFont, vw, vh, depth+1)
	}
	if idx := strings.IndexByte(expr, '*'); idx > 0 {
		return evalSimpleExprDepth(expr[:idx], fontSize, rootFont, vw, vh, depth+1) * evalSimpleExprDepth(expr[idx+1:], fontSize, rootFont, vw, vh, depth+1)
	}
	if idx := strings.IndexByte(expr, '/'); idx > 0 {
		rhs := evalSimpleExprDepth(expr[idx+1:], fontSize, rootFont, vw, vh, depth+1)
		if rhs != 0 {
			return evalSimpleExprDepth(expr[:idx], fontSize, rootFont, vw, vh, depth+1) / rhs
		}
	}
	return EvalMathPx(expr, fontSize, rootFont, vw, vh)
}

func resolveCustomVars(val string, props map[string]string, depth int) string {
	if depth > 8 || len(val) > 4096 || !strings.Contains(val, "var(") {
		return val
	}
	idx := strings.Index(val, "var(")
	end := findMatchingParenStr(val, idx+3)
	if end < 0 {
		return val
	}
	inner := val[idx+4 : end]
	varName := inner
	fallback := ""
	if comma := strings.IndexByte(inner, ','); comma >= 0 {
		varName = strings.TrimSpace(inner[:comma])
		fallback = strings.TrimSpace(inner[comma+1:])
	} else {
		varName = strings.TrimSpace(inner)
	}
	replacement, ok := props[varName]
	if !ok || strings.TrimSpace(replacement) == "" {
		replacement = fallback
	}
	next := val[:idx] + replacement + val[end+1:]
	if len(next) > 4096 {
		return val
	}
	return resolveCustomVars(next, props, depth+1)
}

func evalContentValue(d *dom.NodeData, val string) string {
	v := strings.TrimSpace(val)
	if strings.EqualFold(v, "none") || strings.EqualFold(v, "normal") {
		return ""
	}
	if strings.HasPrefix(strings.ToLower(v), "attr(") && strings.HasSuffix(v, ")") {
		attrName := strings.TrimSpace(v[5 : len(v)-1])
		av, _ := d.Attr(attrName)
		return av
	}
	return strings.Trim(v, "\"'")
}

func matchesMediaQuery(mq string, vw, vh uint32) bool {
	s := strings.ToLower(strings.TrimSpace(mq))
	if s == "" || s == "all" || s == "screen" {
		return true
	}
	if strings.Contains(s, "print") && !strings.Contains(s, "not print") {
		return false
	}
	for _, part := range strings.Split(s, ",") {
		part = strings.TrimSpace(part)
		if matchSingleMedia(part, vw, vh) {
			return true
		}
	}
	return false
}

func matchSingleMedia(q string, vw, vh uint32) bool {
	neg := false
	if strings.HasPrefix(q, "not ") {
		neg = true
		q = strings.TrimSpace(q[4:])
	} else if strings.HasPrefix(q, "only ") {
		q = strings.TrimSpace(q[5:])
	}
	res := true
	for _, cond := range strings.Split(q, "and") {
		cond = strings.Trim(strings.TrimSpace(cond), "()")
		if cond == "" || cond == "all" || cond == "screen" {
			continue
		}
		if cond == "print" {
			res = false
			break
		}
		if strings.HasPrefix(cond, "min-width:") {
			px := uint32(EvalMathPx(strings.TrimSpace(cond[10:]), 16, 16, float32(vw), float32(vh)))
			if vw < px {
				res = false
			}
		} else if strings.HasPrefix(cond, "max-width:") {
			px := uint32(EvalMathPx(strings.TrimSpace(cond[10:]), 16, 16, float32(vw), float32(vh)))
			if vw > px {
				res = false
			}
		} else if strings.HasPrefix(cond, "prefers-color-scheme:") {
			scheme := strings.TrimSpace(cond[21:])
			if scheme != "dark" && scheme != "light" {
				res = false
			}
		}
	}
	if neg {
		return !res
	}
	return res
}

func checkScopeProximity(doc *dom.Document, id dom.NodeId, scopeRoot, scopeLimit string) (int, bool) {
	if scopeRoot == "" {
		return 0, true
	}
	cur := id
	hops := 0
	for {
		if scopeLimit != "" && doc.MatchesSelector(cur, scopeLimit) {
			return 0, false
		}
		if doc.MatchesSelector(cur, scopeRoot) {
			return hops, true
		}
		p, ok := doc.Parent(cur)
		if !ok {
			return 0, false
		}
		cur = p
		hops++
	}
}
