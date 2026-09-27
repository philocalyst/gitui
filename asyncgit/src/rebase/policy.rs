//! One pure function decides everything the user can see and do.
//!
//! [`policy`] takes the folded [`Session`] plus the current
//! [`Snapshot`] and produces the complete projection: the phase
//! label, every rail row with its state, the inspector content,
//! and every action with its availability and, when disabled, the
//! exact blocker. The tab, the command bar, the help popup, and
//! the key handler are all consumers of this one value; none of
//! them re-derive rules.
//!
//! ## Shape
//!
//! Actions are a fixed struct of [`Action`] entries rather than a
//! bag of booleans, so adding an action is a type-level change
//! that the compiler walks through every consumer. Blockers carry
//! data (which paths, which ref, which precondition) — the same
//! evidence renders in the UI and gates the dispatch.
//!
//! Dispatchers still revalidate against a fresh snapshot at
//! execution time: a policy is a projection of a moment, not a
//! token against the future.

use crate::rebase::{
	journal::Session,
	snapshot::{Operation, Snapshot},
	StepId,
};
use crate::sync::CommitId;

/// The complete projection for one moment.
#[derive(Clone, Debug)]
pub struct Policy {
	/// Which phase the workspace presents.
	pub phase: Phase,
	/// Rail rows, in program order.
	pub rail: Vec<Row>,
	/// What the inspector shows.
	pub inspector: Inspector,
	/// Every action and its availability.
	pub actions: Actions,
	/// Progress summary (counts, honestly separated).
	pub progress: Progress,
}

/// The phases the workspace distinguishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
	/// Editing a plan that has not run.
	Draft,
	/// Reviewing before start.
	Review,
	/// A git process is currently executing.
	Executing,
	/// Stopped, waiting for the user.
	Stopped(Stop),
	/// Rebase done, local changes still to restore.
	Restoring,
	/// Terminal: finished (or aborted/quit).
	Done(Done),
	/// Adopted an external rebase; observing only.
	Adopted {
		/// Whether limited mutations are offered.
		managed: bool,
	},
	/// State disagrees with the journal; nothing mutates.
	Reconciling {
		/// One-line human reason (already localized).
		reason: String,
	},
	/// The repository is in a state we do not model.
	Unsupported {
		/// The observed operation.
		operation: Operation,
	},
}

/// Why a rebase is stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
	/// Conflicts need resolution.
	Conflicts {
		/// The unresolved paths.
		paths: Vec<String>,
	},
	/// An edit stop: amend/split/continue.
	Edit {
		/// The commit under amendment.
		commit: CommitId,
	},
	/// A deliberate `break`.
	Break,
	/// A commit became empty while replaying.
	BecameEmpty {
		/// The commit that vanished.
		commit: CommitId,
	},
	/// A message is being composed through the bridge.
	Message {
		/// The file under edit.
		request: String,
	},
	/// An `exec` step failed.
	ExecFailed {
		/// The command that failed.
		command: String,
		/// Its exit status, if recorded.
		code: Option<i32>,
	},
	/// Something else; evidence shown, nothing guessed.
	Unknown {
		/// Best-effort classification note.
		note: String,
	},
}

/// Terminal outcomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Done {
	/// History rewritten (restoration may still be pending).
	Finished,
	/// Aborted.
	Aborted,
	/// Quit.
	Quit,
}

/// One rail row, fully resolved for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
	/// The step this row renders (None for synthetic rows
	/// like a pending exec-failure marker).
	pub step: Option<StepId>,
	/// Execution state of this row.
	pub state: RowState,
	/// Label (subject or command).
	pub label: String,
	/// Short oid, when the row has a commit.
	pub short_oid: Option<String>,
	/// Whether this row is part of the previous row's output
	/// group (a fold).
	pub folds_into_previous: bool,
}

/// Execution state of one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowState {
	/// Not yet reached.
	Pending,
	/// Already applied; output known when recorded.
	Done,
	/// Currently executing / at the stop.
	Current,
	/// Dropped by plan.
	Dropped,
	/// Deliberate stop (edit/break).
	Stop,
	/// Failed (conflict, exec).
	Failed,
}

