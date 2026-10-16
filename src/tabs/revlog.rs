use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo,
		CommitDetailsComponent, CommitList, Component,
		DrawableComponent, EventState, SelectionStatus,
	},
	keys::{key_match, SharedKeyConfig},
	popups::{FileTreeOpen, InspectCommitOpen},
	queue::{InternalEvent, Queue, StackablePopupOpen},
	strings::{self, order},
	try_or_popup,
	ui::style::{SharedTheme, Theme},
};
use anyhow::Result;
use asyncgit::{
	asyncjob::AsyncSingleJob,
	sync::{
		self, filter_commit_by_search, CommitId, LogFilterSearch,
		LogFilterSearchOptions, RepoPathRef,
	},
	AsyncBranchesJob, AsyncCommitFilterJob, AsyncGitNotification,
	AsyncLog, AsyncTags, ProgressPercent,
};
use crossbeam_channel::Sender;
use crossterm::event::{Event, MouseButton, MouseEventKind};
use indexmap::IndexSet;
use ratatui::{
	layout::{Alignment, Constraint, Direction, Layout, Rect},
	text::Span,
	widgets::{Block, Borders, Paragraph},
	Frame,
};
use std::{
	cell::Cell,
	collections::HashSet,
	rc::Rc,
	sync::{
		atomic::{AtomicBool, Ordering},
		Arc,
	},
	time::Duration,
};
use sync::CommitTags;

struct LogSearchResult {
	options: LogFilterSearchOptions,
	duration: Duration,
}

