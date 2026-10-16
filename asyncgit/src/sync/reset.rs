use super::{utils::get_head_repo, CommitId, RepoPath};
use crate::{error::Result, sync::repository::repo};
use git2::{
	build::CheckoutBuilder, Delta, DiffOptions, IndexEntry,
	IndexTime, ObjectType, ResetType,
};
use scopetime::scope_time;

///
pub fn reset_stage(repo_path: &RepoPath, path: &str) -> Result<()> {
	scope_time!("reset_stage");

	let repo = repo(repo_path)?;

	if let Ok(id) = get_head_repo(&repo) {
		let obj =
			repo.find_object(id.into(), Some(ObjectType::Commit))?;

		repo.reset_default(Some(&obj), [path])?;
	} else {
		repo.reset_default(None, [path])?;
	}

	Ok(())
}

/// Reset only the named index entries to `HEAD`, matching paths byte-for-byte.
/// Unlike `reset_default`, this does not interpret paths as Git pathspecs.
pub fn reset_stage_paths(
	repo_path: &RepoPath,
	paths: &[&str],
) -> Result<()> {
	scope_time!("reset_stage_paths");
	let repo = repo(repo_path)?;
	let tree = match get_head_repo(&repo) {
		Ok(id) => Some(repo.find_commit(id.into())?.tree()?),
		Err(crate::error::Error::NoHead) => None,
		Err(crate::error::Error::Git(error))
			if matches!(
				error.code(),
				git2::ErrorCode::NotFound
					| git2::ErrorCode::UnbornBranch
			) =>
		{
			None
		}
		Err(error) => return Err(error),
	};
	let mut index = repo.index()?;
	let mut exact_paths = paths
		.iter()
		.map(std::path::PathBuf::from)
		.collect::<std::collections::BTreeSet<_>>();
	if let Some(tree) = tree.as_ref() {
		let mut diff =
			repo.diff_tree_to_index(Some(tree), Some(&index), None)?;
		diff.find_similar(None)?;
		for delta in diff.deltas() {
			if delta.status() == Delta::Renamed
				&& delta
					.new_file()
					.path()
					.is_some_and(|new| exact_paths.contains(new))
			{
				if let Some(old) = delta.old_file().path() {
					exact_paths.insert(old.to_path_buf());
				}
			}
		}
	}
	for path in exact_paths {
		if (0..=3).any(|stage| index.get_path(&path, stage).is_some())
		{
			index.remove_path(&path)?;
		}
		let entry = match tree.as_ref() {
			Some(tree) => match tree.get_path(&path) {
				Ok(entry) => Some(entry),
				Err(error)
					if error.code() == git2::ErrorCode::NotFound =>
				{
					None
				}
				Err(error) => return Err(error.into()),
			},
			None => None,
		};
		if let Some(entry) = entry {
			index.add(&IndexEntry {
				ctime: IndexTime::new(0, 0),
				mtime: IndexTime::new(0, 0),
				dev: 0,
				ino: 0,
				mode: entry.filemode().cast_unsigned(),
				uid: 0,
				gid: 0,
				file_size: 0,
				id: entry.id(),
				flags: 0,
				flags_extended: 0,
				path: path.to_string_lossy().as_bytes().to_vec(),
			})?;
		}
	}
	index.write()?;
	Ok(())
}

