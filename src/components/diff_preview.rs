//! A read-only viewport for typed native preview rows. Its rows intentionally
//! have no relationship to the native patch rows used by `DiffComponent`.

use super::{CommandBlocking, CommandInfo, Component, EventState};
use crate::{
	components::utils::scroll_vertical::VerticalScroll,
	keys::{key_match, SharedKeyConfig},
	ui::diff_highlight::{
		highlight_diff_rows, DiffHighlightInput, StyledRange,
		MAX_PAIR_LINE_BYTES,
	},
	ui::{
		diff_presentation::{
			inline_block_visual_lines, BlockVisualLine,
		},
		style::Theme,
	},
};
use anyhow::Result;
use asyncgit::AsyncGitNotification;
use asyncgit::{
	asyncjob::AsyncSingleJob,
	diff_preview::{
		DiffPreview, DiffPreviewCancellation, DiffPreviewJob,
		DiffPreviewRequest, DiffPreviewResult, InlineReplacement,
		InlineReplacementBlock, InlineSegmentKind, PreviewLine,
		PreviewLineKind, PreviewOptions, PreviewPreference,
		PreviewStatus,
	},
	structural::{StructuralAnalysis, StructuralRegion},
	sync::{RepoPath, RepoPathRef},
	DiffLineType, DiffParams,
};
use crossbeam_channel::{unbounded, Receiver, Sender};
use crossterm::event::{
	Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind,
};
use ratatui::{
	layout::{Position, Rect},
	style::{Color, Modifier, Style},
	text::{Line, Span, Text},
	widgets::{Block, Borders, Paragraph},
	Frame,
};
use std::{
	cell::{Cell, RefCell},
	collections::{HashMap, HashSet, VecDeque},
	path::PathBuf,
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc,
	},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

fn repo_identity(repo: &RepoPath) -> String {
	let workdir = repo.workdir().map_or_else(String::new, |path| {
		path.to_string_lossy().into_owned()
	});
	format!("{}\0{workdir}", repo.gitpath().to_string_lossy())
}

fn preview_hunk_indices(lines: &[PreviewLine]) -> Vec<usize> {
	let mut hunk = 0_usize;
	lines
		.iter()
		.map(|line| {
			if line.kind == PreviewLineKind::HunkHeader {
				hunk = hunk.saturating_add(1);
			}
			hunk
		})
		.collect()
}

#[allow(clippy::too_many_lines)] // Keep row budgeting and source mapping together.
fn build_preview_highlight_inputs(
	lines: &[PreviewLine],
	hunks: &[usize],
	prioritized_indices: &[usize],
) -> (
	Vec<DiffHighlightInput>,
	Vec<Option<PreviewHighlightMap>>,
	Vec<usize>,
) {
	let mut selected = Vec::new();
	let mut selected_rows = 0_usize;
	let mut selected_bytes = 0_usize;
	for &line_index in prioritized_indices {
		let Some(line) = lines.get(line_index) else {
			continue;
		};
		if selected.contains(&line_index) {
			continue;
		}
		let (row_count, copied) = preview_highlight_input_size(line);
		if selected_rows.saturating_add(row_count)
			> PREVIEW_HIGHLIGHT_MAX_ROWS
		{
			continue;
		}
		if selected_bytes.saturating_add(copied)
			> PREVIEW_HIGHLIGHT_MAX_BYTES
		{
			continue;
		}
		selected_rows += row_count;
		selected_bytes += copied;
		selected.push(line_index);
	}
	selected.sort_unstable();
	let mut inputs = Vec::new();
	let mut mappings = Vec::new();
	let mut copied_bytes = 0;
	for &line_index in &selected {
		let Some(line) = lines.get(line_index) else {
			continue;
		};
		let hunk = hunks.get(line_index).copied().unwrap_or_default();
		if let Some(replacement) = &line.replacement {
			push_preview_highlight_input(
				&mut inputs,
				&mut mappings,
				&mut copied_bytes,
				line_index,
				true,
				hunk,
				DiffLineType::Delete,
				0,
				&replacement.old_text,
			);
			push_preview_highlight_input(
				&mut inputs,
				&mut mappings,
				&mut copied_bytes,
				line_index,
				false,
				hunk,
				DiffLineType::Add,
				0,
				&replacement.new_text,
			);
			continue;
		}
		if let Some(block) = &line.block_replacement {
			for source_line in &block.old_lines {
				if let Some(text) =
					block.old_text.get(source_line.text_range.clone())
				{
					push_preview_highlight_input(
						&mut inputs,
						&mut mappings,
						&mut copied_bytes,
						line_index,
						true,
						hunk,
						DiffLineType::Delete,
						source_line.text_range.start,
						text,
					);
				}
			}
			for source_line in &block.new_lines {
				if let Some(text) =
					block.new_text.get(source_line.text_range.clone())
				{
					push_preview_highlight_input(
						&mut inputs,
						&mut mappings,
						&mut copied_bytes,
						line_index,
						false,
						hunk,
						DiffLineType::Add,
						source_line.text_range.start,
						text,
					);
				}
			}
			continue;
		}
		let (line_type, old_side) = match line.kind {
			PreviewLineKind::Context => (DiffLineType::None, false),
			PreviewLineKind::Addition => (DiffLineType::Add, false),
			PreviewLineKind::Deletion => (DiffLineType::Delete, true),
			PreviewLineKind::FileHeader
			| PreviewLineKind::HunkHeader
			| PreviewLineKind::Replacement
			| PreviewLineKind::BlockReplacement
			| PreviewLineKind::Metadata
			| PreviewLineKind::Notice => (DiffLineType::Header, false),
		};
		push_preview_highlight_input(
			&mut inputs,
			&mut mappings,
			&mut copied_bytes,
			line_index,
			old_side,
			hunk,
			line_type,
			0,
			&line.text,
		);
	}
	(inputs, mappings, selected)
}

fn preview_highlight_input_size(
	line: &PreviewLine,
) -> (usize, usize) {
	line.replacement.as_ref().map_or_else(
		|| {
			line.block_replacement.as_ref().map_or_else(
				|| {
					(
						1,
						copied_highlight_bytes(std::iter::once(
							line.text.as_str(),
						)),
					)
				},
				|block| {
					let texts = block
						.old_lines
						.iter()
						.filter_map(|source| {
							block
								.old_text
								.get(source.text_range.clone())
						})
						.chain(block.new_lines.iter().filter_map(
							|source| {
								block
									.new_text
									.get(source.text_range.clone())
							},
						))
						.collect::<Vec<_>>();
					(
						texts.len(),
						copied_highlight_bytes(texts.into_iter()),
					)
				},
			)
		},
		|replacement| {
			let texts = [
				replacement.old_text.as_str(),
				replacement.new_text.as_str(),
			];
			(2, copied_highlight_bytes(texts.into_iter()))
		},
	)
}

#[allow(clippy::too_many_arguments)] // Keeps source mapping with each bounded input.
fn push_preview_highlight_input(
	inputs: &mut Vec<DiffHighlightInput>,
	mappings: &mut Vec<Option<PreviewHighlightMap>>,
	copied_bytes: &mut usize,
	line_index: usize,
	old_side: bool,
	hunk: usize,
	line_type: DiffLineType,
	source_offset: usize,
	text: &str,
) {
	let content_len = text.len();
	let content = if content_len <= MAX_PAIR_LINE_BYTES
		&& copied_bytes.saturating_add(content_len)
			<= PREVIEW_HIGHLIGHT_MAX_BYTES
	{
		*copied_bytes = copied_bytes.saturating_add(content_len);
		text.to_owned()
	} else {
		String::new()
	};
	inputs.push(DiffHighlightInput {
		hunk,
		line_type,
		content,
		content_len,
	});
	mappings.push((line_type != DiffLineType::Header).then_some(
		PreviewHighlightMap {
			line_index,
			old_side,
			source_offset,
		},
	));
}

fn copied_highlight_bytes<'a>(
	texts: impl Iterator<Item = &'a str>,
) -> usize {
	texts
		.filter(|text| text.len() <= MAX_PAIR_LINE_BYTES)
		.map(str::len)
		.sum()
}

fn translate_syntax_ranges(
	mut ranges: Vec<StyledRange>,
	source_offset: usize,
) -> Vec<StyledRange> {
	for range in &mut ranges {
		range.range.start =
			range.range.start.saturating_add(source_offset);
		range.range.end =
			range.range.end.saturating_add(source_offset);
	}
	ranges
}

/// Owns async preview generations and a small bounded cache. Callers provide
/// exact native `DiffParams`; this controller never returns patch coordinates.
pub struct DiffPreviewController {
	job: AsyncSingleJob<DiffPreviewJob>,
	view: DiffPreviewView,
	current: Option<(String, DiffParams, u64, PreviewPreference)>,
	generation: u64,
	cancellation: Option<DiffPreviewCancellation>,
	cache: VecDeque<(
		String,
		DiffParams,
		u64,
		PreviewPreference,
		DiffPreview,
	)>,
	repo: RepoPathRef,
}

impl DiffPreviewController {
	pub fn new(
		repo: RepoPathRef,
		sender: Sender<asyncgit::AsyncGitNotification>,
		key_config: SharedKeyConfig,
	) -> Self {
		let mut view = DiffPreviewView::new();
		view.set_highlight_notification(sender.clone());
		view.set_key_config(key_config);
		view.set_label("selected file");
		Self {
			job: AsyncSingleJob::new(sender),
			view,
			current: None,
			generation: 0,
			cancellation: None,
			cache: VecDeque::new(),
			repo,
		}
	}

	pub const fn view(&self) -> &DiffPreviewView {
		&self.view
	}
	pub const fn view_mut(&mut self) -> &mut DiffPreviewView {
		&mut self.view
	}

	pub fn request(
		&mut self,
		params: DiffParams,
		_width: usize,
		preference: PreviewPreference,
		source_hash: u64,
	) {
		self.view.set_label(review_label(&params.path));
		let repo_path = self.repo.borrow().clone();
		let repo_id = repo_identity(&repo_path);
		if self.current.as_ref()
			== Some(&(
				repo_id.clone(),
				params.clone(),
				source_hash,
				preference,
			)) {
			return;
		}
		if let Some(cancel) = self.cancellation.take() {
			cancel.cancel();
		}
		self.job.cancel();
		let same_source =
			self.current.as_ref().is_some_and(|(r, p, h, _)| {
				r == &repo_id && p == &params && *h == source_hash
			});
		self.current = Some((
			repo_id.clone(),
			params.clone(),
			source_hash,
			preference,
		));
		self.generation = self.generation.wrapping_add(1);
		if let Some(index) =
			self.cache.iter().position(|(r, p, h, pref, _)| {
				*r == repo_id
					&& *p == params && *h == source_hash
					&& *pref == preference
			}) {
			if let Some((_, _, _, _, preview)) =
				self.cache.remove(index)
			{
				self.cache.push_front((
					repo_id.clone(),
					params.clone(),
					source_hash,
					preference,
					preview.clone(),
				));
				self.view.set_preview(
					DiffPreviewResult {
						request: DiffPreviewRequest {
							generation: self.generation,
							params,
							expected_native_hash: source_hash,
							options: PreviewOptions { preference },
						},
						preview,
					},
					source_hash,
				);
				return;
			}
		}
		self.view.set_pending_for(
			params.clone(),
			source_hash,
			same_source,
			"Rendering preview…",
		);
		let request = DiffPreviewRequest {
			generation: self.generation,
			params,
			expected_native_hash: source_hash,
			options: PreviewOptions { preference },
		};
		let (job, cancellation) =
			DiffPreviewJob::new(repo_path, request);
		self.cancellation = Some(cancellation);
		self.job.spawn(job);
	}

