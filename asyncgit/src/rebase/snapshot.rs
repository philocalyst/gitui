//! One consistent, typed observation of the repository.
//!
//! Everything the product knows about "what is true right now"
//! flows through [`Snapshot`]. The [`crate::rebase::engine`]
//! gathers raw bytes (a porcelain status run, the sequencer
//! directory, the bridge directory); this module turns them into
//! typed values with pure parsers that are tested without a
//! repository.
//!
//! ## Sources
//!
//! - `git status --porcelain=v2 --branch -z` for HEAD, branch,
//!   index, worktree, and — crucially — unmerged entries with all
//!   three stages (stage oids identify conflict sides without
//!   guessing from prose).
//! - the per-worktree `rebase-merge/` directory for sequencer
//!   state (progress, stop reason, remaining program, rewrites).
//! - gitui's bridge directory for pending editor requests.
//!
//! The snapshot never mutates anything and never launches
//! anything. It is cheap to take often and safe to take always.
//!
//! ## Byte safety
//!
//! Status paths and sequencer strings may be non-UTF-8. Parsers
//! here convert lossily: values are for display and matching, and
//! identity that must survive arbitrary bytes stays with the
//! engine (which keeps the raw bytes). The lossiness is
//! deliberate and confined to these entry points.

use crate::rebase::{todo::Todo, Generation};
use crate::sync::CommitId;

/// The whole observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
	/// Counter of this observation (bumped per take).
	pub generation: Generation,
	/// HEAD commit; `None` in an unborn repository.
	pub head: Option<CommitId>,
	/// Checked-out branch; `None` when detached.
	pub branch: Option<String>,
	/// Which git operation owns the repository, if any.
	pub operation: Operation,
	/// Index vs HEAD.
	pub staged: Vec<Change>,
	/// Worktree vs index.
	pub worktree: Vec<Change>,
	/// Unmerged entries, with stages.
	pub conflicts: Vec<Conflict>,
	/// Untracked paths.
	pub untracked: Vec<String>,
	/// Sequencer state, when a rebase owns the repository.
	pub rebase: Option<RebaseState>,
	/// Pending gitui bridge request, if any.
	pub bridge: Option<BridgeRequest>,
}

/// The operation holding the repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
	/// No operation in progress.
	Clean,
	/// Interactive rebase (merge backend).
	RebaseInteractive,
	/// Non-interactive rebase (merge backend).
	Rebase,
	/// Rebase using the apply backend (`rebase-apply`).
	RebaseApply,
	/// Merge.
	Merge,
	/// Revert.
	Revert,
	/// Cherry-pick.
	CherryPick,
	/// `git am`.
	Am,
	/// Something is holding state we do not model.
	Other,
}

impl Operation {
	/// Whether this operation is any rebase flavor.
	#[must_use]
	pub const fn is_rebase(self) -> bool {
		matches!(
			self,
			Self::RebaseInteractive
				| Self::Rebase
				| Self::RebaseApply
		)
	}
}

/// One path change, index or worktree side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
	/// Repository-relative path, `/`-separated.
	pub path: String,
	/// What happened.
	pub kind: ChangeKind,
}

/// The change taxonomy we need (deliberately small).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
	/// Added.
	Added,
	/// Modified.
	Modified,
	/// Deleted.
	Deleted,
	/// Renamed (path is the new name).
	Renamed,
	/// Type changed (file ⇄ symlink ⇄ …).
	TypeChange,
}

/// An unmerged path with its three stages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
	/// Repository-relative path.
	pub path: String,
	/// Stage oids: base (1), ours (2), theirs (3). A missing
	/// stage (e.g. delete/modify) is `None`.
	pub stages: [Option<CommitId>; 3],
	/// Whether the worktree file still carries conflict
	/// markers (a review aid; the index decides resolution).
	/// Always `false` when parsed from status; the engine
	/// sets it after reading files.
	pub markers_in_worktree: bool,
}

impl Conflict {
	/// The kinds of conflict this path represents.
	#[must_use]
	pub fn shapes(&self) -> Vec<ConflictShape> {
		shapes::of(self)
	}
}

/// Structural classification of a conflict path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictShape {
	/// Both sides modified content.
	Content,
	/// One side deleted, the other modified.
	ModifyDelete,
	/// Both sides added.
	BothAdded,
	/// Both sides renamed the file differently.
	RenameRename,
	/// Rename vs delete.
	RenameDelete,
	/// A directory became a file or vice versa.
	DirectoryFile,
}

/// Sequencer state read from `rebase-merge/`.
// The flags are git's own marker files; grouping them would
// only obscure the one-to-one mapping.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseState {
	/// The commit being replayed onto.
	pub onto: CommitId,
	/// Human-readable onto (branch or oid), if recorded.
	pub onto_name: Option<String>,
	/// `refs/heads/…` of the branch being rebased;
	/// `None` while detached.
	pub head_name: Option<String>,
	/// Whether the rebase was started interactively (git's
	/// `interactive` marker file is present). The engine
	/// decides the [`Operation`]; this records the
	/// observation.
	pub interactive: bool,
	/// The tip when the rebase started.
	pub orig_head: CommitId,
	/// Instructions already executed.
	pub done: Todo,
	/// Instructions still to run.
	pub todo: Todo,
	/// The todo as git generated it (canonical program).
	pub backup: Option<Todo>,
	/// 1-based index of the current step, when stopped
	/// mid-program (`msgnum`).
	pub current: Option<usize>,
	/// Total steps (`end`).
	pub total: usize,
	/// The commit at the stop, when the stop is about one.
	pub stopped_sha: Option<CommitId>,
	/// The message git will use for `--continue`, if staged.
	pub message: Option<String>,
	/// Original author environment of the stopped pick.
	pub author: Option<AuthorScript>,
	/// The commit being amended at an edit stop.
	pub amend: Option<CommitId>,
	/// Observed source→output rewrites (`rewritten-list`,
	/// `refs/rewritten/*`-derived where applicable).
	pub rewritten: Vec<(CommitId, CommitId)>,
	/// Rewrites recorded but not yet finalized
	/// (`rewritten-pending`); git folds these into
	/// `rewritten-list` as steps complete, so at a stop
	/// [`Self::rewritten`] is the authoritative list.
	pub rewritten_pending: Vec<(CommitId, CommitId)>,
	/// Whether intermediate refs are being moved
	/// (`--update-refs` marker present).
	pub update_refs: bool,
	/// Whether signing was requested for this rebase
	/// (`gpg_sign_opt` present).
	pub signing: bool,
	/// Sign that git reschedules failed `exec` steps.
	pub reschedule_failed_exec: bool,
}

/// Parsed `author-script`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorScript {
	/// `GIT_AUTHOR_NAME`.
	pub name: String,
	/// `GIT_AUTHOR_EMAIL`.
	pub email: String,
	/// `GIT_AUTHOR_DATE` (raw git date string).
	pub date: String,
}

