//! Shared test scaffolding for the rebase subsystem.
//!
//! A hermetic [`Sandbox`] (temp repository, pinned identity and
//! config), the POSIX-sh double of the editor bridge, and the
//! tiny repo fixtures the engine tests started out with. The
//! differential fuzzer (`diffuzz`) builds on the same pieces.
//!
//! Test scaffolding: failing fast via panic is the point, so the
//! usual lint gates are relaxed like in the sibling test modules.

#![allow(clippy::unwrap_used, clippy::panic)]

use crate::rebase::engine::{
	Base, NativeEngine, StartOptions, REPO_ENV_STRIP,
};
use crate::sync::{CommitId, RepoPath};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

/// Fixed ancient dates keep test commits deterministic;
/// rebased commits get fresh committer dates and are
/// therefore guaranteed distinct oids.
pub const OLD_DATE: &str = "2005-04-07T22:13:13 +0000";

/// POSIX-sh double of the gitui editor bridge, mirroring
/// `asyncgit::rebase::helper` (capture/apply/message,
/// prewritten, request and cancel). Tests exercise the
/// engine↔git↔editor loop against this; the helper's own
/// protocol logic is unit-tested in `helper.rs`.
pub const BRIDGE_SHIM: &str = r#"#!/bin/sh
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

/// A throwaway repository with a hermetic git environment: no
/// user config, fixed identity, no signing. All git traffic
/// goes through [`Sandbox::git`] so failures carry context.
pub struct Sandbox {
	dir: TempDir,
	env: Vec<(String, OsString)>,
}

impl Sandbox {
	pub fn new() -> Self {
		let dir = TempDir::new().unwrap();
		let global = dir.path().join("sandbox-global.gitconfig");
		fs::write(&global, b"[core]\n\tabbrev = 40\n").unwrap();
		let system = dir.path().join("sandbox-system.gitconfig");
		fs::write(&system, b"").unwrap();
		let env: Vec<(String, OsString)> = vec![
			(
				String::from("GIT_CONFIG_GLOBAL"),
				global.into_os_string(),
			),
			(
				String::from("GIT_CONFIG_SYSTEM"),
				system.into_os_string(),
			),
			(String::from("GIT_AUTHOR_NAME"), OsString::from("t")),
			(String::from("GIT_AUTHOR_EMAIL"), OsString::from("t@t")),
			(String::from("GIT_COMMITTER_NAME"), OsString::from("t")),
			(
				String::from("GIT_COMMITTER_EMAIL"),
				OsString::from("t@t"),
			),
		];
		let sandbox = Self { dir, env };
		sandbox.git_ok(&["init", "-q", "--initial-branch=main"]);
		sandbox
	}

	pub fn path(&self) -> PathBuf {
		self.dir.path().to_path_buf()
	}

	/// The hermetic environment, for callers that must spawn
	/// git themselves (fault injection).
	pub fn envs(&self) -> &[(String, OsString)] {
		&self.env
	}

	pub fn git(&self, args: &[&str]) -> Output {
		let extra: Vec<(&str, &str)> = self
			.env
			.iter()
			.map(|(k, v)| {
				(k.as_str(), v.to_str().unwrap_or_default())
			})
			.collect();
		git_in(self.dir.path(), args, &extra)
	}

