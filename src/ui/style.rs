use crate::ui::syntax_text::DEFAULT_SYNTAX_THEME;
use anyhow::Result;
use asyncgit::{DiffLineType, StatusItemType};
use ratatui::style::{Color, Modifier, Style};
use ron::ser::{to_string_pretty, PrettyConfig};
use serde::{Deserialize, Serialize};
use std::{fs::File, io::Write, path::PathBuf, rc::Rc};
use struct_patch::Patch;

pub type SharedTheme = Rc<Theme>;

#[derive(Serialize, Deserialize, Debug, Clone, Patch)]
#[patch(attribute(derive(Serialize, Deserialize)))]
pub struct Theme {
	selected_tab: Color,
	command_fg: Color,
	selection_bg: Color,
	selection_fg: Color,
	use_selection_fg: bool,
	cmdbar_bg: Color,
	disabled_fg: Color,
	diff_line_add: Color,
	diff_line_delete: Color,
	diff_file_added: Color,
	diff_file_removed: Color,
	diff_file_moved: Color,
	diff_file_modified: Color,
	commit_hash: Color,
	commit_time: Color,
	commit_author: Color,
	danger_fg: Color,
	push_gauge_bg: Color,
	push_gauge_fg: Color,
	tag_fg: Color,
	branch_fg: Color,
	line_break: String,
	block_title_focused: Color,
	syntax: String,
}

impl Theme {
	pub fn scroll_bar_pos(&self) -> Style {
		Style::default().fg(self.selection_bg)
	}

	pub fn block(&self, focus: bool) -> Style {
		if focus {
			Style::default()
		} else {
			Style::default().fg(self.disabled_fg)
		}
	}

	pub fn title(&self, focused: bool) -> Style {
		if focused {
			Style::default()
				.fg(self.block_title_focused)
				.add_modifier(Modifier::BOLD)
		} else {
			Style::default().fg(self.disabled_fg)
		}
	}

	pub fn branch(&self, selected: bool, head: bool) -> Style {
		let branch = if head {
			Style::default().add_modifier(Modifier::BOLD)
		} else {
			Style::default()
		}
		.fg(self.branch_fg);

		if selected {
			branch.patch(Style::default().bg(self.selection_bg))
		} else {
			branch
		}
	}

	pub fn tab(&self, selected: bool) -> Style {
		if selected {
			self.text(true, false)
				.fg(self.selected_tab)
				.add_modifier(Modifier::UNDERLINED)
		} else {
			self.text(false, false)
		}
	}

	pub fn tags(&self, selected: bool) -> Style {
		Style::default()
			.fg(self.tag_fg)
			.add_modifier(Modifier::BOLD)
			.bg(if selected {
				self.selection_bg
			} else {
				Color::Reset
			})
	}

	pub fn text(&self, enabled: bool, selected: bool) -> Style {
		match (enabled, selected) {
			(false, false) => Style::default().fg(self.disabled_fg),
			(false, true) => Style::default().bg(self.selection_bg),
			(true, false) => Style::default(),
			(true, true) => Style::default()
				.fg(self.command_fg)
				.bg(self.selection_bg),
		}
	}

	pub fn item(&self, typ: StatusItemType, selected: bool) -> Style {
		let style = match typ {
			StatusItemType::New => {
				Style::default().fg(self.diff_file_added)
			}
			StatusItemType::Modified => {
				Style::default().fg(self.diff_file_modified)
			}
			StatusItemType::Deleted => {
				Style::default().fg(self.diff_file_removed)
			}
			StatusItemType::Renamed => {
				Style::default().fg(self.diff_file_moved)
			}
			StatusItemType::Conflicted => Style::default()
				.fg(self.diff_file_modified)
				.add_modifier(Modifier::BOLD),
			StatusItemType::Typechange => Style::default(),
		};

		self.apply_select(style, selected)
	}

	pub fn file_tree_item(
		&self,
		is_folder: bool,
		selected: bool,
	) -> Style {
		let style = if is_folder {
			Style::default()
		} else {
			Style::default().fg(self.diff_file_modified)
		};

		self.apply_select(style, selected)
	}

	const fn apply_select(
		&self,
		style: Style,
		selected: bool,
	) -> Style {
		if selected {
			if self.use_selection_fg {
				style.bg(self.selection_bg).fg(self.selection_fg)
			} else {
				style.bg(self.selection_bg)
			}
		} else {
			style
		}
	}

	/// Applies the selection background (and selection
	/// foreground, when enabled) to an arbitrary base style.
	/// Lets composed styles like the rebase rail glyphs
	/// participate in the standard selection look without new
	/// theme roles.
	pub const fn selected(
		&self,
		style: Style,
		selected: bool,
	) -> Style {
		self.apply_select(style, selected)
	}

	pub fn option(&self, on: bool) -> Style {
		if on {
			Style::default().fg(self.diff_line_add)
		} else {
			Style::default().fg(self.diff_line_delete)
		}
	}

