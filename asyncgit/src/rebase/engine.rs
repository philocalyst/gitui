//! The only code that mutates the repository or spawns git.
//!
//! An [`Engine`] takes a [`Cmd`], runs exactly one operation, and
//! answers with an [`Outcome`] that always carries a fresh
//! [`crate::rebase::snapshot::Snapshot`]. Callers never see a bare
//! `Result<()>` as the product result: a failed command plus the
//! repository it left behind is itself an outcome.
//!
//! ## Command lifecycle
//!
//! 1. The engine writes the command intent to the session
//!    journal (done by the caller via
//!    [`crate::rebase::journal`], *before* the effect runs —
//!    intent before effect).
//! 2. Exactly one child process runs (or a small file/ref
//!    operation happens directly).
//! 3. The engine re-observes and returns.
//!
//! ## Editor bridge
//!
//! `Start`/`Continue`/`EditTodo` runs set `GIT_SEQUENCE_EDITOR`
//! and `GIT_EDITOR` to gitui itself in
//! [`crate::rebase::helper`] mode, pointed at a fresh bridge
//! directory communicated through the environment. The helper
//! never shows a terminal editor; it applies prepared bytes or
//! blocks for a user-composed message. If gitui dies mid-request
//! the helper fails the edit on a timeout, leaving git stopped at
//! a recoverable boundary rather than hanging or inventing a
//! message.
//!
//! ## Correspondence commands are lock-free
//!
//! [`Cmd::SetBridgeMessage`] and [`Cmd::CancelBridgeMessage`]
//! must be answerable *while another command is in flight* (git
//! is blocked in the helper waiting for exactly that answer).
//! They only touch bridge files and observe without taking the
//! engine mutex; a read racing a running git may tear and is
//! repaired by the next observation — observed state wins over
//! remembered intent.
//!
//! ## Environment hygiene
//!
//! All editor variables are overridden for the child; the rest
//! of the user's environment (and thus their hooks, signing
//! setup, and config) is honored deliberately — silently
//! neutralizing user policy is not an option.

use crate::error::{Error, Result};
use crate::rebase::helper;
use crate::rebase::snapshot::{
	self, BridgeKind, BridgeRequest, Operation, RebaseFiles,
	RebaseState, Snapshot,
};
use crate::rebase::todo::Todo;
use crate::rebase::Generation;
use crate::sync::{CommitId, RepoPath};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Message recorded for stash commits gitui creates.
const STASH_MESSAGE: &str = "gitui-rebase";
/// Bridge file holding a prepared commit message.
const COMMIT_MESSAGE_FILE: &str = "commit-msg.txt";
/// Cadence of the parent heartbeat while git waits in the
/// helper.
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(200);

/// How the replay range is determined.
#[derive(
	Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
pub enum Base {
	/// Rebase everything after this commit onto it.
	Upstream(CommitId),
	/// Rebase everything after `upstream` onto `onto`
	/// (the `--onto` form).
	Onto {
		/// Exclusive start of the range.
		upstream: CommitId,
		/// New parent.
		onto: CommitId,
	},
	/// Replay the whole history, including the root
	/// (`--root`).
	Root,
}

/// What to do when a commit becomes empty while replaying.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	PartialEq,
	Eq,
	serde::Serialize,
	serde::Deserialize,
)]
pub enum EmptyPolicy {
	/// Stop and ask (git's `--empty=stop`).
	#[default]
	Stop,
	/// Drop silently (`--empty=drop`).
	Drop,
	/// Keep as empty commit (`--empty=keep`).
	Keep,
}

/// Options for starting a managed rebase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartOptions {
	/// Range and destination.
	pub base: Base,
	/// Empty-commit behavior.
	pub empty: EmptyPolicy,
	/// Preserve merge topology (`--rebase-merges`); the todo
	/// must be a topology program for this to be honest.
	pub rebase_merges: bool,
	/// Move these refs along (`--update-refs`, git ≥ 2.38).
	pub update_refs: Vec<String>,
}

/// A mutation or query the engine can run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cmd {
	/// Ask git to generate its canonical todo for a scope,
	/// aborting before any replay (the capture editor trick).
	/// The todo comes back in [`Outcome::generated_todo`].
	GenerateTodo {
		/// Scope of the would-be rebase.
		base: Base,
		/// `--rebase-merges` generation.
		rebase_merges: bool,
	},
	/// Start the rebase, applying our compiled todo through
	/// the sequence-editor bridge.
	Start {
		/// Compiled todo bytes.
		todo: Vec<u8>,
		/// Full option set.
		options: StartOptions,
	},
	/// Continue a stopped rebase.
	Continue {
		/// A message the user composed for the current stop
		/// (reword/squash), delivered through the bridge.
		message: Option<String>,
	},
	/// Skip the current step.
	Skip,
	/// Abort: ask git to restore its recorded original state.
	Abort,
	/// Quit: drop sequencer bookkeeping, keep current state.
	Quit,
	/// Rewrite the remaining todo of a stopped rebase
	/// (`git rebase --edit-todo` through the bridge).
	EditTodo {
		/// New remaining program bytes.
		todo: Vec<u8>,
	},
	/// Create a commit at an edit stop (amend or new child).
	Commit {
		/// Message for the commit.
		message: String,
		/// Amend the stopped commit instead of adding a child.
		amend: bool,
		/// Allow an empty result (empty-commit decisions).
		allow_empty: bool,
	},
	/// Create a recovery ref (compare-and-swap: must not
	/// exist). Returns its target.
	CreateRef {
		/// Full ref name, e.g. `refs/gitui/rebase/…`.
		reference: String,
		/// The commit to point at.
		target: CommitId,
	},
	/// Delete a gitui-owned ref (cleanup after close).
	DeleteRef {
		/// Full ref name.
		reference: String,
	},
	/// Protect local changes into a stash; returns the stash
	/// commit oid in [`Outcome::stash`].
	StashCreate {
		/// Include untracked files.
		include_untracked: bool,
	},
	/// Restore a protected stash by exact oid.
	StashApply {
		/// The stash commit recorded at protection time.
		oid: CommitId,
		/// Also drop the stash entry afterwards.
		drop: bool,
	},
	/// Resolve a pending bridge message request (see trait).
	SetBridgeMessage {
		/// The composed message.
		message: String,
	},
	/// Cancel a pending bridge message request; git will fail
	/// the step and stay stopped.
	CancelBridgeMessage,
}

