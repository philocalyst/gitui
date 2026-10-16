//! The rail: the plan's rows as the workspace's spine.
//!
//! One row per instruction (and per raw line) of the rebase
//! program, in program order: a state glyph, the action word,
//! the short oid, and a truncated label. Fold members
//! (`squash`/`fixup`) are indented under their group head. The
//! current pointer (executing/failed/stopped row) is rendered
//! independently of the selection — selecting a row never
//! moves the pointer, and the pointer never hijacks the
//! selection.
//!
//! Rows arrive as [`RailItem`]s built by the tab from the
//! policy projection (see [`rail_items`]); this component owns
//! selection, scrolling, and rendering only.

// TODO(app wiring): remove once app.rs constructs the tab —
// until then the rail is only reachable from tests.
#![allow(dead_code)]

use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		DrawableComponent, EventState, ScrollType, SelectionStatus,
	},
	keys::{key_match, SharedKeyConfig},
	strings,
	ui::{
		calc_scroll_top, draw_scrollbar, style::SharedTheme,
		Orientation,
	},
};
use anyhow::Result;
use asyncgit::rebase::{plan::Plan, policy::Row, policy::RowState};
use crossterm::event::Event;
use ratatui::{
	layout::{Alignment, Rect},
	style::Modifier,
	text::{Line, Span},
	widgets::{Block, Borders, Paragraph},
	Frame,
};
use std::cell::Cell;
use unicode_truncate::UnicodeTruncateStr;

const GLYPH_PENDING: &str = "○";
const GLYPH_DONE: &str = strings::symbol::CHECKMARK;
const GLYPH_CURRENT: &str = "▶";
const GLYPH_DROPPED: &str = "×";
const GLYPH_STOP: &str = "‖";
const GLYPH_FAILED: &str = "!";

/// width of the fixed columns: glyph + space + word + space +
/// oid + space
const FIXED_WIDTH: usize = 1 + 1 + 6 + 1 + 7 + 1;

/// The action column word for a canonical instruction word.
#[must_use]
pub fn action_word(canonical: Option<&str>) -> &'static str {
	match canonical {
		Some("pick") => "PICK",
		Some("reword") => "REWORD",
		Some("edit") => "EDIT",
		Some("squash") => "SQUASH",
		Some("fixup") => "FIXUP",
		Some("drop") => "DROP",
		Some("break") => "BREAK",
		Some("exec") => "EXEC",
		Some("label") => "LABEL",
		Some("reset") => "RESET",
		Some("merge") => "MERGE",
		Some("update-ref") => "UPD-REF",
		Some("noop") => "NOOP",
		_ => "????",
	}
}

/// One renderable rail row: the policy [`Row`] resolved with
/// the action word from the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RailItem {
	/// The plan step this row renders, when it has one.
	pub step: Option<asyncgit::rebase::StepId>,
	/// Execution state (drives the glyph).
	pub state: RowState,
	/// Uppercased action word (`????` for raw rows and rows
	/// without a resolvable step).
	pub word: &'static str,
	/// Short oid, when the row has a commit.
	pub short_oid: Option<String>,
	/// Label (subject, command, or lossy raw bytes).
	pub label: String,
	/// Whether this row folds into the previous row's group.
	pub fold: bool,
}

/// Builds the renderable rows from the policy rail plus the
/// session plan (which carries the instructions behind the
/// step ids). Pure.
#[must_use]
pub fn rail_items(
	rail: &[Row],
	plan: Option<&Plan>,
) -> Vec<RailItem> {
	rail.iter()
		.map(|row| {
			let word = row
				.step
				.and_then(|id| plan.and_then(|p| p.step(id)))
				.and_then(|step| step.line.instruction())
				.map_or_else(
					// No plan behind the step (adopted
					// sessions, observation rows): the
					// policy row carries the canonical
					// word; raw rows carry "" → ????.
					|| action_word(Some(row.action)),
					|instruction| {
						action_word(Some(
							instruction.canonical_word(),
						))
					},
				);
			RailItem {
				step: row.step,
				state: row.state,
				word,
				short_oid: row.short_oid.clone(),
				label: row.label.clone(),
				fold: row.folds_into_previous,
			}
		})
		.collect()
}

