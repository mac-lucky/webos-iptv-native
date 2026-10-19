//! Which glibc symbol versions an ARM binary needs, read from its ELF version tables.
//!
//! The NDK links against a glibc 2.12 sysroot, but Rust's std and the TV's own libraries in that
//! sysroot can still ask for newer symbol versions. The loader refuses to start a binary that
//! needs a version the firmware's libc lacks, so the build fails here instead of on the TV.

use std::collections::BTreeMap;
use std::path::Path;

use crate::Result;

/// The oldest firmware glibc the binary must run on: LG C1, webOS 6.5.3.
const MAX_GLIBC: (u32, u32) = (2, 28);

const SHT_DYNAMIC: u32 = 6;
const SHT_DYNSYM: u32 = 11;
const SHT_GNU_VERNEED: u32 = 0x6fff_fffe;
const SHT_GNU_VERSYM: u32 = 0x6fff_ffff;
const DT_NEEDED: u32 = 1;
const DT_SONAME: u32 = 14;

struct Elf<'a>(&'a [u8]);

struct Section {
    kind: u32,
    offset: usize,
    size: usize,
    link: usize,
    info: usize,
}

impl Elf<'_> {
    fn u16(&self, at: usize) -> Result<u16> {
        let b = self.0.get(at..at + 2).ok_or("truncated ELF")?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&self, at: usize) -> Result<u32> {
        let b = self.0.get(at..at + 4).ok_or("truncated ELF")?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn str(&self, table: &Section, at: u32) -> Result<String> {
        let start = table.offset + at as usize;
        let bytes = self
            .0
            .get(start..table.offset + table.size)
            .ok_or("string out of range")?;
        let end = bytes
            .iter()
            .position(|&b| b == 0)
            .ok_or("unterminated string")?;
        Ok(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }

    fn sections(&self) -> Result<Vec<Section>> {
        if self.0.get(..7) != Some(b"\x7fELF\x01\x01\x01") || self.u16(18)? != 40 {
            return Err("not a little-endian 32-bit ARM ELF".into());
        }
        let (shoff, shentsize, shnum) = (
            self.u32(0x20)? as usize,
            self.u16(0x2e)? as usize,
            self.u16(0x30)?,
        );
        (0..shnum as usize)
            .map(|i| {
                let h = shoff + i * shentsize;
                Ok(Section {
                    kind: self.u32(h + 4)?,
                    offset: self.u32(h + 16)? as usize,
                    size: self.u32(h + 20)? as usize,
                    link: self.u32(h + 24)? as usize,
                    info: self.u32(h + 28)? as usize,
                })
            })
            .collect()
    }
}

/// Versioned needs: (library, version) -> undefined symbols bound to it, plus DT_NEEDED.
type Needs = BTreeMap<(String, String), Vec<String>>;

/// The string values of one `.dynamic` tag (`DT_NEEDED`, `DT_SONAME`).
fn dynamic_strings(data: &[u8], tag: u32) -> Result<Vec<String>> {
    let elf = Elf(data);
    let sections = elf.sections()?;
    let mut out = Vec::new();
    if let Some(dynamic) = sections.iter().find(|s| s.kind == SHT_DYNAMIC) {
        let strtab = sections.get(dynamic.link).ok_or("bad sh_link")?;
        for at in (dynamic.offset..dynamic.offset + dynamic.size).step_by(8) {
            if elf.u32(at)? == tag {
                out.push(elf.str(strtab, elf.u32(at + 4)?)?);
            }
        }
    }
    Ok(out)
}

/// A shared library's `DT_SONAME`, the name the loader and `dlopen` know it by.
pub fn soname(data: &[u8]) -> Result<String> {
    dynamic_strings(data, DT_SONAME)?
        .pop()
        .ok_or_else(|| "no DT_SONAME".into())
}

fn read_needs(data: &[u8]) -> Result<(Needs, Vec<String>)> {
    let elf = Elf(data);
    let sections = elf.sections()?;
    let find = |kind| sections.iter().find(|s| s.kind == kind);
    let link = |s: &Section| sections.get(s.link).ok_or("bad sh_link");
    let needed = dynamic_strings(data, DT_NEEDED)?;

    // Version index (as used in .gnu.version) -> (library, version name).
    let mut versions = BTreeMap::new();
    if let Some(verneed) = find(SHT_GNU_VERNEED) {
        let strtab = link(verneed)?;
        let mut at = verneed.offset;
        for _ in 0..verneed.info {
            let file = elf.str(strtab, elf.u32(at + 4)?)?;
            let mut aux = at + elf.u32(at + 8)? as usize;
            for _ in 0..elf.u16(at + 2)? {
                let index = elf.u16(aux + 6)? & 0x7fff;
                versions.insert(index, (file.clone(), elf.str(strtab, elf.u32(aux + 8)?)?));
                aux += elf.u32(aux + 12)? as usize;
            }
            at += elf.u32(at + 12)? as usize;
        }
    }

    let mut needs = Needs::new();
    if let (Some(dynsym), Some(versym)) = (find(SHT_DYNSYM), find(SHT_GNU_VERSYM)) {
        let strtab = link(dynsym)?;
        for i in 0..dynsym.size / 16 {
            let sym = dynsym.offset + i * 16;
            let undefined = elf.u16(sym + 14)? == 0;
            let index = elf.u16(versym.offset + i * 2)? & 0x7fff;
            if let (true, Some(version)) = (undefined, versions.get(&index)) {
                needs
                    .entry(version.clone())
                    .or_default()
                    .push(elf.str(strtab, elf.u32(sym)?)?);
            }
        }
    }
    Ok((needs, needed))
}

/// `GLIBC_2.17` -> (2, 17); `None` for anything that is not a glibc version.
fn glibc_version(name: &str) -> Option<(u32, u32)> {
    let mut parts = name.strip_prefix("GLIBC_")?.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next().map_or(Some(0), |p| p.parse().ok())?,
    ))
}

