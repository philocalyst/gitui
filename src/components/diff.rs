use super::{
	utils::scroll_horizontal::HorizontalScroll,
	utils::scroll_vertical::VerticalScroll, CommandBlocking,
	Direction, DrawableComponent, HorizontalScrollType, ScrollType,
};
use crate::{
	app::Environment,
	components::{
		CommandInfo, Component, DiffPreviewController, EventState,
		SelectionStatus,
	},
	keys::{key_match, SharedKeyConfig},
	options::{DiffPreviewPreference, SharedOptions},
	queue::{Action, InternalEvent, NeedsUpdate, Queue, ResetItem},
	string_utils::{tabs_to_spaces, trim_offset, truncate_to_width},
	strings, try_or_popup,
	ui::{
		diff_highlight::{
			highlight_diff_rows, DiffHighlightInput,
			DiffRowHighlight as InlineHighlight,
		},
		style::SharedTheme,
	},
	AsyncAppNotification,
};
use anyhow::Result;
use asyncgit::{
	diff_preview::MergedChangeRow,
	hash,
	structural::{StructuralAnalysis, StructuralRegion},
	sync::{
		self, diff::DiffLinePosition, DiffLineReplacement,
		RepoPathRef,
	},
	DiffLine, DiffLineType, DiffParams, DiffType, FileDiff,
};
use bytesize::ByteSize;
use crossbeam_channel::{unbounded, Receiver};
use crossterm::event::{
	Event, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
	layout::Rect,
	text::{Line, Span},
	widgets::{Block, Borders, Paragraph},
	Frame,
};
use std::{
	borrow::Cow,
	cell::{Cell, RefCell},
	cmp,
	collections::{HashMap, HashSet},
	path::PathBuf,
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc,
	},
};
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
struct Current {
	path: String,
	is_stage: bool,
	hash: u64,
}

struct DiffSourceUpdate {
	path: String,
	is_stage: bool,
	diff: FileDiff,
	params: Option<DiffParams>,
	hash: u64,
	compact_projection: bool,
	source_changed: bool,
}

struct RenderedDiffRow {
	hunk_index: usize,
	line_index: usize,
	content: String,
}

struct DisplayDiffRow {
	hunk_index: usize,
	raw_rows: Arc<[usize]>,
	content: String,
	replacement: Vec<DisplayDiffSegment>,
	fold: Option<usize>,
	block: Option<(usize, usize)>,
	block_gutter: Option<(Option<u32>, Option<u32>)>,
	block_sources: Vec<(bool, std::ops::Range<usize>)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NativeBlockReplacement {
	metadata: asyncgit::diff_preview::InlineReplacementBlock,
	old_rows: Vec<usize>,
	new_rows: Vec<usize>,
}

struct NativeFoldGroup {
	id: u64,
	region: StructuralRegion,
	raw_rows: Vec<usize>,
	additions: usize,
	deletions: usize,
}

struct DisplayDiffSegment {
	range: std::ops::Range<usize>,
	kind: asyncgit::diff_preview::InlineSegmentKind,
	source_row: usize,
	source_range: std::ops::Range<usize>,
}

struct ProjectionPair {
	new_row: usize,
	segments: Vec<asyncgit::diff_preview::InlineSegment>,
}

struct ProjectionInput {
	raw_row: usize,
	hunk_index: usize,
	line_type: DiffLineType,
	content: Option<String>,
}

struct ProjectionChunk {
	core_start: usize,
	core_end: usize,
	requested_end: usize,
	leading_continues: bool,
	trailing_continues: bool,
	inputs: Vec<ProjectionInput>,
}

struct ProjectionResult {
	generation: u64,
	path: String,
	is_stage: bool,
	hash: u64,
	core_start: usize,
	core_end: usize,
	requested_end: usize,
	pairs: Vec<(usize, ProjectionPair)>,
}

struct DiffRowAnchor {
	fold_id: Option<u64>,
	block: Option<(usize, bool, usize)>,
	line_type: DiffLineType,
	position: DiffLinePosition,
	content: String,
}

struct DiffHighlightResult {
	generation: u64,
	window_generation: u64,
	start: usize,
	path: String,
	is_stage: bool,
	hash: u64,
	language: String,
	rows: Vec<InlineHighlight>,
}

struct HighlightChunk {
	input_start: usize,
	publish_start: usize,
	publish_end: usize,
	inputs: Vec<DiffHighlightInput>,
}

const MAX_HIGHLIGHT_ROWS: usize = 256;
const MAX_HIGHLIGHT_BYTES: usize = 128 * 1024;
const HIGHLIGHT_CONTEXT_ROWS: usize = 64;
const MAX_HIGHLIGHT_PUBLISH_ROWS: usize =
	MAX_HIGHLIGHT_ROWS - 2 * HIGHLIGHT_CONTEXT_ROWS;
const HIGHLIGHT_PREFIX_BYTES: usize = 32 * 1024;
const HIGHLIGHT_CORE_BYTES: usize = 64 * 1024;
const HIGHLIGHT_SUFFIX_BYTES: usize = MAX_HIGHLIGHT_BYTES
	- HIGHLIGHT_PREFIX_BYTES
	- HIGHLIGHT_CORE_BYTES;
const HIGHLIGHT_WINDOW_ROWS: usize = 512;
const HIGHLIGHT_WINDOW_STRIDE: usize = 128;
const PROJECTION_CONTEXT_ROWS: usize = 32;
const PROJECTION_CORE_ROWS: usize = 64;
const PROJECTION_MAX_BYTES: usize = 128 * 1024;
const PROJECTION_MAX_LINE_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy)]
struct DiffRowRenderConfig {
	number_width: usize,
	highlight: DiffRowHighlight,
	hunk_selected: bool,
	scrolled_right: usize,
}

///
#[derive(Clone, Copy)]
enum Selection {
	Single(usize),
	Multiple(usize, usize),
}

#[derive(Clone, Copy)]
enum DiffRowHighlight {
	Normal,
	Selected,
	Cursor,
	CursorSelected,
}

fn build_highlight_inputs(
	rows: &[RenderedDiffRow],
	diff: &FileDiff,
	start: usize,
	end: usize,
) -> HighlightChunk {
	let mut inputs = Vec::new();
	let prefix_start = start.saturating_sub(HIGHLIGHT_CONTEXT_ROWS);
	let mut prefix = Vec::new();
	let mut prefix_bytes = 0_usize;
	for index in (prefix_start..start).rev() {
		let input = highlight_input(&rows[index], diff);
		let bytes = copied_highlight_bytes(&input);
		if prefix_bytes.saturating_add(bytes) > HIGHLIGHT_PREFIX_BYTES
		{
			break;
		}
		prefix_bytes += bytes;
		prefix.push(input);
	}
	prefix.reverse();
	inputs.extend(prefix);
	let input_start =
		prefix_start.max(start.saturating_sub(inputs.len()));

	let publish_start = start;
	let mut publish_end = start;
	let mut core_bytes = 0_usize;
	while publish_end < end
		&& publish_end - publish_start < MAX_HIGHLIGHT_PUBLISH_ROWS
	{
		let input = highlight_input(&rows[publish_end], diff);
		let bytes = copied_highlight_bytes(&input);
		if core_bytes.saturating_add(bytes) > HIGHLIGHT_CORE_BYTES {
			break;
		}
		core_bytes += bytes;
		inputs.push(input);
		publish_end += 1;
	}

	let mut suffix_bytes = 0_usize;
	let suffix_end =
		end.min(publish_end.saturating_add(HIGHLIGHT_CONTEXT_ROWS));
	for row in &rows[publish_end..suffix_end] {
		let input = highlight_input(row, diff);
		let bytes = copied_highlight_bytes(&input);
		if suffix_bytes.saturating_add(bytes) > HIGHLIGHT_SUFFIX_BYTES
		{
			break;
		}
		suffix_bytes += bytes;
		inputs.push(input);
	}
	HighlightChunk {
		input_start,
		publish_start,
		publish_end,
		inputs,
	}
}

fn highlight_input(
	row: &RenderedDiffRow,
	diff: &FileDiff,
) -> DiffHighlightInput {
	let content_len = row.content.len();
	let line = &diff.hunks[row.hunk_index].lines[row.line_index];
	DiffHighlightInput {
		hunk: row.hunk_index,
		line_type: line.line_type,
		content: if content_len
			<= crate::ui::diff_highlight::MAX_PAIR_LINE_BYTES
		{
			row.content.clone()
		} else {
			String::new()
		},
		content_len,
	}
}

const fn copied_highlight_bytes(input: &DiffHighlightInput) -> usize {
	if input.content_len
		<= crate::ui::diff_highlight::MAX_PAIR_LINE_BYTES
	{
		input.content_len
	} else {
		0
	}
}

fn build_projection_chunk(
	rows: &[RenderedDiffRow],
	diff: &FileDiff,
	core_start: usize,
) -> Option<ProjectionChunk> {
	if core_start >= rows.len() {
		return None;
	}
	let requested_end = rows
		.len()
		.min(core_start.saturating_add(PROJECTION_CORE_ROWS));
	let mut bytes = 0_usize;
	let (core_inputs, actual_core_end) = append_projection_inputs(
		rows,
		diff,
		core_start,
		requested_end,
		false,
		&mut bytes,
	);
	if core_inputs.is_empty() {
		return None;
	}
	let (prefix, prefix_start) = append_projection_inputs(
		rows,
		diff,
		core_start.saturating_sub(PROJECTION_CONTEXT_ROWS),
		core_start,
		true,
		&mut bytes,
	);
	let suffix_end = rows
		.len()
		.min(actual_core_end.saturating_add(PROJECTION_CONTEXT_ROWS));
	let (suffix, suffix_row) = append_projection_inputs(
		rows,
		diff,
		actual_core_end,
		suffix_end,
		false,
		&mut bytes,
	);
	let leading_continues = prefix_start > 0
		&& same_replacement_run(
			&rows[prefix_start - 1],
			&rows[prefix_start],
			diff,
		);
	let trailing_continues = suffix_row < rows.len()
		&& same_replacement_run(
			&rows[suffix_row - 1],
			&rows[suffix_row],
			diff,
		);
	let mut inputs = prefix;
	inputs.extend(core_inputs);
	inputs.extend(suffix);
	Some(ProjectionChunk {
		core_start,
		core_end: actual_core_end,
		requested_end,
		leading_continues,
		trailing_continues,
		inputs,
	})
}

fn append_projection_inputs(
	rows: &[RenderedDiffRow],
	diff: &FileDiff,
	start: usize,
	end: usize,
	reverse: bool,
	bytes: &mut usize,
) -> (Vec<ProjectionInput>, usize) {
	let mut inputs = Vec::new();
	let mut count = 0_usize;
	while count < end.saturating_sub(start)
		&& *bytes < PROJECTION_MAX_BYTES
	{
		let raw_row = if reverse {
			end - count - 1
		} else {
			start + count
		};
		let row = &rows[raw_row];
		let line = &diff.hunks[row.hunk_index].lines[row.line_index];
		let content = (row.content.len()
			<= PROJECTION_MAX_LINE_BYTES)
			.then(|| row.content.clone());
		let next_bytes = bytes
			.saturating_add(content.as_ref().map_or(0, String::len));
		if next_bytes > PROJECTION_MAX_BYTES {
			break;
		}
		*bytes = next_bytes;
		inputs.push(ProjectionInput {
			raw_row,
			hunk_index: row.hunk_index,
			line_type: line.line_type,
			content,
		});
		count += 1;
	}
	if reverse {
		inputs.reverse();
	}
	let boundary = if reverse { end - count } else { start + count };
	(inputs, boundary)
}

fn same_replacement_run(
	first: &RenderedDiffRow,
	second: &RenderedDiffRow,
	diff: &FileDiff,
) -> bool {
	if first.hunk_index != second.hunk_index {
		return false;
	}
	let first = diff.hunks[first.hunk_index].lines[first.line_index]
		.line_type;
	let second = diff.hunks[second.hunk_index].lines
		[second.line_index]
		.line_type;
	(first == DiffLineType::Delete && second == DiffLineType::Delete)
		|| (first == DiffLineType::Add && second == DiffLineType::Add)
}

fn align_projection_chunk(
	inputs: &[ProjectionInput],
	core_start: usize,
	core_end: usize,
	leading_continues: bool,
	trailing_continues: bool,
) -> Vec<(usize, ProjectionPair)> {
	use asyncgit::diff_preview::{
		align_changed_lines, inline_diff_segments,
	};
	let mut pairs = Vec::new();
	let mut index = 0;
	while index < inputs.len() {
		if inputs[index].line_type != DiffLineType::Delete {
			index += 1;
			continue;
		}
		let hunk = inputs[index].hunk_index;
		let delete_start = index;
		while index < inputs.len()
			&& inputs[index].hunk_index == hunk
			&& inputs[index].line_type == DiffLineType::Delete
		{
			index += 1;
		}
		let delete_end = index;
		let add_start = index;
		while index < inputs.len()
			&& inputs[index].hunk_index == hunk
			&& inputs[index].line_type == DiffLineType::Add
		{
			index += 1;
		}
		if add_start == index {
			continue;
		}
		if (delete_start == 0 && leading_continues)
			|| (index == inputs.len() && trailing_continues)
		{
			continue;
		}
		let deleted = &inputs[delete_start..delete_end];
		let added = &inputs[add_start..index];
		if deleted.iter().any(|row| row.content.is_none())
			|| added.iter().any(|row| row.content.is_none())
		{
			continue;
		}
		let old_lines = deleted
			.iter()
			.map(|row| row.content.as_deref().unwrap_or_default())
			.collect::<Vec<_>>();
		let new_lines = added
			.iter()
			.map(|row| row.content.as_deref().unwrap_or_default())
			.collect::<Vec<_>>();
		for (old_index, new_index) in
			align_changed_lines(&old_lines, &new_lines)
		{
			let old = &deleted[old_index];
			if !(core_start..core_end).contains(&old.raw_row) {
				continue;
			}
			let Some(segments) = inline_diff_segments(
				old.content.as_deref().unwrap_or_default(),
				added[new_index]
					.content
					.as_deref()
					.unwrap_or_default(),
			) else {
				continue;
			};
			pairs.push((
				old.raw_row,
				ProjectionPair {
					new_row: added[new_index].raw_row,
					segments,
				},
			));
		}
	}
	pairs
}

fn build_native_fold_groups(
	rows: &[RenderedDiffRow],
	diff: &FileDiff,
	analysis: &StructuralAnalysis,
) -> Vec<NativeFoldGroup> {
	let numbered = rows
		.iter()
		.map(|row| {
			let line =
				&diff.hunks[row.hunk_index].lines[row.line_index];
			(line.position.old_lineno, line.position.new_lineno)
		})
		.collect::<Vec<_>>();
	let regions = analysis
		.regions
		.iter()
		.filter(|region| {
			region.foldable
				&& (matches!(region.kind, "function" | "method")
					|| region.is_test)
		})
		.collect::<Vec<_>>();
	let ranges = regions
		.iter()
		.map(|region| {
			(region.old_lines.clone(), region.new_lines.clone())
		})
		.collect::<Vec<_>>();
	let memberships =
		crate::ui::diff_presentation::region_row_membership(
			&numbered.iter().map(|(old, _)| *old).collect::<Vec<_>>(),
			&numbered.iter().map(|(_, new)| *new).collect::<Vec<_>>(),
			&ranges,
		);
	let mut candidates = Vec::new();
	for (region, member_rows) in regions.into_iter().zip(memberships)
	{
		let mut group_rows = Vec::new();
		let mut previous = None;
		for raw in member_rows {
			if let Some(last) = previous {
				let gap = &rows[last + 1..raw];
				if gap.iter().any(|row| {
					diff.hunks[row.hunk_index].lines[row.line_index]
						.line_type != DiffLineType::Header
				}) {
					candidates.push((
						region,
						std::mem::take(&mut group_rows),
					));
				}
			}
			group_rows.push(raw);
			previous = Some(raw);
		}
		if !group_rows.is_empty() {
			candidates.push((region, group_rows));
		}
	}
	// Child functions/tests win over their containing declarations; groups
	// that overlap an already-selected child are omitted from the default view.
	candidates.sort_unstable_by_key(|(_, rows)| {
		(rows.len(), rows.first().copied().unwrap_or(usize::MAX))
	});
	let mut claimed = HashSet::new();
	let mut groups = Vec::new();
	for (region, raw_rows) in candidates {
		if raw_rows.len() < 2
			|| raw_rows.iter().any(|row| claimed.contains(row))
		{
			continue;
		}
		let mut additions = 0;
		let mut deletions = 0;
		for &raw in &raw_rows {
			let line = &diff.hunks[rows[raw].hunk_index].lines
				[rows[raw].line_index];
			match line.line_type {
				DiffLineType::Add => additions += 1,
				DiffLineType::Delete => deletions += 1,
				_ => {}
			}
		}
		claimed.extend(raw_rows.iter().copied());
		let first = raw_rows[0] as u64;
		groups.push(NativeFoldGroup {
			id: region.id ^ first.rotate_left(32),
			region: region.clone(),
			raw_rows,
			additions,
			deletions,
		});
	}
	groups.sort_unstable_by_key(|group| {
		group.raw_rows.first().copied().unwrap_or(usize::MAX)
	});
	groups
}

fn build_block_source_index(
	rows: &[RenderedDiffRow],
	diff: &FileDiff,
) -> HashMap<(bool, u32), Vec<usize>> {
	let mut index: HashMap<(bool, u32), Vec<usize>> = HashMap::new();
	for (raw, row) in rows.iter().enumerate() {
		let line = &diff.hunks[row.hunk_index].lines[row.line_index];
		let (is_old, number) = match line.line_type {
			DiffLineType::Delete => (true, line.position.old_lineno),
			DiffLineType::Add => (false, line.position.new_lineno),
			_ => continue,
		};
		if let Some(number) = number {
			index.entry((is_old, number)).or_default().push(raw);
		}
	}
	index
}

fn map_native_block(
	rows: &[RenderedDiffRow],
	diff: &FileDiff,
	metadata: asyncgit::diff_preview::InlineReplacementBlock,
	source_index: &HashMap<(bool, u32), Vec<usize>>,
) -> Option<NativeBlockReplacement> {
	fn map_side(
		rows: &[RenderedDiffRow],
		source_lines: &[asyncgit::diff_preview::InlineSourceLine],
		text: &str,
		old_side: bool,
		source_index: &HashMap<(bool, u32), Vec<usize>>,
	) -> Option<Vec<usize>> {
		source_lines
			.iter()
			.map(|source| {
				let expected = text.get(source.text_range.clone())?;
				let line_number = source.line_number?;
				let matches = source_index
					.get(&(old_side, line_number))?
					.iter()
					.copied()
					.filter(|raw| rows[*raw].content == expected)
					.take(2)
					.collect::<Vec<_>>();
				if matches.len() != 1 {
					return None;
				}
				Some(matches[0])
			})
			.collect()
	}

	let old_rows = map_side(
		rows,
		&metadata.old_lines,
		&metadata.old_text,
		true,
		source_index,
	)?;
	let new_rows = map_side(
		rows,
		&metadata.new_lines,
		&metadata.new_text,
		false,
		source_index,
	)?;
	let mut all_rows = old_rows
		.iter()
		.chain(&new_rows)
		.copied()
		.collect::<Vec<_>>();
	all_rows.sort_unstable();
	all_rows.dedup();
	let start = *all_rows.first()?;
	let end = all_rows.last()?.saturating_add(1);
	if all_rows.len() != end.saturating_sub(start)
		|| old_rows.iter().any(|raw| *raw < start || *raw >= end)
		|| new_rows.iter().any(|raw| *raw < start || *raw >= end)
	{
		return None;
	}
	let hunk = rows[start].hunk_index;
	if (start..end).any(|raw| {
		rows[raw].hunk_index != hunk
			|| !matches!(
				diff.hunks[hunk].lines[rows[raw].line_index]
					.line_type,
				DiffLineType::Add | DiffLineType::Delete
			)
	}) || !old_rows.iter().all(|raw| {
		diff.hunks[hunk].lines[rows[*raw].line_index].line_type
			== DiffLineType::Delete
	}) || !new_rows.iter().all(|raw| {
		diff.hunks[hunk].lines[rows[*raw].line_index].line_type
			== DiffLineType::Add
	}) || old_rows.last().is_some_and(|raw| {
		new_rows.first().is_some_and(|new| raw >= new)
	}) {
		return None;
	}
	Some(NativeBlockReplacement {
		metadata,
		old_rows,
		new_rows,
	})
}

fn map_native_replacement(
	rows: &[RenderedDiffRow],
	diff: &FileDiff,
	metadata: &asyncgit::diff_preview::InlineReplacement,
	source_index: &HashMap<(bool, u32), Vec<usize>>,
) -> Option<(usize, ProjectionPair)> {
	fn find_row(
		rows: &[RenderedDiffRow],
		diff: &FileDiff,
		line_number: Option<u32>,
		text: &str,
		old_side: bool,
		source_index: &HashMap<(bool, u32), Vec<usize>>,
	) -> Option<usize> {
		let matches = source_index
			.get(&(old_side, line_number?))?
			.iter()
			.copied()
			.filter(|&raw| {
				rows[raw].content == text
					&& diff.hunks[rows[raw].hunk_index].lines
						[rows[raw].line_index]
						.line_type == if old_side {
						DiffLineType::Delete
					} else {
						DiffLineType::Add
					}
			})
			.take(2)
			.collect::<Vec<_>>();
		(matches.len() == 1).then(|| matches[0])
	}

	let old_row = find_row(
		rows,
		diff,
		metadata.old_lineno,
		&metadata.old_text,
		true,
		source_index,
	)?;
	let new_row = find_row(
		rows,
		diff,
		metadata.new_lineno,
		&metadata.new_text,
		false,
		source_index,
	)?;
	if old_row >= new_row
		|| rows[old_row].hunk_index != rows[new_row].hunk_index
		|| metadata.old_patch_index == metadata.new_patch_index
	{
		return None;
	}
	Some((
		old_row,
		ProjectionPair {
			new_row,
			segments: metadata.segments.clone(),
		},
	))
}

fn native_fold_summary(
	group: &NativeFoldGroup,
	expanded: bool,
) -> String {
	let marker = if expanded { "▾" } else { "▸" };
	let kind = group.region.kind.replace('_', " ");
	let line_span = group
		.region
		.new_lines
		.as_ref()
		.or(group.region.old_lines.as_ref())
		.map(|range| {
			format!(
				"lines {}–{}",
				range.start.saturating_add(1),
				range.end,
			)
		})
		.unwrap_or_default();
	format!(
		"{marker} {kind} {} · {} diff rows · +{} −{} · {line_span} · Enter to toggle",
		group.region.label,
		group.raw_rows.len(),
		group.additions,
		group.deletions,
	)
}

