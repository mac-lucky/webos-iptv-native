//! Channel logos for every screen ([`Logos`]): the first draw of a channel asks for its logo, a
//! worker fetches and decodes it, and the main thread uploads it as a texture that stays for the
//! session. A logo reads as `None` while it loads, after it failed, and without a transport.
//!
//! The worker reads tv_platform's image cache first (a fresh copy needs no network) and otherwise
//! fetches through `iptv::refresh::fetch`, the playlist's own policy (http and https, redirect
//! limit, no downgrade, byte cap, time budget), over whatever transport the app has, keeping what
//! decoded in the cache; a stale copy stands in when the fetch fails. It takes the newest request
//! first, so the rows on screen load before the ones scrolled past, and a request no frame has
//! drawn for [`STALE_FRAMES`], or one pushed out past [`MAX_QUEUED`], is dropped: a fast scroll
//! never leaves a long queue of logos nobody sees. A wake hook ([`LogoCache::set_wake`]) tells
//! the main loop each time one is ready.
//! Pictures are scaled down to fit the largest box a screen draws a logo in, and at most
//! [`RESIDENT`] textures are kept: the least recently drawn goes first.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use iptv::model::Channel;
use iptv::refresh::{self, Fetched, Limits, Transport, Validators};
use tv_platform::imgcache;
use tv_screens::data::Logos;

use crate::logs::log;

/// The largest logo box (the player's info panel); bigger pictures are scaled down to fit it.
const BOX_W: u32 = 240;
const BOX_H: u32 = 165;
/// Logo textures kept at once, about 10 MB at the box size.
const RESIDENT: usize = 64;
/// Requests waiting for the worker: about two screens of rows.
const MAX_QUEUED: usize = 24;
/// A waiting request no frame drew for this long is dropped (half a second of scrolling).
const STALE_FRAMES: u64 = 30;
/// The largest picture decoded, as RGBA bytes (2 Mpx): the general decoder allows far more, and
/// its RGBA conversion is not counted against its own budget, so a logo is measured first.
const MAX_RGBA: u64 = 8 << 20;

/// Whether logos go through the shared disk cache; host tests keep off the machine's.
const DISK: bool = !cfg!(test);
/// The largest logo file downloaded; the transport's own body cap should match it.
pub const MAX_BYTES: u64 = 2 << 20;
/// One logo download.
const LIMITS: Limits = Limits {
    max_bytes: MAX_BYTES,
    max_redirects: 3,
    total: Duration::from_secs(20),
};

/// Decoded RGBA, straight alpha.
pub struct Pixels {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

enum Slot {
    /// Queued or with the worker; `drawn` is the last frame that asked for it.
    Loading {
        drawn: u64,
    },
    Failed,
    Ready {
        tex: u32,
        w: f32,
        h: f32,
        drawn: u64,
    },
}

/// Called on the worker's thread each time a logo is ready.
type Wake = Box<dyn Fn() + Send>;

/// The worker's requests, newest last.
#[derive(Default)]
struct Queue {
    wanted: Mutex<(Vec<String>, bool)>,
    wake: Condvar,
    /// Ends the download in flight.
    cancel: Arc<AtomicBool>,
    /// Wakes the main loop.
    ready: Mutex<Option<Wake>>,
}

struct Worker {
    queue: Arc<Queue>,
    done: Receiver<(String, Option<Pixels>)>,
    thread: Option<JoinHandle<()>>,
}

pub struct LogoCache {
    slots: RefCell<HashMap<String, Slot>>,
    loading: Cell<usize>,
    /// Advanced once per drawn frame; a texture remembers the frame it was last drawn in.
    frame: Cell<u64>,
    worker: Option<Worker>,
}

impl LogoCache {
    /// A cache that loads through `transport`; without one every logo stays `None`.
    pub fn new(transport: Option<Box<dyn Transport>>) -> Self {
        let worker = transport.and_then(|transport| {
            let queue = Arc::new(Queue::default());
            let (tx, done) = mpsc::channel();
            let q = Arc::clone(&queue);
            let thread = tv_base::task::spawn("logos", move || work(&q, &*transport, &tx))?;
            Some(Worker {
                queue,
                done,
                thread: Some(thread),
            })
        });
        LogoCache {
            slots: RefCell::new(HashMap::new()),
            loading: Cell::new(0),
            frame: Cell::new(0),
            worker,
        }
    }

