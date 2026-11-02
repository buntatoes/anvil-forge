// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package anvilengine

import (
	"github.com/buntatoes/anvil-forge/dom"
	"github.com/buntatoes/anvil-forge/layout"
	"github.com/buntatoes/anvil-forge/paint"
	"github.com/buntatoes/anvil-forge/raster"
	"github.com/buntatoes/anvil-forge/script"
	"github.com/buntatoes/anvil-forge/style"
)

const (
	Version             = "0.25.0"
	RenderingEngineName = dom.RenderingEngineName
	JsEngineName        = dom.JsEngineName
	CssEngineName       = dom.CssEngineName
)

type PageRenderResult struct {
	Document    *dom.Document
	Stylesheet  style.Stylesheet
	StyleMap    *style.StyleMap
	LayoutRoot  layout.LayoutBox
	LayoutCache *layout.LayoutCache
	DisplayList paint.DisplayList
	Framebuffer *raster.Framebuffer
	Runtime     *script.ScriptRuntime
}

func RenderHTMLPage(htmlSrc, extraCSS string, width, height uint32) (*PageRenderResult, error) {
	doc := dom.ParseHTML(htmlSrc)
	rt := script.NewScriptRuntime()
	rt.HydrateInteractiveBindings(doc)
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

	doc.ClearDirtyStyleNodes()
	sm := style.ComputeStylesWithViewport(doc, &combined, width, height)
	lcache := layout.NewLayoutCache()
	rootBox := layout.LayoutDocumentIncremental(doc, sm, lcache, width, height)
	dl := paint.BuildDisplayList(&rootBox)
	fb := raster.Rasterize(&dl, width, height)

	return &PageRenderResult{
		Document:    doc,
		Stylesheet:  combined,
		StyleMap:    sm,
		LayoutRoot:  rootBox,
		LayoutCache: lcache,
		DisplayList: dl,
		Framebuffer: fb,
		Runtime:     rt,
	}, nil
}

func (res *PageRenderResult) RestageIncremental(width, height uint32) (int, int) {
	if res == nil || res.Document == nil {
		return 0, 0
	}
	if res.LayoutCache == nil {
		res.LayoutCache = layout.NewLayoutCache()
	}
	prevHits := res.LayoutCache.Hits
	prevReuses := res.LayoutCache.SubtreeReuses
	sm, recascaded := style.ComputeStylesIncrementalCount(res.Document, &res.Stylesheet, res.StyleMap, width, height)
	res.StyleMap = sm
	res.LayoutRoot = layout.LayoutDocumentIncremental(res.Document, sm, res.LayoutCache, width, height)
	res.DisplayList = paint.BuildDisplayList(&res.LayoutRoot)
	res.Framebuffer = raster.Rasterize(&res.DisplayList, width, height)
	delta := (res.LayoutCache.Hits - prevHits) + (res.LayoutCache.SubtreeReuses - prevReuses)
	return recascaded, delta
}

func (res *PageRenderResult) TypeIntoInput(target dom.NodeId, value string) error {
	if res == nil || res.Document == nil || res.Runtime == nil {
		return nil
	}
	res.Document.SetFocusedNode(&target)
	return res.Runtime.DispatchInput(res.Document, target, value)
}

func (res *PageRenderResult) SubmitForm(pageURL string, controlID dom.NodeId) (*dom.FormSubmissionRequest, bool, error) {
	if res == nil || res.Document == nil {
		return nil, false, nil
	}
	formID, ok := res.Document.EnclosingForm(controlID)
	if !ok {
		return nil, false, nil
	}
	if res.Runtime != nil {
		if err := res.Runtime.DispatchKeydown(res.Document, controlID, "Enter"); err != nil {
			return nil, false, err
		}
		if err := res.Runtime.DispatchSubmit(res.Document, formID); err != nil {
			return nil, false, err
		}
		if res.Runtime.LastEventDefaultPrevented() {
			return nil, true, nil
		}
	}
	req, built := res.Document.BuildFormSubmission(pageURL, controlID)
	return req, built, nil
}

func (res *PageRenderResult) CurrentURL() string {
	if res == nil || res.Runtime == nil {
		return ""
	}
	return res.Runtime.CurrentURL()
}

func (res *PageRenderResult) HitTestAtScroll(viewportX, viewportY, scrollY float32) (dom.NodeId, bool) {
	if res == nil {
		return 0, false
	}
	vh := float32(720)
	if res.Framebuffer != nil && res.Framebuffer.Height > 0 {
		vh = float32(res.Framebuffer.Height)
	}
	return layout.HitTestScrolled(&res.LayoutRoot, viewportX, viewportY+scrollY, scrollY, vh)
}

func (res *PageRenderResult) NodeRectAtScroll(id dom.NodeId, scrollY float32) (layout.Rect, bool) {
	if res == nil {
		return layout.Rect{}, false
	}
	vw := float32(1280)
	vh := float32(720)
	if res.Framebuffer != nil && res.Framebuffer.Width > 0 && res.Framebuffer.Height > 0 {
		vw = float32(res.Framebuffer.Width)
		vh = float32(res.Framebuffer.Height)
	}
	return res.LayoutRoot.FindNodeRectInView(id, 0, scrollY, vw, vh)
}

func (res *PageRenderResult) RenderAtScroll(scrollY float32) *raster.Framebuffer {
	if res == nil {
		return nil
	}
	w := uint32(1280)
	h := uint32(720)
	if res.Framebuffer != nil && res.Framebuffer.Width > 0 && res.Framebuffer.Height > 0 {
		w = res.Framebuffer.Width
		h = res.Framebuffer.Height
	}
	return raster.RasterizeScrolled(&res.DisplayList, w, h, scrollY)
}
