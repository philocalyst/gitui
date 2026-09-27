//! The editor-bridge helper: gitui answering git's editor calls.
//!
//! When the engine runs a rebase it points `GIT_SEQUENCE_EDITOR`
//! and `GIT_EDITOR` at gitui itself with a hidden subcommand and
//! a bridge directory passed through the environment. This module
//! is that subcommand's implementation. It is intentionally tiny,
//! pessimistic, and anything unexpected fails the edit (exit ≠ 0),
//! which leaves git stopped at a clean boundary instead of
//! guessing.
//!
//! ## Protocol
//!
//! The engine creates `<bridge>/` before spawning git and removes
//! it after. Files inside are the whole protocol:
//!
//! - `mode` — `capture` (save the todo, refuse to continue),
//!   `apply` (overwrite the todo from `plan.todo` if present),
//!   `message` (deliver a composed message on request), or
//!   `fail` (refuse every request).
//! - `plan.todo` — the program to install (sequence editor).
//! - `generated.todo` — what git generated, always saved.
//! - `prewritten.txt` — a message composed before the command
//!   ran; applied immediately when git asks for one.
//! - `request` — the pending message request: first line is the
//!   absolute file path, second the pid of the answering gitui.
//! - `message.txt` / `cancel` — the UI's answer to a request.
//! - `parent-alive` — heartbeat the answering gitui refreshes
//!   while a command is in flight.
//!
//! The two editor variables are distinguished by the command
//! line: the sequence editor is invoked as
//! `gitui --rebase-bridge <file>`, the message editor as
//! `gitui --rebase-bridge msg <file>` (see [`run_with_kind`]).
//!
//! ## Safety check (apply mode)
//!
//! Before installing `plan.todo` the helper verifies that every
//! pick-family commit git generated is still present in the plan.
//! A plan that would silently exclude commits from git's range
//! fails the start cleanly instead of rewriting history the user
//! never saw.
//!
//! ## Liveness
//!
//! While waiting for a message the helper polls for the answer
//! and for gitui's heartbeat. If the heartbeat goes stale the
//! helper fails the edit promptly: git ends the step with an
//! editor error and the repository stays stopped — recoverable,
//! never hanging, never accepting an empty message silently.

use crate::rebase::snapshot::BridgeKind;
use crate::rebase::todo::Todo;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Maximum time a message request waits for an answer before
/// failing the edit.
pub const MESSAGE_TIMEOUT: std::time::Duration =
	std::time::Duration::from_secs(600);

/// Poll cadence while waiting for an answer.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Heartbeat age after which the answering process is assumed
/// dead.
pub(crate) const HEARTBEAT_STALE: Duration = Duration::from_secs(5);

/// Grace period for the first heartbeat to show up.
pub(crate) const HEARTBEAT_GRACE: Duration = Duration::from_secs(10);

/// Bridge file holding the active mode.
pub(crate) const MODE_FILE: &str = "mode";
/// Mode: save what git generated, then fail the start.
pub(crate) const MODE_CAPTURE: &str = "capture";
/// Mode: install `plan.todo` in place of what git generated.
pub(crate) const MODE_APPLY: &str = "apply";
/// Mode: deliver a composed message when git asks for one.
pub(crate) const MODE_MESSAGE: &str = "message";
/// Mode: refuse every editor request.
pub(crate) const MODE_FAIL: &str = "fail";
/// Bridge file holding the program to install.
pub(crate) const PLAN_FILE: &str = "plan.todo";
/// Bridge file holding what git generated.
pub(crate) const GENERATED_FILE: &str = "generated.todo";
/// Bridge file marking a pending message request.
pub(crate) const REQUEST_FILE: &str = "request";
/// Bridge file holding the UI's composed message.
pub(crate) const MESSAGE_FILE: &str = "message.txt";
/// Bridge file telling the helper to fail the edit.
pub(crate) const CANCEL_FILE: &str = "cancel";
/// Bridge file holding a message composed before the run.
pub(crate) const PREWRITTEN_FILE: &str = "prewritten.txt";
/// Bridge file the answering process heartbeats.
pub(crate) const ALIVE_FILE: &str = "parent-alive";
/// Environment variable carrying the bridge directory.
pub(crate) const BRIDGE_ENV: &str = "GITUI_REBASE_BRIDGE";
/// Environment variable carrying the answering process id.
pub(crate) const PID_ENV: &str = "GITUI_REBASE_PID";

