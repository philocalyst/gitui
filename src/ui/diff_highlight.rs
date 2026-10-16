//! Cached inline and syntax highlighting for actionable patch rows.

use asyncgit::DiffLineType;
use once_cell::sync::Lazy;
use ratatui::style::{Color, Modifier, Style};
use std::{ops::Range, path::Path};
use syntect::{
	easy::HighlightLines,
	highlighting::{Style as SyntectStyle, ThemeSet},
	parsing::SyntaxSet,
};

pub const MAX_PAIR_LINE_BYTES: usize = 4 * 1024;

static SYNTAXES: Lazy<SyntaxSet> =
	Lazy::new(two_face::syntax::extra_no_newlines);
static SYNTAX_THEMES: Lazy<ThemeSet> =
	Lazy::new(ThemeSet::load_defaults);

#[derive(Clone, Debug)]
pub struct DiffHighlightInput {
	pub hunk: usize,
	pub line_type: DiffLineType,
	pub content: String,
	/// Original display-safe byte length. Long lines omit `content` to avoid
	/// copying them into a worker; their changed span still covers the line.
	pub content_len: usize,
}

#[derive(Clone, Debug, Default)]
pub struct StyledRange {
	pub range: Range<usize>,
	pub style: Style,
}

#[derive(Clone, Debug, Default)]
pub struct DiffRowHighlight {
	/// Ranges in the display-safe line content that changed against its pair.
	pub changed: Vec<Range<usize>>,
	/// Syntax foreground/modifiers in the same display-safe byte coordinates.
	pub syntax: Vec<StyledRange>,
}

/// Computes line-pair character emphasis and syntax styles in patch order.
/// Each bounded viewport chunk starts fresh parser state within its hunk, so
/// syntax state is approximate when a chunk begins inside a multiline token.
/// Rows retain their original patch identity; this returns presentation data
/// only and must never be used to form Git mutation coordinates.
pub fn highlight_diff_rows(
	path: &Path,
	rows: &[DiffHighlightInput],
	theme_name: &str,
) -> Vec<DiffRowHighlight> {
	let mut result = vec![DiffRowHighlight::default(); rows.len()];
	mark_changed_ranges(rows, &mut result);
	mark_syntax_ranges(path, rows, theme_name, &mut result);
	result
}

pub fn language_label(path: &Path) -> String {
	let basename = path
		.file_name()
		.and_then(|name| name.to_str())
		.unwrap_or_default();
	if basename.eq_ignore_ascii_case("dockerfile") {
		return "Dockerfile".to_owned();
	}
	let extension = path
		.extension()
		.and_then(|extension| extension.to_str())
		.unwrap_or_default();
	match extension.to_ascii_lowercase().as_str() {
		"rs" => "Rust",
		"ts" | "tsx" => "TypeScript",
		"js" | "jsx" | "mjs" | "cjs" => "JavaScript",
		"py" | "pyw" => "Python",
		"toml" => "TOML",
		"json" => "JSON",
		"md" | "markdown" => "Markdown",
		"html" | "htm" => "HTML",
		"css" => "CSS",
		"yml" | "yaml" => "YAML",
		"sh" | "bash" | "zsh" => "Shell",
		"go" => "Go",
		"java" => "Java",
		"c" | "h" => "C",
		"cc" | "cpp" | "cxx" | "hpp" => "C++",
		"rb" => "Ruby",
		"php" => "PHP",
		"swift" => "Swift",
		"kt" | "kts" => "Kotlin",
		"lua" => "Lua",
		"sql" => "SQL",
		_ => "Text",
	}
	.to_owned()
}

pub fn syntax_language_label(path: &Path) -> String {
	syntax_for_path(path).name.clone()
}

fn syntax_for_path(
	path: &Path,
) -> &'static syntect::parsing::SyntaxReference {
	let basename = path
		.file_name()
		.and_then(|name| name.to_str())
		.unwrap_or_default();
	let syntax = if basename.eq_ignore_ascii_case("dockerfile") {
		SYNTAXES.find_syntax_by_name("Dockerfile")
	} else {
		path.extension()
			.and_then(|extension| extension.to_str())
			.and_then(|extension| {
				SYNTAXES.find_syntax_by_extension(extension)
			})
	};
	syntax.unwrap_or_else(|| SYNTAXES.find_syntax_plain_text())
}

