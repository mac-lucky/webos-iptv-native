//! What a VOD section lists, built from [`TvData::vod`] when the section opens or the data
//! changes: its categories (Continue watching, All, then the playlist's groups in order of first
//! appearance), the movies or the series in each, and a series' seasons and episodes. An item
//! the parental lock covers ([`TvData::vod_locked`]) is left out everywhere until the PIN opens
//! its group or its key.

use std::collections::HashMap;
use std::rc::Rc;

use iptv::model::{Channel, ChannelKey};
use iptv::vod::{VodItem, VodKind, VodProgress};
use tv_platform::i18n::msg;

use crate::data::{TvData, VodSection};
use crate::parental::Parental;

/// Whether `parental` covers `item`: as it would the channel the item was listed as, by its key
/// (a channel blocked before it moved to VOD keeps its key) or any of its groups.
pub fn locked(parental: &Parental, item: &VodItem) -> bool {
    !parental.is_empty()
        && parental.locked(&Channel {
            key: item.key,
            groups: item.groups.clone(),
            ..Channel::default()
        })
}

/// A category of the left column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Category {
    /// Items started and not finished, the latest watched first.
    Continue,
    All,
    /// A playlist group.
    Group(String),
}

impl Category {
    pub fn name(&self) -> &str {
        match self {
            Category::Continue => msg::vod_continue(),
            Category::All => msg::vod_all(),
            Category::Group(name) => name,
        }
    }
}

/// One season of a series; `number` is `None` for the episodes that name none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Season {
    pub number: Option<u32>,
    /// By episode number, then playlist order.
    pub episodes: Vec<Rc<VodItem>>,
}

impl Season {
    pub fn name(&self) -> String {
        match self.number {
            Some(n) => msg::vod_season(i64::from(n)),
            None => msg::vod_episodes().to_owned(),
        }
    }
}

/// A series of the Shows section: its episodes by season.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Series {
    pub name: String,
    /// The first episode's poster.
    pub poster: Option<String>,
    /// By number, the episodes that name no season last.
    pub seasons: Vec<Season>,
}

impl Series {
    pub fn episodes(&self) -> impl Iterator<Item = &Rc<VodItem>> {
        self.seasons.iter().flat_map(|s| &s.episodes)
    }

    pub fn len(&self) -> usize {
        self.seasons.iter().map(|s| s.episodes.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A row of the items list: a movie, or a series.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Movie(Rc<VodItem>),
    Series(Rc<Series>),
}

impl Entry {
    pub fn title(&self) -> &str {
        match self {
            Entry::Movie(m) => &m.title,
            Entry::Series(s) => &s.name,
        }
    }

    pub fn poster(&self) -> Option<&str> {
        match self {
            Entry::Movie(m) => m.poster.as_deref(),
            Entry::Series(s) => s.poster.as_deref(),
        }
    }

    /// The same movie or series, across a rebuild.
    fn same(&self, other: &Entry) -> bool {
        match (self, other) {
            (Entry::Movie(a), Entry::Movie(b)) => a.key == b.key,
            (Entry::Series(a), Entry::Series(b)) => a.name == b.name,
            _ => false,
        }
    }
}

/// A section's categories and what each lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    pub categories: Vec<Category>,
    /// Per category, indexes into `entries`.
    lists: Vec<Vec<usize>>,
    pub entries: Vec<Entry>,
    /// The kept positions of the section's items.
    progress: HashMap<ChannelKey, VodProgress>,
}

impl Catalog {
    /// The movies (or the series) `data` lists for `section`. Empty categories are left out.
    pub fn build(data: &dyn TvData, section: VodSection) -> Self {
        let kind = match section {
            VodSection::Movies => VodKind::Movie,
            VodSection::Shows => VodKind::Episode,
        };
        let items: Vec<Rc<VodItem>> = data
            .vod()
            .iter()
            .filter(|v| v.kind == kind && !data.vod_locked(v))
            .cloned()
            .collect();
        let progress: HashMap<ChannelKey, VodProgress> = items
            .iter()
            .filter_map(|v| data.vod_progress(v.key))
            .map(|p| (p.key, p))
            .collect();
        // Each entry with the groups its items are listed in and when it was last watched.
        let mut entries = Vec::new();
        let mut groups: Vec<Vec<String>> = Vec::new();
        let mut started: Vec<Option<i64>> = Vec::new();
        let resumable = |v: &VodItem| {
            progress
                .get(&v.key)
                .filter(|p| p.resume_at().is_some())
                .map(|p| p.updated_at)
        };
        match section {
            VodSection::Movies => {
                for m in &items {
                    groups.push(m.groups.clone());
                    started.push(resumable(m));
                    entries.push(Entry::Movie(Rc::clone(m)));
                }
            }
            VodSection::Shows => {
                for (series, members) in group_series(&items) {
                    let mut g: Vec<String> = Vec::new();
                    for name in members.iter().flat_map(|v| &v.groups) {
                        if !g.contains(name) {
                            g.push(name.clone());
                        }
                    }
                    groups.push(g);
                    started.push(members.iter().filter_map(|v| resumable(v)).max());
                    entries.push(Entry::Series(Rc::new(series)));
                }
            }
        }
        let mut categories = Vec::new();
        let mut lists = Vec::new();
        let mut cont: Vec<usize> = (0..entries.len())
            .filter(|&i| started[i].is_some())
            .collect();
        cont.sort_by_key(|&i| std::cmp::Reverse(started[i]));
        if !cont.is_empty() {
            categories.push(Category::Continue);
            lists.push(cont);
        }
        if !entries.is_empty() {
            categories.push(Category::All);
            lists.push((0..entries.len()).collect());
        }
        let mut at: HashMap<String, usize> = HashMap::new();
        for (i, gs) in groups.into_iter().enumerate() {
            for g in gs {
                match at.get(&g) {
                    Some(&c) => lists[c].push(i),
                    None => {
                        at.insert(g.clone(), lists.len());
                        categories.push(Category::Group(g));
                        lists.push(vec![i]);
                    }
                }
            }
        }
        Catalog {
            categories,
            lists,
            entries,
            progress,
        }
    }

