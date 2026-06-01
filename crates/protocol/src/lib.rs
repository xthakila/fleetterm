//! FleetTerm wire protocol.
//!
//! Three parties speak this:
//!   * the **UI** (`fleetterm`) — opens a unix socket to the daemon, sends [`Request`], receives [`Event`].
//!   * the **daemon** (`fleetd`) — owns PTYs + state, answers requests, streams events.
//!   * the **hook forwarder** (`fleetterm-hook`) — spawned by an agent's hook, sends one [`HookEnvelope`].
//!
//! Framing on the wire is length-prefixed msgpack (`u32` big-endian length, then the
//! rmp-serde body). The codec lives here so every party agrees; see [`codec`].

use serde::{Deserialize, Serialize};

pub mod codec;
pub mod safety;

/// Stable identifier for a session (shell or agent). Assigned by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId(pub u64);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Which CLI a session is running. `Shell` is a plain login shell (no agent layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tool {
    Shell,
    Claude,
    Codex,
    Aider,
    Gemini,
    Other,
}

/// How much a session may do without asking the human. The core agent-first primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Autonomy {
    /// Ask before every tool/command.
    Manual,
    /// Auto-approve safe actions; escalate risky/destructive ones to the human.
    Guarded,
    /// Run freely within budget. Still subject to the hardcoded never-auto denylist.
    Auto,
}

impl Default for Autonomy {
    fn default() -> Self {
        Autonomy::Guarded
    }
}

/// Why a session is waiting on the human.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionKind {
    /// A tool/command needs approval, e.g. `Bash(rm -rf build/)`.
    Permission { tool: String, command: String },
    /// The agent asked a free-text question and is blocked on the answer.
    Question { prompt: String },
}

/// Coarse lifecycle state, the thing the attention queue sorts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Working,
    NeedsInput(DecisionKind),
    Idle,
    /// Process alive but no progress (grid frozen / no output) past the stuck threshold.
    Stuck,
    Done,
    Dead,
}

impl State {
    /// Attention priority — higher means "deal with me first". Drives the queue + sort.
    pub fn priority(&self) -> u8 {
        match self {
            State::NeedsInput(DecisionKind::Permission { .. }) => 5,
            State::NeedsInput(DecisionKind::Question { .. }) => 4,
            State::Stuck => 3,
            State::Done => 2,
            State::Working => 1,
            State::Idle => 0,
            State::Dead => 0,
        }
    }

    pub fn needs_human(&self) -> bool {
        matches!(self, State::NeedsInput(_) | State::Stuck)
    }
}

/// A point-in-time snapshot of one session, as shown in the fleet sidebar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub tool: Tool,
    pub state: State,
    pub autonomy: Autonomy,
    /// git branch / worktree, if any.
    pub branch: Option<String>,
    /// one-line "what it's doing right now" for the card.
    pub activity: String,
    /// cumulative USD cost, when the tool reports it.
    pub cost_usd: f64,
    /// context-window usage 0.0..=1.0, when known.
    pub context_frac: Option<f32>,
}

/// What spawning a new session needs. Mirrors the sidebar "+ New agent" form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub name: Option<String>,
    pub tool: Tool,
    pub model: Option<String>,
    pub cwd: Option<String>,
    /// create+use a git worktree off this base branch.
    pub worktree_from: Option<String>,
    pub autonomy: Autonomy,
    /// the opening prompt / command to send once live.
    pub opening: Option<String>,
    pub env: Vec<(String, String)>,
}

/// How an input is addressed: one session, or fan-out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Target {
    Session(SessionId),
    /// all sessions currently in the working state — broadcast to busy agents.
    AllWorking,
    All,
}

/// UI → daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Begin receiving [`Event`]s. The daemon replies with [`Event::Snapshot`] first.
    Subscribe,
    Spawn(SpawnSpec),
    /// Write raw bytes to a session's PTY (keystrokes, a prompt, etc.).
    Input { target: Target, data: Vec<u8> },
    /// Resolve a pending decision.
    Decide {
        session: SessionId,
        approve: bool,
        /// optional instruction when denying ("deny & tell it what to do").
        instruction: Option<String>,
    },
    SetAutonomy { session: SessionId, level: Autonomy },
    /// Fleet-wide default for new sessions.
    SetDefaultAutonomy(Autonomy),
    Pause(Target),
    Resume(Target),
    Stop(Target),
    /// Resize a session's PTY to the focused view's dimensions.
    Resize { session: SessionId, cols: u16, rows: u16 },
    /// Ask for the full server-side grid of a session (on attach/focus).
    RequestGrid(SessionId),
    Close(SessionId),
}

/// daemon → UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// Sent once on subscribe: the whole fleet.
    Snapshot {
        sessions: Vec<Session>,
        default_autonomy: Autonomy,
        total_cost_usd: f64,
    },
    /// A session changed (state, activity, cost, autonomy, …). Replaces prior snapshot entry.
    SessionUpdate(Session),
    SessionRemoved(SessionId),
    /// Incremental PTY output for a *visible* session.
    Output { session: SessionId, data: Vec<u8> },
    /// A new decision needs the human — drives the decisions inbox + toast.
    DecisionPending { session: SessionId, kind: DecisionKind },
    /// An autonomy auto-decision was made (for the audit log / activity feed).
    AutoDecision {
        session: SessionId,
        kind: DecisionKind,
        approved: bool,
        reason: String,
    },
    Error { message: String },
}

/// The hook lifecycle points we register with Claude Code. Mirrors hook event names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookKind {
    Notification,
    PreToolUse,
    PostToolUse,
    Stop,
    SessionEnd,
    UserPromptSubmit,
}

/// What `fleetterm-hook` forwards to the daemon: the hook kind, the owning session
/// (from `$FLEETTERM_SESSION`), and the raw hook JSON payload (parsed daemon-side).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEnvelope {
    pub session: SessionId,
    pub kind: HookKind,
    pub payload_json: String,
}

/// The daemon's final answer to a blocking hook (Claude-facing). `Ask` is never sent
/// back as-is — the daemon resolves an escalation into `Allow`/`Deny` by holding the
/// hook open until the human decides, so the hook only ever prints one of these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookDecision {
    Allow,
    Deny { reason: String },
}

/// daemon → hook forwarder. `decision` is `Some` only for events that gate the agent
/// (PreToolUse); for fire-and-forget events the daemon replies `None` and the hook exits 0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookReply {
    pub decision: Option<HookDecision>,
}

/// Top-level frame — the daemon accepts UI clients and hook forwarders on the same
/// listener and dispatches by variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Frame {
    Request(Request),
    Hook(HookEnvelope),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_orders_attention_correctly() {
        let perm = State::NeedsInput(DecisionKind::Permission {
            tool: "Bash".into(),
            command: "rm -rf build/".into(),
        });
        let q = State::NeedsInput(DecisionKind::Question { prompt: "y/n?".into() });
        assert!(perm.priority() > q.priority());
        assert!(q.priority() > State::Stuck.priority());
        assert!(State::Stuck.priority() > State::Done.priority());
        assert!(State::Done.priority() > State::Working.priority());
        assert!(State::Working.priority() > State::Idle.priority());
        assert!(perm.needs_human() && !State::Working.needs_human());
    }
}
