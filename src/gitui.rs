use std::time::Instant;

use anyhow::Result;
use asyncgit::{sync::utils::repo_work_dir, AsyncGitNotification};
use crossbeam_channel::{never, tick, unbounded, Receiver};
use scopetime::scope_time;

#[cfg(test)]
use crossterm::event::{KeyCode, KeyModifiers};

use crate::{
	app::{App, QuitState},
	args::CliArgs,
	draw,
	input::{Input, InputEvent, InputState},
	keys::KeyConfig,
	select_event,
	spinner::Spinner,
	ui::style::Theme,
	watcher::RepoWatcher,
	AsyncAppNotification, AsyncNotification, QueueEvent, Updater,
	SPINNER_INTERVAL, TICK_INTERVAL,
};

pub struct Gitui {
	app: crate::app::App,
	rx_input: Receiver<InputEvent>,
	rx_git: Receiver<AsyncGitNotification>,
	rx_app: Receiver<AsyncAppNotification>,
	rx_ticker: Receiver<Instant>,
	rx_watcher: Receiver<()>,
}

impl Gitui {
	pub(crate) fn new(
		cliargs: CliArgs,
		theme: Theme,
		key_config: &KeyConfig,
		updater: Updater,
	) -> Result<Self, anyhow::Error> {
		let (tx_git, rx_git) = unbounded();
		let (tx_app, rx_app) = unbounded();

		let input = Input::new();

		let (rx_ticker, rx_watcher) = match updater {
			Updater::NotifyWatcher => {
				let repo_watcher = RepoWatcher::new(
					repo_work_dir(&cliargs.repo_path)?.as_str(),
				);

				(never(), repo_watcher.receiver())
			}
			Updater::Ticker => (tick(TICK_INTERVAL), never()),
		};

		let app = App::new(
			cliargs,
			tx_git,
			tx_app,
			input.clone(),
			theme,
			key_config.clone(),
		)?;

		Ok(Self {
			app,
			rx_input: input.receiver(),
			rx_git,
			rx_app,
			rx_ticker,
			rx_watcher,
		})
	}

	pub(crate) fn run_main_loop<B: ratatui::backend::Backend>(
		&mut self,
		terminal: &mut ratatui::Terminal<B>,
	) -> Result<QuitState, anyhow::Error>
	where
		<B as ratatui::backend::Backend>::Error:
			'static + Send + Sync,
	{
		let spinner_ticker = tick(SPINNER_INTERVAL);
		let mut spinner = Spinner::default();

		// Paint a lightweight shell before activating the requested
		// tab. Showing tabs starts repository reads, so activation must
		// happen only after the first frame reaches the terminal.
		self.draw(terminal)?;
		self.app.activate_initial_tab()?;
		self.app.update()?;
		// `App::update` starts the initial asynchronous repository
		// reads; paint their pending state before waiting for results.
		self.draw(terminal)?;
		spinner.set_state(self.app.any_work_pending());
		spinner.draw(terminal)?;

		loop {
			let event = select_event(
				&self.rx_input,
				&self.rx_git,
				&self.rx_app,
				&self.rx_ticker,
				&self.rx_watcher,
				&spinner_ticker,
			)?;

			{
				if matches!(event, QueueEvent::SpinnerUpdate) {
					spinner.update();
					spinner.draw(terminal)?;
					continue;
				}

				scope_time!("loop");

				match event {
					QueueEvent::InputEvent(ev) => {
						if matches!(
							ev,
							InputEvent::State(InputState::Polling)
						) {
							//Note: external ed closed, we need to re-hide cursor
							terminal.hide_cursor()?;
						}
						self.app.event(ev)?;
					}
					QueueEvent::Tick => {
						self.app.poll_repository();
						self.app.update()?;
					}
					QueueEvent::Notify => {
						self.app.invalidate_repository();
						self.app.update()?;
					}
					QueueEvent::AsyncEvent(ev) => {
						if !matches!(
							ev,
							AsyncNotification::Git(
								AsyncGitNotification::FinishUnchanged
							)
						) {
							self.app.update_async(ev)?;
						}
					}
					QueueEvent::SpinnerUpdate => unreachable!(),
				}

				self.draw(terminal)?;

				spinner.set_state(self.app.any_work_pending());
				spinner.draw(terminal)?;

				if self.app.is_quit() {
					break;
				}
			}
		}

		Ok(self.app.quit_state())
	}

	fn draw<B: ratatui::backend::Backend>(
		&self,
		terminal: &mut ratatui::Terminal<B>,
	) -> Result<(), B::Error> {
		draw(terminal, &self.app)
	}

	#[cfg(test)]
	fn activate_initial_tab(&mut self) {
		self.app.activate_initial_tab().unwrap();
	}

	#[cfg(test)]
	fn input_event(
		&mut self,
		code: KeyCode,
		modifiers: KeyModifiers,
	) {
		let event = crossterm::event::KeyEvent::new(code, modifiers);
		self.app
			.event(crate::input::InputEvent::Input(
				crossterm::event::Event::Key(event),
			))
			.unwrap();
	}

