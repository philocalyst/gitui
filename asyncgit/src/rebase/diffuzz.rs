//! The differential fuzzer for the rebase subsystem.
//!
//! Hardware-grade randomized testing, Dan Luu style: generate a
//! random repository and a random rebase plan, run the plan
//! against **real** git through the real [`NativeEngine`], and
//! verify deep invariants against an independent oracle. Hand
//! tests are for sanity; the bulk of bug-finding comes from
//! constrained random generation checking invariants.
//!
//! ## Repositories
//!
//! [`random_repo`] makes 3..=8 commits on `main`, each adding
//! one *disjoint* file — replay is conflict-free by
//! construction, which is what makes an exact oracle possible.
//! With `conflicting`, it additionally grows a `side` branch
//! from the rebase base whose commits rewrite `shared.txt`
//! (also touched by a random in-range main commit): replaying
//! onto `side` then conflicts at exactly that commit, and only
//! structural assertions apply.
//!
//! ## Plans
//!
//! [`random_plan`] always starts from git's canonical todo
//! (captured through a real `Cmd::GenerateTodo`, exercising the
//! capture path) and applies 0..=6 random [`Edit`]s. Plans that
//! do not compile are skipped: invalid plans are plan.rs's
//! tests' business.
//!
//! ## Message policy
//!
//! Reword/squash/`fixup -c` steps open git's message editor
//! mid-replay. The harness answers every such request with the
//! fixed [`FUZZ_MESSAGE`] — via the prewritten bridge path on
//! `Continue`, and via a request watcher (the UI's seat) during
//! `Start`. Git commits the editor content verbatim, so the
//! resulting subject is exactly [`FUZZ_MESSAGE`]; that makes
//! even folded groups oracle-checkable (verified against real
//! git in the engine tests).
//!
//! ## Oracle (conflict-free repositories — exact)
//!
//! 1. Simulate the program in Rust ([`expected_subjects`]):
//!    walk the compiled plan, fold squashes/fixups into groups,
//!    derive each output group's subject from git's documented
//!    fold semantics plus the fixed message policy.
//! 2. `git log --format=%s <base>..HEAD` must equal that
//!    sequence exactly (subjects, not oids: rebased commits get
//!    fresh committer dates).
//! 3. File bytes: each `f{i}.txt` holds its writing commit's
//!    content and exists unless that commit is dropped; dropped
//!    commits' files must be absent.
//! 4. `git fsck --no-dangling` must be clean.
//! 5. The recovery ref must still point at the pre-start tip
//!    with every fixture commit as ancestor.
//!
//! ## Oracle (conflicting repositories — structural)
//!
//! Start stops with conflicts on `shared.txt` only; after
//! deterministic ours-resolution the rebase completes with
//! clean fsck and the expected number of output commits; a
//! fresh Start + `Cmd::Abort` restores the original tip
//! exactly with an empty snapshot.
//!
//! Every failure message carries the seed, so any red run is
//! reproducible: `Rng(<printed seed>)`.

#![allow(clippy::unwrap_used, clippy::panic)]

use crate::rebase::engine::{
	Base, Cmd, Engine, NativeEngine, StartOptions,
};
use crate::rebase::plan::{Action, Edit, Plan};
use crate::rebase::snapshot::{Operation, RebaseState, Snapshot};
use crate::rebase::testutil::{start_opts, two_step_branch, Sandbox};
use crate::rebase::todo::{FixupMessage, Instruction, Todo};
use crate::rebase::StepId;
use crate::sync::CommitId;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The one composed message the harness ever answers a bridge
/// message request with. Git commits the editor content
/// verbatim, so every editor-shaped subject is exactly this.
const FUZZ_MESSAGE: &str = "fuzz-composed";

/// The recovery ref the harness creates before every start.
const RECOVERY_REF: &str = "refs/gitui/fuzz/original";

/// The recovery ref of the abort variant.
const ABORT_REF: &str = "refs/gitui/fuzz/abort";

/// The one shared file conflicting repositories are allowed to
/// conflict on.
const SHARED_FILE: &str = "shared.txt";

/// Upper bound on run-loop rounds; exceeding it means the
/// replay stopped making progress (a reschedule bug).
const MAX_ROUNDS: usize = 40;

/// First seed of the case ladder; each case uses
/// `SEED_BASE + case`. Documented per test.
const SEED_BASE: u64 = 0xD1FF_5EED;

/// Seeded generator, the sibling test modules' LCG verbatim so
/// failures are reproducible from the printed seed.
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
		#[allow(clippy::cast_possible_truncation)]
		{
			(self.next() % bound.max(1) as u64) as usize
		}
	}
}

/// One main-line commit of the generated repository.
struct FuzzCommit {
	oid: String,
	subject: String,
	file: String,
	content: String,
}

impl FuzzCommit {
	fn id(&self) -> CommitId {
		CommitId::from_str_unchecked(&self.oid).unwrap()
	}
}

