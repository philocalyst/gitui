use anyhow::Result;
use asyncgit::sync::{
	diff::DiffOptions, repo_dir, RepoPathRef,
	ShowUntrackedFilesConfig,
};
use ron::{
	de::from_bytes,
	ser::{to_string_pretty, PrettyConfig},
};
use serde::{Deserialize, Serialize};
use std::{
	cell::RefCell,
	fs::{self, File},
	io::{Read, Write},
	path::PathBuf,
	rc::Rc,
};

/// Per-repository native layout for read-only diff reviews.
#[derive(
	Default, Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq,
)]
pub enum DiffPreviewPreference {
	#[serde(alias = "Difftastic", alias = "Delta")]
	#[default]
	Auto,
	Unified,
}

impl From<DiffPreviewPreference>
	for asyncgit::diff_preview::PreviewPreference
{
	fn from(value: DiffPreviewPreference) -> Self {
		match value {
			DiffPreviewPreference::Auto => Self::Auto,
			DiffPreviewPreference::Unified => Self::Unified,
		}
	}
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct OptionsData {
	pub tab: usize,
	pub diff: DiffOptions,
	pub status_show_untracked: Option<ShowUntrackedFilesConfig>,
	pub commit_msgs: Vec<String>,
	pub diff_preview_preference: DiffPreviewPreference,
}

const COMMIT_MSG_HISTORY_LENGTH: usize = 20;

#[derive(Clone)]
pub struct Options {
	repo: RepoPathRef,
	data: OptionsData,
}

#[cfg(test)]
impl Options {
	pub fn test_env() -> Self {
		use asyncgit::sync::RepoPath;
		Self {
			repo: RefCell::new(RepoPath::Path(Default::default())),
			data: Default::default(),
		}
	}
}

pub type SharedOptions = Rc<RefCell<Options>>;

impl Options {
	pub fn new(repo: RepoPathRef) -> SharedOptions {
		let data = match Self::read(&repo) {
			Ok(data) => data,
			Err(error) => {
				log::error!("options read error: {error}");
				OptionsData::default()
			}
		};
		Rc::new(RefCell::new(Self { repo, data }))
	}

	pub fn set_current_tab(&mut self, tab: usize) {
		self.data.tab = tab;
		self.save();
	}

	pub const fn current_tab(&self) -> usize {
		self.data.tab
	}

	pub const fn diff_options(&self) -> DiffOptions {
		self.data.diff
	}

	pub const fn diff_preview_preference(
		&self,
	) -> DiffPreviewPreference {
		self.data.diff_preview_preference
	}

	pub fn set_diff_preview_preference(
		&mut self,
		preference: DiffPreviewPreference,
	) {
		self.data.diff_preview_preference = preference;
		self.save();
	}

	pub const fn status_show_untracked(
		&self,
	) -> Option<ShowUntrackedFilesConfig> {
		self.data.status_show_untracked
	}

	pub fn set_status_show_untracked(
		&mut self,
		value: Option<ShowUntrackedFilesConfig>,
	) {
		self.data.status_show_untracked = value;
		self.save();
	}

	pub fn diff_context_change(&mut self, increase: bool) {
		self.data.diff.context = if increase {
			self.data.diff.context.saturating_add(1)
		} else {
			self.data.diff.context.saturating_sub(1)
		};

		self.save();
	}

	pub fn diff_hunk_lines_change(&mut self, increase: bool) {
		self.data.diff.interhunk_lines = if increase {
			self.data.diff.interhunk_lines.saturating_add(1)
		} else {
			self.data.diff.interhunk_lines.saturating_sub(1)
		};

		self.save();
	}

	pub fn diff_toggle_whitespace(&mut self) {
		self.data.diff.ignore_whitespace =
			!self.data.diff.ignore_whitespace;

		self.save();
	}

	pub fn add_commit_msg(&mut self, msg: &str) {
		self.data.commit_msgs.push(msg.to_owned());
		while self.data.commit_msgs.len() > COMMIT_MSG_HISTORY_LENGTH
		{
			self.data.commit_msgs.remove(0);
		}
		self.save();
	}

	pub const fn has_commit_msg_history(&self) -> bool {
		!self.data.commit_msgs.is_empty()
	}

	pub fn commit_msg(&self, idx: usize) -> Option<String> {
		if self.data.commit_msgs.is_empty() {
			None
		} else {
			let entries = self.data.commit_msgs.len();
			let mut index = idx;

			while index >= entries {
				index -= entries;
			}

			index = entries.saturating_sub(1) - index;

			Some(self.data.commit_msgs[index].clone())
		}
	}

	fn save(&self) {
		if let Err(e) = self.save_failable() {
			log::error!("options save error: {e}");
		}
	}

	fn read(repo: &RepoPathRef) -> Result<OptionsData> {
		let path = Self::options_file(repo)?;
		if path.exists() {
			return Self::read_file(&path);
		}

		let legacy_path = Self::legacy_options_file(repo)?;
		if !legacy_path.is_file() {
			return Ok(OptionsData::default());
		}

		let data = match Self::read_file(&legacy_path) {
			Ok(data) => data,
			Err(error) => {
				let backup = Self::move_legacy_file(&legacy_path)?;
				log::error!(
					"invalid legacy options retained at {}: {error}",
					backup.display()
				);
				fs::create_dir_all(path.parent().ok_or_else(
					|| anyhow::anyhow!("options path has no parent"),
				)?)?;
				return Ok(OptionsData::default());
			}
		};

		let backup = Self::move_legacy_file(&legacy_path)?;
		let parent = path.parent().ok_or_else(|| {
			anyhow::anyhow!("options path has no parent")
		})?;
		if let Err(error) = fs::create_dir_all(parent) {
			let _ = fs::rename(&backup, &legacy_path);
			return Err(error.into());
		}
		if let Err(error) = fs::rename(&backup, &path) {
			log::error!(
				"could not move options to {}: {error}; legacy copy is retained at {}",
				path.display(),
				backup.display()
			);
		}

		Ok(data)
	}

	fn read_file(path: &std::path::Path) -> Result<OptionsData> {
		let mut f = File::open(path)?;
		let mut buffer = Vec::new();
		f.read_to_end(&mut buffer)?;
		Ok(from_bytes(&buffer)?)
	}

	fn save_failable(&self) -> Result<()> {
		let path = Self::options_file(&self.repo)?;
		fs::create_dir_all(path.parent().ok_or_else(|| {
			anyhow::anyhow!("options path has no parent")
		})?)?;

		let mut file = File::create(path)?;
		let data =
			to_string_pretty(&self.data, PrettyConfig::default())?;
		file.write_all(data.as_bytes())?;

		Ok(())
	}

	fn options_file(repo: &RepoPathRef) -> Result<PathBuf> {
		let dir = repo_dir(&repo.borrow())?;
		Ok(dir.join("gitui").join("options.ron"))
	}

	fn legacy_options_file(repo: &RepoPathRef) -> Result<PathBuf> {
		let dir = repo_dir(&repo.borrow())?;
		Ok(dir.join("gitui"))
	}

	fn move_legacy_file(path: &std::path::Path) -> Result<PathBuf> {
		let mut backup = path.with_extension("legacy-options");
		let mut suffix = 0usize;
		while backup.exists() {
			suffix += 1;
			backup = path
				.with_extension(format!("legacy-options-{suffix}"));
		}
		fs::rename(path, &backup)?;
		Ok(backup)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use asyncgit::sync::RepoPath;

	fn repo_paths() -> (tempfile::TempDir, RepoPathRef, PathBuf) {
		let (temp, git_repo) = git2_testing::repo_init_empty();
		let git_dir = git_repo.path().to_path_buf();
		let repo = RepoPath::Path(temp.path().to_path_buf());
		let repo = RefCell::new(repo);
		(temp, repo, git_dir)
	}

	#[test]
	fn old_options_ron_defaults_preview_renderer_without_rewrite() {
		let old = r#"(tab:1,diff:(context:3,interhunk_lines:2,ignore_whitespace:false),status_show_untracked:None,commit_msgs:[])"#;
		let data: OptionsData = from_bytes(old.as_bytes()).unwrap();
		assert_eq!(
			data.diff_preview_preference,
			DiffPreviewPreference::Auto
		);
		assert_eq!(data.tab, 1);
		assert_eq!(data.diff.context, 3);
	}

	#[test]
	fn old_difftastic_preview_choice_migrates_to_native_layout() {
		let old = r#"(diff_preview_preference:Difftastic)"#;
		let data: OptionsData = from_bytes(old.as_bytes()).unwrap();
		assert_eq!(
			data.diff_preview_preference,
			DiffPreviewPreference::Auto
		);
	}

	#[test]
	fn old_delta_preview_choice_migrates_to_native_layout() {
		let old = r#"(tab:2,diff:(context:4,interhunk_lines:1,ignore_whitespace:true),status_show_untracked:None,commit_msgs:["kept"],diff_preview_preference:Delta)"#;
		let data: OptionsData = from_bytes(old.as_bytes()).unwrap();
		assert_eq!(
			data.diff_preview_preference,
			DiffPreviewPreference::Auto
		);
		assert_eq!(data.tab, 2);
		assert_eq!(data.diff.context, 4);
		assert_eq!(data.commit_msgs, vec!["kept"]);
	}

	#[test]
	fn migrates_valid_legacy_options_before_rebase_journal_directory()
	{
		let (_temp, repo, git_dir) = repo_paths();
		let legacy = git_dir.join("gitui");
		let data = OptionsData {
			tab: 4,
			diff: DiffOptions {
				context: 17,
				..DiffOptions::default()
			},
			status_show_untracked: None,
			commit_msgs: vec![String::from("saved")],
			diff_preview_preference: DiffPreviewPreference::Auto,
		};
		fs::write(
			&legacy,
			to_string_pretty(&data, PrettyConfig::default()).unwrap(),
		)
		.unwrap();

		let shared =
			Options::new(RefCell::new(repo.borrow().clone()));
		let options = shared.borrow();
		assert_eq!(options.current_tab(), 4);
		assert_eq!(options.diff_options().context, 17);
		assert_eq!(options.commit_msg(0).as_deref(), Some("saved"));
		assert!(git_dir.join("gitui").is_dir());
		assert!(git_dir.join("gitui/options.ron").is_file());
		assert!(!git_dir.join("gitui.legacy-options").exists());
	}

	#[test]
	fn retains_invalid_legacy_options_and_creates_journal_parent() {
		let (_temp, repo, git_dir) = repo_paths();
		let legacy = git_dir.join("gitui");
		let invalid = b"this is not RON";
		fs::write(&legacy, invalid).unwrap();

		let _options =
			Options::new(RefCell::new(repo.borrow().clone()));
		assert!(git_dir.join("gitui").is_dir());
		let backup = git_dir.join("gitui.legacy-options");
		assert_eq!(fs::read(backup).unwrap(), invalid);
		assert!(!git_dir.join("gitui/options.ron").exists());
	}
}
