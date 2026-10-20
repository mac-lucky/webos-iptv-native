//! Streaming XMLTV parser. Reads a plain or gzip guide from any `BufRead` and hands
//! channels and programmes to a [`Sink`] as they stream, so a guide of any size is never held in
//! memory: the parser keeps one XML event (capped) and the fields of the current element.
//!
//! Parts are ported to Rust from webOS IPTV Player `src/parsers/xmltv-parser.ts` and
//! `xmltv-loader.ts` (commit 7d8fd30, Copyright 2026 lennylxx, Apache-2.0; see
//! THIRD-PARTY-NOTICES.md) and changed: the timestamp parser, the channel and programme checks
//! (filter, time window, budget) with their statistics, the element and entity rules, display-name
//! retention, the order retry and the gzip and UTF-8 handling. The quick-xml streaming reader and
//! the caps are this project's.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read};

use flate2::bufread::MultiGzDecoder;
use quick_xml::events::{BytesStart, BytesText, Event};
use quick_xml::Reader;

use crate::model::{name_key, EpgChannel, Programme, Timestamp};

/// Programmes are kept from `now - WINDOW_SECS` to `now + WINDOW_SECS`.
pub const WINDOW_SECS: i64 = 7 * 86_400;
/// Default programme budget: about 200 channels x 14 days of half-hour slots, with headroom.
const DEFAULT_MAX_PROGRAMMES: usize = 200_000;
/// Most channel ids one guide may make anyone track: the parser's sets, and the catalog channels
/// (outside the filter) it sends.
pub const MAX_CHANNELS: usize = 10_000;

/// Display names kept per channel, plus up to as many later ones the name filter asked for.
const MAX_NAMES: usize = 4;
const MAX_ID_BYTES: usize = 256;
const MAX_NAME_BYTES: usize = 256;
const MAX_TITLE_BYTES: usize = 512;
/// More than the guide's three lines and the programme card ever show; every byte is stored per
/// programme on flash.
const MAX_DESC_BYTES: usize = 2048;
const MAX_CATEGORY_BYTES: usize = 128;
/// An icon URL past this is dropped, not cut: image URLs are a few hundred bytes.
const MAX_URL_BYTES: usize = 1024;
/// One XML event (a tag, a text run, a comment) larger than this aborts the parse.
const MAX_EVENT_BYTES: usize = 1 << 20;
/// A decoded guide larger than this aborts the parse (a gzip bomb ends here).
const MAX_INPUT_BYTES: u64 = 1 << 30;
/// Elements open at once past this abort the parse: quick-xml keeps the name of every open tag.
const MAX_DEPTH: usize = 64;
/// Tags left open past this after a channel or programme (a `<br>` in a description) are
/// dropped from the reader.
const RESYNC_DEPTH: usize = 8;
const CHUNK_BYTES: usize = 64 * 1024;
const REPLACEMENT: &[u8] = "\u{FFFD}".as_bytes();

/// What to keep. With no ids and no names every channel is wanted.
#[derive(Clone, Debug)]
pub struct Options {
    /// Centre of the kept window.
    pub now: Timestamp,
    /// Guide channel ids whose programmes are wanted.
    pub ids: HashSet<String>,
    /// Display names, as [`name_key`]s, that also select a channel (playlists without `tvg-id`).
    pub names: HashSet<String>,
    /// Programmes past this many are counted in [`Stats::dropped_budget`], not emitted.
    pub max_programmes: usize,
}

impl Options {
    pub fn new(now: Timestamp) -> Self {
        Options {
            now,
            ids: HashSet::new(),
            names: HashSet::new(),
            max_programmes: DEFAULT_MAX_PROGRAMMES,
        }
    }
}

