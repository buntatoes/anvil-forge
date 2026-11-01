// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package dom

import (
	"html"
	"strings"
)

var voidElements = map[string]bool{
	"area": true, "base": true, "br": true, "col": true, "embed": true,
	"hr": true, "img": true, "input": true, "link": true, "meta": true,
	"param": true, "source": true, "track": true, "wbr": true,
}

var rawTextElements = map[string]bool{
	"script": true, "style": true, "textarea": true, "title": true, "noscript": true,
}

var headElements = map[string]bool{
	"meta": true, "title": true, "link": true, "style": true, "base": true,
}

var formattingElements = map[string]bool{
	"a": true, "b": true, "big": true, "code": true, "em": true, "font": true,
	"i": true, "nobr": true, "s": true, "small": true, "strike": true,
	"strong": true, "tt": true, "u": true,
}

var pClosingBlocks = map[string]bool{
	"address": true, "article": true, "aside": true, "blockquote": true,
	"details": true, "div": true, "dl": true, "fieldset": true, "figcaption": true,
	"figure": true, "footer": true, "form": true, "h1": true, "h2": true,
	"h3": true, "h4": true, "h5": true, "h6": true, "header": true, "hgroup": true,
	"hr": true, "main": true, "menu": true, "nav": true, "ol": true, "p": true,
	"pre": true, "section": true, "table": true, "ul": true,
}

var headingTags = map[string]bool{
	"h1": true, "h2": true, "h3": true, "h4": true, "h5": true, "h6": true,
}

