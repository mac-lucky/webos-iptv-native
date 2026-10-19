//! Picture geometry for overlays.
//! The ACB sourceInfo supplies the decoded raster and pixel aspect ratio; the dev
//! TV's 1440x1080 capture proves that its full-panel window pillarboxes that picture.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Viewport {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Aspect(u32, u32);

impl Viewport {
    /// The largest centred part of this rect with aspect `a` (square units).
    pub fn cover_of(self, a: Aspect) -> Viewport {
        self.cover_ratio((i64::from(a.0), i64::from(a.1)))
    }

    fn cover_ratio(self, (n, d): (i64, i64)) -> Viewport {
        let (w, h) = (i64::from(self.width), i64::from(self.height));
        if n <= 0 || d <= 0 || w <= 0 || h <= 0 {
            return self;
        }
        if w * d > h * n {
            let nw = (h * n / d) as i32;
            Viewport { x: self.x + (self.width - nw) / 2, width: nw, ..self }
        } else {
            let nh = (w * d / n) as i32;
            Viewport { y: self.y + (self.height - nh) / 2, height: nh, ..self }
        }
    }
}

impl Aspect {
    pub fn from_raster(width: i32, height: i32) -> Option<Self> {
        (width > 0 && height > 0).then_some(Self(width as u32, height as u32))
    }

    pub fn pack(self) -> u64 {
        (u64::from(self.0) << 32) | u64::from(self.1)
    }

    pub fn unpack(value: u64) -> Option<Self> {
        let aspect = Self((value >> 32) as u32, value as u32);
        (aspect.0 != 0 && aspect.1 != 0).then_some(aspect)
    }

    pub fn fit(self, width: i32, height: i32) -> Viewport {
        let (w, h) = (i64::from(width.max(1)), i64::from(height.max(1)));
        let (num, den) = (i64::from(self.0), i64::from(self.1));
        let (w, h) = if w * den > h * num {
            (((h * num + den / 2) / den).clamp(1, w), h)
        } else {
            (w, ((w * den + num / 2) / num).clamp(1, h))
        };
        Viewport {
            x: (width - w as i32) / 2,
            y: (height - h as i32) / 2,
            width: w as i32,
            height: h as i32,
        }
    }

    /// The fitted picture scaled about the screen's centre until its centred `inner`-aspect
    /// region covers the screen: Fill when `inner` is the screen's own aspect, a zoom otherwise.
    pub fn cover(self, inner: Aspect, width: i32, height: i32) -> Viewport {
        let fitted = self.fit(width, height);
        let region = inner.fit(fitted.width, fitted.height);
        let (rw, rh) = (i64::from(region.width.max(1)), i64::from(region.height.max(1)));
        let (w, h) = (i64::from(width.max(1)), i64::from(height.max(1)));
        // the larger of w/rw and h/rh, as a fraction
        let (num, den) = if w * rh >= h * rw { (w, rw) } else { (h, rh) };
        let scale = |v: i32| ((i64::from(v) * num + den / 2) / den) as i32;
        let (sw, sh) = (scale(fitted.width), scale(fitted.height));
        Viewport { x: (width - sw) / 2, y: (height - sh) / 2, width: sw, height: sh }
    }
}

impl Viewport {
    /// The part of this rect that is on a `width`×`height` screen (a Fill window overhangs it).
    pub fn clip(self, width: i32, height: i32) -> Viewport {
        let (x0, y0) = (self.x.max(0), self.y.max(0));
        let (x1, y1) = ((self.x + self.width).min(width), (self.y + self.height).min(height));
        Viewport { x: x0, y: y0, width: (x1 - x0).max(0), height: (y1 - y0).max(0) }
    }
}

/// **How the picture is placed on the screen** - the player's `...` > Aspect ratio. Per playback:
/// every start is [`Auto`](AspectMode::Auto), and nothing is saved.
///
/// It is expressed as the exported window's destination rect alone (`vp_place`'s source is
/// always the whole frame), so a crop is a window larger than the screen that the panel clips.
/// **Device caveat, unmeasured:** the dev set (webOS 4.5, ACB) is seen to letterbox the picture
/// inside a full-screen window by itself. If the webOS 5+ compositor does the same inside the
/// exported window, Stretch reads as Auto there; the crop modes do not depend on it either way.
/// The ACB path (webOS 4) is placed once at bind and does not follow the mode.
///
/// The forced ratios ([`Ratio4x3`](AspectMode::Ratio4x3), [`Ratio16x9`](AspectMode::Ratio16x9))
/// show the whole frame in the largest centred part of the video rectangle with that aspect
/// ([`AspectMode::dest`]): the window shrinks inside the rectangle, never past it, so it stays
/// inside the UI's transparent hole. The caveat above applies to them as to Stretch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AspectMode {
    /// The source's own aspect, letterboxed or pillarboxed: the picture fitted inside the screen.
    #[default]
    Auto,
    /// Cover the screen, cropping what overhangs (the picture keeps its aspect).
    Fill,
    /// The whole screen, the picture's aspect ignored.
    Stretch,
    /// A 4:3 picture inside the frame (pillarboxed into a 16:9 encode) zoomed until it covers.
    Zoom4x3,
    /// A 21:9 picture inside the frame (letterboxed into a 16:9 encode) zoomed until it covers.
    Zoom21x9,
    /// The whole frame shown at 4:3, pillarboxed inside the rectangle.
    Ratio4x3,
    /// The whole frame shown at 16:9 (on a 16:9 rectangle, all of it).
    Ratio16x9,
}