/// Receives the guide in document order.
pub trait Sink {
    type Error;
    /// Every wanted channel, and the first [`MAX_CHANNELS`] others: those (`wanted` false) form
    /// the catalog a manual mapping picks from, and none of their programmes follow.
    fn channel(&mut self, channel: EpgChannel, wanted: bool) -> Result<(), Self::Error>;
    /// A programme of a wanted channel inside the window and the budget.
    fn programme(&mut self, channel_id: &str, programme: Programme) -> Result<(), Self::Error>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub channels_seen: usize,
    /// Channels emitted as wanted.
    pub channels_kept: usize,
    pub programmes_seen: usize,
    pub programmes_kept: usize,
    pub skipped_date: usize,
    pub skipped_range: usize,
    pub skipped_filter: usize,
    pub dropped_budget: usize,
    pub malformed: usize,
}

impl Stats {
    /// Programmes admitted by the channel filter, before the date and budget checks.
    pub fn programmes_matched(&self) -> usize {
        self.programmes_kept + self.skipped_date + self.skipped_range + self.dropped_budget
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub stats: Stats,
    /// `<tv source-info-name>`, else `generator-info-name`.
    pub source_name: Option<String>,
    /// UTC offset of the first kept programme that declares one.
    pub tz_offset_minutes: Option<i32>,
    pub gzip: bool,
    /// The root `</tv>` was read: a guide cut short (a truncated download) must not replace a
    /// complete one.
    pub complete: bool,
    /// The name filter accepted a channel after some of its programmes had already been
    /// skipped: parse the guide again with the wanted channels' ids in [`Options::ids`] to get
    /// them.
    pub retry: bool,
}

#[derive(Debug)]
pub enum Error<E> {
    /// Reading or decompressing failed, or the input exceeded a size cap.
    Read(quick_xml::Error),
    Sink(E),
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Read(e) => write!(f, "XMLTV read failed: {e}"),
            Error::Sink(e) => write!(f, "XMLTV sink failed: {e}"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for Error<E> {}

/// Parses one guide. Malformed elements are counted and skipped; read failures, XML syntax
/// errors, oversized or too deeply nested input and sink errors abort. A guide that ends before
/// its root `</tv>` comes back with [`Summary::complete`] false.
pub fn parse<R: BufRead, S: Sink>(
    input: R,
    options: &Options,
    sink: &mut S,
) -> Result<Summary, Error<S::Error>> {
    let (input, gzip) = open(input).map_err(|e| Error::Read(e.into()))?;
    let filtering = !options.ids.is_empty() || !options.names.is_empty();
    let mut parser = Parser {
        xml: Xml {
            reader: reader(Input::new(input)),
            depth: 0,
            malformed: 0,
            pending: None,
            closed: false,
        },
        buf: Vec::new(),
        options,
        filtering,
        named: HashSet::new(),
        rejected: HashSet::new(),
        catalog: 0,
        min: options.now - WINDOW_SECS,
        max: options.now + WINDOW_SECS,
        summary: Summary {
            gzip,
            ..Summary::default()
        },
    };
    parser.run(sink)?;
    Ok(parser.finish())
}

fn reader(input: Source<'_>) -> Reader<Source<'_>> {
    let mut reader = Reader::from_reader(input);
    let config = reader.config_mut();
    config.allow_dangling_amp = true;
    config.check_end_names = false;
    config.allow_unmatched_ends = true;
    reader
}

/// Detects gzip by its magic bytes and returns the decoded stream.
fn open<'a, R: BufRead + 'a>(mut input: R) -> io::Result<(Box<dyn BufRead + 'a>, bool)> {
    let mut magic = Vec::with_capacity(2);
    input.by_ref().take(2).read_to_end(&mut magic)?;
    let gzip = magic == [0x1f, 0x8b];
    let joined = io::Cursor::new(magic).chain(input);
    Ok(if gzip {
        let decoder = MultiGzDecoder::new(joined);
        (
            Box::new(BufReader::with_capacity(CHUNK_BYTES, decoder)),
            true,
        )
    } else {
        (Box::new(joined), false)
    })
}

/// What quick-xml reads: valid UTF-8 (invalid sequences become U+FFFD, as the TextDecoder of the
/// web app did, instead of ending the parse), at most [`MAX_EVENT_BYTES`] for one event, so hostile
/// input cannot grow the event buffer, and at most [`MAX_INPUT_BYTES`] in all.
struct Input<R> {
    inner: R,
    out: Vec<u8>,
    pos: usize,
    /// An incomplete UTF-8 sequence at the end of the last chunk.
    carry: Vec<u8>,
    event_bytes: usize,
    total_bytes: u64,
}

impl<R: BufRead> Input<R> {
    fn new(inner: R) -> Self {
        Input {
            inner,
            out: Vec::new(),
            pos: 0,
            carry: Vec::new(),
            event_bytes: 0,
            total_bytes: 0,
        }
    }

    fn start_event(&mut self) {
        self.event_bytes = 0;
    }

    fn refill(&mut self) -> io::Result<()> {
        self.out.clear();
        self.pos = 0;
        while self.out.is_empty() {
            let chunk = match self.inner.fill_buf() {
                Ok(chunk) => chunk,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if chunk.is_empty() {
                if !self.carry.is_empty() {
                    self.carry.clear();
                    self.out.extend_from_slice(REPLACEMENT);
                }
                break;
            }
            let len = chunk.len().min(CHUNK_BYTES);
            if self.carry.is_empty() {
                sanitize(&chunk[..len], &mut self.out, &mut self.carry);
            } else {
                let mut joined = std::mem::take(&mut self.carry);
                joined.extend_from_slice(&chunk[..len]);
                sanitize(&joined, &mut self.out, &mut self.carry);
            }
            self.inner.consume(len);
        }
        Ok(())
    }
}

/// Appends `bytes` to `out` as valid UTF-8; a sequence cut at the end goes to `carry`.
fn sanitize(mut bytes: &[u8], out: &mut Vec<u8>, carry: &mut Vec<u8>) {
    loop {
        match std::str::from_utf8(bytes) {
            Ok(_) => return out.extend_from_slice(bytes),
            Err(e) => {
                let (valid, rest) = bytes.split_at(e.valid_up_to());
                out.extend_from_slice(valid);
                match e.error_len() {
                    Some(bad) => {
                        out.extend_from_slice(REPLACEMENT);
                        bytes = &rest[bad..];
                    }
                    None => return carry.extend_from_slice(rest),
                }
            }
        }
    }
}

impl<R: BufRead> Read for Input<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl<R: BufRead> BufRead for Input<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.event_bytes > MAX_EVENT_BYTES || self.total_bytes > MAX_INPUT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "XMLTV input exceeds the size cap",
            ));
        }
        if self.pos == self.out.len() {
            self.refill()?;
        }
        Ok(&self.out[self.pos..])
    }