//TODO: deserves its own component
enum LogSearch {
	Off,
	LoadingHistory(LogFilterSearchOptions),
	Searching(
		AsyncSingleJob<AsyncCommitFilterJob>,
		LogFilterSearchOptions,
		Option<ProgressPercent>,
		Arc<AtomicBool>,
	),
	Results(LogSearchResult),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GraphRequestKey {
	start: usize,
	len: usize,
	branch_tips: HashSet<CommitId>,
	stashes: HashSet<CommitId>,
	head_id: Option<CommitId>,
}

/// Top-level revlog tab that ties together the commit list, details, and search.
pub struct Revlog {
	repo: RepoPathRef,
	commit_details: CommitDetailsComponent,
	list: CommitList,
	git_log: AsyncLog,
	search: LogSearch,
	git_tags: AsyncTags,
	git_local_branches: AsyncSingleJob<AsyncBranchesJob>,
	git_remote_branches: AsyncSingleJob<AsyncBranchesJob>,
	queue: Queue,
	visible: bool,
	key_config: SharedKeyConfig,
	sender: Sender<AsyncGitNotification>,
	theme: SharedTheme,
	list_area: Cell<Rect>,
	details_area: Cell<Rect>,
	mouse_capture_details: Option<bool>,
	history_generation: Option<u64>,
	graph_request: Option<GraphRequestKey>,
}

impl Revlog {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			repo: env.repo.clone(),
			queue: env.queue.clone(),
			commit_details:
				CommitDetailsComponent::new_with_inline_preview(env),
			list: CommitList::new(
				env,
				&strings::log_title(&env.key_config),
			),
			git_log: AsyncLog::new(
				env.repo.borrow().clone(),
				&env.sender_git,
				None,
			),
			search: LogSearch::Off,
			git_tags: AsyncTags::new(
				env.repo.borrow().clone(),
				&env.sender_git,
			),
			git_local_branches: AsyncSingleJob::new(
				env.sender_git.clone(),
			),
			git_remote_branches: AsyncSingleJob::new(
				env.sender_git.clone(),
			),
			visible: false,
			key_config: env.key_config.clone(),
			sender: env.sender_git.clone(),
			theme: env.theme.clone(),
			list_area: Cell::new(Rect::default()),
			details_area: Cell::new(Rect::default()),
			mouse_capture_details: None,
			history_generation: None,
			graph_request: None,
		}
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.git_log.is_pending()
			|| self.is_search_pending()
			|| self.git_tags.is_pending()
			|| self.git_local_branches.is_pending()
			|| self.git_remote_branches.is_pending()
			|| self.commit_details.any_work_pending()
	}

	const fn is_search_pending(&self) -> bool {
		matches!(
			self.search,
			LogSearch::LoadingHistory(_)
				| LogSearch::Searching(_, _, _, _)
		)
	}

	///
	pub fn update(&mut self) -> Result<()> {
		if self.is_visible() {
			self.update_log_range()?;
			self.update_graph_rows()?;
			self.update_search_loading()?;

			self.git_tags.request(Duration::from_secs(3), false)?;

			if self.commit_details.is_visible() {
				let commit = self.selected_commit();
				let tags = self.selected_commit_tags(commit.as_ref());
				self.commit_details
					.set_selected_commit(commit, tags.as_ref())?;
			}
		}

		Ok(())
	}

	/// Computes graph rows for the current commit window. Rows don't depend
	/// on the selection, so identical range requests are shared.
	fn update_log_range(&mut self) -> Result<()> {
		let status = self.git_log.range_status()?;
		if self.history_generation != Some(status.generation) {
			self.list.clear();
			self.history_generation = Some(status.generation);
			self.graph_request = None;
		}

		let known = self.list.loaded_commit_count();
		if status.loaded > known {
			self.list.refresh_extend_data(
				self.git_log
					.get_slice(known, status.loaded - known)?,
			);
		}

		let (start, requested_end) = self.list.requested_log_range();
		let end = requested_end.min(status.loaded);
		if end > start {
			let ids = self.list.ids_in_range(start, end);
			if let Some(infos) =
				self.git_log.get_commit_info_range(&ids)
			{
				self.list.refresh_metadata_range(start, infos);
			} else if !ids.is_empty() {
				self.git_log.request_commit_info_ids(ids)?;
			}
		}

		let status =
			if status.loaded < requested_end && status.has_more {
				self.git_log
					.request_range(start, requested_end - start)?
			} else {
				status
			};
		self.list.set_history_status(status);

		if self.list.should_load_more() && status.has_more {
			let loaded = status.loaded;
			let page = requested_end.saturating_sub(start).max(128);
			self.git_log.request_range(loaded, page)?;
		}
		Ok(())
	}

	fn update_graph_rows(&mut self) -> Result<()> {
		if !self.list.is_graph_visible() {
			return Ok(());
		}
		let (start, requested_end) = self.list.requested_log_range();
		let loaded = self.list.loaded_commit_count();
		let end = requested_end.min(loaded);
		if end <= start {
			return Ok(());
		}
		let len = end - start;

		let head_id = self.list.local_branches().iter().find_map(
			|(id, branches)| {
				let has_head = branches.iter().any(|branch| {
					branch.local_details().is_some_and(|b| b.is_head)
				});
				has_head.then_some(*id)
			},
		);

		let branch_tips: HashSet<_> = self
			.list
			.local_branches()
			.keys()
			.chain(self.list.remote_branches().keys())
			.copied()
			.collect();

		// TODO: include stashes (heh)
		let stashes = HashSet::new();

		let request_key = GraphRequestKey {
			start,
			len,
			branch_tips: branch_tips.clone(),
			stashes: stashes.clone(),
			head_id,
		};
		if self.graph_request.as_ref() != Some(&request_key) {
			self.git_log.request_graph_range(
				start,
				len,
				branch_tips,
				stashes,
				head_id,
			)?;
			self.graph_request = Some(request_key);
		}
		if let Some(rows) =
			self.git_log.get_cached_graph_rows(start, len)
		{
			self.list.set_graph_rows(rows);
		}
		Ok(())
	}

	fn update_search_loading(&mut self) -> Result<()> {
		if !matches!(self.search, LogSearch::LoadingHistory(_)) {
			return Ok(());
		}
		if !self.git_log.range_status()?.complete {
			return Ok(());
		}
		let LogSearch::LoadingHistory(options) =
			std::mem::replace(&mut self.search, LogSearch::Off)
		else {
			return Ok(());
		};
		self.start_search(options);
		Ok(())
	}

	fn start_search(&mut self, options: LogFilterSearchOptions) {
		let filter = filter_commit_by_search(LogFilterSearch::new(
			options.clone(),
		));
		let cancellation_flag = Arc::new(AtomicBool::new(false));
		let job = AsyncSingleJob::new(self.sender.clone());
		job.spawn(AsyncCommitFilterJob::new(
			self.repo.borrow().clone(),
			self.git_log.get_items().unwrap_or_default(),
			filter,
			Arc::clone(&cancellation_flag),
		));
		self.search = LogSearch::Searching(
			job,
			options,
			None,
			cancellation_flag,
		);
	}

	///
	pub fn update_git(
		&mut self,
		ev: AsyncGitNotification,
	) -> Result<()> {
		if self.visible {
			match ev {
				AsyncGitNotification::DiffPreview => {
					self.commit_details.update_preview();
				}
				AsyncGitNotification::CommitFiles
				| AsyncGitNotification::CommitDetails => self.update()?,
				AsyncGitNotification::Log => {
					if let Some(error) = self.git_log.take_error()? {
						self.queue.push(InternalEvent::ShowErrorMsg(
							format!("log fetch: {error}"),
						));
					} else {
						self.update()?;
					}
				}
				AsyncGitNotification::CommitFilter => {
					self.update_search_state();
				}
				AsyncGitNotification::Tags => {
					if let Some(tags) = self.git_tags.last()? {
						self.list.set_tags(tags);
						self.update()?;
					}
				}
				AsyncGitNotification::Branches => {
					if let Some(local_branches) =
						self.git_local_branches.take_last()
					{
						if let Some(Ok(local_branches)) =
							local_branches.result()
						{
							self.list
								.set_local_branches(local_branches);
							self.update()?;
						}
					}

					if let Some(remote_branches) =
						self.git_remote_branches.take_last()
					{
						if let Some(Ok(remote_branches)) =
							remote_branches.result()
						{
							self.list
								.set_remote_branches(remote_branches);
							self.update()?;
						}
					}
				}
				_ => (),
			}
		}

		Ok(())
	}

	/// Apply completed preview projection and highlighting work.
	pub fn update_rendering(&mut self) {
		self.commit_details.update_rendering();
	}

	fn selected_commit(&self) -> Option<CommitId> {
		self.list.selected_commit_id()
	}

	fn action_commit(&self) -> Option<CommitId> {
		self.list.action_commit()
	}

	fn has_multiple_action_commits(&self) -> bool {
		self.list.has_multiple_action_commits()
	}

	fn report_ambiguous_action_target(&self) {
		self.queue.push(InternalEvent::ShowErrorMsg(
			"This action requires one commit; clear selection or select one".to_owned(),
		));
	}

	fn selected_commit_tags(
		&self,
		commit: Option<&CommitId>,
	) -> Option<CommitTags> {
		let tags = self.list.tags();

		commit.and_then(|commit| {
			tags.and_then(|tags| tags.get(commit).cloned())
		})
	}

	///
	pub fn select_commit(&mut self, id: CommitId) -> Result<()> {
		self.list.select_commit(id)
	}

	fn revert_commit(&self) -> Result<()> {
		if let Some(c) = self.action_commit() {
			sync::revert_commit(&self.repo.borrow(), c)?;
			self.queue.push(InternalEvent::TabSwitchStatus);
		}

		Ok(())
	}

	fn inspect_commit(&self) {
		if let Some(commit_id) = self.selected_commit() {
			let tags =
				self.selected_commit_tags(Some(commit_id).as_ref());
			self.queue.push(InternalEvent::OpenPopup(
				StackablePopupOpen::InspectCommit(
					InspectCommitOpen::new_with_tags(commit_id, tags),
				),
			));
		}
	}

	pub fn search(&mut self, options: LogFilterSearchOptions) {
		if !self.can_start_search() {
			return;
		}

		if matches!(
			self.search,
			LogSearch::Off | LogSearch::Results(_)
		) {
			log::info!("start full-history search: {options:?}");
			self.list.set_highlighting(None);
			self.search = LogSearch::LoadingHistory(options);
			if let Err(error) = self.git_log.ensure_loaded(usize::MAX)
			{
				self.search = LogSearch::Off;
				self.queue.push(InternalEvent::ShowErrorMsg(
					format!("search error: {error}"),
				));
			}
		}
	}

	fn cancel_search(&mut self) -> bool {
		if matches!(self.search, LogSearch::LoadingHistory(_)) {
			if let Err(error) = self.git_log.cancel_pending_walk() {
				self.queue.push(InternalEvent::ShowErrorMsg(
					format!("search cancellation error: {error}"),
				));
			}
			self.search = LogSearch::Off;
			return true;
		}
		if let LogSearch::Searching(_, _, _, cancellation_flag) =
			&self.search
		{
			cancellation_flag.store(true, Ordering::Relaxed);
			self.list.set_highlighting(None);
			return true;
		}

		false
	}

	fn update_search_state(&mut self) {
		match &mut self.search {
			LogSearch::Off
			| LogSearch::LoadingHistory(_)
			| LogSearch::Results(_) => (),
			LogSearch::Searching(
				search,
				options,
				progress,
				cancel,
			) => {
				if search.is_pending() {
					//update progress
					*progress = search.progress();
				} else if let Some(search) = search
					.take_last()
					.and_then(|search| search.result())
				{
					match search {
						Ok(search) => {
							let was_aborted =
								cancel.load(Ordering::Relaxed);

							self.search = if was_aborted {
								LogSearch::Off
							} else {
								self.list.set_highlighting(Some(
									Rc::new(
										search
											.result
											.into_iter()
											.collect::<IndexSet<_>>(),
									),
								));

								LogSearch::Results(LogSearchResult {
									options: options.clone(),
									duration: search.duration,
								})
							};
						}
						Err(err) => {
							self.queue.push(
								InternalEvent::ShowErrorMsg(format!(
									"search error: {err}",
								)),
							);

							self.search = LogSearch::Off;
						}
					}
				}
			}
		}
	}

	const fn is_in_search_mode(&self) -> bool {
		!matches!(self.search, LogSearch::Off)
	}

	fn draw_search(&self, f: &mut Frame, area: Rect) {
		let (text, title) = match &self.search {
			LogSearch::LoadingHistory(options) => (
				format!("'{}'", options.search_pattern.clone()),
				format!(
					"(loading history: {} commits)",
					self.list.loaded_commit_count()
				),
			),
			LogSearch::Searching(_, options, progress, _) => (
				format!("'{}'", options.search_pattern.clone()),
				format!(
					"({}%)",
					progress
						.map(|progress| progress.progress)
						.unwrap_or_default()
				),
			),
			LogSearch::Results(results) => {
				let info = self.list.highlighted_selection_info();

				(
					format!(
						"'{}' (duration: {:?})",
						results.options.search_pattern.clone(),
						results.duration,
					),
					format!(
						"({}/{})",
						(info.0 + 1).min(info.1),
						info.1
					),
				)
			}
			LogSearch::Off => (String::new(), String::new()),
		};

		f.render_widget(
			Paragraph::new(text)
				.block(
					Block::default()
						.title(Span::styled(
							format!(
								"{} {}",
								strings::POPUP_TITLE_LOG_SEARCH,
								title
							),
							self.theme.title(true),
						))
						.borders(Borders::ALL)
						.border_style(Theme::attention_block()),
				)
				.alignment(Alignment::Left),
			area,
		);
	}

	const fn can_close_search(&self) -> bool {
		self.is_in_search_mode() && !self.is_search_pending()
	}

	fn can_start_search(&self) -> bool {
		!self.git_log.is_pending() && !self.is_search_pending()
	}
}

