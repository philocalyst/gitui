//! The user's program: a todo document with stable step ids.
//!
//! A [`Plan`] is git's todo list plus gitui's annotations (stable
//! ids, message overrides). It is what the user sees and edits in
//! the rebase workspace; compiling it back to bytes is what git
//! executes. The plan is deliberately *positional*: a fixup folds
//! into whatever output precedes it, exactly like git itself.
//! "Groups" in the UI are a derived view of consecutive
//! pick/squash/fixup runs, not a second data model.
//!
//! Plans are renderable even when invalid — validation attaches
//! issues to steps instead of rejecting the document, so an
//! imported or hand-mangled plan still opens, explains itself, and
//! refuses to execute until repaired.
//!
//! ## Persistence shape
//!
//! The todo bytes are the storage format. The journal stores the
//! serialized document plus the id of every line (and notes);
//! loading re-parses and zips ids back on. A mismatched id count
//! means a corrupted or foreign journal and lands in recovery,
//! never in silent data loss.

use crate::rebase::{
	todo::{CommitArg, Instruction, Todo, TodoLine},
	StepId,
};

/// An editable, validatable rebase program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
	steps: Vec<Step>,
	next_id: StepId,
}

impl Default for Plan {
	fn default() -> Self {
		Self {
			steps: Vec::new(),
			next_id: StepId::FIRST,
		}
	}
}

/// One line of the program with its stable identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
	/// Stable identity across moves and edits.
	pub id: StepId,
	/// The todo line itself.
	pub line: crate::rebase::todo::TodoLine,
	/// gitui-owned annotation.
	pub note: Note,
}

/// gitui annotation for a step (not part of git's todo grammar).
#[derive(
	Clone,
	Debug,
	Default,
	PartialEq,
	Eq,
	serde::Serialize,
	serde::Deserialize,
)]
pub struct Note {
	/// Message the user composed for a `reword`/`squash` step;
	/// replayed through the editor bridge at run time.
	pub message: Option<String>,
}

/// The action a step takes, for `SetAction` edits.
/// Mirrors the pick-family instructions.
#[derive(
	Clone,
	Copy,
	Debug,
	PartialEq,
	Eq,
	serde::Serialize,
	serde::Deserialize,
)]
pub enum Action {
	/// Apply the commit.
	Pick,
	/// Apply, reword via composed message.
	Reword,
	/// Apply, stop for amend/split.
	Edit,
	/// Fold in, meld messages.
	Squash,
	/// Fold in, keep previous message.
	Fixup,
	/// Fold in, keep this commit's message (`fixup -C`).
	FixupAmend,
	/// Fold in, edit this commit's message (`fixup -c`).
	FixupEdit,
	/// Omit the commit.
	Drop,
}

/// One user edit; also one undo unit and one journal event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
	/// Change what a step does.
	SetAction {
		/// The step to change.
		id: StepId,
		/// Its new action.
		action: Action,
	},
	/// Move a step (and nothing else) before another step.
	/// `None` target means: to the end.
	Move {
		/// The step to move.
		id: StepId,
		/// Move it before this step.
		before: Option<StepId>,
	},
	/// Insert a deliberate stop.
	InsertBreak {
		/// Insert before this step; `None` = end.
		before: Option<StepId>,
	},
	/// Insert a shell command step.
	InsertExec {
		/// The command, verbatim.
		command: String,
		/// Insert before this step; `None` = end.
		before: Option<StepId>,
	},
	/// Set the composed message of a reword/squash step.
	SetMessage {
		/// The step to annotate.
		id: StepId,
		/// The composed message.
		message: String,
	},
	/// Replace the whole program (raw todo editing round trip).
	ReplaceAll {
		/// The new document. Ids are reassigned where lines
		/// moved unchanged; fresh ids for new lines.
		todo: Todo,
	},
}

/// A validation issue attached as close to its cause as possible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
	/// The offending step, if the issue is positional.
	pub step: Option<StepId>,
	/// What is wrong.
	pub kind: Issue,
}

/// What can be wrong with a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Issue {
	/// A fold (squash/fixup) has no preceding pick to fold into:
	/// it is the first instruction.
	FoldWithoutTarget,
	/// A fold follows a non-commit step (break/exec/label/…),
	/// so there is no output to fold into.
	FoldAfterNonCommit,
	/// An `exec` step with an empty command.
	ExecEmpty,
	/// The document contains unknown syntax; execution is
	/// blocked until it is resolved or removed.
	UnknownSyntax,
	/// Every step is dropped; the result is the destination
	/// itself. Allowed, but must be review-approved.
	AllDropped,
	/// The program contains no steps at all.
	Empty,
}

impl Plan {
	/// Adopt a todo document as a new plan, assigning fresh ids.
	/// Every line of the document — comments, blanks and unknown
	/// syntax included — becomes a step, so positions survive.
	// Takes ownership to say "the document becomes the plan",
	// even though only its lines are read.
	#[allow(clippy::needless_pass_by_value)]
	#[must_use]
	pub fn from_todo(todo: Todo) -> Self {
		let mut next_id = StepId::FIRST;
		let steps = todo
			.lines()
			.iter()
			.map(|line| {
				let step = Step {
					id: next_id,
					line: line.clone(),
					note: Note::default(),
				};
				next_id = next_id.next();
				step
			})
			.collect();
		Self { steps, next_id }
	}