	pub fn update(&mut self) {
		self.view.apply_highlight_results();
		let Some(job) = self.job.take_last() else {
			return;
		};
		let Some(result) = job.result().cloned() else {
			return;
		};
		let valid = self.current.as_ref().is_some_and(
			|(_repo_id, params, hash, preference)| {
				result.request.generation == self.generation
					&& result.request.params == *params
					&& result.request.expected_native_hash == *hash
					&& result.request.options.preference
						== *preference
			},
		);
		if !valid {
			return;
		}
		if result.preview.status == PreviewStatus::Stale {
			if let Some((_repo_id, params, hash, _)) =
				self.current.clone()
			{
				self.view.set_pending_for(params, hash, true, "Source changed while rendering; refresh the selected diff");
			}
			self.current = None;
			self.cancellation = None;
			return;
		}
		let cacheable = !matches!(
			result.preview.status,
			PreviewStatus::CaptureFailed | PreviewStatus::Cancelled
		);
		if let Some((repo_id, params, hash, preference)) =
			self.current.clone()
		{
			if cacheable {
				self.cache.push_front((
					repo_id,
					params,
					hash,
					preference,
					result.preview.clone(),
				));
				while self.cache.len() > 8 {
					self.cache.pop_back();
				}
			}
		}
		self.cancellation = None;
		let source_hash =
			self.current.as_ref().map_or(0, |(_, _, hash, _)| *hash);
		self.view.set_preview(result, source_hash);
		self.view.apply_highlight_results();
	}

	pub fn is_pending(&self) -> bool {
		self.job.is_pending()
	}

	#[cfg(test)]
	pub const fn generation(&self) -> u64 {
		self.generation
	}

	pub fn clear(&mut self) {
		if let Some(cancel) = self.cancellation.take() {
			cancel.cancel();
		}
		self.job.cancel();
		self.current = None;
		self.generation = self.generation.wrapping_add(1);
		self.view.clear();
	}

	pub fn clear_with_message(&mut self, message: impl Into<String>) {
		self.clear();
		self.view.set_message(message);
	}
}

/// Read-only embedded renderer for typed native preview rows.
pub struct DiffPreviewView {
	preview: Option<DiffPreview>,
	number_width: usize,
	longest_line: Cell<usize>,
	source: Option<DiffParams>,
	source_hash: Option<u64>,
	accepted_generation: Option<u64>,
	accepted_preference: Option<PreviewPreference>,
	label: String,
	message: Option<String>,
	scroll: VerticalScroll,
	horizontal: usize,
	area: Cell<Rect>,
	key_config: Option<SharedKeyConfig>,
	fold_groups: Vec<FoldGroup>,
	folded: HashSet<u64>,
	fold_focus: Option<u64>,
	fold_source_initialized: bool,
	projected_rows: RefCell<Vec<ProjectedRow>>,
	hunk_indices: Vec<usize>,
	block_visual_lines: RefCell<HashMap<usize, Vec<BlockVisualLine>>>,
	block_layout_width: Cell<usize>,
	highlight_rows: RefCell<HashMap<usize, PreviewLineSyntax>>,
	highlight_requests: RefCell<HashSet<usize>>,
	highlight_window: Cell<Option<(usize, usize)>>,
	highlight_generation: Cell<u64>,
	highlight_window_generation: Cell<u64>,
	highlight_epoch: Arc<AtomicU64>,
	highlight_result_sender: Sender<PreviewHighlightResult>,
	highlight_result_receiver: Receiver<PreviewHighlightResult>,
	highlight_notification: Option<Sender<AsyncGitNotification>>,
}

#[derive(Clone, Debug, Default)]
struct PreviewLineSyntax {
	old: Vec<StyledRange>,
	new: Vec<StyledRange>,
}

struct PreviewHighlightResult {
	generation: u64,
	window_generation: u64,
	params: DiffParams,
	source_hash: u64,
	rows: Vec<(usize, PreviewLineSyntax)>,
}

#[derive(Clone, Copy)]
struct PreviewHighlightMap {
	line_index: usize,
	old_side: bool,
	source_offset: usize,
}

const PREVIEW_HIGHLIGHT_WINDOW_ROWS: usize = 256;
const PREVIEW_HIGHLIGHT_WINDOW_STRIDE: usize = 128;
const PREVIEW_HIGHLIGHT_CONTEXT_ROWS: usize = 48;
const PREVIEW_HIGHLIGHT_MAX_ROWS: usize =
	PREVIEW_HIGHLIGHT_WINDOW_ROWS + PREVIEW_HIGHLIGHT_CONTEXT_ROWS;
const PREVIEW_HIGHLIGHT_MAX_BYTES: usize = 128 * 1024;

impl Default for DiffPreviewView {
	fn default() -> Self {
		Self::new()
	}
}

impl DiffPreviewView {
	#[cfg(test)]
	pub fn has_preview(&self) -> bool {
		self.preview.is_some()
	}

	pub fn new() -> Self {
		let (highlight_result_sender, highlight_result_receiver) =
			unbounded();
		Self {
			preview: None,
			number_width: 1,
			longest_line: Cell::new(0),
			source: None,
			source_hash: None,
			accepted_generation: None,
			accepted_preference: None,
			label: "REVIEW".to_string(),
			message: Some("Preparing review…".to_string()),
			scroll: VerticalScroll::new(),
			horizontal: 0,
			area: Cell::new(Rect::default()),
			key_config: None,
			fold_groups: Vec::new(),
			folded: HashSet::new(),
			fold_focus: None,
			fold_source_initialized: false,
			projected_rows: RefCell::new(Vec::new()),
			hunk_indices: Vec::new(),
			block_visual_lines: RefCell::new(HashMap::new()),
			block_layout_width: Cell::new(72),
			highlight_rows: RefCell::new(HashMap::new()),
			highlight_requests: RefCell::new(HashSet::new()),
			highlight_window: Cell::new(None),
			highlight_generation: Cell::new(0),
			highlight_window_generation: Cell::new(0),
			highlight_epoch: Arc::new(AtomicU64::new(0)),
			highlight_result_sender,
			highlight_result_receiver,
			highlight_notification: None,
		}
	}

	fn set_highlight_notification(
		&mut self,
		sender: Sender<AsyncGitNotification>,
	) {
		self.highlight_notification = Some(sender);
	}

	fn set_key_config(&mut self, key_config: SharedKeyConfig) {
		self.key_config = Some(key_config);
	}

	fn invalidate_highlights(&self) {
		self.highlight_generation
			.set(self.highlight_generation.get().wrapping_add(1));
		let window_generation =
			self.highlight_window_generation.get().wrapping_add(1);
		self.highlight_window_generation.set(window_generation);
		self.highlight_epoch
			.store(window_generation, Ordering::Release);
		self.highlight_window.set(None);
		self.highlight_rows.borrow_mut().clear();
		self.highlight_requests.borrow_mut().clear();
	}

	fn apply_highlight_results(&self) {
		let Some(params) = self.source.as_ref() else {
			return;
		};
		let Some(source_hash) = self.source_hash else {
			return;
		};
		for result in self.highlight_result_receiver.try_iter() {
			if result.generation != self.highlight_generation.get()
				|| result.window_generation
					!= self.highlight_window_generation.get()
				|| &result.params != params
				|| result.source_hash != source_hash
			{
				continue;
			}
			let mut requested = self.highlight_requests.borrow_mut();
			let mut highlights = self.highlight_rows.borrow_mut();
			for (line_index, syntax) in result.rows {
				requested.remove(&line_index);
				highlights.insert(line_index, syntax);
			}
		}
	}

	#[allow(clippy::too_many_lines)] // One bounded worker request lifecycle.
	fn schedule_highlights(
		&self,
		visible_start: usize,
		visible_end: usize,
		theme: &Theme,
	) {
		let Some(sender_notification) =
			self.highlight_notification.as_ref()
		else {
			return;
		};
		let Some(preview) = &self.preview else { return };
		let Some(params) = self.source.as_ref() else {
			return;
		};
		let Some(source_hash) = self.source_hash else {
			return;
		};
		let projected = self.projected_rows.borrow();
		let mut visible = projected
			[visible_start..visible_end.min(projected.len())]
			.iter()
			.filter_map(|row| match row {
				ProjectedRow::Patch(index)
				| ProjectedRow::BlockLine(index, _) => Some(*index),
				ProjectedRow::RegionHeader(_)
				| ProjectedRow::FoldSummary(_) => None,
			})
			.collect::<Vec<_>>();
		if visible.is_empty() {
			return;
		}
		visible.sort_unstable();
		visible.dedup();
		let first_visible = visible[0];
		let last_visible = *visible.last().unwrap_or(&first_visible);
		let mut window_start = first_visible
			/ PREVIEW_HIGHLIGHT_WINDOW_STRIDE
			* PREVIEW_HIGHLIGHT_WINDOW_STRIDE;
		let mut window_end = window_start
			.saturating_add(PREVIEW_HIGHLIGHT_WINDOW_ROWS)
			.min(preview.lines.len());
		if last_visible >= window_end {
			window_end = last_visible.saturating_add(1);
			window_start = window_end
				.saturating_sub(PREVIEW_HIGHLIGHT_WINDOW_ROWS);
		}
		let window = (window_start, window_end);
		if self.highlight_window.get() != Some(window) {
			self.highlight_window.set(Some(window));
			let window_generation = self
				.highlight_window_generation
				.get()
				.wrapping_add(1);
			self.highlight_window_generation.set(window_generation);
			self.highlight_epoch
				.store(window_generation, Ordering::Release);
			self.highlight_rows.borrow_mut().retain(|index, _| {
				*index >= window_start && *index < window_end
			});
			self.highlight_requests.borrow_mut().clear();
		}
		let first_missing = {
			let highlights = self.highlight_rows.borrow();
			let requests = self.highlight_requests.borrow();
			(window_start..window_end).find(|index| {
				!highlights.contains_key(index)
					&& !requests.contains(index)
			})
		};
		let Some(_) = first_missing else { return };
		let context_start = window_start
			.saturating_sub(PREVIEW_HIGHLIGHT_CONTEXT_ROWS);
		let mut candidates = visible;
		candidates.extend(context_start..window_start);
		candidates.extend(window_start..window_end);
		{
			let highlights = self.highlight_rows.borrow();
			let requests = self.highlight_requests.borrow();
			candidates.retain(|index| {
				!highlights.contains_key(index)
					&& !requests.contains(index)
			});
		}
		let (inputs, mappings, selected) =
			build_preview_highlight_inputs(
				&preview.lines,
				&self.hunk_indices,
				&candidates,
			);
		if inputs.is_empty() {
			return;
		}
		self.highlight_requests
			.borrow_mut()
			.extend(selected.iter().copied());
		let sender_result = self.highlight_result_sender.clone();
		let sender_notification = sender_notification.clone();
		let path = PathBuf::from(&params.path);
		let params = params.clone();
		let theme_name = theme.get_syntax();
		let generation = self.highlight_generation.get();
		let window_generation =
			self.highlight_window_generation.get();
		let epoch = Arc::clone(&self.highlight_epoch);
		rayon_core::spawn(move || {
			if epoch.load(Ordering::Acquire) != window_generation {
				return;
			}
			let highlighted =
				highlight_diff_rows(&path, &inputs, &theme_name);
			let mut rows = selected
				.iter()
				.map(|index| (*index, PreviewLineSyntax::default()))
				.collect::<Vec<_>>();
			for (mapping, result) in
				mappings.into_iter().zip(highlighted)
			{
				let Some(mapping) = mapping else { continue };
				let Some((_, row)) = rows
					.iter_mut()
					.find(|(index, _)| *index == mapping.line_index)
				else {
					continue;
				};
				let syntax = translate_syntax_ranges(
					result.syntax,
					mapping.source_offset,
				);
				if mapping.old_side {
					row.old.extend(syntax);
				} else {
					row.new.extend(syntax);
				}
			}
			if epoch.load(Ordering::Acquire) != window_generation {
				return;
			}
			let _ = sender_result.send(PreviewHighlightResult {
				generation,
				window_generation,
				params,
				source_hash,
				rows,
			});
			let _ = sender_notification
				.send(AsyncGitNotification::DiffPreview);
		});
	}

