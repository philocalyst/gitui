//! Holds a real repository's history to the same invariants as the
//! tests above.
//!
//! Every bug these invariants have caught so far was reachable from a
//! generated topology once the generator knew to produce the right
//! shape — but two of them were found here first, on a repository
//! busy enough to stumble into states the generator was not reaching.
//! Ignored by default, because it needs a checkout to point at:
//!
//! ```text
//! git -C <repo> rev-list --parents --topo-order --max-count=20000 HEAD > /tmp/parents
//! GITUI_GRAPH_PARENTS=/tmp/parents cargo test -p asyncgit \
//!     graph::walker::real_repository -- --ignored --nocapture
//! ```
use crate::graph::harness::Scene;
use crate::sync::CommitId;
use std::collections::HashSet;

#[test]
#[ignore = "needs GITUI_GRAPH_PARENTS; see the module docs"]
fn holds_every_invariant() {
	let Ok(path) = std::env::var("GITUI_GRAPH_PARENTS") else {
		return;
	};
	let text = std::fs::read_to_string(&path)
		.expect("GITUI_GRAPH_PARENTS names a readable file");

	let history = text
		.lines()
		.map(|line| {
			let mut ids = line.split_whitespace().map(|id| {
				CommitId::from_str_unchecked(id)
					.expect("rev-list emits valid object ids")
			});
			let commit = ids.next().expect("a line names its commit");
			(commit, ids.collect())
		})
		.collect::<Vec<_>>();

	let total = history.len();
	Scene::walk_ids(history, HashSet::new(), HashSet::new(), None);
	eprintln!("{total} commits drawn; every invariant holds");
}