/// Entry point of the hidden subcommand. `file` is the path git
/// wants edited. Returns the process exit code.
#[must_use]
pub fn run(
	bridge_dir: &std::path::Path,
	file: &std::path::Path,
) -> i32 {
	run_with_kind(bridge_dir, file, BridgeKind::Sequence)
}

/// Like [`run`], for the editor kind the engine encoded into the
/// command line: the sequence editor gets no marker, the message
/// editor gets `msg` as the first argument.
#[must_use]
pub fn run_with_kind(
	bridge_dir: &std::path::Path,
	file: &std::path::Path,
	kind: BridgeKind,
) -> i32 {
	i32::from(execute(bridge_dir, file, kind).is_err())
}

/// Run one editor request. `Err` means: fail the edit.
fn execute(
	bridge_dir: &std::path::Path,
	file: &std::path::Path,
	kind: BridgeKind,
) -> Result<(), ()> {
	let mode = read_mode(bridge_dir)?;
	match kind {
		BridgeKind::Sequence => sequence(bridge_dir, file, &mode),
		BridgeKind::Message => message(bridge_dir, file, &mode),
	}
}

/// Read and normalize the mode file. Anything unreadable is a
/// failure: never guess.
fn read_mode(bridge_dir: &std::path::Path) -> Result<String, ()> {
	let bytes =
		std::fs::read(bridge_dir.join(MODE_FILE)).map_err(|_| ())?;
	Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
}

/// Sequence-editor requests: capture or apply the todo.
fn sequence(
	bridge_dir: &std::path::Path,
	file: &std::path::Path,
	mode: &str,
) -> Result<(), ()> {
	match mode {
		MODE_CAPTURE => {
			save_generated(bridge_dir, file)?;
			// Refuse to continue: git aborts the start cleanly
			// and the captured todo is ours.
			Err(())
		}
		MODE_APPLY => apply_sequence(bridge_dir, file),
		_ => Err(()),
	}
}

/// Save what git generated, byte-exact.
fn save_generated(
	bridge_dir: &std::path::Path,
	file: &std::path::Path,
) -> Result<(), ()> {
	let bytes = std::fs::read(file).map_err(|_| ())?;
	std::fs::write(bridge_dir.join(GENERATED_FILE), bytes)
		.map_err(|_| ())
}

/// Apply mode: save git's todo, then install the plan after the
/// coverage check. Without a plan the file is left untouched
/// (git then runs what it generated).
fn apply_sequence(
	bridge_dir: &std::path::Path,
	file: &std::path::Path,
) -> Result<(), ()> {
	let incoming = std::fs::read(file).map_err(|_| ())?;
	std::fs::write(bridge_dir.join(GENERATED_FILE), &incoming)
		.map_err(|_| ())?;
	let Ok(plan) = std::fs::read(bridge_dir.join(PLAN_FILE)) else {
		return Ok(());
	};
	if !covers_generated(&incoming, &plan) {
		// The plan would drop commits git wanted to replay.
		// Fail the start cleanly; the capture is saved.
		return Err(());
	}
	std::fs::write(file, plan).map_err(|_| ())
}

/// Whether every pick-family commit of `generated` is still
/// present in `plan` (drops count as present: they are explicit,
/// reviewable omissions).
fn covers_generated(generated: &[u8], plan: &[u8]) -> bool {
	let plan_commits: HashSet<crate::sync::CommitId> =
		Todo::parse(plan)
			.instructions()
			.filter(|(_, i)| i.is_pick_family())
			.filter_map(|(_, i)| i.commit())
			.collect();
	!Todo::parse(generated)
		.instructions()
		.filter(|(_, i)| i.is_pick_family())
		.filter_map(|(_, i)| i.commit())
		.any(|commit| !plan_commits.contains(&commit))
}

/// Message-editor requests: deliver a composed message. During
/// `apply` runs (reword/squash mid-replay) the same flow applies,
/// so a live UI can answer while the engine command is in flight.
fn message(
	bridge_dir: &std::path::Path,
	file: &std::path::Path,
	mode: &str,
) -> Result<(), ()> {
	match mode {
		MODE_MESSAGE | MODE_APPLY => message_flow(bridge_dir, file),
		_ => Err(()),
	}
}

