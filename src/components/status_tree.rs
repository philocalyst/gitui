use super::{
	utils::{
		filetree::{FileTreeItem, FileTreeItemKind},
		statustree::{MoveSelection, StatusTree},
	},
	CommandBlocking, DrawableComponent,
};
use crate::{
	app::Environment,
	components::{CommandInfo, Component, EventState},
	keys::{key_match, SharedKeyConfig},
	popups::{BlameFileOpen, FileRevOpen},
	queue::{InternalEvent, NeedsUpdate, Queue, StackablePopupOpen},
	strings::{self, order},
	ui::{self, style::SharedTheme},
};
use anyhow::Result;
use asyncgit::{hash, sync::CommitId, StatusItem, StatusItemType};
use crossterm::event::{
	Event, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::{layout::Rect, style::Modifier, text::Span, Frame};
use std::{
	borrow::Cow,
	cell::{Cell, RefCell},
	collections::BTreeSet,
	path::Path,
};
use unicode_width::UnicodeWidthStr;

//TODO: use new `filetreelist` crate

/// Renders the working-tree status as a tree.
#[allow(clippy::struct_excessive_bools)]
pub struct StatusTreeComponent {
	title: String,
	tree: StatusTree,
	pending: bool,
	current_hash: u64,
	focused: bool,
	show_selection: bool,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	scroll_top: Cell<usize>,
	visible: bool,
	revision: Option<CommitId>,
	selected_paths: BTreeSet<String>,
	selection_anchor: Option<String>,
	selection_mode: bool,
	selection_extending: bool,
	selection_base: BTreeSet<String>,
	selection_range: BTreeSet<String>,
	selection_excluded: BTreeSet<String>,
	last_draw_area: Cell<Rect>,
	last_draw_rows: RefCell<Vec<String>>,
	#[cfg(test)]
	formatted_rows: Cell<usize>,
	pointer_dragging: bool,
	pointer_anchor: Option<String>,
}

impl StatusTreeComponent {
	///
	pub fn new(env: &Environment, title: &str, focus: bool) -> Self {
		Self {
			title: title.to_string(),
			tree: StatusTree::default(),
			current_hash: 0,
			focused: focus,
			show_selection: focus,
			queue: env.queue.clone(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			scroll_top: Cell::new(0),
			pending: true,
			visible: false,
			revision: None,
			selected_paths: BTreeSet::new(),
			selection_anchor: None,
			selection_mode: false,
			selection_extending: false,
			selection_base: BTreeSet::new(),
			selection_range: BTreeSet::new(),
			selection_excluded: BTreeSet::new(),
			last_draw_area: Cell::new(Rect::default()),
			last_draw_rows: RefCell::new(Vec::new()),
			#[cfg(test)]
			formatted_rows: Cell::new(0),
			pointer_dragging: false,
			pointer_anchor: None,
		}
	}

	pub fn set_commit(&mut self, revision: Option<CommitId>) {
		if self.revision != revision {
			self.clear_selection();
			self.selection_mode = false;
		}
		self.revision = revision;
	}

	/// Keep the loading view visible while an asynchronous status scan is
	/// reporting partial results. `update` clears this automatically for callers
	/// that don't manage a longer-lived async operation.
	pub const fn set_pending(&mut self, pending: bool) {
		self.pending = pending;
	}

	/// Whether status rows may still be partial results from an async scan.
	pub const fn is_pending(&self) -> bool {
		self.pending
	}

	///
	pub fn update(&mut self, list: &[StatusItem]) -> Result<()> {
		self.pending = false;

		let new_hash = hash(list);
		if self.current_hash != new_hash {
			self.tree.update(list)?;
			self.current_hash = new_hash;
			let paths = list
				.iter()
				.map(|item| item.path.as_str())
				.collect::<BTreeSet<_>>();
			self.selected_paths
				.retain(|path| paths.contains(path.as_str()));
			self.selection_base
				.retain(|path| paths.contains(path.as_str()));
			self.selection_range
				.retain(|path| paths.contains(path.as_str()));
			self.selection_excluded
				.retain(|path| paths.contains(path.as_str()));
			if self.selection_anchor.as_ref().is_some_and(|path| {
				!self
					.tree
					.tree
					.items()
					.iter()
					.any(|item| item.info.full_path == *path)
			}) {
				// Do not silently choose a new anchor when a refresh drops the old
				// one. Preserve the current marks and end the range operation.
				self.stop_extending();
			}
		}

		Ok(())
	}

	/// The exact file paths selected by the modal selector.
	pub fn selected_paths(&self) -> impl Iterator<Item = &String> {
		self.selected_paths.iter()
	}

	/// Return the exact status entries covered by the current cursor row.
	/// Folder rows include all descendants, including entries hidden by a
	/// collapsed folder. Paths are compared as path components so a folder
	/// named `x` never captures a sibling such as `x-file`.
	pub fn selection_file_items(&self) -> Vec<StatusItem> {
		let Some(selected) = self.tree.selected_item() else {
			return Vec::new();
		};
		match selected.kind {
			FileTreeItemKind::File(file) => vec![file],
			FileTreeItemKind::Path(_) => {
				let folder = Path::new(&selected.info.full_path);
				self.tree
					.tree
					.items()
					.iter()
					.filter_map(|item| match &item.kind {
						FileTreeItemKind::File(file)
							if Path::new(&file.path)
								.starts_with(folder) =>
						{
							Some(file.clone())
						}
						_ => None,
					})
					.collect()
			}
		}
	}

	pub fn status_for_path(
		&self,
		path: &str,
	) -> Option<StatusItemType> {
		self.tree.tree.items().iter().find_map(|item| {
			match &item.kind {
				FileTreeItemKind::File(file) if file.path == path => {
					Some(file.status)
				}
				_ => None,
			}
		})
	}

	/// Whether modal selection currently contains any files.
	pub fn has_selection(&self) -> bool {
		!self.selected_paths.is_empty()
	}

	/// Return the current modal selection status for command bars and titles.
	pub fn selection_status(
		&self,
	) -> Option<crate::components::SelectionStatus> {
		if !self.focused
			|| !self.visible
			|| self.selected_paths.is_empty()
		{
			return None;
		}
		Some(crate::components::SelectionStatus {
			extending: self.selection_extending,
			count: self.selected_paths.len(),
			unit: "files",
		})
	}

	fn file_paths_for_index(&self, index: usize) -> Vec<String> {
		let Some(item) = self.tree.tree.items().get(index) else {
			return vec![];
		};
		match &item.kind {
			FileTreeItemKind::File(file) => vec![file.path.clone()],
			FileTreeItemKind::Path(_) => {
				let prefix = format!("{}/", item.info.full_path);
				self.tree
					.tree
					.items()
					.iter()
					.filter_map(|child| match &child.kind {
						FileTreeItemKind::File(file)
							if file.path.starts_with(&prefix) =>
						{
							Some(file.path.clone())
						}
						_ => None,
					})
					.collect()
			}
		}
	}

	fn toggle_current(&mut self) {
		let Some(index) = self.tree.selection_index() else {
			return;
		};
		let paths = self.file_paths_for_index(index);
		let remove = paths
			.iter()
			.all(|path| self.selected_paths.contains(path));
		for path in paths {
			if remove {
				self.selected_paths.remove(&path);
				self.selection_base.remove(&path);
				if self.selection_range.contains(&path) {
					self.selection_excluded.insert(path);
				}
			} else {
				self.selected_paths.insert(path.clone());
				self.selection_base.insert(path.clone());
				self.selection_excluded.remove(&path);
			}
		}
	}

	fn extend_selection(&mut self) {
		let Some(current) = self.tree.selection_index() else {
			return;
		};
		let anchor = self.selection_anchor.clone().or_else(|| {
			self.tree
				.tree
				.items()
				.get(current)
				.map(|item| item.info.full_path.clone())
		});
		let Some(anchor) = anchor else {
			return;
		};
		self.selection_anchor = Some(anchor.clone());
		let Some(anchor_index) = self
			.tree
			.tree
			.items()
			.iter()
			.position(|item| item.info.full_path == anchor)
		else {
			return;
		};
		let (start, end) = if anchor_index <= current {
			(anchor_index, current)
		} else {
			(current, anchor_index)
		};
		self.selection_range.clear();
		let visible_indices = self
			.tree
			.visible_selection_indices()
			.filter(|index| *index >= start && *index <= end)
			.collect::<Vec<_>>();
		for index in visible_indices {
			self.selection_range
				.extend(self.file_paths_for_index(index));
		}
		self.rebuild_selection();
	}

	fn select_all_files(&mut self) {
		self.selected_paths = self
			.tree
			.tree
			.items()
			.iter()
			.filter_map(|item| match &item.kind {
				FileTreeItemKind::File(file) => {
					Some(file.path.clone())
				}
				FileTreeItemKind::Path(_) => None,
			})
			.collect();
		self.selection_base.clone_from(&self.selected_paths);
		self.selection_range.clear();
		self.selection_excluded.clear();
		self.selection_anchor = None;
		self.selection_extending = false;
	}

	fn clear_selection(&mut self) {
		self.selected_paths.clear();
		self.selection_base.clear();
		self.selection_range.clear();
		self.selection_excluded.clear();
		self.selection_anchor = None;
		self.selection_extending = false;
	}

	fn rebuild_selection(&mut self) {
		self.selected_paths.clone_from(&self.selection_base);
		self.selected_paths
			.extend(self.selection_range.iter().cloned());
		for path in &self.selection_excluded {
			self.selected_paths.remove(path);
		}
	}

	fn stop_extending(&mut self) {
		self.selection_extending = false;
		self.selection_anchor = None;
		self.selection_base.clone_from(&self.selected_paths);
		self.selection_range.clear();
		self.selection_excluded.clear();
	}

	///
	pub fn selection(&self) -> Option<FileTreeItem> {
		self.tree.selected_item()
	}

	///
	pub fn selection_file(&self) -> Option<StatusItem> {
		self.tree.selected_item().and_then(|f| {
			if let FileTreeItemKind::File(f) = f.kind {
				Some(f)
			} else {
				None
			}
		})
	}

	///
	pub const fn show_selection(&mut self, show: bool) {
		self.show_selection = show;
	}

	/// returns true if list is empty
	pub const fn is_empty(&self) -> bool {
		self.tree.is_empty()
	}

	///
	pub const fn file_count(&self) -> usize {
		self.tree.tree.file_count()
	}

	///
	pub fn set_title(&mut self, title: String) {
		self.title = title;
	}

	///
	pub fn clear(&mut self) -> Result<()> {
		self.current_hash = 0;
		self.pending = true;
		self.clear_selection();
		self.selection_mode = false;
		self.tree.update(&[])
	}

	///
	pub fn is_file_selected(&self) -> bool {
		self.tree.selected_item().is_some_and(|item| {
			match item.kind {
				FileTreeItemKind::File(_) => true,
				FileTreeItemKind::Path(..) => false,
			}
		})
	}

	fn move_selection(&mut self, dir: MoveSelection) -> bool {
		let changed = self.tree.move_selection(dir);

		if changed {
			self.queue.push(InternalEvent::Update(NeedsUpdate::DIFF));
		}

		changed
	}

	const fn item_status_char(item_type: StatusItemType) -> char {
		match item_type {
			StatusItemType::Modified => 'M',
			StatusItemType::New => '+',
			StatusItemType::Deleted => '-',
			StatusItemType::Renamed => 'R',
			StatusItemType::Typechange => ' ',
			StatusItemType::Conflicted => '!',
		}
	}

	fn file_name_for_display(status_item: &StatusItem) -> String {
		let file = Path::new(&status_item.path)
			.file_name()
			.and_then(std::ffi::OsStr::to_str)
			.expect("invalid path.");
		escape_display_label(file)
	}

	#[allow(
		clippy::too_many_arguments,
		clippy::fn_params_excessive_bools
	)] // Rendering receives independent visual state from the tree.
	fn item_to_text<'b>(
		string: &str,
		indent: usize,
		visible: bool,
		file_item_kind: &FileTreeItemKind,
		width: u16,
		selected: bool,
		marked: bool,
		modal: bool,
		theme: &'b SharedTheme,
	) -> Option<Span<'b>> {
		let indent_str = if indent == 0 {
			String::new()
		} else {
			format!("{:w$}", " ", w = indent * 2)
		};

		if !visible {
			return None;
		}

		match file_item_kind {
			FileTreeItemKind::File(status_item) => {
				let status_char =
					Self::item_status_char(status_item.status);
				let file = Self::file_name_for_display(status_item);

				let txt = if !modal && selected {
					format!(
						"{} {}{:w$}",
						status_char,
						indent_str,
						file,
						w = width as usize
					)
				} else if !modal {
					format!("{status_char} {indent_str}{file}")
				} else {
					let cursor = if selected { '›' } else { ' ' };
					let row_width =
						usize::from(width.saturating_sub(2));
					let prefix = format!(
						"{cursor} {status_char} {indent_str}"
					);
					let padding = row_width
						.saturating_sub(UnicodeWidthStr::width(
							prefix.as_str(),
						))
						.saturating_sub(UnicodeWidthStr::width(
							file.as_str(),
						));
					format!("{prefix}{file}{}", " ".repeat(padding))
				};
				let style = if modal {
					let style =
						theme.item(status_item.status, marked);
					if selected {
						theme
							.selected(style, true)
							.add_modifier(Modifier::UNDERLINED)
					} else {
						style
					}
				} else {
					theme.item(status_item.status, selected)
				};
				Some(Span::styled(Cow::from(txt), style))
			}

			FileTreeItemKind::Path(path_collapsed) => {
				let collapse_char =
					if path_collapsed.0 { '▸' } else { '▾' };
				let string = escape_display_label(string);

				let txt = if !modal && selected {
					format!(
						"  {}{}{:w$}",
						indent_str,
						collapse_char,
						string,
						w = width as usize
					)
				} else if !modal {
					format!("  {indent_str}{collapse_char}{string}")
				} else {
					let cursor = if selected { '›' } else { ' ' };
					let row_width =
						usize::from(width.saturating_sub(2));
					let prefix = format!(
						"{cursor} {indent_str}{collapse_char}"
					);
					let padding = row_width
						.saturating_sub(UnicodeWidthStr::width(
							prefix.as_str(),
						))
						.saturating_sub(UnicodeWidthStr::width(
							string.as_str(),
						));
					format!("{prefix}{string}{}", " ".repeat(padding))
				};
				let style = if modal {
					let style = theme.text(true, marked);
					if selected {
						theme
							.selected(style, true)
							.add_modifier(Modifier::UNDERLINED)
					} else {
						style
					}
				} else {
					theme.text(true, selected)
				};

				Some(Span::styled(Cow::from(txt), style))
			}
		}
	}

	/// Returns a `Vec<TextDrawInfo>` which is used to draw the `FileTreeComponent` correctly,
	/// allowing folders to be folded up if they are alone in their directory
	fn build_vec_text_draw_info_for_drawing(
		&self,
	) -> (Vec<TextDrawInfo<'_>>, usize, usize) {
		let mut should_skip_over: usize = 0;
		let mut selection_offset: usize = 0;
		let mut selection_offset_visible: usize = 0;
		let mut vec_draw_text_info: Vec<TextDrawInfo> = vec![];
		let tree_items = self.tree.tree.items();

		for (index, item) in tree_items.iter().enumerate() {
			if should_skip_over > 0 {
				should_skip_over -= 1;
				continue;
			}

			let index_above_select =
				index < self.tree.selection.unwrap_or(0);

			if !item.info.visible && index_above_select {
				selection_offset_visible += 1;
			}

			vec_draw_text_info.push(TextDrawInfo {
				index,
				name: Cow::Borrowed(&item.info.path),
				indent: item.info.indent,
				visible: item.info.visible,
				item_kind: &item.kind,
			});

			let mut idx_temp = index;

			while idx_temp < tree_items.len().saturating_sub(2)
				&& tree_items[idx_temp].info.indent
					< tree_items[idx_temp + 1].info.indent
			{
				// fold up the folder/file
				idx_temp += 1;
				should_skip_over += 1;

				// don't fold files up
				if let FileTreeItemKind::File(_) =
					&tree_items[idx_temp].kind
				{
					should_skip_over -= 1;
					break;
				}
				// don't fold up if more than one folder in folder
				else if self
					.tree
					.tree
					.multiple_items_at_path(idx_temp)
				{
					should_skip_over -= 1;
					break;
				}

				// There is only one item at this level (i.e only one folder in the folder),
				// so do fold up

				let vec_draw_text_info_len = vec_draw_text_info.len();
				let draw_info = &mut vec_draw_text_info
					[vec_draw_text_info_len - 1];
				draw_info.name = Cow::Owned(format!(
					"{}/{}",
					draw_info.name, tree_items[idx_temp].info.path
				));
				if index_above_select {
					selection_offset += 1;
				}
			}
		}
		(
			vec_draw_text_info,
			selection_offset,
			selection_offset_visible,
		)
	}

	// Copy the real path of selected file to clickboard
	fn copy_file_path(&self) {
		if let Some(item) = self.selection() {
			if crate::clipboard::copy_string(&item.info.full_path)
				.is_err()
			{
				self.queue.push(InternalEvent::ShowErrorMsg(
					strings::POPUP_FAIL_COPY.to_string(),
				));
			}
		}
	}

	fn open_history(&mut self) {
		match self.selection_file() {
			Some(status_item)
				if !matches!(
					status_item.status,
					StatusItemType::New
				) =>
			{
				self.hide();
				self.queue.push(InternalEvent::OpenPopup(
					StackablePopupOpen::FileRevlog(FileRevOpen::new(
						status_item.path,
					)),
				));
			}
			_ => {}
		}
	}

	fn pointer_row(&self, column: u16, row: u16) -> Option<String> {
		let area = self.last_draw_area.get();
		let content_top = area.y.saturating_add(1);
		let content_bottom =
			area.y.saturating_add(area.height.saturating_sub(1));
		if column < area.x.saturating_add(1)
			|| column
				>= area.x.saturating_add(area.width.saturating_sub(1))
			|| row < content_top
			|| row >= content_bottom
		{
			return None;
		}
		let row_offset = usize::from(row.saturating_sub(content_top));
		self.last_draw_rows.borrow().get(row_offset).cloned()
	}

	fn select_pointer_path(&mut self, path: &str) -> Option<usize> {
		let index = self
			.tree
			.tree
			.items()
			.iter()
			.position(|item| item.info.full_path == path)?;
		if !self.tree.available_selections.contains(&index)
			|| !self.tree.tree[index].info.visible
		{
			return None;
		}
		let changed = self.tree.selection != Some(index);
		self.tree.selection = Some(index);
		if changed {
			self.queue.push(InternalEvent::Update(NeedsUpdate::DIFF));
		}
		Some(index)
	}

	fn begin_pointer_range(&mut self, anchor: String) {
		self.selection_mode = true;
		self.selection_base.clone_from(&self.selected_paths);
		self.selection_range.clear();
		self.selection_excluded.clear();
		self.selection_anchor = Some(anchor);
		self.selection_extending = true;
		self.extend_selection();
	}

	fn cancel_pointer_range_to_explicit_marks(&mut self) {
		if self.selection_extending {
			self.selection_anchor = None;
			self.selection_extending = false;
			self.selection_range.clear();
			self.selection_excluded.clear();
			self.selected_paths.clone_from(&self.selection_base);
		}
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

	fn draw_loading(&self, frame: &mut Frame, area: Rect) {
		let items = vec![Span::styled(
			Cow::from(strings::loading_text(&self.key_config)),
			self.theme.text(false, false),
		)];
		ui::draw_list(
			frame,
			area,
			self.title.as_str(),
			items.into_iter(),
			self.focused,
			&self.theme,
		);
	}

	fn draw_rows(&self, frame: &mut Frame, area: Rect) {
		let (
			vec_draw_text_info,
			selection_offset,
			selection_offset_visible,
		) = self.build_vec_text_draw_info_for_drawing();
		let select = self
			.tree
			.selection
			.map(|idx| idx.saturating_sub(selection_offset))
			.unwrap_or_default();
		let tree_height = area.height.saturating_sub(2) as usize;
		self.tree.window_height.set(Some(tree_height));
		self.scroll_top.set(ui::calc_scroll_top(
			self.scroll_top.get(),
			tree_height,
			select.saturating_sub(selection_offset_visible),
		));

		let visible_rows = vec_draw_text_info
			.iter()
			.filter(|draw_text_info| draw_text_info.visible)
			.collect::<Vec<_>>();
		let scroll_top = self.scroll_top.get();
		*self.last_draw_rows.borrow_mut() = visible_rows
			.iter()
			.skip(scroll_top)
			.take(tree_height)
			.map(|draw_text_info| {
				self.tree.tree[draw_text_info.index]
					.info
					.full_path
					.clone()
			})
			.collect();
		#[cfg(test)]
		self.formatted_rows.set(0);
		let items = visible_rows
			.iter()
			.skip(scroll_top)
			.take(tree_height)
			.filter_map(|draw_text_info| {
				#[cfg(test)]
				self.formatted_rows
					.set(self.formatted_rows.get().saturating_add(1));
				let item_index = draw_text_info.index;
				let selected = self.show_selection
					&& self.tree.selection == Some(item_index);
				let marked = match draw_text_info.item_kind {
					FileTreeItemKind::File(file) => {
						self.selected_paths.contains(&file.path)
					}
					FileTreeItemKind::Path(_) => self
						.file_paths_for_index(item_index)
						.iter()
						.any(|path| {
							self.selected_paths.contains(path)
						}),
				};
				Self::item_to_text(
					draw_text_info.name.as_ref(),
					draw_text_info.indent as usize,
					draw_text_info.visible,
					draw_text_info.item_kind,
					area.width,
					selected,
					marked,
					self.key_config.is_modal(),
					&self.theme,
				)
			});

		let mut title = if self.key_config.is_modal()
			&& !self.selected_paths.is_empty()
		{
			format!(
				"{} [SELECT {} files]",
				self.title,
				self.selected_paths.len()
			)
		} else {
			self.title.clone()
		};
		if self.pending {
			title.push_str(" [LOADING]");
		}
		ui::draw_list(
			frame,
			area,
			&title,
			items,
			self.focused,
			&self.theme,
		);
	}
}

