//! Shared compact diff presentation geometry.
use asyncgit::diff_preview::{
	InlineReplacementBlock, InlineSegmentKind,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Old/new zero-based half-open source ranges for one AST region.
pub type StructuralLineRanges =
	(Option<Range<u32>>, Option<Range<u32>>);

/// One source-backed piece of a wrapped prose replacement line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockVisualPiece {
	pub kind: InlineSegmentKind,
	pub is_space: bool,
	pub old_range: Range<usize>,
	pub new_range: Range<usize>,
	pub old_line_index: Option<usize>,
	pub new_line_index: Option<usize>,
}

/// A single hard or soft visual line in a prose replacement block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockVisualLine {
	pub old_lineno: Option<u32>,
	pub new_lineno: Option<u32>,
	pub pieces: Vec<BlockVisualPiece>,
	pub width: usize,
}

/// Projects a bounded backend prose block to display lines. New-side hard
/// breaks determine paragraphs; deleted breaks become spaces. Long words
/// wrap only at Unicode grapheme boundaries. Source byte ranges and line
/// indexes are retained so callers can style and act on original patch rows.
#[must_use]
pub fn inline_block_visual_lines(
	block: &InlineReplacementBlock,
	available_width: usize,
) -> Vec<BlockVisualLine> {
	let mut output = vec![BlockVisualLine::default()];
	let wrap_width = available_width.max(1);
	for segment in &block.segments {
		let removed = segment.kind == InlineSegmentKind::Removed;
		let (source, range) = if removed {
			(&block.old_text, segment.old_range.clone())
		} else {
			(&block.new_text, segment.new_range.clone())
		};
		let Some(text) = source.get(range.clone()) else {
			continue;
		};
		let mut chunk_start = range.start;
		for (offset, _) in text.match_indices('\n') {
			let chunk_end = range.start + offset;
			append_block_source(
				&mut output,
				block,
				segment,
				chunk_start..chunk_end,
				wrap_width,
			);
			if removed {
				let internal_break = chunk_end + 1 < range.end;
				if needs_block_separator(
					&block.old_text,
					chunk_end,
					&block.new_text,
					segment.new_range.start,
					internal_break,
				) {
					append_block_space(
						&mut output,
						block,
						wrap_width,
					);
				}
			} else {
				output.push(BlockVisualLine::default());
			}
			chunk_start = chunk_end + 1;
		}
		append_block_source(
			&mut output,
			block,
			segment,
			chunk_start..range.end,
			wrap_width,
		);
	}
	if output.len() > 1
		&& output.last().is_some_and(|line| line.pieces.is_empty())
	{
		output.pop();
	}
	output
}

fn needs_block_separator(
	old_text: &str,
	old_position: usize,
	new_text: &str,
	new_position: usize,
	internal_break: bool,
) -> bool {
	let old_before = old_text
		.get(..old_position)
		.and_then(|prefix| prefix.chars().next_back());
	let old_after = old_text
		.get(old_position.saturating_add(1)..)
		.and_then(|suffix| suffix.chars().next());
	let new_before = new_text
		.get(..new_position)
		.and_then(|prefix| prefix.chars().next_back());
	let new_after = new_text
		.get(new_position..)
		.and_then(|suffix| suffix.chars().next());
	old_before.is_some_and(char::is_alphanumeric)
		&& old_after.is_some_and(char::is_alphanumeric)
		&& (internal_break
			|| (!new_before.is_some_and(char::is_whitespace)
				&& !new_after.is_some_and(char::is_whitespace)))
}

fn append_block_space(
	output: &mut Vec<BlockVisualLine>,
	block: &InlineReplacementBlock,
	wrap_width: usize,
) {
	let already_separated = output
		.last()
		.and_then(|line| line.pieces.last())
		.is_some_and(|piece| {
			if piece.is_space {
				return true;
			}
			let (source, range) =
				if piece.kind == InlineSegmentKind::Removed {
					(&block.old_text, &piece.old_range)
				} else {
					(&block.new_text, &piece.new_range)
				};
			source
				.get(range.clone())
				.and_then(|text| text.chars().next_back())
				.is_some_and(char::is_whitespace)
		});
	if already_separated {
		return;
	}
	let current = output.last_mut().expect("one visual line");
	if current.width >= wrap_width {
		output.push(BlockVisualLine::default());
	}
	let current = output.last_mut().expect("one visual line");
	current.pieces.push(BlockVisualPiece {
		kind: InlineSegmentKind::Shared,
		is_space: true,
		old_range: 0..0,
		new_range: 0..0,
		old_line_index: None,
		new_line_index: None,
	});
	current.width += 1;
}

