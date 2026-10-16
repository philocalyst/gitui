use super::{
	apply_selection_with_pairs, load_file, validate_selected_pairs,
	DiffLineReplacement,
};
use crate::{
	error::Result,
	sync::{
		diff::{DiffLinePosition, FileDiff},
		patches::get_file_diff_patch,
		patches::patch_get_hunklines,
		repository::repo,
		utils::repo_write_file,
		RepoPath,
	},
};
use scopetime::scope_time;

/// discards specific lines in an unstaged hunk of a diff
pub fn discard_lines(
	repo_path: &RepoPath,
	file_path: &str,
	lines: &[DiffLinePosition],
) -> Result<()> {
	discard_lines_inner(repo_path, file_path, lines, &[], None)
}

pub fn discard_lines_checked(
	repo_path: &RepoPath,
	file_path: &str,
	lines: &[DiffLinePosition],
	expected_diff: &FileDiff,
) -> Result<()> {
	discard_lines_inner(
		repo_path,
		file_path,
		lines,
		&[],
		Some(expected_diff),
	)
}

/// Discard explicitly paired compact replacement rows while preserving the
/// current file's original line ordering.
pub fn discard_lines_with_pairs_checked(
	repo_path: &RepoPath,
	file_path: &str,
	lines: &[DiffLinePosition],
	pairs: &[DiffLineReplacement],
	expected_diff: &FileDiff,
) -> Result<()> {
	discard_lines_inner(
		repo_path,
		file_path,
		lines,
		pairs,
		Some(expected_diff),
	)
}

fn discard_lines_inner(
	repo_path: &RepoPath,
	file_path: &str,
	lines: &[DiffLinePosition],
	pairs: &[DiffLineReplacement],
	expected_diff: Option<&FileDiff>,
) -> Result<()> {
	scope_time!("discard_lines");

	if lines.is_empty() {
		if !pairs.is_empty() {
			return Err(crate::error::Error::Generic(String::from(
				"replacement pairs require selected diff rows",
			)));
		}
		return Ok(());
	}

	let repo = repo(repo_path)?;
	repo.index()?.read(true)?;

	//TODO: check that file is not new (status modified)

	let new_content = {
		let patch =
			get_file_diff_patch(&repo, file_path, false, false)?;
		let hunks = patch_get_hunklines(&patch)?;
		if let Some(expected_diff) = expected_diff {
			super::stage_tracked::validate_selected_lines(
				lines, &hunks,
			)?;
			if !pairs.is_empty() {
				validate_selected_pairs(lines, pairs, &hunks)?;
			}
			super::stage_tracked::validate_selected_content(
				expected_diff,
				lines,
				&hunks,
			)?;
		}

		let working_content = load_file(&repo, file_path)?;
		if let Some(expected_diff) = expected_diff {
			validate_working_lines(
				expected_diff,
				lines,
				&working_content,
			)?;
		}
		let old_lines =
			working_content.split_inclusive('\n').collect::<Vec<_>>();

		apply_selection_with_pairs(
			lines, pairs, &hunks, &old_lines, false, true,
		)?
	};

	repo_write_file(&repo, file_path, new_content.as_str())?;

	Ok(())
}

fn validate_working_lines(
	expected_diff: &FileDiff,
	lines: &[DiffLinePosition],
	working_content: &str,
) -> Result<()> {
	let working_lines =
		working_content.split_inclusive('\n').collect::<Vec<_>>();
	for selected in lines {
		let Some(new_line_number) = selected.new_lineno else {
			continue;
		};
		let expected = expected_diff
			.hunks
			.iter()
			.flat_map(|hunk| &hunk.lines)
			.find(|line| {
				line.position == *selected
					&& line.line_type
						== crate::sync::diff::DiffLineType::Add
			});
		let actual = usize::try_from(new_line_number)
			.ok()
			.and_then(|line| line.checked_sub(1))
			.and_then(|line| working_lines.get(line));
		let matches_source = match (expected, actual) {
			(Some(expected), Some(actual)) => {
				expected.content.as_ref()
					== actual.trim_end_matches('\n')
			}
			_ => false,
		};
		if !matches_source {
			return Err(crate::error::Error::Generic(String::from(
				"diff changed since selection; refresh and retry",
			)));
		}
	}
	Ok(())
}

#[cfg(test)]
mod test {
	use super::*;
	use crate::sync::{
		diff::get_diff,
		tests::{repo_init, write_commit_file},
	};
	use std::fs;