fn apply_native_folds(
	rows: Vec<DisplayDiffRow>,
	groups: &[NativeFoldGroup],
	folded_regions: &HashSet<u64>,
	longest_line: &mut usize,
) -> Vec<DisplayDiffRow> {
	let mut starts = HashMap::<usize, Vec<usize>>::new();
	let mut raw_groups = HashMap::<usize, usize>::new();
	for (group_index, group) in groups.iter().enumerate() {
		for &raw in &group.raw_rows {
			raw_groups.insert(raw, group_index);
		}
	}
	let mut first_display_row = HashMap::<usize, usize>::new();
	for (row_index, row) in rows.iter().enumerate() {
		for raw in row.raw_rows.iter() {
			if let Some(&group_index) = raw_groups.get(raw) {
				first_display_row
					.entry(group_index)
					.or_insert(row_index);
			}
		}
	}
	for (group_index, start) in first_display_row {
		starts.entry(start).or_default().push(group_index);
	}
	for group_indexes in starts.values_mut() {
		group_indexes.sort_unstable();
	}
	let mut result = Vec::with_capacity(rows.len());
	for (row_index, row) in rows.into_iter().enumerate() {
		if let Some(group_indexes) = starts.get(&row_index) {
			for &group_index in group_indexes {
				let group = &groups[group_index];
				let expanded = !folded_regions.contains(&group.id);
				let content = native_fold_summary(group, expanded);
				*longest_line = (*longest_line)
					.max(UnicodeWidthStr::width(content.as_str()));
				result.push(DisplayDiffRow {
					hunk_index: row.hunk_index,
					raw_rows: Arc::from([]),
					content,
					replacement: Vec::new(),
					fold: Some(group_index),
					block: None,
					block_gutter: None,
					block_sources: Vec::new(),
				});
			}
		}
		let is_fully_folded =
			row.raw_rows.first().is_some_and(|raw| {
				let Some(&group_index) = raw_groups.get(raw) else {
					return false;
				};
				let group = &groups[group_index];
				folded_regions.contains(&group.id)
					&& row.raw_rows.iter().all(|member| {
						raw_groups.get(member) == Some(&group_index)
					})
			});
		if !is_fully_folded {
			result.push(row);
		}
	}
	result
}

impl DiffRowHighlight {
	const fn is_selected(self) -> bool {
		matches!(self, Self::Selected | Self::CursorSelected)
	}

	const fn is_cursor(self) -> bool {
		matches!(self, Self::Cursor | Self::CursorSelected)
	}
}

impl Selection {
	const fn get_start(&self) -> usize {
		match self {
			Self::Single(start) | Self::Multiple(start, _) => *start,
		}
	}

	const fn get_end(&self) -> usize {
		match self {
			Self::Single(end) | Self::Multiple(_, end) => *end,
		}
	}

	fn get_top(&self) -> usize {
		match self {
			Self::Single(start) => *start,
			Self::Multiple(start, end) => cmp::min(*start, *end),
		}
	}

	fn get_bottom(&self) -> usize {
		match self {
			Self::Single(start) => *start,
			Self::Multiple(start, end) => cmp::max(*start, *end),
		}
	}

	fn modify(&mut self, direction: Direction, max: usize) {
		let start = self.get_start();
		let old_end = self.get_end();

		*self = match direction {
			Direction::Up => {
				Self::Multiple(start, old_end.saturating_sub(1))
			}

			Direction::Down => {
				Self::Multiple(start, cmp::min(old_end + 1, max))
			}
		};
	}

	fn contains(&self, index: usize) -> bool {
		match self {
			Self::Single(start) => index == *start,
			Self::Multiple(start, end) => {
				if start <= end {
					*start <= index && index <= *end
				} else {
					*end <= index && index <= *start
				}
			}
		}
	}
}

///
#[allow(clippy::struct_excessive_bools)] // Selection mode, extension, and hunk scope are orthogonal states.
pub struct DiffComponent {
	repo: RepoPathRef,
	diff: Option<FileDiff>,
	rendered_rows: Vec<RenderedDiffRow>,
	display_rows: Vec<DisplayDiffRow>,
	projection_pairs: RefCell<HashMap<usize, ProjectionPair>>,
	projection_scanned: RefCell<HashSet<usize>>,
	projection_requests: RefCell<HashSet<usize>>,
	projection_sender: crossbeam_channel::Sender<ProjectionResult>,
	projection_receiver: RefCell<Receiver<ProjectionResult>>,
	structural_preview: RefCell<DiffPreviewController>,
	structural_analysis: Option<StructuralAnalysis>,
	block_replacements: Vec<NativeBlockReplacement>,
	block_preview_generation: Option<u64>,
	block_layout_width: usize,
	fold_groups: Vec<NativeFoldGroup>,
	folded_regions: HashSet<u64>,
	fold_source_initialized: bool,
	row_highlights: RefCell<HashMap<usize, InlineHighlight>>,
	highlight_requests: RefCell<HashSet<usize>>,
	highlight_window: Cell<Option<(usize, usize)>>,
	highlight_sender: crossbeam_channel::Sender<DiffHighlightResult>,
	highlight_receiver: RefCell<Receiver<DiffHighlightResult>>,
	language_label: RefCell<String>,
	sender_app: crossbeam_channel::Sender<AsyncAppNotification>,
	highlight_generation: u64,
	highlight_epoch: Arc<AtomicU64>,
	highlight_window_generation: Cell<u64>,
	longest_line: usize,
	number_width: usize,
	pending: bool,
	selection: Selection,
	selection_anchor: usize,
	selection_cursor: usize,
	selection_mode: bool,
	selection_extending: bool,
	selection_is_hunk: bool,
	selected_hunk: Option<usize>,
	current_size: Cell<(u16, u16)>,
	inner_rect: Cell<Rect>,
	mouse_dragging: bool,
	focused: bool,
	current: Current,
	vertical_scroll: VerticalScroll,
	horizontal_scroll: HorizontalScroll,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	is_immutable: bool,
	options: SharedOptions,
	compact_projection: bool,
	projection_generation: Cell<u64>,
	drawn_generation: Cell<u64>,
	projection_epoch: Arc<AtomicU64>,
	diff_params: Option<asyncgit::DiffParams>,
}