fn append_block_source(
	output: &mut Vec<BlockVisualLine>,
	block: &InlineReplacementBlock,
	segment: &asyncgit::diff_preview::InlineSegment,
	range: Range<usize>,
	wrap_width: usize,
) {
	let kind = segment.kind;
	let old_side = kind == InlineSegmentKind::Removed;
	let chosen_start = if old_side {
		segment.old_range.start
	} else {
		segment.new_range.start
	};
	let source = if old_side {
		&block.old_text
	} else {
		&block.new_text
	};
	let Some(text) = source.get(range.clone()) else {
		return;
	};
	for (local_start, word) in text.split_word_bound_indices() {
		let word_range = range.start + local_start
			..range.start + local_start + word.len();
		let width = UnicodeWidthStr::width(word);
		let current_width =
			output.last().map_or(0, |line| line.width);
		if width <= wrap_width
			&& current_width > 0
			&& current_width.saturating_add(width) > wrap_width
		{
			output.push(BlockVisualLine::default());
		}
		if width > wrap_width {
			let mut chunk_start = word_range.start;
			let mut chunk_width = 0_usize;
			for (offset, grapheme) in word.grapheme_indices(true) {
				let grapheme_width = UnicodeWidthStr::width(grapheme);
				let line_width =
					output.last().map_or(0, |line| line.width);
				let grapheme_start = word_range.start + offset;
				if line_width.saturating_add(chunk_width) > 0
					&& line_width
						.saturating_add(chunk_width)
						.saturating_add(grapheme_width)
						> wrap_width
				{
					if chunk_start < grapheme_start {
						append_block_piece(
							output,
							block,
							segment,
							chunk_start..grapheme_start,
							chunk_start.saturating_sub(chosen_start),
						);
					}
					output.push(BlockVisualLine::default());
					chunk_start = grapheme_start;
					chunk_width = 0;
				}
				chunk_width =
					chunk_width.saturating_add(grapheme_width);
			}
			if chunk_start < word_range.end {
				append_block_piece(
					output,
					block,
					segment,
					chunk_start..word_range.end,
					chunk_start.saturating_sub(chosen_start),
				);
			}
		} else {
			append_block_piece(
				output,
				block,
				segment,
				word_range.clone(),
				word_range.start.saturating_sub(chosen_start),
			);
		}
	}
}

fn append_block_piece(
	output: &mut [BlockVisualLine],
	block: &InlineReplacementBlock,
	segment: &asyncgit::diff_preview::InlineSegment,
	range: Range<usize>,
	relative_start: usize,
) {
	if range.is_empty() {
		return;
	}
	let kind = segment.kind;
	let old_side = kind == InlineSegmentKind::Removed;
	let current = output.last_mut().expect("one visual line");
	let old_position = match kind {
		InlineSegmentKind::Added => None,
		InlineSegmentKind::Removed => Some(range.start),
		InlineSegmentKind::Shared => Some(
			segment.old_range.start.saturating_add(relative_start),
		),
	};
	let new_position = match kind {
		InlineSegmentKind::Removed => None,
		InlineSegmentKind::Added => Some(range.start),
		InlineSegmentKind::Shared => Some(
			segment.new_range.start.saturating_add(relative_start),
		),
	};
	let old_line_index = old_position.and_then(|position| {
		source_line_index(&block.old_lines, position)
	});
	let new_line_index = new_position.and_then(|position| {
		source_line_index(&block.new_lines, position)
	});
	if let Some(index) = old_line_index {
		if current.old_lineno.is_none() {
			current.old_lineno = block.old_lines[index].line_number;
		}
	}
	if let Some(index) = new_line_index {
		if current.new_lineno.is_none() {
			current.new_lineno = block.new_lines[index].line_number;
		}
	}
	let (old_range, new_range) =
		match kind {
			InlineSegmentKind::Shared => (
				segment.old_range.start + relative_start
					..segment.old_range.start
						+ relative_start + range.len(),
				segment.new_range.start + relative_start
					..segment.new_range.start
						+ relative_start + range.len(),
			),
			InlineSegmentKind::Removed => (
				range.clone(),
				segment.new_range.start..segment.new_range.start,
			),
			InlineSegmentKind::Added => (
				segment.old_range.start..segment.old_range.start,
				range.clone(),
			),
		};
	let width = block_text_width(
		if old_side {
			&block.old_text
		} else {
			&block.new_text
		},
		&range,
	);
	current.width = current.width.saturating_add(width);
	current.pieces.push(BlockVisualPiece {
		kind,
		is_space: false,
		old_range,
		new_range,
		old_line_index,
		new_line_index,
	});
}

