// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package raster

import (
	"math"
	"runtime"
	"sync"

	"github.com/buntatoes/anvil-engine/layout"
	"github.com/buntatoes/anvil-engine/paint"
	"github.com/buntatoes/anvil-engine/style"
)

const (
	TileSize           uint32 = 64
	MaxFramebufferSide uint32 = 8192
)

func isFiniteF32(vals ...float32) bool {
	for _, v := range vals {
		f := float64(v)
		if math.IsNaN(f) || math.IsInf(f, 0) {
			return false
		}
	}
	return true
}

type Framebuffer struct {
	Width  uint32
	Height uint32
	Pixels []byte
}

func NewFramebuffer(width, height uint32) *Framebuffer {
	if width == 0 {
		width = 1
	} else if width > MaxFramebufferSide {
		width = MaxFramebufferSide
	}
	if height == 0 {
		height = 1
	} else if height > MaxFramebufferSide {
		height = MaxFramebufferSide
	}
	return &Framebuffer{
		Width:  width,
		Height: height,
		Pixels: make([]byte, int(width)*int(height)*4),
	}
}

func (fb *Framebuffer) Clear(c style.Color) {
	for i := 0; i+3 < len(fb.Pixels); i += 4 {
		fb.Pixels[i] = c.R
		fb.Pixels[i+1] = c.G
		fb.Pixels[i+2] = c.B
		fb.Pixels[i+3] = c.A
	}
}

func (fb *Framebuffer) CopyRowsFrom(src *Framebuffer, yStart, yEnd uint32) {
	if src == nil || src.Width != fb.Width || src.Height != fb.Height {
		return
	}
	if yEnd > fb.Height {
		yEnd = fb.Height
	}
	if yStart >= yEnd {
		return
	}
	rowBytes := int(fb.Width * 4)
	off0 := int(yStart) * rowBytes
	off1 := int(yEnd) * rowBytes
	copy(fb.Pixels[off0:off1], src.Pixels[off0:off1])
}

func (fb *Framebuffer) ToBgraBottomUp() []byte {
	out := make([]byte, len(fb.Pixels))
	w := int(fb.Width)
	h := int(fb.Height)
	stride := w * 4
	for y := 0; y < h; y++ {
		srcRow := y * stride
		dstRow := (h - 1 - y) * stride
		for x := 0; x < w; x++ {
			si := srcRow + x*4
			di := dstRow + x*4
			out[di] = fb.Pixels[si+2]   // B
			out[di+1] = fb.Pixels[si+1] // G
			out[di+2] = fb.Pixels[si]   // R
			out[di+3] = fb.Pixels[si+3] // A
		}
	}
	return out
}

func (fb *Framebuffer) blendPixel(x, y int, c style.Color) {
	if x < 0 || y < 0 || x >= int(fb.Width) || y >= int(fb.Height) || c.A == 0 {
		return
	}
	idx := (y*int(fb.Width) + x) * 4
	if c.A == 255 {
		fb.Pixels[idx] = c.R
		fb.Pixels[idx+1] = c.G
		fb.Pixels[idx+2] = c.B
		fb.Pixels[idx+3] = 255
		return
	}
	sa := uint32(c.A)
	inv := 255 - sa
	fb.Pixels[idx] = uint8((uint32(c.R)*sa + uint32(fb.Pixels[idx])*inv + 127) / 255)
	fb.Pixels[idx+1] = uint8((uint32(c.G)*sa + uint32(fb.Pixels[idx+1])*inv + 127) / 255)
	fb.Pixels[idx+2] = uint8((uint32(c.B)*sa + uint32(fb.Pixels[idx+2])*inv + 127) / 255)
	fb.Pixels[idx+3] = uint8(sa + (uint32(fb.Pixels[idx+3])*inv+127)/255)
}

type clipRect struct {
	x0, y0, x1, y1 int
}

func (fb *Framebuffer) defaultClip() clipRect {
	return clipRect{x0: 0, y0: 0, x1: int(fb.Width), y1: int(fb.Height)}
}

