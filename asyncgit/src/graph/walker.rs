use super::buffer::Buffer;
use super::chunk::LaneSlot;
use super::oids::GraphOids;
use super::{
	CommitAlias, ConnectionType, GraphRow, LaneIndex, UnwalkedAlias,
	MAX_LANE_COLORS,
};
use crate::sync::CommitId;
use core::cmp::Ordering;
use std::collections::HashSet;

/// Get the lane's color index, which cycles through the color palette.
fn lane_color(lane: usize) -> LaneIndex {
	LaneIndex::from(lane % MAX_LANE_COLORS)
}

use bitflags::bitflags;

bitflags! {
	/// The neighboring cells a lane joins to. Overlapping lines
	/// are merged through this representation rather than
	/// a naive overwrite for readability.
	///
	/// [`Self::LEFT`] and [`Self::RIGHT`] are a horizontal run that
	/// *joins* this cell; [`Self::CROSS`] is one that runs past it
	/// without joining. Keeping the two apart is what lets a bridge be
	/// drawn behind an unrelated lane, while a bridge that really ends
	/// on a lane still gets its junction even when a third line
	/// crosses the same cell.
	#[derive(Clone, Copy, Default)]
	struct Directions: u8 {
		const UP = 0b0_0001;
		const DOWN = 0b0_0010;
		const LEFT = 0b0_0100;
		const RIGHT = 0b0_1000;
		const CROSS = 0b1_0000;
	}
}

impl Directions {
	#[allow(clippy::missing_const_for_fn)]
	fn merge(self, other: Self) -> Self {
		Self::from_bits_retain(self.bits() | other.bits())
	}

	#[allow(clippy::missing_const_for_fn)]
	fn vertical(self) -> bool {
		self.intersects(Self::UP | Self::DOWN)
	}
}

/// Extract direction components from a connection glyph.
/// Returns `None` for commit markers, which are never drawn over.
fn connection_to_directions(
	conn: ConnectionType,
) -> Option<Directions> {
	Some(match conn {
		ConnectionType::Vertical | ConnectionType::VerticalDotted => {
			Directions::UP | Directions::DOWN
		}
		ConnectionType::MergeBridgeMid => {
			Directions::LEFT | Directions::RIGHT
		}
		ConnectionType::MergeBridgeStart => {
			Directions::DOWN | Directions::LEFT
		}
		ConnectionType::MergeBridgeEnd => {
			Directions::DOWN | Directions::RIGHT
		}
		ConnectionType::BranchUp => Directions::UP | Directions::LEFT,
		ConnectionType::BranchUpRight => {
			Directions::UP | Directions::RIGHT
		}
		ConnectionType::TeeLeft => {
			Directions::UP | Directions::DOWN | Directions::LEFT
		}
		ConnectionType::TeeRight => {
			Directions::UP | Directions::DOWN | Directions::RIGHT
		}
		ConnectionType::TeeUp => {
			Directions::UP | Directions::LEFT | Directions::RIGHT
		}
		ConnectionType::TeeDown => {
			Directions::DOWN | Directions::LEFT | Directions::RIGHT
		}
		ConnectionType::Cross => {
			Directions::UP
				| Directions::DOWN
				| Directions::LEFT
				| Directions::RIGHT
		}
		ConnectionType::CommitNormal
		| ConnectionType::CommitBranch
		| ConnectionType::CommitMerge
		| ConnectionType::CommitStash
		| ConnectionType::CommitUncommitted => return None,
	})
}

const fn vertical_connection(dotted: bool) -> ConnectionType {
	if dotted {
		ConnectionType::VerticalDotted
	} else {
		ConnectionType::Vertical
	}
}

