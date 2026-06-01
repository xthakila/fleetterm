//! In-memory fleet state: the session table, the fleet-wide default autonomy, the
//! pending-decision map (PreToolUse escalations awaiting a human), and the broadcast
//! channel that fans [`Event`]s out to every subscribed UI.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use protocol::{
    Autonomy, DecisionKind, Event, HookDecision, Session, SessionId, State, Target, Tool,
};
use tokio::sync::{broadcast, oneshot};

pub struct Registry {
    inner: Mutex<Inner>,
    events: broadcast::Sender<Event>,
}

struct Inner {
    sessions: BTreeMap<SessionId, Session>,
    default_autonomy: Autonomy,
    next_id: u64,
    /// Hooks blocked on a human decision, keyed by session. Resolved by `Request::Decide`.
    pending: HashMap<SessionId, oneshot::Sender<HookDecision>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(2048);
        Registry {
            inner: Mutex::new(Inner {
                sessions: BTreeMap::new(),
                default_autonomy: Autonomy::Guarded,
                next_id: 1,
                pending: HashMap::new(),
            }),
            events,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    fn emit(&self, ev: Event) {
        // Err just means no subscribers right now — fine.
        let _ = self.events.send(ev);
    }

    pub fn alloc_id(&self) -> SessionId {
        let mut g = self.inner.lock().unwrap();
        let id = SessionId(g.next_id);
        g.next_id += 1;
        id
    }

    pub fn default_autonomy(&self) -> Autonomy {
        self.inner.lock().unwrap().default_autonomy
    }

    pub fn set_default_autonomy(&self, level: Autonomy) {
        self.inner.lock().unwrap().default_autonomy = level;
    }

    /// Insert (or replace) a session and announce it.
    pub fn insert(&self, session: Session) {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .insert(session.id.clone(), session.clone());
        self.emit(Event::SessionUpdate(session));
    }

    /// Ensure a session exists (hook events may reference one we haven't registered yet),
    /// returning a snapshot of it.
    pub fn ensure(&self, id: &SessionId, name: Option<String>, tool: Tool) -> Session {
        let mut g = self.inner.lock().unwrap();
        let def = g.default_autonomy;
        g.sessions
            .entry(id.clone())
            .or_insert_with(|| Session {
                id: id.clone(),
                name: name.unwrap_or_else(|| format!("agent-{}", id.0)),
                tool,
                state: State::Working,
                autonomy: def,
                branch: None,
                activity: String::new(),
                cost_usd: 0.0,
                context_frac: None,
            })
            .clone()
    }

    pub fn get(&self, id: &SessionId) -> Option<Session> {
        self.inner.lock().unwrap().sessions.get(id).cloned()
    }

    pub fn snapshot_event(&self) -> Event {
        let g = self.inner.lock().unwrap();
        Event::Snapshot {
            sessions: g.sessions.values().cloned().collect(),
            default_autonomy: g.default_autonomy,
            total_cost_usd: g.sessions.values().map(|s| s.cost_usd).sum(),
        }
    }

    /// Resolve a [`Target`] to concrete session ids.
    pub fn resolve_targets(&self, target: &Target) -> Vec<SessionId> {
        let g = self.inner.lock().unwrap();
        match target {
            Target::Session(id) => {
                if g.sessions.contains_key(id) {
                    vec![id.clone()]
                } else {
                    vec![]
                }
            }
            Target::All => g.sessions.keys().cloned().collect(),
            Target::AllWorking => g
                .sessions
                .values()
                .filter(|s| matches!(s.state, State::Working))
                .map(|s| s.id.clone())
                .collect(),
        }
    }

    pub fn set_state(&self, id: &SessionId, state: State, activity: impl Into<String>) {
        let updated = {
            let mut g = self.inner.lock().unwrap();
            g.sessions.get_mut(id).map(|s| {
                s.state = state;
                s.activity = activity.into();
                s.clone()
            })
        };
        if let Some(s) = updated {
            self.emit(Event::SessionUpdate(s));
        }
    }

    pub fn set_autonomy(&self, id: &SessionId, level: Autonomy) {
        let updated = {
            let mut g = self.inner.lock().unwrap();
            g.sessions.get_mut(id).map(|s| {
                s.autonomy = level;
                s.clone()
            })
        };
        if let Some(s) = updated {
            self.emit(Event::SessionUpdate(s));
        }
    }

    pub fn remove(&self, id: &SessionId) {
        self.inner.lock().unwrap().sessions.remove(id);
        self.emit(Event::SessionRemoved(id.clone()));
    }

    /// Stream a chunk of PTY output for a session to subscribers.
    pub fn emit_output(&self, id: SessionId, data: Vec<u8>) {
        self.emit(Event::Output { session: id, data });
    }

    pub fn emit_error(&self, message: String) {
        self.emit(Event::Error { message });
    }

    pub fn emit_decision_pending(&self, id: SessionId, kind: DecisionKind) {
        self.emit(Event::DecisionPending { session: id, kind });
    }

    pub fn emit_auto(&self, id: SessionId, kind: DecisionKind, approved: bool, reason: &str) {
        self.emit(Event::AutoDecision {
            session: id,
            kind,
            approved,
            reason: reason.to_string(),
        });
    }

    /// Register a hook awaiting a human decision.
    pub fn register_pending(&self, id: SessionId, tx: oneshot::Sender<HookDecision>) {
        self.inner.lock().unwrap().pending.insert(id, tx);
    }

    /// Take and remove a pending decision sender (called by `Request::Decide`).
    pub fn take_pending(&self, id: &SessionId) -> Option<oneshot::Sender<HookDecision>> {
        self.inner.lock().unwrap().pending.remove(id)
    }
}
