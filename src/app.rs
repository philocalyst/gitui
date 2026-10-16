use crate::{
	accessors,
	args::CliArgs,
	cmdbar::CommandBar,
	components::{
		command_pump, event_pump, CommandInfo, Component,
		DrawableComponent, FuzzyFinderTarget,
	},
	input::{Input, InputEvent, InputState},
	keys::{key_match, KeyConfig, SharedKeyConfig},
	modal::{ModalAction, ModalInput, ModalOutcome},
	options::{Options, SharedOptions},
	popup_stack::PopupStack,
	popups::{
		AppOption, BlameFilePopup, BranchListPopup,
		CheckoutOptionPopup, CommitPopup, CompareCommitsPopup,
		ConfirmPopup, CreateBranchPopup, CreateRemotePopup,
		ExternalEditorPopup, FetchPopup, FileRevlogPopup,
		FuzzyFindPopup, GotoLinePopup, HelpPopup, InspectCommitPopup,
		LogSearchPopupPopup, MsgPopup, OptionsPopup, PullPopup,
		PushPopup, PushTagsPopup, RemoteListPopup, RenameBranchPopup,
		RenameRemotePopup, ResetPopup, RevisionFilesPopup,
		StashMsgPopup, SubmodulesListPopup, TagCommitPopup,
		TagListPopup, UpdateRemoteUrlPopup,
	},
	queue::{
		Action, AppTabs, InternalEvent, NeedsUpdate, Queue,
		StackablePopupOpen,
	},
	rebase_service::RebaseService,
	setup_popups,
	strings::{self, ellipsis_trim_start, order},
	tabs::SharedBackend,
	tabs::{
		Branches, FilesTab, RebaseTab, Revlog, StashList, Stashing,
		Status,
	},
	try_or_popup,
	ui::style::{SharedTheme, Theme},
	AsyncAppNotification, AsyncNotification,
};
use anyhow::{bail, Result};
use asyncgit::{
	sync::{
		self,
		utils::{repo_work_dir, undo_last_commit},
		RepoPath, RepoPathRef,
	},
	AsyncGitNotification, PushType,
};
use crossbeam_channel::Sender;
use crossterm::event::{
	Event, KeyEvent, KeyEventKind, MouseButton, MouseEventKind,
};
use ratatui::{
	layout::{
		Alignment, Constraint, Direction, Layout, Margin, Rect,
	},
	text::{Line, Span},
	widgets::{Block, Borders, Paragraph, Tabs},
	Frame,
};
use std::{
	cell::{Cell, RefCell},
	path::{Path, PathBuf},
	rc::Rc,
};
use unicode_width::UnicodeWidthStr;

#[derive(Clone)]
pub enum QuitState {
	None,
	Close,
	OpenSubmodule(RepoPath),
}

#[cfg(test)]
mod modal_app_tests {
	use super::*;
	use crossterm::event::{
		KeyCode, KeyEventKind, KeyModifiers, MouseEvent,
	};
	use std::sync::{Mutex, OnceLock};

	static APP_RECEIVERS: OnceLock<
		Mutex<(
			Vec<crossbeam_channel::Receiver<AsyncGitNotification>>,
			Vec<crossbeam_channel::Receiver<AsyncAppNotification>>,
		)>,
	> = OnceLock::new();

	fn modal_app() -> (tempfile::TempDir, App) {
		let (dir, _repository) = git2_testing::repo_init();
		let cliargs = CliArgs {
			keymap: crate::keys::Keymap::Helix,
			theme: PathBuf::new(),
			select_file: None,
			repo_path: RepoPath::Path(dir.path().to_path_buf()),
			notify_watcher: false,
			key_bindings_path: None,
			key_symbols_path: None,
		};
		let (sender_git, receiver_git) =
			crossbeam_channel::unbounded();
		let (sender_app, receiver_app) =
			crossbeam_channel::unbounded();
		let app = App::new(
			cliargs,
			sender_git,
			sender_app,
			Input::new(),
			Theme::default(),
			KeyConfig::with_keymap(crate::keys::Keymap::Helix),
		)
		.unwrap();
		// A running UI drains these channels. Keep the receivers alive for
		// the test process so background git workers do not panic on send.
		let mut receivers = APP_RECEIVERS
			.get_or_init(|| Mutex::new((Vec::new(), Vec::new())))
			.lock()
			.unwrap();
		receivers.0.push(receiver_git);
		receivers.1.push(receiver_app);
		(dir, app)
	}

	fn key(code: KeyCode) -> InputEvent {
		InputEvent::Input(Event::Key(KeyEvent::new(
			code,
			KeyModifiers::NONE,
		)))
	}

	fn key_kind(
		code: KeyCode,
		modifiers: KeyModifiers,
		kind: KeyEventKind,
	) -> InputEvent {
		InputEvent::Input(Event::Key(KeyEvent::new_with_kind(
			code, modifiers, kind,
		)))
	}

	#[test]
	fn prefix_and_palette_consume_q_without_quitting() {
		let (_dir, mut app) = modal_app();
		app.event(key(KeyCode::Char('g'))).unwrap();
		app.event(key(KeyCode::Char('q'))).unwrap();
		assert!(matches!(app.do_quit, QuitState::None));

		app.event(key(app.key_config.keys.command_palette.code))
			.unwrap();
		app.event(key(KeyCode::Char('q'))).unwrap();
		assert!(matches!(app.do_quit, QuitState::None));
		assert!(app.modal_input.active());
	}

	#[test]
	fn command_palette_accepts_text_repeats_but_not_repeated_commands(
	) {
		let (_dir, mut app) = modal_app();
		app.event(key(app.key_config.keys.command_palette.code))
			.unwrap();
		app.event(key(KeyCode::Char('a'))).unwrap();
		app.event(key_kind(
			KeyCode::Char('a'),
			KeyModifiers::NONE,
			KeyEventKind::Repeat,
		))
		.unwrap();
		assert_eq!(app.modal_input.pending_hint(), ":aa");

		app.event(key_kind(
			KeyCode::Char('d'),
			KeyModifiers::CONTROL,
			KeyEventKind::Repeat,
		))
		.unwrap();
		assert_eq!(app.modal_input.pending_hint(), ":aa");
		assert!(matches!(app.do_quit, QuitState::None));
	}

	#[test]
	fn branch_search_repeat_types_q_instead_of_quitting() {
		let (_dir, mut app) = modal_app();
		app.set_tab(6).unwrap();
		app.event(key(KeyCode::Char('/'))).unwrap();
		app.event(key(KeyCode::Char('q'))).unwrap();
		app.event(key_kind(
			KeyCode::Char('q'),
			KeyModifiers::NONE,
			KeyEventKind::Repeat,
		))
		.unwrap();
		assert!(matches!(app.do_quit, QuitState::None));

		let mut terminal = ratatui::Terminal::new(
			ratatui::backend::TestBackend::new(100, 30),
		)
		.unwrap();
		terminal.draw(|frame| app.draw(frame).unwrap()).unwrap();
		let rendered = terminal.backend().to_string();
		assert!(rendered.contains("Branches /qq"), "{rendered}");
	}

	#[test]
	fn inline_commit_accepts_character_repeat_and_rejects_repeat_ctrl_d(
	) {
		let (dir, mut app) = modal_app();
		std::fs::write(dir.path().join("repeat.txt"), "staged\n")
			.unwrap();
		let output = std::process::Command::new("git")
			.args(["add", "repeat.txt"])
			.current_dir(dir.path())
			.output()
			.unwrap();
		assert!(output.status.success());
		app.commit_popup.open(None).unwrap();
		app.event(key(KeyCode::Char('a'))).unwrap();
		app.event(key_kind(
			KeyCode::Char('a'),
			KeyModifiers::NONE,
			KeyEventKind::Repeat,
		))
		.unwrap();

		let head = || {
			std::process::Command::new("git")
				.args(["rev-parse", "HEAD"])
				.current_dir(dir.path())
				.output()
				.unwrap()
		};
		let before = head();
		app.event(key_kind(
			KeyCode::Char('d'),
			KeyModifiers::CONTROL,
			KeyEventKind::Repeat,
		))
		.unwrap();
		let after = head();
		assert_eq!(before.stdout, after.stdout);
		assert!(app.commit_popup.is_visible());

		let mut terminal = ratatui::Terminal::new(
			ratatui::backend::TestBackend::new(100, 30),
		)
		.unwrap();
		terminal.draw(|frame| app.draw(frame).unwrap()).unwrap();
		let rendered = terminal.backend().to_string();
		assert!(rendered.contains("aa"), "{rendered}");
	}

	#[test]
	fn goto_prefix_dispatches_tab_number() {
		let (_dir, mut app) = modal_app();
		app.set_tab(1).unwrap();
		app.event(key(KeyCode::Char('g'))).unwrap();
		app.event(key(KeyCode::Char('1'))).unwrap();
		assert_eq!(app.tab, 0);
	}