/// Synthesize a connection glyph from direction components.
const fn directions_to_connection(
	dirs: Directions,
	dotted: bool,
) -> ConnectionType {
	let up = dirs.contains(Directions::UP);
	let down = dirs.contains(Directions::DOWN);
	let joins_left = dirs.contains(Directions::LEFT);
	let joins_right = dirs.contains(Directions::RIGHT);
	let crossed = dirs.contains(Directions::CROSS);

	// A lane running straight through the cell. A bridge that merely
	// crosses it is drawn behind it, which keeps the lane readable and
	// is the whole reason a crossing is tracked apart from a join. A
	// bridge that ends on the lane needs a junction instead, and when
	// the cell is both joined and crossed there is nothing left to
	// hide the join behind, so it takes the full cross. Drawing that
	// case as a plain vertical, which is all a single horizontal bit
	// could do, loses the merge edge out of the picture entirely.
	if up && down {
		return match (joins_left, joins_right, crossed) {
			(false, false, _) => vertical_connection(dotted),
			(true, false, false) => ConnectionType::TeeLeft,
			(false, true, false) => ConnectionType::TeeRight,
			_ => ConnectionType::Cross,
		};
	}

	// Below here the cell has at most one vertical end, so a crossing
	// and a join on the same side are drawn alike.
	let left = joins_left || crossed;
	let right = joins_right || crossed;

	match (up, down, left, right) {
		(true, _, true, true) => ConnectionType::TeeUp,
		(_, true, true, true) => ConnectionType::TeeDown,
		(true, _, true, false) => ConnectionType::BranchUp,
		(true, _, false, true) => ConnectionType::BranchUpRight,
		(_, true, true, false) => ConnectionType::MergeBridgeStart,
		(_, true, false, true) => ConnectionType::MergeBridgeEnd,
		(false, false, _, _) => ConnectionType::MergeBridgeMid,
		(true | false, _, false, false) => {
			vertical_connection(dotted)
		}
	}
}

/// Draw `add` into a cell, merging with whatever is already there.
/// The line with a vertical component is the chosen way with color
/// ensuring lanes stay visually continuous
fn overlay_cell(
	cell: &mut Option<(ConnectionType, LaneIndex)>,
	add: Directions,
	color: LaneIndex,
) {
	if let Some((conn, existing_color)) = cell {
		// `connection_to_directions` returns `None` for commit markers, which must
		// never be drawn over; when that happens this branch is
		// skipped and `cell` is left untouched.
		if let Some(existing) = connection_to_directions(*conn) {
			let is_dotted = conn.is_dotted();

			let resolved_color =
				if existing.vertical() || !add.vertical() {
					*existing_color
				} else {
					color
				};

			*cell = Some((
				directions_to_connection(
					existing.merge(add),
					is_dotted,
				),
				resolved_color,
			));
		}
	} else {
		*cell = Some((directions_to_connection(add, false), color));
	}
}

/// What the walk learned about one commit, indexed by its
/// [`CommitAlias`]. Aliases are dense `0..n`, so the facts live in a
/// flat vector instead of a map.
#[derive(Clone, Copy, Debug, Default)]
struct CommitFacts {
	/// Whether the commit has already been folded into the buffer.
	///
	/// [`GraphWalker::mint_drawable_parent`] refuses to mint an
	/// [`UnwalkedAlias`] for one of these: a lane waiting on a commit
	/// the walk has already passed would stay open to the end of the
	/// log, drawing a line that never reaches anything.
	processed: bool,

	/// Whether Git reports more than one parent for this commit, which
	/// is what makes it draw a merge marker.
	///
	/// A merge can legitimately end up with a single drawn edge — an
	/// octopus merge past its second parent, a duplicated parent
	/// entry, or a second parent the walk reached first — and keeping
	/// this apart from the lanes lets the marker stay honest about
	/// what the commit is in all three cases.
	is_merge: bool,

	/// The second parent that was given a lane of its own, which is
	/// the far end of the row's merge bridge.
	second_parent: Option<CommitAlias>,
}

#[derive(Default)]
pub struct GraphWalker {
	pub buffer: Buffer,
	pub oids: GraphOids,

	/// What the walk knows about each commit, indexed by alias.
	facts: Vec<CommitFacts>,
}

impl GraphWalker {
	pub fn new() -> Self {
		Self::default()
	}

	fn facts(&self, alias: CommitAlias) -> CommitFacts {
		self.facts.get(*alias).copied().unwrap_or_default()
	}

	fn facts_mut(&mut self, alias: CommitAlias) -> &mut CommitFacts {
		let index = *alias;
		if self.facts.len() <= index {
			self.facts.resize(index + 1, CommitFacts::default());
		}
		&mut self.facts[index]
	}

	/// Mint the drawable alias for a parent commit, or `None` if the
	/// walk already passed it.
	fn mint_drawable_parent(
		&mut self,
		id: &CommitId,
	) -> Option<UnwalkedAlias> {
		let alias = self.oids.get_or_insert(id);
		(!self.facts(alias).processed).then_some(UnwalkedAlias(alias))
	}

