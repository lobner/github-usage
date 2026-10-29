//! Draws the menu-bar dot in code (no binary assets).
//!
//! The app shows the percentage ("<used>%"), as the sibling claude-usage app
//! does, with a red dot in front of it while GitHub has an ongoing incident. The
//! status item always shows one of these two images, so its width - and with it
//! the position of every menu-bar item to its left - never changes:
//!
//! - `red_dot_png`: the incident dot, the same size as claude-usage's. A
//!   coloured (not template) image, so macOS keeps it red on either menu bar.
//! - `blank_png`: transparent, the same size. It holds the slot both between
//!   blinks and while there is no incident at all.

use std::sync::OnceLock;

/// The canvas is 44×44 (22 pt @2x). The status item scales the image to 16 pt,
/// so the dot is drawn well inside the canvas to look dot-sized next to text.
const CANVAS: u32 = 44;

const CX: f64 = CANVAS as f64 / 2.0;
const CY: f64 = CANVAS as f64 / 2.0;
/// Exactly claude-usage's radius, so the two apps agree.
const RADIUS: f64 = 10.0;

/// macOS system red.
const RED: [u8; 3] = [0xff, 0x3b, 0x30];

/// Returns the encoded red dot. It is computed once and shared.
pub fn red_dot_png() -> &'static [u8] {
    static PNG: OnceLock<Vec<u8>> = OnceLock::new();
    PNG.get_or_init(|| encode(&draw_disc(CX, CY, RADIUS, RED)))
}

/// Returns a fully transparent image the same size as the dot. The status item
/// shows it in the off phase of the blink, so the percentage - and the whole
/// menu-bar row to its left - does not jump sideways twice a second. When the
/// incident ends, the icon is removed instead, so the item is text only.
///
/// It is computed once and shared.
pub fn blank_png() -> &'static [u8] {
    static PNG: OnceLock<Vec<u8>> = OnceLock::new();
    PNG.get_or_init(|| encode(&vec![0; (CANVAS * CANVAS * 4) as usize]))
}

/// Paints an anti-aliased filled circle into a transparent RGBA canvas, by 4×4
/// supersampling of each pixel.
fn draw_disc(center_x: f64, center_y: f64, rad: f64, col: [u8; 3]) -> Vec<u8> {
    const SS: u32 = 4;
    let mut px = vec![0u8; (CANVAS * CANVAS * 4) as usize];
    for py in 0..CANVAS {
        for pxl in 0..CANVAS {
            let mut hits = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = pxl as f64 + (sx as f64 + 0.5) / SS as f64 - center_x;
                    let y = py as f64 + (sy as f64 + 0.5) / SS as f64 - center_y;
                    if x * x + y * y <= rad * rad {
                        hits += 1;
                    }
                }
            }
            if hits > 0 {
                let i = ((py * CANVAS + pxl) * 4) as usize;
                px[i..i + 3].copy_from_slice(&col);
                px[i + 3] = (hits * 255 / (SS * SS)) as u8;
            }
        }
    }
    px
}

fn encode(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, CANVAS, CANVAS);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().expect("PNG header");
    w.write_image_data(rgba).expect("PNG data");
    w.finish().expect("PNG end");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(b: &[u8]) -> (png::OutputInfo, Vec<u8>) {
        let mut reader = png::Decoder::new(std::io::Cursor::new(b))
            .read_info()
            .expect("decode");
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).expect("frame");
        (info, buf)
    }

    #[test]
    fn red_dot_png_decodes() {
        let (info, px) = decode(red_dot_png());
        assert_eq!(info.width, CANVAS);
        assert_eq!(info.color_type, png::ColorType::Rgba);

        // Opaque red in the middle, fully transparent in the corner.
        let i = (((CANVAS / 2) * CANVAS + CANVAS / 2) * 4) as usize;
        let (r, g, b, a) = (px[i], px[i + 1], px[i + 2], px[i + 3]);
        assert!(a != 0 && r > g && r > b, "centre pixel = ({r},{g},{b},{a})");
        assert_eq!(px[3], 0, "corner alpha");
    }

    #[test]
    fn blank_png_is_transparent_and_same_size() {
        let (info, px) = decode(blank_png());
        assert_eq!((info.width, info.height), (CANVAS, CANVAS));
        assert!(px.iter().all(|&b| b == 0));
    }

    #[test]
    fn red_dot_png_is_cached() {
        assert!(std::ptr::eq(red_dot_png(), red_dot_png()));
    }

    /// Diagnostic: writes the icons to PNG files, so they can be inspected.
    /// Opt-in:
    ///
    ///     ICON_DUMP_DIR=/tmp cargo test dump_icons -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_icons() {
        let dir = std::env::var("ICON_DUMP_DIR").unwrap_or_else(|_| ".".into());
        for (name, data) in [
            ("icon-blank.png", blank_png()),
            ("icon-red-dot.png", red_dot_png()),
        ] {
            let p = std::path::Path::new(&dir).join(name);
            std::fs::write(&p, data).unwrap_or_else(|e| panic!("write {p:?}: {e}"));
            println!("wrote {} ({} bytes)", p.display(), data.len());
        }
    }
}