	#[test]
	fn modal_status_line_renders_in_a_small_terminal() {
		let (_dir, app) = modal_app();
		let backend = ratatui::backend::TestBackend::new(44, 8);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		terminal.draw(|frame| app.draw(frame).unwrap()).unwrap();

		let line: String = (0..44)
			.map(|x| terminal.backend().buffer()[(x, 7)].symbol())
			.collect();
		assert!(line.contains("NORMAL"), "{line:?}");
	}

	#[test]
	fn clicking_a_tab_label_switches_tabs_but_palette_owns_pointer() {
		let (_dir, mut app) = modal_app();
		let backend = ratatui::backend::TestBackend::new(100, 30);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		terminal.draw(|frame| app.draw(frame).unwrap()).unwrap();
		let status_width =
			strings::tab_status(&app.key_config).width();
		let tab_gap =
			strings::tab_divider(&app.key_config).width() + 2;
		let log_column =
			u16::try_from(2 + status_width + tab_gap).unwrap();
		app.event(InputEvent::Input(Event::Mouse(MouseEvent {
			kind: MouseEventKind::Down(MouseButton::Left),
			column: log_column,
			row: 0,
			modifiers: KeyModifiers::NONE,
		})))
		.unwrap();
		assert_eq!(app.tab, 1);

		app.event(key(app.key_config.keys.command_palette.code))
			.unwrap();
		app.event(InputEvent::Input(Event::Mouse(MouseEvent {
			kind: MouseEventKind::Down(MouseButton::Left),
			column: 2,
			row: 0,
			modifiers: KeyModifiers::NONE,
		})))
		.unwrap();
		assert_eq!(app.tab, 1);

		app.event(key(KeyCode::Esc)).unwrap();
		app.options_popup.show().unwrap();
		app.event(InputEvent::Input(Event::Mouse(MouseEvent {
			kind: MouseEventKind::Down(MouseButton::Left),
			column: 2,
			row: 0,
			modifiers: KeyModifiers::NONE,
		})))
		.unwrap();
		assert_eq!(app.tab, 1);
		assert!(app.options_popup.is_visible());
	}

	#[test]
	fn captured_branch_filter_owns_mouse_input_above_tab_bar() {
		let (_dir, mut app) = modal_app();
		app.set_tab(6).unwrap();
		app.event(key(KeyCode::Char('/'))).unwrap();
		assert!(app.branches_tab.captures_input());

		app.event(InputEvent::Input(Event::Mouse(MouseEvent {
			kind: MouseEventKind::Down(MouseButton::Left),
			column: 2,
			row: 0,
			modifiers: KeyModifiers::NONE,
		})))
		.unwrap();
		assert_eq!(app.tab, 6);
	}
}

/// the main app type
pub struct App {
	repo: RepoPathRef,
	do_quit: QuitState,
	help_popup: HelpPopup,
	msg_popup: MsgPopup,
	confirm_popup: ConfirmPopup,
	commit_popup: CommitPopup,
	blame_file_popup: BlameFilePopup,
	file_revlog_popup: FileRevlogPopup,
	stashmsg_popup: StashMsgPopup,
	inspect_commit_popup: InspectCommitPopup,
	compare_commits_popup: CompareCommitsPopup,
	external_editor_popup: ExternalEditorPopup,
	revision_files_popup: RevisionFilesPopup,
	fuzzy_find_popup: FuzzyFindPopup,
	log_search_popup: LogSearchPopupPopup,
	push_popup: PushPopup,
	push_tags_popup: PushTagsPopup,
	pull_popup: PullPopup,
	fetch_popup: FetchPopup,
	tag_commit_popup: TagCommitPopup,
	create_branch_popup: CreateBranchPopup,
	create_remote_popup: CreateRemotePopup,
	rename_remote_popup: RenameRemotePopup,
	update_remote_url_popup: UpdateRemoteUrlPopup,
	remotes_popup: RemoteListPopup,
	rename_branch_popup: RenameBranchPopup,
	select_branch_popup: BranchListPopup,
	options_popup: OptionsPopup,
	submodule_popup: SubmodulesListPopup,
	tags_popup: TagListPopup,
	reset_popup: ResetPopup,
	checkout_option_popup: CheckoutOptionPopup,
	cmdbar: RefCell<CommandBar>,
	tab_hit_areas: RefCell<Vec<(usize, Rect)>>,
	tab: usize,
	revlog: Revlog,
	status_tab: Status,
	stashing_tab: Stashing,
	stashlist_tab: StashList,
	files_tab: FilesTab,
	branches_tab: Branches,
	rebase_service: Rc<RefCell<RebaseService>>,
	rebase_tab: RebaseTab,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	input: Input,
	modal_input: ModalInput,
	mouse_capture_tab: Option<usize>,
	popup_stack: PopupStack,
	options: SharedOptions,
	repo_path_text: String,
	goto_line_popup: GotoLinePopup,
	/// Initial tab requested by the user. Its activation is
	/// deliberately deferred until the shell has been painted.
	initial_tab: Option<usize>,

	// "Flags"
	requires_redraw: Cell<bool>,
	file_to_open: Option<String>,
}

pub struct Environment {
	pub queue: Queue,
	pub theme: SharedTheme,
	pub key_config: SharedKeyConfig,
	pub repo: RepoPathRef,
	pub options: SharedOptions,
	pub sender_git: Sender<AsyncGitNotification>,
	pub sender_app: Sender<AsyncAppNotification>,
}

/// The need to construct a "whatever" environment only arises in testing right now
#[cfg(test)]
impl Environment {
	pub fn test_env() -> Self {
		use crossbeam_channel::{unbounded, Receiver};
		use std::sync::{Mutex, OnceLock};

		static TEST_RECEIVERS: OnceLock<
			Mutex<(
				Vec<Receiver<AsyncGitNotification>>,
				Vec<Receiver<AsyncAppNotification>>,
			)>,
		> = OnceLock::new();

		let (sender_git, receiver_git) = unbounded();
		let (sender_app, receiver_app) = unbounded();
		let mut receivers = TEST_RECEIVERS
			.get_or_init(|| Mutex::new((Vec::new(), Vec::new())))
			.lock()
			.unwrap();
		receivers.0.push(receiver_git);
		receivers.1.push(receiver_app);
		drop(receivers);

		Self {
			queue: Queue::new(),
			theme: Rc::default(),
			key_config: Rc::default(),
			repo: RefCell::new(RepoPath::Path(PathBuf::default())),
			options: Rc::new(RefCell::new(Options::test_env())),
			sender_git,
			sender_app,
		}
	}
}

// public interface
impl App {
	///
	#[allow(clippy::too_many_lines)]
	pub fn new(
		cliargs: CliArgs,
		sender_git: Sender<AsyncGitNotification>,
		sender_app: Sender<AsyncAppNotification>,
		input: Input,
		theme: Theme,
		key_config: KeyConfig,
	) -> Result<Self> {
		let repo = RefCell::new(cliargs.repo_path.clone());
		log::trace!("open repo at: {repo:?}");

		let repo_path_text =
			repo_work_dir(&repo.borrow()).unwrap_or_default();

		let env = Environment {
			queue: Queue::new(),
			theme: Rc::new(theme),
			key_config: Rc::new(key_config),
			options: Options::new(repo.clone()),
			repo,
			sender_git,
			sender_app,
		};

		let mut select_file: Option<PathBuf> = None;
		let tab = if let Some(file) = cliargs.select_file {
			// convert to relative git path
			if let Ok(abs) = file.canonicalize() {
				if let Ok(path) = abs.strip_prefix(
					env.repo.borrow().gitpath().canonicalize()?,
				) {
					select_file = Some(Path::new(".").join(path));
				}
			}
			2
		} else {
			env.options.borrow().current_tab().min(6)
		};

		let rebase_service =
			Rc::new(RefCell::new(RebaseService::new(&env)?));
		let rebase_tab = RebaseTab::new(
			&env,
			Rc::clone(&rebase_service) as SharedBackend,
		);

		let app = Self {
			input,
			modal_input: ModalInput::new(),
			mouse_capture_tab: None,
			confirm_popup: ConfirmPopup::new(&env),
			commit_popup: CommitPopup::new(&env),
			blame_file_popup: BlameFilePopup::new(
				&env,
				&strings::blame_title(&env.key_config),
			),
			file_revlog_popup: FileRevlogPopup::new(&env),
			revision_files_popup: RevisionFilesPopup::new(&env),
			stashmsg_popup: StashMsgPopup::new(&env),
			inspect_commit_popup: InspectCommitPopup::new(&env),
			compare_commits_popup: CompareCommitsPopup::new(&env),
			external_editor_popup: ExternalEditorPopup::new(&env),
			push_popup: PushPopup::new(&env),
			push_tags_popup: PushTagsPopup::new(&env),
			reset_popup: ResetPopup::new(&env),
			pull_popup: PullPopup::new(&env),
			fetch_popup: FetchPopup::new(&env),
			tag_commit_popup: TagCommitPopup::new(&env),
			create_branch_popup: CreateBranchPopup::new(&env),
			create_remote_popup: CreateRemotePopup::new(&env),
			rename_remote_popup: RenameRemotePopup::new(&env),
			update_remote_url_popup: UpdateRemoteUrlPopup::new(&env),
			remotes_popup: RemoteListPopup::new(&env),
			rename_branch_popup: RenameBranchPopup::new(&env),
			select_branch_popup: BranchListPopup::new(&env),
			tags_popup: TagListPopup::new(&env),
			options_popup: OptionsPopup::new(&env),
			submodule_popup: SubmodulesListPopup::new(&env),
			log_search_popup: LogSearchPopupPopup::new(&env),
			fuzzy_find_popup: FuzzyFindPopup::new(&env),
			do_quit: QuitState::None,
			cmdbar: RefCell::new(CommandBar::new(
				env.theme.clone(),
				env.key_config.clone(),
			)),
			tab_hit_areas: RefCell::new(Vec::new()),
			help_popup: HelpPopup::new(&env),
			msg_popup: MsgPopup::new(&env),
			revlog: Revlog::new(&env),
			status_tab: Status::new(&env),
			stashing_tab: Stashing::new(&env),
			stashlist_tab: StashList::new(&env),
			files_tab: FilesTab::new(&env, select_file),
			branches_tab: Branches::new(&env),
			rebase_service: Rc::clone(&rebase_service),
			rebase_tab,
			checkout_option_popup: CheckoutOptionPopup::new(&env),
			goto_line_popup: GotoLinePopup::new(&env),
			tab,
			initial_tab: Some(tab),
			queue: env.queue,
			theme: env.theme,
			options: env.options,
			key_config: env.key_config,
			requires_redraw: Cell::new(false),
			file_to_open: None,
			repo: env.repo,
			repo_path_text,
			popup_stack: PopupStack::default(),
		};

		Ok(app)
	}