    /// Logos are on their way; the wake hook ([`set_wake`](Self::set_wake)) says when one is
    /// ready for [`pump`](Self::pump).
    fn loading(&self) -> bool {
        self.loading.get() > 0
    }

    /// Call `wake` (on the worker's thread) each time a logo is ready for [`pump`](Self::pump),
    /// so the main loop need not poll while logos load.
    pub fn set_wake(&self, wake: impl Fn() + Send + 'static) {
        if let Some(worker) = &self.worker {
            *worker.queue.ready.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(wake));
        }
    }

    /// A frame is being drawn: requests no frame drew for [`STALE_FRAMES`] are dropped.
    pub fn next_frame(&self) {
        let frame = self.frame.get() + 1;
        self.frame.set(frame);
        let Some(worker) = self.worker.as_ref().filter(|_| self.loading()) else {
            return;
        };
        let mut slots = self.slots.borrow_mut();
        let mut wanted = worker
            .queue
            .wanted
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        wanted.0.retain(|url| {
            let fresh = matches!(slots.get(url), Some(Slot::Loading { drawn }) if frame - drawn <= STALE_FRAMES);
            if !fresh {
                slots.remove(url);
                self.loading.set(self.loading.get() - 1);
            }
            fresh
        });
    }

    /// Take what the worker finished: `upload` makes a texture (0 when it cannot), `free` drops
    /// one evicted to stay within [`RESIDENT`]. Returns whether a logo became drawable.
    pub fn pump(&self, upload: &mut dyn FnMut(&Pixels) -> u32, free: &mut dyn FnMut(u32)) -> bool {
        let Some(worker) = &self.worker else {
            return false;
        };
        let mut ready = false;
        let mut slots = self.slots.borrow_mut();
        while let Ok((url, pixels)) = worker.done.try_recv() {
            self.loading.set(self.loading.get().saturating_sub(1));
            let slot = match pixels.map(|px| (upload(&px), px.w, px.h)) {
                Some((tex @ 1.., w, h)) => {
                    ready = true;
                    Slot::Ready {
                        tex,
                        w: w as f32,
                        h: h as f32,
                        drawn: self.frame.get(),
                    }
                }
                _ => Slot::Failed,
            };
            slots.insert(url, slot);
        }
        while ready && resident(&slots) > RESIDENT {
            let oldest = slots
                .iter()
                .filter_map(|(url, s)| match s {
                    Slot::Ready { tex, drawn, .. } => Some((*drawn, *tex, url)),
                    _ => None,
                })
                .min();
            let Some((_, tex, url)) = oldest else { break };
            let url = url.clone();
            slots.remove(&url);
            free(tex);
        }
        ready
    }
}

fn resident(slots: &HashMap<String, Slot>) -> usize {
    slots
        .values()
        .filter(|s| matches!(s, Slot::Ready { .. }))
        .count()
}

impl Logos for LogoCache {
    fn logo(&self, channel: &Channel) -> Option<(u32, f32, f32)> {
        let url = channel.logo.as_deref().filter(|u| !u.is_empty())?;
        let worker = self.worker.as_ref()?;
        let mut slots = self.slots.borrow_mut();
        let frame = self.frame.get();
        match slots.get_mut(url) {
            Some(Slot::Ready { tex, w, h, drawn }) => {
                *drawn = frame;
                Some((*tex, *w, *h))
            }
            Some(Slot::Loading { drawn }) => {
                *drawn = frame;
                None
            }
            Some(Slot::Failed) => None,
            None => {
                slots.insert(url.to_owned(), Slot::Loading { drawn: frame });
                self.loading.set(self.loading.get() + 1);
                let mut wanted = worker
                    .queue
                    .wanted
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                wanted.0.push(url.to_owned());
                if wanted.0.len() > MAX_QUEUED {
                    // The oldest request: a row scrolled past longest ago.
                    slots.remove(&wanted.0.remove(0));
                    self.loading.set(self.loading.get() - 1);
                }
                drop(wanted);
                worker.queue.wake.notify_one();
                None
            }
        }
    }
}