	pub fn diff_hunk_marker(&self, selected: bool) -> Style {
		if selected {
			Style::default().bg(self.selection_bg)
		} else {
			Style::default().fg(self.disabled_fg)
		}
	}

	pub fn diff_line(
		&self,
		typ: DiffLineType,
		selected: bool,
	) -> Style {
		let style = match typ {
			DiffLineType::Add => {
				Style::default().fg(self.diff_line_add)
			}
			DiffLineType::Delete => {
				Style::default().fg(self.diff_line_delete)
			}
			DiffLineType::Header => Style::default()
				.fg(self.disabled_fg)
				.add_modifier(Modifier::BOLD),
			DiffLineType::None => Style::default().fg(if selected {
				self.command_fg
			} else {
				Color::Reset
			}),
		};

		self.apply_select(style, selected)
	}

	/// Strong inline emphasis for graphemes changed within a paired patch row.
	/// Keep the accent derived from the existing diff colors so custom themes
	/// continue to control the semantic add/delete colors.
	pub fn diff_inline_changed(&self, typ: DiffLineType) -> Style {
		let accent = match typ {
			DiffLineType::Delete => self.diff_line_delete,
			_ => self.diff_line_add,
		};
		let background = match accent {
			Color::Rgb(r, g, b) => Color::Rgb(r / 3, g / 3, b / 3),
			Color::Indexed(index) => match index % 16 {
				1 | 9 => Color::Rgb(78, 32, 32),
				2 | 10 => Color::Rgb(20, 72, 36),
				3 | 11 => Color::Rgb(76, 64, 28),
				4 | 12 => Color::Rgb(30, 48, 80),
				5 | 13 => Color::Rgb(66, 32, 72),
				6 | 14 => Color::Rgb(22, 62, 70),
				_ => Color::Rgb(46, 46, 46),
			},
			Color::Red | Color::LightRed => Color::Rgb(78, 32, 32),
			Color::Green | Color::LightGreen => {
				Color::Rgb(20, 72, 36)
			}
			Color::Yellow | Color::LightYellow => {
				Color::Rgb(76, 64, 28)
			}
			Color::Blue | Color::LightBlue => Color::Rgb(30, 48, 80),
			Color::Magenta | Color::LightMagenta => {
				Color::Rgb(66, 32, 72)
			}
			Color::Cyan | Color::LightCyan => Color::Rgb(22, 62, 70),
			_ => Color::Rgb(46, 46, 46),
		};
		Style::default()
			.fg(accent)
			.bg(background)
			.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
	}

	/// Keeps an optional syntax foreground while adding the muted inline
	/// change background and selected-row treatment.
	pub fn diff_inline_emphasis(
		&self,
		typ: DiffLineType,
		syntax_foreground: Option<Color>,
		selected: bool,
		cursor: bool,
	) -> Style {
		let accent = match typ {
			DiffLineType::Delete => self.diff_line_delete,
			_ => self.diff_line_add,
		};
		let mut style = self
			.diff_inline_changed(typ)
			.fg(syntax_foreground.unwrap_or(accent));
		if cursor {
			style = style.add_modifier(Modifier::BOLD);
		}
		self.apply_select(style, selected)
	}

	/// Subdued style for the old/new line-number columns in a diff.
	pub fn diff_line_gutter(&self, selected: bool) -> Style {
		self.apply_select(
			Style::default().fg(self.disabled_fg),
			selected,
		)
	}

	/// Gives the focused diff row a quiet gutter cue independent of its
	/// add/delete color or any inline character-level emphasis.
	pub fn diff_line_gutter_cursor(
		&self,
		selected: bool,
		cursor: bool,
	) -> Style {
		let style = self.diff_line_gutter(selected);
		if cursor {
			style.add_modifier(Modifier::BOLD)
		} else {
			style
		}
	}

	/// Adds a stable cursor cue without changing configured diff colors.
	pub fn diff_line_cursor(
		&self,
		typ: DiffLineType,
		selected: bool,
	) -> Style {
		self.diff_line(typ, selected).add_modifier(Modifier::BOLD)
	}

	pub fn text_danger(&self) -> Style {
		Style::default().fg(self.danger_fg)
	}

	pub fn line_break(&self) -> String {
		self.line_break.clone()
	}

	pub fn commandbar(&self, enabled: bool) -> Style {
		if enabled {
			Style::default().fg(self.command_fg)
		} else {
			Style::default().fg(self.disabled_fg)
		}
		.bg(self.cmdbar_bg)
	}

	pub fn commit_hash(&self, selected: bool) -> Style {
		self.apply_select(
			Style::default().fg(self.commit_hash),
			selected,
		)
	}

	pub fn commit_unhighlighted(&self) -> Style {
		Style::default().fg(self.disabled_fg)
	}