	/// The parents this commit draws an edge to: the first two
	/// distinct parents the walk has not already passed.
	///
	/// Every parent left out is one that no lane will wait for, and
	/// that is the point. Git lets a commit name the same parent twice
	/// and lets an octopus merge name more than two, and a walk
	/// ordered by commit time can reach a parent before its child. A
	/// lane opened for any of those could never be closed by a later
	/// row, so it would hang off the bottom of the log as a line that
	/// goes nowhere.
	fn drawable_parents(
		&mut self,
		parents: &[CommitId],
	) -> [Option<UnwalkedAlias>; 2] {
		let mut drawn: [Option<UnwalkedAlias>; 2] = [None, None];
		let mut found = 0;

		for parent_id in parents {
			if found == drawn.len() {
				break;
			}

			let Some(parent) = self.mint_drawable_parent(parent_id)
			else {
				continue;
			};

			let repeated = drawn[..found]
				.iter()
				.flatten()
				.any(|seen| **seen == *parent);
			if repeated {
				continue;
			}

			drawn[found] = Some(parent);
			found += 1;
		}

		drawn
	}

	pub fn process(
		&mut self,
		commit_id: CommitId,
		parents: &[CommitId],
	) {
		let commit_alias = self.oids.get_or_insert(&commit_id);

		// Mark the commit walked before resolving its parents, so that
		// a parent entry naming the commit itself is turned down like
		// any other already-walked parent instead of opening a lane
		// that waits for the commit which just closed it.
		self.facts_mut(commit_alias).processed = true;

		let [first_parent, second_parent] =
			self.drawable_parents(parents);

		let chunk = match (first_parent, second_parent) {
			(Some(parent), Some(second)) => LaneSlot::FlowingMerge {
				alias: commit_alias,
				parent,
				second,
			},
			(Some(parent), None) => LaneSlot::Flowing {
				alias: commit_alias,
				parent,
			},
			// `drawable_parents` fills its slots left to right, so
			// there is no second parent without a first.
			(None, _) => LaneSlot::Settled {
				alias: commit_alias,
			},
		};

		let second = second_parent.map(|second| *second);
		let facts = self.facts_mut(commit_alias);
		facts.is_merge = parents.len() > 1;
		facts.second_parent = second;

		if let Some(second) = second {
			if !self.has_lane_to_parent(second) {
				self.buffer.track_merge_commit(commit_alias);
			}
		}

		self.buffer.update(&chunk);
	}

	/// Number of commits already folded into the graph buffer.
	pub const fn processed_commit_count(&self) -> usize {
		self.buffer.deltas.len()
	}

	pub fn compute_rows(
		&self,
		commit_range: &[CommitId],
		global_start_index: usize,
		branch_tips: &HashSet<CommitId>,
		stashes: &HashSet<CommitId>,
		head_id: Option<&CommitId>,
	) -> Vec<GraphRow> {
		if commit_range.is_empty() {
			return Vec::new();
		}

		// Decompress one row before the range to establish predecessor
		// state. `snapshot_start_index` is usually `global_start_index
		// - 1`, which also makes `snapshot_end_index` equal to
		// `snapshot_start_index + commit_range.len()`. But at the very
		// start of the walk `saturating_sub` clamps to `0` instead of
		// going negative, since there is no predecessor row to fetch
		// there; `index_offset` below (`0` in that case, `1`
		// otherwise) accounts for the difference between the two
		// cases when indexing into `snapshots`.
		let snapshot_start_index =
			global_start_index.saturating_sub(1);
		let snapshot_end_index =
			global_start_index + commit_range.len() - 1;
		let snapshots = self
			.buffer
			.decompress(snapshot_start_index, snapshot_end_index);
		let index_offset = global_start_index - snapshot_start_index;

		commit_range
			.iter()
			.enumerate()
			.map(|(range_index, commit_id)| {
				let snapshot_index = range_index + index_offset;

				let current_snapshot = snapshots
					.get(snapshot_index)
					.map(Vec::as_slice)
					.unwrap_or_default();

				let previous_snapshot = snapshot_index
					.checked_sub(1)
					.and_then(|index| snapshots.get(index))
					.map(Vec::as_slice);

				self.render_row(
					commit_id,
					current_snapshot,
					previous_snapshot,
					branch_tips,
					stashes,
					head_id,
				)
			})
			.collect()
	}

