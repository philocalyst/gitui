use std::borrow::Cow;

use asyncgit::sync::CommitId;
use unicode_truncate::UnicodeTruncateStr;
use unicode_width::UnicodeWidthStr;

use crate::keys::SharedKeyConfig;

pub mod order {
	pub const RARE_ACTION: i8 = 30;
	pub const NAV: i8 = 20;
	pub const AVERAGE: i8 = 10;
	pub const PRIORITY: i8 = 1;
}

pub static PUSH_POPUP_MSG: &str = "Push";
pub static FORCE_PUSH_POPUP_MSG: &str = "Force Push";
pub static PULL_POPUP_MSG: &str = "Pull";
pub static FETCH_POPUP_MSG: &str = "Fetch";
pub static PUSH_POPUP_PROGRESS_NONE: &str = "preparing...";
pub static PUSH_POPUP_STATES_ADDING: &str = "adding objects (1/3)";
pub static PUSH_POPUP_STATES_DELTAS: &str = "deltas (2/3)";
pub static PUSH_POPUP_STATES_PUSHING: &str = "pushing (3/3)";
pub static PUSH_POPUP_STATES_TRANSFER: &str = "transfer";
pub static PUSH_POPUP_STATES_DONE: &str = "done";

pub static PUSH_TAGS_POPUP_MSG: &str = "Push Tags";
pub static PUSH_TAGS_STATES_FETCHING: &str = "fetching";
pub static PUSH_TAGS_STATES_PUSHING: &str = "pushing";
pub static PUSH_TAGS_STATES_DONE: &str = "done";

pub static POPUP_TITLE_SUBMODULES: &str = "Submodules";
pub static POPUP_TITLE_REMOTES: &str = "Remotes";
pub static POPUP_SUBTITLE_REMOTES: &str = "Details";
pub static POPUP_TITLE_FUZZY_FIND: &str = "Fuzzy Finder";
pub static POPUP_TITLE_LOG_SEARCH: &str = "Search";

pub static POPUP_FAIL_COPY: &str = "Failed to copy text";
pub static POPUP_SUCCESS_COPY: &str = "Copied Text";
pub static POPUP_COMMIT_SHA_INVALID: &str = "Invalid commit sha";

pub mod symbol {
	pub const CHECKMARK: &str = "\u{2713}"; //✓
	pub const SPACE: &str = "\u{02FD}"; //˽
	pub const EMPTY_SPACE: &str = " ";
	pub const FOLDER_ICON_COLLAPSED: &str = "\u{25b8}"; //▸
	pub const FOLDER_ICON_EXPANDED: &str = "\u{25be}"; //▾
	pub const EMPTY_STR: &str = "";
	pub const ELLIPSIS: char = '\u{2026}'; // …
}

pub fn title_branches() -> String {
	"Branches".to_string()
}
pub fn title_tags() -> String {
	"Tags".to_string()
}
pub fn title_status(_key_config: &SharedKeyConfig) -> String {
	"Unstaged Changes".to_string()
}
pub fn title_diff(_key_config: &SharedKeyConfig) -> String {
	"Diff: ".to_string()
}
pub fn title_index(_key_config: &SharedKeyConfig) -> String {
	"Staged Changes".to_string()
}
pub fn tab_status(key_config: &SharedKeyConfig) -> String {
	format!(
		"Status [{}]",
		key_config.get_hint(key_config.keys.tab_status)
	)
}
pub fn tab_log(key_config: &SharedKeyConfig) -> String {
	format!("Log [{}]", key_config.get_hint(key_config.keys.tab_log))
}
pub fn tab_files(key_config: &SharedKeyConfig) -> String {
	format!(
		"Files [{}]",
		key_config.get_hint(key_config.keys.tab_files)
	)
}
pub fn tab_stashing(key_config: &SharedKeyConfig) -> String {
	format!(
		"Stashing [{}]",
		key_config.get_hint(key_config.keys.tab_stashing)
	)
}
pub fn tab_stashes(key_config: &SharedKeyConfig) -> String {
	format!(
		"Stashes [{}]",
		key_config.get_hint(key_config.keys.tab_stashes)
	)
}
pub fn tab_rebase(key_config: &SharedKeyConfig) -> String {
	format!(
		"Rebase [{}]",
		key_config.get_hint(key_config.keys.tab_rebase)
	)
}
pub fn tab_branches(key_config: &SharedKeyConfig) -> String {
	format!(
		"Branches [{}]",
		key_config.get_hint(key_config.keys.tab_branches)
	)
}
pub fn tab_divider(_key_config: &SharedKeyConfig) -> String {
	" | ".to_string()
}
pub fn cmd_splitter(_key_config: &SharedKeyConfig) -> String {
	" ".to_string()
}
pub fn msg_opening_editor(_key_config: &SharedKeyConfig) -> String {
	"opening editor...".to_string()
}
pub fn msg_title_error(_key_config: &SharedKeyConfig) -> String {
	"Error".to_string()
}
pub fn msg_title_info(_key_config: &SharedKeyConfig) -> String {
	"Info".to_string()
}
pub fn commit_title() -> String {
	"Commit".to_string()
}
pub fn commit_reword_title() -> String {
	"Reword Commit".to_string()
}

pub fn commit_title_merge() -> String {
	"Commit (Merge)".to_string()
}
pub fn commit_title_revert() -> String {
	"Commit (Revert)".to_string()
}
pub fn commit_title_amend() -> String {
	"Commit (Amend)".to_string()
}
pub fn commit_msg(_key_config: &SharedKeyConfig) -> String {
	"type commit message..".to_string()
}
pub fn commit_first_line_warning(count: usize) -> String {
	format!("[subject length: {count}]")
}
pub const fn branch_name_invalid() -> &'static str {
	"[invalid name]"
}
pub fn commit_editor_msg(_key_config: &SharedKeyConfig) -> String {
	r"
# Edit your commit message
# Lines starting with '#' will be ignored"
		.to_string()
}
pub fn stash_popup_title(_key_config: &SharedKeyConfig) -> String {
	"Stash".to_string()
}
pub fn stash_popup_msg(_key_config: &SharedKeyConfig) -> String {
	"type name (optional)".to_string()
}
pub fn confirm_title_reset() -> String {
	"Reset".to_string()
}
pub fn confirm_title_undo_commit() -> String {
	"Undo commit".to_string()
}
pub fn confirm_title_stashdrop(
	_key_config: &SharedKeyConfig,
	multiple: bool,
) -> String {
	format!("Drop Stash{}", if multiple { "es" } else { "" })
}
pub fn confirm_title_stashpop(
	_key_config: &SharedKeyConfig,
) -> String {
	"Pop".to_string()
}
pub fn confirm_title_merge(
	_key_config: &SharedKeyConfig,
	rebase: bool,
) -> String {
	if rebase {
		"Merge (via rebase)".to_string()
	} else {
		"Merge (via commit)".to_string()
	}
}
pub fn confirm_msg_merge(
	_key_config: &SharedKeyConfig,
	incoming: usize,
	rebase: bool,
) -> String {
	if rebase {
		format!("Rebase onto {incoming} incoming commits?")
	} else {
		format!("Merge of {incoming} incoming commits?")
	}
}

