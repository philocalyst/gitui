use crate::components::{
	visibility_blocking, CommandBlocking, CommandInfo, CommandText,
	Component, DrawableComponent, EventState, FuzzyFinderTarget,
	VerticalScroll,
};
use crate::{
	app::Environment,
	components::ScrollType,
	keys::{key_match, SharedKeyConfig},
	queue::{
		Action, InternalEvent, NeedsUpdate, Queue, StackablePopupOpen,
	},
	strings, try_or_popup,
	ui::{self, Size},
};
use anyhow::Result;
use asyncgit::{
	sync::{
		self,
		branch::{
			checkout_remote_branch, BranchDetails, LocalBranch,
			RemoteBranch,
		},
		checkout_branch, get_branches_info,
		status::StatusType,
		BranchInfo, BranchType, CommitId, RepoPathRef, RepoState,
	},
	AsyncGitNotification,
};
use crossterm::event::{
	Event, KeyCode, KeyEvent, KeyModifiers, MouseButton,
	MouseEventKind,
};
use ratatui::{
	layout::{
		Alignment, Constraint, Direction, Layout, Margin, Rect,
	},
	text::{Line, Span, Text},
	widgets::{Block, BorderType, Borders, Clear, Paragraph, Tabs},
	Frame,
};
use std::cell::Cell;
use ui::style::SharedTheme;
use unicode_truncate::UnicodeTruncateStr;
use unicode_width::UnicodeWidthStr;

use super::InspectCommitOpen;

fn is_plain_escape(e: &KeyEvent) -> bool {
	e.code == KeyCode::Esc && e.modifiers.is_empty()
}

const fn plain_action_character(e: &KeyEvent) -> bool {
	e.modifiers.is_empty() && matches!(e.code, KeyCode::Char(_))
}

fn sanitize_summary(summary: &str) -> String {
	summary
		.chars()
		.map(|c| if c.is_control() { ' ' } else { c })
		.collect::<String>()
		.split_whitespace()
		.collect::<Vec<_>>()
		.join(" ")
}

///
#[allow(clippy::struct_excessive_bools)] // Modal filter, range, and selection states are independent.
pub struct BranchListPopup {
	repo: RepoPathRef,
	branches: Vec<BranchInfo>,
	/// Indices into `branches` after applying the inline filter.
	filtered: Vec<usize>,
	filter: String,
	action_mode: bool,
	action_message: Option<&'static str>,
	local: bool,
	has_remotes: bool,
	visible: bool,
	selection: u16,
	scroll: VerticalScroll,
	current_height: Cell<u16>,
	list_area: Cell<Rect>,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
}

impl DrawableComponent for BranchListPopup {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if self.is_visible() {
			const PERCENT_SIZE: Size = Size::new(80, 50);
			const MIN_SIZE: Size = Size::new(60, 20);

			let area = ui::centered_rect(
				PERCENT_SIZE.width,
				PERCENT_SIZE.height,
				f.area(),
			);
			let area =
				ui::rect_inside(MIN_SIZE, f.area().into(), area);
			let area = area.intersection(rect);

			f.render_widget(Clear, area);

			f.render_widget(
				Block::default()
					.title(strings::title_branches())
					.border_type(BorderType::Thick)
					.borders(Borders::ALL),
				area,
			);

			let area = area.inner(Margin {
				vertical: 1,
				horizontal: 1,
			});

			let chunks = Layout::default()
				.direction(Direction::Vertical)
				.constraints(
					[
						Constraint::Length(2),
						Constraint::Length(2),
						Constraint::Min(1),
					]
					.as_ref(),
				)
				.split(area);

			self.draw_tabs(f, chunks[0]);
			self.draw_filter(f, chunks[1]);
			self.list_area.set(chunks[2]);
			self.draw_list(f, chunks[2])?;
		}

		Ok(())
	}
}

