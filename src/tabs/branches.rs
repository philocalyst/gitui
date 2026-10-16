//! Branch relationship and review workspace.

use crate::{
	app::Environment,
	components::{
		CommandBlocking, CommandInfo, CommandText, CommitList,
		Component, DiffComponent, DiffPreviewController,
		DrawableComponent, EventState, SelectionStatus,
	},
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	popups::InspectCommitOpen,
	queue::{Action, InternalEvent, Queue, StackablePopupOpen},
	strings,
	ui::style::{SharedTheme, Theme},
};
use anyhow::Result;
use asyncgit::HeadIdentity;
use asyncgit::{
	asyncjob::AsyncSingleJob,
	branch_view::BranchTopologyRole,
	sync::{BranchInfo, RepoPath},
	AsyncBranchesJob, AsyncDiff, AsyncGitNotification, BranchViewJob,
	BranchViewRequest, BranchViewResult, DiffParams, DiffType,
};
use crossterm::event::{
	Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	style::Style,
	text::{Line, Span},
	widgets::{Block, Borders, Paragraph},
	Frame,
};
use std::collections::HashSet;
use std::{
	cell::Cell,
	cmp,
	time::{Duration, Instant},
};
use unicode_truncate::UnicodeTruncateStr;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
	Branches,
	Commits,
	Files,
	Diff,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitView {
	Topology,
	Contribution,
}

