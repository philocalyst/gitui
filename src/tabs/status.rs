use crate::{
	accessors,
	app::Environment,
	components::{
		command_pump, event_pump, visibility_blocking,
		ChangesComponent, CommandBlocking, CommandInfo, Component,
		DiffComponent, DiffPreviewController, DrawableComponent,
		EventState, FileTreeItemKind, SelectionStatus,
	},
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	queue::{Action, InternalEvent, NeedsUpdate, Queue, ResetItem},
	strings, try_or_popup,
	ui::style::{SharedTheme, Theme},
};
use anyhow::Result;
use asyncgit::{
	asyncjob::{AsyncJob, AsyncSingleJob, RunParams},
	cached,
	sync::{
		self, status::StatusType, RepoPath, RepoPathRef, RepoState,
	},
	sync::{BranchCompare, CommitId},
	AsyncDiff, AsyncGitNotification, AsyncStatus, DiffParams,
	DiffType, PushType, StatusItem, StatusParams,
};
use crossterm::event::Event;
use itertools::Itertools;
use ratatui::{
	layout::{Alignment, Constraint, Direction, Layout, Rect},
	style::{Color, Style},
	widgets::{Block, BorderType, Borders, Paragraph},
};
use std::{
	cell::Cell,
	sync::{Arc, Mutex},
};

enum StatusMetadataState {
	Request {
		repo: RepoPath,
		branch: Option<String>,
		generation: u64,
	},
	Response(u64, StatusMetadata),
}

#[derive(Clone, Default)]
struct StatusMetadataJob {
	state: Arc<Mutex<Option<StatusMetadataState>>>,
}

#[derive(Default)]
struct StatusMetadata {
	remotes: RemoteStatus,
	branch: Option<BranchCompare>,
}

impl StatusMetadataJob {
	fn new(
		repo: RepoPath,
		branch: Option<String>,
		generation: u64,
	) -> Self {
		Self {
			state: Arc::new(Mutex::new(Some(
				StatusMetadataState::Request {
					repo,
					branch,
					generation,
				},
			))),
		}
	}

	fn result(&self) -> Option<(u64, StatusMetadata)> {
		let mut state = self.state.lock().ok()?;
		let result = match state.take()? {
			StatusMetadataState::Request {
				repo,
				branch,
				generation,
			} => {
				*state = Some(StatusMetadataState::Request {
					repo,
					branch,
					generation,
				});
				None
			}
			StatusMetadataState::Response(generation, metadata) => {
				Some((generation, metadata))
			}
		};
		drop(state);
		result
	}
}

impl AsyncJob for StatusMetadataJob {
	type Notification = AsyncGitNotification;
	type Progress = ();

	fn run(
		&mut self,
		_params: RunParams<Self::Notification, Self::Progress>,
	) -> asyncgit::Result<Self::Notification> {
		let request = self
			.state
			.lock()
			.map_err(|_| {
				asyncgit::Error::Generic(String::from(
					"status metadata lock poisoned",
				))
			})?
			.take();
		let Some(StatusMetadataState::Request {
			repo,
			branch,
			generation,
		}) = request
		else {
			return Ok(AsyncGitNotification::StatusMetadata);
		};

		let metadata = StatusMetadata {
			branch: branch.as_deref().and_then(|name| {
				sync::branch_compare_upstream(&repo, name).ok()
			}),
			remotes: RemoteStatus {
				has_remote_for_fetch:
					sync::get_default_remote_for_fetch(&repo).is_ok(),
				has_remote_for_push:
					sync::get_default_remote_for_push(&repo).is_ok(),
			},
		};

		if let Ok(mut state) = self.state.lock() {
			*state = Some(StatusMetadataState::Response(
				generation, metadata,
			));
		}
		Ok(AsyncGitNotification::StatusMetadata)
	}
}

/// what part of the screen is focused
#[derive(Debug, PartialEq, Copy, Clone)]
enum Focus {
	WorkDir,
	Diff,
	Stage,
}

/// focus can toggle between workdir and stage
impl Focus {
	const fn toggled_focus(self) -> Self {
		match self {
			Self::WorkDir => Self::Stage,
			Self::Stage => Self::WorkDir,
			Self::Diff => Self::Diff,
		}
	}

	const fn next(self) -> Self {
		match self {
			Self::WorkDir => Self::Stage,
			Self::Stage => Self::Diff,
			Self::Diff => Self::WorkDir,
		}
	}

	const fn previous(self) -> Self {
		match self {
			Self::WorkDir => Self::Diff,
			Self::Stage => Self::WorkDir,
			Self::Diff => Self::Stage,
		}
	}
}

/// which target are we showing a diff against
#[derive(PartialEq, Copy, Clone)]
enum DiffTarget {
	Stage,
	WorkingDir,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DiffLayout {
	#[default]
	Normal,
	Zoomed,
}

impl DiffLayout {
	const fn is_zoomed(self) -> bool {
		matches!(self, Self::Zoomed)
	}

