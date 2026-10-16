use crate::{
	accessors,
	app::Environment,
	components::{
		command_pump, event_pump, visibility_blocking,
		CommandBlocking, CommandInfo, Component, DrawableComponent,
		EventState, SelectionStatus, StatusTreeComponent,
	},
	keys::{key_match, SharedKeyConfig},
	queue::{InternalEvent, Queue},
	strings,
	ui::style::SharedTheme,
};
use anyhow::Result;
use asyncgit::{
	sync::{self, status::StatusType, RepoPathRef},
	AsyncGitNotification, AsyncStatus, StatusParams,
};
use crossterm::event::{Event, MouseButton, MouseEventKind};
use ratatui::{
	layout::{Alignment, Constraint, Direction, Layout, Rect},
	text::{Line, Span},
	widgets::{Block, Borders, Paragraph},
};
use std::{borrow::Cow, cell::Cell};

#[derive(Default, Clone, Copy, Debug)]
pub struct StashingOptions {
	pub stash_untracked: bool,
	pub keep_index: bool,
}

pub struct Stashing {
	repo: RepoPathRef,
	index: StatusTreeComponent,
	visible: bool,
	options: StashingOptions,
	theme: SharedTheme,
	git_status: AsyncStatus,
	status_display_revision: u64,
	queue: Queue,
	key_config: SharedKeyConfig,
	option_focus: bool,
	index_area: Cell<Rect>,
	options_area: Cell<Rect>,
	mouse_capture_options: Option<bool>,
}

impl Stashing {
	accessors!(self, [index]);

	///
	pub fn new(env: &Environment) -> Self {
		Self {
			repo: env.repo.clone(),
			index: StatusTreeComponent::new(
				env,
				&strings::stashing_files_title(&env.key_config),
				true,
			),
			visible: false,
			options: StashingOptions {
				keep_index: false,
				stash_untracked: true,
			},
			theme: env.theme.clone(),
			git_status: AsyncStatus::new(
				env.repo.borrow().clone(),
				env.sender_git.clone(),
			),
			status_display_revision: 0,
			queue: env.queue.clone(),
			key_config: env.key_config.clone(),
			option_focus: false,
			index_area: Cell::new(Rect::default()),
			options_area: Cell::new(Rect::default()),
			mouse_capture_options: None,
		}
	}

	///
	pub fn update(&self) -> Result<()> {
		if self.is_visible() {
			self.git_status
				//TODO: support options
				.fetch(&StatusParams::new(StatusType::Both, None))?;
		}

		Ok(())
	}

	/// Mark the stashing status snapshot stale after a repository change.
	pub fn invalidate_status(&self) {
		self.git_status.invalidate();
	}

	/// Schedule a refresh on the next update pass when idle.
	pub fn poll_status(&self) {
		self.git_status.invalidate_if_idle();
	}

	///
	pub fn anything_pending(&self) -> bool {
		self.git_status.is_pending()
	}

	///
	pub fn update_git(
		&mut self,
		ev: AsyncGitNotification,
	) -> Result<()> {
		if self.is_visible() && ev == AsyncGitNotification::Status {
			if let Some((revision, status)) = self
				.git_status
				.last_if_changed(self.status_display_revision)?
			{
				self.index.show()?;
				self.index.update(&status.items)?;
				self.status_display_revision = revision;
			}
			self.index.set_pending(self.git_status.is_pending());
			self.update()?;
		}

		Ok(())
	}

	fn get_option_text(&self) -> Vec<Line<'_>> {
		let bracket_open = Span::raw(Cow::from("["));
		let bracket_close = Span::raw(Cow::from("]"));
		let option_on =
			Span::styled(Cow::from("x"), self.theme.option(true));

		let option_off =
			Span::styled(Cow::from("_"), self.theme.option(false));

		vec![
			Line::from(vec![
				bracket_open.clone(),
				if self.options.stash_untracked {
					option_on.clone()
				} else {
					option_off.clone()
				},
				bracket_close.clone(),
				Span::raw(Cow::from(" stash untracked")),
			]),
			Line::from(vec![
				bracket_open,
				if self.options.keep_index {
					option_on
				} else {
					option_off
				},
				bracket_close,
				Span::raw(Cow::from(" keep index")),
			]),
		]
	}

	fn handle_mouse_event(
		&mut self,
		ev: &crossterm::event::Event,
	) -> Result<EventState> {
		let Event::Mouse(mouse) = ev else {
			return Ok(EventState::NotConsumed);
		};
		let options = match mouse.kind {
			MouseEventKind::Drag(_) | MouseEventKind::Up(_) => {
				self.mouse_capture_options
			}
			MouseEventKind::Down(MouseButton::Left)
			| MouseEventKind::ScrollUp
			| MouseEventKind::ScrollDown => {
				let position = (mouse.column, mouse.row).into();
				if self.options_area.get().contains(position) {
					Some(true)
				} else if self.index_area.get().contains(position) {
					Some(false)
				} else {
					None
				}
			}
			_ => None,
		};
		if let Some(options) = options {
			self.option_focus = options;
			self.index.focus(!options);
			if matches!(
				mouse.kind,
				MouseEventKind::Down(MouseButton::Left)
			) {
				self.mouse_capture_options = Some(options);
				if options {
					self.toggle_option_at(mouse.column, mouse.row);
				}
			}
			if !options {
				let _ = event_pump(
					ev,
					self.components_mut().as_mut_slice(),
				)?;
			}
		}
		if matches!(mouse.kind, MouseEventKind::Up(_)) {
			self.mouse_capture_options = None;
		}
		Ok(if options.is_some() {
			EventState::Consumed
		} else {
			EventState::NotConsumed
		})
	}

	const fn toggle_option_at(&mut self, column: u16, row: u16) {
		let area = self.options_area.get();
		let inside_content = column > area.x
			&& column < area.right().saturating_sub(1)
			&& row > area.y
			&& row < area.bottom().saturating_sub(1);
		if !inside_content {
			return;
		}
		match row - area.y - 1 {
			0 => {
				self.options.stash_untracked =
					!self.options.stash_untracked;
			}
			1 => self.options.keep_index = !self.options.keep_index,
			_ => {}
		}
	}
}

