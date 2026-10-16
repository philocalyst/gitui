//! Independent full-file checks for compact replacement mutations.
use asyncgit::sync::{
	self,
	diff::{DiffLinePosition, DiffLineType, FileDiff},
	DiffLineReplacement, RepoPath,
};
use std::{fs, path::Path, process::Command};
fn git(path: &Path, args: &[&str]) -> String {
	let output = Command::new("git")
		.arg("-C")
		.arg(path)
		.args(args)
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"{args:?}: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	String::from_utf8(output.stdout).unwrap()
}
fn fixture(old: &str, new: &str) -> (tempfile::TempDir, RepoPath) {
	let tmp = tempfile::tempdir().unwrap();
	git(tmp.path(), &["init", "-q", "-b", "main"]);
	git(tmp.path(), &["config", "user.name", "Pair reviewer"]);
	git(
		tmp.path(),
		&["config", "user.email", "pairs@example.invalid"],
	);
	git(tmp.path(), &["config", "commit.gpgsign", "false"]);
	git(tmp.path(), &["config", "core.autocrlf", "false"]);
	fs::write(tmp.path().join("sample.rs"), old).unwrap();
	git(tmp.path(), &["add", "."]);
	git(tmp.path(), &["commit", "-qm", "baseline"]);
	fs::write(tmp.path().join("sample.rs"), new).unwrap();
	let repo = tmp.path().to_path_buf().into();
	(tmp, repo)
}
fn diff(repo: &RepoPath, staged: bool) -> FileDiff {
	sync::diff::get_diff(repo, "sample.rs", staged, None).unwrap()
}
fn position(
	diff: &FileDiff,
	text: &str,
	kind: DiffLineType,
) -> DiffLinePosition {
	diff.hunks
		.iter()
		.flat_map(|h| &h.lines)
		.find(|l| {
			l.line_type == kind
				&& l.content.trim_end_matches('\r') == text
		})
		.unwrap()
		.position
}
fn pair(
	diff: &FileDiff,
	old: &str,
	new: &str,
) -> DiffLineReplacement {
	DiffLineReplacement {
		old: position(diff, old, DiffLineType::Delete),
		new: position(diff, new, DiffLineType::Add),
	}
}
const FIRST: &str = "fn first() { let cell_pos = 1; }";
const EDITED: &str = "fn first() { let cell_coord = 1; }";
const SECOND: &str = "fn second() { let other = 2; }";
const SECOND_EDITED: &str = "fn second() { let other = 22; }";
const INSERTED: &str = "fn inserted() { 0; }";
#[test]
fn compact_pair_stage_unstage_and_reset_preserve_neighbor_order() {
	let old = format!("{FIRST}\n{SECOND}\n");
	let new = format!("{INSERTED}\n{EDITED}\n{SECOND_EDITED}\n");
	let (tmp, repo) = fixture(&old, &new);
	let displayed = diff(&repo, false);
	let p = pair(&displayed, FIRST, EDITED);
	sync::stage_lines_with_pairs_checked(
		&repo,
		"sample.rs",
		false,
		&[p.old, p.new],
		&[p],
		&displayed,
	)
	.unwrap();
	assert_eq!(
		git(tmp.path(), &["show", ":sample.rs"]),
		format!("{EDITED}\n{SECOND}\n")
	);
	assert_eq!(
		fs::read_to_string(tmp.path().join("sample.rs")).unwrap(),
		new
	);
	let staged = diff(&repo, true);
	let p = pair(&staged, FIRST, EDITED);
	sync::stage_lines_with_pairs_checked(
		&repo,
		"sample.rs",
		true,
		&[p.old, p.new],
		&[p],
		&staged,
	)
	.unwrap();
	assert_eq!(git(tmp.path(), &["show", ":sample.rs"]), old);
	let displayed = diff(&repo, false);
	let p = pair(&displayed, FIRST, EDITED);
	sync::discard_lines_with_pairs_checked(
		&repo,
		"sample.rs",
		&[p.old, p.new],
		&[p],
		&displayed,
	)
	.unwrap();
	assert_eq!(
		fs::read_to_string(tmp.path().join("sample.rs")).unwrap(),
		format!("{INSERTED}\n{FIRST}\n{SECOND_EDITED}\n")
	);
	assert_eq!(git(tmp.path(), &["show", ":sample.rs"]), old);
}
#[test]
fn mixed_pair_and_insertion_keep_original_relative_positions() {
	for (new, choose_second, expected) in [
		(
			format!("{INSERTED}\n{EDITED}\n{SECOND_EDITED}\n"),
			false,
			format!("{INSERTED}\n{EDITED}\n{SECOND}\n"),
		),
		(
			format!("{EDITED}\n{INSERTED}\n{SECOND_EDITED}\n"),
			false,
			format!("{EDITED}\n{INSERTED}\n{SECOND}\n"),
		),
		(
			format!("{INSERTED}\n{EDITED}\n{SECOND_EDITED}\n"),
			true,
			format!("{INSERTED}\n{FIRST}\n{SECOND_EDITED}\n"),
		),
		(
			format!("{EDITED}\n{INSERTED}\n{SECOND_EDITED}\n"),
			true,
			format!("{FIRST}\n{INSERTED}\n{SECOND_EDITED}\n"),
		),
	] {
		let old = format!("{FIRST}\n{SECOND}\n");
		let (tmp, repo) = fixture(&old, &new);
		let displayed = diff(&repo, false);
		let p = if choose_second {
			pair(&displayed, SECOND, SECOND_EDITED)
		} else {
			pair(&displayed, FIRST, EDITED)
		};
		let inserted =
			position(&displayed, INSERTED, DiffLineType::Add);
		sync::stage_lines_with_pairs_checked(
			&repo,
			"sample.rs",
			false,
			&[p.old, p.new, inserted],
			&[p],
			&displayed,
		)
		.unwrap();
		assert_eq!(
			git(tmp.path(), &["show", ":sample.rs"]),
			expected,
			"chosen second: {choose_second}, new: {new}"
		);
		assert_eq!(
			fs::read_to_string(tmp.path().join("sample.rs")).unwrap(),
			new
		);
	}
}
#[test]
fn complete_compact_selection_roundtrips_exact_crlf_and_eof_bytes() {
	for (sep, eof) in [("\n", "\n"), ("\r\n", "\r\n"), ("\n", "")] {
		let old = format!("{FIRST}{sep}{SECOND}{eof}");
		let new = format!(
			"{INSERTED}{sep}{EDITED}{sep}{SECOND_EDITED}{eof}"
		);
		let (tmp, repo) = fixture(&old, &new);
		let displayed = diff(&repo, false);
		let pairs = [
			pair(&displayed, FIRST, EDITED),
			pair(&displayed, SECOND, SECOND_EDITED),
		];
		let all = displayed
			.hunks
			.iter()
			.flat_map(|h| &h.lines)
			.filter(|l| {
				matches!(
					l.line_type,
					DiffLineType::Add | DiffLineType::Delete
				)
			})
			.map(|l| l.position)
			.collect::<Vec<_>>();
		sync::stage_lines_with_pairs_checked(
			&repo,
			"sample.rs",
			false,
			&all,
			&pairs,
			&displayed,
		)
		.unwrap();
		assert_eq!(git(tmp.path(), &["show", ":sample.rs"]), new);
		let staged = diff(&repo, true);
		sync::stage_lines_with_pairs_checked(
			&repo,
			"sample.rs",
			true,
			&all,
			&pairs,
			&staged,
		)
		.unwrap();
		assert_eq!(git(tmp.path(), &["show", ":sample.rs"]), old);
		sync::discard_lines_with_pairs_checked(
			&repo,
			"sample.rs",
			&all,
			&pairs,
			&displayed,
		)
		.unwrap();
		assert_eq!(
			fs::read_to_string(tmp.path().join("sample.rs")).unwrap(),
			old
		);
	}
}
#[test]
fn invalid_pair_or_stale_source_rejects_without_writing() {
	let old = format!("{FIRST}\n{SECOND}\n");
	let new = format!("{INSERTED}\n{EDITED}\n{SECOND_EDITED}\n");
	let (tmp, repo) = fixture(&old, &new);
	let displayed = diff(&repo, false);
	let p = pair(&displayed, FIRST, EDITED);
	assert!(sync::stage_lines_with_pairs_checked(
		&repo,
		"sample.rs",
		false,
		&[p.old],
		&[p],
		&displayed
	)
	.is_err());
	assert!(sync::stage_lines_with_pairs_checked(
		&repo,
		"sample.rs",
		false,
		&[p.old, p.new],
		&[p, p],
		&displayed
	)
	.is_err());
	assert_eq!(git(tmp.path(), &["show", ":sample.rs"]), old);
	let fresh = new.replace("cell_coord", "cell_fresh");
	fs::write(tmp.path().join("sample.rs"), &fresh).unwrap();
	assert!(sync::stage_lines_with_pairs_checked(
		&repo,
		"sample.rs",
		false,
		&[p.old, p.new],
		&[p],
		&displayed
	)
	.is_err());
	assert!(sync::discard_lines_with_pairs_checked(
		&repo,
		"sample.rs",
		&[p.old, p.new],
		&[p],
		&displayed
	)
	.is_err());
	assert_eq!(git(tmp.path(), &["show", ":sample.rs"]), old);
	assert_eq!(
		fs::read_to_string(tmp.path().join("sample.rs")).unwrap(),
		fresh
	);
}