pub fn confirm_title_abortmerge() -> String {
	"Abort merge?".to_string()
}
pub fn confirm_title_abortrevert() -> String {
	"Abort revert?".to_string()
}
pub fn confirm_msg_revertchanges() -> String {
	"This will revert all uncommitted changes. Are you sure?"
		.to_string()
}
pub fn confirm_title_abortrebase() -> String {
	"Abort rebase?".to_string()
}
pub fn confirm_msg_abortrebase() -> String {
	"This will revert all uncommitted changes. Are you sure?"
		.to_string()
}
pub fn confirm_msg_reset() -> String {
	"confirm file reset?".to_string()
}
pub fn confirm_msg_reset_lines(lines: usize) -> String {
	format!(
		"are you sure you want to discard {lines} selected lines?"
	)
}
pub fn confirm_msg_undo_commit() -> String {
	"confirm undo last commit?".to_string()
}
pub fn confirm_msg_stashdrop(
	_key_config: &SharedKeyConfig,
	ids: &[CommitId],
) -> String {
	format!(
		"Sure you want to drop following {}stash{}?\n\n{}",
		if ids.len() > 1 {
			format!("{} ", ids.len())
		} else {
			String::default()
		},
		if ids.len() > 1 { "es" } else { "" },
		ids.iter()
			.map(CommitId::get_short_string)
			.collect::<Vec<_>>()
			.join(", ")
	)
}
pub fn confirm_msg_stashpop(_key_config: &SharedKeyConfig) -> String {
	"The stash will be applied and removed from the stash list. Confirm stash pop?"
        .to_string()
}
pub fn confirm_msg_resethunk(
	_key_config: &SharedKeyConfig,
) -> String {
	"confirm reset hunk?".to_string()
}
pub fn confirm_title_delete_branch(
	_key_config: &SharedKeyConfig,
) -> String {
	"Delete Branch".to_string()
}
pub fn confirm_msg_delete_branch(
	_key_config: &SharedKeyConfig,
	branch_ref: &str,
) -> String {
	format!("Confirm deleting branch: '{branch_ref}' ?")
}
pub fn confirm_title_delete_remote_branch(
	_key_config: &SharedKeyConfig,
) -> String {
	"Delete Remote Branch".to_string()
}
pub fn confirm_title_delete_remote(
	_key_config: &SharedKeyConfig,
) -> String {
	"Delete Remote".to_string()
}
pub fn confirm_msg_delete_remote(
	_key_config: &SharedKeyConfig,
	remote_name: &str,
) -> String {
	format!("Confirm deleting remote \"{remote_name}\"")
}
pub fn confirm_msg_delete_remote_branch(
	_key_config: &SharedKeyConfig,
	branch_ref: &str,
) -> String {
	format!("Confirm deleting remote branch: '{branch_ref}' ?")
}
pub fn confirm_title_delete_tag(
	_key_config: &SharedKeyConfig,
) -> String {
	"Delete Tag".to_string()
}
pub fn confirm_msg_delete_tag(
	_key_config: &SharedKeyConfig,
	tag_name: &str,
) -> String {
	format!("Confirm deleting Tag: '{tag_name}' ?")
}
pub fn confirm_title_delete_tag_remote() -> String {
	"Delete Tag (remote)".to_string()
}
pub fn confirm_msg_delete_tag_remote(remote_name: &str) -> String {
	format!("Confirm deleting tag on remote '{remote_name}'?")
}
pub fn confirm_title_force_push(
	_key_config: &SharedKeyConfig,
) -> String {
	"Force Push".to_string()
}
pub fn confirm_msg_force_push(
	_key_config: &SharedKeyConfig,
	branch_ref: &str,
) -> String {
	format!(
        "Confirm force push to branch '{branch_ref}' ?  This may rewrite history."
    )
}
pub fn log_title(_key_config: &SharedKeyConfig) -> String {
	"Commit".to_string()
}
pub fn file_log_title(
	file_path: &str,
	selected: usize,
	revisions: usize,
) -> String {
	format!("Revisions of '{file_path}' ({selected}/{revisions})")
}
pub fn blame_title(_key_config: &SharedKeyConfig) -> String {
	"Blame".to_string()
}
pub fn tag_popup_name_title() -> String {
	"Tag".to_string()
}
pub fn tag_popup_name_msg() -> String {
	"type tag name".to_string()
}
pub fn tag_popup_annotation_title(name: &str) -> String {
	format!("Tag Annotation ({name})")
}
pub fn tag_popup_annotation_msg() -> String {
	"type tag annotation".to_string()
}
pub fn stashlist_title(_key_config: &SharedKeyConfig) -> String {
	"Stashes".to_string()
}
pub fn help_title(_key_config: &SharedKeyConfig) -> String {
	"Help: all commands".to_string()
}
pub fn stashing_files_title(_key_config: &SharedKeyConfig) -> String {
	"Files to Stash".to_string()
}
pub fn stashing_options_title(
	_key_config: &SharedKeyConfig,
) -> String {
	"Options".to_string()
}
pub fn loading_text(_key_config: &SharedKeyConfig) -> String {
	"Loading ...".to_string()
}
pub fn create_branch_popup_title(
	_key_config: &SharedKeyConfig,
) -> String {
	"Branch".to_string()
}
pub fn create_branch_popup_msg(
	_key_config: &SharedKeyConfig,
) -> String {
	"type branch name".to_string()
}
pub fn rename_remote_popup_title(
	_key_config: &SharedKeyConfig,
) -> String {
	"Rename remote".to_string()
}
pub fn rename_remote_popup_msg(
	_key_config: &SharedKeyConfig,
) -> String {
	"new remote name".to_string()
}
pub fn update_remote_url_popup_title(
	_key_config: &SharedKeyConfig,
) -> String {
	"Update url".to_string()
}
pub fn update_remote_url_popup_msg(
	_key_config: &SharedKeyConfig,
) -> String {
	"new remote url".to_string()
}
pub fn create_remote_popup_title_name(
	_key_config: &SharedKeyConfig,
) -> String {
	"Remote name".to_string()
}
pub fn create_remote_popup_title_url(
	_key_config: &SharedKeyConfig,
) -> String {
	"Remote url".to_string()
}
pub fn create_remote_popup_msg_name(
	_key_config: &SharedKeyConfig,
) -> String {
	"type remote name".to_string()
}
pub fn create_remote_popup_msg_url(
	_key_config: &SharedKeyConfig,
) -> String {
	"type remote url".to_string()
}
pub const fn remote_name_invalid() -> &'static str {
	"[invalid name]"
}
pub fn username_popup_title(_key_config: &SharedKeyConfig) -> String {
	"Username".to_string()
}
pub fn username_popup_msg(_key_config: &SharedKeyConfig) -> String {
	"type username".to_string()
}
pub fn password_popup_title(_key_config: &SharedKeyConfig) -> String {
	"Password".to_string()
}
pub fn password_popup_msg(_key_config: &SharedKeyConfig) -> String {
	"type password".to_string()
}

pub fn rename_branch_popup_title(
	_key_config: &SharedKeyConfig,
) -> String {
	"Rename Branch".to_string()
}
pub fn rename_branch_popup_msg(
	_key_config: &SharedKeyConfig,
) -> String {
	"new branch name".to_string()
}

pub fn copy_success(s: &str) -> String {
	format!("{POPUP_SUCCESS_COPY} \"{s}\"")
}

pub fn ellipsis_trim_start(s: &str, width: usize) -> Cow<'_, str> {
	if s.width() <= width {
		Cow::Borrowed(s)
	} else {
		Cow::Owned(format!(
			"[{}]{}",
			symbol::ELLIPSIS,
			s.unicode_truncate_start(
				width.saturating_sub(3 /* front indicator */)
			)
			.0
		))
	}
}