	/// Serialize to the bytes git will execute. Invalid plans
	/// do not compile; the diagnostics say why.
	///
	/// # Errors
	///
	/// Returns the full diagnostic list when the plan cannot
	/// safely execute.
	pub fn compile(&self) -> Result<Vec<u8>, Vec<Diagnostic>> {
		let diagnostics = self.validate();
		if diagnostics.is_empty() {
			Ok(self.to_todo().to_bytes())
		} else {
			Err(diagnostics)
		}
	}

	/// Validate without compiling. Diagnostics appear in
	/// document order, then the whole-document ones.
	#[must_use]
	pub fn validate(&self) -> Vec<Diagnostic> {
		let mut diagnostics = Vec::new();
		let mut fold_target = FoldTarget::Absent;
		let mut has_instruction = false;
		let mut has_pick_family = false;
		let mut all_dropped = true;
		for step in &self.steps {
			let Some(instruction) = step.line.instruction() else {
				// Comments and blanks are transparent; raw
				// lines are unknown program text and block
				// folds exactly like other non-commits.
				if matches!(step.line, TodoLine::Raw(_)) {
					diagnostics.push(Diagnostic {
						step: Some(step.id),
						kind: Issue::UnknownSyntax,
					});
					fold_target = FoldTarget::Blocked;
				}
				continue;
			};
			has_instruction = true;
			match instruction {
				Instruction::Pick { .. }
				| Instruction::Reword { .. }
				| Instruction::Edit { .. } => {
					has_pick_family = true;
					all_dropped = false;
					fold_target = FoldTarget::Ready;
				}
				Instruction::Squash { .. }
				| Instruction::Fixup { .. } => {
					has_pick_family = true;
					all_dropped = false;
					let kind = match fold_target {
						FoldTarget::Ready => None,
						FoldTarget::Absent => {
							Some(Issue::FoldWithoutTarget)
						}
						FoldTarget::Blocked => {
							Some(Issue::FoldAfterNonCommit)
						}
					};
					if let Some(kind) = kind {
						diagnostics.push(Diagnostic {
							step: Some(step.id),
							kind,
						});
					}
				}
				// A drop removes its commit but leaves whatever
				// output precedes it untouched, so it neither
				// opens a group nor blocks an open one.
				Instruction::Drop { .. } => {
					has_pick_family = true;
				}
				other => {
					fold_target = FoldTarget::Blocked;
					if let Instruction::Exec { command, .. } = other {
						if command.is_empty() {
							diagnostics.push(Diagnostic {
								step: Some(step.id),
								kind: Issue::ExecEmpty,
							});
						}
					}
				}
			}
		}
		if !has_instruction {
			diagnostics.push(Diagnostic {
				step: None,
				kind: Issue::Empty,
			});
		}
		if has_pick_family && all_dropped {
			diagnostics.push(Diagnostic {
				step: None,
				kind: Issue::AllDropped,
			});
		}
		diagnostics
	}

	/// Apply one edit, returning the new plan. Applying never
	/// validates: the result may be renderably invalid, and
	/// [`Plan::validate`] is the one place that reports that.
	///
	/// # Errors
	///
	/// * [`ApplyError::Stale`] — the edit names a step that
	///   does not exist.
	/// * [`ApplyError::NotACommitStep`] — [`Edit::SetAction`]
	///   targeted a step that replays no commit (exec, break,
	///   label, merge, …).
	/// * [`ApplyError::NotComposable`] — [`Edit::SetMessage`]
	///   targeted a step that takes no composed message
	///   (anything but `reword`/`squash`).
	pub fn apply(&self, edit: &Edit) -> Result<Self, ApplyError> {
		match edit {
			Edit::SetAction { id, action } => {
				self.set_action(*id, *action)
			}
			Edit::Move { id, before } => self.move_step(*id, *before),
			Edit::InsertBreak { before } => self.insert_instruction(
				*before,
				Instruction::break_instruction(),
			),
			Edit::InsertExec { command, before } => self
				.insert_instruction(
					*before,
					Instruction::exec(command.as_bytes())
						.ok_or(ApplyError::ExecEmpty)?,
				),
			Edit::SetMessage { id, message } => {
				self.set_message(*id, message)
			}
			Edit::ReplaceAll { todo } => Ok(self.replace_all(todo)),
		}
	}

	/// The steps, in program order.
	#[must_use]
	pub fn steps(&self) -> &[Step] {
		&self.steps
	}

	/// Look up one step by id.
	#[must_use]
	pub fn step(&self, id: StepId) -> Option<&Step> {
		self.steps.iter().find(|s| s.id == id)
	}

	/// The document without annotations (for diffing).
	#[must_use]
	pub fn to_todo(&self) -> Todo {
		Todo::from_lines(
			self.steps.iter().map(|s| s.line.clone()).collect(),
		)
	}

