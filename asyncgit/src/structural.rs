//! Bounded, in-process syntax-tree regions for native diff folding.
//!
//! This is deliberately an adapter over a small set of Tree-sitter grammars,
//! not a general-purpose structural diff engine. It returns no analysis when
//! either complete source snapshot cannot be parsed within the byte/time
//! budgets. Callers should then keep their ordinary text diff visible.

use std::{
	collections::{BTreeMap, HashMap},
	hash::{Hash, Hasher},
	ops::Range,
	sync::atomic::{AtomicBool, Ordering},
	time::{Duration, Instant},
};

const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_PARSE_TIME: Duration = Duration::from_millis(100);
const MAX_STRUCTURAL_REGIONS: usize = 16_384;
const MAX_TEST_ATTRIBUTE_SCAN: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructuralChange {
	Added,
	Removed,
	Modified,
	Unchanged,
}

/// A syntax-backed region. Line ranges are zero-based and half-open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructuralRegion {
	/// Stable for a source-pair path and sibling occurrence.
	pub id: u64,
	pub parent_id: Option<u64>,
	pub kind: &'static str,
	/// A label copied from the declaration in the source.
	pub label: String,
	pub old_lines: Option<Range<u32>>,
	pub new_lines: Option<Range<u32>>,
	pub change: StructuralChange,
	/// True only when syntax or an explicit test naming convention identifies a test.
	pub is_test: bool,
	pub foldable: bool,
	pub contains_changes: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructuralAnalysis {
	pub language: &'static str,
	pub regions: Vec<StructuralRegion>,
}

#[derive(Clone)]
struct LanguageSpec {
	name: &'static str,
	grammar: tree_sitter::Language,
}

fn language_for_path(path: &str) -> Option<LanguageSpec> {
	let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
	let (name, grammar) = match ext.as_str() {
		"rs" => ("Rust", into_language(tree_sitter_rust::LANGUAGE)),
		"zig" => ("Zig", into_language(tree_sitter_zig::LANGUAGE)),
		"ts" => (
			"TypeScript",
			into_language(
				tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
			),
		),
		"tsx" | "jsx" => (
			"TypeScript/JSX",
			into_language(tree_sitter_typescript::LANGUAGE_TSX),
		),
		"js" | "mjs" | "cjs" => (
			"JavaScript",
			into_language(tree_sitter_javascript::LANGUAGE),
		),
		"py" => {
			("Python", into_language(tree_sitter_python::LANGUAGE))
		}
		"json" => ("JSON", into_language(tree_sitter_json::LANGUAGE)),
		_ => return None,
	};
	Some(LanguageSpec { name, grammar })
}

fn into_language(
	grammar: tree_sitter_language::LanguageFn,
) -> tree_sitter::Language {
	grammar.into()
}

/// Analyze complete old/new file contents. Returns `None` for unsupported
/// files, invalid UTF-8, malformed syntax, cancellation, or exhausted budget.
///
/// Run this on the diff-preview background job, never on the UI thread.
pub fn analyze_regions(
	path: &str,
	old_source: &[u8],
	new_source: &[u8],
	cancel: &AtomicBool,
) -> Option<StructuralAnalysis> {
	if old_source.len() > MAX_SOURCE_BYTES
		|| new_source.len() > MAX_SOURCE_BYTES
		|| std::str::from_utf8(old_source).is_err()
		|| std::str::from_utf8(new_source).is_err()
		|| cancel.load(Ordering::Acquire)
	{
		return None;
	}
	let spec = language_for_path(path)?;
	let deadline = Instant::now() + MAX_PARSE_TIME;
	let old_tree =
		parse(&spec.grammar, old_source, cancel, deadline)?;
	let new_tree =
		parse(&spec.grammar, new_source, cancel, deadline)?;
	if old_tree.root_node().has_error()
		|| new_tree.root_node().has_error()
	{
		return None;
	}
	let old = collect_regions(
		old_tree.root_node(),
		old_source,
		spec.name,
		cancel,
		deadline,
	)?;
	let new = collect_regions(
		new_tree.root_node(),
		new_source,
		spec.name,
		cancel,
		deadline,
	)?;
	compare_regions(
		spec.name, old_source, new_source, &old, &new, cancel,
		deadline,
	)
}

