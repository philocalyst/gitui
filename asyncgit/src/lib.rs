/*!
`AsyncGit` is a library that provides non-blocking access to Git
operations, enabling `GitUI` to perform potentially slow Git operations
in the background while keeping the user interface responsive.

It also provides synchronous Git operations.

It wraps libraries like git2 and gix.
*/

#![deny(
	mismatched_lifetime_syntaxes,
	unused_imports,
	unused_must_use,
	dead_code,
	unstable_name_collisions,
	unused_assignments,
	deprecated
)]
#![deny(clippy::all, clippy::perf, clippy::nursery, clippy::pedantic)]
#![deny(
	clippy::filetype_is_file,
	clippy::cargo,
	clippy::unwrap_used,
	clippy::panic,
	clippy::match_like_matches_macro,
	clippy::needless_update
	//TODO: get this in someday since expect still leads us to crashes sometimes
	// clippy::expect_used
)]
#![allow(
	clippy::module_name_repetitions,
	clippy::must_use_candidate,
	clippy::missing_errors_doc,
	clippy::empty_docs,
	clippy::unnecessary_debug_formatting
)]
//TODO:
#![allow(
	clippy::significant_drop_tightening,
	clippy::missing_panics_doc,
	clippy::multiple_crate_versions
)]

pub mod asyncjob;
mod blame;
pub mod branch_view;
mod branches;
pub mod cached;
mod commit_files;
mod diff;
pub mod diff_preview;
mod error;
mod fetch_job;
mod filter_commits;
pub mod graph;
mod progress;
mod pull;
mod push;
mod push_tags;
pub mod rebase;
pub mod remote_progress;
pub mod remote_tags;
mod revlog;
mod status;
pub mod structural;
pub mod sync;
mod tags;
mod treefiles;

pub use crate::{
	blame::{AsyncBlame, BlameParams},
	branch_view::{
		BranchViewCommit, BranchViewFile, BranchViewJob,
		BranchViewRequest, BranchViewResult,
	},
	branches::{AsyncBranchesJob, BranchListSnapshot, HeadIdentity},
	commit_files::{AsyncCommitFiles, CommitFilesParams},
	diff::{AsyncDiff, DiffParams, DiffType},
	error::{Error, Result},
	fetch_job::AsyncFetchJob,
	filter_commits::{AsyncCommitFilterJob, CommitFilterResult},
	progress::ProgressPercent,
	pull::{AsyncPull, FetchRequest},
	push::{AsyncPush, PushRequest},
	push_tags::{AsyncPushTags, PushTagsRequest},
	remote_progress::{RemoteProgress, RemoteProgressState},
	revlog::{AsyncLog, FetchStatus, LogRangeStatus},
	status::{AsyncStatus, StatusParams},
	sync::{
		diff::{DiffLine, DiffLineType, FileDiff},
		remotes::push::PushType,
		status::{StatusItem, StatusItemType},
	},
	tags::AsyncTags,
	treefiles::AsyncTreeFilesJob,
};
pub use git2::message_prettify;
use std::{
	collections::hash_map::DefaultHasher,
	hash::{Hash, Hasher},
};

/// this type is used to communicate events back through the channel
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AsyncGitNotification {
	/// this indicates that no new state was fetched but that a async process finished
	FinishUnchanged,
	///
	Status,
	///
	Diff,
	///
	Log,
	///
	FileLog,
	///
	CommitFiles,
	///
	CommitDetails,
	///
	Tags,
	///
	Push,
	///
	PushTags,
	///
	Pull,
	///
	Blame,
	///
	RemoteTags,
	///
	Fetch,
	///
	Branches,
	/// Branch comparison details are ready.
	BranchView,
	/// Bounded branch topology is available while exact relationship data loads.
	BranchViewTopology,
	/// External or native diff preview is ready.
	DiffPreview,
	/// Repository branch and remote metadata are ready.
	StatusMetadata,
	///
	TreeFiles,
	///
	CommitFilter,
	///
	Rebase,
}

/// helper function to calculate the hash of an arbitrary type that implements the `Hash` trait
pub fn hash<T: Hash + ?Sized>(v: &T) -> u64 {
	let mut hasher = DefaultHasher::new();
	v.hash(&mut hasher);
	hasher.finish()
}

///
#[cfg(feature = "trace-libgit")]
pub fn register_tracing_logging() -> bool {
	fn git_trace(level: git2::TraceLevel, msg: &[u8]) {
		log::info!("[{:?}]: {}", level, String::from_utf8_lossy(msg));
	}
	git2::trace_set(git2::TraceLevel::Trace, git_trace).is_ok()
}

///
#[cfg(not(feature = "trace-libgit"))]
pub fn register_tracing_logging() -> bool {
	true
}