impl DiffComponent {
	///
	pub fn new(env: &Environment, is_immutable: bool) -> Self {
		let (highlight_sender, highlight_receiver) = unbounded();
		let (projection_sender, projection_receiver) = unbounded();
		Self {
			focused: false,
			queue: env.queue.clone(),
			current: Current::default(),
			pending: false,
			selected_hunk: None,
			diff: None,
			rendered_rows: Vec::new(),
			display_rows: Vec::new(),
			projection_pairs: RefCell::new(HashMap::new()),
			projection_scanned: RefCell::new(HashSet::new()),
			projection_requests: RefCell::new(HashSet::new()),
			projection_sender,
			projection_receiver: RefCell::new(projection_receiver),
			structural_preview: RefCell::new(
				DiffPreviewController::new(
					env.repo.clone(),
					env.sender_git.clone(),
					env.key_config.clone(),
				),
			),
			structural_analysis: None,
			block_replacements: Vec::new(),
			block_preview_generation: None,
			block_layout_width: 0,
			fold_groups: Vec::new(),
			folded_regions: HashSet::new(),
			fold_source_initialized: false,
			row_highlights: RefCell::new(HashMap::new()),
			highlight_requests: RefCell::new(HashSet::new()),
			highlight_window: Cell::new(None),
			highlight_sender,
			highlight_receiver: RefCell::new(highlight_receiver),
			language_label: RefCell::new("Text".to_owned()),
			sender_app: env.sender_app.clone(),
			highlight_generation: 0,
			highlight_epoch: Arc::new(AtomicU64::new(0)),
			highlight_window_generation: Cell::new(0),
			longest_line: 0,
			number_width: 1,
			current_size: Cell::new((0, 0)),
			inner_rect: Cell::new(Rect::default()),
			mouse_dragging: false,
			selection: Selection::Single(0),
			selection_anchor: 0,
			selection_cursor: 0,
			selection_mode: false,
			selection_extending: false,
			selection_is_hunk: false,
			vertical_scroll: VerticalScroll::new(),
			horizontal_scroll: HorizontalScroll::new(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			is_immutable,
			repo: env.repo.clone(),
			options: env.options.clone(),
			compact_projection: true,
			projection_generation: Cell::new(0),
			drawn_generation: Cell::new(u64::MAX),
			projection_epoch: Arc::new(AtomicU64::new(0)),
			diff_params: None,
		}
	}
	///
	const fn can_scroll(&self) -> bool {
		self.display_rows.len() > 1
	}
	///
	pub fn current(&self) -> (String, bool) {
		(self.current.path.clone(), self.current.is_stage)
	}
	///
	const fn can_edit_file(&self) -> bool {
		!self.is_immutable && !self.current.path.is_empty()
	}

	const fn projection_is_drawn(&self) -> bool {
		self.diff.is_none()
			|| self.drawn_generation.get()
				== self.projection_generation.get()
	}
	///
	pub fn clear(&mut self, pending: bool) {
		self.current = Current::default();
		self.diff_params = None;
		self.structural_analysis = None;
		self.fold_groups.clear();
		self.folded_regions.clear();
		self.fold_source_initialized = false;
		self.diff = None;
		self.rendered_rows.clear();
		self.display_rows.clear();
		self.projection_pairs.borrow_mut().clear();
		self.projection_scanned.borrow_mut().clear();
		self.projection_requests.borrow_mut().clear();
		self.row_highlights.borrow_mut().clear();
		self.highlight_requests.borrow_mut().clear();
		self.highlight_window.set(None);
		"Text".clone_into(&mut self.language_label.borrow_mut());
		self.highlight_generation =
			self.highlight_generation.wrapping_add(1);
		self.highlight_window_generation.set(
			self.highlight_window_generation.get().wrapping_add(1),
		);
		self.highlight_epoch.store(
			self.highlight_window_generation.get(),
			Ordering::Release,
		);
		self.projection_generation
			.set(self.projection_generation.get().wrapping_add(1));
		self.projection_epoch.fetch_add(1, Ordering::AcqRel);
		self.drawn_generation.set(u64::MAX);
		self.longest_line = 0;
		self.vertical_scroll.reset();
		self.horizontal_scroll.reset();
		self.selection = Selection::Single(0);
		self.selection_anchor = 0;
		self.selection_cursor = 0;
		self.selection_mode = false;
		self.selection_extending = false;
		self.selection_is_hunk = false;
		self.mouse_dragging = false;
		self.selected_hunk = None;
		self.pending = pending;
	}
	///
	pub fn update(
		&mut self,
		path: String,
		is_stage: bool,
		diff: FileDiff,
	) {
		self.update_with_params(path, is_stage, diff, None);
	}

	/// Update the rendered diff while retaining the exact source parameters
	/// used by immutable review panes. Worker analysis must never infer them
	/// from the current repository/worktree.
	pub fn update_with_params(
		&mut self,
		path: String,
		is_stage: bool,
		diff: FileDiff,
		params: Option<asyncgit::DiffParams>,
	) {
		self.apply_projection_results();
		self.pending = false;

		let hash = hash(&diff);
		let params = params.or_else(|| {
			(!self.is_immutable).then(|| DiffParams {
				path: path.clone(),
				diff_type: if is_stage {
					DiffType::Stage
				} else {
					DiffType::WorkDir
				},
				options: self.options.borrow().diff_options(),
			})
		});
		let compact_projection =
			self.options.borrow().diff_preview_preference()
				!= DiffPreviewPreference::Unified;
		let source_changed = self.current.hash != hash
			|| self.current.path != path
			|| self.current.is_stage != is_stage
			|| self.diff_params != params;

		if source_changed
			|| self.compact_projection != compact_projection
		{
			self.install_diff_source(DiffSourceUpdate {
				path,
				is_stage,
				diff,
				params: params.clone(),
				hash,
				compact_projection,
				source_changed,
			});
		}
		if let Some(params) = params {
			if compact_projection {
				self.structural_preview.borrow_mut().request(
					params,
					usize::from(self.current_size.get().0).max(20),
					asyncgit::diff_preview::PreviewPreference::Auto,
					hash,
				);
			} else {
				self.structural_preview.borrow_mut().clear();
			}
		} else {
			self.structural_preview.borrow_mut().clear();
			self.structural_analysis = None;
			self.fold_groups.clear();
			self.folded_regions.clear();
			self.fold_source_initialized = false;
		}
		self.update_preview();
	}

	fn install_diff_source(&mut self, update: DiffSourceUpdate) {
		let DiffSourceUpdate {
			path,
			is_stage,
			diff,
			params,
			hash,
			compact_projection,
			source_changed,
		} = update;
		let same_file = self.current.path == path
			&& self.current.is_stage == is_stage;
		let old_cursor = self
			.row_anchor(self.selection_cursor)
			.filter(|_| same_file);
		let old_view_top = self
			.row_anchor(self.vertical_scroll.get_top())
			.filter(|_| same_file);
		let old_cursor_index = self.selection_cursor;
		let old_view_top_index = self.vertical_scroll.get_top();

		self.selection_mode = false;
		self.selection_extending = false;
		self.selection_is_hunk = false;
		if source_changed {
			self.structural_analysis = None;
			self.block_replacements.clear();
			self.block_preview_generation = None;
			self.fold_groups.clear();
			self.folded_regions.clear();
			self.fold_source_initialized = false;
		}
		self.diff = Some(diff);
		self.diff_params = params;
		if source_changed {
			self.projection_epoch.fetch_add(1, Ordering::AcqRel);
			self.projection_pairs.borrow_mut().clear();
			self.projection_scanned.borrow_mut().clear();
		}
		self.projection_requests.borrow_mut().clear();
		self.compact_projection = compact_projection;
		self.rebuild_render_cache();
		self.current = Current {
			path,
			is_stage,
			hash,
		};
		self.prepare_highlights();
		self.projection_generation
			.set(self.projection_generation.get().wrapping_add(1));
		self.drawn_generation.set(u64::MAX);
		self.mouse_dragging = false;

		let cursor = if same_file {
			old_cursor
				.as_ref()
				.and_then(|anchor| self.find_anchor(anchor))
				.unwrap_or_else(|| {
					old_cursor_index.min(
						self.display_rows.len().saturating_sub(1),
					)
				})
		} else {
			0
		};
		self.selection = Selection::Single(cursor);
		self.selection_anchor = cursor;
		self.selection_cursor = cursor;
		self.selected_hunk =
			self.display_rows.get(cursor).map(|row| row.hunk_index);
		if same_file {
			let top = old_view_top
				.as_ref()
				.and_then(|anchor| self.find_anchor(anchor))
				.unwrap_or(old_view_top_index);
			self.vertical_scroll.set_top(top);
		} else {
			self.vertical_scroll.reset();
			self.horizontal_scroll.reset();
		}
	}

	/// Pump the shared exact-source preview result and rebuild fold geometry
	/// only after its request has passed the controller's generation checks.
	pub fn update_preview(&mut self) {
		self.structural_preview.borrow_mut().update();
		let next = self
			.diff_params
			.as_ref()
			.filter(|_| {
				self.compact_projection && self.diff.is_some()
			})
			.and_then(|params| {
				self.structural_preview
					.borrow()
					.view()
					.structural_analysis_for(
						params,
						self.current.hash,
					)
					.cloned()
			});
		if !self.compact_projection {
			self.block_preview_generation = None;
			self.block_replacements.clear();
		} else if let Some(params) = &self.diff_params {
			let preview = self.structural_preview.borrow();
			let view = preview.view();
			let accepted_generation = view
				.preview_generation_for(params, self.current.hash);
			if let Some(generation) = accepted_generation {
				if self.block_preview_generation != Some(generation) {
					let source_index = build_block_source_index(
						&self.rendered_rows,
						self.diff.as_ref().expect("preview has diff"),
					);
					let diff =
						self.diff.as_ref().expect("preview has diff");
					let blocks = view
						.block_replacements_for(
							params,
							self.current.hash,
						)
						.into_iter()
						.flatten()
						.map(|(_, block)| block.clone())
						.filter_map(|block| {
							map_native_block(
								&self.rendered_rows,
								diff,
								block,
								&source_index,
							)
						})
						.collect();
					let replacements = view
						.replacements_for(params, self.current.hash)
						.into_iter()
						.flatten()
						.filter_map(|(_, replacement)| {
							map_native_replacement(
								&self.rendered_rows,
								diff,
								replacement,
								&source_index,
							)
						})
						.collect::<Vec<_>>();
					drop(preview);
					self.block_replacements = blocks;
					if !replacements.is_empty() {
						let accepted_old = replacements
							.iter()
							.map(|(old_row, _)| *old_row)
							.collect::<HashSet<_>>();
						let accepted_new = replacements
							.iter()
							.map(|(_, pair)| pair.new_row)
							.collect::<HashSet<_>>();
						let mut pairs =
							self.projection_pairs.borrow_mut();
						pairs.retain(|existing_old, existing| {
							!accepted_old.contains(existing_old)
								&& !accepted_new
									.contains(&existing.new_row)
						});
						for (old_row, pair) in replacements {
							pairs.insert(old_row, pair);
						}
						drop(pairs);
						self.prune_crossing_pair_folds();
					}
					self.block_preview_generation = Some(generation);
					self.rebuild_display_rows_preserving_anchors();
				}
			}
		}
		if self.compact_projection {
			if let Some(next) = next {
				if Some(&next) != self.structural_analysis.as_ref() {
					self.apply_structural_analysis(Some(next));
				}
			}
		}
	}

	/// Apply completed row-projection jobs before the next draw. This keeps
	/// actions tied to the projection the user can actually see.
	pub fn update_rendering(&mut self) {
		self.apply_projection_results();
	}

	fn apply_structural_analysis(
		&mut self,
		analysis: Option<StructuralAnalysis>,
	) {
		let old_cursor = self.row_anchor(self.selection_cursor);
		let old_anchor = self.row_anchor(self.selection_anchor);
		let old_top = self.row_anchor(self.vertical_scroll.get_top());
		self.fold_groups =
			analysis.as_ref().map_or_else(Vec::new, |data| {
				self.diff.as_ref().map_or_else(Vec::new, |diff| {
					build_native_fold_groups(
						&self.rendered_rows,
						diff,
						data,
					)
				})
			});
		self.prune_crossing_pair_folds();
		let group_ids = self
			.fold_groups
			.iter()
			.map(|group| group.id)
			.collect::<HashSet<_>>();
		self.folded_regions.retain(|id| group_ids.contains(id));
		if !self.fold_source_initialized {
			for group in &self.fold_groups {
				let span = group
					.region
					.new_lines
					.as_ref()
					.or(group.region.old_lines.as_ref())
					.map_or(0, |range| {
						range.end.saturating_sub(range.start)
					});
				if group.region.is_test
					|| (!group.region.contains_changes && span >= 16)
				{
					self.folded_regions.insert(group.id);
				}
			}
			self.fold_source_initialized = analysis.is_some();
		}
		self.structural_analysis = analysis;
		self.rebuild_display_rows();
		let cursor = old_cursor
			.as_ref()
			.and_then(|anchor| self.find_anchor(anchor))
			.unwrap_or_else(|| {
				self.selection_cursor
					.min(self.display_rows.len().saturating_sub(1))
			});
		let anchor = old_anchor
			.as_ref()
			.and_then(|anchor| self.find_anchor(anchor))
			.unwrap_or_else(|| {
				self.selection_anchor
					.min(self.display_rows.len().saturating_sub(1))
			});
		self.selection_cursor = cursor;
		self.selection_anchor = anchor;
		self.selection = match self.selection {
			Selection::Single(_) => Selection::Single(cursor),
			Selection::Multiple(_, _) => {
				Selection::Multiple(anchor, cursor)
			}
		};
		self.selected_hunk =
			self.display_rows.get(cursor).map(|row| row.hunk_index);
		if let Some(top) = old_top
			.as_ref()
			.and_then(|anchor| self.find_anchor(anchor))
		{
			self.vertical_scroll.set_top(top);
		}
		self.projection_generation
			.set(self.projection_generation.get().wrapping_add(1));
		self.drawn_generation.set(u64::MAX);
	}

	fn prune_crossing_pair_folds(&mut self) {
		if self.fold_groups.is_empty() {
			return;
		}
		let mut group_for_raw = vec![None; self.rendered_rows.len()];
		for (group_index, group) in
			self.fold_groups.iter().enumerate()
		{
			for &raw in &group.raw_rows {
				group_for_raw[raw] = Some(group_index);
			}
		}
		let pairs = self.projection_pairs.borrow();
		let mut invalid_groups = HashSet::new();
		for (&old_raw, pair) in pairs.iter() {
			let old_group = group_for_raw[old_raw];
			let new_group = group_for_raw[pair.new_row];
			if old_group != new_group {
				invalid_groups.extend(old_group);
				invalid_groups.extend(new_group);
			}
		}
		for block in &self.block_replacements {
			let first_group = block
				.old_rows
				.iter()
				.chain(&block.new_rows)
				.filter_map(|raw| group_for_raw[*raw])
				.min();
			if block
				.old_rows
				.iter()
				.chain(&block.new_rows)
				.any(|raw| group_for_raw[*raw] != first_group)
			{
				invalid_groups.extend(first_group);
				for raw in
					block.old_rows.iter().chain(&block.new_rows)
				{
					invalid_groups.extend(group_for_raw[*raw]);
				}
			}
		}
		if invalid_groups.is_empty() {
			return;
		}
		self.fold_groups = self
			.fold_groups
			.drain(..)
			.enumerate()
			.filter_map(|(index, group)| {
				(!invalid_groups.contains(&index)).then_some(group)
			})
			.collect();
		let group_ids = self
			.fold_groups
			.iter()
			.map(|group| group.id)
			.collect::<HashSet<_>>();
		self.folded_regions.retain(|id| group_ids.contains(id));
	}

	fn rebuild_render_cache(&mut self) {
		self.rendered_rows.clear();
		self.display_rows.clear();
		self.longest_line = 0;
		self.number_width = 1;
		let Some(diff) = &self.diff else { return };
		for (hunk_index, hunk) in diff.hunks.iter().enumerate() {
			for (line_index, line) in hunk.lines.iter().enumerate() {
				let content = Self::display_content(line);
				let width = UnicodeWidthStr::width(content.as_str());
				self.longest_line = self.longest_line.max(width);
				for number in [
					line.position.old_lineno,
					line.position.new_lineno,
				]
				.into_iter()
				.flatten()
				{
					self.number_width = self
						.number_width
						.max(number.to_string().len());
				}
				self.rendered_rows.push(RenderedDiffRow {
					hunk_index,
					line_index,
					content,
				});
			}
		}
		self.rebuild_display_rows();
	}

	fn rebuild_display_rows(&mut self) {
		self.block_layout_width = self.native_block_content_width();
		if self.diff.is_none() {
			self.display_rows.clear();
			return;
		}
		if !self.compact_projection {
			self.display_rows = self
				.rendered_rows
				.iter()
				.enumerate()
				.map(|(raw_index, row)| DisplayDiffRow {
					hunk_index: row.hunk_index,
					raw_rows: Arc::from([raw_index]),
					content: String::new(),
					replacement: Vec::new(),
					fold: None,
					block: None,
					block_gutter: None,
					block_sources: Vec::new(),
				})
				.collect();
			return;
		}
		let ordered = self.compact_rows_in_order();
		self.display_rows = apply_native_folds(
			ordered,
			&self.fold_groups,
			&self.folded_regions,
			&mut self.longest_line,
		);
	}

	fn rebuild_display_rows_preserving_anchors(&mut self) {
		let old_cursor = self.row_anchor(self.selection_cursor);
		let old_anchor = self.row_anchor(self.selection_anchor);
		let old_top = self.row_anchor(self.vertical_scroll.get_top());
		self.rebuild_display_rows();
		let cursor = old_cursor
			.as_ref()
			.and_then(|anchor| self.find_anchor(anchor))
			.unwrap_or_else(|| {
				self.selection_cursor
					.min(self.display_rows.len().saturating_sub(1))
			});
		let anchor = old_anchor
			.as_ref()
			.and_then(|anchor| self.find_anchor(anchor))
			.unwrap_or_else(|| {
				self.selection_anchor
					.min(self.display_rows.len().saturating_sub(1))
			});
		self.selection_cursor = cursor;
		self.selection_anchor = anchor;
		self.selection = match self.selection {
			Selection::Single(_) => Selection::Single(cursor),
			Selection::Multiple(_, _) => {
				Selection::Multiple(anchor, cursor)
			}
		};
		self.selected_hunk =
			self.display_rows.get(cursor).map(|row| row.hunk_index);
		if let Some(top) = old_top
			.as_ref()
			.and_then(|anchor| self.find_anchor(anchor))
		{
			self.vertical_scroll.set_top(top);
		}
		self.projection_generation
			.set(self.projection_generation.get().wrapping_add(1));
		self.drawn_generation.set(u64::MAX);
	}

	fn native_block_content_width(&self) -> usize {
		let viewport = usize::from(self.current_size.get().0);
		if viewport == 0 {
			return 72;
		}
		viewport
			.saturating_sub(
				crate::ui::diff_presentation::line_gutter_width(
					self.number_width,
				),
			)
			.max(1)
	}

	fn refresh_native_block_layout(&mut self) {
		let width = self.native_block_content_width();
		if width == self.block_layout_width
			|| self.block_replacements.is_empty()
		{
			return;
		}
		let old_cursor = self.row_anchor(self.selection_cursor);
		let old_anchor = self.row_anchor(self.selection_anchor);
		let old_top = self.row_anchor(self.vertical_scroll.get_top());
		self.block_layout_width = width;
		self.rebuild_display_rows();
		let cursor = old_cursor
			.as_ref()
			.and_then(|anchor| self.find_anchor(anchor))
			.unwrap_or_else(|| {
				self.selection_cursor
					.min(self.display_rows.len().saturating_sub(1))
			});
		let anchor = old_anchor
			.as_ref()
			.and_then(|anchor| self.find_anchor(anchor))
			.unwrap_or_else(|| {
				self.selection_anchor
					.min(self.display_rows.len().saturating_sub(1))
			});
		self.selection_cursor = cursor;
		self.selection_anchor = anchor;
		self.selection = match self.selection {
			Selection::Single(_) => Selection::Single(cursor),
			Selection::Multiple(_, _) => {
				Selection::Multiple(anchor, cursor)
			}
		};
		if let Some(top) = old_top
			.as_ref()
			.and_then(|anchor| self.find_anchor(anchor))
		{
			self.vertical_scroll.set_top(top);
		}
		self.projection_generation
			.set(self.projection_generation.get().wrapping_add(1));
		self.drawn_generation.set(u64::MAX);
	}

	fn compact_rows_in_order(&self) -> Vec<DisplayDiffRow> {
		let pairs = self.projection_pairs.borrow();
		let mut ordered =
			Vec::with_capacity(self.rendered_rows.len());
		let mut raw_index = 0;
		while raw_index < self.rendered_rows.len() {
			let source = &self.rendered_rows[raw_index];
			let line_type = self.raw_line_type(raw_index);
			if !matches!(
				line_type,
				DiffLineType::Add | DiffLineType::Delete
			) {
				ordered.push(self.single_display_row(raw_index));
				raw_index += 1;
				continue;
			}
			let hunk_index = source.hunk_index;
			let start = raw_index;
			while raw_index < self.rendered_rows.len()
				&& self.rendered_rows[raw_index].hunk_index
					== hunk_index
				&& matches!(
					self.raw_line_type(raw_index),
					DiffLineType::Add | DiffLineType::Delete
				) {
				raw_index += 1;
			}
			let end = raw_index;
			if let Some((block_index, block)) = self
				.block_replacements
				.iter()
				.enumerate()
				.find(|(_, block)| {
					block.old_rows.first() == Some(&start)
						&& block.old_rows.len() + block.new_rows.len()
							== end.saturating_sub(start)
				}) {
				ordered.extend(self.make_block_rows(
					block_index,
					block,
					start..end,
					hunk_index,
				));
				continue;
			}
			ordered.extend(
				self.merge_changed_run(
					start, end, hunk_index, &pairs,
				),
			);
		}
		ordered
	}

	fn make_block_rows(
		&self,
		block_index: usize,
		block: &NativeBlockReplacement,
		raw_range: std::ops::Range<usize>,
		hunk_index: usize,
	) -> Vec<DisplayDiffRow> {
		let content_width = self.block_layout_width.max(1);
		let visual_lines =
			crate::ui::diff_presentation::inline_block_visual_lines(
				&block.metadata,
				content_width,
			);
		let mut raw_rows = raw_range.collect::<Vec<_>>();
		raw_rows.sort_unstable();
		let raw_rows: Arc<[usize]> = raw_rows.into();
		visual_lines
			.into_iter()
			.enumerate()
			.map(|(visual_index, visual)| {
				let mut content = String::new();
				let mut replacement = Vec::new();
				let mut block_sources = Vec::new();
				for piece in visual.pieces {
					if piece.is_space {
						let start = content.len();
						content.push(' ');
						replacement.push(DisplayDiffSegment {
							range: start..content.len(),
							kind: asyncgit::diff_preview::InlineSegmentKind::Shared,
							source_row: raw_rows[0],
							source_range: 0..0,
						});
						continue;
					}
					let (source_row, source, joined_range, source_line) = match piece.kind {
						asyncgit::diff_preview::InlineSegmentKind::Removed => {
							let line = piece.old_line_index;
							let source_row = line
								.and_then(|line| block.old_rows.get(line))
								.copied()
								.unwrap_or(raw_rows[0]);
							(source_row, &block.metadata.old_text, piece.old_range, line)
						}
						asyncgit::diff_preview::InlineSegmentKind::Added => {
							let line = piece.new_line_index;
							let source_row = line
								.and_then(|line| block.new_rows.get(line))
								.copied()
								.unwrap_or_else(|| {
									*block.new_rows.first().unwrap_or(&raw_rows[0])
								});
							(source_row, &block.metadata.new_text, piece.new_range, line)
						}
						asyncgit::diff_preview::InlineSegmentKind::Shared => {
							let line = piece.new_line_index;
							let source_row = line
								.and_then(|line| block.new_rows.get(line))
								.copied()
								.unwrap_or_else(|| {
									*block.new_rows.first().unwrap_or(&raw_rows[0])
								});
							(source_row, &block.metadata.new_text, piece.new_range, line)
						}
					};
					let old_side = piece.kind
						== asyncgit::diff_preview::InlineSegmentKind::Removed;
					block_sources.push((old_side, joined_range.clone()));
					let source_lines = if old_side {
						&block.metadata.old_lines
					} else {
						&block.metadata.new_lines
					};
				let Some(line) = source_line.and_then(|i| source_lines.get(i)) else {
						continue;
					};
				let local_range = joined_range.start.saturating_sub(line.text_range.start)
					..joined_range.end.saturating_sub(line.text_range.start);
				let Some(text) = source.get(joined_range.clone()) else {
						continue;
					};
					let start = content.len();
					content.push_str(text);
					replacement.push(DisplayDiffSegment {
						range: start..content.len(),
						kind: piece.kind,
						source_row,
						source_range: local_range,
					});
				}
				DisplayDiffRow {
					hunk_index,
					raw_rows: Arc::clone(&raw_rows),
					content,
					replacement,
					fold: None,
					block: Some((block_index, visual_index)),
					block_gutter: Some((visual.old_lineno, visual.new_lineno)),
					block_sources,
				}
			})
			.collect()
	}

	fn merge_changed_run(
		&self,
		start: usize,
		end: usize,
		hunk_index: usize,
		pairs: &HashMap<usize, ProjectionPair>,
	) -> Vec<DisplayDiffRow> {
		let deletes = (start..end)
			.filter(|&raw| {
				self.raw_line_type(raw) == DiffLineType::Delete
			})
			.collect::<Vec<_>>();
		let adds = (start..end)
			.filter(|&raw| {
				self.raw_line_type(raw) == DiffLineType::Add
			})
			.collect::<Vec<_>>();
		if !deletes.iter().chain(&adds).copied().eq(start..end) {
			return (start..end)
				.map(|raw| self.single_display_row(raw))
				.collect();
		}
		let local_pairs = deletes
			.iter()
			.enumerate()
			.filter_map(|(old_index, &old_raw)| {
				let pair = pairs.get(&old_raw)?;
				let new_index = adds
					.iter()
					.position(|&raw| raw == pair.new_row)?;
				Some((old_index, new_index))
			})
			.collect::<Vec<_>>();
		let Some(merged) = asyncgit::diff_preview::merge_changed_rows(
			deletes.len(),
			adds.len(),
			&local_pairs,
		) else {
			return (start..end)
				.map(|raw| self.single_display_row(raw))
				.collect();
		};
		merged
			.into_iter()
			.map(|change| match change {
				MergedChangeRow::OldDeleted(index) => {
					self.single_display_row(deletes[index])
				}
				MergedChangeRow::NewAdded(index) => {
					self.single_display_row(adds[index])
				}
				MergedChangeRow::Replacement {
					old_index,
					new_index,
				} => {
					let old_raw = deletes[old_index];
					let new_raw = adds[new_index];
					pairs.get(&old_raw).map_or_else(
						|| self.single_display_row(old_raw),
						|pair| {
							self.make_replacement_row(
								hunk_index,
								old_raw,
								new_raw,
								pair.segments.clone(),
							)
						},
					)
				}
			})
			.collect()
	}

	fn raw_line_type(&self, raw_index: usize) -> DiffLineType {
		let row = &self.rendered_rows[raw_index];
		self.diff.as_ref().expect("rendered rows have a diff").hunks
			[row.hunk_index]
			.lines[row.line_index]
			.line_type
	}

	fn single_display_row(&self, raw_index: usize) -> DisplayDiffRow {
		DisplayDiffRow {
			hunk_index: self.rendered_rows[raw_index].hunk_index,
			raw_rows: Arc::from([raw_index]),
			content: String::new(),
			replacement: Vec::new(),
			fold: None,
			block: None,
			block_gutter: None,
			block_sources: Vec::new(),
		}
	}

	fn schedule_projection(
		&self,
		visible_start: usize,
		visible_end: usize,
	) {
		if !self.compact_projection || self.rendered_rows.is_empty() {
			return;
		}
		let Some(diff) = &self.diff else { return };
		let raw = self.display_rows[visible_start..visible_end]
			.iter()
			.flat_map(|row| row.raw_rows.iter().copied())
			.collect::<Vec<_>>();
		let Some(mut raw_row) = raw.iter().copied().min() else {
			return;
		};
		let raw_end = raw
			.iter()
			.copied()
			.max()
			.map_or(raw_row, |index| index.saturating_add(1));
		while raw_row < raw_end {
			let core_end = self
				.rendered_rows
				.len()
				.min(raw_row.saturating_add(PROJECTION_CORE_ROWS));
			{
				let scanned = self.projection_scanned.borrow();
				let requests = self.projection_requests.borrow();
				while raw_row < core_end
					&& (scanned.contains(&raw_row)
						|| requests.contains(&raw_row))
				{
					raw_row += 1;
				}
			}
			if raw_row >= raw_end {
				break;
			}
			let Some(chunk) = build_projection_chunk(
				&self.rendered_rows,
				diff,
				raw_row,
			) else {
				break;
			};
			self.projection_requests
				.borrow_mut()
				.extend(chunk.core_start..chunk.requested_end);
			self.launch_projection_job(chunk);
			raw_row = raw_row.saturating_add(PROJECTION_CORE_ROWS);
		}
	}

	fn launch_projection_job(&self, chunk: ProjectionChunk) {
		let sender = self.projection_sender.clone();
		let sender_app = self.sender_app.clone();
		let path = self.current.path.clone();
		let is_stage = self.current.is_stage;
		let hash = self.current.hash;
		let generation = self.highlight_generation;
		let epoch = Arc::clone(&self.projection_epoch);
		let expected_epoch = epoch.load(Ordering::Acquire);
		rayon_core::spawn(move || {
			if epoch.load(Ordering::Acquire) != expected_epoch {
				return;
			}
			let pairs = align_projection_chunk(
				&chunk.inputs,
				chunk.core_start,
				chunk.core_end,
				chunk.leading_continues,
				chunk.trailing_continues,
			);
			if epoch.load(Ordering::Acquire) != expected_epoch {
				return;
			}
			let _ = sender.send(ProjectionResult {
				generation,
				path,
				is_stage,
				hash,
				core_start: chunk.core_start,
				core_end: chunk.core_end,
				requested_end: chunk.requested_end,
				pairs,
			});
			let _ = sender_app
				.send(AsyncAppNotification::DiffHighlighting);
		});
	}

	fn apply_projection_results(&mut self) {
		let results = self
			.projection_receiver
			.borrow()
			.try_iter()
			.collect::<Vec<_>>();
		for result in results {
			if result.generation != self.highlight_generation
				|| result.path != self.current.path
				|| result.is_stage != self.current.is_stage
				|| result.hash != self.current.hash
			{
				continue;
			}
			self.projection_requests.borrow_mut().retain(|index| {
				*index < result.core_start
					|| *index >= result.requested_end
			});
			self.projection_scanned
				.borrow_mut()
				.extend(result.core_start..result.core_end);
			if result.pairs.is_empty() {
				continue;
			}
			let old_cursor = self.row_anchor(self.selection_cursor);
			let old_anchor = self.row_anchor(self.selection_anchor);
			let old_top =
				self.row_anchor(self.vertical_scroll.get_top());
			let mut changed = false;
			{
				let mut pairs = self.projection_pairs.borrow_mut();
				for (old_row, pair) in result.pairs {
					let already_used = pairs
						.values()
						.any(|item| item.new_row == pair.new_row);
					if !already_used && !pairs.contains_key(&old_row)
					{
						pairs.insert(old_row, pair);
						changed = true;
					}
				}
			}
			if !changed {
				continue;
			}
			self.prune_crossing_pair_folds();
			self.rebuild_display_rows();
			let cursor = old_cursor
				.as_ref()
				.and_then(|anchor| self.find_anchor(anchor))
				.unwrap_or(self.selection_cursor);
			let anchor = old_anchor
				.as_ref()
				.and_then(|anchor| self.find_anchor(anchor))
				.unwrap_or(self.selection_anchor);
			self.selection_cursor = cursor;
			self.selection_anchor = anchor;
			self.selection = match self.selection {
				Selection::Single(_) => Selection::Single(cursor),
				Selection::Multiple(_, _) => {
					Selection::Multiple(anchor, cursor)
				}
			};
			self.selected_hunk = self
				.display_rows
				.get(cursor)
				.map(|row| row.hunk_index);
			if let Some(top) = old_top
				.as_ref()
				.and_then(|anchor| self.find_anchor(anchor))
			{
				self.vertical_scroll.set_top(top);
			}
			self.projection_generation.set(
				self.projection_generation.get().wrapping_add(1),
			);
			self.drawn_generation.set(u64::MAX);
		}
	}

	fn make_replacement_row(
		&self,
		hunk_index: usize,
		old_raw: usize,
		new_raw: usize,
		segments: Vec<asyncgit::diff_preview::InlineSegment>,
	) -> DisplayDiffRow {
		use asyncgit::diff_preview::InlineSegmentKind as Kind;
		let mut content = String::new();
		let mut display_segments = Vec::with_capacity(segments.len());
		for segment in segments {
			let (source_row, source_range) = match segment.kind {
				Kind::Shared | Kind::Removed => {
					(old_raw, segment.old_range)
				}
				Kind::Added => (new_raw, segment.new_range),
			};
			let start = content.len();
			content.push_str(
				&self.rendered_rows[source_row].content
					[source_range.clone()],
			);
			let end = content.len();
			display_segments.push(DisplayDiffSegment {
				range: start..end,
				kind: segment.kind,
				source_row,
				source_range,
			});
		}
		DisplayDiffRow {
			hunk_index,
			raw_rows: Arc::from([old_raw, new_raw]),
			content,
			replacement: display_segments,
			fold: None,
			block: None,
			block_gutter: None,
			block_sources: Vec::new(),
		}
	}

	fn prepare_highlights(&mut self) {
		self.highlight_generation =
			self.highlight_generation.wrapping_add(1);
		self.highlight_window_generation.set(
			self.highlight_window_generation.get().wrapping_add(1),
		);
		self.highlight_epoch.store(
			self.highlight_window_generation.get(),
			Ordering::Release,
		);
		let path = PathBuf::from(self.current.path.clone());
		*self.language_label.borrow_mut() =
			crate::ui::diff_highlight::language_label(&path);
		self.row_highlights.borrow_mut().clear();
		self.highlight_requests.borrow_mut().clear();
		self.highlight_window.set(None);
	}

	fn schedule_highlights(
		&self,
		visible_start: usize,
		visible_end: usize,
	) {
		if self.rendered_rows.is_empty() {
			return;
		}
		let visible_raw = self.display_rows
			[visible_start..visible_end]
			.iter()
			.flat_map(|row| row.raw_rows.iter().copied())
			.collect::<Vec<_>>();
		let raw_start =
			visible_raw.iter().copied().min().unwrap_or(0);
		let raw_end = visible_raw
			.iter()
			.copied()
			.max()
			.map_or(raw_start, |index| index.saturating_add(1));
		let window_start = raw_start / HIGHLIGHT_WINDOW_STRIDE
			* HIGHLIGHT_WINDOW_STRIDE;
		let window_end = self.rendered_rows.len().min(
			window_start
				.saturating_add(HIGHLIGHT_WINDOW_ROWS)
				.max(raw_end),
		);
		let window = (window_start, window_end);
		if self.highlight_window.get() != Some(window) {
			self.highlight_window.set(Some(window));
			self.highlight_window_generation.set(
				self.highlight_window_generation
					.get()
					.wrapping_add(1),
			);
			self.highlight_epoch.store(
				self.highlight_window_generation.get(),
				Ordering::Release,
			);
			self.row_highlights.borrow_mut().retain(|index, _| {
				*index >= window_start && *index < window_end
			});
			self.highlight_requests.borrow_mut().clear();
		}

		let mut start = window_start;
		{
			let highlights = self.row_highlights.borrow();
			let requests = self.highlight_requests.borrow();
			while start < window_end {
				if !highlights.contains_key(&start)
					&& !requests.contains(&start)
				{
					break;
				}
				start += 1;
			}
			if start == window_end {
				return;
			}
		}

		let Some(diff) = &self.diff else { return };
		let chunk = build_highlight_inputs(
			&self.rendered_rows,
			diff,
			start,
			window_end,
		);
		if chunk.inputs.is_empty() {
			return;
		}
		self.highlight_requests
			.borrow_mut()
			.extend(chunk.publish_start..chunk.publish_end);
		self.launch_highlight_job(chunk);
	}

	fn launch_highlight_job(&self, chunk: HighlightChunk) {
		let sender = self.highlight_sender.clone();
		let path = PathBuf::from(self.current.path.clone());
		let theme_name = self.theme.get_syntax();

		let result_path = self.current.path.clone();
		let is_stage = self.current.is_stage;
		let hash = self.current.hash;
		let generation = self.highlight_generation;
		let window_generation =
			self.highlight_window_generation.get();
		let sender_app = self.sender_app.clone();
		let epoch = Arc::clone(&self.highlight_epoch);
		let input_start = chunk.input_start;
		let publish_start = chunk.publish_start;
		let publish_end = chunk.publish_end;
		rayon_core::spawn(move || {
			if epoch.load(Ordering::Acquire) != window_generation {
				return;
			}
			let highlights = highlight_diff_rows(
				&path,
				&chunk.inputs,
				&theme_name,
			);
			let row_offset = publish_start - input_start;
			let row_end = row_offset + publish_end - publish_start;
			let rows = highlights[row_offset..row_end].to_vec();
			let language =
				crate::ui::diff_highlight::syntax_language_label(
					&path,
				);
			let _ = sender.send(DiffHighlightResult {
				generation,
				window_generation,
				start: publish_start,
				path: result_path,
				is_stage,
				hash,
				language,
				rows,
			});
			let _ = sender_app
				.send(AsyncAppNotification::DiffHighlighting);
		});
	}

	fn apply_highlight_result(&self) {
		for result in self.highlight_receiver.borrow().try_iter() {
			if result.generation == self.highlight_generation
				&& result.window_generation
					== self.highlight_window_generation.get()
				&& result.path == self.current.path
				&& result.is_stage == self.current.is_stage
				&& result.hash == self.current.hash
				&& result.start.saturating_add(result.rows.len())
					<= self.rendered_rows.len()
			{
				let mut requests =
					self.highlight_requests.borrow_mut();
				for index in
					result.start..result.start + result.rows.len()
				{
					requests.remove(&index);
				}
				drop(requests);
				let mut highlights = self.row_highlights.borrow_mut();
				for (index, highlight) in
					result.rows.into_iter().enumerate()
				{
					highlights
						.insert(result.start + index, highlight);
				}
				*self.language_label.borrow_mut() = result.language;
			}
		}
	}

	fn display_content(line: &DiffLine) -> String {
		let converted =
			tabs_to_spaces(line.content.as_ref().to_owned());
		crate::string_utils::sanitize_control_chars(
			converted.trim_end_matches(['\n', '\r']),
		)
	}

	fn row_anchor(&self, row_index: usize) -> Option<DiffRowAnchor> {
		let diff = self.diff.as_ref()?;
		let display_row = self.display_rows.get(row_index)?;
		let raw_patch_index =
			display_row.raw_rows.first().copied().or_else(|| {
				display_row.fold.and_then(|index| {
					self.fold_groups
						.get(index)
						.and_then(|group| group.raw_rows.first())
						.copied()
				})
			})?;
		let row = self.rendered_rows.get(raw_patch_index)?;
		let line = diff
			.hunks
			.get(row.hunk_index)?
			.lines
			.get(row.line_index)?;
		Some(DiffRowAnchor {
			fold_id: display_row
				.fold
				.and_then(|index| self.fold_groups.get(index))
				.map(|group| group.id),
			block: display_row.block.and_then(|(block_index, _)| {
				display_row.block_sources.first().map(
					|(old_side, range)| {
						(block_index, *old_side, range.start)
					},
				)
			}),
			line_type: line.line_type,
			position: line.position,
			content: line.content.to_string(),
		})
	}

	fn find_anchor(&self, anchor: &DiffRowAnchor) -> Option<usize> {
		if let Some((block_index, old_side, position)) = anchor.block
		{
			if let Some(index) =
				self.display_rows.iter().position(|row| {
					row.block.is_some_and(|(index, _)| {
						index == block_index
					}) && row.block_sources.iter().any(
						|(is_old, range)| {
							*is_old == old_side
								&& range.start <= position
								&& position < range.end
						},
					)
				}) {
				return Some(index);
			}
		}
		if let Some(fold_id) = anchor.fold_id {
			if let Some(index) =
				self.display_rows.iter().position(|row| {
					row.fold.is_some_and(|index| {
						self.fold_groups[index].id == fold_id
					})
				}) {
				return Some(index);
			}
		}
		let diff = self.diff.as_ref()?;
		let row_matches = |raw_index: usize| {
			let row = &self.rendered_rows[raw_index];
			let line =
				&diff.hunks[row.hunk_index].lines[row.line_index];
			line.line_type == anchor.line_type
				&& line.position == anchor.position
				&& line.content.as_ref() == anchor.content
		};
		let exact = self.display_rows.iter().position(|row| {
			row.raw_rows.iter().copied().any(row_matches)
		});
		if exact.is_some() {
			return exact;
		}
		let folded = self.display_rows.iter().position(|row| {
			row.fold.is_some_and(|index| {
				let group = &self.fold_groups[index];
				self.folded_regions.contains(&group.id)
					&& group.raw_rows.iter().copied().any(row_matches)
			})
		});
		if folded.is_some() {
			return folded;
		}

		let target_lineno = anchor
			.position
			.new_lineno
			.or(anchor.position.old_lineno)?;
		self.find_nearest_anchor(anchor, diff, target_lineno)
	}

	fn find_nearest_anchor(
		&self,
		anchor: &DiffRowAnchor,
		diff: &FileDiff,
		target_lineno: u32,
	) -> Option<usize> {
		let by_content = self
			.display_rows
			.iter()
			.enumerate()
			.filter(|(_, row)| {
				self.display_row_raw_indexes(row).into_iter().any(
					|raw| {
						let source = &self.rendered_rows[raw];
						let line = &diff.hunks[source.hunk_index]
							.lines[source.line_index];
						line.line_type == anchor.line_type
							&& line.content.as_ref() == anchor.content
					},
				)
			})
			.min_by_key(|(_, row)| {
				self.display_row_raw_indexes(row)
					.iter()
					.map(|raw| {
						let source = &self.rendered_rows[*raw];
						let line = &diff.hunks[source.hunk_index]
							.lines[source.line_index];
						line.position
							.new_lineno
							.or(line.position.old_lineno)
							.map_or(u32::MAX, |line| {
								line.abs_diff(target_lineno)
							})
					})
					.min()
			})
			.map(|(index, _)| index);
		by_content.or_else(|| {
			self.display_rows
				.iter()
				.enumerate()
				.min_by_key(|(_, row)| {
					self.display_row_raw_indexes(row)
						.iter()
						.map(|raw| {
							let source = &self.rendered_rows[*raw];
							let line = &diff.hunks[source.hunk_index]
								.lines[source.line_index];
							line.position
								.new_lineno
								.or(line.position.old_lineno)
								.map_or(u32::MAX, |line| {
									line.abs_diff(target_lineno)
								})
						})
						.min()
				})
				.map(|(index, _)| index)
		})
	}

	fn display_row_raw_indexes(
		&self,
		row: &DisplayDiffRow,
	) -> Vec<usize> {
		let mut indexes = row.raw_rows.to_vec();
		if let Some(group) =
			row.fold.and_then(|index| self.fold_groups.get(index))
		{
			indexes.extend(group.raw_rows.iter().copied());
		}
		indexes
	}

	fn move_selection(&mut self, move_type: ScrollType) {
		if self.diff.is_some() {
			let max = self.display_rows.len().saturating_sub(1);

			let new_start = match move_type {
				ScrollType::Down
					if self.selection_mode
						&& self.selection_extending =>
				{
					self.selection_cursor.saturating_add(1)
				}
				ScrollType::Up
					if self.selection_mode
						&& self.selection_extending =>
				{
					self.selection_cursor.saturating_sub(1)
				}
				ScrollType::Down => {
					self.selection.get_bottom().saturating_add(1)
				}
				ScrollType::Up => {
					self.selection.get_top().saturating_sub(1)
				}
				ScrollType::Home => 0,
				ScrollType::End => max,
				ScrollType::PageDown
					if self.selection_mode
						&& self.selection_extending =>
				{
					self.selection_cursor.saturating_add(
						self.current_size.get().1.saturating_sub(1)
							as usize,
					)
				}
				ScrollType::PageUp
					if self.selection_mode
						&& self.selection_extending =>
				{
					self.selection_cursor.saturating_sub(
						self.current_size.get().1.saturating_sub(1)
							as usize,
					)
				}
				ScrollType::PageDown => {
					self.selection.get_bottom().saturating_add(
						self.current_size.get().1.saturating_sub(1)
							as usize,
					)
				}
				ScrollType::PageUp => {
					self.selection.get_top().saturating_sub(
						self.current_size.get().1.saturating_sub(1)
							as usize,
					)
				}
			};

			if self.selection_mode && self.selection_extending {
				self.selection = Selection::Multiple(
					self.selection_anchor,
					cmp::min(max, new_start),
				);
				self.selection_cursor = cmp::min(max, new_start);
				self.selected_hunk = self
					.display_rows
					.get(self.selection_cursor)
					.map(|row| row.hunk_index);
				self.vertical_scroll.ensure_visible(
					self.selection_cursor,
					self.display_rows.len(),
				);
			} else {
				self.update_selection(new_start);
			}
		}
	}

	fn update_selection(&mut self, new_start: usize) {
		if self.diff.is_some() {
			let max = self.display_rows.len().saturating_sub(1);
			let new_start = cmp::min(max, new_start);
			self.selection = Selection::Single(new_start);
			self.selection_anchor = new_start;
			self.selection_cursor = new_start;
			self.selection_is_hunk = false;
			self.selected_hunk = self
				.display_rows
				.get(new_start)
				.map(|row| row.hunk_index);
			self.vertical_scroll
				.ensure_visible(new_start, self.display_rows.len());
		}
	}

	const fn lines_count(&self) -> usize {
		self.display_rows.len()
	}

	fn max_scroll_right(&self) -> usize {
		self.longest_line.saturating_sub(self.text_width())
	}

	fn text_width(&self) -> usize {
		let width = usize::from(self.current_size.get().0);
		let numbers = self.number_width;
		let gutter_width = if width >= numbers * 2 + 8 {
			crate::ui::diff_presentation::line_gutter_width(numbers)
				+ 2
		} else {
			2
		};
		width.saturating_sub(gutter_width)
	}

	fn modify_selection(&mut self, direction: Direction) {
		if self.diff.is_some() {
			self.selection.modify(
				direction,
				self.lines_count().saturating_sub(1),
			);
			self.selection_cursor = self.selection.get_end();
			self.selection_is_hunk = false;
			self.vertical_scroll.ensure_visible(
				self.selection_cursor,
				self.lines_count(),
			);
		}
	}

	const fn toggle_selection_mode(&mut self) {
		if !self.selection_mode && self.lines_count() == 0 {
			return;
		}
		self.selection_mode = !self.selection_mode;
		self.selection_extending = self.selection_mode;
		self.selection_is_hunk = false;
		if self.selection_mode {
			self.selection_anchor = self.selection.get_end();
			self.selection_cursor = self.selection.get_end();
			self.selection = Selection::Single(self.selection_anchor);
		}
	}

	fn select_hunk(&mut self) {
		let Some(_diff) = &self.diff else { return };
		let Some(hunk) = self.selected_hunk else {
			return;
		};
		let Some(start) = self
			.display_rows
			.iter()
			.position(|row| row.hunk_index == hunk)
		else {
			return;
		};
		let end = self
			.display_rows
			.iter()
			.rposition(|row| row.hunk_index == hunk)
			.unwrap_or(start);
		self.selection_anchor = start;
		self.selection_cursor = end;
		self.selection = Selection::Multiple(start, end);
		self.selection_mode = true;
		self.selection_extending = false;
		self.selection_is_hunk = true;
	}

	const fn select_all(&mut self) {
		if self.diff.is_none() || self.display_rows.is_empty() {
			return;
		}
		self.selection_anchor = 0;
		self.selection_cursor = self.display_rows.len() - 1;
		self.selection =
			Selection::Multiple(0, self.display_rows.len() - 1);
		self.selection_mode = true;
		self.selection_extending = false;
		self.selection_is_hunk = false;
	}

	fn clear_selection(&mut self) -> bool {
		if !self.selection_mode
			&& matches!(self.selection, Selection::Single(_))
		{
			return false;
		}
		let cursor = self.selection_cursor;
		self.selection = Selection::Single(cursor);
		self.selection_anchor = cursor;
		self.selection_mode = false;
		self.selection_extending = false;
		self.selection_is_hunk = false;
		self.selected_hunk =
			self.display_rows.get(cursor).map(|row| row.hunk_index);
		true
	}

	fn toggle_fold(&mut self, display_index: usize) -> bool {
		let Some(group_index) = self
			.display_rows
			.get(display_index)
			.and_then(|row| row.fold)
		else {
			return false;
		};
		let Some(group) = self.fold_groups.get(group_index) else {
			return false;
		};
		let id = group.id;
		let anchor = self.row_anchor(display_index);
		if !self.folded_regions.remove(&id) {
			self.folded_regions.insert(id);
		}
		self.rebuild_display_rows();
		let cursor = anchor
			.as_ref()
			.and_then(|anchor| self.find_anchor(anchor))
			.unwrap_or_else(|| {
				display_index
					.min(self.display_rows.len().saturating_sub(1))
			});
		self.selection = Selection::Single(cursor);
		self.selection_anchor = cursor;
		self.selection_cursor = cursor;
		self.selection_mode = false;
		self.selection_extending = false;
		self.selection_is_hunk = false;
		self.selected_hunk =
			self.display_rows.get(cursor).map(|row| row.hunk_index);
		self.projection_generation
			.set(self.projection_generation.get().wrapping_add(1));
		self.drawn_generation.set(u64::MAX);
		true
	}

	fn selection_status_inner(&self) -> Option<SelectionStatus> {
		if !self.selection_mode {
			return None;
		}
		let selected_rows = self
			.display_rows
			.iter()
			.enumerate()
			.filter(|(index, _)| self.selection.contains(*index))
			.map(|(_, row)| row)
			.collect::<Vec<_>>();
		let paragraph_indexes = selected_rows
			.iter()
			.filter_map(|row| row.block.map(|(index, _)| index))
			.collect::<HashSet<_>>();
		let only_paragraphs = !paragraph_indexes.is_empty()
			&& selected_rows.iter().all(|row| row.block.is_some());
		let patch_line_count = selected_rows
			.iter()
			.flat_map(|row| row.raw_rows.iter().copied())
			.collect::<HashSet<_>>()
			.len();
		Some(SelectionStatus {
			extending: self.selection_extending,
			count: if only_paragraphs {
				paragraph_indexes.len()
			} else {
				patch_line_count
			},
			unit: if only_paragraphs {
				if paragraph_indexes.len() == 1 {
					"paragraph"
				} else {
					"paragraphs"
				}
			} else {
				"patch lines"
			},
		})
	}

	fn active_paragraph_scope(&self) -> Option<(usize, usize)> {
		let block_index =
			self.display_rows.get(self.selection_cursor)?.block?.0;
		let block = self.block_replacements.get(block_index)?;
		Some((block.old_rows.len(), block.new_rows.len()))
	}

	const fn diff_mode_label(&self) -> &'static str {
		if self.compact_projection {
			"inline"
		} else {
			"unified"
		}
	}