/// What the inspector shows right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inspector {
	/// Structural preview of the planned result.
	Preview,
	/// Details of the current stop.
	Stop,
	/// Preflight review before start.
	Review,
	/// Outcome and correspondence.
	Outcome,
	/// Reconciliation explanation.
	Reconcile,
}

/// Honest progress counters. Never summed into one percentage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
	/// Instructions completed.
	pub done: usize,
	/// Total instructions (known once started).
	pub total: Option<usize>,
	/// Unresolved conflict paths.
	pub unresolved: usize,
}

/// Why an action is unavailable. Carries evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocker {
	/// Unresolved paths listed here.
	Conflicts(Vec<String>),
	/// A command is in flight.
	Busy,
	/// The plan has validation issues.
	Invalid(Vec<String>),
	/// Local changes need protection first.
	LocalChanges,
	/// The feature needs a newer git.
	GitVersion {
		/// Feature name.
		feature: String,
		/// Required version.
		needs: String,
	},
	/// Nothing to act on.
	NothingToDo,
	/// Not available in the adopted external session.
	Unmanaged,
	/// The session already ended.
	Ended,
}

/// Availability of one action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
	/// Ready.
	Enabled,
	/// Visible but blocked; carries the why.
	Disabled(Vec<Blocker>),
	/// Not offered in this phase at all.
	Hidden,
}

impl Availability {
	/// Whether the action is ready.
	#[must_use]
	pub const fn is_enabled(&self) -> bool {
		matches!(self, Self::Enabled)
	}
}

/// Every action the workspace knows, in one struct.
#[derive(Clone, Debug)]
pub struct Actions {
	/// Start the reviewed rebase.
	pub start: Availability,
	/// Continue the stopped rebase.
	pub r#continue: Availability,
	/// Skip the current step.
	pub skip: Availability,
	/// Abort the rebase.
	pub abort: Availability,
	/// Quit (keep current state).
	pub quit: Availability,
	/// Edit the remaining program.
	pub edit_plan: Availability,
	/// Mark the selected step dropped/kept.
	pub toggle_drop: Availability,
	/// Cycle the selected step's action.
	pub set_action: Availability,
	/// Move the selected step up.
	pub move_up: Availability,
	/// Move the selected step down.
	pub move_down: Availability,
	/// Insert a break before the selection.
	pub insert_break: Availability,
	/// Insert an exec before the selection.
	pub insert_exec: Availability,
	/// Compose the selected reword/squash message.
	pub compose: Availability,
	/// Undo the last plan edit.
	pub undo: Availability,
	/// Redo an undone plan edit.
	pub redo: Availability,
	/// Resolve pending message (composer save).
	pub save_message: Availability,
	/// Cancel pending message request.
	pub cancel_message: Availability,
	/// Amend at an edit stop.
	pub amend: Availability,
	/// Continue an edit stop unchanged.
	pub continue_unchanged: Availability,
	/// Keep a became-empty commit.
	pub keep_empty: Availability,
	/// Restore protected local changes.
	pub restore: Availability,
	/// Create a recovery branch at the original tip.
	pub recovery_branch: Availability,
	/// Close the session workspace.
	pub close: Availability,
}

/// Derive the full projection. Pure.
#[must_use]
pub fn policy(session: &Session, snapshot: &Snapshot) -> Policy {
	let _ = (session, snapshot);
	Policy {
		phase: Phase::Draft,
		rail: Vec::new(),
		inspector: Inspector::Preview,
		actions: disabled_everywhere(),
		progress: Progress::default(),
	}
}

fn disabled_everywhere() -> Actions {
	let d = || Availability::Disabled(vec![Blocker::NothingToDo]);
	Actions {
		start: d(),
		r#continue: d(),
		skip: d(),
		abort: d(),
		quit: d(),
		edit_plan: d(),
		toggle_drop: d(),
		set_action: d(),
		move_up: d(),
		move_down: d(),
		insert_break: d(),
		insert_exec: d(),
		compose: d(),
		undo: d(),
		redo: d(),
		save_message: d(),
		cancel_message: d(),
		amend: d(),
		continue_unchanged: d(),
		keep_empty: d(),
		restore: d(),
		recovery_branch: d(),
		close: d(),
	}
}
