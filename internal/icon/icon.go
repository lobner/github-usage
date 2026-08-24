// Package icon renders the menu-bar dot programmatically (no binary assets).
//
// The app shows the percentage ("<used>%"), matching the sibling claude-usage
// app, with a red dot in front of it while GitHub has an ongoing incident. The
// status item always carries one of these two images, so its width — and with it
// the position of every menu-bar item to its left — never changes:
//
//   - RedDotPNG: the incident dot, the same size as claude-usage's. A coloured
//     (non-template) image, so macOS keeps it red on either menu bar.
//   - BlankPNG: transparent, same size. Holds the slot both between blinks and
//     while there is no incident at all.
package icon

import (
	"bytes"
	"image"
	"image/color"
	"image/draw"
	"image/png"
	"sync"
)

// canvas is 44×44 (22pt @2x); systray force-sizes the image to 16pt, so the dot
// is drawn well inside the canvas to end up visually dot-sized next to text.
const canvas = 44

const (
	cx = canvas / 2.0
	cy = canvas / 2.0

	// radius matches claude-usage's dot exactly, so the two apps agree.
	radius = 10.0
)

var colRed = color.NRGBA{R: 0xff, G: 0x3b, B: 0x30, A: 0xff} // macOS system red

var (
	redOnce   sync.Once
	redPNG    []byte
	blankOnce sync.Once
	blankPNG  []byte
)

// RedDotPNG returns the incident dot. The result is computed once and shared, so
// callers must not modify the returned slice.
func RedDotPNG() []byte {
	redOnce.Do(func() {
		img := image.NewNRGBA(image.Rect(0, 0, canvas, canvas))
		drawDisc(img, cx, cy, radius, colRed)
		redPNG = encode(img)
	})
	return redPNG
}

// BlankPNG returns a fully transparent image the same size as the dot. It is
// what the status item shows whenever the dot is not lit: between blinks, and for
// as long as there is no incident. Keeping the slot occupied is what stops the
// percentage — and the whole menu-bar row left of it — from jumping sideways
// twice a second while blinking, and again when an incident starts or ends.
//
// The result is computed once and shared, so callers must not modify the returned
// slice.
func BlankPNG() []byte {
	blankOnce.Do(func() {
		blankPNG = encode(image.NewNRGBA(image.Rect(0, 0, canvas, canvas)))
	})
	return blankPNG
}

func encode(img *image.NRGBA) []byte {
	var buf bytes.Buffer
	_ = png.Encode(&buf, img)
	return buf.Bytes()
}

// drawDisc paints an anti-aliased filled circle by 4×4 supersampling each pixel.
func drawDisc(dst *image.NRGBA, centerX, centerY, rad float64, col color.NRGBA) {
	const ss = 4
	b := dst.Bounds()
	mask := image.NewAlpha(b)
	for py := b.Min.Y; py < b.Max.Y; py++ {
		for px := b.Min.X; px < b.Max.X; px++ {
			hits := 0
			for sy := 0; sy < ss; sy++ {
				for sx := 0; sx < ss; sx++ {
					x := float64(px) + (float64(sx)+0.5)/ss - centerX
					y := float64(py) + (float64(sy)+0.5)/ss - centerY
					if x*x+y*y <= rad*rad {
						hits++
					}
				}
			}
			if hits > 0 {
				mask.SetAlpha(px, py, color.Alpha{A: uint8(hits * 255 / (ss * ss))})
			}
		}
	}
	draw.DrawMask(dst, b, image.NewUniform(col), image.Point{}, mask, image.Point{}, draw.Over)
}