    /// The entries of category `i`, in list order.
    pub fn list(&self, i: usize) -> impl Iterator<Item = &Entry> {
        self.lists
            .get(i)
            .into_iter()
            .flatten()
            .map(|&e| &self.entries[e])
    }

    pub fn list_len(&self, i: usize) -> usize {
        self.lists.get(i).map_or(0, Vec::len)
    }

    /// Entry `at` of category `i`.
    pub fn entry(&self, i: usize, at: usize) -> Option<&Entry> {
        self.lists.get(i)?.get(at).map(|&e| &self.entries[e])
    }

    pub fn progress(&self, item: ChannelKey) -> Option<&VodProgress> {
        self.progress.get(&item)
    }

    /// Where `category` and `entry` (from another build) are in this one.
    pub fn find(
        &self,
        category: &Category,
        entry: Option<&Entry>,
    ) -> (Option<usize>, Option<usize>) {
        let c = self.categories.iter().position(|x| x == category);
        let e = c
            .zip(entry)
            .and_then(|(c, entry)| self.list(c).position(|x| x.same(entry)));
        (c, e)
    }
}

/// The episodes as series, in order of each series' first episode; a series is told by its
/// name, in any case.
fn group_series(items: &[Rc<VodItem>]) -> Vec<(Series, Vec<Rc<VodItem>>)> {
    let mut order: Vec<(String, Vec<Rc<VodItem>>)> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for v in items {
        let name = v.series.clone().unwrap_or_else(|| v.title.clone());
        let key = name.to_lowercase();
        match at.get(&key) {
            Some(&i) => order[i].1.push(Rc::clone(v)),
            None => {
                at.insert(key, order.len());
                order.push((name, vec![Rc::clone(v)]));
            }
        }
    }
    order
        .into_iter()
        .map(|(name, members)| {
            let mut seasons: Vec<Season> = Vec::new();
            for v in &members {
                match seasons.iter_mut().find(|s| s.number == v.season) {
                    Some(s) => s.episodes.push(Rc::clone(v)),
                    None => seasons.push(Season {
                        number: v.season,
                        episodes: vec![Rc::clone(v)],
                    }),
                }
            }
            seasons.sort_by_key(|s| s.number.map_or(u64::MAX, u64::from));
            for s in &mut seasons {
                s.episodes
                    .sort_by_key(|e| e.episode.map_or(u64::MAX, u64::from));
            }
            let series = Series {
                name,
                poster: members.iter().find_map(|v| v.poster.clone()),
                seasons,
            };
            (series, members)
        })
        .collect()
}

/// A movie or episode's state as the lists show it: watched, how much is left, or nothing.
#[derive(Clone, Debug, PartialEq)]
pub enum Mark {
    None,
    Watched,
    /// Started: the fraction played and what is left (`42:10 left`).
    Started(f32, String),
}

impl Mark {
    pub fn of(p: Option<&VodProgress>) -> Self {
        match p {
            Some(p) if p.watched => Mark::Watched,
            Some(p) if p.resume_at().is_some() && p.duration_secs > 0 => {
                let frac = p.position_secs as f32 / p.duration_secs as f32;
                let left = i64::from(p.duration_secs.saturating_sub(p.position_secs));
                Mark::Started(
                    frac.clamp(0.0, 1.0),
                    msg::vod_left(&crate::time::clock_secs(left)),
                )
            }
            Some(p) if p.resume_at().is_some() => Mark::Started(0.0, String::new()),
            _ => Mark::None,
        }
    }
}
