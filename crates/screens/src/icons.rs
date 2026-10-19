//! The screens' icon set: white SVG masks rasterized with resvg once per size, uploaded as GL
//! textures and drawn tinted. GL thread only.
//!
//! `tv_ui::icons` keeps a closed list of its own, so this module carries the set the screens
//! use. An asset is drawn in a 24-unit viewBox in `#ffffff` only (coverage is all that
//! survives), as one `<path>`: separate elements composite and leave a seam where they meet.

use std::cell::RefCell;
use tv_ui::{Painter, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    /// A clock with a backwards arrow: the channel has catch-up, and the History tile.
    CatchUp,
    /// Favourite marks.
    Star,
    StarFilled,
    /// The TV guide tile.
    Guide,
    Live,
    Record,
    Lock,
    Settings,
    Search,
    Trash,
    Play,
    Pause,
    Rewind,
    FastForward,
    SkipPrevious,
    SkipNext,
    /// Restart the programme from its beginning.
    Restart,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    ChevronDown,
    /// A programme reminder: the mark on a guide cell and the reminder prompt.
    Bell,
}

impl Icon {
    fn svg(self) -> &'static str {
        match self {
            Icon::CatchUp => include_str!("../../../assets/icons/tv/catchup.svg"),
            Icon::Star => include_str!("../../../assets/icons/tv/star.svg"),
            Icon::StarFilled => include_str!("../../../assets/icons/tv/star-filled.svg"),
            Icon::Guide => include_str!("../../../assets/icons/tv/guide.svg"),
            Icon::Live => include_str!("../../../assets/icons/tv/live.svg"),
            Icon::Record => include_str!("../../../assets/icons/tv/record.svg"),
            Icon::Lock => include_str!("../../../assets/icons/tv/lock.svg"),
            Icon::Settings => include_str!("../../../assets/icons/tv/settings.svg"),
            Icon::Trash => include_str!("../../../assets/icons/tv/trash.svg"),
            Icon::SkipPrevious => include_str!("../../../assets/icons/tv/skip-previous.svg"),
            Icon::SkipNext => include_str!("../../../assets/icons/tv/skip-next.svg"),
            Icon::Search => include_str!("../../../assets/icons/search.svg"),
            Icon::Play => include_str!("../../../assets/icons/play.svg"),
            Icon::Pause => include_str!("../../../assets/icons/pause.svg"),
            Icon::Rewind => include_str!("../../../assets/icons/rewind.svg"),
            Icon::FastForward => include_str!("../../../assets/icons/fast-forward.svg"),
            Icon::Restart => include_str!("../../../assets/icons/restart.svg"),
            Icon::ChevronLeft => include_str!("../../../assets/icons/chevron-left.svg"),
            Icon::ChevronRight => include_str!("../../../assets/icons/chevron.svg"),
            Icon::ChevronUp => include_str!("../../../assets/icons/chevron-up.svg"),
            Icon::ChevronDown => include_str!("../../../assets/icons/chevron-down.svg"),
            Icon::Bell => include_str!("../../../assets/icons/tv/bell.svg"),
        }
    }
}

/// Largest mask edge; a bigger rect draws this mask scaled, which keeps the cache bounded.
const MAX_PX: i32 = 128;

thread_local! {
    /// (icon, logical px, texture). Texture 0 records a failed raster so it is not retried.
    static CACHE: RefCell<Vec<(Icon, i32, u32)>> = const { RefCell::new(Vec::new()) };
}

/// Draw `icon` in `tint` (an accent role paints in the picked accent: [`crate::theme::ink`]), in
/// the largest square centred in `r`.
pub fn draw(p: Painter, icon: Icon, r: Rect, tint: [f32; 4]) {
    let side = r.w.min(r.h);
    let px = (side.round() as i32).min(MAX_PX);
    if px <= 0 || p.is_recording() {
        return;
    }
    let tex = texture(icon, px);
    if tex != 0 {
        // Snap the on-screen origin so the 1:1 mask is not resampled across pixels.
        let snap = |v: f32, d: f32| tv_gfx::gfx::snap(v + d) - d;
        let x = snap(r.x + (r.w - side) / 2.0, p.dx());
        let y = snap(r.y + (r.h - side) / 2.0, p.dy());
        p.tex(
            tex,
            Rect::new(x, y, side, side),
            0.0,
            crate::theme::ink(tint),
        );
    }
}