	fn draw_merge_bridge(
		lanes: &mut [Option<(ConnectionType, LaneIndex)>],
		merge_bridge: Option<(usize, usize)>,
		commit_lane: usize,
		current_snapshot: &[Option<LaneSlot>],
		previous_snapshot: Option<&[Option<LaneSlot>]>,
	) {
		let Some((source_lane, target_lane)) = merge_bridge else {
			return;
		};
		// `drawable_parents` keeps the two parents distinct, so the
		// search in `calculate_merge_bridge` cannot land back on the
		// merge's own lane. The guard stays because the alternative
		// is overwriting the commit marker with a zero-length span.
		if source_lane == target_lane {
			return;
		}

		let destination_lane = if source_lane == commit_lane {
			target_lane
		} else {
			source_lane
		};
		let connection_color = lane_color(destination_lane);

		let continues_upwards = Self::lane_continues_upwards(
			destination_lane,
			current_snapshot,
			previous_snapshot,
		);

		let target_directions = Self::calculate_merge_directions(
			commit_lane,
			destination_lane,
			continues_upwards,
		);

		// Replace the plain vertical fill with the precise corner/junction
		lanes[destination_lane] = None;
		overlay_cell(
			&mut lanes[destination_lane],
			target_directions,
			connection_color,
		);

		Self::draw_bridge_span(
			lanes,
			source_lane,
			target_lane,
			connection_color,
		);
	}

	fn draw_branching_lanes(
		lanes: &mut Vec<Option<(ConnectionType, LaneIndex)>>,
		branching_lanes: &[usize],
		commit_lane: usize,
	) -> Vec<(LaneIndex, LaneIndex)> {
		branching_lanes
			.iter()
			.map(|&branch_lane| {
				let start_lane =
					std::cmp::min(branch_lane, commit_lane);
				let end_lane =
					std::cmp::max(branch_lane, commit_lane);

				Self::ensure_lane_capacity(lanes, end_lane);

				let connection_color = lane_color(branch_lane);
				Self::draw_bridge_span(
					lanes,
					start_lane,
					end_lane,
					connection_color,
				);

				let branch_directions =
					Self::calculate_branch_directions(
						branch_lane,
						start_lane,
						end_lane,
					);
				overlay_cell(
					&mut lanes[branch_lane],
					branch_directions,
					connection_color,
				);

				(
					LaneIndex::from(start_lane),
					LaneIndex::from(end_lane),
				)
			})
			.collect()
	}

	/// Whether a lane is already waiting for `target_parent`, making a
	/// second lane for it both redundant and wrong.
	///
	/// This has to agree exactly with the two other places that go
	/// looking for a lane by the parent it awaits: the one that places
	/// a commit, and the one that picks the far end of a merge bridge.
	/// Both of them take the *first* such lane. So if this said no
	/// while a lane was in fact waiting, the reservation opened on the
	/// strength of that answer would be left over — the bridge would
	/// be drawn to the lane that was already there, and the reserved
	/// column would draw a line with nothing above it.
	fn has_lane_to_parent(&self, target_parent: CommitAlias) -> bool {
		self.buffer
			.current
			.iter()
			.flatten()
			.any(|slot| slot.awaits() == Some(target_parent))
	}

	/// Whether the lane in `target_lane` is the same lane the row above
	/// had there, which is what lets a bridge end draw a junction into
	/// it instead of a corner.
	fn lane_continues_upwards(
		target_lane: usize,
		current_snapshot: &[Option<LaneSlot>],
		previous_snapshot: Option<&[Option<LaneSlot>]>,
	) -> bool {
		let Some(previous_snapshot) = previous_snapshot else {
			return false;
		};

		let occupant = |snapshot: &[Option<LaneSlot>]| {
			snapshot.get(target_lane).copied().flatten()
		};

		let current = occupant(current_snapshot);
		current.is_some() && occupant(previous_snapshot) == current
	}

	/// Uses `Ordering` to elegantly map spatial relationships to visual bitmasks.
	fn calculate_merge_directions(
		commit_lane: usize,
		target_lane: usize,
		continues_upwards: bool,
	) -> Directions {
		let mut directions = Directions::DOWN;

		if continues_upwards {
			directions |= Directions::UP;
		}

		match target_lane.cmp(&commit_lane) {
			Ordering::Greater => directions |= Directions::LEFT,
			Ordering::Less => directions |= Directions::RIGHT,
			Ordering::Equal => {}
		}

		directions
	}

