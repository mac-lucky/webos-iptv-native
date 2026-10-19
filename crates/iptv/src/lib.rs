//! IPTV data layer. Core modules (model, parsers, URL builders, matching) do no I/O; `storage`
//! and `refresh` are the only modules that touch the filesystem or the network.

pub mod catchup;
pub mod epg;
pub mod m3u;
pub mod model;
pub mod redact;
pub mod refresh;
pub mod storage;
pub mod url;
pub mod vod;
pub mod xmltv;