	const fn toggle(self) -> Self {
		match self {
			Self::Normal => Self::Zoomed,
			Self::Zoomed => Self::Normal,
		}
	}
}

#[derive(Default)]
struct RemoteStatus {
	has_remote_for_fetch: bool,
	has_remote_for_push: bool,
}

pub struct Status {
	repo: RepoPathRef,
	visible: bool,
	focus: Focus,
	diff_target: DiffTarget,
	index: ChangesComponent,
	index_wd: ChangesComponent,
	diff: DiffComponent,
	remotes: RemoteStatus,
	git_diff: AsyncDiff,
	git_state: RepoState,
	git_status_workdir: AsyncStatus,
	git_status_stage: AsyncStatus,
	workdir_display_revision: u64,
	stage_display_revision: u64,
	git_branch_state: Option<BranchCompare>,
	git_branch_name: cached::BranchName,
	metadata_job: AsyncSingleJob<StatusMetadataJob>,
	metadata_generation: Cell<u64>,
	metadata_requested_generation: Cell<Option<u64>>,
	metadata_result_generation: Cell<Option<u64>>,
	queue: Queue,
	git_action_executed: bool,
	options: SharedOptions,
	theme: SharedTheme,
	preview: DiffPreviewController,
	review_mode: bool,
	zoomed: DiffLayout,
	key_config: SharedKeyConfig,
	workdir_area: std::cell::Cell<Rect>,
	stage_area: std::cell::Cell<Rect>,
	diff_area: std::cell::Cell<Rect>,
	mouse_capture: Option<Focus>,
}

impl DrawableComponent for Status {
	fn draw(
		&self,
		f: &mut ratatui::Frame,
		rect: ratatui::layout::Rect,
	) -> Result<()> {
		let repo_unclean = self.repo_state_unclean();
		let rects = if repo_unclean {
			Layout::default()
				.direction(Direction::Vertical)
				.constraints(
					[Constraint::Min(1), Constraint::Length(3)]
						.as_ref(),
				)
				.split(rect)
		} else {
			std::rc::Rc::new([rect])
		};

		let chunks = Layout::default()
			.direction(Direction::Horizontal)
			.constraints(
				if self.zoomed.is_zoomed()
					|| (self.focus == Focus::Diff
						&& !self.key_config.is_modal())
				{
					[
						Constraint::Percentage(0),
						Constraint::Percentage(100),
					]
				} else {
					[
						Constraint::Percentage(50),
						Constraint::Percentage(50),
					]
				}
				.as_ref(),
			)
			.split(rects[0]);

		let left_chunks = Layout::default()
			.direction(Direction::Vertical)
			.constraints(
				if self.diff_target == DiffTarget::WorkingDir {
					[
						Constraint::Percentage(60),
						Constraint::Percentage(40),
					]
				} else {
					[
						Constraint::Percentage(40),
						Constraint::Percentage(60),
					]
				}
				.as_ref(),
			)
			.split(chunks[0]);
		// The two file trees are always rendered in this order. Changing the
		// diff target changes only their heights, never their pane identity.
		self.workdir_area.set(left_chunks[0]);
		self.stage_area.set(left_chunks[1]);
		self.diff_area.set(chunks[1]);

		if !self.zoomed.is_zoomed() {
			self.index_wd.draw(f, left_chunks[0])?;
			self.index.draw(f, left_chunks[1])?;
		}
		if self.review_mode {
			self.preview.view().draw(
				f,
				chunks[1],
				&self.theme,
				self.focus == Focus::Diff,
			);
		} else {
			self.diff.draw(f, chunks[1])?;
		}
		self.draw_branch_state(f, &left_chunks);

		if repo_unclean {
			self.draw_repo_state(f, rects[1]);
		}

		Ok(())
	}
}

impl Status {
	fn push_diff_view_commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) {
		let focused = self.focus == Focus::Diff || force_all;
		out.push(CommandInfo::new(
			strings::commands::diff_view_toggle(&self.key_config),
			true,
			focused,
		));
		out.push(CommandInfo::new(
			strings::commands::diff_zoom_toggle(&self.key_config),
			true,
			focused,
		));
	}

	accessors!(self, [index, index_wd, diff]);

	///
	pub fn new(env: &Environment) -> Self {
		let repo_clone = env.repo.borrow().clone();
		Self {
			queue: env.queue.clone(),
			workdir_area: std::cell::Cell::new(Rect::default()),
			stage_area: std::cell::Cell::new(Rect::default()),
			diff_area: std::cell::Cell::new(Rect::default()),
			mouse_capture: None,
			visible: true,
			remotes: RemoteStatus {
				has_remote_for_fetch: false,
				has_remote_for_push: false,
			},
			git_state: RepoState::Clean,
			focus: Focus::WorkDir,
			diff_target: DiffTarget::WorkingDir,
			index_wd: ChangesComponent::new(
				env,
				&strings::title_status(&env.key_config),
				true,
				true,
			),
			index: ChangesComponent::new(
				env,
				&strings::title_index(&env.key_config),
				false,
				false,
			),
			diff: DiffComponent::new(env, false),
			git_diff: AsyncDiff::new(
				repo_clone.clone(),
				&env.sender_git,
			),
			git_status_workdir: AsyncStatus::new(
				repo_clone.clone(),
				env.sender_git.clone(),
			),
			git_status_stage: AsyncStatus::new(
				repo_clone,
				env.sender_git.clone(),
			),
			workdir_display_revision: 0,
			stage_display_revision: 0,
			git_action_executed: false,
			git_branch_state: None,
			git_branch_name: cached::BranchName::new(
				env.repo.clone(),
			),
			metadata_job: AsyncSingleJob::new(env.sender_git.clone()),
			metadata_generation: Cell::new(0),
			metadata_requested_generation: Cell::new(None),
			metadata_result_generation: Cell::new(None),
			key_config: env.key_config.clone(),
			options: env.options.clone(),
			theme: env.theme.clone(),
			preview: DiffPreviewController::new(
				env.repo.clone(),
				env.sender_git.clone(),
				env.key_config.clone(),
			),
			review_mode: false,
			zoomed: DiffLayout::Normal,
			repo: env.repo.clone(),
		}
	}

	fn draw_branch_state(
		&self,
		f: &mut ratatui::Frame,
		chunks: &[ratatui::layout::Rect],
	) {
		if let Some(branch_name) = self.git_branch_name.last() {
			let ahead_behind =
				(self.metadata_result_generation.get()
					== Some(self.metadata_generation.get()))
				.then_some(self.git_branch_state.as_ref())
				.flatten()
				.map_or_else(String::new, |state| {
					format!(
						"\u{2191}{} \u{2193}{} ",
						state.ahead, state.behind,
					)
				});

			let w = Paragraph::new(format!(
				"{ahead_behind}{{{branch_name}}}"
			))
			.alignment(Alignment::Right);

			let mut rect = if self.index_wd.focused() {
				let mut rect = chunks[0];
				rect.y += rect.height.saturating_sub(1);
				rect
			} else {
				chunks[1]
			};

			rect.x += 1;
			rect.width = rect.width.saturating_sub(2);
			rect.height = rect
				.height
				.saturating_sub(rect.height.saturating_sub(1));

			f.render_widget(w, rect);
		}
	}

