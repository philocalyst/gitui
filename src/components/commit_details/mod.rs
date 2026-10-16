mod compare_details;
mod details;
mod style;

use super::{
	command_pump, event_pump, CommandBlocking, CommandInfo,
	Component, DrawableComponent, EventState, StatusTreeComponent,
};
use crate::{
	accessors,
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	strings,
};
use anyhow::Result;
use asyncgit::{
	sync::{commit_files::OldNew, CommitId, CommitTags},
	AsyncCommitFiles, AsyncDiff, CommitFilesParams, DiffParams,
	DiffType,
};
use compare_details::CompareDetailsComponent;
use crossterm::event::Event;
use details::DetailsComponent;
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	Frame,
};
use std::cell::Cell;

pub struct CommitDetailsComponent {
	commit: Option<CommitFilesParams>,
	single_details: DetailsComponent,
	compare_details: CompareDetailsComponent,
	file_tree: StatusTreeComponent,
	preview_diff: super::DiffComponent,
	git_commit_files: AsyncCommitFiles,
	git_diff: AsyncDiff,
	diff_options: SharedOptions,
	preview_area: Cell<Rect>,
	inline_preview: bool,
	visible: bool,
	key_config: SharedKeyConfig,
}

impl CommitDetailsComponent {
	accessors!(
		self,
		[single_details, compare_details, file_tree, preview_diff]
	);

	///
	pub fn new(env: &Environment) -> Self {
		Self::with_inline_preview(env, false)
	}

	/// Construct the persistent Revlog inspector with its embedded diff.
	pub fn new_with_inline_preview(env: &Environment) -> Self {
		Self::with_inline_preview(env, true)
	}

	fn with_inline_preview(
		env: &Environment,
		inline_preview: bool,
	) -> Self {
		Self {
			single_details: DetailsComponent::new(env, false),
			compare_details: CompareDetailsComponent::new(env, false),
			preview_diff: super::DiffComponent::new(env, true),
			git_commit_files: AsyncCommitFiles::new(
				env.repo.borrow().clone(),
				&env.sender_git,
			),
			git_diff: AsyncDiff::new(
				env.repo.borrow().clone(),
				&env.sender_git,
			),
			diff_options: env.options.clone(),
			preview_area: Cell::new(Rect::default()),
			inline_preview,
			file_tree: StatusTreeComponent::new(env, "", false),
			visible: false,
			commit: None,
			key_config: env.key_config.clone(),
		}
	}

	fn get_files_title(&self) -> String {
		let files_count = self.file_tree.file_count();

		format!(
			"{} {}",
			strings::commit::details_files_title(&self.key_config),
			files_count
		)
	}

	///
	pub fn set_commits(
		&mut self,
		params: Option<CommitFilesParams>,
		tags: Option<&CommitTags>,
	) -> Result<()> {
		if params.is_none() {
			self.single_details.set_commit(None, None);
			self.compare_details.set_commits(None);
			self.file_tree.clear()?;
			self.file_tree.set_title(self.get_files_title());
			self.preview_diff.clear(false);
		}

		self.commit = params;

		if let Some(id) = params {
			self.file_tree.set_commit(Some(id.id));

			if let Some(other) = id.other {
				self.single_details.set_commit(None, None);
				self.compare_details.set_commits(Some(OldNew {
					new: id.id,
					old: other,
				}));
			} else {
				self.single_details
					.set_commit(Some(id.id), tags.cloned());
			}

			if let Some((fetched_id, res)) =
				self.git_commit_files.current()?
			{
				if fetched_id == id {
					self.file_tree.update(res.as_slice())?;
					self.file_tree.set_title(self.get_files_title());
					self.update_selected_diff()?;

					return Ok(());
				}
			}

			self.file_tree.clear()?;
			self.preview_diff
				.clear(self.inline_preview && id.other.is_none());
			self.git_commit_files.fetch(id)?;
		}

		self.file_tree.set_title(self.get_files_title());

		Ok(())
	}

	/// Select a commit and keep any tags already loaded for it in the list.
	pub fn set_selected_commit(
		&mut self,
		id: Option<CommitId>,
		tags: Option<&CommitTags>,
	) -> Result<()> {
		self.set_commits(id.map(CommitFilesParams::from), tags)
	}

	/// Pump exact-source structural diff metadata after its async notification.
	pub fn update_preview(&mut self) {
		self.preview_diff.update_preview();
	}

	/// Apply completed native projection/highlighting work without refetching.
	pub fn update_rendering(&mut self) {
		self.preview_diff.update_rendering();
	}