fn compare_regions(
	language: &'static str,
	old_source: &[u8],
	new_source: &[u8],
	old: &BTreeMap<String, RegionData>,
	new: &BTreeMap<String, RegionData>,
	cancel: &AtomicBool,
	deadline: Instant,
) -> Option<StructuralAnalysis> {
	if old.len().saturating_add(new.len()) > MAX_STRUCTURAL_REGIONS
		|| cancel.load(Ordering::Acquire)
		|| Instant::now() >= deadline
	{
		return None;
	}
	let mut keys =
		old.keys().chain(new.keys()).cloned().collect::<Vec<_>>();
	keys.sort();
	keys.dedup();
	let mut changed_keys = std::collections::HashSet::new();
	let mut regions = Vec::with_capacity(keys.len());
	for key in &keys {
		if cancel.load(Ordering::Acquire)
			|| Instant::now() >= deadline
		{
			return None;
		}
		let old_region = old.get(key);
		let new_region = new.get(key);
		let source_changed = match (old_region, new_region) {
			(Some(before), Some(after)) => {
				old_source.get(before.byte_range.clone())
					!= new_source.get(after.byte_range.clone())
			}
			_ => true,
		};
		let change = match (old_region, new_region) {
			(None, Some(_)) => StructuralChange::Added,
			(Some(_), None) => StructuralChange::Removed,
			(Some(before), Some(after))
				if before.fingerprint != after.fingerprint =>
			{
				StructuralChange::Modified
			}
			(Some(_), Some(_)) => StructuralChange::Unchanged,
			(None, None) => continue,
		};
		if source_changed || change != StructuralChange::Unchanged {
			changed_keys.insert(key.clone());
		}
		let template = new_region.or(old_region)?;
		let id = stable_id(key);
		let parent_id = template.parent_key.as_deref().map(stable_id);
		let old_lines = old_region.map(|region| region.lines.clone());
		let new_lines = new_region.map(|region| region.lines.clone());
		let span = new_lines.as_ref().or(old_lines.as_ref());
		let foldable = span.is_some_and(|lines| {
			lines.end.saturating_sub(lines.start) > 1
		});
		regions.push(StructuralRegion {
			id,
			parent_id,
			kind: template.kind,
			label: template.label.clone(),
			old_lines,
			new_lines,
			change,
			is_test: template.is_test,
			foldable,
			contains_changes: source_changed
				|| change != StructuralChange::Unchanged,
		});
	}
	mark_changed_ancestors(
		&mut regions,
		changed_keys,
		old,
		new,
		cancel,
		deadline,
	)?;
	regions.sort_by_key(|region| {
		region
			.new_lines
			.as_ref()
			.or(region.old_lines.as_ref())
			.map_or(u32::MAX, |lines| lines.start)
	});
	if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
		return None;
	}
	Some(StructuralAnalysis { language, regions })
}

fn mark_changed_ancestors(
	regions: &mut [StructuralRegion],
	changed_keys: std::collections::HashSet<String>,
	old: &BTreeMap<String, RegionData>,
	new: &BTreeMap<String, RegionData>,
	cancel: &AtomicBool,
	deadline: Instant,
) -> Option<()> {
	let parent_by_key = new
		.iter()
		.chain(old.iter())
		.map(|(key, region)| (key.clone(), region.parent_key.clone()))
		.collect::<HashMap<_, _>>();
	let index_by_id = regions
		.iter()
		.enumerate()
		.map(|(index, region)| (region.id, index))
		.collect::<HashMap<_, _>>();
	for key in changed_keys {
		let mut parent = parent_by_key.get(&key).cloned().flatten();
		let mut steps = 0;
		while let Some(parent_key) = parent {
			if cancel.load(Ordering::Acquire)
				|| Instant::now() >= deadline
			{
				return None;
			}
			if let Some(index) =
				index_by_id.get(&stable_id(&parent_key))
			{
				if let Some(region) = regions.get_mut(*index) {
					region.contains_changes = true;
				}
			}
			parent =
				parent_by_key.get(&parent_key).cloned().flatten();
			steps += 1;
			if steps > parent_by_key.len() {
				return None;
			}
		}
	}
	Some(())
}

