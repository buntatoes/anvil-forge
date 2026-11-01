// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package style

import (
	"math"
	"strconv"
	"strings"
)

const (
	MaxSelectorNesting   = 64
	MaxSelectorCompounds = 640
	MaxTransformFns      = 16
	MaxTransitions       = 16
	MaxAnimations        = 16
)

type Color struct {
	R, G, B, A uint8
}

func RGBA(r, g, b, a uint8) Color {
	return Color{R: r, G: g, B: b, A: a}
}

type Edge struct {
	Top, Right, Bottom, Left float32
}

type SizeKind int

const (
	SizeAuto SizeKind = iota
	SizePx
	SizePercent
	SizeEm
	SizeRem
	SizeVw
	SizeVh
	SizeCh
)

type Size struct {
	Kind  SizeKind
	Value float32
}

func (s Size) ResolvePx(containing, fontSize, rootFontSize, vw, vh float32) (float32, bool) {
	switch s.Kind {
	case SizeAuto:
		return 0, false
	case SizePx:
		return s.Value, true
	case SizePercent:
		return containing * (s.Value / 100.0), true
	case SizeEm:
		return fontSize * s.Value, true
	case SizeRem:
		return rootFontSize * s.Value, true
	case SizeVw:
		return vw * (s.Value / 100.0), true
	case SizeVh:
		return vh * (s.Value / 100.0), true
	case SizeCh:
		return (fontSize * 0.5) * s.Value, true
	default:
		return 0, false
	}
}

type Display int

const (
	DisplayBlock Display = iota
	DisplayInline
	DisplayInlineBlock
	DisplayListItem
	DisplayFlowRoot
	DisplayFlex
	DisplayInlineFlex
	DisplayGrid
	DisplayInlineGrid
	DisplayTable
	DisplayTableRowGroup
	DisplayTableHeaderGroup
	DisplayTableFooterGroup
	DisplayTableRow
	DisplayTableCell
	DisplayContents
	DisplayNone
)

type Position int

const (
	PositionStatic Position = iota
	PositionRelative
	PositionAbsolute
	PositionFixed
	PositionSticky
)

type Overflow int

const (
	OverflowVisible Overflow = iota
	OverflowHidden
	OverflowAuto
	OverflowScroll
)

type FlexDirection int

const (
	FlexRow FlexDirection = iota
	FlexRowReverse
	FlexColumn
	FlexColumnReverse
)

type FlexWrap int

const (
	FlexNoWrap FlexWrap = iota
	FlexWrapNormal
	FlexWrapReverse
)

type JustifyContent int

const (
	JustifyFlexStart JustifyContent = iota
	JustifyCenter
	JustifyFlexEnd
	JustifySpaceBetween
	JustifySpaceAround
	JustifySpaceEvenly
)

type AlignItems int

const (
	AlignStretch AlignItems = iota
	AlignFlexStart
	AlignCenter
	AlignFlexEnd
	AlignBaseline
)

type AlignContent int

const (
	AlignContentStretch AlignContent = iota
	AlignContentFlexStart
	AlignContentCenter
	AlignContentFlexEnd
	AlignContentSpaceBetween
	AlignContentSpaceAround
	AlignContentSpaceEvenly
)

type GridAutoFlow int

const (
	GridFlowRow GridAutoFlow = iota
	GridFlowColumn
	GridFlowRowDense
	GridFlowColumnDense
)

type GridTrack struct {
	Kind  SizeKind
	Value float32
	Fr    float32
}

type TextAlign int

const (
	TextAlignLeft TextAlign = iota
	TextAlignCenter
	TextAlignRight
	TextAlignJustify
)

type TextTransform int

const (
	TextTransformNone TextTransform = iota
	TextTransformUppercase
	TextTransformLowercase
	TextTransformCapitalize
)

type FontFamily int

const (
	FontSansSerif FontFamily = iota
	FontSerif
	FontMonospace
	FontCustom
)

type ObjectFit int

const (
	ObjectFitFill ObjectFit = iota
	ObjectFitContain
	ObjectFitCover
	ObjectFitNone
	ObjectFitScaleDown
)

type Visibility int

const (
	VisibilityVisible Visibility = iota
	VisibilityHidden
)

type WhiteSpace int

