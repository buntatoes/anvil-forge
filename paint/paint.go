// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package paint

import (
	"math"
	"sort"

	"github.com/buntatoes/anvil-engine/layout"
	"github.com/buntatoes/anvil-engine/style"
)

type DisplayItemKind int

const (
	ItemPushClip DisplayItemKind = iota
	ItemPopClip
	ItemPushFixed
	ItemPopFixed
	ItemBoxShadow
	ItemLinearGradientRect
	ItemRect
	ItemRoundedRect
	ItemImage
	ItemText
)

type DisplayItem struct {
	Kind         DisplayItemKind
	X            float32
	Y            float32
	Width        float32
	Height       float32
	OffsetX      float32
	OffsetY      float32
	BlurRadius   float32
	SpreadRadius float32
	Radius       float32
	BorderWidth  float32
	AngleDeg     float32
	Color        style.Color
	StartColor   style.Color
	EndColor     style.Color
	FillColor    style.Color
	BorderColor  style.Color
	Text         string
	FontSize     float32
	Image        *layout.Image
}

func (item DisplayItem) Mapped(s, tx, ty float32) DisplayItem {
	out := item
	switch item.Kind {
	case ItemPopClip, ItemPushFixed, ItemPopFixed:
		return out
	case ItemPushClip, ItemRect, ItemImage:
		out.X = item.X*s + tx
		out.Y = item.Y*s + ty
		out.Width = item.Width * s
		out.Height = item.Height * s
		out.Radius = item.Radius * s
	case ItemBoxShadow:
		out.X = item.X*s + tx
		out.Y = item.Y*s + ty
		out.Width = item.Width * s
		out.Height = item.Height * s
		out.OffsetX = item.OffsetX * s
		out.OffsetY = item.OffsetY * s
		out.BlurRadius = item.BlurRadius * s
		out.SpreadRadius = item.SpreadRadius * s
		out.Radius = item.Radius * s
	case ItemLinearGradientRect, ItemRoundedRect:
		out.X = item.X*s + tx
		out.Y = item.Y*s + ty
		out.Width = item.Width * s
		out.Height = item.Height * s
		out.Radius = item.Radius * s
		out.BorderWidth = item.BorderWidth * s
	case ItemText:
		out.X = item.X*s + tx
		out.Y = item.Y*s + ty
		out.FontSize = item.FontSize * s
	}
	return out
}

type LayerEffectKind int

const (
	LayerSticky LayerEffectKind = iota
	LayerTransform
	LayerOpacity
)

type LayerEffect struct {
	Kind      LayerEffectKind
	Sticky    layout.StickyConstraint
	Transform layout.Transform2D
	Opacity   float32
}

type Layer struct {
	Start  int
	End    int
	Effect LayerEffect
}

func (l Layer) Scaled(s float32) Layer {
	out := l
	switch l.Effect.Kind {
	case LayerSticky:
		sc := l.Effect.Sticky
		scaleRect := func(r layout.Rect) layout.Rect {
			return layout.Rect{X: r.X * s, Y: r.Y * s, Width: r.Width * s, Height: r.Height * s}
		}
		sc.Rect = scaleRect(sc.Rect)
		sc.Containing = scaleRect(sc.Containing)
		sc.Margin = style.Edge{
			Top:    sc.Margin.Top * s,
			Right:  sc.Margin.Right * s,
			Bottom: sc.Margin.Bottom * s,
			Left:   sc.Margin.Left * s,
		}
		out.Effect.Sticky = sc
	case LayerTransform:
		out.Effect.Transform = l.Effect.Transform.Scaled(s)
	}
	return out
}

type DisplayList struct {
	Items  []DisplayItem
	Layers []Layer
}

func (dl DisplayList) Scaled(scale float32) DisplayList {
	s := float32(math.Max(0.25, math.Min(4.0, float64(scale))))
	if math.Abs(float64(s-1.0)) < 1e-6 {
		items := make([]DisplayItem, len(dl.Items))
		copy(items, dl.Items)
		layers := make([]Layer, len(dl.Layers))
		copy(layers, dl.Layers)
		return DisplayList{Items: items, Layers: layers}
	}
	items := make([]DisplayItem, len(dl.Items))
	for i, it := range dl.Items {
		items[i] = it.Mapped(s, 0, 0)
	}
	layers := make([]Layer, len(dl.Layers))
	for i, ly := range dl.Layers {
		layers[i] = ly.Scaled(s)
	}
	return DisplayList{Items: items, Layers: layers}
}