impl DrawableComponent for Stashing {
	fn draw(
		&self,
		f: &mut ratatui::Frame,
		rect: ratatui::layout::Rect,
	) -> Result<()> {
		let chunks = Layout::default()
			.direction(Direction::Horizontal)
			.constraints(
				[Constraint::Min(1), Constraint::Length(22)].as_ref(),
			)
			.split(rect);

		let right_chunks = Layout::default()
			.direction(Direction::Vertical)
			.constraints(
				[Constraint::Length(4), Constraint::Min(1)].as_ref(),
			)
			.split(chunks[1]);
		self.index_area.set(chunks[0]);
		self.options_area.set(right_chunks[0]);

		f.render_widget(
			Paragraph::new(self.get_option_text())
				.block(
					Block::default()
						.borders(Borders::ALL)
						.title(Span::styled(
							strings::stashing_options_title(
								&self.key_config,
							),
							self.theme.title(self.option_focus),
						))
						.border_style(
							self.theme.block(self.option_focus),
						),
				)
				.alignment(Alignment::Left),
			right_chunks[0],
		);

		self.index.draw(f, chunks[0])?;

		Ok(())
	}
}

impl Component for Stashing {
	fn selection_status(&self) -> Option<SelectionStatus> {
		if self.visible
			&& self.key_config.is_modal()
			&& !self.option_focus
		{
			self.index.selection_status()
		} else {
			None
		}
	}

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

			out.push(CommandInfo::new(
				strings::commands::stashing_save(&self.key_config),
				self.visible && !self.index.is_empty(),
				self.visible || force_all,
			));
			out.push(CommandInfo::new(
				strings::commands::stashing_toggle_indexed(
					&self.key_config,
				),
				self.visible,
				self.visible || force_all,
			));
			out.push(CommandInfo::new(
				strings::commands::stashing_toggle_untracked(
					&self.key_config,
				),
				self.visible,
				self.visible || force_all,
			));
		}

		visibility_blocking(self)
	}

	fn event(
		&mut self,
		ev: &crossterm::event::Event,
	) -> Result<EventState> {
		if self.visible {
			if matches!(ev, Event::Mouse(_)) {
				return self.handle_mouse_event(ev);
			}

			if self.key_config.is_modal() {
				if let Event::Key(k) = ev {
					if key_match(k, self.key_config.keys.focus_next)
						|| key_match(
							k,
							self.key_config.keys.focus_previous,
						) {
						self.option_focus = !self.option_focus;
						self.index.focus(!self.option_focus);
						return Ok(EventState::Consumed);
					}
					if (key_match(
						k,
						self.key_config.keys.focus_right,
					) && !self.option_focus)
						|| (key_match(
							k,
							self.key_config.keys.focus_left,
						) && self.option_focus)
					{
						self.option_focus = !self.option_focus;
						self.index.focus(!self.option_focus);
						return Ok(EventState::Consumed);
					}
				}
			}
			if !self.option_focus
				&& event_pump(
					ev,
					self.components_mut().as_mut_slice(),
				)?
				.is_consumed()
			{
				return Ok(EventState::Consumed);
			}

			if let Event::Key(k) = ev {
				return if key_match(
					k,
					self.key_config.keys.stashing_save,
				) && !self.index.is_empty()
				{
					if self.key_config.is_modal()
						&& self.index.has_selection()
					{
						self.queue.push(InternalEvent::ShowErrorMsg(
							String::from(
								"Stashing saves the whole repository; clear the selected files first.",
							),
						));
						return Ok(EventState::Consumed);
					}
					self.queue.push(InternalEvent::PopupStashing(
						self.options,
					));

					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.stashing_toggle_index,
				) {
					self.options.keep_index =
						!self.options.keep_index;
					self.update()?;
					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.stashing_toggle_untracked,
				) {
					self.options.stash_untracked =
						!self.options.stash_untracked;
					self.update()?;
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
		self.option_focus = false;
	}

	fn show(&mut self) -> Result<()> {
		let config_untracked_files =
			sync::untracked_files_config(&self.repo.borrow())?;

		self.options.stash_untracked =
			!config_untracked_files.include_none();

		self.index.show()?;
		self.option_focus = false;
		self.index.focus(true);
		self.visible = true;
		self.update()?;
		Ok(())
	}
}
