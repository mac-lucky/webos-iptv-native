//! What the viewer watches, kept for History and the recent channels: an archive programme's
//! position is saved every [`PROGRESS_MS`] while it plays and whenever another stream begins, and
//! a live channel becomes a recent channel once it has played for [`RECENT_AFTER_MS`].

use iptv::model::{ChannelKey, Programme};
use tv_screens::data::Stream;

use crate::store::Store;

/// A live channel counts as watched (a recent channel) after playing this long.
const RECENT_AFTER_MS: u64 = 5_000;
/// How often the position of an archive programme is saved while it plays.
const PROGRESS_MS: u64 = 30_000;

#[derive(Default)]
pub enum Watch {
    /// Nothing plays, or the live channel playing is a recent channel already.
    #[default]
    Idle,
    /// A live channel; it becomes a recent channel at `recent_at`.
    Live { key: ChannelKey, recent_at: u64 },
    /// An archive programme, kept past the player's own return to live at its end; its
    /// position is saved next at `save_at`.
    Archive {
        key: ChannelKey,
        programme: Programme,
        save_at: u64,
    },
}

impl Watch {
    /// `stream` begins at `now`. Called before the playback says so: the archive programme
    /// before it is saved where it got to.
    pub fn begin(&mut self, stream: &Stream, store: &mut dyn Store, now: u64) {
        self.save(store);
        *self = match stream {
            Stream::Live(key) => Watch::Live {
                key: *key,
                recent_at: now + RECENT_AFTER_MS,
            },
            Stream::Catchup {
                channel, programme, ..
            } => Watch::Archive {
                key: *channel,
                programme: programme.clone(),
                save_at: now + PROGRESS_MS,
            },
        };
    }

    /// Save where the archive programme playing is (on the way out, too).
    pub fn save(&self, store: &mut dyn Store) {
        if let Watch::Archive { key, programme, .. } = self {
            let position = store.playback_mut().position_secs;
            store.save_progress(*key, programme, position);
        }
    }

    /// Whatever is due at `now`. An archive programme the playback no longer has ended: the
    /// player went live by itself, so it was watched to the end and the channel now plays live.
    pub fn tick(&mut self, store: &mut dyn Store, now: u64) {
        match self {
            Watch::Archive { key, programme, .. } if store.playback_mut().catchup.is_none() => {
                store.save_progress(*key, programme, programme.duration_secs());
                *self = Watch::Live {
                    key: *key,
                    recent_at: now + RECENT_AFTER_MS,
                };
            }
            Watch::Archive { save_at, .. } if now >= *save_at => {
                *save_at = now + PROGRESS_MS;
                self.save(store);
            }
            Watch::Live { key, recent_at } if now >= *recent_at => {
                store.touch_recent(*key);
                *self = Watch::Idle;
            }
            _ => {}
        }
    }

    pub fn next_wake(&self) -> Option<u64> {
        match self {
            Watch::Idle => None,
            Watch::Live { recent_at, .. } => Some(*recent_at),
            Watch::Archive { save_at, .. } => Some(*save_at),
        }
    }
}
