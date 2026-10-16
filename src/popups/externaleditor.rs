use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		DrawableComponent, EventState,
	},
	keys::SharedKeyConfig,
	strings,
	ui::{self, style::SharedTheme},
};
use anyhow::{anyhow, bail, Context, Result};
use asyncgit::sync::{
	get_config_string, utils::repo_work_dir, RepoPath,
};
use crossterm::{
	event::{DisableMouseCapture, EnableMouseCapture, Event},
	terminal::{
		disable_raw_mode, enable_raw_mode, EnterAlternateScreen,
		LeaveAlternateScreen,
	},
	ExecutableCommand,
};
use ratatui::{
	layout::Rect,
	text::{Line, Span},
	widgets::{Block, BorderType, Borders, Clear, Paragraph},
	Frame,
};
use std::{
	env,
	ffi::OsString,
	io::{self, Write},
	path::Path,
	process::{Command, ExitStatus},
};

///
pub struct ExternalEditorPopup {
	visible: bool,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
}

impl ExternalEditorPopup {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			visible: false,
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
		}
	}

	/// Opens `path` in the editor configured for this repository.
	pub fn open_file_in_editor(
		repo: &RepoPath,
		path: &Path,
	) -> Result<()> {
		let work_dir = repo_work_dir(repo)?;
		let path = if path.is_relative() {
			Path::new(&work_dir).join(path)
		} else {
			path.to_path_buf()
		};

		if !path.exists() {
			bail!("file not found: {path:?}");
		}

		let editor = configured_editor(repo)?;
		run_editor(&work_dir, &path, &editor)
	}
}

fn configured_editor(repo: &RepoPath) -> Result<String> {
	if let Some(editor) = env::var_os("GIT_EDITOR") {
		return nonempty_editor(editor, "GIT_EDITOR");
	}
	if let Some(editor) = get_config_string(repo, "core.editor")? {
		return nonempty_editor(editor.into(), "core.editor");
	}
	for variable in ["VISUAL", "EDITOR"] {
		if let Some(editor) = env::var_os(variable) {
			return nonempty_editor(editor, variable);
		}
	}
	Ok(String::from("vi"))
}

fn nonempty_editor(value: OsString, source: &str) -> Result<String> {
	let editor = value
		.into_string()
		.map_err(|_| anyhow!("{source} is not valid UTF-8"))?;
	if editor.trim().is_empty() {
		bail!("editor command from {source} is empty");
	}
	Ok(editor)
}

fn run_editor(
	work_dir: &str,
	path: &Path,
	editor: &str,
) -> Result<()> {
	let status = with_terminal_suspended(|| {
		run_editor_process(work_dir, path, editor)
	})?;
	check_editor_status(editor, status)
}

fn run_editor_process(
	work_dir: &str,
	path: &Path,
	editor: &str,
) -> Result<ExitStatus> {
	#[cfg(unix)]
	{
		let shell_command = format!("{editor} \"$@\"");
		Command::new("sh")
			.arg("-c")
			.arg(shell_command)
			.arg("gitui-editor")
			.arg(path)
			.current_dir(work_dir)
			.status()
			.with_context(|| {
				format!("failed to launch editor {editor:?}")
			})
	}
	#[cfg(windows)]
	{
		Command::new("cmd")
			.args(["/D", "/S", "/C"])
			.arg(format!("{editor} \"%GITUI_EDITOR_FILE%\""))
			.env("GITUI_EDITOR_FILE", path)
			.current_dir(work_dir)
			.status()
			.with_context(|| {
				format!("failed to launch editor {editor:?}")
			})
	}
}

fn check_editor_status(
	editor: &str,
	status: ExitStatus,
) -> Result<()> {
	if status.success() {
		Ok(())
	} else {
		bail!("editor {editor:?} exited with status {status}")
	}
}