fn source_line_index(
	lines: &[asyncgit::diff_preview::InlineSourceLine],
	position: usize,
) -> Option<usize> {
	lines
		.iter()
		.position(|line| line.text_range.contains(&position))
}

fn block_text_width(text: &str, range: &Range<usize>) -> usize {
	text.get(range.clone()).map_or(0, UnicodeWidthStr::width)
}

/// Display width of the aligned old/new line-number gutter.
#[must_use]
pub const fn line_gutter_width(number_width: usize) -> usize {
	number_width.saturating_mul(2).saturating_add(2)
}

/// Formats the fixed-width `old│new│` line-number gutter.
#[must_use]
pub fn format_line_gutter(
	old: Option<u32>,
	new: Option<u32>,
	number_width: usize,
) -> String {
	let old =
		old.map_or_else(String::new, |number| number.to_string());
	let new =
		new.map_or_else(String::new, |number| number.to_string());
	format!("{old:>number_width$}│{new:>number_width$}│")
}

/// Maps numbered patch rows to structural regions. Source line ranges are
/// zero-based and half-open; native diff line numbers are one-based. The
/// sweep is O((rows + regions) log regions + emitted memberships), and a row
/// may belong to nested regions on either side of a replacement.
#[must_use]
pub fn region_row_membership(
	old_line_numbers: &[Option<u32>],
	new_line_numbers: &[Option<u32>],
	ranges: &[StructuralLineRanges],
) -> Vec<Vec<usize>> {
	use std::collections::BTreeSet;
	let mut membership = vec![Vec::new(); ranges.len()];
	for (line_numbers, side) in
		[(old_line_numbers, 0_usize), (new_line_numbers, 1_usize)]
	{
		let mut intervals = ranges
			.iter()
			.enumerate()
			.filter_map(|(region, range)| {
				let range =
					if side == 0 { &range.0 } else { &range.1 };
				let range = range.as_ref()?;
				(range.start < range.end).then_some((
					range.start,
					range.end,
					region,
				))
			})
			.collect::<Vec<_>>();
		intervals.sort_unstable_by_key(|(start, _, region)| {
			(*start, *region)
		});
		let mut rows = line_numbers
			.iter()
			.enumerate()
			.filter_map(|(row, number)| {
				number
					.and_then(|number| number.checked_sub(1))
					.map(|number| (number, row))
			})
			.collect::<Vec<_>>();
		rows.sort_unstable();
		let mut active = BTreeSet::new();
		let mut next = 0;
		for (line, row) in rows {
			while intervals
				.get(next)
				.is_some_and(|(start, _, _)| *start <= line)
			{
				let (_, end, region) = intervals[next];
				active.insert((end, region));
				next += 1;
			}
			for &(end, region) in
				active.range((line.saturating_add(1), 0)..)
			{
				if line < end {
					membership[region].push(row);
				}
			}
		}
	}
	for rows in &mut membership {
		rows.sort_unstable();
		rows.dedup();
	}
	membership
}

#[cfg(test)]
mod tests {
	use super::{
		format_line_gutter, inline_block_visual_lines,
		line_gutter_width, region_row_membership,
	};
	use asyncgit::diff_preview::{
		InlineReplacementBlock, InlineSegment, InlineSegmentKind,
		InlineSourceLine,
	};