/// A pending editor-bridge request from our helper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeRequest {
	/// What git wants edited.
	pub kind: BridgeKind,
	/// The file git passed.
	pub file: String,
}

/// What the bridge was asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeKind {
	/// The sequence editor (todo file).
	Sequence,
	/// A commit message editor (reword/squash).
	Message,
}

/// Bytes of the sequencer directory, as gathered by the engine.
/// Keys are file names within `rebase-merge/`.
pub struct RebaseFiles<'a> {
	/// name → contents
	pub files: &'a [(&'a str, &'a [u8])],
}

impl RebaseState {
	/// Assemble sequencer state from gathered file bytes.
	/// `None` when the directory is absent or too malformed
	/// to identify (the caller then treats it as unknown).
	///
	/// Recognized files (optional unless noted): `onto`
	/// (required), `orig-head` (required), `head-name`
	/// (required; the literal `detached HEAD` yields `None`
	/// for [`Self::head_name`]), `onto_name`, `msgnum`,
	/// `end`, `git-rebase-todo`, `done`,
	/// `git-rebase-todo.backup`, `stopped-sha`, `message`,
	/// `amend`, `author-script`, `rewritten-list`,
	/// `rewritten-pending`, `interactive`, `update-refs`,
	/// `gpg_sign_opt`, `no-reschedule-failed-exec`. Unknown
	/// files (`patch`, `quiet`, `drop_redundant_commits`, …)
	/// are ignored.
	#[must_use]
	pub fn from_files(files: &RebaseFiles<'_>) -> Option<Self> {
		let find = |name: &str| -> Option<&[u8]> {
			files
				.files
				.iter()
				.find(|(file, _)| *file == name)
				.map(|(_, bytes)| *bytes)
		};

		let onto = find("onto").and_then(oid_field)?;
		let orig_head = find("orig-head").and_then(oid_field)?;
		let head_name_bytes = find("head-name")?;
		let head_name = {
			let name = String::from_utf8_lossy(head_name_bytes);
			let name = name.trim();
			if name.is_empty() || name == "detached HEAD" {
				None
			} else {
				Some(name.to_owned())
			}
		};
		let onto_name = find("onto_name").and_then(|bytes| {
			let name = String::from_utf8_lossy(bytes);
			let name = name.trim();
			if name.is_empty() {
				None
			} else {
				Some(name.to_owned())
			}
		});

		let todo = find("git-rebase-todo")
			.map(Todo::parse)
			.unwrap_or_default();
		let done = find("done").map(Todo::parse).unwrap_or_default();
		let backup = find("git-rebase-todo.backup").map(Todo::parse);

		let current = find("msgnum").and_then(number_field);
		let total = find("end").and_then(number_field).unwrap_or(0);
		let stopped_sha = find("stopped-sha").and_then(oid_field);
		let message = find("message").map(|bytes| {
			let text = String::from_utf8_lossy(bytes);
			text.strip_suffix('\n').unwrap_or(&text).to_owned()
		});
		let author =
			find("author-script").and_then(parse_author_script);
		let amend = find("amend").and_then(oid_field);

		let rewritten = find("rewritten-list")
			.map(parse_rewritten)
			.unwrap_or_default();
		let rewritten_pending = find("rewritten-pending")
			.map(parse_rewritten)
			.unwrap_or_default();

		let interactive = find("interactive").is_some();
		let update_refs = find("update-refs").is_some();
		let signing = find("gpg_sign_opt").is_some();
		let reschedule_failed_exec =
			find("no-reschedule-failed-exec").is_none();

		Some(Self {
			onto,
			onto_name,
			head_name,
			interactive,
			orig_head,
			done,
			todo,
			backup,
			current,
			total,
			stopped_sha,
			message,
			author,
			amend,
			rewritten,
			rewritten_pending,
			update_refs,
			signing,
			reschedule_failed_exec,
		})
	}

	/// The remaining program length in executable steps.
	#[must_use]
	pub fn steps_left(&self) -> usize {
		self.todo
			.lines()
			.iter()
			.filter(|l| {
				!matches!(
					l,
					crate::rebase::todo::TodoLine::Comment(_)
						| crate::rebase::todo::TodoLine::Blank
				)
			})
			.count()
	}
}

/// Parse `git status --porcelain=v2 --branch -z` output.
/// Returns head, branch, and all path entries.
///
/// Records are NUL-separated; the last record keeps its
/// trailing NUL, and a missing one on the very tail is
/// tolerated. A renamed entry (`2 …`) carries two path fields,
/// each NUL-terminated — new path then original; only the new
/// path is kept, as [`ChangeKind::Renamed`]. Paths are decoded
/// lossily (see the module docs). Ignored entries (`! `) and
/// unrecognized `#`-headers (`branch.upstream`, `branch.ab`,
/// `stash.*`, …) are skipped.
///
/// # Errors
///
/// Fails only if the header is missing or malformed enough
/// that the observation cannot be attributed: no `#`-records
/// at all, an unparsable `branch.oid`, a structurally broken
/// record (short field counts, bad XY pair, missing rename
/// second path, unparsable unmerged stage oid), or an
/// unknown record type.
pub fn parse_status(
	bytes: &[u8],
) -> Result<
	(Option<CommitId>, Option<String>, StatusEntries),
	crate::error::Error,
> {
	let mut head = None;
	let mut branch = None;
	let mut entries = StatusEntries::default();
	let mut seen_header = false;

	let mut pos = 0;
	while pos < bytes.len() {
		let record = take_field(bytes, &mut pos);
		if record.is_empty() {
			continue;
		}
		match record[0] {
			b'#' => {
				seen_header = true;
				parse_header(record, &mut head, &mut branch)?;
			}
			b'1' => parse_ordinary(record, &mut entries)?,
			b'2' => {
				let orig_path = take_field(bytes, &mut pos);
				parse_rename(record, orig_path, &mut entries)?;
			}
			b'u' => parse_unmerged(record, &mut entries)?,
			b'?' => parse_untracked(record, &mut entries)?,
			b'!' => {}
			_ => {
				return Err(malformed("unknown record type"));
			}
		}
	}

	if !seen_header {
		return Err(malformed("no header records"));
	}

	Ok((head, branch, entries))
}

/// One NUL-terminated field starting at `pos`; advances
/// `pos` past the terminator, or to the end of input when
/// the field is the unterminated tail.
fn take_field<'a>(bytes: &'a [u8], pos: &mut usize) -> &'a [u8] {
	let end = bytes[*pos..]
		.iter()
		.position(|&b| b == 0)
		.map_or(bytes.len(), |at| *pos + at);
	let field = &bytes[*pos..end];
	*pos = if end < bytes.len() { end + 1 } else { end };
	field
}

fn malformed(detail: &str) -> crate::error::Error {
	crate::error::Error::Generic(format!(
		"malformed status output: {detail}"
	))
}