fn with_terminal_suspended<T>(
	run: impl FnOnce() -> Result<T>,
) -> Result<T> {
	let mut stdout = io::stdout();
	let mut keyboard_flags_popped = false;
	let mut raw_mode_disabled = false;
	let mut mouse_capture_disabled = false;
	let mut alternate_screen_left = false;
	let suspend_result = (|| -> Result<()> {
		crate::pop_keyboard_enhancement_flags(&mut stdout)
			.context("failed to pop keyboard enhancement flags")?;
		keyboard_flags_popped = true;
		disable_raw_mode()
			.context("failed to disable terminal raw mode")?;
		raw_mode_disabled = true;
		stdout
			.execute(DisableMouseCapture)
			.context("failed to disable mouse capture")?;
		mouse_capture_disabled = true;
		stdout
			.execute(LeaveAlternateScreen)
			.context("failed to leave alternate screen")?;
		alternate_screen_left = true;
		stdout.flush()?;
		Ok(())
	})();

	let run_result = match suspend_result {
		Ok(()) => run(),
		Err(error) => Err(error),
	};
	let mut restore_errors = Vec::new();
	let mut alternate_screen_active = !alternate_screen_left;
	if alternate_screen_left {
		match stdout
			.execute(EnterAlternateScreen)
			.context("failed to restore alternate screen")
		{
			Ok(_) => alternate_screen_active = true,
			Err(error) => restore_errors.push(error),
		}
	}
	if raw_mode_disabled {
		if let Err(error) = enable_raw_mode()
			.context("failed to restore terminal raw mode")
		{
			restore_errors.push(error);
		}
	}
	if mouse_capture_disabled {
		if let Err(error) = stdout
			.execute(EnableMouseCapture)
			.context("failed to restore mouse capture")
		{
			restore_errors.push(error);
		}
	}
	if keyboard_flags_popped && alternate_screen_active {
		if let Err(error) =
			crate::push_keyboard_enhancement_flags(&mut stdout)
				.context(
					"failed to restore keyboard enhancement flags",
				) {
			restore_errors.push(error);
		}
	}
	if let Err(error) = stdout.flush() {
		restore_errors.push(error.into());
	}
	let resume_result = restore_errors
		.into_iter()
		.next()
		.map_or_else(|| Ok(()), Err);

	match (run_result, resume_result) {
		(Ok(value), Ok(())) => Ok(value),
		(Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
		(Err(error), Err(restore_error)) => {
			Err(error.context(format!(
				"also failed to restore terminal: {restore_error}"
			)))
		}
	}
}

impl DrawableComponent for ExternalEditorPopup {
	fn draw(&self, f: &mut Frame, _rect: Rect) -> Result<()> {
		if self.visible {
			let txt = Line::from(
				strings::msg_opening_editor(&self.key_config)
					.split('\n')
					.map(|string| {
						Span::raw::<String>(string.to_string())
					})
					.collect::<Vec<Span>>(),
			);

			let area = ui::centered_rect_absolute(25, 3, f.area());
			f.render_widget(Clear, area);
			f.render_widget(
				Paragraph::new(txt)
					.block(
						Block::default()
							.borders(Borders::ALL)
							.border_type(BorderType::Thick)
							.border_style(self.theme.block(true)),
					)
					.style(self.theme.block(true)),
				area,
			);
		}

		Ok(())
	}
}

impl Component for ExternalEditorPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible && !force_all {
			out.clear();
		}

		visibility_blocking(self)
	}

	fn event(&mut self, _ev: &Event) -> Result<EventState> {
		if self.visible {
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

#[cfg(all(test, unix))]
mod tests {
	use super::{check_editor_status, run_editor_process};
	use std::{fs, os::unix::fs::PermissionsExt};

	fn make_executable(path: &std::path::Path, contents: &str) {
		fs::write(path, contents).expect("write fake editor");
		let mut permissions = fs::metadata(path)
			.expect("editor metadata")
			.permissions();
		permissions.set_mode(0o755);
		fs::set_permissions(path, permissions)
			.expect("make editor executable");
	}

	#[test]
	fn shell_expansion_quoted_args_and_path_with_spaces_are_safe() {
		let dir = tempfile::tempdir().expect("temporary directory");
		let editor_path = dir.path().join("fake editor");
		let marker_path = dir.path().join("marker");
		let message_path =
			dir.path().join("message $(touch marker) with spaces");
		make_executable(
			&editor_path,
			"#!/bin/sh\ntest \"$1\" = 'argument with spaces' || exit 19\ntest \"$2\" = \"$HOME\" || exit 20\ntest -f \"$3\" || exit 21\nprintf 'edited' > \"$3\"\n",
		);
		fs::write(&message_path, "initial").expect("create message");

		let editor = format!(
			"\"{}\" 'argument with spaces' \"$HOME\"",
			editor_path.display()
		);
		let status = run_editor_process(
			dir.path().to_str().expect("UTF-8 path"),
			&message_path,
			&editor,
		)
		.expect("launch fake editor");
		check_editor_status(&editor, status)
			.expect("editor exits successfully");
		assert_eq!(
			fs::read_to_string(message_path).expect("read message"),
			"edited"
		);
		assert!(!marker_path.exists());
	}

	#[test]
	fn surfaces_editor_failure_and_keeps_recoverable_message() {
		let dir = tempfile::tempdir().expect("temporary directory");
		let editor_path = dir.path().join("fake-editor");
		let message_path = dir.path().join("message");
		make_executable(
			&editor_path,
			"#!/bin/sh\nprintf 'edited' > \"$1\"\nexit 23\n",
		);
		fs::write(&message_path, "recover me")
			.expect("create message");
		let editor = format!("\"{}\"", editor_path.display());
		let status = run_editor_process(
			dir.path().to_str().expect("UTF-8 path"),
			&message_path,
			&editor,
		)
		.expect("launch fake editor");
		assert!(check_editor_status(&editor, status).is_err());
		assert_eq!(
			fs::read_to_string(message_path).expect("read message"),
			"edited"
		);
	}
}
