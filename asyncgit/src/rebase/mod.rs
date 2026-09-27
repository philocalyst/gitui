//! The interactive-rebase domain.
//!
//! gitui's rebase subsystem is built on five seams. Everything else
//! is plumbing between them:
//!
//! 1. [`todo`] — git's todo list as a lossless document
//!    (bytes ⇄ typed lines, byte-exact round trip).
//! 2. [`plan`] — that document annotated with stable step ids,
//!    user edits, validation, and compilation back to bytes.
//! 3. [`journal`] — the session as an append-only event log;
//!    fold it to get state, truncate it to undo, replay it to
//!    recover after a crash.
//! 4. [`snapshot`] — one consistent, typed observation of the
//!    repository (refs, index, worktree, sequencer state).
//! 5. [`engine`] — the only code that touches processes or the
//!    worktree: commands in, outcome plus fresh snapshot out.
//!
//! [`policy`] derives everything the UI shows and may do from a
//! session plus a snapshot. The UI layer never inspects git, and
//! the engine layer never makes product decisions.
//!
//! ## Invariants
//!
//! - Nothing in `todo`, `plan`, `journal`, or `policy` performs
//!   I/O; they compile and test without a repository.
//! - Observed state always wins over remembered intent: after any
//!   doubt (crash, external change, uncertain command outcome) the
//!   session reconciles against a fresh [`snapshot::Snapshot`].
//! - Uncertainty is a first-class value, never an error string.

pub mod engine;
pub mod helper;
pub mod journal;
pub mod plan;
pub mod policy;
pub mod snapshot;
pub mod todo;

/// Identity of one step inside a plan.
///
/// Ids are handed out by the [`plan::Plan`] and stay stable while
/// the user reorders, regroups, or rewrites steps. Rows in the UI,
/// fold targets, and journal references all use this id — never a
/// list index, which changes under every edit.
///
/// The zero id is never minted and is treated as absent in
/// persisted data.
#[derive(
	Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct StepId(u32);

impl StepId {
	/// First id handed out by a fresh plan.
	pub const FIRST: Self = Self(1);

	/// The id after this one.
	#[must_use]
	pub const fn next(self) -> Self {
		Self(self.0 + 1)
	}

	/// Raw numeric value (for persistence).
	#[must_use]
	pub const fn value(self) -> u32 {
		self.0
	}

	/// Reconstruct from a persisted numeric value.
	/// `None` for the reserved zero id.
	#[must_use]
	pub const fn from_value(v: u32) -> Option<Self> {
		if v == 0 {
			None
		} else {
			Some(Self(v))
		}
	}
}

/// Monotonic observation counter.
///
/// Bumped by [`snapshot::observe`] on every read. Lets the UI
/// discard stale async results and lets the journal record which
/// generation a fact was true in.
#[derive(
	Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord,
)]
pub struct Generation(pub u64);

impl Generation {
	/// The next generation after this one.
	#[must_use]
	pub const fn bump(self) -> Self {
		Self(self.0 + 1)
	}
}

/// How confident we are in a fact shown to the user.
///
/// The renderer distinguishes these visibly; the words here are
/// domain terms, not display strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Knowledge<T> {
	/// Directly read from the repository just now.
	Observed(T),
	/// Derived from other observed facts.
	Derived(T),
	/// Part of the plan; no output exists yet.
	Planned(T),
	/// Not knowable (yet). Carries a machine-readable reason.
	Unknown(WhyUnknown),
}

/// Why something is [`Knowledge::Unknown`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhyUnknown {
	/// The relevant process is still running.
	InFlight,
	/// An external tool changed state we cannot attribute.
	ExternallyChanged,
	/// Git did not record the mapping.
	NoRecord,
}

impl<T> Knowledge<T> {
	/// The value, if there is one.
	#[must_use]
	pub const fn value(&self) -> Option<&T> {
		match self {
			Self::Observed(v)
			| Self::Derived(v)
			| Self::Planned(v) => Some(v),
			Self::Unknown(_) => None,
		}
	}
}