const (
	WhiteSpaceNormal WhiteSpace = iota
	WhiteSpacePre
	WhiteSpaceNoWrap
	WhiteSpacePreWrap
)

type BoxSizing int

const (
	BoxSizingContentBox BoxSizing = iota
	BoxSizingBorderBox
)

type BoxShadow struct {
	OffsetX      float32
	OffsetY      float32
	BlurRadius   float32
	SpreadRadius float32
	Color        Color
}

type LinearGradient struct {
	AngleDeg   float32
	StartColor Color
	EndColor   Color
}

type TransformFn struct {
	Kind   string
	Values [6]float32
}

type RegisteredProperty struct {
	Name         string
	Syntax       string
	Inherits     bool
	InitialValue string
}

type FontFaceRule struct {
	Family string
	SrcURL string
	Weight uint16
	Italic bool
}

type Keyframe struct {
	Offset       float32 // 0.0 .. 1.0
	Declarations []Declaration
}

type KeyframesRule struct {
	Name      string
	Keyframes []Keyframe
}

type Declaration struct {
	Property  string
	Value     string
	Important bool
}

type Specificity struct {
	IDs     uint16
	Classes uint16
	Types   uint16
}

func (s Specificity) Less(other Specificity) bool {
	if s.IDs != other.IDs {
		return s.IDs < other.IDs
	}
	if s.Classes != other.Classes {
		return s.Classes < other.Classes
	}
	return s.Types < other.Types
}

type Rule struct {
	Selectors    []string
	Declarations []Declaration
	Media        string
	Layer        string
	ScopeRoot    string
	ScopeLimit   string
}

type Stylesheet struct {
	Rules      []Rule
	Keyframes  []KeyframesRule
	Layers     []string
	Properties []RegisteredProperty
	FontFaces  []FontFaceRule
}

func (s *Stylesheet) Append(later Stylesheet) {
	for _, l := range later.Layers {
		s.internLayer(l)
	}
	s.Rules = append(s.Rules, later.Rules...)
	s.Keyframes = append(s.Keyframes, later.Keyframes...)
	s.FontFaces = append(s.FontFaces, later.FontFaces...)
	for _, p := range later.Properties {
		replaced := false
		for i := range s.Properties {
			if s.Properties[i].Name == p.Name {
				s.Properties[i] = p
				replaced = true
				break
			}
		}
		if !replaced {
			s.Properties = append(s.Properties, p)
		}
	}
}

func (s *Stylesheet) internLayer(name string) {
	if name == "" {
		return
	}
	for _, existing := range s.Layers {
		if existing == name {
			return
		}
	}
	s.Layers = append(s.Layers, name)
}

func ParseCSS(css string) Stylesheet {
	return ParseCSSWithMedia(css, "")
}

func ParseCSSWithMedia(css, outerMedia string) Stylesheet {
	var sheet Stylesheet
	clean := stripCSSComments(css)
	parseRuleBlocks(&sheet, clean, strings.TrimSpace(outerMedia), "", "", "", 0)
	return sheet
}

func stripCSSComments(s string) string {
	var b strings.Builder
	b.Grow(len(s))
	i := 0
	var quote byte
	for i < len(s) {
		c := s[i]
		if quote != 0 {
			b.WriteByte(c)
			if c == '\\' && i+1 < len(s) {
				i++
				b.WriteByte(s[i])
			} else if c == quote {
				quote = 0
			}
			i++
			continue
		}
		if c == '"' || c == '\'' {
			quote = c
			b.WriteByte(c)
			i++
			continue
		}
		if c == '/' && i+1 < len(s) && s[i+1] == '*' {
			end := strings.Index(s[i+2:], "*/")
			if end < 0 {
				break
			}
			i += 2 + end + 2
			continue
		}
		b.WriteByte(c)
		i++
	}
	return b.String()
}

