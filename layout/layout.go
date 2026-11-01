// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package layout

import (
	"math"
	"sort"
	"strings"

	"github.com/buntatoes/anvil-engine/dom"
	"github.com/buntatoes/anvil-engine/style"
)

type Rect struct {
	X      float32
	Y      float32
	Width  float32
	Height float32
}

func (r Rect) Contains(x, y float32) bool {
	return x >= r.X && y >= r.Y && x < r.X+r.Width && y < r.Y+r.Height
}

func (r Rect) Intersects(other Rect) bool {
	return r.Width > 0 && r.Height > 0 &&
		other.Width > 0 && other.Height > 0 &&
		r.X < other.X+other.Width &&
		r.X+r.Width > other.X &&
		r.Y < other.Y+other.Height &&
		r.Y+r.Height > other.Y
}

type Image struct {
	Width  uint32
	Height uint32
	RGBA   []byte
}

type TextRun struct {
	Node        dom.NodeId
	Text        string
	X           float32
	Y           float32
	Width       float32
	FontSize    float32
	Color       style.Color
	Underline   bool
	LineThrough bool
}

type StickyConstraint struct {
	Rect       Rect
	Margin     style.Edge
	Containing Rect
	Top        *float32
	Right      *float32
	Bottom     *float32
	Left       *float32
}

func (sc StickyConstraint) Offset(scrollY, viewportH float32) (float32, float32) {
	var dy float32
	if sc.Top != nil {
		minY := scrollY + *sc.Top
		maxY := sc.Containing.Y + sc.Containing.Height - sc.Rect.Height - sc.Margin.Bottom
		if minY > sc.Rect.Y {
			target := minY
			if maxY > sc.Rect.Y && target > maxY {
				target = maxY
			}
			dy = target - sc.Rect.Y
		}
	} else if sc.Bottom != nil {
		maxY := scrollY + viewportH - *sc.Bottom - sc.Rect.Height
		minY := sc.Containing.Y + sc.Margin.Top
		if maxY < sc.Rect.Y {
			target := maxY
			if target < minY {
				target = minY
			}
			dy = target - sc.Rect.Y
		}
	}
	return 0, dy
}

type Transform2D struct {
	M [6]float32 // [a, b, c, d, tx, ty]
}

func IdentityTransform() Transform2D {
	return Transform2D{M: [6]float32{1, 0, 0, 1, 0, 0}}
}

func (t Transform2D) Scaled(s float32) Transform2D {
	return Transform2D{M: [6]float32{t.M[0], t.M[1], t.M[2], t.M[3], t.M[4] * s, t.M[5] * s}}
}

func (t Transform2D) TransformPoint(x, y float32) (float32, float32) {
	return t.M[0]*x + t.M[2]*y + t.M[4], t.M[1]*x + t.M[3]*y + t.M[5]
}

func (t Transform2D) InversePoint(x, y float32) (float32, float32, bool) {
	det := t.M[0]*t.M[3] - t.M[1]*t.M[2]
	if math.Abs(float64(det)) < 1e-6 {
		return 0, 0, false
	}
	invDet := 1.0 / det
	dx := x - t.M[4]
	dy := y - t.M[5]
	ix := (t.M[3]*dx - t.M[2]*dy) * invDet
	iy := (-t.M[1]*dx + t.M[0]*dy) * invDet
	return ix, iy, true
}

type LayoutBox struct {
	Node               dom.NodeId
	Rect               Rect
	Position           style.Position
	ZIndex             int32
	Overflow           style.Overflow
	Background         *style.Color
	BackgroundGradient *style.LinearGradient
	BoxShadow          *style.BoxShadow
	Border             style.Edge
	BorderColor        style.Color
	BorderColors       [4]style.Color
	BorderRadius       float32
	OutlineWidth       float32
	OutlineColor       style.Color
	OutlineOffset      float32
	ScrollableOverflow bool
	ScrollOffsetX      float32
	ScrollOffsetY      float32
	ScrollSizeW        float32
	ScrollSizeH        float32
	ObjectFit          style.ObjectFit
	Image              *Image
	Sticky             *StickyConstraint
	Transform          *Transform2D
	Opacity            float32
	Text               []TextRun
	Children           []LayoutBox
}

