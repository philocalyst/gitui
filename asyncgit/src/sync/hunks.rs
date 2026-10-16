use super::{
	diff::{
		get_diff_raw, validate_diff_source, DiffOptions, FileDiff,
		HunkHeader,
	},
	RepoPath,
};
use crate::{
	error::{Error, Result},
	hash,
	sync::repository::repo,
};
use git2::{ApplyLocation, ApplyOptions, Diff};
use scopetime::scope_time;

///
pub fn stage_hunk(
	repo_path: &RepoPath,
	file_path: &str,
	hunk_hash: u64,
	options: Option<DiffOptions>,
) -> Result<()> {
	stage_hunk_inner(repo_path, file_path, hunk_hash, options, None)
}

pub fn stage_hunk_checked(
	repo_path: &RepoPath,
	file_path: &str,
	hunk_hash: u64,
	options: Option<DiffOptions>,
	expected_diff: &FileDiff,
) -> Result<()> {
	stage_hunk_inner(
		repo_path,
		file_path,
		hunk_hash,
		options,
		Some(expected_diff),
	)
}

pub fn stage_file_checked(
	repo_path: &RepoPath,
	file_path: &str,
	options: Option<DiffOptions>,
	expected_diff: &FileDiff,
) -> Result<()> {
	let repo = repo(repo_path)?;
	let diff = get_diff_raw(&repo, file_path, false, false, options)?;
	validate_diff_source(&repo, &diff, expected_diff)?;
	let mut index = repo.index()?;
	index.add_path(std::path::Path::new(file_path))?;
	index.write()?;
	Ok(())
}

fn stage_hunk_inner(
	repo_path: &RepoPath,
	file_path: &str,
	hunk_hash: u64,
	options: Option<DiffOptions>,
	expected_diff: Option<&FileDiff>,
) -> Result<()> {
	scope_time!("stage_hunk");

	let repo = repo(repo_path)?;

	let diff = get_diff_raw(&repo, file_path, false, false, options)?;
	if let Some(expected_diff) = expected_diff {
		validate_diff_source(&repo, &diff, expected_diff)?;
	}

	let mut opt = ApplyOptions::new();
	opt.hunk_callback(|hunk| {
		hunk.is_some_and(|hunk| {
			let header = HunkHeader::from(hunk);
			hash(&header) == hunk_hash
		})
	});

	repo.apply(&diff, ApplyLocation::Index, Some(&mut opt))?;

	Ok(())
}

/// this will fail for an all untracked file
pub fn reset_hunk(
	repo_path: &RepoPath,
	file_path: &str,
	hunk_hash: u64,
	options: Option<DiffOptions>,
) -> Result<()> {
	reset_hunk_inner(repo_path, file_path, hunk_hash, options, None)
}

pub fn reset_hunk_checked(
	repo_path: &RepoPath,
	file_path: &str,
	hunk_hash: u64,
	options: Option<DiffOptions>,
	expected_diff: &FileDiff,
) -> Result<()> {
	reset_hunk_inner(
		repo_path,
		file_path,
		hunk_hash,
		options,
		Some(expected_diff),
	)
}

fn reset_hunk_inner(
	repo_path: &RepoPath,
	file_path: &str,
	hunk_hash: u64,
	options: Option<DiffOptions>,
	expected_diff: Option<&FileDiff>,
) -> Result<()> {
	scope_time!("reset_hunk");

	let repo = repo(repo_path)?;

	let diff = get_diff_raw(&repo, file_path, false, false, options)?;
	if let Some(expected_diff) = expected_diff {
		validate_diff_source(&repo, &diff, expected_diff)?;
	}

	let hunk_index = find_hunk_index(&diff, hunk_hash);
	if let Some(hunk_index) = hunk_index {
		let mut hunk_idx = 0;
		let mut opt = ApplyOptions::new();
		opt.hunk_callback(|_hunk| {
			let res = hunk_idx == hunk_index;
			hunk_idx += 1;
			res
		});

		let diff =
			get_diff_raw(&repo, file_path, false, true, options)?;

		repo.apply(&diff, ApplyLocation::WorkDir, Some(&mut opt))?;

		Ok(())
	} else {
		Err(Error::Generic("hunk not found".to_string()))
	}
}

fn find_hunk_index(diff: &Diff, hunk_hash: u64) -> Option<usize> {
	let mut result = None;

	let mut hunk_count = 0;

	let foreach_result = diff.foreach(
		&mut |_, _| true,
		None,
		Some(&mut |_, hunk| {
			let header = HunkHeader::from(hunk);
			if hash(&header) == hunk_hash {
				result = Some(hunk_count);
			}
			hunk_count += 1;
			true
		}),
		None,
	);

	if foreach_result.is_ok() {
		result
	} else {
		None
	}
}

///
pub fn unstage_hunk(
	repo_path: &RepoPath,
	file_path: &str,
	hunk_hash: u64,
	options: Option<DiffOptions>,
) -> Result<bool> {
	unstage_hunk_inner(repo_path, file_path, hunk_hash, options, None)
}