	pub fn log_marker(&self, selected: bool) -> Style {
		let mut style = Style::default()
			.fg(self.commit_author)
			.add_modifier(Modifier::BOLD);

		style = self.apply_select(style, selected);

		style
	}

	pub fn commit_time(&self, selected: bool) -> Style {
		self.apply_select(
			Style::default().fg(self.commit_time),
			selected,
		)
	}

	pub fn commit_author(&self, selected: bool) -> Style {
		self.apply_select(
			Style::default().fg(self.commit_author),
			selected,
		)
	}

	pub fn commit_hash_in_blame(
		&self,
		is_blamed_commit: bool,
	) -> Style {
		if is_blamed_commit {
			Style::default()
				.fg(self.commit_hash)
				.add_modifier(Modifier::BOLD)
		} else {
			Style::default().fg(self.commit_hash)
		}
	}

	pub fn push_gauge(&self) -> Style {
		Style::default()
			.fg(self.push_gauge_fg)
			.bg(self.push_gauge_bg)
	}

	pub fn attention_block() -> Style {
		Style::default().fg(Color::Yellow)
	}

	fn load_patch(theme_path: &PathBuf) -> Result<ThemePatch> {
		let file = File::open(theme_path)?;

		Ok(ron::de::from_reader(file)?)
	}

	fn load_old_theme(theme_path: &PathBuf) -> Result<Self> {
		let old_file = File::open(theme_path)?;

		Ok(ron::de::from_reader::<File, Self>(old_file)?)
	}

	// This is supposed to be called when theme.ron doesn't already exists.
	fn save_patch(&self, theme_path: &PathBuf) -> Result<()> {
		let mut file = File::create(theme_path)?;
		let patch = self.clone().into_patch_by_diff(Self::default());
		let data = to_string_pretty(&patch, PrettyConfig::default())?;

		file.write_all(data.as_bytes())?;

		Ok(())
	}

	pub fn get_syntax(&self) -> String {
		self.syntax.clone()
	}

	pub fn init(theme_path: &PathBuf) -> Self {
		let mut theme = Self::default();

		if let Ok(patch) = Self::load_patch(theme_path).map_err(|e| {
			log::error!("theme error [{theme_path:?}]: {e}");
			e
		}) {
			theme.apply(patch);
		} else if let Ok(old_theme) = Self::load_old_theme(theme_path)
		{
			theme = old_theme;

			if theme.save_patch(theme_path).is_ok() {
				log::info!(
					"Converted old theme to new format. ({theme_path:?})"
				);
			} else {
				log::warn!(
					"Failed to save theme in new format. ({theme_path:?})"
				);
			}
		}

		theme
	}
}

impl Default for Theme {
	fn default() -> Self {
		Self {
			selected_tab: Color::Reset,
			command_fg: Color::White,
			selection_bg: Color::Blue,
			selection_fg: Color::White,
			use_selection_fg: true,
			cmdbar_bg: Color::Blue,
			disabled_fg: Color::DarkGray,
			diff_line_add: Color::Green,
			diff_line_delete: Color::Red,
			diff_file_added: Color::LightGreen,
			diff_file_removed: Color::LightRed,
			diff_file_moved: Color::LightMagenta,
			diff_file_modified: Color::Yellow,
			commit_hash: Color::Magenta,
			commit_time: Color::LightCyan,
			commit_author: Color::Green,
			danger_fg: Color::Red,
			push_gauge_bg: Color::Blue,
			push_gauge_fg: Color::Reset,
			tag_fg: Color::LightMagenta,
			branch_fg: Color::LightYellow,
			line_break: "¶".to_string(),
			block_title_focused: Color::Reset,
			// Available themes can be found in:
			// [ThemeSet::load_defaults function](https://github.com/trishume/syntect/blob/7fe13c0fd53cdfa0f9fea1aa14c5ba37f81d8b71/src/dumps.rs#L215).
			syntax: DEFAULT_SYNTAX_THEME.to_string(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use pretty_assertions::assert_eq;
	use tempfile::NamedTempFile;

	#[test]
	fn test_smoke() {
		let _ = env_logger::builder()
			.is_test(true)
			.filter_level(log::LevelFilter::Trace)
			.try_init();

		let mut file = NamedTempFile::new().unwrap();

		writeln!(
			file,
			r##"
(
	selection_bg: Some("Black"),
	selection_fg: Some("#ffffff"),
	use_selection_fg: Some(false),
	syntax: Some("InspiredGitHub")
)
"##
		)
		.unwrap();

		let theme = Theme::init(&file.path().to_path_buf());

		assert_eq!(theme.selected_tab, Theme::default().selected_tab);

		assert_ne!(theme.selection_bg, Theme::default().selection_bg);
		assert_ne!(theme.syntax, Theme::default().syntax);
		assert_eq!(theme.selection_bg, Color::Black);
		assert_eq!(theme.selection_fg, Color::Rgb(255, 255, 255));
		assert_eq!(theme.syntax, "InspiredGitHub");
	}
}
