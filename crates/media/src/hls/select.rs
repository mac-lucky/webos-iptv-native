//! **Which variant of a master playlist this television plays**: the highest `BANDWIDTH` among
//! the variants whose codecs, raster and frame rate the set can decode, read from the device's own
//! codec table (`tv_platform::devcaps`), never assumed.
//!
//! A variant is judged by what its `CODECS` attribute (RFC 6381 names) and `RESOLUTION` /
//! `FRAME-RATE` declare. A variant that declares nothing is admitted: a master that says nothing
//! is the common live-TV shape, its streams are H.264 in practice, and the demuxer refuses a stream
//! it cannot feed with an explicit error anyway (`ff::demux`). What it declares and the set cannot
//! decode (HEVC on a set without the decoder, a raster past the table's bound, a codec neither
//! the pipeline nor the software audio path takes: Opus, FLAC, ...) rules the variant out.
//! MPEG audio (MP2, MP3, a DVB restream's usual sound) is judged by the same feed decision the
//! Load makes (`stream::AudioFeed::decide`): native where the table lists MP3, else decoded to PCM.
//!
//! **Alternate audio.** The demuxer reads ONE byte stream, so audio carried by a separate
//! `EXT-X-MEDIA` playlist cannot be played beside the video. A variant whose `AUDIO` group
//! names renditions with their own URI is therefore ranked below every variant that carries its
//! audio in its own segments; it is chosen only when nothing else plays, and then plays without
//! the separate audio (the demuxer's video-only path), which the event log says.

use super::playlist::{MasterPlaylist, MediaType, Variant};
use crate::stream::{AudioFeed, SoftwareAudio};
use tv_platform::devcaps::Caps;

/// Why a variant is not playable here. Carries codec names and numbers only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unplayable {
    /// A codec the set has no decoder for, or the pipeline no name for.
    Codec(String),
    /// The declared raster exceeds the set's bound for its codec.
    Raster { width: u32, height: u32 },
    /// The declared frame rate exceeds the table's row.
    FrameRate,
}

/// What a codec entry is, as the pipeline knows codecs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    H264,
    Hevc,
    Av1,
    Vp9,
    Aac,
    Ac3,
    Eac3,
    /// MPEG-1/2 audio, as the feed decision names its layers: one of them must play.
    Mpeg(&'static [&'static str]),
    /// A subtitle/caption codec: carried out of band, never in a variant's media segments.
    Text,
    /// A codec this player cannot decode at all (Opus, FLAC, ALAC, MPEG-1 Layer I, DTS of unknown
    /// channel count, Dolby Vision without a usable base layer, ...).
    Unsupported,
}

/// Classify one RFC 6381 codec entry.
fn kind_of(codec: &str) -> Kind {
    let c = codec.trim().to_ascii_lowercase();
    let fourcc = c.split('.').next().unwrap_or("");
    match fourcc {
        "avc1" | "avc3" => Kind::H264,
        "hvc1" | "hev1" => Kind::Hevc,
        // Dolby Vision over HEVC: profile 8 has an HDR10/SDR/HLG base layer the set shows as
        // ordinary HEVC; 5 (IPT-PQ) and 7 (dual layer) do not (`stream::Dovi`).
        "dvh1" | "dvhe" => match c.split('.').nth(1) {
            Some("08") => Kind::Hevc,
            _ => Kind::Unsupported,
        },
        "av01" => Kind::Av1,
        "vp09" | "vp9" => Kind::Vp9,
        // mp4a.40.<aot>: 2 LC, 5 HE, 29 HEv2 are AAC; 33 is MPEG Layer II, 34 Layer III (MP3).
        // mp4a.a5 / mp4a.a6 are AC-3 / E-AC-3; mp4a.6b (MPEG-1) and mp4a.69 (MPEG-2) audio name
        // no layer, so either decoder may be the one it needs.
        "mp4a" => match c.split('.').nth(1) {
            Some("40") => match c.split('.').nth(2) {
                None | Some("2") | Some("02") | Some("5") | Some("05") | Some("29") => Kind::Aac,
                Some("33") => Kind::Mpeg(&["mp2"]),
                Some("34") => Kind::Mpeg(&["mp3"]),
                _ => Kind::Unsupported,
            },
            Some("a5") => Kind::Ac3,
            Some("a6") => Kind::Eac3,
            Some("6b") | Some("69") => Kind::Mpeg(&["mp2", "mp3"]),
            _ => Kind::Unsupported,
        },
        "mp3" => Kind::Mpeg(&["mp3"]),
        "ac-3" => Kind::Ac3,
        // `ec+3` is E-AC-3 with Atmos (JOC), which the pipeline decodes as E-AC-3.
        "ec-3" | "ec+3" => Kind::Eac3,
        "wvtt" | "stpp" | "tx3g" | "c608" | "c708" => Kind::Text,
        _ => Kind::Unsupported,
    }
}

