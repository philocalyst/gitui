//! The session as an append-only event log.
//!
//! One file — `journal.jsonl` under the session directory — is
//! the persistence, the undo history, and the crash record. The
//! current state is always `fold(events)`; there is no separate
//! "save" step to forget. Each event is a small JSON line
//! (schema-versioned) appended and flushed before its effect is
//! relied upon.
//!
//! ## Undo
//!
//! Plan edits are recorded as `Edited` events that carry the
//! complete resulting plan, so undo is "replay up to the previous
//! `Edited`" — no inverse logic to get wrong, ever.
//!
//! ## Crash protocol
//!
//! A `Command` event is appended *before* the engine runs. On
//! load, a trailing `Command` without its matching `Finished`
//! means the outcome of that command is unknown: git state wins,
//! and the session enters reconciliation rather than assuming
//! anything.
//!
//! ## Git remains authoritative
//!
//! The journal records what gitui *did and intended*. It never
//! replays git effects on load. Re-observation, not replay, is
//! the recovery mechanism.

use crate::rebase::{
	engine::{Base, EmptyPolicy},
	plan::{Edit, Plan},
	Generation,
};
use crate::sync::CommitId;

/// Everything appended to the journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
	/// A draft was created for a scope.
	Created {
		/// The scope pinning the draft.
		scope: Scope,
	},

	/// An in-progress rebase found in the repository was
	/// adopted read-only.
	Adopted,

	/// The user edited the plan. Carries the full new plan.
	Edited {
		/// The edit (for the change ledger).
		edit: Edit,
		/// The complete plan after the edit.
		plan: Plan,
	},

	/// A rebase was started.
	Started {
		/// The todo handed to git.
		todo: Vec<u8>,
		/// The options in force.
		base: Base,
		/// Empty-commit policy.
		empty: EmptyPolicy,
		/// Recovery ref created first, if any.
		recovery_ref: Option<(String, CommitId)>,
		/// Stash protecting local changes, if any.
		protection: Option<Protection>,
	},

	/// A command is about to run (intent recorded).
	Command {
		/// Which command.
		intent: String,
	},

	/// The last command finished; the repository was
	/// re-observed afterwards.
	Finished {
		/// The observed exit code.
		code: i32,
		/// Head after the command, if any.
		head: Option<CommitId>,
		/// The observation generation this fact is from.
		generation: Generation,
	},

	/// Local changes were protected (stash created).
	Protected {
		/// Exact stash commit oid.
		stash: CommitId,
		/// Whether untracked files were included.
		include_untracked: bool,
	},

	/// Protected changes were restored.
	Restored {
		/// The stash that was restored.
		stash: CommitId,
		/// Whether the restore applied cleanly.
		clean: bool,
	},

	/// The rebase ended; branch observed at this commit.
	Completed {
		/// Branch tip after the rebase.
		tip: CommitId,
	},

	/// The rebase was aborted.
	Aborted,

	/// The session was closed by the user.
	Closed,
}

/// The scope a draft is pinned to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
	/// The branch (or detached head marker) being rewritten.
	pub branch: Option<String>,
	/// Original tip at draft time.
	pub original_tip: CommitId,
	/// Range base.
	pub base: Base,
}

/// Recorded local-change protection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Protection {
	/// Exact stash commit oid (never `stash@{n}`).
	pub stash: CommitId,
	/// Whether untracked files were included.
	pub include_untracked: bool,
}

/// The folded session state.
#[derive(Clone, Debug, Default)]
pub struct Session {
	/// Current phase (derived).
	pub phase: Phase,
	/// The current plan, if any.
	pub plan: Option<Plan>,
	/// Pinning scope, once created.
	pub scope: Option<Scope>,
	/// True when the session adopted an external rebase.
	pub adopted: bool,
	/// The `Started` facts, once running.
	pub started: Option<Started>,
	/// Undoable plan edits, in order (for the ledger).
	pub edits: Vec<Edit>,
	/// Edits undone and redoable (bounded by the next
	/// branching edit).
	pub redoable: usize,
	/// Protection in force.
	pub protection: Option<Protection>,
	/// Outcome, once terminal.
	pub outcome: Option<Outcome>,
	/// A command was recorded whose finish is missing.
	pub uncertain_command: Option<String>,
}

/// Facts established at `Started`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
	/// The todo handed to git.
	pub todo: Vec<u8>,
	/// Recovery ref, if created.
	pub recovery_ref: Option<(String, CommitId)>,
	/// Empty policy in force.
	pub empty: EmptyPolicy,
}

/// Terminal session outcomes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
	/// Rebase completed; this is the new tip.
	Completed(CommitId),
	/// Aborted back to the original state.
	Aborted,
	/// Quit: bookkeeping dropped, state kept.
	Quit,
}

/// Where the session is, derived by folding.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase {
	/// No session yet.
	#[default]
	Empty,
	/// A draft exists; nothing has run.
	Draft,
	/// A rebase was started and is between stops.
	Running,
	/// Terminal: completed (possibly with restoration pending).
	Finished(CommitId),
	/// Terminal: aborted.
	Aborted,
	/// Terminal: quit.
	Quit,
	/// The session was closed.
	Closed,
}

impl Session {
	/// Fold events into session state.
	#[must_use]
	pub fn fold(events: &[Event]) -> Self {
		let _ = events;
		Self::default()
	}

	/// Whether a plan edit can be undone now: only while a
	/// draft exists and edits were recorded.
	#[must_use]
	pub fn can_undo(&self) -> bool {
		self.phase == Phase::Draft && !self.edits.is_empty()
	}

	/// Whether an undone edit can be redone now. Redo
	/// history appears once undo trims the edit log (the
	/// folded state tracks it then).
	#[must_use]
	pub const fn can_redo(&self) -> bool {
		self.redoable > 0
	}
}
