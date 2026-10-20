//! **One continuous timeline across a stream's discontinuities.**
//!
//! The source hands the demuxer the segments as one byte stream ([`super::source`]), and after an
//! `EXT-X-DISCONTINUITY` the timestamps may start again anywhere: an archive's ad break comes from
//! another recording, and on the C5 its timestamps jumped back 45 s. Fed as they were, the
//! player's lanes dropped every AU more than 2 s behind what they had already fed (a stale
//! pre-seek AU, to `player::engine`), so the picture froze at the break; and since the position
//! never passed the break, the restart that followed resumed before it, and froze there again. A
//! jump forward is no better: the lanes hold an AU far ahead of the presented position until the
//! clock reaches it, and the clock waits for media.
//!
//! So each run of segments between two discontinuities gets one shift, decided by its first
//! timed packet: none when that packet continues its stream (within [`CONTINUOUS_NS`]), else the
//! shift that starts the run where the stream before it ended. Every packet of the run moves by
//! the same shift, audio and video alike, so they stay as the run interleaves them, and a packet
//! the demuxer completes after the next run has begun keeps its own run's shift. The source
//! numbers the runs ([`super::HlsSource::run_at`], by byte position); the demuxer places every
//! packet here before anything reads its timestamps (`ff::demux`), so positions, History and the
//! restart all read the one timeline.
//!
//! Where a stream ended is only believed once the stream has stayed there ([`SETTLE_NS`]), and a
//! packet's duration only when a frame could last that long: FFmpeg times every frame of a PES
//! from the PES's one timestamp, so one bad timestamp comes out as a burst of frames that continue
//! each other (an MPEG audio PES holds several), and it derives a duration from what the codec
//! declares, not from the stream. On the C1 a live channel's end was taken 55013 s past its
//! timestamps, and the next break, whose timestamps went on, was shifted 55013 s: the picture
//! stopped until the watchdog restarted the channel. For the same reason the first packet does
//! not decide a run alone: until the run settles, each steering stream's first packet of it, and
//! any that does not go on from its stream's packets of the run, decides it again when it
//! disagrees on whether its stream went on, and every stream takes the new shift from there.

use std::collections::VecDeque;

/// A run whose first timestamp lands within this of where its stream stopped continues the
/// timeline as it is: a server may tag a splice whose timestamps do continue, and a run's first
/// packet sits up to a frame, or its interleave, off its stream's last one. A frame lasts less;
/// a stream whose packets come further apart than that may land its spacing further on.
pub const CONTINUOUS_NS: i64 = 500_000_000;

/// How long a stream's packets must continue each other somewhere before the timeline believes
/// it is there: a stream's end moves past a jump, and a run's shift is settled, only then.
pub const SETTLE_NS: i64 = 1_000_000_000;

/// The runs remembered, here and by the source. The demuxer completes a packet at most a few runs
/// after the one its bytes came from (its probe reads ahead), and a run is a segment at least.
pub const RUNS_KEPT: usize = 16;

/// The decisions of a run that are logged: streams whose timestamps keep disagreeing may decide it
/// again at every packet until it settles.
const DECISIONS_LOGGED: u8 = 4;

/// The shifts of a stream's runs.
#[derive(Debug, Default)]
pub struct Timeline {
    /// `(run, shift ns)`, oldest first; the last one is the newest run begun.
    shifts: VecDeque<(u64, i64)>,
    /// Where each steering stream has got to.
    reach: Vec<Reach>,
    /// How the newest run's shift was decided, until it settles.
    pending: Option<Pending>,
    /// The run of each stream's last packet with a position: `(stream index, run)`.
    last_runs: Vec<(i32, u64)>,
}