	/// Activate the requested startup tab after the initial shell
	/// frame. Showing a tab starts repository reads, so keeping this
	/// separate from construction lets the terminal become responsive
	/// before any of those reads begin.
	pub(crate) fn activate_initial_tab(&mut self) -> Result<()> {
		if let Some(tab) = self.initial_tab.take() {
			self.set_tab(tab)?;
		}
		Ok(())
	}

	#[cfg(test)]
	pub(crate) fn startup_status_scan_pending(&self) -> bool {
		self.status_tab.anything_pending()
	}

	#[cfg(test)]
	pub(crate) const fn has_deferred_initial_tab(&self) -> bool {
		self.initial_tab.is_some()
	}

	///
	pub fn draw(&self, f: &mut Frame) -> Result<()> {
		let fsize = f.area();
		let modal_status_height =
			u16::from(self.key_config.is_modal());

		self.cmdbar.borrow_mut().refresh_width(fsize.width);

		let mut constraints = vec![
			Constraint::Length(2),
			Constraint::Min(2),
			Constraint::Length(self.cmdbar.borrow().height()),
		];
		if modal_status_height > 0 {
			constraints.push(Constraint::Length(modal_status_height));
		}
		let chunks_main = Layout::default()
			.direction(Direction::Vertical)
			.constraints(constraints)
			.split(fsize);

		self.cmdbar.borrow().draw(f, chunks_main[2]);

		self.draw_top_bar(f, chunks_main[0]);

		//TODO: component property + a macro `fullscreen_popup_open!`
		// to make this scale better?
		let fullscreen_popup_open =
			self.revision_files_popup.is_visible()
				|| self.inspect_commit_popup.is_visible()
				|| self.compare_commits_popup.is_visible()
				|| self.blame_file_popup.is_visible()
				|| self.file_revlog_popup.is_visible();

		if !fullscreen_popup_open {
			if self.initial_tab.is_some() {
				self.draw_initial_loading(f, chunks_main[1]);
			} else {
				//TODO: macro because of generic draw call
				match self.tab {
					0 => self.status_tab.draw(f, chunks_main[1])?,
					1 => self.revlog.draw(f, chunks_main[1])?,
					2 => self.files_tab.draw(f, chunks_main[1])?,
					3 => self.stashing_tab.draw(f, chunks_main[1])?,
					4 => {
						self.stashlist_tab.draw(f, chunks_main[1])?;
					}
					5 => self.rebase_tab.draw(f, chunks_main[1])?,
					6 => self.branches_tab.draw(f, chunks_main[1])?,
					_ => bail!("unknown tab"),
				}
			}
		}

		self.draw_popups(f)?;
		if self.key_config.is_modal() {
			self.draw_modal_status(f, chunks_main[3]);
			self.modal_input.draw_overlay(
				f,
				&self.theme,
				&self.key_config,
			);
		}

		Ok(())
	}

	fn draw_initial_loading(&self, f: &mut Frame, area: Rect) {
		let tab = match self.tab {
			0 => strings::tab_status(&self.key_config),
			1 => strings::tab_log(&self.key_config),
			2 => strings::tab_files(&self.key_config),
			3 => strings::tab_stashing(&self.key_config),
			4 => strings::tab_stashes(&self.key_config),
			5 => strings::tab_rebase(&self.key_config),
			_ => strings::tab_branches(&self.key_config),
		};
		let message = format!("Loading {tab}…");
		f.render_widget(
			Paragraph::new(message).alignment(Alignment::Center),
			area,
		);
	}

	fn draw_modal_status(&self, f: &mut Frame, area: Rect) {
		let in_dialog = self.any_popup_visible();
		let captures_input = self.modal_captures_input();
		let palette_open =
			self.modal_input.mode_label() == Some("COMMAND");
		let tab = match self.tab {
			0 => "Status",
			1 => "Log",
			2 => "Files",
			3 => "Stashing",
			4 => "Stashes",
			5 => "Rebase",
			6 => "Branches",
			_ => "",
		};
		let selection = if in_dialog || captures_input {
			None
		} else {
			self.components().into_iter().find_map(
				super::components::Component::selection_status,
			)
		};
		let selected = selection.map_or_else(String::new, |s| {
			if s.count == 0 {
				String::new()
			} else if s.extending {
				format!("  {} {}", s.count, s.unit)
			} else {
				format!("  {} {} selected", s.count, s.unit)
			}
		});
		let contextual_hints = if in_dialog || captures_input {
			None
		} else {
			self.components().into_iter().find_map(
				super::components::Component::modal_status_hint,
			)
		};
		let (label, style) = if in_dialog {
			("DIALOG", self.theme.commandbar(false))
		} else if captures_input {
			("INPUT", self.theme.commandbar(false))
		} else if selection.is_some_and(|status| status.extending) {
			("SELECT", self.theme.commandbar(true))
		} else {
			(
				self.modal_input
					.mode_label()
					.or_else(|| {
						self.modal_input.active().then_some("NORMAL")
					})
					.unwrap_or("NORMAL"),
				self.theme.commandbar(true),
			)
		};
		let hints = if in_dialog {
			"Enter Select  Esc Cancel".to_owned()
		} else if palette_open {
			"↑/↓ Move  Enter Run  Esc Close".to_owned()
		} else if captures_input {
			"Esc Cancel".to_owned()
		} else if let Some(hints) = contextual_hints {
			hints
		} else {
			format!(
				"{} Select  {} Clear  {} All  {} Item  {} Actions  {} Commands  {} Cancel",
				self.key_config.get_hint(self.key_config.keys.select_mode),
				self.key_config.get_hint(self.key_config.keys.select_clear),
				self.key_config.get_hint(self.key_config.keys.select_all),
				self.key_config.get_hint(self.key_config.keys.select_item),
				self.key_config.get_hint(self.key_config.keys.action_prefix),
				self.key_config.get_hint(self.key_config.keys.command_palette),
				self.key_config.get_hint(self.key_config.keys.exit_popup),
			)
		};
		let pending = self.modal_input.pending_hint();
		let pending = if pending.is_empty() {
			String::new()
		} else {
			format!("  {pending}")
		};
		let line = Line::from(vec![
			Span::styled(format!(" {label} "), style),
			Span::styled(
				format!(" {tab}{selected}{pending} "),
				style,
			),
			Span::styled(hints, style),
		]);
		f.render_widget(Paragraph::new(line).style(style), area);
	}

