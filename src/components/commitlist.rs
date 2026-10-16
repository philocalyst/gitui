use super::utils::graphrow::{symbol, SYM_HORIZONTAL, SYM_SPACE};
use super::utils::logitems::{ItemBatch, LogEntry};
use crate::{
	app::Environment,
	components::{
		utils::string_width_align, CommandBlocking, CommandInfo,
		Component, DrawableComponent, EventState, ScrollType,
	},
	keys::{key_match, SharedKeyConfig},
	queue::{InternalEvent, Queue},
	strings::{self, symbol},
	try_or_popup,
	ui::style::{SharedTheme, Theme},
	ui::{calc_scroll_top, draw_scrollbar, Orientation},
};
use anyhow::Result;
use asyncgit::{
	graph::GraphRow,
	sync::{
		checkout_commit, BranchDetails, BranchInfo, CommitId,
		CommitInfo, RepoPathRef, Tags,
	},
	LogRangeStatus,
};
use chrono::{DateTime, Local};
use crossterm::event::{
	Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use indexmap::IndexSet;
use itertools::Itertools;
use ratatui::{
	layout::{Alignment, Rect},
	style::{Color, Modifier, Style},
	text::{Line, Span},
	widgets::{Block, Borders, Paragraph},
	Frame,
};
use std::borrow::Cow;
use std::collections::HashSet;
use std::{
	cell::{Cell, RefCell},
	cmp,
	collections::BTreeMap,
	rc::Rc,
	time::Instant,
};
use unicode_width::UnicodeWidthStr;

const ELEMENTS_PER_LINE: usize = 9;
const LOG_PREFETCH_ROWS: usize = 32;
const DEFAULT_LOG_VIEWPORT_ROWS: usize = 22;
const GRAPH_DETAIL_RESERVE: usize = 12;
const GRAPH_WIDTH_LIMIT: usize = 32;

fn escape_display_label(label: &str) -> String {
	label.chars().flat_map(char::escape_debug).collect()
}

const GRAPH_COLORS: &[Color] = &[
	Color::Blue,
	Color::Yellow,
	Color::Magenta,
	Color::Cyan,
	Color::Green,
	Color::Red,
];

#[cfg(test)]
const fn translate_scroll_top(
	old_index_offset: usize,
	old_scroll_top: usize,
	new_index_offset: usize,
) -> usize {
	old_index_offset
		.saturating_add(old_scroll_top)
		.saturating_sub(new_index_offset)
}

const fn clamp_scroll_top(
	current_top: usize,
	height_in_lines: usize,
	selection: usize,
	item_count: usize,
) -> usize {
	let next_top =
		calc_scroll_top(current_top, height_in_lines, selection);
	let max_top = item_count.saturating_sub(height_in_lines);
	if next_top < max_top {
		next_top
	} else {
		max_top
	}
}

/// Renders the commit log with a side-bar graph.
pub struct CommitList {
	repo: RepoPathRef,
	title: Box<str>,
	selection: usize,
	highlighted_selection: Option<usize>,
	items: ItemBatch,
	highlights: Option<Rc<IndexSet<CommitId>>>,
	commits: IndexSet<CommitId>,
	history_status: Option<LogRangeStatus>,
	/// The marked commits.
	/// `self.marked[].0` holds the commit index into `self.items.items` - used for ordering the list.
	/// `self.marked[].1` is the commit id of the marked commit.
	marked: Vec<(usize, CommitId)>,
	/// The stable commit at which a modal selection started.
	selection_anchor: Option<CommitId>,
	selection_extending: bool,
	scroll_state: (Instant, f32),
	tags: Option<Tags>,
	local_branches: BTreeMap<CommitId, Vec<BranchInfo>>,
	remote_branches: BTreeMap<CommitId, Vec<BranchInfo>>,
	current_size: Cell<Option<(u16, u16)>>,
	scroll_top: Cell<usize>,
	theme: SharedTheme,
	queue: Queue,
	key_config: SharedKeyConfig,
	show_graph: bool,
	graph_col_width: Cell<usize>,
	last_draw_area: Cell<Rect>,
	last_draw_rows: RefCell<Vec<CommitId>>,
	pointer_dragging: bool,
	pointer_anchor: Option<CommitId>,
}

impl CommitList {
	///
	pub fn new(env: &Environment, title: &str) -> Self {
		Self {
			repo: env.repo.clone(),
			items: ItemBatch::default(),
			history_status: None,
			marked: Vec::with_capacity(2),
			selection_anchor: None,
			selection_extending: false,
			selection: 0,
			highlighted_selection: None,
			commits: IndexSet::new(),
			highlights: None,
			scroll_state: (Instant::now(), 0_f32),
			tags: None,
			local_branches: BTreeMap::default(),
			remote_branches: BTreeMap::default(),
			current_size: Cell::new(None),
			scroll_top: Cell::new(0),
			theme: env.theme.clone(),
			queue: env.queue.clone(),
			key_config: env.key_config.clone(),
			title: title.into(),
			show_graph: true,
			graph_col_width: Cell::new(0),
			last_draw_area: Cell::new(Rect::default()),
			last_draw_rows: RefCell::new(Vec::new()),
			pointer_dragging: false,
			pointer_anchor: None,
		}
	}

	/// Whether the commit graph column is currently visible.
	pub const fn is_graph_visible(&self) -> bool {
		self.show_graph
	}

	/// Current viewport plus bounded lookahead, in stable history indices.
	pub fn requested_log_range(&self) -> (usize, usize) {
		let height = self
			.current_size()
			.map_or(DEFAULT_LOG_VIEWPORT_ROWS, |size| {
				usize::from(size.1).max(1)
			});
		let start = calc_scroll_top(
			self.scroll_top.get(),
			height,
			self.selection,
		);
		(
			start,
			start
				.saturating_add(height)
				.saturating_add(LOG_PREFETCH_ROWS),
		)
	}

	/// Whether the visible tail is close enough to request another bounded page.
	pub fn should_load_more(&self) -> bool {
		if self.history_status.is_some_and(|status| {
			!status.has_more || status.pending || status.complete
		}) {
			return false;
		}
		let loaded = self.loaded_commit_count();
		let (_, requested_end) = self.requested_log_range();
		let visible_end =
			requested_end.saturating_sub(LOG_PREFETCH_ROWS);
		loaded == 0 || visible_end.saturating_add(8) >= loaded
	}

	/// Commit IDs available from the bounded log walk so far.
	pub fn loaded_commit_count(&self) -> usize {
		if self.commits.is_empty() {
			self.items.index_offset().saturating_add(self.items.len())
		} else {
			self.commits.len()
		}
	}

	/// Update the lazy-history status shown by the log.
	pub const fn set_history_status(
		&mut self,
		status: LogRangeStatus,
	) {
		self.history_status = Some(status);
	}

	/// Install a cache-only metadata range returned by the background log worker.
	pub fn set_metadata_range(
		&mut self,
		start: usize,
		commits: Vec<CommitInfo>,
	) {
		self.items.set_items(
			start,
			commits,
			self.highlights.as_ref(),
		);
		self.update_modal_range();
	}

	///
	pub fn set_graph_rows(&mut self, rows: Vec<GraphRow>) {
		self.items.set_graph_rows(rows);
	}

	///
	pub const fn tags(&self) -> Option<&Tags> {
		self.tags.as_ref()
	}

	///
	pub fn clear(&mut self) {
		self.items.clear();
		self.commits.clear();
		self.selection = 0;
		self.highlighted_selection = None;
		self.history_status = None;
		self.scroll_top.set(0);
		self.graph_col_width.set(0);
		self.selection_anchor = None;
		self.selection_extending = false;
		self.marked.clear();
	}

	///
	pub fn set_tags(&mut self, tags: Tags) {
		self.tags = Some(tags);
	}

	///
	pub fn selected_entry(&self) -> Option<&LogEntry> {
		self.items.get_at(self.selection)
	}

	/// Return the selected ID even while its metadata row is still loading.
	pub fn selected_commit_id(&self) -> Option<CommitId> {
		self.commits.get_index(self.selection).copied().or_else(
			|| {
				self.items
					.get_at(self.selection)
					.map(|entry| entry.id)
			},
		)
	}

	/// Return the commit targeted by a single-commit action.
	///
	/// Modal selections take precedence over the cursor when exactly one
	/// commit is marked. Multiple marked commits have no single target.
	/// Legacy keymaps continue to target the cursor.
	pub fn action_commit(&self) -> Option<CommitId> {
		if self.key_config.is_modal() {
			match self.marked.as_slice() {
				[(_, id)] => Some(*id),
				[_, _, ..] => None,
				[] => self.selected_commit_id(),
			}
		} else {
			self.selected_commit_id()
		}
	}

	/// Whether a modal single-commit action has an ambiguous target.
	pub fn has_multiple_action_commits(&self) -> bool {
		self.key_config.is_modal() && self.marked.len() > 1
	}

	///
	pub const fn marked_count(&self) -> usize {
		self.marked.len()
	}

	///
	pub fn clear_marked(&mut self) {
		self.marked.clear();
	}

	/// Finish a modal range without discarding its selected commit ids.
	///
	/// Containers call this when the commit list stops being the active
	/// component, so a later key press cannot continue extending a hidden range.
	pub const fn finish_selection(&mut self) {
		self.selection_anchor = None;
		self.selection_extending = false;
	}

	/// Return the current modal commit selection for command bars and titles.
	pub fn selection_status(
		&self,
	) -> Option<crate::components::SelectionStatus> {
		if !self.key_config.is_modal()
			|| (self.selection_anchor.is_none()
				&& self.marked.is_empty())
		{
			return None;
		}
		Some(crate::components::SelectionStatus {
			extending: self.selection_extending,
			count: self.marked.len(),
			unit: if self.selection_anchor.is_some() {
				"loaded commits"
			} else {
				"commits"
			},
		})
	}

	fn selection_range_ids(&self) -> Vec<CommitId> {
		let Some(anchor) = self.selection_anchor else {
			return Vec::new();
		};
		if !self.commits.is_empty() {
			let Some(anchor_idx) = self.commits.get_index_of(&anchor)
			else {
				return Vec::new();
			};
			if self.selection >= self.commits.len() {
				return Vec::new();
			}
			let (start, finish) = if anchor_idx <= self.selection {
				(anchor_idx, self.selection)
			} else {
				(self.selection, anchor_idx)
			};
			return self
				.commits
				.iter()
				.skip(start)
				.take(finish - start + 1)
				.copied()
				.collect();
		}
		let Some(anchor_idx) =
			self.items.iter().position(|e| e.id == anchor)
		else {
			return Vec::new();
		};
		let current = self.selection;
		let offset = self.items.index_offset();
		let item_count = self.items.iter().count();
		if current < offset
			|| current >= offset.saturating_add(item_count)
		{
			return Vec::new();
		}
		let end = current - offset;
		let (start, finish) = if anchor_idx <= end {
			(anchor_idx, end)
		} else {
			(end, anchor_idx)
		};
		self.items
			.iter()
			.skip(start)
			.take(finish - start + 1)
			.map(|e| e.id)
			.collect()
	}

	fn update_modal_range(&mut self) {
		if !self.selection_extending {
			return;
		}
		let Some(anchor) = self.selection_anchor else {
			self.selection_extending = false;
			self.marked.clear();
			return;
		};
		let anchor_available = if self.commits.is_empty() {
			self.items.iter().any(|entry| entry.id == anchor)
		} else {
			self.commits.get_index_of(&anchor).is_some()
		};
		if !anchor_available {
			// Discard only the generated range when its stable anchor is no
			// longer available. Explicit marks are retained after extension ends.
			self.selection_anchor = None;
			self.selection_extending = false;
			self.marked.clear();
			return;
		}
		let ids = self.selection_range_ids();
		if ids.is_empty() {
			// The cursor can temporarily move beyond the loaded slice before
			// its replacement arrives. Keep the prior range until then.
			return;
		}
		let first_index = self
			.commits
			.get_index_of(&ids[0])
			.unwrap_or_else(|| self.items.index_offset());
		self.marked = ids
			.into_iter()
			.enumerate()
			.map(|(offset, id)| (first_index + offset, id))
			.collect();
	}

	fn begin_modal_selection(&mut self) {
		let Some(id) = self.selected_commit_id() else {
			return;
		};
		self.selection_anchor = Some(id);
		self.selection_extending = true;
		self.marked.clear();
		self.update_modal_range();
	}

	fn clear_modal_selection(&mut self) -> bool {
		let changed = self.selection_anchor.is_some()
			|| self.selection_extending
			|| !self.marked.is_empty();
		self.selection_anchor = None;
		self.selection_extending = false;
		self.marked.clear();
		changed
	}

	///
	pub fn marked_commits(&self) -> Vec<CommitId> {
		let (_, commits): (Vec<_>, Vec<CommitId>) =
			self.marked.iter().copied().unzip();

		commits
	}

	/// Build string of marked or selected (if none are marked) commit ids
	fn concat_selected_commit_ids(&self) -> Option<String> {
		match self.marked.as_slice() {
			[] => self.selected_commit_id().map(|id| id.to_string()),
			marked => Some(
				marked
					.iter()
					.map(|(_idx, commit)| commit.to_string())
					.join(" "),
			),
		}
	}

	/// Copy currently marked or selected (if none are marked) commit ids
	/// to clipboard
	pub fn copy_commit_hash(&self) -> Result<()> {
		if let Some(yank) = self.concat_selected_commit_ids() {
			crate::clipboard::copy_string(&yank)?;
			self.queue.push(InternalEvent::ShowInfoMsg(
				strings::copy_success(&yank),
			));
		}
		Ok(())
	}

	///
	pub fn checkout(&self) {
		if self.has_multiple_action_commits() {
			self.queue.push(InternalEvent::ShowErrorMsg(
				"This action requires one commit; clear selection or select one".to_owned(),
			));
		} else if let Some(commit_hash) = self.action_commit() {
			try_or_popup!(
				self,
				"failed to checkout commit:",
				checkout_commit(&self.repo.borrow(), commit_hash)
			);
		}
	}

	///
	pub const fn local_branches(
		&self,
	) -> &std::collections::BTreeMap<CommitId, Vec<BranchInfo>> {
		&self.local_branches
	}

	///
	pub const fn remote_branches(
		&self,
	) -> &std::collections::BTreeMap<CommitId, Vec<BranchInfo>> {
		&self.remote_branches
	}

	///
	pub fn set_local_branches(
		&mut self,
		local_branches: Vec<BranchInfo>,
	) {
		self.local_branches.clear();

		for local_branch in local_branches {
			self.local_branches
				.entry(local_branch.top_commit)
				.or_default()
				.push(local_branch);
		}

		// branch tips and head are baked into graph rows
		self.items.graph_ready = false;
	}

	///
	pub fn set_remote_branches(
		&mut self,
		remote_branches: Vec<BranchInfo>,
	) {
		self.remote_branches.clear();

		for remote_branch in remote_branches {
			self.remote_branches
				.entry(remote_branch.top_commit)
				.or_default()
				.push(remote_branch);
		}

		// branch tips are baked into graph rows
		self.items.graph_ready = false;
	}

	///
	pub fn set_commits(&mut self, commits: IndexSet<CommitId>) {
		if commits != self.commits {
			self.items.clear();
			self.scroll_top.set(0);
			self.graph_col_width.set(0);
			self.commits = commits;
			self.selection = self
				.selection
				.min(self.commits.len().saturating_sub(1));
			self.highlighted_selection = None;
			self.selection_anchor = None;
			self.selection_extending = false;
			self.marked.clear();
		}
	}

	/// Append IDs published by the bounded background log walk.
	pub fn refresh_extend_data(&mut self, commits: Vec<CommitId>) {
		self.commits.extend(commits);
		self.update_modal_range();
	}

	/// Rebuild the displayed metadata range from the worker's ID cache.
	pub fn refresh_metadata_range(
		&mut self,
		start: usize,
		infos: Vec<CommitInfo>,
	) {
		self.set_metadata_range(start, infos);
	}

	/// IDs in a requested metadata range, bounded by the known log prefix.
	pub fn ids_in_range(
		&self,
		start: usize,
		end: usize,
	) -> Vec<CommitId> {
		self.commits
			.iter()
			.skip(start)
			.take(end.saturating_sub(start))
			.copied()
			.collect()
	}

	///
	pub fn set_highlighting(
		&mut self,
		highlighting: Option<Rc<IndexSet<CommitId>>>,
	) {
		//note: set highlights to none if there is no highlight
		self.highlights = if highlighting
			.as_ref()
			.is_some_and(|set| set.is_empty())
		{
			None
		} else {
			highlighting
		};
		self.items.set_highlights(self.highlights.as_deref());

		self.select_next_highlight();
		self.set_highlighted_selection_index();
	}

	///
	pub fn select_commit(&mut self, id: CommitId) -> Result<()> {
		let index = self.commits.get_index_of(&id);

		if let Some(index) = index {
			self.selection = index;
			self.set_highlighted_selection_index();
			Ok(())
		} else {
			anyhow::bail!("Could not select commit. It might not be loaded yet or it might be on a different branch.");
		}
	}

	///
	pub fn highlighted_selection_info(&self) -> (usize, usize) {
		let amount = self
			.highlights
			.as_ref()
			.map(|highlights| highlights.len())
			.unwrap_or_default();
		(self.highlighted_selection.unwrap_or_default(), amount)
	}

	fn set_highlighted_selection_index(&mut self) {
		self.highlighted_selection =
			self.highlights.as_ref().and_then(|highlights| {
				highlights.iter().position(|entry| {
					Some(entry) == self.selected_commit_id().as_ref()
				})
			});
	}

	/// will return view size or None before the first render
	const fn current_size(&self) -> Option<(u16, u16)> {
		self.current_size.get()
	}

	fn selection_max(&self) -> usize {
		self.loaded_commit_count().saturating_sub(1)
	}

	fn selected_entry_marked(&self) -> bool {
		self.selected_commit_id()
			.and_then(|id| self.is_marked(&id))
			.unwrap_or_default()
	}

	fn move_selection(&mut self, scroll: ScrollType) -> Result<bool> {
		let needs_update = if self.items.highlighting() {
			self.move_selection_highlighting(scroll)?
		} else {
			self.move_selection_normal(scroll)?
		};

		Ok(needs_update)
	}

	fn move_selection_highlighting(
		&mut self,
		scroll: ScrollType,
	) -> Result<bool> {
		let (current_index, selection_max) =
			self.highlighted_selection_info();

		let new_index = match scroll {
			ScrollType::Up => current_index.saturating_sub(1),
			ScrollType::Down => current_index.saturating_add(1),

			//TODO: support this?
			// ScrollType::Home => 0,
			// ScrollType::End => self.selection_max(),
			_ => return Ok(false),
		};

		let new_index =
			cmp::min(new_index, selection_max.saturating_sub(1));

		let index_changed = new_index != current_index;

		if !index_changed {
			return Ok(false);
		}

		let new_selected_commit =
			self.highlights.as_ref().and_then(|highlights| {
				highlights.iter().nth(new_index).copied()
			});

		if let Some(c) = new_selected_commit {
			self.select_commit(c)?;
			return Ok(true);
		}

		Ok(false)
	}

	fn move_selection_normal(
		&mut self,
		scroll: ScrollType,
	) -> Result<bool> {
		if self.key_config.is_modal() {
			self.scroll_state.1 = 0.0;
		} else {
			self.update_scroll_speed();
		}

		#[allow(clippy::cast_possible_truncation)]
		let speed_int = if self.key_config.is_modal() {
			1
		} else {
			usize::try_from(self.scroll_state.1 as i64)?.max(1)
		};

		let page_offset = usize::from(
			self.current_size.get().unwrap_or_default().1,
		)
		.saturating_sub(1);

		let new_selection = match scroll {
			ScrollType::Up => {
				self.selection.saturating_sub(speed_int)
			}
			ScrollType::Down => {
				self.selection.saturating_add(speed_int)
			}
			ScrollType::PageUp => {
				self.selection.saturating_sub(page_offset)
			}
			ScrollType::PageDown => {
				self.selection.saturating_add(page_offset)
			}
			ScrollType::Home => 0,
			ScrollType::End => self.selection_max(),
		};

		let new_selection =
			cmp::min(new_selection, self.selection_max());
		let needs_update = new_selection != self.selection;

		self.selection = new_selection;
		self.update_modal_range();

		Ok(needs_update)
	}

	fn mark(&mut self) {
		if let Some(id) = self.selected_commit_id() {
			// Selection is an index into the full commit set. Fall back to it
			// for tests and lists whose backing set has not been populated.
			let selected = self
				.commits
				.get_index_of(&id)
				.unwrap_or(self.selection);
			if self.is_marked(&id).unwrap_or_default() {
				self.marked.retain(|marked| marked.1 != id);
			} else {
				self.marked.push((selected, id));

				self.marked.sort_unstable_by(|first, second| {
					first.0.cmp(&second.0)
				});
			}
		}
	}

	fn toggle_modal_item(&mut self) {
		self.mark();
		if self.selection_extending {
			self.finish_selection();
		}
	}

	fn update_scroll_speed(&mut self) {
		const REPEATED_SCROLL_THRESHOLD_MILLIS: u128 = 300;
		const SCROLL_SPEED_START: f32 = 0.1_f32;
		const SCROLL_SPEED_MAX: f32 = 10_f32;
		const SCROLL_SPEED_MULTIPLIER: f32 = 1.05_f32;

		let now = Instant::now();

		let since_last_scroll =
			now.duration_since(self.scroll_state.0);

		self.scroll_state.0 = now;

		let speed = if since_last_scroll.as_millis()
			< REPEATED_SCROLL_THRESHOLD_MILLIS
		{
			self.scroll_state.1 * SCROLL_SPEED_MULTIPLIER
		} else {
			SCROLL_SPEED_START
		};

		self.scroll_state.1 = speed.min(SCROLL_SPEED_MAX);
	}

	fn is_marked(&self, id: &CommitId) -> Option<bool> {
		if self.marked.is_empty() {
			None
		} else {
			let found =
				self.marked.iter().any(|entry| entry.1 == *id);
			Some(found)
		}
	}

	pub(crate) fn graph_row_spans(
		row: &GraphRow,
		graph_col_width: usize,
		_empty_lanes: &std::collections::HashSet<usize>,
	) -> Vec<Span<'static>> {
		let mut spans = Vec::new();
		let lane_limit = graph_col_width / 2;

		for (lane_index, conn) in
			row.lanes.iter().take(lane_limit).enumerate()
		{
			let (sym, graph_color) = match conn {
				None => (SYM_SPACE, Color::Reset),
				Some((conn_type, color_idx)) => (
					symbol(*conn_type),
					GRAPH_COLORS[usize::from(*color_idx)
						% GRAPH_COLORS.len()],
				),
			};
			spans.push(Span::styled(
				sym,
				Style::default().fg(graph_color),
			));

			// Spacer
			let mut is_bridge_lane = false;
			let mut spacer_color = graph_color;

			let commit_lane = usize::from(row.commit_lane);

			let edges = row
				.merge_bridge
				.into_iter()
				.chain(row.branches.iter().copied());

			for (st, fin) in edges {
				let (start, finish) =
					(usize::from(st), usize::from(fin));

				if (start..finish).contains(&lane_index) {
					is_bridge_lane = true;
					let target_lane = if commit_lane == start {
						finish
					} else {
						start
					};
					spacer_color = GRAPH_COLORS
						[target_lane % GRAPH_COLORS.len()];
				}
			}

			if is_bridge_lane {
				spans.push(Span::styled(
					SYM_HORIZONTAL,
					Style::default().fg(spacer_color),
				));
				continue;
			}
			spans.push(Span::raw(SYM_SPACE));
		}

		let truncated = row.lanes.len() > lane_limit
			|| row
				.merge_bridge
				.into_iter()
				.chain(row.branches.iter().copied())
				.any(|(_, finish)| usize::from(finish) >= lane_limit);
		let current_width = spans.len();
		for _ in current_width..graph_col_width {
			spans.push(Span::raw(SYM_SPACE));
		}
		if truncated && lane_limit > 0 {
			// The graph continues beyond the visible lanes. Reserve its final
			// cell for an explicit continuation mark instead of silently hiding
			// parent or branch lanes at the right edge of a narrow pane.
			if let Some(last) = spans.get_mut(graph_col_width - 1) {
				last.content = Cow::Borrowed("…");
				last.style = Style::default().fg(Color::Gray);
			}
		}

		spans
	}

	#[allow(clippy::too_many_arguments)]
	fn get_entry_to_add<'a>(
		&self,
		e: &'a LogEntry,
		selected: bool,
		cursor: bool,
		tags: Option<String>,
		local_branches: Option<String>,
		remote_branches: Option<String>,
		theme: &Theme,
		width: usize,
		now: DateTime<Local>,
		marked: Option<bool>,
		empty_lanes: &std::collections::HashSet<usize>,
	) -> Line<'a> {
		let mut txt: Vec<Span> = Vec::with_capacity(
			ELEMENTS_PER_LINE + if marked.is_some() { 2 } else { 0 },
		);
		let row_selected = selected
			|| (self.key_config.is_modal()
				&& marked.unwrap_or(false));

		if self.show_graph {
			let graph_start = txt.len();
			self.add_graph_spans(e, &mut txt, empty_lanes);
			if row_selected {
				for span in &mut txt[graph_start..] {
					span.style = theme.selected(span.style, true);
				}
			}
		}

		let normal = !self.items.highlighting()
			|| (self.items.highlighting() && e.highlighted);

		let splitter = Span::styled(
			Cow::from(symbol::EMPTY_SPACE),
			if normal {
				theme.text(true, row_selected)
			} else {
				theme.selected(Style::default(), row_selected)
			},
		);

		// marker
		if self.key_config.is_modal() {
			txt.push(Span::styled(
				Cow::from(if cursor { "›" } else { " " }),
				theme
					.log_marker(row_selected)
					.add_modifier(Modifier::BOLD),
			));
			txt.push(splitter.clone());
		} else if let Some(marked) = marked {
			txt.push(Span::styled(
				Cow::from(if marked {
					symbol::CHECKMARK
				} else {
					symbol::EMPTY_SPACE
				}),
				theme.log_marker(selected),
			));
			txt.push(splitter.clone());
		}

		Self::add_entry_details(
			e,
			&mut txt,
			row_selected,
			normal,
			theme,
			width,
			now,
			tags,
			local_branches,
			remote_branches,
			&splitter,
		);

		Line::from(txt)
	}

	fn add_graph_spans<'a>(
		&self,
		e: &'a LogEntry,
		txt: &mut Vec<Span<'a>>,
		empty_lanes: &std::collections::HashSet<usize>,
	) {
		if let Some(ref row) = e.graph {
			txt.extend(Self::graph_row_spans(
				row,
				self.graph_col_width.get(),
				empty_lanes,
			));
		} else {
			txt.push(Span::raw(
				" ".repeat(self.graph_col_width.get()),
			));
		}
		if self.graph_col_width.get() > 0 {
			txt.push(Span::raw(" "));
		}
	}

	#[allow(clippy::too_many_arguments)]
	fn add_entry_details<'a>(
		e: &'a LogEntry,
		txt: &mut Vec<Span<'a>>,
		selected: bool,
		normal: bool,
		theme: &Theme,
		width: usize,
		now: DateTime<Local>,
		tags: Option<String>,
		local_branches: Option<String>,
		remote_branches: Option<String>,
		splitter: &Span<'a>,
	) {
		let (
			style_hash,
			style_time,
			style_author,
			style_tags,
			style_branches,
			style_msg,
		) = if normal {
			(
				theme.commit_hash(selected),
				theme.commit_time(selected),
				theme.commit_author(selected),
				theme.tags(selected),
				theme.branch(selected, true),
				theme.text(true, selected),
			)
		} else {
			let s = theme
				.selected(theme.commit_unhighlighted(), selected);
			(s, s, s, s, s, s)
		};

		// commit hash
		txt.push(Span::styled(Cow::from(&*e.hash_short), style_hash));
		txt.push(splitter.clone());

		// commit timestamp
		txt.push(Span::styled(
			Cow::from(e.time_to_string(now)),
			style_time,
		));
		txt.push(splitter.clone());

		let author_width =
			(width.saturating_sub(19) / 3).clamp(3, 20);
		let author = string_width_align(&e.author, author_width);

		// commit author
		txt.push(Span::styled(author, style_author));
		txt.push(splitter.clone());

		// commit tags
		if let Some(tags) = tags {
			txt.push(splitter.clone());
			txt.push(Span::styled(tags, style_tags));
		}

		if let Some(local_branches) = local_branches {
			txt.push(splitter.clone());
			txt.push(Span::styled(local_branches, style_branches));
		}
		if let Some(remote_branches) = remote_branches {
			txt.push(splitter.clone());
			txt.push(Span::styled(remote_branches, style_branches));
		}

		txt.push(splitter.clone());

		let used_width = txt
			.iter()
			.map(|span| UnicodeWidthStr::width(span.content.as_ref()))
			.sum();
		let message_width = width.saturating_sub(used_width);
		let message = escape_display_label(e.msg.as_ref());
		let message_padding = message_width
			.saturating_sub(UnicodeWidthStr::width(message.as_str()));

		// commit msg
		txt.push(Span::styled(
			format!("{}{}", message, " ".repeat(message_padding)),
			style_msg,
		));
	}

	fn get_text(&self, height: usize, width: usize) -> Vec<Line<'_>> {
		let now = Local::now();
		let any_marked = !self.marked.is_empty();

		if self.show_graph {
			// Keep lane positions stable while the viewport scrolls. Compacting
			// from the rows on screen alone made a lane shift horizontally as
			// soon as its only visible connector scrolled away.
			let max_lane = self
				.items
				.iter()
				.filter_map(|e| e.graph.as_ref())
				.map(|row| row.lanes.len())
				.max()
				.unwrap_or(0);
			// Leave room for the commit id and selection marker. A capped graph
			// row shows an ellipsis in its last visible graph cell.
			let graph_width = GRAPH_WIDTH_LIMIT
				.min(width / 3)
				.min(width.saturating_sub(GRAPH_DETAIL_RESERVE));
			let visible_lanes = max_lane.min(graph_width / 2);
			self.graph_col_width.set(visible_lanes * 2);
		} else {
			self.graph_col_width.set(0);
		}

		self.collect_entries(height, width, now, any_marked)
	}

	fn collect_entries(
		&self,
		height: usize,
		width: usize,
		now: DateTime<Local>,
		any_marked: bool,
	) -> Vec<Line<'_>> {
		let loaded = self.loaded_commit_count();
		let start = self.scroll_top.get().min(loaded);
		(start..start.saturating_add(height).min(loaded))
			.filter_map(|global_index| {
				let id = self.display_commit_id(global_index)?;
				let cursor = global_index == self.selection;
				let is_highlighted = self
					.highlights
					.as_ref()
					.is_some_and(|ids| ids.contains(&id));
				let marked = if self.key_config.is_modal() {
					Some(self.is_marked(&id).unwrap_or(false))
				} else {
					any_marked.then(|| self.is_marked(&id)).flatten()
				};
				let selected = cursor
					|| (self.key_config.is_modal()
						&& marked.unwrap_or(false));

				let Some(entry) = self.items.get_at(global_index)
				else {
					return Some(self.get_pending_entry_to_add(
						id,
						selected,
						cursor,
						width,
						marked,
						is_highlighted,
					));
				};
				let tags =
					self.tags.as_ref().and_then(|t| t.get(&id)).map(
						|tags| {
							tags.iter()
								.map(|tag| format!("<{}>", tag.name))
								.join(" ")
						},
					);

				let local_branches = self
					.local_branches
					.get(&id)
					.map(|local_branch| {
						local_branch
							.iter()
							.map(|branch| {
								format!("{{{}}}", branch.name)
							})
							.join(" ")
					});

				Some(self.get_entry_to_add(
					entry,
					selected,
					cursor,
					tags,
					local_branches,
					self.remote_branches_string(entry),
					&self.theme,
					width,
					now,
					marked,
					&HashSet::new(),
				))
			})
			.collect()
	}

	fn get_pending_entry_to_add(
		&self,
		id: CommitId,
		selected: bool,
		cursor: bool,
		width: usize,
		marked: Option<bool>,
		highlighted: bool,
	) -> Line<'static> {
		let mut spans: Vec<Span<'static>> = Vec::new();
		if self.show_graph {
			spans.push(Span::raw(" ".repeat(
				self.graph_col_width.get().saturating_add(1),
			)));
		}
		if self.key_config.is_modal() {
			spans.push(Span::styled(
				if cursor { "›" } else { " " },
				self.theme
					.log_marker(selected)
					.add_modifier(Modifier::BOLD),
			));
			spans.push(Span::raw(" "));
		} else if let Some(marked) = marked {
			spans.push(Span::styled(
				if marked { symbol::CHECKMARK } else { " " },
				self.theme.log_marker(selected),
			));
			spans.push(Span::raw(" "));
		}

		let hash_style = if highlighted {
			self.theme
				.selected(self.theme.commit_unhighlighted(), selected)
		} else {
			self.theme.commit_hash(selected)
		};
		spans.push(Span::styled(id.get_short_string(), hash_style));
		spans.push(Span::styled(
			" ".to_owned(),
			self.theme.text(true, selected),
		));
		spans.push(Span::raw(
			" ".repeat(10 + (width / 3).clamp(3, 20) + 1),
		));
		spans.push(Span::styled(
			"Loading…".to_owned(),
			self.theme.text(true, selected),
		));
		Line::from(spans)
	}

	fn display_commit_id(&self, index: usize) -> Option<CommitId> {
		self.commits.get_index(index).copied().or_else(|| {
			self.items.get_at(index).map(|entry| entry.id)
		})
	}

	fn remote_branches_string(&self, e: &LogEntry) -> Option<String> {
		self.remote_branches.get(&e.id).and_then(|remote_branches| {
			let filtered_branches: Vec<_> = remote_branches
				.iter()
				.filter(|remote_branch| {
					self.local_branches.get(&e.id).is_none_or(
						|local_branch| {
							local_branch.iter().any(|local_branch| {
								let has_corresponding_local_branch =
									match &local_branch.details {
										BranchDetails::Local(
											details,
										) => details
											.upstream
											.as_ref()
											.is_some_and(
												|upstream| {
													upstream.reference == remote_branch.reference
												},
											),
										BranchDetails::Remote(_) => {
											false
										}
									};

								!has_corresponding_local_branch
							})
						},
					)
				})
				.map(|remote_branch| {
					format!("[{0}]", remote_branch.name)
				})
				.collect();

			if filtered_branches.is_empty() {
				None
			} else {
				Some(filtered_branches.join(" "))
			}
		})
	}

	const fn relative_selection(&self) -> usize {
		self.selection
	}

	fn pointer_entry_id(
		&self,
		column: u16,
		row: u16,
	) -> Option<CommitId> {
		let area = self.last_draw_area.get();
		if column < area.x.saturating_add(1)
			|| column
				>= area.x.saturating_add(area.width.saturating_sub(1))
			|| row < area.y.saturating_add(1)
			|| row
				>= area
					.y
					.saturating_add(area.height.saturating_sub(1))
		{
			return None;
		}
		let row_offset =
			usize::from(row.saturating_sub(area.y.saturating_add(1)));
		self.last_draw_rows.borrow().get(row_offset).copied()
	}

	const fn pointer_inside(&self, column: u16, row: u16) -> bool {
		let area = self.last_draw_area.get();
		column >= area.x.saturating_add(1)
			&& column
				< area.x.saturating_add(area.width.saturating_sub(1))
			&& row >= area.y.saturating_add(1)
			&& row
				< area.y.saturating_add(area.height.saturating_sub(1))
	}

	fn pointer_event(
		&mut self,
		mouse: MouseEvent,
	) -> Result<EventState> {
		match mouse.kind {
			MouseEventKind::Down(MouseButton::Left) => {
				let Some(id) =
					self.pointer_entry_id(mouse.column, mouse.row)
				else {
					return Ok(EventState::NotConsumed);
				};
				// The draw-time row map may outlive a refresh of the loaded
				// commit window. Consume that stale hit without disturbing a
				// deliberate selection or starting a drag from the old cursor.
				if self.commits.get_index_of(&id).is_none()
					&& !self.items.iter().any(|entry| entry.id == id)
				{
					return Ok(EventState::Consumed);
				}
				let old_anchor =
					self.selected_entry().map(|entry| entry.id);
				let shift = self.key_config.is_modal()
					&& mouse.modifiers.contains(KeyModifiers::SHIFT);
				if self.key_config.is_modal()
					&& self.selection_extending
					&& !shift
				{
					self.clear_modal_selection();
				}
				self.set_pointer_selection(id);
				self.pointer_anchor =
					self.selected_entry().map(|entry| entry.id);
				self.pointer_dragging = true;
				if shift {
					if let Some(anchor) = old_anchor {
						self.selection_anchor = Some(anchor);
						self.selection_extending = true;
						self.update_modal_range();
					}
				}
				Ok(EventState::Consumed)
			}
			MouseEventKind::Drag(MouseButton::Left)
				if self.pointer_dragging =>
			{
				if let Some(id) =
					self.pointer_entry_id(mouse.column, mouse.row)
				{
					self.set_pointer_selection(id);
					if self.key_config.is_modal() {
						if let Some(anchor) = self.pointer_anchor {
							if !self.selection_extending {
								self.selection_anchor = Some(anchor);
								self.selection_extending = true;
							}
							self.update_modal_range();
						}
					}
				}
				Ok(EventState::Consumed)
			}
			MouseEventKind::Up(MouseButton::Left)
				if self.pointer_dragging =>
			{
				self.pointer_dragging = false;
				self.pointer_anchor = None;
				if self.selection_extending {
					self.finish_selection();
				}
				Ok(EventState::Consumed)
			}
			MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
				if !self.pointer_inside(mouse.column, mouse.row) {
					return Ok(EventState::NotConsumed);
				}
				let dir =
					if matches!(mouse.kind, MouseEventKind::ScrollUp)
					{
						ScrollType::Up
					} else {
						ScrollType::Down
					};
				self.move_selection(dir)?;
				Ok(EventState::Consumed)
			}
			_ => Ok(EventState::NotConsumed),
		}
	}

	fn set_pointer_selection(&mut self, id: CommitId) -> bool {
		let index = self.commits.get_index_of(&id).or_else(|| {
			self.items.iter().position(|entry| entry.id == id).map(
				|relative| {
					self.items.index_offset().saturating_add(relative)
				},
			)
		});
		let Some(index) = index else {
			return false;
		};
		let changed = self.selection != index;
		self.selection = index;
		self.set_highlighted_selection_index();
		self.update_modal_range();
		changed
	}

	fn select_next_highlight(&mut self) {
		if self.highlights.is_none() {
			return;
		}

		let old_selection = self.selection;

		let mut offset = 0;
		loop {
			let hit_upper_bound =
				old_selection + offset > self.selection_max();
			let hit_lower_bound = offset > old_selection;

			if !hit_upper_bound {
				self.selection = old_selection + offset;

				if self.selection_highlighted() {
					break;
				}
			}

			if !hit_lower_bound {
				self.selection = old_selection - offset;

				if self.selection_highlighted() {
					break;
				}
			}

			if hit_lower_bound && hit_upper_bound {
				self.selection = old_selection;
				break;
			}

			offset += 1;
		}
	}

	fn selection_highlighted(&self) -> bool {
		self.selected_commit_id().is_some_and(|commit| {
			self.highlights.as_ref().is_some_and(|highlights| {
				highlights.contains(&commit)
			})
		})
	}

	fn title(&self) -> String {
		let loaded = self.loaded_commit_count();
		let selected_position = if loaded == 0 {
			0
		} else {
			self.selection.saturating_add(1)
		};
		let more_text = if self
			.history_status
			.is_none_or(|status| status.has_more && !status.complete)
		{
			format!(
				" · {} older",
				self.key_config.get_hint(
					self.key_config.keys.branches_load_more
				)
			)
		} else {
			String::new()
		};
		format!(
			"{} {} / {} loaded{}{}{}",
			self.title,
			selected_position,
			loaded,
			more_text,
			if self
				.history_status
				.is_some_and(|status| status.pending)
			{
				" · loading"
			} else {
				""
			},
			self.selection_status().map_or_else(
				String::new,
				|status| {
					format!(
						"  [{} {}{}]",
						status.count,
						status.unit,
						if status.extending {
							" · extending"
						} else {
							""
						}
					)
				}
			),
		)
	}
}

