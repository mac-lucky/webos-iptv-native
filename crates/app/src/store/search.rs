//! Programme search ([`TvData::search`](tv_screens::data::TvData::search)): one scan of the
//! folded titles in SQLite ([`Storage::search`]), on a worker with its own connection, since on
//! the TV a scan of a whole guide outlasts many frames. The worker answers the newest query and
//! skips the ones typed over. A database in memory has no second connection: its searches run on
//! the caller's thread.
//!
//! What is listed: programmes on air, past ones only while the channel's archive and
//! `epg.past_days` still reach them, and upcoming ones up to [`SEARCH_AHEAD_SECS`] ahead, in
//! [`search_order`] with on air in playlist order.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;

use iptv::epg::{GuideChannelId, Match};
use iptv::model::{fold, Channel, ChannelKey, Timestamp};
use iptv::storage::Storage;
use tv_screens::data::{search_order, GridProgramme, SearchResult, SEARCH_AHEAD_SECS, SEARCH_HITS};
use tv_screens::time::{when, When, DAY};

use crate::logs::log;

/// An answer this many seconds old is looked for again: programmes go on and off air meanwhile.
const FRESH_SECS: u64 = 60;

/// What a search needs of the channels, `Send` for the worker: the playlist channels each guide
/// channel serves.
#[derive(Debug, Default)]
pub struct Index {
    by_guide: HashMap<GuideChannelId, Vec<Served>>,
    /// The largest shift either way, which widens the guide-time window.
    max_shift: i64,
    /// The longest archive of any channel.
    max_archive_days: i64,
}

#[derive(Debug)]
struct Served {
    /// In the playlist, for the on-air order.
    position: usize,
    key: ChannelKey,
    shift: i64,
    /// Days of catch-up; 0 without an archive.
    archive_days: i64,
}

impl Index {
    pub fn new(channels: &[Channel], guides: &HashMap<ChannelKey, Match>) -> Index {
        let mut index = Index::default();
        for (position, c) in channels.iter().enumerate() {
            let Some(m) = guides.get(&c.key) else {
                continue;
            };
            let archive_days = if c.catchup.available() {
                i64::from(c.catchup.days)
            } else {
                0
            };
            let shift = m.shift_secs();
            index.max_shift = index.max_shift.max(shift.abs());
            index.max_archive_days = index.max_archive_days.max(archive_days);
            index
                .by_guide
                .entry(m.guide_channel)
                .or_default()
                .push(Served {
                    position,
                    key: c.key,
                    shift,
                    archive_days,
                });
        }
        index
    }
}

/// The programmes whose folded title contains `query` (folded, not empty), listed as search
/// lists them at `now`.
pub fn find(
    storage: &Storage,
    index: &Index,
    query: &str,
    now: Timestamp,
    past_days: u32,
) -> SearchResult {
    let past_days = i64::from(past_days);
    let from = now - index.max_archive_days.min(past_days) * DAY - index.max_shift;
    let to = now + SEARCH_AHEAD_SECS + index.max_shift;
    let mut kept: Vec<(usize, ChannelKey, GridProgramme)> = Vec::new();
    let scan = storage.search(query, from, to, now, |hit| {
        for s in index.by_guide.get(&hit.guide_channel).into_iter().flatten() {
            let (start, stop) = (hit.start + s.shift, hit.stop + s.shift);
            let listed = match when(start, stop, now) {
                When::OnAir => true,
                When::Past => {
                    s.archive_days > 0
                        && stop > now - s.archive_days.min(past_days) * DAY
                        && start >= now - s.archive_days * DAY
                }
                When::Future => start < now + SEARCH_AHEAD_SECS,
            };
            if listed {
                let title = hit.title.clone();
                kept.push((s.position, s.key, GridProgramme { start, stop, title }));
            }
        }
        // Every row on air comes first (they are listed in playlist order); past them, rows only
        // come later in the list.
        kept.len() <= SEARCH_HITS || when(hit.start, hit.stop, now) == When::OnAir
    });
    if let Err(e) = scan {
        log(&format!("storage: search: {e}"));
    }
    kept.sort_by_key(|(position, _, p)| (search_order(p, now), *position, p.start));
    let capped = kept.len() > SEARCH_HITS;
    kept.truncate(SEARCH_HITS);
    SearchResult {
        query: query.to_owned(),
        hits: kept.into_iter().map(|(_, key, p)| (key, p)).collect(),
        capped,
    }
}