	fn update_selected_diff(&mut self) -> Result<()> {
		if !self.inline_preview {
			return Ok(());
		}
		let Some(commit) = self.commit else {
			self.preview_diff.clear(false);
			self.preview_diff.update_preview();
			return Ok(());
		};
		let Some(file) = self.file_tree.selection_file() else {
			self.preview_diff.clear(false);
			self.preview_diff.update_preview();
			return Ok(());
		};
		let params = DiffParams {
			path: file.path,
			diff_type: commit.other.map_or(
				DiffType::Commit(commit.id),
				|old| {
					DiffType::Commits(OldNew {
						old,
						new: commit.id,
					})
				},
			),
			options: self.diff_options.borrow().diff_options(),
		};
		if let Some(diff) = self.git_diff.request(params.clone())? {
			self.preview_diff.update_with_params(
				params.path.clone(),
				false,
				diff,
				Some(params),
			);
		} else if let Some((loaded_params, diff)) =
			self.git_diff.last()?
		{
			if loaded_params == params {
				self.preview_diff.update_with_params(
					params.path.clone(),
					false,
					diff,
					Some(params),
				);
			} else {
				self.preview_diff.clear(true);
			}
		} else {
			self.preview_diff.clear(true);
		}
		self.preview_diff.update_preview();
		Ok(())
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.git_commit_files.is_pending()
			|| (self.inline_preview && self.git_diff.is_pending())
			|| self.single_details.any_work_pending()
	}

	///
	pub const fn files(&self) -> &StatusTreeComponent {
		&self.file_tree
	}

	fn details_focused(&self) -> bool {
		self.single_details.focused()
			|| self.compare_details.focused()
	}

	fn set_details_focus(&mut self, focus: bool) {
		if self.is_compare() {
			self.compare_details.focus(focus);
		} else {
			self.single_details.focus(focus);
		}
	}

	fn is_compare(&self) -> bool {
		self.commit.is_some_and(|p| p.other.is_some())
	}
}

impl DrawableComponent for CommitDetailsComponent {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if !self.visible {
			return Ok(());
		}

		let constraints = if self.is_compare() {
			[Constraint::Length(10), Constraint::Min(0)]
		} else {
			let max_details_height = rect.height.saturating_sub(4);
			let details_height = self
				.single_details
				.preferred_height(rect.width, max_details_height);
			[Constraint::Length(details_height), Constraint::Min(0)]
		};

		let chunks = Layout::default()
			.direction(Direction::Vertical)
			.constraints(constraints.as_ref())
			.split(rect);

		if self.is_compare() {
			self.preview_area.set(Rect::default());
			self.compare_details.draw(f, chunks[0])?;
			self.file_tree.draw(f, chunks[1])?;
		} else {
			self.single_details.draw(f, chunks[0])?;
			if !self.inline_preview {
				self.preview_area.set(Rect::default());
				self.file_tree.draw(f, chunks[1])?;
			} else if rect.width >= 78 {
				let lower = Layout::default()
					.direction(Direction::Horizontal)
					.constraints([
						Constraint::Percentage(34),
						Constraint::Percentage(66),
					])
					.split(chunks[1]);
				self.preview_area.set(lower[1]);
				self.file_tree.draw(f, lower[0])?;
				self.preview_diff.draw(f, lower[1])?;
			} else {
				let lower_height = chunks[1].height;
				let max_file_height = lower_height
					.saturating_sub(4)
					.min((lower_height / 2).max(3));
				let desired_file_height =
					u16::try_from(self.file_tree.file_count())
						.unwrap_or(u16::MAX)
						.saturating_add(2)
						.max(3);
				let file_height =
					desired_file_height.min(max_file_height);
				let lower = Layout::default()
					.direction(Direction::Vertical)
					.constraints([
						Constraint::Length(file_height),
						Constraint::Min(0),
					])
					.split(chunks[1]);
				self.preview_area.set(lower[1]);
				self.file_tree.draw(f, lower[0])?;
				self.preview_diff.draw(f, lower[1])?;
			}
		}

		Ok(())
	}
}