/// `# <key> <value>` header records.
fn parse_header(
	record: &[u8],
	head: &mut Option<CommitId>,
	branch: &mut Option<String>,
) -> Result<(), crate::error::Error> {
	let mut parts = record.splitn(3, |&b| b == b' ');
	if parts.next() != Some(&b"#"[..]) {
		return Err(malformed("bad header record"));
	}
	let key = parts.next().unwrap_or_default();
	let value = parts.next().unwrap_or_default();
	match key {
		b"branch.oid" => {
			let text = std::str::from_utf8(value)
				.map_err(|_| malformed("bad branch.oid"))?;
			*head = match text {
				"(initial)" | "(detached)" => None,
				_ => Some(parse_oid(value)?),
			};
		}
		b"branch.head" => {
			let name = String::from_utf8_lossy(value);
			*branch = if name == "(detached)" {
				None
			} else {
				Some(name.into_owned())
			};
		}
		// branch.upstream, branch.ab, stash.*, future keys.
		_ => {}
	}
	Ok(())
}

/// The change a porcelain v2 XY letter denotes on one side.
/// `None` for the "no change" letters (`.` and space; v2 uses
/// the dot where v1 uses a space). `M` and any other letter
/// (a newer git, or `C` for copies) is still a change; it
/// maps to the least specific kind rather than being dropped.
const fn side_kind(letter: u8) -> Option<ChangeKind> {
	match letter {
		b' ' | b'.' | b'?' => None,
		b'A' => Some(ChangeKind::Added),
		b'D' => Some(ChangeKind::Deleted),
		b'R' => Some(ChangeKind::Renamed),
		b'T' => Some(ChangeKind::TypeChange),
		// `M` and any unknown letter (a newer git, or `C`
		// for copies): still a change, least specific kind.
		_ => Some(ChangeKind::Modified),
	}
}

/// `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>` — the path
/// is the last field and may contain spaces, so the record is
/// split into at most nine fields.
fn parse_ordinary(
	record: &[u8],
	out: &mut StatusEntries,
) -> Result<(), crate::error::Error> {
	let parts: Vec<&[u8]> =
		record.splitn(9, |&b| b == b' ').collect();
	if parts.len() < 9 {
		return Err(malformed("short ordinary record"));
	}
	let xy = parts[1];
	if xy.len() != 2 {
		return Err(malformed("bad XY pair"));
	}
	let path = String::from_utf8_lossy(parts[8]).into_owned();
	if let Some(kind) = side_kind(xy[0]) {
		out.staged.push(Change {
			path: path.clone(),
			kind,
		});
	}
	if let Some(kind) = side_kind(xy[1]) {
		out.worktree.push(Change { path, kind });
	}
	Ok(())
}

/// `2 <XY> … <X><score> <path><NUL><origPath>` — staged
/// rename detection. The new path is kept; the original is
/// only required to be present (git pairs renames with it,
/// which we do not need).
fn parse_rename(
	record: &[u8],
	orig_path: &[u8],
	out: &mut StatusEntries,
) -> Result<(), crate::error::Error> {
	let parts: Vec<&[u8]> =
		record.splitn(10, |&b| b == b' ').collect();
	if parts.len() < 10 {
		return Err(malformed("short rename record"));
	}
	if orig_path.is_empty() {
		return Err(malformed("rename without original path"));
	}
	let xy = parts[1];
	if xy.len() != 2 {
		return Err(malformed("bad XY pair"));
	}
	let path = String::from_utf8_lossy(parts[9]).into_owned();
	if let Some(kind) = side_kind(xy[0]) {
		out.staged.push(Change {
			path: path.clone(),
			kind,
		});
	}
	if let Some(kind) = side_kind(xy[1]) {
		out.worktree.push(Change { path, kind });
	}
	Ok(())
}

/// `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`.
/// A stage oid of all zeroes marks the stage as absent
/// (delete/modify conflicts). The XY pair carries no
/// information we use; the stages are the truth.
fn parse_unmerged(
	record: &[u8],
	out: &mut StatusEntries,
) -> Result<(), crate::error::Error> {
	let parts: Vec<&[u8]> =
		record.splitn(11, |&b| b == b' ').collect();
	if parts.len() < 11 {
		return Err(malformed("short unmerged record"));
	}
	let stage = |bytes: &[u8]| {
		if bytes.iter().all(|&b| b == b'0') {
			Ok(None)
		} else {
			parse_oid(bytes).map(Some)
		}
	};
	out.conflicts.push(Conflict {
		path: String::from_utf8_lossy(parts[10]).into_owned(),
		stages: [
			stage(parts[7])?,
			stage(parts[8])?,
			stage(parts[9])?,
		],
		markers_in_worktree: false,
	});
	Ok(())
}

/// `? <path>` — untracked; the path is the rest of the
/// record and may contain spaces.
fn parse_untracked(
	record: &[u8],
	out: &mut StatusEntries,
) -> Result<(), crate::error::Error> {
	if record.len() < 2 || record[1] != b' ' {
		return Err(malformed("bad untracked record"));
	}
	out.untracked
		.push(String::from_utf8_lossy(&record[2..]).into_owned());
	Ok(())
}

/// Full hex object id; whitespace-trimmed. Must be exactly
/// this build's oid width (40, sha1): git never writes
/// abbreviated ids here, and git2's parser zero-pads partial
/// hex, which would turn garbage files into plausible oids.
/// 64-hex names sha256 explicitly — this build cannot
/// represent those oids at all, so the verdict says so.
fn parse_oid(bytes: &[u8]) -> Result<CommitId, crate::error::Error> {
	let text = std::str::from_utf8(bytes)
		.map_err(|_| malformed("bad object id"))?
		.trim();
	let all_hex = !text.is_empty()
		&& text.bytes().all(|b| b.is_ascii_hexdigit());
	if all_hex && text.len() == 64 {
		return Err(malformed(
			"object id looks like sha256, which this build \
			 does not support",
		));
	}
	if text.len() != 40 || !all_hex {
		return Err(malformed("bad object id"));
	}
	CommitId::from_str_unchecked(text)
		.map_err(|_| malformed("bad object id"))
}

/// Optional full hex object id from a sequencer file. See
/// [`parse_oid`] for why partial hex is refused rather than
/// zero-padded.
fn oid_field(bytes: &[u8]) -> Option<CommitId> {
	parse_oid(bytes).ok()
}

/// Optional decimal number from a sequencer file.
fn number_field(bytes: &[u8]) -> Option<usize> {
	std::str::from_utf8(bytes).ok()?.trim().parse().ok()
}