/// What running one [`Cmd`] produced.
#[derive(Clone, Debug)]
pub struct Outcome {
	/// Exit status of the child (0 = success); file/ref
	/// operations use 0/1.
	pub code: i32,
	/// Raw stdout, retained for diagnostics.
	pub stdout: Vec<u8>,
	/// Raw stderr, retained for diagnostics.
	pub stderr: Vec<u8>,
	/// Fresh observation taken after the command.
	pub snapshot: Snapshot,
	/// Todo captured by the bridge (`GenerateTodo`, and
	/// every `Start`: git's canonical view of the range).
	pub generated_todo: Option<crate::rebase::todo::Todo>,
	/// Stash identity for `StashCreate`.
	pub stash: Option<CommitId>,
}

/// The seam between the domain and the real world.
pub trait Engine {
	/// Run one command. Implementations must serialize: at
	/// most one command in flight per repository.
	///
	/// The correspondence commands ([`Cmd::SetBridgeMessage`],
	/// [`Cmd::CancelBridgeMessage`]) are deliberately exempt:
	/// they answer a command that is already in flight.
	///
	/// # Errors
	///
	/// Errors mean *we could not run* (spawn failure, unreadable
	/// repository) — never "git returned nonzero", which is a
	/// legitimate [`Outcome`].
	fn exec(&self, cmd: Cmd) -> crate::error::Result<Outcome>;

	/// Take a fresh observation.
	///
	/// # Errors
	///
	/// When the repository cannot be read at all.
	fn observe(&self) -> crate::error::Result<Snapshot>;

	/// Where the session's journal lives (engines own the
	/// directory layout under the worktree's git dir).
	fn session_dir(&self) -> &std::path::Path;
}

/// The `git --version` of the engine's executable, for feature
/// gating by callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
	/// Major version.
	pub major: u32,
	/// Minor version.
	pub minor: u32,
}

/// The native-git engine: real `git` processes, real files.
#[derive(Debug)]
pub struct NativeEngine {
	/// Repository this engine owns.
	pub repo: RepoPath,
	/// Directory for the session journal and bridge state.
	pub session: std::path::PathBuf,
	/// Git executable to invoke.
	pub git: std::path::PathBuf,
	/// Git version of [`NativeEngine::git`].
	pub version: Version,
	/// Directory git runs in (worktree, or git dir when bare).
	workdir: PathBuf,
	/// The all-zero oid of this repository's object format
	/// (create-only CAS in `update-ref`).
	zero_oid: String,
	/// Serializes effectful commands.
	serial: Mutex<()>,
	/// Monotonic observation counter.
	generation: AtomicU64,
	/// Extra environment for child processes (sandboxing in
	/// tests; wrapper setups in production).
	extra_env: Vec<(String, std::ffi::OsString)>,
	/// Editor-bridge executable override (see [`NativeEngine::with_bridge_exe`]).
	bridge_exe_override: Option<PathBuf>,
}

/// Bridge setup for one exec (see [`crate::rebase::helper`]).
struct BridgeSpec {
	/// The mode file's content.
	mode: &'static str,
	/// `plan.todo` bytes (apply mode).
	plan: Option<Vec<u8>>,
	/// `prewritten.txt` bytes (message flow).
	prewritten: Option<Vec<u8>>,
	/// `commit-msg.txt` bytes (Commit).
	commit_message: Option<Vec<u8>>,
	/// Whether to heartbeat while the child runs.
	heartbeat: bool,
	/// Whether to read `generated.todo` afterwards.
	reads_generated: bool,
}

impl BridgeSpec {
	/// Capture mode: take git's todo, refuse to start.
	const fn capture() -> Self {
		Self {
			mode: helper::MODE_CAPTURE,
			plan: None,
			prewritten: None,
			commit_message: None,
			heartbeat: false,
			reads_generated: true,
		}
	}

	/// Apply mode: install `plan` in place of git's todo.
	/// Heartbeats: a reword/squash mid-replay may open a
	/// message request the UI answers while this runs.
	const fn apply(plan: Vec<u8>) -> Self {
		Self {
			mode: helper::MODE_APPLY,
			plan: Some(plan),
			prewritten: None,
			commit_message: None,
			heartbeat: true,
			reads_generated: true,
		}
	}

	/// Message mode with an optional prewritten message.
	const fn message(prewritten: Option<Vec<u8>>) -> Self {
		Self {
			mode: helper::MODE_MESSAGE,
			plan: None,
			prewritten,
			commit_message: None,
			heartbeat: true,
			reads_generated: false,
		}
	}

	/// Fail-fast mode hosting only a prepared commit message.
	const fn commit_message(message: Vec<u8>) -> Self {
		Self {
			mode: helper::MODE_FAIL,
			plan: None,
			prewritten: None,
			commit_message: Some(message),
			heartbeat: false,
			reads_generated: false,
		}
	}
}

/// The command result before observation.
struct RawOutcome {
	/// Exit status of the child (0 = success); file/ref
	/// operations use 0/1.
	code: i32,
	/// Raw stdout, retained for diagnostics.
	stdout: Vec<u8>,
	/// Raw stderr, retained for diagnostics.
	stderr: Vec<u8>,
	/// Todo captured by the bridge.
	generated: Option<Todo>,
	/// Stash identity for `StashCreate`.
	stash: Option<CommitId>,
}

impl RawOutcome {
	/// Collect a child's exit status and output streams.
	fn from_output(
		out: &Output,
		generated: Option<Todo>,
		stash: Option<CommitId>,
	) -> Self {
		Self {
			code: out.status.code().unwrap_or(-1),
			stdout: out.stdout.clone(),
			stderr: out.stderr.clone(),
			generated,
			stash,
		}
	}
}

/// The answer a correspondence command carries. Other
/// commands carry none.
enum BridgeAnswer {
	/// Deliver a composed message to the waiting helper.
	Resolve(String),
	/// Fail the edit; git stays stopped.
	Cancel,
}

/// The answer a command carries, if it is a correspondence
/// command.
fn bridge_answer(cmd: &Cmd) -> Option<BridgeAnswer> {
	match cmd {
		Cmd::SetBridgeMessage { message } => {
			Some(BridgeAnswer::Resolve(message.clone()))
		}
		Cmd::CancelBridgeMessage => Some(BridgeAnswer::Cancel),
		_ => None,
	}
}