/// Stage the selected worktree paths exactly, including both sides of a
/// detected rename. The caller handles deleted files separately.
pub fn stage_add_paths(
	repo_path: &RepoPath,
	paths: &[&str],
) -> Result<()> {
	scope_time!("stage_add_paths");
	let repo = repo(repo_path)?;
	let mut index = repo.index()?;
	let mut diff_options = DiffOptions::new();
	diff_options
		.include_untracked(true)
		.recurse_untracked_dirs(true);
	let mut diff = repo.diff_index_to_workdir(
		Some(&index),
		Some(&mut diff_options),
	)?;
	let mut find_options = git2::DiffFindOptions::new();
	find_options.renames(true).for_untracked(true);
	diff.find_similar(Some(&mut find_options))?;
	let selected = paths
		.iter()
		.copied()
		.collect::<std::collections::BTreeSet<_>>();
	let mut removed = std::collections::BTreeSet::new();
	for delta in diff.deltas() {
		if delta.status() == Delta::Renamed
			&& delta
				.new_file()
				.path()
				.and_then(std::path::Path::to_str)
				.is_some_and(|path| selected.contains(path))
		{
			if let Some(path) = delta.old_file().path() {
				removed.insert(path.to_path_buf());
			}
		}
	}
	for path in removed {
		if (0..=3).any(|stage| index.get_path(&path, stage).is_some())
		{
			index.remove_path(&path)?;
		}
	}
	for path in paths {
		index.add_path(std::path::Path::new(path))?;
	}
	index.write()?;
	Ok(())
}

///
pub fn reset_workdir(repo_path: &RepoPath, path: &str) -> Result<()> {
	scope_time!("reset_workdir");
	let repo = repo(repo_path)?;
	let mut checkout_opts = CheckoutBuilder::new();
	checkout_opts
		.update_index(true) // windows: needs this to be true WTF?!
		.remove_untracked(true)
		.force()
		.path(path);
	repo.checkout_index(None, Some(&mut checkout_opts))?;
	Ok(())
}

/// Reset the selected worktree path literally, even when its name contains
/// pathspec metacharacters. Directory selections still include descendants.
pub fn reset_workdir_path(
	repo_path: &RepoPath,
	path: &str,
) -> Result<()> {
	scope_time!("reset_workdir_path");
	let repo = repo(repo_path)?;
	let selected = std::path::PathBuf::from(path);
	let mut paths = std::collections::BTreeSet::new();
	let workdir = repo.workdir();
	let index = repo.index()?;
	let selected_is_indexed_file = (0..=3)
		.any(|stage| index.get_path(&selected, stage).is_some());
	let selected_is_dir = !selected_is_indexed_file
		&& (workdir.is_some_and(|workdir| {
			std::fs::symlink_metadata(workdir.join(&selected))
				.is_ok_and(|metadata| metadata.file_type().is_dir())
		}) || index.iter().any(|entry| {
			let entry_path = String::from_utf8_lossy(&entry.path);
			let entry_path =
				std::path::Path::new(entry_path.as_ref());
			entry_path != selected
				&& entry_path.starts_with(&selected)
		}));

	if selected_is_dir {
		for entry in index.iter() {
			let entry_path = std::path::PathBuf::from(
				String::from_utf8_lossy(&entry.path).as_ref(),
			);
			if entry_path.starts_with(&selected) {
				paths.insert(entry_path);
			}
		}
		if let Some(workdir) = workdir {
			collect_workdir_paths(
				workdir,
				&workdir.join(&selected),
				&mut paths,
			)?;
		}
	} else {
		paths.insert(selected);
	}
	if paths.is_empty() {
		return Ok(());
	}

	let mut checkout_opts = CheckoutBuilder::new();
	checkout_opts
		.update_index(true) // windows: needs this to be true WTF?!
		.remove_untracked(true)
		.force()
		.disable_pathspec_match(true);
	for path in paths {
		checkout_opts.path(path);
	}

	repo.checkout_index(None, Some(&mut checkout_opts))?;
	Ok(())
}

fn collect_workdir_paths(
	workdir: &std::path::Path,
	directory: &std::path::Path,
	paths: &mut std::collections::BTreeSet<std::path::PathBuf>,
) -> Result<()> {
	let entries = match std::fs::read_dir(directory) {
		Ok(entries) => entries,
		Err(error)
			if error.kind() == std::io::ErrorKind::NotFound =>
		{
			return Ok(())
		}
		Err(error) => return Err(error.into()),
	};
	for entry in entries {
		let entry = entry?;
		let path = entry.path();
		if entry.file_type()?.is_dir() {
			collect_workdir_paths(workdir, &path, paths)?;
		} else if let Ok(relative) = path.strip_prefix(workdir) {
			paths.insert(relative.to_path_buf());
		}
	}
	Ok(())
}

