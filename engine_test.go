// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package anvilengine

import (
	"math"
	"strings"
	"testing"

	"github.com/buntatoes/anvil-forge/dom"
	"github.com/buntatoes/anvil-forge/paint"
	"github.com/buntatoes/anvil-forge/raster"
	"github.com/buntatoes/anvil-forge/script"
	"github.com/buntatoes/anvil-forge/style"
)

func TestFullPipelineRenderHTMLPage(t *testing.T) {
	if Version != "0.25.0" {
		t.Fatalf("expected Version 0.25.0, got %s", Version)
	}
	html := `<!DOCTYPE html>
	<html>
	<head>
		<title>Anvil Forge Pipeline</title>
		<style>
			body { background: #101820; color: #f2aa4c; }
			#banner { width: 240px; height: 60px; border-radius: 8px; background: #ff6b24; }
		</style>
	</head>
	<body>
		<div id="banner">Forge Ready</div>
		<script>
			document.getElementById("banner").textContent = "Forge Active";
		</script>
	</body>
	</html>`

	res, err := RenderHTMLPage(html, "", 320, 180)
	if err != nil {
		t.Fatal(err)
	}
	bannerID, ok := res.Document.GetElementByID("banner")
	if !ok {
		t.Fatal("missing #banner")
	}
	if got := res.Document.TextContent(bannerID); got != "Forge Active" {
		t.Fatalf("expected script-updated banner 'Forge Active', got %q", got)
	}
	if len(res.DisplayList.Items) == 0 {
		t.Fatal("expected non-empty display list")
	}
	if len(res.Framebuffer.Pixels) != 320*180*4 {
		t.Fatalf("unexpected framebuffer byte length: %d", len(res.Framebuffer.Pixels))
	}
}

func TestSecurityBoundaryWasmCssDomAndRaster(t *testing.T) {
	// 1. WASM local count bomb (rep = 0xFFFFFFFF in code section) must fail DecodeWasmModule immediately without OOM.
	wasmLocalBomb := []byte{
		0x00, 'a', 's', 'm', 0x01, 0x00, 0x00, 0x00,
		0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // 1 type () -> ()
		0x03, 0x02, 0x01, 0x00, // 1 func of type 0
		0x0A, 0x0A, 0x01, 0x08, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0x7F, 0x0B, // local rep = 0xFFFFFFFF
	}
	if _, err := script.DecodeWasmModule(wasmLocalBomb); err == nil {
		t.Fatal("expected DecodeWasmModule error on local count bomb")
	}

	// 2. WASM out-of-bounds function typeIdx must be rejected during DecodeWasmModule.
	wasmBadTypeIdx := []byte{
		0x00, 'a', 's', 'm', 0x01, 0x00, 0x00, 0x00,
		0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
		0x03, 0x02, 0x01, 0x09, // typeIdx = 9 >= len(Types)
		0x0A, 0x04, 0x01, 0x02, 0x00, 0x0B,
	}
	if _, err := script.DecodeWasmModule(wasmBadTypeIdx); err == nil {
		t.Fatal("expected DecodeWasmModule error on out-of-bounds typeIdx")
	}

	// 3. WASM i32.load address + offset wraparound (0xFFFFFFFC + 8) must trap instead of wrapping to address 4.
	wasmLoadWrap := []byte{
		0x00, 'a', 's', 'm', 0x01, 0x00, 0x00, 0x00,
		0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7F, // () -> i32
		0x03, 0x02, 0x01, 0x00,
		0x05, 0x03, 0x01, 0x00, 0x01, // 1 page memory
		0x07, 0x05, 0x01, 0x01, 'f', 0x00, 0x00,
		0x0A, 0x09, 0x01, 0x07, 0x00,
		0x41, 0x7C, // i32.const -4 (0xFFFFFFFC)
		0x28, 0x02, 0x08, // i32.load align=2 offset=8
		0x0B,
	}
	mod, err := script.DecodeWasmModule(wasmLoadWrap)
	if err != nil {
		t.Fatalf("unexpected decode error on wasmLoadWrap: %v", err)
	}
	inst, err := script.InstantiateWasm(mod)
	if err != nil {
		t.Fatalf("unexpected instantiate error: %v", err)
	}
	if _, err := inst.CallExport("f"); err == nil {
		t.Fatal("expected i32.load 32-bit wraparound (0xFFFFFFFC + 8) to trap out of bounds")
	}

	// 4. CSS exponential var() bomb and deep nesting must be bounded without hanging.
	varBombCSS := `:root {
		--a0: 10px 10px 10px 10px 10px 10px 10px 10px;
		--a1: var(--a0) var(--a0) var(--a0) var(--a0) var(--a0) var(--a0) var(--a0) var(--a0);
		--a2: var(--a1) var(--a1) var(--a1) var(--a1) var(--a1) var(--a1) var(--a1) var(--a1);
		--a3: var(--a2) var(--a2) var(--a2) var(--a2) var(--a2) var(--a2) var(--a2) var(--a2);
		--a4: var(--a3) var(--a3) var(--a3) var(--a3) var(--a3) var(--a3) var(--a3) var(--a3);
		--a5: var(--a4) var(--a4) var(--a4) var(--a4) var(--a4) var(--a4) var(--a4) var(--a4);
	}
	div { margin: var(--a5); }`
	doc := dom.ParseHTML(`<html><body><div>x</div></body></html>`)
	sheet := style.ParseCSS(varBombCSS)
	_ = style.ComputeStylesWithViewport(doc, &sheet, 800, 600)

	deepCSS := strings.Repeat("@media all { ", 100) + "div { color: red; }" + strings.Repeat(" }", 100)
	_ = style.ParseCSS(deepCSS)

	// 5. NaN coordinates and oversized Framebuffer dimensions in raster must not panic or OOM.
	fb := raster.NewFramebuffer(999999, 999999)
	if fb.Width != raster.MaxFramebufferSide || fb.Height != raster.MaxFramebufferSide {
		t.Fatalf("expected framebuffer clamped to %d, got %dx%d", raster.MaxFramebufferSide, fb.Width, fb.Height)
	}
	dl := &paint.DisplayList{
		Items: []paint.DisplayItem{
			{Kind: paint.ItemRect, X: float32(math.NaN()), Y: 0, Width: 100, Height: 100, Color: style.RGBA(255, 0, 0, 255)},
			{Kind: paint.ItemRoundedRect, X: 0, Y: float32(math.Inf(1)), Width: 50, Height: 50, Radius: 4, FillColor: style.RGBA(0, 255, 0, 255)},
		},
	}
	smallFb := raster.Rasterize(dl, 32, 32)
	if len(smallFb.Pixels) != 32*32*4 {
		t.Fatalf("unexpected smallFb size %d", len(smallFb.Pixels))
	}
}