	fn calculate_branch_directions(
		branch_lane: usize,
		start_lane: usize,
		end_lane: usize,
	) -> Directions {
		let mut directions = Directions::UP;

		if branch_lane == end_lane {
			directions |= Directions::LEFT;
		}
		if branch_lane == start_lane {
			directions |= Directions::RIGHT;
		}

		directions
	}

	fn ensure_lane_capacity(
		lanes: &mut Vec<Option<(ConnectionType, LaneIndex)>>,
		required_index: usize,
	) {
		if lanes.len() <= required_index {
			lanes.resize(required_index + 1, None);
		}
	}

	pub fn render_row(
		&self,
		commit_id: &CommitId,
		current_snapshot: &[Option<LaneSlot>],
		previous_snapshot: Option<&[Option<LaneSlot>]>,
		branch_tips: &HashSet<CommitId>,
		stashes: &HashSet<CommitId>,
		head_id: Option<&CommitId>,
	) -> GraphRow {
		let commit_alias = self.oids.get(commit_id);
		let head_alias = head_id.and_then(|id| self.oids.get(id));
		let facts = commit_alias
			.map_or_else(CommitFacts::default, |alias| {
				self.facts(alias)
			});

		let commit_lane =
			Self::find_commit_lane(current_snapshot, commit_alias);
		let is_merge = facts.is_merge;
		let is_branch_tip = branch_tips.contains(commit_id);
		let is_stash = stashes.contains(commit_id);

		let branching_lanes = Self::find_branching_lanes(
			current_snapshot,
			previous_snapshot,
		);

		let merge_bridge =
			facts.second_parent.and_then(|parent_alias| {
				commit_lane.and_then(|cl| {
					Self::calculate_merge_bridge(
						current_snapshot,
						cl,
						parent_alias,
					)
				})
			});

		let mut lanes: Vec<Option<(ConnectionType, LaneIndex)>> =
			current_snapshot
				.iter()
				.enumerate()
				.map(|(lane_index, chunk_option)| {
					let chunk = chunk_option.as_ref()?;

					if commit_alias.is_some()
						&& chunk.alias() == commit_alias
					{
						let connection =
							Self::determine_commit_connection(
								is_stash,
								is_merge,
								is_branch_tip,
							);
						return Some((
							connection,
							lane_color(lane_index),
						));
					}

					Self::determine_passthrough_connection(
						chunk, head_alias,
					)
					.map(|connection| {
						(connection, lane_color(lane_index))
					})
				})
				.collect();

		let branches = commit_lane.map_or_else(Vec::new, |cl| {
			Self::draw_merge_bridge(
				&mut lanes,
				merge_bridge,
				cl,
				current_snapshot,
				previous_snapshot,
			);

			Self::draw_branching_lanes(
				&mut lanes,
				&branching_lanes,
				cl,
			)
		});

		GraphRow {
			lane_count: LaneIndex::from(lanes.len()),
			commit_lane: LaneIndex::from(commit_lane.unwrap_or(0)),
			is_merge,
			is_branch_tip,
			is_stash,
			lanes,
			merge_bridge: merge_bridge.map(|(source, target)| {
				(LaneIndex::from(source), LaneIndex::from(target))
			}),
			branches,
		}
	}

	/// Locates the primary lane for the current commit.
	///
	/// Returns `None` if `commit_alias` is `None` or no matching lane is found.
	fn find_commit_lane(
		current_snapshot: &[Option<LaneSlot>],
		commit_alias: Option<CommitAlias>,
	) -> Option<usize> {
		let target_alias = commit_alias?;

		current_snapshot.iter().position(|slot| {
			slot.as_ref().is_some_and(|chunk| {
				chunk.alias() == Some(target_alias)
			})
		})
	}

	/// Computes the span (min, max) between the commit's lane and its second parent's lane.
	fn calculate_merge_bridge(
		current_snapshot: &[Option<LaneSlot>],
		commit_lane: usize,
		second_parent_alias: CommitAlias,
	) -> Option<(usize, usize)> {
		current_snapshot
			.iter()
			.position(|chunk_option| {
				chunk_option.as_ref().is_some_and(|chunk| {
					chunk.waits_for_second_parent(second_parent_alias)
				})
			})
			.map(|target_lane| {
				(
					commit_lane.min(target_lane),
					commit_lane.max(target_lane),
				)
			})
	}