/// Environment variables that would redirect a child git to a
/// different repository/index than the one this engine is bound
/// to. They are removed for every child: the engine's repo
/// comes from its [`RepoPath`], never from ambient state.
pub(crate) const REPO_ENV_STRIP: [&str; 7] = [
	"GIT_DIR",
	"GIT_WORK_TREE",
	"GIT_INDEX_FILE",
	"GIT_OBJECT_DIRECTORY",
	"GIT_ALTERNATE_OBJECT_DIRECTORIES",
	"GIT_COMMON_DIR",
	"GIT_NAMESPACE",
];

/// Prepare one child command: run in the repository, immune to
/// repo-redirecting ambient env.
fn prepare_child(cmd: &mut Command, workdir: &Path) {
	cmd.current_dir(workdir);
	for var in REPO_ENV_STRIP {
		cmd.env_remove(var);
	}
}

impl NativeEngine {
	/// Create an engine bound to a repository and a session
	/// directory, discovering the git executable. The
	/// executable is probed once (`--version`, object format)
	/// so failures surface at construction, not mid-rebase.
	///
	/// The environment variable `GITUI_GIT_PATH` overrides the
	/// executable (default: `git` from `PATH`).
	///
	/// # Errors
	///
	/// When no usable git executable exists in `PATH`.
	pub fn new(
		repo: RepoPath,
		session: std::path::PathBuf,
	) -> crate::error::Result<Self> {
		let git = std::env::var_os("GITUI_GIT_PATH")
			.map_or_else(|| PathBuf::from("git"), PathBuf::from);
		let workdir = repo.workdir().map_or_else(
			|| repo.gitpath().to_path_buf(),
			Path::to_path_buf,
		);

		let version_out = {
			let mut cmd = Command::new(&git);
			prepare_child(&mut cmd, &workdir);
			cmd.arg("--version").output()?
		};
		if !version_out.status.success() {
			return Err(Error::Generic(String::from(
				"git --version failed",
			)));
		}
		let version =
			parse_version(&version_out.stdout).ok_or_else(|| {
				Error::Generic(String::from(
					"unparsable git --version output",
				))
			})?;

		// Pre-2.22 git lacks the flag; sha1 is the honest
		// default there (CAS creation-only still holds for
		// 40 zero digits).
		let zero_oid = {
			let mut cmd = Command::new(&git);
			prepare_child(&mut cmd, &workdir);
			cmd.arg("rev-parse")
				.arg("--show-object-format")
				.output()
				.ok()
				.map_or_else(
					|| String::from("0").repeat(40),
					|out| zero_oid_for(&out),
				)
		};

		Ok(Self {
			repo,
			session,
			git,
			version,
			workdir,
			zero_oid,
			serial: Mutex::new(()),
			generation: AtomicU64::new(0),
			extra_env: Vec::new(),
			bridge_exe_override: None,
		})
	}

	/// Add environment variables for every child process.
	/// The engine deliberately inherits the user's
	/// environment (hooks, signing, config); this only
	/// *appends* deterministic overrides, last one wins.
	#[must_use]
	pub fn with_extra_env(
		mut self,
		extra: Vec<(String, std::ffi::OsString)>,
	) -> Self {
		self.extra_env.extend(extra);
		self
	}

	/// Override the executable git invokes as the editor
	/// bridge (defaults to this process, `argv[0]` as
	/// fallback). Testing seam; also usable when gitui runs
	/// under a wrapper.
	#[must_use]
	pub fn with_bridge_exe(mut self, exe: PathBuf) -> Self {
		self.bridge_exe_override = Some(exe);
		self
	}

	/// `<session>/bridge`, the per-exec protocol directory.
	fn bridge_path(&self) -> PathBuf {
		self.session.join("bridge")
	}

	fn ensure_session(&self) -> Result<()> {
		fs::create_dir_all(&self.session)?;
		Ok(())
	}

	/// Assemble a `git` command in the repository.
	fn git_command(&self) -> Command {
		let mut cmd = Command::new(&self.git);
		prepare_child(&mut cmd, &self.workdir);
		for (key, value) in &self.extra_env {
			cmd.env(key, value);
		}
		cmd
	}

	/// Run one plain git command.
	fn git(&self, args: &[&str]) -> Result<Output> {
		self.git_command().args(args).output().map_err(Into::into)
	}

	/// Run one git command with the editor variables pinned to
	/// our bridge in fail-fast fashion: no bridge directory
	/// exists, so the helper exits nonzero on any request.
	/// Insurance for commands that must never open an editor.
	fn git_protected(&self, args: &[&str]) -> Result<Output> {
		let mut cmd = self.git_command();
		cmd.args(args);
		self.set_editor_envs(&mut cmd)?;
		cmd.output().map_err(Into::into)
	}

	/// Point both editor variables at this executable's
	/// hidden bridge subcommand (sequence editor plain,
	/// message editor with the `msg` marker) and pass the
	/// bridge location and our pid through the environment.
	fn set_editor_envs(&self, cmd: &mut Command) -> Result<()> {
		let exe = self.editor_exe()?;
		let sequence = encode_editor_command(&exe);
		let message = format!("{sequence} msg");
		cmd.env("GIT_SEQUENCE_EDITOR", sequence);
		cmd.env("GIT_EDITOR", message);
		cmd.env(
			helper::BRIDGE_ENV,
			absolute_path(&self.bridge_path()),
		);
		cmd.env(helper::PID_ENV, std::process::id().to_string());
		Ok(())
	}

	/// The executable git invokes as the editor bridge: the
	/// engine's override, then `GITUI_REBASE_BRIDGE_EXE`, then
	/// this process (`argv[0]` as final fallback).
	fn editor_exe(&self) -> Result<PathBuf> {
		if let Some(path) = &self.bridge_exe_override {
			return Ok(path.clone());
		}
		bridge_exe()
	}

	/// (Re)create the bridge directory from a spec.
	fn reset_bridge(&self, spec: &BridgeSpec) -> Result<()> {
		let dir = self.bridge_path();
		let _ = fs::remove_dir_all(&dir);
		fs::create_dir_all(&dir)?;
		fs::write(
			dir.join(helper::MODE_FILE),
			format!("{}\n", spec.mode),
		)?;
		if let Some(plan) = &spec.plan {
			fs::write(dir.join(helper::PLAN_FILE), plan)?;
		}
		if let Some(prewritten) = &spec.prewritten {
			fs::write(dir.join(helper::PREWRITTEN_FILE), prewritten)?;
		}
		if let Some(message) = &spec.commit_message {
			fs::write(dir.join(COMMIT_MESSAGE_FILE), message)?;
		}
		Ok(())
	}