/// Whether a packet at `at` (its end; as a vote, its start) goes on from `to`, where its stream's
/// packets had got to: within [`CONTINUOUS_NS`] of it, or as much further on as its stream's
/// packets come apart, `gap` (a still picture's frames, a second or more).
fn goes_on(at: i64, to: i64, gap: i64) -> bool {
    (-CONTINUOUS_NS..=CONTINUOUS_NS.saturating_add(gap)).contains(&at.saturating_sub(to))
}

/// Where a steering stream (the video, the audio played) has got to on the timeline.
#[derive(Debug, Clone, Copy)]
struct Reach {
    stream: i32,
    /// The end of its media where its packets go on continuously, the furthest of them.
    end: i64,
    /// Packets that do not go on from `end` but go on from each other, `(start of the first,
    /// furthest end)`: a jump, or a bad timestamp. They are the stream's end once they have
    /// lasted [`SETTLE_NS`].
    away: Option<(i64, i64)>,
    /// Where its last packet starts, and its last two steps forward from one packet to the next.
    last: i64,
    steps: [i64; 2],
}

impl Reach {
    fn new(stream: i32, start: i64, end: i64) -> Self {
        Reach { stream, end, away: None, last: start, steps: [0; 2] }
    }

    /// How far apart its packets come: the shorter of its last two steps, so the one long step a
    /// timestamp far off makes does not stretch it; 0 until it has made two.
    fn gap(&self) -> i64 {
        self.steps[0].min(self.steps[1])
    }

    /// A packet of the stream placed from `start` to `end`, judged by how far apart the packets
    /// before it came.
    fn reach(&mut self, start: i64, end: i64) {
        let gap = self.gap();
        let step = start.saturating_sub(self.last);
        if step > 0 {
            self.steps = [self.steps[1], step];
        }
        self.last = start;
        if goes_on(end, self.end, gap) {
            self.end = self.end.max(end);
            self.away = None;
            return;
        }
        let (from, to) = match self.away {
            Some((from, to)) if goes_on(end, to, gap) => (from, to.max(end)),
            _ => (start, end),
        };
        self.away = Some((from, to));
        if to.saturating_sub(from) >= SETTLE_NS {
            (self.end, self.away) = (to, None);
        }
    }
}

/// Where the furthest steering stream has got to.
fn furthest(reach: &[Reach]) -> Option<i64> {
    reach.iter().map(|r| r.end).max()
}

/// How the newest run's shift was decided.
#[derive(Debug)]
struct Pending {
    run: u64,
    /// What it is decided against: the run before's shift, and where the furthest stream had got
    /// to when the run began.
    before: i64,
    furthest: i64,
    /// The last decision: the run went on from where its streams were (its shift is `before`).
    continued: bool,
    /// The decisions taken.
    decisions: u8,
    /// Each steering stream's packets of the run.
    spans: Vec<Span>,
}

/// A steering stream's packets of the newest run.
#[derive(Debug)]
struct Span {
    stream: i32,
    /// Where the stream had got to before the run.
    own: i64,
    /// What its latest packets of the run that go on from each other cover on the timeline.
    from: i64,
    to: i64,
}

impl Pending {
    /// Decide the run by a packet at `ts_ns` of a stream that had got to `own`, which went on
    /// from there or did not (`continued`): the run before's shift when it did, else the one that
    /// starts the run past the furthest stream, so no stream is handed a timestamp it already
    /// passed. Logged, the first few times.
    fn decide(&mut self, ts_ns: i64, own: i64, continued: bool) -> i64 {
        let next = if continued { self.before } else { self.furthest.saturating_sub(ts_ns) };
        if self.decisions < DECISIONS_LOGGED {
            tv_base::eventlog::log(&format!(
                "hls: timeline run {}{}: the timestamps jump {} ms at the discontinuity; {}",
                self.run,
                if self.decisions == 0 { "" } else { ", decided again by a later packet" },
                ts_ns.saturating_add(self.before).saturating_sub(own) / 1_000_000,
                if next == self.before { "continued as they are".to_owned() } else { format!("shifted {} ms to continue", next / 1_000_000) }
            ));
        }
        (self.continued, self.decisions) = (continued, self.decisions.saturating_add(1));
        next
    }
}

