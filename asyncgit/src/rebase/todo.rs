//! Git's rebase todo list as a lossless document.
//!
//! A todo file is a line-based text file with a tiny grammar
//! (`pick <commit> # <subject>`, `exec <shell>`, `label`,
//! `merge`, …) plus comments, blanks, and lines gitui does not
//! understand. Unknown syntax is never rewritten and blocks
//! execution of a plan containing it — the honest failure mode
//! for "a newer git wrote this".
//!
//! ## Losslessness by construction
//!
//! For any input bytes `b`: `Todo::parse(b).to_bytes() == b`.
//! This holds structurally, not by stashing originals beside
//! the parse tree: every instruction remembers the exact bytes
//! of its parts — the word as spelled (`p`, `PICK`, …), the
//! commit argument as written (full or abbreviated hex), any
//! tolerated trailing whitespace, the comment — so
//! serialization re-emits them unchanged. A line only
//! serializes differently when its instruction was *replaced*
//! (by an edit), and then it serializes canonically.
//!
//! Instructions are recognized in canonical spacing: exactly
//! one space between fields, optional tolerated tail
//! whitespace, optional ` # comment`. Git accepts slightly more
//! (double spaces, tabs); such lines stay [`TodoLine::Raw`]
//! here — visible, preserved, and barred from execution rather
//! than silently normalized. Lines git itself rejects (bare
//! `exec`, argument-less `label`, …) are `Raw` for the same
//! reason git refuses them.
//!
//! ## Commit arguments
//!
//! Git writes *abbreviated* object ids into todos and accepts
//! any unambiguous prefix back, so [`CommitArg`] stores the
//! token as written; [`CommitArg::full`] yields the id only
//! for full-length hex. Plans gitui builds itself always carry
//! full ids (see [`CommitArg::from_id`]).
//!
//! ## Byte safety
//!
//! Instruction arguments (`exec` shell text, labels, refs) are
//! raw bytes: commit messages and shell commands may
//! legitimately be non-UTF-8. Display code converts lossily;
//! serialization never converts at all.

use crate::sync::CommitId;

/// A parsed todo document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Todo {
	lines: Vec<TodoLine>,
	/// Whether the source ended with a newline. Documents
	/// built in memory default to having one, like the files
	/// git writes.
	trailing_newline: bool,
}

impl Default for Todo {
	fn default() -> Self {
		Self {
			lines: Vec::new(),
			trailing_newline: true,
		}
	}
}

/// One physical line of the document (newline not included).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TodoLine {
	/// A recognized instruction.
	Instruction(Instruction),
	/// A comment line; bytes include the leading `#`.
	Comment(Box<[u8]>),
	/// An empty line.
	Blank,
	/// A line we could not classify. Preserved verbatim forever.
	Raw(Box<[u8]>),
}

/// The todo instruction set, as git defines it. Every variant
/// keeps the source spelling of its word and arguments.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Instruction {
	/// Apply a commit.
	Pick {
		/// The word as spelled in the source.
		word: Box<[u8]>,
		/// The commit argument as written.
		commit: CommitArg,
		/// The exact bytes following the commit argument:
		/// tolerated whitespace and an optional `# comment`,
		/// stored verbatim so serialization is lossless.
		after: Box<[u8]>,
	},
	/// Apply, then let the user reword the message.
	Reword {
		/// The word as spelled in the source.
		word: Box<[u8]>,
		/// The commit argument as written.
		commit: CommitArg,
		/// The exact bytes following the commit argument:
		/// tolerated whitespace and an optional `# comment`,
		/// stored verbatim so serialization is lossless.
		after: Box<[u8]>,
	},
	/// Apply, then stop so the user can amend or split.
	Edit {
		/// The word as spelled in the source.
		word: Box<[u8]>,
		/// The commit argument as written.
		commit: CommitArg,
		/// The exact bytes following the commit argument:
		/// tolerated whitespace and an optional `# comment`,
		/// stored verbatim so serialization is lossless.
		after: Box<[u8]>,
	},
	/// Apply onto the previous commit; meld messages.
	Squash {
		/// The word as spelled in the source.
		word: Box<[u8]>,
		/// The commit argument as written.
		commit: CommitArg,
		/// The exact bytes following the commit argument:
		/// tolerated whitespace and an optional `# comment`,
		/// stored verbatim so serialization is lossless.
		after: Box<[u8]>,
	},
	/// Apply onto the previous commit; message policy below.
	Fixup {
		/// The word as spelled in the source.
		word: Box<[u8]>,
		/// Which message survives.
		message: FixupMessage,
		/// The commit argument as written.
		commit: CommitArg,
		/// The exact bytes following the commit argument:
		/// tolerated whitespace and an optional `# comment`,
		/// stored verbatim so serialization is lossless.
		after: Box<[u8]>,
	},
	/// Omit the commit entirely.
	Drop {
		/// The word as spelled in the source.
		word: Box<[u8]>,
		/// The commit argument as written.
		commit: CommitArg,
		/// The exact bytes following the commit argument:
		/// tolerated whitespace and an optional `# comment`,
		/// stored verbatim so serialization is lossless.
		after: Box<[u8]>,
	},
	/// Stop here; no commit involved.
	Break {
		/// The word as spelled in the source.
		word: Box<[u8]>,
		/// Tolerated trailing whitespace, stored verbatim.
		after: Box<[u8]>,
	},
	/// Run a shell command. Raw bytes, verbatim, never empty
	/// (git rejects empty `exec` and so does parsing).
	Exec {
		/// The word as spelled in the source.
		word: Box<[u8]>,
		/// Everything after the separating space.
		command: Box<[u8]>,
	},
	/// Name the current HEAD (topology programs).
	Label {
		/// The word as spelled in the source.
		word: Box<[u8]>,
		/// The label name, raw bytes.
		label: Box<[u8]>,
	},
	/// Jump to a label (topology programs).
	Reset {
		/// The word as spelled in the source.
		word: Box<[u8]>,
		/// The label name, raw bytes.
		label: Box<[u8]>,
	},
	/// Create a merge commit against labels (topology programs).
	Merge {
		/// The word as spelled in the source.
		word: Box<[u8]>,
		/// Where the message comes from.
		message: MergeMessage,
		/// The non-HEAD parents: labels, raw bytes.
		parents: Vec<Box<[u8]>>,
		/// Trailing `# oneline` comment, if the source had one.
		comment: Option<Box<[u8]>>,
	},
	/// Move a ref here at the end of the rebase.
	UpdateRef {
		/// The word as spelled in the source.
		word: Box<[u8]>,
		/// The ref name, raw bytes.
		reference: Box<[u8]>,
	},
	/// Explicit no-op used by topology programs.
	Noop {
		/// The word as spelled in the source.
		word: Box<[u8]>,
		/// Tolerated trailing whitespace, stored verbatim.
		after: Box<[u8]>,
	},
}