    fn consume(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.out.len());
        self.event_bytes += n;
        self.total_bytes += n as u64;
    }
}

/// The decoded input as quick-xml reads it.
type Source<'a> = Input<Box<dyn BufRead + 'a>>;

/// The event reader, lenient the way the web app's parser was. Events are read into a buffer the
/// caller passes, so a start tag's attributes stay borrowed while its body is read.
struct Xml<'a> {
    reader: Reader<Source<'a>>,
    /// Tags the reader holds open (it keeps every open name).
    depth: usize,
    malformed: usize,
    /// A channel or programme start tag met inside another element, which ended that element:
    /// the main loop takes it next.
    pending: Option<(BytesStart<'static>, bool)>,
    /// The root `</tv>` was read.
    closed: bool,
}

impl Xml<'_> {
    /// The next event, into `buf`. Inside a channel or programme (`inside`), a channel or
    /// programme start tag ends the element early, and so does the root end tag: both read as
    /// the end of the document, and the start tag is kept in `pending`.
    fn read<'b>(
        &mut self,
        buf: &'b mut Vec<u8>,
        inside: bool,
    ) -> Result<Event<'b>, quick_xml::Error> {
        buf.clear();
        self.reader.get_mut().start_event();
        let event = match self.reader.read_event_into(buf) {
            // Ill-formed markup is already consumed and the reader goes on.
            Err(quick_xml::Error::IllFormed(_)) => {
                self.malformed += 1;
                return Ok(Event::Comment(BytesText::from_escaped("")));
            }
            other => other?,
        };
        match &event {
            Event::Start(e) => {
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return Err(
                        io::Error::new(io::ErrorKind::InvalidData, "XMLTV nests too deep").into(),
                    );
                }
                if inside && is_element(e) {
                    self.pending = Some((e.to_owned(), false));
                    return Ok(Event::Eof);
                }
            }
            Event::Empty(e) if inside && is_element(e) => {
                self.pending = Some((e.to_owned(), true));
                return Ok(Event::Eof);
            }
            Event::End(e) => {
                self.depth = self.depth.saturating_sub(1);
                if e.name().as_ref() == "tv" {
                    self.closed = true;
                    if inside {
                        return Ok(Event::Eof);
                    }
                }
            }
            _ => {}
        }
        Ok(event)
    }

    /// Drops the tags left open inside the elements read so far (a `<br>` in a description):
    /// only a new reader forgets the names quick-xml keeps for them.
    fn resync(&mut self) {
        if self.depth <= RESYNC_DEPTH {
            return;
        }
        let idle: Box<dyn BufRead> = Box::new(io::empty());
        let old = std::mem::replace(&mut self.reader, Reader::from_reader(Input::new(idle)));
        self.reader = reader(old.into_inner());
        self.depth = 0;
    }

    /// The text of element `name`, whose start tag was just read, through its end tag: inner
    /// markup dropped, CDATA verbatim, references decoded, trimmed, at most `cap` bytes. `None`
    /// when the element ends early.
    fn text(
        &mut self,
        buf: &mut Vec<u8>,
        name: &str,
        cap: usize,
    ) -> Result<Option<String>, quick_xml::Error> {
        let mut out = String::new();
        loop {
            match self.read(buf, true)? {
                Event::Text(t) => push_capped(&mut out, &t, cap),
                Event::CData(t) => push_capped(&mut out, &t, cap),
                Event::GeneralRef(r) => match resolve(&r) {
                    Some(c) => push_capped(&mut out, c.encode_utf8(&mut [0; 4]), cap),
                    None => push_capped(&mut out, &format!("&{};", &*r), cap),
                },
                Event::End(e) if e.name().as_ref() == name => break,
                Event::Eof => return Ok(None),
                _ => {}
            }
        }
        let trimmed = out.trim_end().len();
        out.truncate(trimmed);
        Ok(Some(out))
    }

    /// Reads through the end of element `name`, whose start tag was just read; false when it
    /// ends early.
    fn skip(&mut self, buf: &mut Vec<u8>, name: &str) -> Result<bool, quick_xml::Error> {
        loop {
            match self.read(buf, true)? {
                Event::End(e) if e.name().as_ref() == name => return Ok(true),
                Event::Eof => return Ok(false),
                _ => {}
            }
        }
    }
}

