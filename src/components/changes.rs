use super::{
	status_tree::StatusTreeComponent,
	utils::filetree::{FileTreeItem, FileTreeItemKind},
	CommandBlocking, DrawableComponent,
};
use crate::{
	app::Environment,
	components::{CommandInfo, Component, EventState},
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	queue::{Action, InternalEvent, NeedsUpdate, Queue, ResetItem},
	strings, try_or_popup,
};
use anyhow::Result;
use asyncgit::{
	sync::{self, RepoPathRef},
	StatusItem, StatusItemType,
};
use crossterm::event::Event;
use ratatui::{layout::Rect, Frame};
use std::path::Path;

///
pub struct ChangesComponent {
	repo: RepoPathRef,
	files: StatusTreeComponent,
	is_working_dir: bool,
	queue: Queue,
	key_config: SharedKeyConfig,
	options: SharedOptions,
}

impl ChangesComponent {
	///
	pub fn new(
		env: &Environment,
		title: &str,
		focus: bool,
		is_working_dir: bool,
	) -> Self {
		Self {
			files: StatusTreeComponent::new(env, title, focus),
			is_working_dir,
			queue: env.queue.clone(),
			key_config: env.key_config.clone(),
			options: env.options.clone(),
			repo: env.repo.clone(),
		}
	}

	///
	pub fn set_items(&mut self, list: &[StatusItem]) -> Result<()> {
		self.files.show()?;
		self.files.update(list)?;
		Ok(())
	}

	/// Keep the nested tree's loading view aligned with the async status scan.
	pub const fn set_pending(&mut self, pending: bool) {
		self.files.set_pending(pending);
	}

	///
	pub fn selection(&self) -> Option<FileTreeItem> {
		self.files.selection()
	}

	///
	pub fn focus_select(&mut self, focus: bool) {
		self.files.focus(focus);
		self.files.show_selection(focus);
	}

	/// returns true if list is empty
	pub const fn is_empty(&self) -> bool {
		self.files.is_empty()
	}

	///
	pub fn is_file_selected(&self) -> bool {
		self.files.is_file_selected()
	}

	fn index_add_remove(&self) -> Result<bool> {
		let marked =
			self.files.selected_paths().cloned().collect::<Vec<_>>();
		if marked.is_empty()
			&& self.files.is_pending()
			&& self.selection().is_some_and(|item| {
				matches!(item.kind, FileTreeItemKind::Path(_))
			}) {
			self.queue.push(InternalEvent::ShowErrorMsg(
				"Folder stage or unstage is unavailable while the status scan is loading".to_string(),
			));
			return Ok(true);
		}
		let files = if marked.is_empty() {
			self.files.selection_file_items()
		} else {
			marked
				.iter()
				.map(|path| {
					self.files
						.status_for_path(path)
						.map(|status| StatusItem {
							path: path.clone(),
							status,
						})
						.ok_or_else(|| {
							anyhow::anyhow!(
							"selected file is no longer in the status list: {path:?}"
						)
						})
				})
				.collect::<Result<Vec<_>>>()?
		};
		if files.is_empty() {
			return Ok(false);
		}

		if self.is_working_dir {
			let mut staged_paths = Vec::new();
			for file in &files {
				if file.status == StatusItemType::Deleted {
					sync::stage_addremoved(
						&self.repo.borrow(),
						Path::new(&file.path),
					)?;
				} else {
					staged_paths.push(file.path.as_str());
				}
			}
			if !staged_paths.is_empty() {
				sync::stage_add_paths(
					&self.repo.borrow(),
					&staged_paths,
				)?;
			}
		} else {
			let paths = files
				.iter()
				.map(|file| file.path.as_str())
				.collect::<Vec<_>>();
			sync::reset_stage_paths(&self.repo.borrow(), &paths)?;
		}

		Ok(true)
	}

