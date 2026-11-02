// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package script

import (
	"fmt"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/buntatoes/anvil-forge/dom"
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

type HistoryEntry struct {
	State string
	Title string
	URL   string
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
	HistoryStack         []HistoryEntry
	HistoryIndex         int
	FormDataObjects      map[string]*dom.FormData
	SearchParamsObjects  map[string][]dom.FormFieldPair
	LastFetchURL         string
	LastFetchMethod      string
	LastFetchContentType string
	LastFetchBody        string
	listeners            map[dom.NodeId][]eventListener
	windowListeners      []eventListener
	unhandledRejections  int
	abortReason          string
	lastDefaultPrevented bool
}

func NewScriptRuntime() *ScriptRuntime {
	initialURL := "https://example.invalid/"
	return &ScriptRuntime{
		PageURL:        initialURL,
		LocalStorage:   make(map[string]string),
		SessionStorage: make(map[string]string),
		Globals:        make(map[string]string),
		FetchMocks:     make(map[string]ScriptFetchReply),
		Telemetry: CinderVmTelemetry{
			KernelVersion: 24,
		},
		HistoryStack: []HistoryEntry{
			{State: "null", Title: "", URL: initialURL},
		},
		HistoryIndex:        0,
		FormDataObjects:     make(map[string]*dom.FormData),
		SearchParamsObjects: make(map[string][]dom.FormFieldPair),
		listeners:           make(map[dom.NodeId][]eventListener),
	}
}

func (rt *ScriptRuntime) SetPageOrigin(url string) {
	url = strings.TrimSpace(url)
	if url == "" {
		return
	}
	rt.PageURL = url
	if len(rt.HistoryStack) <= 1 {
		rt.HistoryStack = []HistoryEntry{
			{State: "null", Title: "", URL: url},
		}
		rt.HistoryIndex = 0
	}
}

func (rt *ScriptRuntime) CurrentURL() string {
	return rt.PageURL
}

func (rt *ScriptRuntime) HistoryState() string {
	if len(rt.HistoryStack) == 0 || rt.HistoryIndex < 0 || rt.HistoryIndex >= len(rt.HistoryStack) {
		return "null"
	}
	return rt.HistoryStack[rt.HistoryIndex].State
}

func (rt *ScriptRuntime) HistoryLength() int {
	if len(rt.HistoryStack) == 0 {
		return 1
	}
	return len(rt.HistoryStack)
}

func (rt *ScriptRuntime) PushState(state, title, rawURL string) {
	resolved := rt.PageURL
	if strings.TrimSpace(rawURL) != "" && rawURL != "null" && rawURL != "undefined" {
		resolved = dom.ResolveFormAction(rt.PageURL, rawURL)
	}
	if strings.TrimSpace(state) == "" {
		state = "null"
	}
	if rt.HistoryIndex+1 < len(rt.HistoryStack) {
		rt.HistoryStack = rt.HistoryStack[:rt.HistoryIndex+1]
	}
	rt.HistoryStack = append(rt.HistoryStack, HistoryEntry{
		State: state,
		Title: title,
		URL:   resolved,
	})
	rt.HistoryIndex = len(rt.HistoryStack) - 1
	rt.PageURL = resolved
}

func (rt *ScriptRuntime) ReplaceState(state, title, rawURL string) {
	resolved := rt.PageURL
	if strings.TrimSpace(rawURL) != "" && rawURL != "null" && rawURL != "undefined" {
		resolved = dom.ResolveFormAction(rt.PageURL, rawURL)
	}
	if strings.TrimSpace(state) == "" {
		state = "null"
	}
	if len(rt.HistoryStack) == 0 {
		rt.HistoryStack = []HistoryEntry{{State: state, Title: title, URL: resolved}}
		rt.HistoryIndex = 0
	} else {
		rt.HistoryStack[rt.HistoryIndex] = HistoryEntry{
			State: state,
			Title: title,
			URL:   resolved,
		}
	}
	rt.PageURL = resolved
}

func (rt *ScriptRuntime) HistoryGo(doc *dom.Document, delta int) error {
	if len(rt.HistoryStack) == 0 {
		return nil
	}
	target := rt.HistoryIndex + delta
	if target < 0 || target >= len(rt.HistoryStack) || target == rt.HistoryIndex {
		return nil
	}
	rt.HistoryIndex = target
	entry := rt.HistoryStack[rt.HistoryIndex]
	rt.PageURL = entry.URL
	rt.Globals["__popstate_state"] = entry.State
	for _, l := range rt.windowListeners {
		if l.eventType == "popstate" {
			if err := rt.EvalScript(doc, l.handler); err != nil {
				return err
			}
		}
	}
	return nil
}

func (rt *ScriptRuntime) HistoryBack(doc *dom.Document) error {
	return rt.HistoryGo(doc, -1)
}

func (rt *ScriptRuntime) HistoryForward(doc *dom.Document) error {
	return rt.HistoryGo(doc, 1)
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
	if doc != nil {
		doc.SetAttribute(target, "value", val)
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
	if (strings.HasPrefix(stmt, "window.addEventListener(") || strings.HasPrefix(stmt, "addEventListener(")) && strings.HasSuffix(stmt, ")") {
		prefix := "window.addEventListener("
		if strings.HasPrefix(stmt, "addEventListener(") {
			prefix = "addEventListener("
		}
		inner := stmt[len(prefix) : len(stmt)-1]
		ev, handler := splitTwoArgs(inner)
		evName := rt.evalExpr(doc, ev)
		body := extractFunctionBody(handler)
		rt.windowListeners = append(rt.windowListeners, eventListener{
			eventType: evName,
			handler:   body,
		})
		return nil
	}
	// History API: history.pushState / replaceState / back / forward / go
	trimmedHistory := strings.TrimPrefix(stmt, "window.")
	if strings.HasPrefix(trimmedHistory, "history.pushState(") && strings.HasSuffix(trimmedHistory, ")") {
		inner := trimmedHistory[len("history.pushState(") : len(trimmedHistory)-1]
		args := splitArgs(inner)
		var stArg, titleArg, urlArg string
		if len(args) > 0 {
			stArg = rt.evalExpr(doc, args[0])
		}
		if len(args) > 1 {
			titleArg = rt.evalExpr(doc, args[1])
		}
		if len(args) > 2 {
			urlArg = rt.evalExpr(doc, args[2])
		}
		rt.PushState(stArg, titleArg, urlArg)
		return nil
	}
	if strings.HasPrefix(trimmedHistory, "history.replaceState(") && strings.HasSuffix(trimmedHistory, ")") {
		inner := trimmedHistory[len("history.replaceState(") : len(trimmedHistory)-1]
		args := splitArgs(inner)
		var stArg, titleArg, urlArg string
		if len(args) > 0 {
			stArg = rt.evalExpr(doc, args[0])
		}
		if len(args) > 1 {
			titleArg = rt.evalExpr(doc, args[1])
		}
		if len(args) > 2 {
			urlArg = rt.evalExpr(doc, args[2])
		}
		rt.ReplaceState(stArg, titleArg, urlArg)
		return nil
	}
	if trimmedHistory == "history.back()" {
		return rt.HistoryBack(doc)
	}
	if trimmedHistory == "history.forward()" {
		return rt.HistoryForward(doc)
	}
	if strings.HasPrefix(trimmedHistory, "history.go(") && strings.HasSuffix(trimmedHistory, ")") {
		inner := trimmedHistory[len("history.go(") : len(trimmedHistory)-1]
		delta, err := strconv.Atoi(strings.TrimSpace(rt.evalExpr(doc, inner)))
		if err == nil {
			return rt.HistoryGo(doc, delta)
		}
		return nil
	}
	// fetch(url, init)
	fetchStmt := strings.TrimSpace(strings.TrimPrefix(stmt, "await "))
	if strings.HasPrefix(fetchStmt, "fetch(") {
		endParen := findMatchingCloseParen(fetchStmt, len("fetch"))
		if endParen > len("fetch(") {
			inner := fetchStmt[len("fetch("):endParen]
			rt.execFetchCall(doc, inner)
			return nil
		}
	}
	// Method calls on FormData or URLSearchParams instances (e.g. fd.append(...), params.set(...))
	if dot := strings.IndexByte(stmt, '.'); dot > 0 && !strings.HasPrefix(stmt, "document.") && !strings.HasPrefix(stmt, "window.") && !strings.HasPrefix(stmt, "localStorage.") && !strings.HasPrefix(stmt, "console.") {
		recv := strings.TrimSpace(stmt[:dot])
		rest := strings.TrimSpace(stmt[dot+1:])
		if fd, ok := rt.FormDataObjects[recv]; ok {
			rt.applyFormDataMethod(doc, fd, rest)
			return nil
		}
		if _, ok := rt.SearchParamsObjects[recv]; ok {
			rt.applySearchParamsMethod(doc, recv, rest)
			return nil
		}
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
				if rt.tryBindSpecialObject(doc, name, rhs) {
					return nil
				}
				rt.Globals[name] = rt.evalExpr(doc, rhs)
			}
			return nil
		}
	}
	// Plain assignment: name = expr
	if eq := strings.IndexByte(stmt, '='); eq > 0 && !strings.Contains(stmt[:eq], ".") {
		name := strings.TrimSpace(stmt[:eq])
		rhs := strings.TrimSpace(stmt[eq+1:])
		if rt.tryBindSpecialObject(doc, name, rhs) {
			return nil
		}
		rt.Globals[name] = rt.evalExpr(doc, rhs)
	}
	return nil
}

