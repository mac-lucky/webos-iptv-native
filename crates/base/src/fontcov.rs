//! Which codepoints a font file can actually draw - read straight out of its `cmap`.
//!
//! Two callers, for the same reason. `text.rs` asks it per character to decide which emoji the
//! app face can draw and which it drops; the test module at the bottom of THIS file asks it of the
//! shipped `pkg/appfont*.ttf` and asserts a **declared codepoint set**, inside `make check`, in
//! milliseconds, with no television and no GL context.
//!
//! ## Why the gate exists
//!
//! `pkg/appfont.ttf` covers 2853 codepoints: Latin, Cyrillic, Greek. A script outside the
//! declared set renders as tofu, and every test tier this project has is structurally blind to
//! it: the host suite draws nothing, the simulator renders through a different rasterizer, and the
//! device FPS scenes grade `loop=`. Coverage is not a rendering question at all (it is a property
//! of a file on disk), so it belongs in the cheapest tier, and this is it.
//!
//! The same defect class already shipped once, visibly: subtitle convention wraps a sung line in
//! U+2669 - U+266C, Inter carries none of them, and a panel capture of the Family Guy theme read
//! `▯NO GLYPH▯ It seems today`. `tools/cut-inter.py` synthesizes those four; nothing asserted it
//! until now.
//!
//! ## Why the cmap and not `TTF_GlyphIsProvided`
//!
//! SDL2_ttf does expose a coverage query, and it is the wrong tool twice over. `TTF_GlyphIsProvided`
//! takes a **`Uint16`**, so it cannot be asked about anything above U+FFFF; and the 32-bit
//! `TTF_GlyphIsProvided32` is declared in the NDK's header, exported by the NDK's
//! `libSDL2_ttf-2.0.so.0.18.0`, and present on **none of the 14 firmware inventories** - the NDK is
//! 2.0.18 and every television ships 2.0.10 or 2.0.14
//! (`tools/fwcompat.py --lib libSDL2_ttf-2.0.so.0 --grep '32$'` returns zero matches on all 14).
//! It would therefore have compiled, linked and passed `make check`, then failed at the first
//! coverage query on every set. `text.rs`'s `extern` block carries the general form of that trap.
//!
//! Either one also needs an open `TTF_Font` - a size, a GL-adjacent init and a device - which is
//! precisely what a host gate must not need. Parsing the cmap answers the same question from the
//! file alone.
//!
//! ## What it reads
//!
//! Only the sfnt table directory and the `cmap` table, by seek, never the whole file. Unicode
//! subtable formats 4, 6, 12 and 13 are decoded and **unioned**; a mapping to glyph 0 is not
//! coverage and is excluded, which is the difference between "the cmap has an entry" and "the
//! font can draw it".

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// A font's codepoint coverage: a flat bitset for the BMP, sorted ranges above it.
///
/// The BMP half is the hot one - `text.rs` asks per character on every cache-missing string - and
/// 8 KB of bitset makes that a shift and a mask instead of a binary search. Astral codepoints
/// (emoji) are rare enough in playlist text to be worth a `partition_point`.
pub struct Coverage {
    bmp: Box<[u64; 1024]>,
    sup: Vec<(u32, u32)>,
    count: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CovErr {
    Io(String),
    /// The bytes are not an sfnt we recognise, or the table we need is missing/malformed. The
    /// string names WHICH, because "the font is bad" is not an actionable log line.
    Malformed(&'static str),
}

impl std::fmt::Display for CovErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CovErr::Io(e) => write!(f, "{e}"),
            CovErr::Malformed(w) => write!(f, "malformed font: {w}"),
        }
    }
}

impl Coverage {
    fn new() -> Coverage {
        Coverage {
            bmp: Box::new([0u64; 1024]),
            sup: Vec::new(),
            count: 0,
        }
    }

    pub fn contains(&self, cp: u32) -> bool {
        if cp <= 0xFFFF {
            let i = cp as usize;
            return self.bmp[i >> 6] & (1u64 << (i & 63)) != 0;
        }
        // ranges are sorted and disjoint: the last one starting at or before `cp` is the only
        // candidate. `partition_point` is the standard spelling of "how many start <= cp".
        let k = self.sup.partition_point(|&(s, _)| s <= cp);
        k > 0 && self.sup[k - 1].1 >= cp
    }

    /// How many codepoints this font maps to a real glyph. The gate quotes it, and `text.rs` logs
    /// it once so an event log says what the face actually loaded.
    pub fn len(&self) -> u32 {
        self.count
    }

    fn add(&mut self, cp: u32) {
        if cp > 0x10FFFF {
            return;
        }
        if cp <= 0xFFFF {
            let i = cp as usize;
            let (w, b) = (i >> 6, 1u64 << (i & 63));
            if self.bmp[w] & b == 0 {
                self.bmp[w] |= b;
                self.count += 1;
            }
        } else {
            self.sup.push((cp, cp));
        }
    }

    fn add_range(&mut self, lo: u32, hi: u32) {
        if lo > hi {
            return;
        }
        // The BMP part goes bit by bit (bounded by 65536 iterations); anything above is stored as
        // a range, so a font mapping all of plane 2 costs one entry rather than 43 000.
        let bmp_hi = hi.min(0xFFFF);
        for cp in lo..=bmp_hi {
            self.add(cp);
        }
        if hi > 0xFFFF {
            let lo = lo.max(0x10000);
            let hi = hi.min(0x10FFFF);
            if lo <= hi {
                self.sup.push((lo, hi));
            }
        }
    }

    /// Sort and coalesce the supplementary ranges, and only then count them. Subtables are unioned,
    /// so the same astral codepoint can arrive from two of them; counting before the merge would
    /// double it, and `contains` needs the sorted invariant `partition_point` assumes.
    fn seal(&mut self) {
        self.sup.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(self.sup.len());
        for &(lo, hi) in &self.sup {
            match merged.last_mut() {
                // `lo <= last.1 + 1` also coalesces ADJACENT ranges, which is what keeps a
                // per-codepoint format-4 walk from leaving thousands of one-wide entries.
                Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
                _ => merged.push((lo, hi)),
            }
        }
        for &(lo, hi) in &merged {
            self.count += hi - lo + 1;
        }
        self.sup = merged;
    }
}

// --- sfnt / cmap decoding -----------------------------------------------------------------------

fn be16(b: &[u8], off: usize) -> Option<u32> {
    Some(u16::from_be_bytes(b.get(off..off + 2)?.try_into().ok()?) as u32)
}
fn be32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn read_at<R: Read + Seek>(r: &mut R, off: u64, len: usize) -> Result<Vec<u8>, CovErr> {
    // A malformed directory can claim a table of any length; refuse absurd ones rather than
    // trying to allocate them. No real cmap is anywhere near 32 MB (Noto Sans CJK's is 233 KB).
    if len > 32 << 20 {
        return Err(CovErr::Malformed("table length is implausible"));
    }
    r.seek(SeekFrom::Start(off))
        .map_err(|e| CovErr::Io(e.to_string()))?;
    let mut v = vec![0u8; len];
    r.read_exact(&mut v)
        .map_err(|e| CovErr::Io(e.to_string()))?;
    Ok(v)
}