	/// Steps grouped into output commits: a group starts at a
	/// pick-family step that is not a fold and spans the folds
	/// that follow it. Non-commit steps stand alone.
	#[must_use]
	pub fn output_groups(&self) -> Vec<OutputGroup> {
		let mut groups: Vec<OutputGroup> = Vec::new();
		let mut open: Option<usize> = None;
		for step in &self.steps {
			let Some(instruction) = step.line.instruction() else {
				continue;
			};
			match instruction {
				Instruction::Pick { .. }
				| Instruction::Reword { .. }
				| Instruction::Edit { .. } => {
					open = Some(groups.len());
					groups.push(OutputGroup {
						members: vec![step.id],
						produces_output: true,
					});
				}
				Instruction::Squash { .. }
				| Instruction::Fixup { .. } => match open {
					Some(group) => {
						groups[group].members.push(step.id);
					}
					None => groups.push(OutputGroup {
						members: vec![step.id],
						produces_output: false,
					}),
				},
				// A drop is its own row but does not close the
				// group: later folds still fold across it.
				Instruction::Drop { .. } => {
					groups.push(OutputGroup {
						members: vec![step.id],
						produces_output: false,
					});
				}
				Instruction::Break { .. }
				| Instruction::Exec { .. }
				| Instruction::Label { .. }
				| Instruction::Reset { .. }
				| Instruction::Merge { .. }
				| Instruction::UpdateRef { .. }
				| Instruction::Noop { .. } => {
					open = None;
					groups.push(OutputGroup {
						members: vec![step.id],
						produces_output: false,
					});
				}
			}
		}
		groups
	}

	/// Index of the step with this id, if any.
	fn position(&self, id: StepId) -> Option<usize> {
		self.steps.iter().position(|s| s.id == id)
	}

	/// Where a `before`-targeted insertion goes.
	fn insert_index(
		&self,
		before: Option<StepId>,
	) -> Result<usize, ApplyError> {
		before.map_or(Ok(self.steps.len()), |target| {
			self.position(target)
				.ok_or(ApplyError::Stale(StaleId(target)))
		})
	}

	fn set_action(
		&self,
		id: StepId,
		action: Action,
	) -> Result<Self, ApplyError> {
		let index = self
			.position(id)
			.ok_or(ApplyError::Stale(StaleId(id)))?;
		let Some((commit, after)) =
			replay_target(self.steps[index].line.instruction())
		else {
			return Err(ApplyError::NotACommitStep(id));
		};
		let mut plan = self.clone();
		plan.steps[index].line = TodoLine::Instruction(
			Instruction::replay(action, commit, after),
		);
		Ok(plan)
	}

	fn move_step(
		&self,
		id: StepId,
		before: Option<StepId>,
	) -> Result<Self, ApplyError> {
		let from = self
			.position(id)
			.ok_or(ApplyError::Stale(StaleId(id)))?;
		let mut steps = self.steps.clone();
		let step = steps.remove(from);
		let target = match before {
			None => steps.len(),
			Some(target) if target == id => from,
			Some(target) => {
				let mut index = self
					.position(target)
					.ok_or(ApplyError::Stale(StaleId(target)))?;
				if index > from {
					index -= 1;
				}
				index
			}
		};
		steps.insert(target, step);
		Ok(Self {
			steps,
			next_id: self.next_id,
		})
	}

	fn insert_instruction(
		&self,
		before: Option<StepId>,
		instruction: Instruction,
	) -> Result<Self, ApplyError> {
		let index = self.insert_index(before)?;
		let mut plan = self.clone();
		let id = plan.next_id;
		plan.next_id = id.next();
		plan.steps.insert(
			index,
			Step {
				id,
				line: TodoLine::Instruction(instruction),
				note: Note::default(),
			},
		);
		Ok(plan)
	}

	fn set_message(
		&self,
		id: StepId,
		message: &str,
	) -> Result<Self, ApplyError> {
		let index = self
			.position(id)
			.ok_or(ApplyError::Stale(StaleId(id)))?;
		let composable = matches!(
			self.steps[index].line.instruction(),
			Some(
				Instruction::Reword { .. }
					| Instruction::Squash { .. }
			)
		);
		if !composable {
			return Err(ApplyError::NotComposable(id));
		}
		let mut plan = self.clone();
		plan.steps[index].note.message = Some(message.to_owned());
		Ok(plan)
	}

	fn replace_all(&self, todo: &Todo) -> Self {
		let lines = todo.lines();
		let inherited = align(&self.steps, lines);
		let mut next_id = self.next_id;
		let steps = lines
			.iter()
			.enumerate()
			.map(|(index, line)| {
				inherited[index].map_or_else(
					|| {
						let id = next_id;
						next_id = id.next();
						Step {
							id,
							line: line.clone(),
							note: Note::default(),
						}
					},
					|old| Step {
						id: self.steps[old].id,
						line: line.clone(),
						note: self.steps[old].note.clone(),
					},
				)
			})
			.collect();
		Self { steps, next_id }
	}

	/// The journal's persistence shape (see the module docs):
	/// the todo document bytes, the stable id of every line in
	/// order, and the non-default notes.
	#[must_use]
	pub fn to_stored(&self) -> StoredPlan {
		StoredPlan {
			todo: self.to_todo().to_bytes(),
			ids: self
				.steps
				.iter()
				.map(|step| step.id.value())
				.collect(),
			notes: self
				.steps
				.iter()
				.filter(|step| step.note != Note::default())
				.map(|step| (step.id.value(), step.note.clone()))
				.collect(),
		}
	}

