// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package anvilengine

import (
	"math"
	"strings"
	"testing"

	"github.com/buntatoes/anvil-engine/dom"
	"github.com/buntatoes/anvil-engine/paint"
	"github.com/buntatoes/anvil-engine/raster"
	"github.com/buntatoes/anvil-engine/script"
	"github.com/buntatoes/anvil-engine/style"
)

func TestFullPipelineRenderHTMLPage(t *testing.T) {
	if Version != "0.24.5.1" {
		t.Fatalf("expected Version 0.24.5.1, got %s", Version)
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