/// Message policy of a `fixup` instruction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FixupMessage {
	/// Keep the previous commit's message (plain `fixup`).
	#[default]
	Previous,
	/// Take this commit's message (`fixup -C`, git ≥ 2.32).
	ThisCommit,
	/// Take and edit this commit's message (`fixup -c`).
	EditThisCommit,
}

/// Message source of a `merge` instruction.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MergeMessage {
	/// Original merge commit whose message to reuse (`-C`).
	FromCommit(CommitArg),
	/// Like [`MergeMessage::FromCommit`] but open an editor.
	EditFromCommit(CommitArg),
	/// No original; the trailing comment line is the oneline.
	Oneline,
}

/// A commit argument, stored exactly as written. Git writes
/// abbreviated ids into todos and accepts any unambiguous
/// prefix back; plans built by gitui carry full ids.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CommitArg(Box<[u8]>);

impl CommitArg {
	/// The full id, when the argument is full-length hex
	/// (40 sha1 / 64 sha256 digits).
	#[must_use]
	pub fn full(&self) -> Option<CommitId> {
		let hex = std::str::from_utf8(&self.0).ok()?;
		if !matches!(hex.len(), 40 | 64) {
			return None;
		}
		if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
			return None;
		}
		// `from_str` accepts prefixes, hence the length check
		// above; 64-hex stays unparsed in a sha1-only git2
		// build and yields `None` here, losslessly.
		git2::Oid::from_str(hex).ok().map(CommitId::new)
	}

	/// The canonical full-hex argument for a known id.
	#[must_use]
	pub fn from_id(id: CommitId) -> Self {
		Self(id.to_string().into_bytes().into_boxed_slice())
	}

	/// The raw argument bytes as written.
	#[must_use]
	pub const fn as_bytes(&self) -> &[u8] {
		&self.0
	}

	/// Whether this argument could name `id`: identical full
	/// hex, or an unambiguous-looking prefix of it. Used to
	/// compare git's generated (possibly abbreviated) todo
	/// against our full-id plans without touching the
	/// repository.
	#[must_use]
	pub fn names(&self, id: &CommitId) -> bool {
		let full = id.to_string();
		let arg = &self.0;
		arg.len() <= full.len()
			&& arg[..] == full.as_bytes()[..arg.len()]
	}
}

impl Instruction {
	/// A canonical `break` instruction (gitui-built).
	#[must_use]
	pub fn break_instruction() -> Self {
		Self::Break {
			word: b"break".as_slice().into(),
			after: b"".as_slice().into(),
		}
	}

	/// A canonical `exec` instruction for `command`
	/// (gitui-built). Empty commands are refused the same
	/// way parsing refuses them.
	#[must_use]
	pub fn exec(command: &[u8]) -> Option<Self> {
		(!command.is_empty()).then(|| Self::Exec {
			word: b"exec".as_slice().into(),
			command: command.to_vec().into_boxed_slice(),
		})
	}

	/// A canonical pick-family instruction for `action` on
	/// `commit` (gitui-built; see [`Action`] in
	/// [`crate::rebase::plan`]). `after` — the bytes that
	/// followed the commit argument in the source, comment
	/// included — carries over verbatim.
	#[must_use]
	pub fn replay(
		action: crate::rebase::plan::Action,
		commit: CommitArg,
		after: Box<[u8]>,
	) -> Self {
		use crate::rebase::plan::Action;
		const PICK: &[u8] = b"pick";
		const REWORD: &[u8] = b"reword";
		const EDIT: &[u8] = b"edit";
		const SQUASH: &[u8] = b"squash";
		const FIXUP: &[u8] = b"fixup";
		const DROP: &[u8] = b"drop";
		let word: Box<[u8]> = match action {
			Action::Pick => PICK,
			Action::Reword => REWORD,
			Action::Edit => EDIT,
			Action::Squash => SQUASH,
			Action::Fixup
			| Action::FixupAmend
			| Action::FixupEdit => FIXUP,
			Action::Drop => DROP,
		}
		.to_vec()
		.into_boxed_slice();
		match action {
			Action::Pick => Self::Pick {
				word,
				commit,
				after,
			},
			Action::Reword => Self::Reword {
				word,
				commit,
				after,
			},
			Action::Edit => Self::Edit {
				word,
				commit,
				after,
			},
			Action::Squash => Self::Squash {
				word,
				commit,
				after,
			},
			Action::Fixup => Self::Fixup {
				word,
				message: FixupMessage::Previous,
				commit,
				after,
			},
			Action::FixupAmend => Self::Fixup {
				word,
				message: FixupMessage::ThisCommit,
				commit,
				after,
			},
			Action::FixupEdit => Self::Fixup {
				word,
				message: FixupMessage::EditThisCommit,
				commit,
				after,
			},
			Action::Drop => Self::Drop {
				word,
				commit,
				after,
			},
		}
	}

	/// The trailing `# comment` on this instruction, if any,
	/// derived from the stored `after` span. Display helper.
	#[must_use]
	pub fn comment(&self) -> Option<&[u8]> {
		let after = match self {
			Self::Pick { after, .. }
			| Self::Reword { after, .. }
			| Self::Edit { after, .. }
			| Self::Squash { after, .. }
			| Self::Fixup { after, .. }
			| Self::Drop { after, .. }
			| Self::Break { after, .. }
			| Self::Noop { after, .. } => after,
			Self::Merge { comment, .. } => return comment.as_deref(),
			_ => return None,
		};
		after
			.iter()
			.position(|&b| b == b'#')
			.map(|pos| &after[pos..])
	}