/// The generated repository, recorded for the oracle.
struct Fixture {
	/// All main-line commits, oldest first.
	commits: Vec<FuzzCommit>,
	/// The rebase upstream: `commits[base_index]`, exclusive.
	base_index: usize,
	/// The in-range commit that also writes `shared.txt`
	/// (conflicting repositories only).
	conflict_index: Option<usize>,
	/// Number of commits on the diverged `side` branch.
	side_count: usize,
	/// Branch tip before any rebase.
	orig_tip: String,
	/// Tip of the diverged `side` branch.
	side_tip: Option<String>,
}

impl Fixture {
	fn conflicting(&self) -> bool {
		self.side_tip.is_some()
	}

	/// The rebase scope: everything after the base commit,
	/// onto `side`'s tip in the conflicting variant.
	fn scope(&self) -> Base {
		let upstream = self.commits[self.base_index].id();
		self.side_tip.as_ref().map_or(
			Base::Upstream(upstream),
			|side| Base::Onto {
				upstream,
				onto: CommitId::from_str_unchecked(side).unwrap(),
			},
		)
	}

	/// Index of the fixture commit an instruction names (the
	/// generated todo carries abbreviated oids, which the plan
	/// keeps verbatim).
	fn index_of(&self, instruction: &Instruction) -> usize {
		let arg = instruction.commit_arg().unwrap_or_else(|| {
			panic!("no commit argument on {instruction:?}")
		});
		self.commits
			.iter()
			.position(|c| arg.names(&c.id()))
			.unwrap_or_else(|| {
				panic!("todo names unknown commit {arg:?}")
			})
	}
}

/// Build the random repository (see the module docs).
fn random_repo(
	rng: &mut Rng,
	s: &Sandbox,
	conflicting: bool,
) -> Fixture {
	let n = 3 + rng.below(6); // 3..=8 commits
						   // At least one commit stays in the replay range.
	let base_index = rng.below(n - 1);
	let conflict_index = if conflicting {
		Some(base_index + 1 + rng.below(n - 1 - base_index))
	} else {
		None
	};
	let mut commits = Vec::new();
	for i in 0..n {
		let content = format!("line-{i}\n");
		s.write(&format!("f{i}.txt"), &content);
		if i == 0 && conflicting {
			// Planted by the base commit so both sides have
			// a common ancestor version to diverge from.
			s.write(SHARED_FILE, "shared-v0\n");
		}
		if Some(i) == conflict_index {
			s.write(SHARED_FILE, &format!("main-{i}\n"));
		}
		let oid = s.commit(&format!("c{i}"));
		commits.push(FuzzCommit {
			oid,
			subject: format!("c{i}"),
			file: format!("f{i}.txt"),
			content,
		});
	}
	let orig_tip = commits.last().unwrap().oid.clone();
	let mut fixture = Fixture {
		commits,
		base_index,
		conflict_index,
		side_count: 0,
		orig_tip,
		side_tip: None,
	};
	if conflicting {
		// Branch from the rebase base itself, so `side`
		// lacks every in-range change: replaying the range
		// onto `side` must conflict at `conflict_index`.
		s.git_ok(&[
			"checkout",
			"-q",
			"-b",
			"side",
			&fixture.commits[base_index].oid,
		]);
		let side_count = 1 + rng.below(2);
		for k in 0..side_count {
			s.write(SHARED_FILE, &format!("side-{k}\n"));
			fixture.side_tip = Some(s.commit(&format!("s{k}")));
		}
		s.git_ok(&["checkout", "-q", "main"]);
		fixture.side_count = side_count;
	}
	fixture
}

/// One random plan edit. `Edit` (a replay stop) appears with
/// low probability and is handled at run time; a `false` exec
/// likewise exercises the failure path.
fn random_edit(rng: &mut Rng, plan: &Plan) -> Edit {
	let ids: Vec<StepId> =
		plan.steps().iter().map(|s| s.id).collect();
	let action = if rng.below(10) == 0 {
		Action::Edit
	} else {
		[
			Action::Pick,
			Action::Reword,
			Action::Squash,
			Action::Fixup,
			Action::FixupAmend,
			Action::FixupEdit,
			Action::Drop,
		][rng.below(7)]
	};
	match rng.below(10) {
		0..=4 => Edit::SetAction {
			id: pick_id(rng, &ids),
			action,
		},
		5..=7 => Edit::Move {
			id: pick_id(rng, &ids),
			before: pick_before(rng, &ids),
		},
		8 => Edit::InsertBreak {
			before: pick_before(rng, &ids),
		},
		_ => Edit::InsertExec {
			command: String::from(if rng.below(4) == 0 {
				"false"
			} else {
				"true"
			}),
			before: pick_before(rng, &ids),
		},
	}
}

/// A live step id drawn at random.
fn pick_id(rng: &mut Rng, ids: &[StepId]) -> StepId {
	ids[rng.below(ids.len())]
}

