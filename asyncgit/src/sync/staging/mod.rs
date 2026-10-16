mod discard_tracked;
mod stage_tracked;

pub use discard_tracked::{
	discard_lines, discard_lines_checked,
	discard_lines_with_pairs_checked,
};
pub use stage_tracked::clean_worktree_contents;
pub use stage_tracked::{
	stage_lines, stage_lines_checked, stage_lines_with_pairs_checked,
};

use super::{
	diff::DiffLinePosition, patches::HunkLines, utils::work_dir,
};
use crate::error::Result;
use git2::{DiffLine, DiffLineType, Repository};
use std::{
	collections::{HashMap, HashSet},
	fs::File,
	io::Read,
};

const NEWLINE: char = '\n';

type PatchLineMap<'patch> =
	HashMap<DiffLinePosition, &'patch DiffLine<'patch>>;
type InsertionSourceMap = HashMap<DiffLinePosition, usize>;
type PairedReplacementMap<'patch> =
	HashMap<u32, &'patch DiffLine<'patch>>;
type SelectedInsertions<'patch> =
	HashMap<usize, Vec<(u32, &'patch DiffLine<'patch>)>>;

/// The deleted and added raw diff rows represented by one compact line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiffLineReplacement {
	pub old: DiffLinePosition,
	pub new: DiffLinePosition,
}

#[derive(Default)]
struct NewFromOldContent {
	lines: Vec<String>,
	line_ends: Vec<bool>,
	old_index: usize,
}

impl NewFromOldContent {
	fn push_line(&mut self, line: &str) {
		let ends_with_newline = line.ends_with(NEWLINE);
		let line = line.strip_suffix(NEWLINE).unwrap_or(line);
		self.lines.push(line.to_string());
		self.line_ends.push(ends_with_newline);
	}

	fn add_from_hunk(&mut self, line: &DiffLine) -> Result<()> {
		let line = String::from_utf8(line.content().into())?;
		self.push_line(&line);

		Ok(())
	}

	const fn skip_old_line(&mut self) {
		self.old_index += 1;
	}

	fn add_old_line(&mut self, old_lines: &[&str]) {
		if let Some(line) = old_lines.get(self.old_index) {
			self.push_line(line);
			self.old_index += 1;
		}
	}

	fn catchup_to_hunkstart(
		&mut self,
		hunk_start: usize,
		old_lines: &[&str],
	) {
		while hunk_start > self.old_index + 1
			&& self.old_index < old_lines.len()
		{
			self.add_old_line(old_lines);
		}
	}

	fn finish(mut self, old_lines: &[&str]) -> String {
		for line in old_lines.iter().skip(self.old_index) {
			self.push_line(line);
		}
		let mut content = String::new();
		for (line, ends_with_newline) in
			self.lines.iter().zip(self.line_ends)
		{
			content.push_str(line);
			if ends_with_newline {
				content.push(NEWLINE);
			}
		}
		content
	}
}

// this is the heart of the per line discard,stage,unstage. heavily inspired by the great work in
// nodegit: https://github.com/nodegit/nodegit
pub fn apply_selection(
	lines: &[DiffLinePosition],
	hunks: &[HunkLines],
	old_lines: &[&str],
	is_staged: bool,
	reverse: bool,
) -> Result<String> {
	let mut new_content = NewFromOldContent::default();
	let lines = lines.iter().collect::<HashSet<_>>();

	let added = if reverse {
		DiffLineType::Deletion
	} else {
		DiffLineType::Addition
	};
	let deleted = if reverse {
		DiffLineType::Addition
	} else {
		DiffLineType::Deletion
	};

	let mut first_hunk_encountered = false;
	for hunk in hunks {
		let hunk_start = if is_staged || reverse {
			usize::try_from(hunk.hunk.new_start)?
		} else {
			usize::try_from(hunk.hunk.old_start)?
		};

		if !first_hunk_encountered {
			let any_selection_in_hunk =
				hunk.lines.iter().any(|line| {
					let line: DiffLinePosition = line.into();
					lines.contains(&line)
				});

			first_hunk_encountered = any_selection_in_hunk;
		}

		if first_hunk_encountered {
			new_content.catchup_to_hunkstart(hunk_start, old_lines);

			for hunk_line in &hunk.lines {
				let hunk_line_pos: DiffLinePosition =
					hunk_line.into();
				let selected_line = lines.contains(&hunk_line_pos);

				log::debug!(
					// println!(
					"{} line: {} [{:?} old, {:?} new] -> {}",
					if selected_line { "*" } else { " " },
					hunk_line.origin(),
					hunk_line.old_lineno(),
					hunk_line.new_lineno(),
					String::from_utf8_lossy(hunk_line.content())
						.trim()
				);

				if hunk_line.origin_value()
					== DiffLineType::DeleteEOFNL
					|| hunk_line.origin_value()
						== DiffLineType::AddEOFNL
				{
					break;
				}

				if (is_staged && !selected_line)
					|| (!is_staged && selected_line)
				{
					if hunk_line.origin_value() == added {
						new_content.add_from_hunk(hunk_line)?;
						if is_staged {
							new_content.skip_old_line();
						}
					} else if hunk_line.origin_value() == deleted {
						if !is_staged {
							new_content.skip_old_line();
						}
					} else {
						new_content.add_old_line(old_lines);
					}
				} else {
					if hunk_line.origin_value() != added {
						new_content.add_from_hunk(hunk_line)?;
					}

					if (is_staged
						&& hunk_line.origin_value() != deleted)
						|| (!is_staged
							&& hunk_line.origin_value() != added)
					{
						new_content.skip_old_line();
					}
				}
			}
		}
	}

	Ok(new_content.finish(old_lines))
}