func parseRuleBlocks(sheet *Stylesheet, src, media, layer, scopeRoot, scopeLimit string, depth int) {
	if depth > MaxSelectorNesting || len(sheet.Rules) >= 65536 {
		return
	}
	i := 0
	n := len(src)
	for i < n {
		for i < n && (src[i] == ' ' || src[i] == '\t' || src[i] == '\r' || src[i] == '\n' || src[i] == ';') {
			i++
		}
		if i >= n {
			break
		}
		braceOrSemi := findNextTopLevel(src, i)
		if braceOrSemi < 0 {
			break
		}
		if src[braceOrSemi] == ';' {
			stmt := strings.TrimSpace(src[i:braceOrSemi])
			if strings.HasPrefix(strings.ToLower(stmt), "@layer ") {
				names := strings.Split(stmt[7:], ",")
				for _, nm := range names {
					sheet.internLayer(qualifyLayer(layer, strings.TrimSpace(nm)))
				}
			}
			i = braceOrSemi + 1
			continue
		}
		prelude := strings.TrimSpace(src[i:braceOrSemi])
		blockEnd := findMatchingBrace(src, braceOrSemi)
		if blockEnd < 0 {
			break
		}
		body := src[braceOrSemi+1 : blockEnd]
		i = blockEnd + 1

		lowerPrelude := strings.ToLower(prelude)
		if strings.HasPrefix(lowerPrelude, "@media") {
			mq := strings.TrimSpace(prelude[6:])
			combinedMedia := mq
			if media != "" && mq != "" {
				combinedMedia = media + " and " + mq
			} else if media != "" {
				combinedMedia = media
			}
			parseRuleBlocks(sheet, body, combinedMedia, layer, scopeRoot, scopeLimit, depth+1)
			continue
		}
		if strings.HasPrefix(lowerPrelude, "@supports") {
			cond := strings.TrimSpace(prelude[9:])
			if evalSupportsCondition(cond) {
				parseRuleBlocks(sheet, body, media, layer, scopeRoot, scopeLimit, depth+1)
			}
			continue
		}
		if strings.HasPrefix(lowerPrelude, "@layer") {
			layerName := strings.TrimSpace(prelude[6:])
			if layerName == "" {
				layerName = "<anon>"
			}
			fullLayer := qualifyLayer(layer, layerName)
			sheet.internLayer(fullLayer)
			parseRuleBlocks(sheet, body, media, fullLayer, scopeRoot, scopeLimit, depth+1)
			continue
		}
		if strings.HasPrefix(lowerPrelude, "@scope") {
			sr, sl := parseScopePrelude(prelude[6:])
			parseRuleBlocks(sheet, body, media, layer, sr, sl, depth+1)
			continue
		}
		if strings.HasPrefix(lowerPrelude, "@container") {
			parseRuleBlocks(sheet, body, media, layer, scopeRoot, scopeLimit, depth+1)
			continue
		}
		if strings.HasPrefix(lowerPrelude, "@keyframes") {
			kfName := strings.TrimSpace(prelude[strings.IndexByte(prelude, ' ')+1:])
			sheet.Keyframes = append(sheet.Keyframes, parseKeyframes(kfName, body))
			continue
		}
		if strings.HasPrefix(lowerPrelude, "@property") {
			propName := strings.TrimSpace(prelude[9:])
			if strings.HasPrefix(propName, "--") {
				decls := parseDeclarationsOnly(body)
				rp := RegisteredProperty{Name: propName, Syntax: "*", Inherits: true}
				for _, d := range decls {
					switch d.Property {
					case "syntax":
						rp.Syntax = strings.Trim(d.Value, "\"'")
					case "inherits":
						rp.Inherits = strings.EqualFold(d.Value, "true")
					case "initial-value":
						rp.InitialValue = d.Value
					}
				}
				sheet.Properties = append(sheet.Properties, rp)
			}
			continue
		}
		if strings.HasPrefix(lowerPrelude, "@font-face") {
			decls := parseDeclarationsOnly(body)
			ff := FontFaceRule{Weight: 400}
			for _, d := range decls {
				switch d.Property {
				case "font-family":
					ff.Family = strings.Trim(d.Value, "\"'")
				case "src":
					ff.SrcURL = extractURL(d.Value)
				case "font-weight":
					if strings.EqualFold(d.Value, "bold") {
						ff.Weight = 700
					} else if v, err := strconv.Atoi(d.Value); err == nil {
						ff.Weight = uint16(v)
					}
				case "font-style":
					ff.Italic = strings.Contains(strings.ToLower(d.Value), "italic")
				}
			}
			if ff.Family != "" {
				sheet.FontFaces = append(sheet.FontFaces, ff)
			}
			continue
		}
		if strings.HasPrefix(lowerPrelude, "@") {
			continue
		}

		sels := splitSelectors(prelude)
		decls, nested := splitDeclarationsAndNestedRules(body)
		if len(sels) > 0 && len(decls) > 0 {
			sheet.Rules = append(sheet.Rules, Rule{
				Selectors:    sels,
				Declarations: decls,
				Media:        media,
				Layer:        layer,
				ScopeRoot:    scopeRoot,
				ScopeLimit:   scopeLimit,
			})
		}
		for _, nr := range nested {
			expandedPrelude := expandNestedSelector(sels, nr.prelude)
			if expandedPrelude != "" {
				parseRuleBlocks(sheet, expandedPrelude+"{"+nr.body+"}", media, layer, scopeRoot, scopeLimit, depth+1)
			}
		}
	}
}