fn is_element(e: &BytesStart<'_>) -> bool {
    matches!(e.name().as_ref(), "channel" | "programme")
}

struct Parser<'o, 'a> {
    xml: Xml<'a>,
    /// Events inside a channel or programme; the main loop reads start tags into its own.
    buf: Vec<u8>,
    options: &'o Options,
    filtering: bool,
    /// Ids the name filter accepted, besides [`Options::ids`].
    named: HashSet<String>,
    /// Programme channel ids the filter skipped while a name match could still accept them.
    rejected: HashSet<String>,
    /// Catalog channels (outside the filter) sent, at most [`MAX_CHANNELS`].
    catalog: usize,
    min: Timestamp,
    max: Timestamp,
    summary: Summary,
}

/// What becomes of a programme, judged from its start tag.
enum Verdict<'c> {
    Malformed,
    Filtered(&'c str),
    Undated,
    OutOfRange,
    OverBudget {
        offset: Option<i32>,
    },
    Keep {
        channel: &'c str,
        programme: Programme,
        offset: Option<i32>,
    },
}

#[derive(Default)]
struct ChannelBody {
    names: Vec<String>,
    icon: Option<String>,
    /// A display name is in the name filter.
    name_match: bool,
}

impl Parser<'_, '_> {
    fn run<S: Sink>(&mut self, sink: &mut S) -> Result<(), Error<S::Error>> {
        let mut outer = Vec::new();
        loop {
            let pending = self.xml.pending.take();
            let event;
            let (start, empty) = match &pending {
                Some((start, empty)) => (start, *empty),
                None => {
                    event = self.xml.read(&mut outer, false).map_err(Error::Read)?;
                    match &event {
                        Event::Start(e) => (e, false),
                        Event::Empty(e) => (e, true),
                        Event::Eof => return Ok(()),
                        _ => continue,
                    }
                }
            };
            match start.name().as_ref() {
                "tv" => {
                    if self.summary.source_name.is_none() {
                        self.summary.source_name = source_name(start);
                    }
                }
                "channel" => {
                    let [id] = attrs(start, ["id"]);
                    self.channel(id, empty, sink)?;
                    self.xml.resync();
                }
                "programme" => {
                    self.programme(attrs(start, ["channel", "start", "stop"]), empty, sink)?;
                    self.xml.resync();
                }
                _ => {}
            }
        }
    }

