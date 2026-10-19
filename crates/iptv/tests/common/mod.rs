//! Fixtures the test binaries share; each uses some of them.
#![allow(dead_code)]

use std::io::Write;
use std::path::PathBuf;

use iptv::model::Timestamp;

/// XMLTV's `YYYYMMDDHHMMSS` for a Unix time, in UTC.
pub fn stamp(t: Timestamp) -> String {
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let (year, month, day) = tv_base::time::civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}{:02}{:02}{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// [`stamp`] with the zone: `YYYYMMDDHHMMSS +0000`.
pub fn date(t: Timestamp) -> String {
    format!("{} +0000", stamp(t))
}

pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// A fresh directory under the system temp dir, one per name and process, removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("iptv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
