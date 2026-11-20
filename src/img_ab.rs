//! Synchronized zoom comparison for two images.
//!
//! The same normalised point of both images is shown side by side at the same
//! magnification, so moving the mouse over either panel inspects the same area
//! of both. With different resolutions the smaller one is visibly softer; that
//! is exactly the difference we want to expose.

pub struct ImgAb {
    pub a_path: String,
    pub b_path: String,
    pub a_dim: (u32, u32),
    pub b_dim: (u32, u32),
    pub a_bytes: u64,
    pub b_bytes: u64,
    /// Normalised centre of the inspected region (0..1).
    pub center: (f32, f32),
    pub zoom: f32,
}

impl ImgAb {
    pub fn new(a_path: &str, b_path: &str, a_dim: (u32, u32), b_dim: (u32, u32), a_bytes: u64, b_bytes: u64) -> Self {
        Self {
            a_path: a_path.to_string(),
            b_path: b_path.to_string(),
            a_dim,
            b_dim,
            a_bytes,
            b_bytes,
            center: (0.5, 0.5),
            zoom: 3.0,
        }
    }
}

/// Source rectangle `(x0, y0, w, h)` covering the same *fraction* of both images,
/// clamped to the image. Using a fraction (not absolute pixels) is what makes
/// the two panels show the same scene region even when resolutions differ.
pub fn crop_frac(img: (u32, u32), center: (f32, f32), frac: (f32, f32)) -> (f32, f32, f32, f32) {
    let (iw, ih) = (img.0.max(1) as f32, img.1.max(1) as f32);
    let vw = (iw * frac.0.clamp(0.0, 1.0)).clamp(1.0, iw);
    let vh = (ih * frac.1.clamp(0.0, 1.0)).clamp(1.0, ih);
    let x0 = if vw >= iw {
        0.0
    } else {
        (center.0 * iw - vw / 2.0).clamp(0.0, iw - vw)
    };
    let y0 = if vh >= ih {
        0.0
    } else {
        (center.1 * ih - vh / 2.0).clamp(0.0, ih - vh)
    };
    (x0, y0, vw, vh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centred_crop_is_centred() {
        let (x, y, w, h) = crop_frac((1000, 800), (0.5, 0.5), (0.1, 0.125));
        assert_eq!((w, h), (100.0, 100.0));
        assert!((x - 450.0).abs() < 0.01 && (y - 350.0).abs() < 0.01);
    }

    #[test]
    fn edges_are_clamped() {
        let (x, y, ..) = crop_frac((1000, 800), (0.0, 0.0), (0.2, 0.125));
        assert_eq!((x, y), (0.0, 0.0));
        let (x, y, ..) = crop_frac((1000, 800), (1.0, 1.0), (0.2, 0.125));
        assert!((x - 800.0).abs() < 0.01 && (y - 700.0).abs() < 0.01);
    }

    #[test]
    fn zoomed_out_bigger_than_image_shows_all() {
        let (x, y, w, h) = crop_frac((100, 50), (0.3, 0.7), (4.0, 4.0));
        assert_eq!((x, y, w, h), (0.0, 0.0, 100.0, 50.0));
    }

    #[test]
    fn different_resolutions_share_the_normalised_region() {
        let (ax, ay, aw, ah) = crop_frac((4000, 3000), (0.25, 0.75), (0.125, 0.125));
        let (bx, by, bw, bh) = crop_frac((1000, 750), (0.25, 0.75), (0.125, 0.125));
        assert!((ax / 4000.0 - bx / 1000.0).abs() < 0.001);
        assert!((ay / 3000.0 - by / 750.0).abs() < 0.001);
        assert!((aw / 4000.0 - bw / 1000.0).abs() < 0.001);
        assert!((ah / 3000.0 - bh / 750.0).abs() < 0.001);
    }
}
