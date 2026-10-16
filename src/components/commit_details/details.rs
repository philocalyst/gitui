use crate::{
	app::Environment,
	components::{
		commit_details::style::style_detail,
		dialog_paragraph,
		utils::{scroll_vertical::VerticalScroll, time_to_string},
		CommandBlocking, CommandInfo, Component, DrawableComponent,
		EventState, ScrollType,
	},
	keys::{key_match, SharedKeyConfig},
	strings::{self, order},
	ui::style::SharedTheme,
};
use anyhow::Result;
use asyncgit::{
	asyncjob::{AsyncJob, AsyncSingleJob, RunParams},
	sync::{
		self, CommitDetails, CommitId, CommitMessage, RepoPath, Tag,
	},
	AsyncGitNotification,
};
use crossterm::event::Event;
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	style::{Modifier, Style},
	text::{Line, Span, Text},
	Frame,
};
use std::{
	borrow::Cow,
	cell::Cell,
	sync::{Arc, Mutex},
};
use sync::CommitTags;

use super::style::Detail;

type CommitDetailsResult = std::result::Result<CommitDetails, String>;
type CommitDetailsJobResult = Option<(CommitId, CommitDetailsResult)>;

pub struct DetailsComponent {
	repo: RepoPath,
	selected_id: Option<CommitId>,
	commit_details_job: AsyncSingleJob<CommitDetailsJob>,
	data: Option<CommitDetails>,
	tags: Vec<Tag>,
	theme: SharedTheme,
	focused: bool,
	current_width: Cell<u16>,
	scroll: VerticalScroll,
	scroll_to_bottom_next_draw: Cell<bool>,
	key_config: SharedKeyConfig,
}

#[derive(Clone)]
struct CommitDetailsJob {
	repo: RepoPath,
	id: CommitId,
	result: Arc<Mutex<CommitDetailsJobResult>>,
}

impl CommitDetailsJob {
	fn new(repo: RepoPath, id: CommitId) -> Self {
		Self {
			repo,
			id,
			result: Arc::new(Mutex::new(None)),
		}
	}

	fn take_result(&self) -> CommitDetailsJobResult {
		self.result.lock().ok()?.take()
	}
}

impl AsyncJob for CommitDetailsJob {
	type Notification = AsyncGitNotification;
	type Progress = ();

	fn run(
		&mut self,
		_params: RunParams<Self::Notification, Self::Progress>,
	) -> asyncgit::Result<Self::Notification> {
		let result = sync::get_commit_details(&self.repo, self.id)
			.map_err(|error| error.to_string());
		if let Ok(mut stored) = self.result.lock() {
			*stored = Some((self.id, result));
		}
		Ok(AsyncGitNotification::CommitDetails)
	}
}

type WrappedCommitMessage<'a> =
	(Vec<Cow<'a, str>>, Vec<Cow<'a, str>>);

impl DetailsComponent {
	///
	pub fn new(env: &Environment, focused: bool) -> Self {
		Self {
			repo: env.repo.borrow().clone(),
			selected_id: None,
			commit_details_job: AsyncSingleJob::new(
				env.sender_git.clone(),
			),
			data: None,
			tags: Vec::new(),
			theme: env.theme.clone(),
			focused,
			scroll_to_bottom_next_draw: Cell::new(false),
			current_width: Cell::new(0),
			scroll: VerticalScroll::new(),
			key_config: env.key_config.clone(),
		}
	}

	pub fn set_commit(
		&mut self,
		id: Option<CommitId>,
		tags: Option<CommitTags>,
	) {
		if self.selected_id != id {
			self.selected_id = id;
			self.data = None;
			self.tags.clear();
			self.scroll.reset();
			self.commit_details_job.cancel();
			if let Some(id) = id {
				self.commit_details_job.spawn(CommitDetailsJob::new(
					self.repo.clone(),
					id,
				));
			}
		}
		self.tags = tags.unwrap_or_default();
		self.poll_commit_details();
	}