/// Parse `author-script`: the `KEY='value'` lines git writes
/// for the stopped pick. Any deviation (missing key, bad
/// quoting, non-UTF-8) yields `None`; the caller then simply
/// does not know the original author.
fn parse_author_script(bytes: &[u8]) -> Option<AuthorScript> {
	let text = std::str::from_utf8(bytes).ok()?;
	let mut name = None;
	let mut email = None;
	let mut date = None;
	for line in text.lines() {
		let (key, value) = line.split_once('=')?;
		let value = dequote_sq(value)?;
		match key {
			"GIT_AUTHOR_NAME" => name = Some(value),
			"GIT_AUTHOR_EMAIL" => email = Some(value),
			"GIT_AUTHOR_DATE" => date = Some(value),
			_ => {}
		}
	}
	Some(AuthorScript {
		name: name?,
		email: email?,
		date: date?,
	})
}

/// Undo git's single-quote shell quoting, as git's
/// `sq_dequote` does: `'a'\''b'` is `a'b`. Returns `None`
/// for anything that is not exactly one quoted word.
fn dequote_sq(value: &str) -> Option<String> {
	let bytes = value.as_bytes();
	if bytes.first() != Some(&b'\'') {
		return None;
	}
	let mut out = Vec::new();
	let mut closed = false;
	let mut i = 1;
	while i < bytes.len() {
		if bytes[i] == b'\'' {
			i += 1;
			if i == bytes.len() {
				closed = true;
				break;
			}
			// The `'\''` escape: closing quote, an escaped
			// literal quote, then the string re-opens.
			if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'\'') {
				out.push(b'\'');
				i += 2;
				if bytes.get(i) != Some(&b'\'') {
					return None;
				}
				i += 1;
			} else if bytes[i..].iter().all(u8::is_ascii_whitespace) {
				closed = true;
				break;
			} else {
				return None;
			}
		} else {
			out.push(bytes[i]);
			i += 1;
		}
	}
	if !closed {
		return None;
	}
	String::from_utf8(out).ok()
}

/// `rewritten-list` / `rewritten-pending`: lines of
/// `<old-oid> <new-oid>`. Unparseable lines are skipped
/// silently; rewrites are informational.
fn parse_rewritten(bytes: &[u8]) -> Vec<(CommitId, CommitId)> {
	let text = String::from_utf8_lossy(bytes);
	let mut pairs = Vec::new();
	for line in text.lines() {
		let mut fields = line.split_whitespace();
		if let (Some(old), Some(new)) = (fields.next(), fields.next())
		{
			if let (Ok(old), Ok(new)) = (
				CommitId::from_str_unchecked(old),
				CommitId::from_str_unchecked(new),
			) {
				pairs.push((old, new));
			}
		}
	}
	pairs
}

/// All path entries of one status run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusEntries {
	/// Index-side changes.
	pub staged: Vec<Change>,
	/// Worktree-side changes.
	pub worktree: Vec<Change>,
	/// Unmerged paths.
	pub conflicts: Vec<Conflict>,
	/// Untracked paths.
	pub untracked: Vec<String>,
}

/// Conflict-shape derivation (pure).
///
/// Only what the three stage oids can prove:
///
/// - base absent, both sides present → both sides created the
///   path ([`ConflictShape::BothAdded`]).
/// - base present, both sides present →
///   [`ConflictShape::Content`].
/// - base present, exactly one side present →
///   [`ConflictShape::ModifyDelete`].
/// - base absent, exactly one side present →
///   [`ConflictShape::RenameDelete`]. This is an
///   approximation: rename/delete needs the pair records
///   git emits around the conflict, which a status run does
///   not carry; the engine may refine the shape later.
/// - base present with no sides (or no stages at all) cannot
///   happen for a real unmerged entry → empty vec.
///
/// [`ConflictShape::RenameRename`] and
/// [`ConflictShape::DirectoryFile`] are never emitted here:
/// neither is determinable from stages alone. The engine may
/// still refine a conflict into one of those after consulting
/// rename records and the filesystem.
mod shapes {
	use super::{Conflict, ConflictShape};

	pub(super) fn of(c: &Conflict) -> Vec<ConflictShape> {
		match c.stages.into() {
			(None, Some(_), Some(_)) => {
				vec![ConflictShape::BothAdded]
			}
			(Some(_), Some(_), Some(_)) => {
				vec![ConflictShape::Content]
			}
			(Some(_), Some(_), None) | (Some(_), None, Some(_)) => {
				vec![ConflictShape::ModifyDelete]
			}
			(None, Some(_), None) | (None, None, Some(_)) => {
				vec![ConflictShape::RenameDelete]
			}
			_ => Vec::new(),
		}
	}
}

#[cfg(test)]
mod tests {
	#![allow(clippy::unwrap_used, clippy::panic)]

	use super::*;
	use std::{fs, path::Path, process::Command};
	use tempfile::TempDir;

