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
//!
//! ## Stop classification (evidence, git 2.55.0)
//!
//! Every rule below was confirmed against real `rebase-merge/`
//! directories produced by fixture repositories (see the
//! evidence tests at the bottom of this file — they are the
//! contract with git):
//!
//! - *conflict stop*: `stopped-sha` present, unmerged entries in
//!   status, `message` carries git's `# Conflicts:` section.
//! - *resolved conflict*: the same files, but the unmerged
//!   entries are gone and the resolutions are staged. The staged
//!   side is what distinguishes it from an empty stop.
//! - *edit stop*: the `amend` file names the commit being
//!   amended (`stopped-sha` is present too, so `amend` is
//!   checked first).
//! - *break*: `break` has been appended to `done`; no
//!   `stopped-sha`, no `amend`.
//! - *exec failure*: the failing `exec` is the **last line of
//!   `done`**; the remaining program stays in the todo; no
//!   `stopped-sha`. This holds with and without
//!   `--reschedule-failed-exec` — the flag changes what
//!   `--continue` does, not the stopped state — so the flag is
//!   not needed for classification. The exit code is not
//!   recorded anywhere; [`Stop::ExecFailed`] therefore always
//!   carries `code: None`.
//! - *empty stop* (`--empty=stop`): `stopped-sha` present, no
//!   conflicts, nothing staged; `done` ends with the pick. This
//!   is what [`Stop::BecameEmpty`] is derived from.
//! - *reword/squash stop* (external editor): gitui routes
//!   message composition through its bridge; if the bridge
//!   request is missing but `done` ends with `reword`/`squash`
//!   and `stopped-sha` is present, the state is foreign —
//!   [`Stop::Unknown`], never a guess.
//! - *completed*: the whole `rebase-merge/` directory is gone.
//!
//! ## Phases not derived here
//!
//! [`Phase::Executing`], [`Phase::Restoring`] and
//! [`Phase::Review`] are never produced by [`policy`]: they are
//! in-flight or presentation states the UI layer merges on top
//! of this projection (a running engine command, a stash restore
//! in progress, the user opening the review tab of a valid
//! draft).

use crate::rebase::{
	journal::{self, Session},
	plan::{Diagnostic, Issue, Plan},
	snapshot::{BridgeKind, Operation, RebaseState, Snapshot},
	todo::{CommitArg, Instruction, TodoLine},
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
	/// Reviewing before start. Never derived by [`policy`] —
	/// the UI enters review from a valid draft on its own.
	Review,
	/// A git process is currently executing. Never derived by
	/// [`policy`] — the UI merges its own in-flight flag.
	Executing,
	/// Stopped, waiting for the user.
	Stopped(Stop),
	/// Rebase done, local changes still to restore. Never
	/// derived by [`policy`] — the UI merges its in-flight
	/// restore flag; the pending restore itself is reported by
	/// the `restore` action under [`Phase::Done`].
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
	/// An empty-commit decision (keep/skip) must come first;
	/// `--continue` alone would just re-stop. Additive variant
	/// for [`Stop::BecameEmpty`].
	EmptyDecision,
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
	let phase = derive_phase(session, snapshot);
	// The stop classification threads into rows, inspector and
	// actions; `Adopted` carries it outside the phase value.
	let stop = match &phase {
		Phase::Stopped(stop) => Some(stop.clone()),
		Phase::Adopted { .. } => classify_stop(snapshot),
		_ => None,
	};
	let (rail, aligned) =
		rail_of(session, snapshot, &phase, stop.as_ref());
	let mut inspector = inspector_of(&phase);
	if !aligned {
		// The plan and the observation disagree (an external
		// edit-todo, most likely): the rail is a best-effort
		// rendering and the inspector explains the mismatch
		// instead of pretending to understand the stop.
		let observing = matches!(
			phase,
			Phase::Stopped(_) | Phase::Adopted { .. }
		);
		if observing {
			inspector = Inspector::Reconcile;
		}
	}
	let actions =
		actions_of(session, snapshot, &phase, stop.as_ref());
	let progress = progress_of(session, snapshot, &phase);
	Policy {
		phase,
		rail,
		inspector,
		actions,
		progress,
	}
}

/// Phase derivation: the journal describes intent, the
/// snapshot describes the world; the phase is where they are
/// compared.
fn derive_phase(session: &Session, snapshot: &Snapshot) -> Phase {
	// The journal's terminal outcomes win over observation: a
	// session that recorded Finished/Aborted/Quit is done even
	// if sequencer files linger (they are cleaned up by git a
	// moment later).
	match session.phase {
		journal::Phase::Finished(_) => {
			return Phase::Done(Done::Finished)
		}
		journal::Phase::Aborted => return Phase::Done(Done::Aborted),
		journal::Phase::Quit => return Phase::Done(Done::Quit),
		_ => {}
	}

	if session.adopted {
		return if session_has_rebase(snapshot) {
			// Observation of the external session; the stop
			// classification rides along for the inspector.
			Phase::Adopted { managed: false }
		} else {
			Phase::Reconciling {
				reason: String::from(
					"the adopted rebase is no longer in \
					 progress",
				),
			}
		};
	}

	match session.phase {
		journal::Phase::Running => match snapshot.operation {
			Operation::RebaseInteractive
			| Operation::Rebase
			| Operation::RebaseApply => {
				classify_stop(snapshot).map_or_else(
					// Either the rebase ended under us or
					// the sequencer state is unreadable;
					// both are divergences.
					|| Phase::Reconciling {
						reason: String::from(
							"the rebase ended outside \
							 gitui",
						),
					},
					Phase::Stopped,
				)
			}
			// Named by the journal's reconciliation
			// verdict (Running + Clean diverges; an
			// uncertain command outcome is not a
			// divergence and keeps this wording).
			Operation::Clean => Phase::Reconciling {
				reason: journal::reconcile(session, snapshot)
					.divergence_reason()
					.unwrap_or_else(|| {
						String::from(
							"the rebase ended outside \
							 gitui",
						)
					}),
			},
			other => Phase::Reconciling {
				reason: format!(
					"the repository moved to another operation \
					 ({other:?}) during the rebase"
				),
			},
		},
		// A live session but the repository moved underneath
		// it (started an external merge/rebase while drafting).
		journal::Phase::Draft => {
			if session_has_rebase(snapshot) {
				Phase::Reconciling {
					reason: String::from(
						"a rebase is running that this \
						 session did not start",
					),
				}
			} else if snapshot.operation == Operation::Clean {
				Phase::Draft
			} else {
				Phase::Reconciling {
					reason: format!(
						"the repository moved to another \
						 operation ({:?}) while drafting",
						snapshot.operation
					),
				}
			}
		}
		// No live session (never created, or closed): a
		// foreign operation is simply unsupported; a rebase
		// without a session is offered for adoption upstream,
		// not reconciled here.
		_ => {
			if session_has_rebase(snapshot) {
				Phase::Reconciling {
					reason: String::from(
						"a rebase is in progress without \
						 a gitui session",
					),
				}
			} else if snapshot.operation == Operation::Clean {
				Phase::Draft
			} else {
				Phase::Unsupported {
					operation: snapshot.operation,
				}
			}
		}
	}
}

/// Whether the repository is inside any rebase flavor right now.
const fn session_has_rebase(snapshot: &Snapshot) -> bool {
	snapshot.operation.is_rebase() || snapshot.rebase.is_some()
}

/// Classify why the observed rebase is stopped. Evidence-only:
/// every rule was verified against real git (see the module
/// docs and the evidence tests).
fn classify_stop(snapshot: &Snapshot) -> Option<Stop> {
	let rebase = snapshot.rebase.as_ref()?;
	let conflicts: Vec<String> =
		snapshot.conflicts.iter().map(|c| c.path.clone()).collect();

	// Conflicts outrank everything: even an `edit` replay can
	// conflict before the amend state is reached.
	if !conflicts.is_empty() {
		return Some(Stop::Conflicts { paths: conflicts });
	}

	// The bridge request is gitui's own knowledge about a
	// message composition in flight (reword/squash).
	if let Some(request) = &snapshot.bridge {
		if request.kind == BridgeKind::Message {
			return Some(Stop::Message {
				request: request.file.clone(),
			});
		}
	}

	// The `amend` file is git's own marker for an edit stop
	// (evidence: `stopped-sha` is present there too, so the
	// amend check must come first).
	if let Some(commit) = rebase.amend {
		return Some(Stop::Edit { commit });
	}

	let done_tail = rebase.done.instructions().last().map(|(_, i)| i);

	if let Some(commit) = rebase.stopped_sha {
		// Resolutions staged, unmerged entries gone: a
		// conflict that has been resolved and is waiting for
		// `--continue`. Distinguished from an empty stop by
		// the staged side (an empty stop stages nothing).
		if !snapshot.staged.is_empty() {
			return Some(Stop::Conflicts { paths: Vec::new() });
		}
		return Some(match done_tail {
			// A message-editor stop without the bridge is a
			// foreign state; do not mistake it for empty.
			Some(
				Instruction::Reword { .. }
				| Instruction::Squash { .. },
			) => Stop::Unknown {
				note: String::from(
					"stopped at a message editor outside the \
					 gitui bridge",
				),
			},
			_ => Stop::BecameEmpty { commit },
		});
	}

	// No stopped-sha: the stop is about a non-pick step.
	match done_tail {
		Some(Instruction::Break { .. }) => Some(Stop::Break),
		Some(Instruction::Exec { command, .. }) => {
			Some(Stop::ExecFailed {
				command: String::from_utf8_lossy(command)
					.into_owned(),
				// git records no exit status for exec
				// steps (verified against 2.55).
				code: None,
			})
		}
		_ => Some(Stop::Unknown {
			note: format!(
				"stopped without a recognized marker \
				 (no stopped-sha, amend or conflicts; done \
				 ends with {})",
				done_tail.map_or_else(
					|| String::from("nothing"),
					|i| String::from(i.canonical_word())
				)
			),
		}),
	}
}

/// The rail: rows from the plan where the session owns one, or
/// from the observation where it does not. The second value
/// reports whether the plan/observation alignment held.
fn rail_of(
	session: &Session,
	snapshot: &Snapshot,
	phase: &Phase,
	stop: Option<&Stop>,
) -> (Vec<Row>, bool) {
	let rebase = snapshot.rebase.as_ref();
	match phase {
		Phase::Done(_) => {
			(terminal_rows(session.plan.as_ref()), true)
		}
		// Between stops with a live program: align plan and
		// observation; misalignment falls back to the raw
		// observation.
		Phase::Stopped(_) | Phase::Adopted { .. }
			if session.started.is_some() && rebase.is_some() =>
		{
			match (rebase, session.plan.as_ref()) {
				(Some(observed), Some(plan)) => {
					aligned_rows(plan, observed, stop).map_or_else(
						|| (observation_rows(observed), false),
						|rows| (rows, true),
					)
				}
				// Adopted sessions have no plan; the
				// observation is all we can render.
				(Some(observed), None) => {
					(observation_rows(observed), true)
				}
				(None, _) => (Vec::new(), true),
			}
		}
		// Draft-like views: a plan that has not run renders
		// its program.
		Phase::Draft
		| Phase::Review
		| Phase::Unsupported { .. }
		| Phase::Executing
		| Phase::Restoring => (draft_rows(session.plan.as_ref()), true),
		// Reconciling: a draft that never started still
		// renders as a draft; once started, nothing is
		// claimed about states we cannot observe.
		Phase::Reconciling { .. } => {
			if session.started.is_none() {
				(draft_rows(session.plan.as_ref()), true)
			} else {
				(Vec::new(), true)
			}
		}
		// Observed phases without an observation to align
		// (guard above failed): the plan's states are not
		// knowable, so nothing is claimed.
		Phase::Stopped(_) | Phase::Adopted { .. } => {
			(Vec::new(), true)
		}
	}
}

