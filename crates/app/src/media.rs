//! The real [`Player`]: `tv_media::playback::Playback` behind the shell's seam.
//!
//! Live channels play their stream URL. An archive programme plays as `tv_media`'s [`Catchup`]:
//! the exact-programme URL where the channel has one (a Flussonic window, or a declared template
//! filled for the programme), and for a shift channel a builder of shift URLs from any start, which
//! `tv_media` uses for the parts of a long programme and as the fallback. The player is ticked as
//! often as it asks (every frame while anything plays); its status is folded into the screens'
//! `Playback` a few times a second and on every event, and a programme without an archive or a
//! failed playback becomes a toast. A movie or an episode plays as `tv_media`'s [`Vod`]:
//! its own URL, seekable anywhere, holding its last picture at its end.

use std::sync::Arc;

use iptv::catchup::{archive_url, Times};
use iptv::model::{CatchupMode, Channel, Programme};
use iptv::vod::VodItem;
use tv_base::task::MainThread;
use tv_machine::present::PresentEvent;
use tv_media::headers::RequestHeaders;
use tv_media::playback::{self as media, AtEnd, Catchup, Event, State, Vod};
use tv_media::player::video_geometry::{AspectMode, Viewport};
use tv_platform::i18n::msg;
use tv_screens::data::{MediaInfo, Playback, Rect};
use tv_screens::playback::DisplayMode;

use crate::logs::log;
use crate::player::{Player, Stream};
use crate::store::{unix_now, utc_offset_at};

/// How often the status is read into `Playback` between events.
const STATUS_MS: u64 = 250;

pub struct MediaPlayer {
    pb: media::Playback,
    /// The video plane is bound to the surface: the UI keeps presenting.
    bound: bool,
    status_at: u64,
    /// The state last folded in, so a failure is announced once.
    state: State,
    /// What `select_audio_track` takes, by position in `Playback::audio_tracks`.
    tracks: Vec<usize>,
    /// Read the audio tracks again at the next status (a new stream, or a switch).
    tracks_stale: bool,
    /// The subtitle tracks' generation last folded in (`tv_media::subtitles`).
    subtitles_seen: Option<u64>,
    notice: Option<String>,
    /// The TV took the decoder back while we were in front: the next key resumes.
    taken: bool,
    /// The time shift playing reached its programme's end ([`Player::take_ended`]).
    ended: bool,
}

impl MediaPlayer {
    /// Bind the video plane (once, at boot, after `ff::boot` and the port's sink install).
    pub fn new(mt: MainThread) -> Self {
        MediaPlayer {
            pb: media::Playback::new(mt),
            bound: false,
            status_at: 0,
            state: State::Idle,
            tracks: Vec::new(),
            tracks_stale: true,
            subtitles_seen: None,
            notice: None,
            taken: false,
            ended: false,
        }
    }

    /// Fold the status into `playback`; returns whether anything shown changed.
    fn fold(&mut self, playback: &mut Playback) -> bool {
        let st = self.pb.status();
        let mut changed = false;
        let media = MediaInfo {
            width: st.media.width,
            height: st.media.height,
            fps: st.media.fps.round() as u32,
            audio_channels: u8::try_from(st.media.audio_channels).unwrap_or(u8::MAX),
        };
        changed |= set(&mut playback.media, media);
        changed |= set(&mut playback.paused, st.paused);
        if let Some(c) = st.catchup {
            changed |= set(&mut playback.position_secs, c.position_ms / 1000);
        }
        if let (Some(at), Some(vod)) = (st.vod, playback.vod.as_mut()) {
            changed |= set(&mut vod.duration_secs, at.duration_ms / 1000);
            changed |= set(&mut vod.ended, at.ended);
            changed |= set(&mut playback.position_secs, at.position_ms / 1000);
        }
        if st.state != self.state {
            match st.state {
                State::NoArchive => self.notice = Some(msg::epg_catchup_unavailable().to_owned()),
                State::Failed(_) => self.notice = self.pb.error().map(|e| e.readout.to_owned()),
                State::Playing => self.tracks_stale = true,
                _ => {}
            }
            self.state = st.state;
        }
        if std::mem::take(&mut self.tracks_stale) {
            let tracks = self.pb.audio_tracks();
            let names: Vec<String> = tracks.iter().map(track_name).collect();
            let selected = tracks.iter().position(|t| t.selected).unwrap_or(0);
            self.tracks = tracks.iter().map(|t| t.index).collect();
            changed |= set(&mut playback.audio_tracks, names);
            changed |= set(&mut playback.audio_track, selected);
        }
        let generation = tv_media::subtitles::tracks_generation();
        if self.subtitles_seen != Some(generation) {
            self.subtitles_seen = Some(generation);
            let names = self
                .pb
                .subtitle_tracks()
                .iter()
                .enumerate()
                .map(|(i, t)| subtitle_name(i, t))
                .collect();
            changed |= set(&mut playback.subtitle_tracks, names);
            changed |= set(&mut playback.subtitle_track, self.pb.subtitle_track());
        }
        changed
    }
}

