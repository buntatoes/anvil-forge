// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package dom

import (
	"testing"
)

func TestParseHTMLAndRecovery(t *testing.T) {
	doc := ParseHTML(`<!DOCTYPE html>
<html>
<head><title>Forge Test</title><style>body { color: #111; }</style></head>
<body>
  <div id="main" class="card primary" data-role="hero">
    <p>First paragraph<div>Auto-closed paragraph</div>
    <b><i>Misnested</b>Still italic</i>
    <table><span>Fostered span</span><tr><td>Cell 1<td>Cell 2</table>
  </div>
</body>
</html>`)

	if got := doc.Title(); got != "Forge Test" {
		t.Fatalf("expected title 'Forge Test', got %q", got)
	}
	if len(doc.StyleSheets()) != 1 {
		t.Fatalf("expected 1 stylesheet, got %d", len(doc.StyleSheets()))
	}
	mainID, ok := doc.GetElementByID("main")
	if !ok {
		t.Fatal("expected #main element")
	}
	if !doc.MatchesSelector(mainID, "div#main.card.primary[data-role^=her]") {
		t.Fatal("expected #main to match compound selector")
	}
	cells := doc.QuerySelectorAll("table td")
	if len(cells) != 2 {
		t.Fatalf("expected 2 table cells, got %d", len(cells))
	}
	fostered := doc.QuerySelectorAll("#main > span")
	if len(fostered) != 1 {
		t.Fatalf("expected 1 fostered span before table, got %d", len(fostered))
	}
}

func TestSelectorsLevel4AndShadowDOM(t *testing.T) {
	doc := ParseHTML(`<ul>
		<li class="item">One</li>
		<li class="item active"><a href="#sec">Two</a></li>
		<li class="item">Three</li>
	</ul>
	<div id="host"><span slot="title">Projected Header</span></div>`)

	items := doc.QuerySelectorAll("ul > li.item:nth-child(2n+1)")
	if len(items) != 2 {
		t.Fatalf("expected 2 odd items, got %d", len(items))
	}
	hasLink := doc.QuerySelectorAll("li:has(a[href])")
	if len(hasLink) != 1 {
		t.Fatalf("expected 1 li:has(a[href]), got %d", len(hasLink))
	}
	host, ok := doc.GetElementByID("host")
	if !ok {
		t.Fatal("missing #host")
	}
	sr, err := doc.AttachShadow(host, true)
	if err != nil {
		t.Fatal(err)
	}
	slotEl := doc.CreateElement("slot")
	doc.Node(slotEl).Data.SetAttr("name", "title")
	_ = doc.Append(sr, slotEl)

	composed := doc.ComposedChildren(host)
	if len(composed) != 1 || doc.TextContent(composed[0]) != "Projected Header" {
		t.Fatalf("unexpected composed children: %v", composed)
	}
}
