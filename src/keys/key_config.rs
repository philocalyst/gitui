use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};
use std::{fs::canonicalize, path::PathBuf, rc::Rc};

use crate::{args::get_app_config_path, strings::symbol};

use super::{
	key_list::{GituiKeyEvent, KeysList},
	symbols::KeySymbols,
};

/// A profile selects interaction semantics as well as default bindings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Keymap {
	Helix,
	#[default]
	Legacy,
}

pub type SharedKeyConfig = Rc<KeyConfig>;
const KEY_LIST_FILENAME: &str = "key_bindings.ron";
const KEY_SYMBOLS_FILENAME: &str = "key_symbols.ron";

#[derive(Default, Clone)]
pub struct KeyConfig {
	pub keys: KeysList,
	symbols: KeySymbols,
	keymap: Keymap,
}

impl KeyConfig {
	fn get_config_file() -> Result<PathBuf> {
		let app_home = get_app_config_path()?;
		let config_file = app_home.join(KEY_LIST_FILENAME);
		canonicalize(&config_file)
			.map_or_else(|_| Ok(config_file), Ok)
	}

	fn get_symbols_file() -> Result<PathBuf> {
		let app_home = get_app_config_path()?;
		let symbols_file = app_home.join(KEY_SYMBOLS_FILENAME);
		canonicalize(&symbols_file)
			.map_or_else(|_| Ok(symbols_file), Ok)
	}

	#[cfg(test)]
	pub fn init(
		key_bindings_path: Option<&PathBuf>,
		key_symbols_path: Option<&PathBuf>,
	) -> Result<Self> {
		Self::init_with_keymap(
			key_bindings_path,
			key_symbols_path,
			Keymap::Legacy,
		)
	}

	pub fn with_keymap(keymap: Keymap) -> Self {
		Self {
			keys: if keymap == Keymap::Helix {
				KeysList::helix()
			} else {
				KeysList::default()
			},
			keymap,
			..Self::default()
		}
	}

	pub const fn is_modal(&self) -> bool {
		matches!(self.keymap, Keymap::Helix)
	}

	pub fn init_with_keymap(
		key_bindings_path: Option<&PathBuf>,
		key_symbols_path: Option<&PathBuf>,
		keymap: Keymap,
	) -> Result<Self> {
		let defaults = Self::with_keymap(keymap).keys;
		let keys = KeysList::try_load(
			defaults,
			key_bindings_path
				.unwrap_or(&Self::get_config_file()?)
				.clone(),
		)?;
		if keymap == Keymap::Helix {
			validate_modal_bindings(&keys)?;
		}
		let symbols = KeySymbols::init(
			key_symbols_path
				.unwrap_or(&Self::get_symbols_file()?)
				.clone(),
		);

		Ok(Self {
			keys,
			symbols,
			keymap,
		})
	}

	fn get_key_symbol(&self, k: KeyCode) -> &str {
		match k {
			KeyCode::Enter => &self.symbols.enter,
			KeyCode::Left => &self.symbols.left,
			KeyCode::Right => &self.symbols.right,
			KeyCode::Up => &self.symbols.up,
			KeyCode::Down => &self.symbols.down,
			KeyCode::Backspace => &self.symbols.backspace,
			KeyCode::Home => &self.symbols.home,
			KeyCode::End => &self.symbols.end,
			KeyCode::PageUp => &self.symbols.page_up,
			KeyCode::PageDown => &self.symbols.page_down,
			KeyCode::Tab => &self.symbols.tab,
			KeyCode::BackTab => &self.symbols.back_tab,
			KeyCode::Delete => &self.symbols.delete,
			KeyCode::Insert => &self.symbols.insert,
			KeyCode::Esc => &self.symbols.esc,
			_ => "?",
		}
	}