pub const fn normalize_branch_name_char(c: char) -> char {
	match c {
		' ' => '-',
		c => c,
	}
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum CheckoutOptions {
	KeepLocalChanges,
	DiscardAllLocalChagnes,
}

impl CheckoutOptions {
	pub const fn previous(self) -> Self {
		match self {
			Self::KeepLocalChanges => Self::DiscardAllLocalChagnes,
			Self::DiscardAllLocalChagnes => Self::KeepLocalChanges,
		}
	}

	pub const fn next(self) -> Self {
		match self {
			Self::KeepLocalChanges => Self::DiscardAllLocalChagnes,
			Self::DiscardAllLocalChagnes => Self::KeepLocalChanges,
		}
	}

	pub const fn to_string_pair(
		self,
	) -> (&'static str, &'static str) {
		const CHECKOUT_OPTION_UNCHANGE: &str =
			" 🟡 Keep local changes";
		const CHECKOUT_OPTION_DISCARD: &str =
			" 🔴 Discard all local changes";

		match self {
			Self::KeepLocalChanges => {
				("Don't change", CHECKOUT_OPTION_UNCHANGE)
			}
			Self::DiscardAllLocalChagnes => {
				("Discard", CHECKOUT_OPTION_DISCARD)
			}
		}
	}
}

pub mod commit {
	use crate::keys::SharedKeyConfig;

	pub fn details_author() -> String {
		"Author: ".to_string()
	}
	pub fn details_committer() -> String {
		"Committer: ".to_string()
	}
	pub fn details_sha() -> String {
		"Sha: ".to_string()
	}
	pub fn details_date() -> String {
		"Date: ".to_string()
	}
	pub fn details_tags() -> String {
		"Tags: ".to_string()
	}
	pub fn details_message() -> String {
		"Subject: ".to_string()
	}
	pub fn details_info_title(
		_key_config: &SharedKeyConfig,
	) -> String {
		"Info".to_string()
	}
	pub fn compare_details_info_title(
		old: bool,
		hash: &str,
	) -> String {
		format!("{}: {hash}", if old { "Old" } else { "New" })
	}
	pub fn details_message_title(
		_key_config: &SharedKeyConfig,
	) -> String {
		"Message".to_string()
	}
	pub fn details_files_title(
		_key_config: &SharedKeyConfig,
	) -> String {
		"Files:".to_string()
	}
}

pub mod commands {
	use crate::components::CommandText;
	use crate::keys::SharedKeyConfig;

	static CMD_GROUP_GENERAL: &str = "-- General --";
	static CMD_GROUP_DIFF: &str = "-- Diff --";
	static CMD_GROUP_CHANGES: &str = "-- Changes --";
	static CMD_GROUP_COMMIT_POPUP: &str = "-- Commit Popup --";
	static CMD_GROUP_STASHING: &str = "-- Stashing --";
	static CMD_GROUP_STASHES: &str = "-- Stashes --";
	static CMD_GROUP_LOG: &str = "-- Log --";
	static CMD_GROUP_BRANCHES: &str = "-- Branches --";

	/// Enter or leave the current list's range-selection mode.
	pub fn selection_mode(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Select [{}]",
				key_config.get_hint(key_config.keys.select_mode)
			),
			"begin or end selection mode",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.select_mode)
	}

	/// Clear the current list's marks or selection range.
	pub fn selection_clear(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Clear selection [{}]",
				key_config.get_hint(key_config.keys.select_clear)
			),
			"clear selected items",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.select_clear)
	}

	/// Select every item supported by the focused list.
	pub fn selection_all(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Select all [{}]",
				key_config.get_hint(key_config.keys.select_all)
			),
			"select all items in this pane (loaded commits only)",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.select_all)
	}

	/// Toggle the highlighted row in a multi-item selection.
	pub fn selection_item(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Toggle item [{}]",
				key_config.get_hint(key_config.keys.select_item)
			),
			"toggle the highlighted item in the selection",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.select_item)
	}

	/// Move focus to the next pane.
	pub fn focus_next(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Next pane [{}]",
				key_config.get_hint(key_config.keys.focus_next)
			),
			"move focus to the next pane",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.focus_next)
	}

	/// Move focus to the previous pane.
	pub fn focus_previous(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Previous pane [{}]",
				key_config.get_hint(key_config.keys.focus_previous)
			),
			"move focus to the previous pane",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.focus_previous)
	}

	/// Open the command palette.
	pub fn command_palette(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Commands [{}]",
				key_config.get_hint(key_config.keys.command_palette)
			),
			"search available commands",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.command_palette)
	}

	/// Enter the goto prefix.
	pub fn goto_prefix(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Goto [{}]",
				key_config.get_hint(key_config.keys.goto_prefix)
			),
			"choose a tab or location",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.goto_prefix)
	}

	/// Enter the common actions prefix.
	pub fn action_prefix(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Actions [{}]",
				key_config.get_hint(key_config.keys.action_prefix)
			),
			"choose a common action",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.action_prefix)
	}

	pub fn toggle_tabs(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Next [{}]",
				key_config.get_hint(key_config.keys.tab_toggle)
			),
			"switch to next tab",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.tab_toggle)
	}
	pub fn find_file(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Find [{}]",
				key_config.get_hint(key_config.keys.file_find)
			),
			"find file in tree",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.file_find)
	}
	pub fn find_branch(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Find [{}]",
				key_config.get_hint(key_config.keys.branch_find)
			),
			"find branch in list",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.branch_find)
	}
	pub fn toggle_tabs_direct(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Tabs [{}, {}, {}, {}, {}, {}, {}]",
				key_config.get_hint(key_config.keys.tab_status),
				key_config.get_hint(key_config.keys.tab_log),
				key_config.get_hint(key_config.keys.tab_files),
				key_config.get_hint(key_config.keys.tab_stashing),
				key_config.get_hint(key_config.keys.tab_stashes),
				key_config.get_hint(key_config.keys.tab_rebase),
				key_config.get_hint(key_config.keys.tab_branches),
			),
			"open a tab with the goto prefix",
			CMD_GROUP_GENERAL,
		)
	}
	pub fn options_popup(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Options [{}]",
				key_config.get_hint(key_config.keys.open_options),
			),
			"open options popup",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.open_options)
	}
	pub fn help_open(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Help [{}]",
				key_config.get_hint(key_config.keys.open_help)
			),
			"open this help screen",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.open_help)
	}
	pub fn navigate_commit_message(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Nav [{}{}]",
				key_config.get_hint(key_config.keys.move_up),
				key_config.get_hint(key_config.keys.move_down)
			),
			"navigate commit message",
			CMD_GROUP_GENERAL,
		)
	}
	pub fn navigate_tree(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Nav [{}{}{}{}]",
				key_config.get_hint(key_config.keys.move_up),
				key_config.get_hint(key_config.keys.move_down),
				key_config.get_hint(key_config.keys.move_right),
				key_config.get_hint(key_config.keys.move_left)
			),
			"navigate tree view, collapse, expand",
			CMD_GROUP_GENERAL,
		)
	}
	pub fn scroll(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Scroll [{}{}]",
				key_config.get_hint(key_config.keys.move_up),
				key_config.get_hint(key_config.keys.move_down)
			),
			"scroll up or down in focused view",
			CMD_GROUP_GENERAL,
		)
	}
	pub fn commit_list_mark(
		key_config: &SharedKeyConfig,
		marked: bool,
	) -> CommandText {
		CommandText::new(
			format!(
				"{} [{}]",
				if marked { "Unmark" } else { "Mark" },
				key_config.get_hint(key_config.keys.log_mark_commit),
			),
			"mark multiple commits",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.log_mark_commit)
	}
	pub fn copy(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Copy [{}]",
				key_config.get_hint(key_config.keys.copy),
			),
			"copy selected lines to clipboard",
			CMD_GROUP_DIFF,
		)
		.with_binding(key_config.keys.copy)
	}
	pub fn copy_hash(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Copy Hash [{}]",
				key_config.get_hint(key_config.keys.copy),
			),
			"copy selected commit hash to clipboard",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.copy)
	}
	pub fn copy_path(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Copy Path [{}]",
				key_config.get_hint(key_config.keys.copy),
			),
			"copy selected file path to clipboard",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.copy)
	}
	pub fn push_tags(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Push Tags [{}]",
				key_config.get_hint(key_config.keys.push),
			),
			"push tags to remote",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.push)
	}
	pub fn toggle_option(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Toggle Option [{}]",
				key_config.get_hint(key_config.keys.log_mark_commit),
			),
			"toggle search option selected",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.log_mark_commit)
	}
	pub fn show_tag_annotation(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Annotation [{}]",
				key_config.get_hint(key_config.keys.move_right),
			),
			"show tag annotation",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.move_right)
	}
	pub fn diff_hunk_next(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Next hunk [{}]",
				key_config.get_hint(key_config.keys.diff_hunk_next),
			),
			"move cursor to next hunk",
			CMD_GROUP_DIFF,
		)
		.with_binding(key_config.keys.diff_hunk_next)
	}
	pub fn diff_hunk_prev(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Prev hunk [{}]",
				key_config.get_hint(key_config.keys.diff_hunk_prev),
			),
			"move cursor to prev hunk",
			CMD_GROUP_DIFF,
		)
		.with_binding(key_config.keys.diff_hunk_prev)
	}
	pub fn diff_home_end(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Jump up/down [{},{},{},{}]",
				key_config.get_hint(key_config.keys.home),
				key_config.get_hint(key_config.keys.end),
				key_config.get_hint(key_config.keys.move_up),
				key_config.get_hint(key_config.keys.move_down)
			),
			"scroll to top or bottom of diff",
			CMD_GROUP_DIFF,
		)
	}
	pub fn diff_hunk_add(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Add hunk [{}]",
				key_config
					.get_hint(key_config.keys.stage_unstage_item),
			),
			"adds selected hunk to stage",
			CMD_GROUP_DIFF,
		)
		.with_binding(key_config.keys.stage_unstage_item)
	}
	pub fn diff_hunk_revert(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Reset hunk [{}]",
				key_config
					.get_hint(key_config.keys.status_reset_item),
			),
			"reverts selected hunk",
			CMD_GROUP_DIFF,
		)
		.with_binding(key_config.keys.status_reset_item)
	}
	pub fn diff_lines_revert(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Reset lines [{}]",
				key_config.get_hint(key_config.keys.diff_reset_lines),
			),
			"resets selected lines",
			CMD_GROUP_DIFF,
		)
		.with_binding(key_config.keys.diff_reset_lines)
	}
	pub fn diff_lines_stage(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		let binding = if key_config.is_modal() {
			key_config.keys.stage_unstage_item
		} else {
			key_config.keys.diff_stage_lines
		};
		CommandText::new(
			format!(
				"Stage selected changes [{}]",
				key_config.get_hint(binding),
			),
			"stage selected lines or hunk",
			CMD_GROUP_DIFF,
		)
		.with_binding(binding)
	}
	pub fn diff_lines_unstage(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		let binding = if key_config.is_modal() {
			key_config.keys.status_unstage_item
		} else {
			key_config.keys.diff_stage_lines
		};
		CommandText::new(
			format!(
				"Unstage lines [{}]",
				key_config.get_hint(binding),
			),
			"unstage selected lines",
			CMD_GROUP_DIFF,
		)
		.with_binding(binding)
	}
	pub fn diff_hunk_remove(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Remove hunk [{}]",
				key_config
					.get_hint(key_config.keys.stage_unstage_item),
			),
			"removes selected hunk from stage",
			CMD_GROUP_DIFF,
		)
		.with_binding(key_config.keys.stage_unstage_item)
	}
	pub fn close_fuzzy_finder(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Close [{}{}]",
				key_config.get_hint(key_config.keys.exit_popup),
				key_config.get_hint(key_config.keys.enter),
			),
			"close fuzzy finder",
			CMD_GROUP_GENERAL,
		)
	}
	pub fn close_popup(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Close [{}]",
				key_config.get_hint(key_config.keys.exit_popup),
			),
			"close overlay (e.g commit, help)",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.exit_popup)
	}
	pub fn scroll_popup(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Scroll [{}{}]",
				key_config.get_hint(key_config.keys.popup_down),
				key_config.get_hint(key_config.keys.popup_up),
			),
			"scroll up or down in popup",
			CMD_GROUP_GENERAL,
		)
	}
	pub fn close_msg(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Close [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"close msg popup (e.g msg)",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.enter)
		.hide_help()
	}
	pub fn validate_msg(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Validate [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"validate msg",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.enter)
		.hide_help()
	}

	pub fn abort_merge(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Abort merge [{}]",
				key_config.get_hint(key_config.keys.abort_merge),
			),
			"abort ongoing merge",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.abort_merge)
	}

	pub fn abort_revert(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Abort revert [{}]",
				key_config.get_hint(key_config.keys.abort_merge),
			),
			"abort ongoing revert",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.abort_merge)
	}

	pub fn view_submodules(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Submodules [{}]",
				key_config.get_hint(key_config.keys.view_submodules),
			),
			"open submodule view",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.view_submodules)
	}

	pub fn view_remotes(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Remotes [{}]",
				key_config.get_hint(key_config.keys.view_remotes)
			),
			"open remotes view",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.view_remotes)
	}

	pub fn update_remote_name(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Edit name [{}]",
				key_config
					.get_hint(key_config.keys.update_remote_name)
			),
			"updates a remote name",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.update_remote_name)
	}

	pub fn update_remote_url(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Edit url [{}]",
				key_config
					.get_hint(key_config.keys.update_remote_url)
			),
			"updates a remote url",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.update_remote_url)
	}

	pub fn create_remote(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Add [{}]",
				key_config.get_hint(key_config.keys.add_remote)
			),
			"creates a new remote",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.add_remote)
	}

	pub fn delete_remote_popup(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Remove [{}]",
				key_config.get_hint(key_config.keys.delete_remote),
			),
			"remove a remote",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.delete_remote)
	}

	pub fn remote_confirm_name_msg(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Confirm name [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"confirm remote name",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.enter)
		.hide_help()
	}

	pub fn remote_confirm_url_msg(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Confirm url [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"confirm remote url",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.enter)
		.hide_help()
	}

	pub fn open_submodule(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Open [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"open submodule",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.enter)
	}

	pub fn open_submodule_parent(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Open Parent [{}]",
				key_config
					.get_hint(key_config.keys.view_submodule_parent),
			),
			"open submodule parent repo",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.view_submodule_parent)
	}

	pub fn update_submodule(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Update [{}]",
				key_config.get_hint(key_config.keys.update_submodule),
			),
			"update submodule",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.update_submodule)
	}

	pub fn continue_rebase(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Continue rebase [{}]",
				key_config.get_hint(key_config.keys.rebase_branch),
			),
			"continue ongoing rebase",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.rebase_branch)
	}

	pub fn abort_rebase(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Abort rebase [{}]",
				key_config.get_hint(key_config.keys.abort_merge),
			),
			"abort ongoing rebase",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.abort_merge)
	}

	pub fn select_staging(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"To stage [{}]",
				key_config.get_hint(key_config.keys.toggle_workarea),
			),
			"focus/select staging area",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.toggle_workarea)
	}
	pub fn select_unstaged(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"To unstaged [{}]",
				key_config.get_hint(key_config.keys.toggle_workarea),
			),
			"focus/select unstaged area",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.toggle_workarea)
	}
	pub fn undo_commit(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Undo Commit [{}]",
				key_config.get_hint(key_config.keys.undo_commit),
			),
			"undo last commit",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.undo_commit)
	}
	pub fn commit_open(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Commit [{}]",
				key_config.get_hint(key_config.keys.open_commit),
			),
			"open commit popup (available in non-empty stage)",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.open_commit)
	}
	pub fn commit_open_editor(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Open editor [{}]",
				key_config
					.get_hint(key_config.keys.open_commit_editor),
			),
			"open commit editor (available in commit popup)",
			CMD_GROUP_COMMIT_POPUP,
		)
		.with_binding(key_config.keys.open_commit_editor)
	}
	pub fn commit_open_inline(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Inline Commit [{}]",
				key_config
					.get_hint(key_config.keys.open_commit_inline),
			),
			"open the inline commit editor",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.open_commit_inline)
	}
	pub fn commit_next_msg_from_history(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Previous Msg [{}]",
				key_config
					.get_hint(key_config.keys.commit_history_next),
			),
			"use previous commit message from history",
			CMD_GROUP_COMMIT_POPUP,
		)
		.with_binding(key_config.keys.commit_history_next)
	}
	pub fn commit_submit(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Do Commit [{}]",
				key_config.get_hint(key_config.keys.commit),
			),
			"commit (available when commit message is non-empty)",
			CMD_GROUP_COMMIT_POPUP,
		)
		.with_binding(key_config.keys.commit)
		.hide_help()
	}
	pub fn newline(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"New line [{}]",
				key_config.get_hint(key_config.keys.newline),
			),
			"create line break",
			CMD_GROUP_COMMIT_POPUP,
		)
		.with_binding(key_config.keys.newline)
		.hide_help()
	}
	pub fn toggle_verify(
		key_config: &SharedKeyConfig,
		current_verify: bool,
	) -> CommandText {
		let verb = if current_verify { "disable" } else { "enable" };
		CommandText::new(
			format!(
				"{} hooks [{}]",
				verb,
				key_config.get_hint(key_config.keys.toggle_verify),
			),
			"toggle running on commit hooks (available in commit popup)",
			CMD_GROUP_COMMIT_POPUP,
		).with_binding(key_config.keys.toggle_verify)
	}

	pub fn commit_amend(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Amend [{}]",
				key_config.get_hint(key_config.keys.commit_amend),
			),
			"amend last commit (available in commit popup)",
			CMD_GROUP_COMMIT_POPUP,
		)
		.with_binding(key_config.keys.commit_amend)
	}
	pub fn commit_signoff(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Sign-off [{}]",
				key_config.get_hint(key_config.keys.toggle_signoff),
			),
			"sign-off commit (-s option)",
			CMD_GROUP_COMMIT_POPUP,
		)
		.with_binding(key_config.keys.toggle_signoff)
	}
	pub fn edit_item(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Edit [{}]",
				key_config.get_hint(key_config.keys.edit_file),
			),
			"edit the currently selected file in an external editor",
			CMD_GROUP_CHANGES,
		)
		.with_binding(key_config.keys.edit_file)
	}
	pub fn stage_item(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Stage [{}]",
				key_config
					.get_hint(key_config.keys.stage_unstage_item),
			),
			"stage currently selected file or entire path",
			CMD_GROUP_CHANGES,
		)
		.with_binding(key_config.keys.stage_unstage_item)
	}
	pub fn stage_all(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Stage All [{}]",
				key_config.get_hint(key_config.keys.status_stage_all),
			),
			"stage all changes (in unstaged files)",
			CMD_GROUP_CHANGES,
		)
		.with_binding(key_config.keys.status_stage_all)
	}
	pub fn unstage_item(key_config: &SharedKeyConfig) -> CommandText {
		let binding = if key_config.is_modal() {
			key_config.keys.status_unstage_item
		} else {
			key_config.keys.stage_unstage_item
		};
		CommandText::new(
			format!("Unstage [{}]", key_config.get_hint(binding)),
			"unstage currently selected file or entire path",
			CMD_GROUP_CHANGES,
		)
		.with_binding(binding)
	}
	pub fn unstage_all(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Unstage all [{}]",
				key_config.get_hint(key_config.keys.status_stage_all),
			),
			"unstage all files (in staged files)",
			CMD_GROUP_CHANGES,
		)
		.with_binding(key_config.keys.status_stage_all)
	}
	pub fn reset_item(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Reset [{}]",
				key_config
					.get_hint(key_config.keys.status_reset_item),
			),
			"revert changes in selected file or entire path",
			CMD_GROUP_CHANGES,
		)
		.with_binding(key_config.keys.status_reset_item)
	}
	pub fn ignore_item(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Ignore [{}]",
				key_config
					.get_hint(key_config.keys.status_ignore_file),
			),
			"Add file or path to .gitignore",
			CMD_GROUP_CHANGES,
		)
		.with_binding(key_config.keys.status_ignore_file)
	}

	pub fn diff_focus_left(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Back [{}]",
				key_config.get_hint(key_config.keys.move_left),
			),
			"view and select changed files",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.move_left)
	}
	pub fn diff_focus_right(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Diff [{}]",
				key_config.get_hint(key_config.keys.move_right),
			),
			"inspect file diff",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.move_right)
	}
	pub fn diff_view_toggle(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Review/Patch [{}]",
				key_config.get_hint(key_config.keys.diff_view_toggle)
			),
			"toggle the read-only native preview and Git patch view",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.diff_view_toggle)
	}
	pub fn diff_zoom_toggle(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Zoom diff [{}]",
				key_config.get_hint(key_config.keys.diff_zoom_toggle)
			),
			"expand the selected diff to the full Status view",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.diff_zoom_toggle)
	}
	pub fn quit(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Quit [{}]",
				key_config.get_hint(key_config.keys.exit),
			),
			"quit gitui application",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.exit)
	}
	pub fn confirm_action(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Confirm [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"confirm action",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.enter)
	}
	pub fn stashing_save(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Save [{}]",
				key_config.get_hint(key_config.keys.stashing_save),
			),
			"opens stash name input popup",
			CMD_GROUP_STASHING,
		)
		.with_binding(key_config.keys.stashing_save)
	}
	pub fn stashing_toggle_indexed(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Toggle Staged [{}]",
				key_config
					.get_hint(key_config.keys.stashing_toggle_index),
			),
			"toggle including staged files into stash",
			CMD_GROUP_STASHING,
		)
		.with_binding(key_config.keys.stashing_toggle_index)
	}
	pub fn stashing_toggle_untracked(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Toggle Untracked [{}]",
				key_config.get_hint(
					key_config.keys.stashing_toggle_untracked
				),
			),
			"toggle including untracked files into stash",
			CMD_GROUP_STASHING,
		)
		.with_binding(key_config.keys.stashing_toggle_untracked)
	}
	pub fn stashing_confirm_msg(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Stash [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"save files to stash",
			CMD_GROUP_STASHING,
		)
		.with_binding(key_config.keys.enter)
	}
	pub fn stashlist_apply(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Apply [{}]",
				key_config.get_hint(key_config.keys.stash_apply),
			),
			"apply selected stash",
			CMD_GROUP_STASHES,
		)
		.with_binding(key_config.keys.stash_apply)
	}
	pub fn stashlist_drop(
		key_config: &SharedKeyConfig,
		marked: usize,
	) -> CommandText {
		CommandText::new(
			format!(
				"Drop{} [{}]",
				if marked == 0 {
					String::default()
				} else {
					format!(" {marked}")
				},
				key_config.get_hint(key_config.keys.stash_drop),
			),
			"drop selected stash",
			CMD_GROUP_STASHES,
		)
		.with_binding(key_config.keys.stash_drop)
	}
	pub fn stashlist_pop(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Pop [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"pop selected stash",
			CMD_GROUP_STASHES,
		)
		.with_binding(key_config.keys.enter)
	}
	pub fn stashlist_inspect(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Inspect [{}]",
				key_config.get_hint(key_config.keys.stash_open),
			),
			"open stash commit details (allows to diff files)",
			CMD_GROUP_STASHES,
		)
		.with_binding(key_config.keys.stash_open)
	}
	pub fn log_details_toggle(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Details [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"open details of selected commit",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.enter)
	}

	pub fn commit_details_open(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Inspect [{}]",
				key_config.get_hint(key_config.keys.move_right),
			),
			"inspect selected commit in detail",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.move_right)
	}

	pub fn blame_file(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Blame [{}]",
				key_config.get_hint(key_config.keys.blame),
			),
			"open blame view of selected file",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.blame)
	}
	pub fn open_file_history(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"History [{}]",
				key_config.get_hint(key_config.keys.file_history),
			),
			"open history of selected file",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.file_history)
	}
	pub fn open_line_number_popup(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Go to [{}]",
				key_config.get_hint(key_config.keys.goto_line),
			),
			"go to a given line number in the blame view",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.goto_line)
	}
	pub fn log_tag_commit(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Tag [{}]",
				key_config.get_hint(key_config.keys.log_tag_commit),
			),
			"tag commit",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.log_tag_commit)
	}
	pub fn log_checkout_commit(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Checkout [{}]",
				key_config
					.get_hint(key_config.keys.log_checkout_commit),
			),
			"checkout commit",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.log_checkout_commit)
	}
	pub fn inspect_file_tree(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Files [{}]",
				key_config.get_hint(key_config.keys.open_file_tree),
			),
			"inspect file tree at specific revision",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.open_file_tree)
	}
	pub fn revert_commit(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Revert [{}]",
				key_config
					.get_hint(key_config.keys.status_reset_item),
			),
			"revert commit",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.status_reset_item)
	}
	pub fn log_reset_commit(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Reset [{}]",
				key_config.get_hint(key_config.keys.log_reset_commit),
			),
			"reset to commit",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.log_reset_commit)
	}
	pub fn log_reword_commit(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Reword [{}]",
				key_config
					.get_hint(key_config.keys.log_reword_commit),
			),
			"reword commit message",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.log_reword_commit)
	}
	pub fn log_rewrite_from(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Rewrite from here [{}]",
				key_config
					.get_hint(key_config.keys.log_rewrite_from),
			),
			"draft an interactive rebase of everything after this commit's parent, including it",
			CMD_GROUP_LOG,
		).with_binding(key_config.keys.log_rewrite_from)
	}
	pub fn log_find_commit(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Find [{}]",
				key_config.get_hint(key_config.keys.file_find),
			),
			"start commit search",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.file_find)
	}
	pub fn log_close_search(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Exit Search [{}]",
				key_config.get_hint(key_config.keys.exit_popup),
			),
			"leave search mode",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.exit_popup)
	}

	pub fn log_toggle_graph(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Toggle Graph [{}]",
				key_config.get_hint(key_config.keys.log_toggle_graph),
			),
			"toggle commit graph",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.log_toggle_graph)
	}

	pub fn reset_commit(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Confirm [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"confirm reset",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.enter)
	}

	pub fn reset_branch(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Reset [{}]",
				key_config.get_hint(key_config.keys.reset_branch),
			),
			"confirm reset",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.reset_branch)
	}

	pub fn reset_type(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Change Type [{}{}]",
				key_config.get_hint(key_config.keys.move_up),
				key_config.get_hint(key_config.keys.move_down)
			),
			"change reset type",
			CMD_GROUP_LOG,
		)
	}
	pub fn tag_commit_confirm_msg(
		key_config: &SharedKeyConfig,
		is_annotation_mode: bool,
	) -> CommandText {
		CommandText::new(
			format!(
				"Tag [{}]",
				key_config.get_hint(if is_annotation_mode {
					key_config.keys.commit
				} else {
					key_config.keys.enter
				}),
			),
			"tag commit",
			CMD_GROUP_LOG,
		)
	}

	pub fn tag_annotate_msg(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Annotate [{}]",
				key_config.get_hint(key_config.keys.tag_annotate),
			),
			"annotate tag",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.tag_annotate)
	}

	pub fn create_branch_confirm_msg(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Create Branch [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"create branch",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.enter)
		.hide_help()
	}
	pub fn open_branch_create_popup(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Create [{}]",
				key_config.get_hint(key_config.keys.create_branch),
			),
			"open create branch popup",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.create_branch)
	}
	pub fn rename_branch_confirm_msg(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Rename Branch [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"rename branch",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.enter)
		.hide_help()
	}
	pub fn rename_branch_popup(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Rename Branch [{}]",
				key_config.get_hint(key_config.keys.rename_branch),
			),
			"rename branch",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.rename_branch)
	}
	pub fn delete_branch_popup(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Delete [{}]",
				key_config.get_hint(key_config.keys.delete_branch),
			),
			"delete a branch",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.delete_branch)
	}
	pub fn merge_branch_popup(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Merge [{}]",
				key_config.get_hint(key_config.keys.merge_branch),
			),
			"merge a branch",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.merge_branch)
	}

	pub fn branch_popup_rebase(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Rebase [{}]",
				key_config.get_hint(key_config.keys.rebase_branch),
			),
			"rebase a branch",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.rebase_branch)
	}

	pub fn compare_with_head(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Compare [{}]",
				key_config.get_hint(key_config.keys.compare_commits),
			),
			"compare with head",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.compare_commits)
	}

	pub fn compare_commits(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Compare Commits [{}]",
				key_config.get_hint(key_config.keys.compare_commits),
			),
			"compare two marked commits",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.compare_commits)
	}

	pub fn branches_set_base(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Set base [{}]",
				key_config
					.get_hint(key_config.keys.branches_set_base),
			),
			"choose the base branch for comparison",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.branches_set_base)
	}

	pub fn branches_load_more(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Load 200 More Commits [{}]",
				key_config
					.get_hint(key_config.keys.branches_load_more),
			),
			"load the next 200 commits into the branch graph",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.branches_load_more)
	}

	pub fn branches_create(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Create branch [{}]",
				key_config.get_hint(key_config.keys.branches_create),
			),
			"create a branch from the selected repository state",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.branches_create)
	}

	pub fn select_branch_popup(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Checkout [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"checkout branch",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.enter)
	}
	pub fn toggle_branch_popup(
		key_config: &SharedKeyConfig,
		local: bool,
	) -> CommandText {
		CommandText::new(
			format!(
				"{} [{}]",
				if local { "Remote" } else { "Local" },
				key_config.get_hint(key_config.keys.tab_toggle),
			),
			"toggle branch type (remote/local)",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.tab_toggle)
	}
	pub fn open_branch_select_popup(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Branches [{}]",
				key_config.get_hint(key_config.keys.select_branch),
			),
			"open branch popup",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.select_branch)
	}

	pub fn open_tags_popup(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Tags [{}]",
				key_config.get_hint(key_config.keys.tags),
			),
			"open tags popup",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.tags)
	}
	pub fn delete_tag_popup(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Delete [{}]",
				key_config.get_hint(key_config.keys.delete_tag),
			),
			"delete a tag",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.delete_tag)
	}
	pub fn select_tag(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Select commit [{}]",
				key_config.get_hint(key_config.keys.select_tag),
			),
			"Select commit in revlog",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.select_tag)
	}

	pub fn status_push(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Push [{}]",
				key_config.get_hint(key_config.keys.push),
			),
			"push to origin",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.push)
	}
	pub fn status_force_push(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Force Push [{}]",
				key_config.get_hint(key_config.keys.force_push),
			),
			"force push to origin",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.force_push)
	}

	pub fn status_fetch(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Fetch [{}]",
				key_config.get_hint(key_config.keys.fetch),
			),
			"fetch",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.fetch)
	}
	pub fn status_pull(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Pull [{}]",
				key_config.get_hint(key_config.keys.pull),
			),
			"fetch/merge",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.pull)
	}

	pub fn fetch_remotes(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Fetch [{}]",
				key_config.get_hint(key_config.keys.fetch),
			),
			"fetch/prune",
			CMD_GROUP_BRANCHES,
		)
		.with_binding(key_config.keys.fetch)
	}

	pub fn find_commit_sha(
		key_config: &SharedKeyConfig,
	) -> CommandText {
		CommandText::new(
			format!(
				"Search Hash [{}]",
				key_config.get_hint(key_config.keys.find_commit_sha),
			),
			"find commit from sha",
			CMD_GROUP_LOG,
		)
		.with_binding(key_config.keys.find_commit_sha)
	}

	pub fn goto_line(key_config: &SharedKeyConfig) -> CommandText {
		CommandText::new(
			format!(
				"Go To [{}]",
				key_config.get_hint(key_config.keys.enter),
			),
			"Go to the given line",
			CMD_GROUP_GENERAL,
		)
		.with_binding(key_config.keys.enter)
	}
}

