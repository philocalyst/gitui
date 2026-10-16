//! Modal routing for the Helix-style keymap.
//!
//! This module only translates modal input into existing application key
//! bindings or high-level navigation requests. The application remains the
//! owner of Git actions and component focus.

use crossterm::event::{
	Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
	MouseButton, MouseEventKind,
};
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	style::Modifier,
	widgets::{
		Block, Borders, Clear, List, ListItem, ListState, Paragraph,
	},
	Frame,
};
use std::cell::Cell;

use crate::{
	components::CommandInfo,
	keys::{key_match, GituiKeyEvent, KeyConfig},
	ui::style::Theme,
};

pub const MAX_COUNT: usize = 999;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ModalAction {
	Dispatch(GituiKeyEvent, usize),
	/// A contextual command selected in the palette. The caller should recheck
	/// live availability before dispatching it.
	PaletteDispatch(GituiKeyEvent),
	SwitchTab(usize),
	Branches,
	Commit,
	Options,
	Help,
	Search,
	Rebase,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModalOutcome {
	Pass(Event),
	Consumed,
	Action(ModalAction),
	OpenPalette,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prefix {
	Goto,
	Action,
	Window,
}

#[derive(Clone)]
struct PaletteEntry {
	label: String,
	description: String,
	group: String,
	binding: Option<GituiKeyEvent>,
	action: Option<ModalAction>,
	enabled: bool,
	conflict: bool,
}

impl PaletteEntry {
	const fn selectable(&self) -> bool {
		self.enabled && !self.conflict
	}
}

struct Palette {
	query: String,
	entries: Vec<PaletteEntry>,
	selected: usize,
	message: Option<String>,
	layout: Cell<Option<PaletteLayout>>,
}

#[derive(Clone, Copy)]
struct PaletteLayout {
	area: Rect,
	list: Rect,
	first_match: usize,
}

impl Palette {
	fn matching_indices(&self) -> Vec<usize> {
		let needle = self.query.to_lowercase();
		let mut matches: Vec<(usize, usize)> = self
			.entries
			.iter()
			.enumerate()
			.filter_map(|(i, entry)| {
				let haystack = format!(
					"{} {} {}",
					entry.label, entry.description, entry.group
				)
				.to_lowercase();
				fuzzy_score(&needle, &haystack)
					.map(|score| (i, score))
			})
			.collect();
		matches.sort_by_key(|(i, score)| {
			(*score, self.entries[*i].label.to_lowercase())
		});
		matches.into_iter().map(|(i, _)| i).collect()
	}

	fn move_selection(&mut self, delta: isize) {
		let matching = self.matching_indices();
		if matching.is_empty() {
			self.selected = 0;
			return;
		}
		let position = matching
			.iter()
			.position(|i| *i == self.selected)
			.unwrap_or(0);
		let next = if delta < 0 {
			if position == 0 {
				matching.len() - 1
			} else {
				position - 1
			}
		} else if position + 1 == matching.len() {
			0
		} else {
			position + 1
		};
		self.selected = matching[next];
	}

	fn selected_entry(&self) -> Option<&PaletteEntry> {
		self.entries.get(self.selected)
	}
}

/// Stateful router. Its prefixes and palette are intentionally sticky until
/// explicitly completed or canceled, so no key can fall through to an action.
#[derive(Default)]
pub struct ModalInput {
	prefix: Option<Prefix>,
	prefix_hint: String,
	count: usize,
	palette: Option<Palette>,
	notice: Option<String>,
}

impl ModalInput {
	pub fn new() -> Self {
		Self::default()
	}

	#[allow(clippy::too_many_lines)] // Prefix, count, and palette modes share ordered key handling.
	pub fn route(
		&mut self,
		event: &Event,
		keys: &KeyConfig,
	) -> ModalOutcome {
		if let Some(mut palette) = self.palette.take() {
			let outcome = route_palette(&mut palette, event, keys);
			if palette.message.as_deref() != Some("__close__") {
				self.palette = Some(palette);
			}
			return outcome;
		}
		if !keys.is_modal() {
			return ModalOutcome::Pass(event.clone());
		}
		let Event::Key(key) = event else {
			return ModalOutcome::Pass(event.clone());
		};
		if key.kind == KeyEventKind::Release {
			return ModalOutcome::Consumed;
		}
		if key.kind == KeyEventKind::Repeat {
			if self.palette.is_some() || self.prefix.is_some() {
				return ModalOutcome::Consumed;
			}
			if let Some(binding) = movement_binding(key, keys) {
				return ModalOutcome::Action(ModalAction::Dispatch(
					binding,
					self.count.max(1),
				));
			}
			return ModalOutcome::Consumed;
		}

		if key.code == KeyCode::Esc {
			if self.prefix.take().is_some()
				|| self.count > 0
				|| self.notice.take().is_some()
			{
				self.count = 0;
				self.prefix_hint.clear();
				return ModalOutcome::Consumed;
			}
			return ModalOutcome::Pass(event.clone());
		}

		if let Some(prefix) = self.prefix.take() {
			self.count = 0;
			self.prefix_hint.clear();
			let action = match prefix {
				Prefix::Goto => goto_action(key, keys),
				Prefix::Action => action_action(key),
				Prefix::Window => window_action(key, keys),
			};
			return action.unwrap_or_else(|| {
				self.notice = Some(
					"Unknown sequence after prefix · Esc clears"
						.into(),
				);
				ModalOutcome::Consumed
			});
		}

		if key_match(key, keys.keys.command_palette) {
			self.count = 0;
			return ModalOutcome::OpenPalette;
		}
		if key_match(key, keys.keys.goto_prefix) {
			self.count = 0;
			self.prefix = Some(Prefix::Goto);
			self.prefix_hint = keys.get_hint(keys.keys.goto_prefix);
			self.notice = None;
			return ModalOutcome::Consumed;
		}
		self.notice = None;
		if key_match(key, keys.keys.action_prefix) {
			self.count = 0;
			self.prefix = Some(Prefix::Action);
			self.prefix_hint = keys.get_hint(keys.keys.action_prefix);
			self.notice = None;
			return ModalOutcome::Consumed;
		}
		if keys.keys.window_prefix.code != KeyCode::Null
			&& key_match(key, keys.keys.window_prefix)
		{
			self.count = 0;
			self.prefix = Some(Prefix::Window);
			self.prefix_hint = keys.get_hint(keys.keys.window_prefix);
			self.notice = None;
			return ModalOutcome::Consumed;
		}

		if key.modifiers.is_empty() {
			if let KeyCode::Char(c @ '0'..='9') = key.code {
				self.count = self
					.count
					.saturating_mul(10)
					.saturating_add(
						c.to_digit(10).unwrap_or(0) as usize
					)
					.min(MAX_COUNT);
				return ModalOutcome::Consumed;
			}
		}

		if let Some(binding) = movement_binding(key, keys) {
			let count = self.count.max(1);
			self.count = 0;
			return ModalOutcome::Action(ModalAction::Dispatch(
				binding, count,
			));
		}

		self.count = 0;
		if key_match(
			key,
			GituiKeyEvent::new(
				KeyCode::Char('G'),
				KeyModifiers::SHIFT,
			),
		) {
			return ModalOutcome::Action(ModalAction::Dispatch(
				keys.keys.end,
				1,
			));
		}
		if key_match(key, keys.keys.open_help)
			|| key_match(
				key,
				GituiKeyEvent::new(
					KeyCode::Char('?'),
					KeyModifiers::NONE,
				),
			) {
			return ModalOutcome::Action(ModalAction::Help);
		}

		ModalOutcome::Pass(event.clone())
	}

	pub fn reset(&mut self) {
		self.prefix = None;
		self.prefix_hint.clear();
		self.count = 0;
		self.palette = None;
		self.notice = None;
	}

	pub const fn active(&self) -> bool {
		self.prefix.is_some()
			|| self.count > 0
			|| self.palette.is_some()
			|| self.notice.is_some()
	}

	/// The command palette owns printable input while it is open.
	pub const fn captures_text_input(&self) -> bool {
		self.palette.is_some()
	}

	pub fn mode_label(&self) -> Option<&'static str> {
		if self.palette.is_some() {
			Some("COMMAND")
		} else if let Some(prefix) = self.prefix {
			Some(match prefix {
				Prefix::Goto => "GOTO",
				Prefix::Action => "ACTION",
				Prefix::Window => "WINDOW",
			})
		} else {
			self.notice.as_ref().map(|_| "NOTICE")
		}
	}

	pub fn pending_hint(&self) -> String {
		if let Some(palette) = &self.palette {
			return format!(":{}", palette.query);
		}
		if let Some(notice) = &self.notice {
			return notice.clone();
		}
		match self.prefix {
			Some(Prefix::Goto) => {
				format!("{} · tabs / branch picker", self.prefix_hint)
			}
			Some(Prefix::Action) => format!(
				"{} · branches / picker / commit / tab / options / help / search",
				self.prefix_hint
			),
			Some(Prefix::Window) => format!(
				"{} · h/j/k/l or arrows focus · w next · Esc cancels",
				self.prefix_hint
			),
			None if self.count > 0 => format!("{}×", self.count),
			None => String::new(),
		}
	}

	#[allow(clippy::too_many_lines)] // Palette setup derives all display state in one place.
	pub fn open_palette(
		&mut self,
		commands: Vec<CommandInfo>,
		keys: &KeyConfig,
	) {
		let mut entries: Vec<PaletteEntry> = commands
			.into_iter()
			.filter(|command| {
				command.available
					&& command.text.binding.is_some()
					&& !command.text.name.starts_with("Goto [")
					&& !command.text.name.starts_with("Actions [")
					&& !command.text.name.starts_with("Commands [")
			})
			.map(|command| {
				let binding = command.text.binding;
				let mut label = command.text.name;
				if let Some(binding) = binding {
					let suffix =
						format!(" [{}]", keys.get_hint(binding));
					if label.ends_with(&suffix) {
						label.truncate(label.len() - suffix.len());
					}
				}
				PaletteEntry {
					label,
					description: command.text.desc.to_owned(),
					group: command.text.group.to_owned(),
					binding,
					action: None,
					enabled: command.enabled,
					conflict: false,
				}
			})
			.collect();
		let builtins = [
			(
				"Status",
				"Open status",
				"Navigation",
				Some(keys.keys.tab_status),
				ModalAction::SwitchTab(0),
			),
			(
				"Log",
				"Open commit log",
				"Navigation",
				Some(keys.keys.tab_log),
				ModalAction::SwitchTab(1),
			),
			(
				"Files",
				"Browse working tree files",
				"Navigation",
				Some(keys.keys.tab_files),
				ModalAction::SwitchTab(2),
			),
			(
				"Stashing",
				"Open stash creation",
				"Navigation",
				Some(keys.keys.tab_stashing),
				ModalAction::SwitchTab(3),
			),
			(
				"Stashes",
				"Browse saved stashes",
				"Navigation",
				Some(keys.keys.tab_stashes),
				ModalAction::SwitchTab(4),
			),
			(
				"Branches",
				"Open the branch workspace",
				"Navigation",
				Some(keys.keys.tab_branches),
				ModalAction::SwitchTab(6),
			),
			(
				"Rebase",
				"Open interactive rebase",
				"Navigation",
				Some(keys.keys.tab_rebase),
				ModalAction::Rebase,
			),
			(
				"Branch picker",
				"Open the quick branch picker (Space+B)",
				"Navigation",
				None,
				ModalAction::Branches,
			),
			(
				"Commit",
				"Create a commit",
				"Git",
				None,
				ModalAction::Commit,
			),
			(
				"Options",
				"Open application options",
				"Application",
				Some(keys.keys.open_options),
				ModalAction::Options,
			),
			(
				"Help",
				"Show key bindings and help",
				"Application",
				Some(keys.keys.open_help),
				ModalAction::Help,
			),
			(
				"Search",
				"Search the current view",
				"Navigation",
				None,
				ModalAction::Search,
			),
		];
		for (label, description, group, binding, action) in builtins {
			entries.push(PaletteEntry {
				label: label.into(),
				description: description.into(),
				group: group.into(),
				binding,
				action: Some(action),
				enabled: true,
				conflict: false,
			});
		}
		entries.sort_by_key(|entry| {
			(
				entry.group.clone(),
				entry.label.to_lowercase(),
				entry.description.to_lowercase(),
			)
		});
		entries.dedup_by(|a, b| {
			let duplicate = a.label == b.label
				&& a.description == b.description
				&& a.group == b.group
				&& same_binding(a.binding, b.binding)
				&& a.action == b.action;
			if duplicate {
				let enabled = a.enabled || b.enabled;
				a.enabled = enabled;
				b.enabled = enabled;
			}
			duplicate
		});
		for i in 0..entries.len() {
			if let Some(binding) =
				entries[i].binding.filter(|_| entries[i].enabled)
			{
				entries[i].conflict =
					entries.iter().enumerate().any(|(j, other)| {
						j != i
							&& other.enabled && other.binding.is_some_and(
							|other_binding| {
								same_binding(
									Some(binding),
									Some(other_binding),
								)
							},
						)
					});
			}
		}
		self.prefix = None;
		self.count = 0;
		self.palette = Some(Palette {
			query: String::new(),
			entries,
			selected: 0,
			message: None,
			layout: Cell::new(None),
		});
	}

	pub fn draw_overlay(
		&self,
		f: &mut Frame,
		theme: &Theme,
		keys: &KeyConfig,
	) {
		if self.palette.is_some() {
			self.draw_palette(f, theme, keys);
		} else if self.prefix.is_some()
			|| self.count > 0
			|| self.notice.is_some()
		{
			self.draw_pending(f, theme, keys);
		}
	}

	fn draw_palette(
		&self,
		f: &mut Frame,
		theme: &Theme,
		keys: &KeyConfig,
	) {
		let Some(palette) = &self.palette else { return };
		let area = f.area();
		if area.width == 0 || area.height == 0 {
			return;
		}
		let width = area.width.clamp(1, 76);
		let height = area.height.clamp(1, 22);
		let rect = centered(area, width, height);
		palette.layout.set(None);
		f.render_widget(Clear, rect);
		let block = Block::default()
			.title(" Commands ")
			.borders(Borders::ALL)
			.border_style(theme.title(true));
		let inner = block.inner(rect);
		f.render_widget(block, rect);
		if inner.height == 0 || inner.width == 0 {
			return;
		}
		let chunks = Layout::default()
			.direction(Direction::Vertical)
			.constraints([
				Constraint::Length(1),
				Constraint::Min(1),
				Constraint::Length(1),
			])
			.split(inner);
		f.render_widget(
			Paragraph::new(format!("› {}", palette.query))
				.style(theme.text(true, false)),
			chunks[0],
		);

		let matches = palette.matching_indices();
		let visible_count = chunks[1].height as usize;
		let selected_pos = matches
			.iter()
			.position(|i| *i == palette.selected)
			.unwrap_or(0);
		let start = selected_pos
			.saturating_sub(visible_count.saturating_sub(1));
		palette.layout.set(Some(PaletteLayout {
			area: rect,
			list: chunks[1],
			first_match: start,
		}));
		let visible =
			matches.into_iter().skip(start).take(visible_count);
		let items: Vec<ListItem> = visible
			.map(|index| {
				let entry = &palette.entries[index];
				let binding = entry
					.binding
					.map(|b| keys.get_hint(b))
					.unwrap_or_default();
				let state = if entry.conflict {
					"binding conflict"
				} else if !entry.enabled {
					"disabled"
				} else {
					""
				};
				let mut text =
					format!("{}  {}", entry.label, entry.description);
				if !binding.is_empty() {
					use std::fmt::Write as _;
					let _ = write!(text, "  [{binding}]");
				}
				if !state.is_empty() {
					use std::fmt::Write as _;
					let _ = write!(text, "  ({state})");
				}
				let style = if index == palette.selected {
					theme
						.text(entry.selectable(), true)
						.add_modifier(Modifier::BOLD)
				} else {
					theme.text(entry.selectable(), false)
				};
				ListItem::new(text).style(style)
			})
			.collect();
		let mut state = ListState::default();
		let selected_visible = selected_pos
			.checked_sub(start)
			.filter(|i| *i < items.len());
		state.select(selected_visible);
		f.render_stateful_widget(
			List::new(items),
			chunks[1],
			&mut state,
		);
		let footer = palette.message.as_deref().unwrap_or(
			"↑/↓ or wheel · click selects · Enter runs · Esc closes",
		);
		f.render_widget(
			Paragraph::new(footer).style(theme.text(false, false)),
			chunks[2],
		);
	}

	#[allow(clippy::too_many_lines)] // The overlay's layout and styling are a single render operation.
	fn draw_pending(
		&self,
		f: &mut Frame,
		theme: &Theme,
		keys: &KeyConfig,
	) {
		let area = f.area();
		if area.width == 0 || area.height == 0 {
			return;
		}
		if let Some(prefix) = self.prefix {
			let rows = match prefix {
				Prefix::Goto => vec![
					(
						format!(
							"{} {}",
							self.prefix_hint, self.prefix_hint
						),
						"start of list".to_owned(),
					),
					(
						format!("{} e", self.prefix_hint),
						"end of list".into(),
					),
					(
						format!("{} b", self.prefix_hint),
						"branches".into(),
					),
					(
						format!("{} f", self.prefix_hint),
						"files".into(),
					),
					(format!("{} l", self.prefix_hint), "log".into()),
					(
						format!("{} s", self.prefix_hint),
						"status".into(),
					),
					(
						format!("{} r", self.prefix_hint),
						"rebase".into(),
					),
					(
						keys.get_hint(keys.keys.tab_status),
						"status".into(),
					),
					(keys.get_hint(keys.keys.tab_log), "log".into()),
					(
						keys.get_hint(keys.keys.tab_files),
						"files".into(),
					),
					(
						keys.get_hint(keys.keys.tab_stashing),
						"stashing".into(),
					),
					(
						keys.get_hint(keys.keys.tab_stashes),
						"stashes".into(),
					),
					(
						keys.get_hint(keys.keys.tab_branches),
						"branch workspace".into(),
					),
					(
						keys.get_hint(keys.keys.tab_rebase),
						"rebase".into(),
					),
				],
				Prefix::Action => vec![
					(
						format!("{} b", self.prefix_hint),
						"branch workspace".into(),
					),
					(
						format!("{} B", self.prefix_hint),
						"quick branch picker".into(),
					),
					(
						format!("{} c", self.prefix_hint),
						"commit".into(),
					),
					(
						format!("{} s", self.prefix_hint),
						"status".into(),
					),
					(format!("{} l", self.prefix_hint), "log".into()),
					(
						format!("{} f", self.prefix_hint),
						"files".into(),
					),
					(
						format!("{} t", self.prefix_hint),
						"stashes".into(),
					),
					(
						format!("{} r", self.prefix_hint),
						"rebase".into(),
					),
					(
						format!("{} o", self.prefix_hint),
						"options".into(),
					),
					(
						format!("{} ?", self.prefix_hint),
						"help".into(),
					),
					(
						format!("{} /", self.prefix_hint),
						"search".into(),
					),
				],
				Prefix::Window => vec![
					(
						format!("{} h / ←", self.prefix_hint),
						"focus left".into(),
					),
					(
						format!("{} j / ↓", self.prefix_hint),
						"focus down".into(),
					),
					(
						format!("{} k / ↑", self.prefix_hint),
						"focus up".into(),
					),
					(
						format!("{} l / →", self.prefix_hint),
						"focus right".into(),
					),
					(
						format!("{} w", self.prefix_hint),
						"next pane".into(),
					),
				],
			};
			let title = match prefix {
				Prefix::Goto => " Goto · Esc cancels ",
				Prefix::Action => " Actions · Esc cancels ",
				Prefix::Window => " Window · Esc cancels ",
			};
			let width = area.width.clamp(1, 48);
			let rows_height = u16::try_from(rows.len())
				.unwrap_or(u16::MAX)
				.saturating_add(2);
			let height = area.height.min(rows_height).max(1);
			let rect = Rect {
				x: area.x + area.width.saturating_sub(width),
				y: area.y
					+ area
						.height
						.saturating_sub(height.saturating_add(1)),
				width,
				height,
			};
			f.render_widget(Clear, rect);
			let block = Block::default()
				.title(title)
				.borders(Borders::ALL)
				.border_style(theme.title(true));
			let inner = block.inner(rect);
			f.render_widget(block, rect);
			let lines = rows
				.into_iter()
				.map(|(key, label)| format!("{key:<10} {label}"))
				.collect::<Vec<_>>()
				.join("\n");
			f.render_widget(
				Paragraph::new(lines).style(theme.text(true, false)),
				inner,
			);
		} else {
			let content = self.pending_hint();
			let content_width =
				u16::try_from(content.chars().count())
					.unwrap_or(u16::MAX)
					.saturating_add(2);
			let width = content_width.min(area.width).max(1);
			let rect = Rect {
				x: area.x + area.width.saturating_sub(width),
				y: area.y + area.height.saturating_sub(1),
				width,
				height: 1,
			};
			f.render_widget(
				Paragraph::new(content).style(theme.text(true, true)),
				rect,
			);
		}
	}
}

#[allow(clippy::too_many_lines)] // The palette owns its query, movement, and dispatch behavior.
fn route_palette(
	palette: &mut Palette,
	event: &Event,
	keys: &KeyConfig,
) -> ModalOutcome {
	if let Event::Mouse(mouse) = event {
		let Some(layout) = palette.layout.get() else {
			return ModalOutcome::Consumed;
		};
		let column = mouse.column;
		let row = mouse.row;
		let inside = column >= layout.area.x
			&& column
				< layout.area.x.saturating_add(layout.area.width)
			&& row >= layout.area.y
			&& row < layout.area.y.saturating_add(layout.area.height);
		if !inside {
			return ModalOutcome::Consumed;
		}
		palette.message = None;
		match mouse.kind {
			MouseEventKind::Down(MouseButton::Left)
				if column >= layout.list.x
					&& column
						< layout
							.list
							.x
							.saturating_add(layout.list.width)
					&& row >= layout.list.y
					&& row
						< layout
							.list
							.y
							.saturating_add(layout.list.height) =>
			{
				let visible_row =
					usize::from(row.saturating_sub(layout.list.y));
				if let Some(index) = palette
					.matching_indices()
					.get(
						layout
							.first_match
							.saturating_add(visible_row),
					)
					.copied()
				{
					palette.selected = index;
				}
			}
			MouseEventKind::ScrollUp => palette.move_selection(-3),
			MouseEventKind::ScrollDown => palette.move_selection(3),
			_ => {}
		}
		return ModalOutcome::Consumed;
	}
	let Event::Key(key) = event else {
		return ModalOutcome::Consumed;
	};
	if key.kind == KeyEventKind::Release {
		return ModalOutcome::Consumed;
	}
	if key.kind == KeyEventKind::Repeat
		&& !palette_navigation_key(key)
		&& !repeat_text_editing_key(key)
	{
		return ModalOutcome::Consumed;
	}
	palette.message = None;
	match key.code {
		KeyCode::Esc
			if key_match(
				key,
				GituiKeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
			) =>
		{
			palette.message = Some("__close__".into());
			ModalOutcome::Consumed
		}
		KeyCode::Enter
			if key_match(
				key,
				GituiKeyEvent::new(
					KeyCode::Enter,
					KeyModifiers::NONE,
				),
			) =>
		{
			if palette.matching_indices().is_empty() {
				palette.message = Some("No matching actions".into());
				return ModalOutcome::Consumed;
			}
			let Some(entry) = palette.selected_entry() else {
				palette.message = Some("No matching actions".into());
				return ModalOutcome::Consumed;
			};
			if !entry.selectable() {
				palette.message = Some(
					if entry.conflict {
						"Binding conflict: choose another action"
					} else {
						"This action is disabled in the current context"
					}
					.into(),
				);
				return ModalOutcome::Consumed;
			}
			let outcome = if let Some(action) = entry.action {
				ModalOutcome::Action(action)
			} else if let Some(binding) = entry.binding {
				ModalOutcome::Action(ModalAction::PaletteDispatch(
					binding,
				))
			} else {
				palette.message = Some(
					"This action has no executable binding".into(),
				);
				return ModalOutcome::Consumed;
			};
			palette.message = Some("__close__".into());
			outcome
		}
		KeyCode::Up
			if key_match(
				key,
				GituiKeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
			) =>
		{
			palette.move_selection(-1);
			ModalOutcome::Consumed
		}
		KeyCode::Down
			if key_match(
				key,
				GituiKeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
			) =>
		{
			palette.move_selection(1);
			ModalOutcome::Consumed
		}
		KeyCode::Char('n')
			if key_match(
				key,
				GituiKeyEvent::new(
					KeyCode::Char('n'),
					KeyModifiers::CONTROL,
				),
			) =>
		{
			palette.move_selection(1);
			ModalOutcome::Consumed
		}
		KeyCode::Char('p')
			if key_match(
				key,
				GituiKeyEvent::new(
					KeyCode::Char('p'),
					KeyModifiers::CONTROL,
				),
			) =>
		{
			palette.move_selection(-1);
			ModalOutcome::Consumed
		}
		KeyCode::Backspace
			if key_match(
				key,
				GituiKeyEvent::new(
					KeyCode::Backspace,
					KeyModifiers::NONE,
				),
			) =>
		{
			palette.query.pop();
			palette.selected = palette
				.matching_indices()
				.first()
				.copied()
				.unwrap_or(0);
			ModalOutcome::Consumed
		}
		KeyCode::Char(c)
			if !key.modifiers.intersects(
				KeyModifiers::CONTROL
					| KeyModifiers::ALT
					| KeyModifiers::SUPER,
			) =>
		{
			palette.query.push(c);
			palette.selected = palette
				.matching_indices()
				.first()
				.copied()
				.unwrap_or(0);
			ModalOutcome::Consumed
		}
		_ => {
			let _ = keys;
			ModalOutcome::Consumed
		}
	}
}

fn fuzzy_score(needle: &str, haystack: &str) -> Option<usize> {
	if needle.is_empty() {
		return Some(0);
	}
	let mut pos = 0;
	let mut score = 0;
	let mut previous = None;
	for c in needle.chars() {
		let offset = haystack.get(pos..)?.find(c)?;
		let found = pos + offset;
		if previous == Some(found.saturating_sub(1)) {
			score += 0;
		} else {
			score += found.saturating_sub(pos);
		}
		previous = Some(found);
		pos = found + c.len_utf8();
	}
	Some(score)
}

fn same_binding(
	a: Option<GituiKeyEvent>,
	b: Option<GituiKeyEvent>,
) -> bool {
	match (a, b) {
		(Some(a), Some(b)) => key_match(&KeyEvent::from(&a), b),
		(None, None) => true,
		_ => false,
	}
}

fn palette_navigation_key(key: &KeyEvent) -> bool {
	key_match(
		key,
		GituiKeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
	) || key_match(
		key,
		GituiKeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
	) || key_match(
		key,
		GituiKeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
	) || key_match(
		key,
		GituiKeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
	)
}

pub fn repeat_text_editing_key(key: &KeyEvent) -> bool {
	if key.modifiers.intersects(
		KeyModifiers::CONTROL
			| KeyModifiers::ALT
			| KeyModifiers::SUPER,
	) {
		return false;
	}
	match key.code {
		KeyCode::Char(character) => !character.is_control(),
		KeyCode::Backspace
		| KeyCode::Delete
		| KeyCode::Left
		| KeyCode::Right
		| KeyCode::Up
		| KeyCode::Down
		| KeyCode::Home
		| KeyCode::End => true,
		_ => false,
	}
}

fn goto_action(
	key: &KeyEvent,
	keys: &KeyConfig,
) -> Option<ModalOutcome> {
	if key_match(key, keys.keys.goto_prefix) {
		return Some(ModalOutcome::Action(ModalAction::Dispatch(
			keys.keys.home,
			1,
		)));
	}
	if plain_char(key, 'e') {
		return Some(ModalOutcome::Action(ModalAction::Dispatch(
			keys.keys.end,
			1,
		)));
	}
	for (binding, action) in [
		(keys.keys.tab_status, ModalAction::SwitchTab(0)),
		(keys.keys.tab_log, ModalAction::SwitchTab(1)),
		(keys.keys.tab_files, ModalAction::SwitchTab(2)),
		(keys.keys.tab_stashing, ModalAction::SwitchTab(3)),
		(keys.keys.tab_stashes, ModalAction::SwitchTab(4)),
		(keys.keys.tab_branches, ModalAction::SwitchTab(6)),
		(keys.keys.tab_rebase, ModalAction::Rebase),
	] {
		if key_match(key, binding) {
			return Some(ModalOutcome::Action(action));
		}
	}
	let action = if plain_char(key, 'b') {
		ModalAction::Branches
	} else if plain_char(key, 'f') || plain_char(key, '3') {
		ModalAction::SwitchTab(2)
	} else if plain_char(key, 'l') || plain_char(key, '2') {
		ModalAction::SwitchTab(1)
	} else if plain_char(key, 's') || plain_char(key, '1') {
		ModalAction::SwitchTab(0)
	} else if plain_char(key, 'r') || plain_char(key, '6') {
		ModalAction::Rebase
	} else if plain_char(key, '4') {
		ModalAction::SwitchTab(3)
	} else if plain_char(key, '5') {
		ModalAction::SwitchTab(4)
	} else {
		return None;
	};
	Some(ModalOutcome::Action(action))
}

fn action_action(key: &KeyEvent) -> Option<ModalOutcome> {
	let action = if plain_char(key, 'b') {
		ModalAction::SwitchTab(6)
	} else if plain_char(key, 'B') {
		ModalAction::Branches
	} else if plain_char(key, 'c') {
		ModalAction::Commit
	} else if plain_char(key, 's') {
		ModalAction::SwitchTab(0)
	} else if plain_char(key, 'l') {
		ModalAction::SwitchTab(1)
	} else if plain_char(key, 'f') {
		ModalAction::SwitchTab(2)
	} else if plain_char(key, 't') {
		ModalAction::SwitchTab(4)
	} else if plain_char(key, 'r') {
		ModalAction::Rebase
	} else if plain_char(key, 'o') {
		ModalAction::Options
	} else if plain_char(key, '?') {
		ModalAction::Help
	} else if plain_char(key, '/') {
		ModalAction::Search
	} else {
		return None;
	};
	Some(ModalOutcome::Action(action))
}

const fn window_action(
	key: &KeyEvent,
	keys: &KeyConfig,
) -> Option<ModalOutcome> {
	let binding = match key.code {
		KeyCode::Char('h') | KeyCode::Left => keys.keys.focus_left,
		KeyCode::Char('j') | KeyCode::Down => keys.keys.focus_down,
		KeyCode::Char('k') | KeyCode::Up => keys.keys.focus_up,
		KeyCode::Char('l') | KeyCode::Right => keys.keys.focus_right,
		KeyCode::Char('w') => keys.keys.focus_next,
		_ => return None,
	};
	if !key.modifiers.is_empty() {
		return None;
	}
	Some(ModalOutcome::Action(ModalAction::Dispatch(binding, 1)))
}

fn plain_char(key: &KeyEvent, c: char) -> bool {
	key_match(
		key,
		GituiKeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
	)
}

fn movement_binding(
	key: &KeyEvent,
	keys: &KeyConfig,
) -> Option<GituiKeyEvent> {
	let choices = [
		(
			keys.keys.move_left,
			GituiKeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
		),
		(
			keys.keys.move_right,
			GituiKeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
		),
		(
			keys.keys.move_up,
			GituiKeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
		),
		(
			keys.keys.move_down,
			GituiKeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
		),
		(
			keys.keys.page_up,
			GituiKeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
		),
		(
			keys.keys.page_down,
			GituiKeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
		),
		(
			keys.keys.home,
			GituiKeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
		),
		(
			keys.keys.end,
			GituiKeyEvent::new(KeyCode::End, KeyModifiers::NONE),
		),
	];
	choices.into_iter().find_map(|(binding, arrow)| {
		if key_match(key, binding) || key_match(key, arrow) {
			Some(binding)
		} else {
			None
		}
	})
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
	Rect {
		x: area.x + area.width.saturating_sub(width) / 2,
		y: area.y + area.height.saturating_sub(height) / 2,
		width: width.min(area.width),
		height: height.min(area.height),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::components::CommandText;
	use ratatui::{backend::TestBackend, Terminal};

	fn key(c: char) -> Event {
		Event::Key(KeyEvent::new(
			KeyCode::Char(c),
			KeyModifiers::NONE,
		))
	}
	fn router() -> ModalInput {
		ModalInput::new()
	}
	fn config() -> KeyConfig {
		KeyConfig::with_keymap(crate::keys::Keymap::Helix)
	}

	#[test]
	fn prefix_cancels_and_unknown_suffix_never_falls_through() {
		let keys = config();
		let mut r = router();
		assert_eq!(r.route(&key('g'), &keys), ModalOutcome::Consumed);
		assert_eq!(r.route(&key('q'), &keys), ModalOutcome::Consumed);
		assert!(r.active());
		assert!(r.pending_hint().contains("Unknown sequence"));
		assert_eq!(
			r.route(
				&Event::Key(KeyEvent::new(
					KeyCode::Esc,
					KeyModifiers::NONE
				)),
				&keys
			),
			ModalOutcome::Consumed
		);
		assert!(!r.active());
		assert_eq!(r.route(&key(' '), &keys), ModalOutcome::Consumed);
		assert_eq!(
			r.route(
				&Event::Key(KeyEvent::new(
					KeyCode::Esc,
					KeyModifiers::NONE
				)),
				&keys
			),
			ModalOutcome::Consumed
		);
		assert!(!r.active());
	}

	#[test]
	fn window_prefix_routes_pane_focus_and_consumes_unknown_suffixes()
	{
		let keys = config();
		let mut router = router();
		let ctrl_w = Event::Key(KeyEvent::new(
			KeyCode::Char('w'),
			KeyModifiers::CONTROL,
		));
		assert_eq!(
			router.route(&ctrl_w, &keys),
			ModalOutcome::Consumed
		);
		assert_eq!(
			router.route(&key('h'), &keys),
			ModalOutcome::Action(ModalAction::Dispatch(
				keys.keys.focus_left,
				1
			))
		);

		assert_eq!(
			router.route(&ctrl_w, &keys),
			ModalOutcome::Consumed
		);
		assert_eq!(
			router.route(
				&Event::Key(KeyEvent::new(
					KeyCode::Down,
					KeyModifiers::NONE
				)),
				&keys
			),
			ModalOutcome::Action(ModalAction::Dispatch(
				keys.keys.focus_down,
				1
			))
		);

		assert_eq!(
			router.route(&ctrl_w, &keys),
			ModalOutcome::Consumed
		);
		assert_eq!(
			router.route(&key('w'), &keys),
			ModalOutcome::Action(ModalAction::Dispatch(
				keys.keys.focus_next,
				1
			))
		);

		assert_eq!(
			router.route(&ctrl_w, &keys),
			ModalOutcome::Consumed
		);
		assert_eq!(
			router.route(&key('x'), &keys),
			ModalOutcome::Consumed
		);
		assert!(router.pending_hint().contains("Unknown sequence"));
		assert_eq!(
			router.route(&ctrl_w, &keys),
			ModalOutcome::Consumed
		);
		assert_eq!(
			router.route(
				&Event::Key(KeyEvent::new(
					KeyCode::Esc,
					KeyModifiers::NONE
				)),
				&keys
			),
			ModalOutcome::Consumed
		);
		assert!(!router.active());
	}

	#[test]
	fn count_applies_only_once_to_movement() {
		let keys = config();
		let mut r = router();
		for c in "12".chars() {
			assert_eq!(
				r.route(&key(c), &keys),
				ModalOutcome::Consumed
			);
		}
		assert_eq!(
			r.route(&key('j'), &keys),
			ModalOutcome::Action(ModalAction::Dispatch(
				keys.keys.move_down,
				12
			))
		);
		assert_eq!(r.route(&key('9'), &keys), ModalOutcome::Consumed);
		let ctrl_two = Event::Key(KeyEvent::new(
			KeyCode::Char('2'),
			KeyModifiers::CONTROL,
		));
		assert_eq!(
			r.route(&ctrl_two, &keys),
			ModalOutcome::Pass(ctrl_two.clone())
		);
		assert_eq!(
			r.route(&key('x'), &keys),
			ModalOutcome::Pass(key('x'))
		);
	}

	#[test]
	fn modified_aliases_do_not_trigger_plain_modal_commands() {
		let keys = config();
		let mut r = router();
		let ctrl_g = Event::Key(KeyEvent::new(
			KeyCode::Char('g'),
			KeyModifiers::CONTROL,
		));
		assert_eq!(
			r.route(&ctrl_g, &keys),
			ModalOutcome::Pass(ctrl_g.clone())
		);
		assert_eq!(r.route(&key('g'), &keys), ModalOutcome::Consumed);
		let ctrl_q = Event::Key(KeyEvent::new(
			KeyCode::Char('q'),
			KeyModifiers::CONTROL,
		));
		assert_eq!(r.route(&ctrl_q, &keys), ModalOutcome::Consumed);
		assert!(r.pending_hint().contains("Unknown sequence"));
	}

	#[test]
	fn repeat_events_only_repeat_movement() {
		let keys = config();
		let mut r = router();
		let mut repeat =
			KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
		repeat.kind = KeyEventKind::Repeat;
		assert_eq!(
			r.route(&Event::Key(repeat), &keys),
			ModalOutcome::Action(ModalAction::Dispatch(
				keys.keys.move_down,
				1
			))
		);
		repeat.code = KeyCode::Char('q');
		assert_eq!(
			r.route(&Event::Key(repeat), &keys),
			ModalOutcome::Consumed
		);
	}

	#[test]
	fn arrows_and_goto_use_configured_movement_bindings() {
		let keys = config();
		let mut r = router();
		assert_eq!(
			r.route(
				&Event::Key(KeyEvent::new(
					KeyCode::Down,
					KeyModifiers::NONE
				)),
				&keys
			),
			ModalOutcome::Action(ModalAction::Dispatch(
				keys.keys.move_down,
				1
			))
		);
		assert_eq!(r.route(&key('g'), &keys), ModalOutcome::Consumed);
		assert_eq!(
			r.route(&key('g'), &keys),
			ModalOutcome::Action(ModalAction::Dispatch(
				keys.keys.home,
				1
			))
		);
	}

	#[test]
	fn branch_workspace_and_quick_picker_have_distinct_routes() {
		let keys = config();
		let mut r = router();
		assert_eq!(r.route(&key('g'), &keys), ModalOutcome::Consumed);
		assert_eq!(
			r.route(&key('7'), &keys),
			ModalOutcome::Action(ModalAction::SwitchTab(6))
		);
		assert_eq!(r.route(&key(' '), &keys), ModalOutcome::Consumed);
		assert_eq!(
			r.route(&key('b'), &keys),
			ModalOutcome::Action(ModalAction::SwitchTab(6))
		);
		assert_eq!(r.route(&key(' '), &keys), ModalOutcome::Consumed);
		assert_eq!(
			r.route(&key('B'), &keys),
			ModalOutcome::Action(ModalAction::Branches)
		);
	}

	#[test]
	fn palette_text_does_not_trigger_motion_and_unicode_backspace_works(
	) {
		let keys = config();
		let mut r = router();
		r.open_palette(vec![], &keys);
		for c in ['h', 'j', 'k', 'l', 'g', ' ', '界'] {
			assert_eq!(
				r.route(&key(c), &keys),
				ModalOutcome::Consumed
			);
		}
		assert_eq!(r.pending_hint(), ":hjklg 界");
		assert_eq!(
			r.route(
				&Event::Key(KeyEvent::new(
					KeyCode::Backspace,
					KeyModifiers::NONE
				)),
				&keys
			),
			ModalOutcome::Consumed
		);
		assert_eq!(r.pending_hint(), ":hjklg ");
	}

	#[test]
	fn palette_conflicts_ignore_commands_in_unfocused_panes() {
		let keys = config();
		let binding = GituiKeyEvent::new(
			KeyCode::Char('s'),
			KeyModifiers::NONE,
		);
		let command = |name: &str, enabled| {
			CommandInfo::new(
				CommandText::new(name.into(), "test", "test")
					.with_binding(binding),
				enabled,
				true,
			)
		};
		let mut r = router();
		r.open_palette(
			vec![
				command("Focused stage", true),
				command("Unfocused diff", false),
			],
			&keys,
		);
		let entries = &r.palette.as_ref().unwrap().entries;
		assert!(
			!entries
				.iter()
				.find(|e| e.label == "Focused stage")
				.unwrap()
				.conflict
		);
		assert!(
			!entries
				.iter()
				.find(|e| e.label == "Unfocused diff")
				.unwrap()
				.conflict
		);
	}

	#[test]
	fn conflicting_and_disabled_commands_cannot_execute() {
		let keys = config();
		let binding = GituiKeyEvent::new(
			KeyCode::Char('x'),
			KeyModifiers::NONE,
		);
		let command = |name: &str, enabled| {
			CommandInfo::new(
				CommandText::new(name.into(), "test", "test")
					.with_binding(binding),
				enabled,
				true,
			)
		};
		let mut r = router();
		r.open_palette(vec![command("disabled", false)], &keys);
		for c in "disabled".chars() {
			r.route(&key(c), &keys);
		}
		assert_eq!(
			r.route(
				&Event::Key(KeyEvent::new(
					KeyCode::Enter,
					KeyModifiers::NONE
				)),
				&keys
			),
			ModalOutcome::Consumed
		);
		assert!(r.active());
		// A fresh query selects both same-binding commands, which stay inert.
		r.open_palette(
			vec![command("one", true), command("two", true)],
			&keys,
		);
		for c in "one".chars() {
			r.route(&key(c), &keys);
		}
		assert_eq!(
			r.route(
				&Event::Key(KeyEvent::new(
					KeyCode::Enter,
					KeyModifiers::NONE
				)),
				&keys
			),
			ModalOutcome::Consumed
		);
		assert!(r.active());
	}

	#[test]
	fn palette_mouse_click_selects_and_enter_runs_the_selected_command(
	) {
		let keys = config();
		let binding = GituiKeyEvent::new(
			KeyCode::Char('x'),
			KeyModifiers::NONE,
		);
		let command = CommandInfo::new(
			CommandText::new(
				"Pointer target".into(),
				"test command",
				"Test",
			)
			.with_binding(binding),
			true,
			true,
		);
		let mut r = router();
		r.open_palette(vec![command], &keys);
		let backend = TestBackend::new(80, 24);
		let mut terminal = Terminal::new(backend).unwrap();
		terminal
			.draw(|f| r.draw_overlay(f, &Theme::default(), &keys))
			.unwrap();

		let palette = r.palette.as_ref().unwrap();
		let target = palette
			.entries
			.iter()
			.position(|entry| entry.label == "Pointer target")
			.unwrap();
		let layout = palette.layout.get().unwrap();
		let matches = palette.matching_indices();
		let target_position = matches
			.iter()
			.position(|index| *index == target)
			.unwrap();
		let selected_position = matches
			.iter()
			.position(|index| *index == palette.selected)
			.unwrap();
		let first_match = selected_position
			.saturating_sub(layout.list.height as usize - 1);
		let click = Event::Mouse(crossterm::event::MouseEvent {
			kind: MouseEventKind::Down(MouseButton::Left),
			column: layout.list.x,
			row: layout.list.y
				+ u16::try_from(target_position - first_match)
					.unwrap(),
			modifiers: KeyModifiers::NONE,
		});
		assert_eq!(r.route(&click, &keys), ModalOutcome::Consumed);
		assert!(r.active());
		assert_eq!(r.palette.as_ref().unwrap().selected, target);
		assert_eq!(
			r.route(
				&Event::Key(KeyEvent::new(
					KeyCode::Enter,
					KeyModifiers::NONE
				)),
				&keys
			),
			ModalOutcome::Action(ModalAction::PaletteDispatch(
				binding
			))
		);
		assert!(!r.active());
	}

	#[test]
	fn palette_mouse_wheel_moves_selection_without_running_commands()
	{
		let keys = config();
		let mut r = router();
		r.open_palette(vec![], &keys);
		let backend = TestBackend::new(80, 24);
		let mut terminal = Terminal::new(backend).unwrap();
		terminal
			.draw(|f| r.draw_overlay(f, &Theme::default(), &keys))
			.unwrap();
		let layout =
			r.palette.as_ref().unwrap().layout.get().unwrap();
		let old_selected = r.palette.as_ref().unwrap().selected;
		let wheel = Event::Mouse(crossterm::event::MouseEvent {
			kind: MouseEventKind::ScrollDown,
			column: layout.area.x,
			row: layout.area.y,
			modifiers: KeyModifiers::NONE,
		});
		assert_eq!(r.route(&wheel, &keys), ModalOutcome::Consumed);
		assert!(r.active());
		assert_ne!(
			r.palette.as_ref().unwrap().selected,
			old_selected
		);
	}

	#[test]
	fn palette_draws_in_a_tiny_terminal() {
		let keys = config();
		let mut r = router();
		r.open_palette(vec![], &keys);
		let backend = TestBackend::new(2, 1);
		let mut terminal = Terminal::new(backend).unwrap();
		terminal
			.draw(|f| r.draw_overlay(f, &Theme::default(), &keys))
			.unwrap();
	}
}