impl Component for BranchListPopup {
	fn captures_input(&self) -> bool {
		self.visible
			&& self.key_config.is_modal()
			&& !self.action_mode
	}

	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible || force_all {
			if !force_all {
				out.clear();
			}

			self.add_commands_internal(out);
		}
		visibility_blocking(self)
	}

	//TODO: cleanup
	#[allow(clippy::cognitive_complexity)]
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if !self.visible {
			return Ok(EventState::NotConsumed);
		}
		if matches!(ev, Event::Mouse(_)) {
			return self.handle_mouse_event(ev);
		}

		if let Event::Key(e) = ev {
			// Modal mode owns every printable key. This is deliberately before
			// the legacy key map: typing `b`, `q`, `h`, etc. must never invoke a
			// branch command while the picker is being filtered.
			if self.key_config.is_modal() {
				return self.modal_event(e);
			}

			if self.move_event(e)?.is_consumed() {
				return Ok(EventState::Consumed);
			}

			let selection_is_cur_branch =
				self.selection_is_cur_branch();

			if key_match(e, self.key_config.keys.enter) {
				try_or_popup!(
					self,
					"switch branch error:",
					self.switch_to_selected_branch()
				);
			} else if key_match(e, self.key_config.keys.create_branch)
				&& self.local
			{
				self.queue.push(InternalEvent::CreateBranch);
			} else if key_match(e, self.key_config.keys.rename_branch)
				&& self.local
				&& self.valid_selection()
			{
				self.rename_branch();
			} else if key_match(e, self.key_config.keys.delete_branch)
				&& !selection_is_cur_branch
				&& self.valid_selection()
			{
				self.delete_branch();
			} else if key_match(e, self.key_config.keys.merge_branch)
				&& !selection_is_cur_branch
				&& self.valid_selection()
			{
				try_or_popup!(
					self,
					"merge branch error:",
					self.merge_branch()
				);
			} else if key_match(e, self.key_config.keys.rebase_branch)
				&& !selection_is_cur_branch
				&& self.valid_selection()
			{
				self.rebase_branch();
			} else if key_match(e, self.key_config.keys.move_right)
				&& self.valid_selection()
			{
				self.inspect_head_of_branch();
			} else if key_match(
				e,
				self.key_config.keys.compare_commits,
			) && self.valid_selection()
			{
				self.hide();
				if let Some(commit_id) = self.get_selected_commit() {
					self.queue.push(InternalEvent::OpenPopup(
						StackablePopupOpen::CompareCommits(
							InspectCommitOpen::new(commit_id),
						),
					));
				}
			} else if key_match(e, self.key_config.keys.fetch)
				&& self.has_remotes
			{
				self.queue.push(InternalEvent::FetchRemotes);
			} else if key_match(e, self.key_config.keys.view_remotes)
			{
				self.queue.push(InternalEvent::ViewRemotes);
			} else if key_match(e, self.key_config.keys.reset_branch)
			{
				if let Some(commit_id) = self.get_selected_commit() {
					self.queue.push(InternalEvent::OpenResetPopup(
						commit_id,
					));
				}
			} else if key_match(
				e,
				self.key_config.keys.cmd_bar_toggle,
			) {
				//do not consume if its the more key
				return Ok(EventState::NotConsumed);
			} else if key_match(e, self.key_config.keys.branch_find) {
				let branches = self
					.branches
					.iter()
					.map(|b| b.name.clone())
					.collect();
				self.queue.push(InternalEvent::OpenFuzzyFinder(
					branches,
					FuzzyFinderTarget::Branches,
				));
			}
		}

		Ok(EventState::Consumed)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn hide(&mut self) {
		self.visible = false;
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;

		Ok(())
	}
}