	fn repo_state_text(repo: &RepoPath, state: &RepoState) -> String {
		match state {
			RepoState::Merge => {
				let ids =
					sync::mergehead_ids(repo).unwrap_or_default();

				format!(
					"Commits: {}",
					ids.iter()
						.map(sync::CommitId::get_short_string)
						.join(",")
				)
			}
			RepoState::Rebase => sync::rebase_progress(repo)
				.map_or_else(
					|_| String::new(),
					|p| {
						format!(
							"Step: {}/{} Current Commit: {}",
							p.current + 1,
							p.steps,
							p.current_commit
								.as_ref()
								.map(CommitId::get_short_string)
								.unwrap_or_default(),
						)
					},
				),
			RepoState::Revert => {
				format!(
					"Revert {}",
					sync::revert_head(repo)
						.ok()
						.as_ref()
						.map(CommitId::get_short_string)
						.unwrap_or_default(),
				)
			}
			_ => format!("{state:?}"),
		}
	}

	fn draw_repo_state(
		&self,
		f: &mut ratatui::Frame,
		r: ratatui::layout::Rect,
	) {
		if self.git_state != RepoState::Clean {
			let txt = Self::repo_state_text(
				&self.repo.borrow(),
				&self.git_state,
			);

			let w = Paragraph::new(txt)
				.block(
					Block::default()
						.border_type(BorderType::Plain)
						.borders(Borders::all())
						.border_style(Theme::attention_block())
						.title(format!(
							"Pending {:?}",
							self.git_state
						)),
				)
				.style(Style::default().fg(Color::Red))
				.alignment(Alignment::Left);

			f.render_widget(w, r);
		}
	}

	fn repo_state_unclean(&self) -> bool {
		self.git_state != RepoState::Clean
	}

	fn can_focus_diff(&self) -> bool {
		match self.focus {
			Focus::WorkDir => self.index_wd.is_file_selected(),
			Focus::Stage => self.index.is_file_selected(),
			Focus::Diff => false,
		}
	}

	fn is_focus_on_diff(&self) -> bool {
		self.focus == Focus::Diff
	}

	fn switch_focus(&mut self, f: Focus) -> Result<bool> {
		if self.focus != f {
			if f != Focus::Diff {
				self.zoomed = DiffLayout::Normal;
			}
			self.focus = f;

			match self.focus {
				Focus::WorkDir => {
					self.set_diff_target(DiffTarget::WorkingDir);
					self.diff.focus(false);
				}
				Focus::Stage => {
					self.set_diff_target(DiffTarget::Stage);
					self.diff.focus(false);
				}
				Focus::Diff => {
					self.index.focus(false);
					self.index_wd.focus(false);

					self.diff.focus(true);
				}
			}

			self.update_diff()?;

			return Ok(true);
		}

		Ok(false)
	}

	fn set_diff_target(&mut self, target: DiffTarget) {
		self.diff_target = target;
		let is_stage = self.diff_target == DiffTarget::Stage;

		self.index_wd.focus_select(!is_stage);
		self.index.focus_select(is_stage);
	}

	pub fn selected_path(&self) -> Option<(String, bool)> {
		let (idx, is_stage) = match self.diff_target {
			DiffTarget::Stage => (&self.index, true),
			DiffTarget::WorkingDir => (&self.index_wd, false),
		};

		if let Some(item) = idx.selection() {
			if let FileTreeItemKind::File(i) = item.kind {
				return Some((i.path, is_stage));
			}
		}
		None
	}

	///
	pub fn update(&mut self) -> Result<()> {
		let branch = self.git_branch_name.lookup().ok();
		self.apply_metadata_result();

		if self.is_visible() {
			self.schedule_metadata(branch);
			let config =
				self.options.borrow().status_show_untracked();

			self.git_diff.refresh()?;
			self.git_status_workdir.fetch(&StatusParams::new(
				StatusType::WorkingDir,
				config,
			))?;
			self.git_status_stage.fetch(&StatusParams::new(
				StatusType::Stage,
				config,
			))?;

			self.git_state = sync::repo_state(&self.repo.borrow())
				.unwrap_or(RepoState::Clean);
		}

		Ok(())
	}

	/// Mark both status snapshots stale after a repository change.
	pub fn invalidate_status(&self) {
		self.git_status_workdir.invalidate();
		self.git_status_stage.invalidate();
		self.metadata_generation
			.set(self.metadata_generation.get().wrapping_add(1));
	}

	/// Schedule an idle-only refresh for the next update pass.
	pub fn poll_status(&self) {
		self.git_status_workdir.invalidate_if_idle();
		self.git_status_stage.invalidate_if_idle();
		if self.is_visible() && !self.metadata_job.is_pending() {
			self.metadata_requested_generation.set(None);
		}
	}

	///
	pub fn anything_pending(&self) -> bool {
		self.git_diff.is_pending()
			|| self.preview.is_pending()
			|| self.git_status_stage.is_pending()
			|| self.git_status_workdir.is_pending()
	}

	fn schedule_metadata(&self, branch: Option<String>) {
		let generation = self.metadata_generation.get();
		if self.metadata_requested_generation.get()
			== Some(generation)
		{
			return;
		}
		self.metadata_requested_generation.set(Some(generation));
		self.metadata_job.spawn(StatusMetadataJob::new(
			self.repo.borrow().clone(),
			branch,
			generation,
		));
	}

	fn apply_metadata_result(&mut self) {
		if let Some(job) = self.metadata_job.take_last() {
			match job.result() {
				Some((generation, metadata))
					if generation
						== self.metadata_generation.get() =>
				{
					self.remotes = metadata.remotes;
					self.git_branch_state = metadata.branch;
					self.metadata_result_generation
						.set(Some(generation));
				}
				Some(_) | None => (),
			}
		}
	}