fn mark_changed_ranges(
	rows: &[DiffHighlightInput],
	result: &mut [DiffRowHighlight],
) {
	let mut cursor = 0;
	while cursor < rows.len() {
		if rows[cursor].line_type != DiffLineType::Delete {
			if rows[cursor].line_type == DiffLineType::Add {
				result[cursor].changed =
					full_range(rows[cursor].content_len);
			}
			cursor += 1;
			continue;
		}
		let hunk = rows[cursor].hunk;
		let delete_start = cursor;
		while cursor < rows.len()
			&& rows[cursor].hunk == hunk
			&& rows[cursor].line_type == DiffLineType::Delete
		{
			cursor += 1;
		}
		let delete_end = cursor;
		let add_start = cursor;
		while cursor < rows.len()
			&& rows[cursor].hunk == hunk
			&& rows[cursor].line_type == DiffLineType::Add
		{
			cursor += 1;
		}
		let add_end = cursor;
		if add_start == add_end {
			for index in delete_start..delete_end {
				result[index].changed =
					full_range(rows[index].content_len);
			}
			continue;
		}
		pair_change_block(
			rows,
			result,
			delete_start..delete_end,
			add_start..add_end,
		);
	}
}

fn pair_change_block(
	rows: &[DiffHighlightInput],
	result: &mut [DiffRowHighlight],
	deletes: Range<usize>,
	adds: Range<usize>,
) {
	let delete_count = deletes.len();
	let add_count = adds.len();
	if delete_count.saturating_add(add_count) > 32
		|| deletes.clone().chain(adds.clone()).any(|index| {
			rows[index].content_len > MAX_PAIR_LINE_BYTES
		}) {
		mark_full_ranges(rows, result, deletes.chain(adds));
		return;
	}
	let total_bytes = deletes
		.clone()
		.chain(adds.clone())
		.fold(0_usize, |total, index| {
			total.saturating_add(rows[index].content_len)
		});
	if total_bytes > MAX_PAIR_LINE_BYTES {
		mark_full_ranges(rows, result, deletes.chain(adds));
		return;
	}
	let old_lines = deletes
		.clone()
		.map(|index| rows[index].content.as_str())
		.collect::<Vec<_>>();
	let new_lines = adds
		.clone()
		.map(|index| rows[index].content.as_str())
		.collect::<Vec<_>>();
	let pairs = asyncgit::diff_preview::align_changed_lines(
		&old_lines, &new_lines,
	);
	if pairs.is_empty() {
		mark_full_ranges(rows, result, deletes.chain(adds));
		return;
	}
	let mut paired_deletes = vec![false; delete_count];
	let mut paired_adds = vec![false; add_count];
	for (delete, add) in pairs {
		let old_index = deletes.start + delete;
		let new_index = adds.start + add;
		let Some(segments) =
			asyncgit::diff_preview::inline_diff_segments(
				&rows[old_index].content,
				&rows[new_index].content,
			)
		else {
			continue;
		};
		for segment in segments {
			match segment.kind {
				asyncgit::diff_preview::InlineSegmentKind::Shared => {}
				asyncgit::diff_preview::InlineSegmentKind::Removed => {
					result[old_index].changed.push(segment.old_range);
				}
				asyncgit::diff_preview::InlineSegmentKind::Added => {
					result[new_index].changed.push(segment.new_range);
				}
			}
		}
		paired_deletes[delete] = true;
		paired_adds[add] = true;
	}
	for (offset, &paired) in paired_deletes.iter().enumerate() {
		if !paired {
			let index = deletes.start + offset;
			result[index].changed =
				full_range(rows[index].content_len);
		}
	}
	for (offset, &paired) in paired_adds.iter().enumerate() {
		if !paired {
			let index = adds.start + offset;
			result[index].changed =
				full_range(rows[index].content_len);
		}
	}
}

#[allow(clippy::single_range_in_vec_init)] // This is one byte span, not a range of integers.
fn full_range(length: usize) -> Vec<Range<usize>> {
	if length == 0 {
		Vec::new()
	} else {
		Vec::from([0..length])
	}
}

