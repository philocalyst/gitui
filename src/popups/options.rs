use crate::{
	app::Environment,
	components::{
		string_width_align, visibility_blocking, CommandBlocking,
		CommandInfo, Component, DrawableComponent, EventState,
	},
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	queue::{InternalEvent, Queue},
	strings,
	ui::{self, style::SharedTheme},
};
use anyhow::Result;
use asyncgit::sync::ShowUntrackedFilesConfig;
use crossterm::event::{
	Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::{
	layout::{Alignment, Rect},
	style::{Modifier, Style},
	text::{Line, Span},
	widgets::{Block, Borders, Clear, Paragraph},
	Frame,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AppOption {
	StatusShowUntracked,
	DiffIgnoreWhitespaces,
	DiffContextLines,
	DiffInterhunkLines,
	DiffPreviewRenderer,
}

pub struct OptionsPopup {
	selection: AppOption,
	queue: Queue,
	visible: bool,
	key_config: SharedKeyConfig,
	options: SharedOptions,
	theme: SharedTheme,
	content_area: std::cell::Cell<Rect>,
}

impl OptionsPopup {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			selection: AppOption::StatusShowUntracked,
			queue: env.queue.clone(),
			visible: false,
			key_config: env.key_config.clone(),
			options: env.options.clone(),
			theme: env.theme.clone(),
			content_area: std::cell::Cell::new(Rect::default()),
		}
	}

	fn get_text(&self, width: u16) -> Vec<Line<'_>> {
		let mut txt: Vec<Line> = Vec::with_capacity(10);

		self.add_status(&mut txt, width);

		txt
	}

	fn add_status(&self, txt: &mut Vec<Line>, width: u16) {
		Self::add_header(txt, "Status");

		self.add_entry(
			txt,
			width,
			"Show untracked",
			match self.options.borrow().status_show_untracked() {
				None => "Gitconfig",
				Some(ShowUntrackedFilesConfig::No) => "No",
				Some(ShowUntrackedFilesConfig::Normal) => "Normal",
				Some(ShowUntrackedFilesConfig::All) => "All",
			},
			self.is_select(AppOption::StatusShowUntracked),
		);
		Self::add_header(txt, "");

		let diff = self.options.borrow().diff_options();
		Self::add_header(txt, "Diff");
		self.add_entry(
			txt,
			width,
			"Ignore whitespaces",
			&diff.ignore_whitespace.to_string(),
			self.is_select(AppOption::DiffIgnoreWhitespaces),
		);
		self.add_entry(
			txt,
			width,
			"Context lines",
			&diff.context.to_string(),
			self.is_select(AppOption::DiffContextLines),
		);
		self.add_entry(
			txt,
			width,
			"Inter hunk lines",
			&diff.interhunk_lines.to_string(),
			self.is_select(AppOption::DiffInterhunkLines),
		);
		let renderer =
			match self.options.borrow().diff_preview_preference() {
				crate::options::DiffPreviewPreference::Auto => {
					"Inline suggestions"
				}
				crate::options::DiffPreviewPreference::Unified => {
					"Plain unified"
				}
			};
		self.add_entry(
			txt,
			width,
			"Review presentation",
			renderer,
			self.is_select(AppOption::DiffPreviewRenderer),
		);
	}

	fn is_select(&self, kind: AppOption) -> bool {
		self.selection == kind
	}

	fn add_header(txt: &mut Vec<Line>, header: &'static str) {
		txt.push(Line::from(vec![Span::styled(
			header,
			//TODO: use style
			Style::default().add_modifier(Modifier::UNDERLINED),
		)]));
	}

	fn add_entry(
		&self,
		txt: &mut Vec<Line>,
		width: u16,
		entry: &'static str,
		value: &str,
		selected: bool,
	) {
		let half = usize::from(width / 2);
		txt.push(Line::from(vec![
			Span::styled(
				string_width_align(entry, half),
				self.theme.text(true, false),
			),
			Span::styled(
				format!("{value:^half$}"),
				self.theme.text(true, selected),
			),
		]));
	}

	const fn move_selection(&mut self, up: bool) {
		if up {
			self.selection = match self.selection {
				AppOption::StatusShowUntracked => {
					AppOption::DiffPreviewRenderer
				}
				AppOption::DiffIgnoreWhitespaces => {
					AppOption::StatusShowUntracked
				}
				AppOption::DiffContextLines => {
					AppOption::DiffIgnoreWhitespaces
				}
				AppOption::DiffInterhunkLines => {
					AppOption::DiffContextLines
				}
				AppOption::DiffPreviewRenderer => {
					AppOption::DiffInterhunkLines
				}
			};
		} else {
			self.selection = match self.selection {
				AppOption::StatusShowUntracked => {
					AppOption::DiffIgnoreWhitespaces
				}
				AppOption::DiffIgnoreWhitespaces => {
					AppOption::DiffContextLines
				}
				AppOption::DiffContextLines => {
					AppOption::DiffInterhunkLines
				}
				AppOption::DiffInterhunkLines
				| AppOption::DiffPreviewRenderer => {
					AppOption::StatusShowUntracked
				}
			};
		}
	}

	fn switch_option(&self, right: bool) {
		if right {
			match self.selection {
				AppOption::StatusShowUntracked => {
					let untracked =
						self.options.borrow().status_show_untracked();

					let untracked = match untracked {
						None => {
							Some(ShowUntrackedFilesConfig::Normal)
						}
						Some(ShowUntrackedFilesConfig::Normal) => {
							Some(ShowUntrackedFilesConfig::All)
						}
						Some(ShowUntrackedFilesConfig::All) => {
							Some(ShowUntrackedFilesConfig::No)
						}
						Some(ShowUntrackedFilesConfig::No) => None,
					};

					self.options
						.borrow_mut()
						.set_status_show_untracked(untracked);
				}
				AppOption::DiffIgnoreWhitespaces => {
					self.options
						.borrow_mut()
						.diff_toggle_whitespace();
				}
				AppOption::DiffContextLines => {
					self.options
						.borrow_mut()
						.diff_context_change(true);
				}
				AppOption::DiffInterhunkLines => {
					self.options
						.borrow_mut()
						.diff_hunk_lines_change(true);
				}
				AppOption::DiffPreviewRenderer => {
					self.cycle_preview_renderer();
				}
			}
		} else {
			match self.selection {
				AppOption::StatusShowUntracked => {
					let untracked =
						self.options.borrow().status_show_untracked();

					let untracked = match untracked {
						None => Some(ShowUntrackedFilesConfig::No),
						Some(ShowUntrackedFilesConfig::No) => {
							Some(ShowUntrackedFilesConfig::All)
						}
						Some(ShowUntrackedFilesConfig::All) => {
							Some(ShowUntrackedFilesConfig::Normal)
						}
						Some(ShowUntrackedFilesConfig::Normal) => {
							None
						}
					};

					self.options
						.borrow_mut()
						.set_status_show_untracked(untracked);
				}
				AppOption::DiffIgnoreWhitespaces => {
					self.options
						.borrow_mut()
						.diff_toggle_whitespace();
				}
				AppOption::DiffContextLines => {
					self.options
						.borrow_mut()
						.diff_context_change(false);
				}
				AppOption::DiffInterhunkLines => {
					self.options
						.borrow_mut()
						.diff_hunk_lines_change(false);
				}
				AppOption::DiffPreviewRenderer => {
					self.cycle_preview_renderer();
				}
			}
		}

		self.queue
			.push(InternalEvent::OptionSwitched(self.selection));
	}

	fn cycle_preview_renderer(&self) {
		use crate::options::DiffPreviewPreference as P;
		let current = self.options.borrow().diff_preview_preference();
		let next = match current {
			P::Auto => P::Unified,
			P::Unified => P::Auto,
		};
		self.options.borrow_mut().set_diff_preview_preference(next);
	}
}