impl DrawableComponent for Revlog {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		let area = if self.is_in_search_mode() {
			Layout::default()
				.direction(Direction::Vertical)
				.constraints(
					[Constraint::Min(1), Constraint::Length(3)]
						.as_ref(),
				)
				.split(area)
		} else {
			Rc::new([area])
		};

		let chunks = Layout::default()
			.direction(Direction::Horizontal)
			.constraints(
				[
					Constraint::Percentage(60),
					Constraint::Percentage(40),
				]
				.as_ref(),
			)
			.split(area[0]);

		if self.commit_details.is_visible() {
			self.list_area.set(chunks[0]);
			self.details_area.set(chunks[1]);
			self.list.draw(f, chunks[0])?;
			self.commit_details.draw(f, chunks[1])?;
		} else {
			self.list_area.set(area[0]);
			self.details_area.set(Rect::default());
			self.list.draw(f, area[0])?;
		}

		if self.is_in_search_mode() {
			self.draw_search(f, area[1]);
		}

		Ok(())
	}
}

impl Component for Revlog {
	fn selection_status(&self) -> Option<SelectionStatus> {
		if !self.visible || !self.key_config.is_modal() {
			return None;
		}
		if self.commit_details.is_visible()
			&& self.commit_details.focused()
		{
			self.commit_details.files().selection_status()
		} else {
			self.list.selection_status()
		}
	}

