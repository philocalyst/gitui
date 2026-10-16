use super::chunk::LaneSlot;
use super::{CommitAlias, UnwalkedAlias};
use std::collections::BTreeMap;

/// A single mutation of the lane state, recorded while processing one
/// commit.
///
/// A lane's column index *is* its visual identity. Rows are drawn one
/// at a time and joined to their neighbours by column, and the glyph
/// set has no diagonal, so a lane that moved sideways between two rows
/// could only be drawn as two segments that do not meet. These three
/// operations are therefore the only edits the walk may make:
/// [`Self::Set`] leaves a column where it is, and [`Self::Push`] and
/// [`Self::Pop`] only ever touch the right-hand end. No sequence of
/// them can shift a lane that is still open, which is why there is no
/// operation here for inserting or removing a column in the middle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeltaOp {
	/// Put `slot` into the existing column `index`.
	Set {
		index: usize,
		slot: Option<LaneSlot>,
	},
	/// Append a new rightmost column, holding `slot`.
	Push { slot: LaneSlot },
	/// Drop the rightmost column, which is always vacant.
	Pop,
}

/// All lane-state mutations caused by processing a single commit.
/// Applying a `Delta` to the lane state of row `n` yields the lane
/// state of row `n + 1`.
#[derive(Clone, Debug)]
pub struct Delta(pub Vec<DeltaOp>);

const CHECKPOINT_INTERVAL: usize = 100;

/// Delta-compressed history of the graph's lane state.
///
/// While walking the log top-down, every commit mutates the set of
/// active lanes (a `Vec<Option<LaneSlot>>`, one slot per lane). So, storign a
/// full copy of that state for each commit is a waste. This
/// buffer preserves ONLY the latest state PLUS the list of [`Delta`]s that
/// produced it.
/// Use [`Buffer::decompress`] to get the complete version.
#[derive(Default)]
pub struct Buffer {
	/// Lane state after the most recently processed commit.
	pub current: Vec<Option<LaneSlot>>,

	/// One [`Delta`] per processed commit, in the order of the walk.
	pub deltas: Vec<Delta>,

	/// Full lane-state snapshots taken every `CHECKPOINT_INTERVAL`
	/// commits, keyed by delta index, for reducing decompression cost.
	pub checkpoints: BTreeMap<usize, Vec<Option<LaneSlot>>>,

	/// Aliases of merge commits whose second parent still needs a new
	/// lane.
	merge_commits: Vec<CommitAlias>,

	/// The [`DeltaOp`]s recorded so far for the commit in flight.
	pending_delta: Vec<DeltaOp>,

	/// Which columns the row above the commit in flight leaves blank,
	/// so a lane may start in one of them without a line above it to
	/// join. Rebuilt at the start of every step.
	blank_above: Vec<bool>,
}

impl Buffer {
	/// Remember `alias` as a merge commit whose second parent must be
	/// given its own lane.
	pub fn track_merge_commit(&mut self, alias: CommitAlias) {
		self.merge_commits.push(alias);
	}

	pub fn update(&mut self, new_chunk: &LaneSlot) {
		self.start_step();

		let placement_index = self.place_chunk(new_chunk);

		if let Some(alias) = new_chunk.alias() {
			self.consume_alias_in_other_chunks(
				alias,
				placement_index,
			);
		}

		self.flush_merge_commits();
		self.commit_delta();
	}

	/// Note which columns the row above draws nothing in.
	///
	/// A column that is vacant *now* is not the same thing: the ones
	/// emptied while this very commit is being processed still have a
	/// lane drawn in the row above, and are the reason
	/// [`Self::reserve_lane_right_of`] cannot simply look for a hole.
	///
	/// A root commit's column is not blank. [`LaneSlot::Settled`]
	/// draws a marker and no line below it, so the column is dead for
	/// the rest of the walk, and it stays occupied on purpose: a
	/// commit marker drawn directly beneath another one, with no line
	/// between them, is how a lane draws two consecutive commits, so
	/// handing the column to an unrelated history would read as
	/// joining the two.
	fn start_step(&mut self) {
		self.blank_above.clear();
		self.blank_above
			.extend(self.current.iter().map(Option::is_none));
	}

	fn place_chunk(&mut self, new_chunk: &LaneSlot) -> usize {
		// Prefer a lane whose current occupant awaits this chunk's
		// commit as its primary parent, then the leftmost column the
		// row above leaves blank, and only then a new column. All
		// three land the chunk somewhere no open lane is drawn, so
		// nothing already on screen has to move.
		let target = self
			.find_lane_awaiting_parent(new_chunk.alias())
			.or_else(|| self.first_empty_lane())
			.unwrap_or(self.current.len());

		if target < self.current.len() {
			self.record_set(target, Some(*new_chunk));
		} else {
			self.record_push(*new_chunk);
		}
		target
	}

	fn find_lane_awaiting_parent(
		&self,
		alias: Option<CommitAlias>,
	) -> Option<usize> {
		alias.and_then(|alias| {
			self.current.iter().position(|slot| {
				slot.as_ref().is_some_and(|chunk| {
					chunk.awaits() == Some(alias)
				})
			})
		})
	}

	fn first_empty_lane(&self) -> Option<usize> {
		self.current.iter().position(Option::is_none)
	}