/// A random insertion point: `None` = the end.
fn pick_before(rng: &mut Rng, ids: &[StepId]) -> Option<StepId> {
	(rng.below(3) != 0).then(|| pick_id(rng, ids))
}

/// Draw a random plan from git's canonical todo (a real
/// `Cmd::GenerateTodo` run, exercising the capture path).
/// `None` when the compiled plan is invalid: invalid plans are
/// plan.rs's tests' business, so the caller skips the draw.
fn random_plan(
	rng: &mut Rng,
	engine: &NativeEngine,
	fixture: &Fixture,
) -> Option<(Plan, Vec<u8>)> {
	let out = engine
		.exec(Cmd::GenerateTodo {
			base: fixture.scope(),
			rebase_merges: false,
		})
		.unwrap();
	assert_eq!(
		out.code, 1,
		"capture mode must abort the start cleanly"
	);
	let mut plan = Plan::from_todo(out.generated_todo.unwrap());
	for _ in 0..rng.below(7) {
		let edit = random_edit(rng, &plan);
		if let Ok(next) = plan.apply(&edit) {
			plan = next;
		}
	}
	let compiled = plan.compile().ok()?;
	Some((plan, compiled))
}

/// [`random_plan`] with bounded retries so a rejected draw
/// skips the draw, not the case; a systematic rejection is a
/// loud failure instead.
fn compilable_plan(
	rng: &mut Rng,
	engine: &NativeEngine,
	fixture: &Fixture,
	seed: u64,
	case: usize,
	acceptable: &dyn Fn(&Plan) -> bool,
) -> (Plan, Vec<u8>) {
	for _ in 0..20 {
		if let Some(found) = random_plan(rng, engine, fixture) {
			if acceptable(&found.0) {
				return found;
			}
		}
	}
	panic!("seed {seed} case {case}: no compilable plan in 20 draws");
}

/// Whether the plan keeps the conflict-writing commit alive:
/// dropping it would make the "conflicting" fixture replay
/// cleanly and the structural oracle meaningless.
fn keeps_conflict(plan: &Plan, fixture: &Fixture) -> bool {
	let k = fixture.conflict_index.unwrap();
	!plan.steps().iter().any(|step| {
		step.line.instruction().is_some_and(|instruction| {
			matches!(instruction, Instruction::Drop { .. })
				&& fixture.index_of(instruction) == k
		})
	})
}

/// Whether the program can open a message editor mid-replay.
/// Those steps need the request watcher while they run inside
/// `Start`, whose bridge carries no prewritten message.
fn opens_editor(todo: &[u8]) -> bool {
	Todo::parse(todo).instructions().any(|(_, i)| {
		matches!(
			i,
			Instruction::Reword { .. }
				| Instruction::Squash { .. }
				| Instruction::Fixup {
					message: FixupMessage::EditThisCommit,
					..
				}
		)
	})
}

/// Answers mid-replay bridge message requests while an engine
/// command is in flight — the UI's seat at the table. Requests
/// only appear during `Start` (the `Continue` bridge carries a
/// prewritten message and answers immediately).
struct Watcher {
	stop: Arc<AtomicBool>,
	handle: std::thread::JoinHandle<()>,
}

impl Watcher {
	fn spawn(engine: Arc<NativeEngine>) -> Self {
		let stop = Arc::new(AtomicBool::new(false));
		let flag = Arc::clone(&stop);
		let handle = std::thread::spawn(move || {
			let request =
				engine.session_dir().join("bridge").join("request");
			let mut answered = 0;
			while !flag.load(Ordering::SeqCst) && answered < 16 {
				if request.exists() {
					// Racing the bridge teardown is
					// fine: a vanished bridge means
					// nobody is waiting anymore.
					let _ = engine.exec(Cmd::SetBridgeMessage {
						message: String::from(FUZZ_MESSAGE),
					});
					answered += 1;
				}
				std::thread::sleep(Duration::from_millis(25));
			}
		});
		Self { stop, handle }
	}

	fn stop(self) {
		self.stop.store(true, Ordering::SeqCst);
		let _ = self.handle.join();
	}
}

/// Resolve every conflicted path with OURS (stage 2) content —
/// the harness's deterministic resolution policy. A missing
/// ours stage means ours deleted the path: resolve by removal.
fn resolve_conflicts(
	s: &Sandbox,
	snapshot: &Snapshot,
	allowed: &[&str],
	seed: u64,
	case: usize,
) {
	for conflict in &snapshot.conflicts {
		assert!(
			allowed.contains(&conflict.path.as_str()),
			"seed {seed} case {case}: unexpected conflict \
			 path {:?} (allowed: {allowed:?})",
			conflict.path,
		);
		let ours = s.git(&["show", &format!(":2:{}", conflict.path)]);
		if ours.status.success() {
			fs::write(s.path().join(&conflict.path), &ours.stdout)
				.unwrap();
			s.git_ok(&["add", &conflict.path]);
		} else {
			s.git_ok(&["rm", "-f", "--", &conflict.path]);
		}
	}
}