	///
	pub fn event(&mut self, ev: InputEvent) -> Result<()> {
		log::trace!("event: {ev:?}");

		if let InputEvent::Input(ev) = ev {
			self.handle_input_event(&ev)?;
		} else if let InputEvent::State(polling_state) = ev {
			self.external_editor_popup.hide();
			if matches!(polling_state, InputState::Paused) {
				let result = (|| {
					if let Some(path) = self.file_to_open.take() {
						ExternalEditorPopup::open_file_in_editor(
							&self.repo.borrow(),
							Path::new(&path),
						)
					} else {
						let changes =
							self.status_tab.get_files_changes()?;
						self.commit_popup.show_editor(changes)
					}
				})();

				if let Err(e) = result {
					let msg =
						format!("failed to launch editor:\n{e}");
					log::error!("{}", msg.as_str());
					self.input.set_polling(true);
					self.msg_popup.show_error(msg.as_str())?;
				} else {
					self.input.set_polling(true);
				}

				self.requires_redraw.set(true);
			}
		}

		Ok(())
	}

	fn handle_input_event(&mut self, ev: &Event) -> Result<()> {
		if matches!(ev, Event::Key(key) if key.kind == KeyEventKind::Release)
		{
			return Ok(());
		}
		let repeated = matches!(
			ev,
			Event::Key(key) if key.kind == KeyEventKind::Repeat
		);
		let repeated_text_input = repeated
			&& (self.modal_input.captures_text_input()
				|| self.modal_captures_input())
			&& matches!(
				ev,
				Event::Key(key)
					if super::modal::repeat_text_editing_key(key)
			);
		if repeated
			&& !self.repeat_key_is_navigation(ev)
			&& !repeated_text_input
		{
			return Ok(());
		}
		if self.check_hard_exit(ev) {
			return Ok(());
		}
		if matches!(ev, Event::Mouse(_)) {
			return self.handle_mouse_event(ev);
		}

		if !self.key_config.is_modal() {
			return self.process_unrouted_event(ev);
		}

		if self.any_popup_visible() || self.modal_captures_input() {
			self.modal_input.reset();
			return self.process_unrouted_event(ev);
		}

		let outcome = self.modal_input.route(ev, &self.key_config);
		if repeated
			&& !self.repeat_outcome_is_motion(&outcome)
			&& !(repeated_text_input
				&& matches!(&outcome, ModalOutcome::Consumed))
		{
			return Ok(());
		}
		match outcome {
			ModalOutcome::Pass(event) => {
				self.process_unrouted_event(&event)?;
			}
			ModalOutcome::Consumed => {
				self.process_queue(NeedsUpdate::COMMANDS)?;
			}
			ModalOutcome::Action(action) => {
				self.process_modal_action(action)?;
			}
			ModalOutcome::OpenPalette => {
				let commands = self.commands(false);
				self.modal_input
					.open_palette(commands, &self.key_config);
			}
		}

		Ok(())
	}

	fn handle_mouse_event(&mut self, ev: &Event) -> Result<()> {
		let Event::Mouse(mouse) = ev else {
			return Ok(());
		};
		// A palette or text prompt owns the pointer just as it owns the
		// keyboard. In particular, a click must never reach the pane below it.
		if self.modal_input.mode_label() == Some("COMMAND") {
			self.mouse_capture_tab = None;
			let outcome =
				self.modal_input.route(ev, &self.key_config);
			match outcome {
				ModalOutcome::Action(action) => {
					self.process_modal_action(action)?;
				}
				ModalOutcome::OpenPalette => {
					let commands = self.commands(false);
					self.modal_input
						.open_palette(commands, &self.key_config);
				}
				ModalOutcome::Consumed | ModalOutcome::Pass(_) => {}
			}
			self.process_queue(NeedsUpdate::COMMANDS)?;
			return Ok(());
		}

		if self.any_popup_visible() {
			self.mouse_capture_tab = None;
			// Popup order comes from the same list used to paint them. Only
			// the topmost visible surface receives the event, and the overlay
			// stack owns the click even when that surface ignores the position.
			for popup in self.popup_components_mut().into_iter().rev()
			{
				if popup.is_visible() {
					let _ = popup.event(ev)?;
					break;
				}
			}
			self.process_queue(NeedsUpdate::COMMANDS)?;
			return Ok(());
		}

		if self.modal_input.active() {
			self.modal_input.route(ev, &self.key_config);
			self.mouse_capture_tab = None;
			return Ok(());
		}

		if !self.modal_captures_input()
			&& mouse.kind == MouseEventKind::Down(MouseButton::Left)
		{
			if let Some(tab) =
				self.tab_at_position(mouse.column, mouse.row)
			{
				self.set_tab(tab)?;
				self.process_queue(NeedsUpdate::COMMANDS)?;
				return Ok(());
			}
		}

		let captured_tab = match mouse.kind {
			MouseEventKind::Drag(_) | MouseEventKind::Up(_) => {
				self.mouse_capture_tab
			}
			_ => None,
		};
		let route_tab = captured_tab.unwrap_or(self.tab);
		// Only the active tab receives background pointer events. Its child
		// components remember their own rendered rectangles and decide whether
		// the coordinate belongs to them.
		let mut components = match route_tab {
			0 => vec![&mut self.status_tab as &mut dyn Component],
			1 => vec![&mut self.revlog as &mut dyn Component],
			2 => vec![&mut self.files_tab as &mut dyn Component],
			3 => vec![&mut self.stashing_tab as &mut dyn Component],
			4 => vec![&mut self.stashlist_tab as &mut dyn Component],
			5 => vec![&mut self.rebase_tab as &mut dyn Component],
			6 => vec![&mut self.branches_tab as &mut dyn Component],
			_ => bail!("unknown tab"),
		};
		let consumed =
			event_pump(ev, components.as_mut_slice())?.is_consumed();
		if matches!(
			mouse.kind,
			MouseEventKind::Down(MouseButton::Left)
		) && consumed
		{
			self.mouse_capture_tab = Some(self.tab);
		} else if matches!(mouse.kind, MouseEventKind::Up(_)) {
			self.mouse_capture_tab = None;
		}
		self.process_queue(NeedsUpdate::COMMANDS)?;
		Ok(())
	}

	fn tab_at_position(
		&self,
		column: u16,
		row: u16,
	) -> Option<usize> {
		self.tab_hit_areas.borrow().iter().find_map(|(tab, area)| {
			area.contains((column, row).into()).then_some(*tab)
		})
	}

	fn repeat_key_is_navigation(&self, event: &Event) -> bool {
		let Event::Key(key) = event else {
			return false;
		};
		match self.modal_input.mode_label() {
			Some("COMMAND") => {
				matches!(
					key.code,
					crossterm::event::KeyCode::Up
						| crossterm::event::KeyCode::Down
				) || (key.modifiers.contains(
					crossterm::event::KeyModifiers::CONTROL,
				) && matches!(
					key.code,
					crossterm::event::KeyCode::Char('n' | 'p')
				))
			}
			Some("GOTO" | "ACTION") => false,
			_ => [
				self.key_config.keys.move_up,
				self.key_config.keys.move_down,
				self.key_config.keys.move_left,
				self.key_config.keys.move_right,
				self.key_config.keys.home,
				self.key_config.keys.end,
				self.key_config.keys.page_up,
				self.key_config.keys.page_down,
			]
			.iter()
			.any(|binding| key_match(key, *binding)),
		}
	}

	fn repeat_outcome_is_motion(
		&self,
		outcome: &ModalOutcome,
	) -> bool {
		let key = match outcome {
			ModalOutcome::Action(ModalAction::Dispatch(key, _)) => {
				*key
			}
			_ => return false,
		};
		[
			self.key_config.keys.move_up,
			self.key_config.keys.move_down,
			self.key_config.keys.move_left,
			self.key_config.keys.move_right,
			self.key_config.keys.home,
			self.key_config.keys.end,
			self.key_config.keys.popup_up,
			self.key_config.keys.popup_down,
			self.key_config.keys.page_up,
			self.key_config.keys.page_down,
		]
		.contains(&key)
	}

	fn modal_captures_input(&self) -> bool {
		self.components()
			.into_iter()
			.any(super::components::Component::captures_input)
	}

	fn process_modal_dispatch(
		&mut self,
		key: crate::keys::GituiKeyEvent,
		count: usize,
	) -> Result<()> {
		let event = Event::Key(KeyEvent::from(&key));
		for _ in 0..count.clamp(1, crate::modal::MAX_COUNT) {
			if self.any_popup_visible() || self.modal_captures_input()
			{
				self.modal_input.reset();
				break;
			}
			self.process_unrouted_event(&event)?;
		}
		Ok(())
	}