	///
	pub fn update_git(
		&mut self,
		ev: AsyncGitNotification,
	) -> Result<()> {
		if ev == AsyncGitNotification::DiffPreview {
			self.preview.update();
			self.diff.update_preview();
		}
		if ev == AsyncGitNotification::StatusMetadata {
			self.apply_metadata_result();
			if self.is_visible() {
				self.schedule_metadata(self.git_branch_name.last());
			}
		}
		if !self.is_visible() {
			return Ok(());
		}

		match ev {
			AsyncGitNotification::Diff => self.update_diff()?,
			AsyncGitNotification::Status => {
				self.update_status()?;
				let config =
					self.options.borrow().status_show_untracked();
				self.git_status_workdir.fetch(&StatusParams::new(
					StatusType::WorkingDir,
					config,
				))?;
				self.git_status_stage.fetch(&StatusParams::new(
					StatusType::Stage,
					config,
				))?;
			}
			AsyncGitNotification::Branches
			| AsyncGitNotification::Push
			| AsyncGitNotification::Pull
			| AsyncGitNotification::CommitFiles => {
				self.metadata_generation.set(
					self.metadata_generation.get().wrapping_add(1),
				);
				self.schedule_metadata(self.git_branch_name.last());
			}
			AsyncGitNotification::DiffPreview => {}
			_ => (),
		}

		Ok(())
	}

	/// Apply completed native diff projection before the redraw.
	pub fn update_rendering(&mut self) {
		self.diff.update_rendering();
	}

	pub fn get_files_changes(&self) -> Result<Vec<StatusItem>> {
		if self.git_status_stage.is_pending()
			|| self.git_status_stage.is_dirty()
		{
			anyhow::bail!(
				"repository status is still being refreshed"
			);
		}
		Ok(self.git_status_stage.complete()?.items)
	}

	fn update_status(&mut self) -> Result<()> {
		let stage_changed = if let Some((revision, stage_status)) =
			self.git_status_stage
				.last_if_changed(self.stage_display_revision)?
		{
			self.index.set_items(&stage_status.items)?;
			self.stage_display_revision = revision;
			true
		} else {
			false
		};
		self.index.set_pending(
			self.git_status_stage.is_pending()
				|| self.git_status_stage.is_dirty(),
		);

		let workdir_changed =
			if let Some((revision, workdir_status)) = self
				.git_status_workdir
				.last_if_changed(self.workdir_display_revision)?
			{
				self.index_wd.set_items(&workdir_status.items)?;
				self.workdir_display_revision = revision;
				true
			} else {
				false
			};
		self.index_wd.set_pending(
			self.git_status_workdir.is_pending()
				|| self.git_status_workdir.is_dirty(),
		);

		if stage_changed || workdir_changed {
			self.update_diff()?;
		}

		if self.git_action_executed
			&& !self.git_status_stage.is_pending()
			&& !self.git_status_stage.is_dirty()
			&& !self.git_status_workdir.is_pending()
			&& !self.git_status_workdir.is_dirty()
		{
			self.git_action_executed = false;

			if self.focus == Focus::WorkDir
				&& self.index_wd.is_empty()
				&& !self.index.is_empty()
			{
				self.switch_focus(Focus::Stage)?;
			} else if self.focus == Focus::Stage
				&& self.index.is_empty()
			{
				self.switch_focus(Focus::WorkDir)?;
			}
		}

		Ok(())
	}

	///
	pub fn update_diff(&mut self) -> Result<()> {
		if let Some((path, is_stage)) = self.selected_path() {
			let diff_type = if is_stage {
				DiffType::Stage
			} else {
				DiffType::WorkDir
			};

			let diff_params = DiffParams {
				path: path.clone(),
				diff_type,
				options: self.options.borrow().diff_options(),
			};
			if self.diff.current() == (path.clone(), is_stage) {
				// we are already showing a diff of the right file
				// maybe the diff changed (outside file change)
				if let Some((params, last)) = self.git_diff.last()? {
					if params == diff_params {
						// all params match, so we might need to update
						self.diff.update_with_params(
							path,
							is_stage,
							last.clone(),
							Some(diff_params.clone()),
						);
						self.request_preview(diff_params, &last);
					} else {
						// params changed, we need to request the right diff
						self.request_diff(
							diff_params,
							path,
							is_stage,
						)?;
					}
				}
			} else {
				// we dont show the right diff right now, so we need to request
				self.request_diff(diff_params, path, is_stage)?;
			}
		} else {
			self.clear_diff(false);
		}

		Ok(())
	}

	fn request_diff(
		&mut self,
		diff_params: DiffParams,
		path: String,
		is_stage: bool,
	) -> Result<(), anyhow::Error> {
		if let Some(diff) =
			self.git_diff.request(diff_params.clone())?
		{
			self.diff.update_with_params(
				path,
				is_stage,
				diff.clone(),
				Some(diff_params.clone()),
			);
			self.request_preview(diff_params, &diff);
		} else {
			self.clear_diff(true);
		}

		Ok(())
	}

	fn clear_diff(&mut self, pending: bool) {
		self.diff.clear(pending);
		if pending && self.review_mode {
			self.preview.clear_with_message("Loading diff preview…");
		} else {
			self.preview.clear();
		}
	}

	fn request_preview(
		&mut self,
		params: DiffParams,
		native_diff: &asyncgit::FileDiff,
	) {
		if !self.review_mode {
			return;
		}
		let preference =
			self.options.borrow().diff_preview_preference().into();
		let width =
			self.diff_area.get().width.saturating_sub(2) as usize;
		self.preview.request(
			params,
			width.max(20),
			preference,
			asyncgit::hash(native_diff),
		);
	}

	/// called after confirmation
	pub fn reset(&self, item: &ResetItem) -> bool {
		if let Err(e) = sync::reset_workdir_path(
			&self.repo.borrow(),
			item.path.as_str(),
		) {
			self.queue.push(InternalEvent::ShowErrorMsg(format!(
				"reset failed:\n{e}"
			)));

			false
		} else {
			true
		}
	}

	pub fn last_file_moved(&mut self) -> Result<()> {
		if !self.is_focus_on_diff() && self.is_visible() {
			self.switch_focus(self.focus.toggled_focus())?;
		}
		Ok(())
	}