fn mark_full_ranges(
	rows: &[DiffHighlightInput],
	result: &mut [DiffRowHighlight],
	indices: impl Iterator<Item = usize>,
) {
	for index in indices {
		result[index].changed = full_range(rows[index].content_len);
	}
}

fn mark_syntax_ranges(
	path: &Path,
	rows: &[DiffHighlightInput],
	theme_name: &str,
	result: &mut [DiffRowHighlight],
) {
	let syntax = syntax_for_path(path);
	let theme = SYNTAX_THEMES
		.themes
		.get(theme_name)
		.or_else(|| SYNTAX_THEMES.themes.get("base16-eighties.dark"));
	let Some(theme) = theme else { return };
	let (mut old_parser, mut new_parser) = (
		HighlightLines::new(syntax, theme),
		HighlightLines::new(syntax, theme),
	);
	let mut active_hunk = None;

	for (index, row) in rows.iter().enumerate() {
		if active_hunk != Some(row.hunk) {
			active_hunk = Some(row.hunk);
			old_parser = HighlightLines::new(syntax, theme);
			new_parser = HighlightLines::new(syntax, theme);
		}
		if row.line_type == DiffLineType::Header {
			old_parser = HighlightLines::new(syntax, theme);
			new_parser = HighlightLines::new(syntax, theme);
			continue;
		}
		match row.line_type {
			DiffLineType::Delete => {
				if row.content_len <= MAX_PAIR_LINE_BYTES {
					result[index].syntax =
						syntax_ranges(&mut old_parser, &row.content);
				}
			}
			DiffLineType::Add => {
				if row.content_len <= MAX_PAIR_LINE_BYTES {
					result[index].syntax =
						syntax_ranges(&mut new_parser, &row.content);
				}
			}
			DiffLineType::None => {
				if row.content_len <= MAX_PAIR_LINE_BYTES {
					let _ =
						syntax_ranges(&mut old_parser, &row.content);
					result[index].syntax =
						syntax_ranges(&mut new_parser, &row.content);
				}
			}
			DiffLineType::Header => {}
		}
	}
}

fn syntax_ranges(
	parser: &mut HighlightLines<'_>,
	line: &str,
) -> Vec<StyledRange> {
	let Ok(spans) = parser.highlight_line(line, &SYNTAXES) else {
		return Vec::new();
	};
	let mut offset = 0;
	spans
		.into_iter()
		.filter_map(|(syntect_style, text)| {
			let start = offset;
			offset += text.len();
			(start < offset).then(|| StyledRange {
				range: start..offset,
				style: syntect_style_to_tui(syntect_style),
			})
		})
		.collect()
}

fn syntect_style_to_tui(style: SyntectStyle) -> Style {
	let mut result = Style::default().fg(Color::Rgb(
		style.foreground.r,
		style.foreground.g,
		style.foreground.b,
	));
	if style
		.font_style
		.contains(syntect::highlighting::FontStyle::BOLD)
	{
		result = result.add_modifier(Modifier::BOLD);
	}
	if style
		.font_style
		.contains(syntect::highlighting::FontStyle::ITALIC)
	{
		result = result.add_modifier(Modifier::ITALIC);
	}
	if style
		.font_style
		.contains(syntect::highlighting::FontStyle::UNDERLINE)
	{
		result = result.add_modifier(Modifier::UNDERLINED);
	}
	result
}

#[cfg(test)]
mod tests {
	use super::{
		highlight_diff_rows, DiffHighlightInput, MAX_PAIR_LINE_BYTES,
	};
	use asyncgit::DiffLineType;
	use std::path::Path;

	fn input(
		line_type: DiffLineType,
		content: &str,
	) -> DiffHighlightInput {
		DiffHighlightInput {
			hunk: 0,
			line_type,
			content: content.to_owned(),
			content_len: content.len(),
		}
	}

	fn length_only(
		line_type: DiffLineType,
		content_len: usize,
	) -> DiffHighlightInput {
		DiffHighlightInput {
			hunk: 0,
			line_type,
			content: String::new(),
			content_len,
		}
	}