func intersectClip(a, b clipRect) clipRect {
	x0 := a.x0
	if b.x0 > x0 {
		x0 = b.x0
	}
	y0 := a.y0
	if b.y0 > y0 {
		y0 = b.y0
	}
	x1 := a.x1
	if b.x1 < x1 {
		x1 = b.x1
	}
	y1 := a.y1
	if b.y1 < y1 {
		y1 = b.y1
	}
	return clipRect{x0: x0, y0: y0, x1: x1, y1: y1}
}

func (fb *Framebuffer) FillRectClipped(x, y, w, h float32, c style.Color, clip clipRect) {
	if !isFiniteF32(x, y, w, h) || w <= 0 || h <= 0 || c.A == 0 {
		return
	}
	rc := intersectClip(clip, clipRect{
		x0: int(math.Floor(float64(x))),
		y0: int(math.Floor(float64(y))),
		x1: int(math.Ceil(float64(x + w))),
		y1: int(math.Ceil(float64(y + h))),
	})
	for py := rc.y0; py < rc.y1; py++ {
		for px := rc.x0; px < rc.x1; px++ {
			fb.blendPixel(px, py, c)
		}
	}
}

func sdfRoundedBox(px, py, cx, cy, halfW, halfH, radius float32) float32 {
	r := radius
	if r > halfW {
		r = halfW
	}
	if r > halfH {
		r = halfH
	}
	if r < 0 {
		r = 0
	}
	qx := float32(math.Abs(float64(px-cx))) - (halfW - r)
	qy := float32(math.Abs(float64(py-cy))) - (halfH - r)
	ox := float32(math.Max(float64(qx), 0))
	oy := float32(math.Max(float64(qy), 0))
	outside := float32(math.Hypot(float64(ox), float64(oy)))
	inside := float32(math.Min(math.Max(float64(qx), float64(qy)), 0))
	return outside + inside - r
}

func (fb *Framebuffer) FillRoundedRectSdf(x, y, w, h, radius, borderW float32, fill, border style.Color, clip clipRect) {
	if !isFiniteF32(x, y, w, h, radius, borderW) || w <= 0 || h <= 0 {
		return
	}
	rc := intersectClip(clip, clipRect{
		x0: int(math.Floor(float64(x - 1))),
		y0: int(math.Floor(float64(y - 1))),
		x1: int(math.Ceil(float64(x + w + 1))),
		y1: int(math.Ceil(float64(y + h + 1))),
	})
	cx := x + w*0.5
	cy := y + h*0.5
	hw := w * 0.5
	hh := h * 0.5
	for py := rc.y0; py < rc.y1; py++ {
		for px := rc.x0; px < rc.x1; px++ {
			d := sdfRoundedBox(float32(px)+0.5, float32(py)+0.5, cx, cy, hw, hh, radius)
			cov := clamp01(0.5 - d)
			if cov <= 0 {
				continue
			}
			col := fill
			if borderW > 0 && d > -borderW {
				col = border
			}
			col.A = uint8(float32(col.A)*cov + 0.5)
			fb.blendPixel(px, py, col)
		}
	}
}

func (fb *Framebuffer) FillBoxShadowSdf(x, y, w, h, offX, offY, blur, spread, radius float32, col style.Color, clip clipRect) {
	if !isFiniteF32(x, y, w, h, offX, offY, blur, spread, radius) || w <= 0 || h <= 0 || col.A == 0 {
		return
	}
	sx := x + offX - spread
	sy := y + offY - spread
	sw := w + spread*2
	sh := h + spread*2
	pad := blur*2 + 2
	rc := intersectClip(clip, clipRect{
		x0: int(math.Floor(float64(sx - pad))),
		y0: int(math.Floor(float64(sy - pad))),
		x1: int(math.Ceil(float64(sx + sw + pad))),
		y1: int(math.Ceil(float64(sy + sh + pad))),
	})
	cx := sx + sw*0.5
	cy := sy + sh*0.5
	hw := sw * 0.5
	hh := sh * 0.5
	b := float32(math.Max(float64(blur), 0.5))
	for py := rc.y0; py < rc.y1; py++ {
		for px := rc.x0; px < rc.x1; px++ {
			d := sdfRoundedBox(float32(px)+0.5, float32(py)+0.5, cx, cy, hw, hh, radius+spread)
			t := clamp01(0.5 - d/b)
			alpha := t * t * (3.0 - 2.0*t)
			if alpha <= 0 {
				continue
			}
			c := col
			c.A = uint8(float32(col.A)*alpha + 0.5)
			fb.blendPixel(px, py, c)
		}
	}
}

