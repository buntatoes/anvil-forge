// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package script

import (
	"testing"

	"github.com/buntatoes/anvil-engine/dom"
)

func TestScriptRuntimeDomEventsAndStorage(t *testing.T) {
	doc := dom.ParseHTML(`<div id="msg">Initial</div><button id="btn">Click</button>
	<script>
		localStorage.setItem("theme", "dark");
		document.getElementById("msg").textContent = "Updated by Cinder";
		document.getElementById("btn").addEventListener("click", () => {
			document.getElementById("msg").textContent = "Clicked!";
		});
	</script>`)

	rt := NewScriptRuntime()
	if err := rt.RunDocumentScripts(doc); err != nil {
		t.Fatal(err)
	}
	if rt.LocalStorage["theme"] != "dark" {
		t.Fatalf("expected localStorage theme=dark, got %q", rt.LocalStorage["theme"])
	}
	msgID, _ := doc.GetElementByID("msg")
	if got := doc.TextContent(msgID); got != "Updated by Cinder" {
		t.Fatalf("expected 'Updated by Cinder', got %q", got)
	}
	btnID, _ := doc.GetElementByID("btn")
	if err := rt.DispatchClick(doc, btnID); err != nil {
		t.Fatal(err)
	}
	if got := doc.TextContent(msgID); got != "Clicked!" {
		t.Fatalf("expected 'Clicked!' after DispatchClick, got %q", got)
	}
}

func TestWasm10ModuleDecodeValidateAndExecute(t *testing.T) {
	// Handcrafted valid WASM 1.0 module exporting "add(i32, i32) -> i32"
	wasmAdd := []byte{
		0x00, 0x61, 0x73, 0x6d, // magic \0asm
		0x01, 0x00, 0x00, 0x00, // version 1
		// Type section (id=1): 1 type: (i32, i32) -> i32
		0x01, 0x07, 0x01, 0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f,
		// Function section (id=3): 1 func of type 0
		0x03, 0x02, 0x01, 0x00,
		// Export section (id=7): 1 export "add" -> func 0
		0x07, 0x07, 0x01, 0x03, 'a', 'd', 'd', 0x00, 0x00,
		// Code section (id=10): 1 body: 0 locals, local.get 0, local.get 1, i32.add, end
		0x0a, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x6a, 0x0b,
	}

	if !ValidateWasm(wasmAdd) {
		t.Fatal("expected wasmAdd to validate")
	}
	mod, err := DecodeWasmModule(wasmAdd)
	if err != nil {
		t.Fatal(err)
	}
	inst, err := InstantiateWasm(mod)
	if err != nil {
		t.Fatal(err)
	}
	res, err := inst.CallExport("add", I32Val(19), I32Val(23))
	if err != nil {
		t.Fatal(err)
	}
	if len(res) != 1 || res[0].AsI32() != 42 {
		t.Fatalf("expected 42 from WASM add(19, 23), got %v", res)
	}
}
