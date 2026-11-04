//! In-memory ring buffer of recent events, backed by an append-only JSONL file.
//!
//! Every accepted event is written to `events.jsonl` before it is published, so ids
//! and history survive a restart. On startup the file is reloaded; when it holds
//! more than twice the buffer size (or a damaged line), it is rewritten with only the
//! retained events.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
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

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Store {
    /// Opens (or creates) the store in `state_dir`. `cap` is the number of events kept
    /// in memory; `live_cap` is the broadcast channel capacity per subscriber.
    pub fn open(state_dir: &Path, cap: usize, live_cap: usize) -> io::Result<Store> {
        let cap = cap.max(1);
        fs::create_dir_all(state_dir)?;
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
        }

        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let (tx, _) = broadcast::channel(live_cap.max(1));
        Ok(Store {
            inner: Mutex::new(Inner { buf, cap, next_id: max_id + 1, file }),
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
        let ev = Arc::new(ev);
        if inner.buf.len() == inner.cap {
            inner.buf.pop_front();
        }
        inner.buf.push_back(ev.clone());
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

    /// Id of the newest event, or 0 when there is none.
    pub fn last_id(&self) -> u64 {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.next_id - 1
    }

    /// Id of the oldest buffered event, or 0 when the buffer is empty.
    pub fn oldest_id(&self) -> u64 {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.buf.front().map(|e| e.id).unwrap_or(0)
    }

    /// Returns the replay (events after `since`) and a subscription for what follows.
    pub fn subscribe(&self, since: u64) -> (Vec<Arc<Event>>, Subscription) {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let rx = self.tx.subscribe();
        let replay = since_locked(&inner, since);
        // Everything up to the newest id is either in the replay or older than `since`;
        // the receiver only carries events pushed after this point.
        (replay, Subscription { rx, cursor: inner.next_id - 1 })
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
        let mut f = File::create(&tmp)?;
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
    /// Waits for the next live events. When this subscriber has fallen behind the
    /// broadcast channel, it resyncs from the ring buffer instead of being dropped.
    /// Returns `None` once the store is gone.
    pub async fn recv(&mut self, store: &Store) -> Option<Vec<Arc<Event>>> {
        loop {
            match self.rx.recv().await {
                Ok(ev) if ev.id > self.cursor => {
                    self.cursor = ev.id;
                    return Some(vec![ev]);
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, cursor = self.cursor, "subscriber lagged; resyncing");
                    let missed = store.since(self.cursor);
                    if let Some(last) = missed.last() {
                        self.cursor = last.id;
                        return Some(missed);
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
        assert_eq!(text.lines().count(), 3, "log compacted to the buffer");
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
        let (replay, mut sub) = s.subscribe(0);
        assert_eq!(replay.len(), 1);
        for i in 0..20 {
            s.push(ev(&format!("e{i}"))).unwrap();
        }
        let mut got = Vec::new();
        while got.len() < 20 {
            for e in sub.recv(&s).await.unwrap() {
                got.push(e.id);
            }
        }
        assert_eq!(got, (2..=21).collect::<Vec<u64>>());
    }

    #[test]
    fn subscribe_replays_only_newer() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), 100, 16).unwrap();
        for i in 0..5 {
            s.push(ev(&format!("e{i}"))).unwrap();
        }
        let (replay, _) = s.subscribe(2);
        let ids: Vec<u64> = replay.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![3, 4, 5]);
        let (replay, _) = s.subscribe(99);
        assert!(replay.is_empty());
    }
}