func (fb *Framebuffer) FillLinearGradientSdf(x, y, w, h, radius, borderW, angleDeg float32, c0, c1, border style.Color, clip clipRect) {
	if !isFiniteF32(x, y, w, h, radius, borderW, angleDeg) || w <= 0 || h <= 0 {
		return
	}
	rc := intersectClip(clip, clipRect{
		x0: int(math.Floor(float64(x))),
		y0: int(math.Floor(float64(y))),
		x1: int(math.Ceil(float64(x + w))),
		y1: int(math.Ceil(float64(y + h))),
	})
	cx := x + w*0.5
	cy := y + h*0.5
	hw := w * 0.5
	hh := h * 0.5
	rad := float64(angleDeg-90.0) * math.Pi / 180.0
	dx := float32(math.Cos(rad))
	dy := float32(math.Sin(rad))
	maxProj := float32(math.Abs(float64(dx))*float64(hw) + math.Abs(float64(dy))*float64(hh))
	if maxProj < 1e-4 {
		maxProj = 1
	}
	for py := rc.y0; py < rc.y1; py++ {
		for px := rc.x0; px < rc.x1; px++ {
			fx := float32(px) + 0.5
			fy := float32(py) + 0.5
			d := sdfRoundedBox(fx, fy, cx, cy, hw, hh, radius)
			cov := clamp01(0.5 - d)
			if cov <= 0 {
				continue
			}
			var col style.Color
			if borderW > 0 && d > -borderW {
				col = border
			} else {
				proj := (fx-cx)*dx + (fy-cy)*dy
				t := clamp01((proj + maxProj) / (2.0 * maxProj))
				col = style.Color{
					R: uint8(float32(c0.R)*(1-t) + float32(c1.R)*t + 0.5),
					G: uint8(float32(c0.G)*(1-t) + float32(c1.G)*t + 0.5),
					B: uint8(float32(c0.B)*(1-t) + float32(c1.B)*t + 0.5),
					A: uint8(float32(c0.A)*(1-t) + float32(c1.A)*t + 0.5),
				}
			}
			col.A = uint8(float32(col.A)*cov + 0.5)
			fb.blendPixel(px, py, col)
		}
	}
}

func (fb *Framebuffer) BlitImage(x, y, w, h, radius float32, img *layout.Image, clip clipRect) {
	if !isFiniteF32(x, y, w, h, radius) || img == nil || img.Width == 0 || img.Height == 0 || w <= 0 || h <= 0 {
		return
	}
	rc := intersectClip(clip, clipRect{
		x0: int(math.Floor(float64(x))),
		y0: int(math.Floor(float64(y))),
		x1: int(math.Ceil(float64(x + w))),
		y1: int(math.Ceil(float64(y + h))),
	})
	for py := rc.y0; py < rc.y1; py++ {
		v := (float32(py) - y) / h
		sy := int(v * float32(img.Height))
		if sy < 0 {
			sy = 0
		} else if sy >= int(img.Height) {
			sy = int(img.Height) - 1
		}
		for px := rc.x0; px < rc.x1; px++ {
			u := (float32(px) - x) / w
			sx := int(u * float32(img.Width))
			if sx < 0 {
				sx = 0
			} else if sx >= int(img.Width) {
				sx = int(img.Width) - 1
			}
			si := (sy*int(img.Width) + sx) * 4
			if si+3 < len(img.RGBA) {
				fb.blendPixel(px, py, style.RGBA(img.RGBA[si], img.RGBA[si+1], img.RGBA[si+2], img.RGBA[si+3]))
			}
		}
	}
}

