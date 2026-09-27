//! The app-facing rebase service: the single owner of the
//! rebase engine, journal, and session lifecycle for the UI.
//!
//! The rebase workspace tab consumes this service through its
//! `RebaseBackend` seam; until that delegate is compiled in,
//! the binary cannot see the use and the items read as dead.
//! Remove the `allow` below when the tab wires the service in.
#![allow(dead_code)]
//!
//!
//! The domain layer ([`asyncgit::rebase`]) is complete and
//! tested; this module is the plumbing that makes it usable
//! from a single-threaded, `Rc`/`RefCell` UI:
//!
//! - **Engine commands are blocking** (git processes, and a
//!   message-bridge wait of up to minutes), so they run on one
//!   dedicated exec worker thread. Observations are cheap but
//!   must stay answerable *while an exec blocks* (the composer
//!   flow depends on seeing a pending bridge request), so they
//!   run on a second, observation-only thread bound to a
//!   second engine instance that shares the session directory
//!   (and therefore the bridge) but owns no effects. Both
//!   workers answer through one channel and wake the app with
//!   [`AsyncGitNotification::Rebase`].
//! - **Correspondence commands**
//!   ([`Cmd::SetBridgeMessage`], [`Cmd::CancelBridgeMessage`])
//!   are lock-free in the engine and are executed
//!   synchronously on the UI thread: they must answer while an
//!   exec is in flight.
//!
//! ## Crash protocol (enforced here)
//!
//! Every worker dispatch appends a journal `Command` event
//! *first*, synchronously and durably; only then is the command
//! sent to the worker and remembered as in-flight. When the
//! outcome lands, a `Finished` event (code, head, generation)
//! is appended, followed by the derived terminal event:
//!
//! - a `Start` that demonstrably began records `Started`; if
//!   its snapshot is already clean the replay completed
//!   instantly and `Completed` follows,
//! - `rebase --abort` on a clean snapshot records `Aborted`,
//! - `rebase --quit` on a clean snapshot records `Quit`,
//! - any other command whose snapshot is clean while the
//!   session was `Running` records `Completed` (the rebase
//!   ended).
//!
//! A `Start` whose outcome shows the rebase never began (a
//! nonzero exit on a clean snapshot) records *no* `Started`;
//! the trailing `Finished` clears the recorded intent, the
//! session stays a draft, and git's one-line reason is surfaced
//! as an error. A spawn failure never ran anything: it is
//! surfaced as an error and closed with a `Finished` built from
//! the last observation, so no intent stays uncertain. A
//! sealed (unreadable) journal refuses all effects: intent
//! before effect, without exception.
//!
//! ## Lifecycle
//!
//! The service is deliberately *not* joined on shutdown: the
//! [`WorkerHandle`] drops the job senders, the workers finish
//! their current blocking exec and then exit on the closed
//! channel. Joining would hang the UI for the duration of a
//! blocked bridge wait; instead the engine process outlives a
//! closing gitui and the bridge heartbeat fails the edit when
//! the parent dies.

use crate::{
	app::Environment,
	queue::{InternalEvent, NeedsUpdate, Queue},
};
use anyhow::Result;
use asyncgit::{
	rebase::{
		engine::{
			Base, Cmd, EmptyPolicy, Engine, NativeEngine, Outcome,
			StartOptions,
		},
		journal::{self, Event, Journal, Protection, Scope, Session},
		plan::{Edit, Plan},
		policy::{self, Done, Phase, Policy},
		snapshot::{Operation, Snapshot},
		todo::Todo,
	},
	sync::CommitId,
	AsyncGitNotification,
};
use crossbeam_channel::{unbounded, Receiver, Sender};
use std::{
	mem,
	path::{Path, PathBuf},
	sync::Arc,
};

/// Intent recorded for a todo-capture run.
const INTENT_CAPTURE: &str = "rebase -i (capture todo)";
/// Intent recorded for a rebase start.
const INTENT_START: &str = "rebase -i (apply todo)";
/// Intent recorded for a continue.
const INTENT_CONTINUE: &str = "rebase --continue";
/// Intent recorded for a skip.
const INTENT_SKIP: &str = "rebase --skip";
/// Intent recorded for an abort; its outcome derives `Aborted`.
const INTENT_ABORT: &str = "rebase --abort";
/// Intent recorded for a quit; its outcome derives `Quit`.
const INTENT_QUIT: &str = "rebase --quit";
/// Intent recorded for an edit-todo run.
const INTENT_EDIT_TODO: &str = "rebase --edit-todo";
/// Intent recorded for a commit at an edit stop.
const INTENT_COMMIT: &str = "commit";
/// Intent recorded for stash protection.
const INTENT_STASH: &str = "stash create";
/// Intent recorded for restoring protection.
const INTENT_STASH_APPLY: &str = "stash apply";
/// Prefix of the dynamic intents for ref creation.
const INTENT_CREATE_REF: &str = "update-ref";
/// Prefix of the dynamic intents for ref deletion.
const INTENT_DELETE_REF: &str = "update-ref -d";
/// Namespace of the recovery ref created before every start.
const RECOVERY_REF_PREFIX: &str = "refs/gitui/rebase";

/// What a finished command was, derived from its recorded
/// intent. Drives the journal's terminal derivations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
	///
	Capture,
	///
	Start,
	///
	Continue,
	///
	Skip,
	///
	Abort,
	///
	Quit,
	///
	EditTodo,
	///
	Commit,
	///
	CreateRef,
	///
	DeleteRef,
	///
	Stash,
	///
	StashApply,
	///
	Other,
}

/// Classify a recorded intent.
fn kind_of(intent: &str) -> Kind {
	match intent {
		INTENT_CAPTURE => Kind::Capture,
		INTENT_START => Kind::Start,
		INTENT_CONTINUE => Kind::Continue,
		INTENT_SKIP => Kind::Skip,
		INTENT_ABORT => Kind::Abort,
		INTENT_QUIT => Kind::Quit,
		INTENT_EDIT_TODO => Kind::EditTodo,
		INTENT_COMMIT => Kind::Commit,
		INTENT_STASH => Kind::Stash,
		INTENT_STASH_APPLY => Kind::StashApply,
		other => {
			if other.starts_with(INTENT_DELETE_REF) {
				Kind::DeleteRef
			} else if other.starts_with(INTENT_CREATE_REF) {
				Kind::CreateRef
			} else {
				Kind::Other
			}
		}
	}
}

/// The start sequence waiting to run, in stages: protect
/// (stash), pin (recovery ref), then start. Each stage is
/// dispatched only after the previous outcome landed.
struct PendingStart {
	/// Compiled program for the eventual start.
	todo: Vec<u8>,
	/// Full option set for the eventual start.
	options: StartOptions,
	/// Recovery ref (name, target) created before the start.
	recovery: Option<(String, CommitId)>,
	/// Protection recorded by a successful stash stage.
	protection: Option<Protection>,
}

/// A job for the exec worker.
enum ExecJob {
	///
	Run {
		///
		intent: String,
		///
		cmd: Cmd,
	},
}

/// What a worker sends back to the UI thread.
enum Reply {
	///
	Exec {
		///
		intent: String,
		///
		outcome: std::result::Result<Outcome, String>,
	},
	///
	Observed(std::result::Result<Snapshot, String>),
}

/// Owns the background workers' lifetimes.
///
/// The handle holds the *only* job senders. Dropping it closes
/// the job channels; each worker finishes its current blocking
/// job and then exits its receive loop. The threads are
/// **deliberately never joined**: a thread can be parked for
/// minutes inside a message-bridge wait, and joining would hang
/// the UI thread on drop. A closing gitui instead outlives the
/// engine process, and the bridge heartbeat fails the edit when
/// the parent disappears — the documented recovery path.
struct WorkerHandle {
	/// Sender for engine commands (the exec worker).
	exec: Sender<ExecJob>,
	/// Sender for observation requests (the observer worker).
	observe: Sender<()>,
}