    fn accepts(&self, id: &str) -> bool {
        !self.filtering || self.options.ids.contains(id) || self.named.contains(id)
    }

    fn channel<S: Sink>(
        &mut self,
        id: Option<Cow<'_, str>>,
        empty: bool,
        sink: &mut S,
    ) -> Result<(), Error<S::Error>> {
        let body = if empty {
            Some(ChannelBody::default())
        } else {
            self.channel_body().map_err(Error::Read)?
        };
        // Like the web app, an element counts only once it is complete.
        let Some(ChannelBody {
            mut names,
            icon,
            name_match,
        }) = body
        else {
            self.summary.stats.malformed += 1;
            return Ok(());
        };
        self.summary.stats.channels_seen += 1;
        let Some(id) = id.filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES) else {
            self.summary.stats.malformed += 1;
            return Ok(());
        };
        let mut wanted = self.accepts(&id);
        if !wanted && name_match && self.options.ids.len() + self.named.len() < MAX_CHANNELS {
            self.named.insert(id.to_string());
            wanted = true;
        }
        if wanted {
            self.summary.stats.channels_kept += 1;
        } else {
            self.summary.stats.skipped_filter += 1;
            if self.catalog == MAX_CHANNELS {
                return Ok(());
            }
            self.catalog += 1;
        }
        let id = id.into_owned();
        if names.is_empty() {
            names.push(id.clone());
        }
        sink.channel(EpgChannel { id, names, icon }, wanted)
            .map_err(Error::Sink)
    }

    /// Display names and icon of a channel; `None` when it ends early.
    fn channel_body(&mut self) -> Result<Option<ChannelBody>, quick_xml::Error> {
        let filter = &self.options.names;
        let mut body = ChannelBody::default();
        loop {
            match self.xml.read(&mut self.buf, true)? {
                Event::Start(e) if e.name().as_ref() == "display-name" => {}
                Event::Start(e) | Event::Empty(e) => {
                    if e.name().as_ref() == "icon" && body.icon.is_none() {
                        body.icon = icon_src(&e);
                    }
                    continue;
                }
                Event::End(e) if e.name().as_ref() == "channel" => return Ok(Some(body)),
                Event::Eof => return Ok(None),
                _ => continue,
            }
            let Some(name) = self
                .xml
                .text(&mut self.buf, "display-name", MAX_NAME_BYTES)?
            else {
                return Ok(None);
            };
            if name.is_empty() {
                continue;
            }
            let matched = !filter.is_empty() && filter.contains(&name_key(&name));
            body.name_match |= matched;
            // Past the first few, keep only names the filter asked for.
            if body.names.len() < MAX_NAMES || (matched && body.names.len() < 2 * MAX_NAMES) {
                body.names.push(name);
            }
        }
    }

    fn programme<S: Sink>(
        &mut self,
        [channel, start, stop]: [Option<Cow<'_, str>>; 3],
        empty: bool,
        sink: &mut S,
    ) -> Result<(), Error<S::Error>> {
        // Decided from the attributes alone, so a skipped body is never decoded.
        let mut verdict = self.verdict(channel.as_deref(), start.as_deref(), stop.as_deref());
        let complete = empty
            || match &mut verdict {
                Verdict::Keep { programme, .. } => self.programme_body(programme),
                _ => self.xml.skip(&mut self.buf, "programme"),
            }
            .map_err(Error::Read)?;
        // Like the web app, an element counts only once it is complete.
        let stats = &mut self.summary.stats;
        if !complete {
            stats.malformed += 1;
            return Ok(());
        }
        stats.programmes_seen += 1;
        match verdict {
            Verdict::Malformed => stats.malformed += 1,
            Verdict::Filtered(channel) => {
                stats.skipped_filter += 1;
                if !self.options.names.is_empty() && self.rejected.len() < MAX_CHANNELS {
                    self.rejected.insert(channel.to_owned());
                }
            }
            Verdict::Undated => stats.skipped_date += 1,
            Verdict::OutOfRange => stats.skipped_range += 1,
            Verdict::OverBudget { offset } => {
                stats.dropped_budget += 1;
                self.summary.tz_offset_minutes = self.summary.tz_offset_minutes.or(offset);
            }
            Verdict::Keep {
                channel,
                programme,
                offset,
            } => {
                stats.programmes_kept += 1;
                self.summary.tz_offset_minutes = self.summary.tz_offset_minutes.or(offset);
                return sink.programme(channel, programme).map_err(Error::Sink);
            }
        }
        Ok(())
    }

    fn verdict<'c>(
        &self,
        channel: Option<&'c str>,
        start: Option<&str>,
        stop: Option<&str>,
    ) -> Verdict<'c> {
        let Some(channel) = channel.filter(|c| !c.is_empty() && c.len() <= MAX_ID_BYTES) else {
            return Verdict::Malformed;
        };
        if !self.accepts(channel) {
            return Verdict::Filtered(channel);
        }
        let (Some((start, offset)), Some((stop, _))) =
            (start.and_then(parse_time), stop.and_then(parse_time))
        else {
            return Verdict::Undated;
        };
        // A programme that does not last is not a programme (and breaks overlap queries).
        if stop <= start {
            return Verdict::Undated;
        }
        if stop < self.min || start > self.max {
            return Verdict::OutOfRange;
        }
        // Past the budget an element costs only a counter.
        if self.summary.stats.programmes_kept >= self.options.max_programmes {
            return Verdict::OverBudget { offset };
        }
        Verdict::Keep {
            channel,
            programme: Programme {
                start,
                stop,
                ..Programme::default()
            },
            offset,
        }
    }

    /// Fills title, description, category and icon from the first of each; false when the
    /// programme ends early.
    fn programme_body(&mut self, programme: &mut Programme) -> Result<bool, quick_xml::Error> {
        loop {
            let (field, slot, cap) = match self.xml.read(&mut self.buf, true)? {
                Event::Start(e) | Event::Empty(e) if e.name().as_ref() == "icon" => {
                    if programme.icon.is_none() {
                        programme.icon = icon_src(&e);
                    }
                    continue;
                }
                Event::Start(e) => match e.name().as_ref() {
                    "title" => ("title", &mut programme.title, MAX_TITLE_BYTES),
                    "desc" => (
                        "desc",
                        programme.description.get_or_insert_default(),
                        MAX_DESC_BYTES,
                    ),
                    "category" => (
                        "category",
                        programme.category.get_or_insert_default(),
                        MAX_CATEGORY_BYTES,
                    ),
                    _ => continue,
                },
                Event::End(e) if e.name().as_ref() == "programme" => {
                    programme.description.take_if(|d| d.is_empty());
                    programme.category.take_if(|c| c.is_empty());
                    return Ok(true);
                }
                Event::Eof => return Ok(false),
                _ => continue,
            };
            let taken = !slot.is_empty();
            let Some(text) = self
                .xml
                .text(&mut self.buf, field, if taken { 0 } else { cap })?
            else {
                return Ok(false);
            };
            if !taken {
                *slot = text;
            }
        }
    }

    fn finish(mut self) -> Summary {
        self.summary.stats.malformed += self.xml.malformed;
        self.summary.complete = self.xml.closed;
        self.summary.retry = self.rejected.iter().any(|id| self.named.contains(id));
        self.summary
    }
}

