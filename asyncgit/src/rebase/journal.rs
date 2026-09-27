//! The session as an append-only event log.
//!
//! One file — `journal.ron` under the session directory — is the
//! persistence, the undo history, and the crash record. The
//! current state is always [`Session::fold`] over the events;
//! there is no separate "save" step to forget. Each event is a
//! small schema-versioned line (RON-serialized) appended,
//! flushed, and synced before its effect is relied upon.
//!
//! ## Undo
//!
//! Plan edits are recorded as `Edited` events that carry the
//! complete resulting plan, so undo is "replay up to the previous
//! `Edited`" — no inverse logic to get wrong, ever. `Undo` and
//! `Redo` events move along that replay: the plan stack and the
//! redo stack are fold state, never separately saved files.
//!
//! ## Crash protocol
//!
//! A `Command` event is appended *before* the engine runs. On
//! load, a trailing `Command` without its matching `Finished`
//! means the outcome of that command is unknown: git state wins,
//! and the session enters reconciliation rather than assuming
//! anything.
//!
//! Loading is itself crash-safe: a line is committed once its
//! terminating newline is on disk. A malformed *final* line (a
//! torn write) is dropped, reported as `truncated_tail`, and
//! repaired away so appending stays safe; a malformed line
//! anywhere else means the journal cannot be trusted and is a
//! hard [`JournalError::Corrupt`]. The first line is the schema
//! marker; a marker from the future fails with
//! [`JournalError::FutureSchema`] — callers must land in a
//! read-only recovery view, never in destructive defaults: this
//! build cannot understand that history.
//!
//! ## Git remains authoritative
//!
//! The journal records what gitui *did and intended*. It never
//! replays git effects on load. Re-observation, not replay, is
//! the recovery mechanism; [`reconcile`] compares the folded
//! session with a fresh observation and classifies their
//! relationship honestly.

use crate::rebase::todo::Todo;
use crate::rebase::{
	engine::{Base, EmptyPolicy},
	plan::{Edit, Plan, StoredPlan},
	snapshot::{Operation, Snapshot},
	Generation, StepId,
};
use crate::sync::CommitId;
use serde::{Deserialize, Serialize};
use std::{
	fs::{self, OpenOptions},
	io::Write,
	path::{Path, PathBuf},
};

/// Everything appended to the journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
	/// The schema marker; the first line of every journal.
	/// A version greater than the one this build writes
	/// fails the load ([`JournalError::FutureSchema`]).
	Schema(u32),

	/// A draft was created for a scope.
	Created {
		/// The scope pinning the draft.
		scope: Scope,
	},

	/// An in-progress rebase found in the repository was
	/// adopted read-only.
	Adopted,

	/// The user adopted this plan as the draft's program: the
	/// first entry of the plan stack. Not counted in
	/// [`Session::edits`].
	PlanAdopted {
		/// The complete plan, in the persistence shape.
		plan: StoredPlan,
	},

	/// The user edited the plan. Carries the full new plan.
	Edited {
		/// The edit (for the change ledger).
		edit: Edit,
		/// The complete plan after the edit.
		plan: Plan,
	},

	/// Undo the last plan edit (one step back along the plan
	/// stack). A no-op when only the adopted plan remains.
	Undo,

	/// Redo the last undone plan edit. A no-op when nothing
	/// is redoable (a branching edit cleared the history).
	Redo,

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

	/// The rebase bookkeeping was dropped; the current
	/// state was kept (gitui's quit).
	Quit,

	/// The session was closed by the user.
	Closed,
}

/// The scope a draft is pinned to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
	/// The branch (or detached head marker) being rewritten.
	pub branch: Option<String>,
	/// Original tip at draft time.
	pub original_tip: CommitId,
	/// Range base.
	pub base: Base,
}

/// Recorded local-change protection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Protection {
	/// Exact stash commit oid (never `stash@{n}`).
	pub stash: CommitId,
	/// Whether untracked files were included.
	pub include_untracked: bool,
}

/// The folded session state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
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

	/// The edit descriptors of the redo stack, in undo
	/// order — the companions of `redoable` so a `Redo`
	/// re-adds the right ledger entry. Fold-only state.
	pub(crate) redo_edits: Vec<Edit>,
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

/// The fold's working state: the plan stack (index 0 is the
/// adopted plan, every `Edited` pushes on top) and the redo
/// stacks it trades steps with.
#[derive(Default)]
struct FoldState {
	stack: Vec<Plan>,
	redo: Vec<Plan>,
	redo_edits: Vec<Edit>,
}

impl FoldState {
	fn apply(&mut self, session: &mut Session, event: &Event) {
		match event {
			// Recorded, nothing to fold: the schema gate
			// ran at load; a restoration is a fact whose
			// protection stays in force.
			Event::Schema(_) | Event::Restored { .. } => {}
			Event::Created { scope } => {
				session.scope = Some(scope.clone());
				session.phase = Phase::Draft;
			}
			Event::Adopted => session.adopted = true,
			Event::PlanAdopted { plan } => {
				self.adopt(session, plan);
			}
			Event::Edited { edit, plan } => {
				self.stack.push(plan.clone());
				session.edits.push(edit.clone());
				// A branching edit ends redoability.
				self.redo.clear();
				self.redo_edits.clear();
			}
			Event::Undo => self.undo(session),
			Event::Redo => self.r#redo(session),
			Event::Started {
				todo,
				empty,
				recovery_ref,
				protection,
				..
			} => {
				session.started = Some(Started {
					todo: todo.clone(),
					recovery_ref: recovery_ref.clone(),
					empty: *empty,
				});
				session.phase = Phase::Running;
				session.protection.clone_from(protection);
			}
			Event::Command { intent } => {
				session.uncertain_command = Some(intent.clone());
			}
			Event::Finished { .. } => {
				session.uncertain_command = None;
			}
			Event::Protected {
				stash,
				include_untracked,
			} => {
				session.protection = Some(Protection {
					stash: *stash,
					include_untracked: *include_untracked,
				});
			}
			Event::Completed { tip } => {
				session.outcome = Some(Outcome::Completed(*tip));
				session.phase = Phase::Finished(*tip);
			}
			Event::Aborted => {
				session.outcome = Some(Outcome::Aborted);
				session.phase = Phase::Aborted;
			}
			Event::Quit => {
				session.outcome = Some(Outcome::Quit);
				session.phase = Phase::Quit;
			}
			Event::Closed => {
				session.phase = Phase::Closed;
			}
		}
	}