func (fb *Framebuffer) DrawText(x, y, fontSize float32, text string, col style.Color, clip clipRect) {
	if !isFiniteF32(x, y, fontSize) || text == "" || col.A == 0 || fontSize <= 0 || fontSize > 1024 {
		return
	}
	scale := int(math.Max(1, math.Round(float64(fontSize)/10.0)))
	adv := 6 * scale
	curX := int(math.Round(float64(x)))
	baseY := int(math.Round(float64(y)))
	for _, ch := range text {
		glyph := glyphBitmap5x7(ch)
		for row := 0; row < 7; row++ {
			bits := glyph[row]
			for colBit := 0; colBit < 5; colBit++ {
				if (bits & (1 << (4 - colBit))) != 0 {
					for dy := 0; dy < scale; dy++ {
						for dx := 0; dx < scale; dx++ {
							px := curX + colBit*scale + dx
							py := baseY + row*scale + dy
							if px >= clip.x0 && px < clip.x1 && py >= clip.y0 && py < clip.y1 {
								fb.blendPixel(px, py, col)
							}
						}
					}
				}
			}
		}
		curX += adv
	}
}

func SrgbToLinearU12(srgb uint8) uint16 {
	s := float64(srgb) / 255.0
	var lin float64
	if s <= 0.04045 {
		lin = s / 12.92
	} else {
		lin = math.Pow((s+0.055)/1.055, 2.4)
	}
	return uint16(math.Round(lin * 4095.0))
}

func LinearU12ToSrgb(lin uint16) uint8 {
	if lin >= 4095 {
		return 255
	}
	l := float64(lin) / 4095.0
	var s float64
	if l <= 0.0031308 {
		s = l * 12.92
	} else {
		s = 1.055*math.Pow(l, 1.0/2.4) - 0.055
	}
	return uint8(math.Round(s * 255.0))
}

func BlendGlyphPixelGammaCorrect(dst [4]uint8, fg style.Color, coverage uint8) [4]uint8 {
	if coverage == 0 || fg.A == 0 {
		return dst
	}
	alpha := (uint32(coverage) * uint32(fg.A)) / 255
	inv := 255 - alpha
	dr := uint32(SrgbToLinearU12(dst[0]))
	dg := uint32(SrgbToLinearU12(dst[1]))
	db := uint32(SrgbToLinearU12(dst[2]))
	sr := uint32(SrgbToLinearU12(fg.R))
	sg := uint32(SrgbToLinearU12(fg.G))
	sb := uint32(SrgbToLinearU12(fg.B))
	or := LinearU12ToSrgb(uint16((sr*alpha + dr*inv + 127) / 255))
	og := LinearU12ToSrgb(uint16((sg*alpha + dg*inv + 127) / 255))
	ob := LinearU12ToSrgb(uint16((sb*alpha + db*inv + 127) / 255))
	oa := uint8(alpha + (uint32(dst[3])*inv+127)/255)
	return [4]uint8{or, og, ob, oa}
}

func Rasterize(dl *paint.DisplayList, width, height uint32) *Framebuffer {
	return RasterizeScrolled(dl, width, height, 0)
}