impl WorkerHandle {
	/// Hand one command to the exec worker. `false` when the
	/// worker is gone (nothing will run).
	fn exec(&self, intent: String, cmd: Cmd) -> bool {
		self.exec.send(ExecJob::Run { intent, cmd }).is_ok()
	}

	/// Request one observation. `false` when the worker is
	/// gone.
	fn observe(&self) -> bool {
		self.observe.send(()).is_ok()
	}
}

/// The rebase workspace, bound to one repository.
pub struct RebaseService {
	/// Folded session state (always `Session::fold(&events)`).
	session: Session,
	/// The full in-memory event history (mirrors the journal).
	events: Vec<Event>,
	/// The durable journal; `None` once it was found unreadable
	/// (the session then refuses all effects).
	journal: Option<Journal>,
	/// Last observed repository state.
	snapshot: Snapshot,
	/// Job senders; dropping it ends the workers.
	worker: WorkerHandle,
	/// Replies from both workers.
	outcomes: Receiver<Reply>,
	/// Intent of the running command, if any.
	in_flight: Option<String>,
	/// The engine; the UI thread only calls
	/// (correspondence commands) and the one fresh observation
	/// in `start` on it.
	engine: Arc<NativeEngine>,
	/// Event sink for UI updates.
	queue: Queue,
	/// Request parameters of the pending capture.
	pending_capture: Option<Base>,
	/// Staged start sequence, when protection/ref stages are
	/// between outcomes.
	pending_start: Option<PendingStart>,
	/// Whether the next observation should be folded into an
	/// adoption (see [`Self::adopt_existing`]).
	pending_adopt: bool,
	/// Whether an observation is in flight (dedupes requests).
	observing: bool,
	/// `--rebase-merges` choice of the current draft (not yet
	/// persisted; see [`Self::start`]).
	draft_rebase_merges: bool,
	/// User-facing errors produced asynchronously.
	errors: Vec<String>,
}

impl RebaseService {
	/// Create a service for the environment's repository. The
	/// journal (and any prior session) under
	/// `<git dir>/gitui/rebase` is loaded and folded.
	///
	/// # Errors
	///
	/// When the repository cannot be read at all, or the
	/// worker threads cannot be spawned.
	pub fn new(env: &Environment) -> Result<Self> {
		Self::build(env, Vec::new(), None)
	}

	/// Where this session's journal and bridge live.
	#[must_use]
	pub fn session_dir(&self) -> &Path {
		self.engine.session_dir()
	}

	/// The folded session state.
	#[must_use]
	pub const fn session(&self) -> &Session {
		&self.session
	}

	/// The last observation (arrives via the worker; the
	/// projection above is derived from it).
	#[must_use]
	pub const fn snapshot(&self) -> &Snapshot {
		&self.snapshot
	}

	/// The current projection: `policy(session, snapshot)`
	/// with the in-flight command merged. While a command runs
	/// the phase becomes `Executing`, except that a restore in
	/// flight over a finished session shows `Restoring`, and
	/// terminal or diagnostic phases keep their label (a
	/// trailing cleanup command does not reopen the workspace).
	#[must_use]
	pub fn policy(&self) -> Policy {
		let mut projection =
			policy::policy(&self.session, &self.snapshot);
		if self.is_busy() {
			let restoring =
				self.in_flight.as_deref() == Some(INTENT_STASH_APPLY);
			projection.phase = match projection.phase {
				Phase::Done(Done::Finished) if restoring => {
					Phase::Restoring
				}
				Phase::Done(_)
				| Phase::Reconciling { .. }
				| Phase::Unsupported { .. } => projection.phase,
				_ => Phase::Executing,
			};
		}
		projection
	}

	/// Whether a worker command is running.
	#[must_use]
	pub const fn is_busy(&self) -> bool {
		self.in_flight.is_some()
	}

	/// Has any session content (draft/active/terminal)? The
	/// schema marker alone does not count.
	#[must_use]
	pub fn exists(&self) -> bool {
		self.session.phase != journal::Phase::Empty
			|| self
				.events
				.iter()
				.any(|event| !matches!(event, Event::Schema(_)))
	}

	/// Called on [`AsyncGitNotification::Rebase`]: drain worker
	/// outcomes, fold them into the journal, and refresh the
	/// snapshot. Pushes `InternalEvent::Update(NeedsUpdate::ALL)`
	/// through the queue when anything changed.
	///
	/// The `Result` shape is part of the service contract (UI
	/// event handlers in this app return `Result`); the method
	/// itself cannot fail.
	///
	/// # Errors
	///
	/// Never fails; every failure is surfaced through
	/// [`Self::take_errors`].
	#[allow(clippy::unnecessary_wraps)]
	pub fn on_worker_event(&mut self) -> Result<()> {
		let mut changed = false;
		while let Ok(reply) = self.outcomes.try_recv() {
			changed |= self.process_reply(reply);
		}
		if changed {
			self.queue.push(InternalEvent::Update(NeedsUpdate::ALL));
		}
		Ok(())
	}

	/// Fold one worker reply. `true` when anything changed.
	fn process_reply(&mut self, reply: Reply) -> bool {
		match reply {
			Reply::Exec { intent, outcome } => {
				self.on_exec_outcome(&intent, outcome);
				true
			}
			Reply::Observed(result) => {
				self.observing = false;
				match result {
					Ok(snapshot) => {
						let adopted =
							self.consume_observation(&snapshot);
						self.snapshot = snapshot;
						adopted
					}
					Err(error) => {
						self.errors.push(error);
						false
					}
				}
			}
		}
	}

	/// Request a fresh observation (async; arrives via
	/// [`Self::on_worker_event`]). Requests are deduplicated
	/// while one is in flight.
	///
	/// The `Result` shape is part of the service contract; the
	/// method itself cannot fail.
	///
	/// # Errors
	///
	/// Never fails; observation failures surface through
	/// [`Self::take_errors`].
	#[allow(clippy::unnecessary_wraps)]
	pub fn refresh(&mut self) -> Result<()> {
		if self.observing {
			return Ok(());
		}
		if self.worker.observe() {
			self.observing = true;
		} else {
			self.errors.push(String::from(
				"rebase: the observer is not running",
			));
		}
		Ok(())
	}

	/// Start a draft for a scope: capture git's canonical todo
	/// (worker), then journal `Created` + `PlanAdopted` on the
	/// outcome. Fire-and-forget: refusals and failures surface
	/// through [`Self::take_errors`].
	pub fn create_draft(&mut self, base: Base, rebase_merges: bool) {
		if self.is_busy() {
			self.errors.push(String::from(
				"rebase: a command is already running",
			));
			return;
		}
		if matches!(
			self.session.phase,
			journal::Phase::Draft | journal::Phase::Running
		) {
			self.errors.push(String::from(
				"rebase: a session is already open",
			));
			return;
		}
		self.draft_rebase_merges = rebase_merges;
		self.pending_capture = Some(base.clone());
		if !self.dispatch(
			INTENT_CAPTURE,
			Cmd::GenerateTodo {
				base,
				rebase_merges,
			},
		) {
			self.pending_capture = None;
		}
	}

	/// Adopt a rebase found in the repository (no plan
	/// adoption; journals `Created` from the observation plus
	/// `Adopted` once a fresh observation confirmed the
	/// external rebase). Fire-and-forget.
	pub fn adopt_existing(&mut self) {
		if self.is_busy() {
			self.errors.push(String::from(
				"rebase: a command is already running",
			));
			return;
		}
		self.pending_adopt = true;
		if self.refresh().is_err() {
			self.pending_adopt = false;
		}
	}

	/// Apply a plan edit (validated against the current plan;
	/// journals `Edited` on success). Synchronous — pure plan
	/// work.
	///
	/// # Errors
	///
	/// A short reason when the service is busy, the plan is
	/// not editable, the edit does not apply, or the event
	/// could not be recorded.
	pub fn apply_edit(&mut self, edit: Edit) -> Result<(), String> {
		self.refuse_not_draft()?;
		let Some(plan) = self.session.plan.clone() else {
			return Err(String::from("no plan to edit"));
		};
		let next = plan.apply(&edit).map_err(|error| {
			format!("the edit does not apply ({error:?})")
		})?;
		if !self.record_all(&[Event::Edited { edit, plan: next }]) {
			return Err(String::from(
				"the edit could not be recorded",
			));
		}
		Ok(())
	}

