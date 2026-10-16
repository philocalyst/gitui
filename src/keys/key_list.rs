use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};
use std::{fs::File, path::PathBuf};
use struct_patch::traits::Patch as PatchTrait;
use struct_patch::Patch;

#[derive(Debug, PartialOrd, Clone, Copy, Serialize, Deserialize)]
pub struct GituiKeyEvent {
	pub code: KeyCode,
	pub modifiers: KeyModifiers,
}

impl GituiKeyEvent {
	pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
		Self { code, modifiers }
	}
}

pub fn key_match(ev: &KeyEvent, binding: GituiKeyEvent) -> bool {
	fn effective_modifiers(
		code: KeyCode,
		mut modifiers: KeyModifiers,
	) -> KeyModifiers {
		// Terminals disagree about reporting Shift once the character already
		// contains it (e.g. 'D', ':', '%'). Keep Ctrl/Alt/Super significant.
		if matches!(code, KeyCode::Char(c) if c.is_uppercase() || c.is_ascii_punctuation())
		{
			modifiers.remove(KeyModifiers::SHIFT);
		}
		modifiers
	}
	ev.code == binding.code
		&& effective_modifiers(ev.code, ev.modifiers)
			== effective_modifiers(binding.code, binding.modifiers)
}

impl PartialEq for GituiKeyEvent {
	fn eq(&self, other: &Self) -> bool {
		let ev: KeyEvent = self.into();
		let other: KeyEvent = other.into();
		ev == other
	}
}

impl From<&GituiKeyEvent> for KeyEvent {
	fn from(other: &GituiKeyEvent) -> Self {
		Self::new(other.code, other.modifiers)
	}
}