	#[test]
	fn old_new_gutter_is_aligned_and_fixed_width() {
		assert_eq!(
			format_line_gutter(Some(7), Some(8), 3),
			"  7│  8│"
		);
		assert_eq!(format_line_gutter(Some(7), None, 3), "  7│   │");
		assert_eq!(format_line_gutter(None, Some(8), 3), "   │  8│");
		assert_eq!(line_gutter_width(3), 8);
	}

	#[test]
	fn structural_membership_maps_one_based_patch_rows_to_half_open_ranges(
	) {
		let membership = region_row_membership(
			&[Some(1), Some(2), Some(4), None],
			&[Some(1), Some(3), Some(4), Some(5)],
			&[(Some(1..3), Some(2..4)), (Some(3..4), None)],
		);
		assert_eq!(membership, vec![vec![1, 2], vec![2]]);
	}

	#[test]
	fn wrapped_block_layout_keeps_unicode_source_ranges_and_new_breaks(
	) {
		let old_text = "alpha\nbeta".to_owned();
		let new_text = "alpha beta".to_owned();
		let block = InlineReplacementBlock {
			old_text,
			new_text,
			old_lines: vec![
				InlineSourceLine {
					line_number: Some(10),
					patch_index: 1,
					text_range: 0..5,
				},
				InlineSourceLine {
					line_number: Some(11),
					patch_index: 2,
					text_range: 6..10,
				},
			],
			new_lines: vec![InlineSourceLine {
				line_number: Some(10),
				patch_index: 3,
				text_range: 0..10,
			}],
			segments: vec![
				InlineSegment {
					kind: InlineSegmentKind::Shared,
					old_range: 0..5,
					new_range: 0..5,
				},
				InlineSegment {
					kind: InlineSegmentKind::Removed,
					old_range: 5..6,
					new_range: 5..5,
				},
				InlineSegment {
					kind: InlineSegmentKind::Added,
					old_range: 6..6,
					new_range: 5..6,
				},
				InlineSegment {
					kind: InlineSegmentKind::Shared,
					old_range: 6..10,
					new_range: 6..10,
				},
			],
		};
		let lines = inline_block_visual_lines(&block, 6);
		assert_eq!(lines.len(), 2);
		assert_eq!(lines[0].old_lineno, Some(10));
		assert_eq!(lines[0].new_lineno, Some(10));
		assert_eq!(lines[1].old_lineno, Some(11));
		assert_eq!(lines[1].new_lineno, Some(10));
		assert!(lines[0].width <= 6);
		assert!(lines[1].width <= 6);
		assert!(lines.iter().flat_map(|line| &line.pieces).any(
			|piece| {
				piece.kind == InlineSegmentKind::Added
					&& piece.new_line_index == Some(0)
			}
		));
		assert_eq!(
			lines
				.iter()
				.flat_map(|line| &line.pieces)
				.map(|piece| {
					if piece.is_space {
						" ".to_owned()
					} else {
						let (source, range) = if piece.kind
							== InlineSegmentKind::Removed
						{
							(&block.old_text, &piece.old_range)
						} else {
							(&block.new_text, &piece.new_range)
						};
						source[range.clone()].to_owned()
					}
				})
				.collect::<String>(),
			"alpha beta"
		);
	}

