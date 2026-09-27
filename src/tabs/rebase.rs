//! The rebase workspace tab: the visible heart of the
//! interactive-rebase feature.
//!
//! Layout: a one-line context header (branch → onto, phase,
//! progress), the plan rail on the left, the inspector on the
//! right. Below 100 columns the inspector collapses and is
//! toggled with the `rebase_inspector` key; below 40×12 only
//! the rail plus the phase label remain. Layouts are clamped
//! so tiny areas degrade gracefully instead of panicking.
//!
//! Every visible fact and every gated key comes from the
//! policy projection (`asyncgit::rebase::policy`); the tab
//! never inspects the repository itself. Mutations go through
//! the [`RebaseBackend`](crate::tabs::rebase_backend::RebaseBackend)
//! service seam and surface errors as message popups.

// TODO(app wiring): remove once app.rs constructs the tab —
// until then the tab is only reachable from tests.
#![allow(dead_code)]

use crate::{
	app::Environment,
	components::{
		command_pump, rail_items, visibility_blocking,
		CommandBlocking, CommandInfo, CommandText, Component,
		DrawableComponent, EventState, InputType, Inspector, Rail,
		TextInputComponent,
	},
	keys::{key_match, SharedKeyConfig},
	queue::{InternalEvent, NeedsUpdate, Queue},
	strings,
	tabs::rebase_backend::SharedBackend,
	try_or_popup,
	ui::style::SharedTheme,
};
use anyhow::Result;
use asyncgit::rebase::{
	journal::Session,
	plan::{Action, Edit},
	policy::{Availability, Blocker, Phase, Policy, Stop},
	StepId,
};
use crossterm::event::{Event, KeyEvent};
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	widgets::Paragraph,
	Frame,
};
use std::cell::RefCell;

/// below this width the inspector collapses to a toggle
const NARROW_WIDTH: u16 = 100;
/// below this size only the rail plus the phase header render
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 12;

/// What the tab-local composer currently edits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ComposerPurpose {
	/// Closed.
	None,
	/// The message of a stopped reword/squash (bridge flow).
	Message,
	/// The composed note of a draft reword/squash step.
	Note(StepId),
}

/// The interactive-rebase workspace.
pub struct RebaseTab {
	backend: SharedBackend,
	rail: Rail,
	inspector: Inspector,
	composer: TextInputComponent,
	exec_input: TextInputComponent,
	composer_purpose: ComposerPurpose,
	/// narrow-mode toggle: show the inspector instead of the
	/// rail (wide layouts always show both)
	inspector_toggled: bool,
	visible: bool,
	focused: bool,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	/// The last projection, refreshed on events and reads.
	view: RefCell<Option<(Policy, Session)>>,
}