	/// Rebuild a plan from stored data: re-parse the todo
	/// bytes and zip the ids back on by line index.
	///
	/// # Errors
	///
	/// [`Corrupt`] when the stored data cannot be trusted:
	/// the id count does not match the document's line count,
	/// or the reserved zero id appears. Mismatch is treated as
	/// corruption, never papered over with fresh ids.
	pub fn from_stored(stored: &StoredPlan) -> Result<Self, Corrupt> {
		let todo = Todo::parse(&stored.todo);
		let lines = todo.lines();
		if lines.len() != stored.ids.len() {
			return Err(Corrupt::IdCount {
				lines: lines.len(),
				ids: stored.ids.len(),
			});
		}
		let mut steps = Vec::with_capacity(lines.len());
		let mut next_id = StepId::FIRST;
		for (line, raw) in lines.iter().zip(stored.ids.iter()) {
			let id =
				StepId::from_value(*raw).ok_or(Corrupt::ZeroId)?;
			let note = stored
				.notes
				.iter()
				.find(|(stored_id, _)| *stored_id == *raw)
				.map_or_else(Note::default, |(_, note)| note.clone());
			steps.push(Step {
				id,
				line: line.clone(),
				note,
			});
			if id >= next_id {
				next_id = id.next();
			}
		}
		Ok(Self { steps, next_id })
	}
}

/// The journal's persistence shape for a [`Plan`] (see the
/// `plan` module docs). Serde mirror for the journal file.
#[derive(
	Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
pub struct StoredPlan {
	/// The todo document bytes (the storage format).
	pub todo: Vec<u8>,
	/// The stable id of every line, in document order.
	pub ids: Vec<u32>,
	/// Annotations, only for steps whose note is not the
	/// default: `(step id, note)`.
	pub notes: Vec<(u32, Note)>,
}

/// Why stored plan data could not be trusted. Loading a
/// corrupted plan lands the session in recovery; nothing is
/// silently repaired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corrupt {
	/// The stored id count does not match the count of lines
	/// re-parsed from the todo bytes.
	IdCount {
		/// Lines re-parsed from the stored todo bytes.
		lines: usize,
		/// Ids stored alongside them.
		ids: usize,
	},
	/// The reserved zero id appeared in stored data.
	ZeroId,
}

/// A derived view: which steps produce one output commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputGroup {
	/// The steps, in order; the first is the group head.
	pub members: Vec<StepId>,
	/// Whether this group produces a commit at all
	/// (drops and non-commit steps do not).
	pub produces_output: bool,
}

/// The edit referred to a step that no longer exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaleId(pub StepId);

/// Why an edit could not be applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyError {
	/// The edit named a step that does not exist.
	Stale(StaleId),
	/// [`Edit::SetAction`] targeted a step that replays no
	/// commit (exec, break, label, merge, …).
	NotACommitStep(StepId),
	/// [`Edit::SetMessage`] targeted a step that takes no
	/// composed message (anything but `reword`/`squash`).
	NotComposable(StepId),
	/// [`Edit::InsertExec`] carried an empty command, which
	/// git refuses to run.
	ExecEmpty,
}

/// Whether a following fold has an output to fold into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FoldTarget {
	/// The nearest preceding step produces output.
	Ready,
	/// Nothing precedes this point.
	Absent,
	/// A non-commit instruction intervenes.
	Blocked,
}

/// The commit argument and the exact bytes that followed it
/// in the source (comment included), if this is a
/// pick-family step at all. Both carry over verbatim when an
/// edit replaces the instruction.
fn replay_target(
	instruction: Option<&Instruction>,
) -> Option<(CommitArg, Box<[u8]>)> {
	let instruction = instruction?;
	if !instruction.is_pick_family() {
		return None;
	}
	let after = match instruction {
		Instruction::Pick { after, .. }
		| Instruction::Reword { after, .. }
		| Instruction::Edit { after, .. }
		| Instruction::Squash { after, .. }
		| Instruction::Fixup { after, .. }
		| Instruction::Drop { after, .. } => after.clone(),
		_ => return None,
	};
	Some((instruction.commit_arg()?.clone(), after))
}

/// Canonical identity of a line, for id preservation across
/// [`Edit::ReplaceAll`]: instructions compare by their parsed
/// value, every other line by its bytes.
#[derive(Clone, PartialEq, Eq)]
enum LineKey {
	Instruction(Instruction),
	Comment(Box<[u8]>),
	Blank,
	Raw(Box<[u8]>),
}

impl LineKey {
	fn of(line: &TodoLine) -> Self {
		match line {
			TodoLine::Instruction(instruction) => {
				Self::Instruction(instruction.clone())
			}
			TodoLine::Comment(bytes) => Self::Comment(bytes.clone()),
			TodoLine::Blank => Self::Blank,
			TodoLine::Raw(bytes) => Self::Raw(bytes.clone()),
		}
	}
}

/// Longest-common-subsequence alignment between the old steps
/// and the new lines: for each new line, the index of the old
/// step of identical content it inherits identity from, if any.
/// Unmatched new lines get fresh ids; removed lines simply vanish.
fn align(old: &[Step], new: &[TodoLine]) -> Vec<Option<usize>> {
	let old_keys: Vec<LineKey> =
		old.iter().map(|s| LineKey::of(&s.line)).collect();
	let new_keys: Vec<LineKey> =
		new.iter().map(LineKey::of).collect();
	let mut table = vec![vec![0u32; new.len() + 1]; old.len() + 1];
	for i in (0..old.len()).rev() {
		for j in (0..new.len()).rev() {
			table[i][j] = if old_keys[i] == new_keys[j] {
				table[i + 1][j + 1] + 1
			} else {
				table[i][j + 1].max(table[i + 1][j])
			};
		}
	}
	let mut inherited = vec![None; new.len()];
	let (mut i, mut j) = (0, 0);
	while i < old.len() && j < new.len() {
		if old_keys[i] == new_keys[j] {
			inherited[j] = Some(i);
			i += 1;
			j += 1;
		} else if table[i][j + 1] >= table[i + 1][j] {
			j += 1;
		} else {
			i += 1;
		}
	}
	inherited
}