fn parse(
	grammar: &tree_sitter::Language,
	source: &[u8],
	cancel: &AtomicBool,
	deadline: Instant,
) -> Option<tree_sitter::Tree> {
	let mut parser = tree_sitter::Parser::new();
	parser.set_language(grammar).ok()?;
	let mut was_cancelled = false;
	let mut progress = |_state: &tree_sitter::ParseState| {
		was_cancelled = cancel.load(Ordering::Acquire)
			|| Instant::now() >= deadline;
		was_cancelled
	};
	let text = std::str::from_utf8(source).ok()?;
	let tree = parser.parse_with_options(
		&mut |offset, _| &text.as_bytes()[offset..],
		None,
		Some(
			tree_sitter::ParseOptions::new()
				.progress_callback(&mut progress),
		),
	)?;
	if was_cancelled
		|| cancel.load(Ordering::Acquire)
		|| Instant::now() >= deadline
	{
		None
	} else {
		Some(tree)
	}
}

#[derive(Clone)]
struct RegionData {
	kind: &'static str,
	label: String,
	is_test: bool,
	lines: Range<u32>,
	fingerprint: u64,
	byte_range: Range<usize>,
	parent_key: Option<String>,
}

fn collect_regions(
	root: tree_sitter::Node<'_>,
	source: &[u8],
	language: &'static str,
	cancel: &AtomicBool,
	deadline: Instant,
) -> Option<BTreeMap<String, RegionData>> {
	let mut output = BTreeMap::new();
	let mut occurrence =
		HashMap::<(String, &'static str, String), usize>::new();
	let mut stack = vec![(root, String::new(), None::<String>)];
	while let Some((node, parent_path, parent_key)) = stack.pop() {
		if cancel.load(Ordering::Acquire)
			|| Instant::now() >= deadline
		{
			return None;
		}
		let region_kind = region_kind(node.kind());
		let mut next_parent_path = parent_path;
		let mut next_parent_key = parent_key;
		if let Some(kind) = region_kind {
			let label = declaration_label(node, source, kind);
			let counter_key =
				(next_parent_path.clone(), kind, label.clone());
			let index = occurrence.entry(counter_key).or_default();
			let occurrence_number = *index;
			*index += 1;
			let key = format!("{next_parent_path}/{kind}:{label}#{occurrence_number}");
			let lines = node_lines(node)?;
			let is_test = is_test_region(
				node, source, language, kind, &label, cancel,
				deadline,
			)?;
			let fingerprint =
				fingerprint(node, source, cancel, deadline)?;
			let byte_range = node.byte_range();
			source.get(byte_range.clone())?;
			output.insert(
				key.clone(),
				RegionData {
					kind,
					label: label.clone(),
					is_test,
					lines,
					fingerprint,
					byte_range,
					parent_key: next_parent_key.clone(),
				},
			);
			if output.len() > MAX_STRUCTURAL_REGIONS {
				return None;
			}
			next_parent_path.clone_from(&key);
			next_parent_key = Some(key);
		}
		for child_index in (0..node.child_count()).rev() {
			if cancel.load(Ordering::Acquire)
				|| Instant::now() >= deadline
			{
				return None;
			}
			if let Some(child) = node.child(child_index) {
				stack.push((
					child,
					next_parent_path.clone(),
					next_parent_key.clone(),
				));
			}
		}
	}
	Some(output)
}

fn is_test_region(
	node: tree_sitter::Node<'_>,
	source: &[u8],
	language: &str,
	region_kind: &str,
	label: &str,
	cancel: &AtomicBool,
	deadline: Instant,
) -> Option<bool> {
	if region_kind == "test" {
		return Some(true);
	}
	match language {
		"Rust" if region_kind == "function" => {
			has_associated_test_attribute(
				node, source, cancel, deadline,
			)
		}
		"Python" if region_kind == "function" => {
			Some(label.starts_with("test_"))
		}
		_ => Some(false),
	}
}

fn has_associated_test_attribute(
	node: tree_sitter::Node<'_>,
	source: &[u8],
	cancel: &AtomicBool,
	deadline: Instant,
) -> Option<bool> {
	let mut sibling = node.prev_sibling();
	let mut scanned = 0;
	while let Some(previous) = sibling {
		if cancel.load(Ordering::Acquire)
			|| Instant::now() >= deadline
		{
			return None;
		}
		if scanned >= MAX_TEST_ATTRIBUTE_SCAN {
			return None;
		}
		scanned += 1;
		if previous.kind() == "attribute_item" {
			if source
				.get(previous.byte_range())
				.and_then(|bytes| std::str::from_utf8(bytes).ok())
				.is_some_and(is_test_attribute)
			{
				return Some(true);
			}
		} else if !previous.is_extra() {
			break;
		}
		sibling = previous.prev_sibling();
	}
	Some(false)
}

fn is_test_attribute(attribute: &str) -> bool {
	let Some(attribute) = attribute
		.trim()
		.strip_prefix("#[")
		.and_then(|attribute| attribute.strip_suffix(']'))
	else {
		return false;
	};
	let path = attribute.split('(').next().unwrap_or_default().trim();
	path.rsplit([':', '.']).next() == Some("test")
}

fn node_lines(node: tree_sitter::Node<'_>) -> Option<Range<u32>> {
	let start = u32::try_from(node.start_position().row).ok()?;
	let end_point = node.end_position();
	let mut end = u32::try_from(end_point.row).ok()?;
	if end_point.column > 0 {
		end = end.checked_add(1)?;
	}
	Some(start..end.max(start))
}

fn fingerprint(
	root: tree_sitter::Node<'_>,
	source: &[u8],
	cancel: &AtomicBool,
	deadline: Instant,
) -> Option<u64> {
	let mut hasher = std::collections::hash_map::DefaultHasher::new();
	let mut stack = vec![root];
	while let Some(node) = stack.pop() {
		if cancel.load(Ordering::Acquire)
			|| Instant::now() >= deadline
		{
			return None;
		}
		if node.is_extra() {
			hash_node_bytes(node, source, &mut hasher)?;
			continue;
		}
		if node.child_count() == 0 {
			hash_node_bytes(node, source, &mut hasher)?;
		} else {
			for child_index in (0..node.child_count()).rev() {
				if let Some(child) = node.child(child_index) {
					stack.push(child);
				}
			}
		}
	}
	Some(hasher.finish())
}

fn hash_node_bytes(
	node: tree_sitter::Node<'_>,
	source: &[u8],
	hasher: &mut impl Hasher,
) -> Option<()> {
	node.kind().hash(hasher);
	let bytes = source.get(node.byte_range())?;
	bytes.len().hash(hasher);
	bytes.hash(hasher);
	Some(())
}

fn stable_id(key: &str) -> u64 {
	key.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
		(hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
	})
}

