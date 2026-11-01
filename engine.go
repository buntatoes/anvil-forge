// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package anvilengine

import (
	"github.com/buntatoes/anvil-engine/dom"
	"github.com/buntatoes/anvil-engine/layout"
	"github.com/buntatoes/anvil-engine/paint"
	"github.com/buntatoes/anvil-engine/raster"
	"github.com/buntatoes/anvil-engine/script"
	"github.com/buntatoes/anvil-engine/style"
)

const (
	Version             = "0.24.5.1"
	RenderingEngineName = dom.RenderingEngineName
	JsEngineName        = dom.JsEngineName
	CssEngineName       = dom.CssEngineName
)

type PageRenderResult struct {
	Document    *dom.Document
	Stylesheet  style.Stylesheet
	StyleMap    *style.StyleMap
	LayoutRoot  layout.LayoutBox
	DisplayList paint.DisplayList
	Framebuffer *raster.Framebuffer
	Runtime     *script.ScriptRuntime
}

func RenderHTMLPage(htmlSrc, extraCSS string, width, height uint32) (*PageRenderResult, error) {
	doc := dom.ParseHTML(htmlSrc)
	rt := script.NewScriptRuntime()
	if err := rt.RunDocumentScripts(doc); err != nil {
		return nil, err
	}

	var combined style.Stylesheet
	for _, src := range doc.StyleSheets() {
		if src.Kind == dom.StyleSheetInline {
			combined.Append(style.ParseCSSWithMedia(src.CSS, src.Media))
		}
	}
	if extraCSS != "" {
		combined.Append(style.ParseCSS(extraCSS))
	}

	sm := style.ComputeStylesWithViewport(doc, &combined, width, height)
	rootBox := layout.LayoutDocument(doc, sm, width, height)
	dl := paint.BuildDisplayList(&rootBox)
	fb := raster.Rasterize(&dl, width, height)

	return &PageRenderResult{
		Document:    doc,
		Stylesheet:  combined,
		StyleMap:    sm,
		LayoutRoot:  rootBox,
		DisplayList: dl,
		Framebuffer: fb,
		Runtime:     rt,
	}, nil
}