func (b *LayoutBox) StackingKey() (int32, uint8) {
	var layered uint8
	if b.Position == style.PositionSticky || b.Transform != nil || b.Opacity < 1.0 {
		layered = 1
	}
	return b.ZIndex, layered
}

func (b *LayoutBox) FindNodeBox(target dom.NodeId) *LayoutBox {
	if b.Node == target {
		return b
	}
	for i := range b.Children {
		if found := b.Children[i].FindNodeBox(target); found != nil {
			return found
		}
	}
	return nil
}

func (b *LayoutBox) FindNodeRect(target dom.NodeId) (Rect, bool) {
	if b.Node == target {
		return b.Rect, true
	}
	for i := range b.Children {
		if r, ok := b.Children[i].FindNodeRect(target); ok {
			return r, true
		}
	}
	for _, run := range b.Text {
		if run.Node == target {
			return Rect{X: run.X, Y: run.Y, Width: run.Width, Height: run.FontSize * 1.25}, true
		}
	}
	return Rect{}, false
}

func (b *LayoutBox) FindScrollableContainer(x, y float32) (dom.NodeId, [2]float32, [2]float32, bool) {
	if !b.Rect.Contains(x, y) && b.Overflow == style.OverflowHidden {
		return 0, [2]float32{}, [2]float32{}, false
	}
	for i := len(b.Children) - 1; i >= 0; i-- {
		if id, off, maxS, ok := b.Children[i].FindScrollableContainer(x, y); ok {
			return id, off, maxS, true
		}
	}
	if b.ScrollableOverflow && b.Rect.Contains(x, y) {
		cw := float32(math.Max(0, float64(b.Rect.Width-b.Border.Left-b.Border.Right)))
		ch := float32(math.Max(0, float64(b.Rect.Height-b.Border.Top-b.Border.Bottom)))
		mx := float32(math.Max(0, float64(b.ScrollSizeW-cw)))
		my := float32(math.Max(0, float64(b.ScrollSizeH-ch)))
		if mx > 0.5 || my > 0.5 {
			return b.Node, [2]float32{b.ScrollOffsetX, b.ScrollOffsetY}, [2]float32{mx, my}, true
		}
	}
	return 0, [2]float32{}, [2]float32{}, false
}

func HitTest(root *LayoutBox, x, y float32) (dom.NodeId, bool) {
	return HitTestScrolled(root, x, y, 0, 720)
}

func HitTestScrolled(box *LayoutBox, x, y, scrollY, viewportH float32) (dom.NodeId, bool) {
	if box == nil {
		return 0, false
	}
	tx, ty := x, y
	if box.Sticky != nil {
		dx, dy := box.Sticky.Offset(scrollY, viewportH)
		tx -= dx
		ty -= dy
	}
	if box.Transform != nil {
		var ok bool
		tx, ty, ok = box.Transform.InversePoint(tx, ty)
		if !ok {
			return 0, false
		}
	}
	if box.Overflow != style.OverflowVisible && !box.Rect.Contains(tx, ty) {
		return 0, false
	}
	indices := make([]int, len(box.Children))
	for i := range indices {
		indices[i] = i
	}
	sort.SliceStable(indices, func(i, j int) bool {
		zA, lA := box.Children[indices[i]].StackingKey()
		zB, lB := box.Children[indices[j]].StackingKey()
		if zA != zB {
			return zA < zB
		}
		if lA != lB {
			return lA < lB
		}
		return indices[i] < indices[j]
	})
	for k := len(indices) - 1; k >= 0; k-- {
		c := &box.Children[indices[k]]
		if hit, ok := HitTestScrolled(c, tx, ty, scrollY, viewportH); ok {
			return hit, true
		}
	}
	for i := len(box.Text) - 1; i >= 0; i-- {
		run := box.Text[i]
		r := Rect{X: run.X, Y: run.Y, Width: run.Width, Height: run.FontSize * 1.25}
		if r.Contains(tx, ty) {
			return run.Node, true
		}
	}
	if box.Rect.Contains(tx, ty) {
		return box.Node, true
	}
	return 0, false
}