	fn push(&self, force: bool) {
		if self.can_push() {
			if let Some(branch) = self.git_branch_name.last() {
				if force {
					self.queue.push(InternalEvent::ConfirmAction(
						Action::ForcePush(branch, force),
					));
				} else {
					self.queue.push(InternalEvent::Push(
						branch,
						PushType::Branch,
						force,
						false,
					));
				}
			}
		}
	}

	fn fetch(&self) {
		if self.can_fetch() {
			self.queue.push(InternalEvent::FetchRemotes);
		}
	}

	fn pull(&self) {
		if self.can_fetch() {
			if let Some(branch) = self.git_branch_name.last() {
				self.queue.push(InternalEvent::Pull(branch));
			}
		}
	}

	fn undo_last_commit(&self) {
		self.queue
			.push(InternalEvent::ConfirmAction(Action::UndoCommit));
	}

	fn can_push(&self) -> bool {
		if self.metadata_result_generation.get()
			!= Some(self.metadata_generation.get())
		{
			return false;
		}
		let is_ahead = self
			.git_branch_state
			.as_ref()
			.is_none_or(|state| state.ahead > 0);

		is_ahead && self.remotes.has_remote_for_push
	}

	fn can_fetch(&self) -> bool {
		self.metadata_result_generation.get()
			== Some(self.metadata_generation.get())
			&& self.remotes.has_remote_for_fetch
			&& self.git_branch_state.is_some()
	}

	fn can_abort_merge(&self) -> bool {
		self.git_state == RepoState::Merge
	}

	fn pending_rebase(&self) -> bool {
		self.git_state == RepoState::Rebase
	}

	fn pending_revert(&self) -> bool {
		self.git_state == RepoState::Revert
	}

	pub fn revert_pending_state(&self) {
		try_or_popup!(
			self,
			"revert pending state",
			sync::abort_pending_state(&self.repo.borrow())
		);
	}

	fn commands_nav(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) {
		let focus_on_diff = self.is_focus_on_diff();
		out.push(
			CommandInfo::new(
				strings::commands::close_popup(&self.key_config),
				true,
				(self.visible && focus_on_diff) || force_all,
			)
			.order(strings::order::NAV),
		);
		out.push(
			CommandInfo::new(
				strings::commands::diff_focus_right(&self.key_config),
				self.can_focus_diff(),
				(self.visible && !focus_on_diff) || force_all,
			)
			.order(strings::order::NAV),
		);
		out.push(
			CommandInfo::new(
				strings::commands::select_staging(&self.key_config),
				!focus_on_diff,
				(self.visible
					&& !focus_on_diff
					&& self.focus == Focus::WorkDir)
					|| force_all,
			)
			.order(strings::order::NAV),
		);
		out.push(
			CommandInfo::new(
				strings::commands::select_unstaged(&self.key_config),
				!focus_on_diff,
				(self.visible
					&& !focus_on_diff
					&& self.focus == Focus::Stage)
					|| force_all,
			)
			.order(strings::order::NAV),
		);
	}

	fn can_commit(&self) -> bool {
		(self.key_config.is_modal() || self.index.focused())
			&& !self.index.is_empty()
			&& !self.git_status_stage.is_pending()
			&& !self.git_status_stage.is_dirty()
			&& !self.pending_rebase()
	}
}

impl Component for Status {
	fn selection_status(&self) -> Option<SelectionStatus> {
		if !self.visible || !self.key_config.is_modal() {
			return None;
		}
		match self.focus {
			Focus::WorkDir => self.index_wd.selection_status(),
			Focus::Stage => self.index.selection_status(),
			Focus::Diff if self.review_mode => None,
			Focus::Diff => self.diff.selection_status(),
		}
	}
	fn modal_status_hint(&self) -> Option<String> {
		(self.visible
			&& self.focus == Focus::Diff
			&& self.review_mode)
			.then(|| {
				format!(
					"↑/↓ Scroll  h/l Pan  {} Zoom  {} Patch  Esc Cancel",
					self.key_config
						.get_hint(self.key_config.keys.diff_zoom_toggle),
					self.key_config
						.get_hint(self.key_config.keys.diff_view_toggle),
				)
			})
	}

	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		let focus_on_diff = self.is_focus_on_diff();

		if self.visible || force_all {
			if self.review_mode {
				let components: [&dyn Component; 2] =
					[&self.index, &self.index_wd];
				command_pump(out, force_all, &components);
			} else {
				command_pump(
					out,
					force_all,
					self.components().as_slice(),
				);
			}
			self.push_diff_view_commands(out, force_all);

			out.push(
				CommandInfo::new(
					strings::commands::commit_open(&self.key_config),
					true,
					self.can_commit() || force_all,
				)
				.order(-1),
			);
			out.push(
				CommandInfo::new(
					strings::commands::commit_open_inline(
						&self.key_config,
					),
					true,
					self.can_commit() || force_all,
				)
				.order(-1),
			);

			out.push(CommandInfo::new(
				strings::commands::open_branch_select_popup(
					&self.key_config,
				),
				true,
				!focus_on_diff,
			));

			out.push(CommandInfo::new(
				strings::commands::status_push(&self.key_config),
				self.can_push(),
				!focus_on_diff,
			));
			out.push(CommandInfo::new(
				strings::commands::status_force_push(
					&self.key_config,
				),
				true,
				self.can_push() && !focus_on_diff,
			));

			out.push(CommandInfo::new(
				strings::commands::status_fetch(&self.key_config),
				self.can_fetch(),
				!focus_on_diff,
			));
			out.push(CommandInfo::new(
				strings::commands::status_pull(&self.key_config),
				self.can_fetch(),
				!focus_on_diff,
			));

			out.push(CommandInfo::new(
				strings::commands::undo_commit(&self.key_config),
				true,
				(!self.pending_rebase() && !focus_on_diff)
					|| force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::abort_merge(&self.key_config),
				true,
				self.can_abort_merge() || force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::continue_rebase(&self.key_config),
				true,
				self.pending_rebase() || force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::abort_rebase(&self.key_config),
				true,
				self.pending_rebase() || force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::abort_revert(&self.key_config),
				true,
				self.pending_revert() || force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::view_submodules(&self.key_config),
				true,
				true,
			));
		}