	/// The canonical instruction word, without arguments.
	#[must_use]
	pub const fn canonical_word(&self) -> &'static str {
		match self {
			Self::Pick { .. } => "pick",
			Self::Reword { .. } => "reword",
			Self::Edit { .. } => "edit",
			Self::Squash { .. } => "squash",
			Self::Fixup { .. } => "fixup",
			Self::Drop { .. } => "drop",
			Self::Break { .. } => "break",
			Self::Exec { .. } => "exec",
			Self::Label { .. } => "label",
			Self::Reset { .. } => "reset",
			Self::Merge { .. } => "merge",
			Self::UpdateRef { .. } => "update-ref",
			Self::Noop { .. } => "noop",
		}
	}

	/// The word bytes as spelled in the source.
	#[must_use]
	pub fn word_bytes(&self) -> &[u8] {
		match self {
			Self::Pick { word, .. }
			| Self::Reword { word, .. }
			| Self::Edit { word, .. }
			| Self::Squash { word, .. }
			| Self::Fixup { word, .. }
			| Self::Drop { word, .. }
			| Self::Break { word, .. }
			| Self::Exec { word, .. }
			| Self::Label { word, .. }
			| Self::Reset { word, .. }
			| Self::Merge { word, .. }
			| Self::UpdateRef { word, .. }
			| Self::Noop { word, .. } => word,
		}
	}

	/// The full commit id this instruction replays, when its
	/// argument is full-length hex (see [`CommitArg::full`]).
	#[must_use]
	pub fn commit(&self) -> Option<CommitId> {
		self.commit_arg().and_then(CommitArg::full)
	}

	/// The commit argument as written, for pick-family and
	/// `merge -C` instructions.
	#[must_use]
	pub const fn commit_arg(&self) -> Option<&CommitArg> {
		match self {
			Self::Pick { commit, .. }
			| Self::Reword { commit, .. }
			| Self::Edit { commit, .. }
			| Self::Squash { commit, .. }
			| Self::Fixup { commit, .. }
			| Self::Drop { commit, .. } => Some(commit),
			Self::Merge { message, .. } => match message {
				MergeMessage::FromCommit(arg)
				| MergeMessage::EditFromCommit(arg) => Some(arg),
				MergeMessage::Oneline => None,
			},
			_ => None,
		}
	}

	/// Whether this is one of the pick-family instructions
	/// (pick/reword/edit/squash/fixup/drop).
	#[must_use]
	pub const fn is_pick_family(&self) -> bool {
		matches!(
			self,
			Self::Pick { .. }
				| Self::Reword { .. }
				| Self::Edit { .. }
				| Self::Squash { .. }
				| Self::Fixup { .. }
				| Self::Drop { .. }
		)
	}

	/// Canonical serialization of this instruction as a full
	/// line (without newline), for instructions gitui built or
	/// replaced. Parsed instructions serialize their source
	/// bytes instead (see the module contract).
	#[must_use]
	pub fn to_line_bytes(&self) -> Vec<u8> {
		serialize::instruction_line(self).into_owned()
	}
}

impl TodoLine {
	/// The instruction, if this line is one.
	#[must_use]
	pub const fn instruction(&self) -> Option<&Instruction> {
		match self {
			Self::Instruction(i) => Some(i),
			_ => None,
		}
	}
}

impl Todo {
	/// Parse raw todo bytes. Never fails: anything
	/// unrecognized becomes [`TodoLine::Raw`].
	#[must_use]
	pub fn parse(bytes: &[u8]) -> Self {
		let trailing_newline = bytes.ends_with(b"\n");
		let mut lines = Vec::new();
		let mut rest = bytes;
		while let Some(pos) = memchr(rest, b'\n') {
			lines.push(parse::parse_line(&rest[..pos]));
			rest = &rest[pos + 1..];
		}
		if !rest.is_empty() {
			lines.push(parse::parse_line(rest));
		}
		Self {
			lines,
			trailing_newline,
		}
	}

	/// Build a document from typed lines. Such lines serialize
	/// from their own bytes; a trailing newline is assumed.
	#[must_use]
	pub const fn from_lines(lines: Vec<TodoLine>) -> Self {
		Self {
			lines,
			trailing_newline: true,
		}
	}

	/// Serialize back to bytes. Byte-exact for any parsed
	/// document (see the module contract).
	#[must_use]
	pub fn to_bytes(&self) -> Vec<u8> {
		serialize::serialize_bytes(self)
	}

	/// The lines, in order.
	#[must_use]
	pub fn lines(&self) -> &[TodoLine] {
		&self.lines
	}

	/// Iterate instructions with their line index.
	pub fn instructions(
		&self,
	) -> impl Iterator<Item = (usize, &Instruction)> {
		self.lines.iter().enumerate().filter_map(|(i, l)| {
			l.instruction().map(|instr| (i, instr))
		})
	}

	/// Whether any line is unknown syntax.
	#[must_use]
	pub fn has_raw_lines(&self) -> bool {
		self.lines.iter().any(|l| matches!(l, TodoLine::Raw(_)))
	}
}

/// Index of the first byte equal to `needle`.
fn memchr(haystack: &[u8], needle: u8) -> Option<usize> {
	haystack.iter().position(|&b| b == needle)
}

/// Parsing internals.
mod parse {
	use super::{
		memchr, CommitArg, FixupMessage, Instruction, MergeMessage,
		TodoLine,
	};

	/// Classify one physical line (no newline).
	pub(super) fn parse_line(line: &[u8]) -> TodoLine {
		if line.is_empty() {
			return TodoLine::Blank;
		}
		if line[0] == b'#' {
			return TodoLine::Comment(intern(line));
		}
		instruction(line).map_or_else(
			|| TodoLine::Raw(intern(line)),
			TodoLine::Instruction,
		)
	}

	/// The grammar kind of an instruction word.
	#[derive(Clone, Copy, Debug, PartialEq, Eq)]
	enum Kind {
		Pick,
		Reword,
		Edit,
		Squash,
		Fixup,
		Drop,
		Exec,
		Break,
		Label,
		Reset,
		Merge,
		UpdateRef,
		Noop,
	}

	/// Recognize an instruction word, case-insensitively, in
	/// long and abbreviated form (git's parser does the same).
	fn classify(word: &[u8]) -> Option<Kind> {
		const TABLE: &[(&str, &str, Kind)] = &[
			("pick", "p", Kind::Pick),
			("reword", "r", Kind::Reword),
			("edit", "e", Kind::Edit),
			("squash", "s", Kind::Squash),
			("fixup", "f", Kind::Fixup),
			("drop", "d", Kind::Drop),
			("exec", "x", Kind::Exec),
			("break", "b", Kind::Break),
			("label", "l", Kind::Label),
			("reset", "t", Kind::Reset),
			("merge", "m", Kind::Merge),
			("update-ref", "u", Kind::UpdateRef),
			("noop", "noop", Kind::Noop),
		];
		TABLE.iter().find_map(|&(long, short, kind)| {
			(word.eq_ignore_ascii_case(long.as_bytes())
				|| word.eq_ignore_ascii_case(short.as_bytes()))
			.then_some(kind)
		})
	}

