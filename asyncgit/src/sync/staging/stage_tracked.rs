use super::{
	apply_selection_with_pairs, validate_selected_pairs,
	DiffLineReplacement,
};
use crate::{
	error::{Error, Result},
	sync::{
		diff::{DiffLinePosition, FileDiff},
		patches::get_file_diff_patch,
		patches::patch_get_hunklines,
		repository::repo,
		RepoPath,
	},
};
use easy_cast::Conv;
use git2::{ErrorCode, IndexEntry, IndexTime};
use scopetime::scope_time;
use std::{
	collections::HashSet,
	fs,
	io::Write,
	path::{Component, Path, PathBuf},
	process::{Command, Stdio},
};

#[derive(Clone, Copy)]
struct StageLineSelection<'a> {
	positions: &'a [DiffLinePosition],
	pairs: &'a [DiffLineReplacement],
	expected_diff: Option<&'a FileDiff>,
}

#[derive(Clone, Copy)]
struct IndexContentState {
	is_stage: bool,
	missing_index_entry: bool,
	new_staged_file: bool,
}

///
pub fn stage_lines(
	repo_path: &RepoPath,
	file_path: &str,
	is_stage: bool,
	lines: &[DiffLinePosition],
) -> Result<()> {
	stage_lines_inner(
		repo_path,
		file_path,
		is_stage,
		lines,
		&[],
		None,
	)
}

/// Stage selected lines only when their displayed source lines are still
/// present with the same content.
pub fn stage_lines_checked(
	repo_path: &RepoPath,
	file_path: &str,
	is_stage: bool,
	lines: &[DiffLinePosition],
	expected_diff: &FileDiff,
) -> Result<()> {
	stage_lines_inner(
		repo_path,
		file_path,
		is_stage,
		lines,
		&[],
		Some(expected_diff),
	)
}

/// Stage or unstage explicitly paired compact replacement rows while
/// preserving the original source-line ordering.
pub fn stage_lines_with_pairs_checked(
	repo_path: &RepoPath,
	file_path: &str,
	is_stage: bool,
	lines: &[DiffLinePosition],
	pairs: &[DiffLineReplacement],
	expected_diff: &FileDiff,
) -> Result<()> {
	stage_lines_inner(
		repo_path,
		file_path,
		is_stage,
		lines,
		pairs,
		Some(expected_diff),
	)
}

fn stage_lines_inner(
	repo_path: &RepoPath,
	file_path: &str,
	is_stage: bool,
	lines: &[DiffLinePosition],
	pairs: &[DiffLineReplacement],
	expected_diff: Option<&FileDiff>,
) -> Result<()> {
	scope_time!("stage_lines");

	if lines.is_empty() {
		return empty_line_selection(pairs);
	}

	let repo = repo(repo_path)?;
	let mut index = repo.index()?;
	index.read(true)?;
	let index_path = Path::new(file_path);
	if (1..=3)
		.any(|stage| index.get_path(index_path, stage).is_some())
	{
		return Err(Error::Generic(String::from(
			"cannot line-stage an unmerged path",
		)));
	}
	let existing_entry = index.get_path(index_path, 0);
	if existing_entry
		.as_ref()
		.is_some_and(|entry| entry.mode & 0o170_000 != 0o100_000)
	{
		return Err(Error::Generic(String::from(
			"line staging is only supported for regular files",
		)));
	}
	if existing_entry.is_none() && !is_stage {
		return stage_untracked_selection(
			&repo,
			file_path,
			lines,
			pairs,
			expected_diff,
		);
	}
	let head_tree = if is_stage {
		match repo.head() {
			Ok(head) => Some(head.peel_to_tree()?),
			Err(error) if error.code() == ErrorCode::UnbornBranch => {
				None
			}
			Err(error) => return Err(error.into()),
		}
	} else {
		None
	};
	let head_entry = head_tree
		.as_ref()
		.map(|tree| -> Result<_> {
			match tree.get_path(index_path) {
				Ok(entry) => Ok(Some(entry)),
				Err(error) if error.code() == ErrorCode::NotFound => {
					Ok(None)
				}
				Err(error) => Err(error.into()),
			}
		})
		.transpose()?
		.flatten();
	if head_entry.as_ref().is_some_and(|entry| {
		entry.filemode() & 0o170_000 != 0o100_000
	}) {
		return Err(Error::Generic(String::from(
			"line staging is only supported for regular files",
		)));
	}
	let indexed_content = if let Some(entry) = &existing_entry {
		String::from_utf8(repo.find_blob(entry.id)?.content().into())?
	} else {
		String::new()
	};

	let new_content = get_index_content(
		&repo,
		file_path,
		IndexContentState {
			is_stage,
			missing_index_entry: existing_entry.is_none(),
			new_staged_file: is_stage
				&& existing_entry.is_some()
				&& head_entry.is_none(),
		},
		StageLineSelection {
			positions: lines,
			pairs,
			expected_diff,
		},
		&indexed_content,
	)?;
	write_index_content(
		&repo,
		&mut index,
		index_path,
		file_path,
		existing_entry,
		head_entry.as_ref(),
		&new_content,
	)?;

	Ok(())
}

fn empty_line_selection(pairs: &[DiffLineReplacement]) -> Result<()> {
	if pairs.is_empty() {
		Ok(())
	} else {
		Err(Error::Generic(String::from(
			"replacement pairs require selected diff rows",
		)))
	}
}

fn stage_untracked_selection(
	repo: &git2::Repository,
	file_path: &str,
	positions: &[DiffLinePosition],
	pairs: &[DiffLineReplacement],
	expected_diff: Option<&FileDiff>,
) -> Result<()> {
	if !pairs.is_empty() {
		return Err(Error::Generic(String::from(
			"new files do not contain replacement pairs",
		)));
	}
	untracked_file_path(repo, file_path)?;
	stage_untracked_lines(
		repo,
		file_path,
		StageLineSelection {
			positions,
			pairs,
			expected_diff,
		},
	)
}

