//! The main card: which session's turn the big card shows.
//!
//! A session is pending from its `your_turn` Stop until it is resolved: it prompts
//! again (answered), its Stop is `all_set` (finished without asking), or SessionEnd.
//! While any session is pending the main card shows `your_turn` for the most recent
//! one. When the last pending session is answered, the card reads `working.` for it
//! until that session's next Stop or SessionEnd. Otherwise the card falls back to the
//! event stream, as it did before pending existed.
//!
//! The pending cards are persisted in `control.json` (0600) so a restart keeps them.
//!
//! A viewer can dismiss the card (swipe it away). A pending `your_turn` leaves the
//! pending list; a `working.` card or a stored event is hidden, so the page shows its
//! idle state until something newer arrives. Each dismissal can be undone during a short
//! window; after it, the dismissal is final and the daemon writes a history record.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Pending sessions remembered; the oldest are dropped first.
pub const MAX_PENDING: usize = 100;

/// A pending `your_turn`: what the main card needs to show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingCard {
    pub session: String,
    /// When the card became pending (unix ms).
    pub ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
}

/// The card the page shows in place of the newest event (`main_card` frame).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MainView {
    /// `your_turn` while sessions are pending, `working` after the last one was answered.
    pub state: &'static str,
    pub session: String,
    pub ts: u64,
    /// Pending sessions, including this one (0 for `working`).
    pub count: usize,
    pub question_line: Option<String>,
    pub project: Option<String>,
    pub session_label: Option<String>,
    pub host: Option<String>,
    pub app: Option<String>,
    pub sender: Option<String>,
    /// Start of the turn, for `working` ("so far" timer).
    pub started_at: Option<u64>,
}

impl MainView {
    fn of(state: &'static str, card: &PendingCard, count: usize, started_at: Option<u64>) -> MainView {
        MainView {
            state,
            session: card.session.clone(),
            ts: card.ts,
            count,
            question_line: card.question_line.clone(),
            project: card.project.clone(),
            session_label: card.session_label.clone(),
            host: card.host.clone(),
            app: card.app.clone(),
            sender: card.sender.clone(),
            started_at,
        }
    }
}

/// The session whose last answered turn shows `working.`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Working {
    card: PendingCard,
    started_at: Option<u64>,
}

/// What a dismissal took off the main card, kept so undo can put it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Dismissed {
    /// A pending `your_turn`.
    Pending { card: PendingCard },
    /// The `working.` card.
    Working { card: PendingCard, started_at: Option<u64> },
    /// A stored event, hidden from the main card.
    Event { event_id: u64 },
}

impl Dismissed {
    pub fn kind(&self) -> &'static str {
        match self {
            Dismissed::Pending { .. } => "pending",
            Dismissed::Working { .. } => "working",
            Dismissed::Event { .. } => "event",
        }
    }
}

/// Context copied onto the history record of a dismissal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    /// The question line or the event title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl From<&PendingCard> for Subject {
    fn from(c: &PendingCard) -> Subject {
        Subject {
            session: Some(c.session.clone()),
            session_label: c.session_label.clone(),
            project: c.project.clone(),
            host: c.host.clone(),
            app: c.app.clone(),
            sender: c.sender.clone(),
            detail: c.question_line.clone(),
        }
    }
}

/// A dismissal inside its undo window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dismissal {
    pub id: u64,
    pub what: Dismissed,
    pub subject: Subject,
    /// `hidden_through` before this dismissal, and the value it set (if it changed it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_hidden: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_hidden: Option<u64>,
    /// The session changed since (asked, answered, finished), so undo would bring back a stale card.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}