impl Drop for LogoCache {
    fn drop(&mut self) {
        let Some(worker) = &mut self.worker else {
            return;
        };
        worker
            .queue
            .wanted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1 = true;
        worker.queue.cancel.store(true, Ordering::Release);
        worker.queue.wake.notify_one();
        if let Some(thread) = worker.thread.take() {
            tv_base::task::join("logos", thread);
        }
    }
}

/// The worker: newest request first, until the cache is dropped or stops listening.
fn work(queue: &Queue, transport: &dyn Transport, done: &Sender<(String, Option<Pixels>)>) {
    loop {
        let url = {
            let mut wanted = queue.wanted.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if wanted.1 {
                    return;
                }
                if let Some(url) = wanted.0.pop() {
                    break url;
                }
                wanted = queue.wake.wait(wanted).unwrap_or_else(|e| e.into_inner());
            }
        };
        let pixels = load(transport, &url, &queue.cancel);
        if done.send((url, pixels)).is_err() {
            return;
        }
        if let Some(wake) = &*queue.ready.lock().unwrap_or_else(|e| e.into_inner()) {
            wake();
        }
    }
}

/// The logo at `url`: the cached file while fresh, else a download (kept once it decodes), else
/// a stale cached file.
fn load(transport: &dyn Transport, url: &str, cancel: &Arc<AtomicBool>) -> Option<Pixels> {
    // The cache keeps the downloaded file, under the box it is shown in.
    let key = imgcache::classify_pixels(url, BOX_W, BOX_H).filter(|_| DISK);
    let generation = imgcache::generation();
    let cached = key.as_ref().and_then(|k| imgcache::read_at(generation, k));
    if let Some(fresh) = cached.as_ref().filter(|c| !c.stale) {
        return decode(&fresh.bytes);
    }
    if let Some(bytes) = download(transport, url, cancel) {
        if let Some(px) = decode(&bytes) {
            if let Some(k) = &key {
                imgcache::write_at(generation, k, &bytes);
            }
            return Some(px);
        }
    }
    decode(&cached?.bytes)
}

fn download(transport: &dyn Transport, url: &str, cancel: &Arc<AtomicBool>) -> Option<Vec<u8>> {
    let fetched = refresh::fetch(
        transport,
        url,
        &Validators::default(),
        &LIMITS,
        cancel,
        |body| {
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            Ok(bytes)
        },
    );
    match fetched {
        Ok(Fetched::New(bytes, _)) => Some(bytes),
        Ok(Fetched::NotModified) => None,
        Err(e) => {
            log(&format!("logos: {}: {e}", iptv::redact::url(url)));
            None
        }
    }
}

/// A PNG, JPEG, GIF or WebP logo within [`MAX_RGBA`], decoded and scaled into the box; anything
/// else is refused before the decoder sees it.
fn decode(bytes: &[u8]) -> Option<Pixels> {
    let (w, h) = dimensions(bytes)?;
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) * 4 > MAX_RGBA {
        log(&format!("logos: {w}x{h} refused, over the logo budget"));
        return None;
    }
    let (w, h, rgba) = tv_gfx::img::img_decode_owned(bytes)?;
    Some(fit(Pixels { w, h, rgba }))
}