	fn poll_commit_details(&mut self) {
		let Some(job) = self.commit_details_job.take_last() else {
			return;
		};
		let Some((id, result)) = job.take_result() else {
			return;
		};
		if self.selected_id != Some(id) {
			return;
		}
		match result {
			Ok(details) => self.data = Some(details),
			Err(error) => log::error!("commit details {id}: {error}"),
		}
	}

	pub fn any_work_pending(&self) -> bool {
		self.commit_details_job.is_pending()
	}

	fn wrap_commit_details(
		message: &CommitMessage,
		width: usize,
	) -> WrappedCommitMessage<'_> {
		let width = width.max(1);
		let wrapped_title = bwrap::wrap!(&message.subject, width)
			.lines()
			.map(String::from)
			.map(Cow::from)
			.collect();

		if let Some(ref body) = message.body {
			let wrapped_message: Vec<Cow<'_, str>> =
				bwrap::wrap!(body, width)
					.lines()
					.map(String::from)
					.map(Cow::from)
					.collect();

			(wrapped_title, wrapped_message)
		} else {
			(wrapped_title, vec![])
		}
	}

	fn get_wrapped_lines(
		data: Option<&CommitDetails>,
		width: usize,
	) -> WrappedCommitMessage<'_> {
		if let Some(data) = data {
			if let Some(message) = &data.message {
				return Self::wrap_commit_details(message, width);
			}
		}

		(vec![], vec![])
	}

	fn get_number_of_lines(
		details: Option<&CommitDetails>,
		width: usize,
	) -> usize {
		let (wrapped_title, wrapped_message) =
			Self::get_wrapped_lines(details, width);

		wrapped_title.len() + wrapped_message.len()
	}

	/// Height for the commit summary when it shares a pane with the file list
	/// and diff. Long messages remain scrollable inside a bounded viewport.
	pub fn preferred_height(
		&self,
		width: u16,
		max_height: u16,
	) -> u16 {
		const MESSAGE_MIN_HEIGHT: u16 = 3;
		const MESSAGE_MAX_LINES: usize = 4;

		let full_info = self.get_text_info(false);
		let full_info_height =
			u16::try_from(full_info.len()).unwrap_or(u16::MAX) + 2;
		let compact_info = max_height
			< full_info_height.saturating_add(MESSAGE_MIN_HEIGHT);
		let info_height =
			u16::try_from(self.get_text_info(compact_info).len())
				.unwrap_or(u16::MAX)
				.saturating_add(2);
		let message_lines = Self::get_number_of_lines(
			self.data.as_ref(),
			usize::from(width.saturating_sub(2)),
		);
		let visible_message_lines =
			message_lines.clamp(1, MESSAGE_MAX_LINES);
		let message_height = u16::try_from(visible_message_lines)
			.unwrap_or(u16::MAX)
			.saturating_add(2)
			.max(MESSAGE_MIN_HEIGHT);

		info_height.saturating_add(message_height).min(max_height)
	}

	fn get_theme_for_line(&self, bold: bool) -> Style {
		if bold {
			self.theme.text(true, false).add_modifier(Modifier::BOLD)
		} else {
			self.theme.text(true, false)
		}
	}

	fn get_wrapped_text_message(
		&self,
		width: usize,
		height: usize,
	) -> Vec<Line<'_>> {
		let (wrapped_title, wrapped_message) =
			Self::get_wrapped_lines(self.data.as_ref(), width);

		[&wrapped_title[..], &wrapped_message[..]]
			.concat()
			.iter()
			.enumerate()
			.skip(self.scroll.get_top())
			.take(height)
			.map(|(i, line)| {
				Line::from(vec![Span::styled(
					line.clone(),
					self.get_theme_for_line(i < wrapped_title.len()),
				)])
			})
			.collect()
	}

	#[allow(clippy::too_many_lines)]
	fn get_text_info(&self, compact: bool) -> Vec<Line<'_>> {
		self.data.as_ref().map_or_else(Vec::new, |data| {
			let mut res = vec![
				Line::from(vec![
					style_detail(&self.theme, &Detail::Author),
					Span::styled(
						Cow::from(format!(
							"{} <{}>",
							data.author.name, data.author.email
						)),
						self.theme.text(true, false),
					),
				]),
				Line::from(vec![
					style_detail(&self.theme, &Detail::Date),
					Span::styled(
						Cow::from(time_to_string(
							data.author.time,
							false,
						)),
						self.theme.text(true, false),
					),
				]),
			];

			if let Some(ref committer) = data.committer {
				res.extend(vec![
					Line::from(vec![
						style_detail(&self.theme, &Detail::Committer),
						Span::styled(
							Cow::from(format!(
								"{} <{}>",
								committer.name, committer.email
							)),
							self.theme.text(true, false),
						),
					]),
					Line::from(vec![
						style_detail(&self.theme, &Detail::Date),
						Span::styled(
							Cow::from(time_to_string(
								committer.time,
								false,
							)),
							self.theme.text(true, false),
						),
					]),
				]);
			}

			res.push(Line::from(vec![
				Span::styled(
					Cow::from(strings::commit::details_sha()),
					self.theme.text(false, false),
				),
				Span::styled(
					Cow::from(data.hash.clone()),
					self.theme.text(true, false),
				),
			]));

			if !self.tags.is_empty() {
				res.push(Line::from(style_detail(
					&self.theme,
					&Detail::Sha,
				)));

				res.push(Line::from(
					itertools::Itertools::intersperse(
						self.tags.iter().map(|tag| {
							Span::styled(
								Cow::from(&tag.name),
								self.theme.text(true, false),
							)
						}),
						Span::styled(
							Cow::from(","),
							self.theme.text(true, false),
						),
					)
					.collect::<Vec<Span>>(),
				));
			}

			if compact {
				// In a short details pane, keep the immutable commit identity
				// visible and retain the author/date. Optional committer and tag
				// rows can use the extra space at larger terminal sizes.
				let sha_index =
					if data.committer.is_some() { 4 } else { 2 };
				let sha = res.remove(sha_index);
				let mut compact = vec![sha];
				compact.extend(res.into_iter().take(2));
				return compact;
			}

			res
		})
	}

	fn move_scroll_top(&self, move_type: ScrollType) -> bool {
		if self.data.is_some() {
			self.scroll.move_top(move_type)
		} else {
			false
		}
	}
}