#[test]
fn mixed_selection_in_later_hunk_uses_local_source_coordinates() {
	let context = (0..80)
		.map(|n| format!("// context {n}\n"))
		.collect::<String>();
	let old =
		format!("{FIRST}\n{SECOND}\n{context}fn tail() {{ 3; }}\n");
	let new=format!("{EDITED}\n{SECOND}\n{}// unrelated insertion one\n// unrelated insertion two\n{}fn selected_tail_insert() {{ 4; }}\nfn tail() {{ 3; }}\n",context[..context.find("// context 40").unwrap()].to_string(),context[context.find("// context 40").unwrap()..].to_string());
	let (tmp, repo) = fixture(&old, &new);
	let displayed = diff(&repo, false);
	let p = pair(&displayed, FIRST, EDITED);
	let tail_insert = position(
		&displayed,
		"fn selected_tail_insert() { 4; }",
		DiffLineType::Add,
	);
	sync::stage_lines_with_pairs_checked(
		&repo,
		"sample.rs",
		false,
		&[p.old, p.new, tail_insert],
		&[p],
		&displayed,
	)
	.unwrap();
	assert_eq!(git(tmp.path(), &["show", ":sample.rs"]),format!("{EDITED}\n{SECOND}\n{context}fn selected_tail_insert() {{ 4; }}\nfn tail() {{ 3; }}\n"));
	assert_eq!(
		fs::read_to_string(tmp.path().join("sample.rs")).unwrap(),
		new
	);
}

