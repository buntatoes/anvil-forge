// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package style

import (
	"testing"

	"github.com/buntatoes/anvil-engine/dom"
)

func TestCascadeLayersNestingAndCustomProps(t *testing.T) {
	html := `<div id="card" class="box"><span class="label">Hello</span></div>`
	css := `
	@layer base, theme;
	@layer theme {
		.box {
			--brand: #ff6b24;
			color: var(--brand);
			width: calc(200px + 40px);
			& .label {
				font-weight: bold;
				text-transform: uppercase;
			}
		}
	}
	@layer base {
		#card {
			color: #000000 !important;
			background: linear-gradient(90deg, #112233, #445566);
		}
	}`

	doc := dom.ParseHTML(html)
	sheet := ParseCSS(css)
	sm := ComputeStylesWithViewport(doc, &sheet, 1280, 720)

	cardID, ok := doc.GetElementByID("card")
	if !ok {
		t.Fatal("missing #card")
	}
	cardStyle, ok := sm.Get(cardID)
	if !ok {
		t.Fatal("missing computed style for #card")
	}
	// Earlier layer !important beats later layer normal
	if cardStyle.Color != RGBA(0, 0, 0, 255) {
		t.Fatalf("expected #000000 from @layer base !important, got %+v", cardStyle.Color)
	}
	if cardStyle.Width.Value != 240 {
		t.Fatalf("expected calc width 240px, got %+v", cardStyle.Width)
	}
	if cardStyle.BackgroundGradient == nil {
		t.Fatal("expected parsed linear-gradient on #card")
	}

	labels := doc.QuerySelectorAll(".label")
	if len(labels) != 1 {
		t.Fatalf("expected 1 .label, got %d", len(labels))
	}
	labelStyle, _ := sm.Get(labels[0])
	if labelStyle.FontWeight != 700 || labelStyle.TextTransform != TextTransformUppercase {
		t.Fatalf("nested selector rule did not apply: %+v", labelStyle)
	}
}