/// `(max_w, max_h, max_fps)` the set decodes `kind` at, 0 for an axis nobody stated.
fn bound(kind: Kind, caps: &Caps) -> (u32, u32, u32) {
    // `hevc_max` is the per-axis minimum across the H.264 and HEVC rows, which `devcaps` applies to
    // every codec at once; the codec's own row tightens it where the table stated one.
    let (gw, gh) = caps.hevc_max;
    let row = match kind {
        Kind::H264 => caps.h264_row,
        Kind::Hevc => caps.hevc_row,
        Kind::Av1 => caps.av1_row,
        Kind::Vp9 => caps.vp9_row,
        _ => (0, 0, 0),
    };
    let min = |a: u32, b: u32| match (a, b) {
        (0, x) | (x, 0) => x,
        (a, b) => a.min(b),
    };
    (min(gw, row.0), min(gh, row.1), row.2)
}

/// Can this set play `v`? `Ok` for a variant that declares nothing.
pub fn playable(v: &Variant, caps: &Caps) -> Result<(), Unplayable> {
    playable_with(v, caps, SoftwareAudio::detected())
}

/// [`playable`] with the software audio path given.
fn playable_with(v: &Variant, caps: &Caps, sw: SoftwareAudio) -> Result<(), Unplayable> {
    let mut video = None;
    for codec in &v.codecs {
        let kind = kind_of(codec);
        let ok = match kind {
            Kind::H264 => true,
            Kind::Hevc => caps.hevc,
            Kind::Av1 => caps.av1,
            Kind::Vp9 => caps.vp9,
            // The same decision the Load takes: the set's own decoder, else the software one.
            Kind::Aac => AudioFeed::decide("aac", 2, caps, sw).rank() < 2,
            Kind::Ac3 => AudioFeed::decide("ac3", 2, caps, sw).rank() < 2,
            Kind::Eac3 => AudioFeed::decide("eac3", 2, caps, sw).rank() < 2,
            // Stereo is all the attribute can say; a 5.1 MP2 is judged again by the probe.
            Kind::Mpeg(names) => names.iter().any(|n| AudioFeed::decide(n, 2, caps, sw).rank() < 2),
            Kind::Text => true,
            Kind::Unsupported => false,
        };
        if !ok {
            return Err(Unplayable::Codec(codec.to_ascii_lowercase()));
        }
        if matches!(kind, Kind::H264 | Kind::Hevc | Kind::Av1 | Kind::Vp9) {
            video = Some(kind);
        }
    }
    // Undeclared video is judged as H.264, the codec every webOS SoC decodes.
    let (mw, mh, mfps) = bound(video.unwrap_or(Kind::H264), caps);
    if let Some((w, h)) = v.resolution {
        if (mw > 0 && w > mw) || (mh > 0 && h > mh) {
            return Err(Unplayable::Raster { width: w, height: h });
        }
    }
    if let (Some(fps), true) = (v.frame_rate, mfps > 0) {
        // FRAME-RATE is rounded to three decimals (29.970); the table's is a ceiling.
        if fps > f64::from(mfps) + 0.5 {
            return Err(Unplayable::FrameRate);
        }
    }
    Ok(())
}

/// Does `v` need an audio rendition from a separate playlist?
pub fn needs_separate_audio(v: &Variant, master: &MasterPlaylist) -> bool {
    let Some(group) = &v.audio_group else { return false };
    let mut in_group = master.renditions.iter().filter(|r| r.kind == MediaType::Audio && &r.group_id == group).peekable();
    // A group whose renditions all lack a URI is muxed into the variant; one with a URI is not.
    in_group.peek().is_some() && in_group.any(|r| r.uri.is_some())
}

/// No variant of the master is playable on this set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoPlayableVariant {
    /// How many variants the master listed.
    pub variants: usize,
    /// Why the first of them was ruled out, for the log.
    pub first_reason: Option<Unplayable>,
}

/// The chosen variant's index into `master.variants`.
pub fn select(master: &MasterPlaylist, caps: &Caps) -> Result<usize, NoPlayableVariant> {
    select_with(master, caps, SoftwareAudio::detected())
}