	/// Parse one line as an instruction, if its shape is
	/// canonical (see the module contract). The word ends at
	/// the first space; every kind then interprets the
	/// remainder *including* that space, so nothing is ever
	/// both consumed here and re-added later.
	fn instruction(line: &[u8]) -> Option<Instruction> {
		let (word, rest) = memchr(line, b' ')
			.map_or((line, None), |pos| {
				(&line[..pos], Some(&line[pos..]))
			});
		let kind = classify(word)?;
		match kind {
			Kind::Pick => commit_line(Kind::Pick, word, rest),
			Kind::Reword => commit_line(Kind::Reword, word, rest),
			Kind::Edit => commit_line(Kind::Edit, word, rest),
			Kind::Squash => commit_line(Kind::Squash, word, rest),
			Kind::Drop => commit_line(Kind::Drop, word, rest),
			Kind::Fixup => fixup(word, rest),
			Kind::Exec => exec(word, rest),
			Kind::Break => bare(word, rest, &Bare::Break),
			Kind::Noop => bare(word, rest, &Bare::Noop),
			Kind::Label => {
				argument(rest).map(|label| Instruction::Label {
					word: intern(word),
					label,
				})
			}
			Kind::Reset => {
				argument(rest).map(|label| Instruction::Reset {
					word: intern(word),
					label,
				})
			}
			Kind::UpdateRef => argument(rest).map(|reference| {
				Instruction::UpdateRef {
					word: intern(word),
					reference,
				}
			}),
			Kind::Merge => merge(word, rest),
		}
	}

	/// Strip the separator space that ended the word,
	/// checking it really is one; `None` when the word
	/// stood alone or the caller handed us nonsense.
	fn after_sep(rest: Option<&[u8]>) -> Option<&[u8]> {
		let rest = rest?;
		(rest.first() == Some(&b' ')).then(|| &rest[1..])
	}

	/// The commit argument and `after` span of a pick-family
	/// remainder (separator already stripped).
	fn commit_fields(rest: &[u8]) -> Option<(CommitArg, Box<[u8]>)> {
		let (token, tail) = token_and_after(rest);
		Some((commit_arg(token)?, after(tail)?))
	}

	/// `pick`-family: one commit token, then `after` — the
	/// exact remaining bytes, valid only when they are
	/// tolerated whitespace optionally followed by a
	/// `# comment`.
	fn commit_line(
		kind: Kind,
		word: &[u8],
		rest: Option<&[u8]>,
	) -> Option<Instruction> {
		let (commit, after) = commit_fields(after_sep(rest)?)?;
		let word = intern(word);
		Some(match kind {
			Kind::Pick => Instruction::Pick {
				word,
				commit,
				after,
			},
			Kind::Reword => Instruction::Reword {
				word,
				commit,
				after,
			},
			Kind::Edit => Instruction::Edit {
				word,
				commit,
				after,
			},
			Kind::Squash => Instruction::Squash {
				word,
				commit,
				after,
			},
			_ => Instruction::Drop {
				word,
				commit,
				after,
			},
		})
	}

	/// `fixup [-C <commit> | -c <commit>] <commit>` with the
	/// same after rules as the pick family.
	fn fixup(
		word: &[u8],
		rest: Option<&[u8]>,
	) -> Option<Instruction> {
		let rest = after_sep(rest)?;
		let (message, rest) = match (
			rest.strip_prefix(b"-C ".as_slice()),
			rest.strip_prefix(b"-c ".as_slice()),
		) {
			(Some(tail), _) => (FixupMessage::ThisCommit, tail),
			(_, Some(tail)) => (FixupMessage::EditThisCommit, tail),
			_ => (FixupMessage::Previous, rest),
		};
		let (commit, after) = commit_fields(rest)?;
		Some(Instruction::Fixup {
			word: intern(word),
			message,
			commit,
			after,
		})
	}

	/// `exec <command>`: the command is everything after the
	/// separator space, verbatim (a trailing `\r` included),
	/// and never empty — git rejects empty `exec` and so does
	/// this grammar.
	fn exec(word: &[u8], rest: Option<&[u8]>) -> Option<Instruction> {
		let command = after_sep(rest)?;
		(!command.is_empty()).then(|| Instruction::Exec {
			word: intern(word),
			command: intern(command),
		})
	}

	/// `break` / `noop`: the word alone, with optional
	/// tolerated trailing whitespace (the separator space
	/// included, since these instructions take no argument).
	enum Bare {
		Break,
		Noop,
	}

	fn bare(
		word: &[u8],
		rest: Option<&[u8]>,
		kind: &Bare,
	) -> Option<Instruction> {
		let after = match rest {
			None => b"".as_slice().into(),
			Some(rest) => after(rest)?,
		};
		Some(match kind {
			Bare::Break => Instruction::Break {
				word: intern(word),
				after,
			},
			Bare::Noop => Instruction::Noop {
				word: intern(word),
				after,
			},
		})
	}

	/// `label|reset|update-ref <name>`: the name is
	/// everything after the separator space, non-empty (git
	/// requires an argument).
	fn argument(rest: Option<&[u8]>) -> Option<Box<[u8]>> {
		let arg = after_sep(rest)?;
		(!arg.is_empty()).then(|| intern(arg))
	}