	/// Heartbeat: proves to the helper that this process is
	/// still alive while git blocks in a message request. The
	/// caller must stop and join before its exec returns.
	fn spawn_heartbeat(&self) -> (Arc<AtomicBool>, JoinHandle<()>) {
		let stop = Arc::new(AtomicBool::new(false));
		let alive = self.bridge_path().join(helper::ALIVE_FILE);
		let stop_flag = Arc::clone(&stop);
		let handle = std::thread::spawn(move || {
			while !stop_flag.load(Ordering::SeqCst) {
				let _ = fs::write(&alive, b"alive");
				std::thread::sleep(HEARTBEAT_INTERVAL);
			}
		});
		(stop, handle)
	}

	/// Run one git command behind a bridge spec: resets the
	/// bridge, heartbeats if requested, collects the generated
	/// todo, and removes the bridge afterwards.
	fn exec_with_bridge(
		&self,
		args: &[String],
		spec: &BridgeSpec,
	) -> Result<(Output, Option<Todo>)> {
		self.reset_bridge(spec)?;
		let mut cmd = self.git_command();
		cmd.args(args);
		self.set_editor_envs(&mut cmd)?;
		let heartbeat = if spec.heartbeat {
			Some(self.spawn_heartbeat())
		} else {
			None
		};
		let run = cmd.output();
		if let Some((stop, handle)) = heartbeat {
			stop.store(true, Ordering::SeqCst);
			let _ = handle.join();
		}
		let out = run?;
		let generated = if spec.reads_generated {
			fs::read(self.bridge_path().join(helper::GENERATED_FILE))
				.ok()
				.map(|bytes| Todo::parse(&bytes))
		} else {
			None
		};
		let _ = fs::remove_dir_all(self.bridge_path());
		Ok((out, generated))
	}

	/// `git rev-parse --verify`, `None` when absent.
	fn try_rev_parse(&self, rev: &str) -> Result<Option<CommitId>> {
		let out = self.git(&["rev-parse", "--verify", rev])?;
		if !out.status.success() {
			return Ok(None);
		}
		let text = String::from_utf8_lossy(&out.stdout);
		Ok(CommitId::from_str_unchecked(text.trim()).ok())
	}

	/// Position of a stash oid in the stash list (its
	/// `stash@{n}` name), `None` when absent.
	fn stash_position(&self, oid: CommitId) -> Result<Option<usize>> {
		let out = self.git(&["stash", "list", "--format=%H"])?;
		if !out.status.success() {
			return Ok(None);
		}
		let text = String::from_utf8_lossy(&out.stdout);
		Ok(text
			.lines()
			.position(|line| line.trim() == oid.to_string()))
	}

	/// The correspondence commands: pure bridge-file
	/// operations, answerable while another command runs.
	fn exec_correspondence(
		&self,
		answer: BridgeAnswer,
	) -> Result<Outcome> {
		self.deliver_bridge_message(answer)?;
		let snapshot = self.observe_locked()?;
		Ok(Outcome {
			code: 0,
			stdout: Vec::new(),
			stderr: Vec::new(),
			snapshot,
			generated_todo: None,
			stash: None,
		})
	}

	/// Write the answer to a pending message request into
	/// the bridge: `message.txt` resolves, `cancel` fails the
	/// edit (git stays stopped). Valid while a request is open
	/// or the bridge is in a mode whose helper may wait for
	/// one.
	fn deliver_bridge_message(
		&self,
		answer: BridgeAnswer,
	) -> Result<()> {
		let dir = self.bridge_path();
		let has_request = dir.join(helper::REQUEST_FILE).exists();
		let mode = fs::read(dir.join(helper::MODE_FILE))
			.map(|bytes| {
				String::from_utf8_lossy(&bytes).trim().to_owned()
			})
			.unwrap_or_default();
		let waiting = mode == helper::MODE_MESSAGE
			|| mode == helper::MODE_APPLY;
		if !has_request && !waiting {
			return Err(Error::Generic(String::from(
				"no pending bridge message request",
			)));
		}
		match answer {
			BridgeAnswer::Resolve(text) => fs::write(
				dir.join(helper::MESSAGE_FILE),
				text.as_bytes(),
			)?,
			BridgeAnswer::Cancel => {
				fs::write(dir.join(helper::CANCEL_FILE), b"")?;
			}
		}
		Ok(())
	}

	/// Run an effectful command. Callers hold the mutex.
	fn run_effect(&self, cmd: Cmd) -> Result<RawOutcome> {
		match cmd {
			Cmd::GenerateTodo {
				base,
				rebase_merges,
			} => self.generate_todo(&base, rebase_merges),
			Cmd::Start { todo, options } => self.start(todo, options),
			Cmd::Continue { message } => {
				self.continue_rebase(message)
			}
			Cmd::Skip => {
				let out =
					self.git_protected(&["rebase", "--skip"])?;
				Ok(RawOutcome::from_output(&out, None, None))
			}
			Cmd::Abort => {
				let out =
					self.git_protected(&["rebase", "--abort"])?;
				Ok(RawOutcome::from_output(&out, None, None))
			}
			Cmd::Quit => {
				let out =
					self.git_protected(&["rebase", "--quit"])?;
				Ok(RawOutcome::from_output(&out, None, None))
			}
			Cmd::EditTodo { todo } => self.edit_todo(todo),
			Cmd::Commit {
				message,
				amend,
				allow_empty,
			} => self.commit(message, amend, allow_empty),
			Cmd::CreateRef { reference, target } => {
				let out = self.git(&[
					"update-ref",
					&reference,
					&target.to_string(),
					&self.zero_oid,
				])?;
				Ok(RawOutcome::from_output(&out, None, None))
			}
			Cmd::DeleteRef { reference } => {
				let out =
					self.git(&["update-ref", "-d", &reference])?;
				Ok(RawOutcome::from_output(&out, None, None))
			}
			Cmd::StashCreate { include_untracked } => {
				self.stash_create(include_untracked)
			}
			Cmd::StashApply { oid, drop } => {
				self.stash_apply(oid, drop)
			}
			Cmd::SetBridgeMessage { .. }
			| Cmd::CancelBridgeMessage => Err(Error::Generic(String::from(
				"correspondence command reached the effect path",
			))),
		}
	}