/// Locate `cmap` in the sfnt directory and return its bytes. Handles a TrueType **collection**
/// (`ttcf`) by taking font 0: the shipped faces are not collections, but reading one as a plain
/// sfnt would otherwise fail with a misleading "not an sfnt".
fn cmap_bytes<R: Read + Seek>(r: &mut R) -> Result<Vec<u8>, CovErr> {
    let mut base = 0u64;
    let head = read_at(r, 0, 12)?;
    if &head[0..4] == b"ttcf" {
        let n = be32(&head, 8).ok_or(CovErr::Malformed("short ttc header"))?;
        if n == 0 {
            return Err(CovErr::Malformed("ttc with no fonts"));
        }
        let dir = read_at(r, 12, 4)?;
        base = be32(&dir, 0).ok_or(CovErr::Malformed("short ttc directory"))? as u64;
    }
    let head = read_at(r, base, 12)?;
    let tag = be32(&head, 0).ok_or(CovErr::Malformed("short sfnt header"))?;
    // 0x00010000 TrueType outlines, 'OTTO' CFF outlines, 'true' the old Apple spelling. The cmap
    // is in the same place in all three, which is the whole point of asking the directory.
    if tag != 0x0001_0000 && tag != 0x4F54_544F && tag != 0x7472_7565 {
        return Err(CovErr::Malformed("not an sfnt (bad version tag)"));
    }
    let num = be16(&head, 4).ok_or(CovErr::Malformed("short sfnt header"))? as usize;
    let dir = read_at(
        r,
        base + 12,
        num.checked_mul(16)
            .ok_or(CovErr::Malformed("absurd table count"))?,
    )?;
    for i in 0..num {
        let rec = &dir[i * 16..];
        if &rec[0..4] == b"cmap" {
            let off = be32(rec, 8).ok_or(CovErr::Malformed("short cmap record"))?;
            let len = be32(rec, 12).ok_or(CovErr::Malformed("short cmap record"))?;
            return read_at(r, base + off as u64, len as usize);
        }
    }
    Err(CovErr::Malformed("no cmap table"))
}

/// Is this (platform, encoding) pair a **Unicode** cmap? Platform 0 is Unicode by definition;
/// platform 3 (Windows) is Unicode only at encoding 1 (BMP) and 10 (full repertoire). Encoding
/// 3/0 is "symbol" - a private-use remapping that would report coverage the font cannot really
/// draw at the codepoints claimed - and platform 1 is Mac Roman. Both are skipped.
fn is_unicode(platform: u32, encoding: u32) -> bool {
    platform == 0 || (platform == 3 && (encoding == 1 || encoding == 10))
}

fn parse_format4(t: &[u8], cov: &mut Coverage) -> Option<()> {
    let seg2 = be16(t, 6)? as usize;
    let segs = seg2 / 2;
    let (end_at, start_at, delta_at, ro_at) = (14, 16 + seg2, 16 + 2 * seg2, 16 + 3 * seg2);
    for i in 0..segs {
        let end = be16(t, end_at + 2 * i)?;
        let start = be16(t, start_at + 2 * i)?;
        let delta = be16(t, delta_at + 2 * i)?;
        let ro = be16(t, ro_at + 2 * i)?;
        if start > end {
            continue;
        }
        for cp in start..=end {
            if cp == 0xFFFF {
                continue; // the mandatory terminating segment, never real coverage
            }
            let g = if ro == 0 {
                (cp.wrapping_add(delta)) & 0xFFFF
            } else {
                // The spec's pointer arithmetic: idRangeOffset is a byte offset FROM ITS OWN slot.
                let at = ro_at + 2 * i + ro as usize + 2 * (cp - start) as usize;
                match be16(t, at) {
                    Some(0) | None => 0,
                    Some(g) => (g.wrapping_add(delta)) & 0xFFFF,
                }
            };
            if g != 0 {
                cov.add(cp);
            }
        }
    }
    Some(())
}

fn parse_format6(t: &[u8], cov: &mut Coverage) -> Option<()> {
    let first = be16(t, 6)?;
    let n = be16(t, 8)? as usize;
    for i in 0..n {
        if be16(t, 10 + 2 * i)? != 0 {
            cov.add(first + i as u32);
        }
    }
    Some(())
}

/// Formats 12 (segmented) and 13 (many-to-one). Identical headers; they differ only in whether
/// `startGlyphID` advances across the group, which does not change WHICH codepoints are covered.
fn parse_format12_13(t: &[u8], many_to_one: bool, cov: &mut Coverage) -> Option<()> {
    let n = be32(t, 12)? as usize;
    // The bounds check is done in u64, and that is load-bearing rather than tidy: **`usize` is
    // 32 BITS on `arm-unknown-linux-gnueabi`**, the only target that ships. A `numGroups` of
    // 357_913_941 makes `n * 12` exactly 0xFFFF_FFFC, so `16 + n * 12` WRAPS to 12 - which is
    // ≤ `t.len()` - and the indexing below then panics out of the SDL main thread on the second
    // group. Release builds have overflow checks off, so it wraps silently rather than aborting.
    // No host test can catch this: `cargo test` runs on a 64-bit Mac where the same expression is
    // simply true. In u64 the product cannot overflow for any u32 count, on either width.
    if 16u64 + n as u64 * 12 > t.len() as u64 {
        return None;
    }
    for i in 0..n {
        let g = &t[16 + i * 12..];
        let (lo, hi, gid) = (be32(g, 0)?, be32(g, 4)?, be32(g, 8)?);
        if lo > hi || hi > 0x10FFFF {
            continue;
        }
        if gid == 0 {
            // Format 13 maps the whole group to one glyph, so gid 0 means the group is empty;
            // format 12 walks the glyph id, so only the first codepoint lands on notdef.
            if !many_to_one && lo < hi {
                cov.add_range(lo + 1, hi);
            }
            continue;
        }
        cov.add_range(lo, hi);
    }
    Some(())
}

/// Coverage of the font in `r`, unioned across every Unicode cmap subtable it carries. The test
/// seam: it takes any `Read + Seek`, which is what lets the decoder be driven from a `Cursor` over
/// a synthetic sfnt - shapes no shipped font has, like a cmap entry pointing at glyph 0.
/// Production reads a path and goes through [`of_file`], which closes the file sooner.
#[cfg(any(test, feature = "test-support"))]
pub fn of_reader<R: Read + Seek>(r: &mut R) -> Result<Coverage, CovErr> {
    decode(&cmap_bytes(r)?)
}