func ParseHTML(src string) *Document {
	doc := NewDocument()
	stack := []NodeId{doc.Body()}
	var activeFormatting []NodeData
	inHead := false

	i := 0
	n := len(src)
	for i < n && doc.NodeCount() < MaxDomNodes {
		lt := strings.IndexByte(src[i:], '<')
		if lt < 0 {
			appendTextWithFoster(doc, stack, html.UnescapeString(src[i:]))
			break
		}
		if lt > 0 {
			appendTextWithFoster(doc, stack, html.UnescapeString(src[i:i+lt]))
			i += lt
		}
		// Comments <!-- ... -->
		if strings.HasPrefix(src[i:], "<!--") {
			end := strings.Index(src[i+4:], "-->")
			if end < 0 {
				break
			}
			i += 4 + end + 3
			continue
		}
		// Doctype or CDATA <! ... >
		if strings.HasPrefix(src[i:], "<!") || strings.HasPrefix(src[i:], "<?") {
			end := strings.IndexByte(src[i:], '>')
			if end < 0 {
				break
			}
			i += end + 1
			continue
		}
		tagEnd := findTagClose(src, i+1)
		if tagEnd < 0 {
			appendTextWithFoster(doc, stack, html.UnescapeString(src[i:]))
			break
		}
		rawTag := strings.TrimSpace(src[i+1 : tagEnd])
		i = tagEnd + 1
		if rawTag == "" {
			continue
		}
		// Closing tag </tag>
		if strings.HasPrefix(rawTag, "/") {
			closeName := strings.ToLower(strings.TrimSpace(rawTag[1:]))
			if sp := strings.IndexAny(closeName, " \t\r\n/"); sp >= 0 {
				closeName = closeName[:sp]
			}
			if closeName == "head" {
				inHead = false
				continue
			}
			if closeName == "html" || closeName == "body" {
				continue
			}
			// Check if in stack
			foundIdx := -1
			for idx := len(stack) - 1; idx >= 1; idx-- {
				if nd := doc.Node(stack[idx]); nd != nil && nd.Data.Tag == closeName {
					foundIdx = idx
					break
				}
			}
			if foundIdx >= 1 {
				// Misnested formatting elements inside stack above foundIdx
				var reopen []NodeData
				for idx := foundIdx + 1; idx < len(stack); idx++ {
					if nd := doc.Node(stack[idx]); nd != nil && formattingElements[nd.Data.Tag] {
						reopen = append(reopen, nd.Data)
					}
				}
				stack = stack[:foundIdx]
				for _, fmtData := range reopen {
					if len(stack) < MaxDomDepth {
						reopened := doc.allocNode(NodeData{
							Kind:  NodeKindElement,
							Tag:   fmtData.Tag,
							Attrs: append([]Attr(nil), fmtData.Attrs...),
						})
						_ = doc.Append(stack[len(stack)-1], reopened)
						stack = append(stack, reopened)
					}
				}
			}
			_ = activeFormatting
			continue
		}

		selfClosing := false
		if strings.HasSuffix(rawTag, "/") {
			selfClosing = true
			rawTag = strings.TrimSpace(rawTag[:len(rawTag)-1])
		}
		tagName, attrs := parseStartTag(rawTag)
		if tagName == "" {
			continue
		}
		if tagName == "html" {
			copyAttrsToNode(doc, doc.HTML(), attrs)
			continue
		}
		if tagName == "head" {
			inHead = true
			copyAttrsToNode(doc, doc.Head(), attrs)
			continue
		}
		if tagName == "body" {
			inHead = false
			copyAttrsToNode(doc, doc.Body(), attrs)
			continue
		}

		// Implicit auto-closures (WHATWG HTML5 tree builder rules)
		if pClosingBlocks[tagName] {
			closeIfOpen(doc, &stack, "p")
		}
		if headingTags[tagName] {
			if top := doc.Node(stack[len(stack)-1]); top != nil && headingTags[top.Data.Tag] {
				stack = stack[:len(stack)-1]
			}
		}
		if tagName == "li" {
			closeIfTopIn(doc, &stack, "li", "ul", "ol", "menu")
		} else if tagName == "dt" || tagName == "dd" {
			closeIfTopIn(doc, &stack, "dt", "dl")
			closeIfTopIn(doc, &stack, "dd", "dl")
		} else if tagName == "tr" {
			closeIfTopIn(doc, &stack, "td", "table")
			closeIfTopIn(doc, &stack, "th", "table")
			closeIfTopIn(doc, &stack, "tr", "table")
		} else if tagName == "td" || tagName == "th" {
			closeIfTopIn(doc, &stack, "td", "tr", "table")
			closeIfTopIn(doc, &stack, "th", "tr", "table")
		}

		el := doc.CreateElement(tagName)
		copyAttrsToNode(doc, el, attrs)

		targetParent := stack[len(stack)-1]
		if (inHead || (len(stack) == 1 && headElements[tagName])) && tagName != "body" {
			targetParent = doc.Head()
		} else if isTableElementExpectingRow(doc, targetParent) && !isTableStructureTag(tagName) {
			// Foster parenting: insert before the table element
			if tbl, ok := findAncestorTable(doc, stack); ok {
				if tblParent, hasParent := doc.Parent(tbl); hasParent {
					_ = doc.InsertBefore(tblParent, el, tbl)
					targetParent = 0
				}
			}
		}
		if targetParent != 0 || (inHead && targetParent == doc.Head()) {
			_ = doc.Append(targetParent, el)
		}

		if rawTextElements[tagName] && !selfClosing {
			closeToken := "</" + tagName
			endIdx := indexCaseInsensitive(src[i:], closeToken)
			var rawContent string
			if endIdx >= 0 {
				rawContent = src[i : i+endIdx]
				afterClose := strings.IndexByte(src[i+endIdx:], '>')
				if afterClose >= 0 {
					i += endIdx + afterClose + 1
				} else {
					i = n
				}
			} else {
				rawContent = src[i:]
				i = n
			}
			if tagName == "textarea" || tagName == "title" {
				rawContent = html.UnescapeString(rawContent)
			}
			if rawContent != "" {
				txt := doc.CreateText(rawContent)
				_ = doc.Append(el, txt)
			}
			continue
		}

		if !voidElements[tagName] && !selfClosing && len(stack) < MaxDomDepth && targetParent != doc.Head() {
			stack = append(stack, el)
		}
	}
	doc.dirtyNodes = nil
	doc.generation = 0
	return doc
}

func appendTextWithFoster(doc *Document, stack []NodeId, text string) {
	if text == "" {
		return
	}
	top := stack[len(stack)-1]
	if isTableElementExpectingRow(doc, top) && strings.TrimSpace(text) != "" {
		if tbl, ok := findAncestorTable(doc, stack); ok {
			if tblParent, hasParent := doc.Parent(tbl); hasParent {
				txt := doc.CreateText(text)
				_ = doc.InsertBefore(tblParent, txt, tbl)
				return
			}
		}
	}
	// Coalesce adjacent text nodes
	children := doc.Children(top)
	if len(children) > 0 {
		last := doc.Node(children[len(children)-1])
		if last != nil && last.Data.Kind == NodeKindText {
			last.Data.Text += text
			return
		}
	}
	txt := doc.CreateText(text)
	_ = doc.Append(top, txt)
}