impl DrawableComponent for DetailsComponent {
	#[allow(clippy::too_many_lines)]
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		const CANSCROLL_STRING: &str = "[\u{2026}]";
		const EMPTY_STRING: &str = "";
		const MESSAGE_MIN_HEIGHT: u16 = 3;

		// At very short terminal heights, two bordered panels cannot both be
		// drawn without their borders colliding. Keep the useful commit message
		// in one clean panel and let it use the full available height.
		if rect.height < 8 {
			let width = rect.width.saturating_sub(2);
			let height = rect.height.saturating_sub(2);
			self.current_width.set(width);
			let number_of_lines = Self::get_number_of_lines(
				self.data.as_ref(),
				usize::from(width),
			);
			self.scroll.update_no_selection(
				number_of_lines,
				usize::from(height),
			);
			if self.scroll_to_bottom_next_draw.get() {
				self.scroll.move_top(ScrollType::End);
				self.scroll_to_bottom_next_draw.set(false);
			}
			let can_scroll = usize::from(height) < number_of_lines;
			let message_title = format!(
				"{} {}",
				strings::commit::details_message_title(
					&self.key_config
				),
				if !self.focused && can_scroll {
					CANSCROLL_STRING
				} else {
					EMPTY_STRING
				},
			);
			f.render_widget(
				dialog_paragraph(
					&message_title,
					Text::from(self.get_wrapped_text_message(
						usize::from(width),
						usize::from(height),
					)),
					&self.theme,
					self.focused,
				),
				rect,
			);
			if self.focused {
				self.scroll.draw(f, rect, &self.theme);
			}
			return Ok(());
		}