	pub fn get_hint(&self, ev: GituiKeyEvent) -> String {
		if self.is_modal()
			&& ev != self.keys.goto_prefix
			&& [
				self.keys.tab_status,
				self.keys.tab_log,
				self.keys.tab_files,
				self.keys.tab_stashing,
				self.keys.tab_stashes,
				self.keys.tab_rebase,
				self.keys.tab_branches,
			]
			.contains(&ev)
		{
			if let KeyCode::Char(c) = ev.code {
				return format!(
					"{}{c}",
					self.get_hint(self.keys.goto_prefix)
				);
			}
		}
		match ev.code {
			KeyCode::Down
			| KeyCode::Up
			| KeyCode::Right
			| KeyCode::Left
			| KeyCode::Enter
			| KeyCode::Backspace
			| KeyCode::Home
			| KeyCode::End
			| KeyCode::PageUp
			| KeyCode::PageDown
			| KeyCode::Tab
			| KeyCode::BackTab
			| KeyCode::Delete
			| KeyCode::Insert
			| KeyCode::Esc => {
				format!(
					"{}{}",
					self.get_modifier_hint(ev.modifiers),
					self.get_key_symbol(ev.code)
				)
			}
			KeyCode::Char(' ') => format!(
				"{}{}",
				self.get_modifier_hint(ev.modifiers),
				symbol::SPACE
			),
			KeyCode::Char(c) => {
				format!(
					"{}{}",
					self.get_modifier_hint(ev.modifiers),
					c
				)
			}
			KeyCode::F(u) => {
				format!(
					"{}F{}",
					self.get_modifier_hint(ev.modifiers),
					u
				)
			}
			KeyCode::Null => self.get_modifier_hint(ev.modifiers),
			_ => String::new(),
		}
	}

	fn get_modifier_hint(&self, modifiers: KeyModifiers) -> String {
		let mut hint = String::new();
		if modifiers.contains(KeyModifiers::CONTROL) {
			hint.push_str(&self.symbols.control);
		}
		if modifiers.contains(KeyModifiers::ALT) {
			hint.push_str(&self.symbols.alt);
		}
		if modifiers.contains(KeyModifiers::SHIFT) {
			hint.push_str(&self.symbols.shift);
		}
		if modifiers.contains(KeyModifiers::SUPER) {
			hint.push_str("Super+");
		}
		hint
	}
}

/// Reject collisions between the keys owned by the modal router and the
/// application-level selection/focus handlers. A collision here would make a
/// configured binding unreachable even though its hint still appears.
fn validate_modal_bindings(keys: &KeysList) -> Result<()> {
	let core = [
		("move left", keys.move_left),
		("move right", keys.move_right),
		("move up", keys.move_up),
		("move down", keys.move_down),
		("focus left", keys.focus_left),
		("focus right", keys.focus_right),
		("focus up", keys.focus_up),
		("focus down", keys.focus_down),
		("focus next", keys.focus_next),
		("focus previous", keys.focus_previous),
		("selection mode", keys.select_mode),
		("clear selection", keys.select_clear),
		("select all", keys.select_all),
		("select item", keys.select_item),
		("goto prefix", keys.goto_prefix),
		("action prefix", keys.action_prefix),
		("window prefix", keys.window_prefix),
		("command palette", keys.command_palette),
		("diff review toggle", keys.diff_view_toggle),
		("diff zoom toggle", keys.diff_zoom_toggle),
	];
	validate_unique_bindings("modal controls", &core)?;
	validate_core_reserved_keys(&core)?;
	validate_goto_bindings(keys)?;
	validate_branch_bindings(keys)?;
	validate_action_bindings(keys, &core)
}

fn validate_goto_bindings(keys: &KeysList) -> Result<()> {
	let tabs = [
		("status", keys.tab_status),
		("log", keys.tab_log),
		("files", keys.tab_files),
		("stashing", keys.tab_stashing),
		("stashes", keys.tab_stashes),
		("rebase", keys.tab_rebase),
		("branches", keys.tab_branches),
	];
	validate_unique_bindings("goto tabs", &tabs)
}

fn validate_branch_bindings(keys: &KeysList) -> Result<()> {
	let branch_actions = [
		("branch search", keys.branch_find),
		("branch base", keys.branches_set_base),
		("branch create", keys.branches_create),
		("branch checkout", keys.branches_checkout),
		("load more branch commits", keys.branches_load_more),
		("rename branch", keys.rename_branch),
		("delete branch", keys.delete_branch),
		("rebase branch", keys.rebase_branch),
	];
	validate_unique_bindings("branch actions", &branch_actions)
}

fn validate_unique_bindings(
	context: &str,
	bindings: &[(&str, GituiKeyEvent)],
) -> Result<()> {
	for (index, (name, binding)) in bindings.iter().enumerate() {
		for (other_name, other_binding) in
			bindings.iter().skip(index + 1)
		{
			if same_binding(*binding, *other_binding) {
				anyhow::bail!(
					"Helix keymap collision: {context} {name} and {other_name} both use {}",
					format_binding(*binding),
				);
			}
		}
	}
	Ok(())
}

