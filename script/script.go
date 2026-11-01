// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package script

import (
	"fmt"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/buntatoes/anvil-engine/dom"
)

const (
	MaxExecutionSteps  = 200000
	MaxScriptSourceLen = 256 * 1024
	MaxScriptFetchBody = 2560 * 1024
	MaxTimers          = 128
	MaxMicrotasks      = 8192
)

type ScriptFetchReply struct {
	Status uint16
	Body   string
}

type FetchHostFunc func(targetURL, pageURL string) ScriptFetchReply

func LimitScriptFetchBody(body string) string {
	if len(body) <= MaxScriptFetchBody {
		return body
	}
	end := MaxScriptFetchBody
	for end > 0 && !utf8.RuneStart(body[end]) {
		end--
	}
	return body[:end]
}

type CinderVmTelemetry struct {
	KernelVersion       uint32
	BytecodeOpsExecuted uint32
	HeapObjectsLive     uint32
	HeapBytesAllocated  uint32
	GcCollections       uint32
	ShapesAllocated     uint32
	IcHits              uint32
	IcMisses            uint32
	PolymorphicIcSites  uint32
	HotLoopsAccelerated uint32
	NativeCompilations  uint32
}

type eventListener struct {
	eventType string
	handler   string
}

type ScriptRuntime struct {
	PageURL              string
	ConsoleLogs          []string
	LocalStorage         map[string]string
	SessionStorage       map[string]string
	Globals              map[string]string
	FetchMocks           map[string]ScriptFetchReply
	FetchHost            FetchHostFunc
	Telemetry            CinderVmTelemetry
	listeners            map[dom.NodeId][]eventListener
	unhandledRejections  int
	abortReason          string
	lastDefaultPrevented bool
}

func NewScriptRuntime() *ScriptRuntime {
	return &ScriptRuntime{
		PageURL:        "https://example.invalid/",
		LocalStorage:   make(map[string]string),
		SessionStorage: make(map[string]string),
		Globals:        make(map[string]string),
		FetchMocks:     make(map[string]ScriptFetchReply),
		Telemetry: CinderVmTelemetry{
			KernelVersion: 24,
		},
		listeners: make(map[dom.NodeId][]eventListener),
	}
}

func (rt *ScriptRuntime) SetPageOrigin(url string) {
	rt.PageURL = url
}

func (rt *ScriptRuntime) SetFetchHost(fn FetchHostFunc) {
	rt.FetchHost = fn
}

func (rt *ScriptRuntime) RegisterFetchMock(url string, status uint16, body string) {
	rt.FetchMocks[url] = ScriptFetchReply{Status: status, Body: LimitScriptFetchBody(body)}
}

func (rt *ScriptRuntime) UnhandledRejections() int {
	return rt.unhandledRejections
}

func (rt *ScriptRuntime) AbortReason() string {
	return rt.abortReason
}

func (rt *ScriptRuntime) LastEventDefaultPrevented() bool {
	return rt.lastDefaultPrevented
}

func (rt *ScriptRuntime) RunDocumentScripts(doc *dom.Document) error {
	scripts := doc.GetElementsByTagName("script")
	for _, sid := range scripts {
		n := doc.Node(sid)
		if n == nil {
			continue
		}
		if _, hasSrc := n.Data.Attr("src"); hasSrc {
			continue
		}
		code := doc.TextContent(sid)
		if strings.TrimSpace(code) != "" {
			if err := rt.EvalScript(doc, code); err != nil {
				return err
			}
		}
	}
	return nil
}

func (rt *ScriptRuntime) HydrateInteractiveBindings(doc *dom.Document) {
	all := doc.GetElementsByTagName("*")
	for _, id := range all {
		n := doc.Node(id)
		if n == nil {
			continue
		}
		for _, ev := range []string{"click", "input", "change", "submit", "keydown"} {
			if handler, ok := n.Data.Attr("on" + ev); ok && strings.TrimSpace(handler) != "" {
				rt.listeners[id] = append(rt.listeners[id], eventListener{
					eventType: ev,
					handler:   handler,
				})
			}
		}
	}
}

func (rt *ScriptRuntime) DispatchClick(doc *dom.Document, target dom.NodeId) error {
	return rt.dispatchEventOnNode(doc, target, "click")
}