func (rt *ScriptRuntime) tryBindSpecialObject(doc *dom.Document, name, rhs string) bool {
	if strings.HasPrefix(rhs, "new FormData(") && strings.HasSuffix(rhs, ")") {
		inner := strings.TrimSpace(rhs[len("new FormData(") : len(rhs)-1])
		if inner == "" {
			rt.FormDataObjects[name] = dom.NewFormData()
			return true
		}
		if doc != nil {
			if formID, _, ok := rt.resolveDOMTarget(doc, inner+"."); ok {
				rt.FormDataObjects[name] = dom.NewFormDataFromForm(doc, formID)
				return true
			}
		}
		rt.FormDataObjects[name] = dom.NewFormData()
		return true
	}
	if strings.HasPrefix(rhs, "new URLSearchParams(") && strings.HasSuffix(rhs, ")") {
		inner := strings.TrimSpace(rhs[len("new URLSearchParams(") : len(rhs)-1])
		raw := ""
		if inner != "" {
			raw = rt.evalExpr(doc, inner)
		}
		rt.SearchParamsObjects[name] = parseSearchParamsInit(raw)
		return true
	}
	if strings.HasPrefix(strings.TrimPrefix(rhs, "await "), "fetch(") {
		fcall := strings.TrimPrefix(rhs, "await ")
		endParen := findMatchingCloseParen(fcall, len("fetch"))
		if endParen > len("fetch(") {
			rt.execFetchCall(doc, fcall[len("fetch("):endParen])
			rt.Globals[name] = rt.LastFetchBody
			return true
		}
	}
	return false
}

