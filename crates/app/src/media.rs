//! The real [`Player`]: `tv_media::playback::Playback` behind the shell's seam.
//!
//! Live channels play their stream URL. An archive programme plays as `tv_media`'s [`Catchup`]:
//! the exact-programme URL where the channel has one (a Flussonic window, or a declared template
//! filled for the programme), and for a shift channel a builder of shift URLs from any start, which
//! `tv_media` uses for the parts of a long programme and as the fallback. The player is ticked as
//! often as it asks (every frame while anything plays); its status is folded into the screens'
//! `Playback` a few times a second and on every event, and a programme without an archive or a
//! failed playback becomes a toast.

use std::sync::Arc;

use iptv::catchup::{archive_url, Times};
use iptv::model::{CatchupMode, Channel, Programme};
use tv_base::task::MainThread;
use tv_machine::present::PresentEvent;
use tv_media::headers::RequestHeaders;
use tv_media::playback::{self as media, AtEnd, Catchup, Event, State};
use tv_media::player::video_geometry::Viewport;
use tv_platform::i18n::msg;
use tv_screens::data::{MediaInfo, Playback, Rect};

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
    notice: Option<String>,
    /// The TV took the decoder back while we were in front: the next key resumes.
    taken: bool,
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
            notice: None,
            taken: false,
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
        changed
    }
}

/// The request headers the playlist gave `channel` (user agent, referrer), or none when they do
/// not pass `tv_media`'s checks; the refusal names the kind and position, never a value.
fn headers(channel: &Channel) -> RequestHeaders {
    RequestHeaders::from_pairs(&channel.http_headers).unwrap_or_else(|why| {
        log(&format!(
            "player: {:?} stream headers refused: {why}",
            channel.name
        ));
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

fn viewport(rect: Option<Rect>) -> Option<Viewport> {
    rect.map(|r| Viewport {
        x: r.x,
        y: r.y,
        width: r.w,
        height: r.h,
    })
}

/// `programme` on `channel` as `tv_media` plays it from the archive, or `None` without one.
fn catchup(channel: &Channel, programme: &Programme, offset_secs: i64) -> Option<Catchup> {
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
        at_end: AtEnd::ReturnToLive,
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
            } => match catchup(channel, programme, *offset_secs) {
                Some(c) => self.pb.play_catchup(c),
                None => self.notice = Some(msg::epg_catchup_unavailable().to_owned()),
            },
        }
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
                Event::ProgrammeEnded => {}
            }
        }
        if due && (self.pb.tick_interval_ms().is_some() || self.state != State::Idle) {
            self.status_at = now_ms + STATUS_MS;
            changed |= self.fold(playback);
        }
        changed
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
        let cu = catchup(&c, &p, 60).unwrap();
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
        let cu = catchup(&c, &programme(now - 600, now + 600), 0).unwrap();
        assert_eq!(cu.window, None);
        assert!(cu.shift.is_some());
    }

    #[test]
    fn a_declared_template_is_the_window_alone_and_no_template_is_no_archive() {
        let c = channel(
            "http://h.example/live/1.ts",
            CatchupMode::Default,
            Some("http://h.example/timeshift/{duration:60}/{Y}-{m}-{d}:{H}-{M}/1.ts"),
        );
        let cu = catchup(&c, &programme(1_700_000_000, 1_700_003_600), 0).unwrap();
        assert!(cu
            .window
            .unwrap()
            .starts_with("http://h.example/timeshift/60/"));
        assert!(cu.shift.is_none());
        let none = channel("http://h.example/live/1.ts", CatchupMode::Default, None);
        assert!(catchup(&none, &programme(0, 60), 0).is_none());
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