#[derive(Debug, Clone, Patch)]
#[patch(attribute(derive(Deserialize, Debug)))]
pub struct KeysList {
	pub select_mode: GituiKeyEvent,
	pub select_clear: GituiKeyEvent,
	pub select_all: GituiKeyEvent,
	pub select_item: GituiKeyEvent,
	pub focus_left: GituiKeyEvent,
	pub focus_right: GituiKeyEvent,
	pub focus_up: GituiKeyEvent,
	pub focus_down: GituiKeyEvent,
	pub focus_next: GituiKeyEvent,
	pub focus_previous: GituiKeyEvent,
	pub goto_prefix: GituiKeyEvent,
	pub action_prefix: GituiKeyEvent,
	pub window_prefix: GituiKeyEvent,
	pub command_palette: GituiKeyEvent,
	pub diff_view_toggle: GituiKeyEvent,
	pub diff_zoom_toggle: GituiKeyEvent,
	pub tab_status: GituiKeyEvent,
	pub tab_log: GituiKeyEvent,
	pub tab_files: GituiKeyEvent,
	pub tab_stashing: GituiKeyEvent,
	pub tab_stashes: GituiKeyEvent,
	pub tab_branches: GituiKeyEvent,
	pub tab_toggle: GituiKeyEvent,
	pub tab_toggle_reverse: GituiKeyEvent,
	pub toggle_workarea: GituiKeyEvent,
	pub exit: GituiKeyEvent,
	pub quit: GituiKeyEvent,
	pub exit_popup: GituiKeyEvent,
	pub open_commit: GituiKeyEvent,
	pub open_commit_editor: GituiKeyEvent,
	pub open_commit_inline: GituiKeyEvent,
	pub open_help: GituiKeyEvent,
	pub open_options: GituiKeyEvent,
	pub move_left: GituiKeyEvent,
	pub move_right: GituiKeyEvent,
	pub move_up: GituiKeyEvent,
	pub move_down: GituiKeyEvent,
	pub tree_collapse_recursive: GituiKeyEvent,
	pub tree_expand_recursive: GituiKeyEvent,
	pub home: GituiKeyEvent,
	pub end: GituiKeyEvent,
	pub popup_up: GituiKeyEvent,
	pub popup_down: GituiKeyEvent,
	pub page_down: GituiKeyEvent,
	pub page_up: GituiKeyEvent,
	pub shift_up: GituiKeyEvent,
	pub shift_down: GituiKeyEvent,
	pub enter: GituiKeyEvent,
	pub blame: GituiKeyEvent,
	pub file_history: GituiKeyEvent,
	pub edit_file: GituiKeyEvent,
	pub status_stage_all: GituiKeyEvent,
	pub status_unstage_item: GituiKeyEvent,
	pub status_reset_item: GituiKeyEvent,
	pub status_ignore_file: GituiKeyEvent,
	pub diff_stage_lines: GituiKeyEvent,
	pub diff_reset_lines: GituiKeyEvent,
	pub stashing_save: GituiKeyEvent,
	pub stashing_toggle_untracked: GituiKeyEvent,
	pub stashing_toggle_index: GituiKeyEvent,
	pub stash_apply: GituiKeyEvent,
	pub stash_open: GituiKeyEvent,
	pub stash_drop: GituiKeyEvent,
	pub cmd_bar_toggle: GituiKeyEvent,
	pub log_tag_commit: GituiKeyEvent,
	pub log_mark_commit: GituiKeyEvent,
	pub log_checkout_commit: GituiKeyEvent,
	pub log_reset_commit: GituiKeyEvent,
	pub log_reword_commit: GituiKeyEvent,
	pub log_rewrite_from: GituiKeyEvent,
	pub log_find: GituiKeyEvent,
	pub find_commit_sha: GituiKeyEvent,
	pub commit_amend: GituiKeyEvent,
	pub toggle_signoff: GituiKeyEvent,
	pub toggle_verify: GituiKeyEvent,
	pub copy: GituiKeyEvent,
	pub create_branch: GituiKeyEvent,
	pub rename_branch: GituiKeyEvent,
	pub select_branch: GituiKeyEvent,
	pub branches_create: GituiKeyEvent,
	pub branches_checkout: GituiKeyEvent,
	pub branches_set_base: GituiKeyEvent,
	pub branches_load_more: GituiKeyEvent,
	pub delete_branch: GituiKeyEvent,
	pub merge_branch: GituiKeyEvent,
	pub rebase_branch: GituiKeyEvent,
	pub reset_branch: GituiKeyEvent,
	pub compare_commits: GituiKeyEvent,
	pub tags: GituiKeyEvent,
	pub delete_tag: GituiKeyEvent,
	pub select_tag: GituiKeyEvent,
	pub push: GituiKeyEvent,
	pub open_file_tree: GituiKeyEvent,
	pub file_find: GituiKeyEvent,
	pub branch_find: GituiKeyEvent,
	pub force_push: GituiKeyEvent,
	pub fetch: GituiKeyEvent,
	pub pull: GituiKeyEvent,
	pub abort_merge: GituiKeyEvent,
	pub undo_commit: GituiKeyEvent,
	pub diff_hunk_next: GituiKeyEvent,
	pub diff_hunk_prev: GituiKeyEvent,
	pub stage_unstage_item: GituiKeyEvent,
	pub tag_annotate: GituiKeyEvent,
	pub view_submodules: GituiKeyEvent,
	pub view_remotes: GituiKeyEvent,
	pub update_remote_name: GituiKeyEvent,
	pub update_remote_url: GituiKeyEvent,
	pub add_remote: GituiKeyEvent,
	pub delete_remote: GituiKeyEvent,
	pub view_submodule_parent: GituiKeyEvent,
	pub update_submodule: GituiKeyEvent,
	pub commit_history_next: GituiKeyEvent,
	pub commit: GituiKeyEvent,
	pub newline: GituiKeyEvent,
	pub goto_line: GituiKeyEvent,
	pub log_toggle_graph: GituiKeyEvent,
	pub tab_rebase: GituiKeyEvent,
	pub rebase_pick: GituiKeyEvent,
	pub rebase_reword: GituiKeyEvent,
	pub rebase_edit_stop: GituiKeyEvent,
	pub rebase_squash: GituiKeyEvent,
	pub rebase_fixup: GituiKeyEvent,
	pub rebase_drop: GituiKeyEvent,
	pub rebase_break: GituiKeyEvent,
	pub rebase_exec: GituiKeyEvent,
	pub rebase_undo: GituiKeyEvent,
	pub rebase_redo: GituiKeyEvent,
	pub rebase_compose: GituiKeyEvent,
	pub rebase_start: GituiKeyEvent,
	pub rebase_skip: GituiKeyEvent,
	pub rebase_keep: GituiKeyEvent,
	pub rebase_abort: GituiKeyEvent,
	pub rebase_inspector: GituiKeyEvent,
}