/// Deliver a message for git's commit-message editor: a
/// prewritten one immediately, otherwise a request the UI
/// answers while we poll.
fn message_flow(
	bridge_dir: &std::path::Path,
	file: &std::path::Path,
) -> Result<(), ()> {
	if let Ok(prewritten) =
		std::fs::read(bridge_dir.join(PREWRITTEN_FILE))
	{
		return std::fs::write(file, prewritten).map_err(|_| ());
	}

	let request_path = bridge_dir.join(REQUEST_FILE);
	let file_abs = std::fs::canonicalize(file)
		.unwrap_or_else(|_| file.to_path_buf());
	let pid =
		std::env::var(PID_ENV).unwrap_or_else(|_| String::from("0"));
	let request = format!("{}\n{}\n", file_abs.display(), pid);
	std::fs::write(&request_path, request).map_err(|_| ())?;

	let started = Instant::now();
	let mut alive_seen = false;
	loop {
		if let Ok(message) =
			std::fs::read(bridge_dir.join(MESSAGE_FILE))
		{
			std::fs::write(file, message).map_err(|_| ())?;
			let _ = std::fs::remove_file(&request_path);
			return Ok(());
		}
		if bridge_dir.join(CANCEL_FILE).exists() {
			let _ = std::fs::remove_file(&request_path);
			return Err(());
		}
		if parent_gone(bridge_dir, &mut alive_seen, started) {
			let _ = std::fs::remove_file(&request_path);
			return Err(());
		}
		if started.elapsed() > MESSAGE_TIMEOUT {
			let _ = std::fs::remove_file(&request_path);
			return Err(());
		}
		std::thread::sleep(POLL_INTERVAL);
	}
}

/// Whether the answering process stopped heartbeating (stale
/// marker) or never started (grace period elapsed). `alive_seen`
/// records that a heartbeat existed, so a deleted marker is not
/// mistaken for a never-starting parent.
fn parent_gone(
	bridge_dir: &std::path::Path,
	alive_seen: &mut bool,
	started: Instant,
) -> bool {
	match std::fs::metadata(bridge_dir.join(ALIVE_FILE))
		.and_then(|m| m.modified())
	{
		Ok(mtime) => {
			*alive_seen = true;
			mtime.elapsed().is_ok_and(|age| age > HEARTBEAT_STALE)
		}
		Err(_) => !*alive_seen && started.elapsed() > HEARTBEAT_GRACE,
	}
}

#[cfg(test)]
mod tests {
	use super::{
		run_with_kind, CANCEL_FILE, GENERATED_FILE, MESSAGE_FILE,
		MODE_APPLY, MODE_CAPTURE, MODE_FAIL, MODE_FILE, MODE_MESSAGE,
		PLAN_FILE, PREWRITTEN_FILE, REQUEST_FILE,
	};
	use crate::rebase::snapshot::BridgeKind;
	use std::fs;
	use tempfile::TempDir;

	const TODO_A: &str =
		"pick 1111111111111111111111111111111111111111 # one\n";
	const TODO_B: &str =
		"pick 2222222222222222222222222222222222222222 # two\n";

	struct Bridge(TempDir);

	impl Bridge {
		fn new(mode: &str) -> Self {
			let dir = TempDir::new().unwrap();
			fs::write(dir.path().join(MODE_FILE), mode).unwrap();
			Self(dir)
		}

		fn path(&self) -> std::path::PathBuf {
			self.0.path().to_path_buf()
		}

		fn put(&self, name: &str, bytes: &[u8]) {
			fs::write(self.path().join(name), bytes).unwrap();
		}

		fn get(&self, name: &str) -> Option<Vec<u8>> {
			fs::read(self.path().join(name)).ok()
		}

		/// The file git hands the editor, inside the same
		/// scratch directory.
		fn edited(&self, bytes: &[u8]) -> std::path::PathBuf {
			let file = self.path().join("edited");
			fs::write(&file, bytes).unwrap();
			file
		}
	}

	#[test]
	fn capture_saves_and_refuses() {
		let bridge = Bridge::new(MODE_CAPTURE);
		let file = bridge.edited(TODO_A.as_bytes());
		let code = run_with_kind(
			&bridge.path(),
			&file,
			BridgeKind::Sequence,
		);
		assert_eq!(code, 1);
		assert_eq!(
			bridge.get(GENERATED_FILE).as_deref(),
			Some(TODO_A.as_bytes())
		);
	}