impl Timeline {
    /// The run of a packet of `stream` read from byte `pos`, through `run_at` (the source's
    /// [`super::HlsSource::run_at`]). A packet with no position (mpegts keeps one only on the first
    /// frame a parser splits off a PES: most AAC frames) is in its stream's last packet's run, so
    /// the frames of a PES the next run's bytes complete keep their own; a stream's first packet
    /// with none is in the newest run.
    pub fn run_of(&mut self, stream: i32, pos: Option<u64>, run_at: impl FnOnce(u64) -> u64) -> u64 {
        let last = self.last_runs.iter_mut().find(|(s, _)| *s == stream);
        let Some(pos) = pos else { return last.map_or_else(|| run_at(u64::MAX), |&mut (_, run)| run) };
        let run = run_at(pos);
        match last {
            Some((_, r)) => *r = run,
            None => self.last_runs.push((stream, run)),
        }
        run
    }

    /// The shift for a packet of `run` from a steering stream (the video, the audio played) at
    /// `ts_ns` lasting `dur_ns` (0: unknown). The first one of a run past the newest begins it.
    pub fn place(&mut self, run: u64, stream: i32, ts_ns: i64, dur_ns: i64) -> i64 {
        // Longer than a frame lasts: not the stream's own duration (see the module doc).
        let dur = if (0..=CONTINUOUS_NS).contains(&dur_ns) { dur_ns } else { 0 };
        let shift = match self.shifts.back() {
            None => {
                self.shifts.push_back((run, 0));
                0
            }
            Some(&(newest, shift)) if run > newest => self.begin(run, stream, ts_ns, dur, shift),
            Some(&(newest, shift)) if run == newest => self.settle(stream, ts_ns, dur, shift),
            Some(_) => self.shift_of(run),
        };
        let start = ts_ns.saturating_add(shift);
        let end = start.saturating_add(dur);
        match self.reach.iter_mut().find(|r| r.stream == stream) {
            Some(r) => r.reach(start, end),
            None => self.reach.push(Reach::new(stream, start, end)),
        }
        shift
    }

    /// The shift of `run` for a packet that does not steer (a subtitle, an untimed AU): the newest
    /// run's for one not begun yet, the oldest remembered for one before it.
    pub fn shift_of(&self, run: u64) -> i64 {
        newest_at(&self.shifts, run).unwrap_or(0)
    }

    /// Begin `run` at its first timed packet, `stream`'s at `ts_ns`; `before` is the run before's
    /// shift.
    fn begin(&mut self, run: u64, stream: i32, ts_ns: i64, dur: i64, before: i64) -> i64 {
        remember(&mut self.shifts, (run, before));
        self.pending = furthest(&self.reach).map(|furthest| Pending {
            run,
            before,
            furthest,
            continued: true,
            decisions: 0,
            spans: Vec::new(),
        });
        self.settle(stream, ts_ns, dur, before)
    }