	/// Adopt the draft's first plan. A corrupt stored plan
	/// (only possible from a hand-mangled journal) keeps the
	/// phase, admits the plan is unknown, and recovers —
	/// never panics.
	fn adopt(&mut self, session: &mut Session, stored: &StoredPlan) {
		let Ok(plan) = Plan::from_stored(stored) else {
			session.plan = None;
			session.uncertain_command =
				Some(String::from("corrupt plan in journal"));
			return;
		};
		self.stack.push(plan);
	}

	/// One step back along the plan stack; a no-op when
	/// only the adopted plan remains.
	fn undo(&mut self, session: &mut Session) {
		if self.stack.len() <= 1 {
			return;
		}
		if let Some(plan) = self.stack.pop() {
			self.redo.push(plan);
		}
		if let Some(edit) = session.edits.pop() {
			self.redo_edits.push(edit);
		}
	}

	/// One step forward again; a no-op when the redo stack
	/// is empty.
	fn r#redo(&mut self, session: &mut Session) {
		let Some(plan) = self.redo.pop() else {
			return;
		};
		self.stack.push(plan);
		if let Some(edit) = self.redo_edits.pop() {
			session.edits.push(edit);
		}
	}
}

impl Session {
	/// Fold events into session state. Pure and total: never
	/// panics on any input, including hand-mangled journals —
	/// corrupt plan data degrades into
	/// [`Session::uncertain_command`] and recovery instead.
	///
	/// `Schema` markers are skipped (any version: loading
	/// already gated the future case; the gate lives in
	/// [`Journal::open`] only because fold may also see
	/// events that never touched a file).
	#[must_use]
	pub fn fold(events: &[Event]) -> Self {
		let mut session = Self::default();
		let mut state = FoldState::default();
		for event in events {
			state.apply(&mut session, event);
		}
		session.plan = state.stack.last().cloned();
		session.redoable = state.redo.len();
		session.redo_edits = state.redo_edits;
		session
	}

	/// Whether a plan edit can be undone now: only while a
	/// draft exists and edits were recorded. For a
	/// well-formed journal this is exactly "the plan stack
	/// is deeper than its adopted base" (every edit pushes
	/// one plan and one ledger entry).
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

/// How the journal and the repository relate, per
/// [`reconcile`]. Nothing here mutates: divergence is
/// reported with both sides for the user to resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconciliation {
	/// Journal and repository agree.
	Consistent,
	/// Journal believes X; repository says Y. Nothing
	/// mutates until the user resolves. Carries both sides.
	Diverged {
		/// What the journal believes.
		journal: Phase,
		/// What the repository is doing.
		observed: Operation,
		/// One short, factual line naming both sides.
		detail: String,
	},
	/// A recorded command never finished; git state wins.
	UncertainOutcome {
		/// The recorded intent.
		intent: String,
	},
}

impl Reconciliation {
	/// The human-facing reason a state diverged; `None` for
	/// consistent and uncertain verdicts (the latter name
	/// their intent instead).
	#[must_use]
	pub fn divergence_reason(&self) -> Option<String> {
		match self {
			Self::Diverged { detail, .. } => Some(detail.clone()),
			Self::Consistent | Self::UncertainOutcome { .. } => None,
		}
	}
}

/// Compare the folded session with a fresh observation.
///
/// Pure. Uncertainty outranks everything (a `Command` whose
/// `Finished` is missing means git state wins); terminal
/// sessions outlive repository state; everything else is
/// named honestly — never guessed away.
#[must_use]
pub fn reconcile(
	session: &Session,
	snapshot: &Snapshot,
) -> Reconciliation {
	if let Some(intent) = &session.uncertain_command {
		return Reconciliation::UncertainOutcome {
			intent: intent.clone(),
		};
	}
	let observed = snapshot.operation;
	let diverged =
		|journal: Phase, detail: String| Reconciliation::Diverged {
			journal,
			observed,
			detail,
		};
	match session.phase {
		// Post-mortem sessions outlive repository state.
		Phase::Finished(_)
		| Phase::Aborted
		| Phase::Quit
		| Phase::Closed => Reconciliation::Consistent,
		Phase::Running => {
			if observed.is_rebase() {
				Reconciliation::Consistent
			} else if observed == Operation::Clean {
				diverged(
					Phase::Running,
					String::from("the rebase ended outside gitui"),
				)
			} else {
				diverged(
					Phase::Running,
					format!(
						"the repository moved to \
						 another operation ({observed:?})"
					),
				)
			}
		}
		// A session that never began reconciles like a
		// draft: nothing of ours is running.
		Phase::Draft | Phase::Empty => {
			if observed.is_rebase() {
				diverged(
					session.phase.clone(),
					String::from(
						"a rebase is running that this \
						 session did not start",
					),
				)
			} else if observed == Operation::Clean {
				Reconciliation::Consistent
			} else {
				diverged(
					session.phase.clone(),
					format!(
						"the repository moved to \
						 another operation ({observed:?})"
					),
				)
			}
		}
	}
}

/// The session file name under the session directory.
const JOURNAL_FILE: &str = "journal.ron";

