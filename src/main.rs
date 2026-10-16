//!
//! The gitui program is a text-based UI for working with a Git repository.
//! The main navigation occurs between a number of tabs.
//! When you execute commands, the program may use popups to communicate
//! with the user. It is possible to customize the keybindings.
//!
//!
//! ## Internal Modules
//! The top-level modules of gitui can be grouped as follows:
//!
//! - User Interface
//!   - [tabs] for main navigation
//!   - [components] for visual elements used on tabs
//!   - [popups] for temporary dialogs
//!   - [ui] for tooling like scrollbars
//! - Git Interface
//!   - [asyncgit] (crate) for async operations on repository
//! - Distribution and Documentation
//!   - Project files
//!   - Github CI
//!   - Installation files
//!   - Usage guides
//!
//! ## Included Crates
//! Some crates are part of the gitui repository:
//! - [asyncgit] for Git operations in the background.
//!   - git2-hooks (used by asyncgit).
//!     - git2-testing (used by git2-hooks).
//!   - invalidstring used by asyncgit for testing with invalid strings.
//! - [filetreelist] for a tree view of files.
//! - [scopetime] for measuring execution time.
//!

#![forbid(unsafe_code)]
#![deny(
	mismatched_lifetime_syntaxes,
	unused_imports,
	unused_must_use,
	dead_code,
	unstable_name_collisions,
	unused_assignments
)]
#![deny(clippy::all, clippy::perf, clippy::nursery, clippy::pedantic)]
#![deny(
	clippy::unwrap_used,
	clippy::filetype_is_file,
	clippy::cargo,
	clippy::panic,
	clippy::match_like_matches_macro
)]
#![allow(
	clippy::multiple_crate_versions,
	clippy::bool_to_int_with_if,
	clippy::module_name_repetitions,
	clippy::empty_docs,
	clippy::unnecessary_debug_formatting
)]

//TODO:
// #![deny(clippy::expect_used)]

mod app;
mod args;
mod bug_report;
mod clipboard;
mod cmdbar;
mod components;
mod gitui;
mod input;
mod keys;
mod modal;
mod notify_mutex;
mod options;
mod popup_stack;
mod popups;
mod queue;
mod rebase_service;
mod spinner;
mod string_utils;
mod strings;
mod tabs;
mod ui;
mod watcher;

use crate::{
	app::App,
	args::{process_cmdline, CliArgs},
};
use anyhow::{anyhow, bail, Result};
use app::QuitState;
use asyncgit::{sync::RepoPath, AsyncGitNotification};
use backtrace::Backtrace;
use crossbeam_channel::{Receiver, Select};
use crossterm::{
	event::{
		DisableMouseCapture, EnableMouseCapture,
		KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
		PushKeyboardEnhancementFlags,
	},
	terminal::{
		disable_raw_mode, enable_raw_mode, EnterAlternateScreen,
		LeaveAlternateScreen,
	},
	ExecutableCommand,
};
use gitui::Gitui;
use input::InputEvent;
use keys::KeyConfig;
use ratatui::backend::CrosstermBackend;
use scopeguard::defer;
use std::{
	io::{self, Stdout},
	panic,
	path::{Path, PathBuf},
	sync::atomic::{AtomicBool, Ordering},
	time::{Duration, Instant},
};
use ui::style::Theme;

type Terminal = ratatui::Terminal<CrosstermBackend<io::Stdout>>;

static TICK_INTERVAL: Duration = Duration::from_secs(5);
static SPINNER_INTERVAL: Duration = Duration::from_millis(80);
static KEYBOARD_ENHANCEMENTS_ACTIVE: AtomicBool =
	AtomicBool::new(false);

