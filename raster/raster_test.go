// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package raster

import (
	"testing"

	"github.com/buntatoes/anvil-engine/dom"
	"github.com/buntatoes/anvil-engine/layout"
	"github.com/buntatoes/anvil-engine/paint"
	"github.com/buntatoes/anvil-engine/style"
)

func TestRasterizeDisplayListAndBgra(t *testing.T) {
	html := `<div style="width: 120px; height: 60px; background: #ff4422; border-radius: 6px; color: #ffffff;">ANVIL FORGE</div>`
	doc := dom.ParseHTML(html)
	sheet := style.ParseCSS("")
	sm := style.ComputeStylesWithViewport(doc, &sheet, 200, 100)
	root := layout.LayoutDocument(doc, sm, 200, 100)
	dl := paint.BuildDisplayList(&root)

	engine := NewRasterEngine()
	fb := engine.RenderDisplayList(&dl, 200, 100, 0)
	if fb.Width != 200 || fb.Height != 100 || len(fb.Pixels) != 200*100*4 {
		t.Fatalf("unexpected framebuffer size: %dx%d", fb.Width, fb.Height)
	}
	nonWhite := false
	for i := 0; i+3 < len(fb.Pixels); i += 4 {
		if fb.Pixels[i] != 255 || fb.Pixels[i+1] != 255 || fb.Pixels[i+2] != 255 {
			nonWhite = true
			break
		}
	}
	if !nonWhite {
		t.Fatal("expected rendered pixels to differ from background white")
	}
	bgra := fb.ToBgraBottomUp()
	if len(bgra) != len(fb.Pixels) {
		t.Fatalf("unexpected bgra length: %d", len(bgra))
	}
}