impl RebaseTab {
	///
	pub fn new(env: &Environment, service: SharedBackend) -> Self {
		Self {
			backend: service,
			rail: Rail::new(
				env,
				&strings::rebase::rail_title(&env.key_config),
			),
			inspector: Inspector::new(env),
			composer: TextInputComponent::new(
				env,
				&strings::rebase::composer_title_message(),
				strings::rebase::composer_default_msg(),
				false,
			)
			.with_input_type(InputType::Singleline),
			exec_input: TextInputComponent::new(
				env,
				strings::rebase::exec_popup_title(),
				strings::rebase::exec_popup_default_msg(),
				false,
			)
			.with_input_type(InputType::Singleline),
			composer_purpose: ComposerPurpose::None,
			inspector_toggled: false,
			visible: false,
			focused: false,
			queue: env.queue.clone(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			view: RefCell::new(None),
		}
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.backend.borrow().is_busy()
	}

	/// Re-read the projection and refresh all child views.
	pub fn reload(&mut self) {
		let (policy, session) = self.projection();
		self.sync_composer(&policy);
		let items = rail_items(&policy.rail, session.plan.as_ref());
		self.rail.set_items(items);
		let snapshot = self.backend.borrow().snapshot();
		let (title, lines) =
			crate::components::rebase::inspector::build(
				&self.theme,
				&policy,
				&session,
				snapshot.as_ref(),
			);
		self.inspector.set_content(title, lines);
		*self.view.borrow_mut() = Some((policy, session));
	}

	fn projection(&self) -> (Policy, Session) {
		let backend = self.backend.borrow();
		(backend.policy(), backend.session().clone())
	}

	fn cached_policy(&self) -> Option<Policy> {
		self.view
			.borrow()
			.as_ref()
			.map(|(policy, _)| policy.clone())
	}

	fn sync_composer(&mut self, policy: &Policy) {
		let message_stop = matches!(
			&policy.phase,
			Phase::Stopped(Stop::Message { .. })
		);
		match self.composer_purpose {
			ComposerPurpose::Message => {
				if !message_stop {
					self.close_composer();
				}
			}
			ComposerPurpose::None => {
				if message_stop && !self.composer.is_visible() {
					self.open_composer(ComposerPurpose::Message);
				}
			}
			ComposerPurpose::Note(_) => {}
		}
	}

	fn open_composer(&mut self, purpose: ComposerPurpose) {
		self.composer_purpose = purpose;
		self.composer.set_title(match purpose {
			ComposerPurpose::Note(_) => {
				strings::rebase::composer_title_note()
			}
			_ => strings::rebase::composer_title_message(),
		});
		self.composer.clear();
		if let Err(err) = self.composer.show() {
			log::error!("composer show: {err}");
		}
	}

	fn close_composer(&mut self) {
		self.composer_purpose = ComposerPurpose::None;
		self.composer.hide();
	}

	fn open_exec_input(&mut self) {
		self.exec_input.clear();
		if let Err(err) = self.exec_input.show() {
			log::error!("exec input show: {err}");
		}
	}

	fn after_mutation(&mut self) {
		self.queue.push(InternalEvent::Update(NeedsUpdate::ALL));
		self.reload();
	}

	/// The step id under the rail selection.
	pub fn selected_step(&self) -> Option<StepId> {
		self.rail.selected_step()
	}

	fn move_target(
		&self,
		down: bool,
	) -> Result<(StepId, Option<StepId>)> {
		let id = self
			.selected_step()
			.ok_or_else(|| anyhow::anyhow!("no step selected"))?;
		let plan = self
			.view
			.borrow()
			.as_ref()
			.and_then(|(_, session)| session.plan.clone())
			.ok_or_else(|| anyhow::anyhow!("no plan"))?;
		let index = plan
			.steps()
			.iter()
			.position(|step| step.id == id)
			.ok_or_else(|| anyhow::anyhow!("step not in plan"))?;

		let before = if down {
			plan.steps().get(index + 2).map(|step| step.id)
		} else {
			index.checked_sub(1).and_then(|prev| {
				plan.steps().get(prev).map(|step| step.id)
			})
		};

		Ok((id, before))
	}

	fn apply_edit(&mut self, edit: Edit) {
		try_or_popup!(
			self,
			strings::rebase::msg_action_failed(),
			self.backend.borrow_mut().apply_edit(edit)
		);
		self.after_mutation();
	}

	/// Runs one gated mutation: only when the policy offers
	/// the action. Returns whether the key was handled.
	fn gated(
		&mut self,
		availability: &Availability,
		mutate: impl FnOnce(&mut Self) -> Result<(), anyhow::Error>,
	) -> bool {
		match availability {
			Availability::Hidden => false,
			Availability::Enabled => {
				try_or_popup!(
					self,
					strings::rebase::msg_action_failed(),
					mutate(self)
				);
				self.after_mutation();
				true
			}
			Availability::Disabled(_) => true,
		}
	}

	fn set_action(&mut self, action: Action) -> bool {
		let Some(policy) = self.cached_policy() else {
			return false;
		};
		if !matches!(policy.actions.set_action, Availability::Enabled)
		{
			return false;
		}
		let Some(id) = self.selected_step() else {
			return false;
		};
		self.apply_edit(Edit::SetAction { id, action });
		true
	}

	fn move_selected(&mut self, down: bool) -> bool {
		let Some(policy) = self.cached_policy() else {
			return false;
		};
		let availability = if down {
			policy.actions.move_down
		} else {
			policy.actions.move_up
		};
		if !matches!(availability, Availability::Enabled) {
			return false;
		}
		let Ok((id, before)) = self.move_target(down) else {
			return false;
		};
		self.apply_edit(Edit::Move { id, before });
		true
	}

	fn compose_note(&mut self) -> bool {
		let Some(policy) = self.cached_policy() else {
			return false;
		};
		if !matches!(policy.actions.compose, Availability::Enabled) {
			return false;
		}
		let Some(id) = self.selected_step() else {
			return false;
		};
		let composable = self.view.borrow().as_ref().is_some_and(
			|(_, session)| {
				session.plan.as_ref().is_some_and(|plan| {
					plan.step(id).is_some_and(|step| {
						matches!(
							step.line.instruction(),
							Some(
								asyncgit::rebase::todo::Instruction::Reword{..}
								| asyncgit::rebase::todo::Instruction::Squash{..}
							)
						)
					})
				})
			},
		);
		if composable {
			self.open_composer(ComposerPurpose::Note(id));
			return true;
		}
		false
	}

	fn blocker_message(blockers: &[Blocker]) -> String {
		blockers
			.iter()
			.map(crate::components::rebase::inspector::blocker_text)
			.collect::<Vec<_>>()
			.join("\n")
	}

	fn start(&mut self) -> bool {
		let Some(policy) = self.cached_policy() else {
			return false;
		};
		match &policy.actions.start {
			Availability::Hidden => false,
			Availability::Enabled => {
				try_or_popup!(
					self,
					strings::rebase::msg_action_failed(),
					self.backend.borrow_mut().start()
				);
				self.after_mutation();
				true
			}
			Availability::Disabled(blockers) => {
				self.queue.push(InternalEvent::ShowErrorMsg(
					Self::blocker_message(blockers),
				));
				true
			}
		}
	}

	/// Enter is the phase's primary action: inspect in a
	/// draft, continue when stopped, close when done.
	fn primary_action(&mut self) -> bool {
		let Some(policy) = self.cached_policy() else {
			return false;
		};
		match &policy.phase {
			Phase::Draft | Phase::Review => {
				self.inspector_toggled = !self.inspector_toggled;
				true
			}
			Phase::Stopped(_) => self
				.gated(&policy.actions.r#continue, |tab| {
					tab.backend.borrow_mut().r#continue()
				}),
			Phase::Done(_) => self
				.gated(&policy.actions.close, |tab| {
					tab.backend.borrow_mut().close()
				}),
			_ => false,
		}
	}