	//TODO: cleanup
	#[allow(clippy::too_many_lines)]
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.visible {
			if let Event::Mouse(mouse) = ev {
				let details = match mouse.kind {
					MouseEventKind::Drag(_)
					| MouseEventKind::Up(_) => self.mouse_capture_details,
					MouseEventKind::Down(MouseButton::Left)
					| MouseEventKind::ScrollUp
					| MouseEventKind::ScrollDown => {
						let position =
							(mouse.column, mouse.row).into();
						if self.commit_details.is_visible()
							&& self
								.details_area
								.get()
								.contains(position)
						{
							Some(true)
						} else if self
							.list_area
							.get()
							.contains(position)
						{
							Some(false)
						} else {
							None
						}
					}
					_ => None,
				};
				if let Some(details) = details {
					if details && self.commit_details.is_visible() {
						self.commit_details.focus(true);
					} else {
						if self.commit_details.focused() {
							self.list.finish_selection();
						}
						self.commit_details.focus(false);
					}
					if matches!(
						mouse.kind,
						MouseEventKind::Down(MouseButton::Left)
					) {
						self.mouse_capture_details = Some(details);
					}
				}
				if matches!(mouse.kind, MouseEventKind::Up(_)) {
					self.mouse_capture_details = None;
				}
			}

			if self.key_config.is_modal() {
				if let Event::Key(k) = ev {
					if key_match(k, self.key_config.keys.focus_next)
						|| key_match(
							k,
							self.key_config.keys.focus_previous,
						) {
						if self.commit_details.is_visible() {
							let focus =
								!self.commit_details.focused();
							if focus {
								self.list.finish_selection();
							}
							self.commit_details.focus(focus);
							return Ok(EventState::Consumed);
						}
					} else if key_match(
						k,
						self.key_config.keys.focus_right,
					) && self.commit_details.is_visible()
						&& !self.commit_details.focused()
					{
						self.list.finish_selection();
						self.commit_details.focus(true);
						return Ok(EventState::Consumed);
					} else if key_match(
						k,
						self.key_config.keys.focus_left,
					) && self.commit_details.focused()
					{
						self.commit_details.focus(false);
						return Ok(EventState::Consumed);
					}
				}

				// Once the details pane owns focus, don't let the commit list
				// consume the event first.
				if self.commit_details.is_visible()
					&& self.commit_details.focused()
				{
					return self.commit_details.event(ev);
				}
			}

			let event_used = self.list.event(ev)?;

			if event_used.is_consumed() {
				self.update()?;
				return Ok(EventState::Consumed);
			} else if let Event::Key(k) = ev {
				if key_match(k, self.key_config.keys.enter) {
					self.commit_details.toggle_visible()?;
					self.update()?;
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.exit_popup,
				) {
					if self.is_search_pending() {
						self.cancel_search();
					} else if self.can_close_search() {
						self.list.set_highlighting(None);
						self.search = LogSearch::Off;
					}
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.copy) {
					try_or_popup!(
						self,
						strings::POPUP_FAIL_COPY,
						self.list.copy_commit_hash()
					);
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.push) {
					self.queue.push(InternalEvent::PushTags);
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.log_tag_commit,
				) {
					if self.has_multiple_action_commits() {
						self.report_ambiguous_action_target();
						return Ok(EventState::Consumed);
					}
					return self.action_commit().map_or(
						Ok(EventState::NotConsumed),
						|id| {
							self.queue
								.push(InternalEvent::TagCommit(id));
							Ok(EventState::Consumed)
						},
					);
				} else if key_match(
					k,
					self.key_config.keys.move_right,
				) && self.commit_details.is_visible()
				{
					self.inspect_commit();
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.select_branch,
				) && !self.is_search_pending()
				{
					self.queue.push(InternalEvent::SelectBranch);
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.status_reset_item,
				) && !self.is_search_pending()
				{
					if self.has_multiple_action_commits() {
						self.report_ambiguous_action_target();
						return Ok(EventState::Consumed);
					}
					try_or_popup!(
						self,
						"revert error:",
						self.revert_commit()
					);

					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.open_file_tree,
				) && !self.is_search_pending()
				{
					return self.selected_commit().map_or(
						Ok(EventState::NotConsumed),
						|id| {
							self.queue.push(
								InternalEvent::OpenPopup(
									StackablePopupOpen::FileTree(
										FileTreeOpen::new(id),
									),
								),
							);
							Ok(EventState::Consumed)
						},
					);
				} else if key_match(k, self.key_config.keys.tags) {
					self.queue.push(InternalEvent::Tags);
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.log_reset_commit,
				) && !self.is_search_pending()
				{
					if self.has_multiple_action_commits() {
						self.report_ambiguous_action_target();
						return Ok(EventState::Consumed);
					}
					return self.action_commit().map_or(
						Ok(EventState::NotConsumed),
						|id| {
							self.queue.push(
								InternalEvent::OpenResetPopup(id),
							);
							Ok(EventState::Consumed)
						},
					);
				} else if key_match(
					k,
					self.key_config.keys.log_reword_commit,
				) && !self.is_search_pending()
				{
					if self.has_multiple_action_commits() {
						self.report_ambiguous_action_target();
						return Ok(EventState::Consumed);
					}
					return self.action_commit().map_or(
						Ok(EventState::NotConsumed),
						|id| {
							self.queue.push(
								InternalEvent::RewordCommit(id),
							);
							Ok(EventState::Consumed)
						},
					);
				} else if key_match(
					k,
					self.key_config.keys.log_rewrite_from,
				) && !self.is_search_pending()
				{
					if self.has_multiple_action_commits() {
						self.report_ambiguous_action_target();
						return Ok(EventState::Consumed);
					}
					return self.action_commit().map_or(
						Ok(EventState::NotConsumed),
						|id| {
							self.queue.push(
								InternalEvent::RebaseRewriteFrom(id),
							);
							Ok(EventState::Consumed)
						},
					);
				} else if key_match(
					k,
					self.key_config.keys.branches_load_more,
				) && !self.is_search_pending()
				{
					let loaded = self.list.loaded_commit_count();
					if self.git_log.range_status()?.has_more {
						self.git_log.request_range(loaded, 128)?;
						self.update()?;
					}
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.log_find)
					&& self.can_start_search()
				{
					self.queue
						.push(InternalEvent::OpenLogSearchPopup);
					return Ok(EventState::Consumed);
				} else if key_match(
					k,
					self.key_config.keys.compare_commits,
				) && self.list.marked_count() > 0
					&& !self.is_search_pending()
				{
					if self.list.marked_count() == 1 {
						// compare against head
						self.queue.push(InternalEvent::OpenPopup(
							StackablePopupOpen::CompareCommits(
								InspectCommitOpen::new(
									self.list.marked_commits()[0],
								),
							),
						));
						return Ok(EventState::Consumed);
					} else if self.list.marked_count() == 2 {
						//compare two marked commits
						let marked = self.list.marked_commits();
						self.queue.push(InternalEvent::OpenPopup(
							StackablePopupOpen::CompareCommits(
								InspectCommitOpen {
									commit_id: marked[0],
									compare_id: Some(marked[1]),
									tags: None,
								},
							),
						));
						return Ok(EventState::Consumed);
					}
				}
			}
		}