	/// Columns whose lane drew a line into the row above and has no
	/// occupant now: each one closed into this row's commit and needs
	/// a bridge drawn across to its lane.
	///
	/// A column the row above held a root commit in is not one of
	/// them. [`LaneSlot::Settled`] draws a marker and no line
	/// downward, so there would be nothing above for the bridge to
	/// join, and drawing one would invent an edge between two
	/// unrelated histories.
	fn find_branching_lanes(
		current_snapshot: &[Option<LaneSlot>],
		previous_snapshot: Option<&[Option<LaneSlot>]>,
	) -> Vec<usize> {
		let Some(previous) = previous_snapshot else {
			return Vec::new();
		};

		previous
			.iter()
			.enumerate()
			.filter_map(|(index, previous_chunk)| {
				(previous_chunk
					.is_some_and(|chunk| chunk.awaits().is_some())
					&& current_snapshot
						.get(index)
						.is_none_or(Option::is_none))
				.then_some(index)
			})
			.collect()
	}

	/// Determines the correct node type for the active commit lane.
	const fn determine_commit_connection(
		is_stash: bool,
		is_merge: bool,
		is_branch_tip: bool,
	) -> ConnectionType {
		match (is_stash, is_merge, is_branch_tip) {
			(true, _, _) => ConnectionType::CommitStash,
			(_, true, _) => ConnectionType::CommitMerge,
			(_, _, true) => ConnectionType::CommitBranch,
			_ => ConnectionType::CommitNormal,
		}
	}

	/// Determines the correct vertical line style for non-commit passthrough lanes.
	fn determine_passthrough_connection(
		chunk: &LaneSlot,
		head_alias: Option<CommitAlias>,
	) -> Option<ConnectionType> {
		// A settled lane draws nothing below its commit.
		if matches!(chunk, LaneSlot::Settled { .. }) {
			return None;
		}

		// Dot the segment of any lane, not just lane 0, that is
		// heading toward HEAD: HEAD is not guaranteed to sit on the
		// leftmost lane, e.g. once earlier lanes have closed and a
		// later one has taken their place.
		let is_dotted = head_alias.is_some()
			&& (chunk.awaits() == head_alias
				|| chunk.second() == head_alias);

		if is_dotted {
			Some(ConnectionType::VerticalDotted)
		} else {
			Some(ConnectionType::Vertical)
		}
	}

	/// Lay the horizontal run of a bridge over the lanes strictly
	/// between its two ends, merging with whatever each cell already
	/// shows.
	///
	/// These cells are passed *through*, never joined — a bridge's two
	/// ends are drawn by the callers — so the run is recorded as a
	/// crossing. That is what keeps an unrelated lane drawn whole
	/// underneath it, and what stops it from swallowing a junction
	/// another bridge left in the same cell.
	fn draw_bridge_span(
		lanes: &mut [Option<(ConnectionType, LaneIndex)>],
		from: usize,
		to: usize,
		color: LaneIndex,
	) {
		for lane in lanes.iter_mut().take(to).skip(from + 1) {
			overlay_cell(lane, Directions::CROSS, color);
		}
	}
}
#[cfg(test)]
mod tests {
	use super::*;

	fn id(n: usize) -> CommitId {
		CommitId::from_str_unchecked(&format!("{n:040x}"))
			.expect("valid oid")
	}

	fn sym(conn: ConnectionType) -> char {
		match conn {
			ConnectionType::Vertical => '┃',
			ConnectionType::VerticalDotted => '╏',
			ConnectionType::CommitNormal => 'o',
			ConnectionType::CommitBranch => '*',
			ConnectionType::CommitMerge => 'M',
			ConnectionType::CommitStash => '*',
			ConnectionType::CommitUncommitted => '+',
			ConnectionType::MergeBridgeStart => '┓',
			ConnectionType::MergeBridgeMid => '━',
			ConnectionType::MergeBridgeEnd => '┏',
			ConnectionType::BranchUp => '┛',
			ConnectionType::BranchUpRight => '┗',
			ConnectionType::TeeLeft => '┫',
			ConnectionType::TeeRight => '┣',
			ConnectionType::TeeUp => '┻',
			ConnectionType::TeeDown => '┳',
			ConnectionType::Cross => '╋',
		}
	}