	/// Accept only after the caller has checked request generation and params.
	pub fn set_preview(
		&mut self,
		result: DiffPreviewResult,
		source_hash: u64,
	) {
		self.invalidate_highlights();
		let same_source = self.source.as_ref()
			== Some(&result.request.params)
			&& self.source_hash == Some(source_hash);
		if !same_source {
			self.folded.clear();
			self.fold_focus = None;
			self.fold_source_initialized = false;
		}
		self.source = Some(result.request.params);
		self.source_hash = Some(source_hash);
		self.accepted_generation = Some(result.request.generation);
		self.accepted_preference =
			Some(result.request.options.preference);
		let has_structural_analysis =
			result.preview.structural.is_some();
		*self.block_visual_lines.borrow_mut() = result
			.preview
			.lines
			.iter()
			.enumerate()
			.filter_map(|(index, line)| {
				line.block_replacement.as_ref().map(|block| {
					(
						index,
						inline_block_visual_lines(
							block,
							self.block_layout_width.get(),
						),
					)
				})
			})
			.collect();
		self.update_line_metrics(&result.preview.lines);
		self.hunk_indices =
			preview_hunk_indices(&result.preview.lines);
		self.preview = Some(result.preview);
		let accepted_analysis =
			self.source.as_ref().and_then(|params| {
				self.structural_analysis_for(params, source_hash)
			});
		let fold_groups =
			match (accepted_analysis, self.preview.as_ref()) {
				(Some(analysis), Some(preview)) => {
					build_fold_groups(&preview.lines, analysis)
				}
				_ => Vec::new(),
			};
		self.fold_groups = fold_groups;
		self.longest_line.set(
			self.longest_line.get().max(
				self.fold_groups
					.iter()
					.map(|group| {
						UnicodeWidthStr::width(
							fold_summary(group, false).as_str(),
						)
					})
					.max()
					.unwrap_or(0),
			),
		);
		let group_ids = self
			.fold_groups
			.iter()
			.map(|group| group.region.id)
			.collect::<HashSet<_>>();
		if has_structural_analysis {
			self.folded.retain(|id| group_ids.contains(id));
		}
		if has_structural_analysis
			&& self
				.fold_focus
				.is_some_and(|id| !group_ids.contains(&id))
		{
			self.fold_focus = None;
		}
		if self.fold_focus.is_none() {
			self.fold_focus =
				self.fold_groups.first().map(|g| g.region.id);
		}
		if has_structural_analysis {
			for group in &self.fold_groups {
				if !self.fold_source_initialized
					&& default_fold(group)
				{
					self.folded.insert(group.region.id);
				}
			}
			self.fold_source_initialized = true;
		}
		self.rebuild_projection();
		self.message = None;
		if !same_source {
			self.scroll.reset();
			self.horizontal = 0;
		}
	}

	fn update_line_metrics(&mut self, lines: &[PreviewLine]) {
		self.number_width = lines
			.iter()
			.enumerate()
			.flat_map(|(index, line)| {
				let mut numbers =
					vec![line.old_lineno, line.new_lineno];
				if let Some(rows) =
					self.block_visual_lines.borrow().get(&index)
				{
					numbers.extend(rows.iter().flat_map(|row| {
						[row.old_lineno, row.new_lineno]
					}));
				}
				numbers
			})
			.flatten()
			.map(|number| number.to_string().len())
			.max()
			.unwrap_or(1)
			.clamp(1, 10);
		self.longest_line.set(lines
			.iter()
			.enumerate()
			.map(|(index, line)| {
				let content_width = self
					.block_visual_lines
					.borrow()
					.get(&index)
					.map_or_else(
						|| {
							line.replacement.as_ref().map_or_else(
								|| UnicodeWidthStr::width(line.text.as_str()),
								replacement_display_width,
							)
						},
						|rows| {
							rows.iter()
								.map(|row| row.width)
								.max()
								.unwrap_or_default()
						},
					);
				if line.old_lineno.is_some()
					|| line.new_lineno.is_some()
				{
					let gutter_width = crate::ui::diff_presentation::line_gutter_width(
						self.number_width,
					)
					.saturating_add(1);
					content_width.saturating_add(gutter_width)
				} else {
					content_width.saturating_add(2)
				}
			})
			.max()
			.unwrap_or(0));
	}

	fn set_pending_for(
		&mut self,
		source: DiffParams,
		source_hash: u64,
		same_source: bool,
		message: impl Into<String>,
	) {
		self.invalidate_highlights();
		self.preview = None;
		self.number_width = 1;
		self.longest_line.set(0);
		self.projected_rows.borrow_mut().clear();
		self.hunk_indices.clear();
		self.block_visual_lines.borrow_mut().clear();
		self.source = Some(source);
		self.source_hash = Some(source_hash);
		self.accepted_generation = None;
		self.accepted_preference = None;
		if !same_source {
			self.fold_groups.clear();
			self.folded.clear();
			self.fold_focus = None;
			self.fold_source_initialized = false;
			self.projected_rows.borrow_mut().clear();
		}
		self.message = Some(message.into());
		if !same_source {
			self.scroll.reset();
			self.horizontal = 0;
		}
	}

	pub fn clear(&mut self) {
		self.invalidate_highlights();
		self.preview = None;
		self.fold_groups.clear();
		self.folded.clear();
		self.fold_focus = None;
		self.fold_source_initialized = false;
		self.projected_rows.borrow_mut().clear();
		self.hunk_indices.clear();
		self.block_visual_lines.borrow_mut().clear();
		self.number_width = 1;
		self.longest_line.set(0);
		self.source = None;
		self.source_hash = None;
		self.accepted_generation = None;
		self.accepted_preference = None;
		self.message = None;
		"selected file".clone_into(&mut self.label);
		self.scroll.reset();
		self.horizontal = 0;
	}

	fn set_message(&mut self, message: impl Into<String>) {
		self.invalidate_highlights();
		self.preview = None;
		self.fold_groups.clear();
		self.folded.clear();
		self.fold_focus = None;
		self.fold_source_initialized = false;
		self.projected_rows.borrow_mut().clear();
		self.hunk_indices.clear();
		self.block_visual_lines.borrow_mut().clear();
		self.number_width = 1;
		self.longest_line.set(0);
		self.source = None;
		self.source_hash = None;
		self.accepted_generation = None;
		self.accepted_preference = None;
		self.message = Some(message.into());
		self.scroll.reset();
		self.horizontal = 0;
	}

	fn toggle_fold(&mut self, id: u64) {
		if !self.folded.remove(&id) {
			if !self
				.fold_groups
				.iter()
				.any(|group| group.region.id == id)
			{
				return;
			}
			self.folded.insert(id);
		}
		self.fold_focus = Some(id);
		self.rebuild_projection();
	}

	fn rebuild_projection(&self) {
		*self.projected_rows.borrow_mut() =
			self.preview.as_ref().map_or_else(Vec::new, |preview| {
				let rows = project_rows(
					preview.lines.len(),
					&self.fold_groups,
					&self.folded,
				);
				expand_block_rows(
					rows,
					&self.block_visual_lines.borrow(),
				)
			});
	}

	#[allow(clippy::too_many_lines)] // Resize rebuild and source-anchor restoration are one operation.
	fn update_block_layout(&self, viewport_width: usize) {
		let text_width = viewport_width
			.saturating_sub(
				crate::ui::diff_presentation::line_gutter_width(
					self.number_width,
				)
				.saturating_add(1),
			)
			.max(1);
		if self.block_layout_width.get() == text_width {
			return;
		}
		let Some(preview) = self.preview.as_ref() else {
			self.block_layout_width.set(text_width);
			return;
		};
		let old_rows = self.projected_rows.borrow();
		let old_block_lines = self.block_visual_lines.borrow();
		let anchor =
			old_rows.get(self.scroll.get_top()).and_then(|row| {
				match row {
					ProjectedRow::Patch(index) => {
						Some((*index, None))
					}
					ProjectedRow::BlockLine(index, visual_index) => {
						let piece = old_block_lines
							.get(index)?
							.get(*visual_index)?
							.pieces
							.iter()
							.find(|piece| !piece.is_space)?;
						if !piece.new_range.is_empty() {
							Some((
								*index,
								Some((false, piece.new_range.start)),
							))
						} else if !piece.old_range.is_empty() {
							Some((
								*index,
								Some((true, piece.old_range.start)),
							))
						} else {
							Some((*index, None))
						}
					}
					_ => None,
				}
			});
		drop(old_block_lines);
		drop(old_rows);
		let mut blocks = HashMap::new();
		for (index, line) in preview.lines.iter().enumerate() {
			if let Some(block) = line.block_replacement.as_ref() {
				blocks.insert(
					index,
					inline_block_visual_lines(block, text_width),
				);
			}
		}
		*self.block_visual_lines.borrow_mut() = blocks;
		let rows = expand_block_rows(
			project_rows(
				preview.lines.len(),
				&self.fold_groups,
				&self.folded,
			),
			&self.block_visual_lines.borrow(),
		);
		self.longest_line.set(
			preview
				.lines
				.iter()
				.enumerate()
				.map(|(index, line)| {
					let content = self
						.block_visual_lines
						.borrow()
						.get(&index)
						.map_or_else(
							|| {
								line.replacement.as_ref().map_or_else(
									|| {
										UnicodeWidthStr::width(
											line.text.as_str(),
										)
									},
									replacement_display_width,
								)
							},
							|lines| {
								lines
									.iter()
									.map(|line| line.width)
									.max()
									.unwrap_or_default()
							},
						);
					content.saturating_add(
						crate::ui::diff_presentation::line_gutter_width(
							self.number_width,
						)
						.saturating_add(1),
					)
				})
				.max()
				.unwrap_or_default(),
		);
		*self.projected_rows.borrow_mut() = rows;
		self.block_layout_width.set(text_width);
		if let Some((anchor_index, anchor_source)) = anchor {
			let visual_target =
				anchor_source.and_then(|(old_side, offset)| {
					self.block_visual_lines
						.borrow()
						.get(&anchor_index)
						.and_then(|lines| {
							lines.iter().position(|line| {
								line.pieces.iter().any(|piece| {
									let range = if old_side {
										&piece.old_range
									} else {
										&piece.new_range
									};
									!piece.is_space
										&& range.start <= offset && offset
										< range.end
								})
							})
						})
				});
			if let Some(row) = self
				.projected_rows
				.borrow()
				.iter()
				.position(|row| match row {
					ProjectedRow::Patch(index) => {
						*index == anchor_index
					}
					ProjectedRow::BlockLine(index, visual_index) => {
						*index == anchor_index
							&& visual_target.is_none_or(|target| {
								*visual_index == target
							})
					}
					_ => false,
				}) {
				self.scroll.set_top(row);
			}
		}
	}

	fn toggle_focused_fold(&mut self) -> bool {
		let Some(id) = self.fold_focus else {
			return false;
		};
		self.toggle_fold(id);
		true
	}

	fn focus_fold_near_scroll(&mut self) {
		let top = self.scroll.get_top();
		let candidate =
			self.projected_rows.borrow().iter().skip(top).find_map(
				|row| match row {
					ProjectedRow::RegionHeader(index)
					| ProjectedRow::FoldSummary(index) => {
						Some(self.fold_groups[*index].region.id)
					}
					ProjectedRow::Patch(_)
					| ProjectedRow::BlockLine(_, _) => None,
				},
			);
		self.fold_focus = candidate.or_else(|| {
			self.fold_groups.last().map(|group| group.region.id)
		});
	}

	pub fn set_label(&mut self, label: impl Into<String>) {
		self.label = label.into();
	}

	/// Syntax regions belong to the accepted source snapshot only.
	pub fn structural_analysis(&self) -> Option<&StructuralAnalysis> {
		self.preview.as_ref()?.structural.as_ref()
	}

	/// Returns metadata only when the accepted snapshot matches the caller's
	/// current source identity. Pending, cleared, and stale sources return None.
	pub fn structural_analysis_for(
		&self,
		params: &DiffParams,
		source_hash: u64,
	) -> Option<&StructuralAnalysis> {
		(self.source.as_ref() == Some(params)
			&& self.source_hash == Some(source_hash))
		.then(|| self.structural_analysis())
		.flatten()
	}