impl DrawableComponent for CommitList {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		self.last_draw_area.set(area);
		let current_size = (
			area.width.saturating_sub(2),
			area.height.saturating_sub(2),
		);
		self.current_size.set(Some(current_size));

		let height_in_lines = current_size.1 as usize;
		let selection = self.relative_selection();

		self.scroll_top.set(clamp_scroll_top(
			self.scroll_top.get(),
			height_in_lines,
			selection,
			self.loaded_commit_count(),
		));
		*self.last_draw_rows.borrow_mut() = self
			.commits
			.iter()
			.skip(self.scroll_top.get())
			.take(height_in_lines)
			.copied()
			.collect();
		if self.commits.is_empty() {
			*self.last_draw_rows.borrow_mut() = self
				.items
				.iter()
				.skip(
					self.scroll_top
						.get()
						.saturating_sub(self.items.index_offset()),
				)
				.take(height_in_lines)
				.map(|entry| entry.id)
				.collect();
		}
		let title = self.title();

		f.render_widget(
			Paragraph::new(
				self.get_text(
					height_in_lines,
					current_size.0 as usize,
				),
			)
			.block(
				Block::default()
					.borders(Borders::ALL)
					.title(Span::styled(
						title.as_str(),
						self.theme.title(true),
					))
					.border_style(self.theme.block(true)),
			)
			.alignment(Alignment::Left),
			area,
		);

