// Package icon renders the menu-bar icons programmatically (no binary assets).
//
// Three states:
//   - BaseTemplatePNG: a filled squircle, monochrome. Used via
//     systray.SetTemplateIcon so macOS tints it to match a light or dark menu bar.
//   - AlertPNG: the squircle filled solid red. Alternated with the base icon to
//     blink while an incident is unacknowledged.
//   - IncidentPNG: the base squircle plus a red notification dot. Shown once the
//     user has opened the menu and the blinking has stopped.
//
// Because a coloured (non-template) icon does NOT auto-adapt, the two red icons
// get a thin white halo so their outline stays visible on light and dark menu
// bars alike.
package icon

import (
	"bytes"
	"image"
	"image/color"
	"image/draw"
	"image/png"
	"math"
)

const size = 44 // 22pt @2x; macOS scales to the menu-bar height

// Geometry (in the 44×44 canvas).
const (
	cx = size / 2.0 // squircle centre
	cy = size / 2.0
	bx = 32.0 // red-dot badge centre, top-right
	by = 12.0
)

var (
	colBody = color.NRGBA{R: 0x1c, G: 0x1c, B: 0x1e, A: 0xff} // near-black squircle
	colHalo = color.NRGBA{R: 0xff, G: 0xff, B: 0xff, A: 0xff} // white outline / badge ring
	colRed  = color.NRGBA{R: 0xff, G: 0x3b, B: 0x30, A: 0xff} // notification dot
	colMono = color.NRGBA{R: 0x00, G: 0x00, B: 0x00, A: 0xff} // template (only alpha matters)
)

// sdRoundBox is the signed distance from p to a rounded box centred at the
// origin with the given half-extent and corner radius (negative = inside).
func sdRoundBox(px, py, half, r float64) float64 {
	qx := math.Abs(px) - half + r
	qy := math.Abs(py) - half + r
	return math.Hypot(math.Max(qx, 0), math.Max(qy, 0)) + math.Min(math.Max(qx, qy), 0) - r
}

func squircle(half, r float64) func(x, y float64) bool {
	return func(x, y float64) bool { return sdRoundBox(x-cx, y-cy, half, r) <= 0 }
}

func disc(centerX, centerY, rad float64) func(x, y float64) bool {
	return func(x, y float64) bool {
		dx, dy := x-centerX, y-centerY
		return dx*dx+dy*dy <= rad*rad
	}
}

// maskFor builds an anti-aliased coverage mask for a shape predicate by 4×4
// supersampling each pixel.
func maskFor(b image.Rectangle, inside func(x, y float64) bool) *image.Alpha {
	const ss = 4
	m := image.NewAlpha(b)
	for py := b.Min.Y; py < b.Max.Y; py++ {
		for px := b.Min.X; px < b.Max.X; px++ {
			hits := 0
			for sy := 0; sy < ss; sy++ {
				for sx := 0; sx < ss; sx++ {
					x := float64(px) + (float64(sx)+0.5)/ss
					y := float64(py) + (float64(sy)+0.5)/ss
					if inside(x, y) {
						hits++
					}
				}
			}
			if hits > 0 {
				m.SetAlpha(px, py, color.Alpha{A: uint8(hits * 255 / (ss * ss))})
			}
		}
	}
	return m
}

func drawLayer(dst *image.NRGBA, inside func(x, y float64) bool, col color.NRGBA) {
	mask := maskFor(dst.Bounds(), inside)
	draw.DrawMask(dst, dst.Bounds(), image.NewUniform(col), image.Point{}, mask, image.Point{}, draw.Over)
}

func encode(img *image.NRGBA) []byte {
	var buf bytes.Buffer
	_ = png.Encode(&buf, img)
	return buf.Bytes()
}

// BaseTemplatePNG returns the idle (all-clear) icon.
func BaseTemplatePNG() []byte {
	img := image.NewNRGBA(image.Rect(0, 0, size, size))
	drawLayer(img, squircle(16, 8), colMono)
	return encode(img)
}

// AlertPNG returns the blink icon: a solid red squircle.
func AlertPNG() []byte {
	img := image.NewNRGBA(image.Rect(0, 0, size, size))
	drawLayer(img, squircle(17.5, 9), colHalo) // white outline (visible on dark bars)
	drawLayer(img, squircle(16, 8), colRed)    // solid red body
	return encode(img)
}

// IncidentPNG returns the acknowledged icon: haloed squircle + red notification dot.
func IncidentPNG() []byte {
	img := image.NewNRGBA(image.Rect(0, 0, size, size))
	drawLayer(img, squircle(17.5, 9), colHalo) // white outline (visible on dark bars)
	drawLayer(img, squircle(16, 8), colBody)   // squircle body
	drawLayer(img, disc(bx, by, 9.5), colHalo) // ring separating the dot from the body
	drawLayer(img, disc(bx, by, 8), colRed)    // the red dot
	return encode(img)
}