impl BranchListPopup {
	pub fn new(env: &Environment) -> Self {
		Self {
			branches: Vec::new(),
			filtered: Vec::new(),
			filter: String::new(),
			action_mode: false,
			action_message: None,
			local: true,
			has_remotes: false,
			visible: false,
			selection: 0,
			scroll: VerticalScroll::new(),
			queue: env.queue.clone(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			current_height: Cell::new(0),
			list_area: Cell::new(Rect::default()),
			repo: env.repo.clone(),
		}
	}

	fn handle_mouse_event(
		&mut self,
		event: &Event,
	) -> Result<EventState> {
		let Event::Mouse(mouse) = event else {
			return Ok(EventState::NotConsumed);
		};
		let area = self.list_area.get();
		match mouse.kind {
			MouseEventKind::Down(MouseButton::Left)
				if area
					.contains((mouse.column, mouse.row).into()) =>
			{
				let visible_row =
					usize::from(mouse.row.saturating_sub(area.y));
				let selected = self.scroll.get_top() + visible_row;
				if selected < self.filtered.len() {
					self.set_selection(selected.try_into()?)?;
				}
			}
			MouseEventKind::ScrollUp
				if area
					.contains((mouse.column, mouse.row).into()) =>
			{
				self.move_selection(ScrollType::Down)?;
			}
			MouseEventKind::ScrollDown
				if area
					.contains((mouse.column, mouse.row).into()) =>
			{
				self.move_selection(ScrollType::Up)?;
			}
			_ => {}
		}
		Ok(EventState::Consumed)
	}

	fn move_event(&mut self, e: &KeyEvent) -> Result<EventState> {
		if key_match(e, self.key_config.keys.exit_popup) {
			self.hide();
		} else if key_match(e, self.key_config.keys.move_down) {
			return self
				.move_selection(ScrollType::Up)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.move_up) {
			return self
				.move_selection(ScrollType::Down)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.page_down) {
			return self
				.move_selection(ScrollType::PageDown)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.page_up) {
			return self
				.move_selection(ScrollType::PageUp)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.home) {
			return self
				.move_selection(ScrollType::Home)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.end) {
			return self
				.move_selection(ScrollType::End)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.tab_toggle) {
			self.local = !self.local;
			self.check_remotes();
			self.update_branches()?;
		}
		Ok(EventState::NotConsumed)
	}

	/// Handle the branch picker as a small, self-contained modal editor.
	/// Printable keys filter; actions are intentionally behind the configured
	/// action prefix so branch names can be explored safely.
	#[allow(clippy::too_many_lines)] // Filtering and branch actions share selection validity checks.
	fn modal_event(&mut self, e: &KeyEvent) -> Result<EventState> {
		if self.action_mode {
			if is_plain_escape(e)
				|| key_match(e, self.key_config.keys.exit_popup)
				|| key_match(e, self.key_config.keys.action_prefix)
			{
				self.action_mode = false;
				self.action_message = None;
				return Ok(EventState::Consumed);
			}
			if let KeyCode::Char(c) = e.code {
				if !plain_action_character(e) {
					self.action_message = Some(
						"Actions require an unmodified letter; Esc cancels",
					);
					return Ok(EventState::Consumed);
				}
				let action = c.to_ascii_lowercase();
				let available = match action {
					'c' => self.local,
					'r' => self.local && self.valid_selection(),
					'd' | 'm' | 'e' => {
						self.valid_selection()
							&& !self.selection_is_cur_branch()
					}
					'x' => self.valid_selection(),
					_ => {
						self.action_message = Some(
							"Unknown action; use c/r/d/m/e/x or Esc",
						);
						return Ok(EventState::Consumed);
					}
				};
				if !available {
					self.action_message = Some(match action {
						'c' => {
							"Create is available on Local branches"
						}
						'r' if !self.local => {
							"Rename is available on Local branches"
						}
						'r' => "No matching branch selected",
						'd' | 'm' | 'e'
							if !self.valid_selection() =>
						{
							"No matching branch selected"
						}
						'd' | 'm' | 'e' => {
							"Current branch cannot be changed"
						}
						'x' => "No matching branch to compare",
						_ => "Action unavailable",
					});
					return Ok(EventState::Consumed);
				}
				self.action_mode = false;
				self.action_message = None;
				match action {
					'c' => {
						self.queue.push(InternalEvent::CreateBranch);
					}
					'r' => self.rename_branch(),
					'd' => self.delete_branch(),
					'm' => {
						try_or_popup!(
							self,
							"merge branch error:",
							self.merge_branch()
						);
					}
					'e' => self.rebase_branch(),
					'x' => {
						self.hide();
						if let Some(commit_id) =
							self.get_selected_commit()
						{
							self.queue
								.push(InternalEvent::OpenPopup(
								StackablePopupOpen::CompareCommits(
									InspectCommitOpen::new(commit_id),
								),
							));
						}
					}
					_ => unreachable!(
						"available actions are exhaustive"
					),
				}
			} else {
				self.action_message =
					Some("Choose c/r/d/m/e/x, or Esc to cancel");
			}
			return Ok(EventState::Consumed);
		}

		if key_match(e, self.key_config.keys.action_prefix) {
			self.action_mode = true;
			self.action_message = None;
			return Ok(EventState::Consumed);
		}
		if is_plain_escape(e) {
			if self.filter.is_empty() {
				self.hide();
			} else {
				self.filter.clear();
				self.rebuild_filter();
			}
			return Ok(EventState::Consumed);
		}
		if key_match(e, self.key_config.keys.exit_popup) {
			self.hide();
			return Ok(EventState::Consumed);
		}
		if e.modifiers == KeyModifiers::CONTROL
			&& matches!(e.code, KeyCode::Char('n' | 'p'))
		{
			let scroll = if e.code == KeyCode::Char('n') {
				ScrollType::Up
			} else {
				ScrollType::Down
			};
			self.move_selection(scroll)?;
			return Ok(EventState::Consumed);
		}
		match e.code {
			KeyCode::Down => {
				self.move_selection(ScrollType::Up)?;
			}
			KeyCode::Up => {
				self.move_selection(ScrollType::Down)?;
			}
			KeyCode::PageDown => {
				self.move_selection(ScrollType::PageDown)?;
			}
			KeyCode::PageUp => {
				self.move_selection(ScrollType::PageUp)?;
			}
			KeyCode::Home => {
				self.move_selection(ScrollType::Home)?;
			}
			KeyCode::End => {
				self.move_selection(ScrollType::End)?;
			}
			KeyCode::Tab => {
				self.local = !self.local;
				self.check_remotes();
				self.update_branches()?;
			}
			KeyCode::Enter if self.valid_selection() => {
				try_or_popup!(
					self,
					"switch branch error:",
					self.switch_to_selected_branch()
				);
			}
			KeyCode::Backspace => {
				self.filter.pop();
				self.rebuild_filter();
			}
			KeyCode::Char(c)
				if e.modifiers.is_empty()
					|| e.modifiers == KeyModifiers::SHIFT =>
			{
				self.filter.push(c);
				self.rebuild_filter();
			}
			_ => {}
		}
		Ok(EventState::Consumed)
	}

	///
	pub fn open(&mut self) -> Result<()> {
		self.filter.clear();
		self.action_mode = false;
		self.action_message = None;
		self.show()?;
		self.update_branches()?;

		Ok(())
	}

	pub fn branch_finder_update(&mut self, idx: usize) -> Result<()> {
		self.set_selection(idx.try_into()?)?;
		Ok(())
	}

	fn check_remotes(&mut self) {
		if self.visible {
			self.has_remotes =
				get_branches_info(&self.repo.borrow(), false)
					.is_ok_and(|branches| !branches.is_empty());
		}
	}

	/// fetch list of branches
	pub fn update_branches(&mut self) -> Result<()> {
		if self.is_visible() {
			let selected_reference = self
				.selected_branch()
				.map(|branch| branch.reference.clone());
			self.check_remotes();
			self.branches =
				get_branches_info(&self.repo.borrow(), self.local)?;
			//remove remote branch called `HEAD`
			if !self.local {
				self.branches
					.iter()
					.position(|b| b.name.ends_with("/HEAD"))
					.map(|idx| self.branches.remove(idx));
			}
			self.rebuild_filter_preserving(
				selected_reference.as_deref(),
			);
		}
		Ok(())
	}

	fn rebuild_filter(&mut self) {
		self.rebuild_filter_preserving(None);
	}

	fn rebuild_filter_preserving(
		&mut self,
		selected_reference: Option<&str>,
	) {
		let query = self.filter.to_lowercase();
		self.filtered = self
			.branches
			.iter()
			.enumerate()
			.filter(|(_, branch)| {
				query.is_empty()
					|| branch.name.to_lowercase().contains(&query)
					|| (query.starts_with("refs/")
						&& branch
							.reference
							.to_lowercase()
							.contains(&query))
					|| branch
						.top_commit_message
						.to_lowercase()
						.contains(&query)
			})
			.map(|(idx, _)| idx)
			.collect();
		self.selection = selected_reference
			.and_then(|reference| {
				self.filtered.iter().position(|idx| {
					self.branches[*idx].reference == reference
				})
			})
			.and_then(|idx| u16::try_from(idx).ok())
			.unwrap_or(0);
		self.scroll.reset();
	}

	///
	pub fn update_git(
		&mut self,
		ev: AsyncGitNotification,
	) -> Result<()> {
		if self.is_visible() && ev == AsyncGitNotification::Push {
			self.update_branches()?;
		}

		Ok(())
	}

	const fn valid_selection(&self) -> bool {
		!self.filtered.is_empty()
	}

	fn selected_branch(&self) -> Option<&BranchInfo> {
		self.filtered
			.get(usize::from(self.selection))
			.and_then(|idx| self.branches.get(*idx))
	}

	fn merge_branch(&mut self) -> Result<()> {
		if let Some(branch) = self.selected_branch() {
			sync::merge_branch(
				&self.repo.borrow(),
				&branch.name,
				self.get_branch_type(),
			)?;

			self.hide_and_switch_tab()?;
		}

		Ok(())
	}

	fn rebase_branch(&mut self) {
		if let Some(branch) = self.selected_branch() {
			// Route through the rebase workspace: a reviewed
			// draft, then a managed native replay.
			self.queue
				.push(InternalEvent::RebaseOnto(branch.clone()));
			self.hide();
		}
	}

	fn inspect_head_of_branch(&mut self) {
		if let Some(commit_id) = self.get_selected_commit() {
			self.hide();
			self.queue.push(InternalEvent::OpenPopup(
				StackablePopupOpen::InspectCommit(
					InspectCommitOpen::new(commit_id),
				),
			));
		}
	}

	const fn get_branch_type(&self) -> BranchType {
		if self.local {
			BranchType::Local
		} else {
			BranchType::Remote
		}
	}

	fn hide_and_switch_tab(&mut self) -> Result<()> {
		self.hide();
		self.queue.push(InternalEvent::Update(NeedsUpdate::ALL));

		if sync::repo_state(&self.repo.borrow())? != RepoState::Clean
		{
			self.queue.push(InternalEvent::TabSwitchStatus);
		}

		Ok(())
	}

	fn selection_is_cur_branch(&self) -> bool {
		self.selected_branch().is_some_and(|b| {
			b.local_details().is_some_and(|details| details.is_head)
		})
	}

	// top commit of selected branch
	fn get_selected_commit(&self) -> Option<CommitId> {
		self.selected_branch().map(|b| b.top_commit)
	}

	///
	fn move_selection(&mut self, scroll: ScrollType) -> Result<bool> {
		let new_selection = match scroll {
			ScrollType::Up => self.selection.saturating_add(1),
			ScrollType::Down => self.selection.saturating_sub(1),
			ScrollType::PageDown => self
				.selection
				.saturating_add(self.current_height.get()),
			ScrollType::PageUp => self
				.selection
				.saturating_sub(self.current_height.get()),
			ScrollType::Home => 0,
			ScrollType::End => {
				let num_branches: u16 =
					self.filtered.len().try_into()?;
				num_branches.saturating_sub(1)
			}
		};

		self.set_selection(new_selection)?;

		Ok(true)
	}

	fn set_selection(&mut self, selection: u16) -> Result<()> {
		let num_branches: u16 = self.filtered.len().try_into()?;
		let num_branches = num_branches.saturating_sub(1);

		let selection = if selection > num_branches {
			num_branches
		} else {
			selection
		};

		self.selection = selection;

		Ok(())
	}

	/// Get branches to display
	fn get_text(
		&self,
		theme: &SharedTheme,
		width_available: u16,
		height: usize,
	) -> Text<'_> {
		const UPSTREAM_SYMBOL: char = '\u{2191}';
		const TRACKING_SYMBOL: char = '\u{2193}';
		const HEAD_SYMBOL: char = '*';
		const EMPTY_SYMBOL: char = ' ';
		const THREE_DOTS: &str = "...";
		const THREE_DOTS_LENGTH: usize = THREE_DOTS.len(); // "..."
		const COMMIT_HASH_LENGTH: usize = 8;
		const IS_HEAD_STAR_LENGTH: usize = 3; // "*  "

		let branch_name_length: usize =
			width_available as usize * 40 / 100;
		// commit message takes up the remaining width
		let commit_message_length: usize = (width_available as usize)
			.saturating_sub(COMMIT_HASH_LENGTH)
			.saturating_sub(branch_name_length)
			.saturating_sub(IS_HEAD_STAR_LENGTH)
			.saturating_sub(THREE_DOTS_LENGTH);
		let mut txt = Vec::new();

		for (i, branch_idx) in self
			.filtered
			.iter()
			.skip(self.scroll.get_top())
			.take(height)
			.enumerate()
		{
			let displaybranch = &self.branches[*branch_idx];
			let mut commit_message =
				sanitize_summary(&displaybranch.top_commit_message);
			if UnicodeWidthStr::width(commit_message.as_str())
				> commit_message_length
			{
				let (truncated, _) = commit_message.unicode_truncate(
					commit_message_length
						.saturating_sub(THREE_DOTS_LENGTH),
				);
				commit_message = truncated.to_owned();
				commit_message += THREE_DOTS;
			}

			let mut branch_name = displaybranch.name.clone();
			if UnicodeWidthStr::width(branch_name.as_str())
				> branch_name_length.saturating_sub(THREE_DOTS_LENGTH)
			{
				branch_name = branch_name
					.unicode_truncate(
						branch_name_length
							.saturating_sub(THREE_DOTS_LENGTH),
					)
					.0
					.to_string();
				branch_name += THREE_DOTS;
			}

			let selected = (self.selection as usize)
				.saturating_sub(self.scroll.get_top())
				== i;

			let is_head = displaybranch
				.local_details()
				.is_some_and(|details| details.is_head);
			let is_head_str =
				if is_head { HEAD_SYMBOL } else { EMPTY_SYMBOL };
			let upstream_tracking_str = match displaybranch.details {
				BranchDetails::Local(LocalBranch {
					has_upstream,
					..
				}) if has_upstream => UPSTREAM_SYMBOL,
				BranchDetails::Remote(RemoteBranch {
					has_tracking,
					..
				}) if has_tracking => TRACKING_SYMBOL,
				_ => EMPTY_SYMBOL,
			};

			let span_prefix = Span::styled(
				format!("{is_head_str}{upstream_tracking_str} "),
				theme.commit_author(selected),
			);
			let span_hash = Span::styled(
				format!(
					"{} ",
					displaybranch.top_commit.get_short_string()
				),
				theme.commit_hash(selected),
			);
			let span_msg = Span::styled(
				commit_message.clone(),
				theme.text(true, selected),
			);
			let span_name = Span::styled(
				format!("{branch_name:branch_name_length$} "),
				theme.branch(selected, is_head),
			);

			txt.push(Line::from(vec![
				span_prefix,
				span_name,
				span_hash,
				span_msg,
			]));
		}

		Text::from(txt)
	}