func (rt *ScriptRuntime) applyFormDataMethod(doc *dom.Document, fd *dom.FormData, rest string) {
	if strings.HasPrefix(rest, "append(") && strings.HasSuffix(rest, ")") {
		args := splitArgs(rest[len("append(") : len(rest)-1])
		if len(args) >= 3 {
			fd.AppendFile(rt.evalExpr(doc, args[0]), rt.evalExpr(doc, args[2]), "application/octet-stream", rt.evalExpr(doc, args[1]))
		} else if len(args) >= 2 {
			fd.Append(rt.evalExpr(doc, args[0]), rt.evalExpr(doc, args[1]))
		}
	} else if strings.HasPrefix(rest, "set(") && strings.HasSuffix(rest, ")") {
		k, v := splitTwoArgs(rest[len("set(") : len(rest)-1])
		fd.Set(rt.evalExpr(doc, k), rt.evalExpr(doc, v))
	} else if strings.HasPrefix(rest, "delete(") && strings.HasSuffix(rest, ")") {
		k := rt.evalExpr(doc, rest[len("delete("):len(rest)-1])
		fd.Delete(k)
	}
}

func (rt *ScriptRuntime) applySearchParamsMethod(doc *dom.Document, recv, rest string) {
	pairs := rt.SearchParamsObjects[recv]
	if strings.HasPrefix(rest, "append(") && strings.HasSuffix(rest, ")") {
		k, v := splitTwoArgs(rest[len("append(") : len(rest)-1])
		pairs = append(pairs, dom.FormFieldPair{Name: rt.evalExpr(doc, k), Value: rt.evalExpr(doc, v)})
		rt.SearchParamsObjects[recv] = pairs
	} else if strings.HasPrefix(rest, "set(") && strings.HasSuffix(rest, ")") {
		k, v := splitTwoArgs(rest[len("set(") : len(rest)-1])
		key := rt.evalExpr(doc, k)
		val := rt.evalExpr(doc, v)
		var next []dom.FormFieldPair
		replaced := false
		for _, p := range pairs {
			if p.Name == key {
				if !replaced {
					next = append(next, dom.FormFieldPair{Name: key, Value: val})
					replaced = true
				}
				continue
			}
			next = append(next, p)
		}
		if !replaced {
			next = append(next, dom.FormFieldPair{Name: key, Value: val})
		}
		rt.SearchParamsObjects[recv] = next
	} else if strings.HasPrefix(rest, "delete(") && strings.HasSuffix(rest, ")") {
		key := rt.evalExpr(doc, rest[len("delete("):len(rest)-1])
		var next []dom.FormFieldPair
		for _, p := range pairs {
			if p.Name != key {
				next = append(next, p)
			}
		}
		rt.SearchParamsObjects[recv] = next
	}
}