	/// Returns validated prose replacement metadata for the accepted source.
	/// Consumers must still map line numbers, text, and patch indexes back to
	/// their own raw rows before using it for any interaction.
	pub fn block_replacements_for<'a>(
		&'a self,
		params: &DiffParams,
		source_hash: u64,
	) -> Option<
		impl Iterator<Item = (usize, &'a InlineReplacementBlock)> + 'a,
	> {
		if self.source.as_ref() != Some(params)
			|| self.source_hash != Some(source_hash)
		{
			return None;
		}
		Some(
			self.preview
				.as_ref()?
				.lines
				.iter()
				.enumerate()
				.filter_map(|(index, line)| {
					line.block_replacement
						.as_ref()
						.map(|block| (index, block))
				}),
		)
	}

	/// Returns backend-aligned one-line replacement metadata for the accepted
	/// source snapshot. The backend owns prose/fence classification; consumers
	/// must map each row back to their exact source before using it.
	pub fn replacements_for<'a>(
		&'a self,
		params: &DiffParams,
		source_hash: u64,
	) -> Option<
		impl Iterator<Item = (usize, &'a InlineReplacement)> + 'a,
	> {
		if self.source.as_ref() != Some(params)
			|| self.source_hash != Some(source_hash)
		{
			return None;
		}
		Some(
			self.preview
				.as_ref()?
				.lines
				.iter()
				.enumerate()
				.filter_map(|(index, line)| {
					line.replacement
						.as_ref()
						.map(|replacement| (index, replacement))
				}),
		)
	}

	/// Exact accepted preview generation for the validated source snapshot.
	pub fn preview_generation_for(
		&self,
		params: &DiffParams,
		source_hash: u64,
	) -> Option<u64> {
		(self.source.as_ref() == Some(params)
			&& self.source_hash == Some(source_hash))
		.then_some(self.accepted_generation)
		.flatten()
	}

	#[allow(clippy::too_many_lines)] // Keep the viewport render path in one place.
	pub fn draw(
		&self,
		f: &mut Frame,
		area: Rect,
		theme: &Theme,
		focused: bool,
	) {
		self.area.set(area);
		let status = self
			.preview
			.as_ref()
			.map(|preview| {
				preview_label(
					preview,
					self.accepted_preference
						.unwrap_or(PreviewPreference::Unified),
				)
			})
			.unwrap_or_default();
		let title = if status.is_empty() {
			format!("REVIEW · {}", self.label)
		} else {
			format!("REVIEW · {status} · {}", self.label)
		};
		let block = Block::default()
			.title(title)
			.borders(Borders::ALL)
			.border_style(theme.block(focused));
		let inner = block.inner(area);
		self.update_block_layout(inner.width as usize);
		let extra_message =
			self.preview.as_ref().and_then(|p| p.message.as_ref());
		let preview_lines = self.projected_rows.borrow().len();
		let line_count = preview_lines
			+ usize::from(
				self.message.is_some() && preview_lines == 0,
			) + usize::from(extra_message.is_some());
		let top = self
			.scroll
			.update_no_selection(line_count, inner.height as usize);
		let width = inner.width as usize;
		let horizontal = self
			.horizontal
			.min(self.longest_line.get().saturating_sub(width));
		self.schedule_highlights(
			top,
			top.saturating_add(inner.height as usize),
			theme,
		);
		let mut visible = Vec::with_capacity(inner.height as usize);
		if let Some(preview) = &self.preview {
			let projected = self.projected_rows.borrow();
			for row in
				projected.iter().skip(top).take(inner.height as usize)
			{
				match row {
					ProjectedRow::Patch(index) => {
						let line = &preview.lines[*index];
						let highlights = self.highlight_rows.borrow();
						let (rendered, fixed_prefix) =
							preview_line_for_width_with_highlights(
								line,
								self.number_width,
								theme,
								width,
								highlights.get(index),
							);
						visible.push(
							horizontal_slice_with_fixed_prefix(
								rendered,
								fixed_prefix,
								horizontal,
								width,
							),
						);
					}
					ProjectedRow::BlockLine(index, line_index) => {
						let Some(block) = preview.lines[*index]
							.block_replacement
							.as_ref()
						else {
							continue;
						};
						let block_layout =
							self.block_visual_lines.borrow();
						let Some(block_lines) =
							block_layout.get(index)
						else {
							continue;
						};
						let highlights = self.highlight_rows.borrow();
						let (rendered, fixed_prefix) =
							preview_block_visual_line(
								block,
								&block_lines[*line_index],
								self.number_width,
								theme,
								width,
								highlights.get(index),
							);
						visible.push(
							horizontal_slice_with_fixed_prefix(
								rendered,
								fixed_prefix,
								horizontal,
								width,
							),
						);
					}
					ProjectedRow::RegionHeader(group_index)
					| ProjectedRow::FoldSummary(group_index) => {
						let group = &self.fold_groups[*group_index];
						let expanded = matches!(
							row,
							ProjectedRow::RegionHeader(_)
						);
						let text = fold_summary(group, expanded);
						visible.push(Line::from(Span::styled(
							text,
							if expanded {
								theme.diff_line(
									DiffLineType::Header,
									false,
								)
							} else {
								Style::default()
									.fg(Color::Yellow)
									.add_modifier(Modifier::BOLD)
							},
						)));
					}
				}
			}
		}
		if visible.is_empty() {
			if let Some(message) = &self.message {
				visible.push(Line::from(message.clone()));
			}
		}
		if visible.len() < inner.height as usize {
			if let Some(message) = extra_message {
				visible.push(Line::from(Span::styled(
					format!("  {message}"),
					Style::default()
						.fg(Color::DarkGray)
						.add_modifier(Modifier::ITALIC),
				)));
			}
		}
		f.render_widget(block, area);
		f.render_widget(Paragraph::new(Text::from(visible)), inner);
	}
}

#[cfg(test)]
fn preview_line<'a>(
	line: &'a PreviewLine,
	number_width: usize,
	theme: &Theme,
) -> Line<'a> {
	preview_line_with_prefix(line, number_width, theme).0
}

#[cfg(test)]
fn preview_line_with_prefix<'a>(
	line: &'a PreviewLine,
	number_width: usize,
	theme: &Theme,
) -> (Line<'a>, usize) {
	preview_line_for_width(line, number_width, theme, usize::MAX)
}

#[cfg(test)]
fn preview_line_for_width<'a>(
	line: &'a PreviewLine,
	number_width: usize,
	theme: &Theme,
	available_width: usize,
) -> (Line<'a>, usize) {
	preview_line_for_width_with_highlights(
		line,
		number_width,
		theme,
		available_width,
		None,
	)
}

#[allow(clippy::too_many_lines)] // Row styling and fixed-gutter clipping stay together.
fn preview_line_for_width_with_highlights<'a>(
	line: &'a PreviewLine,
	number_width: usize,
	theme: &Theme,
	available_width: usize,
	highlights: Option<&'a PreviewLineSyntax>,
) -> (Line<'a>, usize) {
	if let Some(block) = &line.block_replacement {
		let visuals = inline_block_visual_lines(
			block,
			available_width.saturating_sub(
				crate::ui::diff_presentation::line_gutter_width(
					number_width,
				)
				.saturating_add(1),
			),
		);
		return visuals.first().map_or_else(
			|| {
				(
					Line::default(),
					crate::ui::diff_presentation::line_gutter_width(
						number_width,
					)
					.saturating_add(1),
				)
			},
			|visual| {
				preview_block_visual_line(
					block,
					visual,
					number_width,
					theme,
					available_width,
					highlights,
				)
			},
		);
	}
	if let Some(replacement) = &line.replacement {
		let dual_gutter_width =
			crate::ui::diff_presentation::line_gutter_width(
				number_width,
			);
		let show_dual =
			available_width >= dual_gutter_width.saturating_add(3);
		let (gutter, fixed_prefix) = if show_dual {
			(
				crate::ui::diff_presentation::format_line_gutter(
					replacement.old_lineno,
					replacement.new_lineno,
					number_width,
				),
				dual_gutter_width + 1,
			)
		} else {
			let active = replacement
				.new_lineno
				.or(replacement.old_lineno)
				.map_or_else(String::new, |n| n.to_string());
			(format!("{active:>number_width$}│ "), number_width + 2)
		};
		let mut spans =
			vec![Span::styled(gutter, theme.diff_line_gutter(false))];
		if show_dual {
			spans.push(Span::raw(" "));
		}
		append_replacement_text(
			&mut spans,
			replacement,
			highlights,
			theme,
		);
		return (Line::from(spans), fixed_prefix);
	}
	let line_type = match line.kind {
		PreviewLineKind::Addition => DiffLineType::Add,
		PreviewLineKind::Deletion => DiffLineType::Delete,
		PreviewLineKind::Context => DiffLineType::None,
		PreviewLineKind::Replacement
		| PreviewLineKind::BlockReplacement
		| PreviewLineKind::FileHeader
		| PreviewLineKind::HunkHeader
		| PreviewLineKind::Metadata
		| PreviewLineKind::Notice => DiffLineType::Header,
	};
	let style = theme.diff_line(line_type, false);
	let mut spans = Vec::new();
	if line.old_lineno.is_some() || line.new_lineno.is_some() {
		let gutter = crate::ui::diff_presentation::format_line_gutter(
			line.old_lineno,
			line.new_lineno,
			number_width,
		);
		spans.push(Span::styled(
			gutter,
			theme.diff_line_gutter(false),
		));
		spans.push(Span::raw(" "));
	} else {
		let marker = match line.kind {
			PreviewLineKind::HunkHeader => "› ",
			PreviewLineKind::FileHeader
			| PreviewLineKind::Context
			| PreviewLineKind::Addition
			| PreviewLineKind::Deletion
			| PreviewLineKind::Replacement
			| PreviewLineKind::BlockReplacement => "  ",
			PreviewLineKind::Metadata | PreviewLineKind::Notice => {
				"! "
			}
		};
		spans.push(Span::styled(marker, style));
	}
	append_syntax_text(
		&mut spans,
		&line.text,
		0,
		&line.changed_ranges,
		highlights.map_or(&[][..], |highlight| {
			if line.kind == PreviewLineKind::Deletion {
				&highlight.old
			} else {
				&highlight.new
			}
		}),
		line_type,
		style,
		theme,
		false,
	);
	let prefix_width = if line.old_lineno.is_some()
		|| line.new_lineno.is_some()
	{
		crate::ui::diff_presentation::line_gutter_width(number_width)
			.saturating_add(1)
	} else {
		2
	};
	(Line::from(spans), prefix_width)
}

fn preview_block_visual_line<'a>(
	block: &'a InlineReplacementBlock,
	visual: &BlockVisualLine,
	number_width: usize,
	theme: &Theme,
	available_width: usize,
	highlights: Option<&'a PreviewLineSyntax>,
) -> (Line<'a>, usize) {
	let gutter_width =
		crate::ui::diff_presentation::line_gutter_width(number_width);
	let show_dual = available_width >= gutter_width.saturating_add(3);
	let (gutter, fixed_prefix) = if show_dual {
		(
			crate::ui::diff_presentation::format_line_gutter(
				visual.old_lineno,
				visual.new_lineno,
				number_width,
			),
			gutter_width + 1,
		)
	} else {
		let active = visual
			.new_lineno
			.or(visual.old_lineno)
			.map_or_else(String::new, |number| number.to_string());
		(format!("{active:>number_width$}│ "), number_width + 2)
	};
	let mut spans =
		vec![Span::styled(gutter, theme.diff_line_gutter(false))];
	if show_dual {
		spans.push(Span::raw(" "));
	}
	for piece in &visual.pieces {
		if piece.is_space {
			spans.push(Span::raw(" "));
			continue;
		}
		let (source, range, syntax, line_type, changed) =
			match piece.kind {
				InlineSegmentKind::Shared => (
					&block.new_text,
					&piece.new_range,
					highlights.map_or(&[][..], |value| &value.new),
					DiffLineType::None,
					false,
				),
				InlineSegmentKind::Removed => (
					&block.old_text,
					&piece.old_range,
					highlights.map_or(&[][..], |value| &value.old),
					DiffLineType::Delete,
					true,
				),
				InlineSegmentKind::Added => (
					&block.new_text,
					&piece.new_range,
					highlights.map_or(&[][..], |value| &value.new),
					DiffLineType::Add,
					true,
				),
			};
		if let Some(text) = source.get(range.clone()) {
			append_syntax_text(
				&mut spans,
				text,
				range.start,
				&[],
				syntax,
				line_type,
				theme.diff_line(line_type, false),
				theme,
				changed,
			);
		}
	}
	(Line::from(spans), fixed_prefix)
}