	/// Render a row the way the UI does: one glyph per lane plus a
	/// spacer that carries a bridge's horizontal run.
	fn row_to_string(row: &GraphRow) -> String {
		let mut out = String::new();
		for (lane_index, conn) in row.lanes.iter().enumerate() {
			out.push(conn.map_or(' ', |(c, _)| sym(c)));

			let in_bridge = row
				.merge_bridge
				.into_iter()
				.chain(row.branches.iter().copied())
				.any(|(from, to)| {
					lane_index >= usize::from(from)
						&& lane_index < usize::from(to)
				});
			out.push(if in_bridge { '━' } else { ' ' });
		}
		out.trim_end().to_string()
	}

	/// Walk `history` (newest first, `(commit, parents)`) and render
	/// every row.
	fn render(history: &[(usize, &[usize])]) -> Vec<String> {
		let mut walker = GraphWalker::new();
		let ids: Vec<CommitId> =
			history.iter().map(|(c, _)| id(*c)).collect();

		for (commit, parents) in history {
			let parents: Vec<CommitId> =
				parents.iter().map(|p| id(*p)).collect();
			walker.process(id(*commit), &parents);
		}

		walker
			.compute_rows(
				&ids,
				0,
				&HashSet::new(),
				&HashSet::new(),
				None,
			)
			.iter()
			.map(row_to_string)
			.collect()
	}

	#[test]
	fn linear_history() {
		let rows = render(&[(1, &[2]), (2, &[3]), (3, &[])]);
		assert_eq!(rows, vec!["o", "o", "o"]);
	}

	#[test]
	fn history_with_multiple_roots() {
		// Two independent commit chains, each terminating in its own
		// root (a commit with no parents), STAY IN THEIR LANE!
		let rows =
			render(&[(1, &[2]), (2, &[]), (3, &[4]), (4, &[])]);
		assert_eq!(rows, vec!["o", "o", "  o", "  o"]);
	}

	#[test]
	fn simple_merge() {
		// 1 merges 3 into the line 1 → 2 → 4, 3 → 4
		let rows =
			render(&[(1, &[2, 3]), (2, &[4]), (3, &[4]), (4, &[])]);
		assert_eq!(rows, vec!["M━┓", "o ┃", "┃ o", "o━┛"]);
	}

	#[test]
	fn merge_into_tracked_lane_continues_through_corner() {
		// 3's merge line joins lane 1 which keeps flowing to 5,
		// so the corner must be a junction (┫), not a dead end (┓)
		let rows = render(&[
			(1, &[3]),
			(2, &[5]),
			(3, &[4, 5]),
			(4, &[6]),
			(5, &[6]),
			(6, &[]),
		]);
		assert_eq!(
			rows,
			vec!["o", "┃ o", "M━┫", "o ┃", "┃ o", "o━┛"]
		);
	}

	#[test]
	fn merge_bridge_crosses_unrelated_lane() {
		// 3 (lane 2) merges into 1's line (lane 0) while 2's line
		// (lane 1) passes through: the crossed lane keeps its
		// vertical instead of being cut by the bridge
		let rows = render(&[
			(1, &[4]),
			(2, &[5]),
			(3, &[6, 4]),
			(4, &[7]),
			(5, &[7]),
			(6, &[7]),
			(7, &[]),
		]);
		assert_eq!(
			rows,
			vec![
				"o",
				"┃ o",
				"┣━┃━M",
				"o ┃ ┃",
				"┃ o ┃",
				"┃ ┃ o",
				"o━┻━┛",
			]
		);
	}

	#[test]
	fn overlapping_branch_bridges_keep_inner_corner() {
		// lanes 1 and 2 both close into the commit on lane 0; the
		// outer bridge passes through the inner corner (┻) instead
		// of erasing it
		let rows =
			render(&[(1, &[4]), (2, &[4]), (3, &[4]), (4, &[])]);
		assert_eq!(rows, vec!["o", "┃ o", "┃ ┃ o", "o━┻━┛"]);
	}

