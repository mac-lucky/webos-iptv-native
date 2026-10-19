//! The software decode -> PCM path through the FFmpeg the app SHIPS, built for this machine with
//! the same component list (`cargo xtask ffmpeg --host`, which stages it in `pkg/`;
//! `WEBOSIPTV_FFMPEG_DIR` names another directory). The tests skip, loudly, when it is not staged,
//! and the demux tests also without the `ffmpeg` CLI that writes their fixtures. Run alone with
//! `cargo test -p tv_media --lib ff::adec_tests -- --nocapture`.

use super::adec::{self, PcmFeed};
use super::test_support::{bind_host_ffmpeg, host_ffmpeg_dir, HostFfmpeg};
use super::late_audio_tests::{tool_present, FileServer, Scratch};
use super::*;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The host FFmpeg and the decoder bound, the path detected; undone however the test leaves.
struct Bound {
    _ffmpeg: HostFfmpeg,
}

impl Bound {
    /// `None`, after a SKIP line, when no host FFmpeg is staged.
    fn new() -> Option<Bound> {
        let dir = host_ffmpeg_dir();
        if !avcodec::CANDIDATES.iter().any(|n| dir.join(n).is_file()) {
            eprintln!("SKIP: no host FFmpeg in {} - run `cargo xtask ffmpeg --host`", dir.display());
            return None;
        }
        let ffmpeg = bind_host_ffmpeg();
        assert!(adec::load_from(&dir), "the decoder did not bind from {}", dir.display());
        adec::AVAILABLE.store(true, Ordering::Release);
        Some(Bound { _ffmpeg: ffmpeg })
    }
}

impl Drop for Bound {
    fn drop(&mut self) {
        adec::AVAILABLE.store(false, Ordering::Release);
    }
}

/// Every codec the feed decision sends down this path has a decoder in the bundled build: a
/// codec missing from `xtask/src/ffmpeg.rs` would be declared PCM and then fail to open.
#[test]
fn the_bundled_build_decodes_every_soft_codec() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = Bound::new() else { return };
    let missing: Vec<_> = adec::SOFT_CODECS.iter().filter(|c| !adec::decoder_present(c)).collect();
    assert!(missing.is_empty(), "no decoder for {missing:?}");
    assert!(!adec::decoder_present("cook"), "and the check is not vacuous");
}

/// The 440 Hz test tone (1/8 of full scale, -21.1 dBFS RMS) in both channels of a stereo layout,
/// as a mono track, and in all six channels of a 5.1 layout (LFE included; the encoders low-pass
/// it away).
const STEREO: &str = "pan=stereo|c0=c0|c1=c0";
const MONO: &str = "aformat=channel_layouts=mono";
const SURROUND: &str = "pan=5.1|c0=c0|c1=c0|c2=c0|c3=c0|c4=c0|c5=c0";
/// The tone's RMS in S16. Stereo plays it as it is; swr's 5.1 downmix (FL + -3 dB C + -3 dB SL,
/// LFE dropped, normalized so it cannot clip) adds three copies back to the same level; mono is
/// upmixed at -3 dB a side.
const TONE_RMS: f64 = 4096.0 * std::f64::consts::FRAC_1_SQRT_2;

/// Write a 2 s file (its container from `name`'s extension): 640x360 H.264 at 25 fps plus one
/// audio track, the tone shaped by the audio filter `af` and encoded by `audio` (`-c:a` and its
/// options). `extra` goes before the output (muxer options).
fn generate(dir: &Path, name: &str, af: &str, audio: &[&str], extra: &[&str]) -> PathBuf {
    let out = dir.join(name);
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=s=640x360:r=25:d=2"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=2"])
        .args(["-map", "0:v", "-map", "1:a"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "25"])
        .args(["-af", af])
        .args(audio)
        .args(["-strict", "-2"])
        .args(extra)
        .arg(&out)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg could not write {name}");
    out
}

/// Does the `ffmpeg` command line tool carry encoder `name` (AudioToolbox's `aac_at` is macOS
/// only)?
fn encoder_present(name: &str) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.split_whitespace().nth(1) == Some(name)))
}

