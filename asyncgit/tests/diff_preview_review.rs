//! Independent native preview checks. Git CLI below builds fixtures only.
use asyncgit::{
	diff_preview::{
		changed_grapheme_ranges, inline_diff_segments,
		render_diff_preview, DiffPreview, InlineSegmentKind,
		PreviewLineKind, PreviewOptions, PreviewPreference,
		PreviewRenderer, PreviewStatus,
	},
	sync::{self, diff::DiffOptions, RepoPath},
	DiffParams, DiffType,
};
use std::{
	fs, path::Path, process::Command, sync::atomic::AtomicBool,
};
use unicode_segmentation::UnicodeSegmentation;
fn git(root: &Path, args: &[&str]) -> String {
	let out = Command::new("git")
		.arg("-C")
		.arg(root)
		.args(args)
		.output()
		.unwrap();
	assert!(
		out.status.success(),
		"{args:?}: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).unwrap()
}
fn fixture() -> (tempfile::TempDir, RepoPath) {
	let t = tempfile::tempdir().unwrap();
	git(t.path(), &["init", "-q", "-b", "main"]);
	git(t.path(), &["config", "user.name", "Preview reviewer"]);
	git(
		t.path(),
		&["config", "user.email", "preview@example.invalid"],
	);
	git(t.path(), &["config", "commit.gpgsign", "false"]);
	let r = t.path().to_path_buf().into();
	(t, r)
}
fn p(path: &str, diff_type: DiffType) -> DiffParams {
	DiffParams {
		path: path.into(),
		diff_type,
		options: DiffOptions::default(),
	}
}
fn hash(r: &RepoPath, p: &DiffParams) -> u64 {
	let d = match p.diff_type {
		DiffType::WorkDir => {
			sync::diff::get_diff(r, &p.path, false, Some(p.options))
		}
		DiffType::Stage => {
			sync::diff::get_diff(r, &p.path, true, Some(p.options))
		}
		DiffType::Commit(id) => sync::diff::get_diff_commit(
			r,
			id,
			p.path.clone(),
			Some(p.options),
		),
		DiffType::Commits(ids) => sync::diff::get_diff_commits(
			r,
			ids,
			p.path.clone(),
			Some(p.options),
		),
	}
	.unwrap();
	asyncgit::hash(&d)
}
fn render(r: &RepoPath, p: &DiffParams) -> DiffPreview {
	render_mode(r, p, PreviewPreference::Auto)
}
fn render_mode(
	r: &RepoPath,
	p: &DiffParams,
	preference: PreviewPreference,
) -> DiffPreview {
	render_diff_preview(
		r,
		p,
		hash(r, p),
		PreviewOptions { preference },
		&AtomicBool::new(false),
	)
}
type SourceRow =
	(usize, PreviewLineKind, Option<u32>, Option<u32>, String);
fn expanded_rows(preview: &DiffPreview) -> Vec<SourceRow> {
	let mut rows = Vec::new();
	for line in &preview.lines {
		if let Some(block) = &line.block_replacement {
			let mut old = String::new();
			let mut new = String::new();
			for segment in &block.segments {
				if segment.kind != InlineSegmentKind::Added {
					old.push_str(
						&block.old_text[segment.old_range.clone()],
					);
				}
				if segment.kind != InlineSegmentKind::Removed {
					new.push_str(
						&block.new_text[segment.new_range.clone()],
					);
				}
				if segment.kind == InlineSegmentKind::Shared {
					assert_eq!(
						&block.old_text[segment.old_range.clone()],
						&block.new_text[segment.new_range.clone()]
					);
				}
			}
			assert_eq!(old, block.old_text);
			assert_eq!(new, block.new_text);
			for source in &block.old_lines {
				rows.push((
					source.patch_index,
					PreviewLineKind::Deletion,
					source.line_number,
					None,
					block.old_text[source.text_range.clone()]
						.to_owned(),
				));
			}
			for source in &block.new_lines {
				rows.push((
					source.patch_index,
					PreviewLineKind::Addition,
					None,
					source.line_number,
					block.new_text[source.text_range.clone()]
						.to_owned(),
				));
			}
		} else if let Some(pair) = &line.replacement {
			let mut old = String::new();
			let mut new = String::new();
			for segment in &pair.segments {
				if segment.kind != InlineSegmentKind::Added {
					old.push_str(
						&pair.old_text[segment.old_range.clone()],
					);
				}
				if segment.kind != InlineSegmentKind::Removed {
					new.push_str(
						&pair.new_text[segment.new_range.clone()],
					);
				}
				if segment.kind == InlineSegmentKind::Shared {
					assert_eq!(
						&pair.old_text[segment.old_range.clone()],
						&pair.new_text[segment.new_range.clone()]
					);
				}
			}
			assert_eq!(old, pair.old_text);
			assert_eq!(new, pair.new_text);
			assert_ne!(pair.old_patch_index, pair.new_patch_index);
			rows.push((
				pair.old_patch_index,
				PreviewLineKind::Deletion,
				pair.old_lineno,
				None,
				old,
			));
			rows.push((
				pair.new_patch_index,
				PreviewLineKind::Addition,
				None,
				pair.new_lineno,
				new,
			));
		} else {
			rows.push((
				line.patch_index,
				line.kind,
				line.old_lineno,
				line.new_lineno,
				line.text.clone(),
			));
		}
	}
	rows.sort_by_key(|row| row.0);
	rows
}
fn commit(root: &Path, msg: &str) -> sync::CommitId {
	git(root, &["add", "--all"]);
	git(root, &["commit", "-qm", msg]);
	git2::Oid::from_str(git(root, &["rev-parse", "HEAD"]).trim())
		.unwrap()
		.into()
}
fn text(p: &DiffPreview) -> String {
	p.lines
		.iter()
		.map(|l| l.text.as_str())
		.collect::<Vec<_>>()
		.join("\n")
}

#[test]
fn prose_reflow_shares_words_across_physical_lines_and_keeps_exact_patch_ledger(
) {
	let (t, r) = fixture();
	let old = "GPUI lays out elements,\nhandles input and paints windows.\n";
	let new = "GPUI lays out elements, handles input\nand paints windows efficiently.\n";
	fs::write(t.path().join("README.md"), old).unwrap();
	commit(t.path(), "Original paragraph");
	fs::write(t.path().join("README.md"), new).unwrap();
	let params = p("README.md", DiffType::WorkDir);
	let compact = render(&r, &params);
	assert_eq!(
		expanded_rows(&compact),
		expanded_rows(&render_mode(
			&r,
			&params,
			PreviewPreference::Unified
		))
	);
	let blocks = compact
		.lines
		.iter()
		.filter_map(|row| row.block_replacement.as_ref())
		.collect::<Vec<_>>();
	assert_eq!(blocks.len(), 1, "Physical reflow must use one paragraph projection: {compact:?}");
	let block = blocks[0];
	assert_eq!(block.old_lines.len(), 2);
	assert_eq!(block.new_lines.len(), 2);
	let mut display = String::new();
	for segment in &block.segments {
		let value = if segment.kind == InlineSegmentKind::Removed {
			&block.old_text[segment.old_range.clone()]
		} else {
			&block.new_text[segment.new_range.clone()]
		};
		display.push_str(value);
	}
	assert_eq!(display.matches("handles input").count(), 1, "Shared wording crossing an old line boundary appeared twice: {display}");
	assert_eq!(git(t.path(), &["show", ":README.md"]), old);
	assert_eq!(
		fs::read_to_string(t.path().join("README.md")).unwrap(),
		new
	);
}

#[test]
fn gpui_paragraph_rewrite_collapses_across_moved_line_boundaries() {
	let (t, r) = fixture();
	let old = "GPUI is a fast, GPU-accelerated UI framework for Rust. You\ninterface with flexbox-style elements and Tailwind-style methods.\nGPUI owns handles for you, and GPUI draws each window for the\ndisplay on macOS, Windows, Linux and the web.\n";
	let new = "GPUI is a Rust UI framework for macOS, Windows, Linux, and the web. It\nbuilds an element tree for layout, input, and painting; views read\napplication state. After changing state through the relevant handle,\ncall `notify` so GPUI can render the windows that depend on it.\n";
	fs::write(t.path().join("README.md"), old).unwrap();
	commit(t.path(), "Original GPUI prose");
	fs::write(t.path().join("README.md"), new).unwrap();
	let params = p("README.md", DiffType::WorkDir);
	let preview = render(&r, &params);
	assert_eq!(
		expanded_rows(&preview),
		expanded_rows(&render_mode(
			&r,
			&params,
			PreviewPreference::Unified
		))
	);
	let block = preview.lines.iter().find_map(|row| row.block_replacement.as_ref()).expect("Substantial paragraph rewrite must share words across lines");
	assert_eq!(block.old_lines.len(), 4);
	assert_eq!(block.new_lines.len(), 4);
	let display = block
		.segments
		.iter()
		.map(|segment| {
			if segment.kind == InlineSegmentKind::Removed {
				&block.old_text[segment.old_range.clone()]
			} else {
				&block.new_text[segment.new_range.clone()]
			}
		})
		.collect::<String>();
	for shared in ["macOS", "Windows", "Linux"] {
		assert_eq!(
			display.matches(shared).count(),
			1,
			"Moved shared phrase was duplicated: {display}"
		);
	}
	for segment in &block.segments {
		let (text, range) = match segment.kind {
			InlineSegmentKind::Removed => {
				(&block.old_text, &segment.old_range)
			}
			InlineSegmentKind::Added => {
				(&block.new_text, &segment.new_range)
			}
			InlineSegmentKind::Shared => continue,
		};
		for boundary in [range.start, range.end] {
			let before = text[..boundary].chars().next_back();
			let after = text[boundary..].chars().next();
			assert!(
				!(before.is_some_and(char::is_alphanumeric)
					&& after.is_some_and(char::is_alphanumeric)),
				"Prose changed inside a word: {segment:?}"
			);
		}
	}
}

#[test]
fn single_line_prose_uses_whole_words_but_distant_fenced_code_keeps_characters(
) {
	let (t, r) = fixture();
	let old = format!("This sibling remains untouched.\n\n```rust\n{}let cell_pos = 4;\n```\n", "// unchanged context\n".repeat(40));
	let new = old
		.replace("remains untouched", "has an independent edit")
		.replace("cell_pos", "cell_coord");
	fs::write(t.path().join("README.md"), &old).unwrap();
	commit(t.path(), "Prose and fenced code");
	fs::write(t.path().join("README.md"), &new).unwrap();
	let params = p("README.md", DiffType::WorkDir);
	let preview = render(&r, &params);
	assert_eq!(
		expanded_rows(&preview),
		expanded_rows(&render_mode(
			&r,
			&params,
			PreviewPreference::Unified
		))
	);
	let prose = preview
		.lines
		.iter()
		.filter_map(|row| row.replacement.as_ref())
		.find(|pair| pair.old_text.starts_with("This sibling"))
		.expect(
			"short related prose should retain one-to-one pairing",
		);
	let removed = prose
		.segments
		.iter()
		.filter(|segment| segment.kind == InlineSegmentKind::Removed)
		.map(|segment| &prose.old_text[segment.old_range.clone()])
		.collect::<String>();
	assert!(
		removed.contains("remains") && removed.contains("untouched"),
		"Prose was split inside words: {removed}"
	);
	let code = preview
		.lines
		.iter()
		.filter_map(|row| row.replacement.as_ref())
		.find(|pair| pair.old_text.contains("cell_pos"))
		.expect("fenced code should remain paired");
	assert!(
		code.segments
			.iter()
			.any(|segment| segment.kind == InlineSegmentKind::Shared
				&& code.old_text[segment.old_range.clone()]
					.contains("cell_")),
		"Fenced code lost character precision: {code:?}"
	);
}

#[test]
fn distant_markdown_fences_and_unrelated_paragraphs_do_not_become_prose_blocks(
) {
	let (t, r) = fixture();
	let reflow_old = "GPUI lays out elements,\nhandles input and paints windows.\n";
	let reflow_new = "GPUI lays out elements, handles input\nand paints windows efficiently.\n";
	let prefix = format!(
		"Intro.\n\n```text\n{}",
		"unchanged fence content\n".repeat(40)
	);
	let old = format!("{prefix}{reflow_old}```\n");
	let new = format!("{prefix}{reflow_new}```\n");
	fs::write(t.path().join("README.md"), &old).unwrap();
	commit(t.path(), "Distant fenced content");
	fs::write(t.path().join("README.md"), &new).unwrap();
	let params = p("README.md", DiffType::WorkDir);
	let preview = render(&r, &params);
	assert!(preview
		.lines
		.iter()
		.all(|row| row.block_replacement.is_none()));
	assert_eq!(
		expanded_rows(&preview),
		expanded_rows(&render_mode(
			&r,
			&params,
			PreviewPreference::Unified
		))
	);
	fs::write(t.path().join("README.md"), "Cedar winds buffet remote northern cabins.\nOwls gather beside icy streams.\n").unwrap();
	commit(t.path(), "Independent prose");
	fs::write(t.path().join("README.md"), "Marble columns support ancient temples.\nArtists chisel smooth stone statues.\n").unwrap();
	assert!(render(&r, &params)
		.lines
		.iter()
		.all(|row| row.block_replacement.is_none()));
}
#[test]
fn small_edit_is_contextual_unified_with_exact_changed_characters() {
	let (t, r) = fixture();
	let old: String = (0..400)
		.map(|n| format!("let value_{n} = {n};\n"))
		.collect();
	fs::write(t.path().join("sample.rs"), &old).unwrap();
	commit(t.path(), "base");
	fs::write(
		t.path().join("sample.rs"),
		old.replace("value_201 = 201", "value_201 = 999"),
	)
	.unwrap();
	let v = render(&r, &p("sample.rs", DiffType::WorkDir));
	assert_eq!(v.renderer, PreviewRenderer::Native);
	assert_eq!(v.status, PreviewStatus::Unified);
	assert!(v.lines.len() < 30);
	let line = v
		.lines
		.iter()
		.find(|line| line.kind == PreviewLineKind::Replacement)
		.unwrap();
	let pair = line.replacement.as_ref().unwrap();
	assert_eq!(
		(line.old_lineno, line.new_lineno),
		(Some(202), Some(202))
	);
	for (kind, expected) in [
		(InlineSegmentKind::Removed, "201"),
		(InlineSegmentKind::Added, "999"),
	] {
		let chars: String = pair
			.segments
			.iter()
			.filter(|segment| segment.kind == kind)
			.map(|segment| {
				if kind == InlineSegmentKind::Removed {
					&pair.old_text[segment.old_range.clone()]
				} else {
					&pair.new_text[segment.new_range.clone()]
				}
			})
			.collect();
		assert_eq!(chars, expected);
	}
	let plain = render_mode(
		&r,
		&p("sample.rs", DiffType::WorkDir),
		PreviewPreference::Unified,
	);
	assert_eq!(expanded_rows(&v), expanded_rows(&plain));
	assert_eq!(v.lines.len() + 1, plain.lines.len());
	assert!(!text(&v).contains('\x1b'));
}
#[test]
fn source_versions_are_exact_and_rendering_preserves_index_and_worktree(
) {
	let (t, r) = fixture();
	let path = t.path().join("sample.rs");
	fs::write(&path, "const VALUE: i32 = 11;\n").unwrap();
	let old = commit(t.path(), "old");
	fs::write(&path, "const VALUE: i32 = 22;\n").unwrap();
	let new = commit(t.path(), "new");
	fs::write(&path, "const VALUE: i32 = 33;\n").unwrap();
	git(t.path(), &["add", "sample.rs"]);
	fs::write(&path, "const VALUE: i32 = 44;\n").unwrap();
	for (source, a, b) in [
		(DiffType::Commit(new), 11, 22),
		(
			DiffType::Commits(sync::commit_files::OldNew {
				old,
				new,
			}),
			11,
			22,
		),
		(DiffType::Stage, 22, 33),
		(DiffType::WorkDir, 33, 44),
	] {
		let v = render(&r, &p("sample.rs", source));
		let source_rows = expanded_rows(&v);
		assert!(source_rows
			.iter()
			.any(|l| l.1 == PreviewLineKind::Deletion
				&& l.4 == format!("const VALUE: i32 = {a};")));
		assert!(source_rows
			.iter()
			.any(|l| l.1 == PreviewLineKind::Addition
				&& l.4 == format!("const VALUE: i32 = {b};")));
	}
	assert_eq!(
		git(t.path(), &["show", ":sample.rs"]),
		"const VALUE: i32 = 33;\n"
	);
	assert_eq!(
		fs::read_to_string(path).unwrap(),
		"const VALUE: i32 = 44;\n"
	);
}
#[test]
fn multiple_edits_preserve_graphemes_and_unchanged_middle_text() {
	for (old, new) in [
		("alpha red beta blue", "alpha RED beta BLUE"),
		("🇺🇸 e\u{301}", "🇨🇦 e\u{300}"),
		("中文 🐱", "中文 🐈"),
	] {
		let (a, b) = changed_grapheme_ranges(old, new);
		for (s, ranges) in [(old, a), (new, b)] {
			let boundaries: Vec<_> = s
				.grapheme_indices(true)
				.map(|(i, _)| i)
				.chain(std::iter::once(s.len()))
				.collect();
			assert!(!ranges.is_empty());
			for x in &ranges {
				assert!(
					boundaries.contains(&x.start)
						&& boundaries.contains(&x.end)
				);
			}
			if s.starts_with("alpha") {
				assert_eq!(ranges.len(), 2);
				assert!(ranges
					.iter()
					.all(|x| !s[x.clone()].contains("beta")));
			}
		}
	}
}
#[test]
fn stale_native_snapshot_cannot_publish_new_content() {
	let (t, r) = fixture();
	let path = t.path().join("sample.rs");
	fs::write(&path, "fn original() {}\n").unwrap();
	commit(t.path(), "base");
	fs::write(&path, "fn selected_snapshot() {}\n").unwrap();
	let params = p("sample.rs", DiffType::WorkDir);
	let old = hash(&r, &params);
	fs::write(path, "fn changed_after_selection() {}\n").unwrap();
	let v = render_diff_preview(
		&r,
		&params,
		old,
		PreviewOptions::default(),
		&AtomicBool::new(false),
	);
	assert_eq!(v.status, PreviewStatus::Stale);
	assert!(!text(&v).contains("changed_after_selection"));
}
#[test]
fn literal_paths_and_whitespace_options_are_honored() {
	let (t, r) = fixture();
	for name in ["[sample].rs", "-sample.rs", "Dockerfile"] {
		fs::write(t.path().join(name), "value = 1\n").unwrap();
	}
	fs::write(t.path().join("s.rs"), "SECRET_OTHER_FILE\n").unwrap();
	commit(t.path(), "base");
	for name in ["[sample].rs", "-sample.rs", "Dockerfile"] {
		fs::write(t.path().join(name), "value = 2\n").unwrap();
		let v = render(&r, &p(name, DiffType::WorkDir));
		assert!(text(&v).contains(name));
		assert!(!text(&v).contains("SECRET_OTHER_FILE"));
		assert!(expanded_rows(&v)
			.iter()
			.any(|l| l.1 == PreviewLineKind::Addition
				&& l.4 == "value = 2"));
	}
	fs::write(t.path().join("[sample].rs"), "value  =  1\n").unwrap();
	let mut params = p("[sample].rs", DiffType::WorkDir);
	params.options.ignore_whitespace = true;
	let v = render(&r, &params);
	assert!(!v.lines.iter().any(|l| matches!(
		l.kind,
		PreviewLineKind::Addition
			| PreviewLineKind::Deletion
			| PreviewLineKind::Replacement
	)));
}
#[test]
fn oversized_and_binary_data_are_not_fabricated_as_unchanged() {
	let (t, r) = fixture();
	fs::write(
		t.path().join("large.txt"),
		vec![b'a'; 9 * 1024 * 1024],
	)
	.unwrap();
	commit(t.path(), "large base");
	fs::write(
		t.path().join("large.txt"),
		vec![b'b'; 9 * 1024 * 1024],
	)
	.unwrap();
	let v = render(&r, &p("large.txt", DiffType::WorkDir));
	assert_eq!(v.status, PreviewStatus::TooLarge);
	assert!(text(&v).len() < 1024);
	fs::write(t.path().join("binary.dat"), b"\0binary").unwrap();
	assert_eq!(
		render(&r, &p("binary.dat", DiffType::WorkDir)).status,
		PreviewStatus::Binary
	);
}
#[test]
fn native_preview_executes_no_git_difft_or_delta() {
	let (t, _) = fixture();
	fs::write(
		t.path().join("sample.rs"),
		"fn value() -> i32 { 1 }\n",
	)
	.unwrap();
	commit(t.path(), "base");
	fs::write(
		t.path().join("sample.rs"),
		"fn value() -> i32 { 2 }\n",
	)
	.unwrap();
	let bin = tempfile::tempdir().unwrap();
	let marker = bin.path().join("spawned");
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		for name in ["git", "difft", "delta"] {
			let f = bin.path().join(name);
			fs::write(
				&f,
				format!(
					"#!/bin/sh\nprintf called >> '{}'\nexit 88\n",
					marker.display()
				),
			)
			.unwrap();
			fs::set_permissions(f, fs::Permissions::from_mode(0o755))
				.unwrap();
		}
	}
	let out = Command::new(std::env::current_exe().unwrap())
		.args([
			"--exact",
			"render_in_isolated_native_child",
			"--ignored",
			"--nocapture",
		])
		.env("GITUI_NATIVE_REVIEW_REPO", t.path())
		.env("PATH", bin.path())
		.output()
		.unwrap();
	assert!(
		out.status.success(),
		"{}",
		String::from_utf8_lossy(&out.stdout)
	);
	assert!(
		!marker.exists(),
		"Preview ran an external renderer or Git process"
	);
}
#[test]
#[ignore = "invoked by parent test with isolated process environment"]
fn render_in_isolated_native_child() {
	let root = std::env::var_os("GITUI_NATIVE_REVIEW_REPO").unwrap();
	let repo = std::path::PathBuf::from(root).into();
	let v = render(&repo, &p("sample.rs", DiffType::WorkDir));
	assert_eq!(v.status, PreviewStatus::Unified);
	assert!(expanded_rows(&v)
		.iter()
		.any(|l| l.1 == PreviewLineKind::Addition
			&& l.4.contains("{ 2 }")));
}

