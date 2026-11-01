// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package layout

import (
	"testing"

	"github.com/buntatoes/anvil-engine/dom"
	"github.com/buntatoes/anvil-engine/style"
)

func TestBlockFlexGridAndHitTest(t *testing.T) {
	html := `
	<div id="container" style="width: 600px; padding: 10px;">
		<div id="flex" style="display: flex; column-gap: 20px;">
			<div id="item1" style="height: 40px;">Item One</div>
			<div id="item2" style="height: 40px;">Item Two</div>
		</div>
		<div id="grid" style="display: grid; grid-template-columns: 1fr 1fr; gap: 10px;">
			<div id="g1" style="height: 30px;">Grid 1</div>
			<div id="g2" style="height: 30px;">Grid 2</div>
		</div>
	</div>`

	doc := dom.ParseHTML(html)
	sheet := style.ParseCSS("")
	sm := style.ComputeStylesWithViewport(doc, &sheet, 800, 600)
	rootBox := LayoutDocument(doc, sm, 800, 600)

	item1ID, _ := doc.GetElementByID("item1")
	item2ID, _ := doc.GetElementByID("item2")
	r1, ok1 := rootBox.FindNodeRect(item1ID)
	r2, ok2 := rootBox.FindNodeRect(item2ID)
	if !ok1 || !ok2 {
		t.Fatal("missing flex item rects")
	}
	if r2.X <= r1.X {
		t.Fatalf("expected item2 to be placed to the right of item1: r1=%+v r2=%+v", r1, r2)
	}

	hitID, ok := HitTest(&rootBox, r1.X+5, r1.Y+5)
	if !ok {
		t.Fatal("HitTest missed item1")
	}
	_ = hitID

	selText := CollectTextRunsInRect(&rootBox, Rect{X: 0, Y: 0, Width: 800, Height: 600})
	if selText == "" {
		t.Fatal("expected non-empty text runs in viewport")
	}
}
