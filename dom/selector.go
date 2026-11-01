// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package dom

import (
	"strconv"
	"strings"
)

func (doc *Document) QuerySelector(selector string) (NodeId, bool) {
	all := doc.QuerySelectorAll(selector)
	if len(all) == 0 {
		return 0, false
	}
	return all[0], true
}

func (doc *Document) QuerySelectorAll(selector string) []NodeId {
	var out []NodeId
	var walk func(NodeId)
	walk = func(cur NodeId) {
		n := doc.Node(cur)
		if n == nil {
			return
		}
		if n.Data.Kind == NodeKindElement && doc.MatchesSelector(cur, selector) {
			out = append(out, cur)
		}
		for _, c := range n.Children {
			walk(c)
		}
	}
	walk(doc.Root())
	return out
}

func (doc *Document) Closest(start NodeId, selector string) (NodeId, bool) {
	cur := start
	for {
		n := doc.Node(cur)
		if n != nil && n.Data.Kind == NodeKindElement && doc.MatchesSelector(cur, selector) {
			return cur, true
		}
		p, ok := doc.Parent(cur)
		if !ok {
			return 0, false
		}
		cur = p
	}
}

func (doc *Document) MatchesSelector(id NodeId, selectorList string) bool {
	return doc.MatchesSelectorWithScope(id, selectorList, doc.HTML())
}

func (doc *Document) MatchesSelectorWithScope(id NodeId, selectorList string, scope NodeId) bool {
	n := doc.Node(id)
	if n == nil || n.Data.Kind != NodeKindElement {
		return false
	}
	parts := splitTopLevelCommas(selectorList)
	for _, part := range parts {
		part = strings.TrimSpace(part)
		if part == "" {
			continue
		}
		if doc.matchComplexSelector(id, part, scope) {
			return true
		}
	}
	return false
}

type combinatorStep struct {
	comb     byte // ' ', '>', '+', '~', or 0 for rightmost
	compound string
}

func tokenizeComplexSelector(sel string) []combinatorStep {
	var steps []combinatorStep
	var cur strings.Builder
	var pendingComb byte = ' '
	depthParen := 0
	depthBracket := 0
	var quote byte

	flush := func(nextComb byte) {
		comp := strings.TrimSpace(cur.String())
		cur.Reset()
		if comp != "" {
			if len(steps) == 0 {
				steps = append(steps, combinatorStep{comb: 0, compound: comp})
			} else {
				steps = append(steps, combinatorStep{comb: pendingComb, compound: comp})
			}
			pendingComb = nextComb
		} else if nextComb != ' ' {
			pendingComb = nextComb
		}
	}

	for i := 0; i < len(sel); i++ {
		c := sel[i]
		if quote != 0 {
			cur.WriteByte(c)
			if c == quote {
				quote = 0
			}
			continue
		}
		if c == '"' || c == '\'' {
			quote = c
			cur.WriteByte(c)
			continue
		}
		if c == '(' {
			depthParen++
			cur.WriteByte(c)
			continue
		}
		if c == ')' {
			if depthParen > 0 {
				depthParen--
			}
			cur.WriteByte(c)
			continue
		}
		if c == '[' {
			depthBracket++
			cur.WriteByte(c)
			continue
		}
		if c == ']' {
			if depthBracket > 0 {
				depthBracket--
			}
			cur.WriteByte(c)
			continue
		}
		if depthParen == 0 && depthBracket == 0 {
			if c == '>' || c == '+' || c == '~' {
				flush(c)
				continue
			}
			if isSpace(c) {
				if cur.Len() > 0 {
					flush(' ')
				}
				continue
			}
		}
		cur.WriteByte(c)
	}
	flush(0)
	return steps
}

func (doc *Document) matchComplexSelector(id NodeId, sel string, scope NodeId) bool {
	steps := tokenizeComplexSelector(sel)
	if len(steps) == 0 {
		return false
	}
	return doc.matchStepsFromRight(id, steps, len(steps)-1, scope)
}

