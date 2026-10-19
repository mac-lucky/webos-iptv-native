//! A generated 200 MB guide streams through the parser within a flat memory budget. Its own test
//! binary, because the counting allocator sees every allocation in the process.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::convert::Infallible;
use std::fmt::Write as _;
use std::io::{self, BufRead, Read};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

use common::stamp;
use iptv::model::{EpgChannel, Programme, Timestamp};
use iptv::xmltv::{self, Options, Sink};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Relaxed) + bytes;
    PEAK.fetch_max(live, Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, size) };
        if !new.is_null() {
            if size > layout.size() {
                grew(size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - size, Relaxed);
            }
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One measurement at a time: the counters are process-wide.
static MEASURING: Mutex<()> = Mutex::new(());

const NOW: Timestamp = 1_780_000_000;
const CHANNELS: usize = 800;
/// Fourteen days of half-hour slots, all inside the kept window.
const PER_CHANNEL: usize = 14 * 48;
const BUDGET_BYTES: usize = 256 << 10;

/// Writes the guide one element at a time: channels first, then each channel's schedule.
struct Generated {
    item: usize,
    buf: String,
    pos: usize,
    bytes: u64,
}

impl Generated {
    fn new() -> Self {
        Generated {
            item: 0,
            buf: String::with_capacity(1024),
            pos: 0,
            bytes: 0,
        }
    }

    fn next_item(&mut self) {
        self.buf.clear();
        self.pos = 0;
        let item = self.item;
        self.item += 1;
        let b = &mut self.buf;
        let programmes = CHANNELS * PER_CHANNEL;
        if item == 0 {
            b.push_str(r#"<?xml version="1.0" encoding="UTF-8"?><tv generator-info-name="gen">"#);
        } else if item <= CHANNELS {
            let c = item - 1;
            let _ = write!(
                b,
                "\n<channel id=\"ch{c}.example\"><display-name lang=\"en\">Channel {c}</display-name><display-name>Channel {c} HD</display-name><icon src=\"http://logo.example/{c}.png\"/></channel>"
            );
        } else if item <= CHANNELS + programmes {
            let n = item - CHANNELS - 1;
            let (c, slot) = (n / PER_CHANNEL, n % PER_CHANNEL);
            let start = NOW - 7 * 86_400 + slot as i64 * 1800;
            let _ = write!(
                b,
                "\n<programme start=\"{} +0000\" stop=\"{} +0000\" channel=\"ch{c}.example\"><title lang=\"en\">Programme {slot} &amp; friends</title><desc lang=\"en\">Episode {slot} of a long running series about channel {c}: a description long enough to look like a real one, with an entity &#x26; some padding text.</desc><category>News</category><icon src=\"http://img.example/{c}/{slot}.jpg\"/></programme>",
                stamp(start),
                stamp(start + 1800)
            );
        } else if item == CHANNELS + programmes + 1 {
            b.push_str("\n</tv>\n");
        }
        self.bytes += b.len() as u64;
    }
}

impl Read for Generated {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Generated {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos == self.buf.len() {
            self.next_item();
        }
        Ok(&self.buf.as_bytes()[self.pos..])
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
    }
}

#[derive(Default)]
struct Count {
    channels: usize,
    programmes: usize,
    last: Option<Programme>,
}

impl Sink for Count {
    type Error = Infallible;

    fn channel(&mut self, _: EpgChannel, _: bool) -> Result<(), Infallible> {
        self.channels += 1;
        Ok(())
    }

    fn programme(&mut self, _: &str, programme: Programme) -> Result<(), Infallible> {
        self.programmes += 1;
        self.last = Some(programme);
        Ok(())
    }
}

#[test]
fn a_200_mb_guide_parses_in_flat_memory() {
    let _measuring = MEASURING.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(stamp(NOW), "20260528202640");
    let mut input = Generated::new();
    let mut sink = Count::default();
    let options = Options {
        max_programmes: usize::MAX,
        ..Options::new(NOW)
    };
    let baseline = LIVE.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    let started = Instant::now();
    let summary = xmltv::parse(&mut input, &options, &mut sink).expect("parse");
    let elapsed = started.elapsed();
    let peak = PEAK.load(Relaxed) - baseline;
    let mb = input.bytes as f64 / 1e6;
    println!(
        "{mb:.0} MB in {:.2} s = {:.0} MB/s, peak {} KiB over baseline",
        elapsed.as_secs_f64(),
        mb / elapsed.as_secs_f64(),
        peak / 1024
    );
    assert!(
        input.bytes >= 200_000_000,
        "generated {} bytes",
        input.bytes
    );
    assert_eq!(sink.channels, CHANNELS);
    assert_eq!(sink.programmes, CHANNELS * PER_CHANNEL);
    assert_eq!(summary.stats.programmes_kept, CHANNELS * PER_CHANNEL);
    assert_eq!(summary.stats.malformed, 0);
    let last = sink.last.expect("a programme");
    assert_eq!(
        last.title,
        format!("Programme {} & friends", PER_CHANNEL - 1)
    );
    assert!(peak < BUDGET_BYTES, "peak {peak} bytes over baseline");
}

#[test]
fn unclosed_nesting_aborts_before_it_costs_memory() {
    let _measuring = MEASURING.lock().unwrap_or_else(|e| e.into_inner());
    let xml = format!("<tv>{}", "<a>".repeat(1 << 20));
    let mut sink = Count::default();
    let baseline = LIVE.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    let result = xmltv::parse(xml.as_bytes(), &Options::new(NOW), &mut sink);
    let peak = PEAK.load(Relaxed) - baseline;
    assert!(matches!(result, Err(xmltv::Error::Read(_))));
    assert!(peak < BUDGET_BYTES, "peak {peak} bytes over baseline");
}