	/// The column holding `alias`, with its slot.
	fn lane_of(
		&self,
		alias: CommitAlias,
	) -> Option<(usize, LaneSlot)> {
		self.current.iter().enumerate().find_map(|(index, slot)| {
			let chunk = (*slot)?;
			(chunk.alias() == Some(alias)).then_some((index, chunk))
		})
	}

	fn consume_alias_in_other_chunks(
		&mut self,
		alias: CommitAlias,
		skip_index: usize,
	) {
		for index in 0..self.current.len() {
			if index == skip_index {
				continue;
			}

			// `LaneSlot` is `Copy`; matching by value lets us read
			// the slot out before `record_set` mutates `current`.
			let new_chunk = match self.current[index] {
				Some(
					LaneSlot::Flowing { parent, .. }
					| LaneSlot::FlowingMerge { parent, .. }
					| LaneSlot::Reserved { parent },
				) if *parent == alias => {
					// The awaited parent was JUST placed. Close the
					// lane. The pending second parent is dropped
					// with it.
					None
				}
				Some(LaneSlot::FlowingMerge {
					alias: merge_alias,
					parent,
					second,
				}) if *second == alias => {
					// The second parent was just placed
					// bridge resolves and the lane flows
					Some(LaneSlot::Flowing {
						alias: merge_alias,
						parent,
					})
				}
				_ => continue,
			};

			self.record_set(index, new_chunk);
		}
	}

	fn flush_merge_commits(&mut self) {
		while let Some(alias) = self.merge_commits.pop() {
			let Some((index, chunk)) = self.lane_of(alias) else {
				continue;
			};

			// Only a merge still owing its second parent needs a
			// lane split off; anything else is a no-op.
			let LaneSlot::FlowingMerge {
				alias: merge_alias,
				parent,
				second,
			} = chunk
			else {
				continue;
			};

			self.record_set(
				index,
				Some(LaneSlot::Flowing {
					alias: merge_alias,
					parent,
				}),
			);

			self.reserve_lane_right_of(index, second);
		}
	}

	/// Open the lane that a merge's second parent will arrive in,
	/// somewhere to the right of the merge itself.
	///
	/// The nearest column the row above leaves blank wins, which keeps
	/// the bridge short; failing that the lane goes on the right-hand
	/// end. A column that only *became* blank while this very commit
	/// was processed is not a candidate: the row above still draws a
	/// lane there, and taking the column over would replace that
	/// lane's closing corner with this one's, erasing an edge.
	///
	/// Columns to the left are left alone even when they are free.
	/// Reusing them would be just as sound, but it would put a merged
	/// branch on the far side of the history it was merged into, and
	/// `place_chunk` claims those columns for new branch tips anyway.
	fn reserve_lane_right_of(
		&mut self,
		merge_lane: usize,
		parent: UnwalkedAlias,
	) {
		let slot = LaneSlot::Reserved { parent };
		let reusable =
			(merge_lane + 1..self.current.len()).find(|&index| {
				self.current[index].is_none()
					&& self
						.blank_above
						.get(index)
						.copied()
						.unwrap_or(false)
			});

		match reusable {
			Some(index) => self.record_set(index, Some(slot)),
			None => self.record_push(slot),
		}
	}

	fn commit_delta(&mut self) {
		while matches!(self.current.last(), Some(None)) {
			self.record_pop();
		}

		self.deltas
			.push(Delta(std::mem::take(&mut self.pending_delta)));

		let step = self.deltas.len();
		if step.is_multiple_of(CHECKPOINT_INTERVAL) {
			self.checkpoints.insert(step - 1, self.current.clone());
		}
	}

	fn record_set(&mut self, index: usize, slot: Option<LaneSlot>) {
		self.pending_delta.push(DeltaOp::Set { index, slot });
		self.current[index] = slot;
	}

	fn record_push(&mut self, slot: LaneSlot) {
		self.pending_delta.push(DeltaOp::Push { slot });
		self.current.push(Some(slot));
	}

	fn record_pop(&mut self) {
		self.pending_delta.push(DeltaOp::Pop);
		self.current.pop();
	}

	pub fn decompress(
		&self,
		start: usize,
		end: usize,
	) -> Vec<Vec<Option<LaneSlot>>> {
		let (current_index, mut state) =
			self.checkpoints.range(..=start).next_back().map_or_else(
				|| (None, Vec::new()),
				|(&i, s)| (Some(i), s.clone()),
			);

		let mut history =
			Vec::with_capacity(end.saturating_sub(start) + 1);

		if let Some(index) = current_index {
			if index >= start && index <= end {
				history.push(state.clone());
			}
		}

		let loop_start = current_index.map_or(0, |i| i + 1);

		for delta_index in loop_start..=end {
			if let Some(delta) = self.deltas.get(delta_index) {
				Self::apply_delta_to_state(&mut state, delta);

				if delta_index >= start {
					history.push(state.clone());
				}
			} else {
				break;
			}
		}

		history
	}

	fn apply_delta_to_state(
		state: &mut Vec<Option<LaneSlot>>,
		delta: &Delta,
	) {
		for op in &delta.0 {
			match *op {
				DeltaOp::Set { index, slot } => state[index] = slot,
				DeltaOp::Push { slot } => state.push(Some(slot)),
				DeltaOp::Pop => {
					state.pop();
				}
			}
		}
	}
}