func (doc *Document) matchStepsFromRight(cur NodeId, steps []combinatorStep, idx int, scope NodeId) bool {
	if !doc.matchCompound(cur, steps[idx].compound, scope) {
		return false
	}
	if idx == 0 {
		return true
	}
	comb := steps[idx].comb
	switch comb {
	case '>':
		p, ok := doc.Parent(cur)
		if !ok {
			return false
		}
		return doc.matchStepsFromRight(p, steps, idx-1, scope)
	case ' ':
		p, ok := doc.Parent(cur)
		for ok {
			if doc.matchStepsFromRight(p, steps, idx-1, scope) {
				return true
			}
			p, ok = doc.Parent(p)
		}
		return false
	case '+':
		prev, ok := doc.prevElementSibling(cur)
		if !ok {
			return false
		}
		return doc.matchStepsFromRight(prev, steps, idx-1, scope)
	case '~':
		prev, ok := doc.prevElementSibling(cur)
		for ok {
			if doc.matchStepsFromRight(prev, steps, idx-1, scope) {
				return true
			}
			prev, ok = doc.prevElementSibling(prev)
		}
		return false
	}
	return false
}

func (doc *Document) prevElementSibling(id NodeId) (NodeId, bool) {
	p, ok := doc.Parent(id)
	if !ok {
		return 0, false
	}
	var prev NodeId
	hasPrev := false
	for _, c := range doc.Children(p) {
		if c == id {
			return prev, hasPrev
		}
		if cn := doc.Node(c); cn != nil && cn.Data.Kind == NodeKindElement {
			prev = c
			hasPrev = true
		}
	}
	return 0, false
}

func (doc *Document) matchCompound(id NodeId, compound string, scope NodeId) bool {
	n := doc.Node(id)
	if n == nil || n.Data.Kind != NodeKindElement {
		return false
	}
	// Strip ::before / ::after pseudo-elements for element matching
	if idx := strings.Index(compound, "::"); idx >= 0 {
		pseudo := strings.ToLower(compound[idx:])
		if pseudo != "::before" && pseudo != "::after" {
			return false
		}
		compound = compound[:idx]
		if compound == "" {
			return true
		}
	}

	i := 0
	for i < len(compound) {
		c := compound[i]
		switch c {
		case '*':
			i++
		case '#':
			i++
			start := i
			for i < len(compound) && isIdentChar(compound[i]) {
				i++
			}
			wantID := compound[start:i]
			if n.Data.ID() != wantID {
				return false
			}
		case '.':
			i++
			start := i
			for i < len(compound) && isIdentChar(compound[i]) {
				i++
			}
			wantClass := compound[start:i]
			if !n.Data.HasClass(wantClass) {
				return false
			}
		case '[':
			end := findMatchingBracket(compound, i)
			if end < 0 {
				return false
			}
			attrExpr := strings.TrimSpace(compound[i+1 : end])
			i = end + 1
			if !matchAttrSelector(&n.Data, attrExpr) {
				return false
			}
		case ':':
			i++
			start := i
			for i < len(compound) && isIdentChar(compound[i]) {
				i++
			}
			pseudoName := strings.ToLower(compound[start:i])
			var pseudoArg string
			if i < len(compound) && compound[i] == '(' {
				end := findMatchingParen(compound, i)
				if end < 0 {
					return false
				}
				pseudoArg = compound[i+1 : end]
				i = end + 1
			}
			if !doc.matchPseudoClass(id, pseudoName, pseudoArg, scope) {
				return false
			}
		default:
			start := i
			for i < len(compound) && isIdentChar(compound[i]) {
				i++
			}
			if i == start {
				return false
			}
			wantTag := strings.ToLower(compound[start:i])
			if n.Data.Tag != wantTag {
				return false
			}
		}
	}
	return true
}