	/// A throwaway home directory with a hermetic git
	/// config, so the user's real config (signing, identity,
	/// aliases) cannot leak into fixtures.
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
			self.try_git(repo, args)
				.unwrap_or_else(|| panic!("git {args:?} failed"))
		}

		/// Like [`Self::git`] but tolerant of git reporting
		/// failure, which a conflict stop *is*.
		fn try_git(
			&self,
			repo: &Path,
			args: &[&str],
		) -> Option<Vec<u8>> {
			let output = Command::new("git")
				.args(args)
				.current_dir(repo)
				// other tests may probe hostile ambient
				// repo-redirecting variables; never inherit
				// them here
				.env_remove("GIT_DIR")
				.env_remove("GIT_WORK_TREE")
				.env_remove("GIT_INDEX_FILE")
				.env_remove("GIT_OBJECT_DIRECTORY")
				.env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
				.env_remove("GIT_COMMON_DIR")
				.env_remove("GIT_NAMESPACE")
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
				.env("EDITOR", ":")
				.output()
				.unwrap();
			if output.status.success() {
				Some(output.stdout)
			} else {
				None
			}
		}
	}

	fn init_repo(sb: &Sandbox) -> TempDir {
		let dir = TempDir::new().unwrap();
		sb.git(dir.path(), &["init", "-q"]);
		dir
	}

	fn oid(hex: &str) -> CommitId {
		CommitId::from_str_unchecked(hex).unwrap()
	}

	fn change_of<'a>(
		changes: &'a [Change],
		path: &str,
	) -> &'a Change {
		changes
			.iter()
			.find(|c| c.path == path)
			.unwrap_or_else(|| panic!("no change for {path}"))
	}

	fn status_bytes(sb: &Sandbox, repo: &Path) -> Vec<u8> {
		sb.git(repo, &["status", "--porcelain=v2", "--branch", "-z"])
	}

	fn header_bytes() -> Vec<u8> {
		let mut bytes =
			b"# branch.oid 1111111111111111111111111111111111111111\x00"
				.to_vec();
		bytes.extend_from_slice(b"# branch.head main\x00");
		bytes
	}

	fn initial_commit(sb: &Sandbox, dir: &TempDir) {
		sb.git(
			dir.path(),
			&["commit", "-q", "--allow-empty", "-m", "init"],
		);
	}

	#[test]
	fn status_clean_repo() {
		let sb = Sandbox::new();
		let dir = init_repo(&sb);
		initial_commit(&sb, &dir);
		let (head, branch, entries) =
			parse_status(&status_bytes(&sb, dir.path())).unwrap();
		assert!(head.is_some());
		assert_eq!(branch.as_deref(), Some("main"));
		assert_eq!(entries, StatusEntries::default());
	}

	#[test]
	fn status_unborn_repo() {
		let sb = Sandbox::new();
		let dir = init_repo(&sb);
		let (head, branch, entries) =
			parse_status(&status_bytes(&sb, dir.path())).unwrap();
		assert_eq!(head, None);
		assert_eq!(branch.as_deref(), Some("main"));
		assert_eq!(entries, StatusEntries::default());
	}

	#[test]
	fn status_detached_head() {
		let sb = Sandbox::new();
		let dir = init_repo(&sb);
		initial_commit(&sb, &dir);
		sb.git(dir.path(), &["checkout", "-q", "--detach"]);
		let (head, branch, _) =
			parse_status(&status_bytes(&sb, dir.path())).unwrap();
		assert!(head.is_some());
		assert_eq!(branch, None);
	}

	#[test]
	fn status_staged_worktree_rename_untracked() {
		let sb = Sandbox::new();
		let dir = init_repo(&sb);
		for (name, content) in [
			("a.txt", "a\n"),
			("c.txt", "c\n"),
			("d.txt", "d\n"),
			("e.txt", "e\n"),
		] {
			fs::write(dir.path().join(name), content).unwrap();
		}
		sb.git(
			dir.path(),
			&["add", "a.txt", "c.txt", "d.txt", "e.txt"],
		);
		sb.git(dir.path(), &["commit", "-q", "-m", "tracked"]);

		// worktree-only modification
		fs::write(dir.path().join("a.txt"), "a2\n").unwrap();
		// staged modification
		fs::write(dir.path().join("d.txt"), "d2\n").unwrap();
		sb.git(dir.path(), &["add", "d.txt"]);
		// staged addition
		fs::write(dir.path().join("n.txt"), "n\n").unwrap();
		sb.git(dir.path(), &["add", "n.txt"]);
		// staged, then further worktree modification
		fs::write(dir.path().join("e.txt"), "e2\n").unwrap();
		sb.git(dir.path(), &["add", "e.txt"]);
		fs::write(dir.path().join("e.txt"), "e3\n").unwrap();
		// staged rename of a committed file
		sb.git(dir.path(), &["mv", "c.txt", "re named.txt"]);
		// untracked
		fs::write(dir.path().join("u.txt"), "u\n").unwrap();

		let (_, _, entries) =
			parse_status(&status_bytes(&sb, dir.path())).unwrap();

		assert_eq!(
			change_of(&entries.staged, "n.txt").kind,
			ChangeKind::Added
		);
		assert_eq!(
			change_of(&entries.staged, "d.txt").kind,
			ChangeKind::Modified
		);
		assert_eq!(
			change_of(&entries.staged, "e.txt").kind,
			ChangeKind::Modified
		);
		assert_eq!(
			change_of(&entries.staged, "re named.txt").kind,
			ChangeKind::Renamed
		);
		assert!(!entries.staged.iter().any(|c| c.path == "a.txt"));

		assert_eq!(
			change_of(&entries.worktree, "a.txt").kind,
			ChangeKind::Modified
		);
		assert_eq!(
			change_of(&entries.worktree, "e.txt").kind,
			ChangeKind::Modified
		);
		assert!(!entries
			.worktree
			.iter()
			.any(|c| c.path == "re named.txt"));

		assert_eq!(entries.untracked, vec!["u.txt"]);
		assert!(entries.conflicts.is_empty());
	}

	#[test]
	fn status_ignored_entries_skipped() {
		let sb = Sandbox::new();
		let dir = init_repo(&sb);
		fs::write(dir.path().join(".gitignore"), "secret\n").unwrap();
		fs::write(dir.path().join("secret"), "s\n").unwrap();
		let bytes = sb.git(
			dir.path(),
			&[
				"status",
				"--porcelain=v2",
				"--branch",
				"-z",
				"--ignored=matching",
			],
		);
		let (_, _, entries) = parse_status(&bytes).unwrap();
		assert_eq!(entries.untracked, vec![".gitignore"]);
		assert!(entries.staged.is_empty());
		assert!(entries.worktree.is_empty());
		assert_eq!(entries.conflicts.len(), 0);
	}

	#[test]
	fn status_and_rebase_state_at_conflict_stop() {
		let sb = Sandbox::new();
		let dir = init_repo(&sb);
		fs::write(dir.path().join("f.txt"), "base\n").unwrap();
		sb.git(dir.path(), &["add", "f.txt"]);
		sb.git(dir.path(), &["commit", "-q", "-m", "base"]);

		sb.git(dir.path(), &["checkout", "-q", "-b", "side"]);
		fs::write(dir.path().join("f.txt"), "side\n").unwrap();
		sb.git(dir.path(), &["commit", "-q", "-a", "-m", "side"]);
		let onto = String::from_utf8_lossy(
			&sb.git(dir.path(), &["rev-parse", "side"]),
		)
		.trim()
		.to_owned();

		sb.git(dir.path(), &["checkout", "-q", "main"]);
		fs::write(dir.path().join("f.txt"), "main\n").unwrap();
		sb.git(
			dir.path(),
			&["commit", "-q", "-a", "-m", "conflict-main"],
		);
		let orig_head = String::from_utf8_lossy(
			&sb.git(dir.path(), &["rev-parse", "main"]),
		)
		.trim()
		.to_owned();

		// Stop the interactive rebase on the conflict; the
		// nonzero exit *is* the expected outcome.
		let _ = sb.try_git(dir.path(), &["rebase", "-i", "side"]);

		// Observation while stopped.
		let (head, branch, entries) =
			parse_status(&status_bytes(&sb, dir.path())).unwrap();
		assert_eq!(head, Some(oid(&onto)));
		assert_eq!(branch, None);
		assert_eq!(entries.conflicts.len(), 1);
		let conflict = &entries.conflicts[0];
		assert_eq!(conflict.path, "f.txt");
		assert!(!conflict.markers_in_worktree);
		assert!(conflict.stages.iter().all(|stage| stage.is_some()));
		assert_eq!(conflict.shapes(), vec![ConflictShape::Content]);
		assert!(entries.staged.is_empty());
		assert!(entries.worktree.is_empty());
		assert!(entries.untracked.is_empty());

		// Sequencer directory → typed state.
		let merge_dir = dir.path().join(".git").join("rebase-merge");
		let gathered: Vec<(String, Vec<u8>)> =
			fs::read_dir(&merge_dir)
				.unwrap()
				.filter_map(|entry| {
					let entry = entry.unwrap();
					if entry.file_type().unwrap().is_file() {
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
		let files: Vec<(&str, &[u8])> = gathered
			.iter()
			.map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
			.collect();
		let state =
			RebaseState::from_files(&RebaseFiles { files: &files })
				.unwrap();

		assert_eq!(state.onto, oid(&onto));
		assert_eq!(state.orig_head, oid(&orig_head));
		assert_eq!(
			state.head_name.as_deref(),
			Some("refs/heads/main")
		);
		assert_eq!(state.onto_name, None);
		assert!(state.interactive);
		assert_eq!(state.current, Some(1));
		assert_eq!(state.total, 1);
		assert!(state.todo.lines().is_empty());
		assert_eq!(state.done.lines().len(), 1);
		let executed = state.done.instructions().next().unwrap().1;
		assert_eq!(executed.commit(), state.stopped_sha,);
		assert_eq!(state.stopped_sha, Some(oid(&orig_head)));
		let message = state.message.as_deref().unwrap_or_default();
		assert!(message.starts_with("conflict-main"));
		assert!(message.contains("Conflicts"));
		let author = state.author.as_ref().unwrap();
		assert_eq!(author.name, "name");
		assert_eq!(author.email, "email");
		assert!(author.date.starts_with('@'));
		assert_eq!(state.amend, None);
		assert!(!state.signing);
		assert!(!state.update_refs);
		// git 2.55 writes `no-reschedule-failed-exec` unless
		// `--reschedule-failed-exec` was passed.
		assert!(!state.reschedule_failed_exec);
		assert!(state.rewritten.is_empty());
		assert!(state.rewritten_pending.is_empty());
		assert!(state.backup.is_some());
		assert_eq!(state.backup.unwrap().instructions().count(), 1);
	}

	#[test]
	fn status_bytes_clean_and_headers_skipped() {
		let mut bytes = header_bytes();
		bytes.extend_from_slice(b"# branch.upstream origin/main\x00");
		bytes.extend_from_slice(b"# branch.ab +0 -0\x00");
		bytes.extend_from_slice(b"# stash 3\x00");
		let (head, branch, entries) = parse_status(&bytes).unwrap();
		assert_eq!(
			head,
			Some(oid("1111111111111111111111111111111111111111"))
		);
		assert_eq!(branch.as_deref(), Some("main"));
		assert_eq!(entries, StatusEntries::default());
	}

	#[test]
	fn status_bytes_rename_with_spaces() {
		let mut bytes = header_bytes();
		bytes.extend_from_slice(
			b"2 R. N... 100644 100644 100644 \
			  2222222222222222222222222222222222222222 \
			  3333333333333333333333333333333333333333 \
			  R100 re named.txt\x00old name.txt\x00",
		);
		let (_, _, entries) = parse_status(&bytes).unwrap();
		assert_eq!(
			entries.staged,
			vec![Change {
				path: "re named.txt".into(),
				kind: ChangeKind::Renamed,
			}]
		);
		assert!(entries.worktree.is_empty());
		assert!(entries.untracked.is_empty());
	}

	#[test]
	fn status_bytes_ordinary_sides_and_untracked() {
		let mut bytes = header_bytes();
		// staged addition
		bytes.extend_from_slice(
			b"1 A. N... 000000 100644 100644 \
			  0000000000000000000000000000000000000000 \
			  3333333333333333333333333333333333333333 new\x00",
		);
		// worktree-only modification, path with spaces
		bytes.extend_from_slice(
			b"1 .M N... 100644 100644 100644 \
			  2222222222222222222222222222222222222222 \
			  2222222222222222222222222222222222222222 \
			  my file.txt\x00",
		);
		// both sides modified
		bytes.extend_from_slice(
			b"1 MM N... 100644 100644 100644 \
			  2222222222222222222222222222222222222222 \
			  3333333333333333333333333333333333333333 both\x00",
		);
		// untracked; no trailing NUL on the tail record
		bytes.extend_from_slice(b"? tail.txt");
		let (_, _, entries) = parse_status(&bytes).unwrap();
		assert_eq!(
			entries.staged,
			vec![
				Change {
					path: "new".into(),
					kind: ChangeKind::Added,
				},
				Change {
					path: "both".into(),
					kind: ChangeKind::Modified,
				},
			]
		);
		assert_eq!(
			entries.worktree,
			vec![
				Change {
					path: "my file.txt".into(),
					kind: ChangeKind::Modified,
				},
				Change {
					path: "both".into(),
					kind: ChangeKind::Modified,
				},
			]
		);
		assert_eq!(entries.untracked, vec!["tail.txt"]);
	}

	#[test]
	fn status_bytes_untracked_non_utf8_path() {
		let mut bytes = header_bytes();
		bytes.extend_from_slice(b"? \xff\xfe\x00");
		let (_, _, entries) = parse_status(&bytes).unwrap();
		assert_eq!(entries.untracked, vec!["\u{fffd}\u{fffd}"]);
	}

	#[test]
	fn status_bytes_unmerged_stages() {
		let mut bytes = header_bytes();
		// content conflict: all three stages present
		bytes.extend_from_slice(
			b"u UU N... 100644 100644 100644 100644 \
			  1111111111111111111111111111111111111111 \
			  2222222222222222222222222222222222222222 \
			  3333333333333333333333333333333333333333 f\x00",
		);
		// delete/modify: the theirs stage is all zeroes
		bytes.extend_from_slice(
			b"u DU N... 100644 100644 100644 100644 \
			  1111111111111111111111111111111111111111 \
			  2222222222222222222222222222222222222222 \
			  0000000000000000000000000000000000000000 g\x00",
		);
		let (_, _, entries) = parse_status(&bytes).unwrap();
		assert_eq!(entries.conflicts.len(), 2);
		assert_eq!(
			entries.conflicts[0].stages,
			[
				Some(oid("1111111111111111111111111111111111111111")),
				Some(oid("2222222222222222222222222222222222222222")),
				Some(oid("3333333333333333333333333333333333333333")),
			]
		);
		assert!(!entries.conflicts[0].markers_in_worktree);
		assert_eq!(
			entries.conflicts[1].stages,
			[
				Some(oid("1111111111111111111111111111111111111111")),
				Some(oid("2222222222222222222222222222222222222222")),
				None,
			]
		);
	}

	#[test]
	fn status_bytes_ignored_entry_skipped() {
		let mut bytes = header_bytes();
		bytes.extend_from_slice(b"! ignored.txt\x00");
		let (_, _, entries) = parse_status(&bytes).unwrap();
		assert_eq!(entries, StatusEntries::default());
	}

	#[test]
	fn status_bytes_malformed_inputs() {
		// no header at all
		assert!(parse_status(b"").is_err());
		assert!(parse_status(b"1 .M N... x\x00").is_err());
		// unknown record type
		let mut bytes = header_bytes();
		bytes.extend_from_slice(b"9 weird\x00");
		assert!(parse_status(&bytes).is_err());
		// ordinary record with too few fields
		let mut bytes = header_bytes();
		bytes
			.extend_from_slice(b"1 .M N... 100644 100644 100644\x00");
		assert!(parse_status(&bytes).is_err());
		// XY pair of the wrong width
		let mut bytes = header_bytes();
		bytes.extend_from_slice(
			b"1 M N... 100644 100644 100644 a b f\x00",
		);
		assert!(parse_status(&bytes).is_err());
		// rename record without its second path field
		let mut bytes = header_bytes();
		bytes.extend_from_slice(
			b"2 R. N... 100644 100644 100644 \
			  2222222222222222222222222222222222222222 \
			  3333333333333333333333333333333333333333 \
			  R100 new\x00\x00",
		);
		assert!(parse_status(&bytes).is_err());
		// unparsable branch.oid
		let bytes =
			b"# branch.oid nope\x00# branch.head main\x00".to_vec();
		assert!(parse_status(&bytes).is_err());
		// unparsable unmerged stage oid
		let mut bytes = header_bytes();
		bytes.extend_from_slice(
			b"u UU N... 100644 100644 100644 100644 \
			  zz 2222222222222222222222222222222222222222 \
			  3333333333333333333333333333333333333333 f\x00",
		);
		assert!(parse_status(&bytes).is_err());
	}

	const OID_A: &str = "1111111111111111111111111111111111111111";
	const OID_B: &str = "2222222222222222222222222222222222222222";
	const OID_C: &str = "3333333333333333333333333333333333333333";
	const OID_D: &str = "4444444444444444444444444444444444444444";

	fn rebase_files(files: &[(&str, &[u8])]) -> RebaseState {
		RebaseState::from_files(&RebaseFiles { files })
			.unwrap_or_else(|| {
				panic!("from_files rejected {files:?}")
			})
	}

	#[test]
	fn rebase_state_minimal_files() {
		let state = rebase_files(&[
			("onto", OID_A.as_bytes()),
			("orig-head", OID_B.as_bytes()),
			("head-name", b"refs/heads/foo"),
		]);
		assert_eq!(state.onto, oid(OID_A));
		assert_eq!(state.orig_head, oid(OID_B));
		assert_eq!(
			state.head_name.as_deref(),
			Some("refs/heads/foo")
		);
		assert_eq!(state.onto_name, None);
		assert_eq!(state.current, None);
		assert_eq!(state.total, 0);
		assert_eq!(state.stopped_sha, None);
		assert_eq!(state.message, None);
		assert_eq!(state.author, None);
		assert_eq!(state.amend, None);
		assert!(state.todo.lines().is_empty());
		assert!(state.done.lines().is_empty());
		assert_eq!(state.backup, None);
		assert!(state.rewritten.is_empty());
		assert!(state.rewritten_pending.is_empty());
		assert!(!state.interactive);
		assert!(!state.update_refs);
		assert!(!state.signing);
		assert!(state.reschedule_failed_exec);
		assert_eq!(state.steps_left(), 0);
	}

	#[test]
	fn rebase_state_missing_required_files() {
		// onto missing
		let state = RebaseState::from_files(&RebaseFiles {
			files: &[
				("orig-head", OID_B.as_bytes()),
				("head-name", b"refs/heads/foo"),
			],
		});
		assert_eq!(state, None);
		// orig-head missing
		let state = RebaseState::from_files(&RebaseFiles {
			files: &[
				("onto", OID_A.as_bytes()),
				("head-name", b"refs/heads/foo"),
			],
		});
		assert_eq!(state, None);
		// head-name missing
		let state = RebaseState::from_files(&RebaseFiles {
			files: &[
				("onto", OID_A.as_bytes()),
				("orig-head", OID_B.as_bytes()),
			],
		});
		assert_eq!(state, None);
		// onto undecodable
		let state = RebaseState::from_files(&RebaseFiles {
			files: &[
				("onto", b"not-an-oid".as_slice()),
				("orig-head", OID_B.as_bytes()),
				("head-name", b"refs/heads/foo"),
			],
		});
		assert_eq!(state, None);
	}

	#[test]
	fn rebase_state_detached_head_name() {
		let state = rebase_files(&[
			("onto", OID_A.as_bytes()),
			("orig-head", OID_B.as_bytes()),
			("head-name", b"detached HEAD\n"),
		]);
		assert_eq!(state.head_name, None);
	}

	#[test]
	fn rebase_state_full() {
		let author_script = concat!(
			"GIT_AUTHOR_NAME='O'\\''Brien \"The\" Man'\n",
			"GIT_AUTHOR_EMAIL='o+b@x'\n",
			"GIT_AUTHOR_DATE='@1700000000 +0100'\n",
		);
		let state = rebase_files(&[
			("onto", OID_A.as_bytes()),
			("onto_name", b"main\n"),
			("head-name", b"refs/heads/topic"),
			("orig-head", OID_B.as_bytes()),
			("msgnum", b"2\n"),
			("end", b"5"),
			(
				"git-rebase-todo",
				format!("pick {OID_C} # reword me\n").as_bytes(),
			),
			(
				"done",
				format!("pick {OID_A} # one\npick {OID_B} # two\n")
					.as_bytes(),
			),
			(
				"git-rebase-todo.backup",
				format!("pick {OID_A} # one\n# comment\n").as_bytes(),
			),
			("stopped-sha", format!("{OID_D}\n").as_bytes()),
			("message", b"subject\n\nbody\n"),
			("amend", OID_C.as_bytes()),
			("author-script", author_script.as_bytes()),
			("interactive", b""),
			("update-refs", b""),
			("gpg_sign_opt", b"-S"),
			("no-reschedule-failed-exec", b""),
			(
				"rewritten-list",
				format!(
					"{OID_A} {OID_C}\n\
					 {OID_B} {OID_D}\n\
					 garbage line\n"
				)
				.as_bytes(),
			),
			(
				"rewritten-pending",
				format!("{OID_B} {OID_C}\n").as_bytes(),
			),
			// ignored files
			("quiet", b""),
			("drop_redundant_commits", b""),
		]);

		assert_eq!(state.onto, oid(OID_A));
		assert_eq!(state.onto_name.as_deref(), Some("main"));
		assert_eq!(
			state.head_name.as_deref(),
			Some("refs/heads/topic")
		);
		assert!(state.interactive);
		assert_eq!(state.orig_head, oid(OID_B));
		assert_eq!(state.current, Some(2));
		assert_eq!(state.total, 5);
		assert_eq!(state.todo.lines().len(), 1);
		assert_eq!(state.steps_left(), 1);
		assert_eq!(state.done.lines().len(), 2);
		assert_eq!(
			state
				.done
				.instructions()
				.map(|(_, i)| i.commit())
				.collect::<Vec<_>>(),
			vec![Some(oid(OID_A)), Some(oid(OID_B)),]
		);
		let backup = state.backup.unwrap();
		assert_eq!(backup.lines().len(), 2);
		assert_eq!(backup.instructions().count(), 1);
		assert_eq!(state.stopped_sha, Some(oid(OID_D)));
		// only the final newline is stripped
		assert_eq!(state.message.as_deref(), Some("subject\n\nbody"));
		assert_eq!(
			state.author,
			Some(AuthorScript {
				name: "O'Brien \"The\" Man".into(),
				email: "o+b@x".into(),
				date: "@1700000000 +0100".into(),
			})
		);
		assert_eq!(state.amend, Some(oid(OID_C)));
		assert_eq!(
			state.rewritten,
			vec![(oid(OID_A), oid(OID_C)), (oid(OID_B), oid(OID_D)),]
		);
		assert_eq!(
			state.rewritten_pending,
			vec![(oid(OID_B), oid(OID_C))]
		);
		assert!(state.update_refs);
		assert!(state.signing);
		assert!(!state.reschedule_failed_exec);
	}

	#[test]
	fn rebase_state_author_script_failures() {
		fn files(author: &[u8]) -> [(&str, &[u8]); 4] {
			[
				("onto", OID_A.as_bytes()),
				("orig-head", OID_B.as_bytes()),
				("head-name", b"refs/heads/foo"),
				("author-script", author),
			]
		}
		// missing key
		assert_eq!(
			rebase_files(&files(b"GIT_AUTHOR_NAME='a'\n")).author,
			None
		);
		// unquoted value
		assert_eq!(
			rebase_files(&files(b"GIT_AUTHOR_NAME=a\n")).author,
			None
		);
		// unterminated quote
		assert_eq!(
			rebase_files(&files(b"GIT_AUTHOR_NAME='a\n")).author,
			None
		);
		// non-UTF-8
		assert_eq!(
			rebase_files(&files(b"GIT_AUTHOR_NAME='a\xffb'\n"))
				.author,
			None
		);
	}

	#[test]
	fn rebase_state_garbage_oid_files_degrade() {
		// Partial hex must not be zero-padded into plausible
		// oids: git2's parser would, so the field parser
		// refuses anything but full-width hex.
		for (name, value) in [
			("onto", "abc".to_string()),
			("onto", "1".to_string()),
			("orig-head", "".to_string()),
			("stopped-sha", "0123456789abcdef".to_string()),
			("amend", "zz".to_string()),
		] {
			let mut files: Vec<(&str, &[u8])> =
				vec![("head-name", b"refs/heads/x")];
			for (required, bytes) in [
				("onto", OID_A.as_bytes()),
				("orig-head", OID_A.as_bytes()),
			] {
				if required != name {
					files.push((required, bytes));
				}
			}
			files.push((name, value.as_bytes()));
			let state = RebaseState::from_files(&RebaseFiles {
				files: &files,
			});
			if name == "onto" || name == "orig-head" {
				assert_eq!(state, None, "{name}={value}");
			} else {
				let state = state.unwrap();
				assert_eq!(state.stopped_sha, None, "{name}");
				assert_eq!(state.amend, None, "{name}");
			}
		}
		// 64-hex names sha256, which this build cannot
		// represent at all: refused, never padded or guessed.
		let files: Vec<(&str, &[u8])> = vec![
			(
				"onto",
				"1111111111111111111111111111111111111111111111111111111111111111"
					.as_bytes(),
			),
			("orig-head", OID_A.as_bytes()),
			("head-name", b"refs/heads/x"),
		];
		assert_eq!(
			RebaseState::from_files(&RebaseFiles { files: &files }),
			None
		);
	}

	/// porcelain v2 `-z` never quotes: a newline inside a
	/// path arrives raw and must survive parsing unchanged.
	/// Also locks the sha256 verdict for `branch.oid`.
	#[test]
	fn status_z_paths_are_raw_bytes() {
		let sb = Sandbox::new();
		let dir = init_repo(&sb);
		let weird = "we\nird.txt";
		fs::write(dir.path().join(weird), "x\n").unwrap();
		let (head, _, entries) =
			parse_status(&status_bytes(&sb, dir.path())).unwrap();
		assert!(head.is_none(), "unborn: no oid yet");
		assert_eq!(entries.untracked, vec![weird]);

		let sha = dir.path().join("sha256");
		fs::create_dir_all(&sha).unwrap();
		sb.git(&sha, &["init", "-q", "--object-format=sha256", "."]);
		sb.git(&sha, &["commit", "-q", "--allow-empty", "-m", "i"]);
		let bytes = sb.git(
			&sha,
			&["status", "--porcelain=v2", "--branch", "-z"],
		);
		let error = parse_status(&bytes).unwrap_err().to_string();
		assert!(
			error.contains("sha256"),
			"expected a sha256 verdict, got: {error}"
		);
	}

	#[test]
	fn shapes_classification() {
		let base = oid(OID_A);
		let ours = oid(OID_B);
		let theirs = oid(OID_C);
		let conflict = |stages: [Option<CommitId>; 3]| Conflict {
			path: "f".into(),
			stages,
			markers_in_worktree: false,
		};

		assert_eq!(
			conflict([None, Some(ours), Some(theirs)]).shapes(),
			vec![ConflictShape::BothAdded]
		);
		assert_eq!(
			conflict([Some(base), Some(ours), Some(theirs)]).shapes(),
			vec![ConflictShape::Content]
		);
		assert_eq!(
			conflict([Some(base), Some(ours), None]).shapes(),
			vec![ConflictShape::ModifyDelete]
		);
		assert_eq!(
			conflict([Some(base), None, Some(theirs)]).shapes(),
			vec![ConflictShape::ModifyDelete]
		);
		assert_eq!(
			conflict([None, Some(ours), None]).shapes(),
			vec![ConflictShape::RenameDelete]
		);
		assert_eq!(
			conflict([None, None, Some(theirs)]).shapes(),
			vec![ConflictShape::RenameDelete]
		);
		// impossible stage combinations → no shape
		assert!(conflict([Some(base), None, None])
			.shapes()
			.is_empty());
		assert!(conflict([None, None, None]).shapes().is_empty());
		// rename/directory shapes are never derived here
		assert_eq!(
			conflict([Some(base), Some(ours), Some(theirs)])
				.shapes()
				.iter()
				.any(|shape| matches!(
					shape,
					ConflictShape::RenameRename
						| ConflictShape::DirectoryFile
				)),
			false
		);
	}

	#[test]
	fn dequote_sq_cases() {
		assert_eq!(dequote_sq("'a'"), Some("a".into()));
		assert_eq!(dequote_sq("''"), Some(String::new()));
		assert_eq!(dequote_sq("'a'\\''b'"), Some("a'b".into()));
		assert_eq!(
			dequote_sq("'@1700000000 +0100'"),
			Some("@1700000000 +0100".into())
		);
		assert_eq!(dequote_sq("bare"), None);
		assert_eq!(dequote_sq("'unterminated"), None);
		assert_eq!(dequote_sq("'trailing'x"), None);
		assert_eq!(dequote_sq("'a' b"), None);
	}
}