	///
	fn switch_to_selected_branch(&mut self) -> Result<()> {
		if !self.valid_selection() {
			anyhow::bail!("no valid branch selected");
		}

		let status = sync::status::get_status(
			&self.repo.borrow(),
			StatusType::WorkingDir,
			None,
		)
		.expect("Could not get status");

		let selected_branch =
			self.selected_branch().ok_or_else(|| {
				anyhow::anyhow!("no valid branch selected")
			})?;
		if status.is_empty() {
			if self.local {
				checkout_branch(
					&self.repo.borrow(),
					&selected_branch.name,
				)?;
				self.hide();
			} else {
				checkout_remote_branch(
					&self.repo.borrow(),
					selected_branch,
				)?;
				self.local = true;
				self.update_branches()?;
			}
			self.queue.push(InternalEvent::Update(NeedsUpdate::ALL));
		} else {
			self.queue.push(InternalEvent::CheckoutOption(
				selected_branch.clone(),
			));
		}

		Ok(())
	}

	fn draw_tabs(&self, f: &mut Frame, r: Rect) {
		let tabs: Vec<Line> =
			[Span::raw("Local"), Span::raw("Remote")]
				.iter()
				.cloned()
				.map(Line::from)
				.collect();

		f.render_widget(
			Tabs::new(tabs)
				.block(
					Block::default()
						.borders(Borders::BOTTOM)
						.border_style(self.theme.block(false)),
				)
				.style(self.theme.tab(false))
				.highlight_style(self.theme.tab(true))
				.divider(strings::tab_divider(&self.key_config))
				.select(if self.local { 0 } else { 1 }),
			r,
		);
	}

