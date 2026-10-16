use std::path::{Path, PathBuf};

use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		DrawableComponent, EventState, RevisionFilesComponent,
		SelectionStatus,
	},
	keys::{key_match, SharedKeyConfig},
	AsyncNotification,
};
use anyhow::Result;
use asyncgit::sync::{self, RepoPathRef};

pub struct FilesTab {
	repo: RepoPathRef,
	visible: bool,
	files: RevisionFilesComponent,
	key_config: SharedKeyConfig,
}

impl FilesTab {
	///
	pub fn new(
		env: &Environment,
		select_file: Option<PathBuf>,
	) -> Self {
		Self {
			visible: false,
			files: RevisionFilesComponent::new(env, select_file),
			key_config: env.key_config.clone(),
			repo: env.repo.clone(),
		}
	}

	///
	pub fn update(&mut self) -> Result<()> {
		if self.is_visible() {
			if let Ok(head) = sync::get_head(&self.repo.borrow()) {
				self.files.set_commit(head)?;
			}
		}

		Ok(())
	}

	///
	pub fn anything_pending(&self) -> bool {
		self.files.any_work_pending()
	}

	///
	pub fn update_async(
		&mut self,
		ev: AsyncNotification,
	) -> Result<()> {
		if self.is_visible() {
			self.files.update(ev)?;
		}

		Ok(())
	}

	pub fn file_finder_update(&mut self, file: &Path) {
		self.files.find_file(file);
	}
}

impl DrawableComponent for FilesTab {
	fn draw(
		&self,
		f: &mut ratatui::Frame,
		rect: ratatui::layout::Rect,
	) -> Result<()> {
		if self.is_visible() {
			self.files.draw(f, rect)?;
		}
		Ok(())
	}
}

impl Component for FilesTab {
	fn selection_status(&self) -> Option<SelectionStatus> {
		// Revision files are a read-only tree/file viewer and do not expose
		// modal range selection. Keep the footer quiet while this tab owns it.
		None
	}

	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible || force_all {
			return self.files.commands(out, force_all);
		}

		visibility_blocking(self)
	}

	fn event(
		&mut self,
		ev: &crossterm::event::Event,
	) -> Result<EventState> {
		if self.visible {
			if self.key_config.is_modal() {
				if let crossterm::event::Event::Key(k) = ev {
					let forward = key_match(
						k,
						self.key_config.keys.focus_right,
					) || key_match(
						k,
						self.key_config.keys.focus_next,
					);
					let backward =
						key_match(k, self.key_config.keys.focus_left)
							|| key_match(
								k,
								self.key_config.keys.focus_previous,
							);
					let tree_focused = self.files.tree_focused();
					if (forward && tree_focused)
						|| (backward && !tree_focused)
					{
						self.files.focus_next();
						return Ok(EventState::Consumed);
					}
					if key_match(k, self.key_config.keys.focus_next)
						|| key_match(
							k,
							self.key_config.keys.focus_previous,
						) {
						self.files.focus_next();
						return Ok(EventState::Consumed);
					}
				}
			}
			return self.files.event(ev);
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
		self.files.focus_tree();
		self.update()?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		app::Environment,
		components::Component,
		keys::{GituiKeyEvent, KeyConfig, Keymap},
	};
	use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
	use std::rc::Rc;

	fn key(
		tab: &mut FilesTab,
		code: KeyCode,
		modifiers: KeyModifiers,
	) {
		tab.event(&Event::Key(KeyEvent::new(code, modifiers)))
			.unwrap();
	}

	#[test]
	fn modal_focus_keys_round_trip_between_tree_and_file() {
		let mut env = Environment::test_env();
		let mut config = KeyConfig::with_keymap(Keymap::Helix);
		config.keys.focus_right =
			GituiKeyEvent::new(KeyCode::F(5), KeyModifiers::NONE);
		config.keys.focus_left =
			GituiKeyEvent::new(KeyCode::F(6), KeyModifiers::NONE);
		config.keys.focus_next =
			GituiKeyEvent::new(KeyCode::F(7), KeyModifiers::NONE);
		config.keys.focus_previous =
			GituiKeyEvent::new(KeyCode::F(8), KeyModifiers::NONE);
		env.key_config = Rc::new(config);
		let mut tab = FilesTab::new(&env, None);
		tab.show().unwrap();
		assert!(tab.files.tree_focused());

		key(&mut tab, KeyCode::F(5), KeyModifiers::NONE);
		assert!(!tab.files.tree_focused());
		key(&mut tab, KeyCode::F(6), KeyModifiers::NONE);
		assert!(tab.files.tree_focused());
		key(&mut tab, KeyCode::F(7), KeyModifiers::NONE);
		assert!(!tab.files.tree_focused());
		key(&mut tab, KeyCode::F(8), KeyModifiers::NONE);
		assert!(tab.files.tree_focused());
	}

	#[test]
	fn legacy_profile_does_not_interpret_tab_as_pane_focus() {
		let env = Environment::test_env();
		let mut tab = FilesTab::new(&env, None);
		tab.show().unwrap();
		key(&mut tab, KeyCode::Tab, KeyModifiers::NONE);
		assert!(tab.files.tree_focused());
	}
}