func (rt *ScriptRuntime) DispatchInput(doc *dom.Document, target dom.NodeId, val string) error {
	if n := doc.Node(target); n != nil {
		n.Data.SetAttr("value", val)
	}
	return rt.dispatchEventOnNode(doc, target, "input")
}

func (rt *ScriptRuntime) DispatchSubmit(doc *dom.Document, target dom.NodeId) error {
	return rt.dispatchEventOnNode(doc, target, "submit")
}

func (rt *ScriptRuntime) DispatchKeydown(doc *dom.Document, target dom.NodeId, key string) error {
	rt.Globals["__last_key"] = key
	return rt.dispatchEventOnNode(doc, target, "keydown")
}

func (rt *ScriptRuntime) NotifyLayoutAndScroll(doc *dom.Document) {
	_ = doc
}

func (rt *ScriptRuntime) dispatchEventOnNode(doc *dom.Document, target dom.NodeId, evType string) error {
	rt.lastDefaultPrevented = false
	cur := target
	for {
		if n := doc.Node(cur); n != nil {
			if inline, ok := n.Data.Attr("on" + evType); ok && strings.TrimSpace(inline) != "" {
				if err := rt.EvalScript(doc, inline); err != nil {
					return err
				}
			}
		}
		for _, l := range rt.listeners[cur] {
			if l.eventType == evType {
				if err := rt.EvalScript(doc, l.handler); err != nil {
					return err
				}
			}
		}
		p, ok := doc.Parent(cur)
		if !ok {
			break
		}
		cur = p
	}
	return nil
}

func (rt *ScriptRuntime) EvalScript(doc *dom.Document, src string) error {
	if len(src) > MaxScriptSourceLen {
		rt.abortReason = "script source exceeds 256 KiB cap"
		return fmt.Errorf("%s", rt.abortReason)
	}
	stmts := splitJSStatements(src)
	steps := 0
	for _, stmt := range stmts {
		steps++
		rt.Telemetry.BytecodeOpsExecuted++
		if steps > MaxExecutionSteps {
			rt.abortReason = "execution step budget exceeded"
			return fmt.Errorf("%s", rt.abortReason)
		}
		if err := rt.execStatement(doc, strings.TrimSpace(stmt)); err != nil {
			return err
		}
	}
	return nil
}

func (rt *ScriptRuntime) execStatement(doc *dom.Document, stmt string) error {
	if stmt == "" || strings.HasPrefix(stmt, "//") {
		return nil
	}
	if strings.Contains(stmt, "preventDefault()") {
		rt.lastDefaultPrevented = true
	}
	if strings.HasPrefix(stmt, "Promise.reject(") {
		rt.unhandledRejections++
		return nil
	}
	if strings.HasPrefix(stmt, "console.log(") && strings.HasSuffix(stmt, ")") {
		arg := strings.TrimSpace(stmt[len("console.log(") : len(stmt)-1])
		rt.ConsoleLogs = append(rt.ConsoleLogs, rt.evalExpr(doc, arg))
		return nil
	}
	if strings.HasPrefix(stmt, "localStorage.setItem(") && strings.HasSuffix(stmt, ")") {
		inner := stmt[len("localStorage.setItem(") : len(stmt)-1]
		k, v := splitTwoArgs(inner)
		rt.LocalStorage[rt.evalExpr(doc, k)] = rt.evalExpr(doc, v)
		return nil
	}
	if strings.HasPrefix(stmt, "localStorage.removeItem(") && strings.HasSuffix(stmt, ")") {
		inner := stmt[len("localStorage.removeItem(") : len(stmt)-1]
		delete(rt.LocalStorage, rt.evalExpr(doc, inner))
		return nil
	}
	if strings.HasPrefix(stmt, "document.title") {
		if eq := strings.IndexByte(stmt, '='); eq > 0 {
			val := rt.evalExpr(doc, stmt[eq+1:])
			titles := doc.GetElementsByTagName("title")
			if len(titles) > 0 {
				doc.SetTextContent(titles[0], val)
			} else {
				t := doc.CreateElement("title")
				doc.SetTextContent(t, val)
				_ = doc.Append(doc.Head(), t)
			}
		}
		return nil
	}
	// document.getElementById('x').textContent = ... / .innerHTML = ... / .setAttribute(...) / .addEventListener(...)
	if targetID, rest, ok := rt.resolveDOMTarget(doc, stmt); ok {
		return rt.applyDOMOperation(doc, targetID, rest)
	}
	// Variable declarations: const/let/var name = expr
	for _, kw := range []string{"const ", "let ", "var "} {
		if strings.HasPrefix(stmt, kw) {
			rest := strings.TrimSpace(stmt[len(kw):])
			if eq := strings.IndexByte(rest, '='); eq > 0 {
				name := strings.TrimSpace(rest[:eq])
				rhs := strings.TrimSpace(rest[eq+1:])
				rt.Globals[name] = rt.evalExpr(doc, rhs)
			}
			return nil
		}
	}
	// Plain assignment: name = expr
	if eq := strings.IndexByte(stmt, '='); eq > 0 && !strings.Contains(stmt[:eq], ".") {
		name := strings.TrimSpace(stmt[:eq])
		rhs := strings.TrimSpace(stmt[eq+1:])
		rt.Globals[name] = rt.evalExpr(doc, rhs)
	}
	return nil
}

