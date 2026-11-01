// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package main

import (
	"fmt"
	"io"
	"os"

	anvilengine "github.com/buntatoes/anvil-engine"
	"github.com/buntatoes/anvil-engine/dom"
	"github.com/buntatoes/anvil-engine/script"
)

func main() {
	os.Exit(run(os.Args[1:], os.Stdin, os.Stdout, os.Stderr))
}

func run(args []string, stdin io.Reader, stdout, stderr io.Writer) int {
	if len(args) == 0 {
		fmt.Fprintln(stderr, "usage: anvil-engine <self-test|render-html|validate-wasm>")
		return 2
	}
	switch args[0] {
	case "self-test":
		res, err := anvilengine.RenderHTMLPage(
			`<!DOCTYPE html><html><head><title>SelfTest</title><style>#x{width:120px;height:40px;background:#225588;color:#fff;}</style></head><body><div id="x">Init</div><script>document.getElementById("x").textContent="Forge OK";</script></body></html>`,
			"",
			160,
			90,
		)
		if err != nil {
			fmt.Fprintf(stderr, "self-test failed: %v\n", err)
			return 1
		}
		xid, ok := res.Document.GetElementByID("x")
		if !ok || res.Document.TextContent(xid) != "Forge OK" {
			fmt.Fprintln(stderr, "self-test DOM verification failed")
			return 1
		}
		fmt.Fprintf(stdout, "anvil-engine %s (%s/%s/%s) ok\n",
			anvilengine.Version,
			anvilengine.RenderingEngineName,
			anvilengine.CssEngineName,
			anvilengine.JsEngineName,
		)
		return 0
	case "render-html":
		raw, err := io.ReadAll(io.LimitReader(stdin, 16*1024*1024))
		if err != nil {
			fmt.Fprintf(stderr, "read stdin: %v\n", err)
			return 1
		}
		res, err := anvilengine.RenderHTMLPage(string(raw), "", 640, 360)
		if err != nil {
			fmt.Fprintf(stderr, "render-html: %v\n", err)
			return 1
		}
		fmt.Fprintf(stdout, "nodes=%d rules=%d items=%d pixels=%d title=%q\n",
			res.Document.NodeCount(),
			len(res.Stylesheet.Rules),
			len(res.DisplayList.Items),
			len(res.Framebuffer.Pixels),
			res.Document.Title(),
		)
		return 0
	case "validate-wasm":
		raw, err := io.ReadAll(io.LimitReader(stdin, 16*1024*1024))
		if err != nil {
			fmt.Fprintf(stderr, "read stdin: %v\n", err)
			return 1
		}
		if !script.ValidateWasm(raw) {
			fmt.Fprintln(stderr, "invalid wasm module")
			return 1
		}
		fmt.Fprintln(stdout, "wasm valid")
		return 0
	case "parse-html":
		raw, err := io.ReadAll(io.LimitReader(stdin, 16*1024*1024))
		if err != nil {
			fmt.Fprintf(stderr, "read stdin: %v\n", err)
			return 1
		}
		doc := dom.ParseHTML(string(raw))
		fmt.Fprintf(stdout, "nodes=%d title=%q\n", doc.NodeCount(), doc.Title())
		return 0
	default:
		fmt.Fprintf(stderr, "unknown command: %s\n", args[0])
		return 2
	}
}