	/// `Cmd::GenerateTodo`: capture git's canonical todo for a
	/// scope, aborting before any replay. The capture editor
	/// refuses (exit 1); git then aborts the start cleanly.
	/// An empty scope yields a `noop`-only program.
	fn generate_todo(
		&self,
		base: &Base,
		rebase_merges: bool,
	) -> Result<RawOutcome> {
		let mut args =
			vec![String::from("rebase"), String::from("-i")];
		args.extend(base_args(base));
		if rebase_merges {
			args.push(String::from("--rebase-merges"));
		}
		let (out, generated) =
			self.exec_with_bridge(&args, &BridgeSpec::capture())?;
		Ok(RawOutcome::from_output(&out, generated, None))
	}

	/// `Cmd::Start`: replay our compiled todo through the
	/// sequence-editor bridge.
	fn start(
		&self,
		todo: Vec<u8>,
		options: StartOptions,
	) -> Result<RawOutcome> {
		let StartOptions {
			base,
			empty,
			rebase_merges,
			update_refs,
		} = options;
		let mut args =
			vec![String::from("rebase"), String::from("-i")];
		args.extend(base_args(&base));
		args.push(empty_flag(empty));
		if !update_refs.is_empty() {
			args.push(String::from("--update-refs"));
		}
		if rebase_merges {
			args.push(String::from("--rebase-merges"));
		}
		let (out, generated) =
			self.exec_with_bridge(&args, &BridgeSpec::apply(todo))?;
		Ok(RawOutcome::from_output(&out, generated, None))
	}

	/// `Cmd::Continue`: resume a stopped rebase; the bridge
	/// answers any message editor git opens along the way.
	fn continue_rebase(
		&self,
		message: Option<String>,
	) -> Result<RawOutcome> {
		let spec =
			BridgeSpec::message(message.map(String::into_bytes));
		let args =
			vec![String::from("rebase"), String::from("--continue")];
		let (out, _unused) = self.exec_with_bridge(&args, &spec)?;
		Ok(RawOutcome::from_output(&out, None, None))
	}

	/// `Cmd::EditTodo`: install new remaining-program bytes in
	/// a stopped rebase.
	fn edit_todo(&self, todo: Vec<u8>) -> Result<RawOutcome> {
		let spec = BridgeSpec::apply(todo);
		let args =
			vec![String::from("rebase"), String::from("--edit-todo")];
		let (out, generated) = self.exec_with_bridge(&args, &spec)?;
		Ok(RawOutcome::from_output(&out, generated, None))
	}

	/// `Cmd::Commit`: commit at an edit stop from a prepared
	/// message file (hooks and cleanup run naturally; no
	/// editor may open).
	fn commit(
		&self,
		message: String,
		amend: bool,
		allow_empty: bool,
	) -> Result<RawOutcome> {
		let spec = BridgeSpec::commit_message(message.into_bytes());
		self.reset_bridge(&spec)?;
		let message_path =
			self.bridge_path().join(COMMIT_MESSAGE_FILE);
		let mut args = vec![
			String::from("commit"),
			format!("--file={}", message_path.display()),
		];
		if amend {
			args.push(String::from("--amend"));
		}
		if allow_empty {
			args.push(String::from("--allow-empty"));
		}
		let arg_refs: Vec<&str> =
			args.iter().map(String::as_str).collect();
		let out = self.git_protected(&arg_refs)?;
		let _ = fs::remove_dir_all(self.bridge_path());
		Ok(RawOutcome::from_output(&out, None, None))
	}

	/// `Cmd::StashCreate`: tracked changes via
	/// `stash create` + `stash store` (oid-stable, the stash
	/// reflog is only touched on store); untracked via
	/// `stash push --include-untracked` + reading `refs/stash`.
	/// A no-op push leaves the previous stash in place, so the
	/// recorded oid must be new.
	fn stash_create(
		&self,
		include_untracked: bool,
	) -> Result<RawOutcome> {
		if !include_untracked {
			let created = self.git(&["stash", "create"])?;
			let text = String::from_utf8_lossy(&created.stdout)
				.trim()
				.to_owned();
			if !created.status.success() || text.is_empty() {
				return Ok(RawOutcome::from_output(
					&created, None, None,
				));
			}
			let stored = self.git(&[
				"stash",
				"store",
				"-m",
				STASH_MESSAGE,
				&text,
			])?;
			let stash = CommitId::from_str_unchecked(&text).ok();
			return Ok(RawOutcome::from_output(&stored, None, stash));
		}
		let previous = self.try_rev_parse("refs/stash")?;
		let pushed = self.git(&[
			"stash",
			"push",
			"--include-untracked",
			"-m",
			STASH_MESSAGE,
		])?;
		if !pushed.status.success() {
			return Ok(RawOutcome::from_output(&pushed, None, None));
		}
		let current = self.try_rev_parse("refs/stash")?;
		let stash = if current.is_some() && current != previous {
			current
		} else {
			None
		};
		Ok(RawOutcome::from_output(&pushed, None, stash))
	}

	/// `Cmd::StashApply`: restore by exact oid (keeping the
	/// staged/unstaged split), optionally dropping the entry
	/// afterwards. Dropping needs the positional name; a
	/// missing entry is already gone and skipped.
	fn stash_apply(
		&self,
		oid: CommitId,
		drop: bool,
	) -> Result<RawOutcome> {
		let out = self.git(&[
			"stash",
			"apply",
			"--index",
			&oid.to_string(),
		])?;
		let mut raw = RawOutcome::from_output(&out, None, None);
		if drop && raw.code == 0 {
			if let Some(position) = self.stash_position(oid)? {
				let name = format!("stash@{{{position}}}");
				let dropped = self.git(&["stash", "drop", &name])?;
				raw = RawOutcome::from_output(&dropped, None, None);
			}
		}
		Ok(raw)
	}

	/// Observation without the mutex: reads only.
	fn observe_locked(&self) -> Result<Snapshot> {
		let generation = Generation(
			self.generation.fetch_add(1, Ordering::SeqCst),
		);
		let status = self.git(&[
			"status",
			"--porcelain=v2",
			"--branch",
			"-z",
			"--untracked-files=all",
		])?;
		if !status.status.success() {
			return Err(Error::Generic(format!(
				"git status failed: {}",
				String::from_utf8_lossy(&status.stderr)
			)));
		}
		let (head, branch, entries) =
			snapshot::parse_status(&status.stdout)?;
		let gitdir = self.absolute_git_dir()?;
		let operation = detect_operation(&gitdir);
		let rebase = read_rebase_state(&gitdir);
		let bridge = read_bridge_request(&self.bridge_path());
		Ok(Snapshot {
			generation,
			head,
			branch,
			operation,
			staged: entries.staged,
			worktree: entries.worktree,
			conflicts: entries.conflicts,
			untracked: entries.untracked,
			rebase,
			bridge,
		})
	}