	fn draw_list(&self, f: &mut Frame, r: Rect) -> Result<()> {
		let height_in_lines = r.height as usize;
		self.current_height.set(height_in_lines.try_into()?);

		self.scroll.update(
			self.selection as usize,
			self.filtered.len(),
			height_in_lines,
		);

		f.render_widget(
			Paragraph::new(self.get_text(
				&self.theme,
				r.width,
				height_in_lines,
			))
			.alignment(Alignment::Left),
			r,
		);

		let mut r = r;
		r.width += 1;
		r.height += 2;
		r.y = r.y.saturating_sub(1);

		self.scroll.draw(f, r, &self.theme);

		Ok(())
	}

	fn draw_filter(&self, f: &mut Frame, r: Rect) {
		let context = self.selected_branch().map_or_else(
			|| "No matching branches".to_string(),
			|branch| {
				let upstream = match &branch.details {
					BranchDetails::Local(details) => {
						details.upstream.as_ref().map_or_else(
							|| "no upstream".to_string(),
							|u| format!("upstream {}", u.reference),
						)
					}
					BranchDetails::Remote(details) => {
						if details.has_tracking {
							"tracking".to_string()
						} else {
							"remote".to_string()
						}
					}
				};
				let head = if branch
					.local_details()
					.is_some_and(|d| d.is_head)
				{
					"HEAD · "
				} else {
					""
				};
				format!(
					"{head}{} · {upstream} · {}",
					branch.name,
					sanitize_summary(&branch.top_commit_message)
				)
			},
		);
		let mode_hint = if self.action_mode {
			self.action_message.unwrap_or(
				" ACTION: c create  r rename  d delete  m merge  e rebase  x compare  Esc cancel",
			)
		} else {
			" type to filter · Ctrl-N/P or ↑/↓ move · Enter checkout · prefix actions"
		};
		let query = if self.filter.is_empty() {
			"/"
		} else {
			&self.filter
		};
		let line = Line::from(vec![
			Span::styled(
				format!("/ {query}"),
				self.theme.text(true, false),
			),
			Span::styled(
				format!(
					"  {} shown/{}{}",
					self.filtered.len(),
					self.branches.len(),
					mode_hint
				),
				self.theme.commit_author(false),
			),
		]);
		let context = context
			.chars()
			.take(r.width.saturating_sub(1) as usize)
			.collect::<String>();
		f.render_widget(
			Paragraph::new(Text::from(vec![
				line,
				Line::from(Span::styled(
					context,
					self.theme.text(true, false),
				)),
			])),
			r,
		);
	}