	/// Undo the last plan edit.
	///
	/// # Errors
	///
	/// A short reason when undoing is impossible now.
	pub fn undo(&mut self) -> Result<(), String> {
		self.refuse_not_draft()?;
		if !self.session.can_undo() {
			return Err(String::from("nothing to undo"));
		}
		if !self.record_all(&[Event::Undo]) {
			return Err(String::from("could not record the undo"));
		}
		Ok(())
	}

	/// Redo the last undone plan edit.
	///
	/// # Errors
	///
	/// A short reason when redoing is impossible now.
	pub fn redo(&mut self) -> Result<(), String> {
		self.refuse_not_draft()?;
		if !self.session.can_redo() {
			return Err(String::from("nothing to redo"));
		}
		if !self.record_all(&[Event::Redo]) {
			return Err(String::from("could not record the redo"));
		}
		Ok(())
	}

	/// Review+start: when local changes exist they are
	/// protected into a stash first; a recovery ref pins the
	/// original tip; then the rebase starts. The stages are
	/// worker commands dispatched in sequence as their
	/// predecessors' outcomes land (see the module docs for
	/// the journaling order). Fire-and-forget.
	pub fn start(
		&mut self,
		empty: EmptyPolicy,
		update_refs: Vec<String>,
	) {
		if self.is_busy() || self.pending_start.is_some() {
			self.errors.push(String::from(
				"rebase: a command is already running",
			));
			return;
		}
		let Some(plan) = self.session.plan.clone() else {
			self.errors
				.push(String::from("rebase: no plan to start"));
			return;
		};
		let Some(base) = self
			.session
			.scope
			.as_ref()
			.map(|scope| scope.base.clone())
		else {
			self.errors
				.push(String::from("rebase: the draft has no scope"));
			return;
		};
		let Ok(todo) = plan.compile() else {
			self.errors.push(String::from(
				"rebase: the plan is invalid; review before starting",
			));
			return;
		};
		// A policy is a projection of a moment, not a token
		// against the future: the start decision (protection,
		// recovery tip) revalidates against a fresh
		// observation. The engine mutex is free — we checked
		// `is_busy` — so this read is fast.
		match self.engine.observe() {
			Ok(snapshot) => self.snapshot = snapshot,
			Err(error) => {
				self.errors.push(format!("rebase: {error}"));
				return;
			}
		}
		let Some(tip) = self.snapshot.head else {
			self.errors
				.push(String::from("rebase: nothing is checked out"));
			return;
		};
		let reference = format!(
			"{RECOVERY_REF_PREFIX}/{}",
			tip.get_short_string()
		);
		self.pending_start = Some(PendingStart {
			todo,
			options: StartOptions {
				base,
				empty,
				rebase_merges: self.draft_rebase_merges,
				update_refs,
			},
			recovery: Some((reference, tip)),
			protection: None,
		});
		if self.has_local_changes() {
			let dispatched = self.dispatch(
				INTENT_STASH,
				Cmd::StashCreate {
					include_untracked: true,
				},
			);
			if !dispatched {
				self.pending_start = None;
			}
			return;
		}
		self.dispatch_recovery_ref();
	}