	/// The repository's git dir, resolved once per
	/// observation (correct for linked worktrees).
	fn absolute_git_dir(&self) -> Result<PathBuf> {
		let out = self.git(&["rev-parse", "--absolute-git-dir"])?;
		if !out.status.success() {
			return Err(Error::Generic(format!(
				"git rev-parse failed: {}",
				String::from_utf8_lossy(&out.stderr)
			)));
		}
		Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
	}
}

/// `Base` as command-line arguments.
fn base_args(base: &Base) -> Vec<String> {
	match base {
		Base::Upstream(commit) => vec![commit.to_string()],
		Base::Onto { upstream, onto } => vec![
			String::from("--onto"),
			onto.to_string(),
			upstream.to_string(),
		],
		Base::Root => vec![String::from("--root")],
	}
}

/// `EmptyPolicy` as its git flag.
fn empty_flag(empty: EmptyPolicy) -> String {
	match empty {
		EmptyPolicy::Stop => String::from("--empty=stop"),
		EmptyPolicy::Drop => String::from("--empty=drop"),
		EmptyPolicy::Keep => String::from("--empty=keep"),
	}
}

impl Engine for NativeEngine {
	fn exec(&self, cmd: Cmd) -> Result<Outcome> {
		if let Some(answer) = bridge_answer(&cmd) {
			return self.exec_correspondence(answer);
		}
		let _guard = self.serial.lock()?;
		self.ensure_session()?;
		// Stale protocol state must never answer a new
		// command; non-bridge commands leave nothing behind.
		let _ = fs::remove_dir_all(self.bridge_path());
		let raw = self.run_effect(cmd)?;
		let snapshot = self.observe_locked()?;
		Ok(Outcome {
			code: raw.code,
			stdout: raw.stdout,
			stderr: raw.stderr,
			snapshot,
			generated_todo: raw.generated,
			stash: raw.stash,
		})
	}

	fn observe(&self) -> Result<Snapshot> {
		let _guard = self.serial.lock()?;
		self.observe_locked()
	}

	fn session_dir(&self) -> &std::path::Path {
		&self.session
	}
}

/// Encode a value for git's editor environment variables.
///
/// Git invokes these through a shell on POSIX and directly (with
/// its own quoting rules) on Windows, so the encoding is
/// platform-specific and central here. Only paths gitui itself
/// controls are ever embedded; user text travels via files.
#[must_use]
pub fn encode_editor_command(exe: &std::path::Path) -> String {
	let path = exe.to_string_lossy().into_owned();
	if cfg!(windows) {
		format!("\"{}\" --rebase-bridge", path.replace('"', ""))
	} else {
		format!("'{}' --rebase-bridge", path.replace('\'', "'\\''"))
	}
}

/// Locate this process for the editor bridge: the running
/// binary first, `argv[0]` as fallback. Tests (and wrapper
/// setups) override this via [`NativeEngine::with_bridge_exe`].
fn bridge_exe() -> Result<PathBuf> {
	std::env::current_exe().or_else(|_| {
		std::env::args().next().map(PathBuf::from).ok_or_else(|| {
			Error::Generic(String::from(
					"cannot locate the gitui executable for the editor bridge",
				))
		})
	})
}

/// Make a path absolute without requiring it to exist.
fn absolute_path(path: &Path) -> PathBuf {
	if path.is_absolute() {
		path.to_path_buf()
	} else {
		std::path::absolute(path)
			.unwrap_or_else(|_| path.to_path_buf())
	}
}

/// Parse `git --version` output (`git version 2.55.0`).
fn parse_version(stdout: &[u8]) -> Option<Version> {
	let text = String::from_utf8_lossy(stdout);
	let numbers = text.split_whitespace().nth(2)?;
	let mut parts = numbers.split('.');
	let major = parts.next()?.parse().ok()?;
	let minor = parts.next()?.parse().ok()?;
	Some(Version { major, minor })
}

/// The all-zero oid for CAS creation, per object format.
fn zero_oid_for(out: &Output) -> String {
	let text = String::from_utf8_lossy(&out.stdout);
	if out.status.success() && text.trim() == "sha256" {
		String::from("0").repeat(64)
	} else {
		String::from("0").repeat(40)
	}
}

/// Which git operation owns the repository, from its state
/// files under the git dir.
fn detect_operation(gitdir: &Path) -> Operation {
	let rebase_merge = gitdir.join("rebase-merge");
	if rebase_merge.is_dir() {
		return if rebase_merge.join("interactive").exists() {
			Operation::RebaseInteractive
		} else {
			Operation::Rebase
		};
	}
	let rebase_apply = gitdir.join("rebase-apply");
	if rebase_apply.is_dir() {
		return if rebase_apply.join("applying").exists() {
			Operation::Am
		} else {
			Operation::RebaseApply
		};
	}
	for (marker, operation) in [
		("MERGE_HEAD", Operation::Merge),
		("REVERT_HEAD", Operation::Revert),
		("CHERRY_PICK_HEAD", Operation::CherryPick),
	] {
		if gitdir.join(marker).exists() {
			return operation;
		}
	}
	if gitdir.join("sequencer").is_dir() {
		return Operation::Other;
	}
	Operation::Clean
}

/// Gather `rebase-merge/` (files only, sorted by name) for the
/// typed parser. `None` when absent or unidentifiable.
fn read_rebase_state(gitdir: &Path) -> Option<RebaseState> {
	let dir = gitdir.join("rebase-merge");
	if !dir.is_dir() {
		return None;
	}
	let mut files: Vec<(String, Vec<u8>)> = Vec::new();
	let entries = fs::read_dir(&dir).ok()?;
	for entry in entries.flatten() {
		let path = entry.path();
		if path.is_file() {
			let name =
				entry.file_name().to_string_lossy().into_owned();
			let bytes = fs::read(&path).ok()?;
			files.push((name, bytes));
		}
	}
	files.sort_by(|a, b| a.0.cmp(&b.0));
	let refs: Vec<(&str, &[u8])> = files
		.iter()
		.map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
		.collect();
	RebaseState::from_files(&RebaseFiles { files: &refs })
}