fn validate_core_reserved_keys(
	core: &[(&str, GituiKeyEvent)],
) -> Result<()> {
	for (name, binding) in core {
		if matches!(binding.code, KeyCode::Char('0'..='9')) {
			anyhow::bail!(
				"Helix keymap collision: {name} uses a digit reserved for movement counts",
			);
		}
		if !matches!(
			*name,
			"move left" | "move right" | "move up" | "move down"
		) && matches!(binding.code, KeyCode::Char('G' | '?'))
		{
			anyhow::bail!(
				"Helix keymap collision: {name} uses a key reserved for end-of-list or help",
			);
		}
	}
	Ok(())
}

fn validate_action_bindings(
	keys: &KeysList,
	core: &[(&str, GituiKeyEvent)],
) -> Result<()> {
	// Application actions must remain reachable through the normal router.
	let reserved: Vec<_> =
		core.iter().map(|(_, binding)| *binding).collect();
	for (name, binding) in [
		("stage selected item", keys.stage_unstage_item),
		("unstage selected item", keys.status_unstage_item),
		("open inline commit", keys.open_commit_inline),
		("set branch base", keys.branches_set_base),
		("create branch in workspace", keys.branches_create),
		("checkout branch in workspace", keys.branches_checkout),
		("load more branch commits", keys.branches_load_more),
		("reset status item", keys.status_reset_item),
		("reset diff lines", keys.diff_reset_lines),
		("undo commit", keys.undo_commit),
		("abort merge", keys.abort_merge),
		("abort rebase", keys.rebase_abort),
		("delete branch", keys.delete_branch),
		("delete tag", keys.delete_tag),
		("reset branch", keys.reset_branch),
	] {
		if reserved
			.iter()
			.any(|reserved| same_binding(binding, *reserved))
			|| matches!(binding.code, KeyCode::Char('0'..='9' | '?'))
			|| same_binding(
				binding,
				GituiKeyEvent::new(
					KeyCode::Char('G'),
					KeyModifiers::SHIFT,
				),
			) {
			anyhow::bail!(
				"Helix keymap collision: action {name} uses reserved modal key {}",
				format_binding(binding),
			);
		}
	}
	Ok(())
}

fn same_binding(left: GituiKeyEvent, right: GituiKeyEvent) -> bool {
	use crossterm::event::KeyEvent;
	super::key_list::key_match(
		&KeyEvent::new(left.code, left.modifiers),
		right,
	)
}