	#[test]
	fn apply_installs_plan() {
		let bridge = Bridge::new(MODE_APPLY);
		bridge.put(PLAN_FILE, TODO_A.as_bytes());
		let file = bridge.edited(TODO_A.as_bytes());
		let code = run_with_kind(
			&bridge.path(),
			&file,
			BridgeKind::Sequence,
		);
		assert_eq!(code, 0);
		assert_eq!(
			fs::read(&file).unwrap(),
			TODO_A.as_bytes(),
			"plan identical to generated: file unchanged"
		);
	}

	#[test]
	fn apply_without_plan_leaves_file() {
		let bridge = Bridge::new(MODE_APPLY);
		let file = bridge.edited(TODO_B.as_bytes());
		let code = run_with_kind(
			&bridge.path(),
			&file,
			BridgeKind::Sequence,
		);
		assert_eq!(code, 0);
		assert_eq!(fs::read(&file).unwrap(), TODO_B.as_bytes());
		assert!(bridge.get(GENERATED_FILE).is_some());
	}

	#[test]
	fn apply_refuses_plan_dropping_generated_commits() {
		// Needs the todo parser to extract generated oids.
		let bridge = Bridge::new(MODE_APPLY);
		bridge.put(PLAN_FILE, TODO_B.as_bytes());
		let file = bridge.edited(TODO_A.as_bytes());
		let code = run_with_kind(
			&bridge.path(),
			&file,
			BridgeKind::Sequence,
		);
		assert_eq!(code, 1);
		assert_eq!(
			fs::read(&file).unwrap(),
			TODO_A.as_bytes(),
			"refused plan must not touch git's file"
		);
		assert!(bridge.get(GENERATED_FILE).is_some());
	}

	#[test]
	fn fail_mode_and_missing_mode_refuse() {
		let bridge = Bridge::new(MODE_FAIL);
		let file = bridge.edited(TODO_A.as_bytes());
		assert_eq!(
			run_with_kind(
				&bridge.path(),
				&file,
				BridgeKind::Sequence
			),
			1
		);
		let empty = TempDir::new().unwrap();
		assert_eq!(
			run_with_kind(&empty.path(), &file, BridgeKind::Sequence),
			1
		);
	}

	#[test]
	fn message_prewritten_applies_immediately() {
		let bridge = Bridge::new(MODE_MESSAGE);
		bridge.put(PREWRITTEN_FILE, b"composed\n");
		let file = bridge.edited(b"original\n");
		let code =
			run_with_kind(&bridge.path(), &file, BridgeKind::Message);
		assert_eq!(code, 0);
		assert_eq!(fs::read(&file).unwrap(), b"composed\n");
	}

	#[test]
	fn message_cancel_fails_edit_and_clears_request() {
		let bridge = Bridge::new(MODE_MESSAGE);
		bridge.put(CANCEL_FILE, b"");
		let file = bridge.edited(b"original\n");
		let code =
			run_with_kind(&bridge.path(), &file, BridgeKind::Message);
		assert_eq!(code, 1);
		assert_eq!(fs::read(&file).unwrap(), b"original\n");
		assert!(
			bridge.get(REQUEST_FILE).is_none(),
			"cancelled request must be resolved"
		);
	}

	#[test]
	fn message_request_flow_answers_through_request() {
		// The message flow must also answer mid-replay
		// (apply mode): the request appears, then the UI
		// answers through MESSAGE_FILE.
		let bridge = Bridge::new(MODE_APPLY);
		let file = bridge.edited(b"original\n");
		let answer = bridge.path().join(MESSAGE_FILE);
		let watch = bridge.path().join(REQUEST_FILE);
		let watcher = std::thread::spawn(move || {
			for _ in 0..200 {
				if watch.exists() {
					fs::write(&answer, b"answered\n").unwrap();
					return;
				}
				std::thread::sleep(std::time::Duration::from_millis(
					10,
				));
			}
		});
		let code =
			run_with_kind(&bridge.path(), &file, BridgeKind::Message);
		watcher.join().unwrap();
		assert_eq!(code, 0);
		assert_eq!(fs::read(&file).unwrap(), b"answered\n");
		assert!(bridge.get(REQUEST_FILE).is_none());
	}
}