func CollectTextRunsInRect(box *LayoutBox, sel Rect) string {
	var parts []string
	var walk func(*LayoutBox)
	walk = func(cur *LayoutBox) {
		if cur == nil {
			return
		}
		for _, r := range cur.Text {
			rr := Rect{X: r.X, Y: r.Y, Width: r.Width, Height: r.FontSize * 1.25}
			if rr.Intersects(sel) && strings.TrimSpace(r.Text) != "" {
				parts = append(parts, r.Text)
			}
		}
		for i := range cur.Children {
			walk(&cur.Children[i])
		}
	}
	walk(box)
	return strings.Join(parts, " ")
}

type LayoutCache struct {
	LastRoot *LayoutBox
}

func NewLayoutCache() *LayoutCache {
	return &LayoutCache{}
}

func LayoutDocument(doc *dom.Document, sm *style.StyleMap, vw, vh uint32) LayoutBox {
	return LayoutDocumentWithImages(doc, sm, nil, vw, vh)
}

func LayoutDocumentIncremental(doc *dom.Document, sm *style.StyleMap, cache *LayoutCache, vw, vh uint32) LayoutBox {
	box := LayoutDocumentWithImages(doc, sm, nil, vw, vh)
	if cache != nil {
		cp := box
		cache.LastRoot = &cp
	}
	return box
}

func LayoutDocumentWithImages(doc *dom.Document, sm *style.StyleMap, images map[dom.NodeId]*Image, vw, vh uint32) LayoutBox {
	rootFont := float32(16.0)
	if sm != nil && sm.RootFontPx > 0 {
		rootFont = sm.RootFontPx
	}
	return layoutNode(doc, sm, images, doc.HTML(), 0, 0, float32(vw), float32(vh), rootFont, float32(vw), float32(vh))
}