/// Subject of the commit a stop is about (edit stops amend it).
fn stopped_subject(s: &Sandbox, sha: Option<CommitId>) -> String {
	let sha = sha
		.unwrap_or_else(|| panic!("edit stop without a stopped sha"));
	String::from_utf8_lossy(
		&s.git_ok(&["log", "-1", "--format=%s", &sha.to_string()])
			.stdout,
	)
	.trim()
	.to_owned()
}

/// Whether the rebase stopped because an `exec` failed: the
/// last executed instruction is an exec (observed with git
/// 2.55: no `stopped-sha` is written for exec stops).
fn last_done_is_exec(rebase: &RebaseState) -> bool {
	rebase
		.done
		.lines()
		.iter()
		.rev()
		.find_map(|line| line.instruction())
		.is_some_and(|i| matches!(i, Instruction::Exec { .. }))
}

/// Drive one compiled plan to a terminal state — always
/// through the engine, never shelling out to
/// `git rebase --continue` directly. Every stop is classified
/// and answered deterministically (see [`handle_stop`]).
fn run_to_completion(
	engine: &Arc<NativeEngine>,
	sandbox: &Sandbox,
	todo: &[u8],
	options: &StartOptions,
	allowed_conflicts: &[&str],
	seed: u64,
	case: usize,
) -> Snapshot {
	let watcher = (opens_editor(todo))
		.then(|| Watcher::spawn(Arc::clone(engine)));
	let mut started = false;
	let mut rounds = 0;
	let snapshot = loop {
		rounds += 1;
		assert!(
			rounds <= MAX_ROUNDS,
			"seed {seed} case {case}: run loop exceeded \
			 {MAX_ROUNDS} rounds — reschedule bug",
		);
		let outcome = if started {
			engine
				.exec(Cmd::Continue {
					// The prewritten bridge path
					// answers any message editor
					// git opens along the way.
					message: Some(String::from(FUZZ_MESSAGE)),
				})
				.unwrap()
		} else {
			started = true;
			engine
				.exec(Cmd::Start {
					todo: todo.to_vec(),
					options: options.clone(),
				})
				.unwrap()
		};
		if handle_stop(
			engine,
			sandbox,
			&outcome.snapshot,
			allowed_conflicts,
			seed,
			case,
		) {
			break outcome.snapshot;
		}
	};
	if let Some(watcher) = watcher {
		watcher.stop();
	}
	snapshot
}

/// Classify one observed stop and answer it deterministically.
/// True when the rebase reached its terminal clean state;
/// false when an answer was issued and the loop must run the
/// next command.
fn handle_stop(
	engine: &Arc<NativeEngine>,
	sandbox: &Sandbox,
	snapshot: &Snapshot,
	allowed_conflicts: &[&str],
	seed: u64,
	case: usize,
) -> bool {
	if snapshot.operation == Operation::Clean {
		return true;
	}
	if !snapshot.conflicts.is_empty() {
		resolve_conflicts(
			sandbox,
			snapshot,
			allowed_conflicts,
			seed,
			case,
		);
		return false;
	}
	let rebase = snapshot.rebase.clone().unwrap_or_else(|| {
		panic!(
			"seed {seed} case {case}: repository stopped \
			 without rebase state: {snapshot:?}"
		)
	});
	if snapshot.bridge.is_some() {
		// A request survived to an observation (watcher
		// lost the race, or none is running): answer it
		// deterministically.
		let _ = engine.exec(Cmd::SetBridgeMessage {
			message: String::from(FUZZ_MESSAGE),
		});
		return false;
	}
	if rebase.amend.is_some() {
		// Edit stop: amend, then resume.
		let subject = stopped_subject(sandbox, rebase.stopped_sha);
		engine
			.exec(Cmd::Commit {
				message: format!("{subject}-amended\n"),
				amend: true,
				allow_empty: false,
			})
			.unwrap();
		return false;
	}
	if rebase.stopped_sha.is_some() || last_done_is_exec(&rebase) {
		// A commit became empty, or an exec failed: skip
		// the current step and move on.
		engine.exec(Cmd::Skip).unwrap();
		return false;
	}
	// A deliberate `break`: the loop's plain continue.
	false
}

/// A group being folded while walking the program.
struct OpenGroup {
	/// Subject the group's head produces.
	head: String,
	/// A squash or `fixup -c` is in the group: git opens the
	/// message editor when the group completes and the
	/// harness answers with [`FUZZ_MESSAGE`], which becomes
	/// the whole commit message (verified against real git:
	/// even a later `fixup -C` does not suppress the editor).
	editor: bool,
	/// A `fixup -C` replaces the accumulated message with the
	/// folded commit's subject.
	amended: Option<String>,
}