fn escape_display_label(label: &str) -> String {
	label.chars().flat_map(char::escape_debug).collect()
}

/// Used for drawing the `FileTreeComponent`
struct TextDrawInfo<'a> {
	index: usize,
	name: Cow<'a, str>,
	indent: u8,
	visible: bool,
	item_kind: &'a FileTreeItemKind,
}

impl DrawableComponent for StatusTreeComponent {
	fn draw(&self, f: &mut Frame, r: Rect) -> Result<()> {
		self.last_draw_area.set(r);
		self.last_draw_rows.borrow_mut().clear();
		if !self.is_visible() {
			return Ok(());
		}

		if self.pending && self.tree.is_empty() {
			self.draw_loading(f, r);
		} else {
			self.draw_rows(f, r);
		}

		Ok(())
	}
}

impl Component for StatusTreeComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		let available = self.focused || force_all;
		let selection = self.selection_file();
		let selected_is_file = selection.is_some();
		let tracked = selection.is_some_and(|s| {
			!matches!(s.status, StatusItemType::New)
		});

		out.push(
			CommandInfo::new(
				strings::commands::navigate_tree(&self.key_config),
				!self.is_empty(),
				available,
			)
			.order(order::NAV),
		);

		out.push(
			CommandInfo::new(
				strings::commands::blame_file(&self.key_config),
				selected_is_file && tracked,
				available,
			)
			.order(order::RARE_ACTION),
		);

		out.push(
			CommandInfo::new(
				strings::commands::open_file_history(
					&self.key_config,
				),
				selected_is_file && tracked,
				available,
			)
			.order(order::RARE_ACTION),
		);

		out.push(
			CommandInfo::new(
				strings::commands::edit_item(&self.key_config),
				selected_is_file,
				available,
			)
			.order(order::RARE_ACTION),
		);

		out.push(
			CommandInfo::new(
				strings::commands::copy_path(&self.key_config),
				selected_is_file,
				available,
			)
			.order(order::RARE_ACTION),
		);

		CommandBlocking::PassingOn
	}

	#[allow(clippy::too_many_lines)] // Selection transitions remain adjacent to their existing tree navigation.
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if let Event::Mouse(mouse) = ev {
			match mouse.kind {
				MouseEventKind::Down(MouseButton::Left) => {
					let Some(path) =
						self.pointer_row(mouse.column, mouse.row)
					else {
						return Ok(EventState::NotConsumed);
					};
					let previous_anchor = self
						.tree
						.selected_item()
						.map(|item| item.info.full_path);
					let shift =
						mouse.modifiers.contains(KeyModifiers::SHIFT)
							&& self.key_config.is_modal();
					if !shift {
						self.cancel_pointer_range_to_explicit_marks();
					}
					let Some(index) = self.select_pointer_path(&path)
					else {
						return Ok(EventState::NotConsumed);
					};
					let item_path =
						self.tree.tree[index].info.full_path.clone();
					let item_indent =
						self.tree.tree[index].info.indent;
					self.pointer_anchor = Some(item_path);
					self.pointer_dragging = true;

					if shift {
						if let Some(anchor) = previous_anchor
							.or_else(|| self.pointer_anchor.clone())
						{
							self.begin_pointer_range(anchor);
						}
					}

					// Clicking the folder glyph is a direct, predictable fold toggle.
					let glyph_column = self
						.last_draw_area
						.get()
						.x
						.saturating_add(3)
						.saturating_add(
							u16::from(item_indent).saturating_mul(2),
						);
					if mouse.column == glyph_column {
						let collapsed = match &self.tree.tree[index].kind {
							FileTreeItemKind::Path(
								crate::components::utils::filetree::PathCollapsed(
									collapsed,
								),
							) => Some(*collapsed),
							FileTreeItemKind::File(_) => None,
						};
						if let Some(collapsed) = collapsed {
							self.move_selection(if collapsed {
								MoveSelection::Right
							} else {
								MoveSelection::Left
							});
						}
					}
					return Ok(EventState::Consumed);
				}
				MouseEventKind::Drag(MouseButton::Left)
					if self.pointer_dragging =>
				{
					if let Some(path) =
						self.pointer_row(mouse.column, mouse.row)
					{
						if self.select_pointer_path(&path).is_none() {
							return Ok(EventState::Consumed);
						}
						if self.key_config.is_modal() {
							if let Some(anchor) =
								self.pointer_anchor.clone()
							{
								if !self.selection_extending {
									self.begin_pointer_range(anchor);
								}
								self.extend_selection();
							}
						}
					}
					return Ok(EventState::Consumed);
				}
				MouseEventKind::Up(MouseButton::Left)
					if self.pointer_dragging =>
				{
					self.pointer_dragging = false;
					self.pointer_anchor = None;
					if self.selection_extending {
						self.stop_extending();
					}
					return Ok(EventState::Consumed);
				}
				MouseEventKind::ScrollUp
				| MouseEventKind::ScrollDown => {
					if !self.pointer_inside(mouse.column, mouse.row) {
						return Ok(EventState::NotConsumed);
					}
					let dir = if matches!(
						mouse.kind,
						MouseEventKind::ScrollUp
					) {
						MoveSelection::Up
					} else {
						MoveSelection::Down
					};
					if self.move_selection(dir)
						&& self.selection_extending
					{
						self.extend_selection();
					}
					return Ok(EventState::Consumed);
				}
				_ => {}
			}
		}

		if self.focused {
			if let Event::Key(e) = ev {
				return if self.key_config.is_modal()
					&& key_match(e, self.key_config.keys.select_clear)
					&& (!self.selected_paths.is_empty()
						|| self.selection_extending)
				{
					self.clear_selection();
					self.selection_mode = false;
					Ok(EventState::Consumed)
				} else if self.key_config.is_modal()
					&& key_match(e, self.key_config.keys.select_mode)
				{
					if self.selection_extending {
						self.stop_extending();
						self.selection_mode = false;
					} else {
						self.selection_mode = true;
						self.selection_base
							.clone_from(&self.selected_paths);
						self.selection_range.clear();
						self.selection_excluded.clear();
						self.selection_anchor = self
							.tree
							.selection_index()
							.and_then(|index| {
								self.tree.tree.items().get(index).map(
									|item| {
										item.info.full_path.clone()
									},
								)
							});
						self.selection_extending =
							self.selection_anchor.is_some();
						self.extend_selection();
					}
					Ok(EventState::Consumed)
				} else if self.key_config.is_modal()
					&& key_match(e, self.key_config.keys.select_all)
				{
					self.selection_mode = true;
					self.select_all_files();
					Ok(EventState::Consumed)
				} else if self.key_config.is_modal()
					&& key_match(e, self.key_config.keys.select_item)
				{
					self.selection_mode = true;
					self.toggle_current();
					Ok(EventState::Consumed)
				} else if self.selection_extending
					&& (key_match(e, self.key_config.keys.move_down)
						|| key_match(e, self.key_config.keys.move_up)
						|| key_match(e, self.key_config.keys.home)
						|| key_match(e, self.key_config.keys.end)
						|| key_match(e, self.key_config.keys.page_up)
						|| key_match(
							e,
							self.key_config.keys.page_down,
						)) {
					let moved = if key_match(
						e,
						self.key_config.keys.move_down,
					) {
						self.move_selection(MoveSelection::Down)
					} else if key_match(
						e,
						self.key_config.keys.move_up,
					) {
						self.move_selection(MoveSelection::Up)
					} else if key_match(e, self.key_config.keys.home)
					{
						self.move_selection(MoveSelection::Home)
					} else if key_match(e, self.key_config.keys.end) {
						self.move_selection(MoveSelection::End)
					} else if key_match(
						e,
						self.key_config.keys.page_up,
					) {
						self.move_selection(MoveSelection::PageUp)
					} else {
						self.move_selection(MoveSelection::PageDown)
					};
					if moved {
						self.extend_selection();
					}
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.blame) {
					match self.selection_file() {
						Some(status_item)
							if !matches!(
								status_item.status,
								StatusItemType::New
							) =>
						{
							self.hide();
							self.queue.push(
								InternalEvent::OpenPopup(
									StackablePopupOpen::BlameFile(
										BlameFileOpen {
											file_path: status_item
												.path,
											commit_id: self.revision,
											selection: None,
										},
									),
								),
							);
						}
						_ => {}
					}
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.file_history,
				) {
					self.open_history();
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.edit_file)
				{
					if let Some(status_item) = self.selection_file() {
						self.queue.push(
							InternalEvent::OpenExternalEditor(Some(
								status_item.path,
							)),
						);
					}
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.copy) {
					self.copy_file_path();
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.move_down)
				{
					Ok(self
						.move_selection(MoveSelection::Down)
						.into())
				} else if key_match(e, self.key_config.keys.move_up) {
					Ok(self.move_selection(MoveSelection::Up).into())
				} else if key_match(e, self.key_config.keys.home)
					|| key_match(e, self.key_config.keys.shift_up)
				{
					Ok(self
						.move_selection(MoveSelection::Home)
						.into())
				} else if key_match(e, self.key_config.keys.end)
					|| key_match(e, self.key_config.keys.shift_down)
				{
					Ok(self.move_selection(MoveSelection::End).into())
				} else if key_match(e, self.key_config.keys.page_up) {
					Ok(self
						.move_selection(MoveSelection::PageUp)
						.into())
				} else if key_match(e, self.key_config.keys.page_down)
				{
					Ok(self
						.move_selection(MoveSelection::PageDown)
						.into())
				} else if key_match(e, self.key_config.keys.move_left)
				{
					Ok(self
						.move_selection(MoveSelection::Left)
						.into())
				} else if key_match(
					e,
					self.key_config.keys.move_right,
				) {
					Ok(self
						.move_selection(MoveSelection::Right)
						.into())
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
		self.show_selection(focus);
		if !focus {
			self.stop_extending();
			self.selection_mode = false;
			self.pointer_dragging = false;
			self.pointer_anchor = None;
		}
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn hide(&mut self) {
		self.visible = false;
		self.clear_selection();
		self.selection_mode = false;
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::keys::{KeyConfig, Keymap};
	use crossterm::event::{
		KeyCode, KeyEvent, KeyModifiers, MouseEvent,
	};
	use std::rc::Rc;

	fn key(code: char) -> Event {
		Event::Key(KeyEvent::new(
			KeyCode::Char(code),
			KeyModifiers::NONE,
		))
	}

	fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
		Event::Mouse(MouseEvent {
			kind,
			column,
			row,
			modifiers: KeyModifiers::NONE,
		})
	}

	fn modal_tree(items: &[&str]) -> StatusTreeComponent {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut tree = StatusTreeComponent::new(&env, "test", true);
		tree.show().unwrap();
		tree.update(&string_vec_to_status(items)).unwrap();
		tree
	}

	fn string_vec_to_status(items: &[&str]) -> Vec<StatusItem> {
		items
			.iter()
			.map(|a| StatusItem {
				path: String::from(*a),
				status: StatusItemType::Modified,
			})
			.collect::<Vec<_>>()
	}

	#[test]
	fn test_correct_scroll_position() {
		let items = string_vec_to_status(&[
			"a/b/b1", //
			"a/b/b2", //
			"a/c/c1", //
		]);

		//0 a/
		//1   b/
		//2     b1
		//3     b2
		//4  c/
		//5    c1

		// Set up test terminal
		let test_backend =
			ratatui::backend::TestBackend::new(100, 100);
		let mut terminal = ratatui::Terminal::new(test_backend)
			.expect("Unable to set up terminal");
		let mut frame = terminal.get_frame();

		// set up file tree
		let mut ftc = StatusTreeComponent::new(
			&Environment::test_env(),
			"title",
			true,
		);
		ftc.update(&items)
			.expect("Updating FileTreeComponent failed");

		ftc.move_selection(MoveSelection::Down); // Move to b/
		ftc.move_selection(MoveSelection::Left); // Fold b/
		ftc.move_selection(MoveSelection::Down); // Move to c/

		ftc.draw(&mut frame, Rect::new(0, 0, 10, 5))
			.expect("Draw failed");

		assert_eq!(ftc.scroll_top.get(), 0); // should still be at top
	}

	#[test]
	fn test_correct_foldup_and_not_visible_scroll_position() {
		let items = string_vec_to_status(&[
			"a/b/b1", //
			"c/d1",   //
			"c/d2",   //
		]);

		//0 a/b/
		//2     b1
		//3 c/
		//4   d1
		//5   d2

		// Set up test terminal
		let test_backend =
			ratatui::backend::TestBackend::new(100, 100);
		let mut terminal = ratatui::Terminal::new(test_backend)
			.expect("Unable to set up terminal");
		let mut frame = terminal.get_frame();

		// set up file tree
		let mut ftc = StatusTreeComponent::new(
			&Environment::test_env(),
			"title",
			true,
		);
		ftc.update(&items)
			.expect("Updating FileTreeComponent failed");

		ftc.move_selection(MoveSelection::Left); // Fold a/b/
		ftc.move_selection(MoveSelection::Down); // Move to c/

		ftc.draw(&mut frame, Rect::new(0, 0, 10, 5))
			.expect("Draw failed");

		assert_eq!(ftc.scroll_top.get(), 0); // should still be at top
	}

	#[test]
	fn modal_range_reverses_and_explicit_x_does_not_start_a_range() {
		let mut tree = modal_tree(&["a", "b", "c", "d"]);
		for event in [key('v'), key('j'), key('j')] {
			tree.event(&event).unwrap();
		}
		assert_eq!(
			tree.selected_paths().cloned().collect::<Vec<_>>(),
			["a", "b", "c"]
		);
		tree.event(&key('k')).unwrap();
		assert_eq!(
			tree.selected_paths().cloned().collect::<Vec<_>>(),
			["a", "b"]
		);

		let mut tree = modal_tree(&["a", "b", "c"]);
		tree.event(&key('x')).unwrap();
		tree.event(&key('j')).unwrap();
		assert_eq!(
			tree.selected_paths().cloned().collect::<Vec<_>>(),
			["a"]
		);
	}

	#[test]
	fn removed_range_anchor_stops_extension_and_focus_reports_marks()
	{
		let mut tree = modal_tree(&["a", "b", "c"]);
		tree.event(&key('v')).unwrap();
		tree.event(&key('j')).unwrap();
		tree.update(&string_vec_to_status(&["b", "c"])).unwrap();
		assert!(!tree.selection_extending);
		let paths =
			tree.selected_paths().cloned().collect::<Vec<_>>();
		tree.event(&key('j')).unwrap();
		assert_eq!(
			tree.selected_paths().cloned().collect::<Vec<_>>(),
			paths
		);

		let mut tree = modal_tree(&["a", "b"]);
		tree.event(&key('x')).unwrap();
		let status = tree.selection_status().unwrap();
		assert_eq!(status.count, 1);
		assert!(!status.extending);
		tree.focus(false);
		assert!(tree.selection_status().is_none());
		tree.focus(true);
		assert_eq!(tree.selection_status().unwrap().count, 1);
		tree.event(&Event::Key(KeyEvent::new(
			KeyCode::Esc,
			KeyModifiers::NONE,
		)))
		.unwrap();
		assert!(tree.selected_paths().next().is_none());
	}

	#[test]
	fn selection_keys_are_inert_in_legacy_and_rows_keep_legacy_spacing(
	) {
		let env = Environment::test_env();
		let mut tree = StatusTreeComponent::new(&env, "test", true);
		tree.show().unwrap();
		tree.update(&string_vec_to_status(&["a"])).unwrap();
		tree.event(&key('v')).unwrap();
		tree.event(&key('x')).unwrap();
		assert!(tree.selected_paths().next().is_none());
		assert!(!tree.selection_mode);

		let theme = Rc::default();
		let span = StatusTreeComponent::item_to_text(
			"a",
			0,
			true,
			&FileTreeItemKind::File(StatusItem {
				path: "a".into(),
				status: StatusItemType::Modified,
			}),
			20,
			false,
			true,
			false,
			&theme,
		)
		.unwrap();
		assert_eq!(span.content, "M a");
	}

	#[test]
	fn mouse_hit_map_tracks_folded_and_scrolled_rows() {
		let mut tree = modal_tree(&[
			"a/b/one",
			"a/b/two",
			"a/c/three",
			"z/last",
		]);
		let backend = ratatui::backend::TestBackend::new(40, 7);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		let area = Rect::new(3, 2, 24, 6);
		terminal
			.draw(|frame| tree.draw(frame, area).unwrap())
			.unwrap();
		assert!(!tree.last_draw_rows.borrow().is_empty());

		// Border clicks are ignored; the first content row selects the exact
		// underlying item represented by the folded drawing row.
		assert_eq!(
			tree.event(&mouse(
				MouseEventKind::Down(MouseButton::Left),
				3,
				3
			))
			.unwrap(),
			EventState::NotConsumed
		);
		let expected = tree.last_draw_rows.borrow()[0].clone();
		tree.event(&mouse(
			MouseEventKind::Down(MouseButton::Left),
			4,
			3,
		))
		.unwrap();
		assert_eq!(
			tree.tree.selected_item().unwrap().info.full_path,
			expected
		);
		let before_wheel = tree.tree.selection;
		tree.event(&mouse(MouseEventKind::ScrollDown, 4, 3))
			.unwrap();
		assert_ne!(tree.tree.selection, before_wheel);

		// A short viewport stores only rows that were actually drawn. The
		// first visible row remains a valid hit after keyboard scrolling.
		for _ in 0..4 {
			tree.move_selection(MoveSelection::End);
			tree.move_selection(MoveSelection::Up);
		}
		terminal
			.draw(|frame| tree.draw(frame, area).unwrap())
			.unwrap();
		let expected = tree.last_draw_rows.borrow()[0].clone();
		tree.event(&mouse(
			MouseEventKind::Down(MouseButton::Left),
			4,
			3,
		))
		.unwrap();
		assert_eq!(
			tree.tree.selected_item().unwrap().info.full_path,
			expected
		);
	}

	#[test]
	fn large_scrolled_tree_formats_and_maps_only_the_drawn_rows() {
		const FILE_COUNT: usize = 16_384;
		let paths = (0..FILE_COUNT)
			.map(|index| format!("file-{index:05}.txt"))
			.collect::<Vec<_>>();
		let path_refs =
			paths.iter().map(String::as_str).collect::<Vec<_>>();
		let mut tree = modal_tree(&path_refs);
		tree.move_selection(MoveSelection::End);
		let backend = ratatui::backend::TestBackend::new(40, 8);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		let area = Rect::new(0, 0, 40, 8);
		terminal
			.draw(|frame| tree.draw(frame, area).unwrap())
			.unwrap();

		let viewport_rows =
			usize::from(area.height.saturating_sub(2));
		assert!(tree.scroll_top.get() > 0);
		assert_eq!(tree.formatted_rows.get(), viewport_rows);
		assert_eq!(tree.last_draw_rows.borrow().len(), viewport_rows);
		assert_eq!(
			tree.last_draw_rows.borrow().last().map(String::as_str),
			paths.last().map(String::as_str)
		);

		// Clicking the final drawn row must select the same raw file identity
		// that was used to render that row at the end of the large tree.
		tree.event(&mouse(
			MouseEventKind::Down(MouseButton::Left),
			2,
			area.height - 2,
		))
		.unwrap();
		assert_eq!(
			tree.tree.selected_item().unwrap().info.full_path,
			*paths.last().unwrap()
		);
	}

	#[test]
	fn pending_status_keeps_partial_rows_visible_and_pointer_selectable(
	) {
		let mut tree = modal_tree(&["alpha.txt", "beta.txt"]);
		tree.set_pending(true);
		let backend = ratatui::backend::TestBackend::new(40, 8);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		let area = Rect::new(0, 0, 30, 6);
		terminal
			.draw(|frame| tree.draw(frame, area).unwrap())
			.unwrap();

		let text = terminal
			.backend()
			.buffer()
			.content
			.iter()
			.map(|cell| cell.symbol())
			.collect::<String>();
		assert!(text.contains("alpha.txt"));
		assert!(text.contains("beta.txt"));
		assert!(text.contains("LOADING"));
		assert_eq!(
			tree.last_draw_rows.borrow().as_slice(),
			["alpha.txt", "beta.txt"]
		);

		assert_eq!(
			tree.event(&mouse(
				MouseEventKind::Down(MouseButton::Left),
				2,
				2,
			))
			.unwrap(),
			EventState::Consumed
		);
		assert_eq!(
			tree.tree.selected_item().unwrap().info.full_path,
			"beta.txt"
		);
	}

	#[test]
	fn control_characters_in_paths_do_not_change_pointer_row_geometry(
	) {
		assert_eq!(
			escape_display_label("wide界\t\\\u{1b}"),
			"wide界\\t\\\\\\u{1b}"
		);
		let mut tree = modal_tree(&["a\nline.txt", "b.txt"]);
		let backend = ratatui::backend::TestBackend::new(40, 8);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		let area = Rect::new(0, 0, 30, 6);
		terminal
			.draw(|frame| tree.draw(frame, area).unwrap())
			.unwrap();

		let buffer = terminal.backend().buffer();
		let row = |y| {
			(0..area.width)
				.map(|x| buffer[(x, y)].symbol())
				.collect::<String>()
		};
		assert!(row(1).contains("a\\nline.txt"));
		assert!(row(2).contains("b.txt"));
		assert_eq!(
			tree.last_draw_rows.borrow().as_slice(),
			["a\nline.txt", "b.txt"]
		);

		tree.event(&mouse(
			MouseEventKind::Down(MouseButton::Left),
			2,
			2,
		))
		.unwrap();
		assert_eq!(
			tree.tree.selected_item().unwrap().info.full_path,
			"b.txt"
		);
	}

	#[test]
	fn pointer_drag_creates_reversible_highlight_without_checkmarks()
	{
		let mut tree = modal_tree(&["a", "b", "c", "d"]);
		let backend = ratatui::backend::TestBackend::new(40, 8);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		let area = Rect::new(0, 0, 30, 7);
		terminal
			.draw(|frame| tree.draw(frame, area).unwrap())
			.unwrap();
		tree.event(&mouse(
			MouseEventKind::Down(MouseButton::Left),
			2,
			1,
		))
		.unwrap();
		tree.event(&mouse(
			MouseEventKind::Drag(MouseButton::Left),
			2,
			3,
		))
		.unwrap();
		tree.event(&mouse(
			MouseEventKind::Drag(MouseButton::Left),
			2,
			2,
		))
		.unwrap();
		assert_eq!(
			tree.selected_paths().cloned().collect::<Vec<_>>(),
			["a", "b"]
		);
		tree.event(&mouse(
			MouseEventKind::Up(MouseButton::Left),
			2,
			2,
		))
		.unwrap();
		tree.clear_selection();
		assert_eq!(
			tree.selected_paths().cloned().collect::<Vec<_>>(),
			Vec::<String>::new()
		);
		tree.event(&mouse(
			MouseEventKind::Down(MouseButton::Left),
			2,
			3,
		))
		.unwrap();
		tree.event(&mouse(
			MouseEventKind::Drag(MouseButton::Left),
			2,
			1,
		))
		.unwrap();
		tree.event(&mouse(
			MouseEventKind::Drag(MouseButton::Left),
			2,
			2,
		))
		.unwrap();
		tree.event(&mouse(
			MouseEventKind::Up(MouseButton::Left),
			2,
			2,
		))
		.unwrap();
		assert_eq!(
			tree.selected_paths().cloned().collect::<Vec<_>>(),
			["b", "c"]
		);

		let span = StatusTreeComponent::item_to_text(
			"a",
			0,
			true,
			&FileTreeItemKind::File(StatusItem {
				path: "a".into(),
				status: StatusItemType::Modified,
			}),
			30,
			false,
			true,
			true,
			&tree.theme,
		)
		.unwrap();
		assert!(!span.content.contains('✓'));
		assert!(span.style.bg.is_some());
		assert_eq!(UnicodeWidthStr::width(span.content.as_ref()), 28);
	}
}