func TestInteractiveFormSubmissionIncrementalReflowAndSubresourcePriority(t *testing.T) {
	html := `<!DOCTYPE html>
	<html>
	<head>
		<link rel="preload" href="/fonts/ui.ttf" as="font" crossorigin="anonymous">
		<link rel="stylesheet" href="/css/main.css">
		<script src="/js/critical.js"></script>
		<script src="/js/deferred.js" defer></script>
		<style>
			#search-box { width: 320px; height: 40px; }
			.spa-pane { padding: 12px; color: #222; }
			.spa-pane.active { color: #ff6b24; }
		</style>
	</head>
	<body>
		<form id="search-form" action="/search" method="GET">
			<input id="q-input" type="search" name="q" value="initial">
			<input type="hidden" name="hl" value="en">
			<button id="go-btn" type="submit" name="btnG" value="Search">Search</button>
		</form>
		<div id="spa-root" class="spa-pane">
			<div id="spa-child">Inbox (3)</div>
		</div>
		<div id="static-sidebar">Static Folder List</div>
		<img src="/img/hero.png">
		<img src="/img/footer.png" loading="lazy">
	</body>
	</html>`

	res, err := RenderHTMLPage(html, "", 800, 600)
	if err != nil {
		t.Fatalf("RenderHTMLPage failed: %v", err)
	}

	// 1. Subresource discovery & HTTP/2 priority ordering
	subs := res.Document.DiscoverPrioritizedSubresources("https://www.google.com/")
	if len(subs) != 6 {
		t.Fatalf("expected 6 discovered subresources, got %d", len(subs))
	}
	if subs[0].Priority != dom.PriorityCriticalStyle || subs[0].URL != "https://www.google.com/css/main.css" {
		t.Fatalf("expected first subresource to be critical stylesheet, got %+v", subs[0])
	}
	if subs[len(subs)-1].Priority != dom.PriorityLazyMedia || subs[len(subs)-1].URL != "https://www.google.com/img/footer.png" {
		t.Fatalf("expected last subresource to be lazy image, got %+v", subs[len(subs)-1])
	}

	// 2. Interactive form typing & Enter/Submit serialization
	qID, ok := res.Document.GetElementByID("q-input")
	if !ok {
		t.Fatal("missing #q-input")
	}
	if err := res.TypeIntoInput(qID, "anvil browser architecture"); err != nil {
		t.Fatalf("TypeIntoInput failed: %v", err)
	}
	req, built, err := res.SubmitForm("https://www.google.com/", qID)
	if err != nil || !built || req == nil {
		t.Fatalf("SubmitForm failed: built=%v err=%v", built, err)
	}
	expectedNav := "https://www.google.com/search?q=anvil+browser+architecture&hl=en&btnG=Search"
	if req.FinalNavigationURL != expectedNav {
		t.Fatalf("expected navigation URL %q, got %q", expectedNav, req.FinalNavigationURL)
	}

	// 3. Incremental DOM mutation coalescing & subtree layout reuse
	// First restage clears the input dirty flag
	_, _ = res.RestageIncremental(800, 600)
	// Clean restage should re-cascade 0 nodes and hit the layout cache
	recascaded0, hits0 := res.RestageIncremental(800, 600)
	if recascaded0 != 0 || hits0 == 0 {
		t.Fatalf("expected clean restage to re-cascade 0 nodes and hit layout cache, got recascaded=%d hits=%d", recascaded0, hits0)
	}

	// Mutate both #spa-root and #spa-child in one tick; they must coalesce into #spa-root only
	spaRoot, _ := res.Document.GetElementByID("spa-root")
	spaChild, _ := res.Document.GetElementByID("spa-child")
	res.Document.AddClass(spaRoot, "active")
	res.Document.SetTextContent(spaChild, "Inbox (4)")
	roots := res.Document.CoalesceDirtySubtreeRoots()
	if len(roots) != 1 || roots[0] != spaRoot {
		t.Fatalf("expected coalesced dirty root [#spa-root], got %v", roots)
	}
	recascaded1, reuses1 := res.RestageIncremental(800, 600)
	if recascaded1 != 2 {
		t.Fatalf("expected only 2 elements (#spa-root and #spa-child) re-cascaded, got %d", recascaded1)
	}
	if reuses1 == 0 {
		t.Fatalf("expected clean sibling subtrees (#search-form, #static-sidebar) to be reused in LayoutCache")
	}
}