/// The decode half, split from the I/O half so [`of_file`] can CLOSE the file before doing the
/// slow part. The walk over a large format-4 subtable is milliseconds and the read is
/// microseconds; holding a descriptor across the difference is free to avoid and not free to keep
/// (`stream.rs`'s fd-leak assertion grades the process-wide count, and the host suite reads these
/// fonts from several tests at once).
fn decode(t: &[u8]) -> Result<Coverage, CovErr> {
    let n = be16(t, 2).ok_or(CovErr::Malformed("short cmap header"))? as usize;
    let mut cov = Coverage::new();
    let mut seen = 0usize;
    for i in 0..n {
        let rec = 4 + 8 * i;
        let (Some(plat), Some(enc), Some(off)) =
            (be16(&t, rec), be16(&t, rec + 2), be32(&t, rec + 4))
        else {
            break;
        };
        if !is_unicode(plat, enc) {
            continue;
        }
        let Some(sub) = t.get(off as usize..) else {
            continue;
        };
        let Some(format) = be16(sub, 0) else { continue };
        // Formats 0 (byte), 2 (high-byte CJK legacy) and 14 (variation selectors) are `None`, and
        // that is the point: `seen` counts subtables this decoder ACTUALLY READ, never ones it
        // skipped. Counting a skip made a font whose only Unicode subtables are unsupported decode
        // as `Ok(<empty coverage>)` instead of `Err`, and the two are not interchangeable
        // downstream: `text.rs::base_covers` falls back to "the face covers everything" only on
        // `Err`, and takes an empty `Ok` as authoritative. The result would have been that every
        // emoji-class character Inter does draw (the copyright sign, the star, the arrows) was
        // dropped from every label as uncovered, with no error anywhere.
        let decoded = match format {
            4 => parse_format4(sub, &mut cov),
            6 => parse_format6(sub, &mut cov),
            12 => parse_format12_13(sub, false, &mut cov),
            13 => parse_format12_13(sub, true, &mut cov),
            _ => None,
        };
        if decoded.is_some() {
            seen += 1;
        }
    }
    if seen == 0 {
        return Err(CovErr::Malformed("no usable Unicode cmap subtable"));
    }
    cov.seal();
    Ok(cov)
}

/// The errors do NOT name `path` - every caller has it and prefixes its own message with it, and
/// a doubled path reads as a bug in the error plumbing rather than as the missing file it is.
pub fn of_file(path: &Path) -> Result<Coverage, CovErr> {
    let cmap = {
        let mut f = File::open(path).map_err(|e| CovErr::Io(e.to_string()))?;
        cmap_bytes(&mut f)? // `f` is dropped HERE, before `decode` - see `decode`'s doc
    };
    decode(&cmap)
}

/// The shipped faces' coverage, parsed ONCE for the whole test binary.
///
/// Two reasons, both measured on the way in. A format-4 walk is the most expensive thing this
/// module does; and every `of_file` holds a descriptor, so tests reading the fonts in parallel
/// moved the PROCESS-wide descriptor count enough
/// to trip `stream.rs`'s fd-leak assertion - whose slack is +8 over whatever the rest of the suite
/// happens to be holding, and which then fails naming a leak that does not exist. Parsing once
/// turns the concurrent opens into one per face.
#[cfg(any(test, feature = "test-support"))]
pub fn shipped(name: &str) -> &'static Result<Coverage, String> {
    use std::sync::OnceLock;
    static REG: OnceLock<Result<Coverage, String>> = OnceLock::new();
    static BOLD: OnceLock<Result<Coverage, String>> = OnceLock::new();
    let slot = match name {
        "appfont.ttf" => &REG,
        "appfont-bold.ttf" => &BOLD,
        other => panic!("no shipped-font slot for {other:?} — add one beside the others"),
    };
    slot.get_or_init(|| {
        // The fonts are repository payload, not crate data - `pkg/` sits beside `rust-modules/`.
        let p = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../pkg")).join(name);
        of_file(&p).map_err(|e| format!("{}: {e}", p.display()))
    })
}

/// **The shipped faces' horizontal advances, as a host-test text measure.** Unit tests have no
/// SDL_ttf, so every other test `Measure` is a per-character guess - and a guess cannot say whether
/// a translated line fits its row. This one reads `cmap`, `head`, `hhea` and `hmtx` out of
/// `pkg/appfont*.ttf` and sums one advance per character, each rounded to a WHOLE pixel at the
/// requested size: the faces are opened with hinting on (`TTF_HINTING_LIGHT`), so FreeType hands
/// SDL2_ttf 2.0.x (the only versions any television ships) pixel-rounded advances, which it caches
/// per glyph (`FT_CEIL(horiAdvance)`), and `TTF_SizeUTF8` is a plain left-to-right sum of them. The
/// simulator's newer SDL_ttf sums fractional advances instead, which is why a sub-line the
/// simulator fits can still come out ellipsised on the television (2026-09-28, the Belarusian
/// Settings root).
#[cfg(any(test, feature = "test-support"))]
pub mod advances {
    use super::{be16, be32};
    use std::collections::HashMap;

    pub struct Face {
        upem: f32,
        glyph: HashMap<u32, u32>,
        advance: Vec<u32>,
    }