	#[test]
	fn pairs_small_line_edits_and_keeps_context_unchanged() {
		let rows = [
			input(DiffLineType::None, "fn count() {"),
			input(DiffLineType::Delete, "\tlet count = 1;"),
			input(DiffLineType::Add, "\tlet count = 2;"),
		];
		let highlights = highlight_diff_rows(
			Path::new("count.rs"),
			&rows,
			"base16-eighties.dark",
		);

		assert!(highlights[0].changed.is_empty());
		assert_eq!(
			&rows[1].content[highlights[1].changed[0].clone()],
			"1"
		);
		assert_eq!(
			&rows[2].content[highlights[2].changed[0].clone()],
			"2"
		);
		assert!(!highlights[1].syntax.is_empty());
		assert!(!highlights[2].syntax.is_empty());
	}

	#[test]
	fn unrelated_lines_are_marked_whole_and_oversized_lines_skip_syntax(
	) {
		let oversized = "x".repeat(MAX_PAIR_LINE_BYTES + 1);
		let rows = [
			input(DiffLineType::Delete, "old_value();"),
			input(DiffLineType::Add, "unrelated();"),
			input(DiffLineType::Add, &oversized),
		];
		let highlights = highlight_diff_rows(
			Path::new("example.rs"),
			&rows,
			"base16-eighties.dark",
		);

		for index in 0..2 {
			assert_eq!(
				highlights[index].changed,
				vec![0..rows[index].content.len()]
			);
		}
		assert_eq!(
			highlights[2].changed,
			vec![0..rows[2].content.len()]
		);
		assert!(highlights[2].syntax.is_empty());
	}

	#[test]
	fn length_only_long_line_gets_a_full_changed_span_without_a_copy()
	{
		let rows = [
			length_only(
				DiffLineType::Delete,
				MAX_PAIR_LINE_BYTES + 10,
			),
			length_only(DiffLineType::Add, MAX_PAIR_LINE_BYTES + 12),
		];
		let highlights = highlight_diff_rows(
			Path::new("large.rs"),
			&rows,
			"base16-eighties.dark",
		);

		assert_eq!(
			highlights[0].changed,
			vec![0..MAX_PAIR_LINE_BYTES + 10]
		);
		assert_eq!(
			highlights[1].changed,
			vec![0..MAX_PAIR_LINE_BYTES + 12]
		);
		assert!(highlights[0].syntax.is_empty());
		assert!(highlights[1].syntax.is_empty());
	}

	#[test]
	fn shared_inline_ranges_keep_actual_syntect_foreground_colors() {
		let old = "let one = 1; let two = 2;";
		let new = "let one = 3; let two = 2;";
		let rows = [
			input(DiffLineType::Delete, old),
			input(DiffLineType::Add, new),
		];
		let highlights = highlight_diff_rows(
			Path::new("sample.rs"),
			&rows,
			"base16-eighties.dark",
		);
		let changed_old = old.find('1').unwrap();
		let changed_new = new.find('3').unwrap();
		let shared_literal = new.rfind('2').unwrap();
		let changed_old_fg =
			foreground_at(&highlights[0], changed_old);
		let changed_new_fg =
			foreground_at(&highlights[1], changed_new);
		let shared_fg = foreground_at(&highlights[1], shared_literal);
		assert!(changed_old_fg.is_some());
		assert_eq!(changed_old_fg, changed_new_fg);
		assert_eq!(changed_new_fg, shared_fg);
	}

	#[test]
	fn changed_keywords_keep_their_syntax_foreground_on_both_sides() {
		let old = "let value = 1;";
		let new = "const value = 1;";
		let rows = [
			input(DiffLineType::Delete, old),
			input(DiffLineType::Add, new),
		];
		let highlights = highlight_diff_rows(
			Path::new("sample.rs"),
			&rows,
			"base16-eighties.dark",
		);
		assert!(!highlights[0].changed.is_empty());
		assert!(!highlights[1].changed.is_empty());
		let old_fg =
			foreground_at(&highlights[0], old.find("let").unwrap());
		let new_fg =
			foreground_at(&highlights[1], new.find("const").unwrap());
		assert!(old_fg.is_some());
		assert_eq!(old_fg, new_fg);
	}

	fn foreground_at(
		highlight: &super::DiffRowHighlight,
		byte: usize,
	) -> Option<ratatui::style::Color> {
		highlight
			.syntax
			.iter()
			.find(|range| range.range.contains(&byte))
			.and_then(|range| range.style.fg)
	}
}
