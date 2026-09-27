//! The inspector: what the right-hand panel says, per
//! [`asyncgit::rebase::policy::Inspector`] variant.
//!
//! The builder is pure: it takes the theme, the policy
//! projection, the session facts (and optionally the snapshot,
//! for conflict shapes) and produces the block title and the
//! styled lines. The component only renders what was built.

// TODO(app wiring): remove once app.rs constructs the tab —
// until then the inspector is only reachable from tests.
#![allow(dead_code)]

use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		DrawableComponent, EventState,
	},
	keys::SharedKeyConfig,
	strings,
	ui::style::{SharedTheme, Theme},
};
use anyhow::Result;
use asyncgit::rebase::{
	engine::Base,
	journal::{Outcome, Scope, Session},
	policy::{
		Availability, Blocker, Inspector as Variant, Phase, Policy,
		Row, RowState, Stop,
	},
	snapshot::{Conflict, ConflictShape, Snapshot},
};
use crossterm::event::Event;
use ratatui::{
	layout::{Alignment, Rect},
	style::{Modifier, Style},
	text::{Line, Span},
	widgets::{Block, Borders, Paragraph},
	Frame,
};
use std::cell::RefCell;

/// One styled line under construction.
struct Out<'a> {
	theme: &'a Theme,
	lines: Vec<Line<'static>>,
}

impl<'a> Out<'a> {
	const fn new(theme: &'a Theme) -> Self {
		Self {
			theme,
			lines: Vec::new(),
		}
	}

	fn push(&mut self, text: String, style: Style) {
		self.lines.push(Line::from(Span::styled(text, style)));
	}

	fn plain(&mut self, text: String) {
		self.push(text, Style::default());
	}

	fn muted(&mut self, text: String) {
		self.push(text, self.theme.text(false, false));
	}

	fn danger(&mut self, text: String) {
		self.push(text, self.theme.text_danger());
	}

	fn loud(&mut self, text: String) {
		self.push(
			text,
			Style::default().add_modifier(Modifier::BOLD),
		);
	}
}

/// Label of a conflict shape, for the conflict file list.
#[must_use]
pub const fn shape_label(shape: ConflictShape) -> &'static str {
	match shape {
		ConflictShape::Content => "content",
		ConflictShape::ModifyDelete => "modify/delete",
		ConflictShape::BothAdded => "both added",
		ConflictShape::RenameRename => "rename/rename",
		ConflictShape::RenameDelete => "rename/delete",
		ConflictShape::DirectoryFile => "dir/file",
	}
}

/// Summary of the projected result, derived from the rail
/// rows: heads with their fold counts plus dropped rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewGroup {
	///
	pub head_label: String,
	///
	pub head_state: RowState,
	///
	pub folded: usize,
	///
	pub dropped: bool,
}

/// Derive the preview groups from rail rows (order
/// preserved): a group head is a non-fold row; fold rows
/// attach to it. Dropped rows render their own line.
#[must_use]
pub fn preview_groups(rail: &[Row]) -> Vec<PreviewGroup> {
	let mut groups: Vec<PreviewGroup> = Vec::new();
	for row in rail {
		if row.folds_into_previous {
			if let Some(head) = groups.last_mut() {
				head.folded += 1;
				continue;
			}
		}
		groups.push(PreviewGroup {
			head_label: row.label.clone(),
			head_state: row.state,
			folded: 0,
			dropped: row.state == RowState::Dropped,
		});
	}
	groups
}

fn base_name(scope: &Scope) -> String {
	match &scope.base {
		Base::Upstream(id) => id.get_short_string(),
		Base::Onto { onto, .. } => onto.get_short_string(),
		Base::Root => String::from("--root"),
	}
}

/// The short destination label of a draft scope (`--root`
/// when the whole history is replayed).
#[must_use]
pub fn scope_base_name(scope: &Scope) -> String {
	base_name(scope)
}