impl AspectMode {
    /// Every mode, in the menu's order; a mode's index here is its wire value.
    pub const ALL: [AspectMode; 7] = [Self::Auto, Self::Fill, Self::Stretch, Self::Zoom4x3, Self::Zoom21x9, Self::Ratio4x3, Self::Ratio16x9];

    pub fn index(self) -> u8 {
        Self::ALL.iter().position(|m| *m == self).unwrap_or(0) as u8
    }

    /// The mode at `i`; Auto for an index past the list.
    pub fn from_index(i: u8) -> Self {
        Self::ALL.get(usize::from(i)).copied().unwrap_or_default()
    }

    /// The part of a `width`×`height` frame this mode shows on the full screen, in the frame's
    /// pixels: everything for Auto and Stretch; for Fill the centred part with the screen's
    /// aspect; for the zooms the 4:3 / 21:9 picture inside the frame (its bars dropped), then the
    /// centred part of that with the screen's aspect. `picture` is the frame's displayed aspect
    /// (non-square pixels), so a crop keeps the picture's proportions.
    pub fn crop(self, picture: Aspect, width: i32, height: i32, screen: Aspect) -> Viewport {
        let full = Viewport { x: 0, y: 0, width, height };
        // an aspect in displayed units, as a ratio of the frame's pixels
        let raster = |a: Aspect| (i64::from(a.0) * i64::from(width) * i64::from(picture.1), i64::from(a.1) * i64::from(height) * i64::from(picture.0));
        match self {
            Self::Auto | Self::Stretch | Self::Ratio4x3 | Self::Ratio16x9 => full,
            Self::Fill => full.cover_ratio(raster(screen)),
            Self::Zoom4x3 => full.cover_ratio(raster(Aspect(4, 3))).cover_ratio(raster(screen)),
            Self::Zoom21x9 => full.cover_ratio(raster(Aspect(21, 9))).cover_ratio(raster(screen)),
        }
    }

    /// The exported window's destination rect. Auto is the full screen, exactly as every
    /// playback was placed before the modes existed: the pipeline fits the picture inside it
    /// itself, with the pixel aspect it reads from the stream (which a raster alone does not
    /// carry). Stretch asks for the same rect (see the caveat above); the crops overhang.
    pub fn window(self, source: Aspect, width: i32, height: i32) -> Viewport {
        match self {
            Self::Auto | Self::Stretch => Viewport { x: 0, y: 0, width, height },
            _ => self.place(source, width, height),
        }
    }

    /// Where a `source`-aspect picture lands on a `width`×`height` screen in this mode. Fill and
    /// the zooms overhang the screen (negative origin, larger than it).
    pub fn place(self, source: Aspect, width: i32, height: i32) -> Viewport {
        match self {
            Self::Auto => source.fit(width, height),
            Self::Stretch => Viewport { x: 0, y: 0, width, height },
            Self::Fill => source.cover(Aspect(width.max(1) as u32, height.max(1) as u32), width, height),
            Self::Zoom4x3 => source.cover(Aspect(4, 3), width, height),
            Self::Zoom21x9 => source.cover(Aspect(21, 9), width, height),
            Self::Ratio4x3 => Aspect(4, 3).fit(width, height),
            Self::Ratio16x9 => Aspect(16, 9).fit(width, height),
        }
    }

    /// Where the exported window goes for a picture placed in `rect` (the video rectangle): the
    /// largest centred part of it with the forced aspect, and all of it in every other mode.
    /// Never larger than `rect`.
    pub fn dest(self, rect: Viewport) -> Viewport {
        match self {
            Self::Ratio4x3 => rect.cover_of(Aspect(4, 3)),
            Self::Ratio16x9 => rect.cover_of(Aspect(16, 9)),
            _ => rect,
        }
    }
}