///
pub fn reset_repo(
	repo_path: &RepoPath,
	commit: CommitId,
	kind: ResetType,
) -> Result<()> {
	scope_time!("reset_repo");

	let repo = repo(repo_path)?;

	let c = repo.find_commit(commit.into())?;

	repo.reset(c.as_object(), kind, None)?;

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::{
		reset_stage, reset_stage_paths, reset_workdir,
		reset_workdir_path, stage_add_paths,
	};
	use crate::error::Result;
	use crate::sync::{
		commit,
		status::{get_status, StatusType},
		tests::{
			debug_cmd_print, get_statuses, repo_init, repo_init_empty,
		},
		utils::{stage_add_all, stage_add_file},
		RepoPath,
	};
	use std::{
		fs::{self, File},
		io::Write,
		path::Path,
	};

	static HUNK_A: &str = r"
1   start
2
3
4
5
6   middle
7
8
9
0
1   end";

	static HUNK_B: &str = r"
1   start
2   newa
3
4
5
6   middle
7
8
9
0   newb
1   end";

	#[test]
	fn resetting_a_literal_filename_does_not_discard_a_glob_sibling()
	{
		let (temp, _repo) = repo_init().unwrap();
		let repo_path: RepoPath = temp.path().to_path_buf().into();
		for file in ["file*.txt", "fileX.txt"] {
			fs::write(temp.path().join(file), "base\n").unwrap();
			stage_add_file(&repo_path, Path::new(file)).unwrap();
		}
		commit(&repo_path, "base").unwrap();
		for file in ["file*.txt", "fileX.txt"] {
			fs::write(temp.path().join(file), "changed\n").unwrap();
		}
		reset_workdir_path(&repo_path, "file*.txt").unwrap();
		assert_eq!(
			fs::read_to_string(temp.path().join("file*.txt"))
				.unwrap(),
			"base\n"
		);
		assert_eq!(
			fs::read_to_string(temp.path().join("fileX.txt"))
				.unwrap(),
			"changed\n"
		);
	}

	#[test]
	fn resetting_an_empty_directory_does_not_reset_other_paths() {
		let (temp, _repo) = repo_init().unwrap();
		let repo_path: RepoPath = temp.path().to_path_buf().into();
		let sibling = temp.path().join("sibling.txt");
		fs::write(&sibling, "base\n").unwrap();
		stage_add_file(&repo_path, Path::new("sibling.txt")).unwrap();
		commit(&repo_path, "base").unwrap();
		fs::create_dir(temp.path().join("empty")).unwrap();
		fs::write(&sibling, "changed\n").unwrap();

		reset_workdir_path(&repo_path, "empty").unwrap();

		assert_eq!(fs::read_to_string(sibling).unwrap(), "changed\n");
	}

	#[cfg(unix)]
	#[test]
	fn resetting_a_symlink_directory_does_not_touch_its_target() {
		use std::os::unix::fs::symlink;

		let (temp, _repo) = repo_init().unwrap();
		let repo_path: RepoPath = temp.path().to_path_buf().into();
		let target = tempfile::tempdir().unwrap();
		let target_file = target.path().join("file.txt");
		fs::write(&target_file, "base\n").unwrap();
		symlink(target.path(), temp.path().join("linked-dir"))
			.unwrap();
		stage_add_file(&repo_path, Path::new("linked-dir")).unwrap();
		commit(&repo_path, "base").unwrap();
		fs::write(&target_file, "changed\n").unwrap();

		reset_workdir_path(&repo_path, "linked-dir").unwrap();

		assert_eq!(
			fs::read_to_string(target_file).unwrap(),
			"changed\n"
		);
	}

	#[test]
	fn resetting_a_removed_tracked_directory_restores_its_files() {
		let (temp, _repo) = repo_init().unwrap();
		let repo_path: RepoPath = temp.path().to_path_buf().into();
		let file = temp.path().join("folder/file.txt");
		fs::create_dir_all(file.parent().unwrap()).unwrap();
		fs::write(&file, "base\n").unwrap();
		stage_add_file(&repo_path, Path::new("folder/file.txt"))
			.unwrap();
		commit(&repo_path, "base").unwrap();
		fs::remove_dir_all(temp.path().join("folder")).unwrap();

		reset_workdir_path(&repo_path, "folder").unwrap();

		assert_eq!(fs::read_to_string(file).unwrap(), "base\n");
	}

	#[test]
	fn test_reset_only_unstaged() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		let res = get_status(repo_path, StatusType::WorkingDir, None)
			.unwrap();
		assert_eq!(res.len(), 0);

		let file_path = root.join("bar.txt");

		{
			File::create(&file_path)
				.unwrap()
				.write_all(HUNK_A.as_bytes())
				.unwrap();
		}

		debug_cmd_print(repo_path, "git status");

		stage_add_file(repo_path, Path::new("bar.txt")).unwrap();

		debug_cmd_print(repo_path, "git status");

		// overwrite with next content
		{
			File::create(&file_path)
				.unwrap()
				.write_all(HUNK_B.as_bytes())
				.unwrap();
		}

		debug_cmd_print(repo_path, "git status");

		assert_eq!(get_statuses(repo_path), (1, 1));

		reset_workdir(repo_path, "bar.txt").unwrap();

		debug_cmd_print(repo_path, "git status");

		assert_eq!(get_statuses(repo_path), (0, 1));
	}

	#[test]
	fn reset_stage_paths_matches_wildcards_brackets_and_newlines_exactly(
	) {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();
		let paths = [
			"file*.txt",
			"file?.txt",
			"[brackets].txt",
			"line\nname.txt",
		];
		for path in paths {
			fs::write(root.join(path), "base").unwrap();
			let mut index = repo.index().unwrap();
			index.add_path(Path::new(path)).unwrap();
			index.write().unwrap();
		}
		commit(&repo_path, "base files").unwrap();
		for path in paths {
			fs::write(root.join(path), "staged change").unwrap();
			let mut index = repo.index().unwrap();
			index.add_path(Path::new(path)).unwrap();
			index.write().unwrap();
		}

		reset_stage_paths(&repo_path, &["file*.txt"]).unwrap();

		let head = repo
			.head()
			.unwrap()
			.peel_to_commit()
			.unwrap()
			.tree()
			.unwrap();
		let mut index = repo.index().unwrap();
		index.read(true).unwrap();
		for path in paths {
			let staged =
				index.get_path(Path::new(path), 0).unwrap_or_else(
					|| panic!("missing index entry for {path:?}"),
				);
			let committed = head.get_path(Path::new(path)).unwrap();
			assert_eq!(
				staged.id,
				if path == "file*.txt" {
					committed.id()
				} else {
					repo.blob(b"staged change").unwrap()
				}
			);
		}
	}

	#[cfg(unix)]
	#[test]
	fn reset_stage_paths_restores_deletions_modes_and_clears_conflict_stages(
	) {
		use git2::{IndexEntry, IndexTime};
		use std::os::unix::{fs::symlink, fs::PermissionsExt};

		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();
		fs::write(root.join("exec.sh"), "#!/bin/sh\n").unwrap();
		fs::set_permissions(
			root.join("exec.sh"),
			fs::Permissions::from_mode(0o755),
		)
		.unwrap();
		symlink("exec.sh", root.join("link")).unwrap();
		for path in ["deleted.txt", "conflict.txt"] {
			fs::write(root.join(path), "base").unwrap();
		}
		for path in ["exec.sh", "link", "deleted.txt", "conflict.txt"]
		{
			let mut index = repo.index().unwrap();
			index.add_path(Path::new(path)).unwrap();
			index.write().unwrap();
		}
		commit(&repo_path, "base files").unwrap();
		let head = repo
			.head()
			.unwrap()
			.peel_to_commit()
			.unwrap()
			.tree()
			.unwrap();
		let mut index = repo.index().unwrap();
		let mut executable =
			index.get_path(Path::new("exec.sh"), 0).unwrap();
		executable.mode = 0o100644;
		index.add(&executable).unwrap();
		let mut symlink_entry =
			index.get_path(Path::new("link"), 0).unwrap();
		symlink_entry.mode = 0o100644;
		index.add(&symlink_entry).unwrap();
		index.remove_path(Path::new("deleted.txt")).unwrap();
		let conflict_oid =
			index.get_path(Path::new("conflict.txt"), 0).unwrap().id;
		index.remove_path(Path::new("conflict.txt")).unwrap();
		for stage in 1..=3 {
			index
				.add(&IndexEntry {
					ctime: IndexTime::new(0, 0),
					mtime: IndexTime::new(0, 0),
					dev: 0,
					ino: 0,
					mode: 0o100644,
					uid: 0,
					gid: 0,
					file_size: 0,
					id: conflict_oid,
					flags: stage << 12,
					flags_extended: 0,
					path: b"conflict.txt".to_vec(),
				})
				.unwrap();
		}
		index.write().unwrap();

		reset_stage_paths(
			&repo_path,
			&["exec.sh", "link", "deleted.txt", "conflict.txt"],
		)
		.unwrap();

		let mut index = repo.index().unwrap();
		index.read(true).unwrap();
		for path in ["exec.sh", "link", "deleted.txt", "conflict.txt"]
		{
			let index_entry =
				index.get_path(Path::new(path), 0).unwrap();
			let head_entry = head.get_path(Path::new(path)).unwrap();
			assert_eq!(index_entry.id, head_entry.id());
			assert_eq!(
				index_entry.mode,
				head_entry.filemode() as u32
			);
			for stage in 1..=3 {
				assert!(index
					.get_path(Path::new(path), stage)
					.is_none());
			}
		}
	}

	#[test]
	fn selected_rename_stages_and_unstages_both_exact_paths() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();
		fs::write(root.join("old name.txt"), "rename target")
			.unwrap();
		let mut index = repo.index().unwrap();
		index.add_path(Path::new("old name.txt")).unwrap();
		index.write().unwrap();
		commit(&repo_path, "base file").unwrap();
		fs::remove_file(root.join("old name.txt")).unwrap();
		fs::write(root.join("new name.txt"), "rename target")
			.unwrap();

		stage_add_paths(&repo_path, &["new name.txt"]).unwrap();
		let mut index = repo.index().unwrap();
		index.read(true).unwrap();
		assert!(index
			.get_path(Path::new("old name.txt"), 0)
			.is_none());
		assert!(index
			.get_path(Path::new("new name.txt"), 0)
			.is_some());
		drop(index);

		reset_stage_paths(&repo_path, &["new name.txt"]).unwrap();
		let mut index = repo.index().unwrap();
		index.read(true).unwrap();
		assert!(index
			.get_path(Path::new("old name.txt"), 0)
			.is_some());
		assert!(index
			.get_path(Path::new("new name.txt"), 0)
			.is_none());
	}

	#[test]
	fn test_reset_untracked_in_subdir() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		{
			fs::create_dir(root.join("foo")).unwrap();
			File::create(root.join("foo/bar.txt"))
				.unwrap()
				.write_all(b"test\nfoo")
				.unwrap();
		}

		debug_cmd_print(repo_path, "git status");

		assert_eq!(get_statuses(repo_path), (1, 0));

		reset_workdir(repo_path, "foo/bar.txt").unwrap();

		debug_cmd_print(repo_path, "git status");

		assert_eq!(get_statuses(repo_path), (0, 0));
	}

	#[test]
	fn test_reset_folder() -> Result<()> {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		{
			fs::create_dir(root.join("foo"))?;
			File::create(root.join("foo/file1.txt"))?
				.write_all(b"file1")?;
			File::create(root.join("foo/file2.txt"))?
				.write_all(b"file1")?;
			File::create(root.join("file3.txt"))?
				.write_all(b"file3")?;
		}

		stage_add_all(repo_path, "*", None).unwrap();
		commit(repo_path, "msg").unwrap();

		{
			File::create(root.join("foo/file1.txt"))?
				.write_all(b"file1\nadded line")?;
			fs::remove_file(root.join("foo/file2.txt"))?;
			File::create(root.join("foo/file4.txt"))?
				.write_all(b"file4")?;
			File::create(root.join("foo/file5.txt"))?
				.write_all(b"file5")?;
			File::create(root.join("file3.txt"))?
				.write_all(b"file3\nadded line")?;
		}

		assert_eq!(get_statuses(repo_path), (5, 0));

		stage_add_file(repo_path, Path::new("foo/file5.txt"))
			.unwrap();

		assert_eq!(get_statuses(repo_path), (4, 1));

		reset_workdir_path(repo_path, "foo").unwrap();

		assert_eq!(get_statuses(repo_path), (1, 1));

		Ok(())
	}

	#[test]
	fn test_reset_untracked_in_subdir_and_index() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();
		let file = "foo/bar.txt";

		{
			fs::create_dir(root.join("foo")).unwrap();
			File::create(root.join(file))
				.unwrap()
				.write_all(b"test\nfoo")
				.unwrap();
		}

		debug_cmd_print(repo_path, "git status");

		debug_cmd_print(repo_path, "git add .");

		debug_cmd_print(repo_path, "git status");

		{
			File::create(root.join(file))
				.unwrap()
				.write_all(b"test\nfoo\nnewend")
				.unwrap();
		}

		debug_cmd_print(repo_path, "git status");

		assert_eq!(get_statuses(repo_path), (1, 1));

		reset_workdir(repo_path, file).unwrap();

		debug_cmd_print(repo_path, "git status");

		assert_eq!(get_statuses(repo_path), (0, 1));
	}

	#[test]
	fn unstage_in_empty_repo() {
		let file_path = Path::new("foo.txt");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"test\nfoo")
			.unwrap();

		assert_eq!(get_statuses(repo_path), (1, 0));

		stage_add_file(repo_path, file_path).unwrap();

		assert_eq!(get_statuses(repo_path), (0, 1));

		reset_stage(repo_path, file_path.to_str().unwrap()).unwrap();

		assert_eq!(get_statuses(repo_path), (1, 0));
	}

	#[test]
	fn test_reset_untracked_in_subdir_with_cwd_in_subdir() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		{
			fs::create_dir(root.join("foo")).unwrap();
			File::create(root.join("foo/bar.txt"))
				.unwrap()
				.write_all(b"test\nfoo")
				.unwrap();
		}

		debug_cmd_print(repo_path, "git status");

		assert_eq!(get_statuses(repo_path), (1, 0));

		reset_workdir(
			&root.join("foo").as_os_str().to_str().unwrap().into(),
			"foo/bar.txt",
		)
		.unwrap();

		debug_cmd_print(repo_path, "git status");

		assert_eq!(get_statuses(repo_path), (0, 0));
	}

	#[test]
	fn test_reset_untracked_subdir() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		{
			fs::create_dir_all(root.join("foo/bar")).unwrap();
			File::create(root.join("foo/bar/baz.txt"))
				.unwrap()
				.write_all(b"test\nfoo")
				.unwrap();
		}

		debug_cmd_print(repo_path, "git status");

		assert_eq!(get_statuses(repo_path), (1, 0));

		reset_workdir_path(repo_path, "foo/bar").unwrap();

		debug_cmd_print(repo_path, "git status");

		assert_eq!(get_statuses(repo_path), (0, 0));
	}
}