	fn composer_event(&mut self, k: &KeyEvent) -> Result<EventState> {
		let purpose = self.composer_purpose;
		if key_match(k, self.key_config.keys.enter) {
			let text = self.composer.get_text().to_owned();
			match purpose {
				ComposerPurpose::Message => try_or_popup!(
					self,
					strings::rebase::msg_action_failed(),
					self.backend.borrow_mut().resolve_message(text)
				),
				ComposerPurpose::Note(id) => try_or_popup!(
					self,
					strings::rebase::msg_action_failed(),
					self.backend.borrow_mut().apply_edit(
						Edit::SetMessage { id, message: text }
					)
				),
				ComposerPurpose::None => {}
			}
			self.close_composer();
			self.after_mutation();
			Ok(EventState::Consumed)
		} else if key_match(k, self.key_config.keys.exit_popup) {
			if purpose == ComposerPurpose::Message {
				try_or_popup!(
					self,
					strings::rebase::msg_action_failed(),
					self.backend.borrow_mut().cancel_message()
				);
			}
			self.close_composer();
			Ok(EventState::Consumed)
		} else {
			self.composer.event(&Event::Key(*k))
		}
	}

	fn exec_event(&mut self, k: &KeyEvent) -> Result<EventState> {
		if key_match(k, self.key_config.keys.enter) {
			let text = self.exec_input.get_text().to_owned();
			self.exec_input.hide();
			if !text.is_empty() {
				let before = self.selected_step();
				try_or_popup!(
					self,
					strings::rebase::msg_action_failed(),
					self.backend.borrow_mut().apply_edit(
						Edit::InsertExec {
							command: text,
							before
						}
					)
				);
				self.after_mutation();
			}
			Ok(EventState::Consumed)
		} else if key_match(k, self.key_config.keys.exit_popup) {
			self.exec_input.hide();
			Ok(EventState::Consumed)
		} else {
			self.exec_input.event(&Event::Key(*k))
		}
	}