fn declaration_label(
	node: tree_sitter::Node<'_>,
	source: &[u8],
	kind: &str,
) -> String {
	if kind == "json_pair" {
		if let Some(key) = node.child_by_field_name("key") {
			return node_text(key, source);
		}
	}
	if let Some(name) = node.child_by_field_name("name") {
		return node_text(name, source);
	}
	let text = node_text(node, source);
	let first_line = text.lines().next().unwrap_or_default().trim();
	let shortened = first_line.chars().take(100).collect::<String>();
	if shortened.is_empty() {
		kind.to_string()
	} else {
		shortened
	}
}

fn node_text(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
	String::from_utf8_lossy(
		source.get(node.byte_range()).unwrap_or_default(),
	)
	.into_owned()
}

fn region_kind(node_kind: &str) -> Option<&'static str> {
	Some(match node_kind {
		// Rust declarations.
		"function_item"
		| "function_signature_item"
		| "function_declaration"
		| "function_definition" => "function",
		"impl_item" => "impl",
		"mod_item" => "module",
		"struct_item" => "struct",
		"enum_item" | "enum_declaration" => "enum",
		"trait_item" => "trait",
		"type_item" | "type_alias_declaration" => "type",
		// Zig declarations.
		"container_declaration" => "container",
		"test_declaration" => "test",
		"variable_declaration" | "lexical_declaration" => "variable",
		// TypeScript and JavaScript declarations.
		"class_declaration" | "class_definition" => "class",
		"interface_declaration" => "interface",
		"method_definition" => "method",
		// JSON regions carry literal keys where available.
		"pair" => "json_pair",
		"object" => "json_object",
		"array" => "json_array",
		_ => return None,
	})
}