/// The schema version this build writes. Loads accept this
/// version and any lower one.
const SCHEMA_VERSION: u32 = 1;

/// Why a journal could not be opened or extended.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
	/// Filesystem access failed.
	#[error("journal i/o failed: {0}")]
	Io(#[from] std::io::Error),

	/// An event could not be serialized for appending.
	#[error("journal serialization failed: {0}")]
	Format(String),

	/// A malformed line appeared before the tail: the
	/// journal cannot be trusted and needs manual attention.
	#[error(
		"journal is corrupt: a line before the tail is malformed"
	)]
	Corrupt,

	/// The journal was written by a newer schema. Callers
	/// must land in a read-only recovery view, never in
	/// destructive defaults: this build cannot understand
	/// the recorded history.
	#[error("journal schema v{0} is newer than this build supports")]
	FutureSchema(u32),
}

/// A freshly opened journal with its recorded history.
#[derive(Debug)]
pub struct Loaded {
	/// The journal, ready to append.
	pub journal: Journal,
	/// Every committed event, in file order. A torn tail
	/// is not included.
	pub events: Vec<Event>,
	/// Whether a torn or malformed final line was dropped
	/// (and repaired away, so appending stays safe).
	pub truncated_tail: bool,
}

/// The append-only session log (see the module docs).
#[derive(Debug)]
pub struct Journal {
	path: PathBuf,
	file: fs::File,
}

impl Journal {
	/// Open (creating if needed) the journal under `dir` —
	/// creating the directory first — and load its
	/// committed events.
	///
	/// # Errors
	///
	/// * [`JournalError::Corrupt`] — a malformed line before
	///   the tail; the journal needs manual attention.
	/// * [`JournalError::FutureSchema`] — the first line
	///   announces a schema newer than this build; land in
	///   a read-only recovery view.
	/// * [`JournalError::Io`] — filesystem access failed.
	pub fn open(dir: &Path) -> Result<Loaded, JournalError> {
		fs::create_dir_all(dir)?;
		let path = dir.join(JOURNAL_FILE);
		let file = OpenOptions::new()
			.read(true)
			.append(true)
			.create(true)
			.open(&path)?;
		let loaded = load(&path)?;
		if loaded.truncated_tail {
			// Repair the torn tail away so the next
			// append starts on a clean line boundary.
			file.set_len(loaded.committed_len)?;
		}
		Ok(Loaded {
			journal: Self { path, file },
			events: loaded.events,
			truncated_tail: loaded.truncated_tail,
		})
	}

	/// Append one event: serialized to a single line,
	/// written, flushed, and synced before this returns.
	///
	/// If this fails, the event was *not* durably recorded —
	/// but a partial line may remain. Callers must treat the
	/// NEXT load as reconciliation: it drops the torn tail
	/// and reports it, so the recorded history stays exactly
	/// the history of completed appends.
	///
	/// # Errors
	///
	/// * [`JournalError::Format`] — the event cannot be
	///   serialized (nothing is written).
	/// * [`JournalError::Io`] — the write, flush, or sync
	///   failed; the next load reconciles.
	pub fn append(
		&mut self,
		event: &Event,
	) -> Result<(), JournalError> {
		let line = ron::ser::to_string(&event_to_dto(event))
			.map_err(|error| {
				JournalError::Format(error.to_string())
			})?;
		self.file.write_all(line.as_bytes())?;
		self.file.write_all(b"\n")?;
		self.file.flush()?;
		self.file.sync_data()?;
		Ok(())
	}

	/// The full path of the journal file (diagnostics).
	#[must_use]
	pub fn path(&self) -> &Path {
		&self.path
	}
}

/// What loading produced.
struct LoadResult {
	events: Vec<Event>,
	truncated_tail: bool,
	/// Byte length of the committed prefix (everything up
	/// to and including the last complete, accepted line).
	committed_len: u64,
}

/// One parsed journal line.
enum Line {
	Event(Event),
	Malformed,
}

/// Parse one journal line. `Err` is reserved for verdicts
/// (the schema gate, and a first line that is not a marker —
/// the file is not a gitui journal at all); everything
/// merely uninterpretable is `Line::Malformed` and follows
/// the crash protocol.
fn parse_line(line: &str, first: bool) -> Result<Line, JournalError> {
	let Ok(dto) = ron::de::from_str::<EventDto>(line) else {
		return Ok(Line::Malformed);
	};
	match dto {
		EventDto::Schema(version) => {
			if !first {
				return Ok(Line::Malformed);
			}
			if version > SCHEMA_VERSION {
				return Err(JournalError::FutureSchema(version));
			}
			Ok(Line::Event(Event::Schema(version)))
		}
		_ if first => {
			// The first line must be the schema marker.
			Err(JournalError::Corrupt)
		}
		dto => {
			Ok(event_from_dto(dto)
				.map_or(Line::Malformed, Line::Event))
		}
	}
}

/// Load all committed events; a torn or malformed final
/// line is dropped and flagged (see the module docs).
fn load(path: &Path) -> Result<LoadResult, JournalError> {
	let bytes = fs::read(path)?;
	let mut events = Vec::new();
	let mut committed_len = 0usize;
	let mut truncated_tail = false;

	let total = bytes.len();
	let mut first = true;
	let mut chunks =
		bytes.split_inclusive(|&byte| byte == b'\n').peekable();
	while let Some(chunk) = chunks.next() {
		let is_last = chunks.peek().is_none();
		let complete = chunk.ends_with(b"\n");
		// A final chunk without its newline is a torn
		// write: never committed, always dropped.
		if is_last && !complete {
			truncated_tail = true;
			break;
		}
		let body = &chunk[..chunk.len() - usize::from(complete)];
		let parsed = match std::str::from_utf8(body) {
			Ok(line) => parse_line(line, first)?,
			Err(_) => Line::Malformed,
		};
		match parsed {
			Line::Event(event) => {
				events.push(event);
				committed_len += chunk.len();
			}
			Line::Malformed if is_last => {
				truncated_tail = true;
				break;
			}
			Line::Malformed => {
				return Err(JournalError::Corrupt);
			}
		}
		first = false;
	}
	debug_assert!(committed_len <= total);

	Ok(LoadResult {
		events,
		truncated_tail,
		committed_len: u64::try_from(committed_len)
			.unwrap_or(u64::MAX),
	})
}

