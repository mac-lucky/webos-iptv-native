//! Matching playlist channels to guide channels. Pure: the caller loads the guide
//! catalog (storage) and uses the result to query programmes.
//!
//! Order: the user's override, then `tvg-id`, then the channel name or `tvg-name` against every
//! display name (compared as [`name_key`]s), each tried source by source. Only guide channels
//! that have programmes match by id or name, so a feed that merely declares a channel does not
//! hide another feed's schedule for it. A source serves the playlists that list it; a manual source
//! with no playlists serves all of them, and manual sources are tried first.
//!
//! Parts are ported to Rust from webOS IPTV Player `src/services/epg-service.ts` (commit 7d8fd30,
//! Copyright 2026 lennylxx, Apache-2.0; see THIRD-PARTY-NOTICES.md) and changed:
//! `findBaseChannelId`, `sourceAppliesToChannel` and `filterFor`, with the offsets of
//! `applyChannelOffset` reduced to one number per channel. The `tvg-shift` fallback and the
//! diagnostics are this project's.

use std::collections::{HashMap, HashSet};

use crate::model::{name_key, Channel, ChannelKey, EpgChannel};

/// Storage row ids; opaque to the matcher.
pub type PlaylistId = i64;
pub type SourceId = i64;
pub type GuideChannelId = i64;

/// One XMLTV source with its channel catalog.
#[derive(Clone, Debug, PartialEq)]
pub struct GuideSource {
    pub id: SourceId,
    /// Added by the user rather than named by a playlist header.
    pub manual: bool,
    /// Playlists this source serves; a manual source with none serves every playlist.
    pub playlists: Vec<PlaylistId>,
    /// User correction added to every programme time of this source.
    pub offset_minutes: i32,
    pub channels: Vec<GuideChannel>,
}

impl GuideSource {
    pub fn serves(&self, playlist: PlaylistId) -> bool {
        (self.manual && self.playlists.is_empty()) || self.playlists.contains(&playlist)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GuideChannel {
    pub row: GuideChannelId,
    pub epg: EpgChannel,
    pub has_programmes: bool,
}

/// What the user set for one playlist channel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Override {
    /// The guide channel picked by hand: its source and XMLTV id.
    pub guide: Option<(SourceId, String)>,
    /// Replaces the channel's `tvg-shift`.
    pub shift_minutes: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    Override,
    TvgId,
    Name,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub source: SourceId,
    pub guide_channel: GuideChannelId,
    /// Source offset plus the channel's shift: add it to guide times to get the channel's.
    pub shift_minutes: i32,
    pub via: Via,
}

impl Match {
    pub fn shift_secs(&self) -> i64 {
        i64::from(self.shift_minutes) * 60
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Matches {
    /// One entry per playlist channel, in order.
    pub channels: Vec<Option<Match>>,
    /// Indices of channels whose override names a guide channel that is gone or no longer
    /// serves their playlist (matching then fell back to id and name).
    pub stale_overrides: Vec<usize>,
}

impl Matches {
    /// Indices of channels without a guide, for diagnostics.
    pub fn unmatched(&self) -> impl Iterator<Item = usize> + '_ {
        self.channels
            .iter()
            .enumerate()
            .filter_map(|(i, m)| m.is_none().then_some(i))
    }
}

pub struct Matcher<'a> {
    sources: Vec<Indexed<'a>>,
}

struct Indexed<'a> {
    source: &'a GuideSource,
    /// Every channel: an override may pick one without programmes.
    by_id: HashMap<&'a str, &'a GuideChannel>,
    /// The first channel with programmes carrying each [`name_key`].
    by_name: HashMap<String, &'a GuideChannel>,
}

impl<'a> Indexed<'a> {
    fn new(source: &'a GuideSource) -> Self {
        let mut by_id = HashMap::with_capacity(source.channels.len());
        let mut by_name = HashMap::new();
        for channel in &source.channels {
            by_id.entry(channel.epg.id.as_str()).or_insert(channel);
            if channel.has_programmes {
                for name in &channel.epg.names {
                    by_name.entry(name_key(name)).or_insert(channel);
                }
            }
        }
        Indexed {
            source,
            by_id,
            by_name,
        }
    }
}