// rebase tab — until then nothing outside tests reaches it.
pub mod rebase {
	use crate::keys::SharedKeyConfig;
	use asyncgit::rebase::policy::{Done, Phase, Stop};

	static CMD_GROUP_REBASE: &str = "-- Rebase --";

	/// Short label of the current phase for the context header.
	pub fn phase_label(phase: &Phase) -> String {
		match phase {
			Phase::Draft => String::from("draft"),
			Phase::Review => String::from("review"),
			Phase::Executing => String::from("executing…"),
			Phase::Stopped(stop) => {
				format!("stopped: {}", stop_label(stop))
			}
			Phase::Restoring => String::from("restoring…"),
			Phase::Done(done) => {
				format!("done: {}", done_label(*done))
			}
			Phase::Adopted { .. } => {
				String::from("adopted (read-only)")
			}
			Phase::Reconciling { .. } => String::from("reconcile"),
			Phase::Unsupported { .. } => {
				String::from("unsupported state")
			}
		}
	}

	///
	pub fn stop_label(stop: &Stop) -> String {
		match stop {
			Stop::Conflicts { paths } => {
				if paths.is_empty() {
					String::from("conflicts resolved")
				} else {
					format!("conflicts ({})", paths.len())
				}
			}
			Stop::Edit { .. } => String::from("edit"),
			Stop::Break => String::from("break"),
			Stop::BecameEmpty { .. } => {
				String::from("commit became empty")
			}
			Stop::Message { .. } => String::from("message"),
			Stop::ExecFailed { .. } => String::from("exec failed"),
			Stop::Unknown { .. } => String::from("unknown"),
		}
	}