/// Print the binary's DT_NEEDED and versioned needs; fail if any glibc version is above the C1's.
pub fn check_glibc(path: &Path) -> Result {
    let data = crate::ipk::read(path)?;
    let (needs, needed) = read_needs(&data).map_err(|e| format!("{}: {e}", path.display()))?;
    eprintln!("{}: NEEDED {}", path.display(), needed.join(" "));
    let mut too_new = Vec::new();
    for ((file, version), symbols) in &needs {
        eprintln!("  {file} {version}: {} symbols", symbols.len());
        if glibc_version(version).is_some_and(|v| v > MAX_GLIBC) {
            too_new.push(format!("{version} ({})", symbols.join(" ")));
        }
    }
    if too_new.is_empty() {
        eprintln!(
            "glibc: every versioned symbol is at or below GLIBC_{}.{}",
            MAX_GLIBC.0, MAX_GLIBC.1
        );
        Ok(())
    } else {
        Err(format!(
            "needs glibc newer than the C1's {}.{}: {}",
            MAX_GLIBC.0,
            MAX_GLIBC.1,
            too_new.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_glibc_versions() {
        assert_eq!(glibc_version("GLIBC_2.4"), Some((2, 4)));
        assert_eq!(glibc_version("GLIBC_2.28"), Some((2, 28)));
        assert_eq!(glibc_version("GLIBC_2.3.4"), Some((2, 3)));
        assert_eq!(glibc_version("GLIBC_PRIVATE"), None);
        assert_eq!(glibc_version("GCC_3.5"), None);
        assert!(glibc_version("GLIBC_2.29").unwrap() > MAX_GLIBC);
    }

    #[test]
    fn rejects_non_arm_input() {
        assert!(read_needs(b"\x7fELF\x02\x01\x01").is_err());
        assert!(read_needs(b"not an elf").is_err());
    }
}