fn append_replacement_text<'a>(
	spans: &mut Vec<Span<'a>>,
	replacement: &'a InlineReplacement,
	highlights: Option<&PreviewLineSyntax>,
	theme: &Theme,
) {
	for segment in &replacement.segments {
		let (text, source_range, syntax, line_type) = match segment
			.kind
		{
			InlineSegmentKind::Shared => (
				replacement.new_text.get(segment.new_range.clone()),
				segment.new_range.start,
				highlights
					.map_or(&[][..], |highlight| &highlight.new),
				DiffLineType::None,
			),
			InlineSegmentKind::Removed => (
				replacement.old_text.get(segment.old_range.clone()),
				segment.old_range.start,
				highlights
					.map_or(&[][..], |highlight| &highlight.old),
				DiffLineType::Delete,
			),
			InlineSegmentKind::Added => (
				replacement.new_text.get(segment.new_range.clone()),
				segment.new_range.start,
				highlights
					.map_or(&[][..], |highlight| &highlight.new),
				DiffLineType::Add,
			),
		};
		if let Some(text) = text.filter(|text| !text.is_empty()) {
			append_syntax_text(
				spans,
				text,
				source_range,
				&[],
				syntax,
				line_type,
				theme.diff_line(line_type, false),
				theme,
				matches!(
					segment.kind,
					InlineSegmentKind::Removed
						| InlineSegmentKind::Added
				),
			);
		}
	}
}

#[allow(clippy::too_many_arguments)] // Styling needs source, semantic, and syntax context.
fn append_syntax_text<'a>(
	spans: &mut Vec<Span<'a>>,
	text: &'a str,
	source_offset: usize,
	changed_ranges: &[std::ops::Range<usize>],
	syntax_ranges: &[StyledRange],
	line_type: DiffLineType,
	base: Style,
	theme: &Theme,
	force_changed: bool,
) {
	let mut boundaries = vec![0, text.len()];
	for range in changed_ranges
		.iter()
		.chain(syntax_ranges.iter().map(|range| &range.range))
	{
		let start = range.start.max(source_offset);
		let end =
			range.end.min(source_offset.saturating_add(text.len()));
		if start < end {
			boundaries
				.extend([start - source_offset, end - source_offset]);
		}
	}
	boundaries.sort_unstable();
	boundaries.dedup();
	for boundary in boundaries.windows(2) {
		let start = boundary[0];
		let end = boundary[1];
		if start >= end {
			continue;
		}
		let source_position = source_offset + start;
		let changed = force_changed
			|| changed_ranges.iter().any(|range| {
				range.start <= source_position
					&& source_position < range.end
			});
		let syntax = syntax_ranges.iter().find(|range| {
			range.range.start <= source_position
				&& source_position < range.range.end
		});
		let mut style = if changed {
			theme.diff_inline_emphasis(
				line_type,
				syntax.and_then(|range| range.style.fg),
				false,
				false,
			)
		} else {
			base
		};
		if let Some(syntax) = syntax {
			style = if changed {
				style.add_modifier(syntax.style.add_modifier)
			} else {
				base.patch(syntax.style)
			};
		}
		match (line_type, changed) {
			(DiffLineType::Delete, true) => {
				style = style
					.remove_modifier(Modifier::UNDERLINED)
					.add_modifier(Modifier::CROSSED_OUT);
			}
			(DiffLineType::Add, true) => {
				style = style
					.remove_modifier(Modifier::CROSSED_OUT)
					.add_modifier(Modifier::UNDERLINED);
			}
			_ => {}
		}
		spans.push(Span::styled(&text[start..end], style));
	}
}

fn replacement_display_width(
	replacement: &InlineReplacement,
) -> usize {
	replacement.segments.iter().fold(0, |width, segment| {
		let text = match segment.kind {
			InlineSegmentKind::Shared | InlineSegmentKind::Added => {
				replacement.new_text.get(segment.new_range.clone())
			}
			InlineSegmentKind::Removed => {
				replacement.old_text.get(segment.old_range.clone())
			}
		};
		width.saturating_add(text.map_or(0, UnicodeWidthStr::width))
	})
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FoldGroup {
	region: StructuralRegion,
	first_row: usize,
	last_row: usize,
	visible_rows: usize,
	patch_rows: usize,
	additions: usize,
	deletions: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectedRow {
	Patch(usize),
	BlockLine(usize, usize),
	RegionHeader(usize),
	FoldSummary(usize),
}

fn expand_block_rows(
	rows: Vec<ProjectedRow>,
	blocks: &HashMap<usize, Vec<BlockVisualLine>>,
) -> Vec<ProjectedRow> {
	let mut expanded = Vec::with_capacity(rows.len());
	for row in rows {
		match row {
			ProjectedRow::Patch(index) => {
				if let Some(lines) = blocks.get(&index) {
					expanded.extend((0..lines.len()).map(|line| {
						ProjectedRow::BlockLine(index, line)
					}));
				} else {
					expanded.push(row);
				}
			}
			other => expanded.push(other),
		}
	}
	expanded
}

fn build_fold_groups(
	lines: &[PreviewLine],
	analysis: &StructuralAnalysis,
) -> Vec<FoldGroup> {
	let regions = analysis
		.regions
		.iter()
		.filter(|region| {
			region.foldable
				&& (region.is_test
					|| matches!(region.kind, "function" | "method"))
		})
		.collect::<Vec<_>>();
	let old_line_numbers =
		lines.iter().map(|line| line.old_lineno).collect::<Vec<_>>();
	let new_line_numbers =
		lines.iter().map(|line| line.new_lineno).collect::<Vec<_>>();
	let ranges = regions
		.iter()
		.map(|region| {
			(region.old_lines.clone(), region.new_lines.clone())
		})
		.collect::<Vec<_>>();
	let memberships =
		crate::ui::diff_presentation::region_row_membership(
			&old_line_numbers,
			&new_line_numbers,
			&ranges,
		);
	let patch_row_indexes = lines
		.iter()
		.enumerate()
		.filter_map(|(index, line)| {
			is_patch_line(line.kind).then_some(index)
		})
		.collect::<Vec<_>>();
	let blocking_rows = lines
		.iter()
		.enumerate()
		.filter_map(|(index, line)| {
			(!is_patch_line(line.kind)
				&& line.kind != PreviewLineKind::HunkHeader)
				.then_some(index)
		})
		.collect::<Vec<_>>();
	let mut groups = regions
		.into_iter()
		.zip(memberships)
		.filter_map(|(region, indexes)| {
			fold_group_for_region(
				lines,
				region,
				&indexes,
				&patch_row_indexes,
				&blocking_rows,
			)
		})
		.collect::<Vec<_>>();
	// Prefer the smallest declaration groups. Parent syntax nodes overlap their
	// children and would hide the useful function/test-level fold affordances.
	groups.sort_by_key(|group| {
		(group.last_row - group.first_row, group.first_row)
	});
	let mut selected: Vec<FoldGroup> = Vec::new();
	let mut selected_ranges = std::collections::BTreeMap::new();
	for group in groups {
		let prior_overlaps = selected_ranges
			.range(..=group.first_row)
			.next_back()
			.is_some_and(|(_, end)| *end >= group.first_row);
		let next_overlaps = selected_ranges
			.range(group.first_row..)
			.next()
			.is_some_and(|(start, _)| *start <= group.last_row);
		if prior_overlaps || next_overlaps {
			continue;
		}
		selected_ranges.insert(group.first_row, group.last_row);
		selected.push(group);
	}
	selected.sort_by_key(|group| group.first_row);
	selected
}

fn fold_group_for_region(
	lines: &[PreviewLine],
	region: &StructuralRegion,
	indexes: &[usize],
	patch_row_indexes: &[usize],
	blocking_rows: &[usize],
) -> Option<FoldGroup> {
	let first_row = *indexes.first()?;
	let last_row = *indexes.last()?;
	let index_set = indexes.iter().copied().collect::<HashSet<_>>();
	if indexes
		.iter()
		.any(|&index| !row_fully_in_region(&lines[index], region))
	{
		return None;
	}
	let mut patch_rows = 0;
	let mut visible_rows = 0;
	let mut additions = 0;
	let mut deletions = 0;
	let first_patch =
		patch_row_indexes.partition_point(|row| *row < first_row);
	for &index in patch_row_indexes[first_patch..]
		.iter()
		.take_while(|row| **row <= last_row)
	{
		if !index_set.contains(&index) {
			return None;
		}
		patch_rows += represented_patch_rows(&lines[index]);
		visible_rows += 1;
		let (add, delete) = change_counts(&lines[index]);
		additions += add;
		deletions += delete;
	}
	let first_blocking =
		blocking_rows.partition_point(|row| *row < first_row);
	if blocking_rows
		.get(first_blocking)
		.is_some_and(|row| *row <= last_row)
	{
		return None;
	}
	(visible_rows >= 2).then(|| FoldGroup {
		region: region.clone(),
		first_row,
		last_row,
		visible_rows,
		patch_rows,
		additions,
		deletions,
	})
}

fn row_fully_in_region(
	line: &PreviewLine,
	region: &StructuralRegion,
) -> bool {
	let old_matches = line.old_lineno.is_none_or(|number| {
		number.checked_sub(1).is_some_and(|line| {
			region
				.old_lines
				.as_ref()
				.is_some_and(|range| range.contains(&line))
		})
	});
	let new_matches = line.new_lineno.is_none_or(|number| {
		number.checked_sub(1).is_some_and(|line| {
			region
				.new_lines
				.as_ref()
				.is_some_and(|range| range.contains(&line))
		})
	});
	(line.old_lineno.is_some() || line.new_lineno.is_some())
		&& old_matches
		&& new_matches
}

const fn is_patch_line(kind: PreviewLineKind) -> bool {
	matches!(
		kind,
		PreviewLineKind::Context
			| PreviewLineKind::Addition
			| PreviewLineKind::Deletion
			| PreviewLineKind::Replacement
			| PreviewLineKind::BlockReplacement
	)
}

fn change_counts(line: &PreviewLine) -> (usize, usize) {
	match line.kind {
		PreviewLineKind::Addition => (1, 0),
		PreviewLineKind::Deletion => (0, 1),
		PreviewLineKind::Replacement => (1, 1),
		PreviewLineKind::BlockReplacement => {
			line.block_replacement.as_ref().map_or((0, 0), |block| {
				(block.new_lines.len(), block.old_lines.len())
			})
		}
		_ => (0, 0),
	}
}

fn represented_patch_rows(line: &PreviewLine) -> usize {
	match line.kind {
		PreviewLineKind::Replacement => 2,
		PreviewLineKind::BlockReplacement => {
			line.block_replacement.as_ref().map_or(0, |block| {
				block.old_lines.len() + block.new_lines.len()
			})
		}
		PreviewLineKind::Context
		| PreviewLineKind::Addition
		| PreviewLineKind::Deletion => 1,
		_ => 0,
	}
}

fn default_fold(group: &FoldGroup) -> bool {
	if group.visible_rows < 2 {
		return false;
	}
	let region = &group.region;
	let line_span = region
		.new_lines
		.as_ref()
		.or(region.old_lines.as_ref())
		.map_or(0, |range| range.end.saturating_sub(range.start));
	region.is_test || (!region.contains_changes && line_span >= 16)
}

fn project_rows(
	line_count: usize,
	groups: &[FoldGroup],
	folded: &std::collections::HashSet<u64>,
) -> Vec<ProjectedRow> {
	let mut rows = Vec::new();
	let mut index = 0;
	let mut group_index = 0;
	while index < line_count {
		while groups
			.get(group_index)
			.is_some_and(|group| group.first_row < index)
		{
			group_index += 1;
		}
		if let Some(group) = groups
			.get(group_index)
			.filter(|group| group.first_row == index)
		{
			if folded.contains(&group.region.id) {
				rows.push(ProjectedRow::FoldSummary(group_index));
				index = group.last_row.saturating_add(1);
				group_index += 1;
				continue;
			}
			rows.push(ProjectedRow::RegionHeader(group_index));
			group_index += 1;
		}
		rows.push(ProjectedRow::Patch(index));
		index += 1;
	}
	rows
}

fn fold_summary(group: &FoldGroup, expanded: bool) -> String {
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
				range.end
			)
		})
		.unwrap_or_default();
	format!(
		"{marker} {kind} {} · {} diff rows · +{} −{} · {line_span}",
		group.region.label,
		group.patch_rows,
		group.additions,
		group.deletions,
	)
}