/// Apply an explicit compact replacement selection while anchoring each
/// replacement at the selected source line. Ordinary line selections retain
/// their historical patch-order behavior through [`apply_selection`].
pub(super) fn apply_selection_with_pairs(
	lines: &[DiffLinePosition],
	pairs: &[DiffLineReplacement],
	hunks: &[HunkLines],
	source_lines: &[&str],
	is_staged: bool,
	reverse: bool,
) -> Result<String> {
	if pairs.is_empty() {
		return apply_selection(
			lines,
			hunks,
			source_lines,
			is_staged,
			reverse,
		);
	}
	validate_selected_pairs(lines, pairs, hunks)?;

	let source_is_new = is_staged || reverse;
	let (diff_lines, insertion_sources) = build_patch_line_maps(
		hunks,
		pairs,
		source_is_new,
		source_lines.len(),
	)?;
	let (replacements, paired_positions) =
		build_paired_replacements(pairs, &diff_lines, source_is_new)?;
	let (removed, insertions) = build_selected_line_operations(
		lines,
		&paired_positions,
		&diff_lines,
		&insertion_sources,
		source_is_new,
	)?;
	render_paired_selection(
		source_lines,
		&replacements,
		&removed,
		insertions,
	)
}

fn build_patch_line_maps<'patch>(
	hunks: &'patch [HunkLines<'patch>],
	pairs: &[DiffLineReplacement],
	source_is_new: bool,
	source_line_count: usize,
) -> Result<(PatchLineMap<'patch>, InsertionSourceMap)> {
	let mut diff_lines = HashMap::new();
	let mut insertion_sources = InsertionSourceMap::new();
	for line in hunks.iter().flat_map(|hunk| &hunk.lines) {
		if matches!(
			line.origin_value(),
			DiffLineType::Addition | DiffLineType::Deletion
		) {
			diff_lines.insert(DiffLinePosition::from(line), line);
		}
	}
	for hunk in hunks {
		let mut old_cursor = hunk.hunk.old_start.saturating_sub(1);
		let mut new_cursor = hunk.hunk.new_start.saturating_sub(1);
		for line in &hunk.lines {
			let position = DiffLinePosition::from(line);
			match line.origin_value() {
				DiffLineType::Context
				| DiffLineType::ContextEOFNL => {
					old_cursor = old_cursor.saturating_add(1);
					new_cursor = new_cursor.saturating_add(1);
				}
				DiffLineType::Deletion => {
					if source_is_new {
						insertion_sources.insert(
							position,
							usize::try_from(
								new_cursor.saturating_add(1),
							)?,
						);
					}
					old_cursor = old_cursor.saturating_add(1);
				}
				DiffLineType::Addition => {
					if !source_is_new {
						insertion_sources.insert(
							position,
							usize::try_from(
								old_cursor.saturating_add(1),
							)?,
						);
					}
					new_cursor = new_cursor.saturating_add(1);
				}
				_ => {}
			}
		}
		let mut block = Vec::new();
		for line in &hunk.lines {
			match line.origin_value() {
				DiffLineType::Context
				| DiffLineType::ContextEOFNL => {
					add_block_alignment_anchors(
						&block,
						source_is_new,
						pairs,
						source_line_count,
						&mut insertion_sources,
					);
					block.clear();
				}
				DiffLineType::Addition | DiffLineType::Deletion => {
					block.push(line);
				}
				_ => {}
			}
		}
		add_block_alignment_anchors(
			&block,
			source_is_new,
			pairs,
			source_line_count,
			&mut insertion_sources,
		);
	}
	Ok((diff_lines, insertion_sources))
}