func layoutNode(
	doc *dom.Document,
	sm *style.StyleMap,
	images map[dom.NodeId]*Image,
	id dom.NodeId,
	originX, originY, containingW, containingH, rootFont, vw, vh float32,
) LayoutBox {
	st := style.DefaultComputedStyle()
	if sm != nil {
		if found, ok := sm.Get(id); ok {
			st = found
		}
	}
	box := LayoutBox{
		Node:               id,
		Position:           st.Position,
		ZIndex:             st.ZIndex,
		Overflow:           st.Overflow,
		Background:         st.Background,
		BackgroundGradient: st.BackgroundGradient,
		BoxShadow:          st.BoxShadow,
		Border:             st.Border,
		BorderColor:        st.BorderColor,
		BorderColors:       st.BorderColors,
		BorderRadius:       st.BorderRadius,
		OutlineWidth:       st.OutlineWidth,
		OutlineColor:       st.OutlineColor,
		OutlineOffset:      st.OutlineOffset,
		ScrollableOverflow: st.ScrollableOverflow,
		ObjectFit:          st.ObjectFit,
		Opacity:            st.Opacity,
	}
	if sx, sy := doc.NodeScrollOffset(id); sx > 0 || sy > 0 {
		box.ScrollOffsetX = float32(sx)
		box.ScrollOffsetY = float32(sy)
	}
	if st.Display == style.DisplayNone {
		return box
	}

	marginL := st.Margin.Left
	marginR := st.Margin.Right
	marginT := st.Margin.Top
	borderW := st.Border.Left + st.Border.Right
	paddingW := st.Padding.Left + st.Padding.Right
	borderH := st.Border.Top + st.Border.Bottom
	paddingH := st.Padding.Top + st.Padding.Bottom

	var borderBoxW float32
	if wPx, ok := st.Width.ResolvePx(containingW, st.FontSize, rootFont, vw, vh); ok {
		if st.BoxSizing == style.BoxSizingBorderBox {
			borderBoxW = wPx
		} else {
			borderBoxW = wPx + paddingW + borderW
		}
	} else {
		borderBoxW = float32(math.Max(0, float64(containingW-marginL-marginR)))
	}
	if minW, ok := st.MinWidth.ResolvePx(containingW, st.FontSize, rootFont, vw, vh); ok && borderBoxW < minW {
		borderBoxW = minW
	}
	if maxW, ok := st.MaxWidth.ResolvePx(containingW, st.FontSize, rootFont, vw, vh); ok && borderBoxW > maxW {
		borderBoxW = maxW
	}

	// Auto horizontal margin centering
	if st.MarginAuto[1] && st.MarginAuto[3] && borderBoxW < containingW {
		rem := (containingW - borderBoxW) * 0.5
		marginL = rem
		marginR = rem
	} else if st.MarginAuto[3] && borderBoxW < containingW {
		marginL = containingW - borderBoxW - marginR
	}

	x := originX + marginL
	y := originY + marginT
	contentX := x + st.Border.Left + st.Padding.Left
	contentY := y + st.Border.Top + st.Padding.Top
	contentW := float32(math.Max(0, float64(borderBoxW-borderW-paddingW)))

	if images != nil {
		if img, ok := images[id]; ok && img != nil {
			box.Image = img
		}
	}
	if box.Image == nil {
		if surf, ok := doc.CanvasSurface(id); ok && surf != nil {
			box.Image = &Image{Width: surf.Width, Height: surf.Height, RGBA: surf.RGBA}
		}
	}

	cursorY := contentY
	lineX := contentX
	lineH := float32(0)

	// Optional ::before pseudo-element text
	if st.BeforeContent != "" {
		w := MeasureTextWidth(st.BeforeContent, st.FontSize, st.LetterSpacing)
		box.Text = append(box.Text, TextRun{
			Node:     id,
			Text:     st.BeforeContent,
			X:        lineX,
			Y:        cursorY,
			Width:    w,
			FontSize: st.FontSize,
			Color:    st.Color,
		})
		lineX += w
		lineH = st.LineHeight
	}

	children := doc.ComposedChildren(id)
	switch st.Display {
	case style.DisplayFlex, style.DisplayInlineFlex:
		cursorY = layoutFlexChildren(doc, sm, images, &box, children, st, contentX, contentY, contentW, containingH, rootFont, vw, vh)
	case style.DisplayGrid, style.DisplayInlineGrid:
		cursorY = layoutGridChildren(doc, sm, images, &box, children, st, contentX, contentY, contentW, containingH, rootFont, vw, vh)
	default:
		for _, cid := range children {
			cn := doc.Node(cid)
			if cn == nil {
				continue
			}
			if cn.Data.Kind == dom.NodeKindText {
				txt := applyTextTransform(cn.Data.Text, st.TextTransform)
				if st.WhiteSpace != style.WhiteSpacePre && st.WhiteSpace != style.WhiteSpacePreWrap {
					txt = collapseWhitespace(txt)
				}
				if strings.TrimSpace(txt) == "" {
					continue
				}
				words := strings.Split(txt, " ")
				spaceW := st.FontSize*0.3 + st.WordSpacing
				for wi, word := range words {
					if word == "" {
						continue
					}
					ww := MeasureTextWidth(word, st.FontSize, st.LetterSpacing)
					if lineX > contentX && lineX+ww > contentX+contentW && st.WhiteSpace != style.WhiteSpaceNoWrap {
						cursorY += float32(math.Max(float64(lineH), float64(st.LineHeight)))
						lineX = contentX
						lineH = 0
					}
					runTxt := word
					if wi+1 < len(words) {
						runTxt += " "
						ww += spaceW
					}
					box.Text = append(box.Text, TextRun{
						Node:        cid,
						Text:        runTxt,
						X:           lineX,
						Y:           cursorY,
						Width:       ww,
						FontSize:    st.FontSize,
						Color:       st.Color,
						Underline:   st.Underline,
						LineThrough: st.LineThrough,
					})
					lineX += ww
					if st.LineHeight > lineH {
						lineH = st.LineHeight
					}
				}
				continue
			}
			cst := style.DefaultComputedStyle()
			if sm != nil {
				if found, ok := sm.Get(cid); ok {
					cst = found
				}
			}
			if cst.Display == style.DisplayNone {
				continue
			}
			if lineH > 0 && cst.Display != style.DisplayInline && cst.Display != style.DisplayInlineBlock {
				cursorY += lineH
				lineX = contentX
				lineH = 0
			}
			childBox := layoutNode(doc, sm, images, cid, contentX, cursorY, contentW, containingH, rootFont, vw, vh)
			if childBox.Position != style.PositionAbsolute && childBox.Position != style.PositionFixed {
				cursorY = childBox.Rect.Y + childBox.Rect.Height + cst.Margin.Bottom
			}
			box.Children = append(box.Children, childBox)
		}
	}

	if st.AfterContent != "" {
		w := MeasureTextWidth(st.AfterContent, st.FontSize, st.LetterSpacing)
		box.Text = append(box.Text, TextRun{
			Node:     id,
			Text:     st.AfterContent,
			X:        lineX,
			Y:        cursorY,
			Width:    w,
			FontSize: st.FontSize,
			Color:    st.Color,
		})
		if st.LineHeight > lineH {
			lineH = st.LineHeight
		}
	}
	if lineH > 0 {
		cursorY += lineH
	}

	contentH := cursorY - contentY
	if box.Image != nil && contentH < float32(box.Image.Height) {
		contentH = float32(box.Image.Height)
	}
	box.ScrollSizeW = contentW
	box.ScrollSizeH = contentH

	var borderBoxH float32
	if hPx, ok := st.Height.ResolvePx(containingH, st.FontSize, rootFont, vw, vh); ok {
		if st.BoxSizing == style.BoxSizingBorderBox {
			borderBoxH = hPx
		} else {
			borderBoxH = hPx + paddingH + borderH
		}
	} else {
		borderBoxH = contentH + paddingH + borderH
	}
	if minH, ok := st.MinHeight.ResolvePx(containingH, st.FontSize, rootFont, vw, vh); ok && borderBoxH < minH {
		borderBoxH = minH
	}
	if maxH, ok := st.MaxHeight.ResolvePx(containingH, st.FontSize, rootFont, vw, vh); ok && borderBoxH > maxH {
		borderBoxH = maxH
	}

	// Positioned offsets
	switch st.Position {
	case style.PositionRelative:
		if dx, ok := st.Left.ResolvePx(containingW, st.FontSize, rootFont, vw, vh); ok {
			x += dx
		} else if dx, ok := st.Right.ResolvePx(containingW, st.FontSize, rootFont, vw, vh); ok {
			x -= dx
		}
		if dy, ok := st.Top.ResolvePx(containingH, st.FontSize, rootFont, vw, vh); ok {
			y += dy
		} else if dy, ok := st.Bottom.ResolvePx(containingH, st.FontSize, rootFont, vw, vh); ok {
			y -= dy
		}
	case style.PositionAbsolute, style.PositionFixed:
		baseX, baseY := originX, originY
		if st.Position == style.PositionFixed {
			baseX, baseY = 0, 0
		}
		if lx, ok := st.Left.ResolvePx(containingW, st.FontSize, rootFont, vw, vh); ok {
			x = baseX + lx
		} else if rx, ok := st.Right.ResolvePx(containingW, st.FontSize, rootFont, vw, vh); ok {
			x = baseX + containingW - borderBoxW - rx
		}
		if ty, ok := st.Top.ResolvePx(containingH, st.FontSize, rootFont, vw, vh); ok {
			y = baseY + ty
		} else if by, ok := st.Bottom.ResolvePx(containingH, st.FontSize, rootFont, vw, vh); ok {
			y = baseY + containingH - borderBoxH - by
		}
	case style.PositionSticky:
		sc := StickyConstraint{
			Rect:       Rect{X: x, Y: y, Width: borderBoxW, Height: borderBoxH},
			Margin:     st.Margin,
			Containing: Rect{X: originX, Y: originY, Width: containingW, Height: containingH},
		}
		if ty, ok := st.Top.ResolvePx(containingH, st.FontSize, rootFont, vw, vh); ok {
			sc.Top = &ty
		}
		if by, ok := st.Bottom.ResolvePx(containingH, st.FontSize, rootFont, vw, vh); ok {
			sc.Bottom = &by
		}
		box.Sticky = &sc
	}

	box.Rect = Rect{X: x, Y: y, Width: borderBoxW, Height: borderBoxH}
	return box
}