#[rustfmt::skip]
impl Default for KeysList {
	// v1 of the rebase workspace pushed this over clippy's
	// 100-line bar; the table is intentionally one line per key.
	#[allow(clippy::too_many_lines)]
	fn default() -> Self {
		Self {
			select_mode: GituiKeyEvent::new(KeyCode::Char('v'), KeyModifiers::empty()),
			select_clear: GituiKeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
			select_all: GituiKeyEvent::new(KeyCode::Char('%'), KeyModifiers::empty()),
			select_item: GituiKeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()),
			focus_left: GituiKeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
			focus_right: GituiKeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
			focus_up: GituiKeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
			focus_down: GituiKeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
			focus_next: GituiKeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
			focus_previous: GituiKeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
			goto_prefix: GituiKeyEvent::new(KeyCode::Char('g'), KeyModifiers::empty()),
			action_prefix: GituiKeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
			window_prefix: GituiKeyEvent::new(KeyCode::Null, KeyModifiers::NONE),
			command_palette: GituiKeyEvent::new(KeyCode::Char(':'), KeyModifiers::empty()),
			diff_view_toggle: GituiKeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
			diff_zoom_toggle: GituiKeyEvent::new(KeyCode::Char('z'), KeyModifiers::empty()),
			tab_status: GituiKeyEvent::new(KeyCode::Char('1'), KeyModifiers::empty()),
			tab_log: GituiKeyEvent::new(KeyCode::Char('2'),  KeyModifiers::empty()),
			tab_files: GituiKeyEvent::new(KeyCode::Char('3'),  KeyModifiers::empty()),
			tab_stashing: GituiKeyEvent::new(KeyCode::Char('4'),  KeyModifiers::empty()),
			tab_stashes: GituiKeyEvent::new(KeyCode::Char('5'),  KeyModifiers::empty()),
			tab_branches: GituiKeyEvent::new(KeyCode::Char('7'),  KeyModifiers::empty()),
			tab_toggle: GituiKeyEvent::new(KeyCode::Tab,  KeyModifiers::empty()),
			tab_toggle_reverse: GituiKeyEvent::new(KeyCode::BackTab,  KeyModifiers::SHIFT),
			toggle_workarea: GituiKeyEvent::new(KeyCode::Char('w'),  KeyModifiers::empty()),
			exit: GituiKeyEvent::new(KeyCode::Char('c'),  KeyModifiers::CONTROL),
			quit: GituiKeyEvent::new(KeyCode::Char('q'),  KeyModifiers::empty()),
			exit_popup: GituiKeyEvent::new(KeyCode::Esc,  KeyModifiers::empty()),
			open_commit: GituiKeyEvent::new(KeyCode::Char('c'),  KeyModifiers::empty()),
			open_commit_editor: GituiKeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
			open_commit_inline: GituiKeyEvent::new(KeyCode::Null, KeyModifiers::NONE),
			open_help: GituiKeyEvent::new(KeyCode::Char('h'),  KeyModifiers::empty()),
			open_options: GituiKeyEvent::new(KeyCode::Char('o'),  KeyModifiers::empty()),
			move_left: GituiKeyEvent::new(KeyCode::Left,  KeyModifiers::empty()),
			move_right: GituiKeyEvent::new(KeyCode::Right,  KeyModifiers::empty()),
			tree_collapse_recursive: GituiKeyEvent::new(KeyCode::Left,  KeyModifiers::SHIFT),
			tree_expand_recursive: GituiKeyEvent::new(KeyCode::Right,  KeyModifiers::SHIFT),
			home: GituiKeyEvent::new(KeyCode::Home,  KeyModifiers::empty()),
			end: GituiKeyEvent::new(KeyCode::End,  KeyModifiers::empty()),
			move_up: GituiKeyEvent::new(KeyCode::Up,  KeyModifiers::empty()),
			move_down: GituiKeyEvent::new(KeyCode::Down,  KeyModifiers::empty()),
			popup_up: GituiKeyEvent::new(KeyCode::Up,  KeyModifiers::empty()),
			popup_down: GituiKeyEvent::new(KeyCode::Down,  KeyModifiers::empty()),
			page_down: GituiKeyEvent::new(KeyCode::PageDown,  KeyModifiers::empty()),
			page_up: GituiKeyEvent::new(KeyCode::PageUp,  KeyModifiers::empty()),
			shift_up: GituiKeyEvent::new(KeyCode::Up,  KeyModifiers::SHIFT),
			shift_down: GituiKeyEvent::new(KeyCode::Down,  KeyModifiers::SHIFT),
			enter: GituiKeyEvent::new(KeyCode::Enter,  KeyModifiers::empty()),
			blame: GituiKeyEvent::new(KeyCode::Char('B'),  KeyModifiers::SHIFT),
			file_history: GituiKeyEvent::new(KeyCode::Char('H'),  KeyModifiers::SHIFT),
			edit_file: GituiKeyEvent::new(KeyCode::Char('e'),  KeyModifiers::empty()),
			status_stage_all: GituiKeyEvent::new(KeyCode::Char('a'),  KeyModifiers::empty()),
			status_unstage_item: GituiKeyEvent::new(KeyCode::Null, KeyModifiers::NONE),
			status_reset_item: GituiKeyEvent::new(KeyCode::Char('D'),  KeyModifiers::SHIFT),
			diff_reset_lines: GituiKeyEvent::new(KeyCode::Char('d'),  KeyModifiers::empty()),
			status_ignore_file: GituiKeyEvent::new(KeyCode::Char('i'),  KeyModifiers::empty()),
			diff_stage_lines: GituiKeyEvent::new(KeyCode::Char('s'),  KeyModifiers::empty()),
			stashing_save: GituiKeyEvent::new(KeyCode::Char('s'),  KeyModifiers::empty()),
			stashing_toggle_untracked: GituiKeyEvent::new(KeyCode::Char('u'),  KeyModifiers::empty()),
			stashing_toggle_index: GituiKeyEvent::new(KeyCode::Char('i'),  KeyModifiers::empty()),
			stash_apply: GituiKeyEvent::new(KeyCode::Char('a'),  KeyModifiers::empty()),
			stash_open: GituiKeyEvent::new(KeyCode::Right,  KeyModifiers::empty()),
			stash_drop: GituiKeyEvent::new(KeyCode::Char('D'),  KeyModifiers::SHIFT),
			cmd_bar_toggle: GituiKeyEvent::new(KeyCode::Char('.'),  KeyModifiers::empty()),
			log_tag_commit: GituiKeyEvent::new(KeyCode::Char('t'),  KeyModifiers::empty()),
			log_mark_commit: GituiKeyEvent::new(KeyCode::Char(' '),  KeyModifiers::empty()),
			log_checkout_commit: GituiKeyEvent { code: KeyCode::Char('S'), modifiers: KeyModifiers::SHIFT },
			log_reset_commit: GituiKeyEvent { code: KeyCode::Char('R'), modifiers: KeyModifiers::SHIFT },
			log_reword_commit: GituiKeyEvent { code: KeyCode::Char('r'), modifiers: KeyModifiers::empty() },
			log_rewrite_from: GituiKeyEvent { code: KeyCode::Char('W'), modifiers: KeyModifiers::empty() },
			log_find: GituiKeyEvent { code: KeyCode::Char('f'), modifiers: KeyModifiers::empty() },
			find_commit_sha: GituiKeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
			commit_amend: GituiKeyEvent::new(KeyCode::Char('a'),  KeyModifiers::CONTROL),
			toggle_signoff: GituiKeyEvent::new(KeyCode::Char('s'),  KeyModifiers::CONTROL),
			toggle_verify: GituiKeyEvent::new(KeyCode::Char('f'),  KeyModifiers::CONTROL),
			copy: GituiKeyEvent::new(KeyCode::Char('y'),  KeyModifiers::empty()),
			create_branch: GituiKeyEvent::new(KeyCode::Char('c'),  KeyModifiers::empty()),
			rename_branch: GituiKeyEvent::new(KeyCode::Char('r'),  KeyModifiers::empty()),
			select_branch: GituiKeyEvent::new(KeyCode::Char('b'),  KeyModifiers::empty()),
			branches_create: GituiKeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
			branches_checkout: GituiKeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
			branches_set_base: GituiKeyEvent::new(KeyCode::Char('b'),  KeyModifiers::empty()),
			branches_load_more: GituiKeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE),
			delete_branch: GituiKeyEvent::new(KeyCode::Char('D'),  KeyModifiers::SHIFT),
			merge_branch: GituiKeyEvent::new(KeyCode::Char('m'),  KeyModifiers::empty()),
			rebase_branch: GituiKeyEvent::new(KeyCode::Char('R'),  KeyModifiers::SHIFT),
			reset_branch: GituiKeyEvent::new(KeyCode::Char('s'),  KeyModifiers::empty()),
			compare_commits: GituiKeyEvent::new(KeyCode::Char('C'),  KeyModifiers::SHIFT),
			tags: GituiKeyEvent::new(KeyCode::Char('T'),  KeyModifiers::SHIFT),
			delete_tag: GituiKeyEvent::new(KeyCode::Char('D'),  KeyModifiers::SHIFT),
			select_tag: GituiKeyEvent::new(KeyCode::Enter,  KeyModifiers::empty()),
			push: GituiKeyEvent::new(KeyCode::Char('p'),  KeyModifiers::empty()),
			force_push: GituiKeyEvent::new(KeyCode::Char('P'),  KeyModifiers::SHIFT),
			undo_commit: GituiKeyEvent::new(KeyCode::Char('U'),  KeyModifiers::SHIFT),
			fetch: GituiKeyEvent::new(KeyCode::Char('F'),  KeyModifiers::SHIFT),
			pull: GituiKeyEvent::new(KeyCode::Char('f'),  KeyModifiers::empty()),
			abort_merge: GituiKeyEvent::new(KeyCode::Char('A'),  KeyModifiers::SHIFT),
			open_file_tree: GituiKeyEvent::new(KeyCode::Char('F'),  KeyModifiers::SHIFT),
			file_find: GituiKeyEvent::new(KeyCode::Char('f'),  KeyModifiers::empty()),
			branch_find: GituiKeyEvent::new(KeyCode::Char('f'),  KeyModifiers::empty()),
			diff_hunk_next: GituiKeyEvent::new(KeyCode::Char('n'),  KeyModifiers::empty()),
			diff_hunk_prev: GituiKeyEvent::new(KeyCode::Char('p'),  KeyModifiers::empty()),
			stage_unstage_item: GituiKeyEvent::new(KeyCode::Enter,  KeyModifiers::empty()),
			tag_annotate: GituiKeyEvent::new(KeyCode::Char('a'),  KeyModifiers::CONTROL),
			view_submodules: GituiKeyEvent::new(KeyCode::Char('S'),  KeyModifiers::SHIFT),
			view_remotes: GituiKeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
			update_remote_name: GituiKeyEvent::new(KeyCode::Char('n'),KeyModifiers::NONE),
			update_remote_url: GituiKeyEvent::new(KeyCode::Char('u'),KeyModifiers::NONE),
			add_remote: GituiKeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
			delete_remote: GituiKeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
			view_submodule_parent: GituiKeyEvent::new(KeyCode::Char('p'),  KeyModifiers::empty()),
			update_submodule: GituiKeyEvent::new(KeyCode::Char('u'),  KeyModifiers::empty()),
			commit_history_next: GituiKeyEvent::new(KeyCode::Char('n'),  KeyModifiers::CONTROL),
			commit: GituiKeyEvent::new(KeyCode::Char('d'),  KeyModifiers::CONTROL),
			newline: GituiKeyEvent::new(KeyCode::Enter,  KeyModifiers::empty()),
			goto_line: GituiKeyEvent::new(KeyCode::Char('L'),  KeyModifiers::SHIFT),
			log_toggle_graph: GituiKeyEvent::new(KeyCode::Char('g'),  KeyModifiers::empty()),
			tab_rebase: GituiKeyEvent::new(KeyCode::Char('6'),  KeyModifiers::empty()),
			rebase_pick: GituiKeyEvent::new(KeyCode::Char('p'),  KeyModifiers::empty()),
			rebase_reword: GituiKeyEvent::new(KeyCode::Char('r'),  KeyModifiers::empty()),
			rebase_edit_stop: GituiKeyEvent::new(KeyCode::Char('e'),  KeyModifiers::empty()),
			rebase_squash: GituiKeyEvent::new(KeyCode::Char('s'),  KeyModifiers::empty()),
			rebase_fixup: GituiKeyEvent::new(KeyCode::Char('f'),  KeyModifiers::empty()),
			rebase_drop: GituiKeyEvent::new(KeyCode::Char('d'),  KeyModifiers::empty()),
			rebase_break: GituiKeyEvent::new(KeyCode::Char('b'),  KeyModifiers::empty()),
			rebase_exec: GituiKeyEvent::new(KeyCode::Char('x'),  KeyModifiers::empty()),
			rebase_undo: GituiKeyEvent::new(KeyCode::Char('u'),  KeyModifiers::empty()),
			rebase_redo: GituiKeyEvent::new(KeyCode::Char('U'),  KeyModifiers::SHIFT),
			rebase_compose: GituiKeyEvent::new(KeyCode::Char('m'),  KeyModifiers::empty()),
			rebase_start: GituiKeyEvent::new(KeyCode::Char('S'),  KeyModifiers::SHIFT),
			rebase_skip: GituiKeyEvent::new(KeyCode::Char('k'),  KeyModifiers::empty()),
			rebase_keep: GituiKeyEvent::new(KeyCode::Char('K'),  KeyModifiers::SHIFT),
			rebase_abort: GituiKeyEvent::new(KeyCode::Char('A'),  KeyModifiers::empty()),
			rebase_inspector: GituiKeyEvent::new(KeyCode::Char('v'),  KeyModifiers::empty()),
		}
	}
}