	#[test]
	fn merge_and_branch_bridges_overlap() {
		// commit 3 closes a branch from lane 2 while opening a merge
		// to lane 3, crossing lane 1: every line stays continuous
		let rows = render(&[
			(1, &[3, 4]),
			(2, &[3]),
			(3, &[5, 6]),
			(4, &[5]),
			(5, &[7]),
			(6, &[7]),
			(7, &[]),
		]);
		assert_eq!(
			rows,
			vec![
				"M━┓",
				"┃ ┃ o",
				"M━┃━┻━┓",
				"┃ o   ┃",
				"o━┛   ┃",
				"┃     o",
				"o━━━━━┛",
			]
		);
	}

	#[test]
	fn skewed_parent_before_child_leaves_no_phantom_lane() {
		//1 to 2 must be dropped instead of
		// opening a lane that waits for 2 until the end of the walk.
		let rows = render(&[(2, &[3]), (1, &[2, 3]), (3, &[])]);
		assert_eq!(rows, vec!["o", "┃ M", "o━┛"]);
	}

	#[test]
	fn complete_walk_settles_all_lanes() {
		// Both parents of 1 appear before it in the walk (clock
		// skew). After a complete walk no lane may still wait on a
		// parent, creating the evil phantom lines
		let history: &[(usize, &[usize])] =
			&[(2, &[4]), (3, &[4]), (1, &[2, 3]), (4, &[])];

		assert_eq!(render(history), vec!["o", "┃ o", "┃ ┃ M", "o━┛"]);

		let mut walker = GraphWalker::new();
		for (commit, parents) in history {
			let parents: Vec<CommitId> =
				parents.iter().map(|p| id(*p)).collect();
			walker.process(id(*commit), &parents);
		}

		for slot in walker.buffer.current.iter().flatten() {
			assert!(
				matches!(slot, LaneSlot::Settled { .. }),
				"lane still waiting on a parent after a complete walk: {slot:?}"
			);
		}
	}

	#[test]
	fn crossed_lane_keeps_own_color() {
		let rows = &[
			(1usize, &[4usize][..]),
			(2, &[5]),
			(3, &[6, 4]),
			(4, &[7]),
			(5, &[7]),
			(6, &[7]),
			(7, &[]),
		];
		let mut walker = GraphWalker::new();
		let ids: Vec<CommitId> =
			rows.iter().map(|(c, _)| id(*c)).collect();
		for (commit, parents) in rows {
			let parents: Vec<CommitId> =
				parents.iter().map(|p| id(*p)).collect();
			walker.process(id(*commit), &parents);
		}
		let computed = walker.compute_rows(
			&ids,
			0,
			&HashSet::new(),
			&HashSet::new(),
			None,
		);

		// row of commit 3: lane 1 is crossed by the merge bridge but
		// keeps both its vertical glyph and its own lane color
		let crossed = computed[2].lanes[1]
			.expect("crossed lane should not be empty");
		assert_eq!(crossed.0, ConnectionType::Vertical);
		assert_eq!(crossed.1, lane_color(1));
	}

	#[test]
	fn branch_bridge_crosses_unrelated_lane_keeps_own_color() {
		// 5 is the fork point for both 1 and 3
		let history: &[(usize, &[usize])] = &[
			(1, &[5]),
			(2, &[6]),
			(3, &[5]),
			(5, &[8]),
			(6, &[8]),
			(8, &[]),
		];
		let mut walker = GraphWalker::new();
		let ids: Vec<CommitId> =
			history.iter().map(|(c, _)| id(*c)).collect();
		for (commit, parents) in history {
			let parents: Vec<CommitId> =
				parents.iter().map(|p| id(*p)).collect();
			walker.process(id(*commit), &parents);
		}
		let computed = walker.compute_rows(
			&ids,
			0,
			&HashSet::new(),
			&HashSet::new(),
			None,
		);

		assert_eq!(
			computed.iter().map(row_to_string).collect::<Vec<_>>(),
			vec!["o", "┃ o", "┃ ┃ o", "o━┃━┛", "┃ o", "o━┛"]
		);

		// row of commit 5: lane 1 is crossed by the branch bridge but
		// keeps both its vertical glyph and its own lane color (visual identity mans)
		let crossed = computed[3].lanes[1]
			.expect("crossed lane should not be empty");
		assert_eq!(crossed.0, ConnectionType::Vertical);
		assert_eq!(crossed.1, lane_color(1));
	}
}