/// `<tv source-info-name>`, else `generator-info-name`.
fn source_name(e: &BytesStart<'_>) -> Option<String> {
    let [source, generator] = attrs(e, ["source-info-name", "generator-info-name"]);
    [source, generator].into_iter().flatten().find_map(|s| {
        let s = s.trim();
        (!s.is_empty() && s.len() <= MAX_NAME_BYTES).then(|| s.to_owned())
    })
}

/// The named attributes of a tag, entity-decoded and borrowed from it where nothing was
/// decoded; malformed attributes are ignored.
fn attrs<'e, const N: usize>(e: &'e BytesStart<'_>, names: [&str; N]) -> [Option<Cow<'e, str>>; N] {
    let mut found = [const { None }; N];
    for attr in e.attributes().with_checks(false).flatten() {
        if let Some(i) = names.iter().position(|n| *n == attr.key.as_ref()) {
            if found[i].is_none() {
                found[i] = Some(decode(attr.value));
            }
        }
    }
    found
}

fn icon_src(e: &BytesStart<'_>) -> Option<String> {
    let [src] = attrs(e, ["src"]);
    src.filter(|s| !s.is_empty() && s.len() <= MAX_URL_BYTES)
        .map(Cow::into_owned)
}

fn push_capped(out: &mut String, s: &str, cap: usize) {
    let s = if out.is_empty() { s.trim_start() } else { s };
    out.push_str(&s[..s.floor_char_boundary(cap.saturating_sub(out.len()))]);
}

