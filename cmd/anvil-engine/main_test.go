// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package main

import (
	"bytes"
	"strings"
	"testing"
)

func TestCLISelfTestAndRender(t *testing.T) {
	var out, errBuf bytes.Buffer
	code := run([]string{"self-test"}, strings.NewReader(""), &out, &errBuf)
	if code != 0 {
		t.Fatalf("self-test failed (%d): %s", code, errBuf.String())
	}
	if !strings.Contains(out.String(), "anvil-engine 0.24.5.1 (Forge/Prism/Cinder) ok") {
		t.Fatalf("unexpected self-test output: %q", out.String())
	}

	out.Reset()
	errBuf.Reset()
	code = run([]string{"render-html"}, strings.NewReader(`<html><head><title>CLI</title></head><body><h1>Hello</h1></body></html>`), &out, &errBuf)
	if code != 0 {
		t.Fatalf("render-html failed (%d): %s", code, errBuf.String())
	}
	if !strings.Contains(out.String(), `title="CLI"`) {
		t.Fatalf("unexpected render-html output: %q", out.String())
	}
}