/// A pending helper request, if one is open. Requests only
/// exist in the message flow (the sequence editor answers
/// immediately), so the kind is always a message.
fn read_bridge_request(bridge: &Path) -> Option<BridgeRequest> {
	let bytes = fs::read(bridge.join(helper::REQUEST_FILE)).ok()?;
	let text = String::from_utf8_lossy(&bytes);
	let file = text.lines().next()?.trim().to_owned();
	if file.is_empty() {
		None
	} else {
		Some(BridgeRequest {
			kind: BridgeKind::Message,
			file,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::{encode_editor_command, Base, Cmd, Engine};
	use crate::rebase::snapshot::Operation;
	use crate::rebase::testutil::{
		conflicting_branch, start_opts, todo_of, two_step_branch,
		Sandbox,
	};
	use crate::sync::CommitId;
	use std::path::Path;
	use std::process::Command;
	use std::sync::Arc;
	use std::time::Duration;

	#[test]
	fn generate_todo_captures_canonical_todo() {
		let s = Sandbox::new();
		let (a, b, c) = two_step_branch(&s);
		let engine = s.engine();

		let out = engine
			.exec(Cmd::GenerateTodo {
				base: Base::Upstream(s.oid(&a)),
				rebase_merges: false,
			})
			.unwrap();

		assert_eq!(out.code, 1, "capture aborts the start");
		let todo = out.generated_todo.unwrap();
		let picks: Vec<CommitId> = todo
			.instructions()
			.filter(|(_, i)| i.is_pick_family())
			.filter_map(|(_, i)| i.commit())
			.collect();
		assert_eq!(picks, vec![s.oid(&b), s.oid(&c)]);
		assert_eq!(out.snapshot.operation, Operation::Clean);
		assert!(out.snapshot.rebase.is_none());

		// Up to date: git opens the editor once with a `noop`
		// program and refuses from there (exit 1); no commits
		// to replay is the signal callers present.
		let out = engine
			.exec(Cmd::GenerateTodo {
				base: Base::Upstream(s.oid(&c)),
				rebase_merges: false,
			})
			.unwrap();
		assert_eq!(out.code, 1);
		let picks = out
			.generated_todo
			.unwrap()
			.instructions()
			.filter(|(_, i)| i.is_pick_family())
			.count();
		assert_eq!(picks, 0, "up to date: nothing to replay");
		assert_eq!(out.snapshot.operation, Operation::Clean);
	}

	#[test]
	fn start_replays_reordered_plan() {
		let s = Sandbox::new();
		let (a, b, c) = two_step_branch(&s);
		let engine = s.engine();

		let todo =
			todo_of(&[format!("pick {c}"), format!("pick {b}")]);
		let out = engine
			.exec(Cmd::Start {
				todo,
				options: start_opts(Base::Upstream(s.oid(&a))),
			})
			.unwrap();

		assert_eq!(out.code, 0, "replay must complete");
		assert_eq!(out.snapshot.operation, Operation::Clean);
		assert!(out.snapshot.rebase.is_none());
		let head = s.head();
		assert_ne!(head, c, "reordered history is rewritten");
		assert_eq!(
			s.log_subjects(&format!("{a}..HEAD")),
			vec!["B".to_owned(), "C".to_owned()]
		);
		assert_eq!(s.read("b.txt"), "b\n");
		assert_eq!(
			s.ref_value("main").as_deref(),
			Some(head.as_str())
		);
	}

	#[test]
	fn start_stops_on_conflict() {
		let s = Sandbox::new();
		let (a, b, c, x) = conflicting_branch(&s);
		let engine = s.engine();

		let todo =
			todo_of(&[format!("pick {b}"), format!("pick {c}")]);
		let out = engine
			.exec(Cmd::Start {
				todo,
				options: start_opts(Base::Onto {
					upstream: s.oid(&a),
					onto: s.oid(&x),
				}),
			})
			.unwrap();

		assert_eq!(
			out.snapshot.operation,
			Operation::RebaseInteractive
		);
		assert!(!out.snapshot.conflicts.is_empty());
		let rebase = out.snapshot.rebase.unwrap();
		assert_eq!(rebase.current, Some(1));
		assert_eq!(rebase.total, 2);
		assert_eq!(rebase.stopped_sha, Some(s.oid(&b)));
		assert_eq!(rebase.onto, s.oid(&x));
		assert_eq!(rebase.orig_head, s.oid(&c));
	}

	#[test]
	fn continue_with_message_completes_squash() {
		let s = Sandbox::new();
		let (a, b, c, x) = conflicting_branch(&s);
		let engine = s.engine();

		let todo =
			todo_of(&[format!("pick {b}"), format!("squash {c}")]);
		let start = engine
			.exec(Cmd::Start {
				todo,
				options: start_opts(Base::Onto {
					upstream: s.oid(&a),
					onto: s.oid(&x),
				}),
			})
			.unwrap();
		assert_eq!(
			start.snapshot.operation,
			Operation::RebaseInteractive
		);

		// Resolve the conflict, then continue: git opens the
		// message editor for the squash; the prewritten
		// message answers it without blocking.
		s.write("f.txt", "resolved\n");
		s.git_ok(&["add", "f.txt"]);
		let out = engine
			.exec(Cmd::Continue {
				message: Some(String::from("melded subject\n")),
			})
			.unwrap();

		assert_eq!(out.code, 0, "continue must complete");
		assert_eq!(out.snapshot.operation, Operation::Clean);
		assert_eq!(
			s.log_subjects(&format!("{x}..HEAD")),
			vec!["melded subject".to_owned()]
		);
		assert_eq!(s.read("f.txt"), "resolved\n");
		assert_eq!(s.read("c.txt"), "c\n");
	}

	#[test]
	fn reword_during_start_answers_via_bridge() {
		let s = Sandbox::new();
		let (a, b, _c) = two_step_branch(&s);
		let engine = Arc::new(s.engine());
		let base = s.oid(&a);
		let todo =
			todo_of(&[format!("reword {b}"), String::from("# end")]);

		let runner = {
			let engine = Arc::clone(&engine);
			std::thread::spawn(move || {
				engine.exec(Cmd::Start {
					todo,
					options: start_opts(Base::Upstream(base)),
				})
			})
		};

		// The message request appears when git reaches the
		// reword step; answer it like the UI would.
		let request = s.path().join("gitui-session/bridge/request");
		let mut answered = false;
		for _ in 0..600 {
			if request.exists() {
				engine
					.exec(Cmd::SetBridgeMessage {
						message: String::from("reworded subject\n"),
					})
					.unwrap();
				answered = true;
				break;
			}
			std::thread::sleep(Duration::from_millis(50));
		}
		assert!(answered, "no bridge message request appeared");

		let out = runner.join().unwrap().unwrap();
		assert_eq!(out.code, 0);
		assert_eq!(out.snapshot.operation, Operation::Clean);
		assert_eq!(
			s.log_subjects(&format!("{a}..HEAD")),
			vec!["reworded subject".to_owned()]
		);
	}

	#[test]
	fn abort_restores_tip_quit_keeps_state() {
		let s = Sandbox::new();
		let (a, b, c, x) = conflicting_branch(&s);
		let engine = s.engine();
		let todo = todo_of(&[format!("pick {b}")]);
		let options = start_opts(Base::Onto {
			upstream: s.oid(&a),
			onto: s.oid(&x),
		});
		let start = || {
			engine
				.exec(Cmd::Start {
					todo: todo.clone(),
					options: options.clone(),
				})
				.unwrap()
		};

		let stopped = start();
		assert_eq!(
			stopped.snapshot.operation,
			Operation::RebaseInteractive
		);
		let aborted = engine.exec(Cmd::Abort).unwrap();
		assert_eq!(aborted.code, 0);
		assert_eq!(aborted.snapshot.operation, Operation::Clean);
		assert_eq!(s.head(), c, "abort restores the original tip");

		let stopped = start();
		assert_eq!(
			stopped.snapshot.operation,
			Operation::RebaseInteractive
		);
		let quit = engine.exec(Cmd::Quit).unwrap();
		assert_eq!(quit.code, 0);
		assert_eq!(quit.snapshot.operation, Operation::Clean);
		assert_eq!(s.head(), x, "quit keeps the moved state");
		assert_eq!(s.ref_value("main").as_deref(), Some(c.as_str()));
	}

	#[test]
	fn edit_todo_rewrites_remaining_program() {
		let s = Sandbox::new();
		let (a, b, _c, x) = conflicting_branch(&s);
		let engine = s.engine();

		let todo = todo_of(&[
			format!("pick {b}"),
			format!("pick {}", s.head()),
		]);
		let stopped = engine
			.exec(Cmd::Start {
				todo,
				options: start_opts(Base::Onto {
					upstream: s.oid(&a),
					onto: s.oid(&x),
				}),
			})
			.unwrap();
		assert_eq!(
			stopped.snapshot.operation,
			Operation::RebaseInteractive
		);

		s.write("f.txt", "resolved\n");
		s.git_ok(&["add", "f.txt"]);
		let edited = engine
			.exec(Cmd::EditTodo {
				todo: todo_of(&[format!(
					"drop {}",
					s.ref_value("main").unwrap()
				)]),
			})
			.unwrap();
		assert_eq!(edited.code, 0);

		let done = engine
			.exec(Cmd::Continue {
				message: Some(String::from("B\n")),
			})
			.unwrap();
		assert_eq!(done.code, 0);
		assert_eq!(done.snapshot.operation, Operation::Clean);
		assert_eq!(
			s.log_subjects(&format!("{x}..HEAD")),
			vec!["B".to_owned()],
			"the remaining pick was dropped"
		);
		assert_eq!(s.read("f.txt"), "resolved\n");
		assert!(
			!s.exists("c.txt"),
			"the dropped commit's file must be absent"
		);
	}

	#[test]
	fn create_ref_is_create_only() {
		let s = Sandbox::new();
		let (a, _b, _c) = two_step_branch(&s);
		let engine = s.engine();
		let reference = String::from("refs/gitui/test-cas");

		let created = engine
			.exec(Cmd::CreateRef {
				reference: reference.clone(),
				target: s.oid(&a),
			})
			.unwrap();
		assert_eq!(created.code, 0);
		assert_eq!(
			s.ref_value(&reference).as_deref(),
			Some(a.as_str())
		);

		let again = engine
			.exec(Cmd::CreateRef {
				reference: reference.clone(),
				target: s.oid(&a),
			})
			.unwrap();
		assert_ne!(again.code, 0, "CAS must refuse recreation");

		let deleted = engine
			.exec(Cmd::DeleteRef {
				reference: reference.clone(),
			})
			.unwrap();
		assert_eq!(deleted.code, 0);
		assert_eq!(s.ref_value(&reference), None);
	}

	#[test]
	fn stash_protects_and_restores_tracked_state() {
		let s = Sandbox::new();
		let (_a, _b, _c) = two_step_branch(&s);
		let engine = s.engine();
		s.write("f.txt", "dirty\n");

		let created = engine
			.exec(Cmd::StashCreate {
				include_untracked: false,
			})
			.unwrap();
		assert_eq!(created.code, 0);
		let stash = created.stash.unwrap();

		// Clearing the worktree is the caller's policy step
		// (stash create never touches it); then restore.
		s.git_ok(&["checkout", "--", "f.txt"]);
		let restored = engine
			.exec(Cmd::StashApply {
				oid: stash,
				drop: true,
			})
			.unwrap();
		assert_eq!(restored.code, 0);
		assert_eq!(s.read("f.txt"), "dirty\n");
		let list = s.git_ok(&["stash", "list"]).stdout;
		assert!(list.is_empty(), "dropped stash must leave no entry");
	}

	#[test]
	fn stash_create_with_untracked_clears_them() {
		let s = Sandbox::new();
		let (_a, _b, _c) = two_step_branch(&s);
		let engine = s.engine();
		s.write("new.txt", "untracked\n");

		let created = engine
			.exec(Cmd::StashCreate {
				include_untracked: true,
			})
			.unwrap();
		assert_eq!(created.code, 0);
		assert!(created.stash.is_some());
		assert!(
			!s.exists("new.txt"),
			"untracked files move into the stash"
		);
	}

	#[test]
	fn editor_command_is_shell_safe() {
		let path = if cfg!(windows) {
			Path::new(r"C:\Program Files\g ui'te.exe")
		} else {
			Path::new("/usr/local/bin/gi tui'e")
		};
		let encoded = encode_editor_command(path);
		if cfg!(windows) {
			assert_eq!(
				encoded,
				format!("\"{}\" --rebase-bridge", path.display())
			);
		} else {
			assert_eq!(
				encoded,
				"'/usr/local/bin/gi tui'\\''e' --rebase-bridge"
			);
			// Round trip through sh: the path must survive.
			let script = format!("printf '%s\\n' {encoded}");
			let out = Command::new("sh")
				.arg("-c")
				.arg(&script)
				.output()
				.unwrap();
			let echoed = String::from_utf8_lossy(&out.stdout);
			let first = echoed.lines().next().unwrap_or_default();
			assert_eq!(first, path.to_string_lossy());
		}
	}
}