func BuildDisplayList(root *layout.LayoutBox) DisplayList {
	var dl DisplayList
	if root != nil {
		paintBox(&dl, root)
	}
	return dl
}

func paintBox(dl *DisplayList, box *layout.LayoutBox) {
	if box.Opacity <= 0 {
		return
	}
	isFixed := box.Position == style.PositionFixed
	if isFixed {
		dl.Items = append(dl.Items, DisplayItem{Kind: ItemPushFixed})
	}
	startIdx := len(dl.Items)

	r := box.Rect
	maxBorder := max4(box.Border.Top, box.Border.Right, box.Border.Bottom, box.Border.Left)

	if box.BoxShadow != nil && box.BoxShadow.Color.A > 0 {
		bs := box.BoxShadow
		dl.Items = append(dl.Items, DisplayItem{
			Kind:         ItemBoxShadow,
			X:            r.X,
			Y:            r.Y,
			Width:        r.Width,
			Height:       r.Height,
			OffsetX:      bs.OffsetX,
			OffsetY:      bs.OffsetY,
			BlurRadius:   bs.BlurRadius,
			SpreadRadius: bs.SpreadRadius,
			Radius:       box.BorderRadius,
			Color:        bs.Color,
		})
	}

	if box.BackgroundGradient != nil {
		bg := box.BackgroundGradient
		dl.Items = append(dl.Items, DisplayItem{
			Kind:        ItemLinearGradientRect,
			X:           r.X,
			Y:           r.Y,
			Width:       r.Width,
			Height:      r.Height,
			Radius:      box.BorderRadius,
			BorderWidth: maxBorder,
			AngleDeg:    bg.AngleDeg,
			StartColor:  bg.StartColor,
			EndColor:    bg.EndColor,
			BorderColor: box.BorderColor,
		})
	} else if box.BorderRadius > 0 {
		fill := style.RGBA(0, 0, 0, 0)
		if box.Background != nil {
			fill = *box.Background
		}
		if fill.A > 0 || (maxBorder > 0 && box.BorderColor.A > 0) {
			dl.Items = append(dl.Items, DisplayItem{
				Kind:        ItemRoundedRect,
				X:           r.X,
				Y:           r.Y,
				Width:       r.Width,
				Height:      r.Height,
				Radius:      box.BorderRadius,
				BorderWidth: maxBorder,
				FillColor:   fill,
				BorderColor: box.BorderColor,
			})
		}
	} else {
		if box.Background != nil && box.Background.A > 0 {
			dl.Items = append(dl.Items, DisplayItem{
				Kind:   ItemRect,
				X:      r.X,
				Y:      r.Y,
				Width:  r.Width,
				Height: r.Height,
				Color:  *box.Background,
			})
		}
		// 4-side borders
		if box.Border.Top > 0 && box.BorderColors[0].A > 0 {
			dl.Items = append(dl.Items, DisplayItem{
				Kind: ItemRect, X: r.X, Y: r.Y, Width: r.Width, Height: box.Border.Top, Color: box.BorderColors[0],
			})
		}
		if box.Border.Bottom > 0 && box.BorderColors[2].A > 0 {
			dl.Items = append(dl.Items, DisplayItem{
				Kind: ItemRect, X: r.X, Y: r.Y + r.Height - box.Border.Bottom, Width: r.Width, Height: box.Border.Bottom, Color: box.BorderColors[2],
			})
		}
		if box.Border.Left > 0 && box.BorderColors[3].A > 0 {
			dl.Items = append(dl.Items, DisplayItem{
				Kind: ItemRect, X: r.X, Y: r.Y, Width: box.Border.Left, Height: r.Height, Color: box.BorderColors[3],
			})
		}
		if box.Border.Right > 0 && box.BorderColors[1].A > 0 {
			dl.Items = append(dl.Items, DisplayItem{
				Kind: ItemRect, X: r.X + r.Width - box.Border.Right, Y: r.Y, Width: box.Border.Right, Height: r.Height, Color: box.BorderColors[1],
			})
		}
	}

	pushedClip := false
	if box.Overflow != style.OverflowVisible {
		cx := r.X + box.Border.Left
		cy := r.Y + box.Border.Top
		cw := float32(math.Max(0, float64(r.Width-box.Border.Left-box.Border.Right)))
		ch := float32(math.Max(0, float64(r.Height-box.Border.Top-box.Border.Bottom)))
		dl.Items = append(dl.Items, DisplayItem{
			Kind: ItemPushClip, X: cx, Y: cy, Width: cw, Height: ch,
		})
		pushedClip = true
	}

	if box.Image != nil && box.Image.Width > 0 && box.Image.Height > 0 {
		ix := r.X + box.Border.Left
		iy := r.Y + box.Border.Top
		iw := float32(math.Max(0, float64(r.Width-box.Border.Left-box.Border.Right)))
		ih := float32(math.Max(0, float64(r.Height-box.Border.Top-box.Border.Bottom)))
		rad := float32(math.Max(0, float64(box.BorderRadius-box.Border.Top)))
		dl.Items = append(dl.Items, DisplayItem{
			Kind:   ItemImage,
			X:      ix,
			Y:      iy,
			Width:  iw,
			Height: ih,
			Radius: rad,
			Image:  box.Image,
		})
	}

	for _, run := range box.Text {
		dl.Items = append(dl.Items, DisplayItem{
			Kind:     ItemText,
			X:        run.X,
			Y:        run.Y,
			Text:     run.Text,
			Color:    run.Color,
			FontSize: run.FontSize,
		})
		if run.Underline {
			dl.Items = append(dl.Items, DisplayItem{
				Kind:   ItemRect,
				X:      run.X,
				Y:      run.Y + run.FontSize + 1,
				Width:  run.Width,
				Height: 1,
				Color:  run.Color,
			})
		}
		if run.LineThrough {
			dl.Items = append(dl.Items, DisplayItem{
				Kind:   ItemRect,
				X:      run.X,
				Y:      run.Y + run.FontSize*0.5,
				Width:  run.Width,
				Height: 1,
				Color:  run.Color,
			})
		}
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
	for _, idx := range indices {
		paintBox(dl, &box.Children[idx])
	}

	// Overlay 6px scrollbar when scrollable overflow exceeds client height
	if box.ScrollableOverflow && box.ScrollSizeH > r.Height && r.Height > 12 {
		trackX := r.X + r.Width - box.Border.Right - 6
		trackY := r.Y + box.Border.Top
		trackH := r.Height - box.Border.Top - box.Border.Bottom
		dl.Items = append(dl.Items, DisplayItem{
			Kind: ItemRect, X: trackX, Y: trackY, Width: 6, Height: trackH, Color: style.RGBA(0, 0, 0, 32),
		})
		ratio := trackH / box.ScrollSizeH
		thumbH := float32(math.Max(16, float64(trackH*ratio)))
		maxScroll := box.ScrollSizeH - trackH
		scrollFrac := float32(0)
		if maxScroll > 0 {
			scrollFrac = box.ScrollOffsetY / maxScroll
		}
		thumbY := trackY + scrollFrac*(trackH-thumbH)
		dl.Items = append(dl.Items, DisplayItem{
			Kind: ItemRect, X: trackX, Y: thumbY, Width: 6, Height: thumbH, Color: style.RGBA(96, 96, 96, 160),
		})
	}

	if pushedClip {
		dl.Items = append(dl.Items, DisplayItem{Kind: ItemPopClip})
	}

	endIdx := len(dl.Items)
	if endIdx > startIdx {
		if box.Sticky != nil {
			dl.Layers = append(dl.Layers, Layer{
				Start:  startIdx,
				End:    endIdx,
				Effect: LayerEffect{Kind: LayerSticky, Sticky: *box.Sticky},
			})
		}
		if box.Transform != nil {
			dl.Layers = append(dl.Layers, Layer{
				Start:  startIdx,
				End:    endIdx,
				Effect: LayerEffect{Kind: LayerTransform, Transform: *box.Transform},
			})
		}
		if box.Opacity < 1.0 {
			dl.Layers = append(dl.Layers, Layer{
				Start:  startIdx,
				End:    endIdx,
				Effect: LayerEffect{Kind: LayerOpacity, Opacity: box.Opacity},
			})
		}
	}

	if isFixed {
		dl.Items = append(dl.Items, DisplayItem{Kind: ItemPopFixed})
	}
}

func max4(a, b, c, d float32) float32 {
	m := a
	if b > m {
		m = b
	}
	if c > m {
		m = c
	}
	if d > m {
		m = d
	}
	return m
}