/// Emit the open group's expected subject.
fn flush_group(open: &mut Option<OpenGroup>, out: &mut Vec<String>) {
	if let Some(group) = open.take() {
		let subject = if group.editor {
			String::from(FUZZ_MESSAGE)
		} else if let Some(amended) = group.amended {
			amended
		} else {
			group.head
		};
		out.push(subject);
	}
}

/// Oracle step 1: the expected subject sequence, in replay
/// order — an independent Rust simulation of the compiled
/// program, mirroring git's fold semantics plus the harness's
/// fixed message policy.
fn expected_subjects(plan: &Plan, fixture: &Fixture) -> Vec<String> {
	let mut out = Vec::new();
	let mut open: Option<OpenGroup> = None;
	for step in plan.steps() {
		let Some(instruction) = step.line.instruction() else {
			continue;
		};
		match instruction {
			Instruction::Pick { .. }
			| Instruction::Reword { .. }
			| Instruction::Edit { .. } => {
				flush_group(&mut open, &mut out);
				let commit =
					&fixture.commits[fixture.index_of(instruction)];
				let head = match instruction {
					Instruction::Reword { .. } => {
						String::from(FUZZ_MESSAGE)
					}
					Instruction::Edit { .. } => {
						format!("{}-amended", commit.subject)
					}
					_ => commit.subject.clone(),
				};
				open = Some(OpenGroup {
					head,
					editor: false,
					amended: None,
				});
			}
			Instruction::Squash { .. } => {
				open.as_mut()
					.unwrap_or_else(|| {
						panic!(
							"fold without target in a \
						 compiled plan — plan.rs bug"
						)
					})
					.editor = true;
			}
			Instruction::Fixup { message, .. } => {
				let group = open.as_mut().unwrap_or_else(|| {
					panic!(
						"fold without target in a \
							 compiled plan — plan.rs bug"
					)
				});
				match message {
					FixupMessage::Previous => {}
					FixupMessage::ThisCommit => {
						let commit = &fixture.commits
							[fixture.index_of(instruction)];
						group.amended = Some(commit.subject.clone());
					}
					FixupMessage::EditThisCommit => {
						group.editor = true;
					}
				}
			}
			// A drop neither opens nor closes a group:
			// later folds still fold across it.
			Instruction::Drop { .. } => {}
			// Non-commit steps end the open group.
			_ => flush_group(&mut open, &mut out),
		}
	}
	flush_group(&mut open, &mut out);
	out
}

/// Oracle step 3: expected worktree files. Disjoint files mean
/// each file's content is decided by its single writing
/// commit: present with exact bytes unless that commit is
/// dropped, absent when it is. Returns the expected files and
/// the paths that must not exist.
fn expected_files(
	plan: &Plan,
	fixture: &Fixture,
) -> (Vec<(String, String)>, Vec<String>) {
	let dropped: HashSet<usize> = plan
		.steps()
		.iter()
		.filter_map(|step| {
			let instruction = step.line.instruction()?;
			matches!(instruction, Instruction::Drop { .. })
				.then(|| fixture.index_of(instruction))
		})
		.collect();
	let mut present = Vec::new();
	let mut absent = Vec::new();
	for (i, commit) in fixture.commits.iter().enumerate() {
		if i > fixture.base_index && dropped.contains(&i) {
			absent.push(commit.file.clone());
		} else {
			present
				.push((commit.file.clone(), commit.content.clone()));
		}
	}
	(present, absent)
}

/// Oracle step 4.
fn assert_fsck_clean(s: &Sandbox, seed: u64, case: usize) {
	let out = s.git(&["fsck", "--no-dangling"]);
	assert!(
		out.status.success(),
		"seed {seed} case {case}: git fsck failed: {}",
		String::from_utf8_lossy(&out.stderr)
	);
}

/// Oracle step 5: the recovery ref still pins the pre-start
/// tip and every fixture commit is its ancestor.
fn assert_recovery_ref(
	s: &Sandbox,
	fixture: &Fixture,
	reference: &str,
	seed: u64,
	case: usize,
) {
	assert_eq!(
		s.ref_value(reference).as_deref(),
		Some(fixture.orig_tip.as_str()),
		"seed {seed} case {case}: {reference} must pin the \
		 pre-start tip",
	);
	for commit in &fixture.commits {
		let ok = s.git(&[
			"merge-base",
			"--is-ancestor",
			&commit.oid,
			reference,
		]);
		assert!(
			ok.status.success(),
			"seed {seed} case {case}: commit {} is not an \
			 ancestor of {reference}",
			commit.oid,
		);
	}
}

/// Keep the harness's own files (engine session directories,
/// the bridge shim, fault-injection scripts — all living inside
/// the worktree) out of status output, so snapshot assertions
/// can demand a fully clean worktree.
fn exclude_harness_files(s: &Sandbox) {
	let mut exclude = fs::OpenOptions::new()
		.append(true)
		.create(true)
		.open(s.path().join(".git/info/exclude"))
		.unwrap();
	for pattern in
		["gitui-session*", "bridge-shim.sh", "sleep-editor.sh"]
	{
		writeln!(exclude, "{pattern}").unwrap();
	}
}