const INITIAL_TOPOLOGY_LIMIT: usize = 200;
const TOPOLOGY_PAGE_SIZE: usize = 200;
const REF_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
enum BaseChoice {
	Head,
	Reference(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputMode {
	Normal,
	BranchSearch,
	BaseSearch,
	BasePicker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadState {
	Needed,
	Loaded,
	Failed,
}

/// Persistent, searchable branch review workspace.
pub struct Branches {
	repo: RepoPath,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	options: SharedOptions,
	visible: bool,
	focus: Focus,
	commit_view: CommitView,
	refs: Vec<BranchInfo>,
	local_job: AsyncSingleJob<AsyncBranchesJob>,
	remote_job: AsyncSingleJob<AsyncBranchesJob>,
	local_state: LoadState,
	remote_state: LoadState,
	local_generation: u64,
	remote_generation: u64,
	local_started_generation: u64,
	remote_started_generation: u64,
	pending_local_refs: Option<Vec<BranchInfo>>,
	pending_remote_refs: Option<Vec<BranchInfo>>,
	head_identity: Option<HeadIdentity>,
	branch_error: Option<String>,
	last_ref_refresh: Instant,
	filter: String,
	input_mode: InputMode,
	filtered: Vec<usize>,
	selection: usize,
	commit_selection: usize,
	selected_history_id: Option<asyncgit::sync::CommitId>,
	selected_topology_id: Option<asyncgit::sync::CommitId>,
	selected_contribution_id: Option<asyncgit::sync::CommitId>,
	/// Shared loaded-prefix limit for topology and branch-only history.
	topology_limit: usize,
	file_selection: usize,
	base_override: Option<BaseChoice>,
	base_filter: String,
	base_selection: usize,
	base_candidates: Vec<BaseChoice>,
	view_job: AsyncSingleJob<BranchViewJob>,
	view_observer: Option<BranchViewJob>,
	view_generation: u64,
	view_request: Option<BranchViewRequest>,
	view_head_identity: Option<HeadIdentity>,
	view_result: Option<BranchViewResult>,
	view_error: Option<String>,
	git_diff: AsyncDiff,
	diff: DiffComponent,
	preview: DiffPreviewController,
	review_mode: bool,
	zoomed: bool,
	branch_area: Cell<Rect>,
	commit_area: Cell<Rect>,
	file_area: Cell<Rect>,
	diff_area: Cell<Rect>,
	pointer_capture: Option<Focus>,
}

impl Branches {
	/// Construct a hidden workspace tab.
	pub fn new(env: &Environment) -> Self {
		let repo = env.repo.borrow().clone();
		Self {
			repo: repo.clone(),
			queue: env.queue.clone(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			options: env.options.clone(),
			visible: false,
			focus: Focus::Branches,
			commit_view: CommitView::Topology,
			refs: Vec::new(),
			local_job: AsyncSingleJob::new(env.sender_git.clone()),
			remote_job: AsyncSingleJob::new(env.sender_git.clone()),
			local_state: LoadState::Needed,
			remote_state: LoadState::Needed,
			local_generation: 0,
			remote_generation: 0,
			local_started_generation: 0,
			remote_started_generation: 0,
			pending_local_refs: None,
			pending_remote_refs: None,
			head_identity: None,
			branch_error: None,
			last_ref_refresh: Instant::now(),
			filter: String::new(),
			input_mode: InputMode::Normal,
			filtered: Vec::new(),
			selection: 0,
			commit_selection: 0,
			selected_history_id: None,
			selected_topology_id: None,
			selected_contribution_id: None,
			topology_limit: INITIAL_TOPOLOGY_LIMIT,
			file_selection: 0,
			base_override: None,
			base_filter: String::new(),
			base_selection: 0,
			base_candidates: Vec::new(),
			view_job: AsyncSingleJob::new(env.sender_git.clone()),
			view_observer: None,
			view_generation: 0,
			view_request: None,
			view_head_identity: None,
			view_result: None,
			view_error: None,
			git_diff: AsyncDiff::new(repo, &env.sender_git),
			diff: DiffComponent::new(env, true),
			preview: DiffPreviewController::new(
				env.repo.clone(),
				env.sender_git.clone(),
				env.key_config.clone(),
			),
			review_mode: true,
			zoomed: false,
			branch_area: Cell::new(Rect::default()),
			commit_area: Cell::new(Rect::default()),
			file_area: Cell::new(Rect::default()),
			diff_area: Cell::new(Rect::default()),
			pointer_capture: None,
		}
	}

	/// Poll work and launch any missing branch/comparison requests.
	pub fn update(&mut self) -> Result<()> {
		if !self.visible {
			return Ok(());
		}
		if self.local_state == LoadState::Needed
			&& !self.local_job.is_pending()
		{
			self.local_started_generation = self.local_generation;
			self.local_job.spawn(AsyncBranchesJob::new(
				self.repo.clone(),
				true,
			));
		}
		if self.remote_state == LoadState::Needed
			&& !self.remote_job.is_pending()
		{
			self.remote_started_generation = self.remote_generation;
			self.remote_job.spawn(AsyncBranchesJob::new(
				self.repo.clone(),
				false,
			));
		}
		self.ensure_comparison();
		self.update_diff()?;
		Ok(())
	}

	/// Consume notifications from async Git work.
	pub fn update_git(
		&mut self,
		notification: AsyncGitNotification,
	) -> Result<()> {
		match notification {
			AsyncGitNotification::Branches => {
				self.update_branch_notifications()?;
			}
			AsyncGitNotification::BranchView => {
				if let Some(job) = self.view_job.take_last() {
					if let Some((request, result)) = job.result() {
						self.accept_branch_view(
							&request,
							result.map_err(|error| error.to_string()),
						)?;
					}
				}
			}
			AsyncGitNotification::BranchViewTopology => {
				if let Some((request, result)) = self
					.view_observer
					.as_ref()
					.and_then(BranchViewJob::topology_result)
				{
					self.accept_branch_view(&request, Ok(result))?;
				}
			}
			AsyncGitNotification::Diff => self.update_diff()?,
			AsyncGitNotification::DiffPreview => {
				self.preview.update();
			}
			AsyncGitNotification::Fetch
			| AsyncGitNotification::Push => {
				self.invalidate_branches();
				self.update()?;
			}
			_ => (),
		}
		Ok(())
	}

	fn update_branch_notifications(&mut self) -> Result<()> {
		if let Some(job) = self.local_job.take_last() {
			if let Some(snapshot) = job.result_with_head() {
				if self.local_started_generation
					== self.local_generation
				{
					match snapshot.branches {
						Ok(branches) => {
							self.pending_local_refs = Some(branches);
							match snapshot.head {
								Some(Ok(identity)) => {
									self.head_identity =
										Some(identity);
								}
								Some(Err(error)) => {
									self.local_state =
										LoadState::Failed;
									self.branch_error =
										Some(error.to_string());
								}
								None => {
									self.local_state =
										LoadState::Failed;
									self.branch_error = Some(
										"HEAD snapshot missing"
											.to_owned(),
									);
								}
							}
							if self.local_state != LoadState::Failed {
								self.local_state = LoadState::Loaded;
							}
						}
						Err(error) => {
							self.local_state = LoadState::Failed;
							self.branch_error =
								Some(error.to_string());
						}
					}
				} else {
					self.local_state = LoadState::Needed;
				}
			}
		}
		if let Some(job) = self.remote_job.take_last() {
			if let Some(result) = job.result() {
				if self.remote_started_generation
					== self.remote_generation
				{
					match result {
						Ok(branches) => {
							self.pending_remote_refs = Some(branches);
							self.remote_state = LoadState::Loaded;
						}
						Err(error) => {
							self.remote_state = LoadState::Failed;
							self.branch_error =
								Some(error.to_string());
						}
					}
				} else {
					self.remote_state = LoadState::Needed;
				}
			}
		}
		if self.local_state == LoadState::Loaded
			&& self.remote_state == LoadState::Loaded
		{
			self.apply_ref_snapshot();
			self.last_ref_refresh = Instant::now();
			self.reconcile_comparison_snapshot();
			self.ensure_comparison();
			self.update_diff()?;
		}
		Ok(())
	}

	/// Whether branch, comparison, or selected-file work is outstanding.
	pub fn anything_pending(&self) -> bool {
		self.local_job.is_pending()
			|| self.remote_job.is_pending()
			|| self.view_job.is_pending()
			|| self.git_diff.is_pending()
			|| self.preview.is_pending()
	}

	/// Mark branch references stale after a branch mutation or checkout.
	pub fn invalidate_branches(&mut self) {
		self.local_generation = self.local_generation.wrapping_add(1);
		self.remote_generation =
			self.remote_generation.wrapping_add(1);
		self.local_state = LoadState::Needed;
		self.remote_state = LoadState::Needed;
		self.pending_local_refs = None;
		self.pending_remote_refs = None;
		self.branch_error = None;
		self.invalidate_comparison();
		self.last_ref_refresh = Instant::now();
	}

	/// Schedule a coalesced background refs check without discarding a usable view.
	pub fn poll_ref_freshness(&mut self) {
		if self.visible
			&& self.local_state == LoadState::Loaded
			&& self.remote_state == LoadState::Loaded
			&& !self.local_job.is_pending()
			&& !self.remote_job.is_pending()
			&& self.last_ref_refresh.elapsed() >= REF_REFRESH_INTERVAL
		{
			self.local_generation =
				self.local_generation.wrapping_add(1);
			self.remote_generation =
				self.remote_generation.wrapping_add(1);
			self.local_state = LoadState::Needed;
			self.remote_state = LoadState::Needed;
			self.pending_local_refs = None;
			self.pending_remote_refs = None;
			self.branch_error = None;
			self.last_ref_refresh = Instant::now();
		}
	}

	fn invalidate_comparison(&mut self) {
		self.view_generation = self.view_generation.wrapping_add(1);
		self.view_request = None;
		self.view_head_identity = None;
		self.view_result = None;
		self.view_error = None;
		self.selected_history_id = None;
		self.file_selection = 0;
		self.clear_diff(false);
	}

	fn apply_ref_snapshot(&mut self) {
		let (Some(local), Some(remote)) = (
			self.pending_local_refs.take(),
			self.pending_remote_refs.take(),
		) else {
			return;
		};
		let selected_ref = self
			.selected_branch()
			.map(|branch| branch.reference.clone());
		let mut branches = local;
		branches.retain(BranchInfo::is_local);
		let mut remote = remote;
		remote.retain(|branch| {
			!branch.is_local() && !branch.name.ends_with("/HEAD")
		});
		branches.extend(remote);
		self.refs = branches;
		self.refs.sort_by(|a, b| a.reference.cmp(&b.reference));
		self.rebuild_filter(selected_ref.as_deref());
	}

	fn reconcile_comparison_snapshot(&mut self) {
		let Some(request) = self.view_request.clone() else {
			return;
		};
		let Some(branch) = self.selected_branch() else {
			self.invalidate_comparison();
			return;
		};
		let base_reference = self.base_reference(branch);
		let base_tip = match base_reference.as_deref() {
			None => self
				.head_identity
				.as_ref()
				.and_then(|head| head.target),
			Some(reference) => self
				.refs
				.iter()
				.find(|candidate| candidate.reference == reference)
				.map(|candidate| candidate.top_commit),
		};
		let head_matches = base_reference.is_some()
			|| self.view_head_identity == self.head_identity;
		let matches = request.branch_reference == branch.reference
			&& request.branch_tip == branch.top_commit
			&& request.base_reference == base_reference
			&& request.base_tip == base_tip
			&& head_matches;
		if !matches {
			self.invalidate_comparison();
		}
	}

	fn set_focus(&mut self, focus: Focus) {
		if self.focus != focus {
			self.pointer_capture = None;
			if focus != Focus::Diff {
				self.zoomed = false;
			}
		}
		self.focus = focus;
		self.diff.focus(focus == Focus::Diff);
	}

	fn rebuild_filter(&mut self, keep: Option<&str>) {
		let query = self.filter.to_lowercase();
		self.filtered = self
			.refs
			.iter()
			.enumerate()
			.filter_map(|(index, branch)| {
				let matches = query.is_empty()
					|| branch.name.to_lowercase().contains(&query)
					|| branch
						.reference
						.to_lowercase()
						.contains(&query)
					|| branch
						.top_commit_message
						.to_lowercase()
						.contains(&query);
				matches.then_some(index)
			})
			.collect();
		self.selection = keep
			.and_then(|reference| {
				self.filtered.iter().position(|index| {
					self.refs[*index].reference == reference
				})
			})
			.unwrap_or_else(|| {
				self.selection
					.min(self.filtered.len().saturating_sub(1))
			});
	}

	fn selected_branch(&self) -> Option<&BranchInfo> {
		self.filtered
			.get(self.selection)
			.and_then(|index| self.refs.get(*index))
	}

	fn refs_ready(&self) -> bool {
		self.local_state == LoadState::Loaded
			&& self.remote_state == LoadState::Loaded
			&& !self.local_job.is_pending()
			&& !self.remote_job.is_pending()
	}

	fn base_reference(&self, branch: &BranchInfo) -> Option<String> {
		match &self.base_override {
			Some(BaseChoice::Head) => None,
			Some(BaseChoice::Reference(reference)) => {
				Some(reference.clone())
			}
			None => branch.local_details().and_then(|details| {
				details
					.upstream
					.as_ref()
					.map(|upstream| upstream.reference.clone())
			}),
		}
	}

	fn base_label(&self, branch: &BranchInfo) -> String {
		match &self.base_override {
			Some(BaseChoice::Head) => "HEAD".to_owned(),
			Some(BaseChoice::Reference(reference)) => {
				reference.clone()
			}
			None => self
				.base_reference(branch)
				.unwrap_or_else(|| "HEAD".to_owned()),
		}
	}

	fn ensure_comparison(&mut self) {
		if !self.refs_ready() {
			return;
		}
		let Some(branch) = self.selected_branch().cloned() else {
			self.invalidate_comparison();
			return;
		};
		let base_reference = self.base_reference(&branch);
		let base_tip = match base_reference.as_deref() {
			None => self
				.head_identity
				.as_ref()
				.and_then(|head| head.target),
			Some(reference) => self
				.refs
				.iter()
				.find(|candidate| candidate.reference == reference)
				.map(|candidate| candidate.top_commit),
		};
		let request_key = BranchViewRequest {
			branch_reference: branch.reference.clone(),
			base_reference,
			branch_tip: branch.top_commit,
			base_tip,
			generation: self.view_generation,
			topology_limit: self.topology_limit,
		};
		if self.view_request.as_ref().is_some_and(|request| {
			request.branch_reference == request_key.branch_reference
				&& request.base_reference
					== request_key.base_reference
				&& request.branch_tip == request_key.branch_tip
				&& request.base_tip == request_key.base_tip
				&& request.topology_limit
					== request_key.topology_limit
		}) {
			return;
		}
		if let Some(job) = self.view_observer.take() {
			job.cancel();
		}
		self.view_generation = self.view_generation.wrapping_add(1);
		let request = BranchViewRequest {
			generation: self.view_generation,
			..request_key
		};
		self.view_request = Some(request.clone());
		self.view_head_identity = if request.base_reference.is_none()
		{
			self.head_identity.clone()
		} else {
			None
		};
		self.view_result = None;
		self.view_error = None;
		self.clear_diff(true);
		let job = BranchViewJob::new(self.repo.clone(), request);
		self.view_observer = Some(job.clone());
		self.view_job.spawn(job);
	}

	fn update_diff(&mut self) -> Result<()> {
		let Some(result) = &self.view_result else {
			self.clear_diff(self.view_job.is_pending());
			return Ok(());
		};
		let Some(file) = result.files.get(self.file_selection) else {
			self.clear_diff(false);
			return Ok(());
		};
		let params = DiffParams {
			path: file.path.clone(),
			diff_type: DiffType::Commits(
				asyncgit::sync::commit_files::OldNew {
					old: result.merge_base.unwrap_or(result.base),
					new: result.tip,
				},
			),
			options: asyncgit::sync::diff::DiffOptions::default(),
		};
		if let Some(file_diff) =
			self.git_diff.request(params.clone())?
		{
			self.diff.update(
				file.path.clone(),
				false,
				file_diff.clone(),
			);
			if self.review_mode {
				let preference = self
					.options
					.borrow()
					.diff_preview_preference()
					.into();
				self.preview.request(
					params,
					self.diff_area.get().width.saturating_sub(2)
						as usize,
					preference,
					asyncgit::hash(&file_diff),
				);
			}
		} else {
			self.clear_diff(true);
		}
		Ok(())
	}

	fn clear_diff(&mut self, pending: bool) {
		self.diff.clear(pending);
		if pending && self.review_mode {
			self.preview
				.clear_with_message("Loading comparison preview…");
		} else {
			self.preview.clear();
		}
	}

	fn accept_branch_view(
		&mut self,
		request: &BranchViewRequest,
		result: std::result::Result<BranchViewResult, String>,
	) -> Result<()> {
		if self.view_request.as_ref() != Some(request)
			|| request.generation != self.view_generation
		{
			return Ok(());
		}
		if result.as_ref().is_ok_and(|result| {
			!result.relationship_loaded
				&& self.view_result.as_ref().is_some_and(|current| {
					current.relationship_loaded
				})
		}) {
			return Ok(());
		}
		match result {
			Ok(result) => {
				if result.relationship_loaded {
					self.view_error = None;
				}
				self.view_result = Some(result);
				self.commit_selection = self
					.selected_history_id
					.and_then(|id| {
						self.displayed_commit_ids()
							.iter()
							.position(|candidate| *candidate == id)
					})
					.unwrap_or(0);
				self.sync_history_selection();
				self.file_selection = 0;
				self.update_diff()?;
			}
			Err(error) => {
				self.view_error = Some(error);
				self.clear_diff(false);
			}
		}
		Ok(())
	}

	fn displayed_commit_ids(&self) -> Vec<asyncgit::sync::CommitId> {
		let Some(result) = &self.view_result else {
			return Vec::new();
		};
		match self.commit_view {
			CommitView::Topology => result
				.topology
				.iter()
				.map(|commit| commit.id)
				.collect(),
			CommitView::Contribution => result
				.commits
				.iter()
				.map(|commit| commit.id)
				.collect(),
		}
	}

	fn sync_history_selection(&mut self) {
		let selected = self
			.displayed_commit_ids()
			.get(self.commit_selection)
			.copied();
		self.selected_history_id = selected;
		if let Some(id) = selected {
			match self.commit_view {
				CommitView::Topology => {
					self.selected_topology_id = Some(id);
				}
				CommitView::Contribution => {
					self.selected_contribution_id = Some(id);
				}
			}
		}
	}

	fn toggle_commit_view(&mut self) {
		self.sync_history_selection();
		self.commit_view = match self.commit_view {
			CommitView::Topology => CommitView::Contribution,
			CommitView::Contribution => CommitView::Topology,
		};
		let selected = match self.commit_view {
			CommitView::Topology => self.selected_topology_id,
			CommitView::Contribution => self.selected_contribution_id,
		};
		let ids = self.displayed_commit_ids();
		self.commit_selection = selected
			.and_then(|id| {
				ids.iter().position(|candidate| *candidate == id)
			})
			.unwrap_or(0);
		self.sync_history_selection();
	}

	fn request_more_history(&mut self) {
		let has_more = self.view_result.as_ref().is_some_and(
			|result| match self.commit_view {
				CommitView::Topology => result.topology_truncated,
				CommitView::Contribution => {
					result.commit_count.is_some_and(|count| {
						result.commits.len() < count
					})
				}
			},
		);
		if has_more {
			self.topology_limit = self
				.topology_limit
				.saturating_add(TOPOLOGY_PAGE_SIZE);
			self.view_request = None;
			self.ensure_comparison();
		}
	}

	fn selected_file_changed(&mut self) {
		if self
			.view_result
			.as_ref()
			.is_some_and(|result| !result.files.is_empty())
		{
			let len = self
				.view_result
				.as_ref()
				.map_or(0, |result| result.files.len());
			self.file_selection =
				self.file_selection.min(len.saturating_sub(1));
		}
	}

	fn move_selection(&mut self, direction: isize) {
		match self.focus {
			Focus::Branches => {
				if self.is_base_picker() {
					let choices = self.visible_base_choices();
					let pos = choices
						.iter()
						.position(|choice| {
							*choice == self.base_selection
						})
						.unwrap_or(0);
					self.base_selection = choices
						.get(offset_index(
							pos,
							choices.len(),
							direction,
						))
						.copied()
						.unwrap_or(0);
				} else {
					self.selection = offset_index(
						self.selection,
						self.filtered.len(),
						direction,
					);
					self.invalidate_comparison();
				}
			}
			Focus::Commits => {
				let len = self.displayed_commit_ids().len();
				self.commit_selection = offset_index(
					self.commit_selection,
					len,
					direction,
				);
				self.sync_history_selection();
			}
			Focus::Files => {
				let len = self
					.view_result
					.as_ref()
					.map_or(0, |result| result.files.len());
				self.file_selection =
					offset_index(self.file_selection, len, direction);
				self.selected_file_changed();
			}
			Focus::Diff => (),
		}
	}

	fn cycle_base(&mut self) {
		if !self.refs_ready() {
			return;
		}
		let Some(branch) = self.selected_branch().cloned() else {
			return;
		};
		self.base_candidates = std::iter::once(BaseChoice::Head)
			.chain(
				self.refs
					.iter()
					.filter(|candidate| {
						candidate.reference != branch.reference
					})
					.map(|candidate| {
						BaseChoice::Reference(
							candidate.reference.clone(),
						)
					}),
			)
			.collect();
		let current = self.base_reference(&branch);
		self.base_selection = self
			.base_candidates
			.iter()
			.position(|choice| match (choice, current.as_deref()) {
				(BaseChoice::Head, None) => true,
				(BaseChoice::Reference(reference), Some(base)) => {
					reference == base
				}
				_ => false,
			})
			.unwrap_or(0);
		self.input_mode = InputMode::BaseSearch;
		self.base_filter.clear();
	}

	fn visible_base_choices(&self) -> Vec<usize> {
		let query = self.base_filter.to_lowercase();
		self.base_candidates
			.iter()
			.enumerate()
			.filter_map(|(index, choice)| {
				let label = match choice {
					BaseChoice::Head => "HEAD",
					BaseChoice::Reference(reference) => reference,
				};
				(query.is_empty()
					|| label.to_lowercase().contains(&query))
				.then_some(index)
			})
			.collect()
	}

	const fn is_searching(&self) -> bool {
		matches!(
			self.input_mode,
			InputMode::BranchSearch | InputMode::BaseSearch
		)
	}

	const fn is_base_picker(&self) -> bool {
		matches!(
			self.input_mode,
			InputMode::BaseSearch | InputMode::BasePicker
		)
	}

	fn filter_event(
		&mut self,
		key: &crossterm::event::KeyEvent,
	) -> Option<EventState> {
		if (self.is_searching()
			|| self.is_base_picker()
			|| !self.filter.is_empty())
			&& key_match(key, self.key_config.keys.exit_popup)
		{
			match self.input_mode {
				InputMode::BranchSearch => {
					self.input_mode = InputMode::Normal;
				}
				InputMode::BaseSearch => {
					self.input_mode = InputMode::BasePicker;
				}
				InputMode::BasePicker => {
					self.input_mode = InputMode::Normal;
					self.base_filter.clear();
				}
				InputMode::Normal => {
					self.filter.clear();
					self.rebuild_filter(None);
					self.ensure_comparison();
				}
			}
			return Some(EventState::Consumed);
		}
		if self.is_searching() && key.code == KeyCode::Backspace {
			if self.is_base_picker() {
				self.base_filter.pop();
				self.base_selection = self
					.visible_base_choices()
					.first()
					.copied()
					.unwrap_or(0);
			} else {
				self.filter.pop();
				self.rebuild_filter(None);
				self.ensure_comparison();
			}
			return Some(EventState::Consumed);
		}
		if self.is_base_picker()
			&& key_match(key, self.key_config.keys.enter)
		{
			if self.refs_ready() {
				if let Some(choice) = self
					.visible_base_choices()
					.iter()
					.find(|index| **index == self.base_selection)
					.and_then(|index| {
						self.base_candidates.get(*index)
					})
					.cloned()
				{
					self.base_override = Some(choice);
					self.input_mode = InputMode::Normal;
					self.invalidate_comparison();
					self.ensure_comparison();
				}
			}
			return Some(EventState::Consumed);
		}
		if self.is_searching() {
			if let KeyCode::Char(character) = key.code {
				if key.modifiers.is_empty()
					|| key.modifiers == KeyModifiers::SHIFT
				{
					if self.is_base_picker() {
						self.base_filter.push(character);
						self.base_selection = self
							.visible_base_choices()
							.first()
							.copied()
							.unwrap_or(0);
					} else {
						self.filter.push(character);
						self.rebuild_filter(None);
						self.ensure_comparison();
					}
					return Some(EventState::Consumed);
				}
			}
		}
		None
	}

	fn draw_list(&self, f: &mut Frame, area: Rect) {
		let height = usize::from(area.height.saturating_sub(2));
		if self.is_base_picker() {
			self.draw_base_picker(f, area, height);
			return;
		}
		let scroll = visible_start(
			self.selection,
			self.filtered.len(),
			height,
		);
		let lines = self
			.filtered
			.iter()
			.enumerate()
			.skip(scroll)
			.take(height)
			.map(|(row, index)| {
				let branch = &self.refs[*index];
				let current = branch
					.local_details()
					.is_some_and(|details| details.is_head);
				let tag = if current {
					"*"
				} else if branch.is_local() {
					" "
				} else {
					"o"
				};
				let marker = if row + scroll == self.selection {
					"›"
				} else {
					" "
				};
				let text = truncate_line(
					&format!(
						"{marker}{tag} {}",
						one_line(&branch.name)
					),
					area.width.saturating_sub(2),
				);
				Line::from(Span::styled(
					text,
					self.theme.selected(
						Style::default(),
						row + scroll == self.selection,
					),
				))
			})
			.collect::<Vec<_>>();
		let lines = if lines.is_empty() {
			vec![self.empty_branch_line(area.width.saturating_sub(2))]
		} else {
			lines
		};
		let title = if self.filter.is_empty() {
			"Branches".to_owned()
		} else {
			format!("Branches /{}", self.filter)
		};
		f.render_widget(
			Paragraph::new(lines).block(
				Block::default()
					.title(title)
					.borders(Borders::ALL)
					.border_style(if self.focus == Focus::Branches {
						Theme::attention_block()
					} else {
						Style::default()
					}),
			),
			area,
		);
		let shown =
			height.min(self.filtered.len().saturating_sub(scroll));
		highlight_selected_row(
			f,
			area,
			self.selection,
			scroll,
			shown,
			self.theme.selected(Style::default(), true),
		);
	}

	fn empty_branch_line(&self, width: u16) -> Line<'static> {
		let message = self.branch_error.as_ref().map_or_else(
			|| {
				if self.local_job.is_pending()
					|| self.remote_job.is_pending()
					|| self.local_state == LoadState::Needed
					|| self.remote_state == LoadState::Needed
				{
					"Loading branch refs…".to_owned()
				} else {
					"No matching branches".to_owned()
				}
			},
			|error| {
				format!("Branch load failed: {}", one_line(error))
			},
		);
		Line::from(truncate_line(&message, width))
	}

	fn draw_base_picker(
		&self,
		f: &mut Frame,
		area: Rect,
		height: usize,
	) {
		let choices = self.visible_base_choices();
		let position = choices
			.iter()
			.position(|choice| *choice == self.base_selection)
			.unwrap_or(0);
		let scroll = visible_start(position, choices.len(), height);
		let lines = choices
			.iter()
			.enumerate()
			.skip(scroll)
			.take(height)
			.map(|(_row, choice_index)| {
				let choice = &self.base_candidates[*choice_index];
				let label = match choice {
					BaseChoice::Head => "HEAD".to_owned(),
					BaseChoice::Reference(reference) => {
						reference.clone()
					}
				};
				Line::from(Span::styled(
					truncate_line(
						&format!(
							"{} {}",
							if *choice_index == self.base_selection {
								"›"
							} else {
								" "
							},
							label
						),
						area.width.saturating_sub(2),
					),
					self.theme.selected(
						Style::default(),
						*choice_index == self.base_selection,
					),
				))
			})
			.collect::<Vec<_>>();
		f.render_widget(
			Paragraph::new(lines).block(
				Block::default()
					.title(format!(
						"Choose base /{}",
						self.base_filter
					))
					.borders(Borders::ALL)
					.border_style(Theme::attention_block()),
			),
			area,
		);
		highlight_selected_row(
			f,
			area,
			position,
			scroll,
			height.min(choices.len().saturating_sub(scroll)),
			self.theme.selected(Style::default(), true),
		);
	}

	fn draw_review(&self, f: &mut Frame, area: Rect) -> Result<()> {
		let chunks = Layout::default()
			.direction(Direction::Vertical)
			.constraints([
				Constraint::Length(5),
				Constraint::Percentage(30),
				Constraint::Percentage(28),
				Constraint::Min(3),
			])
			.split(area);
		self.draw_review_header(f, chunks[0]);
		self.draw_commit_list(f, chunks[1]);
		self.draw_file_list(f, chunks[2]);
		self.diff_area.set(chunks[3]);
		if self.review_mode {
			self.preview.view().draw(
				f,
				chunks[3],
				&self.theme,
				self.focus == Focus::Diff,
			);
			Ok(())
		} else {
			self.diff.draw(f, chunks[3])
		}
	}

	fn draw_review_header(&self, f: &mut Frame, area: Rect) {
		let branch = self.selected_branch();
		let header = branch.map_or_else(
			|| "No branch selected".to_owned(),
			|branch| {
				let relation =
					self.view_result.as_ref().map_or_else(
						|| self.relationship_status_text(),
						|result| {
							if !result.relationship_loaded {
								return self.relationship_status_text();
							}
							match (result.merge_base, result.ahead, result.behind) {
								(Some(_), Some(ahead), Some(behind)) => {
									format!("branch vs base: ↑{ahead} ↓{behind}")
								}
								(None, Some(ahead), Some(behind)) => {
									format!("no merge-base · branch-only {ahead}, base-only {behind}")
								}
								_ => "Relationship data unavailable".to_owned(),
							}
						},
					);
				format!(
					"{}  vs  {}\n{}  ·  {}\n{}",
					truncate_line(
						&one_line(&branch.reference),
						area.width.saturating_sub(2)
					),
					truncate_line(
						&one_line(&self.base_label(branch)),
						area.width.saturating_sub(2)
					),
					relation,
					branch.top_commit.get_short_string(),
					truncate_line(
						&one_line(&branch.top_commit_message),
						area.width.saturating_sub(2)
					)
				)
			},
		);
		f.render_widget(
			Paragraph::new(header).block(
				Block::default()
					.title("Review")
					.borders(Borders::ALL),
			),
			area,
		);
	}

	#[allow(clippy::too_many_lines)]
	fn draw_commit_list(&self, f: &mut Frame, area: Rect) {
		self.commit_area.set(area);
		let height = usize::from(area.height.saturating_sub(2));
		let len = self.displayed_commit_ids().len();
		let start = visible_start(self.commit_selection, len, height);
		let lines = self.view_result.as_ref().map_or_else(
			|| vec![Line::from(self.loading_text())],
			|result| match self.commit_view {
				CommitView::Topology => result
					.topology
					.iter()
					.enumerate()
					.skip(start)
					.take(height)
					.map(|(index, commit)| {
						self.topology_line(commit, index, area.width)
					})
					.collect(),
				CommitView::Contribution => result
					.commits
					.iter()
					.enumerate()
					.skip(start)
					.take(height)
					.map(|(index, commit)| {
						let selected = index == self.commit_selection;
						let marker =
							if selected { "›" } else { " " };
						Line::from(Span::styled(
							truncate_line(
								&format!(
									"{marker} {} {}",
									commit.id.get_short_string(),
									one_line(&commit.summary)
								),
								area.width.saturating_sub(2),
							),
							self.theme
								.selected(Style::default(), selected),
						))
					})
					.collect(),
			},
		);
		let title = match (&self.view_result, self.commit_view) {
			(_, CommitView::Topology) => {
				self.view_result.as_ref().map_or_else(
					|| "Topology · loading".to_owned(),
					|result| {
						let more = if result.topology_truncated {
							" · + load more"
						} else {
							""
						};
						format!(
							"Topology · {} commits{more}",
							result.topology.len()
						)
					},
				)
			}
			(Some(result), CommitView::Contribution) => {
				let more = if result
					.commit_count
					.is_some_and(|count| result.commits.len() < count)
				{
					" · + load more"
				} else {
					""
				};
				result.commit_count.map_or_else(
					|| {
						format!(
							"Branch-only · {} shown · counting…",
							result.commits.len()
						)
					},
					|count| {
						format!(
							"Branch-only · {} shown of {}{more}",
							result.commits.len(),
							count
						)
					},
				)
			}
			(None, CommitView::Contribution) => {
				"Branch-only commits · loading".to_owned()
			}
		};
		f.render_widget(
			Paragraph::new(lines).block(
				Block::default()
					.title(title)
					.borders(Borders::ALL)
					.border_style(if self.focus == Focus::Commits {
						Theme::attention_block()
					} else {
						Style::default()
					}),
			),
			area,
		);
		highlight_selected_row(
			f,
			area,
			self.commit_selection,
			start,
			height.min(len.saturating_sub(start)),
			self.theme.selected(Style::default(), true),
		);
	}

	fn topology_graph_spans(
		&self,
		commit: &asyncgit::branch_view::BranchViewTopologyCommit,
		selected: bool,
		available: usize,
	) -> (Vec<Span<'static>>, usize) {
		let graph_width = usize::from(commit.graph.lane_count)
			.saturating_mul(2)
			.min(available / 2);
		let spans = CommitList::graph_row_spans(
			&commit.graph,
			graph_width,
			&HashSet::new(),
		);
		let graph_budget = available / 2;
		let mut graph_remaining = graph_budget;
		let mut visible = Vec::with_capacity(spans.len());
		for span in spans {
			let content = span.content.as_ref();
			let clipped = content.unicode_truncate(graph_remaining).0;
			let used = UnicodeWidthStr::width(clipped);
			if used == 0 {
				break;
			}
			graph_remaining = graph_remaining.saturating_sub(used);
			visible.push(Span::styled(
				clipped.to_string(),
				self.theme.selected(span.style, selected),
			));
			if used < UnicodeWidthStr::width(content) {
				break;
			}
		}
		let remaining = available.saturating_sub(
			graph_budget.saturating_sub(graph_remaining),
		);
		(visible, remaining)
	}

	fn topology_line(
		&self,
		commit: &asyncgit::branch_view::BranchViewTopologyCommit,
		index: usize,
		width: u16,
	) -> Line<'static> {
		let selected = index == self.commit_selection;
		let available = usize::from(width.saturating_sub(2));
		let (mut visible, remaining) =
			self.topology_graph_spans(commit, selected, available);
		let roles = commit
			.roles
			.iter()
			.map(|role| match role {
				BranchTopologyRole::BranchTip => "branch tip",
				BranchTopologyRole::BaseTip => "base tip",
				BranchTopologyRole::MergeBase => "merge-base",
			})
			.collect::<Vec<_>>()
			.join(",");
		let outside = if commit.outside_parents.is_empty() {
			String::new()
		} else {
			format!(
				" ↳{}",
				commit
					.outside_parents
					.iter()
					.map(asyncgit::sync::CommitId::get_short_string)
					.collect::<Vec<_>>()
					.join(",")
			)
		};
		let label = if commit.labels.is_empty() {
			String::new()
		} else {
			format!(
				" {}",
				commit
					.labels
					.iter()
					.map(|label| one_line(label))
					.collect::<Vec<_>>()
					.join(",")
			)
		};
		let octopus = if commit.parents.len() > 2 {
			format!(" · {} parents", commit.parents.len())
		} else {
			String::new()
		};
		let role = if roles.is_empty() && octopus.is_empty() {
			String::new()
		} else {
			format!(" [{roles}{label}{octopus}]")
		};
		let prefix = format!(
			"{} {}{role}",
			if selected { "›" } else { " " },
			commit.id.get_short_string(),
		);
		let summary = one_line(&commit.summary);
		let detailed = format!("{prefix}{outside} {summary}");
		let text = if UnicodeWidthStr::width(detailed.as_str())
			<= remaining
		{
			detailed
		} else if !commit.outside_parents.is_empty() {
			format!(
				"{prefix} ↳{} parent edges outside {summary}",
				commit.outside_parents.len()
			)
		} else {
			format!("{prefix} {summary}")
		};
		visible.push(Span::raw(truncate_line(
			&text,
			u16::try_from(remaining).unwrap_or(u16::MAX),
		)));
		Line::from(visible)
	}

	fn draw_file_list(&self, f: &mut Frame, area: Rect) {
		self.file_area.set(area);
		let height = usize::from(area.height.saturating_sub(2));
		let file_start =
			self.view_result.as_ref().map_or(0, |result| {
				visible_start(
					self.file_selection,
					result.files.len(),
					height,
				)
			});
		let file_lines = self.view_result.as_ref().map_or_else(
			|| vec![Line::from(self.relationship_status_text())],
			|result| {
				if result.files.is_empty()
					&& !result.relationship_loaded
				{
					return vec![Line::from(
						self.relationship_status_text(),
					)];
				}
				result
					.files
					.iter()
					.enumerate()
					.skip(file_start)
					.take(height)
					.map(|(index, file)| {
						let marker = if index == self.file_selection {
							"›"
						} else {
							" "
						};
						Line::from(Span::styled(
							truncate_line(
								&format!(
									"{marker} {} {}",
									file.status,
									one_line(&file.path)
								),
								area.width.saturating_sub(2),
							),
							self.theme.selected(
								Style::default(),
								index == self.file_selection,
							),
						))
					})
					.collect::<Vec<_>>()
			},
		);
		f.render_widget(
			Paragraph::new(file_lines).block(
				Block::default()
					.title(self.file_list_title())
					.borders(Borders::ALL)
					.border_style(if self.focus == Focus::Files {
						Theme::attention_block()
					} else {
						Style::default()
					}),
			),
			area,
		);
		let file_count = self
			.view_result
			.as_ref()
			.map_or(0, |result| result.files.len());
		highlight_selected_row(
			f,
			area,
			self.file_selection,
			file_start,
			height.min(file_count.saturating_sub(file_start)),
			self.theme.selected(Style::default(), true),
		);
	}

	fn loading_text(&self) -> String {
		self.view_error.as_ref().map_or_else(
			|| {
				if self.view_job.is_pending() {
					"Loading branch comparison…".to_owned()
				} else {
					"No comparison data".to_owned()
				}
			},
			|error| format!("Comparison failed: {error}"),
		)
	}

	fn relationship_status_text(&self) -> String {
		self.view_error.as_ref().map_or_else(
			|| "Loading relationship…".to_owned(),
			|error| {
				format!("Relationship failed: {}", one_line(error))
			},
		)
	}

	fn file_list_title(&self) -> String {
		match self.view_result.as_ref() {
			Some(result) if !result.relationship_loaded => {
				if self.view_error.is_some() {
					"Files unavailable · relationship failed"
						.to_owned()
				} else {
					"Files pending relationship".to_owned()
				}
			}
			Some(result) if result.merge_base.is_some() => {
				"Files changed since merge-base → branch tip"
					.to_owned()
			}
			Some(_) => {
				"No merge-base · endpoint file diff unavailable"
					.to_owned()
			}
			None if self.view_error.is_some() => {
				"Files unavailable · relationship failed".to_owned()
			}
			None => "Files changed since merge-base → branch tip"
				.to_owned(),
		}
	}

	fn focus_navigation_event(
		&mut self,
		key: &crossterm::event::KeyEvent,
	) -> bool {
		if key_match(key, self.key_config.keys.focus_next)
			|| key_match(key, self.key_config.keys.focus_right)
			|| key_match(key, self.key_config.keys.focus_down)
		{
			self.set_focus(next_focus(self.focus));
			return true;
		}
		if key_match(key, self.key_config.keys.focus_previous)
			|| key_match(key, self.key_config.keys.focus_left)
			|| key_match(key, self.key_config.keys.focus_up)
		{
			self.set_focus(previous_focus(self.focus));
			return true;
		}
		false
	}

	fn history_navigation_event(
		&mut self,
		key: &crossterm::event::KeyEvent,
	) -> bool {
		if self.focus != Focus::Commits {
			return false;
		}
		if key_match(key, self.key_config.keys.log_toggle_graph) {
			self.toggle_commit_view();
			return true;
		}
		if key_match(key, self.key_config.keys.branches_load_more) {
			self.request_more_history();
			return true;
		}
		if key.code == KeyCode::End && key.modifiers.is_empty() {
			self.commit_selection =
				self.displayed_commit_ids().len().saturating_sub(1);
			self.sync_history_selection();
			return true;
		}
		false
	}

	fn inspect_selection(&self) {
		if self.focus == Focus::Branches {
			if self.refs_ready() {
				if let Some(branch) = self.selected_branch() {
					self.queue.push(InternalEvent::OpenPopup(
						StackablePopupOpen::InspectCommit(
							InspectCommitOpen::new(branch.top_commit),
						),
					));
				}
			}
		} else if self.focus == Focus::Commits {
			if let Some(commit_id) = self.selected_history_id {
				self.queue.push(InternalEvent::OpenPopup(
					StackablePopupOpen::InspectCommit(
						InspectCommitOpen::new(commit_id),
					),
				));
			}
		}
	}

	fn push_history_commands(&self, out: &mut Vec<CommandInfo>) {
		out.push(CommandInfo::new(
			strings::commands::log_toggle_graph(&self.key_config),
			true,
			self.focus == Focus::Commits,
		));
		let more_available = self.view_result.as_ref().is_some_and(
			|result| match self.commit_view {
				CommitView::Topology => result.topology_truncated,
				CommitView::Contribution => {
					result.commit_count.is_some_and(|count| {
						result.commits.len() < count
					})
				}
			},
		);
		out.push(CommandInfo::new(
			strings::commands::branches_load_more(&self.key_config),
			more_available,
			more_available && self.focus == Focus::Commits,
		));
	}

	fn key_event(
		&mut self,
		key: &crossterm::event::KeyEvent,
		event: &Event,
	) -> Result<EventState> {
		if self.zoomed
			&& key_match(key, self.key_config.keys.exit_popup)
		{
			self.zoomed = false;
			return Ok(EventState::Consumed);
		}
		if let Some(state) = self.filter_event(key) {
			return Ok(state);
		}
		if self.is_base_picker() {
			if key_match(key, self.key_config.keys.move_up) {
				self.move_selection(-1);
			} else if key_match(key, self.key_config.keys.move_down) {
				self.move_selection(1);
			}
			return Ok(EventState::Consumed);
		}
		if self.focus_navigation_event(key) {
			return Ok(EventState::Consumed);
		}
		if self.focus == Focus::Diff {
			if key_match(key, self.key_config.keys.diff_zoom_toggle) {
				self.zoomed = !self.zoomed;
				return Ok(EventState::Consumed);
			}
			if key_match(key, self.key_config.keys.diff_view_toggle) {
				self.review_mode = !self.review_mode;
				if self.review_mode {
					self.update_diff()?;
				} else {
					self.preview.clear();
				}
				return Ok(EventState::Consumed);
			}
			return if self.review_mode {
				self.preview.view_mut().event(event)
			} else {
				self.diff.event(event)
			};
		}
		if self.history_navigation_event(key) {
			return Ok(EventState::Consumed);
		}
		if key_match(key, self.key_config.keys.move_up) {
			self.move_selection(-1);
			self.ensure_comparison();
			self.update_diff()?;
			return Ok(EventState::Consumed);
		}
		if key_match(key, self.key_config.keys.move_down) {
			self.move_selection(1);
			self.ensure_comparison();
			self.update_diff()?;
			return Ok(EventState::Consumed);
		}
		if self.branch_action(key) {
			return Ok(EventState::Consumed);
		}
		if key_match(key, self.key_config.keys.enter) {
			self.inspect_selection();
			return Ok(EventState::Consumed);
		}
		Ok(EventState::NotConsumed)
	}

	fn select_at(&mut self, area: Rect, x: u16, y: u16) {
		if area.width < 3
			|| area.height < 3
			|| x <= area.x
			|| x >= area.x + area.width - 1
			|| y <= area.y
			|| y >= area.y + area.height - 1
		{
			return;
		}
		let row = usize::from(y.saturating_sub(area.y + 1));
		match self.focus {
			Focus::Branches => {
				if self.is_base_picker() {
					let choices = self.visible_base_choices();
					let position = choices
						.iter()
						.position(|choice| {
							*choice == self.base_selection
						})
						.unwrap_or(0);
					let idx = visible_start(
						position,
						choices.len(),
						usize::from(area.height.saturating_sub(2)),
					)
					.saturating_add(row);
					if let Some(choice) = choices.get(idx) {
						self.base_selection = *choice;
					}
				} else {
					let idx = visible_start(
						self.selection,
						self.filtered.len(),
						usize::from(area.height.saturating_sub(2)),
					)
					.saturating_add(row);
					if idx < self.filtered.len() {
						self.selection = idx;
						self.invalidate_comparison();
					}
				}
			}
			Focus::Commits => {
				let len = self.displayed_commit_ids().len();
				self.commit_selection = visible_start(
					self.commit_selection,
					len,
					usize::from(area.height.saturating_sub(2)),
				)
				.saturating_add(row)
				.min(len.saturating_sub(1));
				self.sync_history_selection();
			}
			Focus::Files => {
				let len = self
					.view_result
					.as_ref()
					.map_or(0, |result| result.files.len());
				self.file_selection = visible_start(
					self.file_selection,
					len,
					usize::from(area.height.saturating_sub(2)),
				)
				.saturating_add(row)
				.min(len.saturating_sub(1));
				self.selected_file_changed();
			}
			Focus::Diff => (),
		}
	}

	fn branch_action(
		&mut self,
		key: &crossterm::event::KeyEvent,
	) -> bool {
		if self.focus != Focus::Branches || self.is_base_picker() {
			return false;
		}
		if self.focus == Focus::Branches
			&& key_match(key, self.key_config.keys.branch_find)
		{
			self.input_mode = InputMode::BranchSearch;
			return true;
		}
		if self.focus == Focus::Branches
			&& key_match(key, self.key_config.keys.branches_set_base)
		{
			self.cycle_base();
			return true;
		}
		if self.focus == Focus::Branches
			&& key_match(key, self.key_config.keys.branches_create)
		{
			self.queue.push(InternalEvent::CreateBranch);
			return true;
		}
		if !self.refs_ready() {
			return key_match(
				key,
				self.key_config.keys.branches_checkout,
			) || key_match(
				key,
				self.key_config.keys.rename_branch,
			) || key_match(
				key,
				self.key_config.keys.delete_branch,
			) || key_match(
				key,
				self.key_config.keys.rebase_branch,
			);
		}
		if self.focus == Focus::Branches
			&& key_match(key, self.key_config.keys.branches_checkout)
		{
			if let Some(branch) = self.selected_branch().cloned() {
				self.queue
					.push(InternalEvent::CheckoutOption(branch));
			}
			return true;
		}
		if self.focus == Focus::Branches
			&& key_match(key, self.key_config.keys.rename_branch)
		{
			if let Some(branch) = self
				.selected_branch()
				.filter(|branch| branch.is_local())
				.cloned()
			{
				self.queue.push(InternalEvent::RenameBranch(
					branch.reference,
					branch.name,
				));
			}
			return true;
		}
		if self.focus == Focus::Branches
			&& key_match(key, self.key_config.keys.delete_branch)
		{
			if let Some(branch) = self
				.selected_branch()
				.filter(|branch| {
					!branch
						.local_details()
						.is_some_and(|details| details.is_head)
				})
				.cloned()
			{
				let action = if branch.is_local() {
					Action::DeleteLocalBranch(branch.reference)
				} else {
					Action::DeleteRemoteBranch(branch.reference)
				};
				self.queue.push(InternalEvent::ConfirmAction(action));
			}
			return true;
		}
		if self.focus == Focus::Branches
			&& key_match(key, self.key_config.keys.rebase_branch)
		{
			if let Some(branch) = self.selected_branch().cloned() {
				self.queue.push(InternalEvent::RebaseOnto(branch));
			}
			return true;
		}
		false
	}

	fn pointer_event(&mut self, event: &Event) -> Result<EventState> {
		let Event::Mouse(mouse) = event else {
			return Ok(EventState::NotConsumed);
		};
		let point = (mouse.column, mouse.row).into();
		let owner_at = |tab: &Self| {
			[
				(Focus::Branches, tab.branch_area.get()),
				(Focus::Commits, tab.commit_area.get()),
				(Focus::Files, tab.file_area.get()),
				(Focus::Diff, tab.diff_area.get()),
			]
			.into_iter()
			.find_map(|(focus, area)| {
				area.contains(point).then_some(focus)
			})
		};
		match mouse.kind {
			MouseEventKind::Down(MouseButton::Left) => {
				let Some(owner) = owner_at(self) else {
					return Ok(EventState::Consumed);
				};
				if self.is_base_picker() && owner != Focus::Branches {
					return Ok(EventState::Consumed);
				}
				self.set_focus(owner);
				self.pointer_capture = Some(owner);
				if owner == Focus::Diff {
					if self.review_mode {
						return Ok(EventState::Consumed);
					}
					return self.diff.event(event);
				}
				self.select_at(
					owner_area(self, owner),
					mouse.column,
					mouse.row,
				);
				self.ensure_comparison();
				self.update_diff()?;
			}
			MouseEventKind::Drag(MouseButton::Left) => {
				if let Some(owner) = self.pointer_capture {
					if owner == Focus::Diff {
						if self.review_mode {
							return Ok(EventState::Consumed);
						}
						return self.diff.event(event);
					}
					if owner_area(self, owner).contains(point) {
						self.select_at(
							owner_area(self, owner),
							mouse.column,
							mouse.row,
						);
						self.ensure_comparison();
						self.update_diff()?;
					}
				}
			}
			MouseEventKind::Up(MouseButton::Left) => {
				let owner = self.pointer_capture.take();
				if owner == Some(Focus::Diff) {
					if self.review_mode {
						return Ok(EventState::Consumed);
					}
					return self.diff.event(event);
				}
			}
			MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
				let Some(owner) = owner_at(self) else {
					return Ok(EventState::Consumed);
				};
				if self.is_base_picker() && owner != Focus::Branches {
					return Ok(EventState::Consumed);
				}
				self.set_focus(owner);
				if owner == Focus::Diff {
					if self.review_mode {
						return self.preview.view_mut().event(event);
					}
					return self.diff.event(event);
				}
				let direction =
					if mouse.kind == MouseEventKind::ScrollUp {
						-3
					} else {
						3
					};
				self.move_selection_for(owner, direction);
				self.ensure_comparison();
				self.update_diff()?;
			}
			_ => return Ok(EventState::NotConsumed),
		}
		Ok(EventState::Consumed)
	}

	fn move_selection_for(&mut self, focus: Focus, direction: isize) {
		self.set_focus(focus);
		self.move_selection(direction);
	}
}

const fn owner_area(tab: &Branches, owner: Focus) -> Rect {
	match owner {
		Focus::Branches => tab.branch_area.get(),
		Focus::Commits => tab.commit_area.get(),
		Focus::Files => tab.file_area.get(),
		Focus::Diff => tab.diff_area.get(),
	}
}

fn offset_index(index: usize, len: usize, direction: isize) -> usize {
	if len == 0 {
		return 0;
	}
	if direction < 0 {
		index.saturating_sub(direction.unsigned_abs())
	} else {
		cmp::min(
			index.saturating_add(direction.cast_unsigned()),
			len - 1,
		)
	}
}

const fn next_focus(focus: Focus) -> Focus {
	match focus {
		Focus::Branches => Focus::Commits,
		Focus::Commits => Focus::Files,
		Focus::Files => Focus::Diff,
		Focus::Diff => Focus::Branches,
	}
}

const fn previous_focus(focus: Focus) -> Focus {
	match focus {
		Focus::Branches => Focus::Diff,
		Focus::Commits => Focus::Branches,
		Focus::Files => Focus::Commits,
		Focus::Diff => Focus::Files,
	}
}

const fn visible_start(
	selection: usize,
	len: usize,
	height: usize,
) -> usize {
	if height == 0 || len <= height {
		0
	} else {
		selection.saturating_add(1).saturating_sub(height)
	}
}

fn highlight_selected_row(
	f: &mut Frame,
	area: Rect,
	selected: usize,
	first: usize,
	shown: usize,
	style: Style,
) {
	if area.width < 3
		|| area.height < 3
		|| selected < first
		|| selected >= first.saturating_add(shown)
	{
		return;
	}
	let row = area.y.saturating_add(1).saturating_add(
		u16::try_from(selected - first).unwrap_or(u16::MAX),
	);
	let rect = Rect::new(
		area.x.saturating_add(1),
		row,
		area.width.saturating_sub(2),
		1,
	);
	f.buffer_mut().set_style(rect, style);
}

fn one_line(text: &str) -> String {
	text.chars()
		.map(|character| {
			if character.is_control() {
				' '
			} else {
				character
			}
		})
		.collect()
}

fn truncate_line(text: &str, width: u16) -> String {
	text.unicode_truncate(usize::from(width)).0.to_string()
}

impl DrawableComponent for Branches {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		if self.zoomed {
			self.diff_area.set(area);
			if self.review_mode {
				self.preview.view().draw(f, area, &self.theme, true);
				return Ok(());
			}
			return self.diff.draw(f, area);
		}
		let chunks = Layout::default()
			.direction(Direction::Horizontal)
			.constraints([
				Constraint::Percentage(32),
				Constraint::Percentage(68),
			])
			.split(area);
		self.branch_area.set(chunks[0]);
		self.draw_list(f, chunks[0]);
		self.draw_review(f, chunks[1])?;
		Ok(())
	}
}