	/// Continue a stopped rebase. `message` is a prewritten
	/// message for a message editor git may open along the way.
	/// Fire-and-forget.
	pub fn r#continue(&mut self, message: Option<String>) {
		self.dispatch_simple(
			INTENT_CONTINUE,
			Cmd::Continue { message },
		);
	}

	/// Skip the current step. Fire-and-forget.
	pub fn skip(&mut self) {
		self.dispatch_simple(INTENT_SKIP, Cmd::Skip);
	}

	/// Abort the rebase. Fire-and-forget.
	pub fn abort(&mut self) {
		self.dispatch_simple(INTENT_ABORT, Cmd::Abort);
	}

	/// Quit: drop sequencer bookkeeping, keep current state.
	/// Fire-and-forget.
	pub fn quit(&mut self) {
		self.dispatch_simple(INTENT_QUIT, Cmd::Quit);
	}

	/// Rewrite the remaining todo of a stopped rebase.
	/// Fire-and-forget.
	pub fn edit_remaining(&mut self, todo: Vec<u8>) {
		self.dispatch_simple(
			INTENT_EDIT_TODO,
			Cmd::EditTodo { todo },
		);
	}

	/// Create a commit at an edit stop (amend or new child).
	/// Fire-and-forget.
	pub fn commit(
		&mut self,
		message: String,
		amend: bool,
		allow_empty: bool,
	) {
		self.dispatch_simple(
			INTENT_COMMIT,
			Cmd::Commit {
				message,
				amend,
				allow_empty,
			},
		);
	}

	/// Resolve a pending bridge message request. Synchronous
	/// and lock-free by design: git is blocked in the helper
	/// waiting for exactly this answer while an exec command
	/// runs on the worker.
	pub fn resolve_message(&mut self, message: String) {
		self.correspond(Cmd::SetBridgeMessage { message });
	}

	/// Cancel a pending bridge message request; git fails the
	/// step and stays stopped. Synchronous and lock-free.
	pub fn cancel_message(&mut self) {
		self.correspond(Cmd::CancelBridgeMessage);
	}

	/// Restore protected local changes (after a finished
	/// rebase). Fire-and-forget.
	pub fn restore_protection(&mut self) {
		let Some(protection) = self.session.protection.clone() else {
			self.errors.push(String::from("nothing is protected"));
			return;
		};
		self.dispatch_simple(
			INTENT_STASH_APPLY,
			Cmd::StashApply {
				oid: protection.stash,
				drop: true,
			},
		);
	}

	/// Close the session: journal `Closed`, then remove the
	/// recovery ref on the worker. Fire-and-forget.
	pub fn close(&mut self) {
		if self.is_busy() {
			self.errors.push(String::from(
				"rebase: a command is already running",
			));
			return;
		}
		if !self.record(&Event::Closed) {
			return;
		}
		self.refold();
		let reference = self
			.session
			.started
			.as_ref()
			.and_then(|started| started.recovery_ref.clone())
			.map(|(reference, _)| reference);
		let Some(reference) = reference else {
			return;
		};
		self.dispatch(
			&format!("{INTENT_DELETE_REF} {reference}"),
			Cmd::DeleteRef { reference },
		);
	}

	/// Drain user-facing errors produced asynchronously.
	#[must_use]
	pub fn take_errors(&mut self) -> Vec<String> {
		mem::take(&mut self.errors)
	}

	// ------------------------------------------------------------------
	// construction
	// ------------------------------------------------------------------

	/// Build the service. `extra_env` and `bridge_exe` are
	/// testing seams mirroring the engine's own.
	fn build(
		env: &Environment,
		mut extra_env: Vec<(String, std::ffi::OsString)>,
		bridge_exe: Option<PathBuf>,
	) -> Result<Self> {
		let repo = env.repo.borrow().clone();
		// Observations race running git (see the observer
		// below); read-only commands must not grab optional
		// locks (`index.lock`) while a rebase writes.
		extra_env.push((
			String::from("GIT_OPTIONAL_LOCKS"),
			std::ffi::OsString::from("0"),
		));
		let mut engine = NativeEngine::new(repo.clone(), None)?;
		let session = engine.session_dir().to_path_buf();
		// Shares the session directory (and therefore the
		// bridge) with `engine`, but owns no effects: its
		// observations stay answerable while an exec blocks.
		let mut observer = NativeEngine::new(repo, Some(session))?;
		if let Some(exe) = bridge_exe {
			engine = engine.with_bridge_exe(exe);
		}
		if !extra_env.is_empty() {
			engine = engine.with_extra_env(extra_env.clone());
			observer = observer.with_extra_env(extra_env);
		}
		let engine = Arc::new(engine);
		let observer = Arc::new(observer);

		let (jobs, jobs_rx) = unbounded();
		let (observe_jobs, observe_jobs_rx) = unbounded();
		let (replies, outcomes) = unbounded();

		Self::spawn_exec_worker(
			Arc::clone(&engine),
			jobs_rx,
			replies.clone(),
			env.sender_git.clone(),
		)?;
		Self::spawn_observe_worker(
			observer,
			observe_jobs_rx,
			replies,
			env.sender_git.clone(),
		)?;

		let (mut events, mut journal, session, mut errors) =
			Self::load_journal(engine.session_dir());
		// A fresh (or empty) journal starts with its schema
		// marker: the first line of every journal.
		if let Some(journal) = journal.as_mut() {
			if events.is_empty() {
				match journal.append(&Event::Schema(1)) {
					Ok(()) => events.push(Event::Schema(1)),
					Err(error) => errors
						.push(format!("rebase journal: {error}")),
				}
			}
		}
		// One synchronous observation: the projection must be
		// honest from the first frame.
		let snapshot = engine.observe()?;

		Ok(Self {
			session,
			events,
			journal,
			snapshot,
			worker: WorkerHandle {
				exec: jobs,
				observe: observe_jobs,
			},
			outcomes,
			in_flight: None,
			engine,
			queue: env.queue.clone(),
			pending_capture: None,
			pending_start: None,
			pending_adopt: false,
			observing: false,
			draft_rebase_merges: false,
			errors,
		})
	}

	/// Spawn the exec worker.
	fn spawn_exec_worker(
		engine: Arc<NativeEngine>,
		jobs: Receiver<ExecJob>,
		replies: Sender<Reply>,
		notify: Sender<AsyncGitNotification>,
	) -> Result<()> {
		std::thread::Builder::new()
			.name(String::from("rebase-exec"))
			.spawn(move || {
				for job in jobs {
					let ExecJob::Run { intent, cmd } = job;
					let outcome = engine
						.exec(cmd)
						.map_err(|error| error.to_string());
					let delivered =
						replies.send(Reply::Exec { intent, outcome });
					let _ = notify.send(AsyncGitNotification::Rebase);
					if delivered.is_err() {
						break;
					}
				}
			})?;
		Ok(())
	}

	/// Spawn the observation worker.
	fn spawn_observe_worker(
		observer: Arc<NativeEngine>,
		observe_jobs: Receiver<()>,
		replies: Sender<Reply>,
		notify: Sender<AsyncGitNotification>,
	) -> Result<()> {
		std::thread::Builder::new()
			.name(String::from("rebase-observe"))
			.spawn(move || {
				for () in observe_jobs {
					let snapshot = observer
						.observe()
						.map_err(|error| error.to_string());
					let delivered =
						replies.send(Reply::Observed(snapshot));
					let _ = notify.send(AsyncGitNotification::Rebase);
					if delivered.is_err() {
						break;
					}
				}
			})?;
		Ok(())
	}

	/// Open the journal and fold its committed history. An
	/// unreadable journal seals the session (read-only
	/// recovery): no effect will ever be dispatched against a
	/// history this build cannot understand.
	fn load_journal(
		dir: &Path,
	) -> (Vec<Event>, Option<Journal>, Session, Vec<String>) {
		match Journal::open(dir) {
			Ok(loaded) => {
				let mut errors = Vec::new();
				if loaded.truncated_tail {
					errors.push(String::from(
						"the rebase journal had a torn \
						 final entry; it was repaired",
					));
				}
				let session = Session::fold(&loaded.events);
				(loaded.events, Some(loaded.journal), session, errors)
			}
			Err(error) => (
				Vec::new(),
				None,
				Session::default(),
				vec![format!(
					"the rebase journal could not be read \
					 ({error}); the session is read-only"
				)],
			),
		}
	}

	// ------------------------------------------------------------------
	// journal helpers
	// ------------------------------------------------------------------

	/// Append one event durably and extend the in-memory
	/// history. A sealed journal or a failed append refuses the
	/// event: intent before effect.
	fn record(&mut self, event: &Event) -> bool {
		let Some(journal) = self.journal.as_mut() else {
			return false;
		};
		match journal.append(event) {
			Ok(()) => {
				self.events.push(event.clone());
				true
			}
			Err(error) => {
				self.errors.push(format!("rebase journal: {error}"));
				false
			}
		}
	}

	/// Append a batch in order, stopping at the first failure,
	/// then re-fold the session. `false` when nothing was
	/// recorded.
	fn record_all(&mut self, batch: &[Event]) -> bool {
		let mut recorded = false;
		for event in batch {
			if !self.record(event) {
				break;
			}
			recorded = true;
		}
		self.refold();
		recorded
	}

	/// Re-fold the session from the event history.
	fn refold(&mut self) {
		self.session = Session::fold(&self.events);
	}

	// ------------------------------------------------------------------
	// dispatch
	// ------------------------------------------------------------------

	/// Record the intent, then hand the command to the worker.
	/// `false` when the intent could not be recorded (nothing
	/// runs) or the worker is gone.
	fn dispatch(&mut self, intent: &str, cmd: Cmd) -> bool {
		if !self.record(&Event::Command {
			intent: String::from(intent),
		}) {
			return false;
		}
		self.refold();
		if !self.worker.exec(String::from(intent), cmd) {
			self.errors
				.push(format!("rebase: {intent} not dispatched"));
			return false;
		}
		self.in_flight = Some(String::from(intent));
		true
	}

	/// Dispatch a user command, refusing politely while
	/// another command runs.
	fn dispatch_simple(&mut self, intent: &str, cmd: Cmd) {
		if self.is_busy() {
			self.errors.push(format!(
				"rebase: {intent} refused, a command is \
				 already running"
			));
			return;
		}
		self.dispatch(intent, cmd);
	}

	/// Run a correspondence command synchronously (lock-free
	/// in the engine) and adopt its snapshot.
	fn correspond(&mut self, cmd: Cmd) {
		match self.engine.exec(cmd) {
			Ok(outcome) => {
				self.snapshot = outcome.snapshot;
				self.queue
					.push(InternalEvent::Update(NeedsUpdate::ALL));
			}
			Err(error) => {
				self.errors.push(format!("rebase: {error}"));
			}
		}
	}

	/// Refuse synchronous plan work unless the session is an
	/// idle draft.
	fn refuse_not_draft(&self) -> Result<(), String> {
		if self.is_busy() {
			return Err(String::from("a rebase command is running"));
		}
		if self.session.phase != journal::Phase::Draft {
			return Err(String::from("the plan is not editable now"));
		}
		Ok(())
	}

	/// Dispatch the recovery-ref stage of a pending start (or
	/// go straight to the start when no ref is planned).
	fn dispatch_recovery_ref(&mut self) {
		let Some(pending) = self.pending_start.as_ref() else {
			return;
		};
		let Some((reference, target)) = pending.recovery.clone()
		else {
			self.dispatch_planned_start();
			return;
		};
		let dispatched = self.dispatch(
			&format!("{INTENT_CREATE_REF} {reference}"),
			Cmd::CreateRef { reference, target },
		);
		if !dispatched {
			self.pending_start = None;
		}
	}

	/// Dispatch the start stage of a pending sequence. The
	/// pending facts stay put: the `Started` journal event
	/// needs them when the outcome lands.
	fn dispatch_planned_start(&mut self) {
		let Some(pending) = self.pending_start.as_ref() else {
			return;
		};
		let dispatched = self.dispatch(
			INTENT_START,
			Cmd::Start {
				todo: pending.todo.clone(),
				options: pending.options.clone(),
			},
		);
		if !dispatched {
			self.pending_start = None;
		}
	}

	// ------------------------------------------------------------------
	// outcomes
	// ------------------------------------------------------------------

	/// Fold one command outcome into the journal.
	fn on_exec_outcome(
		&mut self,
		intent: &str,
		outcome: std::result::Result<Outcome, String>,
	) {
		self.in_flight = None;
		match outcome {
			Err(error) => {
				// The command never ran (spawn failure):
				// close the recorded intent with the last
				// known observation so nothing stays
				// uncertain.
				self.errors.push(format!(
					"rebase: {intent} failed: {error}"
				));
				self.record_all(&[Event::Finished {
					code: -1,
					head: self.snapshot.head,
					generation: self.snapshot.generation,
				}]);
			}
			Ok(outcome) => self.on_outcome(intent, outcome),
		}
	}

	/// Fold one successful (run) outcome: `Finished` first,
	/// then the derived events, then any follow-up stage.
	fn on_outcome(&mut self, intent: &str, outcome: Outcome) {
		let Outcome {
			code,
			stderr,
			snapshot,
			generated_todo,
			stash,
			..
		} = outcome;
		let kind = kind_of(intent);
		let prev_phase = self.session.phase.clone();
		let mut batch = vec![Event::Finished {
			code,
			head: snapshot.head,
			generation: snapshot.generation,
		}];

		match kind {
			Kind::Capture => self.capture_events(
				generated_todo,
				&stderr,
				&snapshot,
				&mut batch,
			),
			Kind::Start => self
				.start_events(code, &stderr, &snapshot, &mut batch),
			Kind::Stash => self
				.protection_events(code, stash, &stderr, &mut batch),
			Kind::CreateRef if code != 0 => {
				self.pending_start = None;
				self.errors.push(one_line(
					&stderr,
					"the recovery ref could not be created",
				));
			}
			Kind::StashApply => {
				self.restore_events(code, &stderr, &mut batch);
			}
			Kind::Abort if snapshot.operation == Operation::Clean => {
				batch.push(Event::Aborted);
			}
			Kind::Quit if snapshot.operation == Operation::Clean => {
				batch.push(Event::Quit);
			}
			_ if snapshot.operation == Operation::Clean
				&& prev_phase == journal::Phase::Running =>
			{
				if let Some(tip) = snapshot.head {
					batch.push(Event::Completed { tip });
				}
			}
			_ => {}
		}

		self.record_all(&batch);
		self.snapshot = snapshot;
		match kind {
			Kind::Stash => self.dispatch_recovery_ref(),
			Kind::CreateRef => self.dispatch_planned_start(),
			_ => {}
		}
	}

	/// `GenerateTodo` outcome: journal `Created` + `PlanAdopted`
	/// when git produced its canonical todo. The capture exit
	/// code is 1 by design (the capture editor refuses, which
	/// aborts the would-be start); the todo is the verdict.
	fn capture_events(
		&mut self,
		generated_todo: Option<Todo>,
		stderr: &[u8],
		snapshot: &Snapshot,
		batch: &mut Vec<Event>,
	) {
		let Some(base) = self.pending_capture.take() else {
			return;
		};
		let Some(todo) = generated_todo else {
			self.errors.push(one_line(
				stderr,
				"git produced no todo for this scope",
			));
			return;
		};
		let Some(tip) = snapshot.head else {
			self.errors.push(String::from(
				"rebase: the repository has no commits",
			));
			return;
		};
		let plan = Plan::from_todo(todo);
		batch.push(Event::Created {
			scope: Scope {
				branch: snapshot.branch.clone(),
				original_tip: tip,
				base,
			},
		});
		batch.push(Event::PlanAdopted {
			plan: plan.to_stored(),
		});
	}

	/// `Start` outcome: `Started` only when the rebase
	/// demonstrably began, plus `Completed` when it already
	/// finished (fast-forward replays complete instantly). A
	/// start that never began leaves the session a draft and
	/// surfaces git's reason.
	fn start_events(
		&mut self,
		code: i32,
		stderr: &[u8],
		snapshot: &Snapshot,
		batch: &mut Vec<Event>,
	) {
		let Some(pending) = self.pending_start.take() else {
			return;
		};
		let began = code == 0
			|| snapshot.operation.is_rebase()
			|| snapshot.rebase.is_some();
		if !began {
			self.errors
				.push(one_line(stderr, "the rebase did not start"));
			return;
		}
		batch.push(Event::Started {
			todo: pending.todo,
			base: pending.options.base,
			empty: pending.options.empty,
			recovery_ref: pending.recovery,
			protection: pending.protection,
		});
		if snapshot.operation == Operation::Clean {
			if let Some(tip) = snapshot.head {
				batch.push(Event::Completed { tip });
			}
		}
	}

	/// `StashCreate` outcome: journal `Protected` and keep the
	/// sequence going; a failure cancels the start.
	fn protection_events(
		&mut self,
		code: i32,
		stash: Option<CommitId>,
		stderr: &[u8],
		batch: &mut Vec<Event>,
	) {
		let Some(stash) = stash.filter(|_| code == 0) else {
			self.pending_start = None;
			self.errors.push(one_line(
				stderr,
				"protecting local changes failed",
			));
			return;
		};
		if let Some(pending) = self.pending_start.as_mut() {
			pending.protection = Some(Protection {
				stash,
				include_untracked: true,
			});
		}
		batch.push(Event::Protected {
			stash,
			include_untracked: true,
		});
	}

	/// `StashApply` outcome: journal `Restored` either way;
	/// a failed restore is surfaced (the stash entry survives).
	fn restore_events(
		&mut self,
		code: i32,
		stderr: &[u8],
		batch: &mut Vec<Event>,
	) {
		let Some(protection) = self.session.protection.clone() else {
			return;
		};
		if code != 0 {
			self.errors.push(one_line(
				stderr,
				"the protected changes could not be restored",
			));
		}
		batch.push(Event::Restored {
			stash: protection.stash,
			clean: code == 0,
		});
	}

	// ------------------------------------------------------------------
	// observation
	// ------------------------------------------------------------------

	/// Fold a fresh observation's pending duties (currently:
	/// confirming an adoption).
	fn consume_observation(&mut self, snapshot: &Snapshot) -> bool {
		if !self.pending_adopt {
			return false;
		}
		self.pending_adopt = false;
		let scope = snapshot.rebase.as_ref().map(|rebase| Scope {
			branch: rebase.head_name.clone(),
			original_tip: rebase.orig_head,
			base: Base::Upstream(rebase.onto),
		});
		let Some(scope) = scope else {
			self.errors.push(String::from(
				"rebase: no rebase is in progress to adopt",
			));
			return false;
		};
		self.record_all(&[Event::Created { scope }, Event::Adopted])
	}

	/// Whether the snapshot reports any local change at all.
	const fn has_local_changes(&self) -> bool {
		let dirty = &self.snapshot;
		!dirty.staged.is_empty()
			|| !dirty.worktree.is_empty()
			|| !dirty.untracked.is_empty()
	}
}