/// `ff::demux` with `channels` of PCM decoding `file`: the audio AUs it pushed, and the first video
/// pts (an MPEG-TS starts at its muxer's offset, not at 0).
fn demux_pcm(file: &Path, channels: u32, acodec: &str) -> (Vec<(Vec<u8>, i64)>, i64) {
    let server = FileServer::start(file);
    SHARED.seek_to_ns.store(-1, Ordering::Relaxed);
    SHARED.desired_audio_idx.store(-1, Ordering::Relaxed);
    SHARED.demux_failed.store(false, Ordering::Relaxed);
    SHARED.demux_io_failed.store(false, Ordering::Relaxed);
    FEED_AUDIO.store(true, Ordering::Relaxed);
    set_soft_pcm(channels);
    let (video_cap, audio_cap) = crate::player::engine::aq_caps();
    let mut video = crate::aq::aq_new(video_cap as c_long);
    let mut audio = crate::aq::aq_new(audio_cap as c_long);
    let mut hs = tv_net::stream::http_stream_boxed();
    let (vp, ap, hp) = (
        SendPtr(&mut *video as *mut AuQueue),
        SendPtr(&mut *audio as *mut AuQueue),
        SendPtr(&mut *hs as *mut HttpStream),
    );
    let name = file.file_name().unwrap().to_string_lossy().into_owned();
    let url = format!("http://127.0.0.1:{}/media/{name}", server.port);
    let acodec = acodec.to_string();
    let th = std::thread::spawn(move || {
        let off = tv_base::task::OffFrame::for_test();
        demux(&off, url, crate::headers::RequestHeaders::none(), acodec, vp, ap, hp)
    });
    let pop_all = |q: &mut AuQueue, out: &mut Vec<(Vec<u8>, i64)>| loop {
        let node = crate::aq::aq_pop(q, std::ptr::null_mut());
        if node.is_null() {
            break;
        }
        let (_es, _key, pts, len, data) = unsafe { crate::aq::au_fields(node) };
        out.push((unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec(), pts));
        unsafe { libc::free(node as *mut c_void) };
    };
    let (mut aus, mut vids) = (Vec::new(), Vec::new());
    let deadline = Instant::now() + Duration::from_secs(20);
    while !th.is_finished() {
        assert!(Instant::now() < deadline, "the demuxer did not finish a 2 s file");
        pop_all(&mut audio, &mut aus);
        pop_all(&mut video, &mut vids);
        std::thread::sleep(Duration::from_millis(5));
    }
    th.join().unwrap();
    pop_all(&mut audio, &mut aus);
    pop_all(&mut video, &mut vids);
    set_soft_pcm(0);
    assert!(!SHARED.demux_failed.load(Ordering::Acquire), "the software audio demux must not fail");
    assert!(vids.len() >= 45, "the video lane carries the video ({} AUs)", vids.len());
    let video_start = vids.iter().map(|v| v.1).min().unwrap();
    (aus, video_start)
}

/// Stereo PCM AUs: whole frames, >= 20 ms but the last, pts advancing by exactly their sample
/// count from the track start (`start_ns`, where the video starts too, give or take the encoder's
/// priming: within one and a half codec frames of `frame` samples), and `samples` of them in all.
fn assert_continuous_pcm(aus: &[(Vec<u8>, i64)], start_ns: i64, frame: i64, samples: RangeInclusive<i64>) {
    let bytes = adec::PCM_CHANNELS as usize * 2;
    let mut count = 0i64;
    for (i, (data, pts)) in aus.iter().enumerate() {
        assert_eq!(data.len() % bytes, 0, "AU {i}: whole sample frames");
        if i + 1 < aus.len() {
            let batch = i64::from(adec::PCM_RATE * adec::PCM_BATCH_MS / 1000) as usize;
            assert!(data.len() >= batch * bytes, "AU {i}: >= {} ms ({} bytes)", adec::PCM_BATCH_MS, data.len());
        }
        let want = aus[0].1 + count * 1_000_000_000 / 48_000;
        assert!((pts - want).abs() <= 1, "AU {i}: pts {pts} vs sample count {want}");
        count += (data.len() / bytes) as i64;
    }
    eprintln!("{count} samples from {} ns (video from {start_ns} ns)", aus[0].1);
    let slack = frame * 1_500_000_000 / 48_000;
    assert!((aus[0].1 - start_ns).abs() < slack, "the first AU sits at the track start ({} ns vs {start_ns})", aus[0].1);
    assert!(samples.contains(&count), "{samples:?} samples at 48 kHz, got {count}");
    assert!(aus.iter().any(|(d, _)| d.iter().any(|&b| b != 0)), "not silence");
}