#[cfg(test)]
mod tests {
	#![allow(clippy::unwrap_used)]

	use super::*;
	use crate::rebase::todo::{CommitArg, FixupMessage};
	use crate::sync::CommitId;

	fn oid(n: u64) -> CommitId {
		CommitId::from_str_unchecked(&format!("{n:040x}")).unwrap()
	}

	fn boxed(bytes: &[u8]) -> Box<[u8]> {
		bytes.to_vec().into_boxed_slice()
	}

	fn arg(n: u64) -> CommitArg {
		CommitArg::from_id(oid(n))
	}

	fn pick(n: u64) -> TodoLine {
		TodoLine::Instruction(Instruction::Pick {
			word: boxed(b"pick"),
			commit: arg(n),
			after: boxed(b"# s"),
		})
	}

	fn reword(n: u64) -> TodoLine {
		TodoLine::Instruction(Instruction::Reword {
			word: boxed(b"reword"),
			commit: arg(n),
			after: b"".as_slice().into(),
		})
	}

	fn squash(n: u64) -> TodoLine {
		TodoLine::Instruction(Instruction::Squash {
			word: boxed(b"squash"),
			commit: arg(n),
			after: b"".as_slice().into(),
		})
	}

	fn fixup(n: u64) -> TodoLine {
		TodoLine::Instruction(Instruction::Fixup {
			word: boxed(b"fixup"),
			message: FixupMessage::Previous,
			commit: arg(n),
			after: b"".as_slice().into(),
		})
	}

	fn drop_line(n: u64) -> TodoLine {
		TodoLine::Instruction(Instruction::Drop {
			word: boxed(b"drop"),
			commit: arg(n),
			after: b"".as_slice().into(),
		})
	}

	fn exec(command: &str) -> TodoLine {
		TodoLine::Instruction(
			Instruction::exec(command.as_bytes()).unwrap(),
		)
	}

	fn comment(text: &str) -> TodoLine {
		TodoLine::Comment(boxed(text.as_bytes()))
	}

	fn raw() -> TodoLine {
		TodoLine::Raw(boxed(b"future git syntax"))
	}

	fn break_line() -> TodoLine {
		TodoLine::Instruction(Instruction::break_instruction())
	}

	fn ids(plan: &Plan) -> Vec<StepId> {
		plan.steps().iter().map(|s| s.id).collect()
	}

	/// Invariants every plan must hold, checked after every
	/// random edit in the state-machine test.
	fn check_invariants(plan: &Plan) {
		let mut seen = std::collections::HashSet::new();
		for step in plan.steps() {
			assert!(
				seen.insert(step.id),
				"duplicate step id {:?}",
				step.id
			);
			assert!(
				step.id.value() < plan.next_id.value(),
				"step id {:?} not below next_id",
				step.id
			);
		}
		// compile agrees with validate exactly
		let diagnostics = plan.validate();
		match plan.compile() {
			Ok(bytes) => {
				assert!(
					diagnostics.is_empty(),
					"compiled with diagnostics: {diagnostics:?}"
				);
				// compiling is round-trippable through parse
				let reparsed = Todo::parse(&bytes);
				assert_eq!(reparsed.to_bytes(), bytes);
			}
			Err(returned) => {
				assert_eq!(
					returned, diagnostics,
					"compile diagnostics diverge"
				);
			}
		}
		// output groups partition every instruction step
		let grouped: usize = plan
			.output_groups()
			.iter()
			.map(|g| g.members.len())
			.sum();
		let instructions = plan
			.steps()
			.iter()
			.filter(|s| s.line.instruction().is_some())
			.count();
		assert_eq!(
			grouped, instructions,
			"groups do not partition the steps"
		);
	}

	fn plan(lines: Vec<TodoLine>) -> Plan {
		Plan::from_todo(Todo::from_lines(lines))
	}

	#[test]
	fn from_todo_ids_every_line() {
		let p = plan(vec![
			pick(1),
			comment("# note"),
			TodoLine::Blank,
			raw(),
			fixup(2),
		]);
		assert_eq!(
			ids(&p),
			vec![
				StepId::FIRST,
				StepId::FIRST.next(),
				StepId::FIRST.next().next(),
				StepId::FIRST.next().next().next(),
				StepId::FIRST.next().next().next().next(),
			]
		);
		assert_eq!(p.steps().len(), 5);
	}

	#[test]
	fn compile_round_trips_document_bytes() {
		let bytes: &[u8] = b"pick 1111111111111111111111111111111111111111 # first\n\
			# a comment\n\
			\n\
			fixup 2222222222222222222222222222222222222222\n\
			exec make test\n\
			break\n";
		let p = Plan::from_todo(Todo::parse(bytes));
		assert_eq!(p.steps().len(), 6);
		assert_eq!(p.compile().unwrap(), bytes.to_vec());
	}

	#[test]
	fn to_todo_reassembles_the_document() {
		let lines =
			vec![pick(1), comment("# x"), TodoLine::Blank, fixup(2)];
		let p = plan(lines.clone());
		assert_eq!(p.to_todo(), Todo::from_lines(lines));
	}