	/// `merge [-C <commit> | -c <commit>] <label>… [# <oneline>]`.
	fn merge(
		word: &[u8],
		rest: Option<&[u8]>,
	) -> Option<Instruction> {
		let rest = after_sep(rest)?;
		let (message, rest) = if let Some(tail) =
			rest.strip_prefix(b"-C ".as_slice())
		{
			let (token, tail) = split_once(tail, b' ');
			(MergeMessage::FromCommit(commit_arg(token)?), tail?)
		} else if let Some(tail) =
			rest.strip_prefix(b"-c ".as_slice())
		{
			let (token, tail) = split_once(tail, b' ');
			(MergeMessage::EditFromCommit(commit_arg(token)?), tail?)
		} else {
			(MergeMessage::Oneline, rest)
		};

		// The comment starts at the first `#` preceded by a
		// space; labels are everything before that space.
		let (labels, comment) =
			match rest.iter().position(|&b| b == b'#') {
				Some(0) => return None,
				Some(pos) if rest[pos - 1] != b' ' => return None,
				Some(pos) => {
					(&rest[..pos - 1], Some(intern(&rest[pos..])))
				}
				None => (rest, None),
			};
		let mut parents = Vec::new();
		for label in labels.split(|&b| b == b' ') {
			// Empty, flag-like, or comment-like tokens are
			// not labels (`-C`/`-c` without their argument
			// must not masquerade as one).
			if label.is_empty()
				|| label[0] == b'#'
				|| label[0] == b'-'
			{
				return None;
			}
			parents.push(intern(label));
		}
		(!parents.is_empty()).then(|| Instruction::Merge {
			word: intern(word),
			message,
			parents,
			comment,
		})
	}

	/// A commit argument token: 4..=64 hex digits. Git accepts
	/// any unambiguous prefix, so abbreviated ids parse as
	/// instructions; anything else is not a commit argument and
	/// the line stays `Raw`.
	fn commit_arg(token: &[u8]) -> Option<CommitArg> {
		let is_arg = (4..=64).contains(&token.len())
			&& token.iter().all(u8::is_ascii_hexdigit);
		is_arg.then(|| CommitArg(intern(token)))
	}

	/// The commit token and the raw bytes following it. A
	/// following space belongs to `after` — with the token
	/// delimited this way the line is reassemblable byte
	/// for byte, whatever the trailing shape.
	fn token_and_after(rest: &[u8]) -> (&[u8], &[u8]) {
		memchr(rest, b' ')
			.map_or((rest, &[]), |pos| (&rest[..pos], &rest[pos..]))
	}

	/// Validate an `after` span: tolerated whitespace
	/// (spaces, CR) optionally followed by a `# comment`.
	/// Stored verbatim on success; anything else keeps the
	/// whole line `Raw`.
	fn after(after: &[u8]) -> Option<Box<[u8]>> {
		let is_tolerated = |b: u8| b == b' ' || b == b'\r';
		let end = after
			.iter()
			.position(|&b| !is_tolerated(b))
			.unwrap_or(after.len());
		let valid = after[end..].is_empty() || after[end] == b'#';
		valid.then(|| intern(after))
	}

	/// Split at the first `needle`; the tail excludes the
	/// needle itself. `None` tail when the needle is absent.
	fn split_once(line: &[u8], needle: u8) -> (&[u8], Option<&[u8]>) {
		memchr(line, needle).map_or((line, None), |pos| {
			(&line[..pos], Some(&line[pos + 1..]))
		})
	}

	fn intern(bytes: &[u8]) -> Box<[u8]> {
		bytes.to_vec().into_boxed_slice()
	}
}

/// Serialization internals.
mod serialize {
	use super::{
		FixupMessage, Instruction, MergeMessage, Todo, TodoLine,
	};

	/// Serialize the whole document: lines joined with `\n`,
	/// plus a trailing `\n` only when the source had one.
	pub(super) fn serialize_bytes(todo: &Todo) -> Vec<u8> {
		let mut out = Vec::new();
		for (index, line) in todo.lines.iter().enumerate() {
			if index > 0 {
				out.push(b'\n');
			}
			extend_line(&mut out, line);
		}
		if todo.trailing_newline && !todo.lines.is_empty() {
			out.push(b'\n');
		}
		out
	}

	/// The exact bytes of one line: instructions re-emit
	/// their stored source parts.
	fn extend_line(out: &mut Vec<u8>, line: &TodoLine) {
		match line {
			TodoLine::Instruction(instruction) => {
				extend_instruction(
					out,
					instruction,
					instruction.word_bytes(),
				);
			}
			TodoLine::Comment(bytes) | TodoLine::Raw(bytes) => {
				out.extend_from_slice(bytes);
			}
			TodoLine::Blank => {}
		}
	}

	/// Write one instruction. `word` overrides the stored
	/// spelling (the canonical word for rebuilt lines).
	/// Everything else re-emits stored source bytes.
	fn extend_instruction(
		out: &mut Vec<u8>,
		instruction: &Instruction,
		word: &[u8],
	) {
		match instruction {
			Instruction::Pick { commit, after, .. }
			| Instruction::Reword { commit, after, .. }
			| Instruction::Edit { commit, after, .. }
			| Instruction::Squash { commit, after, .. }
			| Instruction::Drop { commit, after, .. } => {
				out.extend_from_slice(word);
				out.push(b' ');
				out.extend_from_slice(commit.as_bytes());
				out.extend_from_slice(after);
			}
			Instruction::Fixup {
				message,
				commit,
				after,
				..
			} => {
				out.extend_from_slice(word);
				match message {
					FixupMessage::Previous => {}
					FixupMessage::ThisCommit => {
						out.extend_from_slice(b" -C");
					}
					FixupMessage::EditThisCommit => {
						out.extend_from_slice(b" -c");
					}
				}
				out.push(b' ');
				out.extend_from_slice(commit.as_bytes());
				out.extend_from_slice(after);
			}
			Instruction::Break { after, .. }
			| Instruction::Noop { after, .. } => {
				out.extend_from_slice(word);
				out.extend_from_slice(after);
			}
			Instruction::Exec { command, .. } => {
				out.extend_from_slice(word);
				out.push(b' ');
				out.extend_from_slice(command);
			}
			Instruction::Label { label, .. }
			| Instruction::Reset { label, .. } => {
				out.extend_from_slice(word);
				out.push(b' ');
				out.extend_from_slice(label);
			}
			Instruction::Merge {
				message,
				parents,
				comment,
				..
			} => {
				out.extend_from_slice(word);
				match message {
					MergeMessage::FromCommit(arg) => {
						out.extend_from_slice(b" -C ");
						out.extend_from_slice(arg.as_bytes());
					}
					MergeMessage::EditFromCommit(arg) => {
						out.extend_from_slice(b" -c ");
						out.extend_from_slice(arg.as_bytes());
					}
					MergeMessage::Oneline => {}
				}
				for parent in parents {
					out.push(b' ');
					out.extend_from_slice(parent);
				}
				push_comment(out, comment.as_deref());
			}
			Instruction::UpdateRef { reference, .. } => {
				out.extend_from_slice(word);
				out.push(b' ');
				out.extend_from_slice(reference);
			}
		}
	}