	fn copy_selection(&self) {
		if !self.projection_is_drawn() {
			return;
		}
		if let Some(diff) = &self.diff {
			let mut lines_to_copy = Vec::new();
			let mut copied_blocks = HashSet::new();
			for (index, row) in self.display_rows.iter().enumerate() {
				if !self.selection.contains(index) {
					continue;
				}
				if let Some((block_index, _)) = row.block {
					if copied_blocks.insert(block_index) {
						lines_to_copy.push(
							self.block_replacements[block_index]
								.metadata
								.new_text
								.clone(),
						);
					}
					continue;
				}
				let Some(raw) = row.raw_rows.last().copied() else {
					continue;
				};
				let source = &self.rendered_rows[raw];
				let line = &diff.hunks[source.hunk_index].lines
					[source.line_index];
				lines_to_copy.push(
					line.content
						.trim_matches(|c| c == '\n' || c == '\r')
						.to_owned(),
				);
			}

			try_or_popup!(
				self,
				"copy to clipboard error:",
				crate::clipboard::copy_string(
					&lines_to_copy.join("\n")
				)
			);
		}
	}

	#[cfg(test)]
	fn find_selected_hunk(
		diff: &FileDiff,
		line_selected: usize,
	) -> Option<usize> {
		let mut line_cursor = 0_usize;
		for (i, hunk) in diff.hunks.iter().enumerate() {
			let hunk_len = hunk.lines.len();
			let hunk_min = line_cursor;
			let hunk_max = line_cursor + hunk_len;

			let hunk_selected =
				hunk_min <= line_selected && hunk_max > line_selected;

			if hunk_selected {
				return Some(i);
			}

			line_cursor += hunk_len;
		}

		None
	}

	fn get_text(&self, width: u16, height: u16) -> Vec<Line<'_>> {
		if let Some(diff) = &self.diff {
			return if diff.hunks.is_empty() {
				self.get_text_binary(diff)
			} else {
				self.apply_highlight_result();
				let min = self
					.vertical_scroll
					.get_top()
					.min(self.display_rows.len());
				let max = min
					.saturating_add(usize::from(height))
					.min(self.display_rows.len());
				self.schedule_highlights(min, max);
				self.schedule_projection(min, max);
				let number_width = self.number_width;
				let highlights = self.row_highlights.borrow();
				let default_highlight = InlineHighlight::default();
				self.display_rows[min..max]
					.iter()
					.enumerate()
					.map(|(offset, row)| {
						let line_cursor = min + offset;
						let hunk_selected = self.focused()
							&& self.selected_hunk
								== Some(row.hunk_index);
						let config = DiffRowRenderConfig {
							number_width,
							highlight: Self::row_highlight(
								self.focused()
									&& self.selection_mode && self
									.selection
									.contains(line_cursor),
								self.focused()
									&& self.selection.get_end()
										== line_cursor,
							),
							hunk_selected,
							scrolled_right: self
								.horizontal_scroll
								.get_right(),
						};
						if row.fold.is_some() {
							return self.get_fold_line(width, row);
						}
						if row.replacement.is_empty() {
							let raw_index = row.raw_rows[0];
							let raw = &self.rendered_rows[raw_index];
							let line = &diff.hunks[raw.hunk_index]
								.lines[raw.line_index];
							Self::get_line_to_add(
								width,
								line,
								&raw.content,
								config,
								&self.theme,
								highlights
									.get(&raw_index)
									.unwrap_or(&default_highlight),
							)
						} else {
							Self::get_replacement_line_to_add(
								width,
								row,
								&self.rendered_rows,
								config,
								&highlights,
								&self.theme,
								diff,
							)
						}
					})
					.collect()
			};
		}