	#[test]
	fn set_action_rewrites_the_instruction_canonically() {
		let p = plan(vec![pick(1), fixup(2)]);
		let id = p.steps()[1].id;
		let out = p
			.apply(&Edit::SetAction {
				id,
				action: Action::FixupAmend,
			})
			.unwrap();
		assert_eq!(out.steps()[1].id, id);
		assert_eq!(
			out.steps()[1].line.instruction(),
			Some(&Instruction::Fixup {
				word: boxed(b"fixup"),
				message: FixupMessage::ThisCommit,
				commit: arg(2),
				after: b"".as_slice().into(),
			})
		);
		let out = p
			.apply(&Edit::SetAction {
				id,
				action: Action::FixupEdit,
			})
			.unwrap();
		assert_eq!(
			out.steps()[1].line.instruction(),
			Some(&Instruction::Fixup {
				word: boxed(b"fixup"),
				message: FixupMessage::EditThisCommit,
				commit: arg(2),
				after: b"".as_slice().into(),
			})
		);
	}

	#[test]
	fn set_action_preserves_commit_argument_form() {
		// An abbreviated argument survives an action change
		// byte-for-byte; the plan never expands or corrupts
		// what it did not write.
		let todo = Todo::parse(b"p a1b2c3d4 # keep\r\n");
		let p = Plan::from_todo(todo);
		let id = p.steps()[0].id;
		let out = p
			.apply(&Edit::SetAction {
				id,
				action: Action::Drop,
			})
			.unwrap();
		let Instruction::Drop { commit, after, .. } =
			out.steps()[0].line.instruction().unwrap()
		else {
			panic!("not a drop");
		};
		assert_eq!(commit.as_bytes(), b"a1b2c3d4");
		assert_eq!(after.as_ref(), b" # keep\r");
	}

	#[test]
	fn set_action_rejects_non_commit_steps() {
		let p = plan(vec![exec("true")]);
		let id = p.steps()[0].id;
		assert_eq!(
			p.apply(&Edit::SetAction {
				id,
				action: Action::Pick
			}),
			Err(ApplyError::NotACommitStep(id))
		);
		assert_eq!(
			p.apply(&Edit::SetAction {
				id: StepId::FIRST.next(),
				action: Action::Pick
			}),
			Err(ApplyError::Stale(StaleId(StepId::FIRST.next())))
		);
	}

	#[test]
	fn move_step_repositions_and_keeps_ids() {
		let p = plan(vec![pick(1), pick(2), pick(3)]);
		let original = ids(&p);
		let out = p
			.apply(&Edit::Move {
				id: original[0],
				before: Some(original[2]),
			})
			.unwrap();
		assert_eq!(
			ids(&out),
			vec![original[1], original[0], original[2]]
		);
		let out = out
			.apply(&Edit::Move {
				id: original[1],
				before: None,
			})
			.unwrap();
		assert_eq!(
			ids(&out),
			vec![original[0], original[2], original[1]]
		);
	}

	#[test]
	fn move_before_self_is_a_no_op() {
		let p = plan(vec![pick(1), pick(2)]);
		let original = ids(&p);
		let out = p
			.apply(&Edit::Move {
				id: original[0],
				before: Some(original[0]),
			})
			.unwrap();
		assert_eq!(ids(&out), vec![original[0], original[1]]);
	}

	#[test]
	fn move_with_stale_target_fails() {
		let p = plan(vec![pick(1)]);
		let missing = StepId::FIRST.next();
		assert_eq!(
			p.apply(&Edit::Move {
				id: StepId::FIRST,
				before: Some(missing),
			}),
			Err(ApplyError::Stale(StaleId(missing)))
		);
	}

	#[test]
	fn insert_break_and_exec_get_fresh_ids() {
		let p = plan(vec![pick(1)]);
		let head = p.steps()[0].id;
		let fresh = p.next_id;
		let out = p
			.apply(&Edit::InsertBreak { before: Some(head) })
			.unwrap();
		assert_eq!(ids(&out), vec![fresh, head]);
		assert_eq!(
			out.steps()[0].line.instruction(),
			Some(&Instruction::break_instruction())
		);
		assert_eq!(out.next_id, fresh.next());
		let out = out
			.apply(&Edit::InsertExec {
				command: "make test".into(),
				before: None,
			})
			.unwrap();
		assert_eq!(ids(&out), vec![fresh, head, fresh.next()]);
		assert_eq!(
			out.steps()[2].line.instruction(),
			exec("make test").instruction()
		);
		// Empty exec is refused like git refuses it
		assert_eq!(
			out.apply(&Edit::InsertExec {
				command: String::new(),
				before: None,
			}),
			Err(ApplyError::ExecEmpty)
		);
	}

	#[test]
	fn set_message_targets_reword_and_squash_only() {
		let p = plan(vec![pick(1), reword(2), squash(3)]);
		let original = ids(&p);
		let out = p
			.apply(&Edit::SetMessage {
				id: original[1],
				message: "better".into(),
			})
			.unwrap();
		assert_eq!(
			out.steps()[1].note.message.as_deref(),
			Some("better")
		);
		assert_eq!(
			p.apply(&Edit::SetMessage {
				id: original[0],
				message: "nope".into(),
			}),
			Err(ApplyError::NotComposable(original[0]))
		);
	}