/// Events sent to the event-loop to wake up the UI.
#[derive(Clone)]
pub enum QueueEvent {
	Tick,
	Notify,
	SpinnerUpdate,
	AsyncEvent(AsyncNotification),
	InputEvent(InputEvent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyntaxHighlightProgress {
	Progress,
	Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsyncAppNotification {
	///
	SyntaxHighlighting(SyntaxHighlightProgress),
	/// A native diff row-highlighting job completed and the view should redraw.
	DiffHighlighting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsyncNotification {
	///
	App(AsyncAppNotification),
	///
	Git(AsyncGitNotification),
}

#[derive(Clone, Copy, PartialEq)]
enum Updater {
	Ticker,
	NotifyWatcher,
}

/// Do `log::error!` and `eprintln!` in one line.
macro_rules! log_eprintln {
	( $($arg:tt)* ) => {{
		log::error!($($arg)*);
		eprintln!($($arg)*);
	}};
}

fn main() -> Result<()> {
	if std::env::args().nth(1).as_deref() == Some("--rebase-bridge") {
		run_rebase_bridge();
	}

	let app_start = Instant::now();

	let cliargs = process_cmdline()?;

	asyncgit::register_tracing_logging();
	ensure_valid_path(&cliargs.repo_path)?;

	let key_config = KeyConfig::init_with_keymap(
		cliargs.key_bindings_path.as_ref(),
		cliargs.key_symbols_path.as_ref(),
		cliargs.keymap,
	)
	.map_err(|e| anyhow!("KeyConfig loading error: {e}"))?;
	let theme = Theme::init(&cliargs.theme);

	setup_terminal()?;
	defer! {
		shutdown_terminal();
	}

	set_panic_handler()?;

	let mut terminal =
		start_terminal(io::stdout(), &cliargs.repo_path)?;

	let updater = if cliargs.notify_watcher {
		Updater::NotifyWatcher
	} else {
		Updater::Ticker
	};

	let mut args = cliargs;

	loop {
		let quit_state = run_app(
			app_start,
			args.clone(),
			theme.clone(),
			&key_config,
			updater,
			&mut terminal,
		)?;

		match quit_state {
			QuitState::OpenSubmodule(p) => {
				args = CliArgs {
					keymap: args.keymap,
					repo_path: p,
					select_file: None,
					theme: args.theme,
					notify_watcher: args.notify_watcher,
					key_bindings_path: args.key_bindings_path,
					key_symbols_path: args.key_symbols_path,
				}
			}
			_ => break,
		}
	}

	Ok(())
}

/// The hidden rebase editor-bridge subcommand: git invokes
/// gitui as the (sequence) editor while the main gitui process
/// runs a rebase through [`asyncgit::rebase`]. Runs before any
/// terminal setup and never logs — git captures our output.
///
/// Protocol: `gitui --rebase-bridge [msg] <file>` with the
/// bridge directory in `GITUI_REBASE_BRIDGE`. Always exits with
/// the helper's exit code.
fn run_rebase_bridge() {
	let args: Vec<String> = std::env::args().skip(2).collect();
	let (kind, file) = match args.as_slice() {
		[file] => (
			asyncgit::rebase::snapshot::BridgeKind::Sequence,
			file.clone(),
		),
		[marker, file] if marker == "msg" => (
			asyncgit::rebase::snapshot::BridgeKind::Message,
			file.clone(),
		),
		_ => std::process::exit(1),
	};
	let Some(dir) =
		std::env::var_os("GITUI_REBASE_BRIDGE").map(PathBuf::from)
	else {
		std::process::exit(1);
	};
	let file = PathBuf::from(file);
	let code =
		asyncgit::rebase::helper::run_with_kind(&dir, &file, kind);
	std::process::exit(code);
}

fn run_app(
	app_start: Instant,
	cliargs: CliArgs,
	theme: Theme,
	key_config: &KeyConfig,
	updater: Updater,
	terminal: &mut Terminal,
) -> Result<QuitState, anyhow::Error> {
	let mut gitui = Gitui::new(cliargs, theme, key_config, updater)?;

	log::trace!("app start: {} ms", app_start.elapsed().as_millis());

	gitui.run_main_loop(terminal)
}

fn setup_terminal() -> Result<()> {
	enable_raw_mode()?;
	if let Err(error) = io::stdout().execute(EnterAlternateScreen) {
		let _ = disable_raw_mode();
		return Err(error.into());
	}
	if let Err(error) = io::stdout().execute(EnableMouseCapture) {
		let _ = io::stdout().execute(LeaveAlternateScreen);
		let _ = disable_raw_mode();
		return Err(error.into());
	}
	if let Err(error) =
		push_keyboard_enhancement_flags(&mut io::stdout())
	{
		let _ = io::stdout().execute(PopKeyboardEnhancementFlags);
		let _ = io::stdout().execute(DisableMouseCapture);
		let _ = io::stdout().execute(LeaveAlternateScreen);
		let _ = disable_raw_mode();
		return Err(error.into());
	}
	Ok(())
}

fn shutdown_terminal() {
	if let Err(e) = pop_keyboard_enhancement_flags(&mut io::stdout())
	{
		log::error!("pop_keyboard_enhancement_flags failed:\n{e}");
	}

	if let Err(e) = io::stdout().execute(DisableMouseCapture) {
		log::error!("disable_mouse_capture failed:\n{e}");
	}

	let leave_screen =
		io::stdout().execute(LeaveAlternateScreen).map(|_f| ());

	if let Err(e) = leave_screen {
		log::error!("leave_screen failed:\n{e}");
	}

	let leave_raw_mode = disable_raw_mode();

	if let Err(e) = leave_raw_mode {
		log::error!("leave_raw_mode failed:\n{e}");
	}
}

pub(crate) const fn keyboard_enhancement_flags(
) -> KeyboardEnhancementFlags {
	KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
		.union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
		.union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS)
		.union(
			KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
		)
}

pub(crate) fn push_keyboard_enhancement_flags(
	writer: &mut impl io::Write,
) -> io::Result<()> {
	writer.execute(PushKeyboardEnhancementFlags(
		keyboard_enhancement_flags(),
	))?;
	KEYBOARD_ENHANCEMENTS_ACTIVE.store(true, Ordering::SeqCst);
	Ok(())
}

pub(crate) fn pop_keyboard_enhancement_flags(
	writer: &mut impl io::Write,
) -> io::Result<()> {
	if KEYBOARD_ENHANCEMENTS_ACTIVE.load(Ordering::SeqCst) {
		writer.execute(PopKeyboardEnhancementFlags)?;
		KEYBOARD_ENHANCEMENTS_ACTIVE.store(false, Ordering::SeqCst);
	}
	Ok(())
}

fn draw<B: ratatui::backend::Backend>(
	terminal: &mut ratatui::Terminal<B>,
	app: &App,
) -> Result<(), B::Error> {
	if app.requires_redraw() {
		clear_terminal_for_full_redraw(terminal)?;
	}

	terminal.draw(|f| {
		if let Err(e) = app.draw(f) {
			log::error!("failed to draw: {e:?}");
		}
	})?;

	Ok(())
}

/// Clear terminal contents and invalidate Ratatui's previous frame without
/// querying the cursor, which can race the input reader after an editor exits.
fn clear_terminal_for_full_redraw<B: ratatui::backend::Backend>(
	terminal: &mut ratatui::Terminal<B>,
) -> Result<(), B::Error> {
	let area = terminal.size()?.into();
	terminal.resize(area)
}

fn ensure_valid_path(repo_path: &RepoPath) -> Result<()> {
	match asyncgit::sync::repo_open_error(repo_path) {
		Some(e) => {
			log::error!("invalid repo path: {e}");
			bail!("invalid repo path: {e}")
		}
		None => Ok(()),
	}
}

fn select_event(
	rx_input: &Receiver<InputEvent>,
	rx_git: &Receiver<AsyncGitNotification>,
	rx_app: &Receiver<AsyncAppNotification>,
	rx_ticker: &Receiver<Instant>,
	rx_notify: &Receiver<()>,
	rx_spinner: &Receiver<Instant>,
) -> Result<QueueEvent> {
	let mut sel = Select::new();

	sel.recv(rx_input);
	sel.recv(rx_git);
	sel.recv(rx_app);
	sel.recv(rx_ticker);
	sel.recv(rx_notify);
	sel.recv(rx_spinner);

	let oper = sel.select();
	let index = oper.index();

	let ev = match index {
		0 => oper.recv(rx_input).map(QueueEvent::InputEvent),
		1 => oper.recv(rx_git).map(|e| {
			QueueEvent::AsyncEvent(AsyncNotification::Git(e))
		}),
		2 => oper.recv(rx_app).map(|e| {
			QueueEvent::AsyncEvent(AsyncNotification::App(e))
		}),
		3 => oper.recv(rx_ticker).map(|_| QueueEvent::Tick),
		4 => oper.recv(rx_notify).map(|()| QueueEvent::Notify),
		5 => oper.recv(rx_spinner).map(|_| QueueEvent::SpinnerUpdate),
		_ => bail!("unknown select source"),
	}?;

	Ok(ev)
}

fn start_terminal(
	buf: Stdout,
	repo_path: &RepoPath,
) -> Result<Terminal> {
	let mut path = repo_path.gitpath().canonicalize()?;
	let home = dirs::home_dir().ok_or_else(|| {
		anyhow!("failed to find the home directory")
	})?;
	if path.starts_with(&home) {
		let relative_part = path
			.strip_prefix(&home)
			.expect("can't fail because of the if statement");
		path = Path::new("~").join(relative_part);
	}

	let mut backend = CrosstermBackend::new(buf);
	backend.execute(crossterm::terminal::SetTitle(format!(
		"gitui ({})",
		path.display()
	)))?;

	let mut terminal = Terminal::new(backend)?;
	terminal.hide_cursor()?;
	clear_terminal_for_full_redraw(&mut terminal)?;

	Ok(terminal)
}

fn set_panic_handler() -> Result<()> {
	panic::set_hook(Box::new(|e| {
		let backtrace = Backtrace::new();
		shutdown_terminal();
		log_eprintln!("\nGitUI was closed due to an unexpected panic.\nPlease file an issue on https://github.com/gitui-org/gitui/issues with the following info:\n\n{e}\n\ntrace:\n{backtrace:?}");
	}));

	// global threadpool
	rayon_core::ThreadPoolBuilder::new()
		.num_threads(4)
		.build_global()?;

	Ok(())
}

#[cfg(test)]
mod terminal_redraw_tests {
	use super::clear_terminal_for_full_redraw;
	use ratatui::{
		backend::{Backend, ClearType, TestBackend, WindowSize},
		buffer::Cell,
		layout::{Position, Size},
		widgets::Paragraph,
		Terminal,
	};
	use std::{convert::Infallible, io};

	struct CursorQueryFailureBackend {
		inner: TestBackend,
	}

	impl CursorQueryFailureBackend {
		fn new(width: u16, height: u16) -> Self {
			Self {
				inner: TestBackend::new(width, height),
			}
		}
	}

	fn convert_infallible<T>(
		result: Result<T, Infallible>,
	) -> Result<T, io::Error> {
		result.map_err(|never| match never {})
	}

	impl Backend for CursorQueryFailureBackend {
		type Error = io::Error;

		fn draw<'a, I>(
			&mut self,
			content: I,
		) -> Result<(), Self::Error>
		where
			I: Iterator<Item = (u16, u16, &'a Cell)>,
		{
			convert_infallible(self.inner.draw(content))
		}

		fn flush(&mut self) -> Result<(), Self::Error> {
			convert_infallible(self.inner.flush())
		}

		fn hide_cursor(&mut self) -> Result<(), Self::Error> {
			convert_infallible(self.inner.hide_cursor())
		}

		fn show_cursor(&mut self) -> Result<(), Self::Error> {
			convert_infallible(self.inner.show_cursor())
		}

		fn get_cursor_position(
			&mut self,
		) -> Result<Position, Self::Error> {
			Err(io::Error::other(
				"cursor-position query must not be used during redraw",
			))
		}

		fn set_cursor_position<P: Into<Position>>(
			&mut self,
			position: P,
		) -> Result<(), Self::Error> {
			convert_infallible(
				self.inner.set_cursor_position(position),
			)
		}

		fn clear(&mut self) -> Result<(), Self::Error> {
			convert_infallible(self.inner.clear())
		}

		fn clear_region(
			&mut self,
			clear_type: ClearType,
		) -> Result<(), Self::Error> {
			convert_infallible(self.inner.clear_region(clear_type))
		}

		fn size(&self) -> Result<Size, Self::Error> {
			convert_infallible(self.inner.size())
		}

		fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
			convert_infallible(self.inner.window_size())
		}
	}

	#[test]
	fn full_redraw_reset_restores_ui_without_querying_cursor() {
		let mut terminal =
			Terminal::new(CursorQueryFailureBackend::new(24, 3))
				.unwrap();
		terminal
			.draw(|frame| {
				frame.render_widget(
					Paragraph::new("previous view"),
					frame.area(),
				);
			})
			.unwrap();

		terminal.backend_mut().inner.clear().unwrap();
		clear_terminal_for_full_redraw(&mut terminal).unwrap();
		terminal
			.draw(|frame| {
				frame.render_widget(
					Paragraph::new("restored interface"),
					frame.area(),
				);
			})
			.unwrap();

		let buffer = terminal.backend().inner.buffer();
		let rendered = (0..buffer.area.height)
			.map(|y| {
				(0..buffer.area.width)
					.map(|x| buffer[(x, y)].symbol())
					.collect::<String>()
			})
			.collect::<Vec<_>>()
			.join("\n");
		assert!(
			rendered.contains("restored interface"),
			"{rendered}"
		);
	}
}