impl KeysList {
	/// Selection-first profile. Action keys stay configurable through RON patches.
	pub fn helix() -> Self {
		let plain = |c| {
			GituiKeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
		};
		let ctrl = |c| {
			GituiKeyEvent::new(
				KeyCode::Char(c),
				KeyModifiers::CONTROL,
			)
		};
		let shifted = |c| {
			GituiKeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
		};
		Self {
			window_prefix: ctrl('w'),
			move_left: plain('h'),
			move_down: plain('j'),
			move_up: plain('k'),
			move_right: plain('l'),
			open_help: GituiKeyEvent::new(
				KeyCode::F(1),
				KeyModifiers::NONE,
			),
			popup_up: ctrl('p'),
			popup_down: ctrl('n'),
			page_up: ctrl('b'),
			page_down: ctrl('f'),
			shift_up: shifted('K'),
			shift_down: shifted('J'),
			tab_toggle: GituiKeyEvent::new(
				KeyCode::Char('l'),
				KeyModifiers::ALT,
			),
			tab_toggle_reverse: GituiKeyEvent::new(
				KeyCode::Char('h'),
				KeyModifiers::ALT,
			),
			status_stage_all: shifted('A'),
			stage_unstage_item: plain('s'),
			status_reset_item: shifted('D'),
			diff_reset_lines: shifted('D'),
			diff_stage_lines: GituiKeyEvent::new(
				KeyCode::Null,
				KeyModifiers::NONE,
			),
			open_commit_inline: shifted('C'),
			status_unstage_item: plain('u'),
			branches_checkout: plain('c'),
			branches_load_more: plain('+'),
			reset_branch: shifted('R'),
			rebase_drop: shifted('D'),
			undo_commit: ctrl('z'),
			log_mark_commit: plain('x'),
			log_toggle_graph: ctrl('g'),
			abort_merge: GituiKeyEvent::new(
				KeyCode::Char('a'),
				KeyModifiers::ALT,
			),
			log_find: plain('/'),
			file_find: plain('/'),
			branch_find: plain('/'),
			rebase_skip: ctrl('s'),
			rebase_keep: GituiKeyEvent::new(
				KeyCode::Char('k'),
				KeyModifiers::ALT,
			),
			rebase_inspector: plain('i'),
			rebase_exec: ctrl('x'),
			..Self::default()
		}
	}