		Ok(EventState::NotConsumed)
	}

	#[allow(clippy::too_many_lines)]
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		let action_commit_available = self.action_commit().is_some()
			&& !self.has_multiple_action_commits();
		if self.visible || force_all {
			self.list.commands(out, force_all);
		}

		out.push(
			CommandInfo::new(
				strings::commands::log_close_search(&self.key_config),
				true,
				(self.visible
					&& (self.can_close_search()
						|| self.is_search_pending()))
					|| force_all,
			)
			.order(order::PRIORITY),
		);

		out.push(CommandInfo::new(
			strings::commands::log_details_toggle(&self.key_config),
			true,
			self.visible,
		));

		out.push(CommandInfo::new(
			strings::commands::commit_details_open(&self.key_config),
			true,
			(self.visible && self.commit_details.is_visible())
				|| force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::open_branch_select_popup(
				&self.key_config,
			),
			true,
			(self.visible && !self.is_search_pending()) || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::compare_with_head(&self.key_config),
			self.list.marked_count() == 1,
			(self.visible
				&& !self.is_search_pending()
				&& self.list.marked_count() <= 1)
				|| force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::compare_commits(&self.key_config),
			true,
			(self.visible
				&& !self.is_search_pending()
				&& self.list.marked_count() == 2)
				|| force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::copy_hash(&self.key_config),
			self.selected_commit().is_some(),
			self.visible || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::log_tag_commit(&self.key_config),
			action_commit_available,
			self.visible || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::log_checkout_commit(&self.key_config),
			action_commit_available,
			self.visible || force_all,
		));
		out.push(CommandInfo::new(
			strings::commands::log_rewrite_from(&self.key_config),
			action_commit_available,
			(self.visible && !self.is_search_pending()) || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::open_tags_popup(&self.key_config),
			true,
			self.visible || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::push_tags(&self.key_config),
			true,
			self.visible || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::inspect_file_tree(&self.key_config),
			self.selected_commit().is_some(),
			(self.visible && !self.is_search_pending()) || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::revert_commit(&self.key_config),
			action_commit_available,
			(self.visible && !self.is_search_pending()) || force_all,
		));

		out.push(CommandInfo::new(
			strings::commands::log_reset_commit(&self.key_config),
			action_commit_available,
			(self.visible && !self.is_search_pending()) || force_all,
		));
		out.push(CommandInfo::new(
			strings::commands::log_reword_commit(&self.key_config),
			action_commit_available,
			(self.visible && !self.is_search_pending()) || force_all,
		));
		out.push(CommandInfo::new(
			strings::commands::log_find_commit(&self.key_config),
			self.can_start_search(),
			self.visible || force_all,
		));

		visibility_blocking(self)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn hide(&mut self) {
		self.visible = false;
		if matches!(self.search, LogSearch::LoadingHistory(_)) {
			self.cancel_search();
		}
		self.git_log.set_background();
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;
		// Keep the commit inspector mounted in the log by default so native
		// diff previews follow the selected row as the cached history grows.
		self.commit_details.show()?;

		self.git_local_branches.spawn(AsyncBranchesJob::new(
			self.repo.borrow().clone(),
			true,
		));

		self.git_remote_branches.spawn(AsyncBranchesJob::new(
			self.repo.borrow().clone(),
			false,
		));

		self.update()?;

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		app::Environment,
		keys::{KeyConfig, Keymap},
		queue::InternalEvent,
	};
	use crossterm::event::{KeyEvent, KeyEventKind};
	use std::{cell::RefCell, rc::Rc};

	#[test]
	fn graph_request_identity_changes_when_branch_metadata_arrives() {
		let head = CommitId::from_str_unchecked(
			"1111111111111111111111111111111111111111",
		)
		.unwrap();
		let tip = CommitId::from_str_unchecked(
			"2222222222222222222222222222222222222222",
		)
		.unwrap();
		let unchanged = GraphRequestKey {
			start: 0,
			len: 54,
			branch_tips: HashSet::new(),
			stashes: HashSet::new(),
			head_id: None,
		};
		let with_branch = GraphRequestKey {
			branch_tips: HashSet::from([tip]),
			head_id: Some(head),
			..unchanged.clone()
		};
		let with_new_head = GraphRequestKey {
			head_id: Some(tip),
			..with_branch.clone()
		};
		let unchanged_again = GraphRequestKey {
			..unchanged.clone()
		};

		assert_eq!(unchanged, unchanged_again);
		assert_ne!(unchanged, with_branch);
		assert_ne!(with_branch, with_new_head);
	}

	fn modal_revlog(
	) -> (tempfile::TempDir, Environment, Revlog, Vec<CommitId>) {
		let (dir, repo) = git2_testing::repo_init();
		let signature = repo.signature().unwrap();
		let mut parent =
			repo.head().unwrap().peel_to_commit().unwrap();
		for message in ["second", "third"] {
			let tree = parent.tree().unwrap();
			let id = repo
				.commit(
					Some("HEAD"),
					&signature,
					&signature,
					message,
					&tree,
					&[&parent],
				)
				.unwrap();
			parent = repo.find_commit(id).unwrap();
		}

		let mut revwalk = repo.revwalk().unwrap();
		revwalk.push_head().unwrap();
		let ids: Vec<_> =
			revwalk.map(|id| CommitId::from(id.unwrap())).collect();
		let mut env = Environment::test_env();
		env.repo = RefCell::new(asyncgit::sync::RepoPath::Path(
			dir.path().to_path_buf(),
		));
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut revlog = Revlog::new(&env);
		revlog.visible = true;
		// Exercise the real asynchronous walk and metadata publication before
		// testing commands against the selected commit.
		let deadline =
			std::time::Instant::now() + Duration::from_secs(5);
		loop {
			revlog.update().unwrap();
			if revlog.list.loaded_commit_count() == ids.len()
				&& revlog.list.selected_entry().is_some()
			{
				break;
			}
			assert!(
				std::time::Instant::now() < deadline,
				"history and metadata did not arrive"
			);
			std::thread::sleep(Duration::from_millis(5));
		}
		(dir, env, revlog, ids)
	}

	fn press(key: &crate::keys::GituiKeyEvent) -> Event {
		let mut event = KeyEvent::from(key);
		event.kind = KeyEventKind::Press;
		Event::Key(event)
	}

	#[test]
	fn reset_key_targets_the_single_mark_after_cursor_moves() {
		let (_dir, env, mut revlog, ids) = modal_revlog();
		let marked = ids[0];
		assert!(revlog.list.selected_entry().is_some());
		revlog
			.list
			.event(&press(&revlog.key_config.keys.select_item))
			.unwrap();
		revlog
			.list
			.event(&press(&revlog.key_config.keys.move_down))
			.unwrap();
		revlog
			.event(&press(&revlog.key_config.keys.log_reset_commit))
			.unwrap();

		match env.queue.pop().unwrap() {
			InternalEvent::OpenResetPopup(id) => {
				assert_eq!(id, marked)
			}
			_ => panic!("reset key did not queue a reset popup"),
		}
	}

	#[test]
	fn main_log_renders_the_selected_commits_inline_patch() {
		let (dir, repo) = git2_testing::repo_init();
		let signature = repo.signature().unwrap();
		let path = std::path::Path::new("main-log-preview.txt");
		for (content, message) in [
			("before main log preview\n", "preview base"),
			("after main log preview\n", "selected inline preview"),
		] {
			std::fs::write(dir.path().join(path), content).unwrap();
			let mut index = repo.index().unwrap();
			index.add_path(path).unwrap();
			let tree_id = index.write_tree().unwrap();
			let tree = repo.find_tree(tree_id).unwrap();
			let parent =
				repo.head().unwrap().peel_to_commit().unwrap();
			repo.commit(
				Some("HEAD"),
				&signature,
				&signature,
				message,
				&tree,
				&[&parent],
			)
			.unwrap();
		}
		let selected =
			CommitId::from(repo.head().unwrap().target().unwrap());
		let mut env = Environment::test_env();
		env.repo = RefCell::new(asyncgit::sync::RepoPath::Path(
			dir.path().to_path_buf(),
		));
		let mut revlog = Revlog::new(&env);
		revlog.show().unwrap();
		let backend = ratatui::backend::TestBackend::new(132, 36);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		let deadline =
			std::time::Instant::now() + Duration::from_secs(5);
		let rendered = loop {
			revlog.update().unwrap();
			terminal
				.draw(|frame| {
					revlog.draw(frame, frame.area()).unwrap();
				})
				.unwrap();
			let rendered: String = terminal
				.backend()
				.buffer()
				.content
				.iter()
				.map(|cell| cell.symbol())
				.collect();
			if revlog.selected_commit() == Some(selected)
				&& rendered.contains("after main log preview")
			{
				break rendered;
			}
			assert!(
				std::time::Instant::now() < deadline,
				"selected commit's inline patch did not arrive: {rendered}"
			);
			std::thread::sleep(Duration::from_millis(5));
		};
		assert!(rendered.contains("after main log preview"));
	}

	#[test]
	fn multiple_modal_marks_reject_mutation_keys_and_disable_palette()
	{
		let (dir, env, mut revlog, _ids) = modal_revlog();
		let repo_path =
			asyncgit::sync::RepoPath::Path(dir.path().to_path_buf());
		let original_head =
			sync::utils::get_head(&repo_path).unwrap();
		revlog
			.list
			.event(&press(&revlog.key_config.keys.select_item))
			.unwrap();
		revlog
			.list
			.event(&press(&revlog.key_config.keys.move_down))
			.unwrap();
		revlog
			.list
			.event(&press(&revlog.key_config.keys.select_item))
			.unwrap();

		let mutation_keys = [
			revlog.key_config.keys.log_reset_commit.clone(),
			revlog.key_config.keys.log_reword_commit.clone(),
			revlog.key_config.keys.log_rewrite_from.clone(),
			revlog.key_config.keys.status_reset_item.clone(),
		];
		for key in &mutation_keys {
			revlog.event(&press(key)).unwrap();
			assert!(matches!(
				env.queue.pop(),
				Some(InternalEvent::ShowErrorMsg(message))
					if message.contains("requires one commit")
			));
		}
		revlog
			.list
			.event(&press(
				&revlog.key_config.keys.log_checkout_commit,
			))
			.unwrap();
		assert!(matches!(
			env.queue.pop(),
			Some(InternalEvent::ShowErrorMsg(message))
				if message.contains("requires one commit")
		));

		let mut commands = Vec::new();
		revlog.commands(&mut commands, true);
		for label in [
			strings::commands::log_checkout_commit(
				&revlog.key_config,
			),
			strings::commands::log_reset_commit(&revlog.key_config),
			strings::commands::log_reword_commit(&revlog.key_config),
			strings::commands::log_rewrite_from(&revlog.key_config),
			strings::commands::revert_commit(&revlog.key_config),
			strings::commands::log_tag_commit(&revlog.key_config),
		] {
			let command = commands
				.iter()
				.find(|command| command.text == label)
				.unwrap();
			assert!(!command.enabled);
		}
		assert_eq!(
			sync::utils::get_head(&repo_path).unwrap(),
			original_head
		);
	}

	#[test]
	fn reset_key_without_marks_keeps_cursor_target() {
		let (_dir, env, mut revlog, ids) = modal_revlog();
		let cursor = ids[1];
		assert!(revlog.list.selected_entry().is_some());
		revlog
			.list
			.event(&press(&revlog.key_config.keys.move_down))
			.unwrap();
		revlog
			.event(&press(&revlog.key_config.keys.log_reset_commit))
			.unwrap();

		match env.queue.pop().unwrap() {
			InternalEvent::OpenResetPopup(id) => {
				assert_eq!(id, cursor)
			}
			_ => panic!("reset key did not queue a reset popup"),
		}
	}

	#[test]
	fn log_fetch_error_is_shown_once() {
		let (dir, env, mut revlog, _) = modal_revlog();
		let git_dir = dir.path().join(".git");
		let hidden_git_dir = dir.path().join(".git-hidden-for-test");
		std::fs::rename(&git_dir, &hidden_git_dir).unwrap();

		// An explicit new range wakes the worker after its repository goes
		// away. The error reaches the UI asynchronously, once per failure.
		revlog.git_log.request_range(3, 128).unwrap();
		let deadline =
			std::time::Instant::now() + Duration::from_secs(5);
		loop {
			revlog.update_git(AsyncGitNotification::Log).unwrap();
			if let Some(InternalEvent::ShowErrorMsg(message)) =
				env.queue.pop()
			{
				assert!(
					message.starts_with("log fetch:"),
					"{message}"
				);
				break;
			}
			assert!(
				std::time::Instant::now() < deadline,
				"background repository failure was not reported"
			);
			std::thread::sleep(Duration::from_millis(5));
		}
		revlog.update_git(AsyncGitNotification::Log).unwrap();
		assert!(env.queue.pop().is_none());
		std::fs::rename(&hidden_git_dir, &git_dir).unwrap();
	}
}
