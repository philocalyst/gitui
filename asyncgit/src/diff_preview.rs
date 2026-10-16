//! Bounded in-process preview from exact captured Git blobs.
use crate::{
	asyncjob::{AsyncJob, RunParams},
	diff::{DiffParams, DiffType},
	error::Result,
	sync::{repo, RepoPath},
	AsyncGitNotification,
};
use git2::{Delta, DiffLineType, Index, Repository, Tree};
use std::{
	collections::HashMap,
	fs,
	io::{self, Read},
	ops::Range,
	path::{Component, Path},
	sync::atomic::{AtomicBool, Ordering},
};
use unicode_segmentation::UnicodeSegmentation;
const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_BLOB_BYTES: usize = MAX_INPUT_BYTES / 2;
const MAX_PATCH_BYTES: usize = 2 * 1024 * 1024;
const MAX_PATCH_LINES: usize = 16_384;
const MAX_ALIGNMENT_LINES: usize = 32;
const MAX_ALIGNMENT_BYTES: usize = 4 * 1024;
const MAX_LINE_ALIGNMENT_CELLS: usize = 65_536;
const MIN_LINE_SIMILARITY_PERCENT: usize = 55;
const COMMON_ANCHOR_WORDS: &[&str] = &[
	"about",
	"after",
	"again",
	"against",
	"all",
	"also",
	"among",
	"and",
	"any",
	"are",
	"around",
	"async",
	"await",
	"because",
	"been",
	"before",
	"being",
	"between",
	"both",
	"break",
	"but",
	"can",
	"continue",
	"crate",
	"could",
	"does",
	"doing",
	"down",
	"during",
	"each",
	"else",
	"enum",
	"even",
	"ever",
	"every",
	"extern",
	"false",
	"few",
	"for",
	"from",
	"further",
	"had",
	"has",
	"have",
	"here",
	"how",
	"impl",
	"into",
	"is",
	"its",
	"itself",
	"let",
	"loop",
	"many",
	"may",
	"might",
	"more",
	"most",
	"must",
	"neither",
	"never",
	"nor",
	"not",
	"once",
	"only",
	"option",
	"other",
	"ourselves",
	"over",
	"own",
	"pub",
	"ref",
	"result",
	"return",
	"same",
	"should",
	"some",
	"static",
	"struct",
	"such",
	"super",
	"than",
	"that",
	"the",
	"their",
	"them",
	"themselves",
	"then",
	"there",
	"these",
	"they",
	"this",
	"those",
	"through",
	"trait",
	"true",
	"type",
	"under",
	"until",
	"use",
	"very",
	"was",
	"were",
	"what",
	"when",
	"where",
	"which",
	"while",
	"who",
	"why",
	"will",
	"with",
	"without",
	"would",
	"your",
	"yours",
	"yourself",
	"yourselves",
];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewRenderer {
	Native,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewStatus {
	Unified,
	Unchanged,
	Binary,
	TooLarge,
	Stale,
	Cancelled,
	CaptureFailed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewPreference {
	Auto,
	Unified,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewOptions {
	pub preference: PreviewPreference,
}
impl Default for PreviewOptions {
	fn default() -> Self {
		Self {
			preference: PreviewPreference::Auto,
		}
	}
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffPreviewRequest {
	pub generation: u64,
	pub params: DiffParams,
	pub expected_native_hash: u64,
	pub options: PreviewOptions,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffPreviewResult {
	pub request: DiffPreviewRequest,
	pub preview: DiffPreview,
}
#[derive(Clone, Debug)]
pub struct DiffPreviewCancellation(std::sync::Arc<AtomicBool>);
impl DiffPreviewCancellation {
	pub fn cancel(&self) {
		self.0.store(true, Ordering::Release);
	}
}
#[derive(Clone)]
pub struct DiffPreviewJob {
	repo_path: RepoPath,
	request: DiffPreviewRequest,
	cancellation: DiffPreviewCancellation,
	result: Option<DiffPreviewResult>,
}
impl DiffPreviewJob {
	pub fn new(
		repo_path: RepoPath,
		request: DiffPreviewRequest,
	) -> (Self, DiffPreviewCancellation) {
		let cancellation = DiffPreviewCancellation(
			std::sync::Arc::new(AtomicBool::new(false)),
		);
		(
			Self {
				repo_path,
				request,
				cancellation: cancellation.clone(),
				result: None,
			},
			cancellation,
		)
	}
	pub const fn result(&self) -> Option<&DiffPreviewResult> {
		self.result.as_ref()
	}
}
impl AsyncJob for DiffPreviewJob {
	type Notification = AsyncGitNotification;
	type Progress = ();
	fn run(
		&mut self,
		_params: RunParams<Self::Notification, Self::Progress>,
	) -> Result<Self::Notification> {
		let preview = render_diff_preview(
			&self.repo_path,
			&self.request.params,
			self.request.expected_native_hash,
			self.request.options,
			&self.cancellation.0,
		);
		self.result = Some(DiffPreviewResult {
			request: self.request.clone(),
			preview,
		});
		Ok(AsyncGitNotification::DiffPreview)
	}
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewLineKind {
	FileHeader,
	HunkHeader,
	Context,
	Addition,
	Deletion,
	Replacement,
	BlockReplacement,
	Metadata,
	Notice,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewLine {
	pub kind: PreviewLineKind,
	pub old_lineno: Option<u32>,
	pub new_lineno: Option<u32>,
	pub text: String,
	/// Stable row position only, never a staging coordinate.
	pub patch_index: usize,
	/// UTF-8 byte ranges aligned to grapheme boundaries.
	pub changed_ranges: Vec<Range<usize>>,
	/// Both original source lines and their bounded inline presentation.
	pub replacement: Option<InlineReplacement>,
	/// Atomic, provenance-preserving prose rewrite spanning multiple lines.
	pub block_replacement: Option<InlineReplacementBlock>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineSegmentKind {
	Shared,
	Removed,
	Added,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineSegment {
	pub kind: InlineSegmentKind,
	/// Byte range into the old source text; empty for added text.
	pub old_range: Range<usize>,
	/// Byte range into the new source text; empty for removed text.
	pub new_range: Range<usize>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineReplacement {
	pub old_text: String,
	pub new_text: String,
	pub old_lineno: Option<u32>,
	pub new_lineno: Option<u32>,
	pub old_patch_index: usize,
	pub new_patch_index: usize,
	pub segments: Vec<InlineSegment>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineSourceLine {
	pub line_number: Option<u32>,
	pub patch_index: usize,
	pub text_range: Range<usize>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineReplacementBlock {
	pub old_text: String,
	pub new_text: String,
	pub old_lines: Vec<InlineSourceLine>,
	pub new_lines: Vec<InlineSourceLine>,
	pub segments: Vec<InlineSegment>,
}
#[derive(Clone, Copy, Debug)]
pub struct InlineBlockLine<'a> {
	pub text: &'a str,
	pub line_number: Option<u32>,
	pub patch_index: usize,
}

const MAX_BLOCK_TOKENS: usize = 512;
const MAX_BLOCK_ALIGNMENT_CELLS: usize = 65_536;

/// Align a bounded prose rewrite across physical line breaks. Every source
/// line remains attached to its original patch coordinate; the joined text
/// exists only for read-only presentation.
pub fn inline_diff_block(
	old_lines: &[InlineBlockLine<'_>],
	new_lines: &[InlineBlockLine<'_>],
	cancel: &AtomicBool,
) -> Option<InlineReplacementBlock> {
	if old_lines.is_empty()
		|| new_lines.is_empty()
		|| old_lines.len().saturating_add(new_lines.len())
			> MAX_ALIGNMENT_LINES
	{
		return None;
	}
	let total_bytes = old_lines
		.iter()
		.chain(new_lines)
		.map(|line| line.text.len())
		.sum::<usize>()
		.saturating_add(old_lines.len() + new_lines.len());
	if total_bytes > MAX_ALIGNMENT_BYTES
		|| cancel.load(Ordering::Acquire)
	{
		return None;
	}
	if old_lines.iter().chain(new_lines).any(|line| {
		let trimmed = line.text.trim_start();
		trimmed.is_empty()
			|| is_markdown_boundary(trimmed)
			|| looks_like_code_line(line.text)
	}) {
		return None;
	}
	let old_text = old_lines
		.iter()
		.map(|line| line.text)
		.collect::<Vec<_>>()
		.join("\n");
	let new_text = new_lines
		.iter()
		.map(|line| line.text)
		.collect::<Vec<_>>()
		.join("\n");
	let old_tokens = inline_tokens(&old_text);
	let new_tokens = inline_tokens(&new_text);
	if old_tokens.len() > MAX_BLOCK_TOKENS
		|| new_tokens.len() > MAX_BLOCK_TOKENS
		|| (old_tokens.len() + 1).saturating_mul(new_tokens.len() + 1)
			> MAX_BLOCK_ALIGNMENT_CELLS
	{
		return None;
	}
	let anchor_matches = prose_word_matches(
		&old_text,
		&new_text,
		&old_tokens,
		&new_tokens,
		cancel,
	)?;
	let segments = build_block_segments(
		&old_text,
		&new_text,
		&old_tokens,
		&new_tokens,
		&anchor_matches,
		cancel,
	)?;
	let old_source_lines = source_line_ranges(old_lines, &old_text);
	let new_source_lines = source_line_ranges(new_lines, &new_text);
	Some(InlineReplacementBlock {
		old_text,
		new_text,
		old_lines: old_source_lines,
		new_lines: new_source_lines,
		segments,
	})
}

fn prose_word_matches(
	old_text: &str,
	new_text: &str,
	old_tokens: &[InlineToken],
	new_tokens: &[InlineToken],
	cancel: &AtomicBool,
) -> Option<Vec<(usize, usize)>> {
	let old_words = informative_words(old_text, old_tokens);
	let new_words = informative_words(new_text, new_tokens);
	if old_words.is_empty() || new_words.is_empty() {
		return None;
	}
	let matches =
		weighted_word_lcs_matches(&old_words, &new_words, cancel)?;
	let shared_terms = matches
		.iter()
		.map(|(old_index, _)| {
			old_text[old_tokens[*old_index].range.clone()]
				.to_lowercase()
		})
		.collect::<std::collections::HashSet<_>>();
	(shared_terms.len() >= 5
		&& matches.len() * 5 >= old_words.len().min(new_words.len()))
	.then_some(matches)
}

fn anchor_weight(
	word: &str,
	frequencies: &HashMap<String, usize>,
) -> u16 {
	let lower = word.to_ascii_lowercase();
	if COMMON_ANCHOR_WORDS.contains(&lower.as_str()) {
		return 1;
	}
	let distinctive = lower
		.chars()
		.filter(|character| character.is_alphanumeric())
		.count();
	let occurrences = frequencies.get(&lower).copied().unwrap_or(0);
	let length_bonus =
		distinctive.min(10).try_into().unwrap_or(10_u16);
	match occurrences {
		0 | 1 => 5 + length_bonus,
		2 => 3 + length_bonus,
		_ if distinctive >= 9 => 8,
		_ if distinctive >= 6 => 5,
		_ => 3,
	}
}

fn weighted_word_lcs_matches(
	old_words: &[(usize, &str)],
	new_words: &[(usize, &str)],
	cancel: &AtomicBool,
) -> Option<Vec<(usize, usize)>> {
	let mut frequencies = HashMap::<String, usize>::new();
	for (_, word) in old_words.iter().chain(new_words.iter()) {
		*frequencies.entry(word.to_ascii_lowercase()).or_default() +=
			1;
	}
	let old_weights = old_words
		.iter()
		.map(|(_, word)| anchor_weight(word, &frequencies))
		.collect::<Vec<_>>();
	let width = new_words.len() + 1;
	let mut scores = vec![0_u16; (old_words.len() + 1) * width];
	for old_index in (0..old_words.len()).rev() {
		if cancel.load(Ordering::Acquire) {
			return None;
		}
		for new_index in (0..new_words.len()).rev() {
			let cell = old_index * width + new_index;
			let skip_old =
				scores[(old_index + 1) * width + new_index];
			let skip_new = scores[old_index * width + new_index + 1];
			scores[cell] = if old_words[old_index].1
				== new_words[new_index].1
			{
				(old_weights[old_index].saturating_add(
					scores[(old_index + 1) * width + new_index + 1],
				))
				.max(skip_old.max(skip_new))
			} else {
				skip_old.max(skip_new)
			};
		}
	}
	let mut matches = Vec::new();
	let (mut old_cursor, mut new_cursor) = (0, 0);
	while old_cursor < old_words.len() && new_cursor < new_words.len()
	{
		if old_words[old_cursor].1 == new_words[new_cursor].1
			&& scores[old_cursor * width + new_cursor]
				== old_weights[old_cursor].saturating_add(
					scores[(old_cursor + 1) * width + new_cursor + 1],
				) {
			matches.push((
				old_words[old_cursor].0,
				new_words[new_cursor].0,
			));
			old_cursor += 1;
			new_cursor += 1;
		} else if scores[(old_cursor + 1) * width + new_cursor]
			>= scores[old_cursor * width + new_cursor + 1]
		{
			old_cursor += 1;
		} else {
			new_cursor += 1;
		}
	}
	Some(matches)
}

/// Whether a path is eligible for the conservative prose presentation.
pub fn is_prose_diff_path(path: &str) -> bool {
	let basename = path.rsplit('/').next().unwrap_or(path);
	let lower = basename.to_ascii_lowercase();
	basename.to_ascii_uppercase().starts_with("README")
		|| [".md", ".markdown", ".mdown", ".txt", ".rst", ".adoc"]
			.iter()
			.any(|extension| lower.ends_with(extension))
}

fn informative_words<'a>(
	text: &'a str,
	tokens: &'a [InlineToken],
) -> Vec<(usize, &'a str)> {
	const STOP_WORDS: &[&str] = &[
		"a", "an", "and", "are", "as", "at", "be", "been", "but",
		"by", "for", "from", "has", "have", "in", "is", "it", "of",
		"on", "or", "that", "the", "their", "this", "to", "was",
		"were", "will", "with",
	];
	tokens
		.iter()
		.enumerate()
		.filter_map(|(index, token)| {
			if !token.is_word {
				return None;
			}
			let word = &text[token.range.clone()];
			(word.chars().count() > 2
				&& !STOP_WORDS
					.contains(&word.to_ascii_lowercase().as_str()))
			.then_some((index, word))
		})
		.collect()
}

fn token_lcs_matches(
	old_tokens: &[(usize, &str)],
	new_tokens: &[(usize, &str)],
	cancel: &AtomicBool,
) -> Option<Vec<(usize, usize)>> {
	let width = new_tokens.len() + 1;
	let mut scores = vec![0_u16; (old_tokens.len() + 1) * width];
	for old_index in (0..old_tokens.len()).rev() {
		if cancel.load(Ordering::Acquire) {
			return None;
		}
		for new_index in (0..new_tokens.len()).rev() {
			let cell = old_index * width + new_index;
			scores[cell] = if old_tokens[old_index].1
				== new_tokens[new_index].1
			{
				1 + scores[(old_index + 1) * width + new_index + 1]
			} else {
				scores[(old_index + 1) * width + new_index]
					.max(scores[old_index * width + new_index + 1])
			};
		}
	}
	let mut matches = Vec::new();
	let (mut old_cursor, mut new_cursor) = (0, 0);
	while old_cursor < old_tokens.len()
		&& new_cursor < new_tokens.len()
	{
		if old_tokens[old_cursor].1 == new_tokens[new_cursor].1 {
			matches.push((
				old_tokens[old_cursor].0,
				new_tokens[new_cursor].0,
			));
			old_cursor += 1;
			new_cursor += 1;
		} else if scores[(old_cursor + 1) * width + new_cursor]
			>= scores[old_cursor * width + new_cursor + 1]
		{
			old_cursor += 1;
		} else {
			new_cursor += 1;
		}
	}
	Some(matches)
}

fn build_block_segments(
	old: &str,
	new: &str,
	old_tokens: &[InlineToken],
	new_tokens: &[InlineToken],
	anchors: &[(usize, usize)],
	cancel: &AtomicBool,
) -> Option<Vec<InlineSegment>> {
	let mut context = ProseSegmentContext {
		old,
		new,
		old_tokens,
		new_tokens,
		segments: Vec::new(),
	};
	let (mut old_cursor, mut new_cursor) = (0, 0);
	for &(old_match, new_match) in anchors {
		context.append_gap(
			old_cursor..old_match,
			new_cursor..new_match,
			cancel,
		)?;
		push_inline_segment(
			&mut context.segments,
			InlineSegmentKind::Shared,
			old_tokens[old_match].range.clone(),
			new_tokens[new_match].range.clone(),
		);
		old_cursor = old_match + 1;
		new_cursor = new_match + 1;
	}
	context.append_gap(
		old_cursor..old_tokens.len(),
		new_cursor..new_tokens.len(),
		cancel,
	)?;
	Some(context.segments)
}

struct ProseSegmentContext<'a> {
	old: &'a str,
	new: &'a str,
	old_tokens: &'a [InlineToken],
	new_tokens: &'a [InlineToken],
	segments: Vec<InlineSegment>,
}

impl ProseSegmentContext<'_> {
	fn append_gap(
		&mut self,
		old_gap: Range<usize>,
		new_gap: Range<usize>,
		cancel: &AtomicBool,
	) -> Option<()> {
		let old_matches = self.old_tokens[old_gap.clone()]
			.iter()
			.enumerate()
			.map(|(i, token)| {
				(old_gap.start + i, &self.old[token.range.clone()])
			})
			.collect::<Vec<_>>();
		let new_matches = self.new_tokens[new_gap.clone()]
			.iter()
			.enumerate()
			.map(|(i, token)| {
				(new_gap.start + i, &self.new[token.range.clone()])
			})
			.collect::<Vec<_>>();
		let matches =
			token_lcs_matches(&old_matches, &new_matches, cancel)?;
		let (mut old_cursor, mut new_cursor) =
			(old_gap.start, new_gap.start);
		for (old_match, new_match) in matches {
			append_whole_word_gap(
				self.old,
				self.new,
				self.old_tokens,
				self.new_tokens,
				old_cursor..old_match,
				new_cursor..new_match,
				&mut self.segments,
			);
			push_inline_segment(
				&mut self.segments,
				InlineSegmentKind::Shared,
				self.old_tokens[old_match].range.clone(),
				self.new_tokens[new_match].range.clone(),
			);
			old_cursor = old_match + 1;
			new_cursor = new_match + 1;
		}
		append_whole_word_gap(
			self.old,
			self.new,
			self.old_tokens,
			self.new_tokens,
			old_cursor..old_gap.end,
			new_cursor..new_gap.end,
			&mut self.segments,
		);
		Some(())
	}
}

fn append_whole_word_gap(
	old: &str,
	new: &str,
	old_tokens: &[InlineToken],
	new_tokens: &[InlineToken],
	old_gap: Range<usize>,
	new_gap: Range<usize>,
	segments: &mut Vec<InlineSegment>,
) {
	let old_range = token_gap_range(old_tokens, old_gap, old.len());
	let new_range = token_gap_range(new_tokens, new_gap, new.len());
	let old_end = old_range.end;
	if !old_range.is_empty() {
		push_inline_segment(
			segments,
			InlineSegmentKind::Removed,
			old_range,
			new_range.start..new_range.start,
		);
	}
	if !new_range.is_empty() {
		push_inline_segment(
			segments,
			InlineSegmentKind::Added,
			old_end..old_end,
			new_range,
		);
	}
}

fn source_line_ranges(
	lines: &[InlineBlockLine<'_>],
	joined: &str,
) -> Vec<InlineSourceLine> {
	let mut offset = 0;
	lines
		.iter()
		.map(|line| {
			let start = offset;
			offset += line.text.len();
			let range = start..offset;
			offset += usize::from(offset < joined.len());
			InlineSourceLine {
				line_number: line.line_number,
				patch_index: line.patch_index,
				text_range: range,
			}
		})
		.collect()
}

fn is_markdown_boundary(line: &str) -> bool {
	line.starts_with('#')
		|| line.starts_with("```")
		|| line.starts_with("~~~")
		|| line.starts_with('>')
		|| line.starts_with("- ")
		|| line.starts_with("* ")
		|| line.starts_with("+ ")
		|| line.split_once('.').is_some_and(|(number, rest)| {
			!number.is_empty()
				&& number.chars().all(|c| c.is_ascii_digit())
				&& rest.starts_with(' ')
		})
}

fn looks_like_code_line(line: &str) -> bool {
	let trimmed = line.trim_start();
	line.starts_with([' ', '\t'])
		|| [
			"fn ", "pub ", "let ", "use ", "impl ", "struct ",
			"enum ", "#include",
		]
		.iter()
		.any(|prefix| trimmed.starts_with(prefix))
		|| trimmed == "{"
		|| trimmed == "}"
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffPreview {
	pub renderer: PreviewRenderer,
	pub status: PreviewStatus,
	pub lines: Vec<PreviewLine>,
	/// Optional syntax-backed regions from the exact captured file pair.
	pub structural: Option<crate::structural::StructuralAnalysis>,
	pub message: Option<String>,
}
#[derive(Debug, PartialEq, Eq)]
struct BlobPair {
	old: CapturedContent,
	new: CapturedContent,
	old_path: String,
	new_path: String,
	too_large: bool,
}
#[derive(Debug, Default, PartialEq, Eq)]
struct CapturedContent {
	bytes: Vec<u8>,
	too_large: bool,
	fingerprint: Option<u64>,
}
/// Return symmetric changed spans aligned to grapheme boundaries.
pub fn changed_grapheme_ranges(
	old: &str,
	new: &str,
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
	let old_graphemes: Vec<(usize, &str)> =
		old.grapheme_indices(true).collect();
	let new_graphemes: Vec<(usize, &str)> =
		new.grapheme_indices(true).collect();
	if old == new {
		return (vec![], vec![]);
	}
	if (old_graphemes.len() + 1)
		.saturating_mul(new_graphemes.len() + 1)
		> 16_384
	{
		return fallback_ranges(
			old,
			new,
			&old_graphemes,
			&new_graphemes,
		);
	}
	let row_width = new_graphemes.len() + 1;
	let mut dp = vec![0_u16; (old_graphemes.len() + 1) * row_width];
	for old_index in (0..old_graphemes.len()).rev() {
		for new_index in (0..new_graphemes.len()).rev() {
			let cell = old_index * row_width + new_index;
			dp[cell] = if old_graphemes[old_index].1
				== new_graphemes[new_index].1
			{
				1 + dp[(old_index + 1) * row_width + new_index + 1]
			} else {
				dp[(old_index + 1) * row_width + new_index]
					.max(dp[old_index * row_width + new_index + 1])
			};
		}
	}
	let (mut old_cursor, mut new_cursor) = (0, 0);
	let (mut ar, mut br) = (vec![], vec![]);
	let (mut as_, mut bs) = (None, None);
	while old_cursor < old_graphemes.len()
		|| new_cursor < new_graphemes.len()
	{
		if old_cursor < old_graphemes.len()
			&& new_cursor < new_graphemes.len()
			&& old_graphemes[old_cursor].1
				== new_graphemes[new_cursor].1
		{
			add_range(
				&mut ar,
				as_.take(),
				old_graphemes[old_cursor].0,
			);
			add_range(
				&mut br,
				bs.take(),
				new_graphemes[new_cursor].0,
			);
			old_cursor += 1;
			new_cursor += 1;
		} else if new_cursor == new_graphemes.len()
			|| (old_cursor < old_graphemes.len()
				&& dp[(old_cursor + 1) * row_width + new_cursor]
					>= dp[old_cursor * row_width + new_cursor + 1])
		{
			as_.get_or_insert(old_graphemes[old_cursor].0);
			old_cursor += 1;
		} else {
			bs.get_or_insert(new_graphemes[new_cursor].0);
			new_cursor += 1;
		}
	}
	add_range(&mut ar, as_, old.len());
	add_range(&mut br, bs, new.len());
	(ar, br)
}
fn add_range(
	out: &mut Vec<Range<usize>>,
	start: Option<usize>,
	end: usize,
) {
	if let Some(s) = start {
		if s < end {
			out.push(s..end);
		}
	}
}
fn fallback_ranges(
	old: &str,
	new: &str,
	a: &[(usize, &str)],
	b: &[(usize, &str)],
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
	let mut prefix = 0;
	while prefix < a.len()
		&& prefix < b.len()
		&& a[prefix].1 == b[prefix].1
	{
		prefix += 1;
	}
	let (mut old_end, mut new_end) = (a.len(), b.len());
	while old_end > prefix && new_end > prefix {
		let old_tail = old_end - 1;
		let new_tail = new_end - 1;
		if a[old_tail].1 != b[new_tail].1 {
			break;
		}
		old_end -= 1;
		new_end -= 1;
	}
	let old_start = a.get(prefix).map_or(old.len(), |(i, _)| *i);
	let new_start = b.get(prefix).map_or(new.len(), |(i, _)| *i);
	let old_end = a.get(old_end).map_or(old.len(), |(i, _)| *i);
	let new_end = b.get(new_end).map_or(new.len(), |(i, _)| *i);
	(
		(old_start < old_end)
			.then_some(old_start..old_end)
			.into_iter()
			.collect(),
		(new_start < new_end)
			.then_some(new_start..new_end)
			.into_iter()
			.collect(),
	)
}
/// Render a read-only preview in a background worker.
pub fn render_diff_preview(
	repository_path: &RepoPath,
	params: &DiffParams,
	expected_native_hash: u64,
	options: PreviewOptions,
	cancel: &AtomicBool,
) -> DiffPreview {
	if cancel.load(Ordering::Acquire) {
		return empty(PreviewStatus::Cancelled, "Preview cancelled");
	}
	let initial_native_hash =
		current_native_diff_hash(repository_path, params);
	if !matches!(initial_native_hash, Ok(hash) if hash == expected_native_hash)
	{
		return empty(
			PreviewStatus::Stale,
			"Native diff changed before the preview started",
		);
	}
	let Ok(captured_pair) = capture_pair(repository_path, params)
	else {
		log::debug!("preview capture failed");
		return empty(
			PreviewStatus::CaptureFailed,
			"Could not capture both file versions",
		);
	};
	let preview =
		render_pair(&captured_pair, params, options, cancel);
	if cancel.load(Ordering::Acquire) {
		return empty(PreviewStatus::Cancelled, "Preview cancelled");
	}
	// Re-read both the selected blobs and the native diff: either can change
	// while a large preview is rendered, which would make the result stale.
	let Ok(current_pair) = capture_pair(repository_path, params)
	else {
		return empty(
			PreviewStatus::Stale,
			"Native diff changed while the preview was being prepared",
		);
	};
	let current_native_hash =
		current_native_diff_hash(repository_path, params);
	if current_pair != captured_pair
		|| !matches!(current_native_hash, Ok(hash) if hash == expected_native_hash)
	{
		return empty(
			PreviewStatus::Stale,
			"Native diff changed while the preview was being prepared",
		);
	}
	preview
}
fn current_native_diff_hash(
	repository_path: &RepoPath,
	params: &DiffParams,
) -> Result<u64> {
	let native_diff = match params.diff_type {
		DiffType::Stage => crate::sync::diff::get_diff(
			repository_path,
			&params.path,
			true,
			Some(params.options),
		)?,
		DiffType::WorkDir => crate::sync::diff::get_diff(
			repository_path,
			&params.path,
			false,
			Some(params.options),
		)?,
		DiffType::Commit(id) => crate::sync::diff::get_diff_commit(
			repository_path,
			id,
			params.path.clone(),
			Some(params.options),
		)?,
		DiffType::Commits(ids) => {
			crate::sync::diff::get_diff_commits(
				repository_path,
				ids,
				params.path.clone(),
				Some(params.options),
			)?
		}
	};
	Ok(crate::hash(&native_diff))
}
fn render_pair(
	pair: &BlobPair,
	params: &DiffParams,
	options: PreviewOptions,
	cancel: &AtomicBool,
) -> DiffPreview {
	if pair.too_large {
		return empty(
			PreviewStatus::TooLarge,
			"File exceeds the bounded preview size",
		);
	}
	if pair.old.bytes == pair.new.bytes {
		return empty(PreviewStatus::Unchanged, "No content changes");
	}
	if pair.old.bytes.len() + pair.new.bytes.len() > MAX_INPUT_BYTES {
		return empty(
			PreviewStatus::TooLarge,
			"File exceeds the bounded preview size",
		);
	}
	if pair.old.bytes.contains(&0)
		|| pair.new.bytes.contains(&0)
		|| std::str::from_utf8(&pair.old.bytes).is_err()
		|| std::str::from_utf8(&pair.new.bytes).is_err()
	{
		return empty(
			PreviewStatus::Binary,
			"Binary or non-UTF-8 file; text preview is unavailable",
		);
	}
	let structural = if options.preference == PreviewPreference::Auto
	{
		let source_path = if pair.new.bytes.is_empty() {
			&pair.old_path
		} else {
			&pair.new_path
		};
		crate::structural::analyze_regions(
			source_path,
			&pair.old.bytes,
			&pair.new.bytes,
			cancel,
		)
	} else {
		None
	};
	if cancel.load(Ordering::Acquire) {
		return empty(PreviewStatus::Cancelled, "Preview cancelled");
	}
	match patch_rows(
		pair,
		params.options,
		options.preference == PreviewPreference::Auto,
		cancel,
	) {
		Ok(lines) if lines.is_empty() => empty(
			PreviewStatus::Unchanged,
			"No changes under current diff options",
		),
		Ok(lines) => DiffPreview {
			renderer: PreviewRenderer::Native,
			status: PreviewStatus::Unified,
			lines,
			structural,
			message: None,
		},
		Err(PatchErr::Cancel) => {
			empty(PreviewStatus::Cancelled, "Preview cancelled")
		}
		Err(PatchErr::Large) => empty(
			PreviewStatus::TooLarge,
			"Patch exceeds the bounded preview size",
		),
		Err(PatchErr::Git) => empty(
			PreviewStatus::CaptureFailed,
			"Could not create a native patch preview",
		),
	}
}
fn empty(status: PreviewStatus, msg: &str) -> DiffPreview {
	DiffPreview {
		renderer: PreviewRenderer::Native,
		status,
		lines: vec![PreviewLine {
			kind: PreviewLineKind::Notice,
			old_lineno: None,
			new_lineno: None,
			text: msg.into(),
			patch_index: 0,
			changed_ranges: vec![],
			replacement: None,
			block_replacement: None,
		}],
		structural: None,
		message: None,
	}
}
#[derive(Clone, Copy, Debug)]
enum PatchErr {
	Cancel,
	Large,
	Git,
}
fn patch_rows(
	pair: &BlobPair,
	diff_options: crate::sync::diff::DiffOptions,
	inline_changes: bool,
	cancel: &AtomicBool,
) -> std::result::Result<Vec<PreviewLine>, PatchErr> {
	let prose_maps = if inline_changes
		&& is_prose_diff_path(if pair.new.bytes.is_empty() {
			&pair.old_path
		} else {
			&pair.new_path
		}) {
		Some((
			source_prose_line_map(&pair.old.bytes, cancel)?,
			source_prose_line_map(&pair.new.bytes, cancel)?,
		))
	} else {
		None
	};
	let mut options = git2::DiffOptions::new();
	options
		.context_lines(diff_options.context.min(100))
		.interhunk_lines(diff_options.interhunk_lines.min(100))
		.ignore_whitespace(diff_options.ignore_whitespace);
	let patch = git2::Patch::from_buffers(
		&pair.old.bytes,
		Some(Path::new(&pair.old_path)),
		&pair.new.bytes,
		Some(Path::new(&pair.new_path)),
		Some(&mut options),
	)
	.map_err(|_| PatchErr::Git)?;
	if patch.size(true, true, true) > MAX_PATCH_BYTES {
		return Err(PatchErr::Large);
	}
	if patch.num_hunks() == 0 {
		return Ok(Vec::new());
	}
	let title = if pair.old_path == pair.new_path {
		format!("--- {}", display_path(&pair.new_path))
	} else {
		format!(
			"--- {} → {}",
			display_path(&pair.old_path),
			display_path(&pair.new_path)
		)
	};
	let mut rows = vec![PreviewLine {
		kind: PreviewLineKind::FileHeader,
		old_lineno: None,
		new_lineno: None,
		text: title,
		patch_index: 0,
		changed_ranges: vec![],
		replacement: None,
		block_replacement: None,
	}];
	append_patch_hunks(
		&patch,
		&mut rows,
		prose_maps.as_ref(),
		inline_changes,
		cancel,
	)?;
	Ok(rows)
}

fn append_patch_hunks(
	patch: &git2::Patch<'_>,
	rows: &mut Vec<PreviewLine>,
	prose_maps: Option<&(Vec<bool>, Vec<bool>)>,
	inline_changes: bool,
	cancel: &AtomicBool,
) -> std::result::Result<(), PatchErr> {
	let mut index = rows.len();
	for h in 0..patch.num_hunks() {
		if cancel.load(Ordering::Acquire) {
			return Err(PatchErr::Cancel);
		}
		let (hunk, n) = patch.hunk(h).map_err(|_| PatchErr::Git)?;
		rows.push(PreviewLine {
			kind: PreviewLineKind::HunkHeader,
			old_lineno: None,
			new_lineno: None,
			text: String::from_utf8_lossy(hunk.header())
				.trim()
				.into(),
			patch_index: index,
			changed_ranges: vec![],
			replacement: None,
			block_replacement: None,
		});
		index += 1;
		let mut run = rows.len();
		for k in 0..n {
			if k % 256 == 0 && cancel.load(Ordering::Acquire) {
				return Err(PatchErr::Cancel);
			}
			let line = patch
				.line_in_hunk(h, k)
				.map_err(|_| PatchErr::Git)?;
			let kind = match line.origin_value() {
				DiffLineType::Context
				| DiffLineType::ContextEOFNL => PreviewLineKind::Context,
				DiffLineType::Addition | DiffLineType::AddEOFNL => {
					PreviewLineKind::Addition
				}
				DiffLineType::Deletion
				| DiffLineType::DeleteEOFNL => PreviewLineKind::Deletion,
				DiffLineType::Binary => PreviewLineKind::Metadata,
				_ => continue,
			};
			rows.push(PreviewLine {
				kind,
				old_lineno: line.old_lineno(),
				new_lineno: line.new_lineno(),
				text: safe_line(line.content()),
				patch_index: index,
				changed_ranges: vec![],
				replacement: None,
				block_replacement: None,
			});
			index += 1;
			if index > MAX_PATCH_LINES {
				return Err(PatchErr::Large);
			}
			if kind == PreviewLineKind::Context {
				if inline_changes
					&& !collapse_prose_run(
						rows, run, prose_maps, cancel,
					) {
					pair_runs(rows, run, prose_maps);
				}
				run = rows.len();
			}
		}
		if inline_changes
			&& !collapse_prose_run(rows, run, prose_maps, cancel)
		{
			pair_runs(rows, run, prose_maps);
		}
	}
	Ok(())
}

fn collapse_prose_run(
	rows: &mut Vec<PreviewLine>,
	start: usize,
	prose_maps: Option<&(Vec<bool>, Vec<bool>)>,
	cancel: &AtomicBool,
) -> bool {
	let end = if rows
		.last()
		.is_some_and(|line| line.kind == PreviewLineKind::Context)
	{
		rows.len().saturating_sub(1)
	} else {
		rows.len()
	};
	let Some((old_map, new_map)) = prose_maps else {
		return false;
	};
	if start >= end {
		return false;
	}
	let deletions = (start..end)
		.filter(|&index| {
			rows[index].kind == PreviewLineKind::Deletion
		})
		.collect::<Vec<_>>();
	let additions = (start..end)
		.filter(|&index| {
			rows[index].kind == PreviewLineKind::Addition
		})
		.collect::<Vec<_>>();
	if deletions.is_empty()
		|| additions.is_empty()
		|| !source_line_indexes_are_prose(
			old_map, &deletions, rows, true,
		) || !source_line_indexes_are_prose(
		new_map, &additions, rows, false,
	) {
		return false;
	}
	let old_lines = deletions
		.iter()
		.map(|&index| InlineBlockLine {
			text: rows[index].text.as_str(),
			line_number: rows[index].old_lineno,
			patch_index: rows[index].patch_index,
		})
		.collect::<Vec<_>>();
	let new_lines = additions
		.iter()
		.map(|&index| InlineBlockLine {
			text: rows[index].text.as_str(),
			line_number: rows[index].new_lineno,
			patch_index: rows[index].patch_index,
		})
		.collect::<Vec<_>>();
	let Some(block) =
		inline_diff_block(&old_lines, &new_lines, cancel)
	else {
		return false;
	};
	let old_lineno =
		block.old_lines.first().and_then(|line| line.line_number);
	let new_lineno =
		block.new_lines.first().and_then(|line| line.line_number);
	let patch_index =
		block.old_lines.first().map_or(0, |line| line.patch_index);
	let display = PreviewLine {
		kind: PreviewLineKind::BlockReplacement,
		old_lineno,
		new_lineno,
		text: block.new_text.clone(),
		patch_index,
		changed_ranges: vec![],
		replacement: None,
		block_replacement: Some(block),
	};
	rows.splice(start..end, [display]);
	true
}

fn source_prose_line_map(
	source: &[u8],
	cancel: &AtomicBool,
) -> std::result::Result<Vec<bool>, PatchErr> {
	let Ok(source) = std::str::from_utf8(source) else {
		return Err(PatchErr::Git);
	};
	let mut fence: Option<(char, usize)> = None;
	let mut eligible = Vec::new();
	for (line_index, line) in source.lines().enumerate() {
		if line_index % 256 == 0 && cancel.load(Ordering::Acquire) {
			return Err(PatchErr::Cancel);
		}
		let trimmed = line.trim_start();
		eligible.push(fence.is_none() && is_prose_source_line(line));
		if let Some((marker, count)) = fence {
			let run =
				trimmed.chars().take_while(|&c| c == marker).count();
			if run >= count && trimmed[run..].trim().is_empty() {
				fence = None;
			}
		} else if let Some(marker) =
			trimmed.chars().next().filter(|c| *c == '`' || *c == '~')
		{
			let count =
				trimmed.chars().take_while(|&c| c == marker).count();
			if count >= 3 {
				fence = Some((marker, count));
			}
		}
	}
	Ok(eligible)
}

fn source_line_indexes_are_prose(
	eligible_lines: &[bool],
	rows: &[usize],
	preview: &[PreviewLine],
	old_side: bool,
) -> bool {
	rows.iter().all(|&index| {
		let line_number = if old_side {
			preview[index].old_lineno
		} else {
			preview[index].new_lineno
		};
		line_number
			.and_then(|number| number.checked_sub(1))
			.and_then(|number| eligible_lines.get(number as usize))
			.copied()
			.unwrap_or(false)
	})
}

fn is_prose_source_line(line: &str) -> bool {
	let trimmed = line.trim_start();
	!trimmed.is_empty()
		&& !line.starts_with([' ', '\t'])
		&& !is_markdown_boundary(trimmed)
}

fn is_prose_line_allowed(
	map: &[bool],
	line_number: Option<u32>,
) -> bool {
	line_number
		.and_then(|line| line.checked_sub(1))
		.and_then(|line| map.get(line as usize))
		.copied()
		.unwrap_or(false)
}
#[allow(clippy::too_many_lines)]
fn pair_runs(
	rows: &mut Vec<PreviewLine>,
	start: usize,
	prose_maps: Option<&(Vec<bool>, Vec<bool>)>,
) {
	let mut end = rows.len();
	if end > start && rows[end - 1].kind == PreviewLineKind::Context {
		end -= 1;
	}
	let dels: Vec<_> = (start..end)
		.filter(|&i| rows[i].kind == PreviewLineKind::Deletion)
		.collect();
	let adds: Vec<_> = (start..end)
		.filter(|&i| rows[i].kind == PreviewLineKind::Addition)
		.collect();
	if dels.len().saturating_add(adds.len()) > MAX_ALIGNMENT_LINES
		|| dels
			.iter()
			.chain(&adds)
			.map(|&index| rows[index].text.len())
			.sum::<usize>()
			> MAX_ALIGNMENT_BYTES
	{
		return;
	}
	let old_lines = dels
		.iter()
		.map(|&index| rows[index].text.as_str())
		.collect::<Vec<_>>();
	let new_lines = adds
		.iter()
		.map(|&index| rows[index].text.as_str())
		.collect::<Vec<_>>();
	let pairs = align_changed_lines(&old_lines, &new_lines);
	let mut replacements = HashMap::with_capacity(pairs.len());
	let mut valid_pairs = Vec::with_capacity(pairs.len());
	for (old_line, new_line) in pairs {
		let old_index = dels[old_line];
		let new_index = adds[new_line];
		let old_text = rows[old_index].text.clone();
		let new_text = rows[new_index].text.clone();
		let Some(segments) =
			(if prose_maps.is_some_and(|(old_map, new_map)| {
				is_prose_line_allowed(
					old_map,
					rows[old_index].old_lineno,
				) && is_prose_line_allowed(
					new_map,
					rows[new_index].new_lineno,
				)
			}) && is_prose_source_line(&old_text)
				&& is_prose_source_line(&new_text)
			{
				inline_prose_diff_segments(&old_text, &new_text)
			} else {
				inline_diff_segments(&old_text, &new_text)
			})
		else {
			continue;
		};
		let old_lineno = rows[old_index].old_lineno;
		let new_lineno = rows[new_index].new_lineno;
		let old_patch_index = rows[old_index].patch_index;
		let new_patch_index = rows[new_index].patch_index;
		replacements.insert(
			old_line,
			PreviewLine {
				kind: PreviewLineKind::Replacement,
				old_lineno,
				new_lineno,
				text: new_text.clone(),
				patch_index: old_patch_index,
				changed_ranges: vec![],
				block_replacement: None,
				replacement: Some(InlineReplacement {
					old_text,
					new_text,
					old_lineno,
					new_lineno,
					old_patch_index,
					new_patch_index,
					segments,
				}),
			},
		);
		valid_pairs.push((old_line, new_line));
	}
	let Some(order) =
		merge_changed_rows(dels.len(), adds.len(), &valid_pairs)
	else {
		return;
	};
	let mut merged = Vec::with_capacity(order.len());
	for row in order {
		match row {
			MergedChangeRow::OldDeleted(old_index) => {
				merged.push(rows[dels[old_index]].clone());
			}
			MergedChangeRow::NewAdded(new_index) => {
				merged.push(rows[adds[new_index]].clone());
			}
			MergedChangeRow::Replacement {
				old_index,
				new_index: _,
			} => {
				let Some(replacement) =
					replacements.remove(&old_index)
				else {
					return;
				};
				merged.push(replacement);
			}
		}
	}
	rows.splice(start..end, merged);
}

/// Whole-word inline alignment for a single prose line. The block gate is
/// intentionally bypassed here because the surrounding 1:1 line pairing is
/// already the conservative relatedness decision.
pub fn inline_prose_diff_segments(
	old: &str,
	new: &str,
) -> Option<Vec<InlineSegment>> {
	if old.len().saturating_add(new.len()) > MAX_ALIGNMENT_BYTES {
		return None;
	}
	let old_tokens = inline_tokens(old);
	let new_tokens = inline_tokens(new);
	if old_tokens.len() > MAX_BLOCK_TOKENS
		|| new_tokens.len() > MAX_BLOCK_TOKENS
		|| (old_tokens.len() + 1).saturating_mul(new_tokens.len() + 1)
			> MAX_BLOCK_ALIGNMENT_CELLS
	{
		return None;
	}
	let old_indexed = old_tokens
		.iter()
		.enumerate()
		.map(|(index, token)| (index, &old[token.range.clone()]))
		.collect::<Vec<_>>();
	let new_indexed = new_tokens
		.iter()
		.enumerate()
		.map(|(index, token)| (index, &new[token.range.clone()]))
		.collect::<Vec<_>>();
	let matches = token_lcs_matches(
		&old_indexed,
		&new_indexed,
		&AtomicBool::new(false),
	)?;
	let mut segments = Vec::new();
	let (mut old_cursor, mut new_cursor) = (0, 0);
	for (old_match, new_match) in matches {
		append_whole_word_gap(
			old,
			new,
			&old_tokens,
			&new_tokens,
			old_cursor..old_match,
			new_cursor..new_match,
			&mut segments,
		);
		push_inline_segment(
			&mut segments,
			InlineSegmentKind::Shared,
			old_tokens[old_match].range.clone(),
			new_tokens[new_match].range.clone(),
		);
		old_cursor = old_match + 1;
		new_cursor = new_match + 1;
	}
	append_whole_word_gap(
		old,
		new,
		&old_tokens,
		&new_tokens,
		old_cursor..old_tokens.len(),
		new_cursor..new_tokens.len(),
		&mut segments,
	);
	Some(segments)
}

/// Return a bounded inline alignment over UTF-8 byte ranges in the two
/// original lines. Shared, removed, and added spans together reconstruct
/// each input exactly, and every boundary is a grapheme boundary.
pub fn inline_diff_segments(
	old: &str,
	new: &str,
) -> Option<Vec<InlineSegment>> {
	if old.len().saturating_add(new.len()) > MAX_ALIGNMENT_BYTES {
		return None;
	}
	if old == new {
		return Some(vec![InlineSegment {
			kind: InlineSegmentKind::Shared,
			old_range: 0..old.len(),
			new_range: 0..new.len(),
		}]);
	}
	let old_graphemes: Vec<(usize, &str)> =
		old.grapheme_indices(true).collect();
	let new_graphemes: Vec<(usize, &str)> =
		new.grapheme_indices(true).collect();
	let old_tokens = inline_tokens(old);
	let new_tokens = inline_tokens(new);
	let cells =
		(old_tokens.len() + 1).saturating_mul(new_tokens.len() + 1);
	if cells > 16_384 {
		return Some(prefix_suffix_segments(
			old,
			new,
			&old_graphemes,
			&new_graphemes,
		));
	}
	let width = new_tokens.len() + 1;
	let mut scores = vec![0_u16; cells];
	for old_index in (0..old_tokens.len()).rev() {
		for new_index in (0..new_tokens.len()).rev() {
			let cell = old_index * width + new_index;
			scores[cell] = if old[old_tokens[old_index].range.clone()]
				== new[new_tokens[new_index].range.clone()]
			{
				1 + scores[(old_index + 1) * width + new_index + 1]
			} else {
				scores[(old_index + 1) * width + new_index]
					.max(scores[old_index * width + new_index + 1])
			};
		}
	}
	let (mut old_index, mut new_index) = (0, 0);
	let mut segments = Vec::new();
	let mut matches = Vec::new();
	while old_index < old_tokens.len() && new_index < new_tokens.len()
	{
		if old[old_tokens[old_index].range.clone()]
			== new[new_tokens[new_index].range.clone()]
		{
			matches.push((old_index, new_index));
			old_index += 1;
			new_index += 1;
		} else if scores[(old_index + 1) * width + new_index]
			>= scores[old_index * width + new_index + 1]
		{
			old_index += 1;
		} else {
			new_index += 1;
		}
	}
	let mut old_cursor = 0;
	let mut new_cursor = 0;
	for (old_match, new_match) in matches {
		append_token_gap(
			old,
			new,
			&old_tokens,
			&new_tokens,
			old_cursor..old_match,
			new_cursor..new_match,
			&mut segments,
		);
		push_inline_segment(
			&mut segments,
			InlineSegmentKind::Shared,
			old_tokens[old_match].range.clone(),
			new_tokens[new_match].range.clone(),
		);
		old_cursor = old_match + 1;
		new_cursor = new_match + 1;
	}
	append_token_gap(
		old,
		new,
		&old_tokens,
		&new_tokens,
		old_cursor..old_tokens.len(),
		new_cursor..new_tokens.len(),
		&mut segments,
	);
	Some(segments)
}

#[derive(Clone, Debug)]
struct InlineToken {
	range: Range<usize>,
	is_word: bool,
}

fn inline_tokens(text: &str) -> Vec<InlineToken> {
	let graphemes: Vec<(usize, &str)> =
		text.grapheme_indices(true).collect();
	let mut tokens: Vec<InlineToken> = Vec::new();
	for (index, (start, grapheme)) in graphemes.iter().enumerate() {
		let end = graphemes
			.get(index + 1)
			.map_or(text.len(), |(next_start, _)| *next_start);
		let is_word = grapheme.chars().any(char::is_alphanumeric);
		let is_space = grapheme.chars().all(char::is_whitespace);
		let previous_word_end =
			tokens.last().filter(|token| token.is_word);
		let camel_boundary = previous_word_end.is_some_and(|token| {
			let previous = text[token.range.clone()].chars().last();
			let current = grapheme.chars().next();
			matches!((previous, current), (Some(a), Some(b))
				if (a.is_lowercase() || a.is_numeric()) && b.is_uppercase())
		});
		if is_word {
			if camel_boundary {
				tokens.push(InlineToken {
					range: *start..end,
					is_word: true,
				});
			} else if let Some(previous) =
				tokens.last_mut().filter(|token| {
					token.is_word && token.range.end == *start
				}) {
				previous.range.end = end;
			} else {
				tokens.push(InlineToken {
					range: *start..end,
					is_word: true,
				});
			}
		} else if is_space {
			if let Some(previous) =
				tokens.last_mut().filter(|token| {
					!token.is_word
						&& token.range.end == *start
						&& text[token.range.clone()]
							.chars()
							.all(char::is_whitespace)
				}) {
				previous.range.end = end;
			} else {
				tokens.push(InlineToken {
					range: *start..end,
					is_word: false,
				});
			}
		} else {
			tokens.push(InlineToken {
				range: *start..end,
				is_word: false,
			});
		}
	}
	tokens
}

fn append_token_gap(
	old: &str,
	new: &str,
	old_tokens: &[InlineToken],
	new_tokens: &[InlineToken],
	old_gap: Range<usize>,
	new_gap: Range<usize>,
	segments: &mut Vec<InlineSegment>,
) {
	let old_range =
		token_gap_range(old_tokens, old_gap.clone(), old.len());
	let new_range =
		token_gap_range(new_tokens, new_gap.clone(), new.len());
	if old_gap.len() == 1
		&& new_gap.len() == 1
		&& old_tokens[old_gap.start].is_word
		&& new_tokens[new_gap.start].is_word
	{
		append_word_change(old, new, old_range, new_range, segments);
		return;
	}
	if !old_range.is_empty() {
		push_inline_segment(
			segments,
			InlineSegmentKind::Removed,
			old_range.clone(),
			new_range.start..new_range.start,
		);
	}
	if !new_range.is_empty() {
		push_inline_segment(
			segments,
			InlineSegmentKind::Added,
			old_range.end..old_range.end,
			new_range,
		);
	}
}

fn token_gap_range(
	tokens: &[InlineToken],
	gap: Range<usize>,
	text_len: usize,
) -> Range<usize> {
	let start = tokens
		.get(gap.start)
		.map_or(text_len, |token| token.range.start);
	let end = gap
		.end
		.checked_sub(1)
		.and_then(|index| tokens.get(index))
		.map_or(start, |token| token.range.end);
	start..end
}

fn append_word_change(
	old: &str,
	new: &str,
	old_range: Range<usize>,
	new_range: Range<usize>,
	segments: &mut Vec<InlineSegment>,
) {
	let old_word = &old[old_range.clone()];
	let new_word = &new[new_range.clone()];
	let old_graphemes: Vec<(usize, &str)> =
		old_word.grapheme_indices(true).collect();
	let new_graphemes: Vec<(usize, &str)> =
		new_word.grapheme_indices(true).collect();
	let mut prefix = 0;
	while prefix < old_graphemes.len()
		&& prefix < new_graphemes.len()
		&& old_graphemes[prefix].1 == new_graphemes[prefix].1
	{
		prefix += 1;
	}
	let (mut old_end, mut new_end) =
		(old_graphemes.len(), new_graphemes.len());
	while old_end > prefix && new_end > prefix {
		let old_tail = old_graphemes[old_end - 1].1;
		let new_tail = new_graphemes[new_end - 1].1;
		if old_tail != new_tail {
			break;
		}
		old_end -= 1;
		new_end -= 1;
	}
	for index in 0..prefix {
		push_inline_segment(
			segments,
			InlineSegmentKind::Shared,
			absolute_grapheme_range(
				&old_graphemes,
				index,
				old_range.start,
				old_word.len(),
			),
			absolute_grapheme_range(
				&new_graphemes,
				index,
				new_range.start,
				new_word.len(),
			),
		);
	}
	let old_middle =
		grapheme_range(&old_graphemes, prefix, old_word.len());
	let old_end_offset = old_graphemes
		.get(old_end)
		.map_or(old_word.len(), |(offset, _)| *offset);
	let new_middle =
		grapheme_range(&new_graphemes, prefix, new_word.len());
	let new_end_offset = new_graphemes
		.get(new_end)
		.map_or(new_word.len(), |(offset, _)| *offset);
	let old_middle = old_middle.start..old_end_offset;
	let new_middle = new_middle.start..new_end_offset;
	if !old_middle.is_empty() {
		push_inline_segment(
			segments,
			InlineSegmentKind::Removed,
			(old_range.start + old_middle.start)
				..(old_range.start + old_middle.end),
			(new_range.start + new_middle.start)
				..(new_range.start + new_middle.start),
		);
	}
	if !new_middle.is_empty() {
		push_inline_segment(
			segments,
			InlineSegmentKind::Added,
			(old_range.start + old_middle.end)
				..(old_range.start + old_middle.end),
			(new_range.start + new_middle.start)
				..(new_range.start + new_middle.end),
		);
	}
	for index in old_end..old_graphemes.len() {
		let new_index = new_end + index - old_end;
		push_inline_segment(
			segments,
			InlineSegmentKind::Shared,
			absolute_grapheme_range(
				&old_graphemes,
				index,
				old_range.start,
				old_word.len(),
			),
			absolute_grapheme_range(
				&new_graphemes,
				new_index,
				new_range.start,
				new_word.len(),
			),
		);
	}
}

fn absolute_grapheme_range(
	graphemes: &[(usize, &str)],
	index: usize,
	base: usize,
	text_len: usize,
) -> Range<usize> {
	let range = grapheme_range(graphemes, index, text_len);
	(base + range.start)..(base + range.end)
}

fn grapheme_range(
	graphemes: &[(usize, &str)],
	index: usize,
	text_len: usize,
) -> Range<usize> {
	let start =
		graphemes.get(index).map_or(text_len, |(offset, _)| *offset);
	let end = graphemes
		.get(index + 1)
		.map_or(text_len, |(offset, _)| *offset);
	start..end
}

fn push_inline_segment(
	segments: &mut Vec<InlineSegment>,
	kind: InlineSegmentKind,
	old_range: Range<usize>,
	new_range: Range<usize>,
) {
	if let Some(previous) = segments.last_mut() {
		if previous.kind == kind
			&& previous.old_range.end == old_range.start
			&& previous.new_range.end == new_range.start
		{
			previous.old_range.end = old_range.end;
			previous.new_range.end = new_range.end;
			return;
		}
	}
	segments.push(InlineSegment {
		kind,
		old_range,
		new_range,
	});
}

fn prefix_suffix_segments(
	old: &str,
	new: &str,
	old_graphemes: &[(usize, &str)],
	new_graphemes: &[(usize, &str)],
) -> Vec<InlineSegment> {
	let mut prefix = 0;
	while prefix < old_graphemes.len()
		&& prefix < new_graphemes.len()
		&& old_graphemes[prefix].1 == new_graphemes[prefix].1
	{
		prefix += 1;
	}
	let (mut old_end, mut new_end) =
		(old_graphemes.len(), new_graphemes.len());
	while old_end > prefix && new_end > prefix {
		if old_graphemes[old_end - 1].1
			!= new_graphemes[new_end - 1].1
		{
			break;
		}
		old_end -= 1;
		new_end -= 1;
	}
	let mut segments = Vec::new();
	for index in 0..prefix {
		push_inline_segment(
			&mut segments,
			InlineSegmentKind::Shared,
			grapheme_range(old_graphemes, index, old.len()),
			grapheme_range(new_graphemes, index, new.len()),
		);
	}
	let old_start = old_graphemes
		.get(prefix)
		.map_or(old.len(), |(offset, _)| *offset);
	let old_stop = old_graphemes
		.get(old_end)
		.map_or(old.len(), |(offset, _)| *offset);
	let new_start = new_graphemes
		.get(prefix)
		.map_or(new.len(), |(offset, _)| *offset);
	let new_stop = new_graphemes
		.get(new_end)
		.map_or(new.len(), |(offset, _)| *offset);
	if old_start < old_stop {
		push_inline_segment(
			&mut segments,
			InlineSegmentKind::Removed,
			old_start..old_stop,
			new_start..new_start,
		);
	}
	if new_start < new_stop {
		push_inline_segment(
			&mut segments,
			InlineSegmentKind::Added,
			old_stop..old_stop,
			new_start..new_stop,
		);
	}
	for index in old_end..old_graphemes.len() {
		let new_index = new_end + index - old_end;
		push_inline_segment(
			&mut segments,
			InlineSegmentKind::Shared,
			grapheme_range(old_graphemes, index, old.len()),
			grapheme_range(new_graphemes, new_index, new.len()),
		);
	}
	segments
}

/// Align likely counterparts in a replacement block while keeping work
/// bounded by the block and pairwise-grapheme limits above.
/// Return monotonic likely counterpart pairs for one bounded changed block.
pub fn align_changed_lines(
	old_lines: &[&str],
	new_lines: &[&str],
) -> Vec<(usize, usize)> {
	let old_count = old_lines.len();
	let new_count = new_lines.len();
	if old_count == 0
		|| new_count == 0
		|| old_count.saturating_add(new_count) > MAX_ALIGNMENT_LINES
		|| old_lines
			.iter()
			.chain(new_lines)
			.map(|line| line.len())
			.sum::<usize>()
			> MAX_ALIGNMENT_BYTES
	{
		return Vec::new();
	}
	let old_anchors = old_lines
		.iter()
		.map(|line| anchor_words(line))
		.collect::<Vec<_>>();
	let new_anchors = new_lines
		.iter()
		.map(|line| anchor_words(line))
		.collect::<Vec<_>>();
	let old_anchor_counts = anchor_counts(&old_anchors);
	let new_anchor_counts = anchor_counts(&new_anchors);
	let similarities = old_lines
		.iter()
		.enumerate()
		.map(|(old_index, old)| {
			new_lines
				.iter()
				.enumerate()
				.map(|(new_index, new)| {
					let character_score =
						line_similarity_percent(old, new);
					let anchor_score = lexical_anchor_score(
						&old_anchors[old_index],
						&new_anchors[new_index],
						&old_anchor_counts,
						&new_anchor_counts,
					);
					character_score.max(anchor_score)
				})
				.collect::<Vec<_>>()
		})
		.collect::<Vec<_>>();
	let width = new_count + 1;
	let mut scores = vec![0_i32; (old_count + 1) * width];
	let mut actions = vec![AlignmentAction::None; scores.len()];
	for old_end in 1..=old_count {
		for new_end in 1..=new_count {
			let cell = old_end * width + new_end;
			let skip_old = scores[(old_end - 1) * width + new_end];
			let skip_new = scores[old_end * width + new_end - 1];
			let (mut best, mut action) = if skip_old >= skip_new {
				(skip_old, AlignmentAction::SkipOld)
			} else {
				(skip_new, AlignmentAction::SkipNew)
			};
			if let Some(similarity) = similarities[old_end - 1]
				[new_end - 1]
				.filter(|score| *score >= MIN_LINE_SIMILARITY_PERCENT)
			{
				let matched = scores
					[(old_end - 1) * width + new_end - 1]
					+ i32::try_from(similarity).unwrap_or(i32::MAX);
				if matched > best {
					best = matched;
					action = AlignmentAction::Match;
				}
			}
			scores[cell] = best;
			actions[cell] = action;
		}
	}
	let (mut old_end, mut new_end) = (old_count, new_count);
	let mut pairs = Vec::new();
	while old_end > 0 && new_end > 0 {
		match actions[old_end * width + new_end] {
			AlignmentAction::Match => {
				pairs.push((old_end - 1, new_end - 1));
				old_end -= 1;
				new_end -= 1;
			}
			AlignmentAction::SkipOld => old_end -= 1,
			AlignmentAction::SkipNew => new_end -= 1,
			AlignmentAction::None => break,
		}
	}
	pairs.reverse();
	pairs
}

fn anchor_words(line: &str) -> Vec<String> {
	let mut words = Vec::new();
	let mut current = String::new();
	let mut previous = None;
	for character in line.chars() {
		if character.is_alphanumeric() {
			if character.is_uppercase()
				&& previous.is_some_and(char::is_lowercase)
			{
				push_anchor_word(&mut current, &mut words);
			}
			current.extend(character.to_lowercase());
			previous = Some(character);
		} else {
			push_anchor_word(&mut current, &mut words);
			previous = None;
		}
	}
	push_anchor_word(&mut current, &mut words);
	words.sort_unstable();
	words.dedup();
	words
}

fn push_anchor_word(current: &mut String, words: &mut Vec<String>) {
	if current.chars().count() >= 4
		&& !COMMON_ANCHOR_WORDS.contains(&current.as_str())
	{
		words.push(current.clone());
	}
	current.clear();
}

fn anchor_counts(lines: &[Vec<String>]) -> HashMap<String, usize> {
	let mut counts = HashMap::new();
	for word in lines.iter().flatten() {
		*counts.entry(word.clone()).or_insert(0) += 1;
	}
	counts
}

fn lexical_anchor_score(
	old_words: &[String],
	new_words: &[String],
	old_counts: &HashMap<String, usize>,
	new_counts: &HashMap<String, usize>,
) -> Option<usize> {
	let shared = old_words
		.iter()
		.filter(|word| new_words.contains(word))
		.filter(|word| {
			old_counts.get(word.as_str()) == Some(&1)
				&& new_counts.get(word.as_str()) == Some(&1)
		})
		.collect::<Vec<_>>();
	let has_multiple_anchors = shared.len() >= 2;
	let has_long_anchor =
		shared.iter().any(|word| word.chars().count() >= 7);
	if !has_multiple_anchors && !has_long_anchor {
		return None;
	}
	let union_size = old_words
		.len()
		.saturating_add(new_words.len())
		.saturating_sub(shared.len())
		.max(1);
	let overlap_percent = shared.len() * 100 / union_size;
	(overlap_percent >= 25)
		.then_some(MIN_LINE_SIMILARITY_PERCENT.max(overlap_percent))
}

/// One row in the display-only merge of a changed line block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergedChangeRow {
	OldDeleted(usize),
	NewAdded(usize),
	Replacement { old_index: usize, new_index: usize },
}

/// Merge monotonic old/new counterpart pairs into a readable row order.
///
/// Unmatched source and target rows stay in the gaps around their nearest
/// matched rows. Coordinates are local to the old and new changed-line lists.
pub fn merge_changed_rows(
	old_count: usize,
	new_count: usize,
	pairs: &[(usize, usize)],
) -> Option<Vec<MergedChangeRow>> {
	let capacity = old_count.checked_add(new_count)?;
	let mut rows = Vec::with_capacity(capacity);
	let (mut old_cursor, mut new_cursor) = (0, 0);
	for &(old_pair, new_pair) in pairs {
		if old_pair < old_cursor
			|| new_pair < new_cursor
			|| old_pair >= old_count
			|| new_pair >= new_count
		{
			return None;
		}
		while old_cursor < old_pair {
			rows.push(MergedChangeRow::OldDeleted(old_cursor));
			old_cursor += 1;
		}
		while new_cursor < new_pair {
			rows.push(MergedChangeRow::NewAdded(new_cursor));
			new_cursor += 1;
		}
		rows.push(MergedChangeRow::Replacement {
			old_index: old_pair,
			new_index: new_pair,
		});
		old_cursor = old_pair + 1;
		new_cursor = new_pair + 1;
	}
	while old_cursor < old_count {
		rows.push(MergedChangeRow::OldDeleted(old_cursor));
		old_cursor += 1;
	}
	while new_cursor < new_count {
		rows.push(MergedChangeRow::NewAdded(new_cursor));
		new_cursor += 1;
	}
	Some(rows)
}

#[derive(Clone, Copy)]
enum AlignmentAction {
	None,
	SkipOld,
	SkipNew,
	Match,
}

fn line_similarity_percent(old: &str, new: &str) -> Option<usize> {
	let old_graphemes = old.graphemes(true).collect::<Vec<_>>();
	let new_graphemes = new.graphemes(true).collect::<Vec<_>>();
	let denominator = old_graphemes.len().max(new_graphemes.len());
	if denominator == 0 {
		return None;
	}
	if (old_graphemes.len() + 1)
		.saturating_mul(new_graphemes.len() + 1)
		> MAX_LINE_ALIGNMENT_CELLS
	{
		return None;
	}
	let mut previous = vec![0_u16; new_graphemes.len() + 1];
	let mut current = vec![0_u16; new_graphemes.len() + 1];
	for old_grapheme in old_graphemes {
		for (new_index, new_grapheme) in
			new_graphemes.iter().enumerate()
		{
			current[new_index + 1] = if old_grapheme == *new_grapheme
			{
				previous[new_index] + 1
			} else {
				previous[new_index + 1].max(current[new_index])
			};
		}
		std::mem::swap(&mut previous, &mut current);
		current.fill(0);
	}
	let common = usize::from(previous[new_graphemes.len()]);
	if common >= 4 || is_small_numeric_statement_edit(old, new) {
		Some(common * 100 / denominator)
	} else {
		None
	}
}

fn is_small_numeric_statement_edit(old: &str, new: &str) -> bool {
	fn split_statement(line: &str) -> Option<(&str, &str)> {
		let indentation_len =
			line.bytes().take_while(u8::is_ascii_whitespace).count();
		let (indent, statement) = line.split_at(indentation_len);
		let digits = statement.strip_suffix(';')?;
		(!digits.is_empty()
			&& digits.bytes().all(|byte| byte.is_ascii_digit()))
		.then_some((indent, digits))
	}
	let Some((old_indent, old_number)) = split_statement(old) else {
		return false;
	};
	let Some((new_indent, new_number)) = split_statement(new) else {
		return false;
	};
	old_indent == new_indent && old_number != new_number
}
fn display_path(s: &str) -> String {
	s.chars()
		.map(|c| if c.is_control() { '�' } else { c })
		.collect()
}
fn safe_line(b: &[u8]) -> String {
	String::from_utf8_lossy(b)
		.trim_end_matches(['\r', '\n'])
		.chars()
		.map(|c| {
			if c == '\t' {
				' '
			} else if c.is_control() {
				'�'
			} else {
				c
			}
		})
		.collect()
}
#[allow(clippy::too_many_lines)]
fn capture_pair(
	repo_path: &RepoPath,
	params: &DiffParams,
) -> Result<BlobPair> {
	let relative_path = Path::new(&params.path);
	validate_repo_path(relative_path)?;
	let repository = repo(repo_path)?;
	let path = params.path.clone();
	let (old, new, old_path, new_path) = match params.diff_type {
		DiffType::WorkDir => {
			let index = repository.index()?;
			let mut options = git2::DiffOptions::new();
			options
				.include_untracked(true)
				.recurse_untracked_dirs(true);
			let mut diff = repository.diff_index_to_workdir(
				Some(&index),
				Some(&mut options),
			)?;
			let (old_path, new_path) =
				find_rename_paths(&mut diff, &path)?;
			let old =
				content_from_index(&repository, &index, &old_path)?
					.unwrap_or_default();
			let workdir =
				repo_path.workdir().or_else(|| repository.workdir());
			let workdir = workdir.ok_or(crate::Error::NoWorkDir)?;
			let new_relative_path = Path::new(&new_path);
			validate_repo_path(new_relative_path)?;
			let worktree_file = workdir.join(new_relative_path);
			if parent_path_contains_symlink(
				workdir,
				new_relative_path,
			)? {
				return Err(crate::Error::Generic(
					"preview path crosses a symlinked directory"
						.to_string(),
				));
			}
			let new = match read_worktree_file(&worktree_file) {
				Ok(bytes) => bytes,
				Err(error)
					if error.kind() == io::ErrorKind::NotFound =>
				{
					CapturedContent::default()
				}
				Err(error) => return Err(error.into()),
			};
			(old, new, old_path, new_path)
		}
		DiffType::Stage => {
			let old_tree = match repository.head() {
				Ok(head) => {
					let commit = head.peel_to_commit()?;
					Some(commit.tree()?)
				}
				Err(_) => None,
			};
			let index = repository.index()?;
			let mut diff = repository.diff_tree_to_index(
				old_tree.as_ref(),
				Some(&index),
				None,
			)?;
			let (old_path, new_path) =
				find_rename_paths(&mut diff, &path)?;
			let old = old_tree
				.as_ref()
				.map(|tree| {
					content_from_tree(&repository, tree, &old_path)
				})
				.transpose()?
				.flatten()
				.unwrap_or_default();
			let new =
				content_from_index(&repository, &index, &new_path)?
					.unwrap_or_default();
			(old, new, old_path, new_path)
		}
		DiffType::Commit(id) => {
			let commit = repository.find_commit(id.into())?;
			let new_tree = commit.tree()?;
			let old_tree = if commit.parent_count() > 0 {
				let parent =
					repository.find_commit(commit.parent_id(0)?)?;
				Some(parent.tree()?)
			} else {
				None
			};
			let mut diff = repository.diff_tree_to_tree(
				old_tree.as_ref(),
				Some(&new_tree),
				None,
			)?;
			let (old_path, new_path) =
				find_rename_paths(&mut diff, &path)?;
			let old = old_tree
				.as_ref()
				.map(|tree| {
					content_from_tree(&repository, tree, &old_path)
				})
				.transpose()?
				.flatten();
			let new =
				content_from_tree(&repository, &new_tree, &new_path)?;
			(
				old.unwrap_or_default(),
				new.unwrap_or_default(),
				old_path,
				new_path,
			)
		}
		DiffType::Commits(ids) => {
			let old_commit =
				repository.find_commit(ids.old.into())?;
			let new_commit =
				repository.find_commit(ids.new.into())?;
			let old_tree = old_commit.tree()?;
			let new_tree = new_commit.tree()?;
			let mut diff = repository.diff_tree_to_tree(
				Some(&old_tree),
				Some(&new_tree),
				None,
			)?;
			let (old_path, new_path) =
				find_rename_paths(&mut diff, &path)?;
			let old =
				content_from_tree(&repository, &old_tree, &old_path)?
					.unwrap_or_default();
			let new =
				content_from_tree(&repository, &new_tree, &new_path)?
					.unwrap_or_default();
			(old, new, old_path, new_path)
		}
	};
	Ok(BlobPair {
		too_large: old.too_large || new.too_large,
		old,
		new,
		old_path,
		new_path,
	})
}

fn find_rename_paths(
	diff: &mut git2::Diff<'_>,
	selected_path: &str,
) -> Result<(String, String)> {
	let selected_delta = diff.deltas().find(|delta| {
		delta.old_file().path().and_then(Path::to_str)
			== Some(selected_path)
			|| delta.new_file().path().and_then(Path::to_str)
				== Some(selected_path)
	});
	if let Some(delta) = selected_delta {
		if matches!(
			delta.status(),
			Delta::Modified | Delta::Typechange | Delta::Unmodified
		) {
			return Ok((
				selected_path.to_string(),
				selected_path.to_string(),
			));
		}
		if delta.status() == Delta::Renamed {
			let old_path =
				delta.old_file().path().and_then(Path::to_str);
			let new_path =
				delta.new_file().path().and_then(Path::to_str);
			if let (Some(old_path), Some(new_path)) =
				(old_path, new_path)
			{
				return Ok((
					old_path.to_string(),
					new_path.to_string(),
				));
			}
		}
	}
	let mut options = git2::DiffFindOptions::new();
	options.renames(true).for_untracked(true).all(true);
	diff.find_similar(Some(&mut options))?;
	for delta in diff.deltas() {
		if delta.status() != Delta::Renamed {
			continue;
		}
		let old_path = delta.old_file().path().and_then(Path::to_str);
		let new_path = delta.new_file().path().and_then(Path::to_str);
		if old_path == Some(selected_path)
			|| new_path == Some(selected_path)
		{
			if let (Some(old_path), Some(new_path)) =
				(old_path, new_path)
			{
				return Ok((
					old_path.to_string(),
					new_path.to_string(),
				));
			}
		}
	}
	Ok((selected_path.to_string(), selected_path.to_string()))
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
	use std::os::unix::ffi::OsStrExt;
	path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
	path.to_string_lossy().as_bytes().to_vec()
}

fn read_worktree_file(path: &Path) -> io::Result<CapturedContent> {
	let metadata = fs::symlink_metadata(path)?;
	if metadata.file_type().is_symlink() {
		if metadata.len() > MAX_BLOB_BYTES as u64 {
			return Ok(CapturedContent {
				too_large: true,
				fingerprint: Some(worktree_fingerprint(&metadata)),
				..CapturedContent::default()
			});
		}
		return Ok(CapturedContent {
			bytes: path_bytes(&fs::read_link(path)?),
			..CapturedContent::default()
		});
	}
	if !metadata.is_file() {
		return Err(io::Error::new(
			io::ErrorKind::InvalidInput,
			"preview only supports regular files and symlinks",
		));
	}
	if metadata.len() > MAX_BLOB_BYTES as u64 {
		return Ok(CapturedContent {
			too_large: true,
			fingerprint: Some(worktree_fingerprint(&metadata)),
			..CapturedContent::default()
		});
	}
	let file = open_regular_file_nofollow(path)?;
	let mut bytes = Vec::with_capacity(
		usize::try_from(metadata.len().min(MAX_BLOB_BYTES as u64))
			.unwrap_or(MAX_BLOB_BYTES),
	);
	file.take(MAX_BLOB_BYTES as u64 + 1)
		.read_to_end(&mut bytes)?;
	if bytes.len() > MAX_BLOB_BYTES {
		return Ok(CapturedContent {
			too_large: true,
			fingerprint: Some(worktree_fingerprint(&metadata)),
			..CapturedContent::default()
		});
	}
	Ok(CapturedContent {
		bytes,
		..CapturedContent::default()
	})
}

#[cfg(unix)]
fn worktree_fingerprint(metadata: &fs::Metadata) -> u64 {
	use std::os::unix::fs::MetadataExt;
	crate::hash(&(
		metadata.dev(),
		metadata.ino(),
		metadata.len(),
		metadata.mtime(),
		metadata.mtime_nsec(),
		metadata.ctime(),
		metadata.ctime_nsec(),
	))
}

#[cfg(not(unix))]
fn worktree_fingerprint(metadata: &fs::Metadata) -> u64 {
	crate::hash(&(metadata.len(), metadata.modified().ok()))
}

fn validate_repo_path(path: &Path) -> Result<()> {
	if path.is_absolute()
		|| path.components().any(|component| {
			matches!(
				component,
				Component::ParentDir
					| Component::RootDir
					| Component::Prefix(_)
			)
		}) || !path
		.components()
		.any(|component| matches!(component, Component::Normal(_)))
	{
		return Err(crate::Error::Generic(
			"preview path must be a repository-relative file path"
				.to_string(),
		));
	}
	Ok(())
}

#[cfg(unix)]
fn open_regular_file_nofollow(path: &Path) -> io::Result<fs::File> {
	use std::os::unix::fs::OpenOptionsExt;
	fs::OpenOptions::new()
		.read(true)
		.custom_flags(libc::O_NOFOLLOW)
		.open(path)
}

#[cfg(not(unix))]
fn open_regular_file_nofollow(path: &Path) -> io::Result<fs::File> {
	fs::File::open(path)
}

fn parent_path_contains_symlink(
	root: &Path,
	relative: &Path,
) -> Result<bool> {
	let mut parent = root.to_path_buf();
	let components: Vec<_> = relative.components().collect();
	for component in
		components.iter().take(components.len().saturating_sub(1))
	{
		if let Component::Normal(value) = component {
			parent.push(value);
			match fs::symlink_metadata(&parent) {
				Ok(metadata) if metadata.file_type().is_symlink() => {
					return Ok(true)
				}
				Ok(metadata) if metadata.is_dir() => {}
				Ok(_) => return Ok(false),
				Err(error)
					if error.kind() == io::ErrorKind::NotFound =>
				{
					return Ok(false)
				}
				Err(error) => return Err(error.into()),
			}
		}
	}
	Ok(false)
}

fn content_from_tree(
	repository: &Repository,
	tree: &Tree<'_>,
	path: &str,
) -> Result<Option<CapturedContent>> {
	let Ok(entry) = tree.get_path(Path::new(path)) else {
		return Ok(None);
	};
	Ok(Some(read_blob_bounded(repository, entry.id())?))
}

fn content_from_index(
	repository: &Repository,
	index: &Index,
	path: &str,
) -> Result<Option<CapturedContent>> {
	let Some(entry) = index.get_path(Path::new(path), 0) else {
		return Ok(None);
	};
	Ok(Some(read_blob_bounded(repository, entry.id)?))
}

fn read_blob_bounded(
	repository: &Repository,
	id: git2::Oid,
) -> Result<CapturedContent> {
	let (size, kind) = repository.odb()?.read_header(id)?;
	if kind != git2::ObjectType::Blob {
		return Err(crate::Error::Generic(
			"selected path does not contain a blob".to_string(),
		));
	}
	if size > MAX_BLOB_BYTES {
		return Ok(CapturedContent {
			too_large: true,
			fingerprint: Some(crate::hash(&id)),
			..CapturedContent::default()
		});
	}
	Ok(CapturedContent {
		bytes: repository.find_blob(id)?.content().to_vec(),
		..CapturedContent::default()
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::AtomicBool;

	fn preview_line(
		kind: PreviewLineKind,
		text: &str,
	) -> PreviewLine {
		PreviewLine {
			kind,
			old_lineno: None,
			new_lineno: None,
			text: text.to_string(),
			patch_index: 0,
			changed_ranges: vec![],
			replacement: None,
			block_replacement: None,
		}
	}

	#[test]
	fn merged_change_rows_preserve_gaps_before_between_and_after_pairs(
	) {
		assert_eq!(
			merge_changed_rows(3, 5, &[(1, 1), (2, 3)]),
			Some(vec![
				MergedChangeRow::OldDeleted(0),
				MergedChangeRow::NewAdded(0),
				MergedChangeRow::Replacement {
					old_index: 1,
					new_index: 1,
				},
				MergedChangeRow::NewAdded(2),
				MergedChangeRow::Replacement {
					old_index: 2,
					new_index: 3,
				},
				MergedChangeRow::NewAdded(4),
			])
		);
		assert_eq!(
			merge_changed_rows(2, 3, &[(0, 1), (1, 2)]),
			Some(vec![
				MergedChangeRow::NewAdded(0),
				MergedChangeRow::Replacement {
					old_index: 0,
					new_index: 1,
				},
				MergedChangeRow::Replacement {
					old_index: 1,
					new_index: 2,
				},
			])
		);
		assert!(merge_changed_rows(2, 2, &[(1, 0), (0, 1)]).is_none());
	}

	#[test]
	fn grapheme_ranges_are_symmetric_and_boundary_safe() {
		let old = "go 👩‍🚀 cafe\u{301}!";
		let new = "go 👩‍🚀 café?";
		let (a, b) = changed_grapheme_ranges(old, new);
		assert!(!a.is_empty() && !b.is_empty());
		for r in &a {
			assert!(
				old.is_char_boundary(r.start)
					&& old.is_char_boundary(r.end)
			);
		}
		for r in &b {
			assert!(
				new.is_char_boundary(r.start)
					&& new.is_char_boundary(r.end)
			);
		}
		assert_eq!(
			changed_grapheme_ranges("same", "same"),
			(vec![], vec![])
		);
		let (old_ranges, new_ranges) =
			changed_grapheme_ranges("abXcdY", "abUcdV");
		assert_eq!(old_ranges, vec![2..3, 5..6]);
		assert_eq!(new_ranges, vec![2..3, 5..6]);
		let long_old = format!("{}left", "a".repeat(200));
		let long_new = format!("{}right", "a".repeat(200));
		let (old_ranges, new_ranges) =
			changed_grapheme_ranges(&long_old, &long_new);
		assert_eq!(old_ranges, vec![200..203]);
		assert_eq!(new_ranges, vec![200..204]);
	}

	fn reconstruct(
		text: &str,
		segments: &[InlineSegment],
		old_side: bool,
	) -> String {
		segments
			.iter()
			.filter(|segment| {
				segment.kind == InlineSegmentKind::Shared
					|| (old_side
						&& segment.kind == InlineSegmentKind::Removed)
					|| (!old_side
						&& segment.kind == InlineSegmentKind::Added)
			})
			.map(|segment| {
				let range = if old_side {
					&segment.old_range
				} else {
					&segment.new_range
				};
				&text[range.clone()]
			})
			.collect()
	}

	#[test]
	fn inline_segments_reconstruct_multiple_unicode_edits() {
		let old = "vec2 cell_pos = café; // 👩‍🚀";
		let new = "vec2 cell_coord = cafés; // 👩‍🚀";
		let segments = inline_diff_segments(old, new).unwrap();
		assert_eq!(reconstruct(old, &segments, true), old);
		assert_eq!(reconstruct(new, &segments, false), new);
		assert!(segments.iter().any(|segment| segment.kind
			== InlineSegmentKind::Shared
			&& &old[segment.old_range.clone()] == "vec2 cell_"));
		assert!(segments.iter().any(|segment| segment.kind
			== InlineSegmentKind::Shared
			&& &old[segment.old_range.clone()] == "; // 👩‍🚀"));
		let identifier_segments =
			inline_diff_segments("cell_pos;", "cell_coord;").unwrap();
		assert!(identifier_segments.iter().any(|segment| {
			segment.kind == InlineSegmentKind::Removed
				&& &"cell_pos;"[segment.old_range.clone()] == "pos"
		}));
		assert!(identifier_segments.iter().any(|segment| {
			segment.kind == InlineSegmentKind::Added
				&& &"cell_coord;"[segment.new_range.clone()]
					== "coord"
		}));
		for segment in &segments {
			for offset in
				[segment.old_range.start, segment.old_range.end]
			{
				assert!(old.is_char_boundary(offset));
				assert!(
					old.grapheme_indices(true)
						.any(|(start, _)| start == offset)
						|| offset == old.len()
				);
			}
			for offset in
				[segment.new_range.start, segment.new_range.end]
			{
				assert!(new.is_char_boundary(offset));
				assert!(
					new.grapheme_indices(true)
						.any(|(start, _)| start == offset)
						|| offset == new.len()
				);
			}
		}
	}

	#[test]
	fn inline_segments_handle_pure_additions_and_deletions() {
		for (old, new) in
			[("prefix", "prefix added"), ("removed", "")]
		{
			let segments = inline_diff_segments(old, new).unwrap();
			assert_eq!(reconstruct(old, &segments, true), old);
			assert_eq!(reconstruct(new, &segments, false), new);
		}
	}

	#[test]
	fn inline_segments_bound_work_and_keep_exact_fallback_text() {
		let old = "a".repeat(130) + " old " + &"z".repeat(130);
		let new = "a".repeat(130) + " new " + &"z".repeat(130);
		let segments = inline_diff_segments(&old, &new).unwrap();
		assert_eq!(reconstruct(&old, &segments, true), old);
		assert_eq!(reconstruct(&new, &segments, false), new);
		assert_eq!(
			inline_diff_segments(&"x".repeat(4096), "y"),
			None
		);
	}

	fn block_lines<'a>(
		lines: &'a [&'a str],
		start: u32,
	) -> Vec<InlineBlockLine<'a>> {
		lines
			.iter()
			.enumerate()
			.map(|(index, &text)| InlineBlockLine {
				text,
				line_number: Some(start + index as u32),
				patch_index: index + 10,
			})
			.collect()
	}

	fn reconstruct_block_side(
		text: &str,
		segments: &[InlineSegment],
		old_side: bool,
	) -> String {
		segments
			.iter()
			.filter(|segment| match segment.kind {
				InlineSegmentKind::Shared => true,
				InlineSegmentKind::Removed => old_side,
				InlineSegmentKind::Added => !old_side,
			})
			.map(|segment| {
				let range = if old_side {
					segment.old_range.clone()
				} else {
					segment.new_range.clone()
				};
				&text[range]
			})
			.collect()
	}

	#[test]
	fn prose_block_alignment_collapses_reflowed_four_line_rewrite() {
		let old = [
			"The framework provides a simple and reliable way to build",
			"desktop applications with native performance and modern",
			"interfaces. Developers can create rich experiences for",
			"macOS Windows and Linux from one shared codebase.",
		];
		let new = [
			"Build modern desktop software with a reliable native",
			"framework that delivers great performance and lets",
			"developers create rich interfaces across platforms",
			"including Linux, macOS, and Windows.",
		];
		let old_lines = block_lines(&old, 11);
		let new_lines = block_lines(&new, 12);
		let block = inline_diff_block(
			&old_lines,
			&new_lines,
			&AtomicBool::new(false),
		)
		.expect(
			"substantial shared wording should anchor the rewrite",
		);
		assert_eq!(block.old_text, old.join("\n"));
		assert_eq!(block.new_text, new.join("\n"));
		assert_eq!(block.old_lines.len(), 4);
		assert_eq!(block.new_lines.len(), 4);
		assert_eq!(block.old_lines[0].line_number, Some(11));
		assert_eq!(block.new_lines[3].patch_index, 13);
		assert_eq!(
			reconstruct_block_side(
				&block.old_text,
				&block.segments,
				true
			),
			block.old_text
		);
		assert_eq!(
			reconstruct_block_side(
				&block.new_text,
				&block.segments,
				false
			),
			block.new_text
		);
		assert!(block.segments.iter().any(|segment| {
			segment.kind == InlineSegmentKind::Shared
				&& !block.old_text[segment.old_range.clone()]
					.is_empty()
		}));
	}

	#[test]
	fn prose_blocks_preserve_inline_api_semicolons_and_emit_one_preview_row(
	) {
		let old = [
			"The client uses std::collections::HashMap; each request",
			"keeps its cached result available for later rendering.",
		];
		let new = [
			"The client uses std::collections::BTreeMap; each request",
			"keeps its cached result available for later rendering.",
		];
		let old_lines = block_lines(&old, 1);
		let new_lines = block_lines(&new, 1);
		assert!(inline_diff_block(
			&old_lines,
			&new_lines,
			&AtomicBool::new(false),
		)
		.is_some());
		let pair = BlobPair {
			old: CapturedContent {
				bytes: old.join("\n").into_bytes(),
				..Default::default()
			},
			new: CapturedContent {
				bytes: new.join("\n").into_bytes(),
				..Default::default()
			},
			old_path: "README.md".into(),
			new_path: "README.md".into(),
			too_large: false,
		};
		let params = DiffParams {
			path: "README.md".into(),
			diff_type: DiffType::WorkDir,
			options: crate::sync::diff::DiffOptions {
				context: 0,
				..Default::default()
			},
		};
		let preview = render_pair(
			&pair,
			&params,
			PreviewOptions::default(),
			&AtomicBool::new(false),
		);
		let blocks = preview
			.lines
			.iter()
			.filter(|line| {
				line.kind == PreviewLineKind::BlockReplacement
			})
			.collect::<Vec<_>>();
		assert_eq!(blocks.len(), 1);
		let block = blocks[0].block_replacement.as_ref().unwrap();
		assert_eq!(block.old_lines.len(), 1);
		assert_eq!(block.new_lines.len(), 1);
	}

	#[test]
	fn prose_block_rejects_unrelated_code_lists_and_bounded_overflow()
	{
		let unrelated_old = block_lines(
			&[
				"Quartz lanterns drift quietly",
				"Obsidian rivers fold inward",
			],
			1,
		);
		let unrelated_new = block_lines(
			&[
				"Tulip engines wake brightly",
				"Cobalt planets hum softly",
			],
			1,
		);
		assert!(inline_diff_block(
			&unrelated_old,
			&unrelated_new,
			&AtomicBool::new(false),
		)
		.is_none());
		let list = block_lines(&["- keep this list item"], 1);
		assert!(inline_diff_block(
			&list,
			&list,
			&AtomicBool::new(false),
		)
		.is_none());
		let code = block_lines(&["fn update_value() {"], 1);
		assert!(inline_diff_block(
			&code,
			&code,
			&AtomicBool::new(false),
		)
		.is_none());
		let oversized_line = "word ".repeat(900);
		let oversized_lines = [oversized_line.as_str()];
		let too_large = block_lines(&oversized_lines, 1);
		assert!(inline_diff_block(
			&too_large,
			&too_large,
			&AtomicBool::new(false),
		)
		.is_none());
	}

	#[test]
	fn prose_block_unicode_ranges_and_cancelled_work_are_safe() {
		let old = [
			"The café renders a 🌙 icon beside the shared cached response",
			"while preserving the original Unicode text for every reader.",
		];
		let new = [
			"The café displays a 🌙 icon beside the shared cached response",
			"while preserving the original Unicode text for every client.",
		];
		let old_lines = block_lines(&old, 8);
		let new_lines = block_lines(&new, 9);
		let block = inline_diff_block(
			&old_lines,
			&new_lines,
			&AtomicBool::new(false),
		)
		.expect("shared wording anchors this short Unicode rewrite");
		assert_eq!(
			reconstruct_block_side(
				&block.old_text,
				&block.segments,
				true
			),
			block.old_text
		);
		assert_eq!(
			reconstruct_block_side(
				&block.new_text,
				&block.segments,
				false
			),
			block.new_text
		);
		for segment in &block.segments {
			for (text, range) in [
				(&block.old_text, &segment.old_range),
				(&block.new_text, &segment.new_range),
			] {
				for offset in [range.start, range.end] {
					assert!(text.is_char_boundary(offset));
					assert!(
						text.grapheme_indices(true)
							.any(|(start, _)| start == offset)
							|| offset == text.len()
					);
				}
			}
		}
		assert_eq!(block.old_lines[0].line_number, Some(8));
		assert_eq!(block.new_lines[1].line_number, Some(10));
		assert!(inline_diff_block(
			&old_lines,
			&new_lines,
			&AtomicBool::new(true),
		)
		.is_none());
	}

	#[test]
	fn prose_alignment_prefers_distinctive_platform_anchor_once() {
		let old = [
			"The renderer supports macOS and Windows through native views.",
			"Flexbox layout keeps spacing predictable across every platform.",
		];
		let new = [
			"Native views on macOS and Windows keep the renderer predictable.",
			"Flexbox layout supports every platform with stable spacing.",
		];
		let block = inline_diff_block(
			&block_lines(&old, 1),
			&block_lines(&new, 4),
			&AtomicBool::new(false),
		)
		.expect(
			"distinctive platform and layout words should anchor",
		);
		let macos_shared = block
			.segments
			.iter()
			.filter(|segment| {
				segment.kind == InlineSegmentKind::Shared
					&& block.old_text[segment.old_range.clone()]
						.contains("macOS")
			})
			.count();
		assert_eq!(macos_shared, 1);
	}

	#[test]
	fn prose_changed_words_remain_whole_segments() {
		let old = [
			"The renderer provides readable draw views for macOS users.",
			"Native layout keeps every platform stable and predictable.",
		];
		let new = [
			"The renderer provides readable draviews for macOS users.",
			"Native layout keeps every platform stable and predictable.",
		];
		let block = inline_diff_block(
			&block_lines(&old, 1),
			&block_lines(&new, 3),
			&AtomicBool::new(false),
		)
		.expect("shared prose should anchor the rewrite");
		assert!(block.segments.iter().any(|segment| {
			segment.kind == InlineSegmentKind::Removed
				&& &block.old_text[segment.old_range.clone()]
					== "draw views"
		}));
		assert!(block.segments.iter().any(|segment| {
			segment.kind == InlineSegmentKind::Added
				&& &block.new_text[segment.new_range.clone()]
					== "draviews"
		}));
	}

	#[test]
	fn prose_alignment_keeps_contiguous_common_phrase_with_rare_words(
	) {
		let old = [
			"The UI framework uses Rust and supports macOS windows.",
			"Native layout keeps every platform stable and predictable.",
		];
		let new = [
			"The UI framework uses Rust and supports Windows windows.",
			"Native layout keeps every platform stable and predictable.",
		];
		let block = inline_diff_block(
			&block_lines(&old, 1),
			&block_lines(&new, 3),
			&AtomicBool::new(false),
		)
		.expect(
			"common phrase and rare anchors should remain related",
		);
		assert!(block.segments.iter().any(|segment| {
			segment.kind == InlineSegmentKind::Shared
				&& block.old_text[segment.old_range.clone()]
					.contains("UI framework")
		}));
	}

	#[test]
	fn single_line_prose_alignment_rejects_token_matrix_overflow() {
		let old = "a ".repeat(1_000);
		let new = "b ".repeat(1_000);
		assert!(inline_prose_diff_segments(&old, &new).is_none());
	}

	#[test]
	fn prose_preview_respects_fences_that_start_outside_patch_context(
	) {
		let old_source = b"intro\none\ntwo\nthree\nfour\nfive\n```rust\nold value here for preview\n```\ntail\n";
		let new_source = b"intro\none\ntwo\nthree\nfour\nfive\n```rust\nnew value here for preview\n```\ntail\n";
		let pair = BlobPair {
			old: CapturedContent {
				bytes: old_source.to_vec(),
				..Default::default()
			},
			new: CapturedContent {
				bytes: new_source.to_vec(),
				..Default::default()
			},
			old_path: "README.md".into(),
			new_path: "README.md".into(),
			too_large: false,
		};
		let params = DiffParams {
			path: "README.md".into(),
			diff_type: DiffType::WorkDir,
			options: crate::sync::diff::DiffOptions {
				context: 0,
				..Default::default()
			},
		};
		let preview = render_pair(
			&pair,
			&params,
			PreviewOptions::default(),
			&AtomicBool::new(false),
		);
		assert!(!preview.lines.iter().any(|line| {
			line.kind == PreviewLineKind::BlockReplacement
		}));
	}

	#[test]
	fn contextual_patch_keeps_neighbors_and_pairs_changed_rows() {
		let pair = BlobPair {
			old: CapturedContent {
				bytes: b"before\nfn add() { 1 }\nafter\n".to_vec(),
				..Default::default()
			},
			new: CapturedContent {
				bytes: b"before\nfn add() { 2 }\nafter\n".to_vec(),
				..Default::default()
			},
			old_path: "sample.rs".into(),
			new_path: "sample.rs".into(),
			too_large: false,
		};
		let params = DiffParams {
			path: String::from("sample.rs"),
			diff_type: DiffType::WorkDir,
			options: crate::sync::diff::DiffOptions {
				context: 1,
				..Default::default()
			},
		};
		let inline_preview = render_pair(
			&pair,
			&params,
			PreviewOptions {
				preference: PreviewPreference::Auto,
			},
			&AtomicBool::new(false),
		);
		let plain_preview = render_pair(
			&pair,
			&params,
			PreviewOptions {
				preference: PreviewPreference::Unified,
			},
			&AtomicBool::new(false),
		);
		let rows = &inline_preview.lines;
		assert!(rows.iter().any(|x| x.text == "before"));
		assert!(rows.iter().any(|x| x.text == "after"));
		let replacement = rows
			.iter()
			.find(|x| x.kind == PreviewLineKind::Replacement)
			.unwrap();
		let replacement = replacement.replacement.as_ref().unwrap();
		assert_eq!(replacement.old_text, "fn add() { 1 }");
		assert_eq!(replacement.new_text, "fn add() { 2 }");
		assert_eq!(replacement.old_lineno, Some(2));
		assert_eq!(replacement.new_lineno, Some(2));
		assert!(replacement
			.segments
			.iter()
			.any(|segment| segment.kind == InlineSegmentKind::Shared));
		let raw_old = plain_preview
			.lines
			.iter()
			.find(|line| line.kind == PreviewLineKind::Deletion)
			.unwrap();
		let raw_new = plain_preview
			.lines
			.iter()
			.find(|x| x.kind == PreviewLineKind::Addition)
			.unwrap();
		assert_eq!(raw_old.text, replacement.old_text);
		assert_eq!(raw_new.text, replacement.new_text);
		assert_eq!(raw_old.patch_index, replacement.old_patch_index);
		assert_eq!(raw_new.patch_index, replacement.new_patch_index);
		assert!(plain_preview.lines.iter().any(|line| {
			line.kind == PreviewLineKind::Deletion
				&& line.text == "fn add() { 1 }"
		}));
		assert!(plain_preview.lines.iter().any(|line| {
			line.kind == PreviewLineKind::Addition
				&& line.text == "fn add() { 2 }"
		}));
	}

	#[test]
	fn compact_alignment_does_not_cross_an_unchanged_context_line() {
		let pair = BlobPair {
			old: CapturedContent {
				bytes: b"fn load_configuration(path: &Path) -> Result<Config> {\nunchanged();\nfinish();\n".to_vec(),
				..Default::default()
			},
			new: CapturedContent {
				bytes: b"const MAX_RETRIES: usize = 8;\nunchanged();\nfinish();\n".to_vec(),
				..Default::default()
			},
			old_path: "sample.rs".into(),
			new_path: "sample.rs".into(),
			too_large: false,
		};
		let params = DiffParams {
			path: String::from("sample.rs"),
			diff_type: DiffType::WorkDir,
			options: crate::sync::diff::DiffOptions {
				context: 2,
				..Default::default()
			},
		};
		let preview = render_pair(
			&pair,
			&params,
			PreviewOptions::default(),
			&AtomicBool::new(false),
		);
		assert!(preview
			.lines
			.iter()
			.any(|line| line.kind == PreviewLineKind::Deletion));
		assert!(preview
			.lines
			.iter()
			.any(|line| line.kind == PreviewLineKind::Addition));
		assert!(!preview
			.lines
			.iter()
			.any(|line| line.kind == PreviewLineKind::Replacement));
	}

	#[test]
	fn structural_regions_use_captured_sources_only_for_auto_previews(
	) {
		let pair = BlobPair {
			old: CapturedContent {
				bytes: b"fn add() {\n    let value = 1;\n}\n"
					.to_vec(),
				..Default::default()
			},
			new: CapturedContent {
				bytes: b"fn add() {\n    let value = 2;\n}\n"
					.to_vec(),
				..Default::default()
			},
			old_path: "sample.rs".into(),
			new_path: "sample.rs".into(),
			too_large: false,
		};
		let params = DiffParams {
			path: String::from("sample.rs"),
			diff_type: DiffType::WorkDir,
			options: crate::sync::diff::DiffOptions::default(),
		};
		let auto = render_pair(
			&pair,
			&params,
			PreviewOptions::default(),
			&AtomicBool::new(false),
		);
		let structural = auto.structural.expect("Rust source parses");
		assert_eq!(structural.language, "Rust");
		assert!(structural.regions.iter().any(|region| {
			region.kind == "function"
				&& region.label == "add"
				&& region.contains_changes
		}));

		let unified = render_pair(
			&pair,
			&params,
			PreviewOptions {
				preference: PreviewPreference::Unified,
			},
			&AtomicBool::new(false),
		);
		assert!(unified.structural.is_none());

		let cancelled = render_pair(
			&pair,
			&params,
			PreviewOptions::default(),
			&AtomicBool::new(true),
		);
		assert_eq!(cancelled.status, PreviewStatus::Cancelled);
		assert!(cancelled.structural.is_none());
	}

	#[test]
	fn interhunk_option_merges_separated_edits_in_the_preview() {
		let pair = BlobPair {
			old: CapturedContent {
				bytes: b"a\nold one\nunchanged one\nunchanged two\nold two\nz\n".to_vec(),
				..Default::default()
			},
			new: CapturedContent {
				bytes: b"a\nnew one\nunchanged one\nunchanged two\nnew two\nz\n".to_vec(),
				..Default::default()
			},
			old_path: "sample.txt".into(),
			new_path: "sample.txt".into(),
			too_large: false,
		};
		let make_preview = |interhunk_lines| {
			let params = DiffParams {
				path: String::from("sample.txt"),
				diff_type: DiffType::WorkDir,
				options: crate::sync::diff::DiffOptions {
					context: 0,
					interhunk_lines,
					..Default::default()
				},
			};
			render_pair(
				&pair,
				&params,
				PreviewOptions {
					preference: PreviewPreference::Unified,
				},
				&AtomicBool::new(false),
			)
		};
		let separate_hunks = make_preview(0)
			.lines
			.iter()
			.filter(|line| line.kind == PreviewLineKind::HunkHeader)
			.count();
		let merged_hunks = make_preview(2)
			.lines
			.iter()
			.filter(|line| line.kind == PreviewLineKind::HunkHeader)
			.count();
		assert_eq!(separate_hunks, 2);
		assert_eq!(merged_hunks, 1);
	}

	#[test]
	fn inserted_function_does_not_steal_changed_function_pairing() {
		let mut rows = vec![
			preview_line(
				PreviewLineKind::Deletion,
				"fn validate_input(value: &str) -> bool { value.len() > 2 }",
			),
			preview_line(
				PreviewLineKind::Addition,
				"fn helper() { prepare_defaults(); }",
			),
			preview_line(
				PreviewLineKind::Addition,
				"fn validate_input(value: &str) -> bool { value.len() >= 3 }",
			),
		];
		pair_runs(&mut rows, 0, None);
		assert_eq!(rows.len(), 2);
		assert_eq!(rows[0].kind, PreviewLineKind::Addition);
		assert_eq!(rows[1].kind, PreviewLineKind::Replacement);
		assert_eq!(rows[1].replacement.as_ref().unwrap().new_text,
			"fn validate_input(value: &str) -> bool { value.len() >= 3 }");
		assert_eq!(
			rows[0].text,
			"fn helper() { prepare_defaults(); }"
		);
	}

	#[test]
	fn multiple_edits_align_in_order_around_insertions() {
		let mut rows = vec![
			preview_line(
				PreviewLineKind::Deletion,
				"let alpha = calculate(input);",
			),
			preview_line(
				PreviewLineKind::Deletion,
				"let beta = validate(input);",
			),
			preview_line(
				PreviewLineKind::Addition,
				"let helper = Default::default();",
			),
			preview_line(
				PreviewLineKind::Addition,
				"let alpha = calculate(source);",
			),
			preview_line(
				PreviewLineKind::Addition,
				"let beta = validate(source).is_ok();",
			),
		];
		pair_runs(&mut rows, 0, None);
		assert_eq!(rows.len(), 3);
		assert_eq!(rows[0].kind, PreviewLineKind::Addition);
		assert!(rows[1..]
			.iter()
			.all(|row| row.kind == PreviewLineKind::Replacement));
		assert_eq!(rows[0].text, "let helper = Default::default();");
	}

	#[test]
	fn lexical_anchors_pair_four_rewritten_lines_without_absorbing_gaps(
	) {
		let old = [
			"let decoded_locator = parse_locator(raw_path);",
			"let previous_revision = fetch_previous_revision(commit_key);",
			"let selected_document = open_document(document_id);",
			"let terminal_status = status_for_response(response_code);",
			"fn obsolete_manifest() { erase_expired_record(); }",
		];
		let new = [
			"fn setup_index() { prime_catalog(); }",
			"let canonical_locator = validate_locator(raw_path);",
			"let saved_revision = retrieve_saved_revision(commit_key);",
			"let cached_document = restore_document(document_id);",
			"let final_status = derive_status(response_code);",
			"fn archive_ui() { prepare_gradient(); }",
		];
		assert_eq!(
			align_changed_lines(&old, &new),
			vec![(0, 1), (1, 2), (2, 3), (3, 4)]
		);
		assert_eq!(
			merge_changed_rows(
				5,
				6,
				&[(0, 1), (1, 2), (2, 3), (3, 4)]
			),
			Some(vec![
				MergedChangeRow::NewAdded(0),
				MergedChangeRow::Replacement {
					old_index: 0,
					new_index: 1,
				},
				MergedChangeRow::Replacement {
					old_index: 1,
					new_index: 2,
				},
				MergedChangeRow::Replacement {
					old_index: 2,
					new_index: 3,
				},
				MergedChangeRow::Replacement {
					old_index: 3,
					new_index: 4,
				},
				MergedChangeRow::OldDeleted(4),
				MergedChangeRow::NewAdded(5),
			])
		);
	}

	#[test]
	fn lexical_anchors_leave_equal_count_unrelated_lines_unpaired() {
		let old = [
			"let weather_profile = load_station(forecast_id);",
			"let humidity_report = calculate_rainfall(weather_profile);",
			"let regional_warning = inspect_temperature(forecast_id);",
			"Ok(WeatherSummary::from(regional_warning))",
		];
		let new = [
			"let invoice_bundle = save_vendor(record_key);",
			"let storage_ledger = calculate_archive_cost(invoice_bundle);",
			"let purchase_notice = inspect_shipping(record_key);",
			"Ok(InvoicePacket::from(purchase_notice))",
		];
		assert!(align_changed_lines(&old, &new).is_empty());
	}

	#[test]
	fn lexical_alignment_handles_unicode_tokens_and_crlf_text() {
		let old = "let résumé_locator = parse_locator(路径);\r\n";
		let new = "let résumé_locator = validate_locator(路径);\r\n";
		assert_eq!(align_changed_lines(&[old], &[new]), vec![(0, 0)]);
		let segments = inline_diff_segments(old, new).unwrap();
		assert_eq!(reconstruct(old, &segments, true), old);
		assert_eq!(reconstruct(new, &segments, false), new);
	}

	#[test]
	fn deletion_before_modified_line_does_not_shift_pairing() {
		let mut rows = vec![
			preview_line(
				PreviewLineKind::Deletion,
				"fn obsolete_helper() {}",
			),
			preview_line(
				PreviewLineKind::Deletion,
				"let count = total + 1;",
			),
			preview_line(
				PreviewLineKind::Addition,
				"let count = total + 2;",
			),
		];
		pair_runs(&mut rows, 0, None);
		assert_eq!(rows[0].kind, PreviewLineKind::Deletion);
		assert_eq!(rows[1].kind, PreviewLineKind::Replacement);
		assert_eq!(rows.len(), 2);
	}

	#[test]
	fn one_digit_statement_change_is_compact_but_unrelated_numbers_are_not(
	) {
		let mut rows = vec![
			preview_line(PreviewLineKind::Deletion, " 1;"),
			preview_line(PreviewLineKind::Addition, " 2;"),
		];
		pair_runs(&mut rows, 0, None);
		assert_eq!(rows.len(), 1);
		assert_eq!(rows[0].kind, PreviewLineKind::Replacement);
		let segments =
			&rows[0].replacement.as_ref().unwrap().segments;
		assert!(segments.iter().any(|segment| {
			segment.kind == InlineSegmentKind::Shared
				&& &" 1;"[segment.old_range.clone()] == " "
		}));
		assert!(segments.iter().any(|segment| {
			segment.kind == InlineSegmentKind::Shared
				&& &" 1;"[segment.old_range.clone()] == ";"
		}));

		let mut unrelated = vec![
			preview_line(PreviewLineKind::Deletion, " 1;"),
			preview_line(PreviewLineKind::Addition, "foo();"),
		];
		pair_runs(&mut unrelated, 0, None);
		assert_eq!(unrelated.len(), 2);
	}

	#[test]
	fn unrelated_replacements_keep_whole_line_highlighting() {
		let mut rows = vec![
			preview_line(
				PreviewLineKind::Deletion,
				"fn load_configuration(path: &Path) -> Result<Config> {",
			),
			preview_line(
				PreviewLineKind::Addition,
				"const MAX_RETRIES: usize = 8;",
			),
		];
		pair_runs(&mut rows, 0, None);
		assert!(rows.iter().all(|row| {
			row.kind == PreviewLineKind::Deletion
				|| row.kind == PreviewLineKind::Addition
		}));
	}

	#[test]
	fn oversized_run_skips_line_alignment_and_keeps_grapheme_fallback_bounded(
	) {
		let old = format!("{}left", "x".repeat(3_000));
		let new = format!("{}right", "x".repeat(3_000));
		assert!(align_changed_lines(&[&old], &[&new]).is_empty());
		let mut rows = vec![
			preview_line(PreviewLineKind::Deletion, &old),
			preview_line(PreviewLineKind::Addition, &new),
		];
		pair_runs(&mut rows, 0, None);
		assert!(rows.iter().all(|row| {
			row.kind == PreviewLineKind::Deletion
				|| row.kind == PreviewLineKind::Addition
		}));
		let (old_ranges, new_ranges) =
			changed_grapheme_ranges(&old, &new);
		assert_eq!(old_ranges, vec![3_000..3_003]);
		assert_eq!(new_ranges, vec![3_000..3_004]);
	}

	#[test]
	fn controls_are_replaced_before_frontend_display() {
		assert_eq!(safe_line(b"one\x1b[31m\t\x7f\n"), "one�[31m �");
	}
}