struct Request {
    /// Which ask this answers.
    ticket: u64,
    query: String,
    now: Timestamp,
    past_days: u32,
    index: Arc<Index>,
}

struct Worker {
    /// Dropped first: the worker ends when it is gone.
    requests: Option<Sender<Request>>,
    answers: Receiver<(u64, SearchResult)>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn start(dir: PathBuf) -> Option<Worker> {
        let (requests, inbox) = mpsc::channel::<Request>();
        let (outbox, answers) = mpsc::channel();
        let thread = tv_base::task::spawn("search", move || {
            let storage = Storage::open(&dir)
                .map_err(|e| log(&format!("search: storage: {e}")))
                .ok();
            while let Ok(mut request) = inbox.recv() {
                // Only the newest query matters; the ones typed over are skipped.
                while let Ok(newer) = inbox.try_recv() {
                    request = newer;
                }
                let found = match &storage {
                    Some(s) => find(
                        s,
                        &request.index,
                        &request.query,
                        request.now,
                        request.past_days,
                    ),
                    None => SearchResult {
                        query: request.query,
                        ..SearchResult::default()
                    },
                };
                if outbox.send((request.ticket, found)).is_err() {
                    return;
                }
            }
        })?;
        Some(Worker {
            requests: Some(requests),
            answers,
            thread: Some(thread),
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.requests = None;
        if let Some(thread) = self.thread.take() {
            tv_base::task::join("search", thread);
        }
    }
}

/// The store's side of search: the channels' [`Index`], the worker and the last answer.
#[derive(Default)]
pub struct Search {
    index: Arc<Index>,
    /// Where the worker opens its connection; `None` for a database in memory.
    dir: Option<PathBuf>,
    /// Started on the first search.
    worker: RefCell<Option<Worker>>,
    /// The worker could not start or stopped: searches run on the caller's thread from then on.
    no_worker: Cell<bool>,
    /// The folded query asked last, the `now` it was asked for, the ticket its answer comes back
    /// with, and the answer once it is in.
    asked: RefCell<String>,
    asked_at: Cell<Timestamp>,
    ticket: Cell<u64>,
    found: RefCell<Option<SearchResult>>,
    pending: Cell<bool>,
}

impl Search {
    /// Searches run on a worker over the database in `dir`.
    pub fn set_dir(&mut self, dir: PathBuf) {
        self.dir = Some(dir);
        self.no_worker.set(false);
    }

    /// The channels changed: answers given or on their way are stale.
    pub fn set_index(&mut self, index: Index) {
        self.index = Arc::new(index);
        self.forget();
    }

    fn forget(&self) {
        self.ticket.set(self.ticket.get() + 1);
        self.asked.borrow_mut().clear();
        *self.found.borrow_mut() = None;
        self.pending.set(false);
    }

    /// The answer for `query` (raw, as typed), or `None` while the worker looks for it. Asking
    /// for a new query drops the answer to the old one; so does asking a minute later
    /// ([`FRESH_SECS`]), since what is on air has moved on.
    pub fn ask(
        &self,
        query: &str,
        now: Timestamp,
        past_days: u32,
        storage: &Storage,
    ) -> Option<SearchResult> {
        let query = fold(query.trim());
        if query.is_empty() {
            return Some(SearchResult::default());
        }
        self.take_answers();
        let fresh = now.abs_diff(self.asked_at.get()) < FRESH_SECS;
        if fresh && *self.asked.borrow() == query {
            if let Some(found) = &*self.found.borrow() {
                return Some(found.clone());
            }
            if self.pending.get() {
                return None;
            }
        }
        self.forget();
        self.asked.replace(query.clone());
        self.asked_at.set(now);
        let mut worker = self.worker.borrow_mut();
        if worker.is_none() && !self.no_worker.get() {
            *worker = self.dir.clone().and_then(Worker::start);
            self.no_worker.set(worker.is_none());
        }
        let request = Request {
            ticket: self.ticket.get(),
            query: query.clone(),
            now,
            past_days,
            index: Arc::clone(&self.index),
        };
        let sent = worker
            .as_ref()
            .and_then(|w| w.requests.as_ref())
            .is_some_and(|tx| tx.send(request).is_ok());
        if sent {
            self.pending.set(true);
            return None;
        }
        // No worker: in memory, or the thread could not start.
        let found = find(storage, &self.index, &query, now, past_days);
        *self.found.borrow_mut() = Some(found.clone());
        Some(found)
    }

    /// Keep what the worker answered for the query asked last. A worker that stopped (its
    /// thread died) answers nothing more: it goes, and so does the wait for its answer.
    fn take_answers(&self) {
        let mut worker = self.worker.borrow_mut();
        let Some(answers) = worker.as_ref().map(|w| &w.answers) else {
            return;
        };
        loop {
            match answers.try_recv() {
                Ok((ticket, found)) => {
                    if ticket == self.ticket.get() {
                        self.pending.set(false);
                        *self.found.borrow_mut() = Some(found);
                    }
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        log("search: the worker stopped; searching on this thread");
        *worker = None;
        self.no_worker.set(true);
        self.pending.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Timestamp = 1_700_000_000;

    fn until(done: impl Fn() -> bool) {
        let start = std::time::Instant::now();
        while !done() && start.elapsed().as_secs() < 10 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn an_answer_a_minute_old_is_looked_for_again() {
        let dir = std::env::temp_dir().join(format!("webosiptv-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        drop(Storage::open(&dir).unwrap());
        let storage = Storage::open_in_memory().unwrap();
        let mut search = Search::default();
        search.set_dir(dir.clone());
        assert_eq!(
            search.ask("pogoda", NOW, 7, &storage),
            None,
            "the worker looks"
        );
        until(|| search.ask("pogoda", NOW, 7, &storage).is_some());
        let again = search.ask("Pogoda ", NOW + 59, 7, &storage);
        assert!(
            again.is_some(),
            "the same query within the minute: the same answer"
        );
        let later = search.ask("pogoda", NOW + 60, 7, &storage);
        assert_eq!(later, None, "a minute on: looked for again");
        until(|| search.ask("pogoda", NOW + 60, 7, &storage).is_some());
        drop(search);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_worker_that_stopped_goes_and_searches_answer_at_once() {
        let storage = Storage::open_in_memory().unwrap();
        let mut search = Search::default();
        // Without the worker gone for good, this would start a new one.
        search.set_dir(std::env::temp_dir().join("webosiptv-no-such-search-dir"));
        // A worker whose thread took the request and died before answering.
        let (requests, inbox) = mpsc::channel::<Request>();
        let (outbox, answers) = mpsc::channel::<(u64, SearchResult)>();
        *search.worker.borrow_mut() = Some(Worker {
            requests: Some(requests),
            answers,
            thread: None,
        });
        assert_eq!(search.ask("pogoda", NOW, 7, &storage), None);
        drop((inbox, outbox));
        let found = search.ask("pogoda", NOW, 7, &storage);
        assert_eq!(found.map(|f| f.query).as_deref(), Some("pogoda"));
        assert!(search.worker.borrow().is_none() && !search.pending.get());
        assert!(search.ask("wiad", NOW, 7, &storage).is_some());
        assert!(search.worker.borrow().is_none(), "no new worker");
    }
}