pub fn unstage_hunk_checked(
	repo_path: &RepoPath,
	file_path: &str,
	hunk_hash: u64,
	options: Option<DiffOptions>,
	expected_diff: &FileDiff,
) -> Result<bool> {
	unstage_hunk_inner(
		repo_path,
		file_path,
		hunk_hash,
		options,
		Some(expected_diff),
	)
}

fn unstage_hunk_inner(
	repo_path: &RepoPath,
	file_path: &str,
	hunk_hash: u64,
	options: Option<DiffOptions>,
	expected_diff: Option<&FileDiff>,
) -> Result<bool> {
	scope_time!("revert_hunk");

	let repo = repo(repo_path)?;

	let diff = get_diff_raw(&repo, file_path, true, false, options)?;
	if let Some(expected_diff) = expected_diff {
		validate_diff_source(&repo, &diff, expected_diff)?;
	}
	let diff_count_positive = diff.deltas().len();

	let hunk_index = find_hunk_index(&diff, hunk_hash);
	let hunk_index = hunk_index.map_or_else(
		|| Err(Error::Generic("hunk not found".to_string())),
		Ok,
	)?;

	let diff = get_diff_raw(&repo, file_path, true, true, options)?;

	if diff.deltas().len() != diff_count_positive {
		return Err(Error::Generic(format!(
			"hunk error: {}!={}",
			diff.deltas().len(),
			diff_count_positive
		)));
	}

	let mut count = 0;
	{
		let mut hunk_idx = 0;
		let mut opt = ApplyOptions::new();
		opt.hunk_callback(|_hunk| {
			let res = if hunk_idx == hunk_index {
				count += 1;
				true
			} else {
				false
			};

			hunk_idx += 1;

			res
		});

		repo.apply(&diff, ApplyLocation::Index, Some(&mut opt))?;
	}

	Ok(count == 1)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		error::Result,
		sync::{
			diff::get_diff,
			tests::{repo_init, repo_init_empty, write_commit_file},
		},
	};
	use std::{
		fs::{self, File},
		io::Write,
		path::Path,
	};

	#[test]
	fn reset_untracked_file_which_will_not_find_hunk() -> Result<()> {
		let file_path = Path::new("foo/foo.txt");
		let (_td, repo) = repo_init_empty()?;
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();
		let sub_path = root.join("foo/");

		fs::create_dir_all(&sub_path)?;
		File::create(root.join(file_path))?.write_all(b"test")?;

		let sub_path: &RepoPath = &sub_path.to_str().unwrap().into();
		let diff = get_diff(
			sub_path,
			file_path.to_str().unwrap(),
			false,
			None,
		)?;

		assert!(reset_hunk(
			repo_path,
			file_path.to_str().unwrap(),
			diff.hunks[0].header_hash,
			None,
		)
		.is_err());

		Ok(())
	}

	#[test]
	fn checked_hunk_stage_rejects_same_coordinate_replacement(
	) -> Result<()> {
		let (_tempdir, repo) = repo_init()?;
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();
		write_commit_file(
			&repo,
			"file.txt",
			"one\ntwo\nthree\n",
			"c1",
		);
		let file_path = root.join("file.txt");
		fs::write(&file_path, "one\ndisplayed\nthree\n")?;
		let displayed = get_diff(repo_path, "file.txt", false, None)?;
		fs::write(&file_path, "one\nreplacement\nthree\n")?;

		assert!(stage_hunk_checked(
			repo_path,
			"file.txt",
			displayed.hunks[0].header_hash,
			None,
			&displayed,
		)
		.is_err());
		let cached = std::process::Command::new("git")
			.current_dir(root)
			.args(["diff", "--cached", "--", "file.txt"])
			.output()?;
		assert!(cached.status.success());
		assert!(cached.stdout.is_empty());
		Ok(())
	}

	#[test]
	fn reset_hunk_uses_the_displayed_interhunk_grouping() -> Result<()>
	{
		let (_tempdir, repo) = repo_init()?;
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();
		let base = (0..30)
			.map(|line| format!("line {line}\n"))
			.collect::<String>();
		write_commit_file(&repo, "file.txt", &base, "c1");
		let changed = (0..30)
			.map(|line| match line {
				1 => String::from("first changed\n"),
				3 => String::from("second changed\n"),
				20 => String::from("keep changed\n"),
				_ => format!("line {line}\n"),
			})
			.collect::<String>();
		fs::write(root.join("file.txt"), changed)?;
		let options = DiffOptions {
			context: 0,
			interhunk_lines: 2,
			..DiffOptions::default()
		};
		let displayed =
			get_diff(repo_path, "file.txt", false, Some(options))?;
		assert_eq!(displayed.hunks.len(), 2);

		reset_hunk_checked(
			repo_path,
			"file.txt",
			displayed.hunks[0].header_hash,
			Some(options),
			&displayed,
		)
		.unwrap();
		assert_eq!(
			fs::read_to_string(root.join("file.txt"))?,
			(0..30)
				.map(|line| match line {
					20 => String::from("keep changed\n"),
					_ => format!("line {line}\n"),
				})
				.collect::<String>()
		);
		Ok(())
	}
}