	#[cfg(test)]
	fn input_event_kind(
		&mut self,
		code: KeyCode,
		modifiers: KeyModifiers,
		kind: crossterm::event::KeyEventKind,
	) {
		let event = crossterm::event::KeyEvent::new_with_kind(
			code, modifiers, kind,
		);
		self.app
			.event(crate::input::InputEvent::Input(
				crossterm::event::Event::Key(event),
			))
			.unwrap();
	}

	#[cfg(test)]
	fn wait_for_async_git_notification(
		&mut self,
		expected: AsyncGitNotification,
	) {
		loop {
			let actual = self
				.rx_git
				.recv_timeout(std::time::Duration::from_millis(100))
				.unwrap();
			self.app
				.update_async(crate::AsyncNotification::Git(actual))
				.unwrap();

			if actual == expected {
				break;
			}
		}
	}

	/// Drain every outstanding worker outcome and observation
	/// until the app reports no pending work and the
	/// notification channel has been quiet for a few rounds.
	#[cfg(test)]
	fn pump(&mut self) {
		let deadline = std::time::Instant::now()
			+ std::time::Duration::from_secs(60);
		let mut quiet_rounds = 0_u32;
		while std::time::Instant::now() < deadline {
			match self
				.rx_git
				.recv_timeout(std::time::Duration::from_millis(50))
			{
				Ok(event) => {
					quiet_rounds = 0;
					self.app
						.update_async(crate::AsyncNotification::Git(
							event,
						))
						.unwrap();
				}
				Err(_) => {
					quiet_rounds += 1;
					if quiet_rounds >= 3
						&& !self.app.any_work_pending()
					{
						break;
					}
				}
			}
		}
		self.update();
	}

	#[cfg(test)]
	fn update(&mut self) {
		self.app.update().unwrap();
	}
}

#[cfg(test)]
mod tests {
	use std::{cell::RefCell, path::PathBuf};

	use asyncgit::{sync::RepoPath, AsyncGitNotification};
	use crossterm::event::{KeyCode, KeyModifiers};
	use git2_testing::repo_init_suffix;
	use insta::assert_snapshot;
	use ratatui::{backend::TestBackend, Terminal};

	use crate::{
		args::CliArgs, gitui::Gitui, keys::KeyConfig,
		ui::style::Theme, Updater,
	};

	// Macro adapted from: https://insta.rs/docs/cmd/
	macro_rules! apply_common_filters {
		{} => {
			let mut settings = insta::Settings::clone_current();
			// Windows and MacOS
			// We don't match on the full path, but on the suffix we pass to `repo_init_suffix` below.
			settings.add_filter(r" *\[…\]\S+-insta/?", "[TEMP_FILE]");
			// Linux Temp Folder
			settings.add_filter(r" */tmp/\.tmp\S+-insta/", "[TEMP_FILE]");
			// Commit ids that follow a vertical bar in the
			// Log tab's graph column (glyph `o` and graph
			// whitespace only — those oids are not
			// date-deterministic). The rebase rail's rows
			// (`○ PICK   <oid>`) and inspector lines carry
			// letters in the prefix and are pinned dates,
			// so they deliberately do not match and stay
			// unmasked.
			settings.add_filter(r"(│[*o ]{0,10})[a-f0-9]{7} ", "$1[AAAAA] ");
			// The rebase e2e sandbox repository (its absolute
			// path shows trimmed in the top bar; everything
			// inside the repo is date-deterministic, so oids
			// need no masking).
			settings.add_filter(r"…[^ ]*/repo/", "[TEMP_REPO]/");
			let _bound = settings.bind_to_scope();
		}
	}

	#[test]
	fn gitui_starts() {
		apply_common_filters!();

		let (temp_dir, _repo) = repo_init_suffix(Some("-insta"));
		let path: RepoPath = temp_dir.path().to_str().unwrap().into();
		let cliargs = CliArgs {
			keymap: crate::keys::Keymap::Legacy,
			theme: PathBuf::from("theme.ron"),
			select_file: None,
			repo_path: path,
			notify_watcher: false,
			key_bindings_path: None,
			key_symbols_path: None,
		};

		let theme = Theme::init(&PathBuf::new());
		let key_config = KeyConfig::default();

		let mut gitui =
			Gitui::new(cliargs, theme, &key_config, Updater::Ticker)
				.unwrap();

		let mut terminal =
			Terminal::new(TestBackend::new(90, 12)).unwrap();

		gitui.draw(&mut terminal).unwrap();

		assert_snapshot!("app_loading", terminal.backend());
		assert!(gitui.app.has_deferred_initial_tab());
		assert!(!gitui.app.startup_status_scan_pending());
		gitui.activate_initial_tab();
		gitui.app.update().unwrap();

		gitui.pump();

		gitui.draw(&mut terminal).unwrap();

		assert_snapshot!("app_loading_finished", terminal.backend());

		gitui.input_event(KeyCode::Char('2'), KeyModifiers::empty());
		gitui.input_event(
			key_config.keys.tab_log.code,
			key_config.keys.tab_log.modifiers,
		);

		gitui.wait_for_async_git_notification(
			AsyncGitNotification::Log,
		);

		gitui.update();
		gitui.pump();

		gitui.draw(&mut terminal).unwrap();

		assert_snapshot!(
			"app_log_tab_showing_one_commit",
			terminal.backend()
		);
	}