fn format_binding(binding: GituiKeyEvent) -> String {
	format!("{:?} {:?}", binding.modifiers, binding.code)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;
	use std::io::Write;
	use tempfile::NamedTempFile;

	#[test]
	fn test_get_hint() {
		let config = KeyConfig::default();
		let h = config.get_hint(GituiKeyEvent::new(
			KeyCode::Char('c'),
			KeyModifiers::CONTROL,
		));
		assert_eq!(h, "^c");
	}

	#[test]
	fn hints_show_combined_modifiers_and_modal_tab_prefixes_without_recursion(
	) {
		let config = KeyConfig::with_keymap(Keymap::Helix);
		assert_eq!(
			config.get_hint(GituiKeyEvent::new(
				KeyCode::Char('x'),
				KeyModifiers::CONTROL | KeyModifiers::SHIFT
			)),
			"^⇧x",
		);
		assert_eq!(
			config.get_hint(GituiKeyEvent::new(
				KeyCode::Char('x'),
				KeyModifiers::CONTROL | KeyModifiers::ALT
			)),
			"^⌥x",
		);
		assert_eq!(
			config.get_hint(GituiKeyEvent::new(
				KeyCode::Char(' '),
				KeyModifiers::ALT
			)),
			"⌥˽",
		);
		assert_eq!(config.get_hint(config.keys.tab_status), "g1");
		assert_eq!(config.get_hint(config.keys.tab_branches), "g7");

		let mut duplicated = config;
		duplicated.keys.tab_status = duplicated.keys.goto_prefix;
		assert_eq!(
			duplicated.get_hint(duplicated.keys.tab_status),
			"g"
		);
	}

	#[test]
	fn modal_profile_rejects_core_and_destructive_collisions() {
		let mut keys = KeysList::helix();
		keys.focus_up = keys.move_up;
		let error =
			validate_modal_bindings(&keys).unwrap_err().to_string();
		assert!(error.contains("move up and focus up"));

		let mut keys = KeysList::helix();
		keys.window_prefix = keys.goto_prefix;
		assert!(validate_modal_bindings(&keys)
			.unwrap_err()
			.to_string()
			.contains("goto prefix and window prefix"));

		let mut keys = KeysList::helix();
		keys.undo_commit = keys.select_item;
		let error =
			validate_modal_bindings(&keys).unwrap_err().to_string();
		assert!(error.contains("undo commit"));

		let mut keys = KeysList::helix();
		keys.select_mode = GituiKeyEvent::new(
			KeyCode::Char('?'),
			KeyModifiers::NONE,
		);
		assert!(validate_modal_bindings(&keys)
			.unwrap_err()
			.to_string()
			.contains("reserved for end-of-list or help"));

		let mut keys = KeysList::helix();
		keys.status_reset_item = GituiKeyEvent::new(
			KeyCode::Char('2'),
			KeyModifiers::NONE,
		);
		assert!(validate_modal_bindings(&keys)
			.unwrap_err()
			.to_string()
			.contains("status item"));

		let mut keys = KeysList::helix();
		keys.branches_checkout = keys.branches_create;
		assert!(validate_modal_bindings(&keys)
			.unwrap_err()
			.to_string()
			.contains(
				"branch actions branch create and branch checkout"
			));

		let mut keys = KeysList::helix();
		keys.branches_load_more = keys.branches_create;
		assert!(validate_modal_bindings(&keys)
			.unwrap_err()
			.to_string()
			.contains(
				"branch actions branch create and load more branch commits"
			));

		assert!(validate_modal_bindings(&KeysList::helix()).is_ok());
	}

	#[cfg(not(target_os = "windows"))]
	#[test]
	fn test_symbolic_links() {
		let dir = tempfile::tempdir().unwrap();
		let mut temporary_key_list =
			NamedTempFile::new_in(dir.path()).unwrap();
		writeln!(
			temporary_key_list,
			r#"(move_down: Some((code: Char('j'), modifiers: "CONTROL")))"#
		)
		.unwrap();
		let mut temporary_key_symbols =
			NamedTempFile::new_in(dir.path()).unwrap();
		writeln!(temporary_key_symbols, r#"(esc: Some("Esc"))"#)
			.unwrap();

		let key_list_link =
			dir.path().join("linked_key_bindings.ron");
		let key_symbols_link =
			dir.path().join("linked_key_symbols.ron");
		let missing = KeyConfig::init(
			Some(&key_list_link),
			Some(&key_symbols_link),
		)
		.unwrap();
		assert_eq!(
			missing.keys.move_down,
			KeysList::default().move_down
		);
		assert_eq!(missing.symbols.esc, KeySymbols::default().esc);

		create_symlink(&temporary_key_symbols, &key_symbols_link)
			.unwrap();
		let linked_symbols = KeyConfig::init(
			Some(&key_list_link),
			Some(&key_symbols_link),
		)
		.unwrap();
		assert_eq!(
			linked_symbols.keys.move_down,
			KeysList::default().move_down
		);
		assert_eq!(linked_symbols.symbols.esc, "Esc");

		create_symlink(&temporary_key_list, &key_list_link).unwrap();
		let linked_both = KeyConfig::init(
			Some(&key_list_link),
			Some(&key_symbols_link),
		)
		.unwrap();
		assert_eq!(
			linked_both.keys.move_down,
			GituiKeyEvent::new(
				KeyCode::Char('j'),
				KeyModifiers::CONTROL
			)
		);
		assert_eq!(linked_both.symbols.esc, "Esc");

		fs::remove_file(&key_symbols_link).unwrap();
		let missing_symbols = KeyConfig::init(
			Some(&key_list_link),
			Some(&key_symbols_link),
		)
		.unwrap();
		assert_eq!(
			missing_symbols.keys.move_down,
			GituiKeyEvent::new(
				KeyCode::Char('j'),
				KeyModifiers::CONTROL
			)
		);
		assert_eq!(
			missing_symbols.symbols.esc,
			KeySymbols::default().esc
		);
	}

	#[cfg(not(target_os = "windows"))]
	fn create_symlink<
		P: AsRef<std::path::Path>,
		Q: AsRef<std::path::Path>,
	>(
		original: P,
		link: Q,
	) -> Result<(), std::io::Error> {
		std::os::unix::fs::symlink(original, link)
	}

	#[cfg(target_os = "windows")]
	fn create_symlink<
		P: AsRef<std::path::Path>,
		Q: AsRef<std::path::Path>,
	>(
		original: P,
		link: Q,
	) -> Result<(), std::io::Error> {
		std::os::windows::fs::symlink_file(original, link)
	}
}