func (rt *ScriptRuntime) resolveDOMTarget(doc *dom.Document, stmt string) (dom.NodeId, string, bool) {
	if doc == nil {
		return 0, "", false
	}
	if strings.HasPrefix(stmt, "document.getElementById(") {
		end := strings.IndexByte(stmt, ')')
		if end > 0 {
			idArg := trimQuotes(stmt[len("document.getElementById("):end])
			if nid, ok := doc.GetElementByID(idArg); ok {
				return nid, strings.TrimSpace(stmt[end+1:]), true
			}
		}
	}
	if strings.HasPrefix(stmt, "document.querySelector(") {
		end := strings.IndexByte(stmt, ')')
		if end > 0 {
			selArg := trimQuotes(stmt[len("document.querySelector("):end])
			if nid, ok := doc.QuerySelector(selArg); ok {
				return nid, strings.TrimSpace(stmt[end+1:]), true
			}
		}
	}
	if strings.HasPrefix(stmt, "document.body.") {
		return doc.Body(), "." + stmt[len("document.body."):], true
	}
	return 0, "", false
}

func (rt *ScriptRuntime) applyDOMOperation(doc *dom.Document, id dom.NodeId, op string) error {
	n := doc.Node(id)
	if n == nil {
		return nil
	}
	if strings.HasPrefix(op, ".textContent") {
		if eq := strings.IndexByte(op, '='); eq > 0 {
			doc.SetTextContent(id, rt.evalExpr(doc, op[eq+1:]))
		}
		return nil
	}
	if strings.HasPrefix(op, ".innerHTML") {
		if eq := strings.IndexByte(op, '='); eq > 0 {
			doc.SetInnerHTML(id, rt.evalExpr(doc, op[eq+1:]))
		}
		return nil
	}
	if strings.HasPrefix(op, ".setAttribute(") && strings.HasSuffix(op, ")") {
		inner := op[len(".setAttribute(") : len(op)-1]
		k, v := splitTwoArgs(inner)
		n.Data.SetAttr(rt.evalExpr(doc, k), rt.evalExpr(doc, v))
		return nil
	}
	if strings.HasPrefix(op, ".removeAttribute(") && strings.HasSuffix(op, ")") {
		inner := op[len(".removeAttribute(") : len(op)-1]
		n.Data.RemoveAttr(rt.evalExpr(doc, inner))
		return nil
	}
	if strings.HasPrefix(op, ".classList.add(") && strings.HasSuffix(op, ")") {
		inner := op[len(".classList.add(") : len(op)-1]
		n.Data.AddClass(rt.evalExpr(doc, inner))
		return nil
	}
	if strings.HasPrefix(op, ".classList.remove(") && strings.HasSuffix(op, ")") {
		inner := op[len(".classList.remove(") : len(op)-1]
		n.Data.RemoveClass(rt.evalExpr(doc, inner))
		return nil
	}
	if strings.HasPrefix(op, ".classList.toggle(") && strings.HasSuffix(op, ")") {
		inner := op[len(".classList.toggle(") : len(op)-1]
		n.Data.ToggleClass(rt.evalExpr(doc, inner))
		return nil
	}
	if strings.HasPrefix(op, ".addEventListener(") && strings.HasSuffix(op, ")") {
		inner := op[len(".addEventListener(") : len(op)-1]
		ev, handler := splitTwoArgs(inner)
		evName := rt.evalExpr(doc, ev)
		body := extractFunctionBody(handler)
		rt.listeners[id] = append(rt.listeners[id], eventListener{
			eventType: evName,
			handler:   body,
		})
		return nil
	}
	return nil
}