func matchAttrSelector(d *NodeData, expr string) bool {
	if expr == "" {
		return false
	}
	ops := []string{"~=", "|=", "^=", "$=", "*=", "="}
	opFound := ""
	opPos := -1
	for _, op := range ops {
		if pos := strings.Index(expr, op); pos >= 0 {
			if opPos < 0 || pos < opPos {
				opPos = pos
				opFound = op
			}
		}
	}
	if opPos < 0 {
		_, ok := d.Attr(strings.TrimSpace(expr))
		return ok
	}
	attrName := strings.TrimSpace(expr[:opPos])
	rhs := strings.TrimSpace(expr[opPos+len(opFound):])
	caseInsensitive := false
	if strings.HasSuffix(rhs, " i") || strings.HasSuffix(rhs, " I") {
		caseInsensitive = true
		rhs = strings.TrimSpace(rhs[:len(rhs)-2])
	} else if strings.HasSuffix(rhs, " s") || strings.HasSuffix(rhs, " S") {
		rhs = strings.TrimSpace(rhs[:len(rhs)-2])
	}
	if len(rhs) >= 2 && ((rhs[0] == '"' && rhs[len(rhs)-1] == '"') || (rhs[0] == '\'' && rhs[len(rhs)-1] == '\'')) {
		rhs = rhs[1 : len(rhs)-1]
	}
	actual, ok := d.Attr(attrName)
	if !ok {
		return false
	}
	if caseInsensitive {
		actual = strings.ToLower(actual)
		rhs = strings.ToLower(rhs)
	}
	switch opFound {
	case "=":
		return actual == rhs
	case "~=":
		if rhs == "" {
			return false
		}
		for _, w := range strings.Fields(actual) {
			if w == rhs {
				return true
			}
		}
		return false
	case "|=":
		return actual == rhs || strings.HasPrefix(actual, rhs+"-")
	case "^=":
		return rhs != "" && strings.HasPrefix(actual, rhs)
	case "$=":
		return rhs != "" && strings.HasSuffix(actual, rhs)
	case "*=":
		return rhs != "" && strings.Contains(actual, rhs)
	}
	return false
}

func (doc *Document) matchPseudoClass(id NodeId, name, arg string, scope NodeId) bool {
	n := doc.Node(id)
	if n == nil {
		return false
	}
	switch name {
	case "root":
		return id == doc.HTML()
	case "scope":
		if scope != 0 {
			return id == scope
		}
		return id == doc.HTML()
	case "empty":
		for _, c := range n.Children {
			cn := doc.Node(c)
			if cn == nil {
				continue
			}
			if cn.Data.Kind == NodeKindElement || (cn.Data.Kind == NodeKindText && cn.Data.Text != "") {
				return false
			}
		}
		return true
	case "first-child":
		idx, _ := doc.elementSiblingPosition(id)
		return idx == 1
	case "last-child":
		idx, total := doc.elementSiblingPosition(id)
		return idx > 0 && idx == total
	case "only-child":
		_, total := doc.elementSiblingPosition(id)
		return total == 1
	case "nth-child":
		idx, _ := doc.elementSiblingPosition(id)
		return matchAnPlusB(arg, idx)
	case "nth-last-child":
		idx, total := doc.elementSiblingPosition(id)
		if idx == 0 {
			return false
		}
		return matchAnPlusB(arg, total-idx+1)
	case "is", "where":
		return doc.MatchesSelectorWithScope(id, arg, scope)
	case "not":
		return !doc.MatchesSelectorWithScope(id, arg, scope)
	case "has":
		return doc.matchHas(id, arg)
	case "checked":
		_, ok := n.Data.Attr("checked")
		return ok
	case "disabled":
		_, ok := n.Data.Attr("disabled")
		return ok
	case "enabled":
		_, dis := n.Data.Attr("disabled")
		return !dis
	case "required":
		_, req := n.Data.Attr("required")
		return req
	case "optional":
		_, req := n.Data.Attr("required")
		return !req
	case "valid":
		return doc.IsControlValid(id)
	case "invalid":
		return !doc.IsControlValid(id)
	case "in-range":
		return doc.IsControlInRange(id)
	case "out-of-range":
		return !doc.IsControlInRange(id)
	case "hover":
		return doc.hoveredNode != nil && *doc.hoveredNode == id
	case "active":
		return doc.activeNode != nil && *doc.activeNode == id
	case "focus", "focus-visible":
		return doc.focusedNode != nil && *doc.focusedNode == id
	case "focus-within":
		if doc.focusedNode == nil {
			return false
		}
		return doc.isAncestorOrSelf(id, *doc.focusedNode)
	case "target":
		return doc.targetFragment != "" && n.Data.ID() == doc.targetFragment
	case "link", "any-link":
		if n.Data.Tag == "a" || n.Data.Tag == "area" {
			_, ok := n.Data.Attr("href")
			return ok
		}
		return false
	case "open":
		_, ok := n.Data.Attr("open")
		return ok
	case "closed":
		_, ok := n.Data.Attr("open")
		return !ok
	case "defined":
		if !strings.Contains(n.Data.Tag, "-") {
			return true
		}
		for _, ce := range doc.customElements {
			if ce == n.Data.Tag {
				return true
			}
		}
		return false
	case "host":
		_, isShadow := doc.shadowRoots[id]
		if !isShadow {
			return false
		}
		if strings.TrimSpace(arg) == "" {
			return true
		}
		return doc.MatchesSelectorWithScope(id, arg, id)
	}
	return false
}