	#[test]
	fn replace_all_keeps_ids_of_unchanged_lines() {
		let p = plan(vec![pick(1), reword(2), exec("make")]);
		let original = ids(&p);
		let out = p
			.apply(&Edit::SetMessage {
				id: original[1],
				message: "kept".into(),
			})
			.unwrap();
		let fresh = out.next_id;
		let out = out
			.apply(&Edit::ReplaceAll {
				todo: Todo::from_lines(vec![
					comment("# new"),
					reword(2),
					pick(3),
					exec("make"),
				]),
			})
			.unwrap();
		assert_eq!(out.steps()[1].id, original[1]);
		assert_eq!(
			out.steps()[1].note.message.as_deref(),
			Some("kept")
		);
		assert_eq!(out.steps()[3].id, original[2]);
		assert_eq!(out.steps()[0].id, fresh);
		assert_eq!(out.steps()[2].id, fresh.next());
		assert_eq!(out.next_id, fresh.next().next());
	}

	#[test]
	fn replace_all_follows_reordered_lines() {
		let p = plan(vec![pick(1), pick(2)]);
		let original = ids(&p);
		let out = p
			.apply(&Edit::ReplaceAll {
				todo: Todo::from_lines(vec![pick(2), pick(1)]),
			})
			.unwrap();
		// A pure swap admits no order-preserving alignment of
		// length 2: the earliest match is anchored, the
		// displaced twin is re-minted.
		assert_eq!(ids(&out), vec![out.steps()[0].id, original[0]]);
		assert_ne!(out.steps()[0].id, original[1]);
	}

	#[test]
	fn validation_matrix() {
		let cases = vec![
			(
				plan(vec![
					pick(1),
					fixup(2),
					squash(3),
					comment("# note"),
					TodoLine::Blank,
					reword(4),
					drop_line(5),
					exec("make test"),
					break_line(),
				]),
				Vec::new(),
			),
			(
				plan(vec![
					comment("# hi"),
					TodoLine::Blank,
					squash(1),
				]),
				vec![Issue::FoldWithoutTarget],
			),
			(
				plan(vec![drop_line(1), fixup(2)]),
				vec![Issue::FoldWithoutTarget],
			),
			(
				plan(vec![pick(1), exec("true"), fixup(2)]),
				vec![Issue::FoldAfterNonCommit],
			),
			(
				plan(vec![pick(1), break_line(), fixup(2)]),
				vec![Issue::FoldAfterNonCommit],
			),
			(
				plan(vec![pick(1), raw(), fixup(2)]),
				vec![Issue::UnknownSyntax, Issue::FoldAfterNonCommit],
			),
			(plan(vec![pick(1), raw()]), vec![Issue::UnknownSyntax]),
			(plan(vec![pick(1), drop_line(2), fixup(3)]), Vec::new()),
			(plan(vec![pick(1), squash(2), fixup(3)]), Vec::new()),
			(
				plan(vec![drop_line(1), drop_line(2)]),
				vec![Issue::AllDropped],
			),
			(plan(vec![drop_line(1), pick(2)]), Vec::new()),
			(plan(vec![comment("# only")]), vec![Issue::Empty]),
			(Plan::default(), vec![Issue::Empty]),
		];
		for (p, kinds) in cases {
			let diagnostics = p.validate();
			let found: Vec<Issue> =
				diagnostics.iter().map(|d| d.kind).collect();
			assert_eq!(found, kinds, "plan {kinds:?}");
		}
	}

	#[test]
	fn compile_blocks_on_each_issue() {
		let cases = vec![
			(plan(vec![squash(1)]), Issue::FoldWithoutTarget),
			(
				plan(vec![pick(1), exec("true"), squash(2)]),
				Issue::FoldAfterNonCommit,
			),
			(plan(vec![pick(1), raw()]), Issue::UnknownSyntax),
			(
				plan(vec![drop_line(1), drop_line(2)]),
				Issue::AllDropped,
			),
			(plan(vec![comment("# only")]), Issue::Empty),
			(Plan::default(), Issue::Empty),
		];
		for (p, kind) in cases {
			let diagnostics = p.compile().unwrap_err();
			assert!(
				diagnostics.iter().any(|d| d.kind == kind),
				"expected {kind:?} in {diagnostics:?}"
			);
		}
	}

	#[test]
	fn output_groups_shape() {
		let p = plan(vec![
			pick(1),
			fixup(2),
			squash(3),
			reword(4),
			drop_line(5),
			exec("make"),
		]);
		let original = ids(&p);
		assert_eq!(
			p.output_groups(),
			vec![
				OutputGroup {
					members: vec![
						original[0],
						original[1],
						original[2]
					],
					produces_output: true,
				},
				OutputGroup {
					members: vec![original[3]],
					produces_output: true,
				},
				OutputGroup {
					members: vec![original[4]],
					produces_output: false,
				},
				OutputGroup {
					members: vec![original[5]],
					produces_output: false,
				},
			]
		);
	}

	#[test]
	fn output_groups_skip_comments_and_cross_drops() {
		let p = plan(vec![
			pick(1),
			comment("# x"),
			drop_line(2),
			fixup(3),
			TodoLine::Blank,
		]);
		let original = ids(&p);
		assert_eq!(
			p.output_groups(),
			vec![
				OutputGroup {
					members: vec![original[0], original[3]],
					produces_output: true,
				},
				OutputGroup {
					members: vec![original[2]],
					produces_output: false,
				},
			]
		);
	}