fn build_paired_replacements<'patch>(
	pairs: &[DiffLineReplacement],
	diff_lines: &PatchLineMap<'patch>,
	source_is_new: bool,
) -> Result<(PairedReplacementMap<'patch>, HashSet<DiffLinePosition>)>
{
	let mut replacements = PairedReplacementMap::new();
	let mut paired_positions = HashSet::new();
	for pair in pairs {
		let old_line =
			diff_lines.get(&pair.old).ok_or_else(|| {
				crate::error::Error::Generic(
					"paired old line is no longer present"
						.to_string(),
				)
			})?;
		let new_line =
			diff_lines.get(&pair.new).ok_or_else(|| {
				crate::error::Error::Generic(
					"paired new line is no longer present"
						.to_string(),
				)
			})?;
		paired_positions.insert(pair.old);
		paired_positions.insert(pair.new);
		let (source_position, replacement) = if source_is_new {
			(pair.new, *old_line)
		} else {
			(pair.old, *new_line)
		};
		let source_lineno = if source_is_new {
			source_position.new_lineno
		} else {
			source_position.old_lineno
		}
		.ok_or_else(|| {
			crate::error::Error::Generic(
				"paired source line has no line number".to_string(),
			)
		})?;
		replacements.insert(source_lineno, replacement);
	}
	Ok((replacements, paired_positions))
}

fn build_selected_line_operations<'patch>(
	lines: &[DiffLinePosition],
	paired_positions: &HashSet<DiffLinePosition>,
	diff_lines: &PatchLineMap<'patch>,
	insertion_sources: &InsertionSourceMap,
	source_is_new: bool,
) -> Result<(HashSet<u32>, SelectedInsertions<'patch>)> {
	let mut removed = HashSet::<u32>::new();
	let mut insertions = SelectedInsertions::new();
	for position in lines {
		if paired_positions.contains(position) {
			continue;
		}
		let line = diff_lines.get(position).ok_or_else(|| {
			crate::error::Error::Generic(
				"selected line is no longer present".to_string(),
			)
		})?;
		let is_source_line = if source_is_new {
			line.origin_value() == DiffLineType::Addition
		} else {
			line.origin_value() == DiffLineType::Deletion
		};
		if is_source_line {
			let lineno = if source_is_new {
				position.new_lineno
			} else {
				position.old_lineno
			}
			.ok_or_else(|| {
				crate::error::Error::Generic(
					"selected source line has no line number"
						.to_string(),
				)
			})?;
			removed.insert(lineno);
		} else {
			let source_lineno = *insertion_sources
				.get(position)
				.ok_or_else(|| {
					crate::error::Error::Generic(
						"selected target row has no source insertion point"
							.to_string(),
					)
				})?;
			let target_lineno = if source_is_new {
				position.old_lineno
			} else {
				position.new_lineno
			}
			.ok_or_else(|| {
				crate::error::Error::Generic(
					"selected target line has no line number"
						.to_string(),
				)
			})?;
			insertions
				.entry(source_lineno)
				.or_default()
				.push((target_lineno, *line));
		}
	}
	Ok((removed, insertions))
}

fn render_paired_selection(
	source_lines: &[&str],
	replacements: &PairedReplacementMap<'_>,
	removed: &HashSet<u32>,
	mut insertions: SelectedInsertions<'_>,
) -> Result<String> {
	let mut output = NewFromOldContent::default();
	for source_lineno in 1..=source_lines.len().saturating_add(1) {
		if let Some(lines) = insertions.get_mut(&source_lineno) {
			lines.sort_by_key(|(target_lineno, _)| *target_lineno);
			for (_, line) in lines {
				output.add_from_hunk(line)?;
			}
		}
		if source_lineno <= source_lines.len() {
			let source_lineno_u32 = u32::try_from(source_lineno)
				.map_err(|_| {
					crate::error::Error::Generic(
						"file has too many lines".to_string(),
					)
				})?;
			if let Some(replacement) =
				replacements.get(&source_lineno_u32)
			{
				output.add_from_hunk(replacement)?;
				output.skip_old_line();
			} else if removed.contains(&source_lineno_u32) {
				output.skip_old_line();
			} else {
				output.add_old_line(source_lines);
			}
		}
	}
	if replacements.keys().any(|line| {
		usize::try_from(*line)
			.map_or(true, |line| line > source_lines.len())
	}) || removed.iter().any(|line| {
		usize::try_from(*line)
			.map_or(true, |line| line > source_lines.len())
	}) {
		return Err(crate::error::Error::Generic(
			"selected source line is no longer present".to_string(),
		));
	}
	Ok(output.finish(source_lines))
}