/// Create the recovery ref through the engine, before Start.
fn create_ref(
	engine: &Arc<NativeEngine>,
	reference: &str,
	target: &str,
	seed: u64,
	case: usize,
) {
	let out = engine
		.exec(Cmd::CreateRef {
			reference: String::from(reference),
			target: CommitId::from_str_unchecked(target).unwrap(),
		})
		.unwrap();
	assert_eq!(
		out.code, 0,
		"seed {seed} case {case}: creating {reference} failed"
	);
}

/// One full conflict-free case: random repo, random plan, run
/// to completion, exact oracle.
fn conflict_free_case(seed: u64, case: usize) {
	let s = Sandbox::new();
	exclude_harness_files(&s);
	let engine = Arc::new(s.engine());
	let mut rng = Rng(seed);
	let fixture = random_repo(&mut rng, &s, false);
	let (plan, compiled) = compilable_plan(
		&mut rng,
		&engine,
		&fixture,
		seed,
		case,
		&|_| true,
	);
	let options = start_opts(fixture.scope());
	create_ref(&engine, RECOVERY_REF, &fixture.orig_tip, seed, case);

	let snapshot = run_to_completion(
		&engine,
		&s,
		&compiled,
		&options,
		&[],
		seed,
		case,
	);
	assert_eq!(
		snapshot.operation,
		Operation::Clean,
		"seed {seed} case {case}: rebase must end clean"
	);
	assert!(
		snapshot.rebase.is_none(),
		"seed {seed} case {case}: rebase state must be gone"
	);

	// Oracle step 1+2: exact subject sequence.
	let expected = expected_subjects(&plan, &fixture);
	let base = &fixture.commits[fixture.base_index].oid;
	// `git log` lists newest first; the oracle is in replay
	// order.
	let mut actual = s.log_subjects(&format!("{base}..HEAD"));
	actual.reverse();
	assert_eq!(
		actual, expected,
		"seed {seed} case {case}: subject sequence mismatch"
	);

	// Oracle step 3: exact file bytes; dropped files absent.
	let (present, absent) = expected_files(&plan, &fixture);
	for (file, content) in &present {
		assert_eq!(
			s.read(file),
			*content,
			"seed {seed} case {case}: wrong bytes in {file}"
		);
	}
	for file in &absent {
		assert!(
			!s.exists(file),
			"seed {seed} case {case}: dropped commit's file \
			 {file} must not exist"
		);
	}

	assert_fsck_clean(&s, seed, case);
	assert_recovery_ref(&s, &fixture, RECOVERY_REF, seed, case);
}

/// One full conflicting case plus its abort variant. The
/// ours-resolution policy decides `shared.txt`'s final bytes,
/// so only structural assertions apply.
fn conflicting_case(seed: u64, case: usize) {
	let s = Sandbox::new();
	exclude_harness_files(&s);
	let engine = Arc::new(s.engine());
	let mut rng = Rng(seed);
	let fixture = random_repo(&mut rng, &s, true);
	assert!(
		fixture.conflicting(),
		"seed {seed} case {case}: fixture must be conflicting"
	);
	let (plan, compiled) = compilable_plan(
		&mut rng,
		&engine,
		&fixture,
		seed,
		case,
		&|plan: &Plan| keeps_conflict(plan, &fixture),
	);
	let options = start_opts(fixture.scope());
	create_ref(&engine, RECOVERY_REF, &fixture.orig_tip, seed, case);

	// Completed run, structural oracle.
	let snapshot = run_to_completion(
		&engine,
		&s,
		&compiled,
		&options,
		&[SHARED_FILE],
		seed,
		case,
	);
	assert_eq!(
		snapshot.operation,
		Operation::Clean,
		"seed {seed} case {case}: rebase must end clean"
	);
	assert_fsck_clean(&s, seed, case);
	assert_ne!(
		s.head(),
		fixture.orig_tip,
		"seed {seed} case {case}: replayed tip must differ \
		 from the original",
	);
	let expected_len =
		expected_subjects(&plan, &fixture).len() + fixture.side_count;
	let count = String::from_utf8_lossy(
		&s.git_ok(&[
			"rev-list",
			"--count",
			&format!(
				"{}..HEAD",
				fixture.commits[fixture.base_index].oid
			),
		])
		.stdout,
	)
	.trim()
	.to_owned();
	assert_eq!(
		count,
		expected_len.to_string(),
		"seed {seed} case {case}: history length must equal \
		 the non-dropped output group count (plus the side \
		 branch commits)",
	);

	abort_variant(
		&engine, &s, &fixture, compiled, options, seed, case,
	);
}

/// N conflict-free random repositories and plans, replayed
/// to completion and checked against the exact oracle.
///
/// Seeds: `0xD1FF_5EED + case` for `case in 0..12`.
#[test]
fn random_conflict_free_plans_match_oracle() {
	for case in 0..12 {
		conflict_free_case(SEED_BASE + case as u64, case);
	}
}