func isTableElementExpectingRow(doc *Document, id NodeId) bool {
	n := doc.Node(id)
	if n == nil || n.Data.Kind != NodeKindElement {
		return false
	}
	switch n.Data.Tag {
	case "table", "tbody", "thead", "tfoot", "tr":
		return true
	default:
		return false
	}
}

func isTableStructureTag(tag string) bool {
	switch tag {
	case "caption", "colgroup", "col", "thead", "tbody", "tfoot", "tr", "td", "th", "script", "style", "template":
		return true
	default:
		return false
	}
}

func findAncestorTable(doc *Document, stack []NodeId) (NodeId, bool) {
	for idx := len(stack) - 1; idx >= 1; idx-- {
		if n := doc.Node(stack[idx]); n != nil && n.Data.Tag == "table" {
			return stack[idx], true
		}
	}
	return 0, false
}

func closeIfOpen(doc *Document, stack *[]NodeId, tag string) {
	for idx := len(*stack) - 1; idx >= 1; idx-- {
		n := doc.Node((*stack)[idx])
		if n != nil && n.Data.Tag == tag {
			*stack = (*stack)[:idx]
			return
		}
	}
}

func closeIfTopIn(doc *Document, stack *[]NodeId, target string, scopes ...string) {
	for idx := len(*stack) - 1; idx >= 1; idx-- {
		n := doc.Node((*stack)[idx])
		if n == nil {
			continue
		}
		if n.Data.Tag == target {
			*stack = (*stack)[:idx]
			return
		}
		for _, s := range scopes {
			if n.Data.Tag == s {
				return
			}
		}
	}
}

func findTagClose(s string, start int) int {
	var quote byte
	for i := start; i < len(s); i++ {
		c := s[i]
		if quote != 0 {
			if c == quote {
				quote = 0
			}
		} else if c == '"' || c == '\'' {
			quote = c
		} else if c == '>' {
			return i
		}
	}
	return -1
}

func parseStartTag(raw string) (string, []Attr) {
	i := 0
	n := len(raw)
	for i < n && isSpace(raw[i]) {
		i++
	}
	start := i
	for i < n && !isSpace(raw[i]) && raw[i] != '/' && raw[i] != '>' {
		i++
	}
	tag := strings.ToLower(raw[start:i])
	var attrs []Attr
	for i < n && len(attrs) < MaxAttrsPerElement {
		for i < n && (isSpace(raw[i]) || raw[i] == '/') {
			i++
		}
		if i >= n {
			break
		}
		aStart := i
		for i < n && !isSpace(raw[i]) && raw[i] != '=' && raw[i] != '/' {
			i++
		}
		name := strings.ToLower(raw[aStart:i])
		if name == "" {
			i++
			continue
		}
		for i < n && isSpace(raw[i]) {
			i++
		}
		val := ""
		if i < n && raw[i] == '=' {
			i++
			for i < n && isSpace(raw[i]) {
				i++
			}
			if i < n && (raw[i] == '"' || raw[i] == '\'') {
				q := raw[i]
				i++
				vStart := i
				for i < n && raw[i] != q {
					i++
				}
				val = html.UnescapeString(raw[vStart:i])
				if i < n && raw[i] == q {
					i++
				}
			} else {
				vStart := i
				for i < n && !isSpace(raw[i]) {
					i++
				}
				val = html.UnescapeString(raw[vStart:i])
			}
		}
		attrs = append(attrs, Attr{Name: name, Value: val})
	}
	return tag, attrs
}

func copyAttrsToNode(doc *Document, id NodeId, attrs []Attr) {
	n := doc.Node(id)
	if n == nil {
		return
	}
	for _, a := range attrs {
		n.Data.SetAttr(a.Name, a.Value)
	}
}

func isSpace(b byte) bool {
	return b == ' ' || b == '\t' || b == '\r' || b == '\n'
}

func indexCaseInsensitive(s, sub string) int {
	return strings.Index(strings.ToLower(s), strings.ToLower(sub))
}
