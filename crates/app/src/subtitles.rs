//! The subtitle overlay: the cue `tv_media::subtitles` has for the presented position, drawn over
//! the video and under the player's HUD and every panel.
//!
//! Each rect of a cue is one texture, uploaded when the cue changes (a DVB display set comes every
//! second or two) and deleted with it, so a frame between changes only draws. The cue's canvas
//! (720x576 for DVB unless the stream defines another) is mapped onto the picture where the aspect
//! mode puts it, the size setting scales the cue about its bottom centre, and the result is moved
//! inside the screen's title-safe area. Shown only while the video is full screen: the guide's
//! preview is too small to read.

use std::sync::Arc;

use tv_media::subtitles::Cue;
use tv_screens::panels::{keys, Settings};
use tv_ui::consts::{MARGIN_X as SAFE_X, MARGIN_Y as SAFE_Y, SCR_H, SCR_W};
use tv_ui::{Painter, Rect};

#[derive(Default)]
pub struct Overlay {
    /// The cue the textures hold.
    shown: Option<Arc<Cue>>,
    /// One per rect of `shown`, in its order.
    textures: Vec<u32>,
    /// The size setting as a scale of the cue's own size.
    scale: f32,
    /// The show setting as last followed; `None` before the first [`Overlay::configure`].
    show: Option<bool>,
}

impl Overlay {
    /// Follow the settings: the size, and whether (and in which language) a playback starts with
    /// subtitles on. Turning them on or off also acts on what plays now: on picks the preferred
    /// track when none is shown, off hides the one shown.
    pub fn configure(&mut self, settings: &Settings) {
        use tv_media::subtitles as subs;
        self.scale = size_scale(settings.get(keys::SUBTITLES_SIZE));
        let show = settings.flag(keys::SUBTITLES_SHOW);
        let language = tv_platform::i18n::current().language().tag();
        subs::set_auto(show, language);
        if self.show.is_some_and(|was| was != show) {
            if !show {
                subs::select(None);
            } else if subs::selected().is_none() {
                let tracks = subs::tracks();
                if !tracks.is_empty() {
                    subs::select(Some(subs::preferred_track(&tracks, language)));
                }
            }
        }
        self.show = Some(show);
    }

    /// The size the cues are drawn at, as a scale of their own.
    #[cfg(test)]
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Whether a playback starts with subtitles on.
    #[cfg(test)]
    pub fn shows(&self) -> bool {
        self.show == Some(true)
    }

    /// The cue to show differs from the one drawn: a frame is due.
    pub fn changed(&self, full_screen: bool) -> bool {
        serial(&cue_now(full_screen)) != serial(&self.shown)
    }

    /// Draw the cue for the presented position (none unless `full_screen`).
    pub fn paint(&mut self, p: Painter, full_screen: bool) {
        let cue = cue_now(full_screen);
        if serial(&cue) != serial(&self.shown) {
            self.release();
            if let Some(c) = &cue {
                self.textures = c
                    .rects
                    .iter()
                    .map(|r| {
                        let rgba = r.to_rgba();
                        tv_gfx::gfx::upload_rgba(0, r.w as i32, r.h as i32, rgba.as_ptr())
                    })
                    .collect();
            }
            self.shown = cue;
        }
        let Some(cue) = &self.shown else { return };
        let picture = tv_media::player::video_viewport(SCR_W as i32, SCR_H as i32);
        let picture = Rect::new(
            picture.x as f32,
            picture.y as f32,
            picture.width as f32,
            picture.height as f32,
        );
        let rects = cue.rects.iter().map(|r| (r.x, r.y, r.w, r.h));
        for (tex, r) in self
            .textures
            .iter()
            .zip(place(cue.canvas, rects, picture, self.scale))
        {
            p.tex(*tex, r, 0.0, [1.0; 4]);
        }
    }

    fn release(&mut self) {
        for tex in self.textures.drain(..) {
            tv_gfx::gfx::delete_tex(tex);
        }
        self.shown = None;
    }
}

fn cue_now(full_screen: bool) -> Option<Arc<Cue>> {
    full_screen.then(tv_media::subtitles::current).flatten()
}

fn serial(cue: &Option<Arc<Cue>>) -> Option<u64> {
    cue.as_ref().map(|c| c.serial)
}

/// The size setting's scale.
fn size_scale(value: &str) -> f32 {
    match value {
        "small" => 0.8,
        "large" => 1.3,
        _ => 1.0,
    }
}