/// Serde mirror of [`Edit`]: ids as raw numbers, and
/// [`Edit::ReplaceAll`]'s document as raw bytes —
/// [`Todo::parse`] is infallible, so reconstruction of a
/// well-formed line cannot fail.
#[derive(Serialize, Deserialize)]
enum EditDto {
	SetAction {
		id: u32,
		action: crate::rebase::plan::Action,
	},
	Move {
		id: u32,
		before: Option<u32>,
	},
	InsertBreak {
		before: Option<u32>,
	},
	InsertExec {
		command: String,
		before: Option<u32>,
	},
	SetMessage {
		id: u32,
		message: String,
	},
	ReplaceAll {
		todo: Vec<u8>,
	},
}

/// Serde mirror of [`Event`]: the journal file format. The
/// domain events stay hand-rolled; this type exists only to
/// persist them — plans in [`StoredPlan`] shape, ids as raw
/// numbers (see [`EditDto`]).
#[derive(Serialize, Deserialize)]
enum EventDto {
	Schema(u32),
	Created {
		scope: Scope,
	},
	Adopted,
	PlanAdopted {
		plan: StoredPlan,
	},
	Edited {
		edit: EditDto,
		plan: StoredPlan,
	},
	Undo,
	Redo,
	Started {
		todo: Vec<u8>,
		base: Base,
		empty: EmptyPolicy,
		recovery_ref: Option<(String, CommitId)>,
		protection: Option<Protection>,
	},
	Command {
		intent: String,
	},
	Finished {
		code: i32,
		head: Option<CommitId>,
		generation: Generation,
	},
	Protected {
		stash: CommitId,
		include_untracked: bool,
	},
	Restored {
		stash: CommitId,
		clean: bool,
	},
	Completed {
		tip: CommitId,
	},
	Aborted,
	Quit,
	Closed,
}

fn edit_to_dto(edit: &Edit) -> EditDto {
	match edit {
		Edit::SetAction { id, action } => EditDto::SetAction {
			id: id.value(),
			action: *action,
		},
		Edit::Move { id, before } => EditDto::Move {
			id: id.value(),
			before: before.map(StepId::value),
		},
		Edit::InsertBreak { before } => EditDto::InsertBreak {
			before: before.map(StepId::value),
		},
		Edit::InsertExec { command, before } => EditDto::InsertExec {
			command: command.clone(),
			before: before.map(StepId::value),
		},
		Edit::SetMessage { id, message } => EditDto::SetMessage {
			id: id.value(),
			message: message.clone(),
		},
		Edit::ReplaceAll { todo } => EditDto::ReplaceAll {
			todo: todo.to_bytes(),
		},
	}
}

/// Raw stored id back to a step id; `None` for the reserved
/// zero, which makes the whole line malformed.
const fn step_id_of(raw: u32) -> Option<StepId> {
	StepId::from_value(raw)
}

fn edit_from_dto(edit: EditDto) -> Option<Edit> {
	Some(match edit {
		EditDto::SetAction { id, action } => Edit::SetAction {
			id: step_id_of(id)?,
			action,
		},
		EditDto::Move { id, before } => Edit::Move {
			id: step_id_of(id)?,
			before: before.and_then(step_id_of),
		},
		EditDto::InsertBreak { before } => Edit::InsertBreak {
			before: before.and_then(step_id_of),
		},
		EditDto::InsertExec { command, before } => Edit::InsertExec {
			command,
			before: before.and_then(step_id_of),
		},
		EditDto::SetMessage { id, message } => Edit::SetMessage {
			id: step_id_of(id)?,
			message,
		},
		EditDto::ReplaceAll { todo } => Edit::ReplaceAll {
			todo: Todo::parse(&todo),
		},
	})
}

fn event_to_dto(event: &Event) -> EventDto {
	match event {
		Event::Schema(version) => EventDto::Schema(*version),
		Event::Created { scope } => EventDto::Created {
			scope: scope.clone(),
		},
		Event::Adopted => EventDto::Adopted,
		Event::PlanAdopted { plan } => {
			EventDto::PlanAdopted { plan: plan.clone() }
		}
		Event::Edited { edit, plan } => EventDto::Edited {
			edit: edit_to_dto(edit),
			plan: plan.to_stored(),
		},
		Event::Undo => EventDto::Undo,
		Event::Redo => EventDto::Redo,
		Event::Started {
			todo,
			base,
			empty,
			recovery_ref,
			protection,
		} => EventDto::Started {
			todo: todo.clone(),
			base: base.clone(),
			empty: *empty,
			recovery_ref: recovery_ref.clone(),
			protection: protection.clone(),
		},
		Event::Command { intent } => EventDto::Command {
			intent: intent.clone(),
		},
		Event::Finished {
			code,
			head,
			generation,
		} => EventDto::Finished {
			code: *code,
			head: *head,
			generation: *generation,
		},
		Event::Protected {
			stash,
			include_untracked,
		} => EventDto::Protected {
			stash: *stash,
			include_untracked: *include_untracked,
		},
		Event::Restored { stash, clean } => EventDto::Restored {
			stash: *stash,
			clean: *clean,
		},
		Event::Completed { tip } => EventDto::Completed { tip: *tip },
		Event::Aborted => EventDto::Aborted,
		Event::Quit => EventDto::Quit,
		Event::Closed => EventDto::Closed,
	}
}