impl Component for Branches {
	fn event(&mut self, event: &Event) -> Result<EventState> {
		if !self.visible {
			return Ok(EventState::NotConsumed);
		}
		match event {
			Event::Key(key) => self.key_event(key, event),
			Event::Mouse(_) => self.pointer_event(event),
			_ => Ok(EventState::NotConsumed),
		}
	}

	fn is_visible(&self) -> bool {
		self.visible
	}
	fn show(&mut self) -> Result<()> {
		self.visible = true;
		self.set_focus(self.focus);
		self.update()
	}
	fn hide(&mut self) {
		self.visible = false;
		self.pointer_capture = None;
		self.diff.focus(false);
		self.preview.clear();
	}
	fn captures_input(&self) -> bool {
		self.visible
			&& self.focus == Focus::Branches
			&& (self.is_searching()
				|| self.input_mode == InputMode::BasePicker)
	}
	fn selection_status(&self) -> Option<SelectionStatus> {
		if self.focus == Focus::Diff && !self.review_mode {
			self.diff.selection_status()
		} else {
			None
		}
	}
	fn modal_status_hint(&self) -> Option<String> {
		(self.visible
			&& self.focus == Focus::Diff
			&& self.review_mode)
			.then(|| {
				format!(
					"↑/↓ Scroll  h/l Pan  {} Zoom  {} Patch  Esc Restore",
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
		if self.visible || force_all {
			out.push(CommandInfo::new(
				strings::commands::diff_zoom_toggle(&self.key_config),
				true,
				self.focus == Focus::Diff || force_all,
			));
			if self.review_mode {
				out.push(CommandInfo::new(
					strings::commands::diff_view_toggle(
						&self.key_config,
					),
					true,
					self.focus == Focus::Diff || force_all,
				));
			} else {
				self.diff.commands(out, force_all);
			}
			let branch = self.selected_branch();
			let checkout_available = self.refs_ready()
				&& branch.is_some_and(|branch| {
					!branch
						.local_details()
						.is_some_and(|details| details.is_head)
				});
			let delete_available = checkout_available;
			let local_available = self.refs_ready()
				&& branch.is_some_and(BranchInfo::is_local);
			out.push(CommandInfo::new(
				strings::commands::find_branch(&self.key_config),
				true,
				self.focus == Focus::Branches,
			));
			out.push(CommandInfo::new(
				strings::commands::branches_set_base(
					&self.key_config,
				),
				self.refs_ready() && branch.is_some(),
				self.focus == Focus::Branches
					&& !self.is_base_picker(),
			));
			out.push(CommandInfo::new(
				strings::commands::branches_create(&self.key_config),
				true,
				self.focus == Focus::Branches
					&& !self.is_base_picker(),
			));
			out.push(CommandInfo::new(
				strings::commands::rename_branch_popup(
					&self.key_config,
				),
				local_available,
				self.focus == Focus::Branches
					&& !self.is_base_picker(),
			));
			out.push(CommandInfo::new(
				strings::commands::delete_branch_popup(
					&self.key_config,
				),
				delete_available,
				self.focus == Focus::Branches
					&& !self.is_base_picker(),
			));
			out.push(CommandInfo::new(
				strings::commands::branch_popup_rebase(
					&self.key_config,
				),
				self.refs_ready() && branch.is_some(),
				self.focus == Focus::Branches
					&& !self.is_base_picker(),
			));
			out.push(CommandInfo::new(
				CommandText::new(
					format!(
						"Checkout [{}]",
						self.key_config.get_hint(
							self.key_config.keys.branches_checkout
						)
					),
					"checkout the selected local or remote branch",
					"-- Branches --",
				)
				.with_binding(self.key_config.keys.branches_checkout),
				checkout_available,
				self.focus == Focus::Branches
					&& !self.is_base_picker(),
			));
			out.push(CommandInfo::new(
				CommandText::new(
					format!("Inspect [{}]", self.key_config.get_hint(self.key_config.keys.enter)),
					"inspect the selected branch or branch-only commit",
					"-- Branches --",
				)
				.with_binding(self.key_config.keys.enter),
				self.refs_ready() && branch.is_some(),
				self.focus != Focus::Diff && !self.is_base_picker(),
			));
			self.push_history_commands(out);
		}
		CommandBlocking::PassingOn
	}
}

#[cfg(test)]
mod tests {
	use super::{
		visible_start, BaseChoice, Branches, Focus, HeadIdentity,
	};
	use crate::keys::{GituiKeyEvent, KeyConfig, Keymap};
	use crate::{
		app::Environment,
		components::{Component, DrawableComponent, EventState},
	};
	use asyncgit::branch_view::{
		BranchTopologyRole, BranchViewTopologyCommit,
	};
	use asyncgit::graph::{ConnectionType, GraphRow};
	use asyncgit::sync::branch::{
		LocalBranch, RemoteBranch, UpstreamBranch,
	};
	use asyncgit::sync::diff::{
		DiffLine, DiffLinePosition, DiffLineType, FileDiff, Hunk,
	};
	use asyncgit::{
		sync::{BranchDetails, BranchInfo, CommitId},
		BranchViewCommit, BranchViewFile, BranchViewRequest,
		BranchViewResult,
	};
	use crossterm::event::{
		Event, KeyCode, KeyEvent, KeyModifiers, MouseButton,
		MouseEvent, MouseEventKind,
	};
	use ratatui::{backend::TestBackend, style::Color, Terminal};
	use std::{
		rc::Rc,
		time::{Duration, Instant},
	};

	fn commit_id(n: usize) -> CommitId {
		CommitId::from_str_unchecked(&format!("{n:040x}")).unwrap()
	}

	fn local_branch(n: usize) -> BranchInfo {
		let name = format!("branch-{n:03}");
		BranchInfo {
			name: name.clone(),
			reference: format!("refs/heads/{name}"),
			top_commit_message: format!("tip summary {n}"),
			top_commit: commit_id(n + 1),
			details: BranchDetails::Local(LocalBranch {
				is_head: n == 0,
				has_upstream: false,
				upstream: None,
				remote: None,
			}),
		}
	}

	fn remote_branch(n: usize) -> BranchInfo {
		let name = format!("origin/release-{n}");
		BranchInfo {
			name: name.clone(),
			reference: format!("refs/remotes/{name}"),
			top_commit_message: format!("remote tip {n}"),
			top_commit: commit_id(n + 100),
			details: BranchDetails::Remote(RemoteBranch {
				has_tracking: false,
			}),
		}
	}

	fn workspace(count: usize) -> Branches {
		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut tab = Branches::new(&env);
		tab.visible = true;
		tab.refs = (0..count).map(local_branch).collect();
		tab.local_state = super::LoadState::Loaded;
		tab.remote_state = super::LoadState::Loaded;
		tab.rebuild_filter(None);
		tab
	}

	fn draw(tab: &Branches, width: u16, height: u16) -> TestBackend {
		let mut terminal =
			Terminal::new(TestBackend::new(width, height)).unwrap();
		terminal
			.draw(|frame| tab.draw(frame, frame.area()).unwrap())
			.unwrap();
		terminal.backend().clone()
	}

	fn screen_text(
		tab: &Branches,
		width: u16,
		height: u16,
	) -> String {
		let backend = draw(tab, width, height);
		(0..backend.buffer().area.height)
			.map(|row| {
				(0..backend.buffer().area.width)
					.map(|column| {
						backend.buffer()[(column, row)].symbol()
					})
					.collect::<String>()
			})
			.collect::<String>()
	}

	fn assert_row_background(
		backend: &TestBackend,
		area: ratatui::layout::Rect,
		row: u16,
		color: Color,
		selected: bool,
	) {
		for x in area.x + 1..area.x + area.width - 1 {
			assert_eq!(
				backend.buffer()[(x, row)].bg == color,
				selected,
				"unexpected row style at ({x}, {row})"
			);
		}
	}

	fn key(
		tab: &mut Branches,
		code: KeyCode,
		modifiers: KeyModifiers,
	) {
		tab.event(&Event::Key(KeyEvent::new(code, modifiers)))
			.unwrap();
	}

	fn binding(tab: &mut Branches, binding: GituiKeyEvent) {
		key(tab, binding.code, binding.modifiers);
	}

	fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
		Event::Mouse(MouseEvent {
			kind,
			column,
			row,
			modifiers: KeyModifiers::NONE,
		})
	}

	fn review_diff() -> FileDiff {
		let lines = (0..24)
			.map(|index| DiffLine {
				content: format!("review line {index}\n")
					.into_boxed_str(),
				line_type: DiffLineType::None,
				position: DiffLinePosition::default(),
			})
			.collect::<Vec<_>>();
		FileDiff {
			hunks: vec![Hunk {
				header_hash: 1,
				lines,
			}],
			lines: 24,
			untracked: false,
			sizes: (0, 0),
			size_delta: 0,
		}
	}

	#[test]
	fn mouse_selects_the_branch_drawn_at_a_scrolled_row() {
		let mut tab = workspace(50);
		tab.selection = 40;
		let backend = draw(&tab, 100, 12);
		let area = tab.branch_area.get();
		let height = usize::from(area.height.saturating_sub(2));
		let start =
			visible_start(tab.selection, tab.filtered.len(), height);
		let row = area.y + 1;
		let drawn_line = (area.x + 1..area.x + area.width - 1)
			.map(|column| backend.buffer()[(column, row)].symbol())
			.collect::<String>();
		let expected =
			tab.refs[tab.filtered[start]].reference.clone();
		assert!(
			drawn_line.contains(&tab.refs[tab.filtered[start]].name)
		);

		tab.event(&mouse(
			MouseEventKind::Down(MouseButton::Left),
			area.x + 1,
			row,
		))
		.unwrap();
		assert_eq!(
			tab.selected_branch().unwrap().reference,
			expected
		);
		assert!(tab.queue.pop().is_none());
	}

	#[test]
	fn clicks_on_any_branch_panel_border_leave_selection_unchanged() {
		let mut tab = workspace(12);
		tab.selection = 4;
		let _ = draw(&tab, 100, 20);
		let area = tab.branch_area.get();
		let before = tab.selected_branch().unwrap().reference.clone();
		for (x, y) in [
			(area.x, area.y + 1),
			(area.x + area.width - 1, area.y + 1),
			(area.x + 2, area.y),
			(area.x + 2, area.y + area.height - 1),
		] {
			tab.event(&mouse(
				MouseEventKind::Down(MouseButton::Left),
				x,
				y,
			))
			.unwrap();
			assert_eq!(
				tab.selected_branch().unwrap().reference,
				before
			);
		}
	}

	#[test]
	fn branch_filter_captures_q_and_escape_has_staged_behavior() {
		let mut tab = workspace(8);
		let find = tab.key_config.keys.branch_find;
		binding(&mut tab, find);
		assert!(tab.captures_input());
		for character in ['q', 'j', 'k', 'g'] {
			key(
				&mut tab,
				KeyCode::Char(character),
				KeyModifiers::NONE,
			);
		}
		assert_eq!(tab.filter, "qjkg");
		assert!(tab.captures_input());
		key(&mut tab, KeyCode::Esc, KeyModifiers::NONE);
		assert!(!tab.is_searching());
		assert_eq!(tab.filter, "qjkg");
		key(&mut tab, KeyCode::Esc, KeyModifiers::NONE);
		assert!(tab.filter.is_empty());
	}

	#[test]
	fn escape_from_zoom_restores_filtered_branch_layout_without_changing_focus_or_source(
	) {
		let mut tab = workspace(8);
		tab.filter = "branch-00".to_owned();
		tab.rebuild_filter(None);
		tab.set_focus(Focus::Diff);
		tab.zoomed = true;
		let filter = tab.filter.clone();
		let selection =
			tab.selected_branch().unwrap().reference.clone();
		let _ = draw(&tab, 100, 24);
		assert_eq!(tab.branch_area.get().width, 0);

		key(&mut tab, KeyCode::Esc, KeyModifiers::NONE);

		assert!(!tab.zoomed);
		assert_eq!(tab.filter, filter);
		assert_eq!(tab.focus, Focus::Diff);
		assert_eq!(
			tab.selected_branch().unwrap().reference,
			selection,
		);
		let _ = draw(&tab, 100, 24);
		assert!(tab.branch_area.get().width > 0);
	}

	#[test]
	fn changing_focus_from_zoom_restores_branch_layout_before_navigation(
	) {
		let mut tab = workspace(8);
		tab.set_focus(Focus::Diff);
		let source = tab.selected_branch().unwrap().reference.clone();
		let zoom = tab.key_config.keys.diff_zoom_toggle;
		binding(&mut tab, zoom);
		assert!(tab.zoomed);

		key(&mut tab, KeyCode::Char('h'), KeyModifiers::CONTROL);

		assert_eq!(tab.focus, Focus::Files);
		assert!(!tab.zoomed);
		assert_eq!(tab.selected_branch().unwrap().reference, source);
		let _ = draw(&tab, 100, 24);
		assert!(tab.branch_area.get().width > 0);
	}

	#[test]
	fn ctrl_d_does_not_trigger_branch_deletion() {
		let mut tab = workspace(3);
		tab.selection = 1;
		assert_eq!(
			tab.event(&Event::Key(KeyEvent::new(
				KeyCode::Char('d'),
				KeyModifiers::CONTROL,
			)))
			.unwrap(),
			EventState::NotConsumed,
		);
		assert!(tab.queue.pop().is_none());
	}

	#[test]
	fn workspace_commands_expose_the_bindings_the_event_handler_uses()
	{
		let tab = workspace(3);
		let mut commands = Vec::new();
		tab.commands(&mut commands, false);
		assert_eq!(
			commands
				.iter()
				.find(|command| {
					command.text.name.starts_with("Create branch")
				})
				.and_then(|command| command.text.binding)
				.map(|binding| binding.code),
			Some(tab.key_config.keys.branches_create.code)
		);
		assert_eq!(
			commands
				.iter()
				.find(|command| {
					command.text.name.starts_with("Checkout")
				})
				.and_then(|command| command.text.binding)
				.map(|binding| binding.code),
			Some(tab.key_config.keys.branches_checkout.code)
		);
		assert_eq!(
			commands
				.iter()
				.find(|command| {
					command.text.name.starts_with("Set base")
				})
				.and_then(|command| command.text.binding)
				.map(|binding| binding.code),
			Some(tab.key_config.keys.branches_set_base.code)
		);
	}

	#[test]
	fn diff_wheel_is_forwarded_and_ctrl_hjkl_only_changes_focus() {
		let mut tab = workspace(5);
		// This exercises the patch viewport's existing wheel and selection
		// behavior; separate Review tests cover the read-only preview viewport.
		tab.review_mode = false;
		tab.diff.update(
			"review.txt".to_owned(),
			false,
			review_diff(),
		);
		tab.set_focus(Focus::Diff);
		tab.selection = 3;
		let _ = draw(&tab, 100, 24);
		let diff = tab.diff_area.get();
		let selection = tab.selection;
		let status_before = tab.selection_status();
		tab.event(&mouse(
			MouseEventKind::ScrollDown,
			diff.x + 1,
			diff.y + 1,
		))
		.unwrap();
		assert_eq!(tab.focus, Focus::Diff);
		assert_eq!(tab.selection, selection);
		assert!(tab.diff.focused());
		assert_eq!(tab.selection_status(), status_before);

		let select_mode = tab.key_config.keys.select_mode;
		tab.event(&Event::Key(KeyEvent::new(
			select_mode.code,
			select_mode.modifiers,
		)))
		.unwrap();
		let move_down = tab.key_config.keys.move_down;
		for _ in 0..12 {
			tab.event(&Event::Key(KeyEvent::new(
				move_down.code,
				move_down.modifiers,
			)))
			.unwrap();
		}
		assert_eq!(tab.selection_status().unwrap().count, 13);
		let rendered = draw(&tab, 100, 24);
		let diff = tab.diff_area.get();
		let visible_diff = (diff.y + 1..diff.y + diff.height - 1)
			.map(|row| {
				(diff.x + 1..diff.x + diff.width - 1)
					.map(|column| {
						rendered.buffer()[(column, row)].symbol()
					})
					.collect::<String>()
			})
			.collect::<String>();
		assert!(visible_diff.contains("review line 12"));

		for (key_code, expected) in [
			(KeyCode::Char('h'), Focus::Files),
			(KeyCode::Char('j'), Focus::Branches),
			(KeyCode::Char('k'), Focus::Files),
			(KeyCode::Char('l'), Focus::Branches),
		] {
			tab.set_focus(Focus::Diff);
			key(&mut tab, key_code, KeyModifiers::CONTROL);
			assert_eq!(tab.focus, expected);
			assert!(!tab.diff.focused());
			assert_eq!(tab.selection, selection);
		}
	}

	#[test]
	fn choosing_head_from_a_tracked_branch_is_an_explicit_base() {
		let mut tab = workspace(2);
		tab.refs[0].details = BranchDetails::Local(LocalBranch {
			is_head: true,
			has_upstream: true,
			upstream: Some(UpstreamBranch {
				reference: tab.refs[1].reference.clone(),
			}),
			remote: Some("origin".to_owned()),
		});
		tab.selection = 0;
		let set_base = tab.key_config.keys.branches_set_base;
		binding(&mut tab, set_base);
		assert!(tab.is_base_picker());
		assert!(tab.captures_input());
		assert!(tab.base_selection > 0);
		key(&mut tab, KeyCode::Esc, KeyModifiers::NONE);
		assert!(!tab.is_searching());
		assert!(tab.is_base_picker());
		assert!(tab.captures_input());
		let move_up = tab.key_config.keys.move_up;
		binding(&mut tab, move_up);
		assert_eq!(tab.base_selection, 0);
		key(&mut tab, KeyCode::Enter, KeyModifiers::NONE);
		assert_eq!(tab.base_override, Some(BaseChoice::Head));
		assert_eq!(
			tab.view_request.as_ref().unwrap().base_reference,
			None
		);
	}

	#[test]
	fn base_picker_owns_focus_and_mouse_outside_its_list() {
		let mut tab = workspace(4);
		draw(&tab, 120, 40);
		let set_base = tab.key_config.keys.branches_set_base;
		binding(&mut tab, set_base);
		key(&mut tab, KeyCode::Esc, KeyModifiers::NONE);
		assert!(tab.captures_input());
		let commit_area = tab.commit_area.get();
		tab.event(&mouse(
			MouseEventKind::Down(MouseButton::Left),
			commit_area.x + 2,
			commit_area.y + 2,
		))
		.unwrap();
		assert_eq!(tab.focus, Focus::Branches);
		let focus_next = tab.key_config.keys.focus_next;
		binding(&mut tab, focus_next);
		assert_eq!(tab.focus, Focus::Branches);
		assert!(tab.captures_input());
		let branch_area = tab.branch_area.get();
		tab.event(&mouse(
			MouseEventKind::Down(MouseButton::Left),
			branch_area.x + 2,
			branch_area.y + 2,
		))
		.unwrap();
		assert_eq!(tab.base_selection, 1);
		key(&mut tab, KeyCode::Enter, KeyModifiers::NONE);
		assert!(matches!(
			tab.base_override,
			Some(BaseChoice::Reference(_))
		));
	}

	fn request(
		branch: &BranchInfo,
		base: Option<&str>,
		generation: u64,
	) -> BranchViewRequest {
		BranchViewRequest {
			branch_reference: branch.reference.clone(),
			base_reference: base.map(str::to_owned),
			branch_tip: branch.top_commit,
			base_tip: None,
			generation,
			topology_limit: super::INITIAL_TOPOLOGY_LIMIT,
		}
	}

	fn empty_view(request: BranchViewRequest) -> BranchViewResult {
		BranchViewResult {
			tip: request.branch_tip,
			base: request.branch_tip,
			merge_base: None,
			ahead: None,
			behind: None,
			commit_count: Some(0),
			commits: vec![BranchViewCommit {
				id: request.branch_tip,
				summary: "stale".to_owned(),
				time: 0,
			}],
			files: Vec::new(),
			topology: Vec::new(),
			topology_limit: request.topology_limit,
			topology_truncated: false,
			relationship_loaded: true,
			request,
		}
	}

	fn topology_view(request: BranchViewRequest) -> BranchViewResult {
		let id = request.branch_tip;
		let base_id = commit_id(99);
		let topology_limit = request.topology_limit;
		BranchViewResult {
			request,
			tip: id,
			base: base_id,
			merge_base: None,
			ahead: Some(1),
			behind: Some(0),
			commit_count: Some(1),
			commits: vec![BranchViewCommit {
				id,
				summary: "same history identity".to_owned(),
				time: 0,
			}],
			files: Vec::new(),
			topology: vec![
				BranchViewTopologyCommit {
					id,
					parents: Vec::new(),
					summary: "topology endpoint visible".to_owned(),
					time: 0,
					graph: GraphRow {
						lane_count: 1.into(),
						commit_lane: 0.into(),
						is_branch_tip: true,
						lanes: vec![Some((
							ConnectionType::CommitNormal,
							0.into(),
						))],
						..GraphRow::default()
					},
					roles: vec![BranchTopologyRole::BranchTip],
					labels: vec!["refs/heads/branch-000".to_owned()],
					outside_parents: Vec::new(),
				},
				BranchViewTopologyCommit {
					id: base_id,
					parents: Vec::new(),
					summary: "base endpoint selectable".to_owned(),
					time: 0,
					graph: GraphRow::default(),
					roles: vec![BranchTopologyRole::BaseTip],
					labels: vec!["refs/heads/base".to_owned()],
					outside_parents: Vec::new(),
				},
			],
			topology_limit,
			topology_truncated: true,
			relationship_loaded: true,
		}
	}

	#[test]
	fn topology_is_rendered_and_view_toggle_preserves_commit_identity(
	) {
		let mut tab = workspace(2);
		let branch = tab.refs[0].clone();
		let request = request(&branch, None, 1);
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		tab.accept_branch_view(
			&request,
			Ok(topology_view(request.clone())),
		)
		.unwrap();
		tab.set_focus(Focus::Commits);
		let rendered = draw(&tab, 112, 30);
		let text = (0..rendered.buffer().area.height)
			.map(|row| {
				(0..rendered.buffer().area.width)
					.map(|column| {
						rendered.buffer()[(column, row)].symbol()
					})
					.collect::<String>()
			})
			.collect::<String>();
		assert!(text.contains("Topology"));
		let commit_area = tab.commit_area.get();
		assert_eq!(
			rendered.buffer()[(commit_area.x + 1, commit_area.y + 1)]
				.symbol(),
			"o"
		);
		assert!(text.contains("topology endpoint visible"));
		assert!(text.contains("branch tip"));

		let selected = tab.selected_history_id;
		let toggle = tab.key_config.keys.log_toggle_graph;
		binding(&mut tab, toggle);
		assert_eq!(tab.commit_view, super::CommitView::Contribution);
		assert_eq!(tab.selected_history_id, selected);
		binding(&mut tab, toggle);
		assert_eq!(tab.commit_view, super::CommitView::Topology);
		assert_eq!(tab.selected_history_id, selected);
	}

	#[test]
	fn selection_background_covers_rows_text_padding_and_no_neighbor()
	{
		let mut tab = workspace(3);
		let selected_bg = tab
			.theme
			.selected(super::Style::default(), true)
			.bg
			.unwrap();
		let rendered = draw(&tab, 120, 40);
		let branches = tab.branch_area.get();
		assert_row_background(
			&rendered,
			branches,
			branches.y + 1,
			selected_bg,
			true,
		);
		assert_row_background(
			&rendered,
			branches,
			branches.y + 2,
			selected_bg,
			false,
		);

		let branch = tab.refs[0].clone();
		let request = request(&branch, None, 1);
		let mut result = topology_view(request.clone());
		result.files = vec![
			BranchViewFile {
				path: "first.txt".to_owned(),
				status: 'M',
			},
			BranchViewFile {
				path: "second.txt".to_owned(),
				status: 'A',
			},
		];
		result.commits.push(BranchViewCommit {
			id: commit_id(98),
			summary: "another branch-only commit".to_owned(),
			time: 0,
		});
		tab.view_generation = 1;
		tab.view_request = Some(request);
		tab.view_result = Some(result);
		tab.file_selection = 1;
		let rendered = draw(&tab, 120, 40);
		let commits = tab.commit_area.get();
		assert_row_background(
			&rendered,
			commits,
			commits.y + 1,
			selected_bg,
			true,
		);
		assert_row_background(
			&rendered,
			commits,
			commits.y + 2,
			selected_bg,
			false,
		);
		let files = tab.file_area.get();
		assert_row_background(
			&rendered,
			files,
			files.y + 1,
			selected_bg,
			false,
		);
		assert_row_background(
			&rendered,
			files,
			files.y + 2,
			selected_bg,
			true,
		);

		tab.commit_view = super::CommitView::Contribution;
		let rendered = draw(&tab, 120, 40);
		assert_row_background(
			&rendered,
			commits,
			commits.y + 1,
			selected_bg,
			true,
		);
		assert_row_background(
			&rendered,
			commits,
			commits.y + 2,
			selected_bg,
			false,
		);
	}

	#[test]
	fn topology_selection_survives_empty_contribution_toggle() {
		let mut tab = workspace(2);
		let branch = tab.refs[0].clone();
		let request = request(&branch, None, 1);
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		let mut result = topology_view(request.clone());
		result.commits.clear();
		result.commit_count = Some(0);
		tab.accept_branch_view(&request, Ok(result)).unwrap();
		tab.set_focus(Focus::Commits);
		key(&mut tab, KeyCode::End, KeyModifiers::NONE);
		let selected_base = tab.selected_history_id;
		assert_eq!(selected_base, Some(commit_id(99)));

		let toggle = tab.key_config.keys.log_toggle_graph;
		binding(&mut tab, toggle);
		assert_eq!(tab.commit_view, super::CommitView::Contribution);
		assert_eq!(tab.selected_history_id, None);
		binding(&mut tab, toggle);
		assert_eq!(tab.commit_view, super::CommitView::Topology);
		assert_eq!(tab.selected_history_id, selected_base);
		assert_eq!(tab.commit_selection, 1);
	}

	#[test]
	fn base_only_topology_selection_survives_nonempty_contribution_toggle(
	) {
		let mut tab = workspace(2);
		let branch = tab.refs[0].clone();
		let request = request(&branch, None, 1);
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		tab.accept_branch_view(
			&request,
			Ok(topology_view(request.clone())),
		)
		.unwrap();
		tab.set_focus(Focus::Commits);
		key(&mut tab, KeyCode::End, KeyModifiers::NONE);
		let selected_base = tab.selected_history_id;
		assert_eq!(selected_base, Some(commit_id(99)));

		let toggle = tab.key_config.keys.log_toggle_graph;
		binding(&mut tab, toggle);
		assert_eq!(tab.selected_history_id, Some(branch.top_commit));
		binding(&mut tab, toggle);
		assert_eq!(tab.selected_history_id, selected_base);
		assert_eq!(tab.commit_selection, 1);
	}

	#[test]
	fn clicking_rendered_topology_row_selects_its_commit_id() {
		let mut tab = workspace(2);
		let branch = tab.refs[0].clone();
		let request = request(&branch, None, 1);
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		tab.accept_branch_view(
			&request,
			Ok(topology_view(request.clone())),
		)
		.unwrap();
		draw(&tab, 112, 30);
		let area = tab.commit_area.get();
		let expected = commit_id(99);
		tab.event(&mouse(
			MouseEventKind::Down(MouseButton::Left),
			area.x + 2,
			area.y + 2,
		))
		.unwrap();
		assert_eq!(tab.focus, Focus::Commits);
		assert_eq!(tab.commit_selection, 1);
		assert_eq!(tab.selected_history_id, Some(expected));
	}

	#[test]
	fn wide_graph_lanes_cannot_starve_the_commit_identity() {
		let mut tab = workspace(2);
		let branch = tab.refs[0].clone();
		let request = request(&branch, None, 1);
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		tab.accept_branch_view(
			&request,
			Ok(topology_view(request.clone())),
		)
		.unwrap();
		let graph =
			&mut tab.view_result.as_mut().unwrap().topology[0].graph;
		graph.lane_count = 128.into();
		graph.lanes =
			vec![Some((ConnectionType::Vertical, 0.into())); 128];
		let rendered = draw(&tab, 112, 30);
		let area = tab.commit_area.get();
		let row = (area.x + 1..area.x + area.width - 1)
			.map(|column| {
				rendered.buffer()[(column, area.y + 1)].symbol()
			})
			.collect::<String>();
		assert!(row.contains(&branch.top_commit.get_short_string()));
		assert!(area.x + area.width <= rendered.buffer().area.width);
	}

	#[test]
	fn load_more_extends_topology_request_and_keeps_selected_id() {
		let mut tab = workspace(2);
		let branch = tab.refs[0].clone();
		let request = request(&branch, None, 1);
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		tab.accept_branch_view(
			&request,
			Ok(topology_view(request.clone())),
		)
		.unwrap();
		tab.set_focus(Focus::Commits);
		let selected = tab.selected_history_id;
		let key = tab.key_config.keys.branches_load_more;
		binding(&mut tab, key);
		assert_eq!(
			tab.topology_limit,
			super::INITIAL_TOPOLOGY_LIMIT + super::TOPOLOGY_PAGE_SIZE
		);
		assert_eq!(tab.selected_history_id, selected);
		assert!(tab.view_request.as_ref().is_some_and(|request| {
			request.topology_limit == tab.topology_limit
		}));
	}

	#[test]
	fn load_more_extends_branch_only_contribution_history() {
		let mut tab = workspace(2);
		let branch = tab.refs[0].clone();
		let request = request(&branch, None, 1);
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		let mut result = topology_view(request.clone());
		result.commit_count = Some(500);
		tab.accept_branch_view(&request, Ok(result)).unwrap();
		tab.commit_view = super::CommitView::Contribution;
		tab.set_focus(Focus::Commits);

		let key = tab.key_config.keys.branches_load_more;
		binding(&mut tab, key);
		assert_eq!(
			tab.topology_limit,
			super::INITIAL_TOPOLOGY_LIMIT + super::TOPOLOGY_PAGE_SIZE
		);
		assert!(tab.view_request.as_ref().is_some_and(|request| {
			request.topology_limit == tab.topology_limit
		}));
	}

	#[test]
	fn visible_idle_ref_poll_preserves_snapshot_and_coalesces_refresh(
	) {
		let mut tab = workspace(2);
		let branch = tab.selected_branch().unwrap().clone();
		let mut request = request(&branch, None, 1);
		request.base_tip = Some(commit_id(900));
		tab.head_identity = Some(HeadIdentity {
			reference: Some("refs/heads/main".to_owned()),
			target: Some(commit_id(900)),
			detached: false,
		});
		tab.view_head_identity = tab.head_identity.clone();
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		tab.view_result = Some(topology_view(request));
		tab.commit_selection = 1;
		tab.sync_history_selection();
		let selected_history = tab.selected_history_id;
		tab.last_ref_refresh =
			Instant::now() - Duration::from_secs(6);
		tab.poll_ref_freshness();
		assert_eq!(tab.local_state, super::LoadState::Needed);
		assert_eq!(tab.remote_state, super::LoadState::Needed);
		assert!(tab.view_result.is_some());
		assert_eq!(tab.selected_history_id, selected_history);
		let generations =
			(tab.local_generation, tab.remote_generation);
		tab.poll_ref_freshness();
		assert_eq!(
			(tab.local_generation, tab.remote_generation),
			generations
		);

		tab.local_state = super::LoadState::Loaded;
		tab.remote_state = super::LoadState::Loaded;
		tab.visible = false;
		tab.last_ref_refresh =
			Instant::now() - Duration::from_secs(6);
		tab.poll_ref_freshness();
		assert_eq!(tab.local_state, super::LoadState::Loaded);
		assert_eq!(tab.remote_state, super::LoadState::Loaded);
	}

	#[test]
	fn unchanged_ref_snapshot_retains_comparison_and_history_selection(
	) {
		let mut tab = workspace(3);
		let branch = tab.selected_branch().unwrap().clone();
		let mut request = request(&branch, None, 1);
		let head = HeadIdentity {
			reference: Some("refs/heads/main".to_owned()),
			target: Some(commit_id(900)),
			detached: false,
		};
		request.base_tip = head.target;
		tab.head_identity = Some(head.clone());
		tab.view_head_identity = Some(head.clone());
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		tab.view_result = Some(topology_view(request));
		tab.commit_selection = 1;
		tab.sync_history_selection();
		let selected_history = tab.selected_history_id;
		let selected_ref = branch.reference;

		tab.pending_local_refs =
			Some((0..3).map(local_branch).collect());
		tab.pending_remote_refs = Some(Vec::new());
		tab.local_state = super::LoadState::Loaded;
		tab.remote_state = super::LoadState::Loaded;
		tab.head_identity = Some(head);
		tab.apply_ref_snapshot();
		tab.reconcile_comparison_snapshot();
		tab.ensure_comparison();

		assert_eq!(
			tab.selected_branch().unwrap().reference,
			selected_ref
		);
		assert!(tab.view_result.is_some());
		assert_eq!(tab.selected_history_id, selected_history);
		assert_eq!(tab.commit_selection, 1);
	}

	#[test]
	fn refresh_keeps_a_slow_comparison_request_accepting_its_result()
	{
		let mut tab = workspace(3);
		let branch = tab.selected_branch().unwrap().clone();
		let head = HeadIdentity {
			reference: Some("refs/heads/main".to_owned()),
			target: Some(commit_id(900)),
			detached: false,
		};
		tab.head_identity = Some(head.clone());
		let mut request = request(&branch, None, 1);
		request.base_tip = head.target;
		tab.view_head_identity = Some(head.clone());
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		tab.last_ref_refresh =
			Instant::now() - Duration::from_secs(6);

		tab.poll_ref_freshness();
		assert_eq!(tab.view_request, Some(request.clone()));
		assert_eq!(tab.view_generation, request.generation);

		tab.pending_local_refs =
			Some((0..3).map(local_branch).collect());
		tab.pending_remote_refs = Some(Vec::new());
		tab.local_state = super::LoadState::Loaded;
		tab.remote_state = super::LoadState::Loaded;
		tab.head_identity = Some(head);
		tab.apply_ref_snapshot();
		tab.reconcile_comparison_snapshot();
		tab.accept_branch_view(
			&request,
			Ok(topology_view(request.clone())),
		)
		.unwrap();

		assert!(tab.view_result.is_some());
		assert_eq!(
			tab.view_result.as_ref().unwrap().request,
			request
		);
	}

	#[test]
	fn changed_branch_tip_invalidates_the_old_comparison() {
		let mut tab = workspace(3);
		let branch = tab.selected_branch().unwrap().clone();
		let head = HeadIdentity {
			reference: Some("refs/heads/main".to_owned()),
			target: Some(commit_id(900)),
			detached: false,
		};
		tab.head_identity = Some(head.clone());
		let mut request = request(&branch, None, 1);
		request.base_tip = head.target;
		tab.view_head_identity = Some(head);
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		tab.view_result = Some(topology_view(request));

		let mut changed = branch.clone();
		changed.top_commit = commit_id(77);
		changed.top_commit_message = "new branch tip".to_owned();
		tab.pending_local_refs = Some(vec![changed]);
		tab.pending_remote_refs = Some(Vec::new());
		tab.local_state = super::LoadState::Loaded;
		tab.remote_state = super::LoadState::Loaded;
		tab.head_identity = Some(HeadIdentity {
			reference: Some("refs/heads/main".to_owned()),
			target: Some(commit_id(900)),
			detached: false,
		});
		tab.apply_ref_snapshot();
		tab.reconcile_comparison_snapshot();
		assert!(tab.view_result.is_none());
		assert!(tab.view_request.is_none());
		tab.ensure_comparison();
		assert_eq!(
			tab.view_request.as_ref().unwrap().branch_tip,
			commit_id(77)
		);
		assert_eq!(
			tab.view_request.as_ref().unwrap().base_tip,
			Some(commit_id(900))
		);
	}

	#[test]
	fn changed_explicit_base_tip_invalidates_the_old_comparison() {
		let mut tab = workspace(3);
		let branch = tab.refs[0].clone();
		let base = tab.refs[1].clone();
		tab.base_override =
			Some(BaseChoice::Reference(base.reference.clone()));
		let mut request = request(&branch, Some(&base.reference), 1);
		request.base_tip = Some(base.top_commit);
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		tab.view_result = Some(topology_view(request));

		let mut changed_base = base.clone();
		changed_base.top_commit = commit_id(88);
		tab.pending_local_refs =
			Some(vec![branch.clone(), changed_base]);
		tab.pending_remote_refs = Some(Vec::new());
		tab.local_state = super::LoadState::Loaded;
		tab.remote_state = super::LoadState::Loaded;
		tab.apply_ref_snapshot();
		tab.reconcile_comparison_snapshot();

		assert!(tab.view_result.is_none());
		assert!(tab.view_request.is_none());
		tab.ensure_comparison();
		assert_eq!(
			tab.view_request.as_ref().unwrap().base_tip,
			Some(commit_id(88))
		);
	}

	#[test]
	fn changed_head_target_invalidates_a_head_based_comparison() {
		let mut tab = workspace(3);
		let branch = tab.selected_branch().unwrap().clone();
		let old_head = HeadIdentity {
			reference: Some("refs/heads/main".to_owned()),
			target: Some(commit_id(900)),
			detached: false,
		};
		tab.head_identity = Some(old_head.clone());
		let mut request = request(&branch, None, 1);
		request.base_tip = old_head.target;
		tab.view_head_identity = Some(old_head);
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		tab.view_result = Some(topology_view(request));

		tab.pending_local_refs =
			Some((0..3).map(local_branch).collect());
		tab.pending_remote_refs = Some(Vec::new());
		tab.local_state = super::LoadState::Loaded;
		tab.remote_state = super::LoadState::Loaded;
		tab.head_identity = Some(HeadIdentity {
			reference: None,
			target: Some(commit_id(901)),
			detached: true,
		});
		tab.apply_ref_snapshot();
		tab.reconcile_comparison_snapshot();

		assert!(tab.view_result.is_none());
		assert!(tab.view_request.is_none());
		tab.ensure_comparison();
		assert_eq!(
			tab.view_request.as_ref().unwrap().base_tip,
			Some(commit_id(901))
		);
	}

	#[test]
	fn relationship_change_clears_old_diff_before_stale_completion() {
		let mut tab = workspace(2);
		tab.review_mode = true;
		let branch = tab.selected_branch().unwrap().clone();
		let old_request = request(&branch, None, 1);
		let mut old_result = topology_view(old_request.clone());
		old_result.files = vec![BranchViewFile {
			path: "branch-change.txt".to_owned(),
			status: 'M',
		}];
		tab.view_generation = 1;
		tab.view_request = Some(old_request);
		tab.view_result = Some(old_result);
		tab.diff.update(
			"branch-change.txt".to_owned(),
			false,
			review_diff(),
		);
		let params = super::DiffParams {
			path: "branch-change.txt".to_owned(),
			diff_type: super::DiffType::Commits(
				asyncgit::sync::commit_files::OldNew {
					old: commit_id(1),
					new: branch.top_commit,
				},
			),
			options: asyncgit::sync::diff::DiffOptions::default(),
		};
		tab.preview.view_mut().set_preview(
			asyncgit::diff_preview::DiffPreviewResult {
				request: asyncgit::diff_preview::DiffPreviewRequest {
					generation: 1,
					params,
					expected_native_hash: 0,
					options: asyncgit::diff_preview::PreviewOptions::default(),
				},
				preview: asyncgit::diff_preview::DiffPreview {
					renderer: asyncgit::diff_preview::PreviewRenderer::Native,
					status: asyncgit::diff_preview::PreviewStatus::Unified,
					lines: vec![asyncgit::diff_preview::PreviewLine {
						kind: asyncgit::diff_preview::PreviewLineKind::Addition,
						old_lineno: None,
						new_lineno: Some(1),
						text: "OLD REVIEW CONTENT branch-change.txt".to_owned(),
						patch_index: 0,
						changed_ranges: vec![0..3],
						replacement: None,
						block_replacement: None,
					}],
					structural: None,
					message: None,
				},
			},
			0,
		);
		assert_eq!(tab.diff.current().0, "branch-change.txt");
		assert!(tab.preview.view().has_preview());
		let old_generation = tab.preview.generation();

		let mut changed_branch = branch;
		changed_branch.top_commit = commit_id(777);
		changed_branch.top_commit_message = "changed tip".to_owned();
		tab.pending_local_refs = Some(vec![changed_branch.clone()]);
		tab.pending_remote_refs = Some(Vec::new());
		tab.local_state = super::LoadState::Loaded;
		tab.remote_state = super::LoadState::Loaded;
		tab.apply_ref_snapshot();
		tab.reconcile_comparison_snapshot();
		assert_eq!(tab.diff.current().0, "");
		assert!(tab.view_result.is_none());
		assert!(!tab.preview.view().has_preview());
		assert!(tab.preview.generation() > old_generation);

		let new_request = request(
			&changed_branch,
			None,
			tab.view_generation.wrapping_add(1),
		);
		tab.view_generation = new_request.generation;
		tab.view_request = Some(new_request.clone());
		tab.accept_branch_view(
			&new_request,
			Ok(empty_view(new_request.clone())),
		)
		.unwrap();
		assert_eq!(tab.diff.current().0, "");

		// An old async Diff completion is reconciled against the current empty
		// file list and cannot put the prior file back into the panel.
		tab.update_git(asyncgit::AsyncGitNotification::Diff)
			.unwrap();
		assert_eq!(tab.diff.current().0, "");
		let rendered = draw(&tab, 112, 30);
		let screen = (0..rendered.buffer().area.height)
			.map(|row| {
				(0..rendered.buffer().area.width)
					.map(|column| {
						rendered.buffer()[(column, row)].symbol()
					})
					.collect::<String>()
			})
			.collect::<String>();
		assert!(!screen.contains("branch-change.txt"));
		assert!(!screen.contains("review line 0"));
	}

	#[test]
	fn status_progress_notifications_do_not_invalidate_branch_comparison(
	) {
		let mut tab = workspace(2);
		let branch = tab.refs[0].clone();
		let request = request(&branch, None, 1);
		tab.view_generation = 1;
		tab.view_request = Some(request.clone());
		tab.view_result = Some(topology_view(request));
		let generations =
			(tab.local_generation, tab.remote_generation);

		for _ in 0..10 {
			tab.update_git(asyncgit::AsyncGitNotification::Status)
				.unwrap();
		}

		assert_eq!(
			(tab.local_generation, tab.remote_generation),
			generations
		);
		assert!(tab.view_result.is_some());
		assert!(!tab.local_job.is_pending());
		assert!(!tab.remote_job.is_pending());
	}

	#[test]
	fn stale_results_are_rejected_after_selection_or_base_changes() {
		let mut tab = workspace(4);
		tab.selection = 0;
		let selected = tab.selected_branch().unwrap().clone();
		let old = request(&selected, Some("refs/heads/base-old"), 1);
		tab.view_generation = 1;
		tab.view_request = Some(old.clone());

		tab.selection = 1;
		tab.view_request = None;
		tab.ensure_comparison();
		tab.accept_branch_view(&old, Ok(empty_view(old.clone())))
			.unwrap();
		assert!(tab.view_result.is_none());

		let current = tab.selected_branch().unwrap().clone();
		let stale_base =
			request(&current, Some("refs/heads/base-old"), 2);
		tab.view_generation = 2;
		tab.view_request = Some(stale_base.clone());
		tab.base_override = Some(BaseChoice::Head);
		tab.view_request = None;
		tab.ensure_comparison();
		tab.accept_branch_view(
			&stale_base,
			Ok(empty_view(stale_base.clone())),
		)
		.unwrap();
		assert!(tab.view_result.is_none());
		assert_ne!(
			tab.view_request.as_ref().unwrap().base_reference,
			Some("refs/heads/base-old".to_owned())
		);
	}

	#[test]
	fn unrelated_history_render_labels_unique_commit_counts() {
		let mut tab = workspace(2);
		let branch = tab.selected_branch().unwrap().clone();
		let request = request(&branch, Some("refs/heads/base"), 7);
		let mut result = empty_view(request.clone());
		result.request = request;
		result.ahead = Some(2);
		result.behind = Some(3);
		result.commit_count = Some(2);
		tab.view_request = Some(result.request.clone());
		tab.view_generation = result.request.generation;
		tab.view_result = Some(result);
		let rendered = draw(&tab, 120, 40);
		let header = (0..rendered.buffer().area.height)
			.map(|row| {
				(0..rendered.buffer().area.width)
					.map(|column| {
						rendered.buffer()[(column, row)].symbol()
					})
					.collect::<String>()
			})
			.collect::<String>();
		assert!(header.contains("no merge-base"));
		assert!(header.contains("branch-only 2, base-only 3"));
	}

	#[test]
	fn partial_topology_never_claims_there_is_no_merge_base() {
		let mut tab = workspace(2);
		let branch = tab.selected_branch().unwrap().clone();
		let request = request(&branch, Some("refs/heads/base"), 7);
		let mut partial = topology_view(request.clone());
		partial.request = request.clone();
		partial.merge_base = None;
		partial.ahead = None;
		partial.behind = None;
		partial.commit_count = None;
		partial.relationship_loaded = false;
		tab.view_request = Some(request);
		tab.view_generation = 7;
		tab.view_result = Some(partial);

		let screen = screen_text(&tab, 120, 40);
		assert!(screen.contains("Loading relationship"));
		assert!(screen.contains("Files pending relationship"));
		assert!(!screen.contains("no merge-base"));
	}

	#[test]
	fn relationship_failure_keeps_topology_and_reports_file_failure()
	{
		let mut tab = workspace(2);
		let branch = tab.selected_branch().unwrap().clone();
		let request = request(&branch, Some("refs/heads/base"), 8);
		let mut partial = topology_view(request.clone());
		partial.request = request.clone();
		partial.merge_base = None;
		partial.ahead = None;
		partial.behind = None;
		partial.commit_count = None;
		partial.relationship_loaded = false;
		tab.view_request = Some(request.clone());
		tab.view_generation = 8;
		tab.view_result = Some(partial.clone());
		tab.accept_branch_view(
			&request,
			Err("broken ancestor".to_owned()),
		)
		.unwrap();

		let screen = screen_text(&tab, 120, 40);
		assert!(screen.contains("topology endpoint visible"));
		assert!(
			screen.contains("Relationship failed: broken ancestor")
		);
		assert!(screen.contains("Files unavailable"));
		assert!(!screen.contains("no merge-base"));
	}

	#[test]
	fn late_partial_result_cannot_replace_exact_relationship_result()
	{
		let mut tab = workspace(2);
		let branch = tab.selected_branch().unwrap().clone();
		let request = request(&branch, Some("refs/heads/base"), 9);
		let mut final_result = empty_view(request.clone());
		final_result.merge_base = Some(commit_id(77));
		final_result.ahead = Some(4);
		final_result.behind = Some(2);
		final_result.commit_count = Some(4);
		tab.view_request = Some(request.clone());
		tab.view_generation = 9;
		tab.accept_branch_view(&request, Ok(final_result.clone()))
			.unwrap();

		let mut late_partial = topology_view(request.clone());
		late_partial.request = request.clone();
		late_partial.relationship_loaded = false;
		late_partial.ahead = None;
		late_partial.behind = None;
		late_partial.commit_count = None;
		tab.accept_branch_view(&request, Ok(late_partial)).unwrap();

		let accepted = tab.view_result.as_ref().unwrap();
		assert!(accepted.relationship_loaded);
		assert_eq!(accepted.ahead, Some(4));
		assert_eq!(accepted.behind, Some(2));
		let screen = screen_text(&tab, 120, 40);
		assert!(screen.contains("branch vs base: ↑4 ↓2"));
		assert!(!screen.contains("Loading relationship"));
	}

	#[test]
	fn ref_refresh_keeps_the_selected_ref_identity() {
		let mut tab = workspace(20);
		tab.selection = 12;
		let selected =
			tab.selected_branch().unwrap().reference.clone();
		let mut refreshed =
			(0..20).map(local_branch).collect::<Vec<_>>();
		refreshed.push(local_branch(100));
		tab.pending_local_refs = Some(refreshed);
		tab.pending_remote_refs = Some(Vec::new());
		tab.apply_ref_snapshot();
		assert_eq!(
			tab.selected_branch().unwrap().reference,
			selected
		);
	}

	#[test]
	fn remote_only_checkout_uses_the_selected_remote_ref() {
		let mut tab = workspace(0);
		tab.refs.push(remote_branch(2));
		tab.rebuild_filter(None);
		let selected = tab.selected_branch().unwrap().clone();
		let rendered = draw(&tab, 100, 30);
		let branch_row = (0..rendered.buffer().area.width)
			.map(|column| rendered.buffer()[(column, 1)].symbol())
			.collect::<String>();
		assert!(branch_row.contains("origin/release-2"));
		let checkout = tab.key_config.keys.branches_checkout;
		binding(&mut tab, checkout);
		assert!(matches!(
			tab.queue.pop(),
			Some(crate::queue::InternalEvent::CheckoutOption(branch))
				if branch.reference == selected.reference
		));
	}

	#[test]
	fn invalidated_ref_snapshot_cannot_queue_branch_mutations() {
		let mut tab = workspace(3);
		tab.invalidate_branches();
		for binding_key in [
			tab.key_config.keys.branches_checkout,
			tab.key_config.keys.rename_branch,
			tab.key_config.keys.delete_branch,
			tab.key_config.keys.rebase_branch,
		] {
			binding(&mut tab, binding_key);
		}
		assert!(tab.queue.pop().is_none());
		let mut commands = Vec::new();
		tab.commands(&mut commands, false);
		assert!(commands
			.iter()
			.filter(|command| {
				command.text.name.starts_with("Checkout")
					|| command.text.name.starts_with("Rename branch")
					|| command.text.name.starts_with("Delete branch")
			})
			.all(|command| !command.enabled));
	}
}