	/// Append ` <comment>` when one exists; stored comment
	/// bytes include the leading `#`.
	fn push_comment(out: &mut Vec<u8>, comment: Option<&[u8]>) {
		if let Some(comment) = comment {
			out.push(b' ');
			out.extend_from_slice(comment);
		}
	}

	/// The canonical line for an instruction gitui built or
	/// replaced: the long word, everything else as stored.
	pub(super) fn instruction_line(
		instruction: &Instruction,
	) -> std::borrow::Cow<'static, [u8]> {
		let mut out = Vec::new();
		extend_instruction(
			&mut out,
			instruction,
			instruction.canonical_word().as_bytes(),
		);
		out.into()
	}
}

#[cfg(test)]
mod tests {
	#![allow(clippy::unwrap_used)]

	use super::*;

	const OID_A: &str = "0123456789abcdef0123456789abcdef01234567";
	const OID_B: &str = "89abcdef0123456789abcdef0123456789abcdef";

	fn oid(hex: &str) -> CommitId {
		CommitId::new(git2::Oid::from_str(hex).unwrap())
	}

	fn arg(hex: &str) -> CommitArg {
		CommitArg(hex.as_bytes().to_vec().into_boxed_slice())
	}

	fn boxed(bytes: &[u8]) -> Box<[u8]> {
		bytes.to_vec().into_boxed_slice()
	}

	/// The instruction a single line parses into; panics on
	/// `Raw` lines.
	fn instruction(bytes: &[u8]) -> Instruction {
		match parse::parse_line(bytes) {
			TodoLine::Instruction(instr) => instr,
			other => {
				panic!("not an instruction: {other:?} in {bytes:?}")
			}
		}
	}

	/// Whether a single line is unclassifiable (`Raw`).
	fn is_raw(bytes: &[u8]) -> bool {
		matches!(parse::parse_line(bytes), TodoLine::Raw(_))
	}

	/// Round-trip one document, and check that re-parsing
	/// the serialization is a fixed point.
	fn round_trip(bytes: &[u8]) {
		let parsed = Todo::parse(bytes);
		assert_eq!(
			parsed.to_bytes(),
			bytes,
			"round trip broke for {:?}",
			String::from_utf8_lossy(bytes)
		);
		let again = Todo::parse(&parsed.to_bytes());
		assert_eq!(again.lines(), parsed.lines());
	}

	fn pick_instr(word: &str, hex: &str) -> Instruction {
		Instruction::Pick {
			word: boxed(word.as_bytes()),
			commit: arg(hex),
			after: b"".as_slice().into(),
		}
	}

	#[test]
	fn parses_pick_family_with_spelling_and_abbreviations() {
		assert_eq!(
			instruction(format!("pick {OID_A} # subject").as_bytes()),
			Instruction::Pick {
				word: boxed(b"pick"),
				commit: arg(OID_A),
				after: boxed(b" # subject"),
			}
		);
		assert_eq!(
			instruction(b"P a1b2c3d4"),
			pick_instr("P", "a1b2c3d4")
		);
		assert_eq!(
			instruction(b"p a1b2c3d4 # x"),
			Instruction::Pick {
				word: boxed(b"p"),
				commit: arg("a1b2c3d4"),
				after: boxed(b" # x"),
			}
		);
		assert_eq!(
			instruction(format!("pick {OID_A} \r").as_bytes()),
			Instruction::Pick {
				word: boxed(b"pick"),
				commit: arg(OID_A),
				after: boxed(b" \r"),
			}
		);
		for word in
			["reword", "r", "edit", "e", "squash", "s", "drop", "d"]
		{
			let parsed =
				instruction(format!("{word} {OID_A}").as_bytes());
			assert!(parsed.is_pick_family(), "word {word}");
			assert_eq!(
				parsed.commit_arg(),
				Some(&arg(OID_A)),
				"word {word}"
			);
		}
	}

	#[test]
	fn parses_fixup_variants() {
		assert_eq!(
			instruction(format!("fixup {OID_A}").as_bytes()),
			Instruction::Fixup {
				word: boxed(b"fixup"),
				message: FixupMessage::Previous,
				commit: arg(OID_A),
				after: b"".as_slice().into(),
			}
		);
		assert_eq!(
			instruction(format!("f -C {OID_A} # m").as_bytes()),
			Instruction::Fixup {
				word: boxed(b"f"),
				message: FixupMessage::ThisCommit,
				commit: arg(OID_A),
				after: boxed(b" # m"),
			}
		);
		assert_eq!(
			instruction(format!("fixup -c {OID_B}").as_bytes()),
			Instruction::Fixup {
				word: boxed(b"fixup"),
				message: FixupMessage::EditThisCommit,
				commit: arg(OID_B),
				after: b"".as_slice().into(),
			}
		);
		assert!(is_raw(b"fixup -C nope"));
		assert!(is_raw(b"pick abc"));
		assert!(is_raw(b"pick ab"));
	}

	#[test]
	fn parses_exec_break_label_reset_merge_updateref() {
		assert_eq!(
			instruction(b"exec echo hi # not a comment"),
			Instruction::Exec {
				word: boxed(b"exec"),
				command: boxed(b"echo hi # not a comment"),
			}
		);
		assert_eq!(
			instruction(b"exec echo \xff\xfe"),
			Instruction::Exec {
				word: boxed(b"exec"),
				command: boxed(b"echo \xff\xfe"),
			}
		);
		assert_eq!(
			instruction(b"exec true\r"),
			Instruction::Exec {
				word: boxed(b"exec"),
				command: boxed(b"true\r"),
			}
		);
		assert!(is_raw(b"exec"));
		assert!(is_raw(b"exec "));
		assert_eq!(
			instruction(b"break"),
			Instruction::Break {
				word: boxed(b"break"),
				after: b"".as_slice().into(),
			}
		);
		assert_eq!(
			instruction(b"break \r"),
			Instruction::Break {
				word: boxed(b"break"),
				after: boxed(b" \r"),
			}
		);
		assert_eq!(
			instruction(b"label my-label"),
			Instruction::Label {
				word: boxed(b"label"),
				label: boxed(b"my-label"),
			}
		);
		assert_eq!(
			instruction(b"t my-label"),
			Instruction::Reset {
				word: boxed(b"t"),
				label: boxed(b"my-label"),
			}
		);
		assert_eq!(
			instruction(
				format!("merge -C {OID_A} a b # one").as_bytes()
			),
			Instruction::Merge {
				word: boxed(b"merge"),
				message: MergeMessage::FromCommit(arg(OID_A)),
				parents: vec![boxed(b"a"), boxed(b"b")],
				comment: Some(boxed(b"# one")),
			}
		);
		assert_eq!(
			instruction(format!("m -c {OID_A} a").as_bytes()),
			Instruction::Merge {
				word: boxed(b"m"),
				message: MergeMessage::EditFromCommit(arg(OID_A)),
				parents: vec![boxed(b"a")],
				comment: None,
			}
		);
		assert_eq!(
			instruction(b"m a # one"),
			Instruction::Merge {
				word: boxed(b"m"),
				message: MergeMessage::Oneline,
				parents: vec![boxed(b"a")],
				comment: Some(boxed(b"# one")),
			}
		);
		assert_eq!(
			instruction(b"u refs/heads/x"),
			Instruction::UpdateRef {
				word: boxed(b"u"),
				reference: boxed(b"refs/heads/x"),
			}
		);
		assert_eq!(
			instruction(b"noop"),
			Instruction::Noop {
				word: boxed(b"noop"),
				after: b"".as_slice().into(),
			}
		);
	}