/// The character a reference name (between `&` and `;`) stands for: the five XML entities in
/// any case, or a decimal / hex character reference.
fn resolve(name: &str) -> Option<char> {
    if let Some(number) = name.strip_prefix('#') {
        let (digits, radix) = match number.strip_prefix(['x', 'X']) {
            Some(hex) => (hex, 16),
            None => (number, 10),
        };
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return None;
        }
        let code = u32::from_str_radix(digits, radix).ok()?;
        return char::from_u32(code).filter(|&c| c != '\0');
    }
    const NAMED: [(&str, char); 5] = [
        ("amp", '&'),
        ("lt", '<'),
        ("gt", '>'),
        ("quot", '"'),
        ("apos", '\''),
    ];
    NAMED
        .iter()
        .find(|(entity, _)| entity.eq_ignore_ascii_case(name))
        .map(|&(_, c)| c)
}

/// Decodes references in a raw attribute value; anything that is not one stays as written.
fn decode(raw: Cow<'_, str>) -> Cow<'_, str> {
    // The longest reference the decoder knows is `#x10FFFF`.
    const LONGEST: usize = 8;
    if !raw.contains('&') {
        return raw;
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = &*raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp + 1..];
        let end = rest.bytes().take(LONGEST + 1).position(|b| b == b';');
        match end.and_then(|end| Some((resolve(&rest[..end])?, end))) {
            Some((c, end)) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => out.push('&'),
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// `YYYYMMDDHHMMSS` (or `YYYYMMDDHHMM`, seconds 00), optionally followed by spaces and `+HHMM` /
/// `-HHMM`, as UTC seconds and the declared offset in minutes. A missing offset means UTC.
fn parse_time(value: &str) -> Option<(Timestamp, Option<i32>)> {
    let v = value.trim_matches([' ', '\t', '\n', '\r']).as_bytes();
    let len = v.iter().take_while(|b| b.is_ascii_digit()).count();
    if len != 12 && len != 14 {
        return None;
    }
    let (year, month, day) = (digits(v, 0, 4)?, digits(v, 4, 2)?, digits(v, 6, 2)?);
    let (hour, minute) = (digits(v, 8, 2)?, digits(v, 10, 2)?);
    let second = if len == 14 { digits(v, 12, 2)? } else { 0 };
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let rest = &v[len..];
    let rest = &rest[rest.iter().take_while(|b| b" \t\n\r".contains(b)).count()..];
    let offset = match rest {
        [] => None,
        [sign @ (b'+' | b'-'), _, _, _, _] => {
            let (hours, minutes) = (digits(rest, 1, 2)?, digits(rest, 3, 2)?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            let minutes = (hours * 60 + minutes) as i32;
            Some(if *sign == b'-' { -minutes } else { minutes })
        }
        _ => return None,
    };
    let local = tv_base::time::days_from_civil(i64::from(year), month, day) * 86_400
        + i64::from(hour * 3600 + minute * 60 + second);
    Some((local - i64::from(offset.unwrap_or(0)) * 60, offset))
}

fn digits(v: &[u8], at: usize, len: usize) -> Option<u32> {
    v.get(at..at + len)?.iter().try_fold(0u32, |acc, &b| {
        b.is_ascii_digit().then(|| acc * 10 + u32::from(b - b'0'))
    })
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_convert_to_utc() {
        assert_eq!(parse_time("19700101000000"), Some((0, None)));
        assert_eq!(parse_time("19700101010000 +0100"), Some((0, Some(60))));
        assert_eq!(parse_time(" 19700101000000-0130 "), Some((5400, Some(-90))));
        assert_eq!(
            parse_time("20240229235959 +0000"),
            Some((1_709_251_199, Some(0)))
        );
        assert_eq!(parse_time("20230229000000"), None);
        assert_eq!(parse_time("20240101246000"), None);
        assert_eq!(parse_time("20240101000000 +01"), None);
        assert_eq!(parse_time("20240101000000Z"), None);
        assert_eq!(parse_time("2024010100000"), None);
        assert_eq!(parse_time("197001010100 +0100"), Some((0, Some(60))));
        assert_eq!(parse_time("197001010001"), Some((60, None)));
        assert_eq!(parse_time("202401010000001"), None);
    }

    #[test]
    fn references_resolve_and_unknown_text_stays() {
        assert_eq!(decode("a&amp;b&LT;&#x26;&#38;".into()), "a&b<&&");
        assert_eq!(
            decode("Alpha & Bravo &nope; &#0; &#xZZ;".into()),
            "Alpha & Bravo &nope; &#0; &#xZZ;"
        );
        assert_eq!(decode("&".into()), "&");
        assert_eq!(resolve("#x1F600"), Some('\u{1F600}'));
        assert_eq!(resolve("#xD800"), None);
    }

    #[test]
    fn invalid_utf8_becomes_replacement_characters() {
        let (mut out, mut carry) = (Vec::new(), Vec::new());
        sanitize(b"a\xffb\xc5", &mut out, &mut carry);
        assert_eq!(out, "a\u{FFFD}b".as_bytes());
        assert_eq!(carry, b"\xc5");
        let joined = [carry.as_slice(), b"\x82c"].concat();
        carry.clear();
        sanitize(&joined, &mut out, &mut carry);
        assert_eq!(String::from_utf8(out).unwrap(), "a\u{FFFD}b\u{142}c");
        assert!(carry.is_empty());
    }

    #[test]
    fn capped_text_ends_on_a_char_boundary() {
        let mut out = String::new();
        push_capped(&mut out, "  \u{142}\u{142}\u{142}", 5);
        assert_eq!(out, "\u{142}\u{142}");
    }
}