type nestedBlock struct {
	prelude string
	body    string
}

func splitDeclarationsAndNestedRules(body string) ([]Declaration, []nestedBlock) {
	var decls []Declaration
	var nested []nestedBlock
	i := 0
	n := len(body)
	for i < n {
		for i < n && (body[i] == ' ' || body[i] == '\t' || body[i] == '\r' || body[i] == '\n' || body[i] == ';') {
			i++
		}
		if i >= n {
			break
		}
		next := findNextTopLevel(body, i)
		if next < 0 {
			if d, ok := parseSingleDeclaration(body[i:]); ok {
				decls = append(decls, d)
			}
			break
		}
		if body[next] == ';' {
			if d, ok := parseSingleDeclaration(body[i:next]); ok {
				decls = append(decls, d)
			}
			i = next + 1
		} else {
			prelude := strings.TrimSpace(body[i:next])
			end := findMatchingBrace(body, next)
			if end < 0 {
				break
			}
			nested = append(nested, nestedBlock{
				prelude: prelude,
				body:    body[next+1 : end],
			})
			i = end + 1
		}
	}
	return decls, nested
}

func expandNestedSelector(parents []string, childPrelude string) string {
	if strings.HasPrefix(childPrelude, "@") {
		return childPrelude
	}
	childSels := splitSelectors(childPrelude)
	var out []string
	for _, p := range parents {
		for _, c := range childSels {
			if len(out) >= MaxSelectorCompounds {
				break
			}
			var combined string
			if strings.Contains(c, "&") {
				combined = strings.ReplaceAll(c, "&", p)
			} else {
				combined = p + " " + c
			}
			if len(combined) <= 4096 {
				out = append(out, combined)
			}
		}
	}
	return strings.Join(out, ", ")
}

func parseDeclarationsOnly(body string) []Declaration {
	decls, _ := splitDeclarationsAndNestedRules(body)
	return decls
}

func parseSingleDeclaration(raw string) (Declaration, bool) {
	colon := strings.IndexByte(raw, ':')
	if colon <= 0 {
		return Declaration{}, false
	}
	prop := strings.TrimSpace(raw[:colon])
	if !strings.HasPrefix(prop, "--") {
		prop = strings.ToLower(prop)
	}
	val := strings.TrimSpace(raw[colon+1:])
	if prop == "" || val == "" {
		return Declaration{}, false
	}
	important := false
	lowerVal := strings.ToLower(val)
	if strings.HasSuffix(lowerVal, "!important") {
		important = true
		val = strings.TrimSpace(val[:len(val)-len("!important")])
	}
	return Declaration{
		Property:  prop,
		Value:     val,
		Important: important,
	}, true
}

func qualifyLayer(parent, child string) string {
	if parent == "" {
		return child
	}
	return parent + "." + child
}

func parseScopePrelude(s string) (string, string) {
	s = strings.TrimSpace(s)
	root := ""
	limit := ""
	if strings.HasPrefix(s, "(") {
		end := findMatchingParenStr(s, 0)
		if end > 0 {
			root = strings.TrimSpace(s[1:end])
			rest := strings.TrimSpace(s[end+1:])
			if strings.HasPrefix(strings.ToLower(rest), "to") {
				rest = strings.TrimSpace(rest[2:])
				if strings.HasPrefix(rest, "(") {
					endLim := findMatchingParenStr(rest, 0)
					if endLim > 0 {
						limit = strings.TrimSpace(rest[1:endLim])
					}
				}
			}
		}
	}
	return root, limit
}