fn select_with(master: &MasterPlaylist, caps: &Caps, sw: SoftwareAudio) -> Result<usize, NoPlayableVariant> {
    let mut first_reason = None;
    let mut best: Option<(usize, (bool, u64, u64))> = None;
    for (i, v) in master.variants.iter().enumerate() {
        if let Err(why) = playable_with(v, caps, sw) {
            first_reason.get_or_insert(why);
            continue;
        }
        let area = v.resolution.map_or(0, |(w, h)| u64::from(w) * u64::from(h));
        // Muxed audio first, then bandwidth, then raster; the earliest listed wins a full tie.
        let rank = (!needs_separate_audio(v, master), v.bandwidth, area);
        if best.as_ref().is_none_or(|(_, r)| rank > *r) {
            best = Some((i, rank));
        }
    }
    best.map(|(i, _)| i).ok_or(NoPlayableVariant { variants: master.variants.len(), first_reason })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::playlist::Rendition;

    fn v(bw: u64, codecs: &str, res: Option<(u32, u32)>) -> Variant {
        Variant {
            uri: format!("https://cdn.example/{bw}.m3u8"),
            bandwidth: bw,
            codecs: codecs.split(',').filter(|s| !s.is_empty()).map(str::to_owned).collect(),
            resolution: res,
            ..Variant::default()
        }
    }

    fn master(variants: Vec<Variant>) -> MasterPlaylist {
        MasterPlaylist { variants, renditions: Vec::new() }
    }

    #[test]
    fn the_highest_bandwidth_playable_variant_wins() {
        let caps = Caps::assumed();
        let m = master(vec![
            v(800_000, "avc1.4d401f,mp4a.40.2", Some((640, 360))),
            v(6_000_000, "avc1.640028,mp4a.40.2", Some((1920, 1080))),
            v(3_000_000, "avc1.64001f,mp4a.40.2", Some((1280, 720))),
        ]);
        assert_eq!(select(&m, &caps), Ok(1));
    }

    #[test]
    fn hevc_needs_the_decoder_and_unknown_codecs_are_skipped() {
        let mut caps = Caps::assumed();
        let m = master(vec![
            v(2_000_000, "avc1.64001f,mp4a.40.2", Some((1280, 720))),
            v(8_000_000, "hvc1.2.4.L150.B0,ec-3", Some((3840, 2160))),
            v(9_000_000, "avc1.640028,opus", Some((1920, 1080))),
            v(9_500_000, "avc1.640028,mp4a.40.34", Some((1920, 1080))),
        ]);
        assert_eq!(select_with(&m, &caps, SoftwareAudio::NONE), Ok(1), "the assumed table decodes HEVC 4K and E-AC-3");
        caps.hevc = false;
        assert_eq!(select_with(&m, &caps, SoftwareAudio::NONE), Ok(0), "no HEVC decoder; Opus never, MP3 not without a path");
        assert_eq!(playable_with(&m.variants[2], &caps, SoftwareAudio::NONE), Err(Unplayable::Codec("opus".into())));
        assert_eq!(playable_with(&m.variants[3], &caps, SoftwareAudio::NONE), Err(Unplayable::Codec("mp4a.40.34".into())));
    }

    /// MPEG audio plays where the feed decision says it does: MP3 natively on a set whose table
    /// lists it, MP2 and MP3 through the software PCM path, never without either.
    #[test]
    fn mpeg_audio_variants_play_through_the_same_decision_as_the_load() {
        let pcm = SoftwareAudio { pcm: true };
        let variant = |codecs: &str| v(4_000_000, codecs, Some((1920, 1080)));
        let mut mp3_row = Caps::assumed();
        mp3_row.audio = "aac,ac3,eac3,mp3".into();
        mp3_row.audio_channels.insert("mp3".into(), 2);
        for codecs in ["avc1.640028,mp4a.40.34", "avc1.640028,mp3"] {
            assert_eq!(playable_with(&variant(codecs), &mp3_row, SoftwareAudio::NONE), Ok(()), "{codecs}: native MP3");
        }
        assert!(playable_with(&variant("avc1.640028,mp4a.40.33"), &mp3_row, SoftwareAudio::NONE).is_err(), "MP2 has no native row");
        for codecs in ["avc1.640028,mp4a.40.33", "avc1.640028,mp4a.40.34", "avc1.640028,mp4a.6b", "avc1.640028,mp4a.69"] {
            assert_eq!(playable_with(&variant(codecs), &Caps::assumed(), pcm), Ok(()), "{codecs}: decoded to PCM");
            assert!(playable_with(&variant(codecs), &Caps::assumed(), SoftwareAudio::NONE).is_err(), "{codecs}: no path");
        }
        assert_eq!(playable_with(&variant("avc1.640028,mp4a.6b"), &mp3_row, SoftwareAudio::NONE), Ok(()), "an unnamed layer may be the MP3 the set plays");
        assert!(playable_with(&variant("avc1.640028,mp4a.40.32"), &Caps::assumed(), pcm).is_err(), "Layer I is not decoded");
        let dvb = master(vec![variant("avc1.640028,mp4a.40.33"), v(1_000_000, "avc1.4d401f,mp4a.40.2", Some((640, 360)))]);
        assert_eq!(select_with(&dvb, &Caps::assumed(), pcm), Ok(0), "the MP2 variant is no longer skipped");
    }

    #[test]
    fn the_raster_and_frame_rate_bounds_come_from_the_table() {
        let mut caps = Caps::assumed();
        caps.hevc_max = (1920, 1080);
        caps.h264_row = (1920, 1080, 30);
        let mut fast = v(5_000_000, "avc1.640028", Some((1920, 1080)));
        fast.frame_rate = Some(59.94);
        let m = master(vec![
            v(1_000_000, "avc1.64001f", Some((1280, 720))),
            v(15_000_000, "avc1.640033", Some((3840, 2160))),
            fast,
            v(4_000_000, "avc1.640028", Some((1920, 1080))),
        ]);
        assert_eq!(playable(&m.variants[1], &caps), Err(Unplayable::Raster { width: 3840, height: 2160 }));
        assert_eq!(playable(&m.variants[2], &caps), Err(Unplayable::FrameRate));
        assert_eq!(select(&m, &caps), Ok(3));
        let mut ntsc = v(1, "", Some((1920, 1080)));
        ntsc.frame_rate = Some(29.97);
        assert_eq!(playable(&ntsc, &caps), Ok(()), "29.97 fits a 30 fps row");
    }

    #[test]
    fn an_undeclared_variant_is_admitted_and_nothing_playable_is_an_error() {
        let caps = Caps::assumed();
        assert_eq!(select(&master(vec![v(0, "", None)]), &caps), Ok(0));
        let none = master(vec![v(1, "vp09.00.10.08", None), v(2, "av01.0.08M.08", None)]);
        assert_eq!(
            select(&none, &caps),
            Err(NoPlayableVariant { variants: 2, first_reason: Some(Unplayable::Codec("vp09.00.10.08".into())) }),
            "the assumed table lists neither VP9 nor AV1"
        );
        assert_eq!(select(&master(vec![]), &caps).unwrap_err().variants, 0);
    }

    #[test]
    fn dolby_vision_plays_only_with_a_usable_base_layer() {
        let caps = Caps::assumed();
        assert_eq!(playable(&v(1, "dvh1.08.06", None), &caps), Ok(()));
        assert!(playable(&v(1, "dvh1.05.06", None), &caps).is_err());
        assert!(playable(&v(1, "dvhe.07.06", None), &caps).is_err());
    }

    #[test]
    fn muxed_audio_outranks_a_separate_audio_rendition() {
        let caps = Caps::assumed();
        let mut alt = v(9_000_000, "avc1.640028,mp4a.40.2", Some((1920, 1080)));
        alt.audio_group = Some("aud".into());
        let mut muxed_group = v(2_000_000, "avc1.64001f,mp4a.40.2", Some((1280, 720)));
        muxed_group.audio_group = Some("muxed".into());
        let rendition = |group: &str, uri: Option<&str>| Rendition {
            kind: MediaType::Audio,
            group_id: group.into(),
            name: "en".into(),
            language: None,
            uri: uri.map(str::to_owned),
            default: true,
        };
        let m = MasterPlaylist {
            variants: vec![alt, muxed_group],
            renditions: vec![rendition("aud", Some("https://cdn.example/a.m3u8")), rendition("muxed", None)],
        };
        assert!(needs_separate_audio(&m.variants[0], &m));
        assert!(!needs_separate_audio(&m.variants[1], &m));
        assert_eq!(select(&m, &caps), Ok(1));
        let only_alt = MasterPlaylist { variants: vec![m.variants[0].clone()], renditions: m.renditions.clone() };
        assert_eq!(select(&only_alt, &caps), Ok(0), "chosen when nothing else plays");
    }
}