	#[allow(clippy::too_many_lines)] // Each modal outcome has distinct live-state checks.
	fn process_modal_action(
		&mut self,
		action: ModalAction,
	) -> Result<()> {
		match action {
			ModalAction::Dispatch(key, count) => {
				self.process_modal_dispatch(key, count)?;
			}
			ModalAction::PaletteDispatch(key) => {
				let current = self.commands(false);
				let pressed = KeyEvent::from(&key);
				let mut equivalent: Vec<(&str, &str, bool, bool)> =
					Vec::new();
				for command in current.iter().filter(|command| {
					command.text.binding.is_some_and(|binding| {
						key_match(&pressed, binding)
					})
				}) {
					if let Some(existing) = equivalent
						.iter_mut()
						.find(|(desc, group, _, _)| {
							*desc == command.text.desc
								&& *group == command.text.group
						}) {
						existing.2 |= command.available;
						existing.3 |=
							command.available && command.enabled;
					} else {
						equivalent.push((
							command.text.desc,
							command.text.group,
							command.available,
							command.available && command.enabled,
						));
					}
				}
				match equivalent.as_slice() {
					[(_, _, _, true)] => {
						self.process_modal_dispatch(key, 1)?;
					}
					[(_, _, false, _)] => {
						self.modal_action_notice(
							"Command canceled because it is no longer available in this context.",
							false,
						)?;
					}
					[_] => self.modal_action_notice(
						"Command is disabled in the current context.",
						false,
					)?,
					_ if equivalent.is_empty() => self.modal_action_notice(
						"Command canceled because it is no longer available in this context.",
						false,
					)?,
					_ => self.modal_action_notice(
						"Command canceled because its key now matches multiple commands.",
						true,
					)?,
				}
			}
			ModalAction::SwitchTab(tab) => {
				self.set_tab(tab)?;
				self.modal_input.reset();
				self.process_queue(NeedsUpdate::COMMANDS)?;
			}
			ModalAction::Branches => {
				self.queue.push(InternalEvent::SelectBranch);
				self.process_queue(NeedsUpdate::COMMANDS)?;
			}
			ModalAction::Commit => {
				self.queue.push(InternalEvent::OpenCommitEditor);
				self.process_queue(NeedsUpdate::COMMANDS)?;
			}
			ModalAction::Options => {
				self.options_popup.show()?;
				self.modal_input.reset();
				self.process_queue(NeedsUpdate::ALL)?;
			}
			ModalAction::Help => {
				self.help_popup.show()?;
				self.modal_input.reset();
				self.process_queue(NeedsUpdate::COMMANDS)?;
			}
			ModalAction::Search => {
				if self.tab == 1 {
					self.queue
						.push(InternalEvent::OpenLogSearchPopup);
					self.process_queue(NeedsUpdate::COMMANDS)?;
				} else if self.tab == 2 {
					let event = Event::Key(KeyEvent::from(
						&self.key_config.keys.file_find,
					));
					self.process_unrouted_event(&event)?;
				} else if self.tab == 6 {
					let event = Event::Key(KeyEvent::from(
						&self.key_config.keys.branch_find,
					));
					self.process_unrouted_event(&event)?;
				} else {
					self.set_tab(1)?;
					self.queue
						.push(InternalEvent::OpenLogSearchPopup);
					self.process_queue(NeedsUpdate::COMMANDS)?;
				}
				self.modal_input.reset();
			}
			ModalAction::Rebase => {
				self.queue.push(InternalEvent::OpenRebase);
				self.process_queue(NeedsUpdate::COMMANDS)?;
				self.modal_input.reset();
			}
		}
		Ok(())
	}

	fn modal_action_notice(
		&mut self,
		message: &str,
		is_error: bool,
	) -> Result<()> {
		self.modal_input.reset();
		self.queue.push(if is_error {
			InternalEvent::ShowErrorMsg(message.to_owned())
		} else {
			InternalEvent::ShowInfoMsg(message.to_owned())
		});
		self.process_queue(NeedsUpdate::COMMANDS)
	}

	fn process_unrouted_event(&mut self, ev: &Event) -> Result<()> {
		if self.check_quit(ev) {
			return Ok(());
		}

		let old_tab = self.tab;
		let mut flags = NeedsUpdate::empty();

		if event_pump(ev, self.components_mut().as_mut_slice())?
			.is_consumed()
		{
			flags.insert(NeedsUpdate::COMMANDS);
		} else if let Event::Key(k) = ev {
			let new_flags =
				if key_match(k, self.key_config.keys.tab_toggle) {
					self.toggle_tabs(false)?;
					NeedsUpdate::COMMANDS
				} else if key_match(
					k,
					self.key_config.keys.tab_toggle_reverse,
				) {
					self.toggle_tabs(true)?;
					NeedsUpdate::COMMANDS
				} else if key_match(
					k,
					self.key_config.keys.tab_status,
				) || key_match(
					k,
					self.key_config.keys.tab_log,
				) || key_match(
					k,
					self.key_config.keys.tab_files,
				) || key_match(
					k,
					self.key_config.keys.tab_stashing,
				) || key_match(
					k,
					self.key_config.keys.tab_stashes,
				) || key_match(
					k,
					self.key_config.keys.tab_rebase,
				) || key_match(
					k,
					self.key_config.keys.tab_branches,
				) {
					self.switch_tab(k)?;
					NeedsUpdate::COMMANDS
				} else if key_match(
					k,
					self.key_config.keys.cmd_bar_toggle,
				) {
					self.cmdbar.borrow_mut().toggle_more();
					NeedsUpdate::empty()
				} else if key_match(
					k,
					self.key_config.keys.open_options,
				) {
					self.options_popup.show()?;
					NeedsUpdate::ALL
				} else {
					NeedsUpdate::empty()
				};

			flags.insert(new_flags);
		}

		self.process_queue(flags)?;
		if old_tab != self.tab
			|| self.any_popup_visible()
			|| self.modal_captures_input()
		{
			self.modal_input.reset();
		}
		Ok(())
	}

	//TODO: do we need this?
	/// forward ticking to components that require it
	pub fn update(&mut self) -> Result<()> {
		log::trace!("update");
		self.activate_initial_tab()?;

		self.commit_popup.update();
		self.status_tab.update()?;
		self.revlog.update()?;
		self.files_tab.update()?;
		self.stashing_tab.update()?;
		self.stashlist_tab.update()?;
		self.branches_tab.update()?;
		self.reset_popup.update()?;

		self.rebase_service.borrow_mut().refresh()?;
		self.push_rebase_errors();
		// The workspace re-derives its projection from the
		// service, so worker outcomes (and fresh
		// observations) reach the visible tab without a
		// manual tab switch.
		self.rebase_tab.reload();

		self.update_commands();

		Ok(())
	}

	/// Surface asynchronously produced rebase errors exactly
	/// once, as ordinary message popups.
	fn push_rebase_errors(&self) {
		for error in self.rebase_service.borrow_mut().take_errors() {
			self.queue.push(InternalEvent::ShowErrorMsg(format!(
				"rebase:\n{error}"
			)));
		}
	}

	///
	pub fn update_async(
		&mut self,
		ev: AsyncNotification,
	) -> Result<()> {
		log::trace!("update_async: {ev:?}");
		if ev
			== AsyncNotification::App(
				AsyncAppNotification::DiffHighlighting,
			) {
			self.status_tab.update_rendering();
			self.revlog.update_rendering();
			self.file_revlog_popup.update_rendering();
			self.inspect_commit_popup.update_rendering();
			self.compare_commits_popup.update_rendering();
		}
		if let AsyncNotification::Git(ev) = ev {
			if matches!(ev, AsyncGitNotification::Rebase) {
				self.rebase_service.borrow_mut().on_worker_event()?;
				self.push_rebase_errors();
			}
			self.status_tab.update_git(ev)?;
			self.stashing_tab.update_git(ev)?;
			self.revlog.update_git(ev)?;
			self.file_revlog_popup.update_git(ev)?;
			self.inspect_commit_popup.update_git(ev)?;
			self.compare_commits_popup.update_git(ev)?;
			self.push_popup.update_git(ev)?;
			self.push_tags_popup.update_git(ev)?;
			self.pull_popup.update_git(ev);
			self.fetch_popup.update_git(ev);
			self.select_branch_popup.update_git(ev)?;
			self.branches_tab.update_git(ev)?;
		}

		self.files_tab.update_async(ev)?;
		self.blame_file_popup.update_async(ev)?;
		self.revision_files_popup.update(ev)?;
		self.tags_popup.update(ev);

		//TODO: better system for this
		// can we simply process the queue here and everyone just uses the queue to schedule a cmd update?
		self.process_queue(NeedsUpdate::COMMANDS)?;

		Ok(())
	}

	///
	pub fn is_quit(&self) -> bool {
		!matches!(self.do_quit, QuitState::None)
			|| self.input.is_aborted()
	}

	/// Mark repository status as stale after a filesystem or Git mutation.
	pub fn invalidate_repository(&mut self) {
		self.status_tab.invalidate_status();
		self.stashing_tab.invalidate_status();
		self.branches_tab.invalidate_branches();
	}

	/// Start a status refresh when the active request has settled.
	pub fn poll_repository(&mut self) {
		self.status_tab.poll_status();
		self.stashing_tab.poll_status();
		self.branches_tab.poll_ref_freshness();
	}