		vec![]
	}

	fn get_fold_line<'a>(
		&self,
		width: u16,
		row: &'a DisplayDiffRow,
	) -> Line<'a> {
		let Some(group) =
			row.fold.and_then(|index| self.fold_groups.get(index))
		else {
			return Line::default();
		};
		let position = group.raw_rows.first().and_then(|&raw| {
			let source = &self.rendered_rows[raw];
			let diff = self.diff.as_ref()?;
			Some(
				diff.hunks[source.hunk_index].lines
					[source.line_index]
					.position,
			)
		});
		let gutter = crate::ui::diff_presentation::format_line_gutter(
			position.and_then(|position| position.old_lineno),
			position.and_then(|position| position.new_lineno),
			self.number_width,
		);
		let gutter_width = UnicodeWidthStr::width(gutter.as_str());
		let content_width =
			usize::from(width).saturating_sub(gutter_width);
		let content = truncate_to_width(
			trim_offset(
				&row.content,
				self.horizontal_scroll.get_right(),
			),
			content_width,
		);
		Line::from(vec![
			Span::styled(gutter, self.theme.text(false, false)),
			Span::styled(
				content,
				self.theme.diff_line(DiffLineType::Header, false),
			),
		])
	}

	fn get_text_binary(&self, diff: &FileDiff) -> Vec<Line<'_>> {
		let is_positive = diff.size_delta >= 0;
		let delta_byte_size =
			ByteSize::b(diff.size_delta.unsigned_abs());
		let sign = if is_positive { "+" } else { "-" };
		vec![Line::from(vec![
			Span::raw(Cow::from("size: ")),
			Span::styled(
				Cow::from(format!("{}", ByteSize::b(diff.sizes.0))),
				self.theme.text(false, false),
			),
			Span::raw(Cow::from(" -> ")),
			Span::styled(
				Cow::from(format!("{}", ByteSize::b(diff.sizes.1))),
				self.theme.text(false, false),
			),
			Span::raw(Cow::from(" (")),
			Span::styled(
				Cow::from(format!("{sign}{delta_byte_size:}")),
				self.theme.diff_line(
					if is_positive {
						DiffLineType::Add
					} else {
						DiffLineType::Delete
					},
					false,
				),
			),
			Span::raw(Cow::from(")")),
		])]
	}

	fn mouse_line_at(&self, mouse: MouseEvent) -> Option<usize> {
		let area = self.inner_rect.get();
		if !area.contains((mouse.column, mouse.row).into()) {
			return None;
		}
		let line = self.vertical_scroll.get_top()
			+ usize::from(mouse.row.saturating_sub(area.y));
		(line < self.lines_count()).then_some(line)
	}

	fn handle_mouse(&mut self, mouse: MouseEvent) -> EventState {
		if !self.focused() {
			return EventState::NotConsumed;
		}
		match mouse.kind {
			MouseEventKind::Down(MouseButton::Left) => {
				if !self.projection_is_drawn() {
					return EventState::Consumed;
				}
				let Some(line) = self.mouse_line_at(mouse) else {
					return EventState::NotConsumed;
				};
				if self.toggle_fold(line) {
					return EventState::Consumed;
				}
				self.update_selection(line);
				self.mouse_dragging = true;
				EventState::Consumed
			}
			MouseEventKind::Drag(MouseButton::Left)
				if self.mouse_dragging =>
			{
				if !self.projection_is_drawn() {
					return EventState::Consumed;
				}
				if let Some(line) = self.mouse_line_at(mouse) {
					let max = self.lines_count().saturating_sub(1);
					let line = cmp::min(max, line);
					self.selection = Selection::Multiple(
						self.selection_anchor,
						line,
					);
					self.selection_cursor = line;
					self.selection_mode = true;
					self.selection_extending = true;
					self.selection_is_hunk = false;
					self.selected_hunk = self
						.display_rows
						.get(line)
						.map(|row| row.hunk_index);
				}
				EventState::Consumed
			}
			MouseEventKind::Up(MouseButton::Left)
				if self.mouse_dragging =>
			{
				if !self.projection_is_drawn() {
					self.mouse_dragging = false;
					self.selection_extending = false;
					return EventState::Consumed;
				}
				if let Some(line) = self.mouse_line_at(mouse) {
					let max = self.lines_count().saturating_sub(1);
					let line = cmp::min(max, line);
					if line == self.selection_anchor {
						self.update_selection(line);
					} else {
						self.selection = Selection::Multiple(
							self.selection_anchor,
							line,
						);
						self.selection_cursor = line;
						self.selection_mode = true;
					}
				}
				self.selection_extending = false;
				self.mouse_dragging = false;
				EventState::Consumed
			}
			MouseEventKind::ScrollUp
				if self
					.inner_rect
					.get()
					.contains((mouse.column, mouse.row).into()) =>
			{
				self.vertical_scroll.move_top(ScrollType::Up);
				EventState::Consumed
			}
			MouseEventKind::ScrollDown
				if self
					.inner_rect
					.get()
					.contains((mouse.column, mouse.row).into()) =>
			{
				self.vertical_scroll.move_top(ScrollType::Down);
				EventState::Consumed
			}
			_ => EventState::NotConsumed,
		}
	}

	fn get_line_to_add<'a>(
		width: u16,
		line: &'a DiffLine,
		content: &'a str,
		config: DiffRowRenderConfig,
		theme: &SharedTheme,
		inline: &InlineHighlight,
	) -> Line<'a> {
		let style = Self::row_style(line, config, theme);
		let (mut spans, prefix_width) =
			Self::row_prefix(line, config, theme);
		let available =
			usize::from(width).saturating_sub(prefix_width);
		let visible = Self::visible_line_content(
			line,
			content,
			config.scrolled_right,
			available,
			theme,
		);
		let content_width = UnicodeWidthStr::width(visible.as_ref());
		let padding = if config.highlight.is_selected() {
			available.saturating_sub(content_width)
		} else {
			0
		};
		spans.extend(Self::content_spans(
			visible,
			content,
			line,
			inline,
			style,
			config.highlight,
			theme,
		));
		if padding > 0 {
			spans.push(Span::styled(
				Cow::Owned(" ".repeat(padding)),
				style,
			));
		}
		spans.push(Span::styled(Cow::Borrowed("\n"), style));
		Line::from(spans)
	}

	#[allow(clippy::too_many_lines)] // Rendering one projected row keeps gutter, clipping, and source spans in the same geometry.
	fn get_replacement_line_to_add<'a>(
		width: u16,
		row: &'a DisplayDiffRow,
		raw_rows: &[RenderedDiffRow],
		config: DiffRowRenderConfig,
		highlights: &HashMap<usize, InlineHighlight>,
		theme: &SharedTheme,
		diff: &FileDiff,
	) -> Line<'a> {
		use asyncgit::diff_preview::InlineSegmentKind as Kind;
		let old_raw = row.raw_rows[0];
		let new_raw = row.raw_rows[1];
		let old_source = &raw_rows[old_raw];
		let new_source = &raw_rows[new_raw];
		let old_content_line = &diff.hunks[old_source.hunk_index]
			.lines[old_source.line_index];
		let new_content_line = &diff.hunks[new_source.hunk_index]
			.lines[new_source.line_index];
		let (old_lineno, new_lineno) = row.block_gutter.unwrap_or((
			old_content_line.position.old_lineno,
			new_content_line.position.new_lineno,
		));
		let mut spans = Vec::new();
		let gutter_width =
			crate::ui::diff_presentation::line_gutter_width(
				config.number_width,
			);
		spans.push(Span::styled(
			Cow::Owned(
				crate::ui::diff_presentation::format_line_gutter(
					old_lineno,
					new_lineno,
					config.number_width,
				),
			),
			theme.diff_line_gutter_cursor(
				config.highlight.is_selected(),
				config.highlight.is_cursor(),
			),
		));
		let prefix_width = gutter_width;
		let available =
			usize::from(width).saturating_sub(prefix_width);
		let visible = truncate_to_width(
			trim_offset(&row.content, config.scrolled_right),
			available,
		);
		let visible_start = (visible.as_ptr() as usize)
			.saturating_sub(row.content.as_ptr() as usize);
		let visible_end = visible_start.saturating_add(visible.len());
		let mut inline = InlineHighlight::default();
		for segment in &row.replacement {
			if segment.kind != Kind::Shared {
				inline.changed.push(segment.range.clone());
			}
			if let Some(source) = highlights.get(&segment.source_row)
			{
				for syntax in &source.syntax {
					let start = syntax
						.range
						.start
						.max(segment.source_range.start);
					let end = syntax
						.range
						.end
						.min(segment.source_range.end);
					if start < end {
						let offset =
							start - segment.source_range.start;
						inline.syntax.push(
							crate::ui::diff_highlight::StyledRange {
								range: segment.range.start + offset
									..segment.range.start
										+ offset + (end - start),
								style: syntax.style,
							},
						);
					}
				}
			}
		}
		for segment in &row.replacement {
			let start = segment.range.start.max(visible_start);
			let end = segment.range.end.min(visible_end);
			if start >= end {
				continue;
			}
			let line_type = match segment.kind {
				Kind::Shared => DiffLineType::None,
				Kind::Removed => DiffLineType::Delete,
				Kind::Added => DiffLineType::Add,
			};
			let base = if config.highlight.is_cursor() {
				theme.diff_line_cursor(
					line_type,
					config.highlight.is_selected(),
				)
			} else {
				theme.diff_line(
					line_type,
					config.highlight.is_selected(),
				)
			};
			let mut piece_spans = Self::replacement_content_spans(
				&row.content,
				start..end,
				line_type,
				&inline,
				base,
				config.highlight,
				theme,
			);
			spans.append(&mut piece_spans);
		}
		let content_width = UnicodeWidthStr::width(visible);
		if config.highlight.is_selected() && available > content_width
		{
			spans.push(Span::styled(
				Cow::Owned(" ".repeat(available - content_width)),
				theme.diff_line_gutter_cursor(
					true,
					config.highlight.is_cursor(),
				),
			));
		}
		spans.push(Span::raw(Cow::Borrowed("\n")));
		Line::from(spans)
	}

	fn replacement_content_spans<'a>(
		full: &'a str,
		range: std::ops::Range<usize>,
		line_type: DiffLineType,
		inline: &InlineHighlight,
		base: ratatui::style::Style,
		highlight: DiffRowHighlight,
		theme: &SharedTheme,
	) -> Vec<Span<'a>> {
		let mut boundaries = vec![range.start, range.end];
		for syntax in &inline.syntax {
			let start = syntax.range.start.max(range.start);
			let end = syntax.range.end.min(range.end);
			if start < end {
				boundaries.extend([start, end]);
			}
		}
		boundaries.sort_unstable();
		boundaries.dedup();
		boundaries
			.windows(2)
			.filter_map(|boundary| {
				let start = boundary[0];
				let end = boundary[1];
				(start < end).then(|| {
					let changed = line_type == DiffLineType::Add
						|| line_type == DiffLineType::Delete;
					let syntax =
						inline.syntax.iter().find(|syntax| {
							syntax.range.start <= start
								&& start < syntax.range.end
						});
					let mut style = if changed {
						theme.diff_inline_emphasis(
							line_type,
							syntax.and_then(|style| style.style.fg),
							highlight.is_selected(),
							highlight.is_cursor(),
						)
					} else {
						base
					};
					match line_type {
						DiffLineType::Delete => {
							style = style
								.remove_modifier(ratatui::style::Modifier::UNDERLINED)
								.add_modifier(ratatui::style::Modifier::CROSSED_OUT);
						}
						DiffLineType::Add => {
							style = style.remove_modifier(
								ratatui::style::Modifier::CROSSED_OUT,
							);
						}
						_ => {}
					}
					if let Some(syntax) = syntax {
						style = if changed {
							style.add_modifier(
								syntax.style.add_modifier,
							)
						} else {
							theme.selected(
								base.patch(syntax.style),
								highlight.is_selected(),
							)
						};
					}
					Span::styled(&full[start..end], style)
				})
			})
			.collect()
	}

	fn content_spans<'a>(
		visible: Cow<'a, str>,
		full: &'a str,
		line: &DiffLine,
		inline: &InlineHighlight,
		base: ratatui::style::Style,
		highlight: DiffRowHighlight,
		theme: &SharedTheme,
	) -> Vec<Span<'a>> {
		if visible.is_empty() {
			return vec![Span::styled(visible, base)];
		}
		let visible_ref = visible.as_ref();
		let Some(start) = (visible_ref.as_ptr() as usize)
			.checked_sub(full.as_ptr() as usize)
			.filter(|start| {
				start.saturating_add(visible_ref.len()) <= full.len()
					&& full.get(*start..start + visible_ref.len())
						== Some(visible_ref)
			})
		else {
			return vec![Span::styled(visible, base)];
		};
		let end = start + visible_ref.len();
		let mut boundaries = vec![start, end];
		for range in inline
			.changed
			.iter()
			.chain(inline.syntax.iter().map(|range| &range.range))
		{
			let clipped_start = range.start.max(start);
			let clipped_end = range.end.min(end);
			if clipped_start < clipped_end {
				boundaries.push(clipped_start);
				boundaries.push(clipped_end);
			}
		}
		boundaries.sort_unstable();
		boundaries.dedup();
		boundaries
			.windows(2)
			.filter_map(|boundary| {
				let segment_start = boundary[0];
				let segment_end = boundary[1];
				(segment_start < segment_end).then(|| {
					let locally_changed =
						inline.changed.iter().any(|range| {
							range.start <= segment_start
								&& segment_start < range.end
						});
					let syntax = inline.syntax.iter().find(|range| {
						range.range.start <= segment_start
							&& segment_start < range.range.end
					});
					let mut style = if locally_changed {
						theme.diff_inline_emphasis(
							line.line_type,
							syntax.and_then(|range| range.style.fg),
							highlight.is_selected(),
							highlight.is_cursor(),
						)
					} else {
						base
					};
					if let Some(syntax) = syntax {
						style = if locally_changed {
							style.add_modifier(
								syntax.style.add_modifier,
							)
						} else {
							theme.selected(
								base.patch(syntax.style),
								highlight.is_selected(),
							)
						};
					}
					if !locally_changed
						&& !inline.changed.is_empty()
						&& matches!(
							line.line_type,
							DiffLineType::Add | DiffLineType::Delete
						) {
						style = style.add_modifier(
							ratatui::style::Modifier::DIM,
						);
					}
					Span::styled(
						&full[segment_start..segment_end],
						style,
					)
				})
			})
			.collect()
	}

	fn row_style(
		line: &DiffLine,
		config: DiffRowRenderConfig,
		theme: &SharedTheme,
	) -> ratatui::style::Style {
		if line.line_type == DiffLineType::Header {
			return if config.hunk_selected {
				theme
					.diff_hunk_marker(true)
					.add_modifier(ratatui::style::Modifier::BOLD)
			} else {
				theme.diff_line(line.line_type, false)
			};
		}
		if config.highlight.is_cursor() {
			theme.diff_line_cursor(
				line.line_type,
				config.highlight.is_selected(),
			)
		} else {
			theme.diff_line(
				line.line_type,
				config.highlight.is_selected(),
			)
		}
	}

	fn row_prefix(
		line: &DiffLine,
		config: DiffRowRenderConfig,
		theme: &SharedTheme,
	) -> (Vec<Span<'static>>, usize) {
		if line.line_type == DiffLineType::Header {
			let marker =
				if config.hunk_selected { "› " } else { "  " };
			let marker_style =
				theme.diff_hunk_marker(config.hunk_selected);
			return (
				vec![Span::styled(Cow::from(marker), marker_style)],
				2,
			);
		}

		let gutter = crate::ui::diff_presentation::format_line_gutter(
			line.position.old_lineno,
			line.position.new_lineno,
			config.number_width,
		);
		let gutter_width = UnicodeWidthStr::width(gutter.as_str());
		(
			vec![Span::styled(
				Cow::Owned(gutter),
				theme.diff_line_gutter_cursor(
					config.highlight.is_selected(),
					config.highlight.is_cursor(),
				),
			)],
			gutter_width,
		)
	}

	fn visible_line_content<'a>(
		line: &DiffLine,
		content: &'a str,
		scrolled_right: usize,
		available_width: usize,
		theme: &SharedTheme,
	) -> Cow<'a, str> {
		if line.line_type != DiffLineType::None && content.is_empty()
		{
			let line_break = theme.line_break();
			let suffix = trim_offset(&line_break, scrolled_right);
			Cow::Owned(
				truncate_to_width(suffix, available_width).to_owned(),
			)
		} else {
			let suffix = trim_offset(content, scrolled_right);
			Cow::Borrowed(truncate_to_width(suffix, available_width))
		}
	}

	const fn row_highlight(
		selected: bool,
		cursor: bool,
	) -> DiffRowHighlight {
		match (selected, cursor) {
			(true, true) => DiffRowHighlight::CursorSelected,
			(true, false) => DiffRowHighlight::Selected,
			(false, true) => DiffRowHighlight::Cursor,
			(false, false) => DiffRowHighlight::Normal,
		}
	}

	fn add_reset_commands(
		&self,
		out: &mut Vec<CommandInfo>,
		has_selected_changes: bool,
	) {
		let modal = self.key_config.is_modal();
		let reset_keys_overlap =
			self.key_config.keys.status_reset_item
				== self.key_config.keys.diff_reset_lines;
		let hunk_action_enabled =
			self.selected_hunk.is_some_and(|hunk| {
				!self.hunk_contains_hidden_changes(hunk)
			});
		if modal && reset_keys_overlap {
			let reset = if self.selection_is_hunk {
				strings::commands::diff_hunk_revert(&self.key_config)
			} else {
				strings::commands::diff_lines_revert(&self.key_config)
			};
			out.push(CommandInfo::new(
				reset,
				if self.selection_is_hunk {
					hunk_action_enabled
				} else {
					has_selected_changes
				},
				self.focused() && !self.is_stage(),
			));
		} else {
			out.push(CommandInfo::new(
				strings::commands::diff_hunk_revert(&self.key_config),
				hunk_action_enabled,
				self.focused() && !self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_lines_revert(
					&self.key_config,
				),
				has_selected_changes,
				self.focused() && !self.is_stage(),
			));
		}
	}

	fn unstage_hunk(&self) -> Result<()> {
		if !self.projection_is_drawn()
			|| self.selection_contains_fold()
			|| self.selected_hunk.is_some_and(|hunk| {
				self.hunk_contains_hidden_changes(hunk)
			}) {
			return Ok(());
		}
		if let Some(diff) = &self.diff {
			if let Some(hunk) = self.selected_hunk {
				let hash = diff.hunks[hunk].header_hash;
				sync::unstage_hunk_checked(
					&self.repo.borrow(),
					&self.current.path,
					hash,
					Some(self.options.borrow().diff_options()),
					diff,
				)?;
				self.queue_update();
			}
		}

		Ok(())
	}

	fn stage_hunk(&self) -> Result<()> {
		if !self.projection_is_drawn()
			|| self.selection_contains_fold()
			|| self.selected_hunk.is_some_and(|hunk| {
				self.hunk_contains_hidden_changes(hunk)
			}) {
			return Ok(());
		}
		if let Some(diff) = &self.diff {
			if let Some(hunk) = self.selected_hunk {
				if diff.untracked {
					sync::stage_file_checked(
						&self.repo.borrow(),
						&self.current.path,
						Some(self.options.borrow().diff_options()),
						diff,
					)?;
				} else {
					let hash = diff.hunks[hunk].header_hash;
					sync::stage_hunk_checked(
						&self.repo.borrow(),
						&self.current.path,
						hash,
						Some(self.options.borrow().diff_options()),
						diff,
					)?;
				}

				self.queue_update();
			}
		}

		Ok(())
	}

	fn queue_update(&self) {
		self.queue.push(InternalEvent::Update(NeedsUpdate::ALL));
	}

	fn reset_hunk(&self) {
		if !self.projection_is_drawn()
			|| self.selection_contains_fold()
			|| self.selected_hunk.is_some_and(|hunk| {
				self.hunk_contains_hidden_changes(hunk)
			}) {
			return;
		}
		if let Some(diff) = &self.diff {
			if let Some(hunk) = self.selected_hunk {
				let hash = diff.hunks[hunk].header_hash;

				self.queue.push(InternalEvent::ConfirmAction(
					Action::ResetHunk(
						self.current.path.clone(),
						hash,
						diff.clone(),
					),
				));
			}
		}
	}

	fn reset_lines(&self) {
		if !self.projection_is_drawn() {
			return;
		}
		let selected_lines = self.selected_lines();
		if !selected_lines.is_empty() {
			if let Some(diff) = &self.diff {
				let pairs =
					self.selected_replacements(&selected_lines);
				if !pairs.is_empty() {
					self.queue.push(InternalEvent::ConfirmAction(
						Action::ResetLinesWithPairs(
							self.current.path.clone(),
							selected_lines,
							pairs,
							diff.clone(),
						),
					));
					return;
				}
				self.queue.push(InternalEvent::ConfirmAction(
					Action::ResetLines(
						self.current.path.clone(),
						selected_lines,
						diff.clone(),
					),
				));
			}
		}
	}

	fn stage_lines(&self) {
		if !self.projection_is_drawn() {
			return;
		}
		if let Some(diff) = &self.diff {
			let selected_lines = self.selected_lines();
			if selected_lines.is_empty() {
				return;
			}
			let pairs = self.selected_replacements(&selected_lines);

			try_or_popup!(
				self,
				"(un)stage lines:",
				if pairs.is_empty() {
					sync::stage_lines_checked(
						&self.repo.borrow(),
						&self.current.path,
						self.is_stage(),
						&selected_lines,
						diff,
					)
				} else {
					sync::stage_lines_with_pairs_checked(
						&self.repo.borrow(),
						&self.current.path,
						self.is_stage(),
						&selected_lines,
						&pairs,
						diff,
					)
				}
			);

			self.queue_update();
		}
	}

	fn selected_lines(&self) -> Vec<DiffLinePosition> {
		let Some(diff) = &self.diff else {
			return Vec::new();
		};
		if self.selection_contains_fold() {
			return Vec::new();
		}
		let mut selected = HashSet::new();
		for (display_index, row) in
			self.display_rows.iter().enumerate()
		{
			if self.selection.contains(display_index) {
				selected.extend(row.raw_rows.iter().copied());
			}
		}
		let mut selected = selected.into_iter().collect::<Vec<_>>();
		selected.sort_unstable();
		selected
			.into_iter()
			.filter_map(|raw_index| {
				let row = &self.rendered_rows[raw_index];
				let line =
					&diff.hunks[row.hunk_index].lines[row.line_index];
				matches!(
					line.line_type,
					DiffLineType::Add | DiffLineType::Delete
				)
				.then_some(line.position)
			})
			.collect()
	}

	fn selected_replacements(
		&self,
		selected_lines: &[DiffLinePosition],
	) -> Vec<DiffLineReplacement> {
		let selected =
			selected_lines.iter().copied().collect::<HashSet<_>>();
		let pairs = self.projection_pairs.borrow();
		let Some(diff) = &self.diff else {
			return Vec::new();
		};
		let mut replacements = Vec::new();
		for (display_index, row) in
			self.display_rows.iter().enumerate()
		{
			if !self.selection.contains(display_index)
				|| row.block.is_some()
				|| row.replacement.is_empty()
			{
				continue;
			}
			let Some(&old_raw) = row.raw_rows.first() else {
				continue;
			};
			let Some(pair) = pairs.get(&old_raw) else {
				continue;
			};
			let old_source = &self.rendered_rows[old_raw];
			let new_source = &self.rendered_rows[pair.new_row];
			let old = diff.hunks[old_source.hunk_index].lines
				[old_source.line_index]
				.position;
			let new = diff.hunks[new_source.hunk_index].lines
				[new_source.line_index]
				.position;
			if selected.contains(&old) && selected.contains(&new) {
				replacements.push(DiffLineReplacement { old, new });
			}
		}
		replacements
	}

	fn selection_contains_fold(&self) -> bool {
		self.display_rows.iter().enumerate().any(|(index, row)| {
			self.selection.contains(index)
				&& row.fold.is_some_and(|group_index| {
					self.fold_groups.get(group_index).is_some_and(
						|group| {
							self.folded_regions.contains(&group.id)
						},
					)
				})
		})
	}

	fn hunk_contains_hidden_changes(&self, hunk: usize) -> bool {
		self.fold_groups.iter().any(|group| {
			self.folded_regions.contains(&group.id)
				&& group.additions + group.deletions > 0
				&& group.raw_rows.iter().any(|&raw| {
					self.rendered_rows[raw].hunk_index == hunk
				})
		})
	}

	fn reset_untracked(&self) {
		self.queue.push(InternalEvent::ConfirmAction(Action::Reset(
			ResetItem {
				path: self.current.path.clone(),
			},
		)));
	}

	fn stage_unstage_hunk(&self) -> Result<()> {
		if self.current.is_stage {
			self.unstage_hunk()?;
		} else {
			self.stage_hunk()?;
		}

		Ok(())
	}

	fn calc_hunk_move_target(
		&self,
		direction: isize,
	) -> Option<usize> {
		let diff = self.diff.as_ref()?;
		if diff.hunks.is_empty() {
			return None;
		}
		let max = diff.hunks.len() - 1;
		let target_index = self.selected_hunk.map_or(0, |i| {
			let target = if direction >= 0 {
				i.saturating_add(direction.unsigned_abs())
			} else {
				i.saturating_sub(direction.unsigned_abs())
			};
			std::cmp::min(max, target)
		});
		Some(target_index)
	}

	fn diff_hunk_move_up_down(&mut self, direction: isize) {
		if self.diff.is_none() {
			return;
		}
		let hunk_index = self.calc_hunk_move_target(direction);
		// return if selected_hunk not change
		if self.selected_hunk == hunk_index {
			return;
		}
		if let Some(hunk_index) = hunk_index {
			let Some(line_index) = self
				.display_rows
				.iter()
				.position(|row| row.hunk_index == hunk_index)
			else {
				return;
			};
			let last = self
				.display_rows
				.iter()
				.rposition(|row| row.hunk_index == hunk_index)
				.unwrap_or(line_index);
			self.selection = Selection::Single(line_index);
			self.selection_anchor = line_index;
			self.selection_cursor = line_index;
			self.selection_is_hunk = false;
			self.selected_hunk = Some(hunk_index);
			self.vertical_scroll.move_area_to_visible(
				self.current_size.get().1 as usize,
				line_index,
				last.saturating_add(1),
			);
		}
	}

	const fn is_stage(&self) -> bool {
		self.current.is_stage
	}
}

impl DrawableComponent for DiffComponent {
	fn draw(&self, f: &mut Frame, r: Rect) -> Result<()> {
		self.apply_highlight_result();
		let inner = Rect::new(
			r.x.saturating_add(1),
			r.y.saturating_add(1),
			r.width.saturating_sub(2),
			r.height.saturating_sub(2),
		);
		self.inner_rect.set(inner);
		self.current_size.set((inner.width, inner.height));

		let current_width = self.current_size.get().0;
		let current_height = self.current_size.get().1;

		self.vertical_scroll.update_bounds(
			self.lines_count(),
			usize::from(current_height),
		);

		self.horizontal_scroll.update_no_selection(
			self.longest_line,
			self.text_width(),
		);

		let title = format!(
			"{}{}{}{}{}",
			strings::title_diff(&self.key_config),
			crate::string_utils::sanitize_control_chars(
				&self.current.path,
			),
			if self
				.diff
				.as_ref()
				.is_some_and(|diff| !diff.hunks.is_empty())
			{
				format!(
					"  {} · {}",
					self.language_label.borrow(),
					self.diff_mode_label()
				)
			} else {
				String::new()
			},
			self.selection_status_inner().map_or_else(
				String::new,
				|s| format!(
					"  [{} {}{}]",
					s.count,
					s.unit,
					if s.extending { " · extending" } else { "" }
				)
			),
			self.active_paragraph_scope().map_or_else(
				String::new,
				|(old, new)| {
					format!(
						"  Paragraph · {old} old / {new} new lines"
					)
				},
			),
		);

		let txt = if self.pending {
			vec![Line::from(vec![Span::styled(
				Cow::from(strings::loading_text(&self.key_config)),
				self.theme.text(false, false),
			)])]
		} else {
			self.get_text(current_width, current_height)
		};

		f.render_widget(
			Paragraph::new(txt).block(
				Block::default()
					.title(Span::styled(
						title.as_str(),
						self.theme.title(self.focused()),
					))
					.borders(Borders::ALL)
					.border_style(self.theme.block(self.focused())),
			),
			r,
		);
		if !self.pending && self.diff.is_some() {
			self.drawn_generation
				.set(self.projection_generation.get());
		}

		if self.focused() {
			self.vertical_scroll.draw(f, r, &self.theme);

			if self.max_scroll_right() > 0 {
				self.horizontal_scroll.draw(f, r, &self.theme);
			}
		}

		Ok(())
	}
}