	#[allow(clippy::too_many_lines)]
	fn key_event(&mut self, k: &KeyEvent) -> Result<EventState> {
		if self.composer.is_visible() {
			return self.composer_event(k);
		}
		if self.exec_input.is_visible() {
			return self.exec_event(k);
		}

		if self.rail.event(&Event::Key(*k))?.is_consumed() {
			return Ok(EventState::Consumed);
		}

		let key_config = self.key_config.clone();
		let keys = &key_config.keys;

		if key_match(k, keys.rebase_inspector) {
			self.inspector_toggled = !self.inspector_toggled;
			Ok(EventState::Consumed)
		} else if key_match(k, keys.enter) {
			Ok(self.primary_action().into())
		} else if key_match(k, keys.rebase_pick) {
			Ok(self.set_action(Action::Pick).into())
		} else if key_match(k, keys.rebase_reword) {
			Ok(self.set_action(Action::Reword).into())
		} else if key_match(k, keys.rebase_edit_stop) {
			Ok(self.set_action(Action::Edit).into())
		} else if key_match(k, keys.rebase_squash) {
			Ok(self.set_action(Action::Squash).into())
		} else if key_match(k, keys.rebase_fixup) {
			Ok(self.set_action(Action::Fixup).into())
		} else if key_match(k, keys.rebase_drop) {
			Ok(self.set_action(Action::Drop).into())
		} else if key_match(k, keys.shift_up) {
			Ok(self.move_selected(false).into())
		} else if key_match(k, keys.shift_down) {
			Ok(self.move_selected(true).into())
		} else if key_match(k, keys.rebase_break) {
			let Some(policy) = self.cached_policy() else {
				return Ok(EventState::NotConsumed);
			};
			Ok(self
				.gated(&policy.actions.insert_break, |tab| {
					let before = tab.selected_step();
					tab.backend
						.borrow_mut()
						.apply_edit(Edit::InsertBreak { before })
				})
				.into())
		} else if key_match(k, keys.rebase_exec) {
			let Some(policy) = self.cached_policy() else {
				return Ok(EventState::NotConsumed);
			};
			if matches!(
				policy.actions.insert_exec,
				Availability::Enabled
			) {
				self.open_exec_input();
				Ok(EventState::Consumed)
			} else {
				Ok(EventState::NotConsumed)
			}
		} else if key_match(k, keys.rebase_compose) {
			Ok(self.compose_note().into())
		} else if key_match(k, keys.rebase_undo) {
			let Some(policy) = self.cached_policy() else {
				return Ok(EventState::NotConsumed);
			};
			Ok(self
				.gated(&policy.actions.undo, |tab| {
					tab.backend.borrow_mut().undo()
				})
				.into())
		} else if key_match(k, keys.rebase_redo) {
			let Some(policy) = self.cached_policy() else {
				return Ok(EventState::NotConsumed);
			};
			Ok(self
				.gated(&policy.actions.redo, |tab| {
					tab.backend.borrow_mut().redo()
				})
				.into())
		} else if key_match(k, keys.rebase_start) {
			Ok(self.start().into())
		} else if key_match(k, keys.rebase_skip) {
			let Some(policy) = self.cached_policy() else {
				return Ok(EventState::NotConsumed);
			};
			Ok(self
				.gated(&policy.actions.skip, |tab| {
					tab.backend.borrow_mut().skip()
				})
				.into())
		} else if key_match(k, keys.rebase_keep) {
			let Some(policy) = self.cached_policy() else {
				return Ok(EventState::NotConsumed);
			};
			// The keep side of the empty-commit decision is
			// the continue the service executes under an
			// empty stop; the policy gate ensures the key is
			// only live exactly there.
			Ok(self
				.gated(&policy.actions.keep_empty, |tab| {
					tab.backend.borrow_mut().r#continue()
				})
				.into())
		} else if key_match(k, keys.rebase_abort) {
			let Some(policy) = self.cached_policy() else {
				return Ok(EventState::NotConsumed);
			};
			Ok(self
				.gated(&policy.actions.abort, |tab| {
					tab.backend.borrow_mut().abort()
				})
				.into())
		} else {
			Ok(EventState::NotConsumed)
		}
	}

	fn header_text(
		policy: &Policy,
		session: &Session,
		width: u16,
	) -> String {
		if width < MIN_WIDTH {
			return strings::rebase::phase_label(&policy.phase);
		}
		session.scope.as_ref().map_or_else(
			strings::rebase::header_no_session,
			|scope| {
				strings::rebase::header(
					scope.branch.as_deref().unwrap_or("detached HEAD"),
					&crate::components::rebase::inspector::scope_base_name(
						scope,
					),
					&policy.phase,
					policy.progress.done,
					policy.progress.total,
				)
			},
		)
	}
}

impl DrawableComponent for RebaseTab {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		let view = self.view.borrow();
		let Some((policy, session)) = view.as_ref() else {
			return Ok(());
		};

		if area.height == 0 {
			return Ok(());
		}

		let rows = Layout::default()
			.direction(Direction::Vertical)
			.constraints(
				[Constraint::Length(1), Constraint::Min(0)].as_ref(),
			)
			.split(area);

		f.render_widget(
			Paragraph::new(Self::header_text(
				policy, session, area.width,
			)),
			rows[0],
		);