/// How subtitle track `i` reads in the player menu: its language in its own name, else its
/// number, marked when it is for the hard of hearing.
fn subtitle_name(i: usize, t: &tv_media::subtitles::Track) -> String {
    let name = tv_base::tracks::language_name(&t.language)
        .unwrap_or_else(|| msg::subtitles_track(i64::try_from(i + 1).unwrap_or(i64::MAX)));
    if t.hearing_impaired {
        msg::subtitles_hearing_impaired(&name)
    } else {
        name
    }
}

/// The request headers the playlist gave `channel` (user agent, referrer), or none when they do
/// not pass `tv_media`'s checks; the refusal names the kind and position, never a value.
fn headers(channel: &Channel) -> RequestHeaders {
    headers_of(&channel.name, &channel.http_headers)
}

/// [`headers`] of the stream `name` with `pairs`.
fn headers_of(name: &str, pairs: &[(String, String)]) -> RequestHeaders {
    RequestHeaders::from_pairs(pairs).unwrap_or_else(|why| {
        log(&format!("player: {name:?} stream headers refused: {why}"));
        RequestHeaders::none()
    })
}

/// Replace `*slot` with `value`; whether it differed.
fn set<T: PartialEq>(slot: &mut T, value: T) -> bool {
    let changed = *slot != value;
    *slot = value;
    changed
}

/// How a track reads in the player menu: its name, else its language, else its number, with the
/// codec and channels.
fn track_name(t: &media::AudioTrack) -> String {
    let label = [&t.name, &t.language]
        .into_iter()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| (t.index + 1).to_string());
    format!("{label} ({} {}ch)", t.codec, t.channels)
}

/// How `tv_media` places the picture for `mode`.
fn aspect(mode: DisplayMode) -> AspectMode {
    match mode {
        DisplayMode::Normal => AspectMode::Auto,
        DisplayMode::Fill => AspectMode::Fill,
        DisplayMode::Ratio4x3 => AspectMode::Ratio4x3,
        DisplayMode::Ratio16x9 => AspectMode::Ratio16x9,
    }
}

fn viewport(rect: Option<Rect>) -> Option<Viewport> {
    rect.map(|r| Viewport {
        x: r.x,
        y: r.y,
        width: r.w,
        height: r.h,
    })
}

/// `programme` on `channel` as `tv_media` plays it from the archive, or `None` without one; a
/// time shift (`time_shift`) plays on at its end, any other goes live.
fn catchup(
    channel: &Channel,
    programme: &Programme,
    offset_secs: i64,
    time_shift: bool,
) -> Option<Catchup> {
    let (start, end) = (programme.start, programme.stop);
    let now = unix_now();
    let exact = archive_url(channel, Times { start, end, now }, utc_offset_at)?;
    let shift = (channel.catchup.mode == Some(CatchupMode::Shift)).then(|| {
        let ch = channel.clone();
        // A start that has not ended yet fills the shift template itself, never the window.
        let builder = move |utc: i64| {
            let now = unix_now();
            let times = Times {
                start: utc,
                end: end.max(now + 1),
                now,
            };
            archive_url(&ch, times, utc_offset_at).unwrap_or_default()
        };
        Arc::new(builder) as media::ShiftUrl
    });
    // For a shift channel the URL is a window only when the programme's own (Flussonic) one
    // replaced the shift template; otherwise the shift builder covers it.
    let window = match &shift {
        Some(build) => Some(exact).filter(|url| *url != build(start)),
        None => Some(exact),
    };
    Some(Catchup {
        start,
        end,
        offset_secs,
        window,
        shift,
        live_url: channel.url.clone(),
        headers: headers(channel),
        at_end: if time_shift {
            AtEnd::Continue
        } else {
            AtEnd::ReturnToLive
        },
    })
}