	fn rename_branch(&self) {
		if !self.local || !self.valid_selection() {
			return;
		}
		let Some(cur_branch) = self.selected_branch() else {
			return;
		};
		self.queue.push(InternalEvent::RenameBranch(
			cur_branch.reference.clone(),
			cur_branch.name.clone(),
		));
	}

	fn delete_branch(&self) {
		let Some(branch) = self.selected_branch() else {
			return;
		};
		let reference = branch.reference.clone();

		self.queue.push(InternalEvent::ConfirmAction(
			if self.local {
				Action::DeleteLocalBranch(reference)
			} else {
				Action::DeleteRemoteBranch(reference)
			},
		));
	}

	#[allow(clippy::too_many_lines)] // Actions are listed in the same order as their key bindings.
	fn add_commands_internal(&self, out: &mut Vec<CommandInfo>) {
		if self.key_config.is_modal() {
			let mut add =
				|name: String,
				 desc: &'static str,
				 binding: Option<crate::keys::GituiKeyEvent>,
				 enabled: bool| {
					let mut text =
						CommandText::new(name, desc, "Branch picker");
					if let Some(binding) = binding {
						text = text.with_binding(binding);
					}
					out.push(CommandInfo::new(text, enabled, true));
				};
			add(
				"↑/↓, Ctrl-N/P".to_owned(),
				"Move through matching branches",
				None,
				true,
			);
			add(
				"Type to filter".to_owned(),
				"Filter by branch, reference, or summary",
				None,
				true,
			);
			add(
				"Tab".to_owned(),
				"Switch between local and remote branches",
				None,
				true,
			);
			add(
				self.key_config.get_hint(self.key_config.keys.enter),
				"Check out the selected branch",
				Some(self.key_config.keys.enter),
				self.valid_selection(),
			);
			add(
				self.key_config
					.get_hint(self.key_config.keys.action_prefix),
				"Open branch actions",
				Some(self.key_config.keys.action_prefix),
				true,
			);
			if self.action_mode {
				for (key, desc, enabled) in [
					('c', "Create a branch", self.local),
					(
						'r',
						"Rename selected local branch",
						self.local && self.valid_selection(),
					),
					(
						'd',
						"Delete selected branch",
						self.valid_selection()
							&& !self.selection_is_cur_branch(),
					),
					(
						'm',
						"Merge selected branch",
						self.valid_selection()
							&& !self.selection_is_cur_branch(),
					),
					(
						'e',
						"Rebase onto selected branch",
						self.valid_selection()
							&& !self.selection_is_cur_branch(),
					),
					(
						'x',
						"Compare selected branch",
						self.valid_selection(),
					),
				] {
					add(key.to_string(), desc, None, enabled);
				}
			}
			return;
		}

		let selection_is_cur_branch = self.selection_is_cur_branch();

		out.push(CommandInfo::new(
			strings::commands::scroll(&self.key_config),
			true,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::close_popup(&self.key_config),
			true,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::commit_details_open(&self.key_config),
			true,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::compare_with_head(&self.key_config),
			!selection_is_cur_branch,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::toggle_branch_popup(
				&self.key_config,
				self.local,
			),
			true,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::select_branch_popup(&self.key_config),
			!selection_is_cur_branch && self.valid_selection(),
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::open_branch_create_popup(
				&self.key_config,
			),
			true,
			self.local,
		));

		out.push(CommandInfo::new(
			strings::commands::delete_branch_popup(&self.key_config),
			!selection_is_cur_branch,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::merge_branch_popup(&self.key_config),
			!selection_is_cur_branch,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::branch_popup_rebase(&self.key_config),
			!selection_is_cur_branch,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::rename_branch_popup(&self.key_config),
			true,
			self.local,
		));

		out.push(CommandInfo::new(
			strings::commands::fetch_remotes(&self.key_config),
			self.has_remotes,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::find_branch(&self.key_config),
			true,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::reset_branch(&self.key_config),
			self.valid_selection(),
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::view_remotes(&self.key_config),
			true,
			self.has_remotes,
		));
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		app::Environment,
		keys::{KeyConfig, Keymap},
	};
	use asyncgit::sync::branch::{BranchDetails, UpstreamBranch};
	use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
	use ratatui::{backend::TestBackend, Terminal};
	use std::rc::Rc;