	#[test]
	fn checked_discard_rejects_same_coordinate_replacement() {
		let (repo_path, repo) = repo_init().unwrap();
		let repo_path: &RepoPath =
			&repo_path.path().to_str().unwrap().into();
		write_commit_file(
			&repo,
			"test.txt",
			"one\ntwo\nthree\n",
			"c1",
		);
		let working = repo.path().parent().unwrap().join("test.txt");
		fs::write(&working, "one\ndisplayed\nthree\n").unwrap();
		let displayed =
			get_diff(repo_path, "test.txt", false, None).unwrap();
		fs::write(&working, "one\nreplacement\nthree\n").unwrap();

		assert!(discard_lines_checked(
			repo_path,
			"test.txt",
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(2),
			}],
			&displayed,
		)
		.is_err());
		assert_eq!(
			fs::read_to_string(working).unwrap(),
			"one\nreplacement\nthree\n"
		);
	}

	#[test]
	fn test_discard() {
		static FILE_1: &str = r"0
1
2
3
4
";

		static FILE_2: &str = r"0


3
4
";

		static FILE_3: &str = r"0
2

3
4
";

		let (path, repo) = repo_init().unwrap();
		let path: &RepoPath = &path.path().to_str().unwrap().into();

		write_commit_file(&repo, "test.txt", FILE_1, "c1");

		repo_write_file(&repo, "test.txt", FILE_2).unwrap();

		discard_lines(
			path,
			"test.txt",
			&[
				DiffLinePosition {
					old_lineno: Some(3),
					new_lineno: None,
				},
				DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(2),
				},
			],
		)
		.unwrap();

		let result_file = load_file(&repo, "test.txt").unwrap();

		assert_eq!(result_file.as_str(), FILE_3);
	}

	#[test]
	fn test_discard2() {
		static FILE_1: &str = r"start
end
";

		static FILE_2: &str = r"start
1
2
end
";

		static FILE_3: &str = r"start
1
end
";

		let (path, repo) = repo_init().unwrap();
		let path: &RepoPath = &path.path().to_str().unwrap().into();

		write_commit_file(&repo, "test.txt", FILE_1, "c1");

		repo_write_file(&repo, "test.txt", FILE_2).unwrap();

		discard_lines(
			path,
			"test.txt",
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(3),
			}],
		)
		.unwrap();

		let result_file = load_file(&repo, "test.txt").unwrap();

		assert_eq!(result_file.as_str(), FILE_3);
	}

	#[test]
	fn test_discard3() {
		static FILE_1: &str = r"start
1
end
";

		static FILE_2: &str = r"start
2
end
";

		static FILE_3: &str = r"start
1
end
";

		let (path, repo) = repo_init().unwrap();
		let path: &RepoPath = &path.path().to_str().unwrap().into();

		write_commit_file(&repo, "test.txt", FILE_1, "c1");

		repo_write_file(&repo, "test.txt", FILE_2).unwrap();

		discard_lines(
			path,
			"test.txt",
			&[
				DiffLinePosition {
					old_lineno: Some(2),
					new_lineno: None,
				},
				DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(2),
				},
			],
		)
		.unwrap();

		let result_file = load_file(&repo, "test.txt").unwrap();

		assert_eq!(result_file.as_str(), FILE_3);
	}

	#[test]
	fn test_discard4() {
		static FILE_1: &str = r"start
mid
end
";

		static FILE_2: &str = r"start
1
mid
2
end
";

		static FILE_3: &str = r"start
mid
end
";

		let (path, repo) = repo_init().unwrap();
		let path: &RepoPath = &path.path().to_str().unwrap().into();

		write_commit_file(&repo, "test.txt", FILE_1, "c1");

		repo_write_file(&repo, "test.txt", FILE_2).unwrap();

		discard_lines(
			path,
			"test.txt",
			&[
				DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(2),
				},
				DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(4),
				},
			],
		)
		.unwrap();

		let result_file = load_file(&repo, "test.txt").unwrap();

		assert_eq!(result_file.as_str(), FILE_3);
	}

	#[test]
	fn test_discard_if_first_selected_line_is_not_in_any_hunk() {
		static FILE_1: &str = r"start
end
";

		static FILE_2: &str = r"start
1
end
";

		static FILE_3: &str = r"start
end
";

		let (path, repo) = repo_init().unwrap();
		let path: &RepoPath = &path.path().to_str().unwrap().into();

		write_commit_file(&repo, "test.txt", FILE_1, "c1");

		repo_write_file(&repo, "test.txt", FILE_2).unwrap();

		discard_lines(
			path,
			"test.txt",
			&[
				DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(1),
				},
				DiffLinePosition {
					old_lineno: None,
					new_lineno: Some(2),
				},
			],
		)
		.unwrap();

		let result_file = load_file(&repo, "test.txt").unwrap();

		assert_eq!(result_file.as_str(), FILE_3);
	}

	//this test shows that we require at least a diff context around add/removes of 1
	#[test]
	fn test_discard_deletions_filestart_breaking_with_zero_context() {
		static FILE_1: &str = r"start
mid
end
";

		static FILE_2: &str = r"start
end
";

		static FILE_3: &str = r"start
mid
end
";

		let (path, repo) = repo_init().unwrap();
		let path: &RepoPath = &path.path().to_str().unwrap().into();

		write_commit_file(&repo, "test.txt", FILE_1, "c1");

		repo_write_file(&repo, "test.txt", FILE_2).unwrap();

		discard_lines(
			path,
			"test.txt",
			&[DiffLinePosition {
				old_lineno: Some(2),
				new_lineno: None,
			}],
		)
		.unwrap();

		let result_file = load_file(&repo, "test.txt").unwrap();

		assert_eq!(result_file.as_str(), FILE_3);
	}

	#[test]
	fn test_discard5() {
		static FILE_1: &str = r"start
";

		static FILE_2: &str = r"start
1";

		static FILE_3: &str = r"start
";

		let (path, repo) = repo_init().unwrap();
		let path: &RepoPath = &path.path().to_str().unwrap().into();

		write_commit_file(&repo, "test.txt", FILE_1, "c1");

		repo_write_file(&repo, "test.txt", FILE_2).unwrap();

		discard_lines(
			path,
			"test.txt",
			&[DiffLinePosition {
				old_lineno: None,
				new_lineno: Some(2),
			}],
		)
		.unwrap();

		let result_file = load_file(&repo, "test.txt").unwrap();

		assert_eq!(result_file.as_str(), FILE_3);
	}
}