    fn table<'a>(font: &'a [u8], tag: &[u8; 4]) -> Option<&'a [u8]> {
        let n = be16(font, 4)? as usize;
        (0..n).find_map(|i| {
            let rec = font.get(12 + i * 16..12 + i * 16 + 16)?;
            if &rec[0..4] != tag {
                return None;
            }
            let (off, len) = (be32(rec, 8)? as usize, be32(rec, 12)? as usize);
            font.get(off..off + len)
        })
    }

    fn cmap(t: &[u8], out: &mut HashMap<u32, u32>) -> Option<()> {
        for i in 0..be16(t, 2)? as usize {
            let (plat, enc, off) = (be16(t, 4 + 8 * i)?, be16(t, 6 + 8 * i)?, be32(t, 8 + 8 * i)?);
            if !super::is_unicode(plat, enc) {
                continue;
            }
            let sub = t.get(off as usize..)?;
            match be16(sub, 0)? {
                4 => {
                    let seg2 = be16(sub, 6)? as usize;
                    let (end_at, start_at, delta_at, ro_at) = (14, 16 + seg2, 16 + 2 * seg2, 16 + 3 * seg2);
                    for s in 0..seg2 / 2 {
                        let (end, start) = (be16(sub, end_at + 2 * s)?, be16(sub, start_at + 2 * s)?);
                        let (delta, ro) = (be16(sub, delta_at + 2 * s)?, be16(sub, ro_at + 2 * s)?);
                        for cp in start..=end.min(0xFFFE) {
                            let g = if ro == 0 {
                                cp.wrapping_add(delta) & 0xFFFF
                            } else {
                                match be16(sub, ro_at + 2 * s + ro as usize + 2 * (cp - start) as usize) {
                                    Some(0) | None => 0,
                                    Some(g) => g.wrapping_add(delta) & 0xFFFF,
                                }
                            };
                            if g != 0 {
                                out.entry(cp).or_insert(g);
                            }
                        }
                    }
                }
                12 => {
                    for gi in 0..be32(sub, 12)? as usize {
                        let g = sub.get(16 + gi * 12..)?;
                        let (lo, hi, gid) = (be32(g, 0)?, be32(g, 4)?, be32(g, 8)?);
                        for cp in lo..=hi.min(0x10FFFF) {
                            out.entry(cp).or_insert(gid + (cp - lo));
                        }
                    }
                }
                _ => {}
            }
        }
        Some(())
    }

    impl Face {
        fn parse(font: &[u8]) -> Option<Face> {
            let upem = be16(table(font, b"head")?, 18)? as f32;
            let metrics = be16(table(font, b"hhea")?, 34)? as usize;
            let hmtx = table(font, b"hmtx")?;
            let advance = (0..metrics).map(|i| be16(hmtx, 4 * i)).collect::<Option<Vec<_>>>()?;
            let mut glyph = HashMap::new();
            cmap(table(font, b"cmap")?, &mut glyph)?;
            Some(Face { upem, glyph, advance })
        }

        /// `s` at pixel size `size`, one whole-pixel advance per character (see the item doc).
        pub fn width(&self, s: &str, size: i32) -> f32 {
            s.chars()
                .map(|ch| {
                    let g = self.glyph.get(&(ch as u32)).copied().unwrap_or(0) as usize;
                    // Glyphs past `numberOfHMetrics` share the last advance (the `hmtx` rule).
                    let units = self.advance.get(g).or(self.advance.last()).copied().unwrap_or(0);
                    (units as f32 * size as f32 / self.upem).round()
                })
                .sum()
        }
    }

    /// One of the two shipped text faces, parsed once for the test binary.
    pub fn shipped(bold: bool) -> &'static Face {
        use std::sync::OnceLock;
        static REG: OnceLock<Face> = OnceLock::new();
        static BOLD: OnceLock<Face> = OnceLock::new();
        let (slot, name) = if bold { (&BOLD, "appfont-bold.ttf") } else { (&REG, "appfont.ttf") };
        slot.get_or_init(|| {
            let p = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../pkg")).join(name);
            let bytes = std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            Face::parse(&bytes).unwrap_or_else(|| panic!("{}: unreadable metrics tables", p.display()))
        })
    }

    /// The text measure over the shipped faces' real advances: `tv_machine::machine::Measure` is
    /// implemented for it beside the trait, in `machine/src/machine.rs` (the lowest layer that names both
    /// the trait and this type), and the share of a column a line may fill under it is
    /// `ui::fit::HEADROOM`, which the callers name.
    pub struct ShippedMeasure;
}

// --- emoji, as a renderer without a shaper can draw them --------------------------------------

/// Codepoints that only ever make up an EMOJI: `Extended_Pictographic` (Unicode 16
/// `emoji-data.txt`, coalesced), regional indicators, the skin-tone modifiers, the variation
/// selectors U+FE0E/U+FE0F, ZWJ, the keycap mark U+20E3 and the tag characters. Sorted, disjoint.
const EMOJI_PARTS: &[(u32, u32)] = &[
    (0xA9, 0xA9), (0xAE, 0xAE), (0x200D, 0x200D), (0x203C, 0x203C), (0x2049, 0x2049),
    (0x20E3, 0x20E3), (0x2122, 0x2122), (0x2139, 0x2139), (0x2194, 0x2199), (0x21A9, 0x21AA),
    (0x231A, 0x231B), (0x2328, 0x2328), (0x2388, 0x2388), (0x23CF, 0x23CF), (0x23E9, 0x23F3),
    (0x23F8, 0x23FA), (0x24C2, 0x24C2), (0x25AA, 0x25AB), (0x25B6, 0x25B6), (0x25C0, 0x25C0),
    (0x25FB, 0x25FE), (0x2600, 0x2605), (0x2607, 0x2612), (0x2614, 0x2685), (0x2690, 0x2705),
    (0x2708, 0x2712), (0x2714, 0x2714), (0x2716, 0x2716), (0x271D, 0x271D), (0x2721, 0x2721),
    (0x2728, 0x2728), (0x2733, 0x2734), (0x2744, 0x2744), (0x2747, 0x2747), (0x274C, 0x274C),
    (0x274E, 0x274E), (0x2753, 0x2755), (0x2757, 0x2757), (0x2763, 0x2767), (0x2795, 0x2797),
    (0x27A1, 0x27A1), (0x27B0, 0x27B0), (0x27BF, 0x27BF), (0x2934, 0x2935), (0x2B05, 0x2B07),
    (0x2B1B, 0x2B1C), (0x2B50, 0x2B50), (0x2B55, 0x2B55), (0x3030, 0x3030), (0x303D, 0x303D),
    (0x3297, 0x3297), (0x3299, 0x3299), (0xFE0E, 0xFE0F), (0x1F000, 0x1F0FF), (0x1F10D, 0x1F10F),
    (0x1F12F, 0x1F12F), (0x1F16C, 0x1F171), (0x1F17E, 0x1F17F), (0x1F18E, 0x1F18E),
    (0x1F191, 0x1F19A), (0x1F1AD, 0x1F1FF), (0x1F201, 0x1F20F), (0x1F21A, 0x1F21A),
    (0x1F22F, 0x1F22F), (0x1F232, 0x1F23A), (0x1F23C, 0x1F23F), (0x1F249, 0x1F53D),
    (0x1F546, 0x1F64F), (0x1F680, 0x1F6FF), (0x1F774, 0x1F77F), (0x1F7D5, 0x1F7FF),
    (0x1F80C, 0x1F80F), (0x1F848, 0x1F84F), (0x1F85A, 0x1F85F), (0x1F888, 0x1F88F),
    (0x1F8AE, 0x1F8FF), (0x1F90C, 0x1F93A), (0x1F93C, 0x1F945), (0x1F947, 0x1FAFF),
    (0x1FC00, 0x1FFFD), (0xE0020, 0xE007F),
];

/// Is `cp` part of an emoji (see [`EMOJI_PARTS`])?
pub fn is_emoji_part(cp: u32) -> bool {
    let k = EMOJI_PARTS.partition_point(|&(lo, _)| lo <= cp);
    k > 0 && EMOJI_PARTS[k - 1].1 >= cp
}

/// The sequence marks: ZWJ, VS15/VS16, the keycap mark, regional indicators, skin-tone modifiers
/// and tags. They only mean something to a shaper (GSUB ligatures), and SDL2_ttf 2.0.x has none,
/// so the text path never draws them.
fn is_emoji_mark(cp: u32) -> bool {
    matches!(cp, 0x200D | 0xFE0E | 0xFE0F | 0x20E3 | 0x1F1E6..=0x1F1FF | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F)
}

