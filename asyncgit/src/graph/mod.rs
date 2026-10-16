pub mod buffer;
mod cell;
pub mod chunk;
pub mod oids;
pub mod walker;

#[cfg(test)]
mod harness;

pub use walker::GraphWalker;

/// The maximum number of colors to use for graph lanes
pub const MAX_LANE_COLORS: usize = 16;

/// A lane's column within a row, or its slot in the color palette.
///
/// Repositories that open more lanes than this can hold do exist, and
/// no terminal can show that many columns usefully, so the conversion
/// from a column index saturates rather than wrapping. It is sized so
/// that the clamp is out of reach of any history a person scrolls
/// through, because past the clamp every lane reports the same column:
/// a width that is merely too large is cosmetic, while a column index
/// that is wrong misplaces an edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LaneIndex(u16);

/// Numeric alias assigned to each commit in the graph.
///
/// The alias is a dense integer index created by [`GraphOids`](super::oids::GraphOids)
/// that avoids storing full [`CommitId`](crate::sync::CommitId)s inside the lane state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CommitAlias(usize);

/// An alias for a commit that had not yet been walked when the value
/// was minted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnwalkedAlias(CommitAlias);

impl std::ops::Deref for UnwalkedAlias {
	type Target = CommitAlias;
	fn deref(&self) -> &CommitAlias {
		&self.0
	}
}

impl std::ops::Deref for CommitAlias {
	type Target = usize;
	fn deref(&self) -> &usize {
		&self.0
	}
}

impl From<usize> for CommitAlias {
	fn from(v: usize) -> Self {
		Self(v)
	}
}

impl From<usize> for LaneIndex {
	fn from(lane: usize) -> Self {
		Self(u16::try_from(lane).unwrap_or(u16::MAX))
	}
}

impl From<LaneIndex> for usize {
	fn from(lane: LaneIndex) -> Self {
		lane.0 as Self
	}
}

/// The type of connection between nodes in the graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionType {
	Vertical,
	VerticalDotted,
	CommitNormal,
	CommitBranch,
	CommitMerge,
	CommitStash,
	CommitUncommitted,
	/// a bridge turning down into a lane that starts here
	MergeBridgeStart,
	/// a bridge passing over an empty lane slot
	MergeBridgeMid,
	/// a bridge turning down, lane starting to its right
	MergeBridgeEnd,
	/// a lane from above turning left into a commit
	BranchUp,
	///  a lane from above turning right into a commit
	BranchUpRight,
	/// a continuing lane absorbing a bridge from its left
	TeeLeft,
	/// a continuing lane absorbing a bridge from its right
	TeeRight,
	/// a lane ending from above while a bridge passes through
	TeeUp,
	/// a lane starting downward while a bridge passes through
	TeeDown,
	/// a lane a bridge joins at a cell another bridge runs through
	Cross,
}

impl ConnectionType {
	/// Whether this connection is a dotted vertical line.
	pub const fn is_dotted(self) -> bool {
		matches!(self, Self::VerticalDotted)
	}

	/// Whether this is a commit marker rather than a line.
	///
	/// A marker carries no line of its own and is never drawn over:
	/// whether a line reaches it from above, or leaves it below, is a
	/// fact about the commit's lane and not about the glyph.
	pub const fn is_commit(self) -> bool {
		matches!(
			self,
			Self::CommitNormal
				| Self::CommitBranch
				| Self::CommitMerge
				| Self::CommitStash
				| Self::CommitUncommitted
		)
	}
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphRow {
	/// How many lane columns this row spans, counting any column it
	/// leaves blank between two lanes.
	///
	/// This is the width the row needs, which is what the log view
	/// sizes its graph column from. It is deliberately not a count of
	/// live lanes: a row whose middle lanes have closed still has to
	/// reserve the columns to the right of the gap, and sizing from a
	/// count would hide them behind the truncation marker.
	pub lane_count: LaneIndex,

	/// Which lane index this commit sits on
	pub commit_lane: LaneIndex,

	/// Whether this is a merge commit (two parents)
	pub is_merge: bool,

	/// Whether this commit is a branch tip
	pub is_branch_tip: bool,

	/// Whether this commit has stash marker
	pub is_stash: bool,

	/// Connections emitted per lane:
	/// None = empty space
	/// Some((ConnectionType, `color_index`)) = draw this connector in this color
	pub lanes: Vec<Option<(ConnectionType, LaneIndex)>>,

	/// Horizontal merge bridge: if this commit merges rightward,
	/// (`from_lane`, `to_lane`) — the span to draw ─ ╭ ╮ across
	pub merge_bridge: Option<(LaneIndex, LaneIndex)>,

	/// Horizontal branch bridges: if this commit spawns branches,
	/// spans to draw ─ ╭ ╮ across
	pub branches: Vec<(LaneIndex, LaneIndex)>,
}

/// A glyph and its lane color, or an empty terminal cell.
pub type GraphCell = Option<(ConnectionType, LaneIndex)>;