/// `secs` of the track at 48 kHz, give or take the encoder's priming and the last frame's padding,
/// which a decoder outputs when the container does not trim them (MPEG-TS): at most four frames
/// of `frame` samples per encode (AudioToolbox's HE-AAC takes three), `encodes` of them back to
/// back.
fn duration(secs: i64, frame: i64, encodes: i64) -> RangeInclusive<i64> {
    secs * 48_000 - 1_000..=secs * 48_000 + 4 * frame * encodes
}

/// The RMS of each channel of interleaved S16 stereo, over the middle of the track (the codecs'
/// start-up and the last frame's padding left out).
fn rms(aus: &[(Vec<u8>, i64)]) -> [f64; 2] {
    let pcm: Vec<i16> = aus.iter().flat_map(|(d, _)| d.as_chunks::<2>().0.iter().map(|&b| i16::from_le_bytes(b))).collect();
    let frames = pcm.as_chunks::<2>().0;
    let middle = &frames[frames.len() / 10..frames.len() * 9 / 10];
    let mut sum = [0f64; 2];
    for frame in middle {
        for (s, &v) in sum.iter_mut().zip(frame) {
            *s += f64::from(v) * f64::from(v);
        }
    }
    sum.map(|s| (s / middle.len() as f64).sqrt())
}

/// Both channels carry the tone at `want` RMS, within 1.5 dB (lossy coding moves the level a
/// little; a wrong matrix, a lost channel or a clipped downmix moves it by far more), and no
/// sample is pinned at full scale.
fn assert_level(aus: &[(Vec<u8>, i64)], want: f64) {
    let got = rms(aus);
    eprintln!("RMS L {:.0} R {:.0} (want {want:.0})", got[0], got[1]);
    for (ch, g) in ["left", "right"].iter().zip(got) {
        let db = 20.0 * (g / want).log10();
        assert!(db.abs() <= 1.5, "{ch} channel at {g:.0} RMS, {db:+.1} dB from {want:.0}");
    }
    let clipped = aus.iter().flat_map(|(d, _)| d.as_chunks::<2>().0).filter(|&&b| i16::from_le_bytes(b).unsigned_abs() >= 32_767).count();
    assert_eq!(clipped, 0, "no sample clips");
}

/// The fixture `name` (from `af` and `audio`, plus muxer options `extra`), demuxed with the
/// session's PCM decision on its `acodec` track: continuous stereo 48 kHz PCM at `level` RMS.
fn soft_decode(name: &str, af: &str, audio: &[&str], extra: &[&str], acodec: &str, frame: i64, level: f64) {
    let scratch = Scratch::new(&format!("adec-{name}"));
    let file = generate(&scratch.0, name, af, audio, extra);
    let (aus, start_ns) = demux_pcm(&file, 2, acodec);
    assert_continuous_pcm(&aus, start_ns, frame, duration(2, frame, 1));
    assert_level(&aus, level);
}

/// The ffmpeg CLI and the host FFmpeg both there, else a SKIP line.
fn fixtures() -> Option<Bound> {
    if !tool_present("ffmpeg") {
        eprintln!("SKIP: the ffmpeg command line tool is not installed");
        return None;
    }
    Bound::new()
}