	///
	pub fn quit_state(&self) -> QuitState {
		self.do_quit.clone()
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.status_tab.anything_pending()
			|| self.revlog.any_work_pending()
			|| self.stashing_tab.anything_pending()
			|| self.files_tab.anything_pending()
			|| self.branches_tab.anything_pending()
			|| self.blame_file_popup.any_work_pending()
			|| self.file_revlog_popup.any_work_pending()
			|| self.inspect_commit_popup.any_work_pending()
			|| self.compare_commits_popup.any_work_pending()
			|| self.input.is_state_changing()
			|| self.push_popup.any_work_pending()
			|| self.push_tags_popup.any_work_pending()
			|| self.pull_popup.any_work_pending()
			|| self.fetch_popup.any_work_pending()
			|| self.revision_files_popup.any_work_pending()
			|| self.tags_popup.any_work_pending()
			|| {
				let service = self.rebase_service.borrow();
				service.is_busy() || service.is_loading()
			}
	}

	///
	pub fn requires_redraw(&self) -> bool {
		if self.requires_redraw.get() {
			self.requires_redraw.set(false);
			true
		} else {
			false
		}
	}
}

// private impls
impl App {
	accessors!(
		self,
		[
			log_search_popup,
			fuzzy_find_popup,
			msg_popup,
			confirm_popup,
			commit_popup,
			goto_line_popup,
			blame_file_popup,
			file_revlog_popup,
			stashmsg_popup,
			inspect_commit_popup,
			compare_commits_popup,
			external_editor_popup,
			push_popup,
			push_tags_popup,
			pull_popup,
			fetch_popup,
			tag_commit_popup,
			reset_popup,
			checkout_option_popup,
			create_branch_popup,
			create_remote_popup,
			rename_remote_popup,
			update_remote_url_popup,
			remotes_popup,
			rename_branch_popup,
			select_branch_popup,
			revision_files_popup,
			submodule_popup,
			tags_popup,
			options_popup,
			help_popup,
			revlog,
			status_tab,
			files_tab,
			stashing_tab,
			stashlist_tab,
			rebase_tab,
			branches_tab
		]
	);

	setup_popups!(
		self,
		[
			commit_popup,
			stashmsg_popup,
			help_popup,
			inspect_commit_popup,
			compare_commits_popup,
			blame_file_popup,
			file_revlog_popup,
			external_editor_popup,
			tag_commit_popup,
			select_branch_popup,
			remotes_popup,
			create_remote_popup,
			rename_remote_popup,
			update_remote_url_popup,
			submodule_popup,
			tags_popup,
			reset_popup,
			checkout_option_popup,
			create_branch_popup,
			rename_branch_popup,
			revision_files_popup,
			fuzzy_find_popup,
			log_search_popup,
			push_popup,
			push_tags_popup,
			pull_popup,
			fetch_popup,
			options_popup,
			confirm_popup,
			msg_popup,
			goto_line_popup
		]
	);

	fn check_quit(&mut self, ev: &Event) -> bool {
		if self.any_popup_visible() || self.modal_captures_input() {
			return false;
		}
		if let Event::Key(e) = ev {
			if key_match(e, self.key_config.keys.quit) {
				self.do_quit = QuitState::Close;
				return true;
			}
		}
		false
	}

	fn check_hard_exit(&mut self, ev: &Event) -> bool {
		if let Event::Key(e) = ev {
			if key_match(e, self.key_config.keys.exit) {
				self.do_quit = QuitState::Close;
				return true;
			}
		}
		false
	}

	fn get_tabs(&mut self) -> Vec<&mut dyn Component> {
		vec![
			&mut self.status_tab,
			&mut self.revlog,
			&mut self.files_tab,
			&mut self.stashing_tab,
			&mut self.stashlist_tab,
			&mut self.rebase_tab,
			&mut self.branches_tab,
		]
	}

	fn toggle_tabs(&mut self, reverse: bool) -> Result<()> {
		let tabs_len = self.get_tabs().len();
		let new_tab = if reverse {
			self.tab.wrapping_sub(1).min(tabs_len.saturating_sub(1))
		} else {
			self.tab.saturating_add(1) % tabs_len
		};

		self.set_tab(new_tab)
	}

	fn switch_tab(&mut self, k: &KeyEvent) -> Result<()> {
		if key_match(k, self.key_config.keys.tab_status) {
			self.switch_to_tab(&AppTabs::Status)?;
		} else if key_match(k, self.key_config.keys.tab_log) {
			self.switch_to_tab(&AppTabs::Log)?;
		} else if key_match(k, self.key_config.keys.tab_files) {
			self.switch_to_tab(&AppTabs::Files)?;
		} else if key_match(k, self.key_config.keys.tab_stashing) {
			self.switch_to_tab(&AppTabs::Stashing)?;
		} else if key_match(k, self.key_config.keys.tab_stashes) {
			self.switch_to_tab(&AppTabs::Stashlist)?;
		} else if key_match(k, self.key_config.keys.tab_rebase) {
			self.switch_to_tab(&AppTabs::Rebase)?;
		} else if key_match(k, self.key_config.keys.tab_branches) {
			self.switch_to_tab(&AppTabs::Branches)?;
		}

		Ok(())
	}

	fn set_tab(&mut self, tab: usize) -> Result<()> {
		self.initial_tab = None;
		let tabs = self.get_tabs();
		for (i, t) in tabs.into_iter().enumerate() {
			if tab == i {
				t.show()?;
			} else {
				t.hide();
			}
		}

		self.tab = tab;
		self.mouse_capture_tab = None;
		self.options.borrow_mut().set_current_tab(tab);

		Ok(())
	}

	fn switch_to_tab(&mut self, tab: &AppTabs) -> Result<()> {
		match tab {
			AppTabs::Status => self.set_tab(0)?,
			AppTabs::Log => self.set_tab(1)?,
			AppTabs::Files => self.set_tab(2)?,
			AppTabs::Stashing => self.set_tab(3)?,
			AppTabs::Stashlist => self.set_tab(4)?,
			AppTabs::Rebase => self.set_tab(5)?,
			AppTabs::Branches => self.set_tab(6)?,
		}
		Ok(())
	}

	fn update_commands(&mut self) {
		if self.help_popup.is_visible() {
			self.help_popup.set_cmds(self.commands(true));
		}
		self.cmdbar.borrow_mut().set_cmds(self.commands(false));
	}

	fn process_queue(&mut self, flags: NeedsUpdate) -> Result<()> {
		let mut flags = flags;
		let new_flags = self.process_internal_events()?;
		flags.insert(new_flags);

		if flags.contains(NeedsUpdate::ALL) {
			self.update()?;
		}
		//TODO: make this a queue event?
		//NOTE: set when any tree component changed selection
		if flags.contains(NeedsUpdate::DIFF) {
			self.status_tab.update_diff()?;
			self.inspect_commit_popup.update_diff()?;
			self.compare_commits_popup.update_diff()?;
			self.file_revlog_popup.update_diff()?;
		}
		if flags.contains(NeedsUpdate::COMMANDS) {
			self.update_commands();
		}
		if flags.contains(NeedsUpdate::BRANCHES) {
			self.select_branch_popup.update_branches()?;
		}
		if flags.contains(NeedsUpdate::REMOTES) {
			self.remotes_popup.update_remotes()?;
		}

		Ok(())
	}

	fn open_popup(
		&mut self,
		popup: StackablePopupOpen,
	) -> Result<()> {
		match popup {
			StackablePopupOpen::BlameFile(params) => {
				self.blame_file_popup.open(params)?;
			}
			StackablePopupOpen::FileRevlog(param) => {
				self.file_revlog_popup.open(param)?;
			}
			StackablePopupOpen::FileTree(param) => {
				self.revision_files_popup.open(param)?;
			}
			StackablePopupOpen::InspectCommit(param) => {
				self.inspect_commit_popup.open(param)?;
			}
			StackablePopupOpen::CompareCommits(param) => {
				self.compare_commits_popup.open(param)?;
			}
		}

		Ok(())
	}

	fn process_internal_events(&mut self) -> Result<NeedsUpdate> {
		let mut flags = NeedsUpdate::empty();

		loop {
			let front = self.queue.pop();
			if let Some(e) = front {
				flags.insert(self.process_internal_event(e)?);
			} else {
				break;
			}
		}
		self.queue.clear();

		Ok(flags)
	}