impl Component for DiffComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		_force_all: bool,
	) -> CommandBlocking {
		out.push(CommandInfo::new(
			strings::commands::scroll(&self.key_config),
			self.can_scroll(),
			self.focused(),
		));
		out.push(CommandInfo::new(
			strings::commands::diff_hunk_next(&self.key_config),
			self.calc_hunk_move_target(1) != self.selected_hunk,
			self.focused(),
		));
		out.push(CommandInfo::new(
			strings::commands::diff_hunk_prev(&self.key_config),
			self.calc_hunk_move_target(-1) != self.selected_hunk,
			self.focused(),
		));
		out.push(
			CommandInfo::new(
				strings::commands::diff_home_end(&self.key_config),
				self.can_scroll(),
				self.focused(),
			)
			.hidden(),
		);

		if !self.is_immutable {
			let has_selected_changes =
				!self.selected_lines().is_empty();
			let modal = self.key_config.is_modal();
			out.push(CommandInfo::new(
				strings::commands::edit_item(&self.key_config),
				self.can_edit_file(),
				self.focused() && self.can_edit_file(),
			));
			if !modal {
				let hunk_action_enabled =
					self.selected_hunk.is_some_and(|hunk| {
						!self.hunk_contains_hidden_changes(hunk)
					});
				out.push(CommandInfo::new(
					strings::commands::diff_hunk_remove(
						&self.key_config,
					),
					hunk_action_enabled,
					self.focused() && self.is_stage(),
				));
				out.push(CommandInfo::new(
					strings::commands::diff_hunk_add(
						&self.key_config,
					),
					hunk_action_enabled,
					self.focused() && !self.is_stage(),
				));
			}
			self.add_reset_commands(out, has_selected_changes);
			out.push(CommandInfo::new(
				strings::commands::diff_lines_stage(&self.key_config),
				has_selected_changes,
				self.focused() && !self.is_stage(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_lines_unstage(
					&self.key_config,
				),
				has_selected_changes,
				self.focused() && self.is_stage(),
			));
		}

		out.push(CommandInfo::new(
			strings::commands::copy(&self.key_config),
			true,
			self.focused(),
		));

		CommandBlocking::PassingOn
	}

	#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		self.refresh_native_block_layout();
		if let Event::Mouse(mouse) = ev {
			return Ok(self.handle_mouse(*mouse));
		}
		if self.focused() {
			if let Event::Key(e) = ev {
				if !self.projection_is_drawn() {
					if key_match(e, self.key_config.keys.move_down) {
						self.move_selection(ScrollType::Down);
					} else if key_match(
						e,
						self.key_config.keys.move_up,
					) {
						self.move_selection(ScrollType::Up);
					} else if key_match(
						e,
						self.key_config.keys.page_up,
					) {
						self.move_selection(ScrollType::PageUp);
					} else if key_match(
						e,
						self.key_config.keys.page_down,
					) {
						self.move_selection(ScrollType::PageDown);
					} else if key_match(e, self.key_config.keys.home)
					{
						self.move_selection(ScrollType::Home);
					} else if key_match(e, self.key_config.keys.end) {
						self.move_selection(ScrollType::End);
					}
					return Ok(EventState::Consumed);
				}
				if self.key_config.is_modal() {
					if key_match(e, self.key_config.keys.select_mode)
					{
						self.toggle_selection_mode();
						return Ok(EventState::Consumed);
					}
					if key_match(e, self.key_config.keys.select_clear)
						&& self.clear_selection()
					{
						return Ok(EventState::Consumed);
					}
					if key_match(e, self.key_config.keys.select_all) {
						self.select_all();
						return Ok(EventState::Consumed);
					}
					if key_match(e, self.key_config.keys.select_item)
					{
						self.select_hunk();
						return Ok(EventState::Consumed);
					}
				}
				if key_match(e, self.key_config.keys.enter)
					&& self.toggle_fold(self.selection_cursor)
				{
					return Ok(EventState::Consumed);
				}
				return if key_match(e, self.key_config.keys.move_down)
				{
					self.move_selection(ScrollType::Down);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.shift_down,
				) {
					self.modify_selection(Direction::Down);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.shift_up)
				{
					self.modify_selection(Direction::Up);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.end) {
					self.move_selection(ScrollType::End);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.home) {
					self.move_selection(ScrollType::Home);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.move_up) {
					self.move_selection(ScrollType::Up);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.page_up) {
					self.move_selection(ScrollType::PageUp);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.page_down)
				{
					self.move_selection(ScrollType::PageDown);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.move_right,
				) {
					self.horizontal_scroll
						.move_right(HorizontalScrollType::Right);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.move_left)
				{
					self.horizontal_scroll
						.move_right(HorizontalScrollType::Left);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_hunk_next,
				) {
					self.diff_hunk_move_up_down(1);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_hunk_prev,
				) {
					self.diff_hunk_move_up_down(-1);
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.edit_file)
					&& self.can_edit_file()
				{
					self.queue.push(
						InternalEvent::OpenExternalEditor(Some(
							self.current.path.clone(),
						)),
					);
					Ok(EventState::Consumed)
				} else if !self.is_immutable
					&& ((self.key_config.is_modal()
						&& key_match(e, self.key_config.keys.enter))
						|| (self.is_stage()
							&& key_match(
								e,
								self.key_config
									.keys
									.status_unstage_item,
							))) {
					self.stage_lines();
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.stage_unstage_item,
				) && !self.is_immutable
				{
					if self.key_config.is_modal() {
						self.stage_lines();
					} else {
						try_or_popup!(
							self,
							"hunk error:",
							self.stage_unstage_hunk()
						);
					}

					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.status_reset_item,
				) && !self.is_immutable
					&& !self.is_stage()
				{
					if let Some(diff) = &self.diff {
						if diff.untracked {
							if !self.key_config.is_modal()
								|| self.selection_is_hunk
							{
								self.reset_untracked();
							}
						} else if self.key_config.is_modal()
							&& !self.selection_is_hunk
						{
							self.reset_lines();
						} else {
							self.reset_hunk();
						}
					}
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_stage_lines,
				) && !self.is_immutable
				{
					self.stage_lines();
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.diff_reset_lines,
				) && !self.is_immutable
					&& !self.is_stage()
				{
					if let Some(diff) = &self.diff {
						//TODO: reset untracked lines
						if !diff.untracked {
							self.reset_lines();
						}
					}
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.copy) {
					self.copy_selection();
					Ok(EventState::Consumed)
				} else {
					Ok(EventState::NotConsumed)
				};
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn focused(&self) -> bool {
		self.focused
	}
	fn focus(&mut self, focus: bool) {
		self.focused = focus;
		if !focus {
			self.selection_mode = false;
			self.selection_extending = false;
			self.selection_is_hunk = false;
			self.mouse_dragging = false;
			if let Selection::Multiple(_, end) = self.selection {
				self.selection = Selection::Single(end);
				self.selection_anchor = end;
				self.selection_cursor = end;
			}
			self.selected_hunk = self
				.display_rows
				.get(self.selection_cursor)
				.map(|row| row.hunk_index);
		}
	}

	fn selection_status(&self) -> Option<SelectionStatus> {
		self.selection_status_inner()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		app::Environment,
		keys::{KeyConfig, Keymap},
		queue::{Action, InternalEvent},
		ui::style::Theme,
	};
	use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
	use crossterm::event::{MouseEvent, MouseEventKind};
	use ratatui::{backend::TestBackend, Terminal};
	use std::rc::Rc;
	use std::sync::atomic::AtomicBool;
	use std::{cell::RefCell, fs, io::Write, process::Command};
	use tempfile::NamedTempFile;

	#[test]
	fn native_diff_title_tracks_inline_and_unified_modes() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		assert_eq!(component.diff_mode_label(), "inline");
		component.compact_projection = false;
		assert_eq!(component.diff_mode_label(), "unified");
	}

	#[test]
	fn prose_block_projection_wraps_without_losing_atomic_raw_members(
	) {
		let old = [
			"Gitui keeps the shared paragraph wording readable",
			"when several lines are rewritten in the document",
		];
		let new = [
			"Gitui keeps the same shared paragraph wording readable",
			"after several lines are changed throughout the document",
		];
		let cancel = AtomicBool::new(false);
		let metadata = asyncgit::diff_preview::inline_diff_block(
			&old.iter()
				.enumerate()
				.map(|(index, text)| {
					asyncgit::diff_preview::InlineBlockLine {
						text,
						line_number: Some(
							u32::try_from(index + 1).unwrap(),
						),
						patch_index: index + 1,
					}
				})
				.collect::<Vec<_>>(),
			&new.iter()
				.enumerate()
				.map(|(index, text)| {
					asyncgit::diff_preview::InlineBlockLine {
						text,
						line_number: Some(
							u32::try_from(index + 1).unwrap(),
						),
						patch_index: index + 3,
					}
				})
				.collect::<Vec<_>>(),
			&cancel,
		)
		.expect("shared prose block should align");
		let mut lines = Vec::new();
		for (index, content) in old.iter().enumerate() {
			lines.push(DiffLine {
				content: (*content).into(),
				line_type: DiffLineType::Delete,
				position: DiffLinePosition {
					old_lineno: Some(
						u32::try_from(index + 1).unwrap(),
					),
					..Default::default()
				},
			});
		}
		for (index, content) in new.iter().enumerate() {
			lines.push(DiffLine {
				content: (*content).into(),
				line_type: DiffLineType::Add,
				position: DiffLinePosition {
					new_lineno: Some(
						u32::try_from(index + 1).unwrap(),
					),
					..Default::default()
				},
			});
		}
		let diff = FileDiff {
			lines: lines.len(),
			hunks: vec![asyncgit::sync::diff::Hunk {
				header_hash: 1,
				lines,
			}],
			..Default::default()
		};
		let mut component =
			DiffComponent::new(&Environment::test_env(), false);
		component.diff = Some(diff);
		component.rebuild_render_cache();
		let source_index = build_block_source_index(
			&component.rendered_rows,
			component.diff.as_ref().unwrap(),
		);
		let native = map_native_block(
			&component.rendered_rows,
			component.diff.as_ref().unwrap(),
			metadata.clone(),
			&source_index,
		)
		.expect(
			"exact block provenance maps to a contiguous patch run",
		);
		component.block_replacements.push(native);
		component.current_size.set((22, 10));
		component.rebuild_display_rows();
		assert!(component.display_rows.len() > new.len());
		assert!(component.display_rows.iter().all(|row| {
			row.block.is_some()
				&& row.raw_rows.as_ref() == [0, 1, 2, 3]
		}));
		let displayed = component
			.display_rows
			.iter()
			.map(|row| row.content.as_str())
			.collect::<Vec<_>>()
			.join(" ");
		assert_eq!(displayed.matches("shared").count(), 1);
		let middle = component.display_rows.len() / 2;
		component.selection_cursor = middle;
		component.selection_anchor = middle;
		component.selection = Selection::Single(middle);
		let cursor_anchor = component.row_anchor(middle).unwrap();
		component.current_size.set((42, 10));
		component.refresh_native_block_layout();
		assert_eq!(
			component.find_anchor(&cursor_anchor),
			Some(component.selection_cursor),
			"resizing must keep the same source word under the cursor",
		);
		component.selection = Selection::Multiple(
			0,
			component.display_rows.len().saturating_sub(1),
		);
		let selected = component.selected_lines();
		assert_eq!(selected.len(), 4);
		assert_eq!(selected[0].old_lineno, Some(1));
		assert_eq!(selected[1].old_lineno, Some(2));
		assert_eq!(selected[2].new_lineno, Some(1));
		assert_eq!(selected[3].new_lineno, Some(2));
		assert!(component
			.selected_replacements(&selected)
			.is_empty());
		component.selection_mode = true;
		component.selection = Selection::Single(0);
		assert_eq!(
			component.selection_status_inner(),
			Some(SelectionStatus {
				extending: false,
				count: 1,
				unit: "paragraph",
			})
		);
		component.selection_cursor = 0;
		assert_eq!(component.active_paragraph_scope(), Some((2, 2)));
		let raw_members =
			Arc::clone(&component.display_rows[0].raw_rows);
		let wrapped_row = || DisplayDiffRow {
			hunk_index: 0,
			raw_rows: Arc::clone(&raw_members),
			content: String::new(),
			replacement: Vec::new(),
			fold: None,
			block: Some((0, 0)),
			block_gutter: None,
			block_sources: Vec::new(),
		};
		component.display_rows = vec![
			wrapped_row(),
			wrapped_row(),
			DisplayDiffRow {
				hunk_index: 0,
				raw_rows: Arc::from([4]),
				content: String::new(),
				replacement: Vec::new(),
				fold: None,
				block: None,
				block_gutter: None,
				block_sources: Vec::new(),
			},
		];
		component.selection = Selection::Multiple(0, 2);
		assert_eq!(
			component.selection_status_inner(),
			Some(SelectionStatus {
				extending: false,
				count: 5,
				unit: "patch lines",
			})
		);
	}

	#[test]
	fn test_line_break() {
		let diff_line = DiffLine {
			content: "".into(),
			line_type: DiffLineType::Add,
			position: Default::default(),
		};

		{
			let default_theme = Rc::new(Theme::default());

			assert_eq!(
				DiffComponent::get_line_to_add(
					8,
					&diff_line,
					"",
					DiffRowRenderConfig {
						number_width: 1,
						highlight: DiffRowHighlight::Normal,
						hunk_selected: false,
						scrolled_right: 0,
					},
					&default_theme,
					&InlineHighlight::default(),
				)
				.spans
				.iter()
				.map(|span| span.content.as_ref())
				.collect::<String>(),
				" │ │¶\n"
			);
			assert_eq!(
				DiffComponent::get_line_to_add(
					8,
					&diff_line,
					"",
					DiffRowRenderConfig {
						number_width: 1,
						highlight: DiffRowHighlight::Normal,
						hunk_selected: false,
						scrolled_right: 0,
					},
					&default_theme,
					&InlineHighlight::default(),
				)
				.spans
				.last()
				.unwrap()
				.style,
				Span::styled(
					Cow::from("\n"),
					default_theme
						.diff_line(diff_line.line_type, false)
				)
				.style
			);
		}

		{
			let mut file = NamedTempFile::new().unwrap();

			writeln!(
				file,
				r#"
(
	line_break: Some("+")
)
"#
			)
			.unwrap();

			let theme =
				Rc::new(Theme::init(&file.path().to_path_buf()));

			assert_eq!(
				DiffComponent::get_line_to_add(
					8,
					&diff_line,
					"",
					DiffRowRenderConfig {
						number_width: 1,
						highlight: DiffRowHighlight::Normal,
						hunk_selected: false,
						scrolled_right: 0,
					},
					&theme,
					&InlineHighlight::default(),
				)
				.spans
				.iter()
				.map(|span| span.content.as_ref())
				.collect::<String>(),
				" │ │+\n"
			);
			assert_eq!(
				DiffComponent::get_line_to_add(
					8,
					&diff_line,
					"",
					DiffRowRenderConfig {
						number_width: 1,
						highlight: DiffRowHighlight::Normal,
						hunk_selected: false,
						scrolled_right: 0,
					},
					&theme,
					&InlineHighlight::default(),
				)
				.spans
				.last()
				.unwrap()
				.style,
				Span::styled(
					Cow::from("\n"),
					theme.diff_line(diff_line.line_type, false)
				)
				.style
			);
		}
	}

	#[test]
	fn selection_ranges_are_inclusive_and_support_reverse_motion() {
		let selection = Selection::Multiple(7, 3);
		assert_eq!(selection.get_top(), 3);
		assert_eq!(selection.get_bottom(), 7);
		assert!(selection.contains(3));
		assert!(selection.contains(5));
		assert!(selection.contains(7));
		assert!(!selection.contains(2));
	}

	#[test]
	fn modal_reset_command_describes_the_current_selection_once() {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut component = DiffComponent::new(&env, false);
		component.update(
			"reset.txt".to_string(),
			false,
			synthetic_diff(["changed".to_string()]),
		);
		component.focus(true);

		let mut commands = Vec::new();
		component.commands(&mut commands, false);
		let reset = commands
			.iter()
			.filter(|command| command.text.name.starts_with("Reset "))
			.map(|command| command.text.name.as_str())
			.collect::<Vec<_>>();
		assert_eq!(reset.len(), 1);
		assert!(reset[0].starts_with("Reset lines"));

		component.select_hunk();
		commands.clear();
		component.commands(&mut commands, false);
		let reset = commands
			.iter()
			.filter(|command| command.text.name.starts_with("Reset "))
			.map(|command| command.text.name.as_str())
			.collect::<Vec<_>>();
		assert_eq!(reset.len(), 1);
		assert!(reset[0].starts_with("Reset hunk"));

		let mut legacy_env = Environment::test_env();
		legacy_env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Legacy));
		let mut legacy = DiffComponent::new(&legacy_env, false);
		legacy.update(
			"reset.txt".to_string(),
			false,
			synthetic_diff(["changed".to_string()]),
		);
		let mut legacy_commands = Vec::new();
		legacy.commands(&mut legacy_commands, false);
		let reset = legacy_commands
			.iter()
			.filter(|command| command.text.name.starts_with("Reset "))
			.map(|command| command.text.name.as_str())
			.collect::<Vec<_>>();
		assert_eq!(reset.len(), 2);
		assert!(reset
			.iter()
			.any(|name| name.starts_with("Reset hunk")));
		assert!(reset
			.iter()
			.any(|name| name.starts_with("Reset lines")));
	}

	fn synthetic_diff(
		lines: impl IntoIterator<Item = String>,
	) -> FileDiff {
		let lines = lines
			.into_iter()
			.enumerate()
			.map(|(index, content)| DiffLine {
				content: content.into(),
				line_type: DiffLineType::Add,
				position: DiffLinePosition {
					new_lineno: Some(
						u32::try_from(index + 1).unwrap(),
					),
					..Default::default()
				},
			})
			.collect::<Vec<_>>();
		let count = lines.len();
		FileDiff {
			hunks: vec![asyncgit::sync::diff::Hunk {
				header_hash: 1,
				lines,
			}],
			lines: count,
			..Default::default()
		}
	}

	fn draw_component_now(component: &DiffComponent) {
		let mut terminal =
			Terminal::new(TestBackend::new(80, 24)).unwrap();
		terminal
			.draw(|frame| {
				component.draw(frame, frame.area()).unwrap();
			})
			.unwrap();
	}

	fn display_index_for_raw(
		component: &DiffComponent,
		raw_index: usize,
	) -> usize {
		component
			.display_rows
			.iter()
			.position(|row| row.raw_rows.contains(&raw_index))
			.expect("raw patch row must appear in display projection")
	}

	fn draw_diff(
		component: &mut DiffComponent,
		terminal: &mut Terminal<TestBackend>,
	) {
		terminal
			.draw(|frame| {
				component.draw(frame, frame.area()).unwrap()
			})
			.unwrap();
	}

	fn wait_for_highlights(
		component: &DiffComponent,
		indexes: &[usize],
	) {
		let deadline = std::time::Instant::now()
			+ std::time::Duration::from_secs(5);
		while std::time::Instant::now() < deadline {
			component.apply_highlight_result();
			if indexes.iter().all(|index| {
				component
					.row_highlights
					.borrow()
					.get(index)
					.is_some_and(|highlight| {
						!highlight.changed.is_empty()
					})
			}) {
				return;
			}
			std::thread::sleep(std::time::Duration::from_millis(10));
		}
		panic!("highlight worker results were not applied before timeout");
	}

	fn wait_for_projection(component: &mut DiffComponent) {
		let deadline = std::time::Instant::now()
			+ std::time::Duration::from_secs(5);
		while std::time::Instant::now() < deadline {
			component.apply_projection_results();
			if !component.projection_pairs.borrow().is_empty() {
				return;
			}
			std::thread::sleep(std::time::Duration::from_millis(10));
		}
		panic!("projection worker results were not applied before timeout");
	}

	#[test]
	fn viewport_highlights_arrive_through_the_worker_result_pump() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		let lines = vec![
			DiffLine {
				content: "let count = 1;".into(),
				line_type: DiffLineType::Delete,
				position: DiffLinePosition {
					old_lineno: Some(1),
					..Default::default()
				},
			},
			DiffLine {
				content: "let count = 2;".into(),
				line_type: DiffLineType::Add,
				position: DiffLinePosition {
					new_lineno: Some(1),
					..Default::default()
				},
			},
		];
		component.update(
			"count.rs".to_owned(),
			false,
			FileDiff {
				hunks: vec![asyncgit::sync::diff::Hunk {
					header_hash: 1,
					lines,
				}],
				lines: 2,
				..Default::default()
			},
		);
		component.schedule_highlights(0, 1);
		wait_for_highlights(&component, &[0]);
		let highlights = component.row_highlights.borrow();
		assert_eq!(highlights[&0].changed, vec![12..13]);
		assert_eq!(highlights[&1].changed, vec![12..13]);
	}

	#[test]
	fn redraws_keep_chunk_results_and_pair_edits_across_boundaries() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		let mut lines = Vec::new();
		for index in 0..105 {
			lines.push(DiffLine {
				content: format!("context {index}").into(),
				line_type: DiffLineType::None,
				position: DiffLinePosition::default(),
			});
		}
		for index in 0..15 {
			lines.push(DiffLine {
				content: format!("let count_{index} = 1;").into(),
				line_type: DiffLineType::Delete,
				position: DiffLinePosition {
					old_lineno: Some(index + 1),
					..Default::default()
				},
			});
		}
		for index in 0..15 {
			lines.push(DiffLine {
				content: format!("let count_{index} = 2;").into(),
				line_type: DiffLineType::Add,
				position: DiffLinePosition {
					new_lineno: Some(index + 1),
					..Default::default()
				},
			});
		}
		for index in 0..700 {
			lines.push(DiffLine {
				content: format!("tail {index}").into(),
				line_type: DiffLineType::None,
				position: DiffLinePosition::default(),
			});
		}
		let line_count = lines.len();
		component.update(
			"large.rs".to_owned(),
			false,
			FileDiff {
				hunks: vec![asyncgit::sync::diff::Hunk {
					header_hash: 1,
					lines,
				}],
				lines: line_count,
				..Default::default()
			},
		);

		// Simulate a redraw while the first chunk is still in flight. The
		// next core chunk must finish through the same component channel.
		component.schedule_highlights(0, 1);
		component.schedule_highlights(0, 1);
		wait_for_highlights(&component, &[120, 128]);

		let highlights = component.row_highlights.borrow();
		for index in [120, 128] {
			let row = &component.rendered_rows[index].content;
			let range = highlights[&index].changed[0].clone();
			assert_eq!(&row[range], "2");
		}
		drop(highlights);
		assert!(component.rendered_rows.len() > 800);

		let boundary_highlight =
			component.row_highlights.borrow()[&128].changed.clone();
		let display_boundary = component
			.display_rows
			.iter()
			.position(|row| row.raw_rows.contains(&128))
			.unwrap();
		component.vertical_scroll.set_top(display_boundary);
		component.schedule_highlights(
			display_boundary,
			display_boundary + 1,
		);
		assert_eq!(
			component.row_highlights.borrow()[&128].changed,
			boundary_highlight,
			"overlapping cached rows should remain styled as the window shifts"
		);
		wait_for_highlights(&component, &[128]);
		let row = &component.rendered_rows[128].content;
		let range = component.row_highlights.borrow()[&128].changed
			[0]
		.clone();
		assert_eq!(&row[range], "2");
	}

	#[test]
	fn source_swap_while_highlighting_is_pending_rejects_old_rows() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.update(
			"old.rs".to_owned(),
			false,
			synthetic_diff(
				(0..800).map(|index| {
					format!("let old_value_{index} = 1;")
				}),
			),
		);
		component.schedule_highlights(0, 1);

		let lines = vec![
			DiffLine {
				content: "x = A;".into(),
				line_type: DiffLineType::Delete,
				position: DiffLinePosition {
					old_lineno: Some(1),
					..Default::default()
				},
			},
			DiffLine {
				content: "x = B;".into(),
				line_type: DiffLineType::Add,
				position: DiffLinePosition {
					new_lineno: Some(1),
					..Default::default()
				},
			},
		];
		component.update(
			"new.rs".to_owned(),
			false,
			FileDiff {
				hunks: vec![asyncgit::sync::diff::Hunk {
					header_hash: 2,
					lines,
				}],
				lines: 2,
				..Default::default()
			},
		);
		component.schedule_highlights(0, 1);
		wait_for_highlights(&component, &[0]);
		std::thread::sleep(std::time::Duration::from_millis(100));
		component.apply_highlight_result();

		assert_eq!(component.current.path, "new.rs");
		assert_eq!(
			component.row_highlights.borrow()[&0].changed,
			vec![4..5]
		);
		assert_eq!(
			component.row_highlights.borrow()[&1].changed,
			vec![4..5]
		);
	}

	#[test]
	fn mouse_wheel_moves_viewport_monotonically_without_moving_cursor(
	) {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.update(
			"wheel.txt".into(),
			false,
			synthetic_diff((0..40).map(|i| format!("line {i}"))),
		);
		component.focus(true);
		let mut terminal =
			Terminal::new(TestBackend::new(30, 8)).unwrap();
		draw_diff(&mut component, &mut terminal);
		let cursor = component.selection_cursor;

		let wheel = |kind| {
			Event::Mouse(MouseEvent {
				kind,
				column: 4,
				row: 3,
				modifiers: KeyModifiers::NONE,
			})
		};
		component.event(&wheel(MouseEventKind::ScrollDown)).unwrap();
		let first = component.vertical_scroll.get_top();
		draw_diff(&mut component, &mut terminal);
		component.event(&wheel(MouseEventKind::ScrollDown)).unwrap();
		let second = component.vertical_scroll.get_top();
		draw_diff(&mut component, &mut terminal);
		component.event(&wheel(MouseEventKind::ScrollUp)).unwrap();
		let third = component.vertical_scroll.get_top();

		assert_eq!((first, second, third), (1, 2, 1));
		assert_eq!(component.selection_cursor, cursor);
	}

	#[test]
	fn changed_diff_keeps_cursor_and_viewport_anchors_by_line_content(
	) {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.update(
			"moving.txt".into(),
			false,
			synthetic_diff((0..40).map(|i| format!("line {i}"))),
		);
		component.focus(true);
		let mut terminal =
			Terminal::new(TestBackend::new(30, 8)).unwrap();
		draw_diff(&mut component, &mut terminal);
		component.vertical_scroll.set_top(3);
		component.update_selection(8);
		let cursor_content = component
			.row_anchor(component.selection_cursor)
			.unwrap()
			.content;
		let viewport_content = component
			.row_anchor(component.vertical_scroll.get_top())
			.unwrap()
			.content;

		let mut contents =
			vec![String::from("inserted before anchors")];
		contents.extend((0..40).map(|i| format!("line {i}")));
		component.update(
			"moving.txt".into(),
			false,
			synthetic_diff(contents),
		);

		assert_eq!(
			component
				.row_anchor(component.selection_cursor)
				.unwrap()
				.content,
			cursor_content
		);
		assert_eq!(
			component
				.row_anchor(component.vertical_scroll.get_top())
				.unwrap()
				.content,
			viewport_content
		);
		assert!(matches!(component.selection, Selection::Single(_)));
	}

	#[test]
	fn wheel_render_reuses_cached_rows_and_only_returns_visible_rows()
	{
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.update(
			"large.txt".into(),
			false,
			synthetic_diff((0..20_000).map(|i| format!("line {i}"))),
		);
		component.focus(true);
		let mut terminal =
			Terminal::new(TestBackend::new(30, 8)).unwrap();
		draw_diff(&mut component, &mut terminal);
		let cached_ptr = component.rendered_rows[0].content.as_ptr();
		assert_eq!(component.get_text(28, 6).len(), 6);

		let wheel = Event::Mouse(MouseEvent {
			kind: MouseEventKind::ScrollDown,
			column: 4,
			row: 3,
			modifiers: KeyModifiers::NONE,
		});
		component.event(&wheel).unwrap();
		draw_diff(&mut component, &mut terminal);
		assert_eq!(
			component.rendered_rows[0].content.as_ptr(),
			cached_ptr
		);
		assert_eq!(component.get_text(28, 6).len(), 6);
	}

	#[test]
	fn changing_file_resets_the_diff_selection_even_when_contents_match(
	) {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		let diff = FileDiff {
			hunks: vec![asyncgit::sync::diff::Hunk {
				header_hash: 1,
				lines: vec![
					DiffLine {
						content: "same".into(),
						line_type: DiffLineType::Add,
						position: DiffLinePosition {
							new_lineno: Some(1),
							..Default::default()
						},
					},
					DiffLine {
						content: "other".into(),
						line_type: DiffLineType::Add,
						position: DiffLinePosition {
							new_lineno: Some(2),
							..Default::default()
						},
					},
				],
			}],
			lines: 2,
			..Default::default()
		};

		component.update(
			"first.txt".to_string(),
			false,
			diff.clone(),
		);
		component.update_selection(1);
		component.update("second.txt".to_string(), false, diff);

		assert!(matches!(component.selection, Selection::Single(0)));
	}

	#[test]
	fn changing_to_a_short_file_does_not_keep_an_offscreen_cursor() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.update(
			"long.txt".to_string(),
			false,
			synthetic_diff(
				(0..30).map(|index| format!("line {index}")),
			),
		);
		component.update_selection(20);
		component.vertical_scroll.set_top(14);

		component.update(
			"short.txt".to_string(),
			false,
			synthetic_diff(["only row".to_string()]),
		);

		assert_eq!(component.selection_cursor, 0);
		assert_eq!(component.vertical_scroll.get_top(), 0);
	}

	#[test]
	fn mouse_click_and_drag_select_the_drawn_diff_rows() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.update(
			"mouse.txt".into(),
			false,
			FileDiff {
				hunks: vec![asyncgit::sync::diff::Hunk {
					header_hash: 1,
					lines: vec![
						DiffLine {
							content: "first".into(),
							line_type: DiffLineType::Add,
							position: DiffLinePosition {
								new_lineno: Some(1),
								..Default::default()
							},
						},
						DiffLine {
							content: "second".into(),
							line_type: DiffLineType::Add,
							position: DiffLinePosition {
								new_lineno: Some(2),
								..Default::default()
							},
						},
						DiffLine {
							content: "third".into(),
							line_type: DiffLineType::Add,
							position: DiffLinePosition {
								new_lineno: Some(3),
								..Default::default()
							},
						},
					],
				}],
				lines: 3,
				..Default::default()
			},
		);
		component.focus(true);
		let mut terminal =
			Terminal::new(TestBackend::new(12, 5)).unwrap();
		terminal
			.draw(|frame| {
				component.draw(frame, frame.area()).unwrap();
			})
			.unwrap();
		assert_eq!(
			component.inner_rect.get(),
			Rect::new(1, 1, 10, 3)
		);

		let click = Event::Mouse(MouseEvent {
			kind: MouseEventKind::Down(MouseButton::Left),
			column: 2,
			row: 2,
			modifiers: KeyModifiers::NONE,
		});
		assert!(matches!(
			component.event(&click).unwrap(),
			EventState::Consumed
		));
		assert!(matches!(component.selection, Selection::Single(1)));

		let start = Event::Mouse(MouseEvent {
			kind: MouseEventKind::Down(MouseButton::Left),
			column: 2,
			row: 1,
			modifiers: KeyModifiers::NONE,
		});
		let drag = Event::Mouse(MouseEvent {
			kind: MouseEventKind::Drag(MouseButton::Left),
			column: 2,
			row: 3,
			modifiers: KeyModifiers::NONE,
		});
		let end = Event::Mouse(MouseEvent {
			kind: MouseEventKind::Up(MouseButton::Left),
			column: 2,
			row: 3,
			modifiers: KeyModifiers::NONE,
		});
		component.event(&start).unwrap();
		component.event(&drag).unwrap();
		component.event(&end).unwrap();
		assert_eq!(component.selection.get_top(), 0);
		assert_eq!(component.selection.get_bottom(), 2);
		assert!(!component.selection_extending);
	}

	#[test]
	fn unmatched_rows_use_side_numbers_and_color_without_change_glyphs(
	) {
		let theme = Rc::new(Theme::default());
		let cases = [
			(
				DiffLine {
					content: "old_value".into(),
					line_type: DiffLineType::Delete,
					position: DiffLinePosition {
						old_lineno: Some(3),
						..Default::default()
					},
				},
				"3│ │old_value\n",
				DiffLineType::Delete,
			),
			(
				DiffLine {
					content: "new_value".into(),
					line_type: DiffLineType::Add,
					position: DiffLinePosition {
						new_lineno: Some(4),
						..Default::default()
					},
				},
				" │4│new_value\n",
				DiffLineType::Add,
			),
		];
		for (line, expected, line_type) in cases {
			let rendered = DiffComponent::get_line_to_add(
				80,
				&line,
				&line.content,
				DiffRowRenderConfig {
					number_width: 1,
					highlight: DiffRowHighlight::Normal,
					hunk_selected: false,
					scrolled_right: 0,
				},
				&theme,
				&InlineHighlight {
					changed: vec![0..line.content.len()],
					syntax: vec![],
				},
			);
			let text = rendered
				.spans
				.iter()
				.map(|span| span.content.as_ref())
				.collect::<String>();
			assert_eq!(text, expected);
			assert!(!text.contains('+'));
			assert!(!text.contains('-'));
			let content_span = rendered
				.spans
				.iter()
				.find(|span| {
					span.content.as_ref() == line.content.as_ref()
				})
				.unwrap();
			assert!(content_span.style.bg.is_some());
			assert_eq!(
				content_span.style.fg,
				theme.diff_inline_changed(line_type).fg,
			);
		}
	}

	#[test]
	fn narrow_unicode_diff_rows_keep_aligned_numbers_without_markers()
	{
		let theme = Rc::new(Theme::default());
		let line = DiffLine {
			content: "界e\u{301} text".into(),
			line_type: DiffLineType::Add,
			position: DiffLinePosition {
				new_lineno: Some(12),
				..Default::default()
			},
		};
		for width in 0..8 {
			let rendered = DiffComponent::get_line_to_add(
				width,
				&line,
				&line.content,
				DiffRowRenderConfig {
					number_width: 2,
					highlight: DiffRowHighlight::Normal,
					hunk_selected: false,
					scrolled_right: 0,
				},
				&theme,
				&InlineHighlight::default(),
			);
			let text = rendered
				.spans
				.iter()
				.map(|span| span.content.as_ref())
				.collect::<String>();
			assert!(text.starts_with("  │12│"));
			assert!(!text.contains('+'));
			assert!(!text.contains('~'));
		}
	}

	#[test]
	fn inline_changed_spans_keep_patch_text_and_emphasize_only_ranges(
	) {
		let theme = Rc::new(Theme::default());
		let line = DiffLine {
			content: "let value = 2;".into(),
			line_type: DiffLineType::Add,
			position: DiffLinePosition::default(),
		};
		let inline = InlineHighlight {
			changed: vec![12..13],
			syntax: vec![crate::ui::diff_highlight::StyledRange {
				range: 0..13,
				style: ratatui::style::Style::default()
					.fg(ratatui::style::Color::Cyan),
			}],
		};
		let spans = DiffComponent::content_spans(
			Cow::Borrowed(&line.content),
			&line.content,
			&line,
			&inline,
			theme.diff_line(DiffLineType::Add, false),
			DiffRowHighlight::Normal,
			&theme,
		);
		assert_eq!(
			spans
				.iter()
				.map(|span| span.content.as_ref())
				.collect::<String>(),
			line.content.as_ref()
		);
		let changed =
			spans.iter().find(|span| span.content == "2").unwrap();
		assert!(changed
			.style
			.add_modifier
			.contains(ratatui::style::Modifier::UNDERLINED));
		assert!(changed.style.bg.is_some());
		assert_eq!(
			changed.style.fg,
			Some(ratatui::style::Color::Cyan)
		);
		assert!(spans.iter().any(|span| {
			span.content == "let value = "
				&& span
					.style
					.add_modifier
					.contains(ratatui::style::Modifier::DIM)
		}));
		let selected = DiffComponent::content_spans(
			Cow::Borrowed(&line.content),
			&line.content,
			&line,
			&inline,
			theme.diff_line_cursor(DiffLineType::Add, true),
			DiffRowHighlight::CursorSelected,
			&theme,
		);
		let selected_changed =
			selected.iter().find(|span| span.content == "2").unwrap();
		assert_eq!(
			selected_changed.style.bg,
			theme.diff_line(DiffLineType::Add, true).bg
		);
		let cursor = DiffComponent::content_spans(
			Cow::Borrowed(&line.content),
			&line.content,
			&line,
			&inline,
			theme.diff_line_cursor(DiffLineType::Add, false),
			DiffRowHighlight::Cursor,
			&theme,
		);
		let cursor_changed =
			cursor.iter().find(|span| span.content == "2").unwrap();
		assert_eq!(cursor_changed.style.fg, changed.style.fg);
		assert_eq!(cursor_changed.style.bg, changed.style.bg);
		assert!(!cursor
			.iter()
			.find(|span| span.content == "let value = ")
			.unwrap()
			.style
			.add_modifier
			.contains(ratatui::style::Modifier::UNDERLINED));
		assert!(cursor_changed
			.style
			.add_modifier
			.contains(ratatui::style::Modifier::BOLD));
		assert!(selected_changed
			.style
			.add_modifier
			.contains(ratatui::style::Modifier::BOLD));
	}

	#[test]
	fn single_cursor_keeps_syntax_foreground_until_range_selection() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.update(
			"cursor.rs".to_owned(),
			false,
			synthetic_diff(["let value = 2;".to_owned()]),
		);
		component.focus(true);
		component.row_highlights.borrow_mut().insert(
			0,
			InlineHighlight {
				changed: vec![12..13],
				syntax: vec![
					crate::ui::diff_highlight::StyledRange {
						range: 0..13,
						style: ratatui::style::Style::default()
							.fg(ratatui::style::Color::Cyan),
					},
				],
			},
		);
		let line = component.get_text(80, 1).remove(0);
		let changed = line
			.spans
			.iter()
			.find(|span| span.content == "2")
			.unwrap();
		assert_eq!(
			changed.style.fg,
			Some(ratatui::style::Color::Cyan)
		);
		assert_eq!(
			changed.style.bg,
			component.theme.diff_inline_changed(DiffLineType::Add).bg
		);

		component.selection_mode = true;
		let selected_line = component.get_text(80, 1).remove(0);
		let selected = selected_line
			.spans
			.iter()
			.find(|span| span.content == "2")
			.unwrap();
		assert_eq!(
			selected.style.bg,
			component.theme.diff_line(DiffLineType::Add, true).bg
		);
	}

	#[test]
	fn accepted_source_replacement_wins_over_late_projection_worker()
	{
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		let old_text = "The interface";
		let new_text = "The interfaces";
		let segments =
			asyncgit::diff_preview::inline_prose_diff_segments(
				old_text, new_text,
			)
			.unwrap();
		let lines = vec![
			DiffLine {
				content: old_text.into(),
				line_type: DiffLineType::Delete,
				position: DiffLinePosition {
					old_lineno: Some(1),
					..Default::default()
				},
			},
			DiffLine {
				content: new_text.into(),
				line_type: DiffLineType::Add,
				position: DiffLinePosition {
					new_lineno: Some(1),
					..Default::default()
				},
			},
		];
		component.update(
			"replacement.txt".to_owned(),
			false,
			FileDiff {
				hunks: vec![asyncgit::sync::diff::Hunk {
					header_hash: 1,
					lines,
				}],
				lines: 2,
				..Default::default()
			},
		);
		let authoritative = ProjectionPair {
			new_row: 1,
			segments: segments.clone(),
		};
		component
			.projection_pairs
			.borrow_mut()
			.insert(0, authoritative);
		component
			.projection_sender
			.send(ProjectionResult {
				generation: component.highlight_generation,
				path: component.current.path.clone(),
				is_stage: component.current.is_stage,
				hash: component.current.hash,
				core_start: 0,
				core_end: 1,
				requested_end: 2,
				pairs: vec![(
					0,
					ProjectionPair {
						new_row: 1,
						segments: vec![
							asyncgit::diff_preview::InlineSegment {
								kind: asyncgit::diff_preview::InlineSegmentKind::Removed,
								old_range: 12..13,
								new_range: 11..11,
							},
							asyncgit::diff_preview::InlineSegment {
								kind: asyncgit::diff_preview::InlineSegmentKind::Added,
								old_range: 13..13,
								new_range: 12..13,
							},
						],
					},
				)],
			})
			.unwrap();

		component.apply_projection_results();

		assert_eq!(
			component.projection_pairs.borrow()[&0].new_row,
			1,
		);
		assert_eq!(
			component.projection_pairs.borrow()[&0].segments,
			segments,
		);
	}

	#[test]
	fn rendered_megabyte_unicode_line_is_bounded_after_horizontal_scroll(
	) {
		let content = format!(
			"{}xyz{}",
			"界".repeat(200_000),
			"界".repeat(149_524),
		);
		assert_eq!(content.len(), 1_048_575);
		let line = DiffLine {
			content: content.clone().into(),
			line_type: DiffLineType::Add,
			position: DiffLinePosition::default(),
		};
		let theme = Rc::new(Theme::default());
		let rendered = DiffComponent::get_line_to_add(
			20,
			&line,
			&content,
			DiffRowRenderConfig {
				number_width: 1,
				highlight: DiffRowHighlight::Normal,
				hunk_selected: false,
				scrolled_right: 400_000,
			},
			&theme,
			&InlineHighlight {
				changed: vec![600_000..600_003],
				syntax: vec![],
			},
		);

		assert!(
			rendered.width() <= 20,
			"rendered width {}: {:?}",
			rendered.width(),
			rendered
				.spans
				.iter()
				.map(|span| (
					span.content.as_ref(),
					UnicodeWidthStr::width(span.content.as_ref())
				))
				.collect::<Vec<_>>(),
		);
		assert!(
			rendered
				.spans
				.iter()
				.map(|span| span.content.len())
				.sum::<usize>()
				< 100
		);
		let changed = rendered
			.spans
			.iter()
			.find(|span| span.content.as_ref() == "xyz")
			.unwrap();
		assert!(changed
			.style
			.add_modifier
			.contains(ratatui::style::Modifier::UNDERLINED));
	}

	#[test]
	fn collapsed_replacement_maps_nonadjacent_patch_rows_and_expands_unified(
	) {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut component = DiffComponent::new(&env, false);
		let old_position = DiffLinePosition {
			old_lineno: Some(2),
			..Default::default()
		};
		let new_position = DiffLinePosition {
			new_lineno: Some(4),
			..Default::default()
		};
		let diff = FileDiff {
			hunks: vec![asyncgit::sync::diff::Hunk {
				header_hash: 7,
				lines: vec![
					DiffLine {
						content: "before".into(),
						line_type: DiffLineType::None,
						position: DiffLinePosition {
							old_lineno: Some(1),
							new_lineno: Some(1),
						},
					},
					DiffLine {
						content: "vec2 cell_pos;".into(),
						line_type: DiffLineType::Delete,
						position: old_position,
					},
					DiffLine {
						content: "xxxxxxxx".into(),
						line_type: DiffLineType::Delete,
						position: DiffLinePosition {
							old_lineno: Some(3),
							..Default::default()
						},
					},
					DiffLine {
						content: "yyyyyyyy".into(),
						line_type: DiffLineType::Add,
						position: DiffLinePosition {
							new_lineno: Some(3),
							..Default::default()
						},
					},
					DiffLine {
						content: "vec2 cell_coord;".into(),
						line_type: DiffLineType::Add,
						position: new_position,
					},
					DiffLine {
						content: "after".into(),
						line_type: DiffLineType::None,
						position: DiffLinePosition {
							old_lineno: Some(4),
							new_lineno: Some(5),
						},
					},
				],
			}],
			lines: 6,
			..Default::default()
		};
		component.update("vector.rs".into(), false, diff.clone());
		component.schedule_projection(0, 6);
		wait_for_projection(&mut component);
		assert_eq!(component.display_rows.len(), 5);
		let paired = display_index_for_raw(&component, 1);
		assert_eq!(
			component.display_rows[paired].raw_rows.as_ref(),
			[1, 4]
		);
		component.update_selection(paired);
		assert_eq!(
			component.selected_lines(),
			vec![old_position, new_position]
		);
		component
			.schedule_highlights(0, component.display_rows.len());
		wait_for_highlights(&component, &[1, 4]);
		draw_component_now(&component);

		let rendered = component.get_text(80, 24);
		let replacement = &rendered[paired];
		let text = replacement
			.spans
			.iter()
			.map(|span| span.content.as_ref())
			.collect::<String>();
		assert!(text.contains("2│4│"));
		assert!(!text.contains('~'));
		let removed = replacement
			.spans
			.iter()
			.find(|span| span.content.as_ref() == "pos")
			.unwrap();
		assert!(removed
			.style
			.add_modifier
			.contains(ratatui::style::Modifier::CROSSED_OUT));
		assert!(!removed
			.style
			.add_modifier
			.contains(ratatui::style::Modifier::UNDERLINED));
		let added = replacement
			.spans
			.iter()
			.find(|span| span.content.as_ref() == "coord")
			.unwrap();
		assert!(added
			.style
			.add_modifier
			.contains(ratatui::style::Modifier::UNDERLINED));
		assert!(!added
			.style
			.add_modifier
			.contains(ratatui::style::Modifier::CROSSED_OUT));
		assert_eq!(removed.style.fg, added.style.fg);
		assert_ne!(removed.style.bg, added.style.bg);

		component.focus(true);
		let reset = Event::Key(KeyEvent::new(
			KeyCode::Char('D'),
			KeyModifiers::SHIFT,
		));
		assert!(matches!(
			component.event(&reset).unwrap(),
			EventState::Consumed
		));
		match env.queue.pop().unwrap() {
			InternalEvent::ConfirmAction(
				Action::ResetLinesWithPairs(_, positions, pairs, _),
			) => {
				assert_eq!(
					positions,
					vec![old_position, new_position]
				);
				assert_eq!(
					pairs,
					vec![DiffLineReplacement {
						old: old_position,
						new: new_position,
					}]
				);
			}
			_ => panic!(
				"expected both exact patch rows for the paired reset"
			),
		}

		env.options.borrow_mut().set_diff_preview_preference(
			DiffPreviewPreference::Unified,
		);
		component.update("vector.rs".into(), false, diff);
		assert_eq!(component.display_rows.len(), 6);
		let expanded_add = display_index_for_raw(&component, 4);
		component.update_selection(expanded_add);
		assert_eq!(component.selected_lines(), vec![new_position]);
	}

	#[test]
	fn changing_to_short_source_clears_old_folds_before_rebuilding_rows(
	) {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, true);
		component.update(
			"old.rs".into(),
			false,
			synthetic_diff((0..20).map(|line| format!("old_{line}"))),
		);
		component.apply_structural_analysis(Some(
			StructuralAnalysis {
				language: "Rust",
				regions: vec![StructuralRegion {
					id: 91,
					parent_id: None,
					kind: "function",
					label: "old_test_body".into(),
					old_lines: None,
					new_lines: Some(0..20),
					change:
						asyncgit::structural::StructuralChange::Added,
					is_test: true,
					foldable: true,
					contains_changes: true,
				}],
			},
		));
		assert_eq!(component.display_rows.len(), 1);
		assert!(component.display_rows[0].fold.is_some());

		component.update(
			"short.rs".into(),
			false,
			synthetic_diff(["new_a".into(), "new_b".into()]),
		);
		draw_component_now(&component);

		assert!(component.fold_groups.is_empty());
		assert!(component.folded_regions.is_empty());
		assert_eq!(component.display_rows.len(), 2);
		assert!(component
			.display_rows
			.iter()
			.all(|row| row.fold.is_none()));
		assert_eq!(
			component
				.display_rows
				.iter()
				.map(|row| row.raw_rows.to_vec())
				.collect::<Vec<_>>(),
			vec![vec![0], vec![1]],
		);
	}

	#[test]
	fn structural_test_fold_toggles_without_exposing_hidden_patch_rows(
	) {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut component = DiffComponent::new(&env, false);
		component.update(
			"vector.rs".into(),
			false,
			synthetic_diff((1..=5).map(|n| format!("line_{n}"))),
		);
		component.update_selection(3);
		component.apply_structural_analysis(Some(
			StructuralAnalysis {
				language: "Rust",
				regions: vec![StructuralRegion {
				id: 42,
				parent_id: None,
				kind: "function",
				label: "test_vector".into(),
				old_lines: Some(0..5),
				new_lines: Some(0..5),
				change: asyncgit::structural::StructuralChange::Modified,
				is_test: true,
				foldable: true,
				contains_changes: true,
			}],
			},
		));
		assert_eq!(component.display_rows.len(), 1);
		assert_eq!(component.selection_cursor, 0);
		assert!(component.display_rows[0].fold.is_some());
		assert!(component
			.folded_regions
			.contains(&component.fold_groups[0].id));
		assert!(component.selected_lines().is_empty());
		draw_component_now(&component);

		component.focus(true);
		let enter = Event::Key(KeyEvent::new(
			KeyCode::Enter,
			KeyModifiers::NONE,
		));
		let click = Event::Mouse(MouseEvent {
			kind: MouseEventKind::Down(MouseButton::Left),
			column: 2,
			row: 1,
			modifiers: KeyModifiers::NONE,
		});
		assert!(matches!(
			component.event(&click).unwrap(),
			EventState::Consumed
		));
		assert_eq!(component.display_rows.len(), 6);
		assert!(!component
			.folded_regions
			.contains(&component.fold_groups[0].id));
		assert!(component.selected_lines().is_empty());
		draw_component_now(&component);

		assert!(matches!(
			component.event(&enter).unwrap(),
			EventState::Consumed
		));
		assert_eq!(component.display_rows.len(), 1);
		assert!(component.selected_lines().is_empty());
	}

	#[test]
	fn compact_projection_keeps_insertions_between_replacements_in_new_order(
	) {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut component = DiffComponent::new(&env, false);
		let old_lines = ["first_old", "second_old"];
		let new_lines =
			["inserted", "first_new", "second_new", "tail"];
		let mut lines = old_lines
			.iter()
			.enumerate()
			.map(|(index, content)| DiffLine {
				content: (*content).into(),
				line_type: DiffLineType::Delete,
				position: DiffLinePosition {
					old_lineno: Some(index as u32 + 1),
					..Default::default()
				},
			})
			.collect::<Vec<_>>();
		lines.extend(new_lines.iter().enumerate().map(
			|(index, content)| DiffLine {
				content: (*content).into(),
				line_type: DiffLineType::Add,
				position: DiffLinePosition {
					new_lineno: Some(index as u32 + 1),
					..Default::default()
				},
			},
		));
		component.update(
			"order.txt".into(),
			false,
			FileDiff {
				hunks: vec![asyncgit::sync::diff::Hunk {
					header_hash: 1,
					lines,
				}],
				lines: 6,
				..Default::default()
			},
		);
		let mut pairs = component.projection_pairs.borrow_mut();
		for (old_raw, new_raw) in [(0, 3), (1, 4)] {
			let segments =
				asyncgit::diff_preview::inline_diff_segments(
					&component.rendered_rows[old_raw].content,
					&component.rendered_rows[new_raw].content,
				)
				.unwrap();
			pairs.insert(
				old_raw,
				ProjectionPair {
					new_row: new_raw,
					segments,
				},
			);
		}
		drop(pairs);
		component.rebuild_display_rows();
		assert_eq!(
			component
				.display_rows
				.iter()
				.map(|row| row.raw_rows.to_vec())
				.collect::<Vec<_>>(),
			vec![vec![2], vec![0, 3], vec![1, 4], vec![5]],
		);
	}

	#[test]
	fn navigation_still_works_while_async_projection_needs_a_redraw()
	{
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut component = DiffComponent::new(&env, false);
		component.update(
			"scroll.txt".into(),
			false,
			synthetic_diff((1..=6).map(|n| format!("line_{n}"))),
		);
		component.focus(true);
		component.update_selection(5);
		draw_component_now(&component);
		component.projection_generation.set(
			component.projection_generation.get().wrapping_add(1),
		);
		assert!(!component.projection_is_drawn());

		let home = Event::Key(KeyEvent::new(
			KeyCode::Home,
			KeyModifiers::NONE,
		));
		assert!(matches!(
			component.event(&home).unwrap(),
			EventState::Consumed
		));
		assert_eq!(component.selection_cursor, 0);
	}

	#[test]
	fn fold_groups_are_removed_when_a_pair_crosses_declarations() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		let diff = FileDiff {
			hunks: vec![asyncgit::sync::diff::Hunk {
				header_hash: 1,
				lines: vec![
					DiffLine {
						content: "old test body".into(),
						line_type: DiffLineType::Delete,
						position: DiffLinePosition {
							old_lineno: Some(1),
							..Default::default()
						},
					},
					DiffLine {
						content: "old end".into(),
						line_type: DiffLineType::None,
						position: DiffLinePosition {
							old_lineno: Some(2),
							..Default::default()
						},
					},
					DiffLine {
						content: "new test body".into(),
						line_type: DiffLineType::Add,
						position: DiffLinePosition {
							new_lineno: Some(1),
							..Default::default()
						},
					},
					DiffLine {
						content: "new end".into(),
						line_type: DiffLineType::None,
						position: DiffLinePosition {
							new_lineno: Some(2),
							..Default::default()
						},
					},
				],
			}],
			lines: 4,
			..Default::default()
		};
		component.update("renamed_test.rs".into(), false, diff);
		component.apply_structural_analysis(Some(
			StructuralAnalysis {
				language: "Rust",
				regions: vec![
				StructuralRegion {
					id: 10,
					parent_id: None,
					kind: "function",
					label: "old_test".into(),
					old_lines: Some(0..2),
					new_lines: None,
					change: asyncgit::structural::StructuralChange::Removed,
					is_test: true,
					foldable: true,
					contains_changes: true,
				},
				StructuralRegion {
					id: 11,
					parent_id: None,
					kind: "function",
					label: "new_test".into(),
					old_lines: None,
					new_lines: Some(0..2),
					change: asyncgit::structural::StructuralChange::Added,
					is_test: true,
					foldable: true,
					contains_changes: true,
				},
			],
			},
		));
		assert_eq!(component.fold_groups.len(), 2);
		component.projection_pairs.borrow_mut().insert(
			0,
			ProjectionPair {
				new_row: 2,
				segments: Vec::new(),
			},
		);
		component.prune_crossing_pair_folds();
		assert!(component.fold_groups.is_empty());
		assert!(component.folded_regions.is_empty());
	}

	#[test]
	fn diff_rendering_sanitizes_control_bytes_in_lines_and_paths() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.update(
			"file\u{1b}[2J.txt".to_owned(),
			false,
			FileDiff {
				hunks: vec![asyncgit::sync::diff::Hunk {
					header_hash: 1,
					lines: vec![DiffLine {
						content: "\u{1b}[2J".into(),
						line_type: DiffLineType::Add,
						position: DiffLinePosition {
							new_lineno: Some(1),
							..Default::default()
						},
					}],
				}],
				lines: 1,
				..Default::default()
			},
		);
		let mut terminal =
			Terminal::new(TestBackend::new(40, 5)).unwrap();
		terminal
			.draw(|frame| {
				component.draw(frame, frame.area()).unwrap();
			})
			.unwrap();
		let buffer = terminal.backend().buffer();
		let text = (0..buffer.area.height)
			.flat_map(|y| {
				(0..buffer.area.width)
					.map(move |x| buffer[(x, y)].symbol().to_owned())
			})
			.collect::<String>();
		assert!(!text.chars().any(char::is_control));
		assert!(text.contains("file?[2J.txt"));
		assert!(text.contains("?[2J"));
	}

	#[test]
	fn selected_hunk_uses_exclusive_end_bounds() {
		let diff = FileDiff {
			hunks: vec![
				asyncgit::sync::diff::Hunk {
					header_hash: 1,
					lines: vec![
						DiffLine::default(),
						DiffLine::default(),
					],
				},
				asyncgit::sync::diff::Hunk {
					header_hash: 2,
					lines: vec![DiffLine::default()],
				},
			],
			lines: 3,
			..Default::default()
		};
		assert_eq!(
			DiffComponent::find_selected_hunk(&diff, 0),
			Some(0)
		);
		assert_eq!(
			DiffComponent::find_selected_hunk(&diff, 1),
			Some(0)
		);
		assert_eq!(
			DiffComponent::find_selected_hunk(&diff, 2),
			Some(1)
		);
		assert_eq!(DiffComponent::find_selected_hunk(&diff, 3), None);
	}

	#[test]
	fn legacy_selection_movement_keeps_using_the_range_edges() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.update(
			"file.txt".into(),
			false,
			FileDiff {
				hunks: vec![asyncgit::sync::diff::Hunk {
					header_hash: 1,
					lines: vec![DiffLine::default(); 10],
				}],
				lines: 10,
				..Default::default()
			},
		);

		component.selection = Selection::Multiple(3, 5);
		component.selection_cursor = 5;
		component.move_selection(ScrollType::Down);
		assert_eq!(component.selection.get_end(), 6);

		component.selection = Selection::Multiple(7, 3);
		component.selection_cursor = 3;
		component.move_selection(ScrollType::Up);
		assert_eq!(component.selection.get_end(), 2);
	}

	#[test]
	fn empty_diffs_and_empty_hunks_do_not_create_selections() {
		let env = Environment::test_env();
		let mut component = DiffComponent::new(&env, false);
		component.update(
			"file.txt".into(),
			false,
			FileDiff {
				hunks: vec![asyncgit::sync::diff::Hunk {
					header_hash: 1,
					lines: vec![],
				}],
				lines: 0,
				..Default::default()
			},
		);
		component.selected_hunk = Some(0);

		component.select_hunk();
		component.select_all();
		component.toggle_selection_mode();
		component.move_selection(ScrollType::Down);
		assert!(!component.selection_mode);
		assert!(component.selected_lines().is_empty());
		assert_eq!(component.selection_status(), None);
	}

	#[test]
	fn modal_escape_clears_selection_before_bubbling() {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut component = DiffComponent::new(&env, false);
		component.focus(true);
		component.selection_mode = true;
		component.selection_extending = true;
		component.selection = Selection::Multiple(2, 4);
		component.selection_anchor = 2;
		component.selection_cursor = 4;

		let escape = Event::Key(KeyEvent::new(
			KeyCode::Esc,
			KeyModifiers::empty(),
		));
		assert!(matches!(
			component.event(&escape).unwrap(),
			EventState::Consumed
		));
		assert_eq!(component.selection_status(), None);
		assert!(matches!(component.selection, Selection::Single(4)));
		assert!(matches!(
			component.event(&escape).unwrap(),
			EventState::NotConsumed
		));
	}

	#[test]
	fn focus_loss_clears_extension_and_collapses_to_the_visible_cursor(
	) {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut component = DiffComponent::new(&env, false);
		component.focus(true);
		component.selection_mode = true;
		component.selection_extending = true;
		component.selection = Selection::Multiple(7, 3);
		component.selection_anchor = 7;
		component.selection_cursor = 3;

		component.focus(false);

		assert!(!component.selection_mode);
		assert!(!component.selection_extending);
		assert_eq!(component.selection_status(), None);
		assert!(matches!(component.selection, Selection::Single(3)));
		assert_eq!(component.selection_cursor, 3);
	}

	#[test]
	fn identical_refresh_keeps_selection_but_changed_refresh_clears_mode(
	) {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut component = DiffComponent::new(&env, false);
		let first = FileDiff {
			hunks: vec![asyncgit::sync::diff::Hunk {
				header_hash: 1,
				lines: vec![DiffLine::default(); 3],
			}],
			lines: 3,
			..Default::default()
		};
		component.update("file.txt".into(), false, first.clone());
		component.update_selection(1);
		component.toggle_selection_mode();
		component.move_selection(ScrollType::Down);
		let selected = component.selection;

		component.update("file.txt".into(), false, first);
		assert!(component.selection_mode);
		assert_eq!(
			component.selection.get_start(),
			selected.get_start()
		);
		assert_eq!(component.selection.get_end(), selected.get_end());

		let changed = FileDiff {
			hunks: vec![asyncgit::sync::diff::Hunk {
				header_hash: 2,
				lines: vec![DiffLine::default(); 4],
			}],
			lines: 4,
			..Default::default()
		};
		component.update("file.txt".into(), false, changed);
		assert!(!component.selection_mode);
		assert!(!component.selection_extending);
		assert_eq!(component.selection_status(), None);
		assert!(component.selection.get_end() < 4);
	}

	#[test]
	fn immutable_modal_enter_does_not_stage() {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut component = DiffComponent::new(&env, true);
		component.update(
			"file.txt".into(),
			false,
			FileDiff {
				hunks: vec![asyncgit::sync::diff::Hunk {
					header_hash: 1,
					lines: vec![DiffLine {
						content: "new".into(),
						line_type: DiffLineType::Add,
						position: DiffLinePosition::default(),
					}],
				}],
				lines: 1,
				..Default::default()
			},
		);
		draw_component_now(&component);
		component.focus(true);
		let enter = Event::Key(KeyEvent::new(
			KeyCode::Enter,
			KeyModifiers::empty(),
		));
		assert!(matches!(
			component.event(&enter).unwrap(),
			EventState::NotConsumed
		));
		assert!(env.queue.pop().is_none());
	}

	#[test]
	fn legacy_enter_keeps_staging_the_current_hunk() {
		let dir = tempfile::tempdir().unwrap();
		let run = |args: &[&str]| {
			assert!(Command::new("git")
				.args(args)
				.current_dir(dir.path())
				.status()
				.unwrap()
				.success());
		};
		run(&["init", "-q"]);
		run(&["config", "user.email", "test@example.com"]);
		run(&["config", "user.name", "gitui test"]);
		fs::write(dir.path().join("file.txt"), "one\ntwo\nthree\n")
			.unwrap();
		run(&["add", "file.txt"]);
		run(&["commit", "-qm", "initial"]);
		fs::write(
			dir.path().join("file.txt"),
			"one changed\ntwo changed\nthree\n",
		)
		.unwrap();

		let repo: RepoPathRef =
			RefCell::new(dir.path().to_path_buf().into());
		let mut env = Environment::test_env();
		env.repo = repo.clone();
		let mut component = DiffComponent::new(&env, false);
		let diff = asyncgit::sync::diff::get_diff(
			&repo.borrow(),
			"file.txt",
			false,
			None,
		)
		.unwrap();
		let selected_line = diff
			.hunks
			.iter()
			.flat_map(|hunk| hunk.lines.iter())
			.position(|line| {
				line.line_type == DiffLineType::Add
					&& line.content.contains("one changed")
			})
			.unwrap();
		component.update("file.txt".into(), false, diff);
		component.update_selection(display_index_for_raw(
			&component,
			selected_line,
		));
		component.focus(true);
		draw_component_now(&component);

		let enter = Event::Key(KeyEvent::new(
			KeyCode::Enter,
			KeyModifiers::empty(),
		));
		assert!(matches!(
			component.event(&enter).unwrap(),
			EventState::Consumed
		));
		let staged = asyncgit::sync::diff::get_diff(
			&repo.borrow(),
			"file.txt",
			true,
			None,
		)
		.unwrap();
		let additions = staged
			.hunks
			.iter()
			.flat_map(|hunk| hunk.lines.iter())
			.filter(|line| line.line_type == DiffLineType::Add)
			.map(|line| line.content.as_ref())
			.collect::<Vec<_>>();
		assert!(additions
			.iter()
			.any(|line| line.contains("one changed")));
		assert!(additions
			.iter()
			.any(|line| line.contains("two changed")));
	}

	#[test]
	fn modal_reset_lines_uses_only_selected_changes_and_x_uses_the_hunk(
	) {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut component = DiffComponent::new(&env, false);
		let selected_position = DiffLinePosition::default();
		component.update(
			"file.txt".into(),
			false,
			FileDiff {
				hunks: vec![asyncgit::sync::diff::Hunk {
					header_hash: 42,
					lines: vec![
						DiffLine {
							content: "old".into(),
							line_type: DiffLineType::Delete,
							position: selected_position,
						},
						DiffLine {
							content: "new".into(),
							line_type: DiffLineType::Add,
							position: DiffLinePosition::default(),
						},
					],
				}],
				lines: 2,
				..Default::default()
			},
		);
		component.update_selection(1);
		component.focus(true);
		draw_component_now(&component);
		let reset = Event::Key(KeyEvent::new(
			KeyCode::Char('D'),
			KeyModifiers::SHIFT,
		));
		assert!(matches!(
			component.event(&reset).unwrap(),
			EventState::Consumed
		));
		match env.queue.pop().unwrap() {
			InternalEvent::ConfirmAction(Action::ResetLines(
				path,
				lines,
				_diff,
			)) => {
				assert_eq!(path, "file.txt");
				assert_eq!(lines.len(), 1);
				assert_eq!(lines[0], selected_position);
			}
			_ => panic!("expected selected-line confirmation"),
		}

		let select_hunk = Event::Key(KeyEvent::new(
			KeyCode::Char('x'),
			KeyModifiers::empty(),
		));
		component.event(&select_hunk).unwrap();
		component.event(&reset).unwrap();
		assert!(matches!(
			env.queue.pop().unwrap(),
			InternalEvent::ConfirmAction(Action::ResetHunk(
				path, 42, _diff,
			))
				if path == "file.txt"
		));
	}

	#[test]
	fn modal_enter_stages_cursor_line_and_x_enter_stages_selected_hunk(
	) {
		let dir = tempfile::tempdir().unwrap();
		let run = |args: &[&str]| {
			assert!(Command::new("git")
				.args(args)
				.current_dir(dir.path())
				.status()
				.unwrap()
				.success());
		};
		run(&["init", "-q"]);
		run(&["config", "user.email", "test@example.com"]);
		run(&["config", "user.name", "gitui test"]);
		let base = (0..30)
			.map(|line| format!("line {line}"))
			.collect::<Vec<_>>()
			.join("\n")
			+ "\n";
		fs::write(dir.path().join("file.txt"), &base).unwrap();
		run(&["add", "file.txt"]);
		run(&["commit", "-qm", "initial"]);
		let changed =
			base.lines()
				.enumerate()
				.map(|(index, line)| match index {
					1 => "line one changed".to_owned(),
					20 => "line twenty changed".to_owned(),
					_ => line.to_owned(),
				})
				.collect::<Vec<_>>()
				.join("\n") + "\n";
		fs::write(dir.path().join("file.txt"), changed).unwrap();

		let repo: RepoPathRef =
			RefCell::new(dir.path().to_path_buf().into());
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		env.repo = repo.clone();
		let mut component = DiffComponent::new(&env, false);
		let diff = asyncgit::sync::diff::get_diff(
			&repo.borrow(),
			"file.txt",
			false,
			None,
		)
		.unwrap();
		let first_added_line = diff
			.hunks
			.iter()
			.flat_map(|hunk| hunk.lines.iter())
			.position(|line| {
				line.line_type == DiffLineType::Add
					&& line.content.contains("line one changed")
			})
			.unwrap();
		component.update("file.txt".into(), false, diff);
		component.focus(true);
		component.update_selection(display_index_for_raw(
			&component,
			first_added_line,
		));
		draw_component_now(&component);
		let enter = Event::Key(KeyEvent::new(
			KeyCode::Enter,
			KeyModifiers::empty(),
		));
		assert!(matches!(
			component.event(&enter).unwrap(),
			EventState::Consumed
		));

		let staged = asyncgit::sync::diff::get_diff(
			&repo.borrow(),
			"file.txt",
			true,
			None,
		)
		.unwrap();
		let staged_lines = staged
			.hunks
			.iter()
			.flat_map(|h| h.lines.iter())
			.collect::<Vec<_>>();
		assert!(staged_lines
			.iter()
			.any(|line| line.line_type == DiffLineType::Add
				&& line.content.contains("line one changed")));
		assert!(!staged_lines
			.iter()
			.any(|line| line.line_type == DiffLineType::Add
				&& line.content.contains("line twenty changed")));

		// Refresh the component with the remaining worktree diff, then explicitly
		// select its hunk with x and confirm that Enter stages that hunk.
		let remaining = asyncgit::sync::diff::get_diff(
			&repo.borrow(),
			"file.txt",
			false,
			None,
		)
		.unwrap();
		let second_added_line = remaining
			.hunks
			.iter()
			.flat_map(|hunk| hunk.lines.iter())
			.position(|line| {
				line.line_type == DiffLineType::Add
					&& line.content.contains("line twenty changed")
			})
			.unwrap();
		component.update("file.txt".into(), false, remaining);
		draw_component_now(&component);
		component.update_selection(second_added_line);
		let select_hunk = Event::Key(KeyEvent::new(
			KeyCode::Char('x'),
			KeyModifiers::empty(),
		));
		assert!(matches!(
			component.event(&select_hunk).unwrap(),
			EventState::Consumed
		));
		assert!(component.selection_is_hunk);
		assert!(matches!(
			component.event(&enter).unwrap(),
			EventState::Consumed
		));

		let staged = asyncgit::sync::diff::get_diff(
			&repo.borrow(),
			"file.txt",
			true,
			None,
		)
		.unwrap();
		let staged_additions = staged
			.hunks
			.iter()
			.flat_map(|hunk| hunk.lines.iter())
			.filter(|line| line.line_type == DiffLineType::Add)
			.map(|line| line.content.as_ref())
			.collect::<Vec<_>>();
		assert!(staged_additions
			.iter()
			.any(|line| line.contains("line one changed")));
		assert!(staged_additions
			.iter()
			.any(|line| line.contains("line twenty changed")));
	}

	#[test]
	fn diff_component_opens_editor_for_current_file() {
		let env = Environment::test_env();
		let mut diff = DiffComponent::new(&env, false);

		diff.focus(true);
		diff.current.path = String::from("src/main.rs");

		let event = Event::Key(KeyEvent::new(
			KeyCode::Char('e'),
			KeyModifiers::empty(),
		));

		assert!(matches!(
			diff.event(&event).unwrap(),
			EventState::Consumed
		));

		let event = env.queue.pop();
		assert!(matches!(
			event,
			Some(InternalEvent::OpenExternalEditor(Some(path)))
				if path == "src/main.rs"
		));
	}
}
