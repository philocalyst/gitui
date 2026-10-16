//! Independent checks for syntax-region provenance and safe text-diff fallback.
use asyncgit::structural::{
	analyze_regions, StructuralAnalysis, StructuralChange,
};
use std::{
	collections::HashSet,
	sync::atomic::AtomicBool,
	time::{Duration, Instant},
};

fn analyze(path: &str, old: &str, new: &str) -> StructuralAnalysis {
	analyze_regions(
		path,
		old.as_bytes(),
		new.as_bytes(),
		&AtomicBool::new(false),
	)
	.expect("small complete source pair must parse")
}

#[test]
fn formatting_edits_are_visible_to_fold_mutation_guards() {
	let result = analyze(
		"sample.rs",
		"fn work(){let a=1;}\n",
		"fn work() { let a = 1; }\n",
	);
	let function =
		result.regions.iter().find(|r| r.label == "work").unwrap();
	assert_eq!(
		function.change,
		StructuralChange::Unchanged,
		"syntax tokens are unchanged"
	);
	assert!(function.contains_changes, "raw formatting changes must not become a safe-to-mutate hidden row");
}

#[test]
fn comments_and_literals_belong_to_their_actual_declaration() {
	let old = "mod outer {\n // before\n fn left() { let text = \"a b\"; }\n fn right() { let x = 2; }\n}\n";
	let new = "mod outer {\n // after\n fn left() { let text = \"ab\"; }\n fn right() { let x = 2; }\n}\n";
	let result = analyze("sample.rs", old, new);
	let outer =
		result.regions.iter().find(|r| r.label == "outer").unwrap();
	let left =
		result.regions.iter().find(|r| r.label == "left").unwrap();
	let right =
		result.regions.iter().find(|r| r.label == "right").unwrap();
	assert_eq!(outer.old_lines, Some(0..5));
	assert_eq!(left.old_lines, Some(2..3));
	assert_eq!(right.new_lines, Some(3..4));
	assert_eq!(left.parent_id, Some(outer.id));
	assert_eq!(right.parent_id, Some(outer.id));
	assert!(outer.contains_changes && left.contains_changes);
	assert!(!right.contains_changes);
}

#[test]
fn duplicate_method_names_in_distinct_scopes_never_share_identity() {
	let old = "struct A;\nstruct B;\nimpl A {\n fn run() { 1; }\n}\nimpl B {\n fn run() { 2; }\n}\n";
	let new = old.replace("2;", "3;");
	let result = analyze("sample.rs", old, &new);
	let methods = result
		.regions
		.iter()
		.filter(|r| r.label == "run")
		.collect::<Vec<_>>();
	assert_eq!(methods.len(), 2);
	assert_ne!(methods[0].id, methods[1].id);
	assert_ne!(methods[0].parent_id, methods[1].parent_id);
	assert!(!methods[0].contains_changes);
	assert!(methods[1].contains_changes);
	assert_eq!(result, analyze("sample.rs", old, &new));
	assert_eq!(
		result
			.regions
			.iter()
			.map(|r| r.id)
			.collect::<HashSet<_>>()
			.len(),
		result.regions.len()
	);
}

#[test]
fn moved_declarations_keep_exact_independent_old_and_new_line_ranges()
{
	let old = "fn first() {\n 1;\n}\n\nfn second() {\n 2;\n}\n";
	let new = "fn second() {\n 2;\n}\n\nfn first() {\n 3;\n}\n";
	let result = analyze("sample.rs", old, new);
	let first =
		result.regions.iter().find(|r| r.label == "first").unwrap();
	let second =
		result.regions.iter().find(|r| r.label == "second").unwrap();
	assert_eq!(first.old_lines, Some(0..3));
	assert_eq!(first.new_lines, Some(4..7));
	assert_eq!(second.old_lines, Some(4..7));
	assert_eq!(second.new_lines, Some(0..3));
	assert!(first.contains_changes);
	assert!(!second.contains_changes);
}

#[test]
fn added_and_removed_regions_do_not_invent_a_counterpart() {
	let result = analyze(
		"sample.py",
		"def removed():\n    return 1\n",
		"def added():\n    return 2\n",
	);
	let removed = result
		.regions
		.iter()
		.find(|r| r.label == "removed")
		.unwrap();
	let added =
		result.regions.iter().find(|r| r.label == "added").unwrap();
	assert_eq!(removed.change, StructuralChange::Removed);
	assert_eq!(removed.old_lines, Some(0..2));
	assert_eq!(removed.new_lines, None);
	assert_eq!(added.change, StructuralChange::Added);
	assert_eq!(added.old_lines, None);
	assert_eq!(added.new_lines, Some(0..2));
}

#[test]
fn unicode_crlf_and_last_line_without_newline_preserve_line_bounds() {
	let result = analyze(
		"sample.ts",
		"function café() {\r\n return \"😀\";\r\n}",
		"function café() {\r\n return \"👨‍👩‍👧‍👦\";\r\n}",
	);
	let function =
		result.regions.iter().find(|r| r.label == "café").unwrap();
	assert_eq!(function.old_lines, Some(0..3));
	assert_eq!(function.new_lines, Some(0..3));
	assert!(function.contains_changes);
}

#[test]
fn invalid_binary_unsupported_and_pre_cancelled_inputs_use_text_fallback(
) {
	let cancel = AtomicBool::new(false);
	for (path, old, new) in [
		("broken.rs", b"fn {".as_slice(), b"fn {}".as_slice()),
		("binary.rs", b"\xff".as_slice(), b"".as_slice()),
		("plain.txt", b"hello".as_slice(), b"world".as_slice()),
	] {
		assert!(analyze_regions(path, old, new, &cancel).is_none());
	}
	assert!(analyze_regions(
		"a.rs",
		b"fn work(){}",
		b"fn work(){}",
		&AtomicBool::new(true)
	)
	.is_none());
}

#[test]
fn deeply_nested_or_oversized_inputs_cannot_escape_analysis_budget() {
	let huge = vec![b' '; 1024 * 1024 + 1];
	assert!(analyze_regions(
		"a.rs",
		&huge,
		&huge,
		&AtomicBool::new(false)
	)
	.is_none());
	let nested = format!(
		"fn work() {{ {} 1; {} }}",
		"{".repeat(12_000),
		"}".repeat(12_000)
	);
	let start = Instant::now();
	let _ = analyze_regions(
		"a.rs",
		nested.as_bytes(),
		nested.as_bytes(),
		&AtomicBool::new(false),
	);
	assert!(
		start.elapsed() < Duration::from_secs(2),
		"analysis must respect a bounded background budget"
	);
}

#[test]
fn actual_test_declarations_are_distinguished_from_incidental_names()
{
	let rust = "#[test]\nfn can_stage() {\n assert!(true);\n}\nfn contest() {\n 1;\n}\n";
	let result =
		analyze("source.rs", rust, &rust.replace("true", "false"));
	assert!(
		result
			.regions
			.iter()
			.find(|r| r.label == "can_stage")
			.unwrap()
			.is_test
	);
	assert!(
		!result
			.regions
			.iter()
			.find(|r| r.label == "contest")
			.unwrap()
			.is_test
	);
	let zig = "test \"bounds are checked\" {\n const value: u8 = 1;\n _ = value;\n}\n";
	let result =
		analyze("source.zig", zig, &zig.replace("= 1", "= 2"));
	let actual_test = result
		.regions
		.iter()
		.find(|r| r.is_test)
		.expect("Zig test AST declaration must be recognized");
	assert!(actual_test.label.contains("bounds are checked"));
	assert_eq!(actual_test.new_lines, Some(0..4));
}