/// Renders the plan rows with selection and scrolling.
pub struct Rail {
	title: Box<str>,
	items: Vec<RailItem>,
	selection: Cell<usize>,
	scroll_top: Cell<usize>,
	current_size: Cell<Option<(u16, u16)>>,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	focused: Cell<bool>,
}

impl Rail {
	///
	pub fn new(env: &Environment, title: &str) -> Self {
		Self {
			title: title.into(),
			items: Vec::new(),
			selection: Cell::new(0),
			scroll_top: Cell::new(0),
			current_size: Cell::new(None),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			focused: Cell::new(true),
		}
	}

	/// Replace the rendered rows. The selection follows the
	/// previously selected step when it still exists, and is
	/// clamped to the new bounds otherwise.
	pub fn set_items(&mut self, items: Vec<RailItem>) {
		let previous = self.selected_step();
		self.items = items;
		if let Some(step) = previous {
			self.select_step(step);
		}
		self.selection.set(self.selection());
	}

	/// The step under the selection, if any.
	#[must_use]
	pub fn selected_step(&self) -> Option<asyncgit::rebase::StepId> {
		self.items
			.get(self.selection.get())
			.and_then(|item| item.step)
	}

	/// The rail currently has a single selected step.  Keeping this
	/// projection on the component lets the modal router render a
	/// consistent selection indicator without knowing rebase details.
	#[must_use]
	pub fn selection_status(&self) -> Option<SelectionStatus> {
		if !self.focused() {
			return None;
		}
		self.selected_step().map(|_| SelectionStatus {
			extending: false,
			count: 1,
			unit: "step",
		})
	}

	/// Keep the selection on `step` when it is present.
	pub fn select_step(&self, step: asyncgit::rebase::StepId) {
		if let Some(index) =
			self.items.iter().position(|item| item.step == Some(step))
		{
			self.selection.set(index);
		}
	}

	const fn item_count(&self) -> usize {
		self.items.len()
	}

	const fn selection_max(&self) -> usize {
		self.items.len().saturating_sub(1)
	}

	fn selection(&self) -> usize {
		self.selection.get().min(self.items.len().saturating_sub(1))
	}
	fn move_selection(&self, scroll: ScrollType) -> bool {
		let page_offset = usize::from(
			self.current_size.get().unwrap_or_default().1,
		)
		.saturating_sub(1)
		.max(1);

		let new_selection = match scroll {
			ScrollType::Up => self.selection().saturating_sub(1),
			ScrollType::Down => self.selection().saturating_add(1),
			ScrollType::PageUp => {
				self.selection().saturating_sub(page_offset)
			}
			ScrollType::PageDown => {
				self.selection().saturating_add(page_offset)
			}
			ScrollType::Home => 0,
			ScrollType::End => self.selection_max(),
		};

		let new_selection = new_selection.min(self.selection_max());
		let changed = new_selection != self.selection();
		self.selection.set(new_selection);

		changed
	}

