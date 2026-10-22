//! Vector-painted pairing QR code for sharp output at any zoom.

use egui::{Color32, Rect, Ui, pos2, vec2};

/// QR quiet-zone width in modules.
const QUIET: usize = 4;

pub struct Qr {
    width: usize,
    dark: Vec<bool>,
}

impl Qr {
    pub fn encode(text: &str) -> Option<Self> {
        let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
        let width = code.width();
        let dark = code
            .to_colors()
            .into_iter()
            .map(|color| color == qrcode::Color::Dark)
            .collect();
        Some(Self { width, dark })
    }

    /// Paints the largest centered code that fits `rect`, including a white quiet zone.
    pub fn paint(&self, ui: &Ui, rect: Rect, dark: Color32, light: Color32) {
        let total = self.width + 2 * QUIET;
        // Use whole device pixels per module to avoid seams.
        let ppp = ui.ctx().pixels_per_point();
        let module = ((rect.width().min(rect.height()) / total as f32) * ppp).floor() / ppp;
        if module <= 0.0 {
            return;
        }
        let side = module * total as f32;
        let origin = rect.center() - vec2(side, side) / 2.0;
        let painter = ui.painter();
        painter.rect_filled(Rect::from_min_size(origin, vec2(side, side)), 4.0, light);
        let content = origin + vec2(module, module) * QUIET as f32;
        for y in 0..self.width {
            let mut x = 0;
            while x < self.width {
                if !self.dark[y * self.width + x] {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < self.width && self.dark[y * self.width + x] {
                    x += 1;
                }
                let min = content + vec2(start as f32 * module, y as f32 * module);
                let max = pos2(min.x + (x - start) as f32 * module, min.y + module);
                painter.rect_filled(Rect::from_min_max(min, max), 0.0, dark);
            }
        }
    }
}

/// `text` as a QR code drawn with half-block characters, two modules per
/// line, with its quiet zone, for a terminal (`zapfast serve`). Drawn for the
/// usual light text on a dark background: the blocks are the light modules.
pub fn to_terminal(text: &str) -> Option<String> {
    use qrcode::render::unicode::Dense1x2;
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    Some(
        code.render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .quiet_zone(true)
            .build(),
    )
}

/// `text` as a black-on-white PNG, `scale` pixels per module, with its quiet
/// zone.
pub fn to_png(text: &str, scale: u32) -> Option<Vec<u8>> {
    let qr = Qr::encode(text)?;
    let total = (qr.width + 2 * QUIET) as u32;
    let side = total * scale.max(1);
    let image = image::GrayImage::from_fn(side, side, |x, y| {
        let (x, y) = ((x / scale.max(1)) as usize, (y / scale.max(1)) as usize);
        let dark = (QUIET..QUIET + qr.width).contains(&x)
            && (QUIET..QUIET + qr.width).contains(&y)
            && qr.dark[(y - QUIET) * qr.width + (x - QUIET)];
        image::Luma([if dark { 0 } else { 255 }])
    });
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_linking_string() {
        let qr = Qr::encode("2@abcdefghijklmnopqrstuvwxyz,ABCDEFGHIJKLMNOPQRSTUVWXYZ,0123456789,=")
            .expect("encodes");
        assert!(qr.width >= 21);
        assert_eq!(qr.dark.len(), qr.width * qr.width);
        // The finder-pattern corner is always dark.
        assert!(qr.dark[0]);
    }

    #[test]
    fn draws_for_a_terminal_and_as_a_png() {
        let text = "2@fixture,fixture,fixture";
        let terminal = to_terminal(text).expect("encodes");
        let qr = Qr::encode(text).unwrap();
        // Two modules per line, quiet zone included.
        assert_eq!(terminal.lines().count(), (qr.width + 2 * QUIET).div_ceil(2));
        let png = to_png(text, 4).expect("encodes");
        let image = image::load_from_memory(&png).unwrap().to_luma8();
        let side = (qr.width + 2 * QUIET) as u32 * 4;
        assert_eq!(image.dimensions(), (side, side));
        assert_eq!(image.get_pixel(0, 0).0, [255], "quiet zone");
        let corner = QUIET as u32 * 4;
        assert_eq!(image.get_pixel(corner, corner).0, [0], "finder pattern");
    }
}