/// Back to a domain event; `None` when the data is not
/// trustworthy (a reserved-zero id, or plan ids that do not
/// match the document) — the line then follows the crash
/// protocol instead of being folded silently.
fn event_from_dto(dto: EventDto) -> Option<Event> {
	match dto {
		EventDto::Schema(_) => None,
		EventDto::Created { scope } => Some(Event::Created { scope }),
		EventDto::Adopted => Some(Event::Adopted),
		EventDto::PlanAdopted { plan } => {
			// Deliberately not validated here: a corrupt
			// plan degrades in fold (see
			// [`Session::fold`]), it does not fail the
			// line.
			Some(Event::PlanAdopted { plan })
		}
		EventDto::Edited { edit, plan } => Some(Event::Edited {
			edit: edit_from_dto(edit)?,
			plan: Plan::from_stored(&plan).ok()?,
		}),
		EventDto::Undo => Some(Event::Undo),
		EventDto::Redo => Some(Event::Redo),
		EventDto::Started {
			todo,
			base,
			empty,
			recovery_ref,
			protection,
		} => Some(Event::Started {
			todo,
			base,
			empty,
			recovery_ref,
			protection,
		}),
		EventDto::Command { intent } => {
			Some(Event::Command { intent })
		}
		EventDto::Finished {
			code,
			head,
			generation,
		} => Some(Event::Finished {
			code,
			head,
			generation,
		}),
		EventDto::Protected {
			stash,
			include_untracked,
		} => Some(Event::Protected {
			stash,
			include_untracked,
		}),
		EventDto::Restored { stash, clean } => {
			Some(Event::Restored { stash, clean })
		}
		EventDto::Completed { tip } => Some(Event::Completed { tip }),
		EventDto::Aborted => Some(Event::Aborted),
		EventDto::Quit => Some(Event::Quit),
		EventDto::Closed => Some(Event::Closed),
	}
}

#[cfg(test)]
mod tests {
	#![allow(clippy::unwrap_used, clippy::panic)]

	use super::*;
	use crate::rebase::{
		plan::{Action, Corrupt},
		todo::{CommitArg, TodoLine},
	};
	use tempfile::TempDir;

	fn oid(n: u64) -> CommitId {
		CommitId::from_str_unchecked(&format!("{n:040x}")).unwrap()
	}

	fn scope() -> Scope {
		Scope {
			branch: Some(String::from("main")),
			original_tip: oid(2),
			base: Base::Upstream(oid(1)),
		}
	}

	fn pick_line(n: u64) -> TodoLine {
		TodoLine::Instruction(
			crate::rebase::todo::Instruction::Pick {
				word: b"pick".as_slice().into(),
				commit: CommitArg::from_id(oid(n)),
				after: b" # subject".as_slice().into(),
			},
		)
	}

	fn base_plan() -> Plan {
		Plan::from_todo(Todo::parse(
			b"pick 1000000000000000000000000000000000000001 # one\n\
			  reword 2000000000000000000000000000000000000002 # two\n",
		))
	}

	fn edited(prev: &Plan, edit: &Edit) -> (Event, Plan) {
		let plan = prev.apply(edit).unwrap();
		(
			Event::Edited {
				edit: edit.clone(),
				plan: plan.clone(),
			},
			plan,
		)
	}

	fn draft_events() -> Vec<Event> {
		vec![
			Event::Schema(1),
			Event::Created { scope: scope() },
			Event::PlanAdopted {
				plan: base_plan().to_stored(),
			},
		]
	}

	fn running_events() -> Vec<Event> {
		let mut events = draft_events();
		events.push(Event::Started {
			todo: b"pick x\n".to_vec(),
			base: Base::Upstream(oid(1)),
			empty: EmptyPolicy::Stop,
			recovery_ref: Some((
				String::from("refs/gitui/recovery/x"),
				oid(2),
			)),
			protection: None,
		});
		events
	}

	fn snap(operation: Operation) -> Snapshot {
		Snapshot {
			generation: Generation(0),
			head: Some(oid(3)),
			branch: Some(String::from("main")),
			operation,
			staged: Vec::new(),
			worktree: Vec::new(),
			conflicts: Vec::new(),
			untracked: Vec::new(),
			rebase: None,
			bridge: None,
		}
	}

	#[test]
	fn round_trip_preserves_the_folded_state() {
		let dir = TempDir::new().unwrap();
		let mut journal = Journal::open(dir.path()).unwrap();
		assert!(journal.journal.path().exists());
		assert!(journal.events.is_empty());
		assert!(!journal.truncated_tail);

		let base = base_plan();
		let reword_id = base.steps()[1].id;
		let (e1, p1) = edited(
			&base,
			&Edit::SetAction {
				id: reword_id,
				action: Action::Squash,
			},
		);
		let (e2, p2) =
			edited(&p1, &Edit::InsertBreak { before: None });
		let (e3, _p3) = edited(
			&p2,
			&Edit::SetMessage {
				id: reword_id,
				message: String::from("melded"),
			},
		);

		let history = vec![
			Event::Schema(1),
			Event::Created { scope: scope() },
			Event::PlanAdopted {
				plan: base.to_stored(),
			},
			e1,
			e2,
			e3,
			Event::Started {
				todo: b"pick x\n".to_vec(),
				base: Base::Upstream(oid(1)),
				empty: EmptyPolicy::Keep,
				recovery_ref: None,
				protection: None,
			},
			Event::Command {
				intent: String::from("rebase --continue"),
			},
			Event::Finished {
				code: 0,
				head: Some(oid(8)),
				generation: Generation(4),
			},
			Event::Protected {
				stash: oid(5),
				include_untracked: true,
			},
			Event::Restored {
				stash: oid(5),
				clean: true,
			},
			Event::Completed { tip: oid(9) },
		];

		for event in &history {
			journal.journal.append(event).unwrap();
		}
		drop(journal);

		let loaded = Journal::open(dir.path()).unwrap();
		assert!(!loaded.truncated_tail);
		assert_eq!(loaded.events, history);
		assert_eq!(
			Session::fold(&loaded.events),
			Session::fold(&history)
		);
	}