func layoutFlexChildren(
	doc *dom.Document,
	sm *style.StyleMap,
	images map[dom.NodeId]*Image,
	parentBox *LayoutBox,
	children []dom.NodeId,
	st style.ComputedStyle,
	contentX, contentY, contentW, containingH, rootFont, vw, vh float32,
) float32 {
	isRow := st.FlexDirection == style.FlexRow || st.FlexDirection == style.FlexRowReverse
	curX := contentX
	curY := contentY
	maxCross := float32(0)

	var elemChildren []dom.NodeId
	for _, cid := range children {
		if n := doc.Node(cid); n != nil && n.Data.Kind == dom.NodeKindElement {
			if sm != nil {
				if cst, ok := sm.Get(cid); ok && cst.Display == style.DisplayNone {
					continue
				}
			}
			elemChildren = append(elemChildren, cid)
		}
	}
	if len(elemChildren) == 0 {
		return contentY
	}
	itemW := contentW / float32(len(elemChildren))
	if !isRow {
		itemW = contentW
	}
	for idx, cid := range elemChildren {
		cb := layoutNode(doc, sm, images, cid, curX, curY, itemW, containingH, rootFont, vw, vh)
		parentBox.Children = append(parentBox.Children, cb)
		if isRow {
			curX += cb.Rect.Width
			if idx+1 < len(elemChildren) {
				curX += st.ColumnGap
			}
			if cb.Rect.Height > maxCross {
				maxCross = cb.Rect.Height
			}
		} else {
			curY += cb.Rect.Height
			if idx+1 < len(elemChildren) {
				curY += st.RowGap
			}
		}
	}
	if isRow {
		return contentY + maxCross
	}
	return curY
}