/// What undo is aimed at: the request names a session or a stored event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Session(String),
    Event(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoError {
    /// No dismissal of this target is waiting for undo.
    NotFound,
    /// The card changed since it was dismissed; nothing was restored.
    Stale,
}

#[derive(Debug, Default)]
pub struct MainState {
    pending: HashMap<String, PendingCard>,
    working: Option<Working>,
    /// Stored events up to this id are hidden from the main card.
    hidden_through: Option<u64>,
    /// Dismissals that can still be undone, oldest first.
    dismissals: Vec<Dismissal>,
    next_dismissal: u64,
    /// The view last broadcast to pages. Only the daemon updates it.
    pub sent: Option<MainView>,
}

impl MainState {
    pub fn from_cards(cards: Vec<PendingCard>) -> MainState {
        let mut m = MainState::default();
        for c in cards {
            m.pending.insert(c.session.clone(), c);
        }
        m.evict();
        m
    }

    /// Pending cards, oldest first (the persisted form).
    pub fn cards(&self) -> Vec<PendingCard> {
        let mut v: Vec<PendingCard> = self.pending.values().cloned().collect();
        v.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.session.cmp(&b.session)));
        v
    }

    /// Pending sessions as `(session, ts)`, oldest first.
    pub fn pending_entries(&self) -> Vec<(String, u64)> {
        self.cards().into_iter().map(|c| (c.session, c.ts)).collect()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Puts back the dismissal state saved in control.json.
    pub fn restore_dismissals(&mut self, hidden_through: Option<u64>, dismissals: Vec<Dismissal>) {
        self.hidden_through = hidden_through;
        self.next_dismissal = dismissals.iter().map(|d| d.id).max().unwrap_or(0);
        self.dismissals = dismissals;
    }

    pub fn hidden_through(&self) -> Option<u64> {
        self.hidden_through
    }

    /// Dismissals inside their undo window (the persisted form).
    pub fn dismissals(&self) -> Vec<Dismissal> {
        self.dismissals.clone()
    }

    /// Marks dismissals of `session` stale: the session moved on, so undo must not revive them.
    fn touch(&mut self, session: &str, working_too: bool) {
        for d in &mut self.dismissals {
            let working = matches!(d.what, Dismissed::Working { .. });
            if d.subject.session.as_deref() == Some(session) || (working_too && working) {
                d.stale = true;
            }
        }
    }

    fn push_dismissal(&mut self, what: Dismissed, subject: Subject, prev_hidden: Option<u64>, set_hidden: Option<u64>) -> &Dismissal {
        self.next_dismissal += 1;
        self.dismissals.push(Dismissal { id: self.next_dismissal, what, subject, prev_hidden, set_hidden, stale: false });
        self.dismissals.last().expect("just pushed")
    }

    /// Dismisses the card of `session`: its pending `your_turn` leaves the pending list,
    /// or its `working.` card is hidden together with the events up to `last_event_id`.
    /// `None` when the session shows no card.
    pub fn dismiss_session(&mut self, session: &str, last_event_id: u64) -> Option<Dismissal> {
        if let Some(card) = self.pending.remove(session) {
            let subject = Subject::from(&card);
            return Some(self.push_dismissal(Dismissed::Pending { card }, subject, None, None).clone());
        }
        if self.working.as_ref().is_some_and(|w| w.card.session == session) {
            let w = self.working.take().expect("checked");
            let prev = self.hidden_through;
            let set = prev.unwrap_or(0).max(last_event_id);
            self.hidden_through = Some(set);
            let subject = Subject::from(&w.card);
            let what = Dismissed::Working { card: w.card, started_at: w.started_at };
            return Some(self.push_dismissal(what, subject, prev, Some(set)).clone());
        }
        None
    }

    /// Hides the stored event `event_id` (and every older one) from the main card.
    /// `None` when it is hidden already.
    pub fn dismiss_event(&mut self, event_id: u64, subject: Subject) -> Option<Dismissal> {
        let prev = self.hidden_through;
        if prev.is_some_and(|h| h >= event_id) {
            return None;
        }
        self.hidden_through = Some(event_id);
        Some(self.push_dismissal(Dismissed::Event { event_id }, subject, prev, Some(event_id)).clone())
    }

    /// Undoes the newest dismissal of `target`. A stale one is left to become final.
    pub fn undo(&mut self, target: &Target) -> Result<Dismissal, UndoError> {
        let matches = |d: &Dismissal| match (target, &d.what) {
            (Target::Event(id), Dismissed::Event { event_id }) => id == event_id,
            (Target::Session(s), Dismissed::Pending { card } | Dismissed::Working { card, .. }) => &card.session == s,
            _ => false,
        };
        let i = self.dismissals.iter().rposition(matches).ok_or(UndoError::NotFound)?;
        let d = &self.dismissals[i];
        let hidden_ok = d.set_hidden.is_none() || self.hidden_through == d.set_hidden;
        let ok = !d.stale
            && hidden_ok
            && match &d.what {
                Dismissed::Pending { card } => !self.pending.contains_key(&card.session),
                Dismissed::Working { .. } => self.working.is_none() && self.pending.is_empty(),
                Dismissed::Event { .. } => true,
            };
        if !ok {
            return Err(UndoError::Stale);
        }
        let d = self.dismissals.remove(i);
        if d.set_hidden.is_some() {
            self.hidden_through = d.prev_hidden;
        }
        match &d.what {
            Dismissed::Pending { card } => {
                self.pending.insert(card.session.clone(), card.clone());
                self.evict();
            }
            Dismissed::Working { card, started_at } => {
                self.working = Some(Working { card: card.clone(), started_at: *started_at });
            }
            Dismissed::Event { .. } => {}
        }
        Ok(d)
    }

    /// Ends the undo window of dismissal `id`; returns it if it was still open.
    pub fn finalize(&mut self, id: u64) -> Option<Dismissal> {
        let i = self.dismissals.iter().position(|d| d.id == id)?;
        Some(self.dismissals.remove(i))
    }

    /// Ends every undo window (restart).
    pub fn finalize_all(&mut self) -> Vec<Dismissal> {
        std::mem::take(&mut self.dismissals)
    }

    /// A `your_turn` Stop: the session becomes pending, and no `working.` card is left.
    pub fn your_turn(&mut self, card: PendingCard) {
        self.touch(&card.session, true);
        self.working = None;
        self.pending.insert(card.session.clone(), card);
        self.evict();
    }

    /// The session no longer waits on the user: an `all_set` Stop or SessionEnd. Returns
    /// whether anything changed. Its `working.` card, if it has one, ends too.
    pub fn resolve(&mut self, session: &str) -> bool {
        self.touch(session, false);
        let working_ended = self.working.as_ref().is_some_and(|w| w.card.session == session);
        if working_ended {
            self.working = None;
        }
        let pending_removed = self.pending.remove(session).is_some();
        working_ended || pending_removed
    }

    /// The user answered: `session` prompted. Returns whether it was pending. When it
    /// was the last pending session, the card reads `working.` for it.
    pub fn answered(&mut self, session: &str, started_at: Option<u64>) -> bool {
        self.touch(session, false);
        let Some(card) = self.pending.remove(session) else { return false };
        if self.pending.is_empty() {
            self.working = Some(Working { card, started_at });
        }
        true
    }

    /// The card to show, without the session label (the caller fills it in from the
    /// summarizer, which lives outside this state).
    pub fn view(&self) -> Option<MainView> {
        if let Some(top) = self.pending.values().max_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.session.cmp(&b.session))) {
            return Some(MainView::of("your_turn", top, self.pending.len(), None));
        }
        self.working.as_ref().map(|w| MainView::of("working", &w.card, 0, w.started_at))
    }

    fn evict(&mut self) {
        while self.pending.len() > MAX_PENDING {
            let oldest = self.pending.values().min_by_key(|c| c.ts).map(|c| c.session.clone());
            let Some(s) = oldest else { break };
            self.pending.remove(&s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(session: &str, ts: u64, question: &str) -> PendingCard {
        PendingCard {
            session: session.into(),
            ts,
            question_line: Some(question.into()),
            project: Some("nearpush".into()),
            session_label: None,
            host: None,
            app: Some("Kiro".into()),
            sender: Some("kiro".into()),
        }
    }

    #[test]
    fn user_scenario_keeps_your_turn_until_last_pending_answered() {
        let mut m = MainState::default();
        m.your_turn(card("A", 1, "A asks?"));
        m.your_turn(card("B", 2, "B asks?"));
        // A finishes without asking anything: B still waits, so the card stays your_turn for B.
        assert!(m.resolve("A"));
        let v = m.view().unwrap();
        assert_eq!((v.state, v.session.as_str(), v.count), ("your_turn", "B", 1));
        assert_eq!(v.question_line.as_deref(), Some("B asks?"));
        // B answers: nothing is pending, so the card reads working. for B.
        assert!(m.answered("B", Some(9)));
        let v = m.view().unwrap();
        assert_eq!((v.state, v.session.as_str(), v.count, v.started_at), ("working", "B", 0, Some(9)));
    }

    #[test]
    fn answering_with_others_pending_shows_next_your_turn() {
        let mut m = MainState::default();
        m.your_turn(card("A", 1, "A?"));
        m.your_turn(card("B", 2, "B?"));
        assert!(m.answered("B", None));
        let v = m.view().unwrap();
        assert_eq!((v.state, v.session.as_str(), v.count), ("your_turn", "A", 1));
    }

    #[test]
    fn session_end_clears_its_pending_entry() {
        let mut m = MainState::default();
        m.your_turn(card("A", 1, "A?"));
        assert!(m.resolve("A"));
        assert!(!m.resolve("A"), "already gone");
        assert!(m.view().is_none());
    }

    #[test]
    fn session_end_of_working_session_ends_working() {
        let mut m = MainState::default();
        m.your_turn(card("A", 1, "A?"));
        m.answered("A", None);
        assert_eq!(m.view().unwrap().state, "working");
        m.resolve("A");
        assert!(m.view().is_none(), "falls back to the event stream");
    }

    #[test]
    fn new_your_turn_replaces_working() {
        let mut m = MainState::default();
        m.your_turn(card("A", 1, "A?"));
        m.answered("A", None);
        m.your_turn(card("C", 5, "C?"));
        let v = m.view().unwrap();
        assert_eq!((v.state, v.session.as_str()), ("your_turn", "C"));
        m.resolve("C");
        assert!(m.view().is_none(), "working for A is not revived");
    }

    #[test]
    fn restored_cards_come_back_in_order() {
        let mut m = MainState::default();
        m.your_turn(card("B", 2, "B?"));
        m.your_turn(card("A", 1, "A?"));
        let cards = m.cards();
        assert_eq!(cards.iter().map(|c| c.session.as_str()).collect::<Vec<_>>(), vec!["A", "B"]);
        let restored = MainState::from_cards(cards);
        assert_eq!(restored.view().unwrap().session, "B");
        assert_eq!(restored.pending_count(), 2);
    }

    #[test]
    fn dismissing_your_turn_shows_next_pending_and_undo_restores_it() {
        let mut m = MainState::default();
        m.your_turn(card("A", 1, "A?"));
        m.your_turn(card("B", 2, "B?"));
        let d = m.dismiss_session("B", 7).unwrap();
        assert_eq!(d.what.kind(), "pending");
        assert_eq!(d.subject.detail.as_deref(), Some("B?"));
        let v = m.view().unwrap();
        assert_eq!((v.state, v.session.as_str(), v.count), ("your_turn", "A", 1));
        assert_eq!(m.hidden_through(), None, "a pending dismissal hides no events");
        assert!(m.dismiss_session("B", 7).is_none(), "already gone");
        m.undo(&Target::Session("B".into())).unwrap();
        assert_eq!(m.view().unwrap().session, "B");
        assert_eq!(m.undo(&Target::Session("B".into())).unwrap_err(), UndoError::NotFound);
    }

    #[test]
    fn last_pending_dismissed_leaves_no_card() {
        let mut m = MainState::default();
        m.your_turn(card("A", 1, "A?"));
        m.dismiss_session("A", 3).unwrap();
        assert!(m.view().is_none(), "falls back to the stream (all set)");
        assert_eq!(m.pending_count(), 0);
    }

    #[test]
    fn undo_is_refused_when_the_session_moved_on() {
        let mut m = MainState::default();
        m.your_turn(card("A", 1, "A?"));
        m.dismiss_session("A", 3).unwrap();
        m.your_turn(card("A", 5, "again?"));
        assert_eq!(m.undo(&Target::Session("A".into())).unwrap_err(), UndoError::Stale);
        assert_eq!(m.view().unwrap().question_line.as_deref(), Some("again?"), "newer card kept");
        m.resolve("A");
        assert!(m.view().is_none());
        assert_eq!(m.finalize_all().len(), 1, "a stale dismissal still becomes final");
    }

    #[test]
    fn working_and_event_dismissals_hide_and_restore() {
        let mut m = MainState::default();
        m.your_turn(card("A", 1, "A?"));
        m.answered("A", Some(4));
        let d = m.dismiss_session("A", 9).unwrap();
        assert_eq!((d.what.kind(), m.hidden_through()), ("working", Some(9)));
        assert!(m.view().is_none());
        m.undo(&Target::Session("A".into())).unwrap();
        assert_eq!((m.view().unwrap().state, m.hidden_through()), ("working", None));

        let mut m = MainState::default();
        let d = m.dismiss_event(5, Subject::default()).unwrap();
        assert_eq!(m.hidden_through(), Some(5));
        assert!(m.dismiss_event(4, Subject::default()).is_none(), "already hidden");
        assert!(m.finalize(d.id).is_some());
        assert_eq!(m.undo(&Target::Event(5)).unwrap_err(), UndoError::NotFound, "final after the window");
        assert_eq!(m.hidden_through(), Some(5));
    }

    #[test]
    fn bounded_by_dropping_oldest() {
        let mut m = MainState::default();
        for i in 0..(MAX_PENDING as u64 + 3) {
            m.your_turn(card(&format!("s{i}"), i, "q"));
        }
        assert_eq!(m.pending_count(), MAX_PENDING);
        assert!(!m.resolve("s0"), "oldest dropped");
    }
}