	#[test]
	fn commit_arg_full_and_names() {
		assert_eq!(arg(OID_A).full(), Some(oid(OID_A)));
		assert_eq!(arg("a1b2c3d4").full(), None);
		assert_eq!(
			CommitArg::from_id(oid(OID_A)).full(),
			Some(oid(OID_A))
		);
		let id = oid(OID_A);
		assert!(arg("0123").names(&id));
		assert!(arg(OID_A).names(&id));
		assert!(!arg("a1b2").names(&id));
		assert!(!arg(OID_B).names(&id));
		assert!(!arg(OID_A).names(&oid(OID_B)));
		// A "prefix" longer than the id names nothing.
		let longer = format!("{OID_A}ff");
		assert!(!arg(&longer).names(&id));
	}

	#[test]
	fn non_canonical_lines_stay_raw() {
		for line in [
			b"pick  double".as_slice(),
			b"pick\ttab".as_slice(),
			b"frobnicate x".as_slice(),
			b"break now".as_slice(),
			b"noop now".as_slice(),
			b"pick".as_slice(),
			b"pick ".as_slice(),
			b"label".as_slice(),
			b"merge".as_slice(),
			b"merge -C".as_slice(),
			b"merge -C nope a".as_slice(),
			b"pick 0123z".as_slice(),
			b"merge a# one".as_slice(),
			b"merge # only comment".as_slice(),
		] {
			assert!(is_raw(line), "expected raw: {line:?}");
		}
	}

	#[test]
	fn comments_blanks_and_raw_classify() {
		assert!(matches!(
			parse::parse_line(b"# note"),
			TodoLine::Comment(_)
		));
		assert_eq!(parse::parse_line(b""), TodoLine::Blank);
		assert!(matches!(
			parse::parse_line(b"   "),
			TodoLine::Raw(_)
		));
	}

	#[test]
	fn canonical_rebuild_forms() {
		assert_eq!(
			pick_instr("pick", OID_A).to_line_bytes(),
			format!("pick {OID_A}").into_bytes()
		);
		assert_eq!(
			Instruction::Pick {
				word: boxed(b"p"),
				commit: CommitArg::from_id(oid(OID_A)),
				after: boxed(b" \r"),
			}
			.to_line_bytes(),
			// Canonical rebuild: the word normalizes, the
			// after span carries over verbatim.
			format!("pick {OID_A} \r").into_bytes()
		);
		assert_eq!(
			Instruction::Fixup {
				word: boxed(b"f"),
				message: FixupMessage::ThisCommit,
				commit: CommitArg::from_id(oid(OID_A)),
				after: b"".as_slice().into(),
			}
			.to_line_bytes(),
			format!("fixup -C {OID_A}").into_bytes()
		);
		assert_eq!(
			Instruction::break_instruction().to_line_bytes(),
			b"break".to_vec()
		);
		assert_eq!(
			Instruction::Exec {
				word: boxed(b"x"),
				command: boxed(b"make test"),
			}
			.to_line_bytes(),
			b"exec make test".to_vec()
		);
		assert_eq!(
			Instruction::Merge {
				word: boxed(b"m"),
				message: MergeMessage::Oneline,
				parents: vec![boxed(b"a"), boxed(b"b")],
				comment: Some(boxed(b"# one")),
			}
			.to_line_bytes(),
			b"merge a b # one".to_vec()
		);
	}

	#[test]
	fn instruction_metadata_helpers() {
		let pick = pick_instr("pick", OID_A);
		assert_eq!(pick.word_bytes(), b"pick");
		assert!(pick.is_pick_family());
		assert_eq!(pick.commit(), Some(oid(OID_A)));

		let short = pick_instr("p", "a1b2c3d4");
		assert!(short.is_pick_family());
		assert_eq!(short.commit(), None);

		let fixup = Instruction::Fixup {
			word: boxed(b"fixup"),
			message: FixupMessage::EditThisCommit,
			commit: arg(OID_A),
			after: b"".as_slice().into(),
		};
		assert_eq!(fixup.canonical_word(), "fixup");
		assert!(fixup.is_pick_family());

		let merge = Instruction::Merge {
			word: boxed(b"m"),
			message: MergeMessage::EditFromCommit(arg(OID_A)),
			parents: Vec::new(),
			comment: None,
		};
		assert!(!merge.is_pick_family());
		assert_eq!(merge.commit_arg(), Some(&arg(OID_A)));
		assert_eq!(
			Instruction::Break {
				word: boxed(b"break"),
				after: b"".as_slice().into(),
			}
			.commit(),
			None
		);
	}

	#[test]
	fn round_trip_documents() {
		round_trip(b"");
		round_trip(b"\n");
		round_trip(b"\n\n");
		round_trip(b"pick ");
		round_trip(b"# only a comment");
		round_trip(b"# c\n\npick a1b2c3d4\r\n# tail");
	}

