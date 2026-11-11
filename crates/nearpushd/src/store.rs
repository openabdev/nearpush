//! In-memory ring buffer of recent events, backed by an append-only JSONL file.
//!
//! Every accepted event is written to `events.jsonl` before it is published, so ids
//! and history survive a restart. On startup the file is reloaded; when it holds
//! more than twice the buffer size (or a damaged line), it is rewritten with only the
//! retained events. The same compaction runs at runtime whenever the file grows
//! past twice the buffer size, so a long-running daemon does not grow it forever.
//!
//! The state directory is kept at 0700 and the log (and its compaction temp file)
//! at 0600, because event bodies can carry build output.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::event::{Event, NewEvent};

pub const EVENTS_FILE: &str = "events.jsonl";

struct Inner {
    buf: VecDeque<Arc<Event>>,
    cap: usize,
    next_id: u64,
    file: File,
    /// Lines currently in the log file; compaction resets it to `buf.len()`.
    lines: usize,
}

pub struct Store {
    inner: Mutex<Inner>,
    tx: broadcast::Sender<Arc<Event>>,
    path: PathBuf,
}

/// Live feed for one subscriber. Falls back to the ring buffer when it lags.
pub struct Subscription {
    rx: broadcast::Receiver<Arc<Event>>,
    cursor: u64,
}

/// What a subscriber receives after waiting: live events, plus how many events
/// fell out of the ring buffer before it could resync (0 normally).
pub struct Batch {
    pub events: Vec<Arc<Event>>,
    pub missed: u64,
}