	fn index_add_all(&self) -> Result<()> {
		let config = self.options.borrow().status_show_untracked();

		sync::stage_add_all(&self.repo.borrow(), "*", config)?;

		self.queue.push(InternalEvent::Update(NeedsUpdate::ALL));

		Ok(())
	}

	fn stage_remove_all(&self) -> Result<()> {
		sync::reset_stage(&self.repo.borrow(), "*")?;

		self.queue.push(InternalEvent::Update(NeedsUpdate::ALL));

		Ok(())
	}

	fn dispatch_reset_workdir(&self) -> bool {
		let selected =
			self.files.selected_paths().cloned().collect::<Vec<_>>();
		if selected.len() > 1 {
			self.queue.push(InternalEvent::ShowErrorMsg(
				"reset is disabled for multiple selected files; select one file or clear selection".to_string(),
			));
			return true;
		}
		if let Some(tree_item) = self.selection() {
			let path = selected
				.first()
				.cloned()
				.unwrap_or(tree_item.info.full_path);
			self.queue.push(InternalEvent::ConfirmAction(
				Action::Reset(ResetItem { path }),
			));

			return true;
		}
		false
	}

	fn add_to_ignore(&self) -> bool {
		let selected = self.files.selected_paths().next().cloned();
		if let Some(path) = selected.or_else(|| {
			self.selection().map(|item| item.info.full_path)
		}) {
			if let Err(e) =
				sync::add_to_ignore(&self.repo.borrow(), &path)
			{
				self.queue.push(InternalEvent::ShowErrorMsg(
					format!("ignore error:\n{e}\nfile:\n{path:?}"),
				));
			} else {
				self.queue
					.push(InternalEvent::Update(NeedsUpdate::ALL));

				return true;
			}
		}

		false
	}
}

impl DrawableComponent for ChangesComponent {
	fn draw(&self, f: &mut Frame, r: Rect) -> Result<()> {
		self.files.draw(f, r)?;

		Ok(())
	}
}