fn get_index_content(
	repo: &git2::Repository,
	file_path: &str,
	state: IndexContentState,
	selection: StageLineSelection<'_>,
	indexed_content: &str,
) -> Result<String> {
	let patch =
		get_file_diff_patch(repo, file_path, state.is_stage, false)?;
	let hunks = patch_get_hunklines(&patch)?;
	validate_selected_lines(selection.positions, &hunks)?;
	if !selection.pairs.is_empty() {
		validate_selected_pairs(
			selection.positions,
			selection.pairs,
			&hunks,
		)?;
	}
	if let Some(expected_diff) = selection.expected_diff {
		validate_selected_content(
			expected_diff,
			selection.positions,
			&hunks,
		)?;
	}
	if state.is_stage && state.missing_index_entry {
		return selected_deletion_content(
			selection.positions,
			&hunks,
		);
	}
	if state.is_stage && state.new_staged_file {
		return unstage_new_file_content(
			selection.positions,
			indexed_content,
		);
	}
	let old_lines =
		indexed_content.split_inclusive('\n').collect::<Vec<_>>();
	apply_selection_with_pairs(
		selection.positions,
		selection.pairs,
		&hunks,
		&old_lines,
		state.is_stage,
		false,
	)
}

fn unstage_new_file_content(
	lines: &[DiffLinePosition],
	indexed_content: &str,
) -> Result<String> {
	let selected = lines
		.iter()
		.map(|line| {
			if line.old_lineno.is_some() {
				return Err(Error::Generic(String::from(
					"new file selections must point to added lines",
				)));
			}
			line.new_lineno.ok_or_else(|| {
				Error::Generic(String::from(
					"new file selections must point to added lines",
				))
			})
		})
		.collect::<Result<HashSet<_>>>()?;
	let mut content = String::new();
	for (index, line) in
		indexed_content.split_inclusive('\n').enumerate()
	{
		let line_number = u32::try_from(index + 1).map_err(|_| {
			Error::Generic(String::from("file has too many lines"))
		})?;
		if !selected.contains(&line_number) {
			content.push_str(line);
		}
	}
	if selected.iter().any(|line| {
		usize::try_from(*line).map_or(true, |line| {
			line > indexed_content.lines().count()
		})
	}) {
		return Err(Error::Generic(String::from(
			"selected line is no longer present in the staged file",
		)));
	}
	Ok(content)
}