impl Player for MediaPlayer {
    fn open(&mut self, stream: &Stream<'_>, rect: Option<Rect>) {
        self.pb.set_video_rect(viewport(rect));
        match stream {
            Stream::Live(channel) => self.pb.play_live(&channel.url, headers(channel)),
            Stream::Catchup {
                channel,
                programme,
                offset_secs,
                shift,
            } => match catchup(channel, programme, *offset_secs, *shift) {
                Some(c) => self.pb.play_catchup(c),
                None => self.notice = Some(msg::epg_catchup_unavailable().to_owned()),
            },
        }
        self.tracks_stale = true;
        self.status_at = 0;
        self.ended = false;
    }

    fn open_vod(&mut self, item: &VodItem, offset_secs: i64, rect: Option<Rect>) {
        self.pb.set_video_rect(viewport(rect));
        self.pb.play_vod(Vod {
            url: item.url.clone(),
            headers: headers_of(&item.title, &item.http_headers),
            offset_secs,
        });
        self.tracks_stale = true;
        self.status_at = 0;
    }

    fn set_rect(&mut self, rect: Option<Rect>) {
        self.pb.set_video_rect(viewport(rect));
    }

    fn seek(&mut self, position_secs: i64) {
        self.pb.seek(position_secs.saturating_mul(1000));
    }

    fn go_live(&mut self) {
        self.pb.go_live();
        self.tracks_stale = true;
        self.status_at = 0;
    }

    fn set_paused(&mut self, paused: bool) {
        if !self.pb.set_paused(paused) {
            log("player: pause refused by the pipeline");
        }
        self.status_at = 0;
    }

    fn select_audio_track(&mut self, index: usize) {
        let Some(&track) = self.tracks.get(index) else {
            return;
        };
        if self.pb.select_audio_track(track).is_err() {
            log(&format!(
                "player: audio track {index} cannot play on this set"
            ));
        }
        self.tracks_stale = true;
        self.status_at = 0;
    }

    fn select_subtitle_track(&mut self, index: Option<usize>) {
        self.pb.select_subtitle_track(index);
        self.status_at = 0;
    }

    fn poll(&mut self, now_ms: u64, playback: &mut Playback) -> bool {
        if let Some(bound) = self.pb.tick(now_ms as u32) {
            self.bound = bound;
            tv_machine::idle::note(PresentEvent::VideoPlane(bound));
        }
        let mut changed = false;
        let mut due = now_ms >= self.status_at;
        while let Some(event) = self.pb.take_event() {
            due = true;
            match event {
                Event::ReturnedToLive => {
                    changed |= playback.catchup.take().is_some();
                    playback.position_secs = 0;
                }
                Event::FirstFrame { .. } | Event::Recovered => self.tracks_stale = true,
                Event::DecoderTaken => self.taken = true,
                Event::ProgrammeEnded => self.ended = true,
            }
        }
        if due && (self.pb.tick_interval_ms().is_some() || self.state != State::Idle) {
            self.status_at = now_ms + STATUS_MS;
            changed |= self.fold(playback);
        }
        changed
    }

    fn take_ended(&mut self) -> bool {
        std::mem::take(&mut self.ended)
    }

    fn next_wake_ms(&self, now_ms: u64) -> Option<u64> {
        self.pb.tick_interval_ms().map(|ms| now_ms + u64::from(ms))
    }

    fn needs_frames(&self) -> bool {
        self.bound
    }

    fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    fn suspend(&mut self) {
        self.pb.suspend();
    }

    fn resume(&mut self) {
        self.pb.resume();
        self.taken = false;
        self.status_at = 0;
    }

    fn on_key(&mut self) {
        if self.taken {
            log("player: the decoder was taken back; trying again on a key");
            self.resume();
        }
    }

    fn set_display_mode(&mut self, mode: DisplayMode) {
        log(&format!("player: display mode {}", mode.value()));
        self.pb.set_aspect_mode(aspect(mode));
    }

    fn set_audio_offset(&mut self, offset_ms: i32) {
        log(&format!("player: audio offset {offset_ms} ms"));
        self.pb.set_audio_offset_ms(i64::from(offset_ms));
    }

    fn stop(&mut self) {
        self.pb.stop();
        self.taken = false;
        self.state = State::Idle;
        self.tracks.clear();
        self.status_at = 0;
    }