/// Creates `dir` (and parents) and forces it to 0700.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Opens `path` for appending, created 0600; an existing file is forced to 0600.
fn open_private_append(path: &Path) -> io::Result<File> {
    let f = OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(f)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Store {
    /// Opens (or creates) the store in `state_dir`. `cap` is the number of events kept
    /// in memory; `live_cap` is the broadcast channel capacity per subscriber. The
    /// channel is never larger than the ring, so a subscriber that lags the channel
    /// can normally still resync from the ring.
    pub fn open(state_dir: &Path, cap: usize, live_cap: usize) -> io::Result<Store> {
        let cap = cap.max(1);
        let live_cap = live_cap.clamp(1, cap);
        ensure_private_dir(state_dir)?;
        let path = state_dir.join(EVENTS_FILE);

        let mut buf: VecDeque<Arc<Event>> = VecDeque::with_capacity(cap);
        let mut max_id = 0u64;
        let mut lines = 0usize;
        let mut bad = 0usize;
        match File::open(&path) {
            Ok(f) => {
                for line in BufReader::new(f).lines() {
                    let line = line?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    lines += 1;
                    match serde_json::from_str::<Event>(&line) {
                        Ok(ev) if ev.id > max_id => {
                            max_id = ev.id;
                            if buf.len() == cap {
                                buf.pop_front();
                            }
                            buf.push_back(Arc::new(ev));
                        }
                        // Out-of-order or duplicate id: never emit an id twice.
                        Ok(_) | Err(_) => bad += 1,
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if bad > 0 {
            tracing::warn!(bad, path = %path.display(), "skipped unreadable lines in event log");
        }
        if bad > 0 || lines > cap * 2 {
            compact(&path, &buf)?;
            lines = buf.len();
        }

        let file = open_private_append(&path)?;
        let (tx, _) = broadcast::channel(live_cap);
        Ok(Store {
            inner: Mutex::new(Inner { buf, cap, next_id: max_id + 1, file, lines }),
            tx,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Assigns an id, persists the event, then publishes it.
    pub fn push(&self, new: NewEvent) -> io::Result<Arc<Event>> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let ev = new.into_event(inner.next_id, now_ms());
        let mut line = serde_json::to_vec(&ev).map_err(io::Error::other)?;
        line.push(b'\n');
        inner.file.write_all(&line)?;
        inner.file.flush()?;
        inner.next_id += 1;
        inner.lines += 1;
        let ev = Arc::new(ev);
        if inner.buf.len() == inner.cap {
            inner.buf.pop_front();
        }
        inner.buf.push_back(ev.clone());
        if inner.lines > inner.cap * 2 {
            // The event is already on disk; a failed compaction leaves a longer log, not a lost event.
            match compact(&self.path, &inner.buf).and_then(|()| open_private_append(&self.path)) {
                Ok(f) => {
                    inner.file = f;
                    inner.lines = inner.buf.len();
                }
                Err(e) => tracing::warn!(error = %e, "runtime compaction of the event log failed"),
            }
        }
        // Published under the lock so a concurrent subscribe() sees each event
        // either in its snapshot or on its receiver, never both and never neither.
        let _ = self.tx.send(ev.clone());
        Ok(ev)
    }

    /// Buffered events with id greater than `since`, oldest first.
    pub fn since(&self, since: u64) -> Vec<Arc<Event>> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        since_locked(&inner, since)
    }

    /// The buffered event with this id, if it is still in the ring.
    pub fn get(&self, id: u64) -> Option<Arc<Event>> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let i = inner.buf.partition_point(|e| e.id < id);
        inner.buf.get(i).filter(|e| e.id == id).cloned()
    }

    /// Id of the newest event, or 0 when there is none.
    pub fn last_id(&self) -> u64 {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.next_id - 1
    }

    /// The newest `n` buffered events, newest first.
    pub fn recent(&self, n: usize) -> Vec<Arc<Event>> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.buf.iter().rev().take(n).cloned().collect()
    }

    /// Id of the oldest buffered event, or 0 when the buffer is empty.
    pub fn oldest_id(&self) -> u64 {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.buf.front().map(|e| e.id).unwrap_or(0)
    }

    /// Returns the replay (events after `since`), how many events after `since`
    /// are no longer buffered, and a subscription for what follows.
    pub fn subscribe(&self, since: u64) -> (Vec<Arc<Event>>, u64, Subscription) {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let rx = self.tx.subscribe();
        let replay = since_locked(&inner, since);
        let missed = missed_locked(&inner, since);
        // Everything up to the newest id is either in the replay or older than `since`;
        // the receiver only carries events pushed after this point.
        (replay, missed, Subscription { rx, cursor: inner.next_id - 1 })
    }

    /// Buffered events after `since` and the count of those that fell out of the ring.
    fn since_with_gap(&self, since: u64) -> (Vec<Arc<Event>>, u64) {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        (since_locked(&inner, since), missed_locked(&inner, since))
    }
}

/// Events with id in `since+1 .. oldest buffered` were evicted (ids are dense).
fn missed_locked(inner: &Inner, since: u64) -> u64 {
    match inner.buf.front() {
        Some(front) if front.id > since + 1 => front.id - since - 1,
        _ => 0,
    }
}

fn since_locked(inner: &Inner, since: u64) -> Vec<Arc<Event>> {
    // Ids are strictly increasing, so binary search finds the start.
    let start = inner.buf.partition_point(|e| e.id <= since);
    inner.buf.range(start..).cloned().collect()
}

fn compact(path: &Path, buf: &VecDeque<Arc<Event>>) -> io::Result<()> {
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        for ev in buf {
            let mut line = serde_json::to_vec(ev.as_ref()).map_err(io::Error::other)?;
            line.push(b'\n');
            f.write_all(&line)?;
        }
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

impl Subscription {
    /// Newest event id this subscriber has been handed (replayed or live),
    /// whether or not it was shown.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Waits for the next live events. When this subscriber has fallen behind the
    /// broadcast channel, it resyncs from the ring buffer instead of being dropped.
    /// Returns `None` once the store is gone.
    pub async fn recv(&mut self, store: &Store) -> Option<Batch> {
        loop {
            match self.rx.recv().await {
                Ok(ev) if ev.id > self.cursor => {
                    self.cursor = ev.id;
                    return Some(Batch { events: vec![ev], missed: 0 });
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, cursor = self.cursor, "subscriber lagged; resyncing");
                    let (events, missed) = store.since_with_gap(self.cursor);
                    if missed > 0 {
                        tracing::warn!(missed, cursor = self.cursor, "subscriber lagged past the ring buffer");
                    }
                    if let Some(last) = events.last() {
                        self.cursor = last.id;
                        return Some(Batch { events, missed });
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(title: &str) -> NewEvent {
        NewEvent { title: title.into(), ..Default::default() }
    }

    #[test]
    fn ids_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = Store::open(dir.path(), 10, 16).unwrap();
            for i in 1..=3 {
                assert_eq!(s.push(ev(&format!("e{i}"))).unwrap().id, i);
            }
        }
        let s = Store::open(dir.path(), 10, 16).unwrap();
        assert_eq!(s.last_id(), 3);
        assert_eq!(s.since(0).len(), 3);
        assert_eq!(s.push(ev("e4")).unwrap().id, 4);
    }

    #[test]
    fn buffer_is_bounded_and_compacts_without_losing_ids() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = Store::open(dir.path(), 3, 16).unwrap();
            for i in 0..10 {
                s.push(ev(&format!("e{i}"))).unwrap();
            }
            let ids: Vec<u64> = s.since(0).iter().map(|e| e.id).collect();
            assert_eq!(ids, vec![8, 9, 10]);
            assert_eq!(s.oldest_id(), 8);
        }
        let s = Store::open(dir.path(), 3, 16).unwrap();
        let text = fs::read_to_string(s.path()).unwrap();
        assert!(text.lines().count() <= 6, "log compacted at runtime to at most 2x the buffer");
        assert_eq!(s.push(ev("next")).unwrap().id, 11);
    }

    #[test]
    fn damaged_tail_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = Store::open(dir.path(), 10, 16).unwrap();
            s.push(ev("a")).unwrap();
            s.push(ev("b")).unwrap();
        }
        let path = dir.path().join(EVENTS_FILE);
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"id\":3,\"ts\":1,\"tit").unwrap();
        drop(f);
        let s = Store::open(dir.path(), 10, 16).unwrap();
        assert_eq!(s.last_id(), 2);
        assert_eq!(s.push(ev("c")).unwrap().id, 3);
        drop(s);
        let s = Store::open(dir.path(), 10, 16).unwrap();
        assert_eq!(s.since(0).len(), 3);
    }

    #[tokio::test]
    async fn lagging_subscriber_resyncs_from_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), 100, 4).unwrap();
        s.push(ev("before")).unwrap();
        let (replay, missed, mut sub) = s.subscribe(0);
        assert_eq!((replay.len(), missed), (1, 0));
        for i in 0..20 {
            s.push(ev(&format!("e{i}"))).unwrap();
        }
        let mut got = Vec::new();
        while got.len() < 20 {
            let b = sub.recv(&s).await.unwrap();
            assert_eq!(b.missed, 0);
            got.extend(b.events.iter().map(|e| e.id));
        }
        assert_eq!(got, (2..=21).collect::<Vec<u64>>());
    }

    #[tokio::test]
    async fn lag_past_the_ring_reports_missed() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), 5, 4).unwrap();
        let (_, _, mut sub) = s.subscribe(0);
        for i in 0..20 {
            s.push(ev(&format!("e{i}"))).unwrap();
        }
        // Ring holds 16..=20; the subscriber's cursor is 0, so 1..=15 are gone.
        let b = sub.recv(&s).await.unwrap();
        assert_eq!(b.missed, 15);
        assert_eq!(b.events.iter().map(|e| e.id).collect::<Vec<_>>(), vec![16, 17, 18, 19, 20]);
    }

    #[test]
    fn since_older_than_ring_reports_missed() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), 3, 16).unwrap();
        for i in 0..10 {
            s.push(ev(&format!("e{i}"))).unwrap();
        }
        let (replay, missed, _) = s.subscribe(0);
        assert_eq!((replay.len(), missed), (3, 7));
        assert_eq!(s.subscribe(5).1, 2);
        assert_eq!(s.subscribe(7).1, 0);
        assert_eq!(s.subscribe(10).1, 0);
    }

    #[test]
    fn broadcast_capacity_clamped_to_ring() {
        let dir = tempfile::tempdir().unwrap();
        // Tokio rounds capacity up to a power of two, so use 4 to make the bound exact.
        let s = Store::open(dir.path(), 4, 256).unwrap();
        let (_, _, mut sub) = s.subscribe(0);
        for i in 0..5 {
            s.push(ev(&format!("e{i}"))).unwrap();
        }
        // A 256-slot channel would still hold all five; a 4-slot one has lagged.
        assert!(matches!(sub.rx.try_recv(), Err(broadcast::error::TryRecvError::Lagged(1))));
    }

    #[test]
    fn files_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(state.join(EVENTS_FILE), "").unwrap();
        fs::set_permissions(state.join(EVENTS_FILE), fs::Permissions::from_mode(0o644)).unwrap();
        let s = Store::open(&state, 2, 16).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&state), 0o700);
        assert_eq!(mode(s.path()), 0o600);
        for i in 0..5 {
            s.push(ev(&format!("e{i}"))).unwrap(); // compacts at 5 lines > 2*2
        }
        assert_eq!(mode(s.path()), 0o600, "compacted file keeps 0600");
        assert!(!state.join("events.jsonl.tmp").exists());
    }

    #[test]
    fn compacts_at_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), 3, 16).unwrap();
        for i in 0..7 {
            s.push(ev(&format!("e{i}"))).unwrap();
        }
        // Compacted to 3 lines when the 7th line passed 2*3.
        let text = fs::read_to_string(s.path()).unwrap();
        assert_eq!(text.lines().count(), 3);
        for i in 0..20 {
            s.push(ev(&format!("f{i}"))).unwrap();
            let n = fs::read_to_string(s.path()).unwrap().lines().count();
            assert!(n <= 6, "log never exceeds 2x cap, got {n}");
        }
        drop(s);
        let s = Store::open(dir.path(), 3, 16).unwrap();
        assert_eq!(s.last_id(), 27);
        assert_eq!(s.push(ev("next")).unwrap().id, 28);
    }

    #[test]
    fn subscribe_replays_only_newer() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), 100, 16).unwrap();
        for i in 0..5 {
            s.push(ev(&format!("e{i}"))).unwrap();
        }
        let (replay, _, _) = s.subscribe(2);
        let ids: Vec<u64> = replay.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![3, 4, 5]);
        let (replay, _, _) = s.subscribe(99);
        assert!(replay.is_empty());
    }

    #[test]
    fn recent_is_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), 100, 16).unwrap();
        assert!(s.recent(5).is_empty());
        for i in 0..7 {
            s.push(ev(&format!("e{i}"))).unwrap();
        }
        let ids: Vec<u64> = s.recent(5).iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![7, 6, 5, 4, 3]);
        assert_eq!(s.subscribe(0).2.cursor(), 7);
    }
}