    /// The shift for a packet of the newest run, whose shift is `shift`. Until the run settles (a
    /// stream's packets of it have gone on from each other for [`SETTLE_NS`]), a packet that does
    /// not go on from its stream's packets of the run, its stream's first of the run among them,
    /// votes on whether its stream went on from where it was before the run. One that disagrees
    /// with the decision decides the run again, for every stream: one of the two was a stray (a bad
    /// timestamp, a leftover of the run before), and when it is this one, the next packet the new
    /// shift puts off its stream's packets votes back. When both jump, the decision stands: the
    /// run carries a second jump of its own.
    fn settle(&mut self, stream: i32, ts_ns: i64, dur: i64, shift: i64) -> i64 {
        let Some(p) = self.pending.as_mut() else { return shift };
        let reach = self.reach.iter().find(|r| r.stream == stream);
        let gap = reach.map_or(0, Reach::gap);
        let i = p.spans.iter().position(|s| s.stream == stream);
        let (own, span_to) = match i {
            Some(i) => (p.spans[i].own, Some(p.spans[i].to)),
            // Its stream's first packet of the run: measured from where the stream had got to, or,
            // one that had no packet before, from where the furthest stream has.
            None => (reach.map_or_else(|| furthest(&self.reach).unwrap_or(p.furthest), |r| r.end), None),
        };
        let end = |shift: i64| ts_ns.saturating_add(shift).saturating_add(dur);
        let goes_on_span = |shift| span_to.is_some_and(|to| goes_on(end(shift), to, gap));
        let mut shift = shift;
        if !goes_on_span(shift) {
            let continued = goes_on(ts_ns.saturating_add(p.before), own, gap);
            if p.decisions == 0 || continued != p.continued {
                shift = p.decide(ts_ns, own, continued);
                if let Some(newest) = self.shifts.back_mut() {
                    newest.1 = shift;
                }
            }
        }
        let (from, to) = (ts_ns.saturating_add(shift), end(shift));
        match i {
            Some(i) if goes_on_span(shift) => {
                let s = &mut p.spans[i];
                s.to = s.to.max(to);
                if s.to.saturating_sub(s.from) >= SETTLE_NS {
                    self.pending = None;
                }
            }
            Some(i) => (p.spans[i].from, p.spans[i].to) = (from, to),
            None => p.spans.push(Span { stream, own, from, to }),
        }
        shift
    }
}

/// The value of the newest entry of `q` (keys ascending) keyed at or before `key`, else the
/// oldest one's: a key past the newest belongs to it, one before the oldest remembered to that.
pub(super) fn newest_at<T: Copy>(q: &VecDeque<(u64, T)>, key: u64) -> Option<T> {
    q.iter().rev().find(|(k, _)| *k <= key).or(q.front()).map(|&(_, v)| v)
}

/// Push `entry` as the newest of `q`, forgetting the oldest past [`RUNS_KEPT`].
pub(super) fn remember<T>(q: &mut VecDeque<(u64, T)>, entry: (u64, T)) {
    if q.len() == RUNS_KEPT {
        q.pop_front();
    }
    q.push_back(entry);
}


