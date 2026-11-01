// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package paint

import (
	"testing"

	"github.com/buntatoes/anvil-engine/dom"
	"github.com/buntatoes/anvil-engine/layout"
	"github.com/buntatoes/anvil-engine/style"
)

func TestBuildDisplayListAndScaledZoom(t *testing.T) {
	html := `<div style="width: 200px; height: 100px; background: #224466; border-radius: 8px; box-shadow: 0 4px 12px #000000;">Hello Forge</div>`
	doc := dom.ParseHTML(html)
	sheet := style.ParseCSS("")
	sm := style.ComputeStylesWithViewport(doc, &sheet, 800, 600)
	root := layout.LayoutDocument(doc, sm, 800, 600)

	dl := BuildDisplayList(&root)
	if len(dl.Items) < 3 {
		t.Fatalf("expected shadow, rounded rect, and text items, got %d", len(dl.Items))
	}
	zoomed := dl.Scaled(2.0)
	if len(zoomed.Items) != len(dl.Items) {
		t.Fatalf("scaled item count mismatch: %d vs %d", len(zoomed.Items), len(dl.Items))
	}
}