impl Component for CommitDetailsComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible || force_all {
			command_pump(
				out,
				force_all,
				self.components().as_slice(),
			);
		}

		CommandBlocking::PassingOn
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if let Event::Mouse(mouse) = ev {
			let preview_area = self.preview_area.get();
			let in_preview = !self.is_compare()
				&& preview_area.width > 0
				&& preview_area.height > 0
				&& preview_area
					.contains((mouse.column, mouse.row).into());
			if in_preview
				&& matches!(
					mouse.kind,
					crossterm::event::MouseEventKind::Down(
						crossterm::event::MouseButton::Left
					)
						| crossterm::event::MouseEventKind::ScrollUp
						| crossterm::event::MouseEventKind::ScrollDown
				) {
				self.set_details_focus(false);
				self.file_tree.focus(false);
				self.preview_diff.focus(true);
			}
		}
		// In modal navigation, the enclosing tab owns Tab/BackTab for its
		// outer panes. Explicit up/down focus bindings move between metadata,
		// files, and the native diff without stealing ordinary j/k.
		if self.focused() {
			if let Event::Key(e) = ev {
				if self.key_config.is_modal() {
					if key_match(e, self.key_config.keys.focus_down)
						&& self.details_focused()
					{
						self.set_details_focus(false);
						self.file_tree.focus(true);
						return Ok(EventState::Consumed);
					}
					if self.inline_preview
						&& key_match(
							e,
							self.key_config.keys.focus_down,
						) && self.file_tree.focused()
					{
						self.file_tree.focus(false);
						self.preview_diff.focus(true);
						return Ok(EventState::Consumed);
					}
					if key_match(e, self.key_config.keys.focus_up)
						&& self.preview_diff.focused()
					{
						self.preview_diff.focus(false);
						self.file_tree.focus(true);
						return Ok(EventState::Consumed);
					}
					if key_match(e, self.key_config.keys.focus_up)
						&& self.file_tree.focused()
						&& !self.is_compare()
					{
						self.file_tree.focus(false);
						self.set_details_focus(true);
						return Ok(EventState::Consumed);
					}
				} else if key_match(e, self.key_config.keys.move_down)
					&& self.details_focused()
				{
					self.set_details_focus(false);
					self.file_tree.focus(true);
					return Ok(EventState::Consumed);
				} else if self.inline_preview
					&& key_match(e, self.key_config.keys.move_down)
					&& self.file_tree.focused()
				{
					self.file_tree.focus(false);
					self.preview_diff.focus(true);
					return Ok(EventState::Consumed);
				} else if key_match(e, self.key_config.keys.move_up)
					&& self.preview_diff.focused()
				{
					self.preview_diff.focus(false);
					self.file_tree.focus(true);
					return Ok(EventState::Consumed);
				} else if key_match(e, self.key_config.keys.move_up)
					&& self.file_tree.focused()
					&& !self.is_compare()
				{
					self.file_tree.focus(false);
					self.set_details_focus(true);
					return Ok(EventState::Consumed);
				}
			}
		}

		let event_state =
			event_pump(ev, self.components_mut().as_mut_slice())?;
		self.update_selected_diff()?;
		if event_state.is_consumed() {
			if !self.file_tree.is_visible() {
				self.hide();
			}

			return Ok(EventState::Consumed);
		}

		Ok(EventState::NotConsumed)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}
	fn hide(&mut self) {
		self.visible = false;
	}
	fn show(&mut self) -> Result<()> {
		self.visible = true;
		self.file_tree.show()?;
		Ok(())
	}

	fn focused(&self) -> bool {
		self.details_focused()
			|| self.file_tree.focused()
			|| (self.inline_preview && self.preview_diff.focused())
	}

	fn focus(&mut self, focus: bool) {
		self.single_details.focus(false);
		self.compare_details.focus(false);
		self.preview_diff.focus(false);
		self.file_tree.focus(focus);
		self.file_tree.show_selection(true);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::app::Environment;
	use crossterm::event::{
		Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
	};
	use ratatui::{backend::TestBackend, Terminal};

	#[test]
	fn clicking_embedded_diff_focuses_read_only_native_view() {
		let env = Environment::test_env();
		let mut component =
			CommitDetailsComponent::new_with_inline_preview(&env);
		component.show().unwrap();

		let backend = TestBackend::new(100, 32);
		let mut terminal = Terminal::new(backend).unwrap();
		terminal
			.draw(|frame| {
				component.draw(frame, frame.area()).unwrap();
			})
			.unwrap();

		let area = component.preview_area.get();
		component
			.event(&Event::Mouse(MouseEvent {
				kind: MouseEventKind::Down(MouseButton::Left),
				column: area.x + 2,
				row: area.y + 2,
				modifiers: KeyModifiers::NONE,
			}))
			.unwrap();

		assert!(component.preview_diff.focused());
		assert!(!component.file_tree.focused());
		let mut commands = Vec::new();
		component.preview_diff.commands(&mut commands, true);
		assert!(commands.iter().all(|command| {
			!command.text.name.starts_with("Stage")
				&& !command.text.name.starts_with("Reset")
		}));
	}

	#[test]
	fn popup_default_has_no_embedded_diff_or_diff_job() {
		let (dir, repo) = git2_testing::repo_init();
		let id =
			CommitId::from(repo.head().unwrap().target().unwrap());
		let env = Environment::test_env();
		env.repo.replace(asyncgit::sync::RepoPath::Path(
			dir.path().to_path_buf(),
		));
		let mut component = CommitDetailsComponent::new(&env);
		component.show().unwrap();
		component
			.set_commits(Some(CommitFilesParams::from(id)), None)
			.unwrap();
		let deadline = std::time::Instant::now()
			+ std::time::Duration::from_secs(5);
		loop {
			component
				.set_commits(Some(CommitFilesParams::from(id)), None)
				.unwrap();
			if component.git_commit_files.current().unwrap().is_some()
			{
				break;
			}
			assert!(
				std::time::Instant::now() < deadline,
				"commit files did not arrive"
			);
			std::thread::sleep(std::time::Duration::from_millis(5));
		}

		let backend = TestBackend::new(100, 32);
		let mut terminal = Terminal::new(backend).unwrap();
		terminal
			.draw(|frame| {
				component.draw(frame, frame.area()).unwrap();
			})
			.unwrap();

		assert!(!component.inline_preview);
		assert_eq!(component.preview_area.get(), Rect::default());
		assert!(!component.git_diff.is_pending());
		assert!(!component.any_work_pending());
	}
}