impl Component for DiffPreviewView {
	fn commands(
		&self,
		_out: &mut Vec<CommandInfo>,
		_force_all: bool,
	) -> CommandBlocking {
		CommandBlocking::PassingOn
	}

	#[allow(clippy::too_many_lines)] // Keep pane-specific input handling together.
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		let movement = match ev {
			Event::Key(key)
				if matches!(
					key.kind,
					KeyEventKind::Press | KeyEventKind::Repeat
				) =>
			{
				if key.code == KeyCode::Enter
					&& self.toggle_focused_fold()
				{
					return Ok(EventState::Consumed);
				}
				let bound = self.key_config.as_ref();
				let up = bound
					.is_some_and(|c| key_match(key, c.keys.move_up))
					|| (key.code == KeyCode::Up
						&& key.modifiers.is_empty());
				let down = bound.is_some_and(|c| {
					key_match(key, c.keys.move_down)
				}) || (key.code == KeyCode::Down
					&& key.modifiers.is_empty());
				let left = bound.is_some_and(|c| {
					key_match(key, c.keys.move_left)
				}) || (key.code == KeyCode::Left
					&& key.modifiers.is_empty());
				let right = bound.is_some_and(|c| {
					key_match(key, c.keys.move_right)
				}) || (key.code == KeyCode::Right
					&& key.modifiers.is_empty());
				if left {
					self.horizontal =
						self.horizontal.saturating_sub(3);
					return Ok(EventState::Consumed);
				}
				if right {
					let max = self.longest_line.get().saturating_sub(
						usize::from(
							self.area.get().width.saturating_sub(2),
						),
					);
					self.horizontal =
						self.horizontal.saturating_add(3).min(max);
					return Ok(EventState::Consumed);
				}
				if up {
					Some(super::ScrollType::Up)
				} else if down {
					Some(super::ScrollType::Down)
				} else if bound
					.is_some_and(|c| key_match(key, c.keys.page_up))
					|| (key.code == KeyCode::PageUp
						&& key.modifiers.is_empty())
				{
					Some(super::ScrollType::PageUp)
				} else if bound
					.is_some_and(|c| key_match(key, c.keys.page_down))
					|| (key.code == KeyCode::PageDown
						&& key.modifiers.is_empty())
				{
					Some(super::ScrollType::PageDown)
				} else if bound
					.is_some_and(|c| key_match(key, c.keys.home))
					|| (key.code == KeyCode::Home
						&& key.modifiers.is_empty())
				{
					Some(super::ScrollType::Home)
				} else if bound
					.is_some_and(|c| key_match(key, c.keys.end))
					|| (key.code == KeyCode::End
						&& key.modifiers.is_empty())
				{
					Some(super::ScrollType::End)
				} else {
					None
				}
			}
			Event::Mouse(mouse)
				if self.area.get().contains(Position::new(
					mouse.column,
					mouse.row,
				)) =>
			{
				if mouse.kind
					== MouseEventKind::Down(MouseButton::Left)
				{
					let area = self.area.get();
					let inner_y = area.y.saturating_add(1);
					if mouse.row < inner_y
						|| mouse.row
							>= area
								.y
								.saturating_add(area.height)
								.saturating_sub(1)
					{
						return Ok(EventState::NotConsumed);
					}
					let row = self.scroll.get_top().saturating_add(
						usize::from(
							mouse.row.saturating_sub(inner_y),
						),
					);
					if self.preview.is_some() {
						let target = self
							.projected_rows
							.borrow()
							.get(row)
							.copied();
						if let Some(
							ProjectedRow::RegionHeader(index)
							| ProjectedRow::FoldSummary(index),
						) = target
						{
							let id =
								self.fold_groups[index].region.id;
							self.toggle_fold(id);
							return Ok(EventState::Consumed);
						}
					}
				}
				match mouse.kind {
					MouseEventKind::ScrollUp => {
						Some(super::ScrollType::Up)
					}
					MouseEventKind::ScrollDown => {
						Some(super::ScrollType::Down)
					}
					_ => None,
				}
			}
			_ => None,
		};
		Ok(movement.map_or(EventState::NotConsumed, |movement| {
			self.scroll.move_top(movement);
			self.focus_fold_near_scroll();
			EventState::Consumed
		}))
	}
}

fn preview_label(
	preview: &DiffPreview,
	preference: PreviewPreference,
) -> String {
	let status = match preview.status {
		PreviewStatus::Unified => match preference {
			PreviewPreference::Auto => "Inline suggestions",
			PreviewPreference::Unified => "Plain unified",
		},
		PreviewStatus::Unchanged => "unchanged",
		PreviewStatus::Binary => "binary",
		PreviewStatus::TooLarge => "too large",
		PreviewStatus::Cancelled => "cancelled",
		PreviewStatus::CaptureFailed => "capture failed",
		PreviewStatus::Stale => "stale source",
	};
	status.to_owned()
}

fn review_label(path: &str) -> String {
	let escaped: String = path
		.chars()
		.flat_map(char::escape_default)
		.take(96)
		.collect();
	escaped
}

#[cfg(test)]
fn horizontal_slice(
	line: Line<'_>,
	offset: usize,
	width: usize,
) -> Line<'static> {
	horizontal_slice_with_fixed_prefix(line, 0, offset, width)
}