/// **A DVB restream's audio, end to end:** MPEG-TS with H.264 + MP2 stereo, the audio the C1's
/// buffer feed does not take as `"MP3"`. `ff::demux` with the session's PCM decision feeds S16LE
/// stereo 48 kHz, sample-counted and continuous, for the whole track.
#[test]
fn demux_feeds_an_mp2_track_as_stereo_pcm() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = fixtures() else { return };
    soft_decode("mp2.ts", STEREO, &["-c:a", "mp2"], &[], "mp2", 1152, TONE_RMS);
}

/// **IPTV's usual audio:** AAC-LC in MPEG-TS, ADTS-framed in the packets with no extradata. The
/// decoder reads the ADTS headers; the native path's ADTS reframing never touches this lane.
#[test]
fn demux_feeds_adts_aac_in_mpegts_as_stereo_pcm() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = fixtures() else { return };
    soft_decode("aac.ts", STEREO, &["-c:a", "aac"], &[], "aac", 1024, TONE_RMS);
}

/// AAC in LATM/LOAS framing (MPEG-TS stream type 0x11, DVB's choice for HE-AAC): its own
/// decoder, configured in band.
#[test]
fn demux_feeds_latm_aac_as_stereo_pcm() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = fixtures() else { return };
    soft_decode("latm.ts", STEREO, &["-c:a", "aac"], &["-mpegts_flags", "latm"], "aac_latm", 1024, TONE_RMS);
}

/// Raw AAC from MP4 (fMP4 HLS carries it the same way): no ADTS, the decoder is configured from
/// the codecpar's AudioSpecificConfig, the priming trimmed by the container. Mono, upmixed.
#[test]
fn demux_feeds_raw_mono_aac_from_mp4_as_stereo_pcm() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = fixtures() else { return };
    soft_decode("aac.mp4", MONO, &["-c:a", "aac"], &[], "aac", 1024, TONE_RMS * std::f64::consts::FRAC_1_SQRT_2);
}

/// **A DVB film channel:** AC-3 5.1, downmixed to stereo at the tone's level, not clipped.
#[test]
fn demux_downmixes_ac3_5_1_to_stereo_pcm() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = fixtures() else { return };
    soft_decode("ac3.ts", SURROUND, &["-c:a", "ac3"], &[], "ac3", 1536, TONE_RMS);
}

/// E-AC-3 5.1, downmixed the same way.
#[test]
fn demux_downmixes_eac3_5_1_to_stereo_pcm() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = fixtures() else { return };
    soft_decode("eac3.ts", SURROUND, &["-c:a", "eac3"], &[], "eac3", 1536, TONE_RMS);
}

/// HE-AAC v2 in ADTS: the headers say 24 kHz mono, and only the first decoded frame finds SBR
/// (48 kHz) and PS (stereo). Needs AudioToolbox's encoder (macOS); skipped elsewhere.
#[test]
fn demux_feeds_he_aac_v2_as_stereo_pcm() {
    let _g = tv_base::testlock::serial();
    if !encoder_present("aac_at") {
        eprintln!("SKIP: the ffmpeg command line tool has no aac_at encoder (HE-AAC v2)");
        return;
    }
    let Some(_b) = fixtures() else { return };
    soft_decode("heaac.ts", STEREO, &["-c:a", "aac_at", "-profile:a", "28"], &[], "aac", 2048, TONE_RMS);
}

/// **An HE-AAC v2 track in MPEG-TS is declared as its ADTS header says** - the 24 kHz core rate
/// and one channel - not as a decoded frame comes out (48 kHz stereo, SBR and PS applied): the
/// television decodes those frames itself on a set whose table lists AAC, and its `aacInfo` must
/// match them.
#[test]
fn the_probe_declares_he_aac_by_its_adts_header() {
    let _g = tv_base::testlock::serial();
    if !encoder_present("aac_at") {
        eprintln!("SKIP: the ffmpeg command line tool has no aac_at encoder (HE-AAC v2)");
        return;
    }
    let Some(_b) = fixtures() else { return };
    let scratch = Scratch::new("adec-heaac-probe");
    let file = generate(&scratch.0, "heaac.ts", STEREO, &["-c:a", "aac_at", "-profile:a", "28"], &[]);
    let server = FileServer::start(&file);
    let url = format!("http://127.0.0.1:{}/media/heaac.ts", server.port);
    let io = super::probe::ProbeIo::new();
    let probed = super::probe::probe(&url, &crate::headers::RequestHeaders::none(), &io).expect("probe");
    let a = &probed.audio[0];
    assert_eq!((a.codec.as_str(), a.sample_rate, a.channels), ("aac", 24_000, 1));
}