/// The human sentence for one blocker.
#[must_use]
pub fn blocker_text(blocker: &Blocker) -> String {
	match blocker {
		Blocker::Conflicts(paths) => {
			if paths.is_empty() {
				String::from("conflicts must be resolved first")
			} else {
				format!("unresolved conflicts: {}", paths.join(", "))
			}
		}
		Blocker::Busy => String::from("a command is in flight"),
		Blocker::Invalid(issues) => issues.join("; "),
		Blocker::LocalChanges => {
			String::from("local changes need protection first")
		}
		Blocker::GitVersion { feature, needs } => {
			format!("{feature} needs git {needs}")
		}
		Blocker::NothingToDo => String::from("nothing to do"),
		Blocker::Unmanaged => {
			String::from("not available in an adopted session")
		}
		Blocker::Ended => String::from("the session already ended"),
		Blocker::EmptyDecision => String::from(
			"decide keep/skip for the empty commit first",
		),
	}
}

/// Build title and lines for the current variant. Pure.
#[must_use]
pub fn build(
	theme: &Theme,
	policy: &Policy,
	session: &Session,
	snapshot: Option<&Snapshot>,
) -> (String, Vec<Line<'static>>) {
	let mut out = Out::new(theme);
	let title = match &policy.inspector {
		Variant::Preview => {
			let groups = preview_groups(&policy.rail);
			let commits =
				groups.iter().filter(|g| !g.dropped).count();
			build_preview(&groups, &mut out);
			strings::rebase::inspector_title_preview(commits)
		}
		Variant::Review => {
			build_review(policy, session, &mut out);
			String::from(strings::rebase::inspector_title_review())
		}
		Variant::Stop => {
			build_stop(policy, snapshot, &mut out);
			String::from(strings::rebase::inspector_title_stop())
		}
		Variant::Outcome => {
			build_outcome(policy, session, &mut out);
			String::from(strings::rebase::inspector_title_outcome())
		}
		Variant::Reconcile => {
			build_reconcile(policy, &mut out);
			String::from(strings::rebase::inspector_title_reconcile())
		}
	};
	(title, out.lines)
}

fn build_preview(groups: &[PreviewGroup], out: &mut Out<'_>) {
	for group in groups {
		let glyph = if group.dropped {
			String::from("×")
		} else {
			String::from(match group.head_state {
				RowState::Done => strings::symbol::CHECKMARK,
				RowState::Failed => "!",
				RowState::Stop => "‖",
				RowState::Current => "▶",
				_ => "○",
			})
		};
		let label = if group.dropped {
			strings::rebase::preview_dropped(&group.head_label)
		} else if group.folded > 0 {
			format!(
				"{}{}",
				group.head_label,
				strings::rebase::preview_folded(group.folded)
			)
		} else {
			group.head_label.clone()
		};
		out.plain(format!("{glyph} {label}"));
	}
}

fn build_review(
	policy: &Policy,
	session: &Session,
	out: &mut Out<'_>,
) {
	let Some(scope) = session.scope.as_ref() else {
		out.muted(String::from(
			"no scope — the draft is not pinned to a branch yet",
		));
		return;
	};

	let branch = scope.branch.as_deref().unwrap_or("detached HEAD");
	out.plain(strings::rebase::review_destination(
		branch,
		&base_name(scope),
	));

	let groups = preview_groups(&policy.rail);
	let drops = groups.iter().filter(|g| g.dropped).count();
	let heads = groups.iter().filter(|g| !g.dropped).count();
	out.plain(strings::rebase::review_counts(heads, drops));

	let local_changes = matches!(
		&policy.actions.start,
		Availability::Disabled(blockers)
			if blockers.contains(&Blocker::LocalChanges)
	);
	if local_changes {
		out.loud(String::from(
			strings::rebase::review_changes_pending(),
		));
	} else {
		out.plain(String::from(
			strings::rebase::review_changes_none(),
		));
	}
}