/// Parse only the sourceInfo video object, never a similarly named nested field.
/// Observed on the TV as `video:{width,height,pixelAspectRatio:{width,height}}`.
/// This runs only on metadata callbacks, not on the frame/position callback path.
pub(super) fn source_aspect(bytes: &[u8]) -> Option<Aspect> {
    let root: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let video = root.get("video")?;
    let positive = |v: &serde_json::Value| v.as_u64().filter(|&n| n > 0 && n <= 65_536);
    let w = positive(video.get("width")?)?;
    let h = positive(video.get("height")?)?;
    let (pw, ph) = match video.get("pixelAspectRatio") {
        Some(par) => (positive(par.get("width")?)?, positive(par.get("height")?)?),
        None => (1, 1),
    };
    let (num, den) = (w * pw, h * ph);
    let (mut a, mut b) = (num, den);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    Some(Aspect(
        u32::try_from(num / a).ok()?,
        u32::try_from(den / a).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fill and the zooms crop the SOURCE and show it on the full screen: a window larger than
    /// the screen at a negative origin made the LG compositor twitch the picture on the C1.
    #[test]
    fn the_crops_pick_a_centred_part_of_the_frame_and_fill_the_screen() {
        let crop = |m: AspectMode, w: i32, h: i32| m.crop(Aspect(w as u32, h as u32), w, h, Aspect(16, 9));
        let full = Viewport { x: 0, y: 0, width: 1920, height: 1080 };
        assert_eq!(crop(AspectMode::Auto, 1920, 1080), full);
        assert_eq!(crop(AspectMode::Stretch, 1920, 1080), full);
        assert_eq!(crop(AspectMode::Fill, 1920, 1080), full, "16:9 on 16:9: nothing to crop");
        // a 2.4:1 encode filling a 16:9 screen loses its sides
        assert_eq!(crop(AspectMode::Fill, 1920, 800), Viewport { x: 249, y: 0, width: 1422, height: 800 });
        // a 4:3 encode loses its top and bottom
        assert_eq!(crop(AspectMode::Fill, 1440, 1080), Viewport { x: 0, y: 135, width: 1440, height: 810 });
        // a 21:9 film letterboxed into a 16:9 encode: the bars go, then its sides to fill
        assert_eq!(crop(AspectMode::Zoom21x9, 1920, 1080), Viewport { x: 0, y: 129, width: 1920, height: 822 }.cover_of(Aspect(16, 9)));
        // a 4:3 show pillarboxed into a 16:9 encode: the pillars go, then top and bottom
        assert_eq!(crop(AspectMode::Zoom4x3, 1920, 1080), Viewport { x: 240, y: 0, width: 1440, height: 1080 }.cover_of(Aspect(16, 9)));
        for m in [AspectMode::Fill, AspectMode::Zoom4x3, AspectMode::Zoom21x9] {
            let c = crop(m, 1920, 1080);
            assert!(c.x >= 0 && c.y >= 0 && c.x + c.width <= 1920 && c.y + c.height <= 1080, "{m:?} stays inside the frame: {c:?}");
        }
    }

    #[test]
    fn picture_fit_covers_pillarbox_letterbox_and_anamorphic_sources() {
        let fit = |json: &[u8]| source_aspect(json).unwrap().fit(1920, 1080);
        assert_eq!(fit(br#"{"video":{"width":1440,"height":1080,"pixelAspectRatio":{"width":1,"height":1}}}"#),
            Viewport { x: 240, y: 0, width: 1440, height: 1080 });
        assert_eq!(
            fit(br#"{"video":{"width":1920,"height":800}}"#),
            Viewport {
                x: 0,
                y: 140,
                width: 1920,
                height: 800
            }
        );
        assert_eq!(fit(br#"{"video":{"width":720,"height":576,"pixelAspectRatio":{"width":64,"height":45}}}"#),
            Viewport { x: 0, y: 0, width: 1920, height: 1080 });
        assert_eq!(
            Aspect::from_raster(3840, 2160).unwrap().fit(1920, 1080),
            Viewport {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            }
        );
    }

    #[test]
    fn every_aspect_mode_places_letterbox_and_pillarbox_sources() {
        let vp = |x, y, width, height| Viewport { x, y, width, height };
        let scope = Aspect::from_raster(1920, 800).unwrap(); // 2.4:1, letterboxed on 16:9
        let academy = Aspect::from_raster(1440, 1080).unwrap(); // 4:3, pillarboxed
        let hd = Aspect::from_raster(1920, 1080).unwrap();
        let at = |m: AspectMode, a: Aspect| m.place(a, 1920, 1080);
        // Auto: the source's own aspect inside the screen
        assert_eq!(at(AspectMode::Auto, scope), vp(0, 140, 1920, 800));
        assert_eq!(at(AspectMode::Auto, academy), vp(240, 0, 1440, 1080));
        // Fill: covers the screen, the overhang cropped by the panel
        assert_eq!(at(AspectMode::Fill, scope), vp(-336, 0, 2592, 1080));
        assert_eq!(at(AspectMode::Fill, academy), vp(0, -180, 1920, 1440));
        assert_eq!(at(AspectMode::Fill, hd), vp(0, 0, 1920, 1080), "a 16:9 picture already fills");
        // Stretch: the whole screen whatever the source
        for a in [scope, academy] {
            assert_eq!(at(AspectMode::Stretch, a), vp(0, 0, 1920, 1080));
        }
        // 4:3 zoom: a 4:3 picture pillarboxed into a 16:9 encode loses its bars
        assert_eq!(at(AspectMode::Zoom4x3, hd), vp(-320, -180, 2560, 1440));
        assert_eq!(at(AspectMode::Zoom4x3, academy), vp(0, -180, 1920, 1440), "a native 4:3 source fills");
        // 21:9 zoom: a 21:9 picture letterboxed into a 16:9 encode loses its bars
        assert_eq!(at(AspectMode::Zoom21x9, hd), vp(-300, -168, 2520, 1417));
        assert_eq!(at(AspectMode::Zoom21x9, scope), vp(-336, 0, 2592, 1080), "a wider-than-21:9 source covers by height");
        // the window: Auto keeps the full-screen window every playback had; a crop overhangs
        assert_eq!(AspectMode::Auto.window(scope, 1920, 1080), vp(0, 0, 1920, 1080));
        assert_eq!(AspectMode::Stretch.window(academy, 1920, 1080), vp(0, 0, 1920, 1080));
        assert_eq!(AspectMode::Fill.window(scope, 1920, 1080), at(AspectMode::Fill, scope));
        // what is on the screen of any placement
        assert_eq!(at(AspectMode::Fill, scope).clip(1920, 1080), vp(0, 0, 1920, 1080));
        assert_eq!(at(AspectMode::Auto, scope).clip(1920, 1080), vp(0, 140, 1920, 800));
        // the wire value round-trips, and a stale one reads as Auto
        for m in AspectMode::ALL {
            assert_eq!(AspectMode::from_index(m.index()), m);
        }
        assert_eq!(AspectMode::from_index(99), AspectMode::Auto);
    }

    /// The forced ratios keep the whole frame and shrink the window inside the video rectangle,
    /// full screen and preview alike: never past it, so the UI's hole and the picture match.
    #[test]
    fn the_forced_ratios_show_the_whole_frame_inside_the_video_rect() {
        let vp = |x, y, width, height| Viewport { x, y, width, height };
        let full = vp(0, 0, 1920, 1080);
        let preview = vp(1180, 96, 640, 360);
        assert_eq!(AspectMode::Ratio4x3.dest(full), vp(240, 0, 1440, 1080));
        assert_eq!(AspectMode::Ratio16x9.dest(full), full);
        assert_eq!(AspectMode::Ratio4x3.dest(preview), vp(1260, 96, 480, 360));
        assert_eq!(AspectMode::Ratio16x9.dest(vp(0, 0, 1440, 1080)), vp(0, 135, 1440, 810));
        for m in AspectMode::ALL {
            for rect in [full, preview] {
                let d = m.dest(rect);
                assert!(d.x >= rect.x && d.y >= rect.y && d.x + d.width <= rect.x + rect.width && d.y + d.height <= rect.y + rect.height, "{m:?} leaves {rect:?}: {d:?}");
            }
            if !matches!(m, AspectMode::Ratio4x3 | AspectMode::Ratio16x9) {
                assert_eq!(m.dest(preview), preview, "{m:?} fills the rectangle");
            }
        }
        let hd = Aspect::from_raster(1920, 1080).unwrap();
        for m in [AspectMode::Ratio4x3, AspectMode::Ratio16x9] {
            assert_eq!(m.crop(hd, 1920, 1080, hd), full, "{m:?} shows the whole frame");
            assert_eq!(m.window(hd, 1920, 1080), m.dest(full));
            assert_eq!(AspectMode::from_index(m.index()), m);
        }
        // where an overlay finds the picture (`player::video_viewport`)
        assert_eq!(AspectMode::Ratio4x3.place(hd, 1920, 1080), vp(240, 0, 1440, 1080));
    }

    #[test]
    fn missing_or_invalid_geometry_does_not_publish_an_aspect() {
        for value in [
            br#"{}"#.as_slice(),
            br#"{"video":{"width":0,"height":1080}}"#,
            br#"{"video":{"width":1920,"height":1080,"pixelAspectRatio":{"width":0,"height":1}}}"#,
            br#"{"video":{"width":-1,"height":1080}}"#,
            br#"{"video":{"width":4294967295,"height":1080}}"#,
        ] {
            assert_eq!(source_aspect(value), None);
        }
        assert_eq!(Aspect::unpack(0), None);
        let aspect = source_aspect(br#"{"video":{"width":1440,"height":1080}}"#).unwrap();
        assert_eq!(Aspect::unpack(aspect.pack()), Some(aspect));
    }
}