func parseKeyframes(name, body string) KeyframesRule {
	rule := KeyframesRule{Name: name}
	i := 0
	for i < len(body) {
		next := findNextTopLevel(body, i)
		if next < 0 || body[next] != '{' {
			break
		}
		sel := strings.TrimSpace(body[i:next])
		end := findMatchingBrace(body, next)
		if end < 0 {
			break
		}
		decls := parseDeclarationsOnly(body[next+1 : end])
		i = end + 1
		for _, tok := range strings.Split(sel, ",") {
			tok = strings.ToLower(strings.TrimSpace(tok))
			var off float32
			if tok == "from" {
				off = 0
			} else if tok == "to" {
				off = 1
			} else if strings.HasSuffix(tok, "%") {
				if v, err := strconv.ParseFloat(strings.TrimSuffix(tok, "%"), 32); err == nil {
					off = float32(v / 100.0)
				}
			}
			rule.Keyframes = append(rule.Keyframes, Keyframe{
				Offset:       off,
				Declarations: decls,
			})
		}
	}
	return rule
}

func evalSupportsCondition(cond string) bool {
	c := strings.ToLower(strings.TrimSpace(cond))
	if strings.HasPrefix(c, "not ") {
		return !evalSupportsCondition(c[4:])
	}
	return strings.Contains(c, ":")
}

func extractURL(val string) string {
	idx := strings.Index(strings.ToLower(val), "url(")
	if idx < 0 {
		return strings.Trim(val, "\"'")
	}
	sub := val[idx+4:]
	end := strings.IndexByte(sub, ')')
	if end >= 0 {
		sub = sub[:end]
	}
	return strings.Trim(strings.TrimSpace(sub), "\"'")
}

func splitSelectors(prelude string) []string {
	var out []string
	start := 0
	depthParen := 0
	depthBracket := 0
	var quote byte
	for i := 0; i < len(prelude); i++ {
		c := prelude[i]
		if quote != 0 {
			if c == quote {
				quote = 0
			}
			continue
		}
		switch c {
		case '"', '\'':
			quote = c
		case '(':
			depthParen++
		case ')':
			if depthParen > 0 {
				depthParen--
			}
		case '[':
			depthBracket++
		case ']':
			if depthBracket > 0 {
				depthBracket--
			}
		case ',':
			if depthParen == 0 && depthBracket == 0 {
				s := strings.TrimSpace(prelude[start:i])
				if s != "" {
					out = append(out, s)
				}
				start = i + 1
			}
		}
	}
	if s := strings.TrimSpace(prelude[start:]); s != "" {
		out = append(out, s)
	}
	return out
}

func findNextTopLevel(s string, start int) int {
	depthParen := 0
	var quote byte
	for i := start; i < len(s); i++ {
		c := s[i]
		if quote != 0 {
			if c == '\\' && i+1 < len(s) {
				i++
			} else if c == quote {
				quote = 0
			}
			continue
		}
		switch c {
		case '"', '\'':
			quote = c
		case '(':
			depthParen++
		case ')':
			if depthParen > 0 {
				depthParen--
			}
		case '{', ';':
			if depthParen == 0 {
				return i
			}
		}
	}
	return -1
}

func findMatchingBrace(s string, openIdx int) int {
	depth := 0
	var quote byte
	for i := openIdx; i < len(s); i++ {
		c := s[i]
		if quote != 0 {
			if c == '\\' && i+1 < len(s) {
				i++
			} else if c == quote {
				quote = 0
			}
			continue
		}
		switch c {
		case '"', '\'':
			quote = c
		case '{':
			depth++
		case '}':
			depth--
			if depth == 0 {
				return i
			}
		}
	}
	return -1
}

func findMatchingParenStr(s string, openIdx int) int {
	depth := 0
	for i := openIdx; i < len(s); i++ {
		if s[i] == '(' {
			depth++
		} else if s[i] == ')' {
			depth--
			if depth == 0 {
				return i
			}
		}
	}
	return -1
}