fn texture(icon: Icon, px: i32) -> u32 {
    CACHE.with_borrow_mut(|cache| {
        if let Some(&(.., tex)) = cache.iter().find(|e| e.0 == icon && e.1 == px) {
            return tex;
        }
        // The simulator supersamples; the television renders at scale 1.
        let size = px * tv_base::surface::render_scale();
        let tex =
            mask(icon, size).map_or(0, |m| tv_gfx::gfx::upload_rgba(0, size, size, m.as_ptr()));
        cache.push((icon, px, tex));
        tex
    })
}

/// `icon` as a `size`-square RGBA mask. Colour is forced white so a straight-alpha edge never
/// samples a dark fringe; the tint supplies the colour.
fn mask(icon: Icon, size: i32) -> Option<Vec<u8>> {
    let mut rgba = tv_gfx::svg::rasterize(icon.svg(), size, size)?;
    for px in rgba.as_chunks_mut::<4>().0 {
        px[..3].fill(255);
    }
    Some(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Icon; 21] = [
        Icon::CatchUp,
        Icon::Star,
        Icon::StarFilled,
        Icon::Guide,
        Icon::Live,
        Icon::Record,
        Icon::Lock,
        Icon::Settings,
        Icon::Search,
        Icon::Trash,
        Icon::Play,
        Icon::Pause,
        Icon::Rewind,
        Icon::FastForward,
        Icon::SkipPrevious,
        Icon::SkipNext,
        Icon::Restart,
        Icon::ChevronLeft,
        Icon::ChevronRight,
        Icon::ChevronUp,
        Icon::ChevronDown,
    ];

    #[test]
    fn every_icon_rasterizes_to_a_solid_mask_inside_its_box() {
        const S: usize = 32;
        for icon in ALL {
            let m = mask(icon, S as i32).unwrap_or_else(|| panic!("{icon:?} failed to rasterize"));
            let alpha = |x: usize, y: usize| m[(y * S + x) * 4 + 3];
            assert!(
                (0..S * S).any(|i| alpha(i % S, i / S) == 255),
                "{icon:?} never reaches full ink"
            );
            let edge = (0..S).all(|i| {
                alpha(i, 0) == 0 && alpha(i, S - 1) == 0 && alpha(0, i) == 0 && alpha(S - 1, i) == 0
            });
            assert!(edge, "{icon:?} inks the border");
            assert!(m.as_chunks::<4>().0.iter().all(|px| px[..3] == [255; 3]));
        }
    }

    #[test]
    fn the_bell_rasterizes_inside_its_box() {
        const S: usize = 32;
        let m = mask(Icon::Bell, S as i32).expect("the bell rasterizes");
        let alpha = |x: usize, y: usize| m[(y * S + x) * 4 + 3];
        assert!((0..S * S).any(|i| alpha(i % S, i / S) == 255));
        assert!((0..S).all(|i| alpha(i, 0) == 0 && alpha(i, S - 1) == 0));
        assert!((0..S).all(|i| alpha(0, i) == 0 && alpha(S - 1, i) == 0));
    }

    #[test]
    fn the_two_stars_share_one_silhouette() {
        let bounds = |icon| {
            let m = mask(icon, 48).unwrap();
            let inked: Vec<usize> = (0..48 * 48).filter(|i| m[i * 4 + 3] > 127).collect();
            let xs = inked.iter().map(|i| i % 48);
            let ys = inked.iter().map(|i| i / 48);
            (xs.clone().min(), xs.max(), ys.clone().min(), ys.max())
        };
        assert_eq!(bounds(Icon::Star), bounds(Icon::StarFilled));
    }
}