		draw_scrollbar(
			f,
			area,
			&self.theme,
			self.commits.len(),
			self.selection,
			Orientation::Vertical,
		);

		Ok(())
	}
}

impl Component for CommitList {
	fn selection_status(
		&self,
	) -> Option<crate::components::SelectionStatus> {
		Self::selection_status(self)
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if let Event::Mouse(mouse) = ev {
			return self.pointer_event(*mouse);
		}

		if let Event::Key(k) = ev {
			if self.key_config.is_modal() {
				if key_match(k, self.key_config.keys.select_mode) {
					if self.selection_extending {
						self.finish_selection();
					} else {
						self.begin_modal_selection();
					}
					return Ok(EventState::Consumed);
				}
				if key_match(k, self.key_config.keys.select_clear)
					&& self.clear_modal_selection()
				{
					return Ok(EventState::Consumed);
				}
				if key_match(k, self.key_config.keys.select_all) {
					let ids: Vec<_> = self
						.items
						.iter()
						.map(|entry| entry.id)
						.collect();
					if let (Some(first), false) =
						(ids.first().copied(), ids.is_empty())
					{
						self.selection_anchor = Some(first);
						self.selection_extending = false;
						self.marked = ids
							.into_iter()
							.enumerate()
							.map(|(idx, id)| {
								(self.items.index_offset() + idx, id)
							})
							.collect();
					}
					return Ok(EventState::Consumed);
				}
				if key_match(k, self.key_config.keys.select_item) {
					self.toggle_modal_item();
					return Ok(EventState::Consumed);
				}
			}
			let selection_changed =
				if key_match(k, self.key_config.keys.move_up) {
					self.move_selection(ScrollType::Up)?
				} else if key_match(k, self.key_config.keys.move_down)
				{
					self.move_selection(ScrollType::Down)?
				} else if key_match(k, self.key_config.keys.shift_up)
					|| key_match(k, self.key_config.keys.home)
				{
					self.move_selection(ScrollType::Home)?
				} else if key_match(
					k,
					self.key_config.keys.shift_down,
				) || key_match(k, self.key_config.keys.end)
				{
					self.move_selection(ScrollType::End)?
				} else if key_match(k, self.key_config.keys.page_up) {
					self.move_selection(ScrollType::PageUp)?
				} else if key_match(k, self.key_config.keys.page_down)
				{
					self.move_selection(ScrollType::PageDown)?
				} else if key_match(
					k,
					self.key_config.keys.log_mark_commit,
				) {
					if self.key_config.is_modal() {
						self.toggle_modal_item();
					} else {
						self.mark();
					}
					true
				} else if key_match(
					k,
					self.key_config.keys.log_checkout_commit,
				) {
					self.checkout();
					true
				} else if key_match(
					k,
					self.key_config.keys.log_toggle_graph,
				) {
					self.show_graph = !self.show_graph;
					true
				} else {
					false
				};
			return Ok(selection_changed.into());
		}

		Ok(EventState::NotConsumed)
	}

	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		_force_all: bool,
	) -> CommandBlocking {
		out.push(CommandInfo::new(
			strings::commands::scroll(&self.key_config),
			self.selected_entry().is_some(),
			true,
		));
		out.push(CommandInfo::new(
			strings::commands::commit_list_mark(
				&self.key_config,
				self.selected_entry_marked(),
			),
			true,
			true,
		));
		out.push(CommandInfo::new(
			strings::commands::log_toggle_graph(&self.key_config),
			true,
			true,
		));
		CommandBlocking::PassingOn
	}

	fn focus(&mut self, focused: bool) {
		if !focused {
			self.pointer_dragging = false;
			self.pointer_anchor = None;
			if self.selection_extending {
				self.finish_selection();
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::super::utils::graphrow::{SYM_COMMIT, SYM_VERTICAL};
	use crate::keys::{KeyConfig, Keymap};
	use asyncgit::graph::ConnectionType;
	use asyncgit::sync::{CommitInfo, RepoPath};
	use crossterm::event::{KeyModifiers, MouseEvent};

	use super::*;

	impl Default for CommitList {
		fn default() -> Self {
			Self {
				title: String::new().into_boxed_str(),
				selection: 0,
				highlighted_selection: Option::None,
				highlights: Option::None,
				tags: Option::None,
				items: ItemBatch::default(),
				commits: IndexSet::default(),
				history_status: None,
				marked: Vec::default(),
				selection_anchor: None,
				selection_extending: false,
				scroll_top: Cell::default(),
				local_branches: BTreeMap::default(),
				remote_branches: BTreeMap::default(),
				theme: SharedTheme::default(),
				key_config: SharedKeyConfig::default(),
				scroll_state: (Instant::now(), 0.0),
				current_size: Cell::default(),
				repo: RepoPathRef::new(RepoPath::Path(
					std::path::PathBuf::default(),
				)),
				queue: Queue::default(),
				show_graph: true,
				graph_col_width: Cell::new(0),
				last_draw_area: Cell::new(Rect::default()),
				last_draw_rows: RefCell::new(Vec::new()),
				pointer_dragging: false,
				pointer_anchor: None,
			}
		}
	}

	#[test]
	fn test_string_width_align() {
		assert_eq!(string_width_align("123", 3), "123");
		assert_eq!(string_width_align("123", 2), "..");
		assert_eq!(string_width_align("123", 3), "123");
		assert_eq!(string_width_align("12345", 6), "12345 ");
		assert_eq!(string_width_align("1234556", 4), "12..");
	}

	#[test]
	fn test_string_width_align_unicode() {
		assert_eq!(string_width_align("äste", 3), "ä..");
		assert_eq!(
			string_width_align("wüsten äste", 10),
			"wüsten ä.."
		);
		assert_eq!(
			string_width_align("Jon Grythe Stødle", 19),
			"Jon Grythe Stødle  "
		);
	}

	/// Build a commit list with a few commits loaded
	fn build_commit_list_with_some_commits() -> CommitList {
		let mut items = ItemBatch::default();
		let basic_commit_info = CommitInfo {
			message: String::default(),
			time: 0,
			author: String::default(),
			id: CommitId::default(),
			parents: Vec::default().into(),
		};
		// This just creates a sequence of fake ordered ids
		// 0000000000000000000000000000000000000000
		// 0000000000000000000000000000000000000001
		// 0000000000000000000000000000000000000002
		// ...
		items.set_items(
			2, /* randomly choose an offset */
			(0..20)
				.map(|idx| CommitInfo {
					id: CommitId::from_str_unchecked(&format!(
						"{idx:040}",
					))
					.unwrap(),
					..basic_commit_info.clone()
				})
				.collect(),
			None,
		);
		CommitList {
			items,
			selection: 4, // Randomly select one commit
			..Default::default()
		}
	}

	#[test]
	fn log_range_tracks_a_bounded_viewport_and_tail_loading_state() {
		let mut list = build_commit_list_with_some_commits();
		let ids = (0..160)
			.map(|idx| {
				CommitId::from_str_unchecked(&format!("{idx:040}"))
					.unwrap()
			})
			.collect();
		list.set_commits(ids);

		let (start, end) = list.requested_log_range();
		assert_eq!(start, 0);
		assert_eq!(
			end,
			DEFAULT_LOG_VIEWPORT_ROWS + LOG_PREFETCH_ROWS
		);
		assert!(!list.should_load_more());

		list.selection = 151;
		assert!(list.should_load_more());
		list.set_history_status(LogRangeStatus {
			generation: 1,
			loaded: 160,
			pending: true,
			has_more: true,
			complete: false,
		});
		assert!(!list.should_load_more());
		list.set_history_status(LogRangeStatus {
			generation: 1,
			loaded: 160,
			pending: false,
			has_more: false,
			complete: true,
		});
		assert!(!list.should_load_more());
	}

	fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
		Event::Mouse(MouseEvent {
			kind,
			column,
			row,
			modifiers: KeyModifiers::NONE,
		})
	}

	#[test]
	fn commit_summary_display_escapes_controls_and_preserves_unicode()
	{
		assert_eq!(
			escape_display_label("wide界\tmessage\\\u{1b}"),
			"wide界\\tmessage\\\\\\u{1b}"
		);
	}

	#[test]
	fn pointer_hits_follow_drawn_commit_ids_after_scroll() {
		let mut list = build_commit_list_with_some_commits();
		list.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let backend = ratatui::backend::TestBackend::new(40, 8);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		let area = Rect::new(2, 1, 30, 7);

		terminal
			.draw(|frame| list.draw(frame, area).unwrap())
			.unwrap();
		let first = list.last_draw_rows.borrow()[0];
		assert_eq!(
			list.event(&mouse(
				MouseEventKind::Down(MouseButton::Left),
				2,
				2
			))
			.unwrap(),
			EventState::NotConsumed,
		);
		list.event(&mouse(
			MouseEventKind::Down(MouseButton::Left),
			3,
			2,
		))
		.unwrap();
		assert_eq!(list.selected_entry().unwrap().id, first);
		list.event(&mouse(
			MouseEventKind::Up(MouseButton::Left),
			3,
			2,
		))
		.unwrap();

		list.selection = 12;
		list.scroll_top.set(5);
		terminal
			.draw(|frame| list.draw(frame, area).unwrap())
			.unwrap();
		let first_scrolled = list.last_draw_rows.borrow()[0];
		list.event(&mouse(
			MouseEventKind::Down(MouseButton::Left),
			3,
			2,
		))
		.unwrap();
		assert_eq!(list.selected_entry().unwrap().id, first_scrolled);
		list.event(&mouse(
			MouseEventKind::Up(MouseButton::Left),
			3,
			2,
		))
		.unwrap();

		let before_wheel = list.selected_entry().unwrap().id;
		list.event(&mouse(MouseEventKind::ScrollDown, 3, 6))
			.unwrap();
		assert_ne!(list.selected_entry().unwrap().id, before_wheel);
	}

	#[test]
	fn modal_marked_commit_is_a_full_row_highlight_without_checkmark()
	{
		let mut list = build_commit_list_with_some_commits();
		list.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let entry = list.selected_entry().unwrap();
		let line = list.get_entry_to_add(
			entry,
			false,
			false,
			None,
			None,
			None,
			&list.theme,
			60,
			Local::now(),
			Some(true),
			&HashSet::new(),
		);
		assert!(line
			.spans
			.iter()
			.all(|span| span.style.bg.is_some()));
		assert!(line
			.spans
			.iter()
			.all(|span| !span.content.contains('✓')));
	}

	/// Build a value for cl.marked based on indices into cl.items
	fn build_marked_from_indices(
		cl: &CommitList,
		marked_indices: &[usize],
	) -> Vec<(usize, CommitId)> {
		let offset = cl.items.index_offset();
		marked_indices
			.iter()
			.map(|idx| {
				(*idx, cl.items.iter().nth(*idx - offset).unwrap().id)
			})
			.collect()
	}

	#[test]
	fn action_commit_prefers_one_modal_mark_after_cursor_moves() {
		let mut cl = build_commit_list_with_some_commits();
		cl.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		cl.marked = build_marked_from_indices(&cl, &[3]);
		cl.selection = 8;

		assert_eq!(
			cl.action_commit(),
			cl.marked.first().map(|m| m.1)
		);
	}

	#[test]
	fn action_commit_rejects_multiple_modal_marks() {
		let mut cl = build_commit_list_with_some_commits();
		cl.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		cl.marked = build_marked_from_indices(&cl, &[3, 5]);

		assert_eq!(cl.action_commit(), None);
		assert!(cl.has_multiple_action_commits());
	}

	#[test]
	fn action_commit_without_marks_uses_cursor_for_both_keymaps() {
		let legacy = build_commit_list_with_some_commits();
		let mut modal = build_commit_list_with_some_commits();
		modal.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));

		assert_eq!(
			legacy.action_commit(),
			legacy.selected_entry().map(|e| e.id)
		);
		assert_eq!(
			modal.action_commit(),
			modal.selected_entry().map(|e| e.id)
		);
	}

	#[test]
	fn test_copy_commit_list_empty() {
		assert_eq!(
			CommitList::default().concat_selected_commit_ids(),
			None
		);
	}

	#[test]
	fn test_copy_commit_none_marked() {
		let cl = CommitList {
			selection: 4,
			..build_commit_list_with_some_commits()
		};
		// ids from build_commit_list_with_some_commits() are
		// offset by two, so we expect commit id 2 for
		// selection = 4
		assert_eq!(
			cl.concat_selected_commit_ids(),
			Some(String::from(
				"0000000000000000000000000000000000000002"
			))
		);
	}

	#[test]
	fn test_copy_commit_one_marked() {
		let cl = build_commit_list_with_some_commits();
		let cl = CommitList {
			marked: build_marked_from_indices(&cl, &[3]),
			..cl
		};
		assert_eq!(
			cl.concat_selected_commit_ids(),
			Some(String::from(
				"0000000000000000000000000000000000000001",
			))
		);
	}

	#[test]
	fn test_copy_commit_range_marked() {
		let cl = build_commit_list_with_some_commits();
		let cl = CommitList {
			marked: build_marked_from_indices(&cl, &[4, 5, 6, 7]),
			..cl
		};
		assert_eq!(
			cl.concat_selected_commit_ids(),
			Some(String::from(concat!(
				"0000000000000000000000000000000000000002 ",
				"0000000000000000000000000000000000000003 ",
				"0000000000000000000000000000000000000004 ",
				"0000000000000000000000000000000000000005"
			)))
		);
	}

	#[test]
	fn test_copy_commit_random_marked() {
		let cl = build_commit_list_with_some_commits();
		let cl = CommitList {
			marked: build_marked_from_indices(&cl, &[4, 7]),
			..cl
		};
		assert_eq!(
			cl.concat_selected_commit_ids(),
			Some(String::from(concat!(
				"0000000000000000000000000000000000000002 ",
				"0000000000000000000000000000000000000005"
			)))
		);
	}

	#[test]
	fn modal_range_is_stable_when_selection_reverses() {
		let mut cl = build_commit_list_with_some_commits();
		let anchor = cl
			.items
			.iter()
			.nth(6 - cl.items.index_offset())
			.unwrap()
			.id;
		cl.selection = 6;
		cl.selection_anchor = Some(anchor);
		cl.selection_extending = true;
		cl.update_modal_range();
		assert_eq!(cl.marked_commits().len(), 1);

		cl.selection = 3;
		cl.update_modal_range();
		let selected = cl.marked_commits();
		assert_eq!(selected.len(), 4);
		assert_eq!(
			selected.first().unwrap(),
			&CommitId::from_str_unchecked(
				"0000000000000000000000000000000000000001"
			)
			.unwrap()
		);
		assert_eq!(
			selected.last().unwrap(),
			&CommitId::from_str_unchecked(
				"0000000000000000000000000000000000000004"
			)
			.unwrap()
		);
	}

	#[test]
	fn modal_range_reversal_at_loaded_boundaries_includes_both_ends()
	{
		let mut cl = build_commit_list_with_some_commits();
		cl.selection =
			cl.items.index_offset() + cl.items.iter().count() - 1;
		cl.selection_anchor =
			cl.selected_entry().map(|entry| entry.id);
		cl.selection_extending = true;
		cl.selection = cl.items.index_offset();
		cl.update_modal_range();
		assert_eq!(cl.marked.len(), cl.items.iter().count());
		assert_eq!(
			cl.marked.first().unwrap().1,
			cl.items.iter().next().unwrap().id
		);
		assert_eq!(
			cl.marked.last().unwrap().1,
			cl.items.iter().last().unwrap().id
		);
		assert_eq!(
			cl.marked.first().unwrap().0,
			cl.items.index_offset()
		);
		assert_eq!(
			cl.marked.last().unwrap().0,
			cl.items.index_offset() + cl.items.iter().count() - 1
		);
	}

	#[test]
	fn modal_status_reports_marks_after_extension_finishes() {
		let mut cl = build_commit_list_with_some_commits();
		cl.key_config = Rc::new(crate::keys::KeyConfig::with_keymap(
			crate::keys::Keymap::Helix,
		));
		cl.commits = cl.items.iter().map(|entry| entry.id).collect();
		cl.selection = 6;
		cl.begin_modal_selection();
		cl.selection = 9;
		cl.update_modal_range();
		assert_eq!(cl.selection_status().unwrap().count, 4);
		assert_eq!(
			cl.selection_status().unwrap().unit,
			"loaded commits"
		);

		// Toggling off the current marked commit ends the range and leaves
		// the actual remaining marks as the selection.
		cl.toggle_modal_item();
		let marked_count = cl.marked.len();
		assert!(!cl.selection_extending);
		assert_eq!(
			cl.selection_status().unwrap().count,
			marked_count
		);
		assert_eq!(cl.selection_status().unwrap().unit, "commits");

		cl.selection = 12;
		cl.update_modal_range();
		assert_eq!(
			cl.selection_status().unwrap().count,
			marked_count
		);
	}

	#[test]
	fn modal_status_includes_a_single_x_mark_without_an_anchor() {
		let mut cl = build_commit_list_with_some_commits();
		cl.key_config = Rc::new(crate::keys::KeyConfig::with_keymap(
			crate::keys::Keymap::Helix,
		));
		cl.selection = 4;
		cl.mark();

		let status = cl.selection_status().unwrap();
		assert_eq!(status.count, 1);
		assert_eq!(status.unit, "commits");
		assert!(!status.extending);
	}

	#[test]
	fn modal_select_all_status_names_loaded_commit_scope() {
		let mut cl = build_commit_list_with_some_commits();
		cl.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let first = cl.items.iter().next().unwrap().id;
		cl.selection_anchor = Some(first);
		cl.marked = cl
			.items
			.iter()
			.enumerate()
			.map(|(idx, entry)| {
				(cl.items.index_offset() + idx, entry.id)
			})
			.collect();

		let status = cl.selection_status().unwrap();
		assert_eq!(status.count, cl.items.iter().count());
		assert_eq!(status.unit, "loaded commits");
		assert!(!status.extending);
	}

	#[test]
	fn scroll_top_tracks_the_same_global_row_when_the_loaded_window_moves(
	) {
		assert_eq!(translate_scroll_top(800, 100, 850), 50);
		assert_eq!(translate_scroll_top(800, 100, 950), 0);
	}

	#[test]
	fn scroll_top_clamps_to_a_short_loaded_window_after_resize() {
		let mut cl = build_commit_list_with_some_commits();
		let info = CommitInfo {
			message: String::new(),
			time: 0,
			author: String::new(),
			id: CommitId::from_str_unchecked(
				"9999999999999999999999999999999999999998",
			)
			.unwrap(),
			parents: Vec::new().into(),
		};
		let last_id = CommitId::from_str_unchecked(
			"9999999999999999999999999999999999999999",
		)
		.unwrap();
		cl.set_commits([info.id, last_id].into_iter().collect());
		cl.set_metadata_range(
			0,
			vec![
				info.clone(),
				CommitInfo {
					id: last_id,
					..info
				},
			],
		);
		cl.selection = 1;
		cl.scroll_top.set(clamp_scroll_top(900, 80, 1, 2));

		let rows = cl.get_text(80, 60);

		assert_eq!(cl.scroll_top.get(), 0);
		assert_eq!(rows.len(), 2);
	}

	#[test]
	fn growing_commit_stream_keeps_selection_stable_while_metadata_loads(
	) {
		let mut cl = build_commit_list_with_some_commits();
		cl.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let commit_ids: Vec<_> = (0..384)
			.map(|idx| {
				CommitId::from_str_unchecked(&format!("{idx:040}"))
					.unwrap()
			})
			.collect();
		cl.set_commits(commit_ids[..128].iter().copied().collect());
		assert!(cl.items.is_empty());
		assert_eq!(cl.loaded_commit_count(), 128);

		cl.refresh_extend_data(commit_ids[128..256].to_vec());
		assert_eq!(cl.loaded_commit_count(), 256);

		cl.selection = 138;
		let selected_id = commit_ids[cl.selection];
		assert_eq!(cl.selected_commit_id(), Some(selected_id));

		cl.refresh_extend_data(commit_ids[256..].to_vec());
		assert_eq!(cl.loaded_commit_count(), 384);
		assert_eq!(cl.selected_commit_id(), Some(selected_id));
		assert!(cl.selected_entry().is_none());

		cl.set_metadata_range(
			138,
			vec![CommitInfo {
				message: "selected commit".to_owned(),
				time: 0,
				author: "author".to_owned(),
				id: selected_id,
				parents: Vec::new().into(),
			}],
		);
		assert_eq!(
			cl.selected_entry().map(|entry| entry.id),
			Some(selected_id)
		);

		let height = 38;
		cl.scroll_top.set(clamp_scroll_top(
			cl.scroll_top.get(),
			height,
			cl.relative_selection(),
			cl.loaded_commit_count(),
		));
		let rows = cl.get_text(height, 120);
		let row_index = cl.selection - cl.scroll_top.get();
		let selected_row = &rows[row_index];
		let rendered_row = selected_row
			.spans
			.iter()
			.map(|span| span.content.as_ref())
			.collect::<String>();
		assert!(rendered_row.contains('›'));
		let selected_prefix = selected_id.get_short_string();
		assert!(rendered_row.contains(&selected_prefix));
	}

	#[test]
	fn graph_lane_positions_do_not_shift_when_scrolling_the_viewport()
	{
		let mut cl = build_commit_list_with_some_commits();
		let short_row = GraphRow {
			lanes: vec![Some((
				ConnectionType::CommitNormal,
				0.into(),
			))],
			..GraphRow::default()
		};
		let wide_row = GraphRow {
			lanes: (0..15)
				.map(|index| {
					Some((
						if index == 0 {
							ConnectionType::CommitMerge
						} else {
							ConnectionType::Vertical
						},
						index.into(),
					))
				})
				.collect(),
			..GraphRow::default()
		};
		let mut rows =
			vec![short_row.clone(); cl.items.iter().count()];
		rows[1] = wide_row;
		cl.set_graph_rows(rows);

		let _ = cl.get_text(1, 90);
		let width_at_first_row = cl.graph_col_width.get();
		cl.scroll_top.set(2);
		let _ = cl.get_text(1, 90);

		assert_eq!(width_at_first_row, 30);
		assert_eq!(cl.graph_col_width.get(), width_at_first_row);
		let spans = CommitList::graph_row_spans(
			cl.items.iter().nth(1).unwrap().graph.as_ref().unwrap(),
			width_at_first_row,
			&HashSet::new(),
		);
		assert_eq!(spans[28].content, SYM_VERTICAL);

		let info = CommitInfo {
			message: String::new(),
			time: 0,
			author: String::new(),
			id: CommitId::from_str_unchecked(
				"8888888888888888888888888888888888888888",
			)
			.unwrap(),
			parents: Vec::new().into(),
		};
		cl.items.set_items(100, vec![info], None);
		cl.set_graph_rows(vec![short_row]);
		cl.scroll_top.set(0);
		let _ = cl.get_text(1, 90);

		assert_eq!(cl.graph_col_width.get(), 2);
	}

	#[test]
	fn graph_width_leaves_message_space_in_a_narrow_pane() {
		let mut cl = build_commit_list_with_some_commits();
		let info = CommitInfo {
			message: "short merge".to_owned(),
			time: 0,
			author: "A. Author".to_owned(),
			id: CommitId::from_str_unchecked(
				"7777777777777777777777777777777777777777",
			)
			.unwrap(),
			parents: Vec::new().into(),
		};
		cl.items.set_items(0, vec![info], None);
		let row = GraphRow {
			lane_count: 16.into(),
			commit_lane: 0.into(),
			is_merge: true,
			lanes: (0..16)
				.map(|index| {
					Some((
						match index {
							0 => ConnectionType::MergeBridgeStart,
							15 => ConnectionType::MergeBridgeEnd,
							_ => ConnectionType::MergeBridgeMid,
						},
						index.into(),
					))
				})
				.collect(),
			merge_bridge: Some((0.into(), 15.into())),
			..GraphRow::default()
		};
		cl.set_graph_rows(vec![row]);

		let line = cl.get_text(1, 90).remove(0);
		let text: String = line
			.spans
			.iter()
			.map(|span| span.content.as_ref())
			.collect();

		assert_eq!(cl.graph_col_width.get(), 30);
		assert!(text.contains("…"));
		assert!(text.contains("short merge"));
	}

	#[test]
	fn narrow_graph_row_marks_clipped_parent_lanes() {
		let row = GraphRow {
			lane_count: 15.into(),
			commit_lane: 0.into(),
			is_merge: true,
			lanes: (0..15)
				.map(|index| {
					Some((
						match index {
							0 => ConnectionType::MergeBridgeStart,
							14 => ConnectionType::MergeBridgeEnd,
							_ => ConnectionType::MergeBridgeMid,
						},
						index.into(),
					))
				})
				.collect(),
			merge_bridge: Some((0.into(), 14.into())),
			..GraphRow::default()
		};
		let spans = CommitList::graph_row_spans(
			&row,
			8,
			&std::collections::HashSet::new(),
		);

		assert_eq!(spans.len(), 8);
		assert_eq!(spans.last().unwrap().content, "…");
	}

	#[test]
	fn explicit_marks_survive_window_refresh_and_keep_commit_order() {
		let mut cl = build_commit_list_with_some_commits();
		let before_window = [
			CommitId::from_str_unchecked(
				"fffffffffffffffffffffffffffffffffffffffe",
			)
			.unwrap(),
			CommitId::from_str_unchecked(
				"ffffffffffffffffffffffffffffffffffffffff",
			)
			.unwrap(),
		];
		cl.commits = before_window
			.into_iter()
			.chain(cl.items.iter().map(|entry| entry.id))
			.collect();
		cl.selection = 5;
		cl.mark();
		let selected_id = cl.marked[0].1;

		let info = CommitInfo {
			message: String::new(),
			time: 0,
			author: String::new(),
			id: cl.commits[8],
			parents: Vec::new().into(),
		};
		cl.items.set_items(8, vec![info], None);
		cl.selection_anchor = None;
		cl.selection_extending = false;

		assert_eq!(cl.marked_commits(), vec![selected_id]);
		assert_eq!(cl.marked[0].0, 5);
	}

	#[test]
	fn modal_anchor_is_cleared_when_commit_data_changes() {
		let mut cl = build_commit_list_with_some_commits();
		cl.key_config = Rc::new(crate::keys::KeyConfig::with_keymap(
			crate::keys::Keymap::Helix,
		));
		cl.commits = cl.items.iter().map(|entry| entry.id).collect();
		cl.selection_anchor =
			cl.items.iter().next().map(|entry| entry.id);
		cl.selection_extending = true;
		cl.set_commits(IndexSet::new());
		assert!(cl.selection_anchor.is_none());
		assert!(!cl.selection_extending);
		assert!(cl.marked.is_empty());
	}

	#[test]
	fn explicit_marks_survive_ordinary_cursor_movement() {
		let mut cl = build_commit_list_with_some_commits();
		cl.mark();
		let marked = cl.marked_commits();
		cl.move_selection_normal(ScrollType::Down).unwrap();
		assert_eq!(cl.marked_commits(), marked);
	}

	#[test]
	fn stale_anchor_cannot_map_marks_into_a_new_window() {
		let mut cl = build_commit_list_with_some_commits();
		cl.selection_anchor = cl
			.items
			.iter()
			.nth(4 - cl.items.index_offset())
			.map(|e| e.id);
		cl.selection_extending = true;
		cl.update_modal_range();
		assert!(!cl.marked.is_empty());

		let info = CommitInfo {
			message: String::new(),
			time: 0,
			author: String::new(),
			id: CommitId::from_str_unchecked(
				"9999999999999999999999999999999999999999",
			)
			.unwrap(),
			parents: Vec::new().into(),
		};
		cl.items.set_items(0, vec![info], None);
		cl.update_modal_range();
		assert!(cl.selection_anchor.is_none());
		assert!(!cl.selection_extending);
		assert!(cl.marked.is_empty());
	}

	#[test]
	fn test_build_graph_spans() {
		let row = GraphRow {
			lane_count: 1.into(),
			commit_lane: 0.into(),
			is_merge: false,
			is_branch_tip: false,
			is_stash: false,
			lanes: vec![Some((
				ConnectionType::CommitNormal,
				0.into(),
			))],
			merge_bridge: None,
			branches: vec![],
		};
		let spans = CommitList::graph_row_spans(
			&row,
			2,
			&std::collections::HashSet::new(),
		);

		assert_eq!(spans.len(), 2);
		assert_eq!(spans[0].content, Cow::from(SYM_COMMIT));
		assert_eq!(spans[1].content, Cow::from(SYM_SPACE));
	}

	#[test]
	fn graph_spans_preserve_physical_lane_positions() {
		let row = GraphRow {
			lanes: vec![
				None,
				Some((ConnectionType::Vertical, 1.into())),
			],
			..GraphRow::default()
		};
		let spans = CommitList::graph_row_spans(
			&row,
			4,
			&std::collections::HashSet::new(),
		);

		assert_eq!(spans.len(), 4);
		assert_eq!(spans[0].content, SYM_SPACE);
		assert_eq!(spans[1].content, SYM_SPACE);
		assert_eq!(spans[2].content, SYM_VERTICAL);
		assert_eq!(spans[3].content, SYM_SPACE);
	}
}