func ComputeSpecificity(sel string) Specificity {
	var spec Specificity
	if strings.HasPrefix(sel, ":where(") {
		return spec
	}
	i := 0
	for i < len(sel) {
		c := sel[i]
		switch c {
		case '#':
			spec.IDs++
			i++
			for i < len(sel) && isIdent(sel[i]) {
				i++
			}
		case '.':
			spec.Classes++
			i++
			for i < len(sel) && isIdent(sel[i]) {
				i++
			}
		case '[':
			spec.Classes++
			end := strings.IndexByte(sel[i:], ']')
			if end < 0 {
				return spec
			}
			i += end + 1
		case ':':
			if i+1 < len(sel) && sel[i+1] == ':' {
				spec.Types++
				i += 2
				for i < len(sel) && isIdent(sel[i]) {
					i++
				}
				continue
			}
			i++
			start := i
			for i < len(sel) && isIdent(sel[i]) {
				i++
			}
			name := strings.ToLower(sel[start:i])
			if i < len(sel) && sel[i] == '(' {
				end := findMatchingParenStr(sel, i)
				if end > i {
					arg := sel[i+1 : end]
					i = end + 1
					if name == "where" {
						continue
					}
					if name == "is" || name == "not" || name == "has" {
						var best Specificity
						for _, part := range splitSelectors(arg) {
							sub := ComputeSpecificity(part)
							if best.Less(sub) {
								best = sub
							}
						}
						spec.IDs += best.IDs
						spec.Classes += best.Classes
						spec.Types += best.Types
						continue
					}
				}
			}
			spec.Classes++
		case '*', '>', '+', '~', ' ', '\t', '\r', '\n':
			i++
		default:
			if isIdent(c) {
				spec.Types++
				for i < len(sel) && isIdent(sel[i]) {
					i++
				}
			} else {
				i++
			}
		}
	}
	return spec
}

func isIdent(b byte) bool {
	return (b >= 'a' && b <= 'z') || (b >= 'A' && b <= 'Z') ||
		(b >= '0' && b <= '9') || b == '-' || b == '_'
}

func ParseColor(raw string) (Color, bool) {
	s := strings.ToLower(strings.TrimSpace(raw))
	if s == "" {
		return Color{}, false
	}
	switch s {
	case "transparent":
		return RGBA(0, 0, 0, 0), true
	case "black":
		return RGBA(0, 0, 0, 255), true
	case "white":
		return RGBA(255, 255, 255, 255), true
	case "red":
		return RGBA(255, 0, 0, 255), true
	case "green":
		return RGBA(0, 128, 0, 255), true
	case "blue":
		return RGBA(0, 0, 255, 255), true
	case "yellow":
		return RGBA(255, 255, 0, 255), true
	case "cyan", "aqua":
		return RGBA(0, 255, 255, 255), true
	case "magenta", "fuchsia":
		return RGBA(255, 0, 255, 255), true
	case "gray", "grey":
		return RGBA(128, 128, 128, 255), true
	case "silver":
		return RGBA(192, 192, 192, 255), true
	case "orange":
		return RGBA(255, 165, 0, 255), true
	case "rebeccapurple":
		return RGBA(102, 51, 153, 255), true
	}
	if strings.HasPrefix(s, "#") {
		hex := s[1:]
		switch len(hex) {
		case 3:
			r, err1 := strconv.ParseUint(strings.Repeat(hex[0:1], 2), 16, 8)
			g, err2 := strconv.ParseUint(strings.Repeat(hex[1:2], 2), 16, 8)
			b, err3 := strconv.ParseUint(strings.Repeat(hex[2:3], 2), 16, 8)
			if err1 == nil && err2 == nil && err3 == nil {
				return RGBA(uint8(r), uint8(g), uint8(b), 255), true
			}
		case 4:
			r, err1 := strconv.ParseUint(strings.Repeat(hex[0:1], 2), 16, 8)
			g, err2 := strconv.ParseUint(strings.Repeat(hex[1:2], 2), 16, 8)
			b, err3 := strconv.ParseUint(strings.Repeat(hex[2:3], 2), 16, 8)
			a, err4 := strconv.ParseUint(strings.Repeat(hex[3:4], 2), 16, 8)
			if err1 == nil && err2 == nil && err3 == nil && err4 == nil {
				return RGBA(uint8(r), uint8(g), uint8(b), uint8(a)), true
			}
		case 6:
			r, err1 := strconv.ParseUint(hex[0:2], 16, 8)
			g, err2 := strconv.ParseUint(hex[2:4], 16, 8)
			b, err3 := strconv.ParseUint(hex[4:6], 16, 8)
			if err1 == nil && err2 == nil && err3 == nil {
				return RGBA(uint8(r), uint8(g), uint8(b), 255), true
			}
		case 8:
			r, err1 := strconv.ParseUint(hex[0:2], 16, 8)
			g, err2 := strconv.ParseUint(hex[2:4], 16, 8)
			b, err3 := strconv.ParseUint(hex[4:6], 16, 8)
			a, err4 := strconv.ParseUint(hex[6:8], 16, 8)
			if err1 == nil && err2 == nil && err3 == nil && err4 == nil {
				return RGBA(uint8(r), uint8(g), uint8(b), uint8(a)), true
			}
		}
		return Color{}, false
	}
	if (strings.HasPrefix(s, "rgb(") || strings.HasPrefix(s, "rgba(")) && strings.HasSuffix(s, ")") {
		inner := s[strings.IndexByte(s, '(')+1 : len(s)-1]
		inner = strings.ReplaceAll(inner, "/", " ")
		inner = strings.ReplaceAll(inner, ",", " ")
		fields := strings.Fields(inner)
		if len(fields) < 3 {
			return Color{}, false
		}
		r := parseColorChannel(fields[0])
		g := parseColorChannel(fields[1])
		b := parseColorChannel(fields[2])
		a := uint8(255)
		if len(fields) >= 4 {
			a = parseAlphaChannel(fields[3])
		}
		return RGBA(r, g, b, a), true
	}
	if (strings.HasPrefix(s, "hsl(") || strings.HasPrefix(s, "hsla(")) && strings.HasSuffix(s, ")") {
		inner := s[strings.IndexByte(s, '(')+1 : len(s)-1]
		inner = strings.ReplaceAll(inner, "/", " ")
		inner = strings.ReplaceAll(inner, ",", " ")
		fields := strings.Fields(inner)
		if len(fields) < 3 {
			return Color{}, false
		}
		h, _ := strconv.ParseFloat(strings.TrimSuffix(fields[0], "deg"), 64)
		sat, _ := strconv.ParseFloat(strings.TrimSuffix(fields[1], "%"), 64)
		l, _ := strconv.ParseFloat(strings.TrimSuffix(fields[2], "%"), 64)
		a := uint8(255)
		if len(fields) >= 4 {
			a = parseAlphaChannel(fields[3])
		}
		r, g, b := hslToRGB(h, sat/100.0, l/100.0)
		return RGBA(r, g, b, a), true
	}
	return Color{}, false
}