	#[test]
	fn torn_final_line_is_dropped_and_repaired() {
		let dir = TempDir::new().unwrap();
		{
			let mut journal = Journal::open(dir.path()).unwrap();
			journal.journal.append(&Event::Schema(1)).unwrap();
			journal
				.journal
				.append(&Event::Created { scope: scope() })
				.unwrap();
		}
		// Simulate a crash mid-append: a half-written line
		// without its terminating newline.
		let mut raw = OpenOptions::new()
			.append(true)
			.open(dir.path().join(JOURNAL_FILE))
			.unwrap();
		raw.write_all(b"(Schema:3").unwrap();
		drop(raw);

		let mut loaded = Journal::open(dir.path()).unwrap();
		assert!(loaded.truncated_tail);
		assert_eq!(
			loaded.events,
			vec![Event::Schema(1), Event::Created { scope: scope() },]
		);

		// The tail was repaired: appending works cleanly.
		loaded.journal.append(&Event::Aborted).unwrap();
		drop(loaded.journal);
		let again = Journal::open(dir.path()).unwrap();
		assert!(!again.truncated_tail);
		assert_eq!(
			again.events,
			vec![
				Event::Schema(1),
				Event::Created { scope: scope() },
				Event::Aborted,
			]
		);
	}

	#[test]
	fn malformed_final_line_is_dropped_and_flagged() {
		let dir = TempDir::new().unwrap();
		{
			let mut journal = Journal::open(dir.path()).unwrap();
			journal.journal.append(&Event::Schema(1)).unwrap();
			journal.journal.append(&Event::Aborted).unwrap();
		}
		let mut raw = OpenOptions::new()
			.append(true)
			.open(dir.path().join(JOURNAL_FILE))
			.unwrap();
		// Complete (newline terminated) but uninterpretable.
		raw.write_all(b"not a journal line\n").unwrap();
		drop(raw);

		let loaded = Journal::open(dir.path()).unwrap();
		assert!(loaded.truncated_tail);
		assert_eq!(
			loaded.events,
			vec![Event::Schema(1), Event::Aborted]
		);
	}

	#[test]
	fn mid_file_corruption_is_a_hard_error() {
		let dir = TempDir::new().unwrap();
		{
			let mut journal = Journal::open(dir.path()).unwrap();
			journal.journal.append(&Event::Schema(1)).unwrap();
			journal.journal.append(&Event::Aborted).unwrap();
		}
		let mut raw = OpenOptions::new()
			.append(true)
			.open(dir.path().join(JOURNAL_FILE))
			.unwrap();
		// Corrupt in the middle, then a well-formed line
		// after it: the damage is genuinely mid-file.
		raw.write_all(b"garbage\n").unwrap();
		let redo = ron::ser::to_string(&EventDto::Quit).unwrap();
		raw.write_all(redo.as_bytes()).unwrap();
		raw.write_all(b"\n").unwrap();
		drop(raw);

		assert!(matches!(
			Journal::open(dir.path()),
			Err(JournalError::Corrupt)
		));
	}

	#[test]
	fn future_schema_is_rejected_for_read_only_recovery() {
		let dir = TempDir::new().unwrap();
		let mut journal = Journal::open(dir.path()).unwrap();
		// Appending a future marker is allowed (it is just
		// data); loading refuses to interpret it.
		journal.journal.append(&Event::Schema(2)).unwrap();
		drop(journal);
		assert!(matches!(
			Journal::open(dir.path()),
			Err(JournalError::FutureSchema(2))
		));
	}

	#[test]
	fn missing_schema_marker_is_corrupt() {
		let dir = TempDir::new().unwrap();
		let mut journal = Journal::open(dir.path()).unwrap();
		journal
			.journal
			.append(&Event::Created { scope: scope() })
			.unwrap();
		drop(journal);
		assert!(matches!(
			Journal::open(dir.path()),
			Err(JournalError::Corrupt)
		));
	}