func (doc *Document) isAncestorOrSelf(anc, target NodeId) bool {
	cur := target
	for {
		if cur == anc {
			return true
		}
		p, ok := doc.Parent(cur)
		if !ok {
			return false
		}
		cur = p
	}
}

func (doc *Document) matchHas(anchor NodeId, relSelectorList string) bool {
	parts := splitTopLevelCommas(relSelectorList)
	for _, rel := range parts {
		rel = strings.TrimSpace(rel)
		if rel == "" {
			continue
		}
		var found bool
		var walk func(NodeId)
		walk = func(cur NodeId) {
			if found {
				return
			}
			for _, c := range doc.Children(cur) {
				if doc.MatchesSelectorWithScope(c, rel, anchor) {
					found = true
					return
				}
				walk(c)
			}
		}
		walk(anchor)
		if found {
			return true
		}
	}
	return false
}

func (doc *Document) elementSiblingPosition(id NodeId) (int, int) {
	p, ok := doc.Parent(id)
	if !ok {
		return 1, 1
	}
	idx := 0
	total := 0
	for _, c := range doc.Children(p) {
		cn := doc.Node(c)
		if cn != nil && cn.Data.Kind == NodeKindElement {
			total++
			if c == id {
				idx = total
			}
		}
	}
	return idx, total
}

func matchAnPlusB(expr string, index int) bool {
	if index <= 0 {
		return false
	}
	s := strings.ToLower(strings.ReplaceAll(strings.TrimSpace(expr), " ", ""))
	if idx := strings.Index(s, "of"); idx >= 0 {
		s = s[:idx]
	}
	if s == "odd" {
		return index%2 == 1
	}
	if s == "even" {
		return index%2 == 0
	}
	nPos := strings.IndexByte(s, 'n')
	if nPos < 0 {
		v, err := strconv.Atoi(s)
		return err == nil && index == v
	}
	aStr := s[:nPos]
	bStr := s[nPos+1:]
	var a int
	switch aStr {
	case "", "+":
		a = 1
	case "-":
		a = -1
	default:
		var err error
		a, err = strconv.Atoi(aStr)
		if err != nil {
			return false
		}
	}
	b := 0
	if bStr != "" {
		var err error
		b, err = strconv.Atoi(bStr)
		if err != nil {
			return false
		}
	}
	if a == 0 {
		return index == b
	}
	diff := index - b
	if (diff > 0 && a < 0) || (diff < 0 && a > 0) {
		return false
	}
	return diff%a == 0
}

func splitTopLevelCommas(s string) []string {
	var out []string
	start := 0
	depthParen := 0
	depthBracket := 0
	var quote byte
	for i := 0; i < len(s); i++ {
		c := s[i]
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
				out = append(out, s[start:i])
				start = i + 1
			}
		}
	}
	out = append(out, s[start:])
	return out
}

func findMatchingBracket(s string, start int) int {
	var quote byte
	for i := start + 1; i < len(s); i++ {
		c := s[i]
		if quote != 0 {
			if c == quote {
				quote = 0
			}
			continue
		}
		if c == '"' || c == '\'' {
			quote = c
		} else if c == ']' {
			return i
		}
	}
	return -1
}

func findMatchingParen(s string, start int) int {
	depth := 0
	var quote byte
	for i := start; i < len(s); i++ {
		c := s[i]
		if quote != 0 {
			if c == quote {
				quote = 0
			}
			continue
		}
		if c == '"' || c == '\'' {
			quote = c
		} else if c == '(' {
			depth++
		} else if c == ')' {
			depth--
			if depth == 0 {
				return i
			}
		}
	}
	return -1
}

func isIdentChar(b byte) bool {
	return (b >= 'a' && b <= 'z') || (b >= 'A' && b <= 'Z') ||
		(b >= '0' && b <= '9') || b == '-' || b == '_' || b >= 0x80
}