	#[test]
	fn saved_tab_is_shown_in_the_shell_before_repository_reads() {
		let (temp_dir, _repo) = repo_init_suffix(None::<&str>);
		let repo = RefCell::new(RepoPath::Path(
			temp_dir.path().to_path_buf(),
		));
		let options = crate::options::Options::new(repo);
		options.borrow_mut().set_current_tab(1);

		let cliargs = CliArgs {
			keymap: crate::keys::Keymap::Legacy,
			theme: PathBuf::from("theme.ron"),
			select_file: None,
			repo_path: RepoPath::Path(temp_dir.path().to_path_buf()),
			notify_watcher: false,
			key_bindings_path: None,
			key_symbols_path: None,
		};
		let theme = Theme::init(&PathBuf::new());
		let key_config = KeyConfig::default();
		let gitui =
			Gitui::new(cliargs, theme, &key_config, Updater::Ticker)
				.unwrap();
		let mut terminal =
			Terminal::new(TestBackend::new(90, 12)).unwrap();
		gitui.draw(&mut terminal).unwrap();

		let rendered = terminal.backend().to_string();
		assert!(rendered.contains("Loading Log"), "{rendered}");
		assert!(gitui.app.has_deferred_initial_tab());
		assert!(!gitui.app.startup_status_scan_pending());
	}

	// ------------------------------------------------------------------
	// end-to-end rebase workspace harness: the real app over
	// real (sandboxed) repositories
	// ------------------------------------------------------------------

	/// Fixed ancient dates keep the fixtures' commit oids (and
	/// therefore every rendered short oid) deterministic.
	const OLD_DATE: &str = "2005-04-07T22:13:13 +0000";

	/// POSIX-sh double of the gitui editor bridge, mirroring
	/// `asyncgit::rebase::helper` (see `rebase_service::tests`
	/// for the annotated original): the test runner binary
	/// does not understand `--rebase-bridge`.
	const BRIDGE_SHIM: &str = r#"#!/bin/sh
set -u
dir="$GITUI_REBASE_BRIDGE"
[ "${1:-}" = "--rebase-bridge" ] && shift
kind="seq"
if [ "${1:-}" = "msg" ]; then
	kind="msg"
	shift
fi
file="${1:-}"
cp "$file" "$dir/generated.todo" || exit 1
mode="$(cat "$dir/mode" 2>/dev/null)"
if [ "$kind" = "seq" ]; then
	case "$mode" in
	capture) exit 1 ;;
	apply)
		if [ -f "$dir/plan.todo" ]; then
			cp "$dir/plan.todo" "$file"
		fi
		exit 0
		;;
	*) exit 1 ;;
	esac
fi
if [ -f "$dir/prewritten.txt" ]; then
	cp "$dir/prewritten.txt" "$file"
	exit 0
fi
printf '%s\n%s\n' "$file" "${GITUI_REBASE_PID:-0}" > "$dir/request"
while true; do
	if [ -f "$dir/message.txt" ]; then
		cp "$dir/message.txt" "$file"
		rm -f "$dir/request"
		exit 0
	fi
	if [ -f "$dir/cancel" ]; then
		rm -f "$dir/request"
		exit 1
	fi
	sleep 0.05