fn horizontal_slice_with_fixed_prefix(
	line: Line<'_>,
	fixed_prefix: usize,
	mut offset: usize,
	width: usize,
) -> Line<'static> {
	let mut remaining = width;
	let mut prefix_remaining = fixed_prefix;
	let mut spans = Vec::new();
	for span in line.spans {
		let mut content = String::new();
		for grapheme in span.content.graphemes(true) {
			let grapheme_width = UnicodeWidthStr::width(grapheme);
			if grapheme_width == 0 {
				if prefix_remaining > 0
					|| (offset == 0 && remaining > 0)
				{
					content.push_str(grapheme);
				}
				continue;
			}
			if prefix_remaining > 0 {
				if grapheme_width > remaining {
					break;
				}
				content.push_str(grapheme);
				prefix_remaining =
					prefix_remaining.saturating_sub(grapheme_width);
				remaining -= grapheme_width;
				continue;
			}
			if offset > 0 {
				if offset >= grapheme_width {
					offset -= grapheme_width;
				} else {
					offset = 0;
				}
				continue;
			}
			if grapheme_width > remaining || remaining == 0 {
				break;
			}
			content.push_str(grapheme);
			remaining -= grapheme_width;
		}
		if !content.is_empty() {
			spans.push(Span::styled(content, span.style));
		}
		if remaining == 0 {
			break;
		}
	}
	Line::from(spans)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::keys::GituiKeyEvent;
	use asyncgit::{
		diff_preview::{
			InlineReplacement, InlineSegment, InlineSegmentKind,
			PreviewOptions, PreviewRenderer, PreviewStatus,
		},
		sync::diff::DiffOptions,
		DiffType,
	};

	fn replacement_row() -> PreviewLine {
		PreviewLine {
			kind: PreviewLineKind::Replacement,
			old_lineno: Some(12),
			new_lineno: Some(14),
			text: "fn dog()".to_owned(),
			patch_index: 4,
			changed_ranges: vec![],
			replacement: Some(InlineReplacement {
				old_text: "fn cat()".to_owned(),
				new_text: "fn dog()".to_owned(),
				old_lineno: Some(12),
				new_lineno: Some(14),
				old_patch_index: 4,
				new_patch_index: 5,
				segments: vec![
					InlineSegment {
						kind: InlineSegmentKind::Shared,
						old_range: 0..3,
						new_range: 0..3,
					},
					InlineSegment {
						kind: InlineSegmentKind::Removed,
						old_range: 3..6,
						new_range: 3..3,
					},
					InlineSegment {
						kind: InlineSegmentKind::Added,
						old_range: 6..6,
						new_range: 3..6,
					},
					InlineSegment {
						kind: InlineSegmentKind::Shared,
						old_range: 6..8,
						new_range: 6..8,
					},
				],
			}),
			block_replacement: None,
		}
	}

	fn numbered_line(
		kind: PreviewLineKind,
		old_lineno: Option<u32>,
		new_lineno: Option<u32>,
		patch_index: usize,
	) -> PreviewLine {
		PreviewLine {
			kind,
			old_lineno,
			new_lineno,
			text: format!("row {patch_index}"),
			patch_index,
			changed_ranges: vec![],
			replacement: None,
			block_replacement: None,
		}
	}

	fn fold_region(
		id: u64,
		old_lines: Option<std::ops::Range<u32>>,
		new_lines: Option<std::ops::Range<u32>>,
	) -> StructuralRegion {
		StructuralRegion {
			id,
			parent_id: None,
			kind: "function",
			label: format!("work_{id}"),
			is_test: false,
			old_lines,
			new_lines,
			change: asyncgit::structural::StructuralChange::Modified,
			foldable: true,
			contains_changes: true,
		}
	}

	#[test]
	fn replacement_row_keeps_dual_numbers_and_styles_inline_segments()
	{
		let env = crate::app::Environment::test_env();
		let row = replacement_row();
		let rendered = preview_line(&row, 2, &env.theme);
		let rendered_text = rendered
			.spans
			.iter()
			.map(|span| span.content.as_ref())
			.collect::<String>();
		assert_eq!(rendered_text, "12│14│ fn catdog()");
		assert_eq!(rendered.spans[0].content, "12│14│");
		assert!(rendered.spans.iter().any(|span| {
			span.content == "cat"
				&& span
					.style
					.add_modifier
					.contains(Modifier::CROSSED_OUT)
		}));
		assert!(rendered.spans.iter().any(|span| {
			span.content == "cat"
				&& !span
					.style
					.add_modifier
					.contains(Modifier::UNDERLINED)
				&& span
					.style
					.sub_modifier
					.contains(Modifier::UNDERLINED)
		}));
		assert!(rendered.spans.iter().any(|span| {
			span.content == "dog"
				&& span
					.style
					.add_modifier
					.contains(Modifier::UNDERLINED)
		}));
		assert!(rendered.spans.iter().any(|span| {
			span.content == "dog"
				&& !span
					.style
					.add_modifier
					.contains(Modifier::CROSSED_OUT)
				&& span
					.style
					.sub_modifier
					.contains(Modifier::CROSSED_OUT)
		}));
		assert_eq!(
			replacement_row().text,
			"fn dog()",
			"copy/fallback text stays the new source line"
		);

		let (line, fixed_prefix) =
			preview_line_with_prefix(&row, 2, &env.theme);
		let clipped = horizontal_slice_with_fixed_prefix(
			line,
			fixed_prefix,
			2,
			14,
		);
		assert!(clipped
			.spans
			.first()
			.is_some_and(|span| span.content == "12│14│"));
		let (narrow, narrow_prefix) =
			preview_line_for_width(&row, 2, &env.theme, 5);
		assert!(narrow.spans[0].content.starts_with("14│"));
		assert_eq!(narrow_prefix, 4);
	}

	#[test]
	fn preview_title_uses_the_accepted_presentation_choice() {
		let params = DiffParams {
			path: "README.md".to_owned(),
			diff_type: DiffType::WorkDir,
			options: DiffOptions::default(),
		};
		let make_result =
			|generation, preference| DiffPreviewResult {
				request: DiffPreviewRequest {
					generation,
					params: params.clone(),
					expected_native_hash: 99,
					options: PreviewOptions { preference },
				},
				preview: DiffPreview {
					renderer: PreviewRenderer::Native,
					status: PreviewStatus::Unified,
					lines: Vec::new(),
					structural: None,
					message: None,
				},
			};
		let mut view = DiffPreviewView::new();
		view.set_preview(make_result(1, PreviewPreference::Auto), 99);
		assert_eq!(
			preview_label(
				view.preview.as_ref().unwrap(),
				view.accepted_preference.unwrap(),
			),
			"Inline suggestions"
		);
		view.set_preview(
			make_result(2, PreviewPreference::Unified),
			99,
		);
		assert_eq!(
			preview_label(
				view.preview.as_ref().unwrap(),
				view.accepted_preference.unwrap(),
			),
			"Plain unified"
		);
		let mut binary = view.preview.as_ref().unwrap().clone();
		binary.status = PreviewStatus::Binary;
		assert_eq!(
			preview_label(&binary, PreviewPreference::Auto),
			"binary"
		);
	}

	#[test]
	fn block_resize_keeps_the_first_visible_unicode_source_piece_anchored(
	) {
		let old = [
			"Readers can follow the review carefully while retaining",
			"the original context for each important change for a naïve reader",
		];
		let new = [
			"Readers can carefully follow each detailed review",
			"while retaining original context for every important change for a naïve reader",
		];
		let old_lines = old
			.iter()
			.enumerate()
			.map(|(index, text)| {
				asyncgit::diff_preview::InlineBlockLine {
					text,
					line_number: Some(8 + index as u32),
					patch_index: index,
				}
			})
			.collect::<Vec<_>>();
		let new_lines = new
			.iter()
			.enumerate()
			.map(|(index, text)| {
				asyncgit::diff_preview::InlineBlockLine {
					text,
					line_number: Some(14 + index as u32),
					patch_index: index + 2,
				}
			})
			.collect::<Vec<_>>();
		let block = asyncgit::diff_preview::inline_diff_block(
			&old_lines,
			&new_lines,
			&std::sync::atomic::AtomicBool::new(false),
		)
		.expect("prose rewrite is eligible for compact presentation");
		let row = PreviewLine {
			kind: PreviewLineKind::BlockReplacement,
			old_lineno: Some(8),
			new_lineno: Some(14),
			text: block.new_text.clone(),
			patch_index: 0,
			changed_ranges: Vec::new(),
			replacement: None,
			block_replacement: Some(block),
		};
		let params = DiffParams {
			path: "README.md".to_owned(),
			diff_type: DiffType::WorkDir,
			options: DiffOptions::default(),
		};
		let mut view = DiffPreviewView::new();
		view.set_preview(
			DiffPreviewResult {
				request: DiffPreviewRequest {
					generation: 1,
					params,
					expected_native_hash: 99,
					options: PreviewOptions::default(),
				},
				preview: DiffPreview {
					renderer: PreviewRenderer::Native,
					status: PreviewStatus::Unified,
					lines: vec![row],
					structural: None,
					message: None,
				},
			},
			99,
		);
		view.update_block_layout(42);
		let old_layout = view.block_visual_lines.borrow();
		let old_visuals = &old_layout[&0];
		assert!(
			old_visuals.len() > 2,
			"paragraph wraps at initial width"
		);
		let unicode_offset = view.preview.as_ref().unwrap().lines[0]
			.block_replacement
			.as_ref()
			.unwrap()
			.new_text
			.find("naïve")
			.unwrap();
		let anchor_visual = old_visuals
			.iter()
			.position(|line| {
				line.pieces.iter().any(|piece| {
					!piece.is_space
						&& piece.new_range.start <= unicode_offset
						&& unicode_offset < piece.new_range.end
				})
			})
			.expect("Unicode word is visible in initial layout");
		view.scroll.set_top(anchor_visual);
		assert!(old_visuals[anchor_visual]
			.pieces
			.iter()
			.find(|piece| {
				!piece.is_space
					&& piece.new_range.start <= unicode_offset
					&& unicode_offset < piece.new_range.end
			})
			.is_some());
		let anchor_offset = unicode_offset;
		drop(old_layout);

		view.update_block_layout(25);
		let top = view.scroll.get_top();
		let projected = view.projected_rows.borrow();
		let ProjectedRow::BlockLine(0, visual_index) = projected[top]
		else {
			panic!("resize preserves the source-backed block row at the top")
		};
		let visual =
			&view.block_visual_lines.borrow()[&0][visual_index];
		assert!(visual.pieces.iter().any(|piece| {
			!piece.is_space
				&& piece.new_range.start <= anchor_offset
				&& anchor_offset < piece.new_range.end
		}));
		assert_eq!(
			view.preview.as_ref().unwrap().lines[0]
				.block_replacement
				.as_ref()
				.unwrap()
				.new_text
				.get(anchor_offset..anchor_offset + "naïve".len()),
			Some("naïve")
		);
	}
	use crossterm::event::{KeyEvent, KeyModifiers};
	use std::rc::Rc;

	#[test]
	fn typed_rows_render_inline_changes_without_terminal_controls() {
		let env = crate::app::Environment::test_env();
		let row = PreviewLine {
			kind: PreviewLineKind::Addition,
			old_lineno: None,
			new_lineno: Some(4),
			text: "aéx".to_owned(),
			patch_index: 0,
			changed_ranges: vec![1..3],
			replacement: None,
			block_replacement: None,
		};
		let rendered = preview_line(&row, 1, &env.theme);
		let rendered_text = rendered
			.spans
			.iter()
			.map(|span| span.content.as_ref())
			.collect::<String>();
		assert_eq!(rendered_text, " │4│ aéx");
		assert!(!rendered_text.contains('+'));
		assert!(!rendered_text.contains('-'));
		assert!(!rendered_text.contains('~'));
		assert!(rendered.spans.iter().any(|span| {
			span.content == "é"
				&& span.style.fg
					== env
						.theme
						.diff_inline_changed(DiffLineType::Add)
						.fg && span.style.bg
				== env.theme.diff_inline_changed(DiffLineType::Add).bg
				&& span
					.style
					.add_modifier
					.contains(Modifier::UNDERLINED)
				&& !span
					.style
					.add_modifier
					.contains(Modifier::CROSSED_OUT)
		}));
		assert!(rendered_text
			.chars()
			.all(|character| !character.is_control()));
	}

	#[test]
	fn visible_syntax_rows_are_prioritized_over_large_context_prefixes(
	) {
		let lines = (0..65)
			.map(|index| {
				let mut line = numbered_line(
					PreviewLineKind::Addition,
					None,
					Some(index as u32 + 1),
					index,
				);
				line.text = if index < 64 {
					"x".repeat(MAX_PAIR_LINE_BYTES)
				} else {
					"let visible_later = true;".to_owned()
				};
				line
			})
			.collect::<Vec<_>>();
		let mut prioritized = vec![64];
		prioritized.extend(0..64);
		let (inputs, _, selected) = build_preview_highlight_inputs(
			&lines,
			&vec![0; lines.len()],
			&prioritized,
		);
		assert!(selected.contains(&64));
		assert!(!selected.contains(&31));
		assert!(inputs.iter().any(|input| {
			input.content == "let visible_later = true;"
		}));
		assert!(
			inputs
				.iter()
				.map(|input| input.content.len())
				.sum::<usize>()
				<= PREVIEW_HIGHLIGHT_MAX_BYTES
		);
	}

	#[test]
	fn block_syntax_ranges_keep_joined_offsets_after_physical_newlines(
	) {
		let old_lines = [
			"The setting `old_flag` remains available to readers",
			"when the recorded `session` ends safely",
		]
		.into_iter()
		.enumerate()
		.map(|(index, text)| {
			asyncgit::diff_preview::InlineBlockLine {
				text,
				line_number: Some(index as u32 + 1),
				patch_index: index,
			}
		})
		.collect::<Vec<_>>();
		let new_lines = [
			"The setting `new_flag` stays available to readers",
			"when the recorded `session` finishes safely",
		]
		.into_iter()
		.enumerate()
		.map(|(index, text)| {
			asyncgit::diff_preview::InlineBlockLine {
				text,
				line_number: Some(index as u32 + 1),
				patch_index: index + 2,
			}
		})
		.collect::<Vec<_>>();
		let block = asyncgit::diff_preview::inline_diff_block(
			&old_lines,
			&new_lines,
			&std::sync::atomic::AtomicBool::new(false),
		)
		.expect("Markdown prose block with inline code is eligible");
		let second_line_start = block.new_lines[1].text_range.start;
		let row = PreviewLine {
			kind: PreviewLineKind::BlockReplacement,
			old_lineno: Some(1),
			new_lineno: Some(1),
			text: block.new_text.clone(),
			patch_index: 0,
			changed_ranges: Vec::new(),
			replacement: None,
			block_replacement: Some(block),
		};
		let (inputs, mappings, selected) =
			build_preview_highlight_inputs(
				std::slice::from_ref(&row),
				&[1],
				&[0],
			);
		assert_eq!(selected, vec![0]);
		let highlighted = highlight_diff_rows(
			std::path::Path::new("README.md"),
			&inputs,
			"base16-eighties.dark",
		);
		let new_mapping = mappings
			.iter()
			.position(|mapping| {
				mapping.is_some_and(|mapping| {
					!mapping.old_side
						&& mapping.source_offset == second_line_start
				})
			})
			.expect("new side mapping exists");
		let translated = translate_syntax_ranges(
			highlighted[new_mapping].syntax.clone(),
			second_line_start,
		);
		assert!(translated.iter().any(|range| {
			range.range.start >= second_line_start
				&& range.style.fg.is_some()
		}));
		let mut syntax = PreviewLineSyntax::default();
		for (mapping, highlight) in mappings.iter().zip(&highlighted)
		{
			let Some(mapping) = mapping else { continue };
			let ranges = translate_syntax_ranges(
				highlight.syntax.clone(),
				mapping.source_offset,
			);
			if mapping.old_side {
				syntax.old.extend(ranges);
			} else {
				syntax.new.extend(ranges);
			}
		}
		let block = row.block_replacement.as_ref().unwrap();
		let visuals = inline_block_visual_lines(block, 120);
		let env = crate::app::Environment::test_env();
		let (rendered, _) = preview_block_visual_line(
			block,
			&visuals[0],
			1,
			&env.theme,
			120,
			Some(&syntax),
		);
		let code_fg = syntax
			.new
			.iter()
			.find(|range| {
				range.range.start
					<= block.new_text.find("new_flag").unwrap()
					&& block.new_text.find("new_flag").unwrap()
						< range.range.end
			})
			.and_then(|range| range.style.fg)
			.expect("Markdown inline code has syntax foreground");
		let old_code_offset =
			block.old_text.find("old_flag").unwrap();
		let old_code_fg = syntax
			.old
			.iter()
			.find(|range| {
				range.range.start <= old_code_offset
					&& old_code_offset < range.range.end
			})
			.and_then(|range| range.style.fg)
			.expect("old Markdown inline code has syntax foreground");
		assert!(rendered.spans.iter().any(|span| {
			span.content == "new"
				&& span.style.fg == Some(code_fg)
				&& span
					.style
					.add_modifier
					.contains(Modifier::UNDERLINED)
				&& !span
					.style
					.add_modifier
					.contains(Modifier::CROSSED_OUT)
		}));
		assert!(rendered.spans.iter().any(|span| {
			span.content == "old"
				&& span.style.fg == Some(old_code_fg)
				&& span
					.style
					.add_modifier
					.contains(Modifier::CROSSED_OUT)
				&& !span
					.style
					.add_modifier
					.contains(Modifier::UNDERLINED)
		}));
	}

	#[test]
	fn horizontal_slice_uses_terminal_cells_and_bounds_allocated_text(
	) {
		let line = Line::from("a界bc");
		assert_eq!(
			horizontal_slice(line.clone(), 1, 2)
				.spans
				.iter()
				.map(|span| span.content.as_ref())
				.collect::<String>(),
			"界",
		);
		assert_eq!(
			horizontal_slice(line, 3, 2)
				.spans
				.iter()
				.map(|span| span.content.as_ref())
				.collect::<String>(),
			"bc",
		);
		let unicode = Line::from("ae\u{301}界x");
		assert_eq!(
			horizontal_slice(unicode.clone(), 2, 2)
				.spans
				.iter()
				.map(|span| span.content.as_ref())
				.collect::<String>(),
			"界",
		);
		assert_eq!(
			horizontal_slice(unicode, 3, 1)
				.spans
				.iter()
				.map(|span| span.content.as_ref())
				.collect::<String>(),
			"x",
		);
	}

	#[test]
	fn preview_spans_borrow_full_source_and_clip_only_viewport_text()
	{
		let env = crate::app::Environment::test_env();
		let row = PreviewLine {
			kind: PreviewLineKind::Context,
			old_lineno: Some(1),
			new_lineno: Some(1),
			text: "a".repeat(100_000),
			patch_index: 0,
			changed_ranges: vec![],
			replacement: None,
			block_replacement: None,
		};
		let (line, fixed_prefix) =
			preview_line_for_width(&row, 1, &env.theme, 18);
		assert!(line.spans.iter().any(|span| {
			matches!(&span.content, std::borrow::Cow::Borrowed(_))
		}));
		let visible = horizontal_slice_with_fixed_prefix(
			line,
			fixed_prefix,
			0,
			18,
		);
		let visible_width = visible
			.spans
			.iter()
			.map(|span| UnicodeWidthStr::width(span.content.as_ref()))
			.sum::<usize>();
		assert!(visible_width <= 18);
		assert!(visible
			.spans
			.iter()
			.all(|span| span.content.len() < row.text.len()));
	}

	#[test]
	fn folds_only_contiguous_rows_and_reports_exact_change_counts() {
		let lines = vec![
			PreviewLine {
				kind: PreviewLineKind::HunkHeader,
				old_lineno: None,
				new_lineno: None,
				text: "@@ hunk @@".to_owned(),
				patch_index: 0,
				changed_ranges: vec![],
				replacement: None,
				block_replacement: None,
			},
			numbered_line(
				PreviewLineKind::Deletion,
				Some(2),
				None,
				1,
			),
			numbered_line(
				PreviewLineKind::Addition,
				None,
				Some(2),
				2,
			),
			numbered_line(
				PreviewLineKind::Context,
				Some(3),
				Some(3),
				3,
			),
		];
		let analysis = StructuralAnalysis {
			language: "Rust",
			regions: vec![fold_region(7, Some(1..3), Some(1..3))],
		};
		let groups = build_fold_groups(&lines, &analysis);
		assert_eq!(groups.len(), 1);
		assert_eq!(groups[0].patch_rows, 3);
		assert_eq!(
			(groups[0].additions, groups[0].deletions),
			(1, 1)
		);
		assert_eq!(
			fold_summary(&groups[0], false),
			"▸ function work_7 · 3 diff rows · +1 −1 · lines 2–3"
		);

		let folded = HashSet::from([7]);
		assert_eq!(
			project_rows(lines.len(), &groups, &folded),
			vec![
				ProjectedRow::Patch(0),
				ProjectedRow::FoldSummary(0),
			]
		);
		assert_eq!(
			project_rows(lines.len(), &groups, &HashSet::new()),
			vec![
				ProjectedRow::Patch(0),
				ProjectedRow::RegionHeader(0),
				ProjectedRow::Patch(1),
				ProjectedRow::Patch(2),
				ProjectedRow::Patch(3),
			]
		);

		let paired_rows = vec![
			replacement_row(),
			numbered_line(
				PreviewLineKind::Context,
				Some(13),
				Some(15),
				6,
			),
		];
		let paired_analysis = StructuralAnalysis {
			language: "Rust",
			regions: vec![fold_region(8, Some(11..13), Some(13..15))],
		};
		let paired_groups =
			build_fold_groups(&paired_rows, &paired_analysis);
		assert_eq!(paired_groups[0].patch_rows, 3);
		assert_eq!(
			(paired_groups[0].additions, paired_groups[0].deletions),
			(1, 1)
		);
	}

	#[test]
	fn fold_group_rejects_sibling_rows_inside_a_moved_declaration_span(
	) {
		let lines = vec![
			PreviewLine {
				kind: PreviewLineKind::HunkHeader,
				old_lineno: None,
				new_lineno: None,
				text: "@@ shared hunk @@".to_owned(),
				patch_index: 0,
				changed_ranges: vec![],
				replacement: None,
				block_replacement: None,
			},
			numbered_line(
				PreviewLineKind::Deletion,
				Some(2),
				None,
				1,
			),
			numbered_line(
				PreviewLineKind::Addition,
				None,
				Some(3),
				2,
			),
			numbered_line(
				PreviewLineKind::Addition,
				None,
				Some(10),
				3,
			),
			numbered_line(
				PreviewLineKind::Deletion,
				Some(4),
				None,
				4,
			),
		];
		let analysis = StructuralAnalysis {
			language: "Rust",
			regions: vec![fold_region(9, Some(1..4), Some(2..4))],
		};
		assert!(build_fold_groups(&lines, &analysis).is_empty());
	}

	#[test]
	fn fold_group_rejects_replacement_paired_with_a_sibling_declaration(
	) {
		let lines = vec![
			replacement_row(),
			numbered_line(
				PreviewLineKind::Deletion,
				Some(13),
				None,
				6,
			),
		];
		let analysis = StructuralAnalysis {
			language: "Rust",
			regions: vec![fold_region(
				10,
				Some(11..14),
				Some(20..22),
			)],
		};
		assert!(build_fold_groups(&lines, &analysis).is_empty());
	}

	#[test]
	fn row_projection_scales_over_large_cached_previews() {
		let groups = (0..1_000)
			.map(|index| FoldGroup {
				region: fold_region(index as u64, None, None),
				first_row: index * 20,
				last_row: index * 20 + 4,
				visible_rows: 5,
				patch_rows: 5,
				additions: 2,
				deletions: 2,
			})
			.collect::<Vec<_>>();
		let folded = groups
			.iter()
			.map(|group| group.region.id)
			.collect::<HashSet<_>>();
		let projected = project_rows(30_000, &groups, &folded);
		assert_eq!(projected.len(), 26_000);
		assert_eq!(projected[0], ProjectedRow::FoldSummary(0));
		assert_eq!(projected[16], ProjectedRow::FoldSummary(1));
		assert_eq!(
			projected.last(),
			Some(&ProjectedRow::Patch(29_999))
		);
	}

	#[test]
	fn default_folding_uses_syntax_test_metadata_not_name_substrings()
	{
		let mut ordinary = FoldGroup {
			region: fold_region(1, Some(0..24), Some(0..24)),
			first_row: 0,
			last_row: 2,
			visible_rows: 3,
			patch_rows: 3,
			additions: 1,
			deletions: 1,
		};
		ordinary.region.label = "contest".to_owned();
		assert!(!default_fold(&ordinary));
		ordinary.region.is_test = true;
		assert!(default_fold(&ordinary));
		ordinary.visible_rows = 1;
		assert!(!default_fold(&ordinary));
	}

	#[test]
	fn fold_state_survives_presentation_changes_and_toggles_by_keyboard_and_mouse(
	) {
		let params = DiffParams {
			path: "src/lib.rs".to_owned(),
			diff_type: DiffType::WorkDir,
			options: DiffOptions::default(),
		};
		let lines = vec![
			numbered_line(
				PreviewLineKind::Deletion,
				Some(1),
				None,
				0,
			),
			numbered_line(
				PreviewLineKind::Addition,
				None,
				Some(1),
				1,
			),
		];
		let make_result =
			|generation, preference, structural| DiffPreviewResult {
				request: DiffPreviewRequest {
					generation,
					params: params.clone(),
					expected_native_hash: 77,
					options: PreviewOptions { preference },
				},
				preview: DiffPreview {
					renderer: PreviewRenderer::Native,
					status: PreviewStatus::Unified,
					lines: lines.clone(),
					structural,
					message: None,
				},
			};
		let analysis = || StructuralAnalysis {
			language: "Rust",
			regions: vec![fold_region(77, Some(0..2), Some(0..2))],
		};
		let mut view = DiffPreviewView::new();
		view.set_preview(
			make_result(1, PreviewPreference::Auto, Some(analysis())),
			77,
		);
		assert!(view.folded.is_empty());
		assert_eq!(view.projected_rows.borrow().len(), 3);

		let enter = Event::Key(KeyEvent::new(
			KeyCode::Enter,
			KeyModifiers::empty(),
		));
		assert_eq!(view.event(&enter).unwrap(), EventState::Consumed);
		assert!(view.folded.contains(&77));
		assert_eq!(
			*view.projected_rows.borrow(),
			vec![ProjectedRow::FoldSummary(0)]
		);

		view.set_preview(
			make_result(2, PreviewPreference::Unified, None),
			77,
		);
		assert!(view.folded.contains(&77));
		view.set_preview(
			make_result(3, PreviewPreference::Auto, Some(analysis())),
			77,
		);
		assert!(view.folded.contains(&77));

		view.area.set(Rect::new(10, 10, 40, 8));
		let click = Event::Mouse(crossterm::event::MouseEvent {
			kind: MouseEventKind::Down(MouseButton::Left),
			column: 12,
			row: 11,
			modifiers: KeyModifiers::empty(),
		});
		assert_eq!(view.event(&click).unwrap(), EventState::Consumed);
		assert!(!view.folded.contains(&77));
		assert_eq!(view.projected_rows.borrow().len(), 3);
		view.set_preview(
			make_result(4, PreviewPreference::Auto, Some(analysis())),
			77,
		);
		assert!(!view.folded.contains(&77));

		view.set_pending_for(
			params.clone(),
			88,
			false,
			"Rendering new source…",
		);
		assert!(view.preview.is_none());
		assert!(view.fold_groups.is_empty());
		assert!(view.folded.is_empty());
	}

	#[test]
	fn review_view_obeys_custom_motion_and_preserves_only_same_source_scroll(
	) {
		let mut config = crate::keys::KeyConfig::with_keymap(
			crate::keys::Keymap::Helix,
		);
		config.keys.move_down = GituiKeyEvent::new(
			KeyCode::Char('n'),
			KeyModifiers::empty(),
		);
		let mut view = DiffPreviewView::new();
		view.set_key_config(Rc::new(config));
		view.scroll.update_no_selection(30, 5);
		let params = DiffParams {
			path: "src/lib.rs".to_string(),
			diff_type: DiffType::WorkDir,
			options: DiffOptions::default(),
		};
		let make_result =
			|generation, params: DiffParams| DiffPreviewResult {
				request: DiffPreviewRequest {
					generation,
					params,
					expected_native_hash: 42,
					options: PreviewOptions::default(),
				},
				preview: DiffPreview {
					renderer: PreviewRenderer::Native,
					status: PreviewStatus::Unified,
					lines: (0..30)
						.map(|i| PreviewLine {
							kind: PreviewLineKind::Addition,
							old_lineno: None,
							new_lineno: Some(
								u32::try_from(i + 1).unwrap(),
							),
							text: format!("line {i}"),
							patch_index: i,
							changed_ranges: vec![],
							replacement: None,
							block_replacement: None,
						})
						.collect(),
					structural: None,
					message: None,
				},
			};
		view.set_preview(make_result(1, params.clone()), 42);
		view.scroll.set_top(9);
		view.set_preview(make_result(2, params.clone()), 42); // viewport resize
		assert_eq!(view.scroll.get_top(), 9);
		view.set_preview(make_result(3, params), 43); // same path, changed native diff
		assert_eq!(view.scroll.get_top(), 0);
		view.scroll.update_no_selection(30, 5);
		let event = Event::Key(KeyEvent::new(
			KeyCode::Char('n'),
			KeyModifiers::empty(),
		));
		assert_eq!(view.event(&event).unwrap(), EventState::Consumed);
		assert_eq!(view.scroll.get_top(), 1);
		let unrelated = Event::Key(KeyEvent::new(
			KeyCode::Char('j'),
			KeyModifiers::empty(),
		));
		assert_eq!(
			view.event(&unrelated).unwrap(),
			EventState::NotConsumed
		);
	}
}