#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;
    const V: i32 = 0;
    const A: i32 = 1;

    /// Place 40 ms video and 20 ms audio packets of `run` from `from_ms` for `len_ms`, interleaved
    /// as a muxer does, and return their placed timestamps, ms.
    fn play(tl: &mut Timeline, run: u64, from_ms: i64, len_ms: i64) -> (Vec<i64>, Vec<i64>) {
        let (mut v, mut a) = (Vec::new(), Vec::new());
        for t in (from_ms..from_ms + len_ms).step_by(20) {
            if (t - from_ms) % 40 == 0 {
                v.push(t + tl.place(run, V, t * MS, 40 * MS) / MS);
            }
            a.push(t + tl.place(run, A, t * MS, 20 * MS) / MS);
        }
        (v, a)
    }

    fn continuous(ts: &[i64], step: i64) -> bool {
        ts.windows(2).all(|w| w[1] - w[0] == step)
    }

    /// Place a packet of `stream` read from byte `pos` (`None`: no position) at `ts_ms` lasting
    /// `dur_ms`, its run told by `runs` (`(byte offset, run)`) as the source tells it, and return
    /// its placed timestamp, ms.
    fn read(tl: &mut Timeline, runs: &VecDeque<(u64, u64)>, stream: i32, pos: Option<u64>, ts_ms: i64, dur_ms: i64) -> i64 {
        let run = tl.run_of(stream, pos, |pos| newest_at(runs, pos).unwrap_or(0));
        ts_ms + tl.place(run, stream, ts_ms * MS, dur_ms * MS) / MS
    }

    #[test]
    fn a_backward_jump_continues_where_the_run_before_ended() {
        let mut tl = Timeline::default();
        // 6 s from 103 s, then the ad break's timestamps 45 s back, as on the C5.
        let (v0, a0) = play(&mut tl, 0, 103_000, 6_000);
        let (v1, a1) = play(&mut tl, 1, 64_000, 6_000);
        assert_eq!((v0[0], a0[0]), (103_000, 103_000), "the first run is not moved");
        assert_eq!((v1[0], a1[0]), (109_000, 109_000), "the next run starts where the first ended");
        let (v, a) = ([v0, v1].concat(), [a0, a1].concat());
        assert!(continuous(&v, 40) && continuous(&a, 20), "one timeline: {v:?}");
    }

    #[test]
    fn a_forward_jump_and_a_wrapped_one_are_closed_too() {
        let mut tl = Timeline::default();
        let (v0, _) = play(&mut tl, 0, 600_000, 1_000);
        let (v1, _) = play(&mut tl, 1, 630_000, 1_000);
        // FFmpeg's wrap guard moves a timestamp 60 s before the first one by 2^33 ticks (26.5 h).
        let wrapped = 540_000 + (1i64 << 33) / 90;
        let (v2, a2) = play(&mut tl, 2, wrapped, 1_000);
        assert_eq!((v1[0], v2[0], a2[0]), (601_000, 602_000, 602_000));
        assert!(continuous(&[v0, v1, v2].concat(), 40));
    }

    #[test]
    fn a_run_whose_timestamps_continue_is_not_moved() {
        let mut tl = Timeline::default();
        let _ = play(&mut tl, 0, 10_000, 2_000);
        // A splice tagged although its timestamps go on, its first packet a few frames off.
        assert_eq!(tl.place(1, V, 12_120 * MS, 40 * MS), 0);
        assert_eq!(tl.place(1, A, 11_980 * MS, 20 * MS), 0);
        assert_eq!(tl.shift_of(1), 0);
    }

    #[test]
    fn a_late_packet_of_a_run_keeps_its_runs_shift_and_others_take_the_newest() {
        let mut tl = Timeline::default();
        let _ = play(&mut tl, 0, 100_000, 1_960);
        // The audio of the new run comes out before the demuxer completes the old run's last frame.
        let shift = tl.place(1, A, 50_000 * MS, 20 * MS);
        assert_eq!(shift, 51_960 * MS);
        assert_eq!(tl.place(0, V, 101_960 * MS, 40 * MS), 0, "the old run's frame keeps its shift");
        assert_eq!(tl.place(1, V, 50_000 * MS, 40 * MS), shift, "the new run's video takes the audio's shift");
        assert_eq!(tl.shift_of(2), shift, "a run not begun yet: the newest shift");
        assert_eq!(tl.shift_of(0), 0);
    }

    #[test]
    fn the_frames_of_a_pes_the_next_runs_bytes_complete_keep_their_run() {
        let mut tl = Timeline::default();
        // The source's runs by byte: run 1 (50 s back) from byte 10_000, run 2 from 20_000.
        let runs = VecDeque::from([(0, 0), (10_000, 1), (20_000, 2)]);
        let _ = play(&mut tl, 0, 100_000, 2_000);
        // The run's last audio PES, cut at the splice: its first frame has the PES's position.
        assert_eq!(read(&mut tl, &runs, A, Some(9_900), 102_000, 20), 102_000);
        // The next run's video begins it; only then do its bytes complete the PES, whose other
        // frames mpegts leaves with no position.
        assert_eq!(read(&mut tl, &runs, V, Some(10_200), 52_000, 40), 102_020);
        assert_eq!(read(&mut tl, &runs, A, None, 102_020, 20), 102_020, "the PES keeps its run's shift");
        assert_eq!(read(&mut tl, &runs, A, None, 102_040, 20), 102_040);
        assert_eq!(read(&mut tl, &runs, A, Some(10_400), 52_040, 20), 102_060);
        // Not shifted 50 s further by those frames: the run after starts where the audio ended.
        assert_eq!(read(&mut tl, &runs, V, Some(20_100), 0, 40), 102_080);
        assert_eq!(tl.run_of(2, None, |pos| newest_at(&runs, pos).unwrap_or(0)), 2, "a stream's first packet with none: the newest run");
    }

    #[test]
    fn one_timestamp_far_ahead_does_not_move_the_runs_after_it() {
        let mut tl = Timeline::default();
        let _ = play(&mut tl, 0, 10_000, 5_000);
        // A corrupt video packet an hour ahead, then the run goes on, after a real 1 s gap.
        let _ = tl.place(0, V, 3_615_000 * MS, 40 * MS);
        let _ = play(&mut tl, 0, 16_000, 4_000);
        // The next run starts again at 0: it continues where the run really ended, at 20 s.
        assert_eq!(tl.place(1, V, 0, 40 * MS), 20_000 * MS);
    }

    #[test]
    fn a_bad_timestamp_on_a_pes_of_several_frames_does_not_move_the_runs_after_it() {
        let mut tl = Timeline::default();
        let _ = play(&mut tl, 0, 13_160_000, 6_000);
        // One audio PES 55013434 ms ahead: FFmpeg times its five frames from it, so they continue
        // each other there. Then the stream goes on where it was.
        for k in 0..5 {
            let _ = tl.place(0, A, (13_166_000 + 55_013_434 + 24 * k) * MS, 24 * MS);
        }
        let _ = play(&mut tl, 0, 13_166_000, 6_000);
        // The next run's timestamps go on, its first packet audio (live on the C1: shifted 55013434 ms).
        assert_eq!(tl.place(1, A, 13_172_000 * MS, 20 * MS), 0);
        assert_eq!(tl.place(1, V, 13_172_000 * MS, 40 * MS), 0);
    }

    #[test]
    fn a_packet_duration_far_too_long_does_not_move_a_continuing_run() {
        let mut tl = Timeline::default();
        // A codec declaring frames hours long (an H.264 SPS's VUI timing), its timestamps 40 ms apart.
        for t in (0..2_000).step_by(40) {
            let _ = tl.place(0, V, (10_000 + t) * MS, 55_013_434 * MS);
        }
        assert_eq!(tl.place(1, V, 12_000 * MS, 55_013_434 * MS), 0);
    }

    #[test]
    fn a_streams_first_timestamp_far_off_is_not_where_it_ends() {
        let mut tl = Timeline::default();
        let _ = tl.place(0, A, 68_000_000 * MS, 24 * MS);
        let _ = play(&mut tl, 0, 10_000, 3_000);
        assert_eq!(tl.place(1, A, 13_000 * MS, 20 * MS), 0);
    }

    #[test]
    fn a_run_begun_by_a_packet_off_its_stream_is_decided_again_by_the_next() {
        let mut tl = Timeline::default();
        let _ = play(&mut tl, 0, 3_610_000, 2_000);
        // The run's first video packet an hour back, the rest going on where the run before ended.
        assert_eq!(tl.place(1, V, 10_000 * MS, 40 * MS), 3_602_000 * MS, "placed where the stream ended");
        assert_eq!(tl.place(1, V, 3_612_040 * MS, 40 * MS), 0, "the next decides again: the run goes on as it is");
        assert_eq!(tl.place(1, A, 3_612_000 * MS, 20 * MS), 0);
        let (v, _) = play(&mut tl, 1, 3_612_080, 1_000);
        assert!(continuous(&v, 40) && v[0] == 3_612_080, "{v:?}");
        // Settled: a packet off its stream now is the stream's own business.
        assert_eq!(tl.place(1, V, 10_000 * MS, 40 * MS), 0);
    }

    #[test]
    fn a_run_begun_by_a_straggler_of_the_run_before_is_shifted_by_the_packets_after_it() {
        let mut tl = Timeline::default();
        let _ = play(&mut tl, 0, 100_000, 2_000);
        // The run's first video packet still continues the run before; the rest jump 45 s back.
        assert_eq!(tl.place(1, V, 102_000 * MS, 40 * MS), 0);
        assert_eq!(tl.place(1, V, 57_040 * MS, 40 * MS), 44_960 * MS);
        let (v, a) = play(&mut tl, 1, 57_080, 1_000);
        assert_eq!((v[0], a[0]), (102_040, 102_040), "the run goes on from where the stream ended");
    }

    #[test]
    fn a_run_whose_packets_jump_again_keeps_the_shift_its_first_decided() {
        let mut tl = Timeline::default();
        let _ = play(&mut tl, 0, 100_000, 2_000);
        // The run begins 45 s back; then a burst of its audio from 30 s before that, then on as it began.
        assert_eq!(tl.place(1, A, 57_000 * MS, 20 * MS), 45_000 * MS);
        for t in (27_020..27_200).step_by(20) {
            assert_eq!(tl.place(1, A, t * MS, 20 * MS), 45_000 * MS, "the burst falls behind: dropped as stale");
        }
        let (v, a) = play(&mut tl, 1, 57_020, 1_000);
        assert_eq!((v[0], a[0]), (102_020, 102_020));
    }

    #[test]
    fn a_stream_new_in_a_run_is_measured_against_the_furthest_stream() {
        let mut tl = Timeline::default();
        for t in (0..2_000).step_by(40) {
            let _ = tl.place(0, V, (5_000 + t) * MS, 40 * MS);
        }
        // Audio appears only after the break, 30 s back.
        assert_eq!(tl.place(1, A, -25_000 * MS, 20 * MS), 32_000 * MS);
    }

    #[test]
    fn only_the_newest_runs_are_remembered() {
        let mut tl = Timeline::default();
        for run in 0..(RUNS_KEPT as u64 + 4) {
            // Every run starts again at 0: each is shifted a second further.
            let _ = play(&mut tl, run, 0, 1_000);
        }
        assert_eq!(tl.shifts.len(), RUNS_KEPT);
        assert_eq!(tl.shifts[0], (4, 4_000 * MS));
        assert_eq!(tl.shift_of(0), 4_000 * MS, "a forgotten run: the oldest remembered");
    }

    #[test]
    fn the_other_streams_first_packet_undoes_a_stray_that_began_a_run() {
        let mut tl = Timeline::default();
        let _ = play(&mut tl, 0, 3_610_000, 2_000);
        // The run's first video packet an hour back; its first audio packet, before the next video
        // one, goes on where the run before ended.
        assert_eq!(tl.place(1, V, 10_000 * MS, 40 * MS), 3_602_000 * MS);
        assert_eq!(tl.place(1, A, 3_612_000 * MS, 20 * MS), 0, "decided again: the audio is not an hour ahead");
        assert_eq!(tl.place(1, V, 3_612_040 * MS, 40 * MS), 0);
        let (v, a) = play(&mut tl, 1, 3_612_080, 1_000);
        assert_eq!((v[0], a[0]), (3_612_080, 3_612_080));

        // The other way round: the run's first audio PES 55013434 ms ahead, its frames going on
        // from each other there, then its video going on where the run before ended.
        let mut tl = Timeline::default();
        let _ = play(&mut tl, 0, 13_160_000, 2_000);
        for k in 0..5 {
            let _ = tl.place(1, A, (13_162_000 + 55_013_434 + 24 * k) * MS, 24 * MS);
        }
        assert_eq!(tl.place(1, V, 13_162_000 * MS, 40 * MS), 0, "the video is not 55013 s behind, dropped as stale");
        let (v, a) = play(&mut tl, 1, 13_162_040, 1_000);
        assert_eq!((v[0], a[0]), (13_162_040, 13_162_040));
    }

    #[test]
    fn the_other_streams_first_packet_shifts_a_run_a_straggler_began() {
        let mut tl = Timeline::default();
        let _ = play(&mut tl, 0, 100_000, 2_000);
        // The run's first audio packet still continues the run before; its video, and the rest of
        // it, 30 s on. Left 30 s ahead until the audio's next packet, the video held the lanes.
        assert_eq!(tl.place(1, A, 102_000 * MS, 20 * MS), 0);
        assert_eq!(tl.place(1, V, 132_000 * MS, 40 * MS), -30_000 * MS);
        assert_eq!(tl.place(1, V, 132_040 * MS, 40 * MS), -30_000 * MS);
        let (v, a) = play(&mut tl, 1, 132_080, 1_000);
        assert_eq!((v[0], a[0]), (102_080, 102_080));
    }

    #[test]
    fn a_straggler_of_the_other_stream_does_not_take_the_runs_jump() {
        let mut tl = Timeline::default();
        let _ = play(&mut tl, 0, 100_000, 2_000);
        // The run's audio jumps 45 s back; its first video packet still continues the run before,
        // the rest of its video jumps with the audio.
        assert_eq!(tl.place(1, A, 57_000 * MS, 20 * MS), 45_000 * MS);
        assert_eq!(tl.place(1, V, 102_000 * MS, 40 * MS), 0, "not 45 s ahead: where its stream was");
        let (v, a) = play(&mut tl, 1, 57_020, 1_000);
        assert!(v[0] == 102_000 && a[0] == 102_000 && continuous(&v, 40) && continuous(&a, 20), "{v:?} {a:?}");
    }

    #[test]
    fn a_bad_timestamp_early_in_a_run_is_undone_by_the_next_packet() {
        let mut tl = Timeline::default();
        let _ = play(&mut tl, 0, 13_160_000, 2_000);
        // The run goes on, begun by its audio; 100 ms in, an audio PES 55013434 ms ahead.
        assert_eq!(tl.place(1, A, 13_162_000 * MS, 20 * MS), 0);
        let _ = play(&mut tl, 1, 13_162_020, 80);
        for k in 0..5 {
            let _ = tl.place(1, A, (13_162_100 + 55_013_434 + 24 * k) * MS, 24 * MS);
        }
        let (v, a) = play(&mut tl, 1, 13_162_100, 2_000);
        assert!(v[0] == 13_162_100 && a[0] == 13_162_100 && continuous(&v, 40) && continuous(&a, 20), "{v:?}");
    }

    #[test]
    fn a_still_pictures_frames_go_on_across_runs() {
        let mut tl = Timeline::default();
        // A radio channel's still picture, a frame a second, its audio 300 ms ahead of it, and a
        // splice every 6 s whose timestamps go on, each run begun by a video frame.
        for run in 0..5 {
            let from = 10_000 + 6_000 * run as i64;
            for t in (from..from + 6_000).step_by(20) {
                if (t - from) % 1_000 == 0 {
                    assert_eq!(tl.place(run, V, t * MS, 40 * MS), 0, "run {run}: the video at {t} ms");
                }
                assert_eq!(tl.place(run, A, (t + 300) * MS, 20 * MS), 0, "run {run}: the audio at {t} ms");
            }
        }
        assert_eq!(tl.place(5, V, -5_000 * MS, 40 * MS), 45_300 * MS, "a jump is still one");

        // With no audio, its frames 2 s apart and of no known duration.
        let mut tl = Timeline::default();
        for run in 0..5 {
            for t in (0..10_000).step_by(2_000) {
                assert_eq!(tl.place(run, V, (10_000 + 10_000 * run as i64 + t) * MS, 0), 0, "run {run}");
            }
        }
    }
}