func parseColorChannel(s string) uint8 {
	if strings.HasSuffix(s, "%") {
		v, _ := strconv.ParseFloat(strings.TrimSuffix(s, "%"), 64)
		return clampByte(v * 255.0 / 100.0)
	}
	v, _ := strconv.ParseFloat(s, 64)
	return clampByte(v)
}

func parseAlphaChannel(s string) uint8 {
	if strings.HasSuffix(s, "%") {
		v, _ := strconv.ParseFloat(strings.TrimSuffix(s, "%"), 64)
		return clampByte(v * 255.0 / 100.0)
	}
	v, _ := strconv.ParseFloat(s, 64)
	if v <= 1.0 && strings.Contains(s, ".") || v == 0 || v == 1 {
		return clampByte(v * 255.0)
	}
	return clampByte(v)
}

func clampByte(v float64) uint8 {
	if v <= 0 {
		return 0
	}
	if v >= 255 {
		return 255
	}
	return uint8(math.Round(v))
}

func hslToRGB(h, s, l float64) (uint8, uint8, uint8) {
	h = math.Mod(h, 360.0)
	if h < 0 {
		h += 360.0
	}
	c := (1 - math.Abs(2*l-1)) * s
	x := c * (1 - math.Abs(math.Mod(h/60.0, 2)-1))
	m := l - c/2
	var r1, g1, b1 float64
	switch {
	case h < 60:
		r1, g1, b1 = c, x, 0
	case h < 120:
		r1, g1, b1 = x, c, 0
	case h < 180:
		r1, g1, b1 = 0, c, x
	case h < 240:
		r1, g1, b1 = 0, x, c
	case h < 300:
		r1, g1, b1 = x, 0, c
	default:
		r1, g1, b1 = c, 0, x
	}
	return clampByte((r1 + m) * 255), clampByte((g1 + m) * 255), clampByte((b1 + m) * 255)
}