	const fn glyph_of(state: RowState) -> &'static str {
		match state {
			RowState::Pending => GLYPH_PENDING,
			RowState::Done => GLYPH_DONE,
			RowState::Current => GLYPH_CURRENT,
			RowState::Dropped => GLYPH_DROPPED,
			RowState::Stop => GLYPH_STOP,
			RowState::Failed => GLYPH_FAILED,
		}
	}

	/// Style of the glyph column. The current pointer is
	/// louder than plain rows regardless of selection; stop
	/// rows are deliberate (bold, not danger); failed rows
	/// are danger.
	fn glyph_style(
		&self,
		state: RowState,
		raw: bool,
		selected: bool,
	) -> ratatui::style::Style {
		if raw {
			// unknown syntax: muted, but selectable
			return self.theme.text(false, selected);
		}
		let theme = &self.theme;
		let base = match state {
			RowState::Pending => theme.text(true, false),
			RowState::Done => theme.option(true),
			RowState::Current => {
				theme.text_danger().add_modifier(Modifier::BOLD)
			}
			RowState::Dropped => theme.text(false, false),
			RowState::Stop => {
				theme.text(true, false).add_modifier(Modifier::BOLD)
			}
			RowState::Failed => theme.text_danger(),
		};
		theme.selected(base, selected)
	}

	fn is_raw(item: &RailItem) -> bool {
		item.word == "????"
	}

	fn row_line(&self, index: usize, width: usize) -> Line<'_> {
		let item = &self.items[index];
		let selected = index == self.selection();
		let raw = Self::is_raw(item);

		let label_width = width.saturating_sub(FIXED_WIDTH);
		let label = if item.fold {
			format!(" └ {}", item.label)
		} else {
			item.label.clone()
		};
		let label = truncate_ellipsis(&label, label_width);

		let text_style = if raw {
			self.theme.text(false, selected)
		} else {
			self.theme.text(true, selected)
		};

		let oid = item.short_oid.as_deref().map_or_else(
			|| String::from("       "),
			|oid| String::from(oid.unicode_truncate(7).0),
		);

		Line::from(vec![
			Span::styled(
				Self::glyph_of(item.state),
				self.glyph_style(item.state, raw, selected),
			),
			Span::styled(strings::symbol::EMPTY_SPACE, text_style),
			Span::styled(
				format!("{:<6}", item.word.unicode_truncate(6).0),
				text_style,
			),
			Span::styled(strings::symbol::EMPTY_SPACE, text_style),
			Span::styled(oid, self.theme.commit_hash(selected)),
			Span::styled(strings::symbol::EMPTY_SPACE, text_style),
			Span::styled(label, text_style),
		])
	}

	fn get_text(&self, height: usize, width: usize) -> Vec<Line<'_>> {
		let scroll_top = calc_scroll_top(
			self.scroll_top.get(),
			height,
			self.selection(),
		);
		self.scroll_top.set(scroll_top);

		self.items
			.iter()
			.enumerate()
			.skip(scroll_top)
			.take(height)
			.map(|(index, _)| self.row_line(index, width))
			.collect()
	}
}

/// Truncate to `width` display columns, marking a cut with an
/// ellipsis. Control characters are sanitized first: they are
/// unrenderable in a terminal, and zero-width boundaries are
/// where naive truncation goes wrong (see the audit — the
/// truncation itself is hand-rolled because the
/// `unicode-truncate` crate panics on right-to-left text).
#[must_use]
pub fn truncate_ellipsis(text: &str, width: usize) -> String {
	if width == 0 {
		return String::new();
	}
	let text = &crate::string_utils::sanitize_control_chars(text);
	let cut = crate::string_utils::truncate_to_width(text, width);
	if cut.len() < text.len() {
		let shortened =
			crate::string_utils::truncate_to_width(text, width - 1);
		format!("{shortened}{}", strings::symbol::ELLIPSIS)
	} else {
		String::from(cut)
	}
}

impl DrawableComponent for Rail {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		let current_size = (
			area.width.saturating_sub(2),
			area.height.saturating_sub(2),
		);
		self.current_size.set(Some(current_size));

		let height_in_lines = current_size.1 as usize;
		let width_in_cols = current_size.0 as usize;

		if self.items.is_empty() {
			f.render_widget(
				Paragraph::new(strings::rebase::rail_empty_hint())
					.block(
						Block::default()
							.borders(Borders::ALL)
							.title(Span::styled(
								self.title.as_ref(),
								self.theme.title(self.focused()),
							))
							.border_style(
								self.theme.block(self.focused()),
							),
					)
					.alignment(Alignment::Center),
				area,
			);
			return Ok(());
		}

		let title = format!(
			"{} {}/{}",
			self.title,
			self.item_count().saturating_sub(self.selection()),
			self.item_count(),
		);

		f.render_widget(
			Paragraph::new(
				self.get_text(height_in_lines, width_in_cols),
			)
			.block(
				Block::default()
					.borders(Borders::ALL)
					.title(Span::styled(
						title.as_str(),
						self.theme.title(self.focused()),
					))
					.border_style(self.theme.block(self.focused())),
			)
			.alignment(Alignment::Left),
			area,
		);

		draw_scrollbar(
			f,
			area,
			&self.theme,
			self.items.len(),
			self.selection(),
			Orientation::Vertical,
		);

		Ok(())
	}
}