func (rt *ScriptRuntime) execFetchCall(doc *dom.Document, inner string) {
	args := splitArgs(inner)
	if len(args) == 0 {
		return
	}
	targetURL := dom.ResolveFormAction(rt.PageURL, rt.evalExpr(doc, args[0]))
	method := "GET"
	contentType := ""
	body := ""
	if len(args) >= 2 {
		initStr := strings.TrimSpace(args[1])
		if m := extractInitProp(initStr, "method"); m != "" {
			method = strings.ToUpper(rt.evalExpr(doc, m))
		}
		if bExpr := extractInitProp(initStr, "body"); bExpr != "" {
			if fd, ok := rt.FormDataObjects[bExpr]; ok {
				body, contentType = fd.EncodeMultipart(dom.DefaultMultipartBoundary)
			} else if sp, ok := rt.SearchParamsObjects[bExpr]; ok {
				body = dom.SerializeFormFields(sp)
				contentType = "application/x-www-form-urlencoded;charset=UTF-8"
			} else {
				body = rt.evalExpr(doc, bExpr)
				contentType = "text/plain;charset=UTF-8"
			}
		}
	}
	rt.LastFetchURL = targetURL
	rt.LastFetchMethod = method
	rt.LastFetchContentType = contentType
	rt.LastFetchBody = body
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
		doc.SetAttribute(id, rt.evalExpr(doc, k), rt.evalExpr(doc, v))
		return nil
	}
	if strings.HasPrefix(op, ".removeAttribute(") && strings.HasSuffix(op, ")") {
		inner := op[len(".removeAttribute(") : len(op)-1]
		doc.RemoveAttribute(id, rt.evalExpr(doc, inner))
		return nil
	}
	if strings.HasPrefix(op, ".classList.add(") && strings.HasSuffix(op, ")") {
		inner := op[len(".classList.add(") : len(op)-1]
		doc.AddClass(id, rt.evalExpr(doc, inner))
		return nil
	}
	if strings.HasPrefix(op, ".classList.remove(") && strings.HasSuffix(op, ")") {
		inner := op[len(".classList.remove(") : len(op)-1]
		doc.RemoveClass(id, rt.evalExpr(doc, inner))
		return nil
	}
	if strings.HasPrefix(op, ".classList.toggle(") && strings.HasSuffix(op, ")") {
		inner := op[len(".classList.toggle(") : len(op)-1]
		doc.ToggleClass(id, rt.evalExpr(doc, inner))
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
	switch strings.TrimPrefix(e, "window.") {
	case "location.href":
		return rt.PageURL
	case "location.pathname":
		return urlPathname(rt.PageURL)
	case "location.search":
		return urlSearch(rt.PageURL)
	case "location.hash":
		return urlHash(rt.PageURL)
	case "history.state":
		return rt.HistoryState()
	case "history.length":
		return strconv.Itoa(rt.HistoryLength())
	case "event.state", "e.state":
		return rt.Globals["__popstate_state"]
	}
	if dot := strings.IndexByte(e, '.'); dot > 0 {
		recv := strings.TrimSpace(e[:dot])
		rest := strings.TrimSpace(e[dot+1:])
		if fd, ok := rt.FormDataObjects[recv]; ok {
			if strings.HasPrefix(rest, "get(") && strings.HasSuffix(rest, ")") {
				v, _ := fd.Get(rt.evalExpr(doc, rest[len("get("):len(rest)-1]))
				return v
			}
			if strings.HasPrefix(rest, "has(") && strings.HasSuffix(rest, ")") {
				if fd.Has(rt.evalExpr(doc, rest[len("has("):len(rest)-1])) {
					return "true"
				}
				return "false"
			}
			if rest == "toMultipart()" {
				body, _ := fd.EncodeMultipart(dom.DefaultMultipartBoundary)
				return body
			}
			if rest == "toUrlEncoded()" {
				return fd.EncodeURLEncoded()
			}
		}
		if pairs, ok := rt.SearchParamsObjects[recv]; ok {
			if strings.HasPrefix(rest, "get(") && strings.HasSuffix(rest, ")") {
				key := rt.evalExpr(doc, rest[len("get("):len(rest)-1])
				for _, p := range pairs {
					if p.Name == key {
						return p.Value
					}
				}
				return ""
			}
			if strings.HasPrefix(rest, "has(") && strings.HasSuffix(rest, ")") {
				key := rt.evalExpr(doc, rest[len("has("):len(rest)-1])
				for _, p := range pairs {
					if p.Name == key {
						return "true"
					}
				}
				return "false"
			}
			if rest == "toString()" {
				return dom.SerializeFormFields(pairs)
			}
		}
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

func urlPathname(raw string) string {
	schemeEnd := strings.Index(raw, "://")
	rest := raw
	if schemeEnd >= 0 {
		rest = raw[schemeEnd+3:]
	}
	slashIdx := strings.IndexByte(rest, '/')
	if slashIdx < 0 {
		return "/"
	}
	path := rest[slashIdx:]
	if qIdx := strings.IndexAny(path, "?#"); qIdx >= 0 {
		path = path[:qIdx]
	}
	if path == "" {
		return "/"
	}
	return path
}

func urlSearch(raw string) string {
	noHash := raw
	if hIdx := strings.IndexByte(noHash, '#'); hIdx >= 0 {
		noHash = noHash[:hIdx]
	}
	if qIdx := strings.IndexByte(noHash, '?'); qIdx >= 0 && qIdx+1 < len(noHash) {
		return noHash[qIdx:]
	}
	return ""
}

func urlHash(raw string) string {
	if hIdx := strings.IndexByte(raw, '#'); hIdx >= 0 && hIdx+1 < len(raw) {
		return raw[hIdx:]
	}
	return ""
}

func parseSearchParamsInit(raw string) []dom.FormFieldPair {
	s := strings.TrimPrefix(strings.TrimSpace(raw), "?")
	if s == "" {
		return nil
	}
	var out []dom.FormFieldPair
	for _, part := range strings.Split(s, "&") {
		if part == "" {
			continue
		}
		if eq := strings.IndexByte(part, '='); eq >= 0 {
			out = append(out, dom.FormFieldPair{Name: part[:eq], Value: part[eq+1:]})
		} else {
			out = append(out, dom.FormFieldPair{Name: part, Value: ""})
		}
	}
	return out
}

func findMatchingCloseParen(s string, openSearchFrom int) int {
	depth := 0
	var quote byte
	for i := openSearchFrom; i < len(s); i++ {
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
		case '"', '\'', '`':
			quote = c
		case '(':
			depth++
		case ')':
			depth--
			if depth == 0 {
				return i
			}
		}
	}
	return -1
}

func splitArgs(inner string) []string {
	var args []string
	start := 0
	depth := 0
	var quote byte
	for i := 0; i < len(inner); i++ {
		c := inner[i]
		if quote != 0 {
			if c == '\\' && i+1 < len(inner) {
				i++
			} else if c == quote {
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
				args = append(args, strings.TrimSpace(inner[start:i]))
				start = i + 1
			}
		}
	}
	if tail := strings.TrimSpace(inner[start:]); tail != "" {
		args = append(args, tail)
	}
	return args
}

func extractInitProp(initObj, key string) string {
	s := strings.TrimSpace(initObj)
	s = strings.TrimPrefix(s, "{")
	s = strings.TrimSuffix(s, "}")
	for _, entry := range splitArgs(s) {
		colon := strings.IndexByte(entry, ':')
		if colon > 0 {
			k := trimQuotes(entry[:colon])
			if k == key {
				return strings.TrimSpace(entry[colon+1:])
			}
		}
	}
	return ""
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