/// The abort variant: same fixture, branch reset to the
/// pre-start tip, fresh recovery ref, fresh start. Abort
/// must restore the original tip exactly, with a clean
/// snapshot.
fn abort_variant(
	engine: &Arc<NativeEngine>,
	s: &Sandbox,
	fixture: &Fixture,
	compiled: Vec<u8>,
	options: StartOptions,
	seed: u64,
	case: usize,
) {
	s.git_ok(&["reset", "--hard", &fixture.orig_tip]);
	create_ref(engine, ABORT_REF, &fixture.orig_tip, seed, case);
	let started = engine
		.exec(Cmd::Start {
			todo: compiled,
			options,
		})
		.unwrap();
	assert!(
		!started.snapshot.conflicts.is_empty(),
		"seed {seed} case {case}: start must stop with conflicts",
	);
	assert!(
		started
			.snapshot
			.conflicts
			.iter()
			.all(|c| c.path == SHARED_FILE),
		"seed {seed} case {case}: conflicts must be on \
		 {SHARED_FILE} only, got {:?}",
		started.snapshot.conflicts,
	);
	let k = fixture.conflict_index.unwrap();
	assert_eq!(
		started.snapshot.rebase.as_ref().and_then(|r| r.stopped_sha),
		Some(fixture.commits[k].id()),
		"seed {seed} case {case}: the conflict must stop at c{k}",
	);
	let aborted = engine.exec(Cmd::Abort).unwrap();
	assert_eq!(
		aborted.code, 0,
		"seed {seed} case {case}: abort must succeed",
	);
	assert_eq!(
		aborted.snapshot.operation,
		Operation::Clean,
		"seed {seed} case {case}: abort must end clean",
	);
	assert_eq!(
		s.head(),
		fixture.orig_tip,
		"seed {seed} case {case}: abort must restore the tip \
		 exactly",
	);
	assert!(
		aborted.snapshot.worktree.is_empty()
			&& aborted.snapshot.staged.is_empty()
			&& aborted.snapshot.untracked.is_empty(),
		"seed {seed} case {case}: abort must leave a clean \
		 snapshot: {aborted:?}",
	);
}

/// N conflicting random repositories: complete after
/// deterministic ours-resolution (structural oracle), then a
/// fresh start and abort must restore the original tip
/// exactly with a clean snapshot.
///
/// Seeds: `0xD1FF_5EED + case` for `case in 0..6`.
#[test]
fn random_conflicting_plans_complete_and_abort_recovers() {
	for case in 0..6 {
		conflicting_case(SEED_BASE + case as u64, case);
	}
}

/// The two fault-injection scenarios: a gitui death after a
/// conflicting stop, and a kill mid-start.
#[test]
fn crash_recovery_observes_and_aborts() {
	crash_after_stop_is_observable_and_abortable();
	kill_during_start_is_observable_and_abortable();
}

/// Scenario 1: run a conflicting Start through the engine,
/// then simulate gitui dying — a fresh engine over the same
/// repository (fresh session directory) must observe the
/// intact rebase state and abort it back to the original tip.
/// Seed `0xD1FF_5EED + 40` (documented for reproducibility).
fn crash_after_stop_is_observable_and_abortable() {
	let seed = SEED_BASE + 40;
	let case = 40;
	let s = Sandbox::new();
	exclude_harness_files(&s);
	let engine = Arc::new(s.engine());
	let mut rng = Rng(seed);
	let fixture = random_repo(&mut rng, &s, true);
	// The unedited canonical todo replays every commit, so
	// the conflict at `conflict_index` is guaranteed.
	let todo = engine
		.exec(Cmd::GenerateTodo {
			base: fixture.scope(),
			rebase_merges: false,
		})
		.unwrap()
		.generated_todo
		.unwrap();
	let compiled = Plan::from_todo(todo).compile().unwrap();
	let options = start_opts(fixture.scope());
	let tip_before = s.head();

	let started = engine
		.exec(Cmd::Start {
			todo: compiled,
			options,
		})
		.unwrap();
	assert!(
		!started.snapshot.conflicts.is_empty(),
		"seed {seed} case {case}: start must conflict",
	);

	// gitui dies here. A fresh engine takes over.
	let fresh = Arc::new(s.engine_at("gitui-session-crash"));
	let observed = fresh.observe().unwrap_or_else(|e| {
		panic!("seed {seed} case {case}: observe after crash failed: {e}")
	});
	assert_eq!(
		observed.operation,
		Operation::RebaseInteractive,
		"seed {seed} case {case}: rebase state must be \
		 intact after the crash",
	);
	let rebase = observed.rebase.clone().unwrap_or_else(|| {
		panic!(
			"seed {seed} case {case}: rebase state lost after \
			 the crash"
		)
	});
	assert!(
		rebase.stopped_sha.is_some(),
		"seed {seed} case {case}: stopped sha must survive the \
		 crash",
	);
	assert!(
		!observed.conflicts.is_empty(),
		"seed {seed} case {case}: conflicts must survive the \
		 crash",
	);

	let aborted = fresh.exec(Cmd::Abort).unwrap();
	assert_eq!(
		aborted.code, 0,
		"seed {seed} case {case}: abort must succeed",
	);
	assert_eq!(
		aborted.snapshot.operation,
		Operation::Clean,
		"seed {seed} case {case}: abort must end clean",
	);
	assert_eq!(
		s.head(),
		tip_before,
		"seed {seed} case {case}: abort through the fresh \
		 engine must restore the tip",
	);
}