/// First line of a command's stderr, or a fixed fallback.
fn one_line(stderr: &[u8], fallback: &str) -> String {
	let text = String::from_utf8_lossy(stderr);
	let line = text.lines().next().unwrap_or_default().trim();
	if line.is_empty() {
		String::from(fallback)
	} else {
		String::from(line)
	}
}

#[cfg(test)]
mod tests {
	#![allow(clippy::unwrap_used, clippy::panic)]

	use super::*;
	use crate::{
		keys::KeyConfig, options::Options, ui::style::Theme,
	};
	use asyncgit::rebase::{
		journal::Journal as SessionJournal, plan::Action,
		todo::Instruction, StepId,
	};
	use asyncgit::sync::RepoPath;
	use std::{
		cell::RefCell,
		fs,
		os::unix::fs::PermissionsExt,
		process::{Command, Output},
		rc::Rc,
		time::{Duration, Instant},
	};
	use tempfile::TempDir;

	/// Fixed ancient dates keep test commits deterministic.
	const OLD_DATE: &str = "2005-04-07T22:13:13 +0000";
	/// Generous bound for waiting on real git processes.
	const WAIT: Duration = Duration::from_secs(60);

	/// POSIX-sh double of the gitui editor bridge, mirroring
	/// `asyncgit::rebase::helper` (capture/apply/message,
	/// prewritten, request and cancel). Tests inject it via
	/// the engine's `with_bridge_exe` seam because the test
	/// runner binary does not understand `--rebase-bridge`.
	const BRIDGE_SHIM: &str = r#"#!/bin/sh
set -u
dir="$GITUI_REBASE_BRIDGE"
[ "${1:-}" = "--rebase-bridge" ] && shift
kind="seq"
if [ "${1:-}" = "msg" ]; then
	kind="msg"
	shift
fi
file="${1:-}"
cp "$file" "$dir/generated.todo" || exit 1
mode="$(cat "$dir/mode" 2>/dev/null)"
if [ "$kind" = "seq" ]; then
	case "$mode" in
	capture) exit 1 ;;
	apply)
		if [ -f "$dir/plan.todo" ]; then
			cp "$dir/plan.todo" "$file"
		fi
		exit 0
		;;
	*) exit 1 ;;
	esac