	fn branch(name: &str, is_head: bool) -> BranchInfo {
		BranchInfo {
			name: name.to_owned(),
			reference: format!("refs/heads/{name}"),
			top_commit_message: format!("{name} tip"),
			top_commit: CommitId::from_str_unchecked(
				"0000000000000000000000000000000000000001",
			)
			.unwrap(),
			details: BranchDetails::Local(LocalBranch {
				is_head,
				has_upstream: true,
				upstream: Some(UpstreamBranch {
					reference: "refs/remotes/origin/main".to_owned(),
				}),
				remote: Some("origin".to_owned()),
			}),
		}
	}

	fn modal_popup() -> BranchListPopup {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut popup = BranchListPopup::new(&env);
		popup.visible = true;
		popup.branches = vec![
			branch("main", true),
			branch("feature/search", false),
			branch("release", false),
		];
		popup.rebuild_filter();
		popup
	}

	#[test]
	fn modal_typing_owns_printable_keys_without_dispatching_actions()
	{
		let mut popup = modal_popup();
		let event = Event::Key(KeyEvent::new(
			KeyCode::Char('q'),
			KeyModifiers::empty(),
		));

		assert_eq!(
			popup.event(&event).unwrap(),
			EventState::Consumed
		);
		assert_eq!(popup.filter, "q");
		assert!(popup.captures_input());
		assert!(popup.queue.pop().is_none());
		assert!(popup.visible);

		popup.action_mode = true;
		assert!(!popup.captures_input());
	}