/// Where each rect `(x, y, w, h)` of a cue on `canvas` goes on the screen: the canvas stretched
/// over `picture`, the whole cue scaled by `scale` about its bottom centre, then moved (never
/// shrunk) to lie inside the title-safe area, bottom and left edges first when it is larger.
fn place(
    canvas: (u32, u32),
    rects: impl IntoIterator<Item = (i32, i32, u32, u32)>,
    picture: Rect,
    scale: f32,
) -> Vec<Rect> {
    let (sx, sy) = (
        picture.w / canvas.0.max(1) as f32,
        picture.h / canvas.1.max(1) as f32,
    );
    let mapped: Vec<Rect> = rects
        .into_iter()
        .map(|(x, y, w, h)| {
            Rect::new(
                picture.x + x as f32 * sx,
                picture.y + y as f32 * sy,
                w as f32 * sx,
                h as f32 * sy,
            )
        })
        .collect();
    let Some(bounds) = mapped.iter().copied().reduce(union) else {
        return mapped;
    };
    let (ax, ay) = (bounds.x + bounds.w * 0.5, bounds.y + bounds.h);
    let scaled = |r: Rect| {
        Rect::new(
            ax + (r.x - ax) * scale,
            ay + (r.y - ay) * scale,
            r.w * scale,
            r.h * scale,
        )
    };
    let b = scaled(bounds);
    let dx = shift(b.x, b.w, SAFE_X, SCR_W - SAFE_X);
    let dy = shift(b.y, b.h, SAFE_Y, SCR_H - SAFE_Y);
    mapped
        .into_iter()
        .map(|r| {
            let r = scaled(r);
            Rect::new(r.x + dx, r.y + dy, r.w, r.h)
        })
        .collect()
}

/// How far to move a span at `pos` of length `len` to lie inside `[lo, hi]`; a span longer than
/// the room keeps its far edge (right or bottom) at `hi`.
fn shift(pos: f32, len: f32, lo: f32, hi: f32) -> f32 {
    if pos + len > hi {
        hi - (pos + len)
    } else if pos < lo {
        lo - pos
    } else {
        0.0
    }
}

fn union(a: Rect, b: Rect) -> Rect {
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect::new(
        x,
        y,
        (a.x + a.w).max(b.x + b.w) - x,
        (a.y + a.h).max(b.y + b.h) - y,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: Rect = Rect::FULL;

    fn xywh(r: Rect) -> (f32, f32, f32, f32) {
        (r.x, r.y, r.w, r.h)
    }

    #[test]
    fn the_canvas_is_stretched_over_the_picture() {
        // A DVB 720x576 canvas over a full-screen 16:9 picture: x by 8/3, y by 15/8.
        let r = place((720, 576), [(180, 480, 360, 48)], FULL, 1.0);
        assert_eq!(xywh(r[0]), (480.0, 900.0, 960.0, 90.0));
        // The same cue over a 4:3 picture pillarboxed in the middle.
        let picture = Rect::new(240.0, 0.0, 1440.0, 1080.0);
        let r = place((720, 576), [(180, 480, 360, 48)], picture, 1.0);
        assert_eq!(xywh(r[0]), (600.0, 900.0, 720.0, 90.0));
    }

    #[test]
    fn the_size_scales_about_the_bottom_centre_and_keeps_the_layout() {
        let rects = [(180, 400, 360, 48), (280, 480, 160, 48)];
        let r = place((720, 576), rects, FULL, 1.3);
        let bottom = r[1].y + r[1].h;
        assert!(
            (bottom - 990.0).abs() < 0.01,
            "the bottom stays put: {bottom}"
        );
        let centre = r[0].x + r[0].w * 0.5;
        assert!((centre - 960.0).abs() < 0.01, "and the centre: {centre}");
        assert!((r[0].w - 960.0 * 1.3).abs() < 0.01);
        assert!(
            r[1].y > r[0].y + r[0].h - 0.01,
            "the second line stays under the first"
        );
    }

    #[test]
    fn a_cue_off_the_safe_area_is_moved_into_it() {
        // At the very bottom of the canvas, and grown past the right edge.
        let r = place((720, 576), [(600, 540, 120, 36)], FULL, 1.3);
        let (right, bottom) = (r[0].x + r[0].w, r[0].y + r[0].h);
        assert!(right <= SCR_W - SAFE_X + 0.01, "{right}");
        assert!(bottom <= SCR_H - SAFE_Y + 0.01, "{bottom}");
        // A zoomed picture larger than the screen pushes the cue below it; it comes back up.
        let zoomed = Rect::new(-240.0, -135.0, 2400.0, 1350.0);
        let r = place((720, 576), [(180, 520, 360, 48)], zoomed, 1.0);
        assert!(r[0].y + r[0].h <= SCR_H - SAFE_Y + 0.01);
        assert_eq!(place((720, 576), [], FULL, 1.0), Vec::<Rect>::new());
    }

    #[test]
    fn sizes_read_from_the_setting() {
        assert_eq!(size_scale("small"), 0.8);
        assert_eq!(size_scale("medium"), 1.0);
        assert_eq!(size_scale("large"), 1.3);
        assert_eq!(size_scale(""), 1.0);
    }
}