		let full_info = self.get_text_info(false);
		let full_info_height = u16::try_from(full_info.len())
			.unwrap_or(u16::MAX)
			.saturating_add(2);
		let compact_info = rect.height
			< full_info_height.saturating_add(MESSAGE_MIN_HEIGHT);
		let info = if compact_info {
			self.get_text_info(true)
		} else {
			full_info
		};
		let info_height = u16::try_from(info.len())
			.unwrap_or(u16::MAX)
			.saturating_add(2)
			.min(rect.height.saturating_sub(MESSAGE_MIN_HEIGHT));
		let chunks = Layout::default()
			.direction(Direction::Vertical)
			.constraints([
				Constraint::Length(info_height),
				Constraint::Min(0),
			])
			.split(rect);

		f.render_widget(
			dialog_paragraph(
				&strings::commit::details_info_title(
					&self.key_config,
				),
				Text::from(info),
				&self.theme,
				false,
			),
			chunks[0],
		);

		// We have to take the border into account which is one character on
		// each side.
		let border_width: u16 = 2;

		let width = chunks[1].width.saturating_sub(border_width);
		let height = chunks[1].height.saturating_sub(border_width);

		self.current_width.set(width);

		let number_of_lines = Self::get_number_of_lines(
			self.data.as_ref(),
			usize::from(width),
		);

		self.scroll.update_no_selection(
			number_of_lines,
			usize::from(height),
		);

		if self.scroll_to_bottom_next_draw.get() {
			self.scroll.move_top(ScrollType::End);
			self.scroll_to_bottom_next_draw.set(false);
		}

		let can_scroll = usize::from(height) < number_of_lines;

		f.render_widget(
			dialog_paragraph(
				&format!(
					"{} {}",
					strings::commit::details_message_title(
						&self.key_config,
					),
					if !self.focused && can_scroll {
						CANSCROLL_STRING
					} else {
						EMPTY_STRING
					}
				),
				Text::from(self.get_wrapped_text_message(
					width as usize,
					height as usize,
				)),
				&self.theme,
				self.focused,
			),
			chunks[1],
		);

		if self.focused {
			self.scroll.draw(f, chunks[1], &self.theme);
		}

		Ok(())
	}
}