impl Component for ChangesComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		self.files.commands(out, force_all);

		let some_selection = self.selection().is_some();
		let can_index_selection = some_selection
			&& (!self.files.is_pending()
				|| self.files.is_file_selected()
				|| self.files.has_selection());

		if self.is_working_dir {
			out.push(CommandInfo::new(
				strings::commands::stage_all(&self.key_config),
				true,
				some_selection && self.focused(),
			));
			out.push(CommandInfo::new(
				strings::commands::stage_item(&self.key_config),
				can_index_selection,
				can_index_selection && self.focused(),
			));
			out.push(CommandInfo::new(
				strings::commands::reset_item(&self.key_config),
				true,
				some_selection && self.focused(),
			));
			out.push(CommandInfo::new(
				strings::commands::ignore_item(&self.key_config),
				true,
				some_selection && self.focused(),
			));
		} else {
			out.push(CommandInfo::new(
				strings::commands::unstage_item(&self.key_config),
				can_index_selection,
				can_index_selection && self.focused(),
			));
			out.push(CommandInfo::new(
				strings::commands::unstage_all(&self.key_config),
				true,
				some_selection && self.focused(),
			));
		}

		CommandBlocking::PassingOn
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.files.event(ev)?.is_consumed() {
			return Ok(EventState::Consumed);
		}

		if self.focused() {
			if let Event::Key(e) = ev {
				return if !self.is_working_dir
					&& key_match(
						e,
						self.key_config.keys.status_unstage_item,
					) {
					try_or_popup!(
						self,
						"unstaging error:",
						self.index_add_remove()
					);
					self.queue.push(InternalEvent::Update(
						NeedsUpdate::ALL,
					));
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.stage_unstage_item,
				) {
					try_or_popup!(
						self,
						"staging error:",
						self.index_add_remove()
					);

					self.queue.push(InternalEvent::Update(
						NeedsUpdate::ALL,
					));
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.status_stage_all,
				) && !self.is_empty()
				{
					if self.is_working_dir {
						try_or_popup!(
							self,
							"staging all error:",
							self.index_add_all()
						);
					} else {
						self.stage_remove_all()?;
					}
					self.queue
						.push(InternalEvent::StatusLastFileMoved);
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.status_reset_item,
				) && self.is_working_dir
				{
					Ok(self.dispatch_reset_workdir().into())
				} else if key_match(
					e,
					self.key_config.keys.status_ignore_file,
				) && self.is_working_dir
					&& !self.is_empty()
				{
					if self.files.selected_paths().count() > 1 {
						self.queue.push(InternalEvent::ShowErrorMsg(
							"ignore is disabled for multiple selected files; select one file or clear selection".to_string(),
						));
						return Ok(EventState::Consumed);
					}
					Ok(self.add_to_ignore().into())
				} else {
					Ok(EventState::NotConsumed)
				};
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn focused(&self) -> bool {
		self.files.focused()
	}

	fn selection_status(
		&self,
	) -> Option<crate::components::SelectionStatus> {
		self.files.selection_status()
	}

	fn focus(&mut self, focus: bool) {
		self.files.focus(focus);
	}

	fn is_visible(&self) -> bool {
		self.files.is_visible()
	}

	fn hide(&mut self) {
		self.files.hide();
	}

	fn show(&mut self) -> Result<()> {
		self.files.show()?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		app::Environment,
		keys::{KeyConfig, Keymap},
	};
	use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
	use std::{fs, path::Path as FsPath, process::Command, rc::Rc};

	fn key(code: KeyCode) -> Event {
		Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
	}

	fn cached_paths(repo_path: &FsPath) -> Vec<Vec<u8>> {
		let output = Command::new("git")
			.arg("diff")
			.arg("--cached")
			.arg("--name-only")
			.arg("-z")
			.current_dir(repo_path)
			.output()
			.unwrap();
		assert!(output.status.success());
		output
			.stdout
			.split(|byte| *byte == 0)
			.filter(|path| !path.is_empty())
			.map(<[u8]>::to_vec)
			.collect()
	}

	fn path_bytes(
		paths: impl IntoIterator<Item = String>,
	) -> Vec<Vec<u8>> {
		let mut paths = paths
			.into_iter()
			.map(String::into_bytes)
			.collect::<Vec<_>>();
		paths.sort();
		paths
	}

	#[test]
	fn selected_exact_paths_stage_and_unstage_without_touching_glob_sibling(
	) {
		let (temp, _repo) = git2_testing::repo_init_empty();
		let paths = [
			"[brackets].txt",
			"file*.txt",
			"file?.txt",
			"fileX.txt",
			"line\nname.txt",
		];
		let mut paths = paths
			.iter()
			.map(|path| (*path).to_string())
			.collect::<Vec<_>>();
		paths.sort();
		for path in &paths {
			fs::write(temp.path().join(path), "content").unwrap();
		}
		let statuses = paths
			.iter()
			.map(|path| StatusItem {
				path: path.clone(),
				status: StatusItemType::New,
			})
			.collect::<Vec<_>>();

		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		env.repo.replace(temp.path().to_path_buf().into());
		let mut workdir =
			ChangesComponent::new(&env, "workdir", true, true);
		workdir.set_items(&statuses).unwrap();
		workdir.event(&key(KeyCode::Char('u'))).unwrap();
		assert!(cached_paths(temp.path()).is_empty());
		for event in [
			key(KeyCode::Char('x')),
			key(KeyCode::Char('j')),
			key(KeyCode::Char('x')),
			key(KeyCode::Enter),
		] {
			workdir.event(&event).unwrap();
		}
		assert!(
			cached_paths(temp.path()).is_empty(),
			"Enter must not stage marked files in the worktree pane"
		);
		workdir.event(&key(KeyCode::Char('s'))).unwrap();
		let selected = workdir
			.files
			.selected_paths()
			.cloned()
			.collect::<Vec<_>>();
		assert_eq!(selected.len(), 2);
		assert_eq!(
			cached_paths(temp.path()),
			path_bytes(selected.clone())
		);
		assert!(!cached_paths(temp.path())
			.iter()
			.any(|path| path == b"fileX.txt"));

		// Stage every sibling, then ensure Enter does not unstage the marked
		// paths. Helix's explicit `u` action should unstage only those paths.
		for path in &paths {
			sync::stage_add_file(&env.repo.borrow(), Path::new(path))
				.unwrap();
		}
		let staged_statuses = paths
			.iter()
			.map(|path| StatusItem {
				path: path.clone(),
				status: StatusItemType::New,
			})
			.collect::<Vec<_>>();
		let mut index =
			ChangesComponent::new(&env, "index", true, false);
		index.set_items(&staged_statuses).unwrap();
		for event in [
			key(KeyCode::Char('x')),
			key(KeyCode::Char('j')),
			key(KeyCode::Char('x')),
			key(KeyCode::Enter),
		] {
			index.event(&event).unwrap();
		}
		assert_eq!(
			cached_paths(temp.path()),
			path_bytes(paths.clone())
		);
		index.event(&key(KeyCode::Char('u'))).unwrap();
		let remaining = paths.into_iter().skip(2).collect::<Vec<_>>();
		assert_eq!(cached_paths(temp.path()), path_bytes(remaining));
	}

	#[test]
	fn literal_folder_selection_stages_and_unstages_only_its_descendants(
	) {
		let (temp, _repo) = git2_testing::repo_init_empty();
		let folder_file = "[x]/inside.txt";
		let sibling_file = "x/sibling.txt";
		for path in [folder_file, sibling_file] {
			fs::create_dir_all(
				temp.path().join(path).parent().unwrap(),
			)
			.unwrap();
			fs::write(temp.path().join(path), "changed").unwrap();
		}
		let statuses = [folder_file, sibling_file]
			.into_iter()
			.map(|path| StatusItem {
				path: path.to_string(),
				status: StatusItemType::New,
			})
			.collect::<Vec<_>>();

		let mut env = Environment::test_env();
		env.key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		env.repo.replace(temp.path().to_path_buf().into());
		let mut workdir =
			ChangesComponent::new(&env, "workdir", true, true);
		workdir.set_items(&statuses).unwrap();
		assert_eq!(
			workdir.selection().unwrap().info.full_path,
			"[x]"
		);
		workdir.set_pending(true);
		workdir.event(&key(KeyCode::Char('s'))).unwrap();
		assert!(
			cached_paths(temp.path()).is_empty(),
			"cursor-folder staging must wait for the full status scan"
		);
		workdir.set_pending(false);
		workdir.event(&key(KeyCode::Char('s'))).unwrap();
		assert_eq!(
			cached_paths(temp.path()),
			path_bytes([folder_file.into()])
		);

		// Stage the similarly named sibling as well, then unstage the selected
		// literal folder. Its component path must not match the `x` directory.
		sync::stage_add_file(
			&env.repo.borrow(),
			Path::new(sibling_file),
		)
		.unwrap();
		let staged_statuses = statuses
			.iter()
			.map(|item| StatusItem {
				path: item.path.clone(),
				status: StatusItemType::Modified,
			})
			.collect::<Vec<_>>();
		let mut index =
			ChangesComponent::new(&env, "index", true, false);
		index.set_items(&staged_statuses).unwrap();
		assert_eq!(index.selection().unwrap().info.full_path, "[x]");
		index.set_pending(true);
		index.event(&key(KeyCode::Char('u'))).unwrap();
		assert_eq!(
			cached_paths(temp.path()),
			path_bytes([folder_file.into(), sibling_file.into()]),
			"cursor-folder unstaging must wait for the full status scan"
		);
		index.set_pending(false);
		index.event(&key(KeyCode::Char('u'))).unwrap();
		assert_eq!(
			cached_paths(temp.path()),
			path_bytes([sibling_file.into()])
		);
	}
}