/// Rows for a plan that has not run: every instruction and raw
/// line is a row (comments and blanks are not rows), pending
/// except drops and unknown syntax.
fn draft_rows(plan: Option<&Plan>) -> Vec<Row> {
	let Some(plan) = plan else {
		return Vec::new();
	};
	plan.steps()
		.iter()
		.filter_map(|step| match &step.line {
			TodoLine::Instruction(instruction) => Some(Row {
				step: Some(step.id),
				state: if matches!(
					instruction,
					Instruction::Drop { .. }
				) {
					RowState::Dropped
				} else {
					RowState::Pending
				},
				label: label_of(instruction),
				short_oid: short_of_instruction(instruction),
				folds_into_previous: folds(instruction),
			}),
			// Raw lines are rows too: visible and failed,
			// they are why the plan cannot start.
			TodoLine::Raw(bytes) => Some(Row {
				step: Some(step.id),
				state: RowState::Failed,
				label: String::from_utf8_lossy(bytes).into_owned(),
				short_oid: None,
				folds_into_previous: false,
			}),
			TodoLine::Comment(_) | TodoLine::Blank => None,
		})
		.collect()
}

/// Rows for a finished program: everything applied or dropped.
fn terminal_rows(plan: Option<&Plan>) -> Vec<Row> {
	let Some(plan) = plan else {
		return Vec::new();
	};
	plan.steps()
		.iter()
		.filter_map(|step| match &step.line {
			TodoLine::Instruction(instruction) => Some(Row {
				step: Some(step.id),
				state: if matches!(
					instruction,
					Instruction::Drop { .. }
				) {
					RowState::Dropped
				} else {
					RowState::Done
				},
				label: label_of(instruction),
				short_oid: short_of_instruction(instruction),
				folds_into_previous: folds(instruction),
			}),
			_ => None,
		})
		.collect()
}

/// Whether the observation agrees with the plan, and the rows
/// that result. Matching is positional over instructions only:
/// pick-family steps match by commit argument (git may
/// abbreviate), everything else by canonical word plus
/// arguments. Instructions git inserts on its own
/// (`noop`, `update-ref` for `--update-refs`) do not consume
/// plan steps.
fn aligned_rows(
	plan: &Plan,
	rebase: &RebaseState,
	stop: Option<&Stop>,
) -> Option<Vec<Row>> {
	let steps: Vec<(StepId, &Instruction)> = plan
		.steps()
		.iter()
		.filter_map(|step| {
			step.line
				.instruction()
				.map(|instruction| (step.id, instruction))
		})
		.collect();

	let mut obs: Vec<(&Instruction, bool)> =
		rebase.done.instructions().map(|(_, i)| (i, true)).collect();
	obs.extend(rebase.todo.instructions().map(|(_, i)| (i, false)));

	// Map each plan step to its observation position, in
	// order.
	let mut match_of_step: Vec<Option<usize>> =
		vec![None; steps.len()];
	let mut next = 0usize;
	for (index, (instruction, _)) in obs.iter().enumerate() {
		if next < steps.len()
			&& same_instruction(steps[next].1, instruction)
		{
			match_of_step[next] = Some(index);
			next += 1;
		}
		// else: a git-internal instruction; skip it.
	}
	if next != steps.len() {
		// The plan has steps the observation never accounted
		// for: the program was edited externally.
		return None;
	}

	// The boundary is the instruction git stopped on: the
	// last executed one, or the head of the todo when nothing
	// ran yet (a `break` as the very first step). Then map it
	// back to the plan step it belongs to.
	let boundary_obs = if rebase.done.instructions().count() > 0 {
		rebase.done.instructions().count() - 1
	} else if !obs.is_empty() {
		0
	} else {
		return Some(Vec::new());
	};
	let boundary_plan =
		match_of_step.iter().position(|m| *m == Some(boundary_obs));

	let rows = steps
		.iter()
		.enumerate()
		.map(|(index, (id, instruction))| {
			let in_done = match_of_step[index]
				.is_some_and(|obs_index| obs[obs_index].1);
			let state = if !in_done {
				RowState::Pending
			} else if boundary_plan == Some(index) {
				boundary_row_state(stop)
			} else {
				match instruction {
					Instruction::Drop { .. } => RowState::Dropped,
					_ => RowState::Done,
				}
			};
			// Executed commits show their rewritten output
			// oid when git recorded one; the source
			// argument otherwise (display only).
			let short_oid = if in_done {
				rewritten_short(rebase, instruction)
					.or_else(|| short_of_instruction(instruction))
			} else {
				short_of_instruction(instruction)
			};
			Row {
				step: Some(*id),
				state,
				label: label_of(instruction),
				short_oid,
				folds_into_previous: folds(instruction),
			}
		})
		.collect();
	Some(rows)
}

/// State of the boundary row, per the classified stop:
/// conflict/exec failed; edit/break/empty are deliberate
/// stops; message and unknown stay neutral.
const fn boundary_row_state(stop: Option<&Stop>) -> RowState {
	match stop {
		Some(Stop::Conflicts { .. } | Stop::ExecFailed { .. }) => {
			RowState::Failed
		}
		Some(
			Stop::Edit { .. }
			| Stop::Break
			| Stop::BecameEmpty { .. },
		) => RowState::Stop,
		Some(Stop::Message { .. } | Stop::Unknown { .. }) => {
			RowState::Current
		}
		None => RowState::Current,
	}
}

/// Short oid of a rewritten output for an executed step, when
/// git recorded the mapping (observed fact).
fn rewritten_short(
	rebase: &RebaseState,
	instruction: &Instruction,
) -> Option<String> {
	let full = instruction.commit();
	rebase
		.rewritten
		.iter()
		.find(|(old, _)| Some(*old) == full)
		.map(|(_, new)| new.get_short_string())
}

/// Rows rendered purely from the observation (adopted
/// sessions, or a plan the observation no longer matches).
/// States are only Pending/Done/Dropped here: without an
/// alignment there is no honest finer classification.
fn observation_rows(rebase: &RebaseState) -> Vec<Row> {
	let mut rows = Vec::new();
	for (_, instruction) in rebase.done.instructions() {
		rows.push(observation_row(instruction, true));
	}
	for (_, instruction) in rebase.todo.instructions() {
		rows.push(observation_row(instruction, false));
	}
	rows
}

fn observation_row(instruction: &Instruction, in_done: bool) -> Row {
	let state = if in_done {
		match instruction {
			Instruction::Drop { .. } => RowState::Dropped,
			_ => RowState::Done,
		}
	} else {
		RowState::Pending
	};
	Row {
		step: None,
		state,
		label: label_of(instruction),
		short_oid: short_of_instruction(instruction),
		folds_into_previous: folds(instruction),
	}
}

/// Whether this instruction folds into the previous row's
/// output group.
const fn folds(instruction: &Instruction) -> bool {
	matches!(
		instruction,
		Instruction::Squash { .. } | Instruction::Fixup { .. }
	)
}

/// Display label: the commit subject comment when git wrote
/// one, the argument/command otherwise.
fn label_of(instruction: &Instruction) -> String {
	if let Some(comment) = instruction.comment() {
		let text =
			comment.strip_prefix(b"# ".as_slice()).unwrap_or(comment);
		if !text.is_empty() {
			return String::from_utf8_lossy(text).into_owned();
		}
	}
	match instruction {
		Instruction::Exec { command, .. } => {
			String::from_utf8_lossy(command).into_owned()
		}
		Instruction::Break { .. } => String::from("break"),
		Instruction::Noop { .. } => String::from("noop"),
		Instruction::Label { label, .. }
		| Instruction::Reset { label, .. } => {
			String::from_utf8_lossy(label).into_owned()
		}
		Instruction::UpdateRef { reference, .. } => {
			String::from_utf8_lossy(reference).into_owned()
		}
		Instruction::Merge { parents, .. } => parents
			.iter()
			.map(|p| String::from_utf8_lossy(p).into_owned())
			.collect::<Vec<_>>()
			.join(" "),
		// Pick-family without a comment: the canonical word
		// beside its short oid.
		_ => String::from(instruction.canonical_word()),
	}
}

/// First seven bytes of the commit argument as written
/// (display only; identity stays with the engine).
fn short_of_instruction(instruction: &Instruction) -> Option<String> {
	let arg: &CommitArg = instruction.commit_arg()?;
	let bytes = arg.as_bytes();
	Some(
		String::from_utf8_lossy(&bytes[..bytes.len().min(7)])
			.into_owned(),
	)
}

/// Whether a plan step and an observed instruction are the same
/// step: pick-family compares by commit (git may abbreviate the
/// argument it echoes back), everything else by canonical word
/// and arguments.
fn same_instruction(step: &Instruction, obs: &Instruction) -> bool {
	if step.canonical_word() != obs.canonical_word() {
		return false;
	}
	if step.is_pick_family() {
		return match (
			step.commit_arg().and_then(CommitArg::full),
			obs.commit_arg(),
		) {
			(Some(full), Some(arg)) => arg.names(&full),
			(None, Some(arg)) => {
				arg.as_bytes()
					== step
						.commit_arg()
						.map(CommitArg::as_bytes)
						.unwrap_or_default()
			}
			(_, None) => false,
		};
	}
	match (step, obs) {
		(
			Instruction::Exec { command, .. },
			Instruction::Exec { command: other, .. },
		) => command == other,
		(
			Instruction::Label { label, .. },
			Instruction::Label { label: other, .. },
		)
		| (
			Instruction::Reset { label, .. },
			Instruction::Reset { label: other, .. },
		) => label == other,
		(
			Instruction::UpdateRef { reference, .. },
			Instruction::UpdateRef {
				reference: other, ..
			},
		) => reference == other,
		(Instruction::Merge { .. }, Instruction::Merge { .. }) => {
			step.to_line_bytes() == obs.to_line_bytes()
		}
		// `break`/`noop` carry no arguments worth comparing.
		_ => true,
	}
}

/// What the inspector shows for the phase.
const fn inspector_of(phase: &Phase) -> Inspector {
	match phase {
		Phase::Draft | Phase::Review => Inspector::Preview,
		Phase::Done(_) => Inspector::Outcome,
		// Both explain a divergence: one in the journal, one
		// in the repository itself.
		Phase::Reconciling { .. } | Phase::Unsupported { .. } => {
			Inspector::Reconcile
		}
		// Stopped, the adopted external session, and the
		// UI-merged in-flight states all describe a stop.
		Phase::Stopped(_)
		| Phase::Adopted { .. }
		| Phase::Executing
		| Phase::Restoring => Inspector::Stop,
	}
}