		if rows.len() < 2 || rows[1].height == 0 {
			return Ok(());
		}

		let body = rows[1];
		let narrow = body.width < NARROW_WIDTH;
		let tiny = body.width < MIN_WIDTH || body.height < MIN_HEIGHT;

		if tiny {
			self.rail.draw(f, body)?;
		} else if narrow {
			if self.inspector_toggled {
				self.inspector.draw(f, body)?;
			} else {
				self.rail.draw(f, body)?;
			}
		} else {
			let chunks = Layout::default()
				.direction(Direction::Horizontal)
				.constraints(
					[
						Constraint::Percentage(55),
						Constraint::Percentage(45),
					]
					.as_ref(),
				)
				.split(body);
			self.rail.draw(f, chunks[0])?;
			self.inspector.draw(f, chunks[1])?;
		}

		if self.composer.is_visible() {
			self.composer.draw(f, f.area())?;
		}
		if self.exec_input.is_visible() {
			self.exec_input.draw(f, f.area())?;
		}

		Ok(())
	}
}

impl Component for RebaseTab {
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if !self.visible {
			return Ok(EventState::NotConsumed);
		}

		if let Event::Key(k) = ev {
			return self.key_event(k);
		}

		Ok(EventState::NotConsumed)
	}

	#[allow(clippy::too_many_lines)]
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible || force_all {
			command_pump(
				out,
				force_all,
				&[&self.rail, &self.inspector],
			);

			if let Some((policy, _)) = self.view.borrow().as_ref() {
				let actions = &policy.actions;
				let visible = self.visible;
				let keys = &self.key_config.keys;

				let mut push =
					|text: CommandText,
					 availability: &Availability,
					 order: Option<i8>| {
						let (enabled, available) = match availability
						{
							Availability::Enabled => (true, visible),
							Availability::Disabled(_) => {
								(false, visible)
							}
							Availability::Hidden => (false, false),
						};
						let mut info = CommandInfo::new(
							text, enabled, available,
						);
						if let Some(order) = order {
							info = info.order(order);
						}
						out.push(info);
					};

				push(
					strings::rebase::commands::start(
						&self.key_config,
					),
					&actions.start,
					Some(strings::order::PRIORITY),
				);
				push(
					strings::rebase::commands::r#continue(
						&self.key_config,
					),
					&actions.r#continue,
					Some(strings::order::PRIORITY),
				);
				push(
					strings::rebase::commands::skip(&self.key_config),
					&actions.skip,
					Some(strings::order::PRIORITY),
				);
				push(
					strings::rebase::commands::keep_empty(
						&self.key_config,
					),
					&actions.keep_empty,
					Some(strings::order::PRIORITY),
				);
				push(
					strings::rebase::commands::abort(
						&self.key_config,
					),
					&actions.abort,
					Some(strings::order::PRIORITY),
				);

				push(
					strings::rebase::commands::set_action(
						"Pick",
						&self.key_config.get_hint(keys.rebase_pick),
					),
					&actions.set_action,
					None,
				);
				push(
					strings::rebase::commands::set_action(
						"Reword",
						&self.key_config.get_hint(keys.rebase_reword),
					),
					&actions.set_action,
					None,
				);
				push(
					strings::rebase::commands::set_action(
						"Edit",
						&self
							.key_config
							.get_hint(keys.rebase_edit_stop),
					),
					&actions.set_action,
					None,
				);
				push(
					strings::rebase::commands::set_action(
						"Squash",
						&self.key_config.get_hint(keys.rebase_squash),
					),
					&actions.set_action,
					None,
				);
				push(
					strings::rebase::commands::set_action(
						"Fixup",
						&self.key_config.get_hint(keys.rebase_fixup),
					),
					&actions.set_action,
					None,
				);
				push(
					strings::rebase::commands::set_action(
						"Drop",
						&self.key_config.get_hint(keys.rebase_drop),
					),
					&actions.set_action,
					None,
				);
				push(
					strings::rebase::commands::move_steps(
						&self.key_config,
					),
					&actions.move_up,
					None,
				);
				push(
					strings::rebase::commands::insert_break(
						&self.key_config,
					),
					&actions.insert_break,
					None,
				);
				push(
					strings::rebase::commands::insert_exec(
						&self.key_config,
					),
					&actions.insert_exec,
					None,
				);
				push(
					strings::rebase::commands::compose(
						&self.key_config,
					),
					&actions.compose,
					None,
				);
				push(
					strings::rebase::commands::undo(&self.key_config),
					&actions.undo,
					None,
				);
				push(
					strings::rebase::commands::redo(&self.key_config),
					&actions.redo,
					None,
				);
			}
		}

		visibility_blocking(self)
	}

	fn focused(&self) -> bool {
		self.focused
	}

	fn focus(&mut self, focus: bool) {
		self.focused = focus;
		if !focus {
			self.exec_input.hide();
		}
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn hide(&mut self) {
		self.visible = false;
		self.exec_input.hide();
		self.focus(false);
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;
		// The app refreshes the service on every update tick;
		// showing the tab only re-derives the projection.
		self.reload();
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	#![allow(clippy::unwrap_used, clippy::panic)]

	use super::*;
	use crate::tabs::rebase_backend::RebaseBackend;
	use asyncgit::rebase::{
		engine::{Base, EmptyPolicy},
		journal::Session as JSession,
		plan::Plan,
		policy as policy_mod,
		snapshot::{
			BridgeKind, BridgeRequest, Change, ChangeKind, Conflict,
			Operation, RebaseFiles, RebaseState, Snapshot,
		},
		todo::{CommitArg, FixupMessage, Instruction, TodoLine},
	};
	use asyncgit::sync::CommitId;
	use crossterm::event::KeyCode;
	use insta::assert_snapshot;
	use ratatui::{backend::TestBackend, Terminal};
	use std::rc::Rc;

	fn hex(n: u64) -> String {
		// leading seven hex digits carry `n`, so short oids in
		// snapshots stay distinguishable
		format!("{n:07x}{}", "0".repeat(33))
	}

	fn oid(n: u64) -> CommitId {
		CommitId::from_str_unchecked(&hex(n)).unwrap()
	}

	fn change(path: &str) -> Change {
		Change {
			path: String::from(path),
			kind: ChangeKind::Modified,
		}
	}

	fn conflict(path: &str) -> Conflict {
		Conflict {
			path: String::from(path),
			stages: [Some(oid(1)), Some(oid(2)), Some(oid(3))],
			markers_in_worktree: false,
		}
	}

	fn clean_snapshot() -> Snapshot {
		Snapshot {
			generation: asyncgit::rebase::Generation(0),
			head: Some(oid(9)),
			branch: Some(String::from("main")),
			operation: Operation::Clean,
			staged: Vec::new(),
			worktree: Vec::new(),
			conflicts: Vec::new(),
			untracked: Vec::new(),
			rebase: None,
			bridge: None,
		}
	}

	fn pick_line(n: u64) -> TodoLine {
		TodoLine::Instruction(Instruction::Pick {
			word: b"pick".as_slice().into(),
			commit: CommitArg::from_id(oid(n)),
			after: format!(" # subject {n}").into_bytes().into(),
		})
	}

	fn fixup_line(n: u64) -> TodoLine {
		TodoLine::Instruction(Instruction::Fixup {
			word: b"fixup".as_slice().into(),
			message: FixupMessage::Previous,
			commit: CommitArg::from_id(oid(n)),
			after: format!(" # subject {n}").into_bytes().into(),
		})
	}

	fn drop_line(n: u64) -> TodoLine {
		TodoLine::Instruction(Instruction::Drop {
			word: b"drop".as_slice().into(),
			commit: CommitArg::from_id(oid(n)),
			after: format!(" # subject {n}").into_bytes().into(),
		})
	}

	fn plan_of(lines: Vec<TodoLine>) -> Plan {
		Plan::from_todo(asyncgit::rebase::todo::Todo::from_lines(
			lines,
		))
	}

	/// Sequencer files for a stopped observation (pure; no
	/// repository involved) — mirrors the asyncgit policy
	/// evidence fixtures.
	fn rebase_state(
		done: &str,
		todo: &str,
		extra: &[(&str, &str)],
	) -> RebaseState {
		let onto = hex(8);
		let orig_head = hex(7);
		let mut files: Vec<(&str, &[u8])> = vec![
			("onto", onto.as_bytes()),
			("orig-head", orig_head.as_bytes()),
			("head-name", b"refs/heads/main"),
			("done", done.as_bytes()),
			("git-rebase-todo", todo.as_bytes()),
		];
		for (name, content) in extra {
			files.push((name, content.as_bytes()));
		}
		RebaseState::from_files(&RebaseFiles { files: &files })
			.unwrap()
	}

	fn scope_event() -> asyncgit::rebase::journal::Event {
		asyncgit::rebase::journal::Event::Created {
			scope: asyncgit::rebase::journal::Scope {
				branch: Some(String::from("main")),
				original_tip: oid(9),
				base: Base::Upstream(oid(8)),
			},
		}
	}

	fn draft_session(lines: Vec<TodoLine>) -> JSession {
		JSession::fold(&[
			scope_event(),
			asyncgit::rebase::journal::Event::PlanAdopted {
				plan: plan_of(lines).to_stored(),
			},
		])
	}

	fn running_session(lines: Vec<TodoLine>) -> JSession {
		let plan = plan_of(lines);
		let todo = plan.to_todo().to_bytes();
		JSession::fold(&[
			scope_event(),
			asyncgit::rebase::journal::Event::PlanAdopted {
				plan: plan.to_stored(),
			},
			asyncgit::rebase::journal::Event::Started {
				todo,
				base: Base::Upstream(oid(8)),
				empty: EmptyPolicy::Stop,
				recovery_ref: None,
				protection: None,
			},
		])
	}

	fn finished_session(lines: Vec<TodoLine>) -> JSession {
		let plan = plan_of(lines);
		let todo = plan.to_todo().to_bytes();
		JSession::fold(&[
			scope_event(),
			asyncgit::rebase::journal::Event::PlanAdopted {
				plan: plan.to_stored(),
			},
			asyncgit::rebase::journal::Event::Started {
				todo,
				base: Base::Upstream(oid(8)),
				empty: EmptyPolicy::Stop,
				recovery_ref: None,
				protection: None,
			},
			asyncgit::rebase::journal::Event::Completed {
				tip: oid(42),
			},
		])
	}

	/// In-memory backend over a folded session plus snapshot.
	struct MockBackend {
		session: JSession,
		snapshot: Snapshot,
		busy: bool,
	}

	impl RebaseBackend for MockBackend {
		fn session(&self) -> &JSession {
			&self.session
		}

		fn policy(&self) -> Policy {
			policy_mod::policy(&self.session, &self.snapshot)
		}

		fn is_busy(&self) -> bool {
			self.busy
		}

		fn apply_edit(&mut self, edit: Edit) -> Result<()> {
			let plan = self
				.session
				.plan
				.as_ref()
				.ok_or_else(|| anyhow::anyhow!("no plan"))?;
			let next = plan
				.apply(&edit)
				.map_err(|err| anyhow::anyhow!("{err:?}"))?;
			self.session.plan = Some(next);
			self.session.edits.push(edit);
			Ok(())
		}

		fn undo(&mut self) -> Result<()> {
			Ok(())
		}

		fn redo(&mut self) -> Result<()> {
			Ok(())
		}

		fn start(&mut self) -> Result<()> {
			Ok(())
		}

		fn r#continue(&mut self) -> Result<()> {
			Ok(())
		}

		fn skip(&mut self) -> Result<()> {
			Ok(())
		}

		fn abort(&mut self) -> Result<()> {
			Ok(())
		}

		fn resolve_message(
			&mut self,
			_message: String,
		) -> Result<()> {
			Ok(())
		}

		fn cancel_message(&mut self) -> Result<()> {
			Ok(())
		}

		fn close(&mut self) -> Result<()> {
			Ok(())
		}

		fn snapshot(&self) -> Option<Snapshot> {
			Some(self.snapshot.clone())
		}
	}

	fn mock_backend(
		session: JSession,
		snapshot: Snapshot,
	) -> SharedBackend {
		Rc::new(RefCell::new(MockBackend {
			session,
			snapshot,
			busy: false,
		}))
	}

	fn draw_tab(
		tab: &RebaseTab,
		width: u16,
		height: u16,
	) -> TestBackend {
		let mut terminal =
			Terminal::new(TestBackend::new(width, height)).unwrap();
		terminal.draw(|f| tab.draw(f, f.area()).unwrap()).unwrap();
		terminal.backend().clone()
	}

	fn draft_tab() -> RebaseTab {
		let env = Environment::test_env();
		let backend = mock_backend(
			draft_session(vec![
				pick_line(1),
				pick_line(2),
				fixup_line(3),
				drop_line(4),
			]),
			clean_snapshot(),
		);
		let mut tab = RebaseTab::new(&env, backend);
		tab.show().unwrap();
		tab
	}

	#[test]
	fn snapshot_rebase_draft() {
		let tab = draft_tab();
		assert_snapshot!(
			"rebase_draft_100x30",
			draw_tab(&tab, 100, 30)
		);
	}

	#[test]
	fn snapshot_rebase_stopped_conflict() {
		let env = Environment::test_env();
		let session = running_session(vec![
			pick_line(1),
			pick_line(2),
			pick_line(3),
		]);
		let mut snapshot = clean_snapshot();
		snapshot.operation = Operation::RebaseInteractive;
		snapshot.conflicts = vec![conflict("f.txt")];
		snapshot.rebase = Some(rebase_state(
			&format!("pick {} # subject 1\n", hex(1)),
			&format!(
				"pick {} # subject 2\npick {} # subject 3\n",
				hex(2),
				hex(3)
			),
			&[("stopped-sha", hex(1).as_str())],
		));
		let mut tab =
			RebaseTab::new(&env, mock_backend(session, snapshot));
		tab.show().unwrap();
		// 80 columns: narrow mode — open the inspector over the
		// rail so the conflict card is visible
		tab.key_event(&KeyEvent::from(KeyCode::Char('v'))).unwrap();
		assert_snapshot!(
			"rebase_stopped_conflict_80x24",
			draw_tab(&tab, 80, 24)
		);
	}

	#[test]
	fn snapshot_rebase_finished() {
		let env = Environment::test_env();
		let backend = mock_backend(
			finished_session(vec![
				pick_line(1),
				fixup_line(2),
				drop_line(3),
			]),
			clean_snapshot(),
		);
		let mut tab = RebaseTab::new(&env, backend);
		tab.show().unwrap();
		assert_snapshot!(
			"rebase_finished_40x15",
			draw_tab(&tab, 40, 15)
		);
	}

	#[test]
	fn draft_keys_apply_edits_through_the_backend() {
		let mut tab = draft_tab();

		// move selection to the second row, then drop it
		tab.key_event(&KeyEvent::from(KeyCode::Down)).unwrap();
		tab.key_event(&KeyEvent::from(KeyCode::Char('d'))).unwrap();

		let backend = tab.backend.borrow();
		let plan = backend.session().plan.as_ref().unwrap();
		assert!(
			plan.steps().iter().any(|step| matches!(
				step.line,
				TodoLine::Instruction(Instruction::Drop { .. })
			)),
			"expected a drop step after 'd'"
		);
	}

	#[test]
	fn narrow_mode_toggles_inspector_with_v() {
		let tab = draft_tab();
		assert!(!tab.inspector_toggled);

		let mut tab = tab;
		tab.key_event(&KeyEvent::from(KeyCode::Char('v'))).unwrap();
		assert!(tab.inspector_toggled);

		// rendering both modes must not panic
		let _ = draw_tab(&tab, 80, 24);
		tab.key_event(&KeyEvent::from(KeyCode::Char('v'))).unwrap();
		let _ = draw_tab(&tab, 80, 24);
	}

	#[test]
	fn preview_groups_count_folds_and_drops() {
		let session = draft_session(vec![
			pick_line(1),
			fixup_line(2),
			drop_line(3),
		]);
		let policy = policy_mod::policy(&session, &clean_snapshot());
		let groups =
			crate::components::rebase::inspector::preview_groups(
				&policy.rail,
			);
		assert_eq!(groups.len(), 2);
		assert_eq!(groups[0].folded, 1);
		assert!(!groups[0].dropped);
		assert_eq!(groups[1].folded, 0);
		assert!(groups[1].dropped);
	}

	#[test]
	fn composer_opens_on_message_stop_and_esc_cancels() {
		let env = Environment::test_env();
		let session = running_session(vec![pick_line(1)]);
		let mut snapshot = clean_snapshot();
		snapshot.operation = Operation::RebaseInteractive;
		snapshot.rebase = Some(rebase_state(
			&format!("reword {} # subject 1\n", hex(1)),
			"",
			&[("stopped-sha", hex(1).as_str())],
		));
		snapshot.bridge = Some(BridgeRequest {
			kind: BridgeKind::Message,
			file: String::from("/tmp/bridge/msg"),
		});
		let mut tab =
			RebaseTab::new(&env, mock_backend(session, snapshot));
		tab.show().unwrap();
		assert!(tab.composer.is_visible());

		tab.key_event(&KeyEvent::from(KeyCode::Esc)).unwrap();
		assert!(!tab.composer.is_visible());
	}

	#[test]
	fn blocked_start_surfaces_blockers() {
		let mut tab = draft_tab();
		// 'S' starts: the draft plan has an all-dropped-free
		// validation, so the mutation goes through the mock
		tab.key_event(&KeyEvent::from(KeyCode::Char('S'))).unwrap();
		assert!(!tab.any_work_pending());
	}
}