/// The size a PNG, JPEG, GIF or WebP header declares; `None` for anything else.
fn dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| {
        Some(u32::from(u16::from_be_bytes(
            b.get(i..i + 2)?.try_into().ok()?,
        )))
    };
    let le16 = |i: usize| {
        Some(u32::from(u16::from_le_bytes(
            b.get(i..i + 2)?.try_into().ok()?,
        )))
    };
    let le24 = |i: usize| {
        let x = b.get(i..i + 3)?;
        Some(u32::from(x[0]) | u32::from(x[1]) << 8 | u32::from(x[2]) << 16)
    };
    if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16) == Some(b"IHDR") {
        let be32 = |i: usize| Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?));
        return Some((be32(16)?, be32(20)?));
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some((le16(6)?, le16(8)?));
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return match b.get(12..16)? {
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            b"VP8L" => {
                let bits = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
                Some((1 + (bits & 0x3fff), 1 + (bits >> 14 & 0x3fff)))
            }
            b"VP8X" => Some((1 + le24(24)?, 1 + le24(27)?)),
            _ => None,
        };
    }
    if !b.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    // JPEG: walk the segments to the frame header (SOF0-15 but DHT, JPG and DAC).
    let mut i = 2;
    while *b.get(i)? == 0xFF {
        let marker = *b.get(i + 1)?;
        match marker {
            0xFF => i += 1,
            0x01 | 0xD0..=0xD7 => i += 2,
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                return Some((be16(i + 7)?, be16(i + 5)?));
            }
            _ => i += 2 + usize::try_from(be16(i + 2)?).ok()?,
        }
    }
    None
}