#[cfg(test)]
mod tests {
	use super::{analyze_regions, StructuralChange};
	use std::{ops::Range, sync::atomic::AtomicBool};

	fn analyze(
		path: &str,
		before: &str,
		after: &str,
	) -> Option<super::StructuralAnalysis> {
		analyze_regions(
			path,
			before.as_bytes(),
			after.as_bytes(),
			&AtomicBool::new(false),
		)
	}

	fn region<'a>(
		analysis: &'a super::StructuralAnalysis,
		kind: &str,
		label: &str,
	) -> &'a super::StructuralRegion {
		analysis
			.regions
			.iter()
			.find(|region| {
				region.kind == kind && region.label == label
			})
			.expect("requested structural region is present")
	}

	fn line_range(start: u32, end: u32) -> Range<u32> {
		start..end
	}

	#[test]
	fn rust_regions_use_zero_based_half_open_lines_and_include_comment_changes(
	) {
		let before = "mod outer {\n    // old comment\n    fn work() {\n        let x = 1;\n    }\n}\n";
		let after = "mod outer {\n    // new comment\n    fn work() {\n        let x = 1;\n    }\n}\n";
		let analysis = analyze("src/lib.rs", before, after)
			.expect("Rust parses");
		assert_eq!(analysis.language, "Rust");
		let module = region(&analysis, "module", "outer");
		let function = region(&analysis, "function", "work");
		assert_eq!(module.old_lines, Some(line_range(0, 6)));
		assert_eq!(module.new_lines, Some(line_range(0, 6)));
		assert_eq!(function.change, StructuralChange::Unchanged);
		assert!(!function.contains_changes);
		assert!(function.parent_id.is_some());
		assert!(module.contains_changes);
		assert_eq!(module.change, StructuralChange::Modified);
	}

	#[test]
	fn syntax_fingerprint_ignores_spacing_but_preserves_string_and_comment_tokens(
	) {
		let spacing = analyze(
			"src/lib.rs",
			"fn work(){let x=1;}\n",
			"fn work() { let x = 1; }\n",
		)
		.expect("Rust parses");
		assert_eq!(
			region(&spacing, "function", "work").change,
			StructuralChange::Unchanged
		);
		assert!(
			region(&spacing, "function", "work").contains_changes
		);

		let comment = analyze(
			"src/lib.rs",
			"fn work(){ // a\n  1 }\n",
			"fn work(){ // b\n  1 }\n",
		)
		.expect("Rust parses");
		assert_eq!(
			region(&comment, "function", "work").change,
			StructuralChange::Modified
		);

		let literal = analyze(
			"src/lib.rs",
			"fn work(){let x=\"a b\";}\n",
			"fn work(){let x=\"ab\";}\n",
		)
		.expect("Rust parses");
		assert_eq!(
			region(&literal, "function", "work").change,
			StructuralChange::Modified
		);
	}

	#[test]
	fn duplicate_sibling_declarations_have_distinct_ids() {
		let analysis = analyze(
			"src/lib.rs",
			"fn run() { 1; }\nfn run() { 2; }\n",
			"fn run() { 1; }\nfn run() { 3; }\n",
		)
		.expect("Rust parses");
		let runs = analysis
			.regions
			.iter()
			.filter(|region| {
				region.kind == "function" && region.label == "run"
			})
			.collect::<Vec<_>>();
		assert_eq!(runs.len(), 2);
		assert_ne!(runs[0].id, runs[1].id);
		assert_eq!(runs[0].change, StructuralChange::Unchanged);
		assert_eq!(runs[1].change, StructuralChange::Modified);
	}

	#[test]
	fn supported_language_adapters_return_actual_declaration_nodes() {
		for (path, source, language, kind, label) in [
			(
				"src/main.zig",
				"pub fn work() void { }\n",
				"Zig",
				"function",
				"work",
			),
			(
				"src/main.ts",
				"function work(): number { return 1; }\n",
				"TypeScript",
				"function",
				"work",
			),
			(
				"src/main.js",
				"function work() { return 1; }\n",
				"JavaScript",
				"function",
				"work",
			),
			(
				"src/main.py",
				"def work():\n    return 1\n",
				"Python",
				"function",
				"work",
			),
			(
				"src/data.json",
				"{\"work\": {\"value\": 1}}\n",
				"JSON",
				"json_pair",
				"\"work\"",
			),
		] {
			let analysis =
				analyze(path, source, source).expect("source parses");
			assert_eq!(analysis.language, language, "{path}");
			assert_eq!(
				region(&analysis, kind, label).change,
				StructuralChange::Unchanged,
				"{path}"
			);
			assert!(
				!region(&analysis, kind, label).is_test,
				"{path}"
			);
		}
	}

	#[test]
	fn tests_are_classified_by_real_rust_attrs_zig_nodes_and_python_prefix(
	) {
		let rust = analyze(
			"src/tests.rs",
			"#[test]\nfn can_stage() {}\n#[tokio::test]\nasync fn can_run() {}\nfn contest() {}\n",
			"#[test]\nfn can_stage() {}\n#[tokio::test]\nasync fn can_run() {}\nfn contest() {}\n",
		)
		.expect("Rust parses");
		assert!(region(&rust, "function", "can_stage").is_test);
		assert!(region(&rust, "function", "can_run").is_test);
		assert!(!region(&rust, "function", "contest").is_test);

		let zig = analyze(
			"src/tests.zig",
			"test \"commit graph\" {\n    try expect(true);\n}\n",
			"test \"commit graph\" {\n    try expect(true);\n}\n",
		)
		.expect("Zig parses");
		assert!(zig
			.regions
			.iter()
			.any(|region| region.kind == "test" && region.is_test));

		let python = analyze(
			"test_branch.py",
			"def test_branch():\n    assert True\n\ndef contest():\n    pass\n",
			"def test_branch():\n    assert True\n\ndef contest():\n    pass\n",
		)
		.expect("Python parses");
		assert!(region(&python, "function", "test_branch").is_test);
		assert!(!region(&python, "function", "contest").is_test);
	}

	#[test]
	fn malformed_unsupported_oversized_and_cancelled_inputs_fall_back(
	) {
		assert!(analyze("src/lib.rs", "fn {", "fn {").is_none());
		assert!(analyze(
			"src/data.txt",
			"fn work() {}",
			"fn work() {}"
		)
		.is_none());
		let oversized = vec![b' '; 1024 * 1024 + 1];
		assert!(analyze_regions(
			"src/lib.rs",
			&oversized,
			&oversized,
			&AtomicBool::new(false)
		)
		.is_none());
		assert!(analyze_regions(
			"src/lib.rs",
			b"fn a(){}",
			b"fn a(){}",
			&AtomicBool::new(true)
		)
		.is_none());
	}
}