	pub fn git_ok(&self, args: &[&str]) -> Output {
		let out = self.git(args);
		assert!(
			out.status.success(),
			"sandbox git {args:?} failed: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		out
	}

	pub fn write(&self, file: &str, content: &str) {
		fs::write(self.path().join(file), content).unwrap();
	}

	pub fn read(&self, file: &str) -> String {
		fs::read_to_string(self.path().join(file)).unwrap()
	}

	pub fn exists(&self, file: &str) -> bool {
		self.path().join(file).exists()
	}

	pub fn commit(&self, message: &str) -> String {
		self.git_ok(&["add", "-A"]);
		git_in(
			self.dir.path(),
			&["commit", "-m", message],
			&[
				("GIT_AUTHOR_DATE", OLD_DATE),
				("GIT_COMMITTER_DATE", OLD_DATE),
			],
		);
		self.head()
	}

	pub fn head(&self) -> String {
		String::from_utf8_lossy(
			&self.git_ok(&["rev-parse", "HEAD"]).stdout,
		)
		.trim()
		.to_owned()
	}

	pub fn ref_value(&self, name: &str) -> Option<String> {
		let out = self.git(&["rev-parse", "--verify", name]);
		if !out.status.success() {
			return None;
		}
		Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
	}

	pub fn log_subjects(&self, range: &str) -> Vec<String> {
		let out = self.git_ok(&["log", "--format=%s", range]);
		String::from_utf8_lossy(&out.stdout)
			.lines()
			.map(str::to_owned)
			.collect()
	}

	/// Parse a commit id (convenience for call sites that
	/// already hold a sandbox).
	#[allow(clippy::unused_self)]
	pub fn oid(&self, value: &str) -> CommitId {
		CommitId::from_str_unchecked(value).unwrap()
	}

	/// An engine over this repository bound to the named
	/// session directory (distinct sessions simulate a fresh
	/// gitui process after a crash). Unix tests wire the
	/// bridge shim in as the editor executable.
	pub fn engine_at(&self, session: &str) -> NativeEngine {
		let mut engine = NativeEngine::new(
			RepoPath::Workdir {
				gitdir: self.path().join(".git"),
				workdir: self.path(),
			},
			Some(self.path().join(session)),
		)
		.unwrap()
		.with_extra_env(self.env.clone());
		if cfg!(unix) {
			let shim = self.path().join("bridge-shim.sh");
			fs::write(&shim, BRIDGE_SHIM).unwrap();
			fs::set_permissions(
				&shim,
				fs::Permissions::from_mode(0o755),
			)
			.unwrap();
			engine =
				engine.with_bridge_exe(shim.canonicalize().unwrap());
		}
		engine
	}

	pub fn engine(&self) -> NativeEngine {
		self.engine_at("gitui-session")
	}
}

/// Run git in `dir` with `env` layered over the ambient
/// environment (repo-redirecting variables stripped).
pub fn git_in(
	dir: &Path,
	args: &[&str],
	env: &[(&str, &str)],
) -> Output {
	let mut cmd = Command::new("git");
	cmd.args(args).current_dir(dir);
	for var in REPO_ENV_STRIP {
		cmd.env_remove(var);
	}
	for (key, value) in env {
		cmd.env(key, value);
	}
	cmd.output().unwrap()
}

/// Base commit `A`, then `B` and `C` on separate files.
pub fn two_step_branch(
	sandbox: &Sandbox,
) -> (String, String, String) {
	sandbox.write("f.txt", "a\n");
	let base = sandbox.commit("A");
	sandbox.write("b.txt", "b\n");
	let left = sandbox.commit("B");
	sandbox.write("c.txt", "c\n");
	let right = sandbox.commit("C");
	(base, left, right)
}

/// `A`, `B`, `C` on `main` and a conflicting `X` on
/// `other` (both `B` and `X` rewrite `f.txt`; `C` only
/// adds `c.txt` and replays cleanly).
pub fn conflicting_branch(
	sandbox: &Sandbox,
) -> (String, String, String, String) {
	sandbox.write("f.txt", "a\n");
	let base = sandbox.commit("A");
	sandbox.write("f.txt", "b\n");
	let left = sandbox.commit("B");
	sandbox.write("c.txt", "c\n");
	let right = sandbox.commit("C");
	sandbox.git_ok(&["checkout", "-q", "-b", "other", &base]);
	sandbox.write("f.txt", "x\n");
	let other = sandbox.commit("X");
	sandbox.git_ok(&["checkout", "-q", "main"]);
	(base, left, right, other)
}

pub fn start_opts(base: Base) -> StartOptions {
	StartOptions {
		base,
		empty: crate::rebase::engine::EmptyPolicy::Stop,
		rebase_merges: false,
		update_refs: Vec::new(),
	}
}

pub fn todo_of(lines: &[String]) -> Vec<u8> {
	format!("{}\n", lines.join("\n")).into_bytes()
}