#[test]
fn mixed_reverse_selection_restores_deletion_between_retained_neighbors(
) {
	let removed = "const REMOVED: i32 = 7;";
	let old = format!("{FIRST}\n{removed}\n{SECOND}\n");
	let new = format!("{EDITED}\n{SECOND_EDITED}\n");
	let expected = format!("{EDITED}\n{removed}\n{SECOND}\n");
	let (tmp, repo) = fixture(&old, &new);
	let displayed = diff(&repo, false);
	let p = pair(&displayed, SECOND, SECOND_EDITED);
	let restore = position(&displayed, removed, DiffLineType::Delete);
	sync::discard_lines_with_pairs_checked(
		&repo,
		"sample.rs",
		&[p.old, p.new, restore],
		&[p],
		&displayed,
	)
	.unwrap();
	assert_eq!(
		fs::read_to_string(tmp.path().join("sample.rs")).unwrap(),
		expected
	);
	fs::write(tmp.path().join("sample.rs"), &new).unwrap();
	git(tmp.path(), &["add", "sample.rs"]);
	let staged = diff(&repo, true);
	let p = pair(&staged, SECOND, SECOND_EDITED);
	let restore = position(&staged, removed, DiffLineType::Delete);
	sync::stage_lines_with_pairs_checked(
		&repo,
		"sample.rs",
		true,
		&[p.old, p.new, restore],
		&[p],
		&staged,
	)
	.unwrap();
	assert_eq!(git(tmp.path(), &["show", ":sample.rs"]), expected);
	assert_eq!(
		fs::read_to_string(tmp.path().join("sample.rs")).unwrap(),
		new
	);
}
