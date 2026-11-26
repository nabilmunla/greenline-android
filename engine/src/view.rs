//! Small view helpers for the worker: image sizes and moving rendered RGBA into events.
//!
//! The renders in `lib.rs` already produce tightly packed RGBA (alpha 255), which is what the WebView's
//! `ImageData` takes, so no pixel conversion happens here: the bytes are moved, not copied.

use crate::api::LOWRES_WIDTH;
use crate::Rendered;

/// The low-res fit width for a requested fit width: [`LOWRES_WIDTH`], or the fit width if smaller.
pub fn lowres_width(fit_width: u32) -> u32 {
    LOWRES_WIDTH.min(fit_width)
}

/// An RGBA image taken out of a render: (bytes, width, height, render + convert ms).
pub struct Image {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub ms: f64,
}

impl From<Rendered> for Image {
    fn from(r: Rendered) -> Self {
        let ms = r.timings.total_ms();
        Image { rgba: r.image.data, w: r.image.width, h: r.image.height, ms }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowres_never_wider_than_fit() {
        assert_eq!(lowres_width(1600), 600);
        assert_eq!(lowres_width(400), 400);
    }
}