/// Improve insertion placement within one contiguous changed block by using
/// monotonic likely counterparts, while keeping explicitly selected pairs
/// authoritative. The unpaired path still uses the patch cursor fallback.
fn add_block_alignment_anchors(
	block: &[&DiffLine<'_>],
	source_is_new: bool,
	pairs: &[DiffLineReplacement],
	source_line_count: usize,
	insertion_sources: &mut InsertionSourceMap,
) {
	let mut anchors =
		block_alignment_anchors(block, source_is_new, pairs);
	if anchors.is_empty() {
		return;
	}
	anchors.sort_by_key(|(_, target_line)| *target_line);
	for line in block {
		let position = DiffLinePosition::from(*line);
		let target_line = if source_is_new {
			if line.origin_value() == DiffLineType::Deletion {
				position.old_lineno
			} else {
				None
			}
		} else if line.origin_value() == DiffLineType::Addition {
			position.new_lineno
		} else {
			None
		};
		let Some(target_line) = target_line else {
			continue;
		};
		let Some((source_anchor, target_anchor)) = anchors
			.iter()
			.min_by_key(|(_, anchor)| anchor.abs_diff(target_line))
			.copied()
		else {
			continue;
		};
		let translated = i64::from(source_anchor)
			+ i64::from(target_line)
			- i64::from(target_anchor);
		let maximum = i64::try_from(source_line_count)
			.unwrap_or(i64::MAX)
			.saturating_add(1);
		insertion_sources.insert(
			position,
			usize::try_from(translated.clamp(1, maximum))
				.unwrap_or(1),
		);
	}
}

fn block_alignment_anchors(
	block: &[&DiffLine<'_>],
	source_is_new: bool,
	pairs: &[DiffLineReplacement],
) -> Vec<(u32, u32)> {
	let deleted = block
		.iter()
		.copied()
		.filter(|line| line.origin_value() == DiffLineType::Deletion)
		.collect::<Vec<_>>();
	let added = block
		.iter()
		.copied()
		.filter(|line| line.origin_value() == DiffLineType::Addition)
		.collect::<Vec<_>>();
	if deleted.is_empty() || added.is_empty() {
		return Vec::new();
	}
	let changed_line_count =
		deleted.len().saturating_add(added.len());
	let changed_byte_count =
		deleted.iter().chain(&added).fold(0usize, |total, line| {
			total.saturating_add(line.content().len())
		});
	if changed_line_count > 32 || changed_byte_count > 4_096 {
		return Vec::new();
	}
	let old_text = deleted
		.iter()
		.map(|line| {
			String::from_utf8_lossy(line.content()).into_owned()
		})
		.collect::<Vec<_>>();
	let new_text = added
		.iter()
		.map(|line| {
			String::from_utf8_lossy(line.content()).into_owned()
		})
		.collect::<Vec<_>>();
	let old_refs =
		old_text.iter().map(String::as_str).collect::<Vec<_>>();
	let new_refs =
		new_text.iter().map(String::as_str).collect::<Vec<_>>();
	let mut anchors = crate::diff_preview::align_changed_lines(
		&old_refs, &new_refs,
	)
	.into_iter()
	.filter_map(|(old_index, new_index)| {
		let old_position = DiffLinePosition::from(deleted[old_index]);
		let new_position = DiffLinePosition::from(added[new_index]);
		if pairs.iter().any(|pair| {
			pair.old == old_position || pair.new == new_position
		}) {
			return None;
		}
		let (source_position, target_position) = if source_is_new {
			(new_position, old_position)
		} else {
			(old_position, new_position)
		};
		Some((
			source_position
				.old_lineno
				.or(source_position.new_lineno)?,
			target_position
				.old_lineno
				.or(target_position.new_lineno)?,
		))
	})
	.collect::<Vec<_>>();
	for pair in pairs {
		let (source_position, target_position) = if source_is_new {
			(pair.new, pair.old)
		} else {
			(pair.old, pair.new)
		};
		if block.iter().any(|line| {
			DiffLinePosition::from(*line) == source_position
		}) && block.iter().any(|line| {
			DiffLinePosition::from(*line) == target_position
		}) {
			if let (Some(source_line), Some(target_line)) = (
				source_position
					.old_lineno
					.or(source_position.new_lineno),
				target_position
					.old_lineno
					.or(target_position.new_lineno),
			) {
				anchors.push((source_line, target_line));
			}
		}
	}
	anchors
}

pub(super) fn validate_selected_pairs(
	selected: &[DiffLinePosition],
	pairs: &[DiffLineReplacement],
	hunks: &[HunkLines],
) -> Result<()> {
	let selected = selected.iter().copied().collect::<HashSet<_>>();
	let mut positions = HashMap::new();
	for (hunk_index, hunk) in hunks.iter().enumerate() {
		let mut change_run = 0usize;
		for (line_index, line) in hunk.lines.iter().enumerate() {
			match line.origin_value() {
				DiffLineType::Context
				| DiffLineType::ContextEOFNL => {
					change_run = change_run.saturating_add(1);
				}
				DiffLineType::Addition | DiffLineType::Deletion => {
					positions.insert(
						DiffLinePosition::from(line),
						(
							hunk_index,
							change_run,
							line_index,
							line.origin_value(),
						),
					);
				}
				_ => {}
			}
		}
	}
	let mut seen = HashSet::new();
	let mut previous = None;
	for pair in pairs {
		if !selected.contains(&pair.old)
			|| !selected.contains(&pair.new)
			|| !seen.insert(pair.old)
			|| !seen.insert(pair.new)
		{
			return Err(crate::error::Error::Generic(
				"replacement pairs must use unique selected diff rows".to_string(),
			));
		}
		let old = positions.get(&pair.old).ok_or_else(|| {
			crate::error::Error::Generic(
				"paired old row is not a deletion".to_string(),
			)
		})?;
		let new = positions.get(&pair.new).ok_or_else(|| {
			crate::error::Error::Generic(
				"paired new row is not an addition".to_string(),
			)
		})?;
		let replacement_is_valid = old.3 == DiffLineType::Deletion
			&& new.3 == DiffLineType::Addition
			&& old.0 == new.0
			&& old.1 == new.1
			&& old.2 < new.2
			&& !previous.is_some_and(
				|(old_hunk, old_position, new_hunk, new_position)| {
					(old.0, old.2) <= (old_hunk, old_position)
						|| (new.0, new.2) <= (new_hunk, new_position)
				},
			);
		if !replacement_is_valid {
			return Err(crate::error::Error::Generic(
				"replacement pairs must be monotonic rows in one contiguous change block".to_string(),
			));
		}
		previous = Some((old.0, old.2, new.0, new.2));
	}
	Ok(())
}

pub fn load_file(
	repo: &Repository,
	file_path: &str,
) -> Result<String> {
	let repo_path = work_dir(repo)?;
	let mut file = File::open(repo_path.join(file_path).as_path())?;
	let mut res = String::new();
	file.read_to_string(&mut res)?;

	Ok(res)
}

#[cfg(test)]
mod tests {
	use super::NewFromOldContent;

	// Regression for #2953: indexing old_lines past its length used to panic
	// in add_old_line / catchup_to_hunkstart when a hunk_start pointed past
	// the end of the working copy. The bounds-checked helpers must stop
	// catching up at end-of-buffer instead of panicking.
	#[test]
	fn catchup_to_hunkstart_past_end_does_not_panic() {
		let old_lines = ["a", "b", "c"];
		let mut content = NewFromOldContent::default();

		content.catchup_to_hunkstart(99, &old_lines);

		assert_eq!(content.old_index, old_lines.len());
		assert_eq!(content.lines, vec!["a", "b", "c"]);
	}

	#[test]
	fn add_old_line_at_end_is_noop() {
		let old_lines = ["only-line"];
		let mut content = NewFromOldContent::default();
		content.add_old_line(&old_lines);
		assert_eq!(content.old_index, 1);

		content.add_old_line(&old_lines);
		assert_eq!(content.old_index, 1);
		assert_eq!(content.lines, vec!["only-line"]);
	}
}