fn build_stop(
	policy: &Policy,
	snapshot: Option<&Snapshot>,
	out: &mut Out<'_>,
) {
	let stop = match &policy.phase {
		Phase::Stopped(stop) => stop.clone(),
		Phase::Adopted { .. } => {
			out.plain(String::from(
				"adopted an external rebase — observing only",
			));
			return;
		}
		_ => {
			out.plain(String::from(
				"a command is running — the workspace follows",
			));
			return;
		}
	};

	match &stop {
		Stop::Conflicts { paths } => {
			if paths.is_empty() {
				out.loud(String::from(
					strings::rebase::stop_conflicts_resolved(),
				));
				return;
			}
			out.plain(strings::rebase::stop_conflicts_unresolved(
				paths.len(),
			));
			for path in paths {
				let shapes = snapshot
					.and_then(|snapshot| {
						snapshot
							.conflicts
							.iter()
							.find(|conflict| &conflict.path == path)
					})
					.map(Conflict::shapes)
					.unwrap_or_default();
				let shapes = shapes
					.iter()
					.map(|shape| shape_label(*shape))
					.collect::<Vec<_>>()
					.join("/");
				if shapes.is_empty() {
					out.danger(strings::rebase::stop_conflict_path(
						path,
					));
				} else {
					out.danger(format!("! {path} ({shapes})"));
				}
			}
		}
		Stop::Edit { commit } => {
			out.plain(strings::rebase::stop_edit(
				&commit.get_short_string(),
			));
			out.muted(
				String::from(strings::rebase::stop_edit_hint()),
			);
		}
		Stop::Break => {
			out.plain(String::from(strings::rebase::stop_break()));
		}
		Stop::BecameEmpty { commit } => {
			out.plain(strings::rebase::stop_became_empty(
				&commit.get_short_string(),
			));
			out.muted(String::from(
				"keep [K] or skip [k] to proceed",
			));
		}
		Stop::Message { request } => {
			out.plain(strings::rebase::stop_message(request));
			out.muted(String::from(
				strings::rebase::stop_message_hint(),
			));
		}
		Stop::ExecFailed { command, code } => {
			out.danger(strings::rebase::stop_exec_failed(command));
			out.muted(code.map_or_else(
				|| {
					String::from(
						strings::rebase::stop_exec_unknown_exit(),
					)
				},
				|code| format!("exit status: {code}"),
			));
		}
		Stop::Unknown { note } => {
			out.plain(strings::rebase::stop_unknown(note));
		}
	}
}

fn build_outcome(
	policy: &Policy,
	session: &Session,
	out: &mut Out<'_>,
) {
	let Phase::Done(done) = policy.phase else {
		return;
	};

	out.loud(strings::rebase::outcome_label(done));

	if let Some(Outcome::Completed(tip)) = session.outcome.as_ref() {
		out.plain(strings::rebase::outcome_tip(
			&tip.get_short_string(),
		));
	}

	if session.protection.is_some() {
		if policy.actions.restore.is_enabled() {
			out.plain(String::from(
				strings::rebase::outcome_protection_outstanding(),
			));
			out.muted(String::from(
				strings::rebase::outcome_restore_hint(),
			));
		} else {
			out.muted(String::from(
				strings::rebase::outcome_protection_restored(),
			));
		}
	}

	out.muted(String::from(strings::rebase::outcome_close_hint()));
}

fn build_reconcile(policy: &Policy, out: &mut Out<'_>) {
	match &policy.phase {
		Phase::Reconciling { reason } => {
			out.plain(strings::rebase::reconcile_reason(reason));
		}
		Phase::Unsupported { operation } => {
			out.plain(format!(
				"the repository is in an unsupported state: \\
				 {operation:?}"
			));
		}
		_ => {}
	}
	out.muted(String::from(strings::rebase::reconcile_no_mutation()));
}

/// Renders the prebuilt inspector content.
pub struct Inspector {
	title: RefCell<String>,
	lines: RefCell<Vec<Line<'static>>>,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
}

impl Inspector {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			title: RefCell::new(String::new()),
			lines: RefCell::new(Vec::new()),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
		}
	}

	///
	pub fn set_content(
		&self,
		title: String,
		lines: Vec<Line<'static>>,
	) {
		*self.title.borrow_mut() = title;
		*self.lines.borrow_mut() = lines;
	}
}

impl DrawableComponent for Inspector {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		let title = self.title.borrow();
		let lines = self.lines.borrow();

		f.render_widget(
			Paragraph::new(lines.clone())
				.block(
					Block::default()
						.borders(Borders::ALL)
						.title(Span::styled(
							title.as_str(),
							self.theme.title(true),
						))
						.border_style(self.theme.block(true)),
				)
				.alignment(Alignment::Left),
			area,
		);

		Ok(())
	}
}

impl Component for Inspector {
	fn event(&mut self, _ev: &Event) -> Result<EventState> {
		Ok(EventState::NotConsumed)
	}

	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		_force_all: bool,
	) -> CommandBlocking {
		out.push(CommandInfo::new(
			strings::rebase::commands::toggle_inspector(
				&self.key_config,
			),
			true,
			true,
		));
		visibility_blocking(self)
	}

	fn is_visible(&self) -> bool {
		true
	}
}