impl DrawableComponent for OptionsPopup {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		if self.is_visible() {
			const SIZE: (u16, u16) = (50, 10);
			let area =
				ui::centered_rect_absolute(SIZE.0, SIZE.1, area);

			let width = area.width;

			f.render_widget(Clear, area);
			f.render_widget(
				Paragraph::new(self.get_text(width))
					.block(
						Block::default()
							.borders(Borders::ALL)
							.title(Span::styled(
								"Options",
								self.theme.title(true),
							))
							.border_style(self.theme.block(true)),
					)
					.alignment(Alignment::Left),
				area,
			);
			self.content_area.set(area.inner(
				ratatui::layout::Margin {
					vertical: 1,
					horizontal: 1,
				},
			));
		}

		Ok(())
	}
}

impl Component for OptionsPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.is_visible() || force_all {
			out.push(
				CommandInfo::new(
					strings::commands::close_popup(&self.key_config),
					true,
					true,
				)
				.order(1),
			);
			out.push(
				CommandInfo::new(
					strings::commands::navigate_tree(
						&self.key_config,
					),
					true,
					true,
				)
				.order(1),
			);
		}

		visibility_blocking(self)
	}

	fn event(
		&mut self,
		event: &crossterm::event::Event,
	) -> Result<EventState> {
		if self.is_visible() {
			if let Event::Mouse(mouse) = event {
				let area = self.content_area.get();
				match mouse.kind {
					MouseEventKind::Down(MouseButton::Left)
						if area.contains(
							(mouse.column, mouse.row).into(),
						) =>
					{
						let row = mouse.row.saturating_sub(area.y);
						self.selection = match row {
							1 => AppOption::StatusShowUntracked,
							4 => AppOption::DiffIgnoreWhitespaces,
							5 => AppOption::DiffContextLines,
							6 => AppOption::DiffInterhunkLines,
							7 => AppOption::DiffPreviewRenderer,
							_ => self.selection,
						};
					}
					MouseEventKind::ScrollUp
						if area.contains(
							(mouse.column, mouse.row).into(),
						) =>
					{
						self.move_selection(true);
					}
					MouseEventKind::ScrollDown
						if area.contains(
							(mouse.column, mouse.row).into(),
						) =>
					{
						self.move_selection(false);
					}
					_ => {}
				}
				return Ok(EventState::Consumed);
			}
			if let Event::Key(key) = &event {
				let modal_up = self.key_config.is_modal()
					&& key.code == KeyCode::Up
					&& key.modifiers == KeyModifiers::NONE;
				let modal_down = self.key_config.is_modal()
					&& key.code == KeyCode::Down
					&& key.modifiers == KeyModifiers::NONE;

				if key_match(key, self.key_config.keys.exit_popup) {
					self.hide();
				} else if modal_up
					|| key_match(key, self.key_config.keys.move_up)
				{
					self.move_selection(true);
				} else if modal_down
					|| key_match(key, self.key_config.keys.move_down)
				{
					self.move_selection(false);
				} else if key_match(
					key,
					self.key_config.keys.move_right,
				) {
					self.switch_option(true);
				} else if key_match(
					key,
					self.key_config.keys.move_left,
				) {
					self.switch_option(false);
				}
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

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::keys::{KeyConfig, Keymap};

	#[test]
	fn modal_arrow_down_moves_options_selection() {
		let mut env = Environment::test_env();
		env.key_config =
			std::rc::Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let mut popup = OptionsPopup::new(&env);
		popup.show().unwrap();

		popup
			.event(&Event::Key(crossterm::event::KeyEvent::new(
				KeyCode::Down,
				KeyModifiers::NONE,
			)))
			.unwrap();

		assert!(matches!(
			popup.selection,
			AppOption::DiffIgnoreWhitespaces
		));
	}

	#[test]
	fn pointer_selects_only_rendered_option_rows_and_wheel_moves_selection(
	) {
		let env = Environment::test_env();
		let mut popup = OptionsPopup::new(&env);
		popup.show().unwrap();
		let backend = ratatui::backend::TestBackend::new(80, 24);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		terminal
			.draw(|frame| popup.draw(frame, frame.area()).unwrap())
			.unwrap();

		let area = popup.content_area.get();
		let option_row = area.y + 4;
		popup
			.event(&Event::Mouse(crossterm::event::MouseEvent {
				kind: MouseEventKind::Down(MouseButton::Left),
				column: area.x + 1,
				row: option_row,
				modifiers: KeyModifiers::NONE,
			}))
			.unwrap();
		assert!(matches!(
			popup.selection,
			AppOption::DiffIgnoreWhitespaces
		));

		popup
			.event(&Event::Mouse(crossterm::event::MouseEvent {
				kind: MouseEventKind::ScrollDown,
				column: area.x + 1,
				row: option_row,
				modifiers: KeyModifiers::NONE,
			}))
			.unwrap();
		assert!(matches!(
			popup.selection,
			AppOption::DiffContextLines
		));

		popup
			.event(&Event::Mouse(crossterm::event::MouseEvent {
				kind: MouseEventKind::Down(MouseButton::Left),
				column: 0,
				row: 0,
				modifiers: KeyModifiers::NONE,
			}))
			.unwrap();
		assert!(matches!(
			popup.selection,
			AppOption::DiffContextLines
		));
	}
}