	///
	pub const fn done_label(done: Done) -> &'static str {
		match done {
			Done::Finished => "finished",
			Done::Aborted => "aborted",
			Done::Quit => "quit",
		}
	}

	/// The one-line context header: `branch → onto · phase · progress`.
	pub fn header(
		branch: &str,
		onto: &str,
		phase: &Phase,
		done: usize,
		total: Option<usize>,
	) -> String {
		let progress = total.map_or_else(
			|| {
				String::from(match phase {
					// Nothing is queued or running in these
					// states; a count would only be noise.
					Phase::Done(_)
					| Phase::Reconciling { .. }
					| Phase::Unsupported { .. } => "—",
					Phase::Adopted { .. } => "observed",
					_ => "not started",
				})
			},
			|total| format!("{done}/{total} steps"),
		);
		format!(
			"{branch} → {onto} · {} · {progress}",
			phase_label(phase)
		)
	}

	///
	pub fn header_no_session() -> String {
		String::from("no rebase session")
	}

	///
	pub fn rail_title(key_config: &SharedKeyConfig) -> String {
		format!(
			"Plan [{}{} move]",
			key_config.get_hint(key_config.keys.move_up),
			key_config.get_hint(key_config.keys.move_down),
		)
	}

	///
	pub fn rail_empty_hint() -> String {
		String::from(
			"no rebase session — start one from the Log tab (R on a commit)",
		)
	}

	///
	pub fn inspector_title_preview(count: usize) -> String {
		if count == 1 {
			String::from("planned result · 1 commit")
		} else {
			format!("planned result · {count} commits")
		}
	}

	///
	pub const fn inspector_title_review() -> &'static str {
		"review"
	}

	///
	pub const fn inspector_title_stop() -> &'static str {
		"stopped"
	}

	///
	pub const fn inspector_title_outcome() -> &'static str {
		"outcome"
	}

	///
	pub const fn inspector_title_reconcile() -> &'static str {
		"reconcile"
	}

	///
	pub fn preview_folded(count: usize) -> String {
		format!(" (+{count} folded)")
	}

	///
	pub fn preview_dropped(label: &str) -> String {
		format!("dropped: {label}")
	}

	///
	pub fn review_destination(branch: &str, onto: &str) -> String {
		format!("destination: {branch} onto {onto}")
	}

	///
	pub fn review_counts(groups: usize, drops: usize) -> String {
		format!("groups: {groups} · drops: {drops}")
	}

	///
	pub const fn review_changes_pending() -> &'static str {
		"local changes: yes — protected before start"
	}

	///
	pub const fn review_changes_none() -> &'static str {
		"local changes: none"
	}

	///
	pub fn stop_conflict_path(path: &str) -> String {
		format!("! {path}")
	}

	///
	pub fn stop_conflicts_unresolved(count: usize) -> String {
		format!("{count} unresolved conflict(s) — resolve in the Status tab")
	}

	///
	pub const fn stop_conflicts_resolved() -> &'static str {
		"all conflicts resolved — continue to proceed"
	}

	///
	pub fn stop_edit(commit: &str) -> String {
		format!("stopped at {commit} for amend/split")
	}

	///
	pub const fn stop_edit_hint() -> &'static str {
		"stage changes and continue to amend, or continue unchanged"
	}

	///
	pub fn stop_message(file: &str) -> String {
		format!("compose the message for {file}")
	}

	///
	pub const fn stop_message_hint() -> &'static str {
		"enter saves the message, esc cancels"
	}

	///
	pub const fn stop_break() -> &'static str {
		"deliberate stop — continue when ready"
	}

	///
	pub fn stop_became_empty(commit: &str) -> String {
		format!("{commit} became empty — keep it or skip it")
	}

	///
	pub fn stop_exec_failed(command: &str) -> String {
		format!("exec failed: {command}")
	}

	///
	pub const fn stop_exec_unknown_exit() -> &'static str {
		"exit status unknown (git does not record it)"
	}

	///
	pub fn stop_unknown(note: &str) -> String {
		format!("unrecognized stop: {note}")
	}

	///
	pub fn outcome_label(done: Done) -> String {
		String::from(done_label(done))
	}

	///
	pub fn outcome_tip(tip: &str) -> String {
		format!("new tip: {tip}")
	}

	///
	pub const fn outcome_protection_outstanding() -> &'static str {
		"protected local changes are still stashed"
	}

	///
	pub const fn outcome_protection_restored() -> &'static str {
		"protected local changes restored"
	}

	///
	pub const fn outcome_restore_hint() -> &'static str {
		"continue to restore the protected changes"
	}

	///
	pub const fn outcome_close_hint() -> &'static str {
		"enter closes the session"
	}

	/// Why the reviewed start is currently refused.
	pub fn start_blocked(reason: &str) -> String {
		format!("! start blocked: {reason}")
	}

	/// The local-change evidence behind the protection need.
	pub fn local_changes_counts(
		staged: usize,
		unstaged: usize,
		untracked: usize,
	) -> String {
		format!(
			"local changes: {staged} staged · {unstaged} unstaged \
			 · {untracked} untracked"
		)
	}

	///
	pub const fn stop_adopted_observing() -> &'static str {
		"adopted an external rebase — observing only"
	}

	///
	pub const fn stop_adopted_mutations() -> &'static str {
		"native continue/skip/abort stay available"
	}

	///
	pub fn reconcile_reason(reason: &str) -> String {
		format!("divergence: {reason}")
	}

	///
	pub const fn reconcile_no_mutation() -> &'static str {
		"nothing will mutate until resolved"
	}

	///
	pub fn composer_title_message() -> String {
		String::from("message")
	}

	///
	pub fn composer_title_note() -> String {
		String::from("reword note")
	}

	///
	pub const fn composer_default_msg() -> &'static str {
		"type message.."
	}

	///
	pub const fn exec_popup_title() -> &'static str {
		"exec command"
	}

	///
	pub const fn exec_popup_default_msg() -> &'static str {
		"shell command to insert.."
	}

	/// Errors from plan/session mutations surface verbatim with this prefix.
	pub const fn msg_action_failed() -> &'static str {
		"rebase action failed"
	}

	pub mod commands {
		use super::CMD_GROUP_REBASE;
		use crate::components::CommandText;
		use crate::keys::SharedKeyConfig;

		///
		pub fn start(key_config: &SharedKeyConfig) -> CommandText {
			CommandText::new(
				format!(
					"Start [{}]",
					key_config.get_hint(key_config.keys.rebase_start)
				),
				"start the reviewed rebase",
				CMD_GROUP_REBASE,
			)
			.with_binding(key_config.keys.rebase_start)
		}

		///
		pub fn r#continue(
			key_config: &SharedKeyConfig,
		) -> CommandText {
			CommandText::new(
				format!(
					"Continue [{}]",
					key_config.get_hint(key_config.keys.enter)
				),
				"continue the stopped rebase",
				CMD_GROUP_REBASE,
			)
			.with_binding(key_config.keys.enter)
		}

		///
		pub fn skip(key_config: &SharedKeyConfig) -> CommandText {
			CommandText::new(
				format!(
					"Skip [{}]",
					key_config.get_hint(key_config.keys.rebase_skip)
				),
				"skip the stopped step",
				CMD_GROUP_REBASE,
			)
			.with_binding(key_config.keys.rebase_skip)
		}

		///
		pub fn keep_empty(
			key_config: &SharedKeyConfig,
		) -> CommandText {
			CommandText::new(
				format!(
					"Keep [{}]",
					key_config.get_hint(key_config.keys.rebase_keep)
				),
				"keep the commit that became empty",
				CMD_GROUP_REBASE,
			)
			.with_binding(key_config.keys.rebase_keep)
		}

		///
		pub fn abort(key_config: &SharedKeyConfig) -> CommandText {
			CommandText::new(
				format!(
					"Abort [{}]",
					key_config.get_hint(key_config.keys.rebase_abort)
				),
				"abort the rebase and restore the original state",
				CMD_GROUP_REBASE,
			)
			.with_binding(key_config.keys.rebase_abort)
		}

		///
		pub fn set_action(word: &str, hint: &str) -> CommandText {
			CommandText::new(
				format!("{word} [{hint}]"),
				"change what the selected step does",
				CMD_GROUP_REBASE,
			)
		}

		///
		pub fn insert_break(
			key_config: &SharedKeyConfig,
		) -> CommandText {
			CommandText::new(
				format!(
					"Break [{}]",
					key_config.get_hint(key_config.keys.rebase_break)
				),
				"insert a deliberate stop before the selection",
				CMD_GROUP_REBASE,
			)
			.with_binding(key_config.keys.rebase_break)
		}

		///
		pub fn insert_exec(
			key_config: &SharedKeyConfig,
		) -> CommandText {
			CommandText::new(
				format!(
					"Exec [{}]",
					key_config.get_hint(key_config.keys.rebase_exec)
				),
				"insert a shell command before the selection",
				CMD_GROUP_REBASE,
			)
			.with_binding(key_config.keys.rebase_exec)
		}

		///
		pub fn compose(key_config: &SharedKeyConfig) -> CommandText {
			CommandText::new(
				format!(
					"Message [{}]",
					key_config
						.get_hint(key_config.keys.rebase_compose)
				),
				"compose the selected reword/squash message",
				CMD_GROUP_REBASE,
			)
			.with_binding(key_config.keys.rebase_compose)
		}

		///
		pub fn undo(key_config: &SharedKeyConfig) -> CommandText {
			CommandText::new(
				format!(
					"Undo [{}]",
					key_config.get_hint(key_config.keys.rebase_undo)
				),
				"undo the last plan edit",
				CMD_GROUP_REBASE,
			)
			.with_binding(key_config.keys.rebase_undo)
		}

		///
		pub fn redo(key_config: &SharedKeyConfig) -> CommandText {
			CommandText::new(
				format!(
					"Redo [{}]",
					key_config.get_hint(key_config.keys.rebase_redo)
				),
				"redo an undone plan edit",
				CMD_GROUP_REBASE,
			)
			.with_binding(key_config.keys.rebase_redo)
		}

		///
		pub fn move_steps(
			key_config: &SharedKeyConfig,
		) -> CommandText {
			CommandText::new(
				format!(
					"Move [{}{}]",
					key_config.get_hint(key_config.keys.shift_up),
					key_config.get_hint(key_config.keys.shift_down),
				),
				"move the selected step up or down",
				CMD_GROUP_REBASE,
			)
		}

		///
		pub fn toggle_inspector(
			key_config: &SharedKeyConfig,
		) -> CommandText {
			CommandText::new(
				format!(
					"Details [{}]",
					key_config
						.get_hint(key_config.keys.rebase_inspector)
				),
				"toggle the inspector panel",
				CMD_GROUP_REBASE,
			)
			.with_binding(key_config.keys.rebase_inspector)
		}

		///
		pub fn scroll(key_config: &SharedKeyConfig) -> CommandText {
			CommandText::new(
				format!(
					"Scroll [{}{}]",
					key_config.get_hint(key_config.keys.move_up),
					key_config.get_hint(key_config.keys.move_down),
				),
				"scroll through the plan",
				CMD_GROUP_REBASE,
			)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::keys::{KeyConfig, Keymap};
	use std::rc::Rc;

	#[test]
	fn test_spaces_are_replaced_by_dashes_in_branch_name() {
		let input = "feature/auto replace spaces in branch name";
		let output: String =
			input.chars().map(normalize_branch_name_char).collect();
		assert_eq!(
			output,
			"feature/auto-replace-spaces-in-branch-name"
		);
	}

	#[test]
	fn modal_help_commands_include_the_active_executable_bindings() {
		let key_config =
			Rc::new(KeyConfig::with_keymap(Keymap::Helix));
		let cases = [
			(
				commands::selection_mode(&key_config),
				key_config.keys.select_mode,
			),
			(
				commands::selection_clear(&key_config),
				key_config.keys.select_clear,
			),
			(
				commands::selection_all(&key_config),
				key_config.keys.select_all,
			),
			(
				commands::selection_item(&key_config),
				key_config.keys.select_item,
			),
			(
				commands::focus_next(&key_config),
				key_config.keys.focus_next,
			),
			(
				commands::focus_previous(&key_config),
				key_config.keys.focus_previous,
			),
			(
				commands::command_palette(&key_config),
				key_config.keys.command_palette,
			),
			(
				commands::goto_prefix(&key_config),
				key_config.keys.goto_prefix,
			),
			(
				commands::action_prefix(&key_config),
				key_config.keys.action_prefix,
			),
			(
				commands::stage_item(&key_config),
				key_config.keys.stage_unstage_item,
			),
			(
				commands::unstage_item(&key_config),
				key_config.keys.status_unstage_item,
			),
			(
				commands::commit_open_inline(&key_config),
				key_config.keys.open_commit_inline,
			),
			(
				commands::branches_create(&key_config),
				key_config.keys.branches_create,
			),
			(
				commands::branches_set_base(&key_config),
				key_config.keys.branches_set_base,
			),
			(
				commands::branches_load_more(&key_config),
				key_config.keys.branches_load_more,
			),
		];
		for (command, expected) in cases {
			assert_eq!(command.binding, Some(expected));
		}
	}
}