/// Honest progress counters for the phase.
fn progress_of(
	session: &Session,
	snapshot: &Snapshot,
	phase: &Phase,
) -> Progress {
	let unresolved = snapshot.conflicts.len();
	match (phase, session.started.as_ref(), snapshot.rebase.as_ref())
	{
		// Counts are only honest once a program was handed to
		// git and the observation can see it.
		(
			Phase::Stopped(_)
			| Phase::Adopted { .. }
			| Phase::Executing
			| Phase::Restoring,
			Some(_),
			Some(rebase),
		) => {
			let done = rebase.done.instructions().count();
			Progress {
				done,
				total: Some(
					done + rebase.todo.instructions().count(),
				),
				unresolved,
			}
		}
		// A session that never started: nothing has run; the
		// total is unknown until the todo is handed over.
		_ => Progress {
			done: 0,
			total: None,
			unresolved,
		},
	}
}

/// Every action, computed for the phase. `Hidden` means the
/// phase does not offer the action at all; `Disabled` names the
/// blockers, most specific first.
fn actions_of(
	session: &Session,
	snapshot: &Snapshot,
	phase: &Phase,
	stop: Option<&Stop>,
) -> Actions {
	let draft = matches!(phase, Phase::Draft);
	Actions {
		start: start_action(session, snapshot, phase),
		r#continue: continue_action(phase, stop),
		skip: skip_action(stop),
		abort: abort_action(phase),
		quit: quit_action(phase),
		edit_plan: edit_plan_action(stop),
		// Plan-editing actions live in the draft only for v1:
		// a stopped rebase's remaining todo is editable in
		// principle, but the edit-todo round trip is a
		// separate feature. Not offered, not merely blocked.
		toggle_drop: if draft {
			Availability::Enabled
		} else {
			Availability::Hidden
		},
		set_action: if draft {
			Availability::Enabled
		} else {
			Availability::Hidden
		},
		move_up: if draft {
			Availability::Enabled
		} else {
			Availability::Hidden
		},
		move_down: if draft {
			Availability::Enabled
		} else {
			Availability::Hidden
		},
		insert_break: if draft {
			Availability::Enabled
		} else {
			Availability::Hidden
		},
		insert_exec: if draft {
			Availability::Enabled
		} else {
			Availability::Hidden
		},
		compose: if draft {
			Availability::Enabled
		} else {
			Availability::Hidden
		},
		// History edits are draft-only by definition; the
		// session's own ledger decides whether any exist.
		undo: if draft && session.can_undo() {
			Availability::Enabled
		} else {
			Availability::Hidden
		},
		redo: if draft && session.can_redo() {
			Availability::Enabled
		} else {
			Availability::Hidden
		},
		save_message: message_action(stop),
		cancel_message: message_action(stop),
		amend: amend_action(snapshot, stop),
		continue_unchanged: continue_unchanged_action(snapshot, stop),
		keep_empty: keep_empty_action(stop),
		restore: restore_action(session, phase),
		recovery_branch: if session.started.is_some() {
			// Once a rebase ran, the recovery ref (or at
			// least the recorded original tip) exists to
			// branch from — even after the end.
			Availability::Enabled
		} else {
			// Nothing ran; there is no tip of ours to
			// recover.
			Availability::Hidden
		},
		// Closing never aborts anything, so it is offered in
		// every phase — except while a message composition is
		// pending, which would strand the editor bridge.
		close: if matches!(stop, Some(Stop::Message { .. })) {
			Availability::Disabled(vec![Blocker::Busy])
		} else {
			Availability::Enabled
		},
	}
}

/// Start: only a clean draft on a clean repository.
fn start_action(
	session: &Session,
	snapshot: &Snapshot,
	phase: &Phase,
) -> Availability {
	// Starting makes sense only from a draft; running,
	// adopted, reconciling and terminal states have nothing
	// to start.
	if !matches!(phase, Phase::Draft) {
		return Availability::Hidden;
	}
	let mut blockers = Vec::new();
	let diagnostics = session.plan.as_ref().map_or_else(
		|| {
			vec![Diagnostic {
				step: None,
				kind: Issue::Empty,
			}]
		},
		Plan::validate,
	);
	let issues = issue_sentences(&diagnostics);
	if !issues.is_empty() {
		blockers.push(Blocker::Invalid(issues));
	}
	if !snapshot.staged.is_empty() || !snapshot.worktree.is_empty() {
		blockers.push(Blocker::LocalChanges);
	}
	if blockers.is_empty() {
		Availability::Enabled
	} else {
		Availability::Disabled(blockers)
	}
}

/// Fixed sentences for validation issues, one per unique kind,
/// in first-seen order.
fn issue_sentences(diagnostics: &[Diagnostic]) -> Vec<String> {
	let mut seen: Vec<Issue> = Vec::new();
	let mut out = Vec::new();
	for diagnostic in diagnostics {
		if seen.contains(&diagnostic.kind) {
			continue;
		}
		seen.push(diagnostic.kind);
		out.push(String::from(match diagnostic.kind {
			Issue::FoldWithoutTarget => {
				"a fold has no commit before it to fold into"
			}
			Issue::FoldAfterNonCommit => {
				"a fold follows a step that produces no commit"
			}
			Issue::ExecEmpty => "an exec step has an empty command",
			Issue::UnknownSyntax => {
				"the plan contains syntax gitui does not \
				 understand"
			}
			Issue::AllDropped => {
				"every commit is dropped; review before \
				 starting"
			}
			Issue::Empty => "the plan has no steps",
		}));
	}
	out
}

/// Continue, per stop kind.
fn continue_action(
	_phase: &Phase,
	stop: Option<&Stop>,
) -> Availability {
	let Some(stop) = stop else {
		// Nothing is stopped: between stops git is driving,
		// or the session is a draft or terminal one.
		return Availability::Hidden;
	};
	match stop {
		// Everything resolved and staged: `--continue` is
		// exactly the next move.
		Stop::Conflicts { paths } if paths.is_empty() => {
			Availability::Enabled
		}
		Stop::Conflicts { paths } => {
			Availability::Disabled(vec![Blocker::Conflicts(
				paths.clone(),
			)])
		}
		// The commit of an edit stop is already in place; a
		// break just marked the pause; a failed exec is
		// simply passed (git's default does not re-run it):
		// continuing is the normal move in all three.
		Stop::Edit { .. } | Stop::Break | Stop::ExecFailed { .. } => {
			Availability::Enabled
		}
		// The empty commit needs a keep/skip decision first;
		// plain `--continue` would re-stop.
		Stop::BecameEmpty { .. } => {
			Availability::Disabled(vec![Blocker::EmptyDecision])
		}
		// The composer flow saves the message, which drives
		// the engine; a bare continue would bypass it.
		Stop::Message { .. } => Availability::Hidden,
		// An unclassified stop: policy refuses to act, and no
		// specific blocker would be honest to name.
		Stop::Unknown { .. } => Availability::Disabled(Vec::new()),
	}
}

const fn skip_action(stop: Option<&Stop>) -> Availability {
	match stop {
		// Every stop except the message flow can be skipped:
		// git supports it in each of those states.
		Some(stop) if !matches!(stop, Stop::Message { .. }) => {
			Availability::Enabled
		}
		// The composer owns the flow; skipping would strand
		// the bridge request. Not stopped: nothing to skip.
		Some(_) | None => Availability::Hidden,
	}
}

/// Abort as the escape hatch: offered whenever a rebase is
/// observable in a session that could have one. Aborting an
/// externally started rebase is legitimate recovery.
const fn abort_action(phase: &Phase) -> Availability {
	if matches!(
		phase,
		Phase::Stopped(_)
			| Phase::Adopted { .. }
			| Phase::Reconciling { .. }
	) {
		Availability::Enabled
	} else {
		// Draft (nothing to abort) and terminal (already
		// over).
		Availability::Hidden
	}
}

/// Quit keeps the current state and drops git's bookkeeping.
/// Deliberately harder to reach than abort: offered only where
/// the session does not manage the rebase anyway.
const fn quit_action(phase: &Phase) -> Availability {
	if matches!(
		phase,
		Phase::Adopted { .. } | Phase::Reconciling { .. }
	) {
		Availability::Enabled
	} else {
		// Managed running sessions use abort; a draft and a
		// terminal session have no sequencer state to quit.
		Availability::Hidden
	}
}

/// Editing the remaining program of a stopped rebase, except at
/// a message stop (the composer owns the flow).
const fn edit_plan_action(stop: Option<&Stop>) -> Availability {
	match stop {
		Some(stop) if !matches!(stop, Stop::Message { .. }) => {
			Availability::Enabled
		}
		// The composer owns the flow; a draft *is* the
		// editable plan; terminal states have no remaining
		// program.
		Some(_) | None => Availability::Hidden,
	}
}

/// Both composer actions hinge on the same pending request.
const fn message_action(stop: Option<&Stop>) -> Availability {
	if matches!(stop, Some(Stop::Message { .. })) {
		Availability::Enabled
	} else {
		// No message request: the composer is not part of
		// this phase.
		Availability::Hidden
	}
}

/// Amend at an edit stop needs something staged to amend with.
fn amend_action(
	snapshot: &Snapshot,
	stop: Option<&Stop>,
) -> Availability {
	match stop {
		Some(Stop::Edit { .. }) if snapshot.staged.is_empty() => {
			// Nothing staged: amending would be a no-op
			// commit rewrite.
			Availability::Disabled(vec![Blocker::LocalChanges])
		}
		Some(Stop::Edit { .. }) => Availability::Enabled,
		// Only an edit stop amends.
		_ => Availability::Hidden,
	}
}

/// Continuing an edit stop without changes, and leaving a
/// break, are the same "move along" gesture.
const fn continue_unchanged_action(
	snapshot: &Snapshot,
	stop: Option<&Stop>,
) -> Availability {
	match stop {
		Some(Stop::Edit { .. }) if snapshot.staged.is_empty() => {
			Availability::Enabled
		}
		// With staged changes the amend action is the one
		// that applies; continue-unchanged would discard the
		// user's staging intent.
		Some(Stop::Break) => Availability::Enabled,
		// Other stops have their own actions.
		_ => Availability::Hidden,
	}
}

/// The keep side of the empty-commit decision (the skip side
/// is `skip`).
const fn keep_empty_action(stop: Option<&Stop>) -> Availability {
	if matches!(stop, Some(Stop::BecameEmpty { .. })) {
		Availability::Enabled
	} else {
		// No empty commit awaits a decision.
		Availability::Hidden
	}
}

/// Restoring protected changes once the rebase finished.
const fn restore_action(
	session: &Session,
	phase: &Phase,
) -> Availability {
	if matches!(phase, Phase::Done(Done::Finished))
		&& session.protection.is_some()
	{
		Availability::Enabled
	} else {
		// Nothing protected, or the session did not finish
		// (an aborted rebase already restored its state).
		Availability::Hidden
	}
}

