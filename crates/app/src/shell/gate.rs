//! The parental gate: an output that would play a locked channel is held back while the PIN
//! dialog asks for the PIN, and carried out once the PIN opened the channel.
//!
//! Every way to play goes through [`Command::Play`] (the guide and its preview, the channel
//! list, zapping, number entry, Search, History, the recent channels, a reminder), so one check
//! in the shell covers them all. The whole output waits, its navigation too, so a list that
//! closes on play stays open when the PIN is not entered.

use iptv::model::ChannelKey;
use tv_screens::data::{Command, Lock, Output, TvData};

/// An output held back for the PIN.
pub struct Held {
    pub out: Output,
    /// The screen that returned it (`None`: the player).
    pub from: Option<usize>,
    pub lock: Lock,
    /// The PIN opened it: carry the output out once the dialog's own output is done.
    pub ready: bool,
}

impl Held {
    pub fn new(out: Output, from: Option<usize>, key: ChannelKey) -> Self {
        Held {
            out,
            from,
            lock: Lock::Channel(key),
            ready: false,
        }
    }
}

/// The first channel `out` plays that takes the PIN.
pub fn locked_play(out: &Output, data: &dyn TvData) -> Option<ChannelKey> {
    out.commands.iter().find_map(|c| match c {
        Command::Play { stream, .. } => {
            let key = stream.channel();
            data.channel(key).filter(|ch| data.locked(ch)).map(|_| key)
        }
        _ => None,
    })
}