	#[test]
	fn filtering_preserves_branch_identity_and_modal_actions_are_explicit(
	) {
		let mut popup = modal_popup();
		popup.filter = "feature".to_owned();
		popup.rebuild_filter();
		assert_eq!(popup.filtered, vec![1]);
		assert_eq!(
			popup.selected_branch().unwrap().name,
			"feature/search"
		);

		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char(' '),
				KeyModifiers::empty(),
			)))
			.unwrap();
		assert!(popup.action_mode);
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char('r'),
				KeyModifiers::empty(),
			)))
			.unwrap();
		assert!(!popup.action_mode);
		assert!(matches!(
			popup.queue.pop(),
			Some(InternalEvent::RenameBranch(reference, name))
				if reference == "refs/heads/feature/search" && name == "feature/search"
		));

		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char(' '),
				KeyModifiers::empty(),
			)))
			.unwrap();
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char('d'),
				KeyModifiers::empty(),
			)))
			.unwrap();
		assert!(
			matches!(popup.queue.pop(), Some(InternalEvent::ConfirmAction(Action::DeleteLocalBranch(reference))) if reference == "refs/heads/feature/search")
		);
	}

	#[test]
	fn modal_navigation_moves_within_filtered_results() {
		let mut popup = modal_popup();
		popup.filter = "e".to_owned();
		popup.rebuild_filter();
		assert_eq!(popup.filtered, vec![1, 2]);
		assert_eq!(
			popup.selected_branch().unwrap().name,
			"feature/search"
		);
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char('n'),
				KeyModifiers::CONTROL,
			)))
			.unwrap();
		assert_eq!(popup.selected_branch().unwrap().name, "release");
	}

	#[test]
	fn single_letter_filter_does_not_match_reference_prefixes() {
		let mut popup = modal_popup();
		popup.filter = "e".to_owned();
		popup.rebuild_filter();
		assert_eq!(popup.filtered, vec![1, 2]);
	}

	#[test]
	fn modal_filter_accepts_slashes_in_branch_names() {
		let mut popup = modal_popup();
		for c in "feature/".chars() {
			popup
				.event(&Event::Key(KeyEvent::new(
					KeyCode::Char(c),
					KeyModifiers::empty(),
				)))
				.unwrap();
		}
		assert_eq!(popup.filter, "feature/");
		assert_eq!(popup.filtered, vec![1]);
	}

	#[test]
	fn escape_cancels_actions_then_clears_filter_then_closes() {
		let mut popup = modal_popup();
		popup.filter = "feature".to_owned();
		popup.rebuild_filter();
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char(' '),
				KeyModifiers::empty(),
			)))
			.unwrap();
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Esc,
				KeyModifiers::empty(),
			)))
			.unwrap();
		assert!(popup.visible);
		assert!(!popup.action_mode);
		assert_eq!(popup.filter, "feature");
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Esc,
				KeyModifiers::empty(),
			)))
			.unwrap();
		assert!(popup.visible);
		assert!(popup.filter.is_empty());
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Esc,
				KeyModifiers::empty(),
			)))
			.unwrap();
		assert!(!popup.visible);
	}

	#[test]
	fn action_prefix_uses_configured_binding_and_rejects_modifiers() {
		let mut popup = modal_popup();
		let mut config = KeyConfig::with_keymap(Keymap::Helix);
		config.keys.action_prefix = crate::keys::GituiKeyEvent::new(
			KeyCode::Char(':'),
			KeyModifiers::empty(),
		);
		popup.key_config = Rc::new(config);
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char(' '),
				KeyModifiers::empty(),
			)))
			.unwrap();
		assert!(!popup.action_mode);
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char(':'),
				KeyModifiers::empty(),
			)))
			.unwrap();
		assert!(popup.action_mode);
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char('d'),
				KeyModifiers::CONTROL,
			)))
			.unwrap();
		assert!(popup.action_mode);
		assert!(popup.queue.pop().is_none());
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char('d'),
				KeyModifiers::ALT,
			)))
			.unwrap();
		assert!(popup.action_mode);
		assert!(popup.queue.pop().is_none());
	}

	#[test]
	fn enter_with_no_matches_is_inert() {
		let mut popup = modal_popup();
		popup.filter = "no such branch".to_owned();
		popup.rebuild_filter();
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Enter,
				KeyModifiers::empty(),
			)))
			.unwrap();
		assert!(popup.visible);
		assert!(popup.queue.pop().is_none());
	}

	#[test]
	fn filter_rebuild_resets_index_but_reload_can_preserve_reference()
	{
		let mut popup = modal_popup();
		popup.filter = "e".to_owned();
		popup.rebuild_filter();
		popup.set_selection(1).unwrap();
		assert_eq!(popup.selected_branch().unwrap().name, "release");
		popup.filter = "feature".to_owned();
		popup.rebuild_filter();
		assert_eq!(popup.selection, 0);
		assert_eq!(
			popup.selected_branch().unwrap().name,
			"feature/search"
		);

		let reference =
			popup.selected_branch().unwrap().reference.clone();
		popup.branches.reverse();
		popup.rebuild_filter_preserving(Some(&reference));
		assert_eq!(
			popup.selected_branch().unwrap().reference,
			reference
		);
	}

	#[test]
	fn remote_rename_is_reported_unavailable_without_mutation() {
		let mut popup = modal_popup();
		popup.local = false;
		popup.branches = vec![BranchInfo {
			name: "origin/feature".to_owned(),
			reference: "refs/remotes/origin/feature".to_owned(),
			top_commit_message: "tip".to_owned(),
			top_commit: CommitId::from_str_unchecked(
				"0000000000000000000000000000000000000001",
			)
			.unwrap(),
			details: BranchDetails::Remote(RemoteBranch {
				has_tracking: false,
			}),
		}];
		popup.rebuild_filter();
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char(' '),
				KeyModifiers::empty(),
			)))
			.unwrap();
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char('r'),
				KeyModifiers::empty(),
			)))
			.unwrap();
		assert!(popup.action_mode);
		assert_eq!(
			popup.action_message,
			Some("Rename is available on Local branches")
		);
		assert!(popup.queue.pop().is_none());
	}

	#[test]
	fn modal_commands_show_picker_controls_and_configured_prefix() {
		let mut popup = modal_popup();
		let mut config = KeyConfig::with_keymap(Keymap::Helix);
		config.keys.action_prefix = crate::keys::GituiKeyEvent::new(
			KeyCode::Char(':'),
			KeyModifiers::empty(),
		);
		popup.key_config = Rc::new(config);
		let mut commands = Vec::new();
		popup.add_commands_internal(&mut commands);
		let names: Vec<_> = commands
			.iter()
			.map(|command| command.text.name.as_str())
			.collect();
		assert!(names.contains(&"↑/↓, Ctrl-N/P"));
		assert!(names.contains(&"Type to filter"));
		assert!(names.contains(&"Tab"));
		assert!(names.contains(&":"));
		assert!(!names
			.iter()
			.any(|name| name.contains("rename branch")));
	}

	#[test]
	fn filtered_viewport_selection_never_underflows() {
		let mut popup = modal_popup();
		popup.filtered = vec![0, 1, 2];
		popup.scroll.update(2, 3, 1);
		popup.selection = 0;
		let text = popup.get_text(&popup.theme, 20, 1);
		assert_eq!(text.lines.len(), 1);
	}

	#[test]
	fn narrow_modal_render_does_not_panic() {
		let popup = modal_popup();
		let mut terminal =
			Terminal::new(TestBackend::new(24, 8)).unwrap();
		terminal
			.draw(|frame| popup.draw(frame, frame.area()).unwrap())
			.unwrap();
	}
}