		self.commands_nav(out, force_all);

		visibility_blocking(self)
	}

	#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
	fn event(
		&mut self,
		ev: &crossterm::event::Event,
	) -> Result<EventState> {
		if self.visible {
			if let Event::Key(key) = ev {
				if self.zoomed.is_zoomed()
					&& key_match(key, self.key_config.keys.exit_popup)
				{
					self.zoomed = DiffLayout::Normal;
					return Ok(EventState::Consumed);
				}
				if self.focus == Focus::Diff
					&& key_match(
						key,
						self.key_config.keys.diff_zoom_toggle,
					) {
					self.zoomed = self.zoomed.toggle();
					return Ok(EventState::Consumed);
				}
				if key_match(
					key,
					self.key_config.keys.diff_view_toggle,
				) && self.focus == Focus::Diff
				{
					self.review_mode = !self.review_mode;
					if self.review_mode {
						self.update_diff()?;
					} else {
						self.preview.clear();
					}
					return Ok(EventState::Consumed);
				}
			}
			if let Event::Mouse(mouse) = ev {
				let pane = match mouse.kind {
					crossterm::event::MouseEventKind::Drag(_)
					| crossterm::event::MouseEventKind::Up(_) => self.mouse_capture,
					crossterm::event::MouseEventKind::Down(
						crossterm::event::MouseButton::Left,
					)
					| crossterm::event::MouseEventKind::ScrollUp
					| crossterm::event::MouseEventKind::ScrollDown => {
						let position =
							(mouse.column, mouse.row).into();
						[
							(self.workdir_area.get(), Focus::WorkDir),
							(self.stage_area.get(), Focus::Stage),
							(self.diff_area.get(), Focus::Diff),
						]
						.into_iter()
						.find_map(|(area, focus)| {
							area.contains(position).then_some(focus)
						})
					}
					_ => None,
				};
				if let Some(pane) = pane {
					self.switch_focus(pane)?;
					if matches!(
						mouse.kind,
						crossterm::event::MouseEventKind::Down(
							crossterm::event::MouseButton::Left
						)
					) {
						self.mouse_capture = Some(pane);
					}
				}
				if matches!(
					mouse.kind,
					crossterm::event::MouseEventKind::Up(_)
				) {
					self.mouse_capture = None;
				}
			}

			// Bulk stage-all operates on the complete repository snapshot.
			// Consume the binding while either status scan is pending or dirty
			// so it cannot act on an older partial view.
			if let Event::Key(key) = ev {
				if key_match(
					key,
					self.key_config.keys.status_stage_all,
				) && (self.git_status_stage.is_pending()
					|| self.git_status_stage.is_dirty()
					|| self.git_status_workdir.is_pending()
					|| self.git_status_workdir.is_dirty())
				{
					return Ok(EventState::Consumed);
				}
			}

			// Modal focus keys belong to the tab. Handle them before children so
			// a focused tree or diff cannot consume them as navigation.
			if self.key_config.is_modal() {
				if let Event::Key(k) = ev {
					// Commit shortcuts belong to the Status tab, even while a
					// file list or diff owns focus. The modal profile opens the
					// configured editor directly; Shift+C remains the inline form.
					if self.can_commit()
						&& key_match(
							k,
							self.key_config.keys.open_commit,
						) {
						self.queue
							.push(InternalEvent::OpenCommitEditor);
						return Ok(EventState::Consumed);
					}
					if self.can_commit()
						&& key_match(
							k,
							self.key_config.keys.open_commit_inline,
						) {
						self.queue
							.push(InternalEvent::OpenCommitInline);
						return Ok(EventState::Consumed);
					}

					let next = if key_match(
						k,
						self.key_config.keys.focus_next,
					) {
						Some(self.focus.next())
					} else if key_match(
						k,
						self.key_config.keys.focus_previous,
					) {
						Some(self.focus.previous())
					} else if key_match(
						k,
						self.key_config.keys.focus_left,
					) {
						match self.focus {
							Focus::Diff => {
								Some(match self.diff_target {
									DiffTarget::Stage => Focus::Stage,
									DiffTarget::WorkingDir => {
										Focus::WorkDir
									}
								})
							}
							_ => None,
						}
					} else if key_match(
						k,
						self.key_config.keys.focus_right,
					) {
						(self.focus != Focus::Diff
							&& self.can_focus_diff())
						.then_some(Focus::Diff)
					} else if key_match(
						k,
						self.key_config.keys.focus_up,
					) {
						(self.focus == Focus::Stage
							&& !self.index_wd.is_empty())
						.then_some(Focus::WorkDir)
					} else if key_match(
						k,
						self.key_config.keys.focus_down,
					) {
						(self.focus == Focus::WorkDir
							&& !self.index.is_empty())
						.then_some(Focus::Stage)
					} else {
						None
					};
					if let Some(focus) = next {
						return self
							.switch_focus(focus)
							.map(Into::into);
					}
				}
			}

			let child_consumed = if self.review_mode
				&& self.focus == Focus::Diff
			{
				self.preview.view_mut().event(ev)?.is_consumed()
			} else {
				event_pump(ev, self.components_mut().as_mut_slice())?
					.is_consumed()
			};
			if child_consumed {
				self.git_action_executed = true;
				return Ok(EventState::Consumed);
			}

			if let Event::Key(k) = ev {
				return if key_match(
					k,
					self.key_config.keys.open_commit_inline,
				) && self.can_commit()
				{
					self.queue.push(InternalEvent::OpenCommitInline);
					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.open_commit,
				) && self.can_commit()
				{
					self.queue.push(InternalEvent::OpenCommit);
					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.toggle_workarea,
				) && !self.is_focus_on_diff()
				{
					self.switch_focus(self.focus.toggled_focus())
						.map(Into::into)
				} else if key_match(
					k,
					self.key_config.keys.move_right,
				) && self.can_focus_diff()
				{
					self.switch_focus(Focus::Diff).map(Into::into)
				} else if key_match(
					k,
					self.key_config.keys.exit_popup,
				) {
					self.switch_focus(match self.diff_target {
						DiffTarget::Stage => Focus::Stage,
						DiffTarget::WorkingDir => Focus::WorkDir,
					})
					.map(Into::into)
				} else if !self.key_config.is_modal()
					&& key_match(k, self.key_config.keys.move_down)
					&& self.focus == Focus::WorkDir
					&& !self.index.is_empty()
				{
					self.switch_focus(Focus::Stage).map(Into::into)
				} else if !self.key_config.is_modal()
					&& key_match(k, self.key_config.keys.move_up)
					&& self.focus == Focus::Stage
					&& !self.index_wd.is_empty()
				{
					self.switch_focus(Focus::WorkDir).map(Into::into)
				} else if key_match(
					k,
					self.key_config.keys.select_branch,
				) && !self.is_focus_on_diff()
				{
					self.queue.push(InternalEvent::SelectBranch);
					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.force_push,
				) && !self.is_focus_on_diff()
					&& self.can_push()
				{
					self.push(true);
					Ok(EventState::Consumed)
				} else if key_match(k, self.key_config.keys.push)
					&& !self.is_focus_on_diff()
				{
					self.push(false);
					Ok(EventState::Consumed)
				} else if key_match(k, self.key_config.keys.fetch)
					&& !self.is_focus_on_diff()
					&& self.can_fetch()
				{
					self.fetch();
					Ok(EventState::Consumed)
				} else if key_match(k, self.key_config.keys.pull)
					&& !self.is_focus_on_diff()
					&& self.can_fetch()
				{
					self.pull();
					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.undo_commit,
				) && !self.is_focus_on_diff()
				{
					self.undo_last_commit();
					self.queue.push(InternalEvent::Update(
						NeedsUpdate::ALL,
					));
					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.abort_merge,
				) {
					if self.can_abort_merge() {
						self.queue.push(
							InternalEvent::ConfirmAction(
								Action::AbortMerge,
							),
						);
					} else if self.pending_rebase() {
						self.queue.push(
							InternalEvent::ConfirmAction(
								Action::AbortRebase,
							),
						);
					} else if self.pending_revert() {
						self.queue.push(
							InternalEvent::ConfirmAction(
								Action::AbortRevert,
							),
						);
					}

					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.rebase_branch,
				) && self.pending_rebase()
				{
					self.queue.push(InternalEvent::OpenRebase);
					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.view_submodules,
				) {
					self.queue.push(InternalEvent::ViewSubmodules);
					Ok(EventState::Consumed)
				} else {
					Ok(EventState::NotConsumed)
				};
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn hide(&mut self) {
		self.visible = false;
		self.preview.clear();

		self.index.hide();
		self.index_wd.hide();
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;
		self.index.show()?;
		self.index_wd.show()?;
		self.metadata_generation
			.set(self.metadata_generation.get().wrapping_add(1));

		self.update()?;

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::{Focus, Status, StatusMetadataJob};
	use crate::{
		app::Environment,
		components::{Component, DrawableComponent, EventState},
		keys::{KeyConfig, Keymap},
		queue::InternalEvent,
	};
	use asyncgit::{StatusItem, StatusItemType};
	use crossbeam_channel::unbounded;
	use crossterm::event::{
		Event, KeyCode, KeyEvent, KeyModifiers, MouseButton,
		MouseEvent, MouseEventKind,
	};
	use ratatui::layout::Rect;
	use std::rc::Rc;
	use std::time::Duration;

	#[test]
	fn status_metadata_is_loaded_off_the_ui_thread() {
		let (temp, _repo) = git2_testing::repo_init_empty();
		let repo =
			asyncgit::sync::RepoPath::Path(temp.path().to_path_buf());
		let (sender, receiver) = unbounded();
		let job = asyncgit::asyncjob::AsyncSingleJob::new(sender);
		assert!(job.spawn(StatusMetadataJob::new(repo, None, 7)));
		assert_eq!(
			receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
			asyncgit::AsyncGitNotification::StatusMetadata
		);
		let result = job.take_last().unwrap().result().unwrap();
		assert_eq!(result.0, 7);
		assert!(!result.1.remotes.has_remote_for_fetch);
		assert!(!result.1.remotes.has_remote_for_push);
		assert!(result.1.branch.is_none());
	}

	fn modal_status() -> Status {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut status = Status::new(&env);
		let one = [StatusItem {
			path: String::from("one.txt"),
			status: StatusItemType::Modified,
		}];
		status.index_wd.set_items(&one).unwrap();
		status.index.set_items(&one).unwrap();
		status
	}

	#[test]
	fn stale_branch_metadata_cannot_enable_remote_actions() {
		let mut status = modal_status();
		status.remotes = super::RemoteStatus {
			has_remote_for_fetch: true,
			has_remote_for_push: true,
		};
		status.git_branch_state =
			Some(asyncgit::sync::BranchCompare {
				ahead: 1,
				behind: 0,
			});
		let initial_generation = status.metadata_generation.get();
		status
			.metadata_result_generation
			.set(Some(initial_generation));
		assert!(status.can_push());
		assert!(status.can_fetch());

		status.invalidate_status();
		assert!(!status.can_push());
		assert!(!status.can_fetch());

		let refreshed_generation = status.metadata_generation.get();
		status
			.metadata_result_generation
			.set(Some(refreshed_generation));
		assert!(status.can_push());
		assert!(status.can_fetch());
	}

	fn send(
		status: &mut Status,
		code: KeyCode,
		modifiers: KeyModifiers,
	) -> EventState {
		status
			.event(&Event::Key(KeyEvent::new(code, modifiers)))
			.unwrap()
	}

	#[test]
	fn modal_focus_cycles_forward_and_backward() {
		let mut status = modal_status();
		assert_eq!(status.focus, Focus::WorkDir);
		assert_eq!(
			send(&mut status, KeyCode::Tab, KeyModifiers::NONE),
			EventState::Consumed
		);
		assert_eq!(status.focus, Focus::Stage);
		assert_eq!(
			send(&mut status, KeyCode::Tab, KeyModifiers::NONE),
			EventState::Consumed
		);
		assert_eq!(status.focus, Focus::Diff);
		assert_eq!(
			send(&mut status, KeyCode::Tab, KeyModifiers::NONE),
			EventState::Consumed
		);
		assert_eq!(status.focus, Focus::WorkDir);
		assert_eq!(
			send(&mut status, KeyCode::BackTab, KeyModifiers::SHIFT),
			EventState::Consumed
		);
		assert_eq!(status.focus, Focus::Diff);
		assert_eq!(
			send(&mut status, KeyCode::BackTab, KeyModifiers::SHIFT),
			EventState::Consumed
		);
		assert_eq!(status.focus, Focus::Stage);
		assert_eq!(
			send(&mut status, KeyCode::BackTab, KeyModifiers::SHIFT),
			EventState::Consumed
		);
		assert_eq!(status.focus, Focus::WorkDir);
	}

	#[test]
	fn leaving_zoomed_status_diff_restores_panes_and_file_selection()
	{
		let mut status = modal_status();
		let selected_path = status.selected_path();
		status.focus = Focus::Diff;
		status.review_mode = true;
		status.zoomed = super::DiffLayout::Zoomed;

		assert_eq!(
			send(&mut status, KeyCode::Tab, KeyModifiers::NONE),
			EventState::Consumed,
		);

		assert_eq!(status.focus, Focus::WorkDir);
		assert_eq!(status.zoomed, super::DiffLayout::Normal);
		assert_eq!(status.selected_path(), selected_path);
	}

	#[test]
	fn modal_commit_opens_editor_from_diff_focus() {
		let mut status = modal_status();
		status.focus = Focus::Diff;

		assert_eq!(
			send(&mut status, KeyCode::Char('c'), KeyModifiers::NONE),
			EventState::Consumed
		);
		assert!(matches!(
			status.queue.pop(),
			Some(InternalEvent::OpenCommitEditor)
		));
	}

	#[test]
	fn modal_commit_opens_editor_from_workdir_focus() {
		let mut status = modal_status();
		assert_eq!(status.focus, Focus::WorkDir);

		assert_eq!(
			send(&mut status, KeyCode::Char('c'), KeyModifiers::NONE),
			EventState::Consumed
		);
		assert!(matches!(
			status.queue.pop(),
			Some(InternalEvent::OpenCommitEditor)
		));
		assert!(status.queue.pop().is_none());
	}

	#[test]
	fn legacy_commit_stays_inline_and_modal_shift_c_is_inline() {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Legacy));
		let mut legacy = Status::new(&env);
		let staged = [StatusItem {
			path: String::from("one.txt"),
			status: StatusItemType::Modified,
		}];
		legacy.index.set_items(&staged).unwrap();
		legacy.switch_focus(Focus::Stage).unwrap();
		assert_eq!(
			send(&mut legacy, KeyCode::Char('c'), KeyModifiers::NONE),
			EventState::Consumed
		);
		assert!(matches!(
			legacy.queue.pop(),
			Some(InternalEvent::OpenCommit)
		));

		let mut modal = modal_status();
		assert_eq!(
			send(&mut modal, KeyCode::Char('C'), KeyModifiers::SHIFT),
			EventState::Consumed
		);
		assert!(matches!(
			modal.queue.pop(),
			Some(InternalEvent::OpenCommitInline)
		));
	}

	#[test]
	fn modal_commit_without_staged_changes_does_not_queue_an_action()
	{
		let mut status = modal_status();
		status.index.set_items(&[]).unwrap();
		let _ =
			send(&mut status, KeyCode::Char('c'), KeyModifiers::NONE);
		assert!(status.queue.pop().is_none());
	}

	#[test]
	fn mouse_focus_uses_rendered_panes_and_keeps_drag_capture() {
		let mut status = modal_status();
		let backend = ratatui::backend::TestBackend::new(100, 30);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		terminal
			.draw(|frame| status.draw(frame, frame.area()).unwrap())
			.unwrap();

		let stage = status.stage_area.get();
		let workdir = status.workdir_area.get();
		let mouse = |kind, area: Rect| MouseEvent {
			kind,
			column: area.x + 2,
			row: area.y + 1,
			modifiers: KeyModifiers::NONE,
		};

		status
			.event(&Event::Mouse(mouse(
				MouseEventKind::Down(MouseButton::Left),
				stage,
			)))
			.unwrap();
		assert_eq!(status.focus, Focus::Stage);

		let mut cross_pane =
			mouse(MouseEventKind::Drag(MouseButton::Left), workdir);
		cross_pane.column = workdir.x + 2;
		cross_pane.row = workdir.y + 1;
		status.event(&Event::Mouse(cross_pane)).unwrap();
		assert_eq!(status.focus, Focus::Stage);

		status
			.event(&Event::Mouse(mouse(
				MouseEventKind::Up(MouseButton::Left),
				workdir,
			)))
			.unwrap();
		assert_eq!(status.focus, Focus::Stage);

		status
			.event(&Event::Mouse(mouse(
				MouseEventKind::Down(MouseButton::Left),
				workdir,
			)))
			.unwrap();
		assert_eq!(status.focus, Focus::WorkDir);
	}

	#[test]
	fn modal_list_navigation_at_boundaries_keeps_current_pane() {
		let mut status = modal_status();
		let _ =
			send(&mut status, KeyCode::Char('k'), KeyModifiers::NONE);
		assert_eq!(status.focus, Focus::WorkDir);
		assert_eq!(
			send(&mut status, KeyCode::Tab, KeyModifiers::NONE),
			EventState::Consumed
		);
		assert_eq!(status.focus, Focus::Stage);
		let _ =
			send(&mut status, KeyCode::Char('j'), KeyModifiers::NONE);
		assert_eq!(status.focus, Focus::Stage);
	}
}
