pub const SYM_COMMIT: &str = "o";
pub const SYM_COMMIT_BRANCH: &str = "*";
pub const SYM_COMMIT_MERGE: &str = "M";
pub const SYM_COMMIT_STASH: &str = "*";
pub const SYM_COMMIT_UNCOMMITTED: &str = "+";
pub const SYM_VERTICAL: &str = "┃";
pub const SYM_VERTICAL_DOTTED: &str = "╏";
pub const SYM_HORIZONTAL: &str = "━";
pub const SYM_MERGE_BRIDGE_START: &str = "┓";
pub const SYM_MERGE_BRIDGE_MID: &str = "━";
pub const SYM_MERGE_BRIDGE_END: &str = "┏";
pub const SYM_BRANCH_UP: &str = "┛";
pub const SYM_BRANCH_UP_RIGHT: &str = "┗";
pub const SYM_TEE_LEFT: &str = "┫";
pub const SYM_TEE_RIGHT: &str = "┣";
pub const SYM_TEE_UP: &str = "┻";
pub const SYM_TEE_DOWN: &str = "┳";
pub const SYM_CROSS: &str = "╋";
pub const SYM_SPACE: &str = " ";

/// The glyph the graph column draws for `conn`.
pub const fn symbol(conn: ConnectionType) -> &'static str {
	match conn {
		ConnectionType::Vertical => SYM_VERTICAL,
		ConnectionType::VerticalDotted => SYM_VERTICAL_DOTTED,
		ConnectionType::CommitNormal => SYM_COMMIT,
		ConnectionType::CommitBranch => SYM_COMMIT_BRANCH,
		ConnectionType::CommitMerge => SYM_COMMIT_MERGE,
		ConnectionType::CommitStash => SYM_COMMIT_STASH,
		ConnectionType::CommitUncommitted => SYM_COMMIT_UNCOMMITTED,
		ConnectionType::MergeBridgeStart => SYM_MERGE_BRIDGE_START,
		ConnectionType::MergeBridgeMid => SYM_MERGE_BRIDGE_MID,
		ConnectionType::MergeBridgeEnd => SYM_MERGE_BRIDGE_END,
		ConnectionType::BranchUp => SYM_BRANCH_UP,
		ConnectionType::BranchUpRight => SYM_BRANCH_UP_RIGHT,
		ConnectionType::TeeLeft => SYM_TEE_LEFT,
		ConnectionType::TeeRight => SYM_TEE_RIGHT,
		ConnectionType::TeeUp => SYM_TEE_UP,
		ConnectionType::TeeDown => SYM_TEE_DOWN,
		ConnectionType::Cross => SYM_CROSS,
	}
}

use asyncgit::graph::ConnectionType;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn symbols_are_single_char_and_distinct() {
		let symbols = [
			SYM_COMMIT,
			SYM_COMMIT_BRANCH,
			SYM_COMMIT_MERGE,
			SYM_COMMIT_STASH,
			SYM_COMMIT_UNCOMMITTED,
			SYM_VERTICAL,
			SYM_VERTICAL_DOTTED,
			SYM_HORIZONTAL,
			SYM_MERGE_BRIDGE_START,
			SYM_MERGE_BRIDGE_MID,
			SYM_MERGE_BRIDGE_END,
			SYM_BRANCH_UP,
			SYM_BRANCH_UP_RIGHT,
			SYM_TEE_LEFT,
			SYM_TEE_RIGHT,
			SYM_TEE_UP,
			SYM_TEE_DOWN,
			SYM_CROSS,
			SYM_SPACE,
		];

		for symbol in symbols {
			assert_eq!(
				symbol.chars().count(),
				1,
				"{symbol:?} should be a single glyph"
			);
		}
	}
}