/// **A broadcast switching shape mid-stream**: 2 s of stereo AAC at 44.1 kHz, then 2 s of 5.1 AAC
/// at 48 kHz in the same PID (two MPEG-TS files back to back, the second 2 s later). The
/// resampler follows each change, and the PCM stays continuous and at the tone's level across it.
#[test]
fn demux_follows_a_layout_and_rate_change_mid_stream() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = fixtures() else { return };
    let scratch = Scratch::new("adec-switch");
    let a = generate(&scratch.0, "a.ts", &format!("{STEREO},aresample=44100"), &["-c:a", "aac"], &[]);
    let b = generate(&scratch.0, "b.ts", SURROUND, &["-c:a", "aac"], &["-output_ts_offset", "2"]);
    let file = scratch.0.join("switch.ts");
    let mut both = std::fs::read(a).unwrap();
    both.extend(std::fs::read(b).unwrap());
    std::fs::write(&file, both).unwrap();
    let (aus, start_ns) = demux_pcm(&file, 2, "aac");
    assert_continuous_pcm(&aus, start_ns, 1024, duration(4, 1024, 2));
    assert_level(&aus, TONE_RMS);
}

/// One ADTS frame whose payload is as many of `adts`'s raw AAC blocks as fit its 13-bit length:
/// the decoder reads every block of a packet as another frame, so a silent 4-byte block becomes
/// 1024 samples.
fn many_blocks_in_one_adts_frame(adts: &[u8]) -> Vec<u8> {
    let mut frames = Vec::new();
    let mut i = 0;
    while i + 7 <= adts.len() {
        assert!(adts[i] == 0xFF && adts[i + 1] & 0xF6 == 0xF0, "an ADTS sync word at {i}");
        let header = if adts[i + 1] & 1 == 1 { 7 } else { 9 };
        let len = (usize::from(adts[i + 3] & 3) << 11) | (usize::from(adts[i + 4]) << 3) | usize::from(adts[i + 5] >> 5);
        frames.push((&adts[i..i + 7], &adts[i + header..i + len]));
        i += len;
    }
    let mut payload = Vec::new();
    for (_, block) in &frames {
        if 7 + payload.len() + block.len() > 8191 {
            break;
        }
        payload.extend_from_slice(block);
    }
    let len = 7 + payload.len();
    let mut out = frames[0].0.to_vec();
    out[1] |= 1; // no CRC
    out[3] = (out[3] & 0xFC) | ((len >> 11) & 3) as u8;
    out[4] = (len >> 3) as u8;
    out[5] = (out[5] & 0x1F) | ((len & 7) << 5) as u8;
    out.extend(payload);
    out
}