fi
if [ -f "$dir/prewritten.txt" ]; then
	cp "$dir/prewritten.txt" "$file"
	exit 0
fi
printf '%s\n%s\n' "$file" "${GITUI_REBASE_PID:-0}" > "$dir/request"
while true; do
	if [ -f "$dir/message.txt" ]; then
		cp "$dir/message.txt" "$file"
		rm -f "$dir/request"
		exit 0
	fi
	if [ -f "$dir/cancel" ]; then
		rm -f "$dir/request"
		exit 1
	fi
	sleep 0.05
done
"#;

	/// A throwaway repository with a hermetic git environment
	/// (no user config, fixed identity, no signing).
	struct Sandbox {
		dir: TempDir,
		env: Vec<(String, std::ffi::OsString)>,
	}

	impl Sandbox {
		fn new() -> Self {
			let dir = TempDir::new().unwrap();
			let global = dir.path().join("global.gitconfig");
			fs::write(&global, b"[core]\n\tabbrev = 40\n").unwrap();
			let system = dir.path().join("system.gitconfig");
			fs::write(&system, b"").unwrap();
			let env: Vec<(String, std::ffi::OsString)> = vec![
				(
					String::from("GIT_CONFIG_GLOBAL"),
					global.into_os_string(),
				),
				(
					String::from("GIT_CONFIG_SYSTEM"),
					system.into_os_string(),
				),
				(
					String::from("GIT_AUTHOR_NAME"),
					std::ffi::OsString::from("t"),
				),
				(
					String::from("GIT_AUTHOR_EMAIL"),
					std::ffi::OsString::from("t@t"),
				),
				(
					String::from("GIT_COMMITTER_NAME"),
					std::ffi::OsString::from("t"),
				),
				(
					String::from("GIT_COMMITTER_EMAIL"),
					std::ffi::OsString::from("t@t"),
				),
			];
			let sandbox = Self { dir, env };
			fs::create_dir(sandbox.repo()).unwrap();
			sandbox.git_ok(&["init", "-q", "--initial-branch=main"]);
			sandbox
		}

		/// The repository root (all git traffic happens here);
		/// sandbox support files live outside of it so the
		/// worktree starts clean.
		fn repo(&self) -> PathBuf {
			self.dir.path().join("repo")
		}

		fn repo_path(&self) -> RepoPath {
			RepoPath::Workdir {
				gitdir: self.repo().join(".git"),
				workdir: self.repo(),
			}
		}

		/// An app environment pointed at this repository.
		fn environment(&self) -> Environment {
			let repo = RefCell::new(self.repo_path());
			Environment {
				queue: Queue::new(),
				theme: Rc::new(Theme::init(&PathBuf::new())),
				key_config: Rc::new(KeyConfig::default()),
				options: Options::new(repo.clone()),
				repo,
				sender_git: unbounded().0,
				sender_app: unbounded().0,
			}
		}

		/// A service wired for this repository: hermetic env
		/// for every child git, and the shim as editor bridge.
		fn service(&self) -> RebaseService {
			let env = self.environment();
			let shim = self.bridge_shim();
			RebaseService::build(&env, self.env.clone(), Some(shim))
				.unwrap()
		}

		fn bridge_shim(&self) -> PathBuf {
			let shim = self.dir.path().join("bridge-shim.sh");
			fs::write(&shim, BRIDGE_SHIM).unwrap();
			fs::set_permissions(
				&shim,
				fs::Permissions::from_mode(0o755),
			)
			.unwrap();
			shim.canonicalize().unwrap()
		}

		fn git(&self, args: &[&str]) -> Output {
			self.git_env(args, &[])
		}

		fn git_env(
			&self,
			args: &[&str],
			extra: &[(&str, &str)],
		) -> Output {
			let mut cmd = Command::new("git");
			cmd.args(args).current_dir(self.repo());
			for (key, _) in &self.env {
				cmd.env_remove(key);
			}
			for (key, value) in &self.env {
				cmd.env(key, value);
			}
			for (key, value) in extra {
				cmd.env(key, value);
			}
			cmd.output().unwrap()
		}

		fn git_ok(&self, args: &[&str]) -> Output {
			let out = self.git(args);
			assert!(
				out.status.success(),
				"sandbox git {args:?} failed: {}",
				String::from_utf8_lossy(&out.stderr)
			);
			out
		}

		fn write(&self, file: &str, content: &str) {
			fs::write(self.repo().join(file), content).unwrap();
		}

		fn read(&self, file: &str) -> String {
			fs::read_to_string(self.repo().join(file)).unwrap()
		}

		fn commit(&self, message: &str) -> String {
			self.git_ok(&["add", "-A"]);
			self.git_env(
				&["commit", "-m", message],
				&[
					("GIT_AUTHOR_DATE", OLD_DATE),
					("GIT_COMMITTER_DATE", OLD_DATE),
				],
			);
			self.head()
		}

		fn head(&self) -> String {
			String::from_utf8_lossy(
				&self.git_ok(&["rev-parse", "HEAD"]).stdout,
			)
			.trim()
			.to_owned()
		}

		fn ref_value(&self, name: &str) -> Option<String> {
			let out = self.git(&["rev-parse", "--verify", name]);
			if !out.status.success() {
				return None;
			}
			Some(
				String::from_utf8_lossy(&out.stdout)
					.trim()
					.to_owned(),
			)
		}

		fn log_subjects(&self, range: &str) -> Vec<String> {
			let out = self.git_ok(&["log", "--format=%s", range]);
			String::from_utf8_lossy(&out.stdout)
				.lines()
				.map(str::to_owned)
				.collect()
		}

		fn oid(&self, value: &str) -> CommitId {
			CommitId::from_str_unchecked(value).unwrap()
		}

		/// Base commit `A`, then `B` and `C` on separate files.
		fn two_step(&self) -> (String, String, String) {
			self.write("f.txt", "a\n");
			let base = self.commit("A");
			self.write("b.txt", "b\n");
			let left = self.commit("B");
			self.write("c.txt", "c\n");
			let right = self.commit("C");
			(base, left, right)
		}

		/// `A`, `B`, `C` on `main` and a conflicting `X` on
		/// `other` (both `B` and `X` rewrite `f.txt`).
		fn conflicting(&self) -> (String, String, String, String) {
			self.write("f.txt", "a\n");
			let base = self.commit("A");
			self.write("f.txt", "b\n");
			let left = self.commit("B");
			self.write("c.txt", "c\n");
			let right = self.commit("C");
			self.git_ok(&["checkout", "-q", "-b", "other", &base]);
			self.write("f.txt", "x\n");
			let other = self.commit("X");
			self.git_ok(&["checkout", "-q", "main"]);
			(base, left, right, other)
		}
	}

	/// The id of the plan's first pick-family step.
	fn first_pick(service: &RebaseService) -> Option<StepId> {
		service.session().plan.as_ref()?.steps().iter().find_map(
			|step| {
				step.line
					.instruction()
					.filter(|instruction| {
						instruction.is_pick_family()
					})
					.map(|_| step.id)
			},
		)
	}

	/// Re-read the journal from disk.
	fn load_events(service: &RebaseService) -> Vec<Event> {
		SessionJournal::open(service.session_dir()).unwrap().events
	}

	/// Every `Command` must be immediately followed by its
	/// `Finished` (the crash protocol's pairing rule), and the
	/// journal must open with the schema marker.
	fn assert_paired(events: &[Event]) {
		assert_eq!(events.first(), Some(&Event::Schema(1)));
		let commands = events
			.iter()
			.filter(|event| matches!(event, Event::Command { .. }))
			.count();
		assert!(commands > 0, "no commands were recorded");
		let mut index = 0;
		while index < events.len() {
			if matches!(events[index], Event::Command { .. }) {
				assert!(
					matches!(
						events.get(index + 1),
						Some(Event::Finished { .. })
					),
					"command at {index} lacks its finish: \
					 {events:?}"
				);
				index += 2;
			} else {
				index += 1;
			}
		}
	}

	#[test]
	fn create_draft_captures_and_journals() {
		let s = Sandbox::new();
		let (a, _b, c) = s.two_step();
		let mut service = s.service();
		assert!(!service.exists());

		service.create_draft(Base::Upstream(s.oid(&a)), false);
		service.wait_idle(WAIT);

		assert!(service.exists());
		assert!(!service.is_busy());
		assert!(service.take_errors().is_empty());
		assert_eq!(service.session().phase, journal::Phase::Draft);
		let picks = service
			.session()
			.plan
			.as_ref()
			.unwrap()
			.steps()
			.iter()
			.filter(|step| {
				matches!(
					step.line.instruction(),
					Some(instruction)
						if instruction.is_pick_family()
				)
			})
			.count();
		assert_eq!(picks, 2, "b and c are the scope");
		let scope = service.session().scope.as_ref().unwrap();
		assert_eq!(scope.original_tip, s.oid(&c));
		assert_eq!(scope.base, Base::Upstream(s.oid(&a)));

		// The journal exists on disk with the expected head:
		// schema, then the capture's intent/finish pair, then
		// the draft's creation.
		let journal_path = service.session_dir().join("journal.ron");
		assert!(journal_path.exists());
		let events = load_events(&service);
		assert_paired(&events);
		assert!(matches!(events.get(1), Some(Event::Command { .. })));
		assert!(matches!(
			events.get(2),
			Some(Event::Finished { .. })
		));
		assert!(matches!(events.get(3), Some(Event::Created { .. })));
		assert!(matches!(
			events.get(4),
			Some(Event::PlanAdopted { .. })
		));
		assert!(service.policy().rail.len() == picks);
	}

	#[test]
	fn apply_edit_undo_redo_round_trip() {
		let s = Sandbox::new();
		let (a, _b, _c) = s.two_step();
		let mut service = s.service();

		service.create_draft(Base::Upstream(s.oid(&a)), false);
		service.wait_idle(WAIT);

		let id = first_pick(&service).unwrap();
		service
			.apply_edit(Edit::SetAction {
				id,
				action: Action::Drop,
			})
			.unwrap();
		let step = &service
			.session()
			.plan
			.as_ref()
			.unwrap()
			.step(id)
			.unwrap()
			.line;
		assert!(matches!(
			step.instruction(),
			Some(Instruction::Drop { .. })
		));
		assert_eq!(service.session().edits.len(), 1);

		service.undo().unwrap();
		assert!(matches!(
			service
				.session()
				.plan
				.as_ref()
				.unwrap()
				.step(id)
				.unwrap()
				.line
				.instruction(),
			Some(Instruction::Pick { .. })
		));
		assert_eq!(service.session().edits.len(), 0);
		assert!(service.undo().is_err(), "nothing left to undo");

		service.redo().unwrap();
		assert!(matches!(
			service
				.session()
				.plan
				.as_ref()
				.unwrap()
				.step(id)
				.unwrap()
				.line
				.instruction(),
			Some(Instruction::Drop { .. })
		));
		assert!(!service.redo().is_ok(), "nothing left to redo");

		// A stale id is an honest error string.
		let error = service
			.apply_edit(Edit::SetAction {
				id: StepId::FIRST.next().next().next().next(),
				action: Action::Drop,
			})
			.unwrap_err();
		assert!(!error.is_empty());
	}

	#[test]
	fn start_clean_runs_to_completion_and_closes() {
		let s = Sandbox::new();
		let (a, _b, c) = s.two_step();
		let mut service = s.service();

		service.create_draft(Base::Upstream(s.oid(&a)), false);
		service.wait_idle(WAIT);
		assert!(service.take_errors().is_empty());

		// Drop the first pick so the replay is guaranteed to
		// rewrite history.
		let drop_id = first_pick(&service).unwrap();
		service
			.apply_edit(Edit::SetAction {
				id: drop_id,
				action: Action::Drop,
			})
			.unwrap();

		service.start(EmptyPolicy::Stop, vec![]);
		service.wait_idle(WAIT);

		assert!(service.take_errors().is_empty());
		let tip = match &service.session().phase {
			journal::Phase::Finished(tip) => *tip,
			other => panic!("expected finished, got {other:?}"),
		};
		assert_eq!(
			s.ref_value("main").as_deref(),
			Some(tip.to_string().as_str())
		);
		assert_ne!(tip, s.oid(&c), "history was rewritten");
		assert_eq!(
			s.log_subjects(&format!("{a}..HEAD")),
			vec!["C".to_owned()],
			"the dropped commit's change is gone"
		);

		let recovery = format!("{RECOVERY_REF_PREFIX}/{}", &c[..7]);
		assert_eq!(
			s.ref_value(&recovery).as_deref(),
			Some(c.as_str()),
			"the recovery ref pins the original tip"
		);

		service.close();
		service.wait_idle(WAIT);
		assert_eq!(service.session().phase, journal::Phase::Closed);
		assert_eq!(s.ref_value(&recovery), None);
		assert!(service.take_errors().is_empty());
		assert_paired(&load_events(&service));
	}

	#[test]
	fn start_with_dirty_worktree_protects_and_restores() {
		let s = Sandbox::new();
		let (a, _b, _c) = s.two_step();
		let mut service = s.service();

		// Draft first (the capture refuses on a dirty worktree),
		// then dirty the worktree before the start.
		service.create_draft(Base::Upstream(s.oid(&a)), false);
		service.wait_idle(WAIT);
		assert!(service.take_errors().is_empty());
		s.write("f.txt", "dirty\n");

		service.start(EmptyPolicy::Stop, vec![]);
		service.wait_idle(WAIT);

		assert!(service.take_errors().is_empty());
		assert!(matches!(
			service.session().phase,
			journal::Phase::Finished(_)
		));
		let protection =
			service.session().protection.clone().unwrap();
		assert!(s.ref_value("refs/stash").is_some());
		assert!(load_events(&service)
			.iter()
			.any(|event| matches!(event, Event::Protected { .. })));

		service.restore_protection();
		service.wait_idle(WAIT);

		assert_eq!(s.read("f.txt"), "dirty\n");
		assert!(
			s.git_ok(&["stash", "list"]).stdout.is_empty(),
			"the consumed stash entry is dropped"
		);
		assert!(load_events(&service).iter().any(|event| matches!(
			event,
			Event::Restored { clean: true, .. }
		)));
		assert_eq!(
			service.session().protection.as_ref(),
			Some(&protection)
		);
		assert!(service.take_errors().is_empty());
	}

	#[test]
	fn conflict_stop_continue_and_completion() {
		let s = Sandbox::new();
		let (a, _b, _c, x) = s.conflicting();
		let mut service = s.service();

		service.create_draft(
			Base::Onto {
				upstream: s.oid(&a),
				onto: s.oid(&x),
			},
			false,
		);
		service.wait_idle(WAIT);
		service.start(EmptyPolicy::Stop, vec![]);
		service.wait_idle(WAIT);

		assert!(matches!(
			service.policy().phase,
			policy::Phase::Stopped(policy::Stop::Conflicts { .. })
		));
		assert!(service.take_errors().is_empty());

		// Continuing without resolving is git's refusal, not
		// ours: the session stays running, no error surfaces.
		service.r#continue(None);
		service.wait_idle(WAIT);
		assert_eq!(service.session().phase, journal::Phase::Running);
		assert!(service.take_errors().is_empty());
		assert!(matches!(
			service.policy().phase,
			policy::Phase::Stopped(policy::Stop::Conflicts { .. })
		));

		s.write("f.txt", "resolved\n");
		s.git_ok(&["add", "f.txt"]);
		// git opens the message editor when the resolved stop
		// is committed; the composed message answers it.
		service.r#continue(Some(String::from("B\n")));
		service.wait_idle(WAIT);

		assert!(matches!(
			service.session().phase,
			journal::Phase::Finished(_)
		));
		assert_eq!(
			s.log_subjects(&format!("{x}..HEAD")),
			vec!["C".to_owned(), "B".to_owned()],
			"newest first: the replayed program"
		);
		assert!(service.take_errors().is_empty());
		assert_paired(&load_events(&service));
	}

	#[test]
	fn abort_restores_the_original_tip() {
		let s = Sandbox::new();
		let (a, _b, c, x) = s.conflicting();
		let mut service = s.service();

		service.create_draft(
			Base::Onto {
				upstream: s.oid(&a),
				onto: s.oid(&x),
			},
			false,
		);
		service.wait_idle(WAIT);
		service.start(EmptyPolicy::Stop, vec![]);
		service.wait_idle(WAIT);
		assert!(matches!(
			service.session().phase,
			journal::Phase::Running
		));

		service.abort();
		service.wait_idle(WAIT);

		assert_eq!(service.session().phase, journal::Phase::Aborted);
		assert_eq!(s.head(), c, "abort restores the tip");
		assert_eq!(s.ref_value("main").as_deref(), Some(c.as_str()));
		assert!(service.take_errors().is_empty());
	}

	#[test]
	fn adopt_existing_adopts_external_rebase() {
		let s = Sandbox::new();
		let (a, _b, _c) = s.two_step();
		// An external interactive rebase, stopped at a failing
		// exec (the no-op sequence editor leaves git's todo).
		s.git_env(
			&["rebase", "-i", "--exec", "false", &a],
			&[("GIT_SEQUENCE_EDITOR", "true")],
		);
		assert!(s.repo().join(".git/rebase-merge").exists());

		let mut service = s.service();
		service.adopt_existing();
		service.wait_idle(WAIT);

		assert!(service.session().adopted);
		assert!(matches!(
			service.policy().phase,
			policy::Phase::Adopted { .. }
		));
		let scope = service.session().scope.as_ref().unwrap();
		assert_eq!(scope.base, Base::Upstream(s.oid(&a)));
		assert!(service.take_errors().is_empty());
	}

	#[test]
	fn bridge_message_flow_and_busy_gating() {
		let s = Sandbox::new();
		let (a, _b, _c) = s.two_step();
		let mut service = s.service();

		service.create_draft(Base::Upstream(s.oid(&a)), false);
		service.wait_idle(WAIT);
		let reword_id = first_pick(&service).unwrap();
		service
			.apply_edit(Edit::SetAction {
				id: reword_id,
				action: Action::Reword,
			})
			.unwrap();

		// A reword without a composed message: git will wait in
		// the bridge while the start command is in flight.
		service.start(EmptyPolicy::Stop, vec![]);
		assert!(service.is_busy());
		let stale_id = first_pick(&service).unwrap();
		assert!(
			service
				.apply_edit(Edit::SetAction {
					id: stale_id,
					action: Action::Drop,
				})
				.is_err(),
			"plan edits are refused while busy"
		);

		let answered = service.wait_until(WAIT, |service| {
			service.snapshot.bridge.is_some()
		});
		assert!(answered, "no bridge message request appeared");

		service.resolve_message(String::from("composed subject\n"));
		service.wait_idle(WAIT);

		assert!(matches!(
			service.session().phase,
			journal::Phase::Finished(_)
		));
		let subjects = s.log_subjects(&format!("{a}..HEAD"));
		assert!(
			subjects.contains(&String::from("composed subject")),
			"the reworded subject is on the rewritten commit: 			 {subjects:?}"
		);
		assert!(service.take_errors().is_empty());
	}

	#[test]
	fn journal_records_intent_before_every_effect() {
		let s = Sandbox::new();
		let (a, _b, _c) = s.two_step();
		let mut service = s.service();

		service.create_draft(Base::Upstream(s.oid(&a)), false);
		service.wait_idle(WAIT);
		service.start(EmptyPolicy::Stop, vec![]);
		service.wait_idle(WAIT);

		let events = load_events(&service);
		assert_paired(&events);
		assert!(events
			.iter()
			.any(|event| matches!(event, Event::Started { .. })));
		assert!(events
			.iter()
			.any(|event| matches!(event, Event::Completed { .. })));
	}

	#[test]
	fn failed_start_never_records_started() {
		let s = Sandbox::new();
		let (_a, _b, _c) = s.two_step();
		let mut service = s.service();

		// An orphan root commit as the base; it is pruned
		// before the start, so git cannot resolve the upstream
		// and the rebase never begins.
		s.git_ok(&["checkout", "-q", "--orphan", "scratch"]);
		s.write("x.txt", "x\n");
		let x = s.commit("X");
		s.git_ok(&["checkout", "-q", "main"]);

		service.create_draft(Base::Upstream(s.oid(&x)), false);
		service.wait_idle(WAIT);
		assert_eq!(service.session().phase, journal::Phase::Draft);
		assert!(service.take_errors().is_empty());

		s.git_ok(&["branch", "-D", "scratch"]);
		s.git_ok(&["reflog", "expire", "--expire=now", "--all"]);
		s.git_ok(&[
			"-c",
			"gc.cruftPacks=false",
			"gc",
			"--prune=now",
			"--quiet",
		]);
		assert!(
			!s.git(&["cat-file", "-e", &x]).status.success(),
			"the base is gone"
		);

		service.start(EmptyPolicy::Stop, vec![]);
		service.wait_idle(WAIT);

		assert_eq!(
			service.session().phase,
			journal::Phase::Draft,
			"a refused start never leaves the draft"
		);
		let events = load_events(&service);
		assert_paired(&events);
		assert!(events.iter().any(|event| matches!(
			event,
			Event::Command { intent } if intent == INTENT_START
		)));
		assert!(!events
			.iter()
			.any(|event| matches!(event, Event::Started { .. })));
		assert!(!service.take_errors().is_empty());
	}

	// ------------------------------------------------------------------
	// test-only wait helpers
	// ------------------------------------------------------------------

	impl RebaseService {
		/// Pump outcomes until no command or observation is in
		/// flight (or the timeout passes).
		fn wait_idle(&mut self, timeout: Duration) {
			let deadline = Instant::now() + timeout;
			loop {
				let _ = self.on_worker_event();
				if !self.is_busy() && !self.observing {
					return;
				}
				if Instant::now() >= deadline {
					break;
				}
				if let Ok(reply) = self
					.outcomes
					.recv_timeout(Duration::from_millis(20))
				{
					let _ = self.process_reply(reply);
				}
			}
			let _ = self.on_worker_event();
		}

		/// Pump observations until the predicate holds.
		fn wait_until(
			&mut self,
			timeout: Duration,
			mut predicate: impl FnMut(&Self) -> bool,
		) -> bool {
			let deadline = Instant::now() + timeout;
			while !predicate(self) {
				let _ = self.on_worker_event();
				if predicate(self) || Instant::now() >= deadline {
					break;
				}
				let _ = self.refresh();
				if let Ok(reply) = self
					.outcomes
					.recv_timeout(Duration::from_millis(20))
				{
					let _ = self.process_reply(reply);
				}
			}
			predicate(self)
		}
	}
}