func (rt *ScriptRuntime) evalExpr(doc *dom.Document, expr string) string {
	e := strings.TrimSpace(expr)
	if len(e) >= 2 && ((e[0] == '"' && e[len(e)-1] == '"') || (e[0] == '\'' && e[len(e)-1] == '\'') || (e[0] == '`' && e[len(e)-1] == '`')) {
		return e[1 : len(e)-1]
	}
	if strings.HasPrefix(e, "localStorage.getItem(") && strings.HasSuffix(e, ")") {
		k := rt.evalExpr(doc, e[len("localStorage.getItem("):len(e)-1])
		return rt.LocalStorage[k]
	}
	if doc != nil && strings.HasPrefix(e, "document.getElementById(") && strings.HasSuffix(e, ").textContent") {
		end := strings.IndexByte(e, ')')
		if end > 0 {
			idArg := trimQuotes(e[len("document.getElementById("):end])
			if nid, ok := doc.GetElementByID(idArg); ok {
				return doc.TextContent(nid)
			}
		}
	}
	if v, ok := rt.Globals[e]; ok {
		return v
	}
	if plus := strings.IndexByte(e, '+'); plus > 0 {
		lhs := rt.evalExpr(doc, e[:plus])
		rhs := rt.evalExpr(doc, e[plus+1:])
		n1, err1 := strconv.ParseFloat(lhs, 64)
		n2, err2 := strconv.ParseFloat(rhs, 64)
		if err1 == nil && err2 == nil {
			return strconv.FormatFloat(n1+n2, 'f', -1, 64)
		}
		return lhs + rhs
	}
	return e
}

func splitJSStatements(src string) []string {
	var out []string
	start := 0
	depthParen := 0
	depthBrace := 0
	var quote byte
	for i := 0; i < len(src); i++ {
		c := src[i]
		if quote != 0 {
			if c == '\\' && i+1 < len(src) {
				i++
			} else if c == quote {
				quote = 0
			}
			continue
		}
		switch c {
		case '"', '\'', '`':
			quote = c
		case '(':
			depthParen++
		case ')':
			if depthParen > 0 {
				depthParen--
			}
		case '{':
			depthBrace++
		case '}':
			if depthBrace > 0 {
				depthBrace--
			}
		case ';', '\n':
			if depthParen == 0 && depthBrace == 0 {
				s := strings.TrimSpace(src[start:i])
				if s != "" {
					out = append(out, s)
				}
				start = i + 1
			}
		}
	}
	if s := strings.TrimSpace(src[start:]); s != "" {
		out = append(out, s)
	}
	return out
}

func splitTwoArgs(inner string) (string, string) {
	depth := 0
	var quote byte
	for i := 0; i < len(inner); i++ {
		c := inner[i]
		if quote != 0 {
			if c == quote {
				quote = 0
			}
			continue
		}
		switch c {
		case '"', '\'', '`':
			quote = c
		case '(', '{', '[':
			depth++
		case ')', '}', ']':
			if depth > 0 {
				depth--
			}
		case ',':
			if depth == 0 {
				return strings.TrimSpace(inner[:i]), strings.TrimSpace(inner[i+1:])
			}
		}
	}
	return strings.TrimSpace(inner), ""
}

func extractFunctionBody(fn string) string {
	open := strings.IndexByte(fn, '{')
	close := strings.LastIndexByte(fn, '}')
	if open >= 0 && close > open {
		return strings.TrimSpace(fn[open+1 : close])
	}
	if arrow := strings.Index(fn, "=>"); arrow >= 0 {
		return strings.TrimSpace(fn[arrow+2:])
	}
	return strings.TrimSpace(fn)
}

func trimQuotes(s string) string {
	return strings.Trim(strings.TrimSpace(s), "\"'`")
}