func layoutGridChildren(
	doc *dom.Document,
	sm *style.StyleMap,
	images map[dom.NodeId]*Image,
	parentBox *LayoutBox,
	children []dom.NodeId,
	st style.ComputedStyle,
	contentX, contentY, contentW, containingH, rootFont, vw, vh float32,
) float32 {
	cols := len(st.GridTemplateCols)
	if cols <= 0 {
		cols = 1
	}
	totalGaps := float32(cols-1) * st.ColumnGap
	colW := float32(math.Max(0, float64(contentW-totalGaps))) / float32(cols)

	colIdx := 0
	rowY := contentY
	rowMaxH := float32(0)
	for _, cid := range children {
		n := doc.Node(cid)
		if n == nil || n.Data.Kind != dom.NodeKindElement {
			continue
		}
		if sm != nil {
			if cst, ok := sm.Get(cid); ok && cst.Display == style.DisplayNone {
				continue
			}
		}
		cellX := contentX + float32(colIdx)*(colW+st.ColumnGap)
		cb := layoutNode(doc, sm, images, cid, cellX, rowY, colW, containingH, rootFont, vw, vh)
		parentBox.Children = append(parentBox.Children, cb)
		if cb.Rect.Height > rowMaxH {
			rowMaxH = cb.Rect.Height
		}
		colIdx++
		if colIdx >= cols {
			colIdx = 0
			rowY += rowMaxH + st.RowGap
			rowMaxH = 0
		}
	}
	return rowY + rowMaxH
}

func MeasureTextWidth(text string, fontSize, letterSpacing float32) float32 {
	if text == "" {
		return 0
	}
	runes := []rune(text)
	return float32(len(runes))*(fontSize*0.55) + float32(len(runes))*letterSpacing
}

func applyTextTransform(s string, tt style.TextTransform) string {
	switch tt {
	case style.TextTransformUppercase:
		return strings.ToUpper(s)
	case style.TextTransformLowercase:
		return strings.ToLower(s)
	case style.TextTransformCapitalize:
		words := strings.Fields(s)
		for i, w := range words {
			if len(w) > 0 {
				words[i] = strings.ToUpper(w[:1]) + strings.ToLower(w[1:])
			}
		}
		return strings.Join(words, " ")
	default:
		return s
	}
}

func collapseWhitespace(s string) string {
	return strings.Join(strings.Fields(s), " ")
}