fn selected_deletion_content(
	lines: &[DiffLinePosition],
	hunks: &[crate::sync::patches::HunkLines<'_>],
) -> Result<String> {
	let selected = lines.iter().collect::<HashSet<_>>();
	let mut content = String::new();
	for line in hunks.iter().flat_map(|hunk| &hunk.lines) {
		let position = DiffLinePosition::from(line);
		if line.origin_value() == git2::DiffLineType::Deletion
			&& selected.contains(&position)
		{
			content
				.push_str(&String::from_utf8(line.content().into())?);
		}
	}
	if content.is_empty() {
		return Err(Error::Generic(String::from(
			"selected staged deletion has no restorable lines",
		)));
	}
	Ok(content)
}

fn write_index_content(
	repo: &git2::Repository,
	index: &mut git2::Index,
	index_path: &Path,
	file_path: &str,
	existing_entry: Option<IndexEntry>,
	head_entry: Option<&git2::TreeEntry<'_>>,
	content: &str,
) -> Result<()> {
	if content.is_empty() && head_entry.is_none() {
		index.remove_path(index_path)?;
		smudge_second_resolution_racy_entries(index)?;
		index.write()?;
		index.read(true)?;
		return Ok(());
	}
	if existing_entry.is_none() && content.is_empty() {
		return Ok(());
	}
	let blob_id = repo.blob(content.as_bytes())?;
	let file_size = u32::try_conv(content.len())?;
	let index_entry = if let Some(mut entry) = existing_entry {
		entry.id = blob_id;
		entry.file_size = file_size;
		entry
	} else {
		let entry = head_entry.ok_or_else(|| {
			Error::Generic(String::from(
				"staged path has no index or HEAD version",
			))
		})?;
		IndexEntry {
			ctime: IndexTime::new(0, 0),
			mtime: IndexTime::new(0, 0),
			dev: 0,
			ino: 0,
			mode: entry.filemode().cast_unsigned(),
			uid: 0,
			gid: 0,
			file_size,
			id: blob_id,
			flags: 0,
			flags_extended: 0,
			path: file_path.as_bytes().to_vec(),
		}
	};
	index.add(&index_entry)?;
	smudge_second_resolution_racy_entries(index)?;
	index.write()?;
	index.read(true)?;
	Ok(())
}

/// Mark entries whose cached mtime falls in or after the existing index's
/// second as needing a content check. Git's status readers commonly compare
/// timestamps at second resolution, while libgit2 preserves nanoseconds.
/// Advancing the index timestamp can otherwise make a same-second edit look
/// clean.
fn smudge_second_resolution_racy_entries(
	index: &mut git2::Index,
) -> Result<()> {
	let Some(index_path) = index.path() else {
		return Ok(());
	};
	let index_metadata = match fs::metadata(index_path) {
		Ok(metadata) => metadata,
		Err(error)
			if error.kind() == std::io::ErrorKind::NotFound =>
		{
			return Ok(());
		}
		Err(error) => return Err(error.into()),
	};
	let index_time = index_metadata
		.modified()?
		.duration_since(std::time::UNIX_EPOCH)
		.map_err(|error| {
			Error::Generic(format!(
				"index timestamp predates Unix epoch: {error}"
			))
		})?
		.as_secs();
	// The stage bits identify unresolved merge entries, which must not be
	// passed back through `Index::add` as stage-zero entries.
	let entries = (0..index.len())
		.filter_map(|position| index.get(position))
		.filter(|entry| {
			entry.flags & 0x3000 == 0
				&& u64::try_from(entry.mtime.seconds())
					.unwrap_or_default()
					>= index_time
		})
		.collect::<Vec<_>>();
	for mut entry in entries {
		entry.file_size = 0;
		index.add(&entry)?;
	}
	Ok(())
}

pub fn validate_selected_lines(
	lines: &[DiffLinePosition],
	hunks: &[crate::sync::patches::HunkLines<'_>],
) -> Result<()> {
	for selected in lines {
		let found =
			hunks.iter().flat_map(|hunk| &hunk.lines).any(|line| {
				line.old_lineno() == selected.old_lineno
					&& line.new_lineno() == selected.new_lineno
					&& matches!(
						line.origin_value(),
						git2::DiffLineType::Addition
							| git2::DiffLineType::Deletion
					)
			});
		if !found {
			return Err(Error::Generic(String::from(
				"selected line is no longer a changed line in this diff",
			)));
		}
	}
	Ok(())
}

pub fn validate_selected_content(
	expected_diff: &FileDiff,
	lines: &[DiffLinePosition],
	hunks: &[crate::sync::patches::HunkLines<'_>],
) -> Result<()> {
	for selected in lines {
		let expected = expected_diff
			.hunks
			.iter()
			.flat_map(|hunk| &hunk.lines)
			.find(|line| {
				line.position == *selected
					&& matches!(
						line.line_type,
						crate::sync::diff::DiffLineType::Add
							| crate::sync::diff::DiffLineType::Delete
					)
			});
		let actual =
			hunks.iter().flat_map(|hunk| &hunk.lines).find(|line| {
				DiffLinePosition::from(*line) == *selected
					&& matches!(
						line.origin_value(),
						git2::DiffLineType::Addition
							| git2::DiffLineType::Deletion
					)
			});
		let matches_source = match (expected, actual) {
			(Some(expected), Some(actual)) => {
				let actual_type =
					crate::sync::diff::DiffLineType::from(
						actual.origin_value(),
					);
				expected.line_type == actual_type
					&& expected.content.as_ref()
						== String::from_utf8_lossy(actual.content())
							.trim_end_matches('\n')
			}
			_ => false,
		};
		if !matches_source {
			return Err(Error::Generic(String::from(
				"diff changed since selection; refresh and retry",
			)));
		}
	}
	Ok(())
}

/// Stage selected added lines from a new file. The working tree is left
/// untouched; only the index receives a blob containing those exact lines.
#[allow(clippy::filetype_is_file)] // `symlink_metadata` must reject symlinks and special files.
fn stage_untracked_lines(
	repo: &git2::Repository,
	file_path: &str,
	selection: StageLineSelection<'_>,
) -> Result<()> {
	let workdir = repo.workdir().ok_or_else(|| {
		Error::Generic(String::from("repository has no workdir"))
	})?;
	let (metadata, contents) = read_untracked_file(repo, file_path)?;
	let canonical =
		clean_worktree_contents(repo, workdir, file_path, &contents)?;
	let staged = selected_untracked_content(&canonical, selection)?;
	write_untracked_content(repo, file_path, &metadata, &staged)
}

#[allow(clippy::filetype_is_file)] // `symlink_metadata` must reject symlinks and special files.
fn read_untracked_file(
	repo: &git2::Repository,
	file_path: &str,
) -> Result<(fs::Metadata, Vec<u8>)> {
	let relative_path = Path::new(file_path);
	let full_path = untracked_file_path(repo, file_path)?;
	let metadata = fs::symlink_metadata(&full_path)?;
	if !metadata.file_type().is_file() {
		return Err(Error::Generic(String::from(
			"line staging is only supported for regular text files",
		)));
	}
	let contents = fs::read(&full_path)?;
	if contents.contains(&0) {
		return Err(Error::BinaryFile);
	}
	if repo
		.status_file(relative_path)?
		.contains(git2::Status::IGNORED)
	{
		return Err(Error::Generic(String::from(
			"cannot line-stage an ignored file",
		)));
	}
	Ok((metadata, contents))
}

fn selected_untracked_content(
	canonical_contents: &[u8],
	selection: StageLineSelection<'_>,
) -> Result<Vec<u8>> {
	if canonical_contents.contains(&0) {
		return Err(Error::BinaryFile);
	}
	std::str::from_utf8(canonical_contents)
		.map_err(|error| Error::Generic(error.to_string()))?;
	let selected = selection
		.positions
		.iter()
		.map(|line| {
			line.new_lineno.ok_or_else(|| {
				Error::Generic(String::from(
					"new file selections must point to added lines",
				))
			})
		})
		.collect::<Result<HashSet<_>>>()?;
	if selected.contains(&0)
		|| selection
			.positions
			.iter()
			.any(|line| line.old_lineno.is_some())
	{
		return Err(Error::Generic(String::from(
			"new file selections must point to added lines",
		)));
	}
	let file_lines = canonical_contents
		.split_inclusive(|byte| *byte == b'\n')
		.collect::<Vec<_>>();
	if selected.iter().any(|line| {
		usize::try_from(*line)
			.map_or(true, |line| line > file_lines.len())
	}) {
		return Err(Error::Generic(String::from(
			"selected line is no longer present in the new file",
		)));
	}
	if let Some(expected_diff) = selection.expected_diff {
		validate_untracked_selected_content(
			expected_diff,
			selection.positions,
			&file_lines,
		)?;
	}
	let mut staged = Vec::new();
	for (index, line) in file_lines.iter().enumerate() {
		let line_number = u32::try_from(index + 1).map_err(|_| {
			Error::Generic(String::from("file has too many lines"))
		})?;
		if selected.contains(&line_number) {
			staged.extend_from_slice(line);
		}
	}
	Ok(staged)
}

fn write_untracked_content(
	repo: &git2::Repository,
	file_path: &str,
	metadata: &fs::Metadata,
	staged: &[u8],
) -> Result<()> {
	let mut index = repo.index()?;
	index.read(true)?;
	let id = repo.blob(staged)?;
	let filemode_enabled = repo
		.config()?
		.get_bool("core.filemode")
		.unwrap_or(cfg!(unix));
	let mode = if filemode_enabled && is_executable(metadata) {
		0o100_755
	} else {
		0o100_644
	};
	index.add(&IndexEntry {
		ctime: IndexTime::new(0, 0),
		mtime: IndexTime::new(0, 0),
		dev: 0,
		ino: 0,
		mode,
		uid: 0,
		gid: 0,
		file_size: u32::try_conv(staged.len())?,
		id,
		flags: 0,
		flags_extended: 0,
		path: file_path.as_bytes().to_vec(),
	})?;
	smudge_second_resolution_racy_entries(&mut index)?;
	index.write()?;
	index.read(true)?;
	Ok(())
}

fn validate_untracked_selected_content(
	expected_diff: &FileDiff,
	lines: &[DiffLinePosition],
	file_lines: &[&[u8]],
) -> Result<()> {
	for selected in lines {
		let expected = expected_diff
			.hunks
			.iter()
			.flat_map(|hunk| &hunk.lines)
			.find(|line| {
				line.position == *selected
					&& line.line_type
						== crate::sync::diff::DiffLineType::Add
			});
		let actual = selected
			.new_lineno
			.and_then(|line| usize::try_from(line).ok())
			.and_then(|line| line.checked_sub(1))
			.and_then(|line| file_lines.get(line));
		let matches_source = match (expected, actual) {
			(Some(expected), Some(actual)) => {
				expected.content.as_ref()
					== String::from_utf8_lossy(actual)
						.trim_end_matches('\n')
			}
			_ => false,
		};
		if !matches_source {
			return Err(Error::Generic(String::from(
				"diff changed since selection; refresh and retry",
			)));
		}
	}
	Ok(())
}

pub fn clean_worktree_contents(
	repo: &git2::Repository,
	workdir: &Path,
	file_path: &str,
	contents: &[u8],
) -> Result<Vec<u8>> {
	let path_argument = format!("--path={file_path}");
	let mut child = Command::new("git")
		.current_dir(workdir)
		.arg("--git-dir")
		.arg(repo.path())
		.arg("--work-tree")
		.arg(workdir)
		.args(["hash-object", "-w", &path_argument, "--stdin"])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()?;
	let mut stdin = child.stdin.take().ok_or_else(|| {
		Error::Generic(String::from(
			"failed to open git filter input",
		))
	})?;
	stdin.write_all(contents)?;
	drop(stdin);
	let output = child.wait_with_output()?;
	if !output.status.success() {
		return Err(Error::Generic(format!(
			"git clean filter failed: {}",
			String::from_utf8_lossy(&output.stderr).trim()
		)));
	}
	let oid = std::str::from_utf8(&output.stdout)
		.map_err(|error| Error::Generic(error.to_string()))?
		.trim();
	let blob_id = git2::Oid::from_str(oid)?;
	Ok(repo.find_blob(blob_id)?.content().to_vec())
}

/// Resolve an untracked path without traversing symlinks in its parents.
fn untracked_file_path(
	repo: &git2::Repository,
	file_path: &str,
) -> Result<PathBuf> {
	let relative_path = Path::new(file_path);
	if relative_path.is_absolute()
		|| relative_path.components().any(|component| {
			!matches!(component, Component::Normal(_))
		}) {
		return Err(Error::Generic(String::from(
			"new file path must be a normalized repository-relative path",
		)));
	}
	let workdir = repo.workdir().ok_or_else(|| {
		Error::Generic(String::from("repository has no workdir"))
	})?;
	let components = relative_path.components().collect::<Vec<_>>();
	if components.is_empty() {
		return Err(Error::Generic(String::from(
			"new file path is empty",
		)));
	}
	let mut full_path = workdir.to_path_buf();
	for (index, component) in components.iter().enumerate() {
		let Component::Normal(name) = component else {
			return Err(Error::Generic(String::from(
				"new file path must be normalized",
			)));
		};
		full_path.push(name);
		let metadata = fs::symlink_metadata(&full_path)?;
		if metadata.file_type().is_symlink() {
			return Err(Error::Generic(String::from(
				"line staging does not follow symlinks",
			)));
		}
		if index + 1 < components.len() && !metadata.is_dir() {
			return Err(Error::Generic(String::from(
				"new file path parent is not a directory",
			)));
		}
	}
	Ok(full_path)
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
	use std::os::unix::fs::PermissionsExt;
	metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
const fn is_executable(_metadata: &fs::Metadata) -> bool {
	false
}

#[cfg(test)]
mod test {
	use super::*;
	use crate::sync::{
		diff::get_diff,
		tests::{
			get_statuses, repo_init, repo_init_empty,
			write_commit_file,
		},
		utils::{repo_write_file, stage_add_file},
	};
	use std::fs;

	fn read_index(repo: &git2::Repository) -> git2::Index {
		let mut index = repo.index().unwrap();
		index.read(true).unwrap();
		index
	}

	fn assert_index_entry_unchanged(
		before: &IndexEntry,
		after: &IndexEntry,
	) {
		assert_eq!(after.ctime.seconds(), before.ctime.seconds());
		assert_eq!(
			after.ctime.nanoseconds(),
			before.ctime.nanoseconds()
		);
		assert_eq!(after.mtime.seconds(), before.mtime.seconds());
		assert_eq!(
			after.mtime.nanoseconds(),
			before.mtime.nanoseconds()
		);
		assert_eq!(after.dev, before.dev);
		assert_eq!(after.ino, before.ino);
		assert_eq!(after.uid, before.uid);
		assert_eq!(after.gid, before.gid);
		assert_eq!(after.file_size, before.file_size);
		assert_eq!(after.mode, before.mode);
		assert_eq!(after.id, before.id);
		assert_eq!(after.flags, before.flags);
		assert_eq!(after.flags_extended, before.flags_extended);
		assert_eq!(after.path, before.path);
	}

	#[test]
	fn racy_index_cleanup_preserves_old_and_conflict_entries() {
		let (_repo_path, repo) = repo_init_empty().unwrap();
		let index_path = repo.path().join("index");
		let index_second = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap()
			.as_secs()
			.saturating_sub(10);
		let mut index = repo.index().unwrap();
		index.read(true).unwrap();

		let stable_path = b"stable.txt";
		let stable_blob = repo.blob(b"stable blob").unwrap();
		let stable_entry = IndexEntry {
			ctime: IndexTime::new(
				(index_second - 2).try_into().unwrap(),
				123_456_789,
			),
			mtime: IndexTime::new(
				(index_second - 1).try_into().unwrap(),
				234_567_890,
			),
			dev: 17,
			ino: 1234,
			mode: 0o100_644,
			uid: 501,
			gid: 20,
			file_size: 11,
			id: stable_blob,
			flags: stable_path.len().try_into().unwrap(),
			flags_extended: 0,
			path: stable_path.to_vec(),
		};
		index.add(&stable_entry).unwrap();

		let conflict_path = b"conflict.txt";
		let mut conflict_entries = Vec::new();
		for stage in 1_u16..=3 {
			let contents = format!("conflict stage {stage}");
			let entry = IndexEntry {
				ctime: IndexTime::new(
					(index_second + 2).try_into().unwrap(),
					stage.into(),
				),
				mtime: IndexTime::new(
					(index_second + 1).try_into().unwrap(),
					stage.into(),
				),
				dev: 31,
				ino: 5678,
				mode: 0o100_644,
				uid: 501,
				gid: 20,
				file_size: contents.len().try_into().unwrap(),
				id: repo.blob(contents.as_bytes()).unwrap(),
				flags: u16::try_from(conflict_path.len()).unwrap()
					| (stage << 12),
				flags_extended: 0,
				path: conflict_path.to_vec(),
			};
			index.add(&entry).unwrap();
		}
		index.write().unwrap();
		fs::File::open(&index_path)
			.unwrap()
			.set_times(fs::FileTimes::new().set_modified(
				std::time::UNIX_EPOCH
					+ std::time::Duration::from_secs(index_second),
			))
			.unwrap();
		index.read(true).unwrap();

		let stable_before =
			index.get_path(Path::new("stable.txt"), 0).unwrap();
		for stage in 1..=3 {
			conflict_entries.push(
				index
					.get_path(
						Path::new("conflict.txt"),
						i32::from(stage),
					)
					.unwrap(),
			);
		}
		assert!(index.has_conflicts());

		smudge_second_resolution_racy_entries(&mut index).unwrap();

		let stable_after =
			index.get_path(Path::new("stable.txt"), 0).unwrap();
		assert_index_entry_unchanged(&stable_before, &stable_after);
		for (stage, before) in (1..=3).zip(&conflict_entries) {
			let after = index
				.get_path(Path::new("conflict.txt"), stage)
				.unwrap();
			assert_index_entry_unchanged(before, &after);
		}
		assert!(index
			.get_path(Path::new("conflict.txt"), 0)
			.is_none());
		assert!(index.has_conflicts());
	}

	#[test]
	fn test_stage() {
		static FILE_1: &str = r"0
";

		static FILE_2: &str = r"0
1
2
3
";

		let (path, repo) = repo_init().unwrap();
		let path: &RepoPath = &path.path().to_str().unwrap().into();

		write_commit_file(&repo, "test.txt", FILE_1, "c1");

		repo_write_file(&repo, "test.txt", FILE_2).unwrap();

		stage_lines(
			path,
			"test.txt",
			false,
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(2),
			}],
		)
		.unwrap();

		let diff = get_diff(path, "test.txt", true, None).unwrap();

		assert_eq!(diff.lines, 3);
		assert_eq!(&*diff.hunks[0].lines[0].content, "@@ -1 +1,2 @@");
	}

	#[test]
	fn test_panic_stage_no_newline() {
		static FILE_1: &str = r"a = 1
b = 2";

		static FILE_2: &str = r"a = 2
b = 3
c = 4";

		let (path, repo) = repo_init().unwrap();
		let path: &RepoPath = &path.path().to_str().unwrap().into();

		write_commit_file(&repo, "test.txt", FILE_1, "c1");

		repo_write_file(&repo, "test.txt", FILE_2).unwrap();

		stage_lines(
			path,
			"test.txt",
			false,
			&[
				DiffLinePosition {
					old_lineno: Some(1),
					new_lineno: None,
				},
				DiffLinePosition {
					old_lineno: Some(2),
					new_lineno: None,
				},
			],
		)
		.unwrap();

		let diff = get_diff(path, "test.txt", true, None).unwrap();

		assert_eq!(diff.lines, 4);
		assert_eq!(
			&*diff.hunks[0].lines[0].content,
			"@@ -1,2 +0,0 @@"
		);
	}

	#[test]
	fn test_unstage() {
		static FILE_1: &str = r"0
";

		static FILE_2: &str = r"0
1
2
3
";

		let (path, repo) = repo_init().unwrap();
		let path: &RepoPath = &path.path().to_str().unwrap().into();

		write_commit_file(&repo, "test.txt", FILE_1, "c1");

		repo_write_file(&repo, "test.txt", FILE_2).unwrap();

		assert_eq!(get_statuses(path), (1, 0));

		stage_add_file(path, Path::new("test.txt")).unwrap();

		assert_eq!(get_statuses(path), (0, 1));

		let diff_before =
			get_diff(path, "test.txt", true, None).unwrap();

		assert_eq!(diff_before.lines, 5);

		stage_lines(
			path,
			"test.txt",
			true,
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(2),
			}],
		)
		.unwrap();

		assert_eq!(get_statuses(path), (1, 1));

		let diff = get_diff(path, "test.txt", true, None).unwrap();

		assert_eq!(diff.lines, 4);
	}

	#[test]
	fn test_stage_selected_lines_of_untracked_file_preserves_bytes_and_mode(
	) {
		for (contents, selected_line, expected) in [
			("first\nsecond\nthird\n", 1, "first\n"),
			("first\r\nsecond\r\nthird\r\n", 2, "second\r\n"),
			("first\ntail", 2, "tail"),
		] {
			let (repo_path, repo) = repo_init_empty().unwrap();
			let path: &RepoPath =
				&repo_path.path().to_str().unwrap().into();
			repo.config()
				.unwrap()
				.set_bool("core.autocrlf", false)
				.unwrap();
			let working =
				repo.path().parent().unwrap().join("new.txt");
			fs::write(&working, contents).unwrap();
			let original = fs::read(&working).unwrap();

			stage_lines(
				path,
				"new.txt",
				false,
				&[DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(selected_line),
				}],
			)
			.unwrap();

			let index = read_index(&repo);
			let entry =
				index.get_path(Path::new("new.txt"), 0).unwrap();
			let blob = repo.find_blob(entry.id).unwrap();
			assert_eq!(blob.content(), expected.as_bytes());
			assert_eq!(fs::read(working).unwrap(), original);
		}

		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			let (repo_path, repo) = repo_init().unwrap();
			let path: &RepoPath =
				&repo_path.path().to_str().unwrap().into();
			let working =
				repo.path().parent().unwrap().join("script.sh");
			fs::write(&working, "#!/bin/sh\necho hi\n").unwrap();
			fs::set_permissions(
				&working,
				fs::Permissions::from_mode(0o755),
			)
			.unwrap();
			stage_lines(
				path,
				"script.sh",
				false,
				&[DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(1),
				}],
			)
			.unwrap();
			let entry = read_index(&repo)
				.get_path(Path::new("script.sh"), 0)
				.unwrap();
			assert_eq!(entry.mode & 0o111, 0o111);

			let (repo_path, repo) = repo_init_empty().unwrap();
			let path: &RepoPath =
				&repo_path.path().to_str().unwrap().into();
			let mut config = repo.config().unwrap();
			config.set_bool("core.filemode", false).unwrap();
			let working =
				repo.path().parent().unwrap().join("script.sh");
			fs::write(&working, "#!/bin/sh\necho hi\n").unwrap();
			fs::set_permissions(
				&working,
				fs::Permissions::from_mode(0o755),
			)
			.unwrap();
			stage_lines(
				path,
				"script.sh",
				false,
				&[DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(1),
				}],
			)
			.unwrap();
			let entry = read_index(&repo)
				.get_path(Path::new("script.sh"), 0)
				.unwrap();
			assert_eq!(entry.mode & 0o111, 0);
		}
	}

	#[test]
	fn checked_line_staging_accepts_unchanged_crlf_diff() {
		let (repo_path, repo) = repo_init().unwrap();
		let path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		repo.config()
			.unwrap()
			.set_bool("core.autocrlf", false)
			.unwrap();
		write_commit_file(&repo, "crlf.txt", "base\n", "c1");
		let working = repo.path().parent().unwrap().join("crlf.txt");
		fs::write(&working, b"base\r\nadded\r\n").unwrap();

		let displayed =
			get_diff(path, "crlf.txt", false, None).unwrap();
		let selected = displayed
			.hunks
			.iter()
			.flat_map(|hunk| hunk.lines.iter())
			.find(|line| {
				line.line_type == crate::sync::diff::DiffLineType::Add
					&& line.content.contains("added")
			})
			.unwrap();
		assert_eq!(selected.content.as_ref(), "added\r");
		stage_lines_checked(
			path,
			"crlf.txt",
			false,
			&[selected.position],
			&displayed,
		)
		.unwrap();
		let entry = read_index(&repo)
			.get_path(Path::new("crlf.txt"), 0)
			.unwrap();
		assert_eq!(
			repo.find_blob(entry.id).unwrap().content(),
			b"base\nadded\r\n"
		);
	}

	#[test]
	fn test_unstage_last_line_of_a_new_file_removes_its_index_entry()
	{
		let (repo_path, repo) = repo_init_empty().unwrap();
		let path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		fs::write(
			repo.path().parent().unwrap().join("new.txt"),
			"first\nsecond\n",
		)
		.unwrap();
		let first = DiffLinePosition {
			old_lineno: None,
			new_lineno: Some(1),
		};
		let second = DiffLinePosition {
			old_lineno: None,
			new_lineno: Some(2),
		};

		stage_lines(path, "new.txt", false, &[first]).unwrap();
		stage_lines(path, "new.txt", false, &[second]).unwrap();
		assert!(read_index(&repo)
			.get_path(Path::new("new.txt"), 0)
			.is_some());
		stage_lines(path, "new.txt", true, &[first]).unwrap();
		let entry = read_index(&repo)
			.get_path(Path::new("new.txt"), 0)
			.unwrap();
		assert_eq!(
			repo.find_blob(entry.id).unwrap().content(),
			b"second\n"
		);

		stage_lines(
			path,
			"new.txt",
			true,
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(1),
			}],
		)
		.unwrap();
		assert!(read_index(&repo)
			.get_path(Path::new("new.txt"), 0)
			.is_none());
	}

	#[test]
	fn test_unstage_new_file_when_head_exists_preserves_other_staged_lines(
	) {
		let (repo_path, repo) = repo_init().unwrap();
		let path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		fs::write(
			repo.path().parent().unwrap().join("new.txt"),
			"first\nsecond\n",
		)
		.unwrap();
		let first = DiffLinePosition {
			old_lineno: None,
			new_lineno: Some(1),
		};
		let second = DiffLinePosition {
			old_lineno: None,
			new_lineno: Some(2),
		};

		stage_lines(path, "new.txt", false, &[first]).unwrap();
		stage_lines(path, "new.txt", false, &[second]).unwrap();
		let cached_diff = Command::new("git")
			.current_dir(repo.path().parent().unwrap())
			.args(["diff", "--cached", "--", "new.txt"])
			.output()
			.unwrap();
		assert!(cached_diff.status.success());
		let cached_diff =
			String::from_utf8(cached_diff.stdout).unwrap();
		assert!(cached_diff.contains("+first\n"));
		assert!(cached_diff.contains("+second\n"));

		stage_lines(path, "new.txt", true, &[first]).unwrap();
		let index = read_index(&repo);
		let entry = index.get_path(Path::new("new.txt"), 0).unwrap();
		assert_eq!(
			repo.find_blob(entry.id).unwrap().content(),
			b"second\n"
		);
		let cached_diff = Command::new("git")
			.current_dir(repo.path().parent().unwrap())
			.args(["diff", "--cached", "--", "new.txt"])
			.output()
			.unwrap();
		assert!(cached_diff.status.success());
		let cached_diff =
			String::from_utf8(cached_diff.stdout).unwrap();
		assert!(!cached_diff.contains("+first\n"));
		assert!(cached_diff.contains("+second\n"));

		stage_lines(
			path,
			"new.txt",
			true,
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(1),
			}],
		)
		.unwrap();
		assert!(read_index(&repo)
			.get_path(Path::new("new.txt"), 0)
			.is_none());
		let cached_diff = Command::new("git")
			.current_dir(repo.path().parent().unwrap())
			.args(["diff", "--cached", "--", "new.txt"])
			.output()
			.unwrap();
		assert!(cached_diff.status.success());
		assert!(cached_diff.stdout.is_empty());
	}

	#[test]
	fn test_checked_line_staging_rejects_replaced_worktree_line() {
		let (repo_path, repo) = repo_init().unwrap();
		let path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		let original = "one\ntwo\nthree\n";
		write_commit_file(&repo, "test.txt", original, "c1");
		let working = repo.path().parent().unwrap().join("test.txt");
		fs::write(&working, "one\ndisplayed\nthree\n").unwrap();
		let displayed =
			get_diff(path, "test.txt", false, None).unwrap();
		fs::write(&working, "one\nreplacement\nthree\n").unwrap();

		let result = stage_lines_checked(
			path,
			"test.txt",
			false,
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(2),
			}],
			&displayed,
		);
		assert!(result.is_err());
		let cached = Command::new("git")
			.current_dir(repo.path().parent().unwrap())
			.args(["diff", "--cached", "--", "test.txt"])
			.output()
			.unwrap();
		assert!(cached.status.success());
		assert!(cached.stdout.is_empty());
	}

	#[test]
	fn checked_line_staging_preserves_control_bytes_in_selected_content(
	) {
		let (repo_path, repo) = repo_init().unwrap();
		let path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		write_commit_file(&repo, "test.txt", "base\n", "c1");
		let working = repo.path().parent().unwrap().join("test.txt");
		let contents = b"base\nsafe \x1b[2J\n";
		fs::write(&working, contents).unwrap();
		let displayed =
			get_diff(path, "test.txt", false, None).unwrap();
		stage_lines_checked(
			path,
			"test.txt",
			false,
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(2),
			}],
			&displayed,
		)
		.unwrap();
		let entry = read_index(&repo)
			.get_path(Path::new("test.txt"), 0)
			.unwrap();
		assert_eq!(
			repo.find_blob(entry.id).unwrap().content(),
			contents
		);
	}

	#[test]
	fn partial_stage_does_not_refresh_unrelated_same_size_index_entries(
	) {
		let (repo_path, repo) = repo_init_empty().unwrap();
		let path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		let old_readme = "# Rendering\n\nGPUI lays out elements,\nhandles input and paints windows.\n\nUnchanged context 0.\nUnchanged context 1.\nUnchanged context 2.\nUnchanged context 3.\nUnchanged context 4.\nUnchanged context 5.\nUnchanged context 6.\nUnchanged context 7.\n\nThis sibling remains untouched.\n";
		let old_paragraph =
			"GPUI lays out elements,\nhandles input and paints windows.\n";
		let new_paragraph =
			"GPUI lays out elements, handles input\nand paints windows efficiently.\n";
		let rust_old = "fn adjust() {\n    let unchanged = 42;\n    let stable = false;\n    let count = 4;\n    let enabled = false;\n    consume(unchanged, count, enabled);\n}\n";
		let changed_sample = "fn adjust() {\n    let unchanged = 42;\n    let stable = false;\n    let count = 44;\n    let enabled = true;\n    consume(unchanged, count, enabled);\n}\n";
		fs::write(
			repo.path().parent().unwrap().join("README.md"),
			old_readme,
		)
		.unwrap();
		fs::write(
			repo.path().parent().unwrap().join("sample.rs"),
			rust_old,
		)
		.unwrap();
		let git = |args: &[&str]| {
			Command::new("git")
				.current_dir(repo.path().parent().unwrap())
				.args(args)
				.output()
				.unwrap()
		};
		assert!(git(&["config", "user.name", "Reviewer"])
			.status
			.success());
		assert!(git(&[
			"config",
			"user.email",
			"review@example.invalid"
		])
		.status
		.success());
		assert!(git(&["config", "core.autocrlf", "false"])
			.status
			.success());
		assert!(git(&["config", "core.trustctime", "false"])
			.status
			.success());
		assert!(git(&["add", "."]).status.success());
		assert!(git(&["commit", "-m", "Exact original source"])
			.status
			.success());
		let readme_path =
			repo.path().parent().unwrap().join("README.md");
		let sample_path =
			repo.path().parent().unwrap().join("sample.rs");
		let original_index = read_index(&repo);
		let original_sample_id = original_index
			.get_path(Path::new("sample.rs"), 0)
			.unwrap()
			.id;
		assert_eq!(
			fs::read_to_string(&sample_path).unwrap().len(),
			changed_sample.len()
		);
		fs::write(&sample_path, changed_sample).unwrap();
		let index_file = repo.path().join("index");
		let test_second = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap()
			.as_secs()
			.saturating_sub(10);
		let cached_time = std::time::UNIX_EPOCH
			+ std::time::Duration::from_secs(test_second);
		let sample_time =
			cached_time + std::time::Duration::from_millis(700);
		// Git's default status comparison uses whole seconds, while libgit2
		// retains nanoseconds. Keep all three timestamps in one second, but
		// order them so libgit2 considers the cached entry non-racy.
		let mut index = repo.index().unwrap();
		index.read(true).unwrap();
		let mut sample_entry =
			index.get_path(Path::new("sample.rs"), 0).unwrap();
		sample_entry.mtime =
			IndexTime::new(test_second.try_into().unwrap(), 0);
		index.add(&sample_entry).unwrap();
		index.write().unwrap();
		index.read(true).unwrap();
		fs::File::open(&sample_path)
			.unwrap()
			.set_times(fs::FileTimes::new().set_modified(sample_time))
			.unwrap();
		fs::File::open(&index_file)
			.unwrap()
			.set_times(fs::FileTimes::new().set_modified(
				cached_time + std::time::Duration::from_millis(500),
			))
			.unwrap();
		let new_readme =
			old_readme.replace(old_paragraph, new_paragraph).replace(
				"This sibling remains untouched.",
				"This sibling has an independent edit.",
			);
		fs::write(&readme_path, &new_readme).unwrap();
		let (working_statuses, _) = get_statuses(path);
		assert!(
			working_statuses >= 2,
			"both files should be dirty before staging README"
		);
		let before = git(&["status", "--short", "--", "sample.rs"]);
		assert!(before.status.success());
		assert!(
			String::from_utf8_lossy(&before.stdout).contains("sample.rs"),
			"the controlled same-second edit should be visible before staging"
		);
		let displayed =
			get_diff(path, "README.md", false, None).unwrap();
		let changed_lines = displayed
			.hunks
			.iter()
			.flat_map(|hunk| &hunk.lines)
			.filter(|line| {
				matches!(
					line.line_type,
					crate::sync::diff::DiffLineType::Add
						| crate::sync::diff::DiffLineType::Delete
				) && (line.content.contains("GPUI lays out")
					|| line.content.contains("handles input")
					|| line.content.contains("paints windows"))
			})
			.collect::<Vec<_>>();
		let deletions = changed_lines
			.iter()
			.filter(|line| {
				line.line_type
					== crate::sync::diff::DiffLineType::Delete
			})
			.collect::<Vec<_>>();
		let additions = changed_lines
			.iter()
			.filter(|line| {
				line.line_type == crate::sync::diff::DiffLineType::Add
			})
			.collect::<Vec<_>>();
		assert_eq!(deletions.len(), 2);
		assert_eq!(additions.len(), 2);
		let selected = changed_lines
			.iter()
			.map(|line| line.position)
			.collect::<Vec<_>>();
		stage_lines_with_pairs_checked(
			path,
			"README.md",
			false,
			&selected,
			&[],
			&displayed,
		)
		.unwrap();
		let after = Command::new("git")
			.current_dir(repo.path().parent().unwrap())
			.args(["status", "--short", "--", "sample.rs"])
			.output()
			.unwrap();
		assert!(after.status.success());
		assert!(
			String::from_utf8_lossy(&after.stdout)
				.contains("sample.rs"),
			"staging README must not hide an unrelated same-size Rust edit"
		);
		assert_eq!(
			fs::read_to_string(&sample_path).unwrap(),
			changed_sample
		);
		let after_index = read_index(&repo);
		let sample_entry =
			after_index.get_path(Path::new("sample.rs"), 0).unwrap();
		assert_eq!(sample_entry.id, original_sample_id);
		assert_eq!(sample_entry.file_size, 0);
		assert_eq!(
			repo.find_blob(sample_entry.id).unwrap().content(),
			b"fn adjust() {\n    let unchanged = 42;\n    let stable = false;\n    let count = 4;\n    let enabled = false;\n    consume(unchanged, count, enabled);\n}\n"
		);
		let readme_entry =
			after_index.get_path(Path::new("README.md"), 0).unwrap();
		assert_eq!(
			repo.find_blob(readme_entry.id).unwrap().content(),
			old_readme
				.replace(old_paragraph, new_paragraph)
				.as_bytes()
		);
	}

	#[test]
	fn test_checked_line_unstage_rejects_replaced_index_line() {
		let (repo_path, repo) = repo_init().unwrap();
		let path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		write_commit_file(
			&repo,
			"test.txt",
			"one\ntwo\nthree\n",
			"c1",
		);
		let working = repo.path().parent().unwrap().join("test.txt");
		fs::write(&working, "one\ndisplayed\nthree\n").unwrap();
		crate::sync::utils::stage_add_file(
			path,
			Path::new("test.txt"),
		)
		.unwrap();
		let displayed =
			get_diff(path, "test.txt", true, None).unwrap();
		fs::write(&working, "one\nreplacement\nthree\n").unwrap();
		crate::sync::utils::stage_add_file(
			path,
			Path::new("test.txt"),
		)
		.unwrap();

		let result = stage_lines_checked(
			path,
			"test.txt",
			true,
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(2),
			}],
			&displayed,
		);
		assert!(result.is_err());
		let index = read_index(&repo);
		let entry = index.get_path(Path::new("test.txt"), 0).unwrap();
		assert_eq!(
			repo.find_blob(entry.id).unwrap().content(),
			b"one\nreplacement\nthree\n"
		);
	}

	#[test]
	fn test_empty_untracked_file_does_not_create_an_index_entry() {
		let (repo_path, repo) = repo_init_empty().unwrap();
		let path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		fs::write(
			repo.path().parent().unwrap().join("empty.txt"),
			"",
		)
		.unwrap();

		assert!(stage_lines(
			path,
			"empty.txt",
			false,
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(1),
			}],
		)
		.is_err());
		assert!(read_index(&repo)
			.get_path(Path::new("empty.txt"), 0)
			.is_none());
	}

	#[test]
	fn test_stage_path_with_pathspec_magic_characters_literally() {
		let (repo_path, repo) = repo_init().unwrap();
		let path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		let filename = ":(exclude)*";
		fs::write(
			repo.path().parent().unwrap().join(filename),
			"keep\n",
		)
		.unwrap();
		fs::write(
			repo.path().parent().unwrap().join("other.txt"),
			"other\n",
		)
		.unwrap();

		let diff = get_diff(path, filename, false, None).unwrap();
		assert_eq!(diff.lines, 2);
		stage_lines(
			path,
			filename,
			false,
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(1),
			}],
		)
		.unwrap();

		let index = read_index(&repo);
		assert!(
			index.get_path(Path::new(filename), 0).is_some(),
			"index paths: {:?}",
			index
				.iter()
				.map(|entry| String::from_utf8_lossy(&entry.path)
					.into_owned())
				.collect::<Vec<_>>()
		);
		assert!(index.get_path(Path::new("other.txt"), 0).is_none());
	}

	#[test]
	fn test_partial_new_file_uses_git_clean_filters() {
		let (repo_path, repo) = repo_init().unwrap();
		let path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		write_commit_file(
			&repo,
			".gitattributes",
			"new.txt text eol=lf\n",
			"attributes",
		);
		fs::write(
			repo.path().parent().unwrap().join("new.txt"),
			"first\r\nsecond\r\nthird\r\n",
		)
		.unwrap();
		let diff = get_diff(path, "new.txt", false, None).unwrap();
		let selected_line = diff
			.hunks
			.iter()
			.flat_map(|hunk| &hunk.lines)
			.find(|line| line.content.contains("second"))
			.unwrap()
			.position;

		stage_lines(path, "new.txt", false, &[selected_line])
			.unwrap();
		let entry = read_index(&repo)
			.get_path(Path::new("new.txt"), 0)
			.unwrap();
		let blob = repo.find_blob(entry.id).unwrap();
		assert_eq!(blob.content(), b"second\n");
	}

	#[test]
	fn test_unstage_selected_line_from_staged_deletion_ignores_recreated_file(
	) {
		let (repo_path, repo) = repo_init().unwrap();
		let path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		write_commit_file(
			&repo,
			"deleted.txt",
			"head one\nhead two\n",
			"base",
		);
		let working =
			repo.path().parent().unwrap().join("deleted.txt");
		fs::remove_file(&working).unwrap();
		let mut index = repo.index().unwrap();
		index.remove_path(Path::new("deleted.txt")).unwrap();
		index.write().unwrap();
		fs::write(&working, "recreated worktree content\n").unwrap();

		let staged_diff =
			get_diff(path, "deleted.txt", true, None).unwrap();
		let first_deleted = staged_diff
			.hunks
			.iter()
			.flat_map(|hunk| hunk.lines.iter())
			.find(|line| {
				line.line_type
					== crate::sync::diff::DiffLineType::Delete
					&& line.content.contains("head one")
			})
			.unwrap();
		stage_lines(
			path,
			"deleted.txt",
			true,
			&[first_deleted.position],
		)
		.unwrap();

		let entry = read_index(&repo)
			.get_path(Path::new("deleted.txt"), 0)
			.unwrap();
		let blob = repo.find_blob(entry.id).unwrap();
		assert_eq!(blob.content(), b"head one\n");
		assert_eq!(
			fs::read(working).unwrap(),
			b"recreated worktree content\n"
		);
	}

	#[cfg(unix)]
	#[test]
	fn test_untracked_symlink_is_not_staged_or_followed() {
		use std::os::unix::fs::symlink;
		let (repo_path, repo) = repo_init().unwrap();
		let path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		let target = repo.path().parent().unwrap().join("target.txt");
		fs::write(&target, "leave me\n").unwrap();
		symlink(
			"target.txt",
			repo.path().parent().unwrap().join("link.txt"),
		)
		.unwrap();
		assert!(stage_lines(
			path,
			"link.txt",
			false,
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(1),
			}],
		)
		.is_err());
		assert_eq!(fs::read(&target).unwrap(), b"leave me\n");
		assert!(read_index(&repo)
			.get_path(Path::new("link.txt"), 0)
			.is_none());

		symlink(
			repo.path(),
			repo.path().parent().unwrap().join("linked-directory"),
		)
		.unwrap();
		assert!(stage_lines(
			path,
			"linked-directory/config",
			false,
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(1),
			}],
		)
		.is_err());
	}

	#[test]
	fn test_ignored_file_cannot_be_partially_staged() {
		let (repo_path, repo) = repo_init_empty().unwrap();
		let path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		let root = repo.path().parent().unwrap();
		fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
		fs::write(root.join("ignored.txt"), "ignored\n").unwrap();

		assert!(stage_lines(
			path,
			"ignored.txt",
			false,
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(1),
			}],
		)
		.is_err());
		assert!(read_index(&repo)
			.get_path(Path::new("ignored.txt"), 0)
			.is_none());
	}
}