/// **`s` with its emoji spelled the way a renderer without a shaper can draw them**, or `None` when
/// it is drawn as spelled (the common case, and every ASCII string, which returns before a lookup).
///
/// A face draws single glyphs at best; everything an emoji font builds with GSUB ligatures is
/// rewritten first:
///
/// - a **flag** (a regional-indicator pair) becomes its two-letter region code, "🇩🇪" -> "DE";
/// - a **ZWJ sequence** draws its FIRST pictograph ("👨‍👩‍👧" -> "👨"): the components side by side
///   read as three separate emoji, and are three times as wide, where the first one is the subject;
/// - a **keycap** ("1️⃣") draws its digit: VS16 and U+20E3 go;
/// - skin-tone modifiers, VS15/VS16 and tag characters go (a subdivision flag keeps its 🏴).
///
/// Then emoji parts `covers` (the caller's face) says it lacks are dropped rather than
/// boxed, and the whitespace that separated them collapses: a gap becomes one space, and a gap at
/// either end of the string or a line goes. Anything that is not an emoji part (Hebrew, say) keeps
/// its honest tofu box (see this module's header for why a box is the right answer for text).
pub fn simplify_emoji(s: &str, covers: impl Fn(u32) -> bool) -> Option<String> {
    if s.is_ascii() || !s.chars().any(|c| is_emoji_part(c as u32)) {
        return None;
    }
    let ri = |c: char| (0x1F1E6..=0x1F1FF).contains(&(c as u32));
    let pictograph = |c: char| is_emoji_part(c as u32) && !is_emoji_mark(c as u32);
    // Pass 1: flags to letters, a ZWJ sequence to its first pictograph.
    let mut spelled: Vec<char> = Vec::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match it.peek().copied() {
            Some(n) if ri(c) && ri(n) => {
                it.next();
                for r in [c, n] {
                    spelled.push(char::from(b'A' + (r as u32 - 0x1F1E6) as u8));
                }
            }
            // U+200D joins emoji only between pictographs (the previous one may carry marks);
            // between letters (Devanagari, Malayalam) it is a shaping control and is left alone.
            Some(n) if c == '\u{200D}' && pictograph(n)
                && spelled.iter().rev().find(|&&p| !is_emoji_mark(p as u32)).is_some_and(|&p| pictograph(p)) =>
            {
                it.next();
            }
            _ => spelled.push(c),
        }
    }
    // Pass 2: drop marks and uncovered pictographs, collapsing the gap they leave. `gap`: the
    // whitespace since the last kept non-space; `cut`: an emoji was dropped in it. A line break is
    // an edge, like either end of the string, so multi-line text (a subtitle cue) keeps its lines.
    // A ZWJ left after pass 1 is not joining emoji, so it goes only when no face has it.
    let gone = |c: char| {
        let cp = c as u32;
        is_emoji_part(cp) && ((is_emoji_mark(cp) && c != '\u{200D}') || !covers(cp))
    };
    let line_break = |c: char| c == '\n' || c == '\r';
    let mut out = String::with_capacity(s.len());
    let (mut gap, mut cut) = (String::new(), false);
    for c in spelled {
        if gone(c) {
            cut = true;
        } else if c.is_whitespace() && !line_break(c) {
            gap.push(c);
        } else {
            let edge = line_break(c) || out.is_empty() || out.ends_with(line_break);
            match (cut, gap.chars().next()) {
                (false, _) => out.push_str(&gap),
                (true, Some(sp)) if !edge => out.push(sp),
                _ => {}
            }
            gap.clear();
            cut = false;
            out.push(c);
        }
    }
    if !cut {
        out.push_str(&gap);
    }
    (out != s).then_some(out)
}