	#[test]
	fn undo_redo_moves_the_plan_stack() {
		let base = base_plan();
		let reword_id = base.steps()[1].id;
		let (e1, p1) = edited(
			&base,
			&Edit::SetAction {
				id: reword_id,
				action: Action::Squash,
			},
		);
		let (e2, p2) =
			edited(&p1, &Edit::InsertBreak { before: None });
		let (e3, p3) = edited(
			&p2,
			&Edit::SetMessage {
				id: reword_id,
				message: String::from("melded"),
			},
		);
		// A branching edit after two undos.
		let (e4, p4) = edited(
			&p2,
			&Edit::InsertExec {
				command: String::from("make"),
				before: None,
			},
		);

		let mut events = draft_events();
		// Undo with only the adopted plan: a no-op.
		events.push(Event::Undo);
		let session = Session::fold(&events);
		assert_eq!(session.plan.as_ref(), Some(&base));
		assert!(session.edits.is_empty());
		assert_eq!(session.redoable, 0);
		assert!(!session.can_undo());
		assert!(!session.can_redo());

		events.push(e1);
		let session = Session::fold(&events);
		assert_eq!(session.plan.as_ref(), Some(&p1));
		assert_eq!(session.edits.len(), 1);
		assert!(session.can_undo());
		assert!(!session.can_redo());

		events.push(e2);
		let session = Session::fold(&events);
		assert_eq!(session.plan.as_ref(), Some(&p2));
		assert_eq!(session.edits.len(), 2);

		events.push(e3);
		let session = Session::fold(&events);
		assert_eq!(session.plan.as_ref(), Some(&p3));
		assert_eq!(session.edits.len(), 3);
		assert_eq!(session.redoable, 0);

		events.push(Event::Undo);
		let session = Session::fold(&events);
		assert_eq!(session.plan.as_ref(), Some(&p2));
		assert_eq!(session.edits.len(), 2);
		assert_eq!(session.redoable, 1);
		assert!(session.can_redo());

		events.push(Event::Undo);
		let session = Session::fold(&events);
		assert_eq!(session.plan.as_ref(), Some(&p1));
		assert_eq!(session.edits.len(), 1);
		assert_eq!(session.redoable, 2);

		events.push(Event::Redo);
		let session = Session::fold(&events);
		assert_eq!(session.plan.as_ref(), Some(&p2));
		assert_eq!(session.edits.len(), 2);
		assert_eq!(session.redoable, 1);

		// Redo until empty, then once more: a no-op.
		events.push(Event::Redo);
		events.push(Event::Redo);
		let session = Session::fold(&events);
		assert_eq!(session.plan.as_ref(), Some(&p3));
		assert_eq!(session.edits.len(), 3);
		assert_eq!(session.redoable, 0);

		// The branching edit lands on the un-undone state
		// and clears the redo history.
		events.push(e4);
		let session = Session::fold(&events);
		assert_eq!(session.plan.as_ref(), Some(&p4));
		assert_eq!(session.edits.len(), 4);
		assert_eq!(session.redoable, 0);
		assert!(!session.can_redo());
	}

	#[test]
	fn corrupt_stored_plan_degrades_never_panics() {
		// from_stored rejects both corruption shapes
		// directly.
		let todo = Todo::from_lines(vec![pick_line(1)]);
		let mismatched = StoredPlan {
			todo: todo.to_bytes(),
			ids: vec![1, 2],
			notes: Vec::new(),
		};
		assert_eq!(
			Plan::from_stored(&mismatched),
			Err(Corrupt::IdCount { lines: 1, ids: 2 })
		);
		let zeroed = StoredPlan {
			todo: todo.to_bytes(),
			ids: vec![0],
			notes: Vec::new(),
		};
		assert_eq!(Plan::from_stored(&zeroed), Err(Corrupt::ZeroId));

		// Hand-mangled journals can carry such data; fold
		// must degrade into recovery, not panic.
		let session = Session::fold(&[
			Event::Schema(1),
			Event::Created { scope: scope() },
			Event::PlanAdopted { plan: mismatched },
		]);
		assert_eq!(session.phase, Phase::Draft);
		assert_eq!(session.plan, None);
		assert_eq!(
			session.uncertain_command.as_deref(),
			Some("corrupt plan in journal")
		);
	}

	#[test]
	fn fold_records_lifecycle_facts() {
		let mut events = running_events();
		events.push(Event::Protected {
			stash: oid(6),
			include_untracked: true,
		});
		events.push(Event::Command {
			intent: String::from("rebase --continue"),
		});
		let uncertain = Session::fold(&events);
		assert_eq!(uncertain.phase, Phase::Running);
		assert_eq!(
			uncertain.uncertain_command.as_deref(),
			Some("rebase --continue")
		);

		events.push(Event::Finished {
			code: 0,
			head: Some(oid(8)),
			generation: Generation(2),
		});
		events.push(Event::Restored {
			stash: oid(6),
			clean: true,
		});
		events.push(Event::Completed { tip: oid(8) });
		let done = Session::fold(&events);
		assert_eq!(done.phase, Phase::Finished(oid(8)));
		assert_eq!(done.outcome, Some(Outcome::Completed(oid(8))));
		assert_eq!(done.uncertain_command, None);
		assert_eq!(
			done.protection,
			Some(Protection {
				stash: oid(6),
				include_untracked: true,
			})
		);
		assert_eq!(done.scope.as_ref(), Some(&scope()));
		let started = done.started.as_ref().unwrap();
		assert_eq!(started.empty, EmptyPolicy::Stop);
		assert!(started.recovery_ref.is_some());
		assert!(!done.can_undo(), "running: no undo");
		assert!(done.edits.is_empty());

		// Terminal and closed phases.
		assert_eq!(
			Session::fold(&[
				Event::Schema(1),
				Event::Created { scope: scope() },
				Event::Aborted,
			])
			.phase,
			Phase::Aborted
		);
		assert_eq!(
			Session::fold(&[
				Event::Schema(1),
				Event::Created { scope: scope() },
				Event::Quit,
			])
			.phase,
			Phase::Quit
		);
		assert_eq!(
			Session::fold(&[
				Event::Schema(1),
				Event::Created { scope: scope() },
				Event::Closed,
			])
			.phase,
			Phase::Closed
		);
		assert!(Session::fold(&[Event::Adopted]).adopted);
	}