	#[test]
	fn deleted_prose_newlines_separate_shared_words_without_duplicate_spaces(
	) {
		let old_text = "You\ninterface\nGPUI".to_owned();
		let new_text = "YouinterfaceGPUI".to_owned();
		let block = InlineReplacementBlock {
			old_text,
			new_text,
			old_lines: vec![
				InlineSourceLine {
					line_number: Some(1),
					patch_index: 1,
					text_range: 0..3,
				},
				InlineSourceLine {
					line_number: Some(2),
					patch_index: 2,
					text_range: 4..13,
				},
				InlineSourceLine {
					line_number: Some(3),
					patch_index: 3,
					text_range: 14..18,
				},
			],
			new_lines: vec![InlineSourceLine {
				line_number: Some(1),
				patch_index: 4,
				text_range: 0..16,
			}],
			segments: vec![
				InlineSegment {
					kind: InlineSegmentKind::Shared,
					old_range: 0..3,
					new_range: 0..3,
				},
				InlineSegment {
					kind: InlineSegmentKind::Removed,
					old_range: 3..4,
					new_range: 3..3,
				},
				InlineSegment {
					kind: InlineSegmentKind::Shared,
					old_range: 4..13,
					new_range: 3..12,
				},
				InlineSegment {
					kind: InlineSegmentKind::Removed,
					old_range: 13..14,
					new_range: 12..12,
				},
				InlineSegment {
					kind: InlineSegmentKind::Shared,
					old_range: 14..18,
					new_range: 12..16,
				},
			],
		};
		let lines = inline_block_visual_lines(&block, 40);
		let displayed = lines
			.iter()
			.flat_map(|line| &line.pieces)
			.map(|piece| {
				if piece.is_space {
					" ".to_owned()
				} else {
					let (source, range) =
						if piece.kind == InlineSegmentKind::Removed {
							(&block.old_text, &piece.old_range)
						} else {
							(&block.new_text, &piece.new_range)
						};
					source[range.clone()].to_owned()
				}
			})
			.collect::<String>();
		assert_eq!(displayed, "You interface GPUI");
	}

	#[test]
	fn internal_deleted_newline_separates_words_despite_other_inserted_space(
	) {
		let block = InlineReplacementBlock {
			old_text: "You\ninterface".to_owned(),
			new_text: "You interface".to_owned(),
			old_lines: vec![
				InlineSourceLine {
					line_number: Some(1),
					patch_index: 1,
					text_range: 0..3,
				},
				InlineSourceLine {
					line_number: Some(2),
					patch_index: 2,
					text_range: 4..13,
				},
			],
			new_lines: vec![InlineSourceLine {
				line_number: Some(1),
				patch_index: 3,
				text_range: 0..13,
			}],
			segments: vec![
				InlineSegment {
					kind: InlineSegmentKind::Shared,
					old_range: 0..3,
					new_range: 0..3,
				},
				InlineSegment {
					kind: InlineSegmentKind::Added,
					old_range: 3..3,
					new_range: 3..4,
				},
				InlineSegment {
					kind: InlineSegmentKind::Removed,
					old_range: 3..13,
					new_range: 4..4,
				},
			],
		};
		let displayed = inline_block_visual_lines(&block, 40)
			.iter()
			.flat_map(|line| &line.pieces)
			.map(|piece| {
				if piece.is_space {
					" ".to_owned()
				} else {
					let (source, range) =
						if piece.kind == InlineSegmentKind::Removed {
							(&block.old_text, &piece.old_range)
						} else {
							(&block.new_text, &piece.new_range)
						};
					source[range.clone()].to_owned()
				}
			})
			.collect::<String>();

		assert_eq!(displayed, "You interface");
	}

	#[test]
	fn long_unicode_word_wraps_only_at_grapheme_boundaries() {
		let text = "界e\u{301}界";
		let block = InlineReplacementBlock {
			old_text: text.to_owned(),
			new_text: text.to_owned(),
			old_lines: vec![InlineSourceLine {
				line_number: Some(1),
				patch_index: 1,
				text_range: 0..text.len(),
			}],
			new_lines: vec![InlineSourceLine {
				line_number: Some(1),
				patch_index: 2,
				text_range: 0..text.len(),
			}],
			segments: vec![InlineSegment {
				kind: InlineSegmentKind::Shared,
				old_range: 0..text.len(),
				new_range: 0..text.len(),
			}],
		};
		let lines = inline_block_visual_lines(&block, 2);
		assert_eq!(lines.len(), 3);
		assert!(lines.iter().all(|line| line.width <= 2));
		let reconstructed = lines
			.iter()
			.flat_map(|line| &line.pieces)
			.map(|piece| {
				if piece.is_space {
					" "
				} else {
					&block.new_text[piece.new_range.clone()]
				}
			})
			.collect::<String>();
		assert_eq!(reconstructed, text);
		assert!(lines.iter().all(|line| {
			line.pieces.iter().all(|piece| {
				std::str::from_utf8(
					block
						.new_text
						.as_bytes()
						.get(piece.new_range.clone())
						.unwrap_or_default(),
				)
				.is_ok()
			})
		}));
	}
}