impl Component for Rail {
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if let Event::Key(k) = ev {
			let selection_changed =
				if key_match(k, self.key_config.keys.move_up) {
					self.move_selection(ScrollType::Up)
				} else if key_match(k, self.key_config.keys.move_down)
				{
					self.move_selection(ScrollType::Down)
				} else if key_match(k, self.key_config.keys.home) {
					self.move_selection(ScrollType::Home)
				} else if key_match(k, self.key_config.keys.end) {
					self.move_selection(ScrollType::End)
				} else if key_match(k, self.key_config.keys.page_up) {
					self.move_selection(ScrollType::PageUp)
				} else if key_match(k, self.key_config.keys.page_down)
				{
					self.move_selection(ScrollType::PageDown)
				} else {
					false
				};
			return Ok(selection_changed.into());
		}
		Ok(EventState::NotConsumed)
	}

	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		_force_all: bool,
	) -> CommandBlocking {
		out.push(CommandInfo::new(
			strings::rebase::commands::scroll(&self.key_config),
			!self.items.is_empty(),
			true,
		));
		visibility_blocking(self)
	}

	fn is_visible(&self) -> bool {
		true
	}

	fn focused(&self) -> bool {
		self.focused.get()
	}

	fn focus(&mut self, focus: bool) {
		self.focused.set(focus);
	}
}

#[cfg(test)]
mod tests {
	#![allow(clippy::unwrap_used, clippy::panic)]

	use super::*;
	use crate::app::Environment;
	use asyncgit::rebase::plan::Plan;
	use asyncgit::rebase::todo::{
		CommitArg, Instruction, Todo, TodoLine,
	};
	use asyncgit::rebase::{policy::Row, StepId};
	use asyncgit::sync::CommitId;

	fn oid_hex(n: u64) -> String {
		format!("{n:040x}")
	}

	fn pick_row(n: u64) -> Row {
		Row {
			step: Some(StepId::FIRST),
			state: RowState::Pending,
			action: "pick",
			label: format!("subject {n}"),
			short_oid: Some(oid_hex(n)[..7].to_owned()),
			folds_into_previous: false,
		}
	}

	fn plan_with_pick(n: u64) -> Plan {
		Plan::from_todo(Todo::from_lines(vec![
			TodoLine::Instruction(Instruction::Pick {
				word: b"pick".as_slice().into(),
				commit: CommitArg::from_id(
					CommitId::from_str_unchecked(&oid_hex(n))
						.unwrap(),
				),
				after: b"".as_slice().into(),
			}),
		]))
	}

	#[test]
	fn action_word_mapping() {
		assert_eq!(action_word(Some("pick")), "PICK");
		assert_eq!(action_word(Some("update-ref")), "UPD-REF");
		assert_eq!(action_word(Some("mystic")), "????");
		assert_eq!(action_word(None), "????");
	}

	#[test]
	fn rail_items_resolves_words_from_plan() {
		let plan = plan_with_pick(7);
		let rows = vec![pick_row(7)];
		let items = rail_items(&rows, Some(&plan));
		assert_eq!(items[0].word, "PICK");
		assert_eq!(items[0].label, "subject 7");
	}

	#[test]
	fn rail_items_raw_rows_are_muted_placeholders() {
		let rows = vec![Row {
			step: Some(StepId::FIRST),
			state: RowState::Failed,
			action: "",
			label: String::from("future syntax"),
			short_oid: None,
			folds_into_previous: false,
		}];
		let items = rail_items(&rows, None);
		assert_eq!(items[0].word, "????");
		assert!(Rail::is_raw(&items[0]));
	}

	#[test]
	fn truncate_ellipsis_cuts_and_marks() {
		assert_eq!(truncate_ellipsis("hello", 10), "hello");
		assert_eq!(truncate_ellipsis("hello world", 5), "hell…");
		assert_eq!(truncate_ellipsis("hello", 0), "");
	}

	#[test]
	fn selection_movement_and_step_lookup() {
		let env = Environment::test_env();
		let mut rail = Rail::new(&env, "Plan");
		let rows: Vec<RailItem> = (1..=5)
			.map(|n| RailItem {
				step: Some(StepId::from_value(n).unwrap()),
				state: RowState::Pending,
				word: "PICK",
				short_oid: None,
				label: format!("s{n}"),
				fold: false,
			})
			.collect();
		rail.set_items(rows);

		assert_eq!(
			rail.selected_step(),
			Some(StepId::from_value(1).unwrap())
		);
		assert!(rail.move_selection(ScrollType::Down));
		assert_eq!(
			rail.selected_step(),
			Some(StepId::from_value(2).unwrap())
		);
		assert!(rail.move_selection(ScrollType::End));
		assert_eq!(
			rail.selected_step(),
			Some(StepId::from_value(5).unwrap())
		);
		assert!(rail.move_selection(ScrollType::Home));
		// moving above the top clamps
		assert!(!rail.move_selection(ScrollType::Up));
	}