/// Scenario 2: git killed mid-`rebase -i` while blocked in a
/// sleeping sequence editor.
///
/// Observed with git 2.55: SIGKILL while the sequence editor
/// runs leaves `rebase-merge/` with `onto`, `orig-head`,
/// `head-name`, the todos, `gpg_sign_opt` and the `interactive`
/// marker — but *no* `end`, `msgnum` or `done`: the editor
/// runs before progress files are written. `RebaseState::
/// from_files` therefore still parses the state; observation
/// must succeed and `git rebase --abort` must recover cleanly
/// with the tip untouched.
fn kill_during_start_is_observable_and_abortable() {
	let s = Sandbox::new();
	exclude_harness_files(&s);
	let (a, _b, _c) = two_step_branch(&s);
	let tip_before = s.head();

	// A sequence editor that never returns.
	let editor = s.path().join("sleep-editor.sh");
	fs::write(&editor, "#!/bin/sh\nsleep 5\n").unwrap();
	fs::set_permissions(&editor, fs::Permissions::from_mode(0o755))
		.unwrap();
	let mut child = Command::new("git")
		.args(["rebase", "-i", "--empty=stop", &a])
		.current_dir(s.path())
		.envs(
			s.envs().iter().map(|(k, v)| (k.as_str(), v.as_os_str())),
		)
		.env_remove("GIT_EDITOR")
		.env_remove("GIT_SEQUENCE_EDITOR")
		.env_remove("VISUAL")
		.env("GIT_SEQUENCE_EDITOR", editor.canonicalize().unwrap())
		.spawn()
		.unwrap();

	// Wait for the rebase state to appear, then kill git.
	let merge_dir = s.path().join(".git").join("rebase-merge");
	let mut appeared = false;
	for _ in 0..200 {
		if merge_dir.is_dir() {
			appeared = true;
			break;
		}
		std::thread::sleep(Duration::from_millis(25));
	}
	assert!(appeared, "rebase-merge never appeared");
	child.kill().unwrap();
	child.wait().unwrap();

	// Observation over the half-initialized state must not
	// fail, whatever git left behind.
	let engine = Arc::new(s.engine_at("gitui-session-kill"));
	let observed = engine
		.observe()
		.unwrap_or_else(|e| panic!("observe after kill failed: {e}"));
	assert_eq!(
		observed.operation,
		Operation::RebaseInteractive,
		"observed: {observed:?}",
	);
	// Observed: onto/orig-head/head-name are written before
	// the editor runs, so the state still parses.
	assert!(
		observed.rebase.is_some(),
		"the half-initialized rebase state must still be \
		 readable: {observed:?}",
	);
	assert!(
		observed.conflicts.is_empty(),
		"a kill before any replay cannot conflict: {observed:?}",
	);

	// Abort must recover cleanly and leave the tip alone.
	let aborted = engine.exec(Cmd::Abort).unwrap();
	assert_eq!(
		aborted.code,
		0,
		"abort after kill must recover: {}",
		String::from_utf8_lossy(&aborted.stderr),
	);
	assert_eq!(
		aborted.snapshot.operation,
		Operation::Clean,
		"abort after kill must end clean",
	);
	assert_eq!(
		s.head(),
		tip_before,
		"abort after kill must leave the tip unchanged",
	);
}

/// One fixed seed, run twice: identical oracle expectations —
/// guards accidental nondeterminism in the harness itself.
///
/// Seed: `0xD1FF_5EED + 50`.
#[test]
fn determinism() {
	let seed = SEED_BASE + 50;
	let expectations = || {
		let s = Sandbox::new();
		exclude_harness_files(&s);
		let engine = s.engine();
		let mut rng = Rng(seed);
		let fixture = random_repo(&mut rng, &s, false);
		let (plan, _compiled) = compilable_plan(
			&mut rng,
			&engine,
			&fixture,
			seed,
			50,
			&|_| true,
		);
		let (present, absent) = expected_files(&plan, &fixture);
		(expected_subjects(&plan, &fixture), present, absent)
	};
	let first = expectations();
	let second = expectations();
	assert_eq!(
		first, second,
		"seed {seed}: the harness must be deterministic",
	);
}