#[test]
fn combined_identifier_has_one_shared_prefix_and_whole_changed_subwords(
) {
	let old = "vec2 cell_pos;";
	let new = "vec2 cell_coord;";
	let parts = inline_diff_segments(old, new).unwrap();
	let descriptions: Vec<_> = parts
		.iter()
		.map(|part| {
			(
				part.kind,
				if part.kind == InlineSegmentKind::Added {
					&new[part.new_range.clone()]
				} else {
					&old[part.old_range.clone()]
				},
			)
		})
		.collect();
	assert_eq!(
		descriptions,
		vec![
			(InlineSegmentKind::Shared, "vec2 cell_"),
			(InlineSegmentKind::Removed, "pos"),
			(InlineSegmentKind::Added, "coord"),
			(InlineSegmentKind::Shared, ";")
		]
	);
}

#[test]
fn compact_projection_reconstructs_uneven_changes_and_separate_hunks()
{
	let (t, r) = fixture();
	let mut old = vec![
		"fn first() -> i32 { 10 }".to_owned(),
		"fn retained() -> i32 { 20 }".to_owned(),
	];
	old.extend((0..30).map(|n| format!("// unchanged context {n}")));
	old.push("fn last() -> i32 { 30 }".to_owned());
	fs::write(t.path().join("sample.rs"), old.join("\n") + "\n")
		.unwrap();
	commit(t.path(), "base");
	let mut new = vec![
		"fn newly_inserted() -> i32 { 777 }".to_owned(),
		"fn first() -> i32 { 11 }".to_owned(),
		"fn retained() -> i32 { 20 }".to_owned(),
	];
	new.extend((0..30).map(|n| format!("// unchanged context {n}")));
	new.push("fn last() -> i32 { 31 }".to_owned());
	fs::write(t.path().join("sample.rs"), new.join("\n") + "\n")
		.unwrap();
	let params = p("sample.rs", DiffType::WorkDir);
	let compact = render(&r, &params);
	let plain = render_mode(&r, &params, PreviewPreference::Unified);
	assert_eq!(expanded_rows(&compact), expanded_rows(&plain));
	let new_numbers = compact
		.lines
		.iter()
		.filter_map(|line| line.new_lineno)
		.collect::<Vec<_>>();
	assert!(
		new_numbers.windows(2).all(|pair| pair[0] < pair[1]),
		"compact rows must follow new source order: {new_numbers:?}"
	);
	let inserted_at = compact
		.lines
		.iter()
		.position(|line| line.text.contains("newly_inserted"))
		.unwrap();
	let replacement_at = compact
		.lines
		.iter()
		.position(|line| line.replacement.is_some())
		.unwrap();
	assert!(
		inserted_at < replacement_at,
		"insertion before a replacement must be displayed before it"
	);
	let replacements: Vec<_> = compact
		.lines
		.iter()
		.filter_map(|line| line.replacement.as_ref())
		.collect();
	assert_eq!(replacements.len(), 2);
	assert!(replacements[0].old_text.contains("fn first()"));
	assert!(replacements[0].new_text.contains("fn first()"));
	assert_eq!(
		(replacements[0].old_lineno, replacements[0].new_lineno),
		(Some(1), Some(2))
	);
	assert!(compact
		.lines
		.iter()
		.any(|line| line.kind == PreviewLineKind::Addition
			&& line.text.contains("newly_inserted")));
}