	#[cfg(test)]
	pub fn init(file: PathBuf) -> Self {
		Self::load(Self::default(), file)
	}

	#[cfg(test)]
	pub fn load(keys_list: Self, file: PathBuf) -> Self {
		Self::try_load(keys_list.clone(), file).unwrap_or_else(
			|error| {
				log::error!("{error:#}");
				keys_list
			},
		)
	}

	/// Apply an optional RON patch, preserving profile defaults when no file
	/// exists and returning malformed or inaccessible explicit config to caller.
	pub fn try_load(
		mut keys_list: Self,
		file: PathBuf,
	) -> Result<Self> {
		let file_display = file.display().to_string();
		let f = match File::open(file) {
			Ok(file) => file,
			Err(error)
				if error.kind() == std::io::ErrorKind::NotFound =>
			{
				return Ok(keys_list);
			}
			Err(error) => {
				return Err(error).with_context(|| {
					format!(
						"failed to open key bindings file {file_display}"
					)
				});
			}
		};
		let patch = ron::de::from_reader(f).with_context(|| {
			format!(
				"failed to parse key bindings file {file_display}"
			)
		})?;
		keys_list.apply(patch);
		Ok(keys_list)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use pretty_assertions::assert_eq;
	use std::io::Write;
	use tempfile::NamedTempFile;

	#[test]
	fn helix_stage_unstage_and_inline_commit_defaults_are_contextual()
	{
		let legacy = KeysList::default();
		let helix = KeysList::helix();
		assert_eq!(
			legacy.stage_unstage_item,
			GituiKeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
		);
		assert_eq!(
			helix.stage_unstage_item,
			GituiKeyEvent::new(
				KeyCode::Char('s'),
				KeyModifiers::NONE
			)
		);
		assert_eq!(
			helix.status_unstage_item,
			GituiKeyEvent::new(
				KeyCode::Char('u'),
				KeyModifiers::NONE
			)
		);
		assert_eq!(
			helix.open_commit_inline,
			GituiKeyEvent::new(
				KeyCode::Char('C'),
				KeyModifiers::SHIFT
			)
		);
		assert_eq!(
			helix.branches_checkout,
			GituiKeyEvent::new(
				KeyCode::Char('c'),
				KeyModifiers::NONE
			)
		);
		assert_eq!(
			helix.branches_create,
			GituiKeyEvent::new(
				KeyCode::Char('n'),
				KeyModifiers::NONE
			)
		);
		assert_eq!(
			helix.branches_load_more,
			GituiKeyEvent::new(
				KeyCode::Char('+'),
				KeyModifiers::NONE
			)
		);
		assert_eq!(
			legacy.branches_load_more,
			GituiKeyEvent::new(
				KeyCode::Char('+'),
				KeyModifiers::NONE
			)
		);
		assert_eq!(
			legacy.create_branch,
			GituiKeyEvent::new(
				KeyCode::Char('c'),
				KeyModifiers::NONE
			)
		);
		assert_eq!(
			KeysList::default().branches_checkout,
			GituiKeyEvent::new(
				KeyCode::Char('c'),
				KeyModifiers::NONE
			)
		);
		for binding in [
			helix.status_reset_item,
			helix.diff_reset_lines,
			helix.reset_branch,
			helix.rebase_drop,
		] {
			assert!(matches!(binding.code, KeyCode::Char('D' | 'R')));
			assert!(binding.modifiers.contains(KeyModifiers::SHIFT));
		}
	}

	#[test]
	fn test_apply_vim_style_example() {
		let mut keys_list = KeysList::default();
		let f = File::open("vim_style_key_config.ron")
			.expect("vim style config should exist");
		let patch = ron::de::from_reader(f)
			.expect("vim style config format incorrect");
		keys_list.apply(patch);
	}

	#[test]
	fn test_smoke() {
		let mut file = NamedTempFile::new().unwrap();

		writeln!(
			file,
			r#"
(
	move_down: Some(( code: Char('j'), modifiers: "CONTROL")),
	move_up: Some((code: Char('h'), modifiers: ""))
)
"#
		)
		.unwrap();

		let keys = KeysList::init(file.path().to_path_buf());

		assert_eq!(keys.move_right, KeysList::default().move_right);
		assert_eq!(
			keys.move_down,
			GituiKeyEvent::new(
				KeyCode::Char('j'),
				KeyModifiers::CONTROL
			)
		);
		assert_eq!(
			keys.move_up,
			GituiKeyEvent::new(
				KeyCode::Char('h'),
				KeyModifiers::NONE
			)
		);
	}

	#[test]
	fn profile_patch_preserves_defaults_and_invalid_ron_is_reported()
	{
		let mut file = NamedTempFile::new().unwrap();
		writeln!(
			file,
			r#"(move_down: Some((code: Char('n'), modifiers: "")))"#
		)
		.unwrap();
		let keys = KeysList::try_load(
			KeysList::helix(),
			file.path().to_path_buf(),
		)
		.unwrap();
		assert_eq!(
			keys.move_down,
			GituiKeyEvent::new(
				KeyCode::Char('n'),
				KeyModifiers::NONE
			)
		);
		assert_eq!(
			keys.move_left,
			GituiKeyEvent::new(
				KeyCode::Char('h'),
				KeyModifiers::NONE
			)
		);
		let helix_defaults = KeysList::helix();
		assert_eq!(
			keys.branches_create,
			helix_defaults.branches_create
		);
		assert_eq!(
			keys.branches_checkout,
			helix_defaults.branches_checkout
		);
		assert_eq!(
			keys.branches_load_more,
			helix_defaults.branches_load_more
		);
		assert_eq!(keys.select_branch, helix_defaults.select_branch);

		let legacy = KeysList::try_load(
			KeysList::default(),
			file.path().to_path_buf(),
		)
		.unwrap();
		assert_eq!(
			legacy.create_branch,
			KeysList::default().create_branch
		);
		assert_eq!(
			legacy.select_branch,
			KeysList::default().select_branch
		);

		let mut invalid = NamedTempFile::new().unwrap();
		writeln!(invalid, "not RON").unwrap();
		assert!(KeysList::try_load(
			KeysList::helix(),
			invalid.path().to_path_buf(),
		)
		.is_err());
	}

	#[test]
	fn key_match_accepts_shift_in_character_or_modifier_for_terminal_variants(
	) {
		let shifted_letter = GituiKeyEvent::new(
			KeyCode::Char('A'),
			KeyModifiers::SHIFT,
		);
		assert!(key_match(
			&KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE),
			shifted_letter,
		));
		assert!(key_match(
			&KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
			shifted_letter,
		));

		let punctuation = GituiKeyEvent::new(
			KeyCode::Char('%'),
			KeyModifiers::NONE,
		);
		assert!(key_match(
			&KeyEvent::new(KeyCode::Char('%'), KeyModifiers::SHIFT),
			punctuation,
		));
		assert!(!key_match(
			&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT),
			GituiKeyEvent::new(
				KeyCode::Char('a'),
				KeyModifiers::NONE
			),
		));
	}
}