	/// Hostile bytes: NULs, lone CRs, invalid UTF-8 in every
	/// field position, 1-byte files, only newlines, and
	/// 100k-byte single lines. The parser must stay total,
	/// byte-exact, and fast (the loop below is the suite-sized
	/// distillation of a 10^5-iteration audit fuzz that found
	/// nothing).
	#[test]
	fn hostile_bytes_round_trip() {
		let cases: Vec<Vec<u8>> = vec![
			b"x".to_vec(),
			vec![0],
			vec![b'\r'],
			b"\r\n".to_vec(),
			b"\n\n\n".to_vec(),
			b"p \0\0\0".to_vec(),
			b"pick \0".to_vec(),
			b"merge -C \0 a".to_vec(),
			b"merge a \0 b".to_vec(),
			b"m -C \xff\xfe lbl".to_vec(),
			b"l \0".to_vec(),
			b"t label\x0b".to_vec(),
			b"u refs/heads/\xff".to_vec(),
			b"break\r\n".to_vec(),
			b"noop\t".to_vec(),
			b"b b b b".to_vec(),
			b"p \xff\xfe\xfd # \xc3\x28".to_vec(),
			b"exec \0 echo \xff\xfe hi".to_vec(),
			b"fixup -c -C 0123456789abcdef0123456789abcdef01234567"
				.to_vec(),
			{
				let mut huge = b"exec ".to_vec();
				huge.extend(std::iter::repeat_n(b'A', 100_000));
				huge
			},
			{
				let mut huge = b"pick ".to_vec();
				huge.extend(std::iter::repeat_n(b'f', 100_000));
				huge
			},
			vec![b'\n'; 10_000],
		];
		for case in &cases {
			round_trip(case);
		}
	}

	/// A grammar-aware generator: documents built from the
	/// real grammar with adversarial field contents. Seeded,
	/// deterministic, reproducible.
	struct Generator(u64);

	impl Generator {
		fn next(&mut self) -> u64 {
			self.0 = self
				.0
				.wrapping_mul(6_364_136_223_846_793_005)
				.wrapping_add(1_442_695_040_888_963_407);
			self.0 >> 11
		}

		fn below(&mut self, bound: usize) -> usize {
			(self.next() % bound.max(1) as u64) as usize
		}

		fn pick(&mut self) -> bool {
			self.next() % 2 == 0
		}

		fn hex_arg(&mut self) -> String {
			const HEX: &[u8] = b"0123456789abcdefABCDEF";
			let len = 4 + self.below(61);
			(0..len)
				.map(|_| HEX[self.below(HEX.len())] as char)
				.collect()
		}

		fn garbage_arg(&mut self) -> String {
			const POOL: &[u8] = b"xyzXYZ_-. ~#\t\r\"'\\";
			let len = self.below(12);
			(0..len)
				.map(|_| POOL[self.below(POOL.len())] as char)
				.collect()
		}

		fn tail(&mut self) -> &'static str {
			match self.below(5) {
				0 => "",
				1 => " ",
				2 => "  ",
				3 => " \r",
				_ => "\r",
			}
		}

		fn line(&mut self) -> Vec<u8> {
			const WORDS: [&str; 26] = [
				"pick",
				"p",
				"PICK",
				"reword",
				"r",
				"edit",
				"e",
				"squash",
				"s",
				"fixup",
				"f",
				"drop",
				"d",
				"exec",
				"x",
				"break",
				"b",
				"label",
				"l",
				"reset",
				"t",
				"merge",
				"m",
				"update-ref",
				"u",
				"noop",
			];
			let word = WORDS[self.below(WORDS.len())];
			let arg = if self.pick() {
				self.hex_arg()
			} else {
				self.garbage_arg()
			};
			let tail = self.tail();
			let comment = if self.pick() {
				format!(" # {}", self.garbage_arg())
			} else {
				String::new()
			};
			let flag = match self.below(3) {
				0 => "",
				1 => " -C ",
				_ => " -c ",
			};
			let (sep, sep2) = if self.below(10) == 0 {
				("\t", "\t")
			} else {
				(" ", " ")
			};
			match self.below(8) {
				0 => {
					format!("{word}{sep}{arg}{tail}{comment}")
				}
				1 => format!("{word}{flag}{arg}{tail}{comment}"),
				2 => format!("{word}{sep}{arg}"),
				3 => format!("exec {arg}"),
				4 => format!("{word}{tail}"),
				5 => format!("{word}{sep}{arg} {arg}"),
				6 => format!("merge{flag}{arg} lbl{tail}{comment}"),
				_ => format!("{word}{sep}{arg}{sep2}{comment}"),
			}
			.into_bytes()
		}

		fn document(&mut self, lines: usize) -> Vec<u8> {
			let mut out = Vec::new();
			for i in 0..lines {
				if i > 0 {
					out.push(b'\n');
				}
				out.extend_from_slice(&self.line());
			}
			if self.pick() {
				out.push(b'\n');
			}
			out
		}
	}

	#[test]
	fn generated_documents_round_trip() {
		let mut gen = Generator(0x5EED_C0FF_EE00_0001);
		for _ in 0..2000 {
			let lines = gen.below(40);
			let doc = gen.document(lines);
			round_trip(&doc);
		}
	}

	#[test]
	fn generated_documents_parse_idempotently() {
		let mut gen = Generator(0xD1CE_B0A0_0000_0042);
		for _ in 0..2000 {
			let lines = gen.below(80);
			let doc = gen.document(lines);
			let parsed = Todo::parse(&doc);
			assert_eq!(parsed.to_bytes(), doc);
			let again = Todo::parse(&parsed.to_bytes());
			assert_eq!(again.lines(), parsed.lines());
		}
	}

	/// Byte-level mutation fuzz on top of structured inputs:
	/// mutations of grammar-shaped documents, not pure noise.
	#[test]
	fn mutated_documents_round_trip() {
		let mut gen = Generator(0xFEED_FACE_0000_0003);
		let mut rng = Generator(0x0BAD_C0DE_0000_0009);
		for _ in 0..3000 {
			let lines = gen.below(20);
			let mut doc = gen.document(lines);
			for _ in 0..(1 + rng.below(5)) {
				match rng.below(4) {
					0 if !doc.is_empty() => {
						let pos = rng.below(doc.len());
						doc[pos] = (rng.next() & 0xff) as u8;
					}
					1 => {
						let pos = rng.below(doc.len() + 1);
						doc.insert(pos, (rng.next() & 0xff) as u8);
					}
					2 if !doc.is_empty() => {
						let pos = rng.below(doc.len());
						doc.remove(pos);
					}
					_ => {
						let pos = rng.below(doc.len() + 1);
						doc.truncate(pos);
					}
				}
			}
			round_trip(&doc);
		}
	}
}