func RasterizeScrolled(dl *paint.DisplayList, width, height uint32, scrollY float32) *Framebuffer {
	fb := NewFramebuffer(width, height)
	fb.Clear(style.RGBA(255, 255, 255, 255))
	if dl == nil {
		return fb
	}
	clipStack := []clipRect{fb.defaultClip()}
	fixedDepth := 0

	for _, item := range dl.Items {
		curClip := clipStack[len(clipStack)-1]
		sy := scrollY
		if fixedDepth > 0 {
			sy = 0
		}
		switch item.Kind {
		case paint.ItemPushFixed:
			fixedDepth++
		case paint.ItemPopFixed:
			if fixedDepth > 0 {
				fixedDepth--
			}
		case paint.ItemPushClip:
			nc := intersectClip(curClip, clipRect{
				x0: int(math.Floor(float64(item.X))),
				y0: int(math.Floor(float64(item.Y - sy))),
				x1: int(math.Ceil(float64(item.X + item.Width))),
				y1: int(math.Ceil(float64(item.Y - sy + item.Height))),
			})
			clipStack = append(clipStack, nc)
		case paint.ItemPopClip:
			if len(clipStack) > 1 {
				clipStack = clipStack[:len(clipStack)-1]
			}
		case paint.ItemRect:
			fb.FillRectClipped(item.X, item.Y-sy, item.Width, item.Height, item.Color, curClip)
		case paint.ItemRoundedRect:
			fb.FillRoundedRectSdf(item.X, item.Y-sy, item.Width, item.Height, item.Radius, item.BorderWidth, item.FillColor, item.BorderColor, curClip)
		case paint.ItemBoxShadow:
			fb.FillBoxShadowSdf(item.X, item.Y-sy, item.Width, item.Height, item.OffsetX, item.OffsetY, item.BlurRadius, item.SpreadRadius, item.Radius, item.Color, curClip)
		case paint.ItemLinearGradientRect:
			fb.FillLinearGradientSdf(item.X, item.Y-sy, item.Width, item.Height, item.Radius, item.BorderWidth, item.AngleDeg, item.StartColor, item.EndColor, item.BorderColor, curClip)
		case paint.ItemImage:
			fb.BlitImage(item.X, item.Y-sy, item.Width, item.Height, item.Radius, item.Image, curClip)
		case paint.ItemText:
			fb.DrawText(item.X, item.Y-sy, item.FontSize, item.Text, item.Color, curClip)
		}
	}
	return fb
}

type RasterEngine struct {
	Workers int
}

func NewRasterEngine() *RasterEngine {
	w := runtime.NumCPU()
	if w < 1 {
		w = 1
	}
	return &RasterEngine{Workers: w}
}

func (re *RasterEngine) RenderDisplayList(dl *paint.DisplayList, width, height uint32, scrollY float32) *Framebuffer {
	var wg sync.WaitGroup
	var out *Framebuffer
	wg.Add(1)
	go func() {
		defer wg.Done()
		out = RasterizeScrolled(dl, width, height, scrollY)
	}()
	wg.Wait()
	return out
}

func clamp01(v float32) float32 {
	if math.IsNaN(float64(v)) || v <= 0 {
		return 0
	}
	if v >= 1 {
		return 1
	}
	return v
}

func glyphBitmap5x7(ch rune) [7]uint8 {
	if ch >= 'a' && ch <= 'z' {
		ch -= 32
	}
	switch ch {
	case 'A':
		return [7]uint8{0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11}
	case 'B':
		return [7]uint8{0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E}
	case 'C':
		return [7]uint8{0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E}
	case 'D':
		return [7]uint8{0x1C, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1C}
	case 'E':
		return [7]uint8{0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F}
	case 'F':
		return [7]uint8{0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10}
	case 'G':
		return [7]uint8{0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F}
	case 'H':
		return [7]uint8{0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11}
	case 'I':
		return [7]uint8{0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E}
	case 'L':
		return [7]uint8{0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F}
	case 'N':
		return [7]uint8{0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11}
	case 'O':
		return [7]uint8{0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E}
	case 'R':
		return [7]uint8{0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11}
	case 'S':
		return [7]uint8{0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E}
	case 'T':
		return [7]uint8{0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04}
	case 'V':
		return [7]uint8{0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04}
	case ' ':
		return [7]uint8{0, 0, 0, 0, 0, 0, 0}
	default:
		return [7]uint8{0x0E, 0x11, 0x15, 0x15, 0x15, 0x11, 0x0E}
	}
}