	#[test]
	fn reconcile_classifies_every_combination() {
		let clean = snap(Operation::Clean);
		let rebase = snap(Operation::RebaseInteractive);
		let merge = snap(Operation::Merge);

		// No session: like a draft — consistent on a clean
		// repository, diverged against a foreign rebase.
		assert_eq!(
			reconcile(&Session::default(), &clean),
			Reconciliation::Consistent
		);
		assert!(matches!(
			reconcile(&Session::default(), &rebase),
			Reconciliation::Diverged { .. }
		));

		// Draft: consistent clean, diverged against a
		// rebase (external, or a crashed start) and
		// against other operations.
		let draft = Session::fold(&draft_events());
		assert_eq!(
			reconcile(&draft, &clean),
			Reconciliation::Consistent
		);
		assert!(matches!(
			reconcile(&draft, &rebase),
			Reconciliation::Diverged {
				journal: Phase::Draft,
				..
			}
		));
		assert!(matches!(
			reconcile(&draft, &merge),
			Reconciliation::Diverged { .. }
		));

		// Running: consistent while a rebase is observed;
		// a clean repository names both sides honestly.
		let running = Session::fold(&running_events());
		assert_eq!(
			reconcile(&running, &rebase),
			Reconciliation::Consistent
		);
		let ended = reconcile(&running, &clean);
		assert!(matches!(
			&ended,
			Reconciliation::Diverged {
				journal: Phase::Running,
				observed: Operation::Clean,
				..
			}
		));
		assert_eq!(
			ended.divergence_reason().as_deref(),
			Some("the rebase ended outside gitui")
		);
		assert!(matches!(
			reconcile(&running, &merge),
			Reconciliation::Diverged { .. }
		));

		// Uncertainty outranks everything: a command
		// without its finish wins regardless of state.
		let mut events = running_events();
		events.push(Event::Command {
			intent: String::from("rebase --continue"),
		});
		let uncertain = Session::fold(&events);
		for snapshot in [&clean, &rebase, &merge] {
			assert!(matches!(
				reconcile(&uncertain, snapshot),
				Reconciliation::UncertainOutcome {
					ref intent,
				} if intent == "rebase --continue"
			));
		}

		// Terminal sessions outlive repository state.
		let mut events = running_events();
		events.push(Event::Completed { tip: oid(8) });
		let finished = Session::fold(&events);
		for snapshot in [&clean, &rebase] {
			assert_eq!(
				reconcile(&finished, snapshot),
				Reconciliation::Consistent
			);
		}
		for events in [
			vec![
				Event::Schema(1),
				Event::Created { scope: scope() },
				Event::Aborted,
			],
			vec![
				Event::Schema(1),
				Event::Created { scope: scope() },
				Event::Quit,
			],
			vec![
				Event::Schema(1),
				Event::Created { scope: scope() },
				Event::Closed,
			],
		] {
			let terminal = Session::fold(&events);
			assert_eq!(
				reconcile(&terminal, &rebase),
				Reconciliation::Consistent
			);
		}
	}

	/// Seeded generator for the property test.
	struct Rng(u64);

	impl Rng {
		fn next(&mut self) -> u64 {
			self.0 = self
				.0
				.wrapping_mul(6_364_136_223_846_793_005)
				.wrapping_add(1_442_695_040_888_963_407);
			self.0 >> 11
		}

		fn below(&mut self, bound: usize) -> usize {
			(self.next() % bound.max(1) as u64) as usize
		}
	}

	fn random_plan(rng: &mut Rng) -> Plan {
		let count = 1 + rng.below(4);
		let lines =
			(0..count).map(|i| pick_line(i as u64 + 1)).collect();
		Plan::from_todo(Todo::from_lines(lines))
	}

	/// One random edit against a plan (some stale on
	/// purpose; rejected edits are skipped like the UI
	/// would).
	fn random_edit(plan: &Plan, rng: &mut Rng) -> Edit {
		let live = plan
			.steps()
			.get(rng.below(plan.steps().len().max(1)))
			.map_or(StepId::FIRST, |step| step.id);
		match rng.below(5) {
			0 => Edit::SetAction {
				id: live,
				action: Action::Pick,
			},
			1 => Edit::InsertBreak { before: Some(live) },
			2 => Edit::SetMessage {
				id: live,
				message: format!("m{}", rng.below(50)),
			},
			3 => Edit::Move {
				id: live,
				before: Some(live),
			},
			_ => Edit::InsertExec {
				command: format!("cmd {}", rng.below(50)),
				before: None,
			},
		}
	}

	#[test]
	fn random_histories_survive_undo_and_redo() {
		let mut rng = Rng(0x5EED_0000_CAFE_0001);
		for _case in 0..40 {
			let mut plan = random_plan(&mut rng);
			let mut events = vec![
				Event::Schema(1),
				Event::Created { scope: scope() },
				Event::PlanAdopted {
					plan: plan.to_stored(),
				},
			];
			// Shadow of the fold: the plan stack tip and
			// the redo stack depth.
			let mut history = vec![plan.clone()];
			let mut undone: Vec<Plan> = Vec::new();

			for _step in 0..40 {
				match rng.below(4) {
					0 | 1 => {
						let edit = random_edit(&plan, &mut rng);
						if let Ok(next) = plan.apply(&edit) {
							events.push(Event::Edited {
								edit,
								plan: next.clone(),
							});
							history.push(next.clone());
							undone.clear();
						}
					}
					2 => {
						events.push(Event::Undo);
						if history.len() > 1 {
							if let Some(popped) = history.pop() {
								undone.push(popped);
							}
						}
					}
					_ => {
						events.push(Event::Redo);
						if let Some(restored) = undone.pop() {
							history.push(restored);
						}
					}
				}
				let session = Session::fold(&events);
				let tip = history.last().unwrap().clone();
				plan = tip.clone();
				assert_eq!(session.plan, Some(tip));
				assert_eq!(session.edits.len(), history.len() - 1);
				assert_eq!(session.redoable, undone.len());
				assert_eq!(
					session.can_undo(),
					history.len() > 1
						&& session.phase == Phase::Draft
				);
				assert_eq!(session.can_redo(), !undone.is_empty());
			}

			// Undo/redo transparency: unwinding the whole
			// stack and replaying exactly those redos
			// lands back on the same plan top, with the
			// untouched redo history preserved.
			let depth = history.len() - 1;
			let mut replay = events.clone();
			replay.extend(std::iter::repeat_n(Event::Undo, depth));
			replay.extend(std::iter::repeat_n(Event::Redo, depth));
			let session = Session::fold(&replay);
			assert_eq!(session.plan, Some(plan));
			assert_eq!(session.redoable, undone.len());
			assert_eq!(session.can_redo(), !undone.is_empty());
		}
	}
}