impl Component for DetailsComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		let width = usize::from(self.current_width.get());
		let number_of_lines =
			Self::get_number_of_lines(self.data.as_ref(), width);

		out.push(
			CommandInfo::new(
				strings::commands::navigate_commit_message(
					&self.key_config,
				),
				number_of_lines > 0,
				self.focused || force_all,
			)
			.order(order::NAV),
		);

		CommandBlocking::PassingOn
	}

	fn event(&mut self, event: &Event) -> Result<EventState> {
		if self.focused {
			if let Event::Key(e) = event {
				return Ok(
					if key_match(e, self.key_config.keys.move_up) {
						self.move_scroll_top(ScrollType::Up).into()
					} else if key_match(
						e,
						self.key_config.keys.move_down,
					) {
						self.move_scroll_top(ScrollType::Down).into()
					} else if key_match(
						e,
						self.key_config.keys.page_up,
					) {
						self.move_scroll_top(ScrollType::PageUp)
							.into()
					} else if key_match(
						e,
						self.key_config.keys.page_down,
					) {
						self.move_scroll_top(ScrollType::PageDown)
							.into()
					} else if key_match(e, self.key_config.keys.home)
						|| key_match(e, self.key_config.keys.shift_up)
					{
						self.move_scroll_top(ScrollType::Home).into()
					} else if key_match(e, self.key_config.keys.end)
						|| key_match(
							e,
							self.key_config.keys.shift_down,
						) {
						self.move_scroll_top(ScrollType::End).into()
					} else {
						EventState::NotConsumed
					},
				);
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn focused(&self) -> bool {
		self.focused
	}

	fn focus(&mut self, focus: bool) {
		if self.focused == focus {
			return;
		}
		if focus {
			self.scroll_to_bottom_next_draw.set(true);
		} else {
			self.scroll.reset();
		}

		self.focused = focus;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		app::Environment,
		components::{
			CommitDetailsComponent, Component, DrawableComponent,
		},
	};
	use asyncgit::sync::CommitId;

	fn get_wrapped_lines(
		message: &CommitMessage,
		width: usize,
	) -> Vec<Cow<'_, str>> {
		let (wrapped_title, wrapped_message) =
			DetailsComponent::wrap_commit_details(message, width);

		[&wrapped_title[..], &wrapped_message[..]].concat()
	}

	#[test]
	fn test_textwrap() {
		let message = CommitMessage::from("Commit message");

		assert_eq!(
			get_wrapped_lines(&message, 7),
			vec!["Commit", "message"]
		);
		assert_eq!(
			get_wrapped_lines(&message, 14),
			vec!["Commit message"]
		);
		assert_eq!(
			get_wrapped_lines(&message, 0),
			vec!["Commit", "message"]
		);

		let message_with_newline =
			CommitMessage::from("Commit message\n");

		assert_eq!(
			get_wrapped_lines(&message_with_newline, 7),
			vec!["Commit", "message"]
		);
		assert_eq!(
			get_wrapped_lines(&message_with_newline, 14),
			vec!["Commit message"]
		);
		assert_eq!(
			get_wrapped_lines(&message, 0),
			vec!["Commit", "message"]
		);

		let message_with_body = CommitMessage::from(
			"Commit message\nFirst line\nSecond line",
		);

		assert_eq!(
			get_wrapped_lines(&message_with_body, 7),
			vec![
				"Commit", "message", "First", "line", "Second",
				"line"
			]
		);
		assert_eq!(
			get_wrapped_lines(&message_with_body, 14),
			vec!["Commit message", "First line", "Second line"]
		);
		assert_eq!(
			get_wrapped_lines(&message_with_body, 7),
			vec![
				"Commit", "message", "First", "line", "Second",
				"line"
			]
		);
	}

	#[test]
	fn repeating_selected_commit_keeps_cached_details_and_scroll() {
		let env = Environment::test_env();
		let mut details = DetailsComponent::new(&env, false);
		let id = CommitId::default();
		details.selected_id = Some(id);
		details.data = Some(CommitDetails {
			message: Some(CommitMessage::from("cached details")),
			..CommitDetails::default()
		});
		details.scroll.set_top(7);

		details.set_commit(Some(id), None);

		assert_eq!(
			details
				.data
				.as_ref()
				.and_then(|data| data.message.as_ref())
				.map(|message| message.subject.as_str()),
			Some("cached details"),
		);
		assert_eq!(details.scroll.get_top(), 7);
		assert!(!details.any_work_pending());
	}

	#[test]
	fn commit_identity_and_subject_fit_compact_inspector_layouts() {
		const SUBJECT: &str = "inspector compact subject";
		let (temp, repo) = git2_testing::repo_init_empty();
		std::fs::write(temp.path().join("file.txt"), "content")
			.unwrap();
		let add = std::process::Command::new("git")
			.args(["add", "file.txt"])
			.current_dir(temp.path())
			.output()
			.unwrap();
		assert!(add.status.success());
		let commit = std::process::Command::new("git")
			.args(["commit", "-m", SUBJECT])
			.env("GIT_AUTHOR_NAME", "Author")
			.env("GIT_AUTHOR_EMAIL", "author@example.com")
			.env("GIT_COMMITTER_NAME", "Author")
			.env("GIT_COMMITTER_EMAIL", "author@example.com")
			.current_dir(temp.path())
			.output()
			.unwrap();
		assert!(commit.status.success());
		let oid = repo.head().unwrap().target().unwrap();
		let id = CommitId::new(oid);
		let env = Environment::test_env();
		env.repo.replace(temp.path().to_path_buf().into());
		let mut details = CommitDetailsComponent::new(&env);
		details
			.set_commits(
				Some(asyncgit::CommitFilesParams::from(id)),
				None,
			)
			.unwrap();
		details.show().unwrap();
		for _ in 0..100 {
			details.single_details.poll_commit_details();
			if details.single_details.data.is_some() {
				break;
			}
			std::thread::sleep(std::time::Duration::from_millis(5));
		}
		assert!(details.single_details.data.is_some());
		let full_hash = id.to_string();
		let default_backend =
			ratatui::backend::TestBackend::new(90, 8);
		let mut default_terminal =
			ratatui::Terminal::new(default_backend).unwrap();
		default_terminal
			.draw(|frame| {
				details
					.single_details
					.draw(frame, Rect::new(0, 0, 90, 8))
					.unwrap();
			})
			.unwrap();
		let default_rendered = default_terminal
			.backend()
			.buffer()
			.content
			.iter()
			.map(|cell| cell.symbol())
			.collect::<String>();
		assert!(
			default_rendered.contains(SUBJECT),
			"default layout lost the commit subject: {default_rendered}"
		);
		assert!(
			default_rendered.contains(&full_hash),
			"default layout lost the commit SHA: {default_rendered}"
		);
		assert!(
			default_rendered.contains(&strings::commit::details_info_title(
				&details.single_details.key_config
			)),
			"default layout should keep the metadata panel: {default_rendered}"
		);

		let compact_backend =
			ratatui::backend::TestBackend::new(90, 6);
		let mut compact_terminal =
			ratatui::Terminal::new(compact_backend).unwrap();
		compact_terminal
			.draw(|frame| {
				details
					.single_details
					.draw(frame, Rect::new(0, 0, 90, 6))
					.unwrap();
			})
			.unwrap();
		let compact_rendered = compact_terminal
			.backend()
			.buffer()
			.content
			.iter()
			.map(|cell| cell.symbol())
			.collect::<String>();
		assert!(
			compact_rendered.contains(SUBJECT),
			"short layout lost the commit subject: {compact_rendered}"
		);
		assert!(
			!compact_rendered.contains(&strings::commit::details_info_title(
				&details.single_details.key_config
			)),
			"short layout should use one clean message panel: {compact_rendered}"
		);

		for (width, height) in [(132, 36), (120, 24)] {
			let backend =
				ratatui::backend::TestBackend::new(width, height);
			let mut terminal =
				ratatui::Terminal::new(backend).unwrap();
			terminal
				.draw(|frame| {
					details
						.draw(frame, Rect::new(0, 0, width, height))
						.unwrap();
				})
				.unwrap();
			let rendered = terminal
				.backend()
				.buffer()
				.content
				.iter()
				.map(|cell| cell.symbol())
				.collect::<String>();
			assert!(
				rendered.contains(&full_hash),
				"full SHA is missing at {width}x{height}: {rendered}"
			);
			assert!(
				rendered.contains(SUBJECT),
				"commit subject is missing at {width}x{height}: {rendered}"
			);
		}

		let backend = ratatui::backend::TestBackend::new(16, 4);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		terminal
			.draw(|frame| {
				details.draw(frame, Rect::new(1, 1, 2, 1)).unwrap();
			})
			.unwrap();
	}
}

#[cfg(test)]
mod test_line_count {
	use super::*;

	#[test]
	fn test_smoke() {
		let commit = CommitDetails {
			message: Some(CommitMessage {
				subject: String::from("subject line"),
				body: Some(String::from("body lone")),
			}),
			..CommitDetails::default()
		};
		let lines = DetailsComponent::get_number_of_lines(
			Some(commit.clone()).as_ref(),
			50,
		);
		assert_eq!(lines, 2);

		let lines = DetailsComponent::get_number_of_lines(
			Some(commit).as_ref(),
			8,
		);
		assert_eq!(lines, 4);
	}
}