	#[test]
	fn selection_status_describes_the_active_step() {
		let env = Environment::test_env();
		let mut rail = Rail::new(&env, "Plan");
		assert_eq!(rail.selection_status(), None);
		rail.set_items(vec![RailItem {
			step: Some(StepId::FIRST),
			state: RowState::Pending,
			word: "PICK",
			short_oid: None,
			label: String::from("subject"),
			fold: false,
		}]);
		assert_eq!(
			rail.selection_status(),
			Some(SelectionStatus {
				extending: false,
				count: 1,
				unit: "step",
			})
		);
		rail.focus(false);
		assert_eq!(rail.selection_status(), None);
	}

	#[test]
	fn set_items_keeps_selection_on_step() {
		let env = Environment::test_env();
		let mut rail = Rail::new(&env, "Plan");
		let items: Vec<RailItem> = (1..=3)
			.map(|n| RailItem {
				step: Some(StepId::from_value(n).unwrap()),
				state: RowState::Pending,
				word: "PICK",
				short_oid: None,
				label: format!("s{n}"),
				fold: false,
			})
			.collect();
		rail.set_items(items.clone());
		rail.move_selection(ScrollType::Down);
		rail.move_selection(ScrollType::Down);

		let reordered: Vec<RailItem> = vec![
			items[2].clone(),
			items[0].clone(),
			items[1].clone(),
		];
		rail.set_items(reordered);
		assert_eq!(
			rail.selected_step(),
			Some(StepId::from_value(3).unwrap())
		);
	}

	/// Hostile rendering: a thousand rows (scrolling math to
	/// the end and back), control characters, RTL text, 300
	/// character paths, combining marks, and degenerate
	/// terminal sizes (down to the 1x1 TestBackend minimum).
	/// None of these may panic or corrupt the draw.
	#[test]
	fn hostile_rendering_never_panics() {
		use ratatui::{backend::TestBackend, Terminal};

		fn draw(rail: &Rail, width: u16, height: u16) {
			let mut terminal =
				Terminal::new(TestBackend::new(width, height))
					.unwrap();
			terminal
				.draw(|f| rail.draw(f, f.area()).unwrap())
				.unwrap();
		}

		let env = Environment::test_env();
		let mut rail = Rail::new(&env, "Plan");

		// 1000 rows with adversarial labels
		let mut long = String::from("very/long/");
		long.extend(std::iter::repeat_n('x', 300));
		let samples: [&str; 6] = [
			"normal subject",
			"ctrl \u{7}\u{1b}[31m chars",
			"RTL \u{5d9}\u{5d5}\u{5dd} \u{633}\u{644}\u{627} mixed",
			&long,
			"combining e\u{301}\u{301}\u{301} marks",
			"",
		];
		let items: Vec<RailItem> = (0..1000)
			.map(|n| RailItem {
				step: StepId::from_value(
					u32::try_from(n).unwrap() + 1,
				),
				state: match n % 6 {
					0 => RowState::Pending,
					1 => RowState::Done,
					2 => RowState::Current,
					3 => RowState::Dropped,
					4 => RowState::Stop,
					_ => RowState::Failed,
				},
				word: "PICK",
				short_oid: Some(format!("{n:07x}")),
				label: samples[n % samples.len()].to_owned(),
				fold: n % 3 == 2,
			})
			.collect();
		rail.set_items(items);

		for (width, height) in
			[(200u16, 50u16), (80, 24), (20, 5), (2, 2), (1, 1)]
		{
			draw(&rail, width, height);
		}
		// scroll to both extremes through degenerate sizes
		rail.move_selection(ScrollType::End);
		draw(&rail, 1, 1);
		draw(&rail, 20, 5);
		rail.move_selection(ScrollType::Home);
		draw(&rail, 20, 5);
		rail.move_selection(ScrollType::PageDown);
		rail.move_selection(ScrollType::PageUp);
		draw(&rail, 200, 50);
	}
}