#[cfg(test)]
mod tests {
	#![allow(
		clippy::unwrap_used,
		clippy::panic,
		clippy::cast_possible_truncation
	)]

	use super::*;
	use crate::rebase::{
		engine::EmptyPolicy,
		journal::{Protection, Started},
		snapshot::{Change, ChangeKind, Conflict, RebaseFiles},
		todo::{CommitArg, FixupMessage, Todo, TodoLine},
		Generation,
	};
	use std::{fs, path::Path, process::Command};
	use tempfile::TempDir;

	// ------------------------------------------------------------------
	// fixture helpers (pure)
	// ------------------------------------------------------------------

	const OID_BASE: &str = "1000000000000000000000000000000000000000";

	fn hex(n: u64) -> String {
		format!("{n:040x}")
	}

	fn oid(n: u64) -> CommitId {
		CommitId::from_str_unchecked(&hex(n)).unwrap()
	}

	fn snap() -> Snapshot {
		Snapshot {
			generation: Generation(0),
			head: Some(oid(9)),
			branch: Some(String::from("main")),
			operation: Operation::Clean,
			staged: Vec::new(),
			worktree: Vec::new(),
			conflicts: Vec::new(),
			untracked: Vec::new(),
			rebase: None,
			bridge: None,
		}
	}

	fn conflict_of(path: &str) -> Conflict {
		Conflict {
			path: String::from(path),
			stages: [Some(oid(1)), Some(oid(2)), Some(oid(3))],
			markers_in_worktree: false,
		}
	}

	fn change_of(path: &str) -> Change {
		Change {
			path: String::from(path),
			kind: ChangeKind::Modified,
		}
	}

	fn with_op(mut s: Snapshot, op: Operation) -> Snapshot {
		s.operation = op;
		s
	}

	fn with_conflicts(mut s: Snapshot, paths: &[&str]) -> Snapshot {
		s.conflicts = paths.iter().map(|p| conflict_of(p)).collect();
		s
	}

	fn with_staged(mut s: Snapshot, paths: &[&str]) -> Snapshot {
		s.staged = paths.iter().map(|p| change_of(p)).collect();
		s
	}

	fn with_worktree(mut s: Snapshot, paths: &[&str]) -> Snapshot {
		s.worktree = paths.iter().map(|p| change_of(p)).collect();
		s
	}

	fn with_bridge(mut s: Snapshot, file: &str) -> Snapshot {
		s.bridge = Some(crate::rebase::snapshot::BridgeRequest {
			kind: crate::rebase::snapshot::BridgeKind::Message,
			file: String::from(file),
		});
		s
	}

	fn with_rebase(mut s: Snapshot, rebase: RebaseState) -> Snapshot {
		s.operation = Operation::RebaseInteractive;
		s.rebase = Some(rebase);
		s
	}

	/// A sequencer state whose `done` and todo carry the given
	/// documents, plus extra raw files (stopped-sha, amend, …).
	fn rebase_state(
		done: &str,
		todo: &str,
		extra: &[(&str, &str)],
	) -> RebaseState {
		let orig_head = hex(8);
		let mut files: Vec<(&str, &[u8])> = vec![
			("onto", OID_BASE.as_bytes()),
			("orig-head", orig_head.as_bytes()),
			("head-name", b"refs/heads/main"),
			("done", done.as_bytes()),
			("git-rebase-todo", todo.as_bytes()),
		];
		for (name, content) in extra {
			files.push((name, content.as_bytes()));
		}
		RebaseState::from_files(&RebaseFiles { files: &files })
			.unwrap()
	}

	fn pick_line(n: u64) -> TodoLine {
		TodoLine::Instruction(Instruction::Pick {
			word: b"pick".as_slice().into(),
			commit: CommitArg::from_id(oid(n)),
			after: format!(" # subject {n}").into_bytes().into(),
		})
	}

	fn fixup_line(n: u64) -> TodoLine {
		TodoLine::Instruction(Instruction::Fixup {
			word: b"fixup".as_slice().into(),
			message: FixupMessage::Previous,
			commit: CommitArg::from_id(oid(n)),
			after: format!(" # subject {n}").into_bytes().into(),
		})
	}

	fn drop_line(n: u64) -> TodoLine {
		TodoLine::Instruction(Instruction::Drop {
			word: b"drop".as_slice().into(),
			commit: CommitArg::from_id(oid(n)),
			after: format!(" # subject {n}").into_bytes().into(),
		})
	}

	fn exec_line(command: &str) -> TodoLine {
		TodoLine::Instruction(
			Instruction::exec(command.as_bytes()).unwrap(),
		)
	}

	fn break_line() -> TodoLine {
		TodoLine::Instruction(Instruction::break_instruction())
	}

	fn plan_of(lines: Vec<TodoLine>) -> Plan {
		Plan::from_todo(Todo::from_lines(lines))
	}

	/// The todo bytes a plan compiles to, as the engine hands
	/// them to git; the observation must echo them back.
	fn compiled(lines: &[TodoLine]) -> Vec<u8> {
		plan_of(lines.to_vec()).to_todo().to_bytes()
	}

	#[allow(clippy::needless_pass_by_value)]
	fn running_session(lines: Vec<TodoLine>) -> Session {
		let plan = plan_of(lines.clone());
		let todo = compiled(&lines);
		Session {
			phase: crate::rebase::journal::Phase::Running,
			plan: Some(plan),
			started: Some(Started {
				todo,
				recovery_ref: None,
				empty: EmptyPolicy::Stop,
			}),
			..Session::default()
		}
	}

	fn draft_session(lines: Vec<TodoLine>) -> Session {
		Session {
			phase: crate::rebase::journal::Phase::Draft,
			plan: Some(plan_of(lines)),
			..Session::default()
		}
	}

	// ------------------------------------------------------------------
	// phase derivation
	// ------------------------------------------------------------------

	#[test]
	fn no_session_clean_is_draft() {
		let session = Session::default();
		let policy = policy(&session, &snap());
		assert_eq!(policy.phase, Phase::Draft);
		assert_eq!(policy.inspector, Inspector::Preview);
		assert!(policy.rail.is_empty());
		assert_eq!(policy.progress, Progress::default());
		// No plan at all: the start blocker says why.
		assert_eq!(
			policy.actions.start,
			Availability::Disabled(vec![Blocker::Invalid(vec![
				String::from("the plan has no steps")
			])])
		);
	}

	#[test]
	fn no_session_foreign_operation_is_unsupported() {
		for op in [
			Operation::Merge,
			Operation::Revert,
			Operation::CherryPick,
			Operation::Am,
			Operation::Other,
		] {
			let policy =
				policy(&Session::default(), &with_op(snap(), op));
			assert_eq!(
				policy.phase,
				Phase::Unsupported { operation: op }
			);
			assert_eq!(policy.inspector, Inspector::Reconcile);
			// Nothing mutates in an unsupported state.
			assert!(!policy.actions.start.is_enabled());
			assert!(!policy.actions.abort.is_enabled());
		}
	}

	#[test]
	fn no_session_rebase_in_progress_reconciles() {
		let mut snapshot = snap();
		snapshot.operation = Operation::RebaseInteractive;
		let policy = policy(&Session::default(), &snapshot);
		assert!(matches!(policy.phase, Phase::Reconciling { .. }));
		assert_eq!(policy.inspector, Inspector::Reconcile);
	}

	#[test]
	fn draft_with_external_rebase_reconciles() {
		let session = draft_session(vec![pick_line(1)]);
		let snapshot = with_rebase(
			snap(),
			rebase_state(&format!("pick {} # x\n", hex(1)), "", &[]),
		);
		let policy = policy(&session, &snapshot);
		assert!(matches!(policy.phase, Phase::Reconciling { .. }));
	}

	#[test]
	fn draft_with_foreign_operation_reconciles() {
		let session = draft_session(vec![pick_line(1)]);
		let policy =
			policy(&session, &with_op(snap(), Operation::Merge));
		assert!(matches!(policy.phase, Phase::Reconciling { .. }));
	}

	#[test]
	fn running_with_conflicts_stops() {
		let session = running_session(vec![
			pick_line(1),
			pick_line(2),
			pick_line(3),
		]);
		let snapshot = with_conflicts(
			with_rebase(
				snap(),
				rebase_state(
					&format!("pick {} # subject 1\n", hex(1)),
					&format!(
						"pick {} # subject 2\npick {} # subject 3\n",
						hex(2),
						hex(3)
					),
					&[("stopped-sha", &hex(1))],
				),
			),
			&["f.txt"],
		);
		let policy = policy(&session, &snapshot);
		assert_eq!(
			policy.phase,
			Phase::Stopped(Stop::Conflicts {
				paths: vec![String::from("f.txt")]
			})
		);
		assert_eq!(policy.inspector, Inspector::Stop);
	}

	#[test]
	fn running_with_resolved_conflict_stops_resolved() {
		// Unmerged entries gone, resolution staged: still a
		// conflict stop, but nothing left to resolve.
		let session =
			running_session(vec![pick_line(1), pick_line(2)]);
		let mut snapshot = with_staged(
			with_rebase(
				snap(),
				rebase_state(
					&format!("pick {} # subject 1\n", hex(1)),
					&format!("pick {} # subject 2\n", hex(2)),
					&[("stopped-sha", &hex(1))],
				),
			),
			&["f.txt"],
		);
		snapshot.operation = Operation::RebaseInteractive;
		let policy = policy(&session, &snapshot);
		assert_eq!(
			policy.phase,
			Phase::Stopped(Stop::Conflicts { paths: Vec::new() })
		);
	}

	#[test]
	fn running_with_amend_stops_at_edit() {
		let session =
			running_session(vec![pick_line(1), pick_line(2)]);
		let snapshot = with_rebase(
			snap(),
			rebase_state(
				&format!("edit {} # subject 1\n", hex(1)),
				&format!("pick {} # subject 2\n", hex(2)),
				&[
					("stopped-sha", hex(1).as_str()),
					("amend", hex(1).as_str()),
				],
			),
		);
		let policy = policy(&session, &snapshot);
		assert_eq!(
			policy.phase,
			Phase::Stopped(Stop::Edit { commit: oid(1) })
		);
	}

	#[test]
	fn running_with_break_in_done_stops_at_break() {
		let session = running_session(vec![
			pick_line(1),
			break_line(),
			pick_line(2),
		]);
		let snapshot = with_rebase(
			snap(),
			rebase_state(
				&format!("pick {} # subject 1\nbreak\n", hex(1)),
				&format!("pick {} # subject 2\n", hex(2)),
				&[],
			),
		);
		let policy = policy(&session, &snapshot);
		assert_eq!(policy.phase, Phase::Stopped(Stop::Break));
	}

	#[test]
	fn running_empty_stop_without_conflicts_is_became_empty() {
		// Evidence (git 2.55): `--empty=stop` leaves
		// stopped-sha, no unmerged entries, nothing staged.
		let session =
			running_session(vec![pick_line(1), pick_line(2)]);
		let snapshot = with_rebase(
			snap(),
			rebase_state(
				&format!("pick {} # subject 1\n", hex(1)),
				&format!("pick {} # subject 2\n", hex(2)),
				&[("stopped-sha", hex(1).as_str())],
			),
		);
		let policy = policy(&session, &snapshot);
		assert_eq!(
			policy.phase,
			Phase::Stopped(Stop::BecameEmpty { commit: oid(1) })
		);
	}

	#[test]
	fn running_reword_tail_without_bridge_is_unknown() {
		let session =
			running_session(vec![pick_line(1), pick_line(2)]);
		let snapshot = with_rebase(
			snap(),
			rebase_state(
				&format!("reword {} # subject 1\n", hex(1)),
				&format!("pick {} # subject 2\n", hex(2)),
				&[("stopped-sha", hex(1).as_str())],
			),
		);
		let policy = policy(&session, &snapshot);
		assert!(matches!(
			policy.phase,
			Phase::Stopped(Stop::Unknown { .. })
		));
	}

	#[test]
	fn running_with_bridge_stops_at_message() {
		let session =
			running_session(vec![pick_line(1), pick_line(2)]);
		let snapshot = with_bridge(
			with_rebase(
				snap(),
				rebase_state(
					&format!("reword {} # subject 1\n", hex(1)),
					&format!("pick {} # subject 2\n", hex(2)),
					&[("stopped-sha", hex(1).as_str())],
				),
			),
			"/tmp/bridge/msg",
		);
		let policy = policy(&session, &snapshot);
		assert_eq!(
			policy.phase,
			Phase::Stopped(Stop::Message {
				request: String::from("/tmp/bridge/msg")
			})
		);
	}

	#[test]
	fn running_exec_failure_is_the_done_tail() {
		// Evidence (git 2.55): the failing exec is the last
		// line of `done`; the rest of the program stays in
		// the todo; no stopped-sha. The reschedule flag does
		// not change the stopped state.
		let session = running_session(vec![
			pick_line(1),
			exec_line("false"),
			pick_line(2),
		]);
		let snapshot = with_rebase(
			snap(),
			rebase_state(
				&format!("pick {} # subject 1\nexec false\n", hex(1)),
				&format!("pick {} # subject 2\n", hex(2)),
				&[("no-reschedule-failed-exec", "")],
			),
		);
		let policy = policy(&session, &snapshot);
		assert_eq!(
			policy.phase,
			Phase::Stopped(Stop::ExecFailed {
				command: String::from("false"),
				code: None,
			})
		);
	}

	#[test]
	fn running_without_any_marker_is_unknown() {
		let session = running_session(vec![pick_line(1)]);
		let snapshot = with_rebase(
			snap(),
			rebase_state(&format!("pick {}\n", hex(1)), "", &[]),
		);
		// stopped-sha present but staged side also present is
		// the resolved case; here nothing explains the stop.
		let policy = policy(&session, &snapshot);
		assert!(matches!(
			policy.phase,
			Phase::Stopped(Stop::Unknown { .. })
		));
	}

	#[test]
	fn running_with_clean_repo_reconciles() {
		// The crash / external-continue case.
		let session = running_session(vec![pick_line(1)]);
		let policy = policy(&session, &snap());
		assert!(matches!(policy.phase, Phase::Reconciling { .. }));
		// And the rail claims nothing it cannot observe.
		assert!(policy.rail.is_empty());
		assert_eq!(policy.inspector, Inspector::Reconcile);
	}

	#[test]
	fn running_under_foreign_operation_reconciles() {
		let session = running_session(vec![pick_line(1)]);
		let policy =
			policy(&session, &with_op(snap(), Operation::Merge));
		assert!(matches!(policy.phase, Phase::Reconciling { .. }));
	}

	#[test]
	fn journal_terminal_wins_over_observation() {
		// A rebase still observable, but the journal already
		// recorded the end: the journal wins.
		for (phase, done) in [
			(
				crate::rebase::journal::Phase::Finished(oid(9)),
				Done::Finished,
			),
			(crate::rebase::journal::Phase::Aborted, Done::Aborted),
			(crate::rebase::journal::Phase::Quit, Done::Quit),
		] {
			let session = Session {
				phase,
				..running_session(vec![pick_line(1)])
			};
			let snapshot = with_rebase(
				snap(),
				rebase_state(&format!("pick {}\n", hex(1)), "", &[]),
			);
			let policy = policy(&session, &snapshot);
			assert_eq!(policy.phase, Phase::Done(done));
			assert_eq!(policy.inspector, Inspector::Outcome);
		}
	}

	#[test]
	fn adopted_observes_the_external_session() {
		let session = Session {
			adopted: true,
			..running_session(vec![pick_line(1)])
		};
		let snapshot = with_conflicts(
			with_rebase(
				snap(),
				rebase_state(
					&format!("pick {}\n", hex(1)),
					&format!("pick {}\n", hex(2)),
					&[("stopped-sha", hex(1).as_str())],
				),
			),
			&["f.txt"],
		);
		let policy = policy(&session, &snapshot);
		assert_eq!(policy.phase, Phase::Adopted { managed: false });
		assert_eq!(policy.inspector, Inspector::Stop);
		// The observation still classifies for the inspector.
		assert_eq!(
			policy.actions.skip,
			Availability::Enabled,
			"adopted stops are read-only but still classified"
		);
	}

	#[test]
	fn adopted_without_rebase_reconciles() {
		let session = Session {
			adopted: true,
			..running_session(vec![pick_line(1)])
		};
		let policy = policy(&session, &snap());
		assert!(matches!(policy.phase, Phase::Reconciling { .. }));
	}

	// ------------------------------------------------------------------
	// rail rows
	// ------------------------------------------------------------------

	#[test]
	fn draft_rows_skip_comments_and_blanks() {
		let session = draft_session(vec![
			pick_line(1),
			TodoLine::Comment(b"# note".as_slice().into()),
			TodoLine::Blank,
			TodoLine::Raw(b"future git syntax".as_slice().into()),
			fixup_line(2),
			drop_line(3),
		]);
		let policy = policy(&session, &snap());
		assert_eq!(policy.rail.len(), 4);
		assert_eq!(policy.rail[0].state, RowState::Pending);
		assert_eq!(policy.rail[0].label, "subject 1");
		assert_eq!(
			policy.rail[0].short_oid.as_deref(),
			Some(&hex(1)[..7])
		);
		assert!(!policy.rail[0].folds_into_previous);
		assert_eq!(policy.rail[1].state, RowState::Failed);
		assert_eq!(policy.rail[1].label, "future git syntax");
		assert_eq!(policy.rail[2].state, RowState::Pending);
		assert!(policy.rail[2].folds_into_previous);
		assert_eq!(policy.rail[3].state, RowState::Dropped);
	}

	#[test]
	fn running_rows_align_with_the_observation() {
		// Distinct leading digits so short oids are
		// unambiguous.
		let id_a = "a111111111111111111111111111111111111111";
		let id_b = "b222222222222222222222222222222222222222";
		let id_c = "c333333333333333333333333333333333333333";
		let id_d = "d444444444444444444444444444444444444444";
		let id_r = "f777777777777777777777777777777777777777";
		let pick_hex = |hex40: &str| {
			TodoLine::Instruction(Instruction::Pick {
				word: b"pick".as_slice().into(),
				commit: CommitArg::from_id(
					CommitId::from_str_unchecked(hex40).unwrap(),
				),
				after: b" # subject".as_slice().into(),
			})
		};
		let fixup_hex = |hex40: &str| {
			TodoLine::Instruction(Instruction::Fixup {
				word: b"fixup".as_slice().into(),
				message: FixupMessage::Previous,
				commit: CommitArg::from_id(
					CommitId::from_str_unchecked(hex40).unwrap(),
				),
				after: b" # subject".as_slice().into(),
			})
		};
		let drop_hex = |hex40: &str| {
			TodoLine::Instruction(Instruction::Drop {
				word: b"drop".as_slice().into(),
				commit: CommitArg::from_id(
					CommitId::from_str_unchecked(hex40).unwrap(),
				),
				after: b" # subject".as_slice().into(),
			})
		};
		let session = running_session(vec![
			pick_hex(id_a),
			fixup_hex(id_b),
			pick_hex(id_c),
			drop_hex(id_d),
			exec_line("make test"),
		]);
		let subject = " # subject\n";
		let mut snapshot = with_conflicts(
			with_rebase(
				snap(),
				rebase_state(
					&format!(
						"pick {id_a}{subject}fixup {id_b}{subject}"
					),
					&format!(
						"pick {id_c}{subject}drop {id_d}{subject}\
						 exec make test\n"
					),
					&[("stopped-sha", id_b)],
				),
			),
			&["f.txt"],
		);
		snapshot.rebase.as_mut().unwrap().rewritten = vec![(
			CommitId::from_str_unchecked(id_a).unwrap(),
			CommitId::from_str_unchecked(id_r).unwrap(),
		)];
		let policy = policy(&session, &snapshot);

		let states: Vec<RowState> =
			policy.rail.iter().map(|row| row.state).collect();
		assert_eq!(
			states,
			vec![
				RowState::Done,
				RowState::Failed,
				RowState::Pending,
				RowState::Pending,
				RowState::Pending,
			]
		);
		// Executed steps prefer the rewritten output oid.
		assert_eq!(
			policy.rail[0].short_oid.as_deref(),
			Some(&id_r[..7])
		);
		assert_eq!(
			policy.rail[1].short_oid.as_deref(),
			Some(&id_b[..7])
		);
		assert!(policy.rail[1].folds_into_previous);
		// Progress is honest and unsummed.
		assert_eq!(
			policy.progress,
			Progress {
				done: 2,
				total: Some(5),
				unresolved: 1,
			}
		);
	}

	#[test]
	fn running_rows_mark_a_break_boundary() {
		let session = running_session(vec![
			pick_line(1),
			break_line(),
			pick_line(2),
		]);
		let snapshot = with_rebase(
			snap(),
			rebase_state(
				&format!("pick {} # subject 1\nbreak\n", hex(1)),
				&format!("pick {} # subject 2\n", hex(2)),
				&[],
			),
		);
		let policy = policy(&session, &snapshot);
		assert_eq!(
			policy.rail.iter().map(|r| r.state).collect::<Vec<_>>(),
			vec![RowState::Done, RowState::Stop, RowState::Pending]
		);
	}

	#[test]
	fn misaligned_plan_falls_back_to_observation_rows() {
		// The user removed a step externally (edit-todo): the
		// plan no longer matches the observation.
		let session = running_session(vec![
			pick_line(1),
			pick_line(2),
			pick_line(3),
		]);
		let snapshot = with_rebase(
			snap(),
			rebase_state(
				&format!("pick {} # subject 1\n", hex(1)),
				&format!("pick {} # subject 3\n", hex(3)),
				&[("stopped-sha", hex(1).as_str())],
			),
		);
		let policy = policy(&session, &snapshot);
		assert!(matches!(policy.phase, Phase::Stopped(_)));
		// Fallback rows come from the observation, plan-less.
		assert_eq!(policy.rail.len(), 2);
		assert!(policy.rail.iter().all(|r| r.step.is_none()));
		assert_eq!(policy.rail[0].state, RowState::Done);
		assert_eq!(policy.rail[1].state, RowState::Pending);
		// The inspector explains the mismatch instead.
		assert_eq!(policy.inspector, Inspector::Reconcile);
	}

	#[test]
	fn adopted_rows_come_from_the_observation() {
		let session = Session {
			adopted: true,
			plan: None,
			started: Some(Started {
				todo: Vec::new(),
				recovery_ref: None,
				empty: EmptyPolicy::Stop,
			}),
			..Session::default()
		};
		let snapshot = with_rebase(
			snap(),
			rebase_state(
				&format!("pick {}\n", hex(1)),
				&format!("drop {}\n", hex(2)),
				&[],
			),
		);
		let policy = policy(&session, &snapshot);
		assert_eq!(policy.rail.len(), 2);
		assert_eq!(policy.rail[0].state, RowState::Done);
		// The drop is still ahead: pending, not dropped.
		assert_eq!(policy.rail[1].state, RowState::Pending);
		assert!(policy.rail.iter().all(|r| r.step.is_none()));
	}

	#[test]
	fn terminal_rows_are_all_settled() {
		let session = Session {
			phase: crate::rebase::journal::Phase::Finished(oid(9)),
			..running_session(vec![pick_line(1), drop_line(2)])
		};
		let policy = policy(&session, &snap());
		assert_eq!(
			policy.rail.iter().map(|r| r.state).collect::<Vec<_>>(),
			vec![RowState::Done, RowState::Dropped]
		);
	}

	#[test]
	fn labels_fall_back_to_arguments() {
		let session = draft_session(vec![
			exec_line("make test"),
			break_line(),
			TodoLine::Instruction(Instruction::Label {
				word: b"label".as_slice().into(),
				label: b"top".as_slice().into(),
			}),
			TodoLine::Instruction(Instruction::UpdateRef {
				word: b"update-ref".as_slice().into(),
				reference: b"refs/heads/x".as_slice().into(),
			}),
		]);
		let policy = policy(&session, &snap());
		assert_eq!(policy.rail[0].label, "make test");
		assert_eq!(policy.rail[1].label, "break");
		assert_eq!(policy.rail[2].label, "top");
		assert_eq!(policy.rail[3].label, "refs/heads/x");
	}

	// ------------------------------------------------------------------
	// actions (full-struct asserts)
	// ------------------------------------------------------------------

	fn all_hidden() -> Actions {
		Actions {
			start: Availability::Hidden,
			r#continue: Availability::Hidden,
			skip: Availability::Hidden,
			abort: Availability::Hidden,
			quit: Availability::Hidden,
			edit_plan: Availability::Hidden,
			toggle_drop: Availability::Hidden,
			set_action: Availability::Hidden,
			move_up: Availability::Hidden,
			move_down: Availability::Hidden,
			insert_break: Availability::Hidden,
			insert_exec: Availability::Hidden,
			compose: Availability::Hidden,
			undo: Availability::Hidden,
			redo: Availability::Hidden,
			save_message: Availability::Hidden,
			cancel_message: Availability::Hidden,
			amend: Availability::Hidden,
			continue_unchanged: Availability::Hidden,
			keep_empty: Availability::Hidden,
			restore: Availability::Hidden,
			recovery_branch: Availability::Hidden,
			close: Availability::Hidden,
		}
	}

	fn expected(set: impl FnOnce(&mut Actions)) -> Actions {
		let mut actions = all_hidden();
		set(&mut actions);
		actions
	}

	fn enabled() -> Availability {
		Availability::Enabled
	}

	#[test]
	fn actions_in_a_clean_draft() {
		let session = draft_session(vec![
			pick_line(1),
			fixup_line(2),
			exec_line("make"),
		]);
		let policy = policy(&session, &snap());
		assert_eq!(
			policy.actions,
			expected(|a| {
				a.start = enabled();
				a.toggle_drop = enabled();
				a.set_action = enabled();
				a.move_up = enabled();
				a.move_down = enabled();
				a.insert_break = enabled();
				a.insert_exec = enabled();
				a.compose = enabled();
				a.close = enabled();
			})
		);
	}

	#[test]
	fn actions_draft_blocked_by_local_changes() {
		let session = draft_session(vec![pick_line(1)]);
		let staged =
			policy(&session, &with_staged(snap(), &["f.txt"]));
		assert_eq!(
			staged.actions.start,
			Availability::Disabled(vec![Blocker::LocalChanges])
		);
		let worktree =
			policy(&session, &with_worktree(snap(), &["f.txt"]));
		assert_eq!(
			worktree.actions.start,
			Availability::Disabled(vec![Blocker::LocalChanges])
		);
	}

	#[test]
	fn actions_draft_blocked_by_invalid_plan_first() {
		let session =
			draft_session(vec![squash_first(), pick_line(1)]);
		let policy =
			policy(&session, &with_staged(snap(), &["f.txt"]));
		assert_eq!(
			policy.actions.start,
			Availability::Disabled(vec![
				Blocker::Invalid(vec![String::from(
					"a fold has no commit before it to fold into"
				)]),
				Blocker::LocalChanges,
			])
		);
	}

	#[test]
	fn actions_invalid_plan_sentences_are_unique_per_kind() {
		let session = draft_session(vec![
			squash_first(),
			pick_line(1),
			squash_first(),
		]);
		let policy = policy(&session, &snap());
		assert_eq!(
			policy.actions.start,
			Availability::Disabled(vec![Blocker::Invalid(vec![
				String::from(
					"a fold has no commit before it to fold into"
				),
			])])
		);
	}

	fn squash_first() -> TodoLine {
		TodoLine::Instruction(Instruction::Squash {
			word: b"squash".as_slice().into(),
			commit: CommitArg::from_id(oid(1)),
			after: b"".as_slice().into(),
		})
	}

	#[test]
	fn actions_at_a_conflict_stop() {
		let session =
			running_session(vec![pick_line(1), pick_line(2)]);
		let snapshot = with_conflicts(
			with_rebase(
				snap(),
				rebase_state(
					&format!("pick {} # subject 1\n", hex(1)),
					&format!("pick {} # subject 2\n", hex(2)),
					&[("stopped-sha", hex(1).as_str())],
				),
			),
			&["a.txt", "b.txt"],
		);
		let policy = policy(&session, &snapshot);
		assert_eq!(
			policy.actions,
			expected(|a| {
				a.r#continue =
					Availability::Disabled(vec![Blocker::Conflicts(
						vec![
							String::from("a.txt"),
							String::from("b.txt"),
						],
					)]);
				a.skip = enabled();
				a.abort = enabled();
				a.edit_plan = enabled();
				a.close = enabled();
				a.recovery_branch = enabled();
			})
		);
	}

	#[test]
	fn actions_resolved_conflict_continues() {
		let session =
			running_session(vec![pick_line(1), pick_line(2)]);
		let mut snapshot = with_staged(
			with_rebase(
				snap(),
				rebase_state(
					&format!("pick {} # subject 1\n", hex(1)),
					&format!("pick {} # subject 2\n", hex(2)),
					&[("stopped-sha", hex(1).as_str())],
				),
			),
			&["f.txt"],
		);
		snapshot.operation = Operation::RebaseInteractive;
		let policy = policy(&session, &snapshot);
		assert!(policy.actions.r#continue.is_enabled());
		assert!(policy.actions.skip.is_enabled());
	}

	#[test]
	fn actions_at_an_edit_stop() {
		let session = running_session(vec![pick_line(1)]);
		let rebase = rebase_state(
			&format!("edit {} # subject 1\n", hex(1)),
			"",
			&[
				("stopped-sha", hex(1).as_str()),
				("amend", hex(1).as_str()),
			],
		);
		// Nothing staged: amend is blocked, leave-as-is works.
		let unstaged =
			policy(&session, &with_rebase(snap(), rebase.clone()));
		assert_eq!(
			unstaged.actions,
			expected(|a| {
				a.r#continue = enabled();
				a.skip = enabled();
				a.abort = enabled();
				a.edit_plan = enabled();
				a.close = enabled();
				a.recovery_branch = enabled();
				a.amend = Availability::Disabled(vec![
					Blocker::LocalChanges,
				]);
				a.continue_unchanged = enabled();
			})
		);
		// Staged changes: amend becomes the move.
		let staged = policy(
			&session,
			&with_staged(with_rebase(snap(), rebase), &["f.txt"]),
		);
		assert!(staged.actions.amend.is_enabled());
		assert!(!staged.actions.continue_unchanged.is_enabled());
	}

	#[test]
	fn actions_at_a_break() {
		let session = running_session(vec![
			pick_line(1),
			break_line(),
			pick_line(2),
		]);
		let snapshot = with_rebase(
			snap(),
			rebase_state(
				&format!("pick {} # subject 1\nbreak\n", hex(1)),
				&format!("pick {} # subject 2\n", hex(2)),
				&[],
			),
		);
		let policy = policy(&session, &snapshot);
		assert!(policy.actions.r#continue.is_enabled());
		assert!(policy.actions.continue_unchanged.is_enabled());
		assert!(policy.actions.skip.is_enabled());
		assert!(!policy.actions.amend.is_enabled());
	}

	#[test]
	fn actions_at_an_empty_stop() {
		let session =
			running_session(vec![pick_line(1), pick_line(2)]);
		let snapshot = with_rebase(
			snap(),
			rebase_state(
				&format!("pick {} # subject 1\n", hex(1)),
				&format!("pick {} # subject 2\n", hex(2)),
				&[("stopped-sha", hex(1).as_str())],
			),
		);
		let policy = policy(&session, &snapshot);
		assert_eq!(
			policy.actions.r#continue,
			Availability::Disabled(vec![Blocker::EmptyDecision])
		);
		assert!(policy.actions.keep_empty.is_enabled());
		assert!(policy.actions.skip.is_enabled());
	}

	#[test]
	fn actions_at_a_message_stop() {
		let session = running_session(vec![pick_line(1)]);
		let snapshot = with_bridge(
			with_rebase(
				snap(),
				rebase_state(
					&format!("reword {} # subject 1\n", hex(1)),
					"",
					&[("stopped-sha", hex(1).as_str())],
				),
			),
			"bridge/msg",
		);
		let policy = policy(&session, &snapshot);
		assert_eq!(
			policy.actions,
			expected(|a| {
				a.abort = enabled();
				a.close = Availability::Disabled(vec![Blocker::Busy]);
				a.save_message = enabled();
				a.cancel_message = enabled();
				a.recovery_branch = enabled();
			})
		);
	}

	#[test]
	fn actions_after_an_exec_failure() {
		let session = running_session(vec![
			pick_line(1),
			exec_line("false"),
			pick_line(2),
		]);
		let snapshot = with_rebase(
			snap(),
			rebase_state(
				&format!("pick {} # subject 1\nexec false\n", hex(1)),
				&format!("pick {} # subject 2\n", hex(2)),
				&[],
			),
		);
		let policy = policy(&session, &snapshot);
		assert!(policy.actions.r#continue.is_enabled());
		assert!(policy.actions.skip.is_enabled());
		assert!(policy.actions.abort.is_enabled());
	}

	#[test]
	fn actions_in_terminal_states() {
		let base = running_session(vec![pick_line(1)]);
		// Finished with protection: restore is offered.
		let session = Session {
			phase: crate::rebase::journal::Phase::Finished(oid(9)),
			protection: Some(Protection {
				stash: oid(5),
				include_untracked: false,
			}),
			..base.clone()
		};
		let protected = policy(&session, &snap());
		assert_eq!(
			protected.actions,
			expected(|a| {
				a.restore = enabled();
				a.recovery_branch = enabled();
				a.close = enabled();
			})
		);
		// Finished without protection: nothing to restore.
		let unprotected = Session {
			phase: crate::rebase::journal::Phase::Finished(oid(9)),
			..base.clone()
		};
		let without = policy(&unprotected, &snap());
		assert_eq!(without.actions.restore, Availability::Hidden);
		// Aborted: the stash was already popped by abort.
		let session = Session {
			phase: crate::rebase::journal::Phase::Aborted,
			protection: Some(Protection {
				stash: oid(5),
				include_untracked: false,
			}),
			..base
		};
		let aborted = policy(&session, &snap());
		assert_eq!(aborted.actions.restore, Availability::Hidden);
		assert!(aborted.actions.close.is_enabled());
	}

	#[test]
	fn actions_in_the_adopted_phase() {
		let session = Session {
			adopted: true,
			..running_session(vec![pick_line(1)])
		};
		let snapshot = with_rebase(
			snap(),
			rebase_state(
				&format!("pick {}\n", hex(1)),
				&format!("pick {}\n", hex(2)),
				&[],
			),
		);
		let policy = policy(&session, &snapshot);
		assert_eq!(
			policy.actions,
			expected(|a| {
				// The observation has no stop marker: an
				// unclassified stop, so continue refuses to
				// act and skip stays available.
				a.r#continue = Availability::Disabled(Vec::new());
				a.skip = enabled();
				a.abort = enabled();
				a.quit = enabled();
				a.edit_plan = enabled();
				a.close = enabled();
				a.recovery_branch = enabled();
			})
		);
	}

	#[test]
	fn actions_undo_redo_are_draft_only() {
		let mut session = draft_session(vec![pick_line(1)]);
		assert_eq!(
			policy(&session, &snap()).actions.undo,
			Availability::Hidden
		);
		session.edits.push(crate::rebase::plan::Edit::InsertBreak {
			before: None,
		});
		assert!(policy(&session, &snap()).actions.undo.is_enabled());
		session.redoable = 1;
		assert!(policy(&session, &snap()).actions.redo.is_enabled());
		let running = running_session(vec![pick_line(1)]);
		assert_eq!(
			policy(&running, &snap()).actions.undo,
			Availability::Hidden
		);
	}

	#[test]
	fn close_is_only_blocked_by_a_pending_message() {
		let message = running_session(vec![pick_line(1)]);
		let snapshot = with_bridge(
			with_rebase(
				snap(),
				rebase_state(
					&format!("reword {}\n", hex(1)),
					"",
					&[("stopped-sha", hex(1).as_str())],
				),
			),
			"bridge/msg",
		);
		let blocked = policy(&message, &snapshot);
		assert_eq!(
			blocked.actions.close,
			Availability::Disabled(vec![Blocker::Busy])
		);
		// Even mid-conflict, closing is allowed (it does not
		// abort anything).
		let session = running_session(vec![pick_line(1)]);
		let snapshot = with_conflicts(
			with_rebase(
				snap(),
				rebase_state(
					&format!("pick {}\n", hex(1)),
					&format!("pick {}\n", hex(2)),
					&[("stopped-sha", hex(1).as_str())],
				),
			),
			&["f.txt"],
		);
		assert!(policy(&session, &snapshot)
			.actions
			.close
			.is_enabled());
	}

	#[test]
	fn recovery_branch_needs_a_started_session() {
		let draft = draft_session(vec![pick_line(1)]);
		assert_eq!(
			policy(&draft, &snap()).actions.recovery_branch,
			Availability::Hidden
		);
		let running = running_session(vec![pick_line(1)]);
		assert!(policy(&running, &snap())
			.actions
			.recovery_branch
			.is_enabled());
	}

	// ------------------------------------------------------------------
	// evidence tests: real git, real sequencer directories
	// ------------------------------------------------------------------

	/// A throwaway home directory with a hermetic git config,
	/// so the user's real config cannot leak into fixtures.
	struct Sandbox(TempDir);

	impl Sandbox {
		fn new() -> Self {
			let dir = TempDir::new().unwrap();
			fs::write(
				dir.path().join("config"),
				"[user]\n\tname = name\n\
				 \temail = email\n\
				 [commit]\n\tgpgsign = false\n\
				 [init]\n\tdefaultBranch = main\n",
			)
			.unwrap();
			Self(dir)
		}

		fn git(&self, repo: &Path, args: &[&str]) -> Vec<u8> {
			self.try_git(repo, args, &[])
				.unwrap_or_else(|| panic!("git {args:?} failed"))
		}

		/// Tolerant of git reporting failure, which a stop
		/// *is*. `envs` carries per-call environment (the
		/// sequence editor trick).
		fn try_git(
			&self,
			repo: &Path,
			args: &[&str],
			envs: &[(&str, &str)],
		) -> Option<Vec<u8>> {
			let mut command = Command::new("git");
			command
				.args(args)
				.current_dir(repo)
				.env(
					"GIT_CONFIG_GLOBAL",
					self.0.path().join("config"),
				)
				.env(
					"GIT_CONFIG_SYSTEM",
					self.0.path().join("config"),
				)
				.env("GIT_CONFIG_NOSYSTEM", "1")
				.env("HOME", self.0.path())
				// no editor may ever open
				.env("GIT_SEQUENCE_EDITOR", ":")
				.env("GIT_EDITOR", ":")
				.env("EDITOR", ":");
			for (key, value) in envs {
				command.env(key, value);
			}
			let output = command.output().unwrap();
			if output.status.success() {
				Some(output.stdout)
			} else {
				None
			}
		}
	}

	fn rev(sb: &Sandbox, dir: &TempDir, revision: &str) -> String {
		String::from_utf8_lossy(
			&sb.git(dir.path(), &["rev-parse", revision]),
		)
		.trim()
		.to_owned()
	}

	/// main: base → a1 → a2. The rebase then replays a1 and a2
	/// onto base via a hand-written todo (the sequence-editor
	/// trick, exactly what the engine does).
	fn linear_repo(sb: &Sandbox) -> (TempDir, String, String) {
		let dir = TempDir::new().unwrap();
		sb.git(dir.path(), &["init", "-q"]);
		fs::write(dir.path().join("f.txt"), "base\n").unwrap();
		sb.git(dir.path(), &["add", "f.txt"]);
		sb.git(dir.path(), &["commit", "-q", "-m", "base"]);
		fs::write(dir.path().join("f.txt"), "one\n").unwrap();
		sb.git(dir.path(), &["commit", "-q", "-a", "-m", "a1"]);
		let a1 = rev(sb, &dir, "HEAD");
		fs::write(dir.path().join("f.txt"), "two\n").unwrap();
		sb.git(dir.path(), &["commit", "-q", "-a", "-m", "a2"]);
		let a2 = rev(sb, &dir, "HEAD");
		(dir, a1, a2)
	}

	/// Run a rebase with the given todo injected wholesale.
	fn run_rebase(
		sb: &Sandbox,
		dir: &TempDir,
		todo: &str,
		args: &[&str],
		base: &str,
	) {
		let todo_file = sb.0.path().join("injected-todo.txt");
		fs::write(&todo_file, todo).unwrap();
		let editor = format!("cp {}", todo_file.display());
		let mut git_args = vec!["rebase", "-i"];
		git_args.extend_from_slice(args);
		git_args.push(base);
		let _ = sb.try_git(
			dir.path(),
			&git_args,
			&[("GIT_SEQUENCE_EDITOR", editor.as_str())],
		);
	}

	/// One consistent observation, exactly as the engine would
	/// gather it: real status bytes and real sequencer files.
	fn observe(sb: &Sandbox, dir: &TempDir) -> Snapshot {
		let bytes = sb.git(
			dir.path(),
			&["status", "--porcelain=v2", "--branch", "-z"],
		);
		let (head, branch, entries) =
			crate::rebase::snapshot::parse_status(&bytes).unwrap();
		let merge_dir = dir.path().join(".git").join("rebase-merge");
		let rebase = if merge_dir.is_dir() {
			let mut gathered: Vec<(String, Vec<u8>)> =
				fs::read_dir(&merge_dir)
					.unwrap()
					.filter_map(|entry| {
						let entry = entry.unwrap();
						let is_file = entry
							.metadata()
							.is_ok_and(|meta| meta.is_file());
						if is_file {
							Some((
								entry
									.file_name()
									.to_string_lossy()
									.into_owned(),
								fs::read(entry.path()).unwrap(),
							))
						} else {
							None
						}
					})
					.collect();
			gathered.sort_by(|a, b| a.0.cmp(&b.0));
			let files: Vec<(&str, &[u8])> = gathered
				.iter()
				.map(|(name, bytes)| {
					(name.as_str(), bytes.as_slice())
				})
				.collect();
			RebaseState::from_files(&RebaseFiles { files: &files })
		} else {
			None
		};
		Snapshot {
			generation: Generation(0),
			head,
			branch,
			operation: if rebase.is_some() {
				Operation::RebaseInteractive
			} else {
				Operation::Clean
			},
			staged: entries.staged,
			worktree: entries.worktree,
			conflicts: entries.conflicts,
			untracked: entries.untracked,
			rebase,
			bridge: None,
		}
	}

	fn session_for(todo: &str) -> Session {
		running_session(Todo::parse(todo.as_bytes()).lines().to_vec())
	}

	#[test]
	fn evidence_conflict_stop() {
		let sb = Sandbox::new();
		let (dir, a1, a2) = linear_repo(&sb);
		// Replaying a2 before a1 conflicts: a2's diff (2→3)
		// cannot apply to f=1.
		let todo = format!("pick {a2}\npick {a1}\n");
		run_rebase(&sb, &dir, &todo, &[], "HEAD~2");

		let snapshot = observe(&sb, &dir);
		let policy = policy(&session_for(&todo), &snapshot);
		assert_eq!(
			policy.phase,
			Phase::Stopped(Stop::Conflicts {
				paths: vec![String::from("f.txt")]
			}),
			"git must stop with an unmerged f.txt"
		);
		assert_eq!(policy.inspector, Inspector::Stop);
		// The boundary row is the conflicted pick.
		assert_eq!(policy.rail[0].state, RowState::Failed);
		assert_eq!(policy.rail[1].state, RowState::Pending);
		// Continue is blocked by exactly those paths.
		assert_eq!(
			policy.actions.r#continue,
			Availability::Disabled(vec![Blocker::Conflicts(vec![
				String::from("f.txt")
			])])
		);
		assert!(policy.actions.abort.is_enabled());
	}

	#[test]
	fn evidence_resolved_conflict_is_continueable() {
		let sb = Sandbox::new();
		let (dir, a1, a2) = linear_repo(&sb);
		let todo = format!("pick {a2}\npick {a1}\n");
		run_rebase(&sb, &dir, &todo, &[], "HEAD~2");
		// Resolve and stage.
		fs::write(dir.path().join("f.txt"), "resolved\n").unwrap();
		sb.git(dir.path(), &["add", "f.txt"]);

		let snapshot = observe(&sb, &dir);
		let policy = policy(&session_for(&todo), &snapshot);
		assert_eq!(
			policy.phase,
			Phase::Stopped(Stop::Conflicts { paths: Vec::new() }),
			"stopped-sha lingers but the resolution is staged"
		);
		assert!(policy.actions.r#continue.is_enabled());
	}

	#[test]
	fn evidence_edit_stop() {
		let sb = Sandbox::new();
		let (dir, a1, a2) = linear_repo(&sb);
		let todo = format!("edit {a1}\npick {a2}\n");
		run_rebase(&sb, &dir, &todo, &[], "HEAD~2");

		let snapshot = observe(&sb, &dir);
		let policy = policy(&session_for(&todo), &snapshot);
		let a1_id = CommitId::from_str_unchecked(&a1).unwrap();
		assert_eq!(
			policy.phase,
			Phase::Stopped(Stop::Edit { commit: a1_id }),
			"the amend file names the commit under amendment"
		);
		// Nothing staged: amend blocked, continue unchanged
		// offered.
		assert_eq!(
			policy.actions.amend,
			Availability::Disabled(vec![Blocker::LocalChanges])
		);
		assert!(policy.actions.continue_unchanged.is_enabled());
		// The boundary row is the edit step.
		assert_eq!(policy.rail[0].state, RowState::Stop);
	}

	#[test]
	fn evidence_break_stop() {
		let sb = Sandbox::new();
		let (dir, a1, a2) = linear_repo(&sb);
		let todo = format!("pick {a1}\nbreak\npick {a2}\n");
		run_rebase(&sb, &dir, &todo, &[], "HEAD~2");

		let snapshot = observe(&sb, &dir);
		let policy = policy(&session_for(&todo), &snapshot);
		assert_eq!(
			policy.phase,
			Phase::Stopped(Stop::Break),
			"break moves to done with no stopped-sha"
		);
		assert_eq!(
			policy.rail.iter().map(|r| r.state).collect::<Vec<_>>(),
			vec![RowState::Done, RowState::Stop, RowState::Pending]
		);
		assert!(policy.actions.r#continue.is_enabled());
		assert!(policy.actions.continue_unchanged.is_enabled());
	}

	#[test]
	fn evidence_became_empty_stop() {
		let sb = Sandbox::new();
		// main: base(1) → x(2). Branch dup sits at base and
		// made the identical change c1: replaying c1 onto x
		// is clean but empty — git's `--empty=stop` stop.
		let dir = TempDir::new().unwrap();
		sb.git(dir.path(), &["init", "-q"]);
		fs::write(dir.path().join("f.txt"), "base\n").unwrap();
		sb.git(dir.path(), &["add", "f.txt"]);
		sb.git(dir.path(), &["commit", "-q", "-m", "base"]);
		fs::write(dir.path().join("f.txt"), "one\n").unwrap();
		sb.git(dir.path(), &["commit", "-q", "-a", "-m", "x"]);
		let x = rev(&sb, &dir, "HEAD");
		sb.git(
			dir.path(),
			&["checkout", "-q", "-b", "dup", "HEAD~1"],
		);
		fs::write(dir.path().join("f.txt"), "one\n").unwrap();
		sb.git(dir.path(), &["commit", "-q", "-a", "-m", "c1"]);
		let c1 = rev(&sb, &dir, "HEAD");

		let todo = format!("pick {c1}\n");
		run_rebase(&sb, &dir, &todo, &["--empty=stop"], &x);

		let snapshot = observe(&sb, &dir);
		let policy = policy(&session_for(&todo), &snapshot);
		// The empirically confirmed contract: stopped-sha is
		// present, conflicts are empty, nothing is staged.
		assert!(snapshot.conflicts.is_empty());
		assert!(snapshot.staged.is_empty());
		assert!(snapshot
			.rebase
			.as_ref()
			.unwrap()
			.stopped_sha
			.is_some());
		let c1_id = CommitId::from_str_unchecked(&c1).unwrap();
		assert_eq!(
			policy.phase,
			Phase::Stopped(Stop::BecameEmpty { commit: c1_id })
		);
		assert_eq!(
			policy.actions.r#continue,
			Availability::Disabled(vec![Blocker::EmptyDecision])
		);
		assert!(policy.actions.keep_empty.is_enabled());
		assert!(policy.actions.skip.is_enabled());
	}

	#[test]
	fn evidence_exec_failure() {
		let sb = Sandbox::new();
		let (dir, a1, a2) = linear_repo(&sb);
		let todo = format!("pick {a1}\nexec false\npick {a2}\n");
		// Default settings: git 2.55 writes
		// `no-reschedule-failed-exec`.
		run_rebase(&sb, &dir, &todo, &[], "HEAD~2");

		let snapshot = observe(&sb, &dir);
		let policy = policy(&session_for(&todo), &snapshot);
		// The task brief speculated the todo ends up empty and
		// the rebase over; real git stops with the exec as the
		// last executed instruction and the rest still queued.
		assert_eq!(
			policy.phase,
			Phase::Stopped(Stop::ExecFailed {
				command: String::from("false"),
				code: None,
			})
		);
		assert!(!snapshot
			.rebase
			.as_ref()
			.unwrap()
			.todo
			.lines()
			.is_empty());
		assert_eq!(
			policy.rail.iter().map(|r| r.state).collect::<Vec<_>>(),
			vec![RowState::Done, RowState::Failed, RowState::Pending]
		);
		assert!(policy.actions.r#continue.is_enabled());
		assert!(policy.actions.abort.is_enabled());
	}

	#[test]
	fn evidence_exec_failure_with_reschedule_flag() {
		// The flag changes what `--continue` does, not the
		// stopped state: classification must not care.
		let sb = Sandbox::new();
		let (dir, a1, a2) = linear_repo(&sb);
		let todo = format!("pick {a1}\nexec false\npick {a2}\n");
		run_rebase(
			&sb,
			&dir,
			&todo,
			&["--reschedule-failed-exec"],
			"HEAD~2",
		);
		let snapshot = observe(&sb, &dir);
		let policy = policy(&session_for(&todo), &snapshot);
		assert_eq!(
			policy.phase,
			Phase::Stopped(Stop::ExecFailed {
				command: String::from("false"),
				code: None,
			})
		);
	}

	#[test]
	fn evidence_completed_rebase() {
		let sb = Sandbox::new();
		let (dir, a1, a2) = linear_repo(&sb);
		let todo = format!("pick {a1}\npick {a2}\n");
		run_rebase(&sb, &dir, &todo, &[], "HEAD~2");

		let snapshot = observe(&sb, &dir);
		// The sequencer directory is gone; the repository is
		// clean again.
		assert!(snapshot.rebase.is_none());
		assert_eq!(snapshot.operation, Operation::Clean);
		// A session still marked Running means git finished
		// outside gitui: reconcile.
		let drifted = policy(&session_for(&todo), &snapshot);
		assert!(matches!(drifted.phase, Phase::Reconciling { .. }));
		// A session that recorded the finish is done.
		let tip =
			CommitId::from_str_unchecked(&rev(&sb, &dir, "HEAD"))
				.unwrap();
		let session = Session {
			phase: crate::rebase::journal::Phase::Finished(tip),
			..session_for(&todo)
		};
		let finished = policy(&session, &snapshot);
		assert_eq!(finished.phase, Phase::Done(Done::Finished));
		assert!(finished
			.rail
			.iter()
			.all(|r| matches!(r.state, RowState::Done)));
	}

	// ------------------------------------------------------------------
	// property-lite: random inputs never panic, invariants hold
	// ------------------------------------------------------------------

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

		fn chance(&mut self) -> bool {
			self.next().is_multiple_of(2)
		}
	}

	fn random_line(rng: &mut Rng, n: u64) -> TodoLine {
		match rng.below(5) {
			0 => pick_line(n),
			1 => fixup_line(n),
			2 => drop_line(n),
			3 => exec_line("make"),
			_ => break_line(),
		}
	}

	fn random_todo_bytes(rng: &mut Rng, lines: usize) -> String {
		let mut out = String::new();
		for _ in 0..lines {
			let n = 1 + rng.below(4) as u64;
			out.push_str("pick ");
			out.push_str(&hex(n));
			out.push_str(" # subject\n");
		}
		out
	}

	#[test]
	fn random_inputs_never_panic_and_respect_invariants() {
		let mut rng = Rng(0x5EED_F00D_0000_0042);
		for _case in 0..500 {
			// Random session.
			let lines: Vec<TodoLine> = (0..rng.below(5))
				.enumerate()
				.map(|(i, _)| random_line(&mut rng, i as u64 + 1))
				.collect();
			let mut session = match rng.below(4) {
				0 => Session::default(),
				1 => draft_session(lines.clone()),
				2 => running_session(lines.clone()),
				_ => Session {
					adopted: rng.chance(),
					..running_session(lines.clone())
				},
			};
			session.phase = match rng.below(5) {
				0 => crate::rebase::journal::Phase::Empty,
				1 => crate::rebase::journal::Phase::Draft,
				2 => crate::rebase::journal::Phase::Running,
				3 => crate::rebase::journal::Phase::Finished(oid(9)),
				_ => crate::rebase::journal::Phase::Aborted,
			};

			// Random snapshot: flags combined freely.
			let mut snapshot = snap();
			snapshot.operation = match rng.below(4) {
				0 => Operation::Clean,
				1 => Operation::RebaseInteractive,
				2 => Operation::Merge,
				_ => Operation::RebaseApply,
			};
			if rng.chance() {
				snapshot.conflicts =
					vec![conflict_of("f.txt"); rng.below(2)];
			}
			if rng.chance() {
				snapshot.staged =
					vec![change_of("f.txt"); rng.below(2)];
			}
			if rng.chance() {
				snapshot.worktree =
					vec![change_of("g.txt"); rng.below(2)];
			}
			if rng.chance() {
				snapshot = with_bridge(snapshot, "bridge/msg");
			}
			if rng.chance() && session.started.is_some() {
				let done_count = rng.below(3);
				let todo_count = rng.below(3);
				let done = random_todo_bytes(&mut rng, done_count);
				let todo = random_todo_bytes(&mut rng, todo_count);
				let mut extra: Vec<(&str, String)> = Vec::new();
				if rng.chance() {
					extra.push(("stopped-sha", hex(1)));
				}
				if rng.chance() {
					extra.push(("amend", hex(2)));
				}
				let raw: Vec<(&str, &str)> = extra
					.iter()
					.map(|(name, content)| (*name, content.as_str()))
					.collect();
				snapshot = with_rebase(
					snapshot,
					rebase_state(&done, &todo, &raw),
				);
			}

			let policy = policy(&session, &snapshot);

			// Pairwise invariants.
			assert!(
				!(policy.actions.start.is_enabled()
					&& policy.actions.r#continue.is_enabled()),
				"start and continue are never both enabled"
			);
			assert!(
				!(policy.actions.skip.is_enabled()
					&& policy.actions.save_message.is_enabled()),
				"skip and save_message are never both enabled"
			);
			assert!(
				!(policy.actions.amend.is_enabled()
					&& policy.actions.keep_empty.is_enabled()),
				"amend and keep_empty are never both enabled"
			);
			assert!(
				!matches!(policy.actions.close, Availability::Hidden),
				"close is offered (or explicitly blocked) in \
				 every phase"
			);
			// Progress never fabricates a total.
			if session.started.is_none() {
				assert_eq!(policy.progress.total, None);
			}
		}
	}
}