    fn shutdown(&mut self) {
        self.pb.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iptv::model::Catchup as ChannelCatchup;

    fn channel(url: &str, mode: CatchupMode, source: Option<&str>) -> Channel {
        let mut c = tv_screens::data::fake::FakeData::new(0, 1)
            .channels
            .remove(0);
        c.url = url.into();
        c.catchup = ChannelCatchup {
            mode: Some(mode),
            days: 7,
            source: source.map(str::to_owned),
            ..Default::default()
        };
        c
    }

    fn programme(start: i64, stop: i64) -> Programme {
        Programme {
            start,
            stop,
            title: "P".into(),
            ..Programme::default()
        }
    }

    #[test]
    fn an_ended_programme_on_a_flussonic_shift_channel_gets_its_window_and_shift() {
        let url = "http://h.example/ch/mono.m3u8";
        let derived = format!("{url}?utc={{utc}}&lutc={{lutc}}");
        let c = channel(url, CatchupMode::Shift, Some(&derived));
        let p = programme(1_700_000_000, 1_700_003_600);
        let cu = catchup(&c, &p, 60, false).unwrap();
        assert_eq!(
            cu.window.as_deref(),
            Some("http://h.example/ch/mono-1700000000-3600.m3u8")
        );
        let shift = cu.shift.unwrap()(1_700_000_000 + 3 * 3600);
        assert!(shift.starts_with("http://h.example/ch/mono.m3u8?utc=1700010800&lutc="));
        assert_eq!((cu.start, cu.end, cu.offset_secs), (p.start, p.stop, 60));
        assert_eq!(cu.live_url, url);
    }

    #[test]
    fn the_programme_on_air_on_a_shift_channel_is_shift_only() {
        let url = "http://h.example/ch/mono.m3u8";
        let derived = format!("{url}?utc={{utc}}&lutc={{lutc}}");
        let c = channel(url, CatchupMode::Shift, Some(&derived));
        let now = unix_now();
        let cu = catchup(&c, &programme(now - 600, now + 600), 0, true).unwrap();
        assert_eq!(cu.window, None);
        assert!(cu.shift.is_some());
    }

    #[test]
    fn a_time_shift_plays_on_at_its_end_and_any_other_archive_goes_live() {
        let url = "http://h.example/ch/mono.m3u8";
        let derived = format!("{url}?utc={{utc}}&lutc={{lutc}}");
        let c = channel(url, CatchupMode::Shift, Some(&derived));
        let p = programme(1_700_000_000, 1_700_003_600);
        assert_eq!(catchup(&c, &p, 0, true).unwrap().at_end, AtEnd::Continue);
        assert_eq!(
            catchup(&c, &p, 0, false).unwrap().at_end,
            AtEnd::ReturnToLive
        );
    }

    #[test]
    fn a_declared_template_is_the_window_alone_and_no_template_is_no_archive() {
        let c = channel(
            "http://h.example/live/1.ts",
            CatchupMode::Default,
            Some("http://h.example/timeshift/{duration:60}/{Y}-{m}-{d}:{H}-{M}/1.ts"),
        );
        let cu = catchup(&c, &programme(1_700_000_000, 1_700_003_600), 0, false).unwrap();
        assert!(cu
            .window
            .unwrap()
            .starts_with("http://h.example/timeshift/60/"));
        assert!(cu.shift.is_none());
        let none = channel("http://h.example/live/1.ts", CatchupMode::Default, None);
        assert!(catchup(&none, &programme(0, 60), 0, false).is_none());
    }

    #[test]
    fn every_display_mode_places_the_picture_its_own_way() {
        let modes: Vec<_> = DisplayMode::ALL.into_iter().map(aspect).collect();
        assert_eq!(
            modes,
            [
                AspectMode::Auto,
                AspectMode::Fill,
                AspectMode::Ratio4x3,
                AspectMode::Ratio16x9
            ]
        );
    }

    #[test]
    fn track_names_and_rects() {
        let t = media::AudioTrack {
            index: 2,
            codec: "aac".into(),
            channels: 2,
            language: "pol".into(),
            ..Default::default()
        };
        assert_eq!(track_name(&t), "pol (aac 2ch)");
        let r = Rect {
            x: 1,
            y: 2,
            w: 3,
            h: 4,
        };
        assert_eq!(
            viewport(Some(r)),
            Some(Viewport {
                x: 1,
                y: 2,
                width: 3,
                height: 4
            })
        );
    }
}
