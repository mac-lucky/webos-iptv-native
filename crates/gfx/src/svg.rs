//! Runtime SVG rasterizer (resvg). Rasterizes a vector icon *asset* to an RGBA mask at a target
//! pixel size; `tv_ui::icons` uploads that as a GL texture and tints it per state. White where
//! drawn (author icons in #ffffff), alpha = coverage. Replaces the upstream nanosvg C shim with
//! the same contract: uniform fit into `w*h`, centered, straight (non-premultiplied) RGBA.
use resvg::{tiny_skia, usvg};

/// Rasterize `svg` to a `w*h` RGBA buffer (row-major, 4 bytes/px). `None` on bad args or a
/// parse/alloc failure.
pub fn rasterize(svg: &str, w: i32, h: i32) -> Option<Vec<u8>> {
    if svg.is_empty() || w <= 0 || h <= 0 {
        return None;
    }
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).ok()?;
    let size = tree.size();
    let (sw, sh) = (size.width(), size.height());
    if sw <= 0.0 || sh <= 0.0 {
        return None;
    }
    let scale = (w as f32 / sw).min(h as f32 / sh);
    let tx = (w as f32 - sw * scale) * 0.5;
    let ty = (h as f32 - sh * scale) * 0.5;
    let mut pixmap = tiny_skia::Pixmap::new(w as u32, h as u32)?;
    let transform = tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, tx, ty);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(pixmap.pixels().iter().flat_map(|p| {
        let c = p.demultiply();
        [c.red(), c.green(), c.blue(), c.alpha()]
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::rasterize;

    const SQUARE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#ffffff"/></svg>"##;

    #[test]
    fn fills_white_and_keeps_straight_alpha() {
        let px = rasterize(SQUARE, 4, 4).unwrap();
        assert_eq!(px.len(), 4 * 4 * 4);
        assert!(px.chunks(4).all(|c| c == [255, 255, 255, 255]));
    }

    #[test]
    fn fits_uniformly_and_centers() {
        // A 10x10 icon in an 8x4 box scales to 4x4 and sits in columns 2..6.
        let px = rasterize(SQUARE, 8, 4).unwrap();
        let alpha = |x: usize, y: usize| px[(y * 8 + x) * 4 + 3];
        assert_eq!(alpha(0, 2), 0);
        assert_eq!(alpha(3, 2), 255);
        assert_eq!(alpha(7, 2), 0);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(rasterize("", 4, 4).is_none());
        assert!(rasterize(SQUARE, 0, 4).is_none());
        assert!(rasterize("<not svg", 4, 4).is_none());
    }
}