// ------------------------------------------------------------------------------------------------
// THE GATE. Everything below runs in `make check`.
// ------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn cov(name: &str) -> &'static Coverage {
        shipped(name).as_ref().unwrap_or_else(|e| panic!("{e}"))
    }

    /// Every codepoint in `lo..=hi` that the coverage is missing, capped so a totally absent
    /// block reports as a block rather than eleven thousand lines.
    fn missing(c: &Coverage, lo: u32, hi: u32) -> Vec<u32> {
        (lo..=hi).filter(|&cp| !c.contains(cp)).take(12).collect()
    }

    fn count(c: &Coverage, lo: u32, hi: u32) -> u32 {
        (lo..=hi).filter(|&cp| c.contains(cp)).count() as u32
    }

    // --- the declared set ------------------------------------------------------------------------
    //
    // `appfont.ttf` / `appfont-bold.ttf` (Inter, cut by tools/cut-inter.py). Every codepoint the
    // product chrome itself can emit, plus the scripts the playlists and guides this player is
    // built for actually contain. Whole blocks, not samples: a partial block is how "Cyrillic
    // works" and "one letter is a box" coexist.
    const DECLARED_BLOCKS: &[(&str, u32, u32)] = &[
        ("ASCII printable", 0x0020, 0x007E),
        ("Latin-1 letters", 0x00C0, 0x00FF),
        // Split around U+0149 (ŉ), which Inter does not carry and should not: Unicode DEPRECATED
        // it, no locale produces it, and it decomposes to U+02BC U+006E. Excluding it here is the
        // difference between a declared set and a wish list - every other position is asserted.
        ("Latin Extended-A (before U+0149)", 0x0100, 0x0148),
        ("Latin Extended-A (after U+0149)", 0x014A, 0x017E),
        // U+03A2 is a permanent hole in Unicode itself (reserved between Ρ and Σ), so the block
        // is split around it for the same reason as U+0149 above.
        ("Greek and Coptic (before U+03A2)", 0x0391, 0x03A1),
        ("Greek and Coptic (after U+03A2)", 0x03A3, 0x03CE),
        ("Cyrillic", 0x0400, 0x045F),
        // The regression that named this whole unit. Subtitle convention wraps a sung line in
        // these four; Inter has none of them, and `tools/cut-inter.py` synthesizes them. A panel
        // capture of the Family Guy theme read "▯NO GLYPH▯ It seems today" before it did.
        ("musical symbols (subtitle convention)", 0x2669, 0x266C),
    ];

    /// Individual codepoints the face must carry: the typographic punctuation the UI composes by
    /// hand (`elide` appends U+2026; headings join with U+00B7; PMS metadata is full of curly
    /// quotes and en/em dashes) and the marks drawn beside ratings.
    const DECLARED_CHARS: &[(&str, char)] = &[
        ("ellipsis (elide appends it)", '\u{2026}'),
        ("middle dot (heading separator)", '\u{00B7}'),
        ("bullet", '\u{2022}'),
        ("en dash", '\u{2013}'),
        ("em dash", '\u{2014}'),
        ("left single quote", '\u{2018}'),
        ("right single quote / apostrophe", '\u{2019}'),
        ("left double quote", '\u{201C}'),
        ("right double quote", '\u{201D}'),
        ("multiplication sign", '\u{00D7}'),
        ("degree sign", '\u{00B0}'),
        ("no-break space", '\u{00A0}'),
        ("copyright", '\u{00A9}'),
        ("registered", '\u{00AE}'),
    ];

    /// Real titles, in the scripts the face claims. A block assertion cannot catch a font whose
    /// cmap is fine and whose *combination* is not, and these read as evidence in a PR body.
    const SAMPLES: &[(&str, &str)] = &[
        ("Russian", "Ирония судьбы, или С лёгким паром!"),
        // Ukrainian's own letters: Ye, I, Yi and Ghe with upturn.
        ("Ukrainian", "\u{0404}\u{0432}\u{0440}\u{043E} \u{0456} \u{0407}\u{0436}\u{0430}\u{043A} \u{0490}\u{0430}\u{043D}\u{043E}\u{043A} \u{0491}"),
        // Polish: the nine letters with diacritics, both cases.
        ("Polish", "\u{0104}\u{0106}\u{0118}\u{0141}\u{0143}\u{00D3}\u{015A}\u{0179}\u{017B} \u{0105}\u{0107}\u{0119}\u{0142}\u{0144}\u{00F3}\u{015B}\u{017A}\u{017C}"),
        ("Greek", "Ο Θίασος"),
        ("Latin with diacritics", "Amélie · Das Boot · Coração"),
        ("subtitle sung line", "\u{266A} It seems today \u{266B}"),
    ];

    /// Report EVERY gap in one run, not the first. A fail-fast gate over eight blocks and
    /// fourteen characters makes a font swap an eight-iteration guessing game, which is how a
    /// re-cut ends up half-checked.
    fn report(font: &str, gaps: Vec<String>) {
        assert!(
            gaps.is_empty(),
            "pkg/{font} does not cover the declared set:\n  {}",
            gaps.join("\n  ")
        );
    }

    #[test]
    fn shipped_regular_font_covers_the_declared_latin_set() {
        let c = cov("appfont.ttf");
        let mut gaps = Vec::new();
        for &(name, lo, hi) in DECLARED_BLOCKS {
            let m = missing(&c, lo, hi);
            if !m.is_empty() {
                gaps.push(format!(
                    "{name} (U+{lo:04X}..U+{hi:04X}) is missing {m:04X?}"
                ));
            }
        }
        for &(name, ch) in DECLARED_CHARS {
            if !c.contains(ch as u32) {
                gaps.push(format!("{name} (U+{:04X})", ch as u32));
            }
        }
        report("appfont.ttf", gaps);
    }

    /// The bold face is a separate cut and can rot on its own. A bold-only hole is invisible in
    /// every other tier - the same string renders fine as body text and tofus as a heading.
    #[test]
    fn bold_face_covers_exactly_what_the_regular_one_does() {
        let (r, b) = (cov("appfont.ttf"), cov("appfont-bold.ttf"));
        assert_eq!(
            r.len(),
            b.len(),
            "appfont.ttf covers {} codepoints, appfont-bold.ttf {}",
            r.len(),
            b.len()
        );
        for &(name, lo, hi) in DECLARED_BLOCKS {
            for cp in lo..=hi {
                assert_eq!(
                    r.contains(cp),
                    b.contains(cp),
                    "regular/bold disagree on U+{cp:04X} ({name})"
                );
            }
        }
        for &(name, ch) in DECLARED_CHARS {
            assert!(
                b.contains(ch as u32),
                "pkg/appfont-bold.ttf is missing {name} (U+{:04X})",
                ch as u32
            );
        }
    }

    /// Table (offset, length) for `tag` in a plain sfnt, by seek.
    fn table_span(f: &mut File, tag: &[u8; 4]) -> (u64, u64) {
        let head = read_at(f, 0, 12).unwrap();
        let num = be16(&head, 4).unwrap() as usize;
        let dir = read_at(f, 12, num * 16).unwrap();
        for i in 0..num {
            let rec = &dir[i * 16..];
            if &rec[0..4] == tag {
                return (be32(rec, 8).unwrap() as u64, be32(rec, 12).unwrap() as u64);
            }
        }
        panic!("no {} table", String::from_utf8_lossy(tag));
    }

    /// **Every shipped face must open under the OLDEST FreeType a television ships.** FreeType
    /// 2.4.11 through 2.7.0 reject a font whose `loca` is `>= 0x40000` bytes (`ttpload.c`,
    /// `table_len >= 0x40000L` -> `Invalid_Table`); a long `loca` holds `numGlyphs + 1` four-byte
    /// offsets, so 65535 glyphs is exactly 0x40000 and fails with error 8 while 65534 opens.
    /// SDL2_ttf then reports a face that "cannot be opened" and the app falls back to DroidSans.
    /// No host FreeType here is that old, so the bound is checked on the bytes.
    #[test]
    fn shipped_faces_open_under_old_freetype() {
        for name in ["appfont.ttf", "appfont-bold.ttf"] {
            let p = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../pkg")).join(name);
            let mut f = File::open(&p).unwrap();
            let (maxp, _) = table_span(&mut f, b"maxp");
            let n = be16(&read_at(&mut f, maxp + 4, 2).unwrap(), 0).unwrap();
            let (head, _) = table_span(&mut f, b"head");
            let fmt = be16(&read_at(&mut f, head + 50, 2).unwrap(), 0).unwrap();
            let (_, loca_len) = table_span(&mut f, b"loca");
            assert!(n <= 65_534, "{name}: {n} glyphs; FreeType <= 2.7.0 needs <= 65534");
            if fmt == 1 {
                assert!(
                    loca_len < 0x40000,
                    "{name}: long loca is {loca_len} bytes; FreeType 2.4.11-2.7.0 rejects >= 0x40000"
                );
            }
        }
    }

    /// The whole point: real strings, not blocks.
    #[test]
    fn the_shipped_face_can_draw_every_sample_string() {
        for face in ["appfont.ttf", "appfont-bold.ttf"] {
            let c = cov(face);
            for &(script, s) in SAMPLES {
                let bad: Vec<char> = s.chars().filter(|&ch| !c.contains(ch as u32)).collect();
                assert!(bad.is_empty(), "{script}: pkg/{face} cannot draw {bad:?} in {s:?}");
            }
        }
    }

    /// **RTL is out of scope, and this is where that is written down as a test rather than a
    /// comment.** The shipped face carries no Hebrew or Arabic, so the app cannot draw them.
    ///
    /// Coverage would not be enough even if it existed. "The font contains the codepoint" is not
    /// "the renderer supports the script": Arabic needs joining and contextual shaping, Hebrew
    /// needs bidi reordering, and `text.rs` has neither - it hands byte runs to
    /// `TTF_RenderUTF8_Blended`, which in SDL2_ttf 2.0.x is a left-to-right advance loop with no
    /// shaper behind it. Adding a face here would turn tofu into *fluent-looking wrong text*,
    /// which is worse.
    ///
    /// So this test asserts the ABSENCE, deliberately: it fails the moment someone adds an RTL
    /// face, and the failure message is the reason not to. **Checklist #6 / #48 must not be marked
    /// Pass for RTL content on the strength of this unit.**
    #[test]
    fn rtl_is_out_of_scope_and_stays_that_way() {
        for name in ["appfont.ttf", "appfont-bold.ttf"] {
            let c = cov(name);
            for (script, lo, hi) in [("Hebrew", 0x05D0u32, 0x05EAu32), ("Arabic", 0x0620, 0x064A)] {
                let n = count(&c, lo, hi);
                assert_eq!(
                    n, 0,
                    "pkg/{name} now carries {n} {script} codepoints. Coverage is NOT support: \
                     text.rs does no bidi and no shaping, so this would render fluent-looking \
                     wrong text instead of tofu. Ship a shaper before you ship the face, and do \
                     not mark checklist #6/#48 Pass for RTL until you have."
                );
            }
        }
    }

    /// The emoji table against characters whose class is not in doubt, and the whitespace rule.
    #[test]
    fn emoji_parts_are_classified_and_dropped_with_their_gap() {
        for c in ['\u{1F525}', '\u{1F1E9}', '\u{1F3FD}', '\u{FE0F}', '\u{200D}', '\u{20E3}', '\u{E007F}', '\u{26A1}', '\u{2B50}'] {
            assert!(is_emoji_part(c as u32), "U+{:04X}", c as u32);
        }
        for c in ['a', '1', ' ', '\u{2026}', '\u{00E9}', '\u{05D0}', '\u{4E00}', '\u{2606}', '\u{1F3FA}' as char] {
            assert_eq!(is_emoji_part(c as u32), c == '\u{1F3FA}', "U+{:04X}", c as u32);
        }
        let none = |_| false;
        assert_eq!(simplify_emoji("plain ascii", none), None);
        assert_eq!(simplify_emoji("Amélie", none), None, "nothing to drop is not a copy");
        assert_eq!(simplify_emoji("\u{1F525}  Top  10 \u{1F525} ", none).as_deref(), Some("Top  10"));
        assert_eq!(simplify_emoji("A \u{1F525}\u{FE0F} B", none).as_deref(), Some("A B"));
        assert_eq!(simplify_emoji("A\u{1F525}B", none).as_deref(), Some("AB"));
        assert_eq!(
            simplify_emoji("\u{1F3B5} la la \u{1F3B5}\r\n\u{1F3B5} la\nend \u{1F3B5}", none).as_deref(),
            Some("la la\r\nla\nend"),
            "a line break is an edge: lines are kept, the gaps beside the emoji go"
        );
        // a covered emoji-class character is kept, spaces and all
        let star = |cp| cp == 0x2605;
        assert_eq!(simplify_emoji("\u{2605} 8 \u{1F525}", star).as_deref(), Some("\u{2605} 8"));
    }

    /// The sequences only a shaper can ligate, rewritten into what single glyphs can say. `covers`
    /// here says yes to every pictograph, so only the rewriting is under test.
    #[test]
    fn emoji_sequences_are_spelled_for_a_renderer_without_a_shaper() {
        let all = |cp| !is_emoji_mark(cp);
        let s = |t: &str| simplify_emoji(t, all).unwrap_or_else(|| t.to_owned());
        assert_eq!(s("\u{1F1E9}\u{1F1EA} Deutsch"), "DE Deutsch", "a flag is its region code");
        assert_eq!(s("\u{1F1EC}\u{1F1E7}\u{1F1FA}\u{1F1F8}"), "GBUS", "pairs pair left to right");
        assert_eq!(s("x \u{1F1E9} y"), "x y", "a lone regional indicator is nothing");
        assert_eq!(s("Kids \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} Mix"), "Kids \u{1F468} Mix", "ZWJ: the first pictograph");
        assert_eq!(s("\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}"), "\u{1F3F3}", "VS16 before the joiner");
        assert_eq!(s("\u{1F44B}\u{1F3FD}\u{200D}\u{2642}\u{FE0F}!"), "\u{1F44B}!", "a skin tone before the joiner");
        assert_eq!(s("Season 1\u{FE0F}\u{20E3}"), "Season 1", "a keycap is its digit");
        assert_eq!(s("#\u{FE0F}\u{20E3}1"), "#1");
        assert_eq!(s("\u{1F3F4}\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F} Scotland"), "\u{1F3F4} Scotland");
        assert_eq!(s("[\u{1F6F0}\u{FE0F}] SKY 1"), "[\u{1F6F0}] SKY 1");
        assert_eq!(simplify_emoji("[\u{1F3E0}] SKY 1", all), None, "a bare pictograph is drawn as spelled");
        // ZWJ between letters is a shaping control, not an emoji joiner
        assert_eq!(simplify_emoji("\u{0915}\u{094D}\u{200D}\u{0937} \u{1F525}", |_| true), None);
    }

    // --- decoder unit tests, on synthetic sfnts -------------------------------------------------

    /// A minimal one-table sfnt wrapping `cmap`, so the decoder can be exercised on shapes no
    /// shipped font has (a notdef mapping, a format 12 group, a truncated table).
    fn sfnt(cmap: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        v.extend_from_slice(&1u16.to_be_bytes()); // numTables
        v.extend_from_slice(&[0; 6]); // searchRange/entrySelector/rangeShift
        v.extend_from_slice(b"cmap");
        v.extend_from_slice(&0u32.to_be_bytes()); // checksum
        v.extend_from_slice(&28u32.to_be_bytes()); // offset (12 + 16)
        v.extend_from_slice(&(cmap.len() as u32).to_be_bytes());
        v.extend_from_slice(cmap);
        v
    }

    /// cmap header + one subtable record pointing at `sub`.
    fn cmap1(platform: u16, encoding: u16, sub: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&0u16.to_be_bytes());
        v.extend_from_slice(&1u16.to_be_bytes());
        v.extend_from_slice(&platform.to_be_bytes());
        v.extend_from_slice(&encoding.to_be_bytes());
        v.extend_from_slice(&12u32.to_be_bytes());
        v.extend_from_slice(sub);
        v
    }

    fn format12(groups: &[(u32, u32, u32)]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&12u16.to_be_bytes());
        v.extend_from_slice(&0u16.to_be_bytes()); // reserved
        v.extend_from_slice(&(16 + 12 * groups.len() as u32).to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes()); // language
        v.extend_from_slice(&(groups.len() as u32).to_be_bytes());
        for &(lo, hi, gid) in groups {
            v.extend_from_slice(&lo.to_be_bytes());
            v.extend_from_slice(&hi.to_be_bytes());
            v.extend_from_slice(&gid.to_be_bytes());
        }
        v
    }

    /// A cmap entry pointing at glyph 0 is a mapping to `.notdef`, the tofu box itself. Counting
    /// it as coverage is exactly the bug this module exists to prevent: the gate would pass a face
    /// that draws a box, and an emoji it lacks would be drawn as one instead of dropped.
    #[test]
    fn a_mapping_to_glyph_zero_is_not_coverage() {
        let f = sfnt(&cmap1(
            3,
            10,
            &format12(&[(0x41, 0x43, 0), (0x61, 0x62, 7)]),
        ));
        let c = of_reader(&mut Cursor::new(f)).expect("decodes");
        assert!(!c.contains(0x41), "U+0041 maps to glyph 0");
        assert!(
            c.contains(0x42) && c.contains(0x43),
            "the rest of the group walks off glyph 1, 2"
        );
        assert!(c.contains(0x61) && c.contains(0x62));
        assert_eq!(c.len(), 4);
    }

    /// Astral coverage lives in a range list, not the bitset. `partition_point` has an off-by-one
    /// on both edges of every range, and the whole emoji plane rides on it.
    #[test]
    fn supplementary_plane_ranges_answer_at_both_edges() {
        let f = sfnt(&cmap1(
            3,
            10,
            &format12(&[(0x2_0000, 0x2_A6DF, 1), (0x1_F600, 0x1_F64F, 9)]),
        ));
        let c = of_reader(&mut Cursor::new(f)).expect("decodes");
        for cp in [0x2_0000, 0x2_A6DF, 0x1_F600, 0x1_F64F, 0x2_5000] {
            assert!(c.contains(cp), "U+{cp:05X} is inside a declared group");
        }
        for cp in [0x1_FFFF, 0x2_A6E0, 0x1_F5FF, 0x1_F650] {
            assert!(
                !c.contains(cp),
                "U+{cp:05X} is outside every declared group"
            );
        }
        assert_eq!(
            c.len(),
            (0x2_A6DF - 0x2_0000 + 1) + (0x1_F64F - 0x1_F600 + 1)
        );
    }

    /// Format 4's second addressing mode (`idRangeOffset != 0`) indexes a trailing glyph array by
    /// pointer arithmetic relative to its own slot. It is the mode every real font uses for its
    /// sparse segments, and getting the base wrong silently reports a plausible WRONG set.
    #[test]
    fn format4_honours_the_id_range_offset_indirection() {
        // one real segment U+0041..U+0043 through glyphIdArray, plus the mandatory 0xFFFF segment
        let mut sub: Vec<u8> = Vec::new();
        let seg2 = 4u16;
        sub.extend_from_slice(&4u16.to_be_bytes()); // format
        sub.extend_from_slice(&0u16.to_be_bytes()); // length (unused by the decoder)
        sub.extend_from_slice(&0u16.to_be_bytes()); // language
        sub.extend_from_slice(&seg2.to_be_bytes());
        sub.extend_from_slice(&[0; 6]); // searchRange/entrySelector/rangeShift
        sub.extend_from_slice(&0x0043u16.to_be_bytes()); // endCode[0]
        sub.extend_from_slice(&0xFFFFu16.to_be_bytes()); // endCode[1]
        sub.extend_from_slice(&0u16.to_be_bytes()); // reservedPad
        sub.extend_from_slice(&0x0041u16.to_be_bytes()); // startCode[0]
        sub.extend_from_slice(&0xFFFFu16.to_be_bytes()); // startCode[1]
        sub.extend_from_slice(&0u16.to_be_bytes()); // idDelta[0]
        sub.extend_from_slice(&1u16.to_be_bytes()); // idDelta[1]
                                                    // idRangeOffset[0] must skip the rest of the array (2 entries * 2 bytes = 4) to reach
                                                    // glyphIdArray; entry 1 is the terminator and addresses nothing.
        sub.extend_from_slice(&4u16.to_be_bytes());
        sub.extend_from_slice(&0u16.to_be_bytes());
        sub.extend_from_slice(&5u16.to_be_bytes()); // glyphIdArray: U+0041 -> 5
        sub.extend_from_slice(&0u16.to_be_bytes()); //               U+0042 -> notdef
        sub.extend_from_slice(&6u16.to_be_bytes()); //               U+0043 -> 6
        let c = of_reader(&mut Cursor::new(sfnt(&cmap1(3, 1, &sub)))).expect("decodes");
        assert!(c.contains(0x41) && c.contains(0x43));
        assert!(!c.contains(0x42), "U+0042 indexes a zero in glyphIdArray");
        assert!(
            !c.contains(0xFFFF),
            "the terminating segment is not coverage"
        );
        assert_eq!(c.len(), 2);
    }

    /// A symbol cmap (3,0) remaps into the private-use area; treating it as Unicode would claim
    /// coverage of U+F020.. that no caller can ever ask for, and - worse - could make a garbage
    /// font look like it covers the declared set.
    #[test]
    fn non_unicode_subtables_are_ignored() {
        let f = sfnt(&cmap1(3, 0, &format12(&[(0xF020, 0xF0FF, 1)])));
        let err = of_reader(&mut Cursor::new(f))
            .err()
            .expect("a symbol-only cmap has no Unicode coverage");
        assert_eq!(err, CovErr::Malformed("no usable Unicode cmap subtable"));
    }

    /// Truncation must be an error, never a panic. `text.rs` calls this on a file the installer
    /// wrote, which is exactly where a short read comes from - and it runs on the SDL thread.
    #[test]
    fn a_truncated_font_is_an_error_not_a_panic() {
        let full = sfnt(&cmap1(3, 10, &format12(&[(0x41, 0x5A, 1)])));
        for cut in [0, 4, 11, 20, 30, full.len() - 1] {
            let r = of_reader(&mut Cursor::new(full[..cut].to_vec()));
            assert!(
                r.is_err(),
                "a font truncated to {cut} bytes decoded successfully"
            );
        }
        // ...and a group count the table cannot hold must not be trusted either. The last two are
        // the values that WRAP a 32-bit `usize`, which is the width the shipped ARM binary uses:
        // 12 × 357_913_941 is 0xFFFF_FFFC, so `16 + n * 12` becomes 12 in 32-bit arithmetic and
        // sails through a naive bounds check. This assertion cannot fail on the 64-bit dev Mac
        // whichever way `parse_format12_13` is written - it is here to pin the intent and to fail
        // for anyone who runs the suite on a 32-bit host. The guarantee itself comes from doing
        // the arithmetic in u64; see the comment at the check.
        for groups in [9999u32, 357_913_941, 357_913_942, u32::MAX] {
            let mut lying = format12(&[(0x41, 0x5A, 1)]);
            lying[12..16].copy_from_slice(&groups.to_be_bytes());
            let r = of_reader(&mut Cursor::new(sfnt(&cmap1(3, 10, &lying))));
            assert!(
                r.is_err(),
                "a format 12 claiming {groups} groups in 28 bytes decoded successfully"
            );
        }
    }

    /// A font whose only Unicode cmap subtable is one this decoder does not read must be an
    /// **error**, never an empty success. `text.rs::base_covers` falls back to "the face covers
    /// everything" on `Err` and treats `Ok` as authoritative, so an empty `Ok` for `appfont.ttf`
    /// would drop every emoji-class character Inter does draw (the copyright sign, the star) from
    /// every label, with nothing logged.
    #[test]
    fn a_font_with_only_unsupported_subtables_is_an_error_not_empty_coverage() {
        // format 0: a 256-byte byte-encoding table. Unicode platform, so it is not skipped as
        // non-Unicode - it is skipped because the decoder does not read format 0.
        let mut sub = vec![0u8; 262];
        sub[0..2].copy_from_slice(&0u16.to_be_bytes()); // format 0
        sub[2..4].copy_from_slice(&262u16.to_be_bytes());
        for (i, b) in sub.iter_mut().enumerate().skip(6) {
            *b = (i - 6) as u8; // every byte maps to a non-zero glyph
        }
        let err = of_reader(&mut Cursor::new(sfnt(&cmap1(3, 1, &sub))))
            .err()
            .expect("a format-0-only cmap is not coverage this decoder can vouch for");
        assert_eq!(err, CovErr::Malformed("no usable Unicode cmap subtable"));
    }
}
