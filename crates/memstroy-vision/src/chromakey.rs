use image::{Rgb, RgbImage, Rgba, RgbaImage};
use memstroy_core::ChromaKeyParams;

/// Apply HSV-based chroma keying to a single RGB frame, producing RGBA
/// where pixels close in hue/saturation to the key colour become
/// transparent. Includes spill suppression: pixels near the key get
/// their green channel reduced toward the average of red/blue.
pub struct HsvChromaKey {
    pub params: ChromaKeyParams,
}

impl HsvChromaKey {
    pub fn new(params: ChromaKeyParams) -> Self {
        Self { params }
    }

    pub fn apply(&self, src: &RgbImage) -> RgbaImage {
        let (w, h) = src.dimensions();
        let mut out = RgbaImage::new(w, h);
        let key_hsv = rgb_to_hsv(self.params.key_color);
        // similarity is in [0, 1]; map to a hue tolerance of up to 60deg
        // and a saturation/value tolerance of up to 0.5 each.
        let hue_tol_deg = 60.0 * self.params.similarity.clamp(0.0, 1.0);
        let sv_tol = 0.5 * self.params.similarity.clamp(0.0, 1.0);
        let blend = self.params.blend.clamp(0.0, 1.0);

        for (x, y, p) in src.enumerate_pixels() {
            let Rgb([r, g, b]) = *p;
            let hsv = rgb_to_hsv([r, g, b]);
            let mut alpha = 1.0f32;
            // Hue distance on the circular axis.
            let dh = hue_distance_deg(hsv.0, key_hsv.0);
            let ds = (hsv.1 - key_hsv.1).abs();
            let dv = (hsv.2 - key_hsv.2).abs();
            // Inside core: fully transparent.
            if dh < hue_tol_deg && ds < sv_tol && dv < sv_tol {
                alpha = 0.0;
            } else if dh < hue_tol_deg + 360.0 * blend && ds < sv_tol + blend && dv < sv_tol + blend
            {
                // Soft edge: linearly fade alpha based on distance.
                let edge = ((dh - hue_tol_deg) / (360.0 * blend + f32::EPSILON))
                    .max((ds - sv_tol) / (blend + f32::EPSILON))
                    .max((dv - sv_tol) / (blend + f32::EPSILON));
                alpha = edge.clamp(0.0, 1.0);
            }

            // Spill suppression on the green channel.
            let (r2, g2, b2) = if alpha > 0.0 && self.params.spill > 0.0 {
                let avg_rb = ((r as f32 + b as f32) * 0.5) as u8;
                let suppressed_g = if g > avg_rb {
                    let delta = (g as f32 - avg_rb as f32) * self.params.spill.clamp(0.0, 1.0);
                    (g as f32 - delta).clamp(0.0, 255.0) as u8
                } else {
                    g
                };
                (r, suppressed_g, b)
            } else {
                (r, g, b)
            };

            let a = (alpha * 255.0).round() as u8;
            out.put_pixel(x, y, Rgba([r2, g2, b2, a]));
        }
        out
    }
}

/// Convert an `[r, g, b]` triple in 0..=255 to HSV with H in [0, 360),
/// S in [0, 1], V in [0, 1].
pub fn rgb_to_hsv(rgb: [u8; 3]) -> (f32, f32, f32) {
    let r = rgb[0] as f32 / 255.0;
    let g = rgb[1] as f32 / 255.0;
    let b = rgb[2] as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let h = if delta == 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / delta) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let h = if h < 0.0 { h + 360.0 } else { h };
    let s = if max == 0.0 { 0.0 } else { delta / max };
    let v = max;
    (h, s, v)
}

/// Smallest positive distance between two hue values on the [0, 360)
/// circular axis.
pub fn hue_distance_deg(a: f32, b: f32) -> f32 {
    let d = (a - b).abs() % 360.0;
    d.min(360.0 - d)
}