/// `px` scaled down to fit the logo box, each output pixel the alpha-weighted mean of the source
/// pixels under it; pictures that already fit are kept as they are.
fn fit(px: Pixels) -> Pixels {
    let scale = (BOX_W as f32 / px.w as f32).min(BOX_H as f32 / px.h as f32);
    if scale >= 1.0 {
        return px;
    }
    let w = ((px.w as f32 * scale).round() as u32).max(1);
    let h = ((px.h as f32 * scale).round() as u32).max(1);
    let span = |i: u32, n: u32, of: u32| (i * of / n, ((i + 1) * of / n).max(i * of / n + 1));
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let (y0, y1) = span(y, h, px.h);
        for x in 0..w {
            let (x0, x1) = span(x, w, px.w);
            let mut sum = [0u64; 4];
            for sy in y0..y1 {
                let row = (sy * px.w) as usize * 4;
                let run = &px.rgba[row + x0 as usize * 4..row + x1 as usize * 4];
                for [r, g, b, a] in run.as_chunks::<4>().0 {
                    let a = u64::from(*a);
                    sum[0] += u64::from(*r) * a;
                    sum[1] += u64::from(*g) * a;
                    sum[2] += u64::from(*b) * a;
                    sum[3] += a;
                }
            }
            let n = u64::from((y1 - y0) * (x1 - x0));
            for c in 0..3 {
                rgba.push(sum[c].checked_div(sum[3]).unwrap_or(0) as u8);
            }
            rgba.push((sum[3] / n) as u8);
        }
    }
    Pixels { w, h, rgba }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iptv::refresh::{NetError, Response};
    use std::time::Instant;

    /// Serves a 480x330 PNG at `/logo.png`, a 404 elsewhere.
    struct Fake;

    impl Transport for Fake {
        fn get(
            &self,
            url: &str,
            _headers: &[String],
            _budget: Duration,
            _cancel: &Arc<AtomicBool>,
        ) -> Result<Response, NetError> {
            static PNG: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
            let png = url.ends_with("/logo.png");
            let body = if png {
                PNG.get_or_init(|| {
                    let rgba = [200u8, 100, 50, 255].repeat(480 * 330);
                    tv_gfx::img::img_encode_png(480, 330, &rgba).unwrap()
                })
                .clone()
            } else {
                Vec::new()
            };
            Ok(Response {
                status: if png { 200 } else { 404 },
                validators: Validators::default(),
                location: None,
                body: Box::new(std::io::Cursor::new(body)),
            })
        }
    }

    fn channel(logo: &str) -> Channel {
        let mut c = tv_screens::data::fake::FakeData::new(0, 1)
            .channels
            .remove(0);
        c.logo = Some(logo.into());
        c
    }

    /// Pump until nothing is loading; the textures handed out, in order.
    fn settle(cache: &LogoCache, uploads: &mut Vec<(u32, u32)>, freed: &mut Vec<u32>) {
        let start = Instant::now();
        while cache.loading() && start.elapsed() < Duration::from_secs(10) {
            let mut upload = |px: &Pixels| {
                uploads.push((px.w, px.h));
                uploads.len() as u32
            };
            cache.pump(&mut upload, &mut |t| freed.push(t));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!cache.loading());
    }

    #[test]
    fn a_logo_loads_once_scaled_into_the_box_and_a_failure_stays_none() {
        let cache = LogoCache::new(Some(Box::new(Fake)));
        let (mut uploads, mut freed) = (Vec::new(), Vec::new());
        let good = channel("http://host/logo.png");
        let bad = channel("http://host/missing.png");
        assert_eq!(cache.logo(&good), None, "loading");
        assert_eq!(cache.logo(&bad), None);
        assert_eq!(cache.logo(&good), None, "asked for once");
        settle(&cache, &mut uploads, &mut freed);
        assert_eq!(uploads, [(240, 165)]);
        assert_eq!(cache.logo(&good), Some((1, 240.0, 165.0)));
        assert_eq!(cache.logo(&bad), None);
        assert!(!cache.loading(), "a failure is not asked for again");
        assert!(freed.is_empty());
    }

    #[test]
    fn without_a_transport_or_a_logo_nothing_is_asked() {
        let cache = LogoCache::new(None);
        assert_eq!(cache.logo(&channel("http://host/logo.png")), None);
        assert!(!cache.loading());
        let cache = LogoCache::new(Some(Box::new(Fake)));
        assert_eq!(cache.logo(&channel("")), None);
        assert!(!cache.loading());
    }

    #[test]
    fn the_least_recently_drawn_texture_goes_beyond_the_cap() {
        let cache = LogoCache::new(Some(Box::new(Fake)));
        let (mut uploads, mut freed) = (Vec::new(), Vec::new());
        for i in 0..=RESIDENT {
            cache.next_frame();
            cache.logo(&channel(&format!("http://host/{i}/logo.png")));
            settle(&cache, &mut uploads, &mut freed);
            cache.logo(&channel(&format!("http://host/{i}/logo.png")));
        }
        assert_eq!(freed, [1], "the first one, drawn longest ago");
        assert_eq!(cache.logo(&channel("http://host/0/logo.png")), None);
    }

    /// [`Fake`] behind a gate: each download waits until the gate opens (or the cache cancels),
    /// counting the downloads that started.
    #[derive(Clone, Default)]
    struct Gate(Arc<(Mutex<(bool, usize)>, Condvar)>);

    impl Gate {
        fn open(&self) {
            self.0 .0.lock().unwrap().0 = true;
            self.0 .1.notify_all();
        }

        fn started(&self) -> usize {
            self.0 .0.lock().unwrap().1
        }
    }

    impl Transport for Gate {
        fn get(
            &self,
            url: &str,
            headers: &[String],
            budget: Duration,
            cancel: &Arc<AtomicBool>,
        ) -> Result<Response, NetError> {
            let (lock, opened) = &*self.0;
            let mut state = lock.lock().unwrap();
            state.1 += 1;
            while !state.0 && !cancel.load(Ordering::Acquire) {
                state = opened
                    .wait_timeout(state, Duration::from_millis(5))
                    .unwrap()
                    .0;
            }
            drop(state);
            Fake.get(url, headers, budget, cancel)
        }
    }

    fn queued(cache: &LogoCache) -> Vec<String> {
        let worker = cache.worker.as_ref().unwrap();
        worker.queue.wanted.lock().unwrap().0.clone()
    }

    fn until(done: impl Fn() -> bool) {
        let start = Instant::now();
        while !done() && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn requests_no_frame_draws_any_more_are_dropped_and_the_queue_is_capped() {
        let gate = Gate::default();
        let cache = LogoCache::new(Some(Box::new(gate.clone())));
        let logo = |i: usize| channel(&format!("http://host/{i}/logo.png"));
        cache.logo(&logo(0));
        until(|| gate.started() == 1);
        for i in 1..=MAX_QUEUED + 3 {
            cache.logo(&logo(i));
        }
        let urls = queued(&cache);
        assert_eq!(urls.len(), MAX_QUEUED, "capped");
        assert_eq!(urls[0], "http://host/4/logo.png", "the oldest went");
        assert_eq!(cache.loading.get(), MAX_QUEUED + 1, "one is downloading");
        // Only the last row stays on screen.
        let last = logo(MAX_QUEUED + 3);
        for _ in 0..=STALE_FRAMES {
            cache.next_frame();
            assert_eq!(cache.logo(&last), None);
        }
        assert_eq!(queued(&cache), [last.logo.clone().unwrap()]);
        assert_eq!(cache.loading.get(), 2);
        let ready = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let woken = Arc::clone(&ready);
        cache.set_wake(move || {
            woken.fetch_add(1, Ordering::SeqCst);
        });
        gate.open();
        let (mut uploads, mut freed) = (Vec::new(), Vec::new());
        settle(&cache, &mut uploads, &mut freed);
        assert_eq!(
            uploads.len(),
            2,
            "the one in flight and the one still drawn"
        );
        // The worker wakes the loop right after handing a logo over.
        until(|| ready.load(Ordering::SeqCst) == 2);
        assert_eq!(ready.load(Ordering::SeqCst), 2, "each one woke the loop");
        assert_eq!(
            cache.logo(&logo(1)),
            None,
            "a dropped one is asked for again"
        );
        assert!(cache.loading());
    }

    #[test]
    fn headers_give_the_size_and_oversized_or_unknown_pictures_are_refused() {
        let png = tv_gfx::img::img_encode_png(3, 2, &[9u8; 3 * 2 * 4]).unwrap();
        assert_eq!(dimensions(&png), Some((3, 2)));
        assert!(decode(&png).is_some());
        let mut gif = b"GIF89a".to_vec();
        gif.extend([0x40, 0x01, 0xF0, 0x00]);
        assert_eq!(dimensions(&gif), Some((320, 240)));
        // JPEG: SOI, an APP0 of 4 bytes, then SOF0 with height 1080 and width 1920.
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x04,
            0x38, 0x07, 0x80,
        ];
        assert_eq!(dimensions(&jpeg), Some((1920, 1080)));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        webp.extend([0; 8]);
        webp.extend([0x7F, 0x02, 0x00, 0x67, 0x01, 0x00]);
        assert_eq!(dimensions(&webp), Some((640, 360)));
        // A PNG header claiming 5000x5000 never reaches the decoder.
        let mut huge = png.clone();
        huge[16..24].copy_from_slice(&[0, 0, 0x13, 0x88, 0, 0, 0x13, 0x88]);
        assert_eq!(dimensions(&huge), Some((5000, 5000)));
        assert!(decode(&huge).is_none());
        assert_eq!(
            dimensions(b"<svg xmlns='http://www.w3.org/2000/svg'/>"),
            None
        );
        assert_eq!(dimensions(&[0xFF, 0xD8, 0xFF]), None, "a cut header");
    }

    #[test]
    fn scaling_keeps_the_aspect_and_weights_colour_by_alpha() {
        // 4x2: left half opaque red, right half transparent green; to fit 2x1 at most it halves.
        let mut rgba = Vec::new();
        for _ in 0..2 {
            rgba.extend([255, 0, 0, 255, 255, 0, 0, 255, 0, 255, 0, 0, 0, 255, 0, 0]);
        }
        let big = Pixels {
            w: BOX_W * 4,
            h: BOX_H * 2,
            rgba: rgba.repeat((BOX_W * BOX_H) as usize),
        };
        let out = fit(big);
        assert_eq!((out.w, out.h), (BOX_W, BOX_H / 2 + 1));
        let half = Pixels { w: 4, h: 2, rgba };
        let same = fit(half);
        assert_eq!((same.w, same.h), (4, 2), "small pictures stay");
        let mixed = fit(Pixels {
            w: BOX_W * 2,
            h: 1,
            rgba: [255, 0, 0, 255, 0, 255, 0, 0].repeat(BOX_W as usize),
        });
        assert_eq!(
            &mixed.rgba[..4],
            [255, 0, 0, 127],
            "transparent green adds no colour"
        );
    }
}