/// **A hostile packet is bounded.** An ADTS frame packed with a minute of silent AAC blocks at
/// 7.35 kHz decodes to tens of megabytes of 48 kHz PCM; each packet yields at most a second of it
/// and the rest of the packet is dropped, so a playlist entry cannot run the TV out of memory.
#[test]
fn a_packet_that_decodes_to_minutes_of_pcm_is_cut_at_a_second() {
    let _g = tv_base::testlock::serial();
    let Some(_b) = fixtures() else { return };
    let scratch = Scratch::new("adec-hostile");
    let silence = scratch.0.join("silence.aac");
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "anullsrc=r=7350:cl=mono", "-t", "60"])
        .args(["-c:a", "aac", "-f", "adts"])
        .arg(&silence)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg could not write the silence");
    let crafted = scratch.0.join("crafted.aac");
    std::fs::write(&crafted, many_blocks_in_one_adts_frame(&std::fs::read(&silence).unwrap()).repeat(4)).unwrap();
    let second = (adec::PCM_RATE * adec::PCM_CHANNELS * 2) as usize;
    unsafe {
        let mut fmt: *mut AVFormatContext = std::ptr::null_mut();
        let c = CString::new(crafted.to_string_lossy().into_owned()).unwrap();
        assert!(avformat_open_input(&mut fmt, c.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut()) >= 0);
        assert!(avformat_find_stream_info(fmt, std::ptr::null_mut()) >= 0);
        let st = *(*fmt).streams;
        let mut feed = PcmFeed::open(stream_codecpar(st), stream_time_base(st), 2).expect("open the decoder");
        let pkt = av_packet_alloc();
        let (mut packets, mut total) = (0, 0usize);
        while av_read_frame(fmt, pkt) >= 0 {
            packets += 1;
            if let Ok(out) = feed.push(pkt, 0) {
                let n: usize = out.iter().map(|a| a.data.len()).sum();
                assert!(n <= second, "packet {packets} yielded {n} bytes of PCM");
                total += n;
            }
            av_packet_unref(pkt);
        }
        let mut pkt_m = pkt;
        av_packet_free(&mut pkt_m);
        avformat_close_input(&mut fmt);
        assert!(packets >= 4, "the demuxer read the crafted frames ({packets})");
        assert!(total <= packets * second, "{total} bytes from {packets} packets");
    }
}

/// **The CPU budget, measured through the shipping path** (the bundled FFmpeg): decode +
/// resample cost per second of audio, single-threaded, for each file in `WEBOSIPTV_ADEC_BENCH` (a
/// `:`-separated list of `path=channels`). Prints, asserts nothing but that it decoded.
#[test]
#[ignore = "a measurement: set WEBOSIPTV_ADEC_BENCH and run with --nocapture"]
fn soft_decode_cost_per_second_of_audio() {
    let _g = tv_base::testlock::serial();
    let Ok(list) = std::env::var("WEBOSIPTV_ADEC_BENCH") else {
        eprintln!("SKIP: WEBOSIPTV_ADEC_BENCH not set");
        return;
    };
    let Some(_b) = Bound::new() else { return };
    for item in list.split(':') {
        let (path, ch) = item.rsplit_once('=').expect("path=channels");
        let ch: u32 = ch.parse().unwrap();
        unsafe {
            let mut fmt: *mut AVFormatContext = std::ptr::null_mut();
            let c = CString::new(path).unwrap();
            assert!(avformat_open_input(&mut fmt, c.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut()) >= 0, "open {path}");
            assert!(avformat_find_stream_info(fmt, std::ptr::null_mut()) >= 0);
            let ai = av_find_best_stream(fmt, AVMEDIA_TYPE_AUDIO, -1, -1, std::ptr::null_mut(), 0);
            assert!(ai >= 0);
            let st = *(*fmt).streams.add(ai as usize);
            let mut feed = PcmFeed::open(stream_codecpar(st), stream_time_base(st), ch).expect("open the decoder");
            let pkt = av_packet_alloc();
            let (mut busy, mut bytes) = (Duration::ZERO, 0usize);
            while av_read_frame(fmt, pkt) >= 0 {
                if (*pkt).stream_index == ai {
                    let t = Instant::now();
                    let out = feed.push(pkt, 0).expect("decode");
                    busy += t.elapsed();
                    bytes += out.iter().map(|a| a.data.len()).sum::<usize>();
                }
                av_packet_unref(pkt);
            }
            let mut pkt_m = pkt;
            av_packet_free(&mut pkt_m);
            avformat_close_input(&mut fmt);
            let secs = bytes as f64 / (48_000.0 * f64::from(feed.channels) * 2.0);
            assert!(secs > 1.0, "{path}: decoded only {secs:.2} s");
            eprintln!(
                "BENCH {path} -> PCM {}ch: {:.3} ms CPU per s of audio ({:.0}x realtime) over {secs:.0} s",
                feed.channels,
                busy.as_secs_f64() * 1000.0 / secs,
                secs / busy.as_secs_f64()
            );
        }
    }
}