done
"#;

	/// A throwaway repository with a hermetic git environment
	/// (scratch global/system config, fixed identity, no
	/// signing, fixed dates). Every git child the *app*
	/// spawns inherits the same environment through the
	/// service's testing seam.
	struct Sandbox {
		dir: tempfile::TempDir,
		env: Vec<(String, std::ffi::OsString)>,
	}

	impl Sandbox {
		fn new() -> Self {
			let dir = tempfile::TempDir::new().unwrap();
			let global = dir.path().join("global.gitconfig");
			std::fs::write(
				&global,
				b"[user]\n\tname = t\n\temail = t@t\n\
				  [commit]\n\tgpgsign = false\n\
				  [core]\n\tabbrev = 40\n",
			)
			.unwrap();
			let system = dir.path().join("system.gitconfig");
			std::fs::write(&system, b"").unwrap();
			let env: Vec<(String, std::ffi::OsString)> = vec![
				(
					String::from("GIT_CONFIG_GLOBAL"),
					global.into_os_string(),
				),
				(
					String::from("GIT_CONFIG_SYSTEM"),
					system.into_os_string(),
				),
				(String::from("GIT_CONFIG_NOSYSTEM"), "1".into()),
				(
					String::from("HOME"),
					dir.path().to_path_buf().into_os_string(),
				),
				(String::from("GIT_AUTHOR_NAME"), "t".into()),
				(String::from("GIT_AUTHOR_EMAIL"), "t@t".into()),
				(String::from("GIT_COMMITTER_NAME"), "t".into()),
				(String::from("GIT_COMMITTER_EMAIL"), "t@t".into()),
				// Pinned dates make every object created in
				// or out of gitui (including rebased
				// commits) byte-deterministic.
				(String::from("GIT_AUTHOR_DATE"), OLD_DATE.into()),
				(String::from("GIT_COMMITTER_DATE"), OLD_DATE.into()),
			];
			let sandbox = Self { dir, env };
			std::fs::create_dir(sandbox.repo()).unwrap();
			sandbox.git_ok(&["init", "-q", "--initial-branch=main"]);
			sandbox
		}

		fn repo(&self) -> PathBuf {
			self.dir.path().join("repo")
		}

		/// The service testing seam's parts: hermetic env for
		/// every git child the app spawns, plus the bridge
		/// shim as the editor-bridge executable.
		fn harness_parts(
			&self,
		) -> (Vec<(String, std::ffi::OsString)>, PathBuf) {
			let shim = self.dir.path().join("bridge-shim.sh");
			std::fs::write(&shim, BRIDGE_SHIM).unwrap();
			use std::os::unix::fs::PermissionsExt;
			std::fs::set_permissions(
				&shim,
				std::fs::Permissions::from_mode(0o755),
			)
			.unwrap();
			(self.env.clone(), shim.canonicalize().unwrap())
		}

		/// Install the service testing seam for app
		/// construction. Returns a guard that uninstalls it.
		fn harness(&self) -> HarnessGuard {
			let (env, shim) = self.harness_parts();
			crate::rebase_service::set_test_harness(Some((
				env, shim,
			)));
			HarnessGuard
		}

		fn git(&self, args: &[&str]) -> std::process::Output {
			self.git_env(args, &[])
		}

		fn git_env(
			&self,
			args: &[&str],
			extra: &[(&str, &str)],
		) -> std::process::Output {
			let mut cmd = std::process::Command::new("git");
			cmd.args(args).current_dir(self.repo());
			for (key, _) in &self.env {
				cmd.env_remove(key);
			}
			for (key, value) in &self.env {
				cmd.env(key, value);
			}
			for (key, value) in extra {
				cmd.env(key, value);
			}
			cmd.output().unwrap()
		}

		fn git_ok(&self, args: &[&str]) -> std::process::Output {
			let out = self.git(args);
			assert!(
				out.status.success(),
				"sandbox git {args:?} failed: {}",
				String::from_utf8_lossy(&out.stderr)
			);
			out
		}

		fn write(&self, file: &str, content: &str) {
			std::fs::write(self.repo().join(file), content).unwrap();
		}

		fn commit(&self, message: &str) -> String {
			self.git_ok(&["add", "-A"]);
			self.git_env(
				&["commit", "-m", message],
				&[
					("GIT_AUTHOR_DATE", OLD_DATE),
					("GIT_COMMITTER_DATE", OLD_DATE),
				],
			);
			self.head()
		}

		fn head(&self) -> String {
			String::from_utf8_lossy(
				&self.git_ok(&["rev-parse", "HEAD"]).stdout,
			)
			.trim()
			.to_owned()
		}

		/// A linear history: `base`, then `one`, `two`,
		/// `three` (newest), each on its own file.
		fn four_commits(&self) -> (String, String) {
			self.write("f.txt", "base\n");
			let base = self.commit("base");
			self.write("one.txt", "1\n");
			self.commit("one");
			self.write("two.txt", "2\n");
			self.commit("two");
			self.write("three.txt", "3\n");
			let tip = self.commit("three");
			(base, tip)
		}

		/// Build the real app over this repository (the
		/// harness must be installed first).
		fn app(&self) -> Gitui {
			self.app_with_keymap(crate::keys::Keymap::Legacy)
		}

		fn app_with_keymap(
			&self,
			keymap: crate::keys::Keymap,
		) -> Gitui {
			let cliargs = CliArgs {
				keymap,
				theme: PathBuf::from("theme.ron"),
				select_file: None,
				repo_path: RepoPath::Path(self.repo()),
				notify_watcher: false,
				key_bindings_path: None,
				key_symbols_path: None,
			};
			let theme = Theme::init(&PathBuf::new());
			let key_config = KeyConfig::with_keymap(keymap);
			let mut app = Gitui::new(
				cliargs,
				theme,
				&key_config,
				Updater::Ticker,
			)
			.unwrap();
			app.activate_initial_tab();
			app.app.update().unwrap();
			app
		}
	}

	/// Uninstalls the service testing seam.
	struct HarnessGuard;

	impl Drop for HarnessGuard {
		fn drop(&mut self) {
			crate::rebase_service::set_test_harness(None);
		}
	}

	/// Pump the real app loop until every rebase worker
	/// outcome and observation has landed.
	fn settle(gitui: &mut Gitui) {
		gitui.pump();
	}

	fn modal_key(app: &mut Gitui, c: char) {
		app.input_event(KeyCode::Char(c), KeyModifiers::NONE);
	}

	fn rendered(app: &Gitui, width: u16, height: u16) -> String {
		let mut terminal =
			Terminal::new(TestBackend::new(width, height)).unwrap();
		app.draw(&mut terminal).unwrap();
		terminal.backend().to_string()
	}

	#[test]
	fn legacy_repeat_navigation_moves_but_repeat_stage_is_ignored() {
		let sandbox = Sandbox::new();
		sandbox.write("base.txt", "base\n");
		sandbox.commit("base");
		sandbox.write("a.txt", "changed a\n");
		sandbox.write("b.txt", "changed b\n");
		let _harness = sandbox.harness();
		let mut app = sandbox.app();
		settle(&mut app);

		app.input_event_kind(
			KeyCode::Enter,
			KeyModifiers::NONE,
			crossterm::event::KeyEventKind::Repeat,
		);
		assert_eq!(
			sandbox
				.git_ok(&["diff", "--cached", "--name-only", "-z"])
				.stdout,
			b""
		);

		app.input_event_kind(
			KeyCode::Down,
			KeyModifiers::NONE,
			crossterm::event::KeyEventKind::Repeat,
		);
		app.input_event(KeyCode::Enter, KeyModifiers::NONE);
		settle(&mut app);
		assert_eq!(
			sandbox
				.git_ok(&["diff", "--cached", "--name-only", "-z"])
				.stdout,
			b"b.txt\0"
		);
	}

	#[test]
	fn modal_e2e_reverse_range_stages_only_the_visible_selection() {
		let s = Sandbox::new();
		for name in ["a.txt", "b.txt", "c.txt"] {
			s.write(name, "base\n");
		}
		s.commit("base");
		let original_head = s.head();
		for name in ["a.txt", "b.txt", "c.txt"] {
			s.write(name, "changed\n");
		}
		let _harness = s.harness();
		let mut app = s.app_with_keymap(crate::keys::Keymap::Helix);
		app.update();
		settle(&mut app);
		modal_key(&mut app, 'v');
		modal_key(&mut app, 'j');
		modal_key(&mut app, 'k');
		assert!(rendered(&app, 100, 25).contains("SELECT 1 files"));
		modal_key(&mut app, 's');
		let staged = s
			.git_ok(&["diff", "--cached", "--name-only", "-z"])
			.stdout;
		assert_eq!(staged, b"a.txt\0");
		assert_eq!(s.head(), original_head);
	}

	#[test]
	fn modal_e2e_explicit_marks_do_not_extend_with_cursor_motion() {
		let s = Sandbox::new();
		for name in ["a.txt", "b.txt", "c.txt"] {
			s.write(name, "base\n");
		}
		s.commit("base");
		for name in ["a.txt", "b.txt", "c.txt"] {
			s.write(name, "changed\n");
		}
		let _harness = s.harness();
		let mut app = s.app_with_keymap(crate::keys::Keymap::Helix);
		app.update();
		settle(&mut app);
		modal_key(&mut app, 'x');
		modal_key(&mut app, '2');
		modal_key(&mut app, 'j');
		modal_key(&mut app, 'x');
		modal_key(&mut app, 's');
		let staged = s
			.git_ok(&["diff", "--cached", "--name-only", "-z"])
			.stdout;
		assert_eq!(staged, b"a.txt\0c.txt\0");
	}

	#[test]
	fn modal_e2e_prefix_palette_and_branch_filter_own_printable_input(
	) {
		let s = Sandbox::new();
		s.four_commits();
		s.git_ok(&["branch", "feature/topic"]);
		let original_head = s.head();
		let _harness = s.harness();
		let mut app = s.app_with_keymap(crate::keys::Keymap::Helix);
		app.update();
		settle(&mut app);
		modal_key(&mut app, 'g');
		modal_key(&mut app, 'q');
		assert!(matches!(
			app.app.quit_state(),
			crate::app::QuitState::None
		));
		app.input_event(KeyCode::Esc, KeyModifiers::NONE);
		modal_key(&mut app, ':');
		for c in "qhjkl g".chars() {
			modal_key(&mut app, c);
		}
		assert!(rendered(&app, 100, 25).contains("qhjkl g"));
		assert!(matches!(
			app.app.quit_state(),
			crate::app::QuitState::None
		));
		app.input_event(KeyCode::Esc, KeyModifiers::NONE);
		modal_key(&mut app, ' ');
		modal_key(&mut app, 'B');
		for c in "feature/topic".chars() {
			modal_key(&mut app, c);
		}
		assert!(rendered(&app, 120, 30).contains("feature/topic"));
		modal_key(&mut app, ' ');
		app.input_event(KeyCode::Esc, KeyModifiers::NONE);
		assert!(rendered(&app, 120, 30).contains("Branches"));
		assert_eq!(s.head(), original_head);
	}

	#[test]
	fn modal_e2e_mouse_then_stage_targets_the_drawn_file() {
		use crossterm::event::{
			MouseButton, MouseEvent, MouseEventKind,
		};
		let sandbox = Sandbox::new();
		for name in ["alpha.txt", "beta.txt", "gamma.txt"] {
			sandbox.write(name, "base\n");
		}
		sandbox.commit("base");
		for name in ["alpha.txt", "beta.txt", "gamma.txt"] {
			sandbox.write(name, "changed\n");
		}
		let _harness = sandbox.harness();
		let mut app =
			sandbox.app_with_keymap(crate::keys::Keymap::Helix);
		app.update();
		settle(&mut app);
		let screen = rendered(&app, 132, 36);
		let (row, column) = screen
			.lines()
			.enumerate()
			.find_map(|(row, line)| {
				line.find("beta.txt")
					.filter(|column| *column < 65)
					.map(|column| (row, column))
			})
			.expect("beta must be visible in the working tree");
		for kind in [
			MouseEventKind::Down(MouseButton::Left),
			MouseEventKind::Up(MouseButton::Left),
		] {
			app.app
				.event(crate::input::InputEvent::Input(
					crossterm::event::Event::Mouse(MouseEvent {
						kind,
						column: u16::try_from(column).unwrap(),
						row: u16::try_from(row).unwrap(),
						modifiers: KeyModifiers::NONE,
					}),
				))
				.unwrap();
			let _ = rendered(&app, 132, 36);
		}
		modal_key(&mut app, 's');
		assert_eq!(
			sandbox
				.git_ok(&["diff", "--cached", "--name-only", "-z"])
				.stdout,
			b"beta.txt\0"
		);
		settle(&mut app);
		assert!(rendered(&app, 132, 36).contains("Staged Changes"));
	}

	#[test]
	fn modal_e2e_mouse_routes_worktree_and_staged_rows_after_diff_focus(
	) {
		use crossterm::event::{
			MouseButton, MouseEvent, MouseEventKind,
		};
		let sandbox = Sandbox::new();
		for name in ["recovery.txt", "staged.txt"] {
			sandbox.write(name, "base\n");
		}
		sandbox.commit("base");
		sandbox.write("recovery.txt", "worktree edit\n");
		sandbox.write("staged.txt", "staged edit\n");
		sandbox.git_ok(&["add", "staged.txt"]);

		let _harness = sandbox.harness();
		let mut app =
			sandbox.app_with_keymap(crate::keys::Keymap::Helix);
		app.update();
		settle(&mut app);
		app.input_event(KeyCode::Tab, KeyModifiers::NONE);
		app.input_event(KeyCode::Tab, KeyModifiers::NONE);

		let click_label = |app: &mut Gitui, label: &str| {
			let screen = rendered(app, 132, 36);
			let (row, column) = screen
				.lines()
				.enumerate()
				.find_map(|(row, line)| {
					line.find(label)
						.filter(|column| *column < 65)
						.map(|column| (row, column))
				})
				.unwrap_or_else(|| {
					panic!("{label:?} must be visible")
				});
			for kind in [
				MouseEventKind::Down(MouseButton::Left),
				MouseEventKind::Up(MouseButton::Left),
			] {
				app.app
					.event(crate::input::InputEvent::Input(
						crossterm::event::Event::Mouse(MouseEvent {
							kind,
							column: u16::try_from(column).unwrap(),
							row: u16::try_from(row).unwrap(),
							modifiers: KeyModifiers::NONE,
						}),
					))
					.unwrap();
				let _ = rendered(app, 132, 36);
			}
		};

		// Diff focus targets the staged side and changes the vertical pane
		// proportions. The visible worktree file must still route to Workdir.
		click_label(&mut app, "recovery.txt");
		modal_key(&mut app, 's');
		settle(&mut app);
		assert_eq!(
			sandbox
				.git_ok(&["diff", "--cached", "--name-only", "-z"])
				.stdout,
			b"recovery.txt\0staged.txt\0"
		);

		click_label(&mut app, "staged.txt");
		modal_key(&mut app, 'u');
		settle(&mut app);
		assert_eq!(
			sandbox
				.git_ok(&["diff", "--cached", "--name-only", "-z"])
				.stdout,
			b"recovery.txt\0"
		);
		assert_eq!(
			sandbox.git_ok(&["diff", "--name-only", "-z"]).stdout,
			b"staged.txt\0"
		);
	}

	#[test]
	fn modal_e2e_enter_stages_one_line_and_preserves_another_hunk() {
		let s = Sandbox::new();
		let base = (0..30)
			.map(|i| format!("line {i}\n"))
			.collect::<String>();
		s.write("file.txt", &base);
		s.commit("base");
		s.write(
			"file.txt",
			&base
				.replace("line 2\n", "first edit\n")
				.replace("line 22\n", "second edit\n"),
		);
		let diff = asyncgit::sync::diff::get_diff(
			&RepoPath::Path(s.repo()),
			"file.txt",
			false,
			None,
		)
		.unwrap();
		let selected = diff
			.hunks
			.iter()
			.flat_map(|h| &h.lines)
			.position(|line| {
				line.line_type == asyncgit::DiffLineType::Add
					&& line.content.contains("first edit")
			})
			.unwrap();
		let _harness = s.harness();
		let mut app = s.app_with_keymap(crate::keys::Keymap::Helix);
		app.update();
		settle(&mut app);
		// App input is not a paint cycle. Establish the projection the user
		// can see before navigation and the following line-stage action.
		let _screen = rendered(&app, 120, 40);
		app.input_event(KeyCode::Char('l'), KeyModifiers::CONTROL);
		modal_key(&mut app, 'g');
		modal_key(&mut app, 'g');
		if selected > 0 {
			for c in selected.to_string().chars() {
				modal_key(&mut app, c);
			}
			modal_key(&mut app, 'j');
		}
		app.input_event(KeyCode::Enter, KeyModifiers::NONE);
		let index = s.git_ok(&["show", ":file.txt"]).stdout;
		let index = String::from_utf8(index).unwrap();
		assert!(index.contains("first edit"));
		assert!(!index.contains("second edit"));
		let remaining = s.git_ok(&["diff", "--", "file.txt"]).stdout;
		assert!(String::from_utf8(remaining)
			.unwrap()
			.contains("second edit"));
	}

	#[test]
	fn modal_e2e_checkout_uses_the_marked_commit_after_cursor_moves()
	{
		let s = Sandbox::new();
		s.four_commits();
		let expected = String::from_utf8(
			s.git_ok(&["rev-parse", "HEAD~1"]).stdout,
		)
		.unwrap()
		.trim()
		.to_owned();
		let _harness = s.harness();
		let mut app = s.app_with_keymap(crate::keys::Keymap::Helix);
		modal_key(&mut app, 'g');
		modal_key(&mut app, '2');
		settle(&mut app);
		modal_key(&mut app, 'j');
		modal_key(&mut app, 'x');
		modal_key(&mut app, 'g');
		modal_key(&mut app, 'g');
		app.input_event(KeyCode::Char('S'), KeyModifiers::SHIFT);
		assert_eq!(s.head(), expected);
	}

	#[test]
	fn modal_e2e_multi_commit_selection_cannot_checkout_the_cursor() {
		let s = Sandbox::new();
		s.four_commits();
		let expected = s.head();
		let _harness = s.harness();
		let mut app = s.app_with_keymap(crate::keys::Keymap::Helix);
		modal_key(&mut app, 'g');
		modal_key(&mut app, '2');
		settle(&mut app);
		modal_key(&mut app, 'v');
		modal_key(&mut app, 'j');
		app.input_event(KeyCode::Char('S'), KeyModifiers::SHIFT);
		assert_eq!(s.head(), expected);
		assert_eq!(
			s.git_ok(&["symbolic-ref", "--short", "HEAD"]).stdout,
			b"main\n"
		);
		assert!(rendered(&app, 120, 30).contains("one commit"));
	}

	#[test]
	fn rebase_e2e_draft() {
		apply_common_filters!();

		let s = Sandbox::new();
		let (_base, _tip) = s.four_commits();
		let _harness = s.harness();

		let mut gitui = s.app();
		let mut terminal =
			Terminal::new(TestBackend::new(100, 30)).unwrap();
		settle(&mut gitui);

		// Log tab, then select `one` (third row of the
		// newest-first list) and rewrite from there: the
		// draft covers `one` and both of its descendants.
		gitui.input_event(KeyCode::Char('2'), KeyModifiers::empty());
		gitui.wait_for_async_git_notification(
			AsyncGitNotification::Log,
		);
		gitui.update();
		gitui.input_event(KeyCode::Down, KeyModifiers::empty());
		gitui.input_event(KeyCode::Down, KeyModifiers::empty());
		gitui.input_event(KeyCode::Char('W'), KeyModifiers::empty());

		settle(&mut gitui);

		gitui.draw(&mut terminal).unwrap();

		// The draft exists on disk before anything visual is
		// asserted (intent before pixels): the journal must
		// carry the scope creation and the adopted plan.
		let journal = s.repo().join(".git/gitui/rebase/journal.ron");
		let journal = std::fs::read_to_string(journal).unwrap();
		assert!(
			journal.contains("Created")
				&& journal.contains("PlanAdopted"),
			"the draft was not journaled"
		);

		assert_snapshot!("rebase_e2e_draft", terminal.backend());
	}

	#[test]
	fn rebase_e2e_conflict_stop() {
		apply_common_filters!();

		let s = Sandbox::new();
		s.write("f.txt", "a\n");
		let base = s.commit("A");
		s.write("f.txt", "b\n");
		s.commit("B");
		s.write("c.txt", "c\n");
		s.commit("C");
		// `other` rewrote the same file differently: replaying
		// main onto it must conflict.
		s.git_ok(&["checkout", "-q", "-b", "other", &base]);
		s.write("f.txt", "x\n");
		let other = s.commit("X");
		s.git_ok(&["checkout", "-q", "main"]);

		// An externally started interactive rebase (git's own
		// todo, the no-op `:` editor), stopped on the conflict
		// before gitui ever runs.
		s.git_env(
			&["rebase", "-i", &other],
			&[
				("GIT_SEQUENCE_EDITOR", ":"),
				("GIT_AUTHOR_DATE", OLD_DATE),
				("GIT_COMMITTER_DATE", OLD_DATE),
			],
		);
		assert!(s.repo().join(".git/rebase-merge").exists());

		let _harness = s.harness();
		let mut gitui = s.app();
		let mut terminal =
			Terminal::new(TestBackend::new(100, 30)).unwrap();

		settle(&mut gitui);

		// The service adopted the external rebase at
		// construction (load-on-start); opening the workspace
		// shows the adopted rail and the conflict inspector.
		gitui.input_event(KeyCode::Char('6'), KeyModifiers::empty());
		gitui.update();

		gitui.draw(&mut terminal).unwrap();

		assert_snapshot!(
			"rebase_e2e_conflict_stop",
			terminal.backend()
		);
	}

	#[test]
	fn rebase_e2e_finished() {
		apply_common_filters!();

		let s = Sandbox::new();
		let (base, tip) = s.four_commits();

		// A complete session, created through the real
		// service before the app exists: the app's own
		// service must open and fold the journal on startup.
		{
			let repo = std::cell::RefCell::new(
				asyncgit::sync::RepoPath::Path(s.repo()),
			);
			let env = crate::app::Environment {
				queue: crate::queue::Queue::new(),
				theme: std::rc::Rc::new(Theme::init(&PathBuf::new())),
				key_config: std::rc::Rc::new(KeyConfig::default()),
				options: crate::options::Options::new(repo.clone()),
				repo,
				sender_git: crossbeam_channel::unbounded().0,
				sender_app: crossbeam_channel::unbounded().0,
			};
			let (engine_env, shim) = s.harness_parts();
			let mut service =
				crate::rebase_service::RebaseService::build(
					&env,
					engine_env,
					Some(shim),
				)
				.unwrap();
			wait_idle(&mut service);

			service.create_draft(
				asyncgit::rebase::engine::Base::Upstream(
					asyncgit::sync::CommitId::from_str_unchecked(
						&base,
					)
					.unwrap(),
				),
				false,
			);
			wait_idle(&mut service);

			// Drop the first pick so the replay rewrites
			// history instead of fast-forwarding.
			let plan = service.session().plan.clone().unwrap();
			let drop_id = plan
				.steps()
				.iter()
				.find(|step| {
					matches!(
						step.line.instruction(),
						Some(
							asyncgit::rebase::todo::Instruction::Pick {
								..
							}
						)
					)
				})
				.map(|step| step.id)
				.unwrap();
			service
				.apply_edit(asyncgit::rebase::plan::Edit::SetAction {
					id: drop_id,
					action: asyncgit::rebase::plan::Action::Drop,
				})
				.unwrap();

			service.start(
				asyncgit::rebase::engine::EmptyPolicy::Stop,
				vec![],
			);
			wait_idle(&mut service);

			assert!(matches!(
				service.session().phase,
				asyncgit::rebase::journal::Phase::Finished(_)
			));
			assert_ne!(s.head(), tip, "history was rewritten");
		}

		let _harness = s.harness();
		let mut gitui = s.app();
		let mut terminal =
			Terminal::new(TestBackend::new(100, 30)).unwrap();

		settle(&mut gitui);

		gitui.input_event(KeyCode::Char('6'), KeyModifiers::empty());
		gitui.update();

		gitui.draw(&mut terminal).unwrap();

		assert_snapshot!("rebase_e2e_finished", terminal.backend());
	}

	/// Pump a service's worker outcomes by polling (the
	/// outcomes channel itself is private; the events surface
	/// through `on_worker_event`).
	fn wait_idle(service: &mut crate::rebase_service::RebaseService) {
		if service.is_loading() {
			service.refresh().unwrap();
		}
		let deadline = std::time::Instant::now()
			+ std::time::Duration::from_secs(60);
		loop {
			let _ = service.on_worker_event();
			if !service.is_busy() && !service.is_loading() {
				// one final drain in case an observation is
				// still landing
				std::thread::sleep(std::time::Duration::from_millis(
					30,
				));
				let _ = service.on_worker_event();
				if !service.is_busy() && !service.is_loading() {
					return;
				}
			}
			assert!(
				std::time::Instant::now() < deadline,
				"the rebase service never went idle"
			);
			std::thread::sleep(std::time::Duration::from_millis(20));
		}
	}
}