impl<'a> Matcher<'a> {
    pub fn new(sources: &'a [GuideSource]) -> Self {
        let mut sources: Vec<Indexed<'a>> = sources.iter().map(Indexed::new).collect();
        // Stable: manual sources first, each group in the order given.
        sources.sort_by_key(|s| !s.source.manual);
        Matcher { sources }
    }

    /// The guide for one channel of `playlist`.
    pub fn find(
        &self,
        playlist: PlaylistId,
        channel: &Channel,
        user: Option<&Override>,
    ) -> Option<Match> {
        self.resolve(playlist, channel, user).0
    }

    pub fn match_playlist(
        &self,
        playlist: PlaylistId,
        channels: &[Channel],
        overrides: &HashMap<ChannelKey, Override>,
    ) -> Matches {
        let mut matches = Matches::default();
        for (i, channel) in channels.iter().enumerate() {
            let (found, stale) = self.resolve(playlist, channel, overrides.get(&channel.key));
            if stale {
                matches.stale_overrides.push(i);
            }
            matches.channels.push(found);
        }
        matches
    }

    /// The match, and whether an override was set but could not be used.
    fn resolve(
        &self,
        playlist: PlaylistId,
        channel: &Channel,
        user: Option<&Override>,
    ) -> (Option<Match>, bool) {
        let shift = user
            .and_then(|o| o.shift_minutes)
            .unwrap_or(channel.tvg_shift_minutes);
        let found = |source: &Indexed, guide: &GuideChannel, via| Match {
            source: source.source.id,
            guide_channel: guide.row,
            shift_minutes: source.source.offset_minutes.saturating_add(shift),
            via,
        };
        let serving = || self.sources.iter().filter(|s| s.source.serves(playlist));

        let mut stale = false;
        if let Some((source, id)) = user.and_then(|o| o.guide.as_ref()) {
            let picked = serving()
                .filter(|s| s.source.id == *source)
                .find_map(|s| Some((s, *s.by_id.get(id.as_str())?)));
            match picked {
                Some((s, guide)) => return (Some(found(s, guide, Via::Override)), false),
                None => stale = true,
            }
        }

        if let Some(id) = channel.tvg_id.as_deref().filter(|id| !id.is_empty()) {
            for s in serving() {
                if let Some(guide) = s.by_id.get(id).filter(|g| g.has_programmes) {
                    return (Some(found(s, guide, Via::TvgId)), stale);
                }
            }
        }

        let mut keys = channel
            .guide_names()
            .map(name_key)
            .filter(|n| !n.is_empty());
        let keys = [keys.next(), keys.next()];
        for s in serving() {
            if let Some(guide) = keys
                .iter()
                .flatten()
                .find_map(|n| s.by_name.get(n.as_str()))
            {
                return (Some(found(s, guide, Via::Name)), stale);
            }
        }
        (None, stale)
    }
}

/// The XMLTV filter for one source: `(ids, name keys)` of the channels it serves, plus
/// the guide ids users mapped to it, so a parse keeps exactly what can match.
pub fn filter(
    source: &GuideSource,
    playlists: &[(PlaylistId, &[Channel])],
    overrides: &HashMap<ChannelKey, Override>,
) -> (HashSet<String>, HashSet<String>) {
    let (mut ids, mut names) = (HashSet::new(), HashSet::new());
    for (playlist, channels) in playlists {
        if !source.serves(*playlist) {
            continue;
        }
        for channel in *channels {
            let picked = overrides.get(&channel.key).and_then(|o| o.guide.as_ref());
            if let Some((_, id)) = picked.filter(|(s, _)| *s == source.id) {
                ids.insert(id.clone());
            }
            if let Some(id) = channel.tvg_id.as_ref().filter(|id| !id.is_empty()) {
                ids.insert(id.clone());
            }
            names.extend(
                channel
                    .guide_names()
                    .map(name_key)
                    .filter(|n| !n.is_empty()),
            );
        }
    }
    (ids, names)
}