	#[allow(clippy::too_many_lines)]
	fn process_internal_event(
		&mut self,
		ev: InternalEvent,
	) -> Result<NeedsUpdate> {
		let mut flags = NeedsUpdate::empty();
		match ev {
			InternalEvent::ConfirmedAction(action) => {
				self.process_confirmed_action(action, &mut flags)?;
			}
			InternalEvent::ConfirmAction(action) => {
				self.confirm_popup.open(action)?;
				flags.insert(NeedsUpdate::COMMANDS);
			}
			InternalEvent::ShowErrorMsg(msg) => {
				self.msg_popup.show_error(msg.as_str())?;
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::ShowInfoMsg(msg) => {
				self.msg_popup.show_info(msg.as_str())?;
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::Update(u) => {
				if u.contains(NeedsUpdate::ALL) {
					self.invalidate_repository();
					flags.insert(NeedsUpdate::BRANCHES);
				}
				flags.insert(u);
			}
			InternalEvent::OpenCommit
			| InternalEvent::OpenCommitInline => {
				self.commit_popup.show()?;
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::OpenCommitEditor => {
				self.commit_popup.open(None)?;
				self.queue
					.push(InternalEvent::OpenExternalEditor(None));
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::RewordCommit(id) => {
				self.commit_popup.open(Some(id))?;
			}
			InternalEvent::PopupStashing(opts) => {
				self.stashmsg_popup.options(opts);
				self.stashmsg_popup.show()?;
			}
			InternalEvent::TagCommit(id) => {
				self.tag_commit_popup.open(id)?;
			}
			InternalEvent::CreateRemote => {
				self.create_remote_popup.open()?;
			}
			InternalEvent::RenameRemote(cur_name) => {
				self.rename_remote_popup.open(cur_name)?;
			}
			InternalEvent::UpdateRemoteUrl(remote_name, cur_url) => {
				self.update_remote_url_popup
					.open(remote_name, cur_url)?;
			}
			InternalEvent::ViewRemotes => {
				self.remotes_popup.open()?;
			}
			InternalEvent::CreateBranch => {
				self.create_branch_popup.open()?;
			}
			InternalEvent::RenameBranch(branch_ref, cur_name) => {
				self.rename_branch_popup
					.open(branch_ref, cur_name)?;
			}
			InternalEvent::SelectBranch => {
				self.select_branch_popup.open()?;
			}
			InternalEvent::ViewSubmodules => {
				self.submodule_popup.open()?;
			}
			InternalEvent::Tags => {
				self.tags_popup.open()?;
			}
			InternalEvent::TabSwitchStatus => self.set_tab(0)?,
			InternalEvent::TabSwitch(tab) => {
				self.switch_to_tab(&tab)?;
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::SelectCommitInRevlog(id) => {
				if let Err(error) = self.revlog.select_commit(id) {
					self.queue.push(InternalEvent::ShowErrorMsg(
						error.to_string(),
					));
				} else {
					self.tags_popup.hide();
					flags.insert(NeedsUpdate::ALL);
				}
			}
			InternalEvent::OpenExternalEditor(path) => {
				self.input.set_polling(false);
				self.external_editor_popup.show()?;
				self.file_to_open = path;
				flags.insert(NeedsUpdate::COMMANDS);
			}
			InternalEvent::Push(branch, push_type, force, delete) => {
				self.push_popup
					.push(branch, push_type, force, delete)?;
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::Pull(branch) => {
				if let Err(error) = self.pull_popup.fetch(branch) {
					self.queue.push(InternalEvent::ShowErrorMsg(
						error.to_string(),
					));
				}
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::FetchRemotes => {
				if let Err(error) = self.fetch_popup.fetch() {
					self.queue.push(InternalEvent::ShowErrorMsg(
						error.to_string(),
					));
				}
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::PushTags => {
				self.push_tags_popup.push_tags()?;
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::StatusLastFileMoved => {
				self.status_tab.last_file_moved()?;
			}
			InternalEvent::OpenFuzzyFinder(contents, target) => {
				self.fuzzy_find_popup.open(contents, target)?;
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::OpenLogSearchPopup => {
				self.log_search_popup.open()?;
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::OptionSwitched(o) => {
				match o {
					AppOption::StatusShowUntracked => {
						self.status_tab.update()?;
					}
					AppOption::DiffContextLines
					| AppOption::DiffIgnoreWhitespaces
					| AppOption::DiffInterhunkLines
					| AppOption::DiffPreviewRenderer => {
						self.status_tab.update_diff()?;
					}
				}

				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::FuzzyFinderChanged(
				idx,
				content,
				target,
			) => {
				match target {
					FuzzyFinderTarget::Branches => self
						.select_branch_popup
						.branch_finder_update(idx)?,
					FuzzyFinderTarget::Files => {
						self.files_tab.file_finder_update(
							&PathBuf::from(content.clone()),
						);
						self.revision_files_popup.file_finder_update(
							&PathBuf::from(content),
						);
					}
				}

				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::OpenPopup(popup) => {
				self.open_popup(popup)?;
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::PopupStackPop => {
				if let Some(popup) = self.popup_stack.pop() {
					self.open_popup(popup)?;
					flags.insert(
						NeedsUpdate::ALL | NeedsUpdate::COMMANDS,
					);
				}
			}
			InternalEvent::PopupStackPush(popup) => {
				self.popup_stack.push(popup);
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::OpenRepo { path } => {
				let submodule_repo_path = RepoPath::Path(
					Path::new(&repo_work_dir(&self.repo.borrow())?)
						.join(path),
				);
				//TODO: validate this is a valid repo first, so we can show proper error otherwise
				self.do_quit =
					QuitState::OpenSubmodule(submodule_repo_path);
			}
			InternalEvent::OpenResetPopup(id) => {
				self.reset_popup.open(id)?;
			}
			InternalEvent::CommitSearch(options) => {
				self.revlog.search(options);
			}
			InternalEvent::OpenGotoLinePopup(max_line) => {
				self.goto_line_popup.open(max_line);
			}
			InternalEvent::GotoLine(line) => {
				if self.blame_file_popup.is_visible() {
					self.blame_file_popup.goto_line(line);
				}
			}
			InternalEvent::CheckoutOption(branch) => {
				self.checkout_option_popup.open(branch)?;
			}
			InternalEvent::RebaseRewriteFrom(commit) => {
				let parent =
					sync::commit_parent(&self.repo.borrow(), commit);
				if let Some(parent) = parent {
					self.rebase_service.borrow_mut().create_draft(
						asyncgit::rebase::engine::Base::Upstream(
							parent,
						),
						false,
					);
					self.switch_to_tab(&AppTabs::Rebase)?;
					flags.insert(NeedsUpdate::ALL);
				} else {
					self.queue.push(InternalEvent::ShowErrorMsg(
						"cannot rewrite history from the root commit this way".to_string(),
					));
				}
			}
			InternalEvent::RebaseOnto(branch) => {
				self.rebase_service.borrow_mut().create_draft(
					asyncgit::rebase::engine::Base::Upstream(
						branch.top_commit,
					),
					false,
				);
				self.switch_to_tab(&AppTabs::Rebase)?;
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::OpenRebase => {
				{
					let mut service =
						self.rebase_service.borrow_mut();
					if !service.exists() {
						service.adopt_existing();
					}
				}
				self.switch_to_tab(&AppTabs::Rebase)?;
				flags.insert(NeedsUpdate::ALL);
			}
		}

		Ok(flags)
	}

	fn process_confirmed_action(
		&mut self,
		action: Action,
		flags: &mut NeedsUpdate,
	) -> Result<()> {
		match action {
			Action::Reset(r) => {
				self.status_tab.reset(&r);
			}
			Action::StashDrop(_) | Action::StashPop(_) => {
				if let Err(e) = self
					.stashlist_tab
					.action_confirmed(&self.repo.borrow(), &action)
				{
					self.queue.push(InternalEvent::ShowErrorMsg(
						e.to_string(),
					));
				}
			}
			Action::ResetHunk(path, hash, diff) => {
				sync::reset_hunk_checked(
					&self.repo.borrow(),
					&path,
					hash,
					Some(self.options.borrow().diff_options()),
					&diff,
				)?;
			}
			Action::ResetLines(path, lines, diff) => {
				sync::discard_lines_checked(
					&self.repo.borrow(),
					&path,
					&lines,
					&diff,
				)?;
			}
			Action::ResetLinesWithPairs(path, lines, pairs, diff) => {
				sync::discard_lines_with_pairs_checked(
					&self.repo.borrow(),
					&path,
					&lines,
					&pairs,
					&diff,
				)?;
			}
			Action::DeleteLocalBranch(branch_ref) => {
				if let Err(e) = sync::delete_branch(
					&self.repo.borrow(),
					&branch_ref,
				) {
					self.queue.push(InternalEvent::ShowErrorMsg(
						e.to_string(),
					));
				}

				self.select_branch_popup.update_branches()?;
			}
			Action::DeleteRemoteBranch(branch_ref) => {
				self.delete_remote_branch(&branch_ref)?;
			}
			Action::DeleteRemote(remote_name) => {
				self.delete_remote(&remote_name);
			}
			Action::DeleteTag(tag_name) => {
				self.delete_tag(tag_name)?;
			}
			Action::DeleteRemoteTag(tag_name, _remote) => {
				self.queue.push(InternalEvent::Push(
					tag_name,
					PushType::Tag,
					false,
					true,
				));
			}
			Action::ForcePush(branch, force) => {
				self.queue.push(InternalEvent::Push(
					branch,
					PushType::Branch,
					force,
					false,
				));
			}
			Action::PullMerge { rebase, .. } => {
				self.pull_popup.try_conflict_free_merge(rebase);
			}
			Action::AbortRevert | Action::AbortMerge => {
				self.status_tab.revert_pending_state();
			}
			Action::AbortRebase => {
				// Rebases abort through the workspace now; keep
				// the confirmed action for older flows only.
				self.rebase_service.borrow_mut().abort();
			}
			Action::UndoCommit => {
				try_or_popup!(
					self,
					"undo commit failed:",
					undo_last_commit(&self.repo.borrow())
				);
			}
		}

		flags.insert(NeedsUpdate::ALL);
		self.invalidate_repository();

		Ok(())
	}

	fn delete_tag(&mut self, tag_name: String) -> Result<()> {
		if let Err(error) =
			sync::delete_tag(&self.repo.borrow(), &tag_name)
		{
			self.queue
				.push(InternalEvent::ShowErrorMsg(error.to_string()));
		} else {
			let remote =
				sync::get_default_remote(&self.repo.borrow())?;

			self.queue.push(InternalEvent::ConfirmAction(
				Action::DeleteRemoteTag(tag_name, remote),
			));

			self.tags_popup.update_tags()?;
		}
		Ok(())
	}

	fn delete_remote_branch(
		&mut self,
		branch_ref: &str,
	) -> Result<()> {
		self.queue.push(
			//TODO: check if this is correct based on the fix in `c6abbaf`
			branch_ref.rsplit('/').next().map_or_else(
				|| {
					InternalEvent::ShowErrorMsg(format!(
						    "Failed to find the branch name in {branch_ref}"
					    ))
				},
				|name| {
					InternalEvent::Push(
						name.to_string(),
						PushType::Branch,
						false,
						true,
					)
				},
			),
		);

		self.select_branch_popup.update_branches()?;

		Ok(())
	}

	fn delete_remote(&self, remote_name: &str) {
		let res =
			sync::delete_remote(&self.repo.borrow(), remote_name);
		match res {
			Ok(()) => {
				self.queue.push(InternalEvent::Update(
					NeedsUpdate::ALL | NeedsUpdate::REMOTES,
				));
			}
			Err(e) => {
				log::error!("delete remote: {e:?}");
				self.queue.push(InternalEvent::ShowErrorMsg(
					format!("delete remote error:\n{e}"),
				));
			}
		}
	}

	#[allow(clippy::too_many_lines)] // The command list intentionally stays in display order.
	fn commands(&self, force_all: bool) -> Vec<CommandInfo> {
		let mut res = Vec::new();

		command_pump(&mut res, force_all, &self.components());

		res.push(CommandInfo::new(
			strings::commands::find_file(&self.key_config),
			!self.fuzzy_find_popup.is_visible(),
			(!self.any_popup_visible()
				&& self.files_tab.is_visible())
				|| self.revision_files_popup.is_visible()
				|| force_all,
		));

		res.push(
			CommandInfo::new(
				strings::commands::toggle_tabs(&self.key_config),
				true,
				!self.any_popup_visible(),
			)
			.order(order::NAV),
		);
		res.push(
			CommandInfo::new(
				strings::commands::toggle_tabs_direct(
					&self.key_config,
				),
				true,
				!self.any_popup_visible(),
			)
			.order(order::NAV),
		);
		res.push(
			CommandInfo::new(
				strings::commands::options_popup(&self.key_config),
				true,
				!self.any_popup_visible(),
			)
			.order(order::NAV),
		);

		res.push(
			CommandInfo::new(
				strings::commands::quit(&self.key_config),
				true,
				!self.any_popup_visible(),
			)
			.order(100),
		);

		if self.key_config.is_modal() {
			let modal_available = !self.any_popup_visible();
			let selection_available =
				modal_available && matches!(self.tab, 0 | 1);
			for (index, (text, available)) in [
				(
					strings::commands::selection_mode(
						&self.key_config,
					),
					selection_available,
				),
				(
					strings::commands::selection_clear(
						&self.key_config,
					),
					selection_available,
				),
				(
					strings::commands::selection_all(
						&self.key_config,
					),
					selection_available,
				),
				(
					strings::commands::selection_item(
						&self.key_config,
					),
					selection_available,
				),
				(
					strings::commands::focus_next(&self.key_config),
					modal_available,
				),
				(
					strings::commands::focus_previous(
						&self.key_config,
					),
					modal_available,
				),
				(
					strings::commands::command_palette(
						&self.key_config,
					),
					modal_available,
				),
				(
					strings::commands::goto_prefix(&self.key_config),
					modal_available,
				),
				(
					strings::commands::action_prefix(
						&self.key_config,
					),
					modal_available,
				),
			]
			.into_iter()
			.enumerate()
			{
				let command = CommandInfo::new(text, true, available);
				res.push(if index == 0 {
					command
				} else {
					command.hidden()
				});
			}
		}

		res
	}

	//TODO: make this dynamic
	fn draw_top_bar(&self, f: &mut Frame, r: Rect) {
		const DIVIDER_PAD_SPACES: usize = 2;
		const SIDE_PADS: usize = 2;
		const MARGIN_LEFT_AND_RIGHT: usize = 2;

		let r = r.inner(Margin {
			vertical: 0,
			horizontal: 1,
		});

		let mut tab_entries = vec![
			(0, Span::raw(strings::tab_status(&self.key_config))),
			(1, Span::raw(strings::tab_log(&self.key_config))),
			(2, Span::raw(strings::tab_files(&self.key_config))),
			(3, Span::raw(strings::tab_stashing(&self.key_config))),
			(4, Span::raw(strings::tab_stashes(&self.key_config))),
		];
		// The rebase workspace appears once there is anything
		// to show: a draft, an active session, or an outcome.
		if self.key_config.is_modal()
			|| self.rebase_service.borrow().exists()
		{
			tab_entries.push((
				5,
				Span::raw(strings::tab_rebase(&self.key_config)),
			));
		}
		tab_entries.push((
			6,
			Span::raw(strings::tab_branches(&self.key_config)),
		));
		let divider = strings::tab_divider(&self.key_config);

		// heuristic, since tui doesn't provide a way to know
		// how much space is needed to draw a `Tabs`
		let tabs_len: usize = tab_entries
			.iter()
			.map(|(_, span)| span.width())
			.sum::<usize>()
			+ tab_entries.len().saturating_sub(1)
				* (divider.width() + DIVIDER_PAD_SPACES)
			+ SIDE_PADS
			+ MARGIN_LEFT_AND_RIGHT;

		let left_right = Layout::default()
			.direction(Direction::Horizontal)
			.constraints(vec![
				Constraint::Length(
					u16::try_from(tabs_len).unwrap_or(r.width),
				),
				Constraint::Min(0),
			])
			.split(r);

		let table_area = r; // use entire area to allow drawing the horizontal separator line
		let text_area = left_right[1];
		*self.tab_hit_areas.borrow_mut() = calculate_tab_hit_areas(
			table_area,
			&tab_entries,
			divider.width(),
		);

		let selected_tab = tab_entries
			.iter()
			.position(|(tab, _)| *tab == self.tab)
			.unwrap_or_default();
		let tabs: Vec<Line> = tab_entries
			.into_iter()
			.map(|(_, label)| Line::from(label))
			.collect();

		f.render_widget(
			Tabs::new(tabs)
				.block(
					Block::default()
						.borders(Borders::BOTTOM)
						.border_style(self.theme.block(false)),
				)
				.style(self.theme.tab(false))
				.highlight_style(self.theme.tab(true))
				.divider(divider)
				.select(selected_tab),
			table_area,
		);

		f.render_widget(
			Paragraph::new(Line::from(vec![Span::styled(
				ellipsis_trim_start(
					&self.repo_path_text,
					text_area.width as usize,
				),
				self.theme.title(false),
			)]))
			.alignment(Alignment::Right),
			text_area,
		);
	}
}

fn calculate_tab_hit_areas(
	area: Rect,
	tabs: &[(usize, Span<'_>)],
	divider_width: usize,
) -> Vec<(usize, Rect)> {
	let mut hit_areas = Vec::new();
	let mut x = area.x;
	for (index, (tab, label)) in tabs.iter().enumerate() {
		if x >= area.right() {
			break;
		}
		x = x.saturating_add(1);
		let width = u16::try_from(label.width())
			.unwrap_or(u16::MAX)
			.min(area.right().saturating_sub(x));
		if width > 0 {
			hit_areas.push((
				*tab,
				Rect {
					x,
					y: area.y,
					width,
					height: 1.min(area.height),
				},
			));
		}
		x = x.saturating_add(width);
		if index + 1 < tabs.len() {
			x = x.saturating_add(1).saturating_add(
				u16::try_from(divider_width).unwrap_or(u16::MAX),
			);
		}
	}
	hit_areas
}