	#[test]
	fn stored_round_trips_documents_and_notes() {
		// Built from parsed bytes: plans adopt documents, so
		// the lines are canonical and byte-stable.
		let bytes: &[u8] =
			b"pick 1000000000000000000000000000000000000001 # one\n\
			# x\n\
			reword 2000000000000000000000000000000000000002 # two\n\
			\n";
		let p = Plan::from_todo(Todo::parse(bytes));
		let reword_id = p.steps()[2].id;
		let p = p
			.apply(&Edit::SetMessage {
				id: reword_id,
				message: "hello".into(),
			})
			.unwrap();
		let stored = p.to_stored();
		assert_eq!(
			stored.ids,
			ids(&p).iter().map(|id| id.value()).collect::<Vec<_>>()
		);
		// Only the non-default note is stored.
		assert_eq!(
			stored.notes,
			vec![(
				reword_id.value(),
				Note {
					message: Some(String::from("hello"))
				}
			)]
		);
		// The document bytes survive byte-exactly.
		assert_eq!(stored.todo, p.to_todo().to_bytes());
		assert_eq!(Plan::from_stored(&stored).unwrap(), p);
	}

	#[test]
	fn stored_notes_default_when_absent() {
		let p = Plan::from_todo(Todo::parse(
			b"pick 1000000000000000000000000000000000000001 # one\n",
		));
		let mut stored = p.to_stored();
		assert!(stored.notes.is_empty());
		// A lost note list is not corruption: steps simply
		// have no annotation.
		stored.notes.clear();
		assert_eq!(Plan::from_stored(&stored).unwrap(), p);
	}

	#[test]
	fn stored_rejects_mismatched_id_counts() {
		let todo = Todo::from_lines(vec![pick(1)]);
		let stored = StoredPlan {
			todo: todo.to_bytes(),
			ids: vec![1, 2],
			notes: Vec::new(),
		};
		assert_eq!(
			Plan::from_stored(&stored),
			Err(Corrupt::IdCount { lines: 1, ids: 2 })
		);
	}

	#[test]
	fn stored_rejects_the_zero_id() {
		let todo = Todo::from_lines(vec![pick(1)]);
		let stored = StoredPlan {
			todo: todo.to_bytes(),
			ids: vec![0],
			notes: Vec::new(),
		};
		assert_eq!(Plan::from_stored(&stored), Err(Corrupt::ZeroId));
	}

	/// Seeded generator for the state-machine test.
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

	/// A live step id chosen at random (`default` for empty
	/// plans, where every edit is expected to fail politely).
	fn some_id(plan: &Plan, rng: &mut Rng) -> StepId {
		plan.steps()
			.get(rng.below(plan.steps().len().max(1)))
			.map_or(StepId::FIRST, |s| s.id)
	}

	/// One random edit against a plan, as the UI would issue
	/// them (targets picked from live ids, some stale on
	/// purpose).
	fn random_edit(plan: &Plan, rng: &mut Rng) -> Edit {
		const ACTIONS: [Action; 8] = [
			Action::Pick,
			Action::Reword,
			Action::Edit,
			Action::Squash,
			Action::Fixup,
			Action::FixupAmend,
			Action::FixupEdit,
			Action::Drop,
		];
		match rng.below(8) {
			0 => Edit::SetAction {
				id: some_id(plan, rng),
				action: ACTIONS[rng.below(ACTIONS.len())],
			},
			1 => Edit::Move {
				id: some_id(plan, rng),
				before: (rng.below(3) > 0)
					.then(|| some_id(plan, rng)),
			},
			2 => Edit::InsertBreak {
				before: (rng.below(2) > 0)
					.then(|| some_id(plan, rng)),
			},
			3 => Edit::InsertExec {
				command: format!("cmd {}", rng.below(100)),
				before: (rng.below(2) > 0)
					.then(|| some_id(plan, rng)),
			},
			4 => Edit::SetMessage {
				id: some_id(plan, rng),
				message: format!("m{}", rng.below(100)),
			},
			5 => {
				let mut lines = Vec::new();
				for i in 0..rng.below(8) {
					lines.push(match i % 3 {
						0 => comment("# c"),
						1 => pick(i as u64 + rng.next() % 50),
						_ => fixup(i as u64 + rng.next() % 50),
					});
				}
				Edit::ReplaceAll {
					todo: Todo::from_lines(lines),
				}
			}
			_ => Edit::Move {
				id: some_id(plan, rng),
				before: None,
			},
		}
	}

	#[test]
	fn random_edit_sequences_hold_invariants() {
		let mut rng = Rng(0xAB1E_5EED_0000_0007);
		for _case in 0..40 {
			// A fresh semi-random starting document.
			let lines = (0..rng.below(12))
				.map(|i| match rng.below(4) {
					0 => pick(i as u64),
					1 => fixup(i as u64),
					2 => comment("# c"),
					_ => exec("true"),
				})
				.collect();
			let mut plan = plan(lines);
			let mut history: Vec<(Plan, Edit)> = Vec::new();
			for _edit in 0..60 {
				let edit = random_edit(&plan, &mut rng);
				match plan.apply(&edit) {
					Ok(next) => {
						history.push((plan, edit));
						plan = next;
					}
					// Rejected edits (stale/non-commit) must
					// leave the plan unchanged.
					Err(_) => {}
				}
				check_invariants(&plan);
			}
			// Undo by replay: every prefix of the applied
			// edits must be re-derivable — fold the edits
			// onto the starting plan again and land on the
			// same current plan.
			let mut replay = history.first().map_or_else(
				|| plan.clone(),
				|(start, _)| start.clone(),
			);
			for (_, edit) in &history {
				if let Ok(next) = replay.apply(edit) {
					replay = next;
				}
			}
			assert_eq!(
				replay.steps(),
				plan.steps(),
				"replaying the edit history diverged"
			);
		}
	}
}