#[test]
fn structural_metadata_uses_exact_commit_index_and_worktree_snapshots(
) {
	let (t, r) = fixture();
	let path = t.path().join("source.rs");
	fs::write(&path, "fn original() {\n 1;\n}\n").unwrap();
	let old = commit(t.path(), "old declaration");
	fs::write(&path, "fn committed() {\n 2;\n}\n").unwrap();
	let new = commit(t.path(), "committed declaration");
	fs::write(&path, "fn indexed() {\n 3;\n}\n").unwrap();
	git(t.path(), &["add", "source.rs"]);
	fs::write(&path, "fn dirty() {\n 4;\n}\n").unwrap();
	for (source, before, after) in [
		(DiffType::Commit(new), "original", "committed"),
		(
			DiffType::Commits(sync::commit_files::OldNew {
				old,
				new,
			}),
			"original",
			"committed",
		),
		(DiffType::Stage, "committed", "indexed"),
		(DiffType::WorkDir, "indexed", "dirty"),
	] {
		let params = p("source.rs", source);
		let preview = render(&r, &params);
		let regions = &preview
			.structural
			.as_ref()
			.expect("valid Rust pair yields structural regions")
			.regions;
		assert_eq!(regions.len(), 2);
		let removed = regions
			.iter()
			.find(|region| region.label == before)
			.unwrap();
		let added = regions
			.iter()
			.find(|region| region.label == after)
			.unwrap();
		assert_eq!(removed.old_lines, Some(0..3));
		assert_eq!(removed.new_lines, None);
		assert_eq!(added.old_lines, None);
		assert_eq!(added.new_lines, Some(0..3));
		assert!(removed.contains_changes && added.contains_changes);
		assert!(render_mode(&r, &params, PreviewPreference::Unified)
			.structural
			.is_none());
	}
	assert_eq!(
		git(t.path(), &["show", ":source.rs"]),
		"fn indexed() {\n 3;\n}\n"
	);
	assert_eq!(
		fs::read_to_string(path).unwrap(),
		"fn dirty() {\n 4;\n}\n"
	);
}