func TestMultipartFormDataHistoryApiAndStickyPositioning(t *testing.T) {
	html := `<!DOCTYPE html>
	<html>
	<head>
		<style>
			body { margin: 0; padding: 0; }
			#hero-spacer { width: 400px; height: 80px; background: #eeeeee; }
			#article-section { width: 400px; height: 300px; background: #ffffff; }
			#sticky-toc { position: sticky; top: 10px; width: 400px; height: 40px; background: #ff6b24; }
			#article-body { width: 400px; height: 260px; background: #cccccc; }
			#footer-block { width: 400px; height: 400px; background: #222222; }
		</style>
	</head>
	<body>
		<div id="hero-spacer">Hero</div>
		<div id="article-section">
			<div id="sticky-toc">Sticky Table of Contents</div>
			<div id="article-body">Article Content</div>
		</div>
		<div id="footer-block">Footer</div>
		<form id="upload-form" action="/api/upload" method="POST" enctype="multipart/form-data">
			<input id="title-in" type="text" name="title" value="Q4 Report">
			<input id="file-in" type="file" name="attachment" filename="report.txt" data-content-type="text/plain" data-file-content="Confidential Q4 Data">
			<button id="upload-btn" type="submit" name="submit_action" value="send">Upload</button>
		</form>
		<div id="route-view">Home</div>
	</body>
	</html>`

	res, err := RenderHTMLPage(html, "", 400, 300)
	if err != nil {
		t.Fatalf("RenderHTMLPage failed: %v", err)
	}
	res.Runtime.SetPageOrigin("https://mail.google.com/mail/u/0/")

	// 1. Multipart Form Submission (<form enctype="multipart/form-data">) & FormData + fetch()
	uploadBtn, ok := res.Document.GetElementByID("upload-btn")
	if !ok {
		t.Fatal("missing #upload-btn")
	}
	req, built, err := res.SubmitForm("https://mail.google.com/mail/u/0/", uploadBtn)
	if err != nil || !built || req == nil {
		t.Fatalf("SubmitForm multipart failed: built=%v err=%v", built, err)
	}
	expectedCT := "multipart/form-data; boundary=" + dom.DefaultMultipartBoundary
	if req.Method != "POST" || req.ContentType != expectedCT {
		t.Fatalf("expected POST with %q, got method=%s ct=%q", expectedCT, req.Method, req.ContentType)
	}
	if !strings.Contains(req.QueryOrBody, `Content-Disposition: form-data; name="title"`) ||
		!strings.Contains(req.QueryOrBody, "Q4 Report") ||
		!strings.Contains(req.QueryOrBody, `filename="report.txt"`) ||
		!strings.Contains(req.QueryOrBody, "Confidential Q4 Data") {
		t.Fatalf("multipart payload missing expected parts:\n%s", req.QueryOrBody)
	}

	// Verify JS FormData + fetch(url, { method: 'POST', body: fd })
	scriptSrc := `
		const fd = new FormData(document.getElementById('upload-form'));
		fd.append('folder', 'archive');
		fetch('/api/compose', { method: 'POST', body: fd });
	`
	if err := res.Runtime.EvalScript(res.Document, scriptSrc); err != nil {
		t.Fatalf("EvalScript FormData+fetch failed: %v", err)
	}
	if res.Runtime.LastFetchURL != "https://mail.google.com/api/compose" ||
		res.Runtime.LastFetchMethod != "POST" ||
		res.Runtime.LastFetchContentType != expectedCT ||
		!strings.Contains(res.Runtime.LastFetchBody, `name="folder"`) {
		t.Fatalf("unexpected fetch FormData state: url=%s method=%s ct=%s", res.Runtime.LastFetchURL, res.Runtime.LastFetchMethod, res.Runtime.LastFetchContentType)
	}

	// 2. History API (pushState / replaceState / back / forward / popstate) & URLSearchParams
	spaScript := `
		window.addEventListener('popstate', function(e) {
			document.getElementById('route-view').textContent = location.pathname + location.search + ':' + e.state;
		});
		const params = new URLSearchParams('q=wiki&lang=en');
		params.set('lang', 'fr');
		history.pushState('inbox-state', '', '/mail/u/0/inbox?' + params.toString());
		history.pushState('starred-state', '', '/mail/u/0/starred');
		history.replaceState('starred-updated', '', '/mail/u/0/starred?filter=unread');
	`
	if err := res.Runtime.EvalScript(res.Document, spaScript); err != nil {
		t.Fatalf("EvalScript History API failed: %v", err)
	}
	if got := res.CurrentURL(); got != "https://mail.google.com/mail/u/0/starred?filter=unread" {
		t.Fatalf("expected pushed/replaced URL, got %q", got)
	}
	if res.Runtime.HistoryLength() != 3 || res.Runtime.HistoryState() != "starred-updated" {
		t.Fatalf("unexpected history length=%d state=%q", res.Runtime.HistoryLength(), res.Runtime.HistoryState())
	}
	if err := res.Runtime.HistoryBack(res.Document); err != nil {
		t.Fatalf("HistoryBack failed: %v", err)
	}
	routeView, _ := res.Document.GetElementByID("route-view")
	if got := res.Document.TextContent(routeView); got != "/mail/u/0/inbox?q=wiki&lang=fr:inbox-state" {
		t.Fatalf("expected popstate listener to update #route-view, got %q", got)
	}

	// 3. CSS Sticky Positioning & Scroll-Anchored Viewport Hit-Testing
	stickyID, ok := res.Document.GetElementByID("sticky-toc")
	if !ok {
		t.Fatal("missing #sticky-toc")
	}
	// At scrollY = 0: #sticky-toc is at its natural document Y = 80
	rect0, ok := res.NodeRectAtScroll(stickyID, 0)
	if !ok || rect0.Y != 80 {
		t.Fatalf("expected sticky-toc Y=80 at scrollY=0, got %+v", rect0)
	}
	// At scrollY = 150: #sticky-toc (top: 10px) is pinned at document Y = 150 + 10 = 160 (viewport Y = 10)
	rect150, ok := res.NodeRectAtScroll(stickyID, 150)
	if !ok || rect150.Y != 160 {
		t.Fatalf("expected sticky-toc clamped to Y=160 at scrollY=150, got %+v", rect150)
	}
	// Viewport hit-test at (x=50, viewportY=20) when scrolled to scrollY=150 must hit #sticky-toc
	hitID, hitOK := res.HitTestAtScroll(50, 20, 150)
	if !hitOK {
		t.Fatal("expected HitTestAtScroll(50, 20, 150) to hit sticky header")
	}
	if hitID != stickyID {
		if p, okP := res.Document.Parent(hitID); !okP || p != stickyID {
			t.Fatalf("expected HitTestAtScroll to hit #sticky-toc (%d), got %d", stickyID, hitID)
		}
	}
	// At scrollY = 400: parent #article-section ends at Y = 80 + 300 = 380, so #sticky-toc (height 40) stops at maxY = 380 - 40 = 340
	rect400, ok := res.NodeRectAtScroll(stickyID, 400)
	if !ok || rect400.Y != 340 {
		t.Fatalf("expected sticky-toc to stop at parent bottom Y=340 when scrollY=400, got %+v", rect400)
	}
}
