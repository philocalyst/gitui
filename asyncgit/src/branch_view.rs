//! Background branch-to-base review data.

use crate::{
	asyncjob::{AsyncJob, RunParams},
	error::Result,
	graph::{GraphRow, GraphWalker},
	sync::{CommitId, RepoPath},
	AsyncGitNotification,
};
use git2::{DiffOptions, Oid, Repository, RepositoryOpenFlags};
use std::{
	cmp::Reverse,
	collections::{BinaryHeap, HashMap, HashSet, VecDeque},
	path::PathBuf,
	sync::atomic::{AtomicBool, Ordering},
	sync::{Arc, Mutex, OnceLock},
};

const DEFAULT_TOPOLOGY_LIMIT: usize = 200;
const RELATIONSHIP_CACHE_CAPACITY: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RelationshipCacheKey {
	repository: PathBuf,
	branch: Oid,
	base: Oid,
}

#[derive(Clone)]
struct CachedRelationship {
	merge_base: Option<CommitId>,
	ahead: usize,
	behind: usize,
	commits: Vec<BranchViewCommit>,
	files: Vec<BranchViewFile>,
}

#[derive(Default)]
struct RelationshipCache {
	entries: HashMap<RelationshipCacheKey, CachedRelationship>,
	order: VecDeque<RelationshipCacheKey>,
}

static RELATIONSHIP_CACHE: OnceLock<Mutex<RelationshipCache>> =
	OnceLock::new();

/// Exact refs to compare. The generation lets UI consumers reject stale work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchViewRequest {
	/// The selected ref, for example `refs/heads/topic`.
	pub branch_reference: String,
	/// Comparison ref. `None` means the repository's current `HEAD`.
	pub base_reference: Option<String>,
	/// Branch tip captured before the background request is started.
	pub branch_tip: CommitId,
	/// Explicit base tip captured when it is a known branch ref.
	/// `None` means resolve `HEAD` in the worker.
	pub base_tip: Option<CommitId>,
	/// Monotonically increasing UI request identity.
	pub generation: u64,
	/// Maximum number of union and branch-only history rows to return.
	/// Zero uses the backend default of 200 rows.
	pub topology_limit: usize,
}

/// One commit which is reachable from the selected branch and not its base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchViewCommit {
	/// Commit object id.
	pub id: CommitId,
	/// One-line summary.
	pub summary: String,
	/// Author timestamp in seconds since the Unix epoch.
	pub time: i64,
}

/// File changed between the selected branch tip and its merge base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchViewFile {
	/// Repository-relative path at the branch tip, when present.
	pub path: String,
	/// Git status character, such as `M`, `A`, or `D`.
	pub status: char,
}

/// The endpoint role a topology commit has in the comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchTopologyRole {
	/// Captured selected branch tip.
	BranchTip,
	/// Captured comparison base tip.
	BaseTip,
	/// Actual merge base, when present in this returned window.
	MergeBase,
}

/// One commit in the bounded union history of the selected branch and base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchViewTopologyCommit {
	/// Commit object id.
	pub id: CommitId,
	/// Actual parent ids from the commit object, including parents outside the window.
	pub parents: Vec<CommitId>,
	/// One-line summary.
	pub summary: String,
	/// Author timestamp in seconds since the Unix epoch.
	pub time: i64,
	/// Lane and connection data computed by the shared graph walker.
	pub graph: GraphRow,
	/// Roles this commit has; multiple roles apply when endpoints coincide.
	pub roles: Vec<BranchTopologyRole>,
	/// Exact request ref labels attached to this endpoint.
	pub labels: Vec<String>,
	/// Exact parent ids whose rows are outside the returned prefix.
	pub outside_parents: Vec<CommitId>,
}

/// Computed branch relationship and review material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchViewResult {
	/// The request that produced this result.
	pub request: BranchViewRequest,
	/// Selected branch tip.
	pub tip: CommitId,
	/// Selected base tip.
	pub base: CommitId,
	/// Best common ancestor, absent for unrelated histories.
	pub merge_base: Option<CommitId>,
	/// Commits unique to the selected branch when a merge base exists.
	pub ahead: Option<usize>,
	/// Commits unique to the base when a merge base exists.
	pub behind: Option<usize>,
	/// Exact number of branch-only commits, including rows beyond the result limit.
	pub commit_count: Option<usize>,
	/// Branch-only commits, newest first.
	pub commits: Vec<BranchViewCommit>,
	/// Files changed from merge base to selected branch tip.
	pub files: Vec<BranchViewFile>,
	/// Topology rows from the requested bounded union-history window.
	pub topology: Vec<BranchViewTopologyCommit>,
	/// Effective limit applied to both branch and union-history rows.
	pub topology_limit: usize,
	/// More rows exist after the returned prefix.
	pub topology_truncated: bool,
	/// Whether merge-base, ahead/behind, count, and file data have completed.
	pub relationship_loaded: bool,
}

enum State {
	Request(RepoPath, BranchViewRequest),
	Response(BranchViewRequest, Box<Result<BranchViewResult>>),
}

/// Async job for loading branch comparison details off the UI thread.
#[derive(Clone)]
pub struct BranchViewJob {
	state: Arc<Mutex<Option<State>>>,
	topology:
		Arc<Mutex<Option<(BranchViewRequest, BranchViewResult)>>>,
	topology_delivered: Arc<AtomicBool>,
	cancelled: Arc<AtomicBool>,
}

impl BranchViewJob {
	/// Create a comparison request.
	pub fn new(repo: RepoPath, request: BranchViewRequest) -> Self {
		Self {
			state: Arc::new(Mutex::new(Some(State::Request(
				repo, request,
			)))),
			topology: Arc::new(Mutex::new(None)),
			topology_delivered: Arc::new(AtomicBool::new(false)),
			cancelled: Arc::new(AtomicBool::new(false)),
		}
	}

	/// Stop this request before starting its unbounded relationship work.
	pub fn cancel(&self) {
		self.cancelled.store(true, Ordering::Relaxed);
	}

	/// Take the completed result, if this job has finished.
	pub fn result(
		&self,
	) -> Option<(BranchViewRequest, Result<BranchViewResult>)> {
		let mut state = self.state.lock().ok()?;
		match state.take()? {
			State::Response(request, result) => {
				Some((request, *result))
			}
			State::Request(repo, request) => {
				*state = Some(State::Request(repo, request));
				None
			}
		}
	}

	/// Take the topology-only snapshot published before exact relationship work.
	pub fn topology_result(
		&self,
	) -> Option<(BranchViewRequest, BranchViewResult)> {
		let snapshot = self.topology.lock().ok()?.clone()?;
		if self.topology_delivered.swap(true, Ordering::AcqRel) {
			None
		} else {
			Some(snapshot)
		}
	}
}

impl AsyncJob for BranchViewJob {
	type Notification = AsyncGitNotification;
	type Progress = ();

	fn run(
		&mut self,
		params: RunParams<Self::Notification, Self::Progress>,
	) -> Result<Self::Notification> {
		let request =
			self.state.lock().ok().and_then(|mut state| match state
				.take()?
			{
				State::Request(path, request) => {
					Some((path, request))
				}
				response @ State::Response(_, _) => {
					*state = Some(response);
					None
				}
			});
		if let Some((path, request)) = request {
			let result = load_topology_snapshot(&path, &request);
			if let Ok(topology) = &result {
				if let Ok(mut snapshot) = self.topology.lock() {
					*snapshot =
						Some((request.clone(), topology.clone()));
				}
				params
					.send(AsyncGitNotification::BranchViewTopology)?;
			}
			let result = result.and_then(|topology| {
				if self.cancelled.load(Ordering::Relaxed) {
					Ok(topology)
				} else {
					complete_relationship(&path, topology)
				}
			});
			if let Ok(mut state) = self.state.lock() {
				*state =
					Some(State::Response(request, Box::new(result)));
			}
		}
		Ok(AsyncGitNotification::BranchView)
	}
}

/// Load the relationship between two refs. This function performs Git work
/// synchronously and is normally called only by [`BranchViewJob`].
pub fn load_branch_view(
	path: &RepoPath,
	request: &BranchViewRequest,
) -> Result<BranchViewResult> {
	let repository = Repository::open_ext(
		path.gitpath(),
		RepositoryOpenFlags::FROM_ENV,
		Vec::<&std::path::Path>::new(),
	)?;
	if let Some(workdir) = path.workdir() {
		repository.set_workdir(workdir, false)?;
	}
	load_from_repository(&repository, request)
}

fn resolve_commit(
	repository: &Repository,
	reference: &str,
) -> Result<Oid> {
	let object = repository.revparse_single(reference)?;
	Ok(object.peel_to_commit()?.id())
}

fn load_from_repository(
	repository: &Repository,
	request: &BranchViewRequest,
) -> Result<BranchViewResult> {
	let branch = request.branch_tip.get_oid();
	let base = if let Some(id) = request.base_tip {
		id.get_oid()
	} else {
		let base_name =
			request.base_reference.as_deref().unwrap_or("HEAD");
		resolve_commit(repository, base_name)?
	};
	let topology_limit = if request.topology_limit == 0 {
		DEFAULT_TOPOLOGY_LIMIT
	} else {
		request.topology_limit
	};
	let topology = build_topology_snapshot(
		repository,
		request,
		branch,
		base,
		topology_limit,
	)?;
	complete_relationship_with_repository(repository, topology)
}

fn load_topology_snapshot(
	path: &RepoPath,
	request: &BranchViewRequest,
) -> Result<BranchViewResult> {
	let repository = Repository::open_ext(
		path.gitpath(),
		RepositoryOpenFlags::FROM_ENV,
		Vec::<&std::path::Path>::new(),
	)?;
	if let Some(workdir) = path.workdir() {
		repository.set_workdir(workdir, false)?;
	}
	let branch = request.branch_tip.get_oid();
	let base = if let Some(id) = request.base_tip {
		id.get_oid()
	} else {
		let base_name =
			request.base_reference.as_deref().unwrap_or("HEAD");
		resolve_commit(&repository, base_name)?
	};
	let topology_limit = if request.topology_limit == 0 {
		DEFAULT_TOPOLOGY_LIMIT
	} else {
		request.topology_limit
	};
	build_topology_snapshot(
		&repository,
		request,
		branch,
		base,
		topology_limit,
	)
}

fn complete_relationship(
	path: &RepoPath,
	view: BranchViewResult,
) -> Result<BranchViewResult> {
	let repository = Repository::open_ext(
		path.gitpath(),
		RepositoryOpenFlags::FROM_ENV,
		Vec::<&std::path::Path>::new(),
	)?;
	if let Some(workdir) = path.workdir() {
		repository.set_workdir(workdir, false)?;
	}
	complete_relationship_with_repository(&repository, view)
}

fn build_topology_snapshot(
	repository: &Repository,
	request: &BranchViewRequest,
	branch: Oid,
	base: Oid,
	topology_limit: usize,
) -> Result<BranchViewResult> {
	let (topology, topology_truncated) = walk_topology(
		repository,
		branch,
		base,
		None,
		request,
		topology_limit,
	)?;
	Ok(BranchViewResult {
		request: request.clone(),
		tip: branch.into(),
		base: base.into(),
		merge_base: None,
		ahead: None,
		behind: None,
		commit_count: None,
		commits: Vec::new(),
		files: Vec::new(),
		topology,
		topology_limit,
		topology_truncated,
		relationship_loaded: false,
	})
}

fn complete_relationship_with_repository(
	repository: &Repository,
	mut view: BranchViewResult,
) -> Result<BranchViewResult> {
	let branch = view.tip.get_oid();
	let base = view.base.get_oid();
	let mut relationship = relationship_for(
		repository,
		branch,
		base,
		view.topology_limit,
	)?;
	let merge_base_id = relationship.merge_base;
	for row in &mut view.topology {
		if Some(row.id) == merge_base_id
			&& !row.roles.contains(&BranchTopologyRole::MergeBase)
		{
			row.roles.push(BranchTopologyRole::MergeBase);
		}
	}
	view.merge_base = merge_base_id;
	view.ahead = Some(relationship.ahead);
	view.behind = Some(relationship.behind);
	view.commit_count = Some(relationship.ahead);
	view.commits = std::mem::take(&mut relationship.commits);
	view.files = relationship.files;
	view.relationship_loaded = true;
	Ok(view)
}

fn relationship_for(
	repository: &Repository,
	branch: Oid,
	base: Oid,
	limit: usize,
) -> Result<CachedRelationship> {
	let key = RelationshipCacheKey {
		repository: repository.path().to_path_buf(),
		branch,
		base,
	};
	let mut relationship = if let Some(cached) =
		cached_relationship(&key)
	{
		cached
	} else {
		let merge_base = match repository.merge_base(branch, base) {
			Ok(id) => Some(id),
			Err(error)
				if error.code() == git2::ErrorCode::NotFound =>
			{
				None
			}
			Err(error) => return Err(error.into()),
		};
		let (ahead, behind) =
			repository.graph_ahead_behind(branch, base)?;
		let files = if let Some(merge_base) = merge_base {
			let base_tree =
				repository.find_commit(merge_base)?.tree()?;
			let branch_tree =
				repository.find_commit(branch)?.tree()?;
			let mut options = DiffOptions::new();
			options.include_typechange(true).include_untracked(false);
			let diff = repository.diff_tree_to_tree(
				Some(&base_tree),
				Some(&branch_tree),
				Some(&mut options),
			)?;
			diff.deltas()
				.map(|delta| {
					let path = delta
						.new_file()
						.path()
						.or_else(|| delta.old_file().path())
						.unwrap_or_else(|| std::path::Path::new(""))
						.to_string_lossy()
						.into_owned();
					BranchViewFile {
						path,
						status: match delta.status() {
							git2::Delta::Added
							| git2::Delta::Untracked => 'A',
							git2::Delta::Deleted => 'D',
							git2::Delta::Renamed => 'R',
							git2::Delta::Copied => 'C',
							git2::Delta::Typechange => 'T',
							git2::Delta::Conflicted => 'U',
							_ => 'M',
						},
					}
				})
				.collect()
		} else {
			Vec::new()
		};
		CachedRelationship {
			merge_base: merge_base.map(Into::into),
			ahead,
			behind,
			commits: Vec::new(),
			files,
		}
	};
	if relationship.commits.len() < limit
		&& relationship.commits.len() < relationship.ahead
	{
		relationship.commits =
			walk_range(repository, branch, base, limit)?
				.into_iter()
				.map(|id| {
					let commit = repository.find_commit(id)?;
					Ok(BranchViewCommit {
						id: commit.id().into(),
						summary: commit
							.summary()?
							.unwrap_or_default()
							.to_owned(),
						time: commit.time().seconds(),
					})
				})
				.collect::<Result<Vec<_>>>()?;
	}
	store_cached_relationship(key, relationship.clone());
	Ok(relationship)
}

fn cached_relationship(
	key: &RelationshipCacheKey,
) -> Option<CachedRelationship> {
	let cache = RELATIONSHIP_CACHE.get_or_init(Default::default);
	let mut cache = cache.lock().ok()?;
	let value = cache.entries.get(key)?.clone();
	if let Some(index) =
		cache.order.iter().position(|candidate| candidate == key)
	{
		cache.order.remove(index);
	}
	cache.order.push_back(key.clone());
	Some(value)
}

fn store_cached_relationship(
	key: RelationshipCacheKey,
	value: CachedRelationship,
) {
	let cache = RELATIONSHIP_CACHE.get_or_init(Default::default);
	if let Ok(mut cache) = cache.lock() {
		if let Some(index) =
			cache.order.iter().position(|candidate| candidate == &key)
		{
			cache.order.remove(index);
		}
		cache.order.push_back(key.clone());
		cache.entries.insert(key, value);
		while cache.order.len() > RELATIONSHIP_CACHE_CAPACITY {
			if let Some(expired) = cache.order.pop_front() {
				cache.entries.remove(&expired);
			}
		}
	}
}

fn walk_topology(
	repository: &Repository,
	branch: Oid,
	base: Oid,
	merge_base: Option<Oid>,
	request: &BranchViewRequest,
	limit: usize,
) -> Result<(Vec<BranchViewTopologyCommit>, bool)> {
	let (discovered_ids, truncated) =
		discover_topology_prefix(repository, branch, base, limit)?;
	let page_ids =
		sort_known_topology_prefix(repository, discovered_ids)?;
	let rows = build_topology_rows(
		repository, &page_ids, branch, base, merge_base, request,
	)?;
	Ok((rows, truncated))
}

fn discover_topology_prefix(
	repository: &Repository,
	branch: Oid,
	base: Oid,
	limit: usize,
) -> Result<(Vec<Oid>, bool)> {
	let gix_repository = crate::sync::gix_repo(&RepoPath::from(
		repository.path().to_path_buf(),
	))?;
	let branch_id =
		gix::ObjectId::from_hex(branch.to_string().as_bytes())
			.map_err(|error| {
				crate::error::Error::Generic(error.to_string())
			})?;
	let base_gix_id =
		gix::ObjectId::from_hex(base.to_string().as_bytes())
			.map_err(|error| {
				crate::error::Error::Generic(error.to_string())
			})?;
	let mut tips = vec![branch_id];
	if branch != base {
		tips.push(base_gix_id);
	}
	let walk = gix_repository.rev_walk(tips).all()?;
	let mut ids = Vec::with_capacity(limit.saturating_add(1));
	for info in walk.take(limit.saturating_add(1)) {
		let info = info.map_err(|error| {
			crate::error::Error::Generic(error.to_string())
		})?;
		ids.push(Oid::from_str(&info.id.to_string())?);
	}
	let topology_truncated = ids.len() > limit;
	let prefix_end = ids.len().min(limit);
	Ok((ids[..prefix_end].to_vec(), topology_truncated))
}

fn sort_known_topology_prefix(
	repository: &Repository,
	mut page_ids: Vec<Oid>,
) -> Result<Vec<Oid>> {
	let mut parents_by_id = HashMap::with_capacity(page_ids.len());
	for id in &page_ids {
		let commit = repository.find_commit(*id)?;
		parents_by_id
			.insert(*id, commit.parent_ids().collect::<Vec<_>>());
	}
	// The bounded gix walk discovers a prefix without traversing the full
	// history. Reorder only that known prefix so every included child precedes
	// each included parent, even when merge paths have uneven lengths.
	let mut index_by_id = HashMap::with_capacity(page_ids.len());
	for (index, id) in page_ids.iter().copied().enumerate() {
		index_by_id.insert(id, index);
	}
	let mut pending_children = vec![0usize; page_ids.len()];
	let mut parents_by_index = vec![Vec::new(); page_ids.len()];
	for (child_index, child) in page_ids.iter().enumerate() {
		if let Some(parents) = parents_by_id.get(child) {
			for parent in parents {
				if let Some(parent_index) = index_by_id.get(parent) {
					pending_children[*parent_index] += 1;
					parents_by_index[child_index].push(*parent_index);
				}
			}
		}
	}
	let mut ready = BinaryHeap::new();
	for (index, count) in pending_children.iter().enumerate() {
		if *count == 0 {
			ready.push(Reverse(index));
		}
	}
	let mut topo_indices = Vec::with_capacity(page_ids.len());
	while let Some(Reverse(index)) = ready.pop() {
		topo_indices.push(index);
		for parent_index in &parents_by_index[index] {
			pending_children[*parent_index] -= 1;
			if pending_children[*parent_index] == 0 {
				ready.push(Reverse(*parent_index));
			}
		}
	}
	if topo_indices.len() != page_ids.len() {
		return Err(crate::error::Error::Generic(
			"commit graph contains a cycle".to_owned(),
		));
	}
	page_ids = topo_indices
		.into_iter()
		.map(|index| page_ids[index])
		.collect();
	Ok(page_ids)
}

fn build_topology_rows(
	repository: &Repository,
	page_ids: &[Oid],
	branch: Oid,
	base: Oid,
	merge_base: Option<Oid>,
	request: &BranchViewRequest,
) -> Result<Vec<BranchViewTopologyCommit>> {
	let page_ids_set =
		page_ids.iter().copied().collect::<HashSet<_>>();
	let branch_id: CommitId = branch.into();
	let base_id: CommitId = base.into();
	let merge_base_id = merge_base.map(Into::into);
	let branch_label = request.branch_reference.clone();
	let base_label = request
		.base_reference
		.clone()
		.unwrap_or_else(|| "HEAD".to_owned());
	let mut walker = GraphWalker::new();
	let mut topology_commits = Vec::with_capacity(page_ids.len());
	let mut page_records = Vec::with_capacity(page_ids.len());
	for id in page_ids {
		let commit = repository.find_commit(*id)?;
		let parents = commit
			.parent_ids()
			.map(Into::into)
			.collect::<Vec<CommitId>>();
		let commit_id: CommitId = (*id).into();
		walker.process(commit_id, &parents);
		page_records.push((
			commit_id,
			parents,
			commit.summary()?.unwrap_or_default().to_owned(),
			commit.time().seconds(),
		));
	}
	let page_commits = &page_records;
	let commit_ids = page_commits
		.iter()
		.map(|(id, _, _, _)| *id)
		.collect::<Vec<_>>();
	let mut branch_tips = HashSet::new();
	branch_tips.insert(branch_id);
	branch_tips.insert(base_id);
	let graph_rows = walker.compute_rows(
		&commit_ids,
		0,
		&branch_tips,
		&HashSet::new(),
		request.base_reference.is_none().then_some(&base_id),
	);
	for ((id, parents, summary, time), graph) in
		page_commits.iter().zip(graph_rows)
	{
		let mut labels = Vec::new();
		let mut roles = Vec::new();
		if *id == branch_id {
			roles.push(BranchTopologyRole::BranchTip);
			labels.push(branch_label.clone());
		}
		if *id == base_id {
			roles.push(BranchTopologyRole::BaseTip);
			labels.push(base_label.clone());
		}
		if Some(*id) == merge_base_id {
			roles.push(BranchTopologyRole::MergeBase);
		}
		topology_commits.push(BranchViewTopologyCommit {
			id: *id,
			parents: parents.clone(),
			summary: summary.clone(),
			time: *time,
			graph,
			roles,
			labels,
			outside_parents: parents
				.iter()
				.filter(|parent| {
					!page_ids_set.contains(&parent.get_oid())
				})
				.copied()
				.collect(),
		});
	}
	Ok(topology_commits)
}

fn walk_range(
	repository: &Repository,
	include: Oid,
	exclude: Oid,
	limit: usize,
) -> Result<Vec<Oid>> {
	let mut walk = repository.revwalk()?;
	walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)?;
	walk.push(include)?;
	walk.hide(exclude)?;
	Ok(walk
		.take(limit)
		.collect::<std::result::Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
	use super::{load_branch_view, BranchViewRequest};
	use crate::sync::{
		commit_files::OldNew, diff::DiffOptions, RepoPath,
	};
	use crate::{asyncjob::AsyncSingleJob, AsyncGitNotification};
	use crossbeam_channel::unbounded;
	use git2::{Repository, Signature};
	use std::collections::{HashMap, HashSet};
	use std::path::Path;
	use std::process::Command;
	use std::time::Duration;

	fn commit(
		repository: &Repository,
		reference: &str,
		message: &str,
		parent: Option<git2::Oid>,
		file: Option<(&str, &[u8])>,
	) -> git2::Oid {
		let mut index = repository.index().unwrap();
		if let Some(parent) = parent {
			let tree = repository
				.find_commit(parent)
				.unwrap()
				.tree()
				.unwrap();
			index.read_tree(&tree).unwrap();
		} else {
			index.clear().unwrap();
		}
		if let Some((path, content)) = file {
			std::fs::write(
				repository.workdir().unwrap().join(path),
				content,
			)
			.unwrap();
			index.add_path(Path::new(path)).unwrap();
		}
		index.write().unwrap();
		let tree = repository
			.find_tree(index.write_tree().unwrap())
			.unwrap();
		let signature = Signature::now(
			"Branch View",
			"branch-view@example.invalid",
		)
		.unwrap();
		let parents = parent
			.into_iter()
			.map(|id| repository.find_commit(id).unwrap())
			.collect::<Vec<_>>();
		let parent_refs = parents.iter().collect::<Vec<_>>();
		repository
			.commit(
				Some(reference),
				&signature,
				&signature,
				message,
				&tree,
				&parent_refs,
			)
			.unwrap()
	}

	fn request(
		branch_reference: &str,
		base_reference: Option<&str>,
		branch_tip: git2::Oid,
		base_tip: Option<git2::Oid>,
		generation: u64,
	) -> BranchViewRequest {
		BranchViewRequest {
			branch_reference: branch_reference.to_owned(),
			base_reference: base_reference.map(str::to_owned),
			branch_tip: branch_tip.into(),
			base_tip: base_tip.map(Into::into),
			generation,
			topology_limit: 200,
		}
	}

	fn assert_parent_rows_follow_children(
		rows: &[super::BranchViewTopologyCommit],
	) {
		let index_by_id = rows
			.iter()
			.enumerate()
			.map(|(index, row)| (row.id, index))
			.collect::<std::collections::HashMap<_, _>>();
		for (index, row) in rows.iter().enumerate() {
			for parent in &row.parents {
				if let Some(parent_index) = index_by_id.get(parent) {
					assert!(
						index < *parent_index,
						"child {} must precede included parent {}",
						row.id,
						parent
					);
				}
			}
		}
	}

	fn cli_parent_map(
		directory: &Path,
		refs: &[&str],
	) -> HashMap<String, Vec<String>> {
		let mut command = Command::new("git");
		command.arg("-C").arg(directory).args([
			"rev-list",
			"--parents",
			"--topo-order",
		]);
		command.args(refs);
		let output = command.output().unwrap();
		assert!(output.status.success());
		String::from_utf8(output.stdout)
			.unwrap()
			.lines()
			.map(|line| {
				let mut ids = line.split_whitespace();
				let id = ids.next().unwrap().to_owned();
				(id, ids.map(str::to_owned).collect::<Vec<_>>())
			})
			.collect()
	}

	fn assert_rows_match_cli_parents(
		rows: &[super::BranchViewTopologyCommit],
		parents_by_id: &HashMap<String, Vec<String>>,
	) {
		for row in rows {
			let expected =
				parents_by_id.get(&row.id.to_string()).unwrap();
			assert_eq!(
				&row.parents
					.iter()
					.map(ToString::to_string)
					.collect::<Vec<_>>(),
				expected
			);
		}
	}

	#[test]
	fn reports_divergence_and_literal_path_from_captured_tips() {
		let dir = tempfile::tempdir().unwrap();
		let repository = Repository::init(dir.path()).unwrap();
		let base = commit(
			&repository,
			"refs/heads/main",
			"base",
			None,
			Some(("base.txt", b"base")),
		);
		let branch = commit(
			&repository,
			"refs/heads/feature/topic-#3",
			"topic change",
			Some(base),
			Some(("space [literal].txt", b"topic")),
		);
		let main = commit(
			&repository,
			"refs/heads/main",
			"main change",
			Some(base),
			Some(("main.txt", b"main")),
		);
		let view = load_branch_view(
			&RepoPath::from(dir.path().to_path_buf()),
			&request(
				"refs/heads/feature/topic-#3",
				Some("refs/heads/main"),
				branch,
				Some(main),
				9,
			),
		)
		.unwrap();
		assert_eq!((view.ahead, view.behind), (Some(1), Some(1)));
		assert_eq!(view.commit_count, Some(1));
		assert_eq!(view.commits[0].summary, "topic change");
		let branch_row = view
			.topology
			.iter()
			.find(|row| row.id == branch.into())
			.unwrap();
		assert!(branch_row
			.roles
			.contains(&super::BranchTopologyRole::BranchTip));
		assert_eq!(branch_row.parents, vec![base.into()]);
		assert!(branch_row.outside_parents.is_empty());
		assert!(view
			.files
			.iter()
			.any(|file| file.path == "space [literal].txt"));
		let diff = crate::sync::diff::get_diff_commits(
			&RepoPath::from(dir.path().to_path_buf()),
			OldNew {
				old: base.into(),
				new: branch.into(),
			},
			"space [literal].txt".to_owned(),
			Some(DiffOptions::default()),
		)
		.unwrap();
		assert!(!diff.hunks.is_empty());
		assert_eq!(view.request.generation, 9);
	}

	#[test]
	fn handles_unrelated_history_without_claiming_a_merge_base() {
		let dir = tempfile::tempdir().unwrap();
		let repository = Repository::init(dir.path()).unwrap();
		let left = commit(
			&repository,
			"refs/heads/left",
			"left root",
			None,
			Some(("left.txt", b"left")),
		);
		let right = commit(
			&repository,
			"refs/heads/right",
			"right root",
			None,
			Some(("right.txt", b"right")),
		);
		let view = load_branch_view(
			&RepoPath::from(dir.path().to_path_buf()),
			&request(
				"refs/heads/left",
				Some("refs/heads/right"),
				left,
				Some(right),
				1,
			),
		)
		.unwrap();
		assert_eq!(view.merge_base, None);
		assert_eq!((view.ahead, view.behind), (Some(1), Some(1)));
		assert_eq!(view.commit_count, Some(1));
		assert_eq!(view.commits[0].summary, "left root");
		assert_eq!(view.topology.len(), 2);
		assert!(view
			.topology
			.iter()
			.all(|row| row.parents.is_empty()));
		assert!(view
			.topology
			.iter()
			.all(|row| row.outside_parents.is_empty()));
		assert!(view.topology.iter().all(|row| !row
			.roles
			.contains(&super::BranchTopologyRole::MergeBase)));
		assert!(view.files.is_empty());
	}

	#[test]
	fn detached_head_is_resolved_as_the_explicit_head_base() {
		let dir = tempfile::tempdir().unwrap();
		let repository = Repository::init(dir.path()).unwrap();
		let base = commit(
			&repository,
			"refs/heads/main",
			"base",
			None,
			Some(("base.txt", b"base")),
		);
		let branch = commit(
			&repository,
			"refs/heads/topic",
			"topic",
			Some(base),
			Some(("topic.txt", b"topic")),
		);
		repository.set_head_detached(base).unwrap();
		let view = load_branch_view(
			&RepoPath::from(dir.path().to_path_buf()),
			&request("refs/heads/topic", None, branch, None, 5),
		)
		.unwrap();
		assert_eq!(view.base, base.into());
		assert_eq!(view.merge_base, Some(base.into()));
		assert_eq!((view.ahead, view.behind), (Some(1), Some(0)));
		let base_row = view
			.topology
			.iter()
			.find(|row| row.id == base.into())
			.unwrap();
		assert!(base_row
			.roles
			.contains(&super::BranchTopologyRole::BaseTip));
		assert_eq!(view.commits[0].summary, "topic");
	}

	#[test]
	fn merge_topology_counts_only_commits_missing_from_base() {
		let dir = tempfile::tempdir().unwrap();
		let repository = Repository::init(dir.path()).unwrap();
		let root = commit(
			&repository,
			"refs/heads/main",
			"root",
			None,
			Some(("root.txt", b"root")),
		);
		let topic = commit(
			&repository,
			"refs/heads/topic-parent",
			"topic-only",
			Some(root),
			Some(("topic.txt", b"topic")),
		);
		let base = commit(
			&repository,
			"refs/heads/main",
			"base-only",
			Some(root),
			Some(("base.txt", b"base")),
		);
		let topic_commit = repository.find_commit(topic).unwrap();
		let base_commit = repository.find_commit(base).unwrap();
		let topic_tree = topic_commit.tree().unwrap();
		let signature =
			Signature::now("test", "test@example.com").unwrap();
		let merge = repository
			.commit(
				Some("refs/heads/topic"),
				&signature,
				&signature,
				"merge base into topic",
				&topic_tree,
				&[&topic_commit, &base_commit],
			)
			.unwrap();
		let view = load_branch_view(
			&RepoPath::from(dir.path().to_path_buf()),
			&request(
				"refs/heads/topic",
				Some("refs/heads/main"),
				merge,
				Some(base),
				6,
			),
		)
		.unwrap();
		assert_eq!(view.merge_base, Some(base.into()));
		assert_eq!((view.ahead, view.behind), (Some(2), Some(0)));
		assert_eq!(view.commit_count, Some(2));
		assert_eq!(view.topology[0].id, merge.into());
		assert_eq!(
			view.topology[0].parents,
			vec![topic.into(), base.into()]
		);
		assert!(view.topology[0].graph.is_merge);
		assert!(view.topology[0]
			.roles
			.contains(&super::BranchTopologyRole::BranchTip));
		assert!(view.topology.iter().any(|row| {
			row.id == base.into()
				&& row
					.roles
					.contains(&super::BranchTopologyRole::BaseTip)
		}));
		assert!(view.topology.iter().any(|row| {
			row.id == base.into()
				&& row
					.roles
					.contains(&super::BranchTopologyRole::MergeBase)
		}));
		let cli_rows = cli_parent_map(
			dir.path(),
			&["refs/heads/topic", "refs/heads/main"],
		);
		assert_eq!(
			view.topology
				.iter()
				.map(|row| row.id.to_string())
				.collect::<HashSet<_>>(),
			cli_rows.keys().cloned().collect::<HashSet<_>>()
		);
		assert_rows_match_cli_parents(&view.topology, &cli_rows);
		assert_parent_rows_follow_children(&view.topology);
		assert!(view
			.commits
			.iter()
			.any(|commit| commit.summary == "topic-only"));
		assert!(view
			.commits
			.iter()
			.any(|commit| commit.summary == "merge base into topic"));
		assert!(!view
			.commits
			.iter()
			.any(|commit| commit.summary == "base-only"));
		assert!(view
			.files
			.iter()
			.any(|file| file.path == "topic.txt"));
	}

	#[test]
	fn bounded_topology_orders_uneven_merge_paths_before_shared_ancestor(
	) {
		let dir = tempfile::tempdir().unwrap();
		let repository = Repository::init(dir.path()).unwrap();
		let root = commit(
			&repository,
			"refs/heads/main",
			"root",
			None,
			Some(("root.txt", b"root")),
		);
		let left = commit(
			&repository,
			"refs/heads/left",
			"left child",
			Some(root),
			Some(("left.txt", b"left")),
		);
		let right_parent = commit(
			&repository,
			"refs/heads/right-parent",
			"right long path",
			Some(root),
			Some(("right.txt", b"right")),
		);
		let right = commit(
			&repository,
			"refs/heads/right",
			"right child",
			Some(right_parent),
			None,
		);
		let left_commit = repository.find_commit(left).unwrap();
		let right_commit = repository.find_commit(right).unwrap();
		let tree = right_commit.tree().unwrap();
		let signature =
			Signature::now("test", "test@example.com").unwrap();
		let merge = repository
			.commit(
				Some("refs/heads/topic"),
				&signature,
				&signature,
				"merge uneven paths",
				&tree,
				&[&left_commit, &right_commit],
			)
			.unwrap();
		let path = RepoPath::from(dir.path().to_path_buf());
		let mut request = request(
			"refs/heads/topic",
			Some("refs/heads/main"),
			merge,
			Some(root),
			17,
		);
		request.topology_limit = 5;
		let partial =
			super::load_topology_snapshot(&path, &request).unwrap();
		let cli_rows = cli_parent_map(
			dir.path(),
			&["refs/heads/topic", "refs/heads/main"],
		);

		assert!(!partial.relationship_loaded);
		assert_eq!(partial.commit_count, None);
		assert_eq!(partial.topology.len(), 5);
		assert_eq!(
			partial
				.topology
				.iter()
				.map(|row| row.id.to_string())
				.collect::<HashSet<_>>(),
			cli_rows.keys().cloned().collect::<HashSet<_>>()
		);
		assert_rows_match_cli_parents(&partial.topology, &cli_rows);
		assert_parent_rows_follow_children(&partial.topology);
		let ids = partial
			.topology
			.iter()
			.map(|row| row.id.get_oid())
			.collect::<Vec<_>>();
		assert_eq!(ids, vec![merge, left, right, right_parent, root]);
	}

	#[test]
	fn topology_snapshot_survives_final_relationship_failure() {
		let dir = tempfile::tempdir().unwrap();
		let repository = Repository::init(dir.path()).unwrap();
		let root = commit(
			&repository,
			"refs/heads/base",
			"base root",
			None,
			Some(("base.txt", b"base")),
		);
		let mut branch_tip = root;
		let mut corrupt_id = None;
		for index in 1..=220 {
			branch_tip = commit(
				&repository,
				"refs/heads/topic",
				&format!("topic {index}"),
				Some(branch_tip),
				None,
			);
			if index == 1 {
				corrupt_id = Some(branch_tip);
			}
		}
		let request = request(
			"refs/heads/topic",
			Some("refs/heads/base"),
			branch_tip,
			Some(root),
			220,
		);
		let path = RepoPath::from(dir.path().to_path_buf());
		let corrupt_id = corrupt_id.unwrap().to_string();
		let object_path = repository
			.path()
			.join("objects")
			.join(&corrupt_id[..2])
			.join(&corrupt_id[2..]);
		drop(repository);
		std::fs::remove_file(&object_path).unwrap();
		std::fs::write(object_path, b"damaged commit object")
			.unwrap();

		let (sender, receiver) = unbounded();
		let async_job = AsyncSingleJob::new(sender);
		let observer = super::BranchViewJob::new(path, request);
		assert!(async_job.spawn(observer));
		assert_eq!(
			receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
			AsyncGitNotification::BranchViewTopology
		);
		assert_eq!(
			receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
			AsyncGitNotification::BranchView
		);
		let completed = async_job.take_last().unwrap();
		let (partial_request, partial) =
			completed.topology_result().unwrap();
		assert_eq!(partial_request.generation, 220);
		assert_eq!(partial.topology.len(), 200);
		assert!(partial.topology_truncated);
		assert!(!partial.relationship_loaded);
		let (_, result) = completed.result().unwrap();
		assert!(result.is_err());
		assert!(completed.topology_result().is_none());
	}

	#[test]
	fn bounded_topology_expands_past_default_window() {
		let dir = tempfile::tempdir().unwrap();
		let repository = Repository::init(dir.path()).unwrap();
		let mut tip = commit(
			&repository,
			"refs/heads/long",
			"root",
			None,
			Some(("history.txt", b"0")),
		);
		let root = tip;
		for index in 1..=205 {
			tip = commit(
				&repository,
				"refs/heads/long",
				&format!("commit {index}"),
				Some(tip),
				None,
			);
		}
		repository
			.reference("refs/heads/base", root, true, "test base")
			.unwrap();
		let path = RepoPath::from(dir.path().to_path_buf());
		let mut bounded_request = request(
			"refs/heads/long",
			Some("refs/heads/base"),
			tip,
			Some(root),
			12,
		);
		let bounded =
			load_branch_view(&path, &bounded_request).unwrap();
		assert!(bounded.topology_truncated);
		assert_eq!(bounded.topology.len(), 200);
		assert_eq!(bounded.commit_count, Some(205));
		assert_eq!(bounded.commits.len(), 200);
		assert_eq!(bounded.merge_base, Some(root.into()));
		let cli_rows = cli_parent_map(
			dir.path(),
			&["refs/heads/long", "refs/heads/base"],
		);
		assert_rows_match_cli_parents(&bounded.topology, &cli_rows);
		assert!(bounded.topology.iter().all(|row| {
			cli_rows.contains_key(&row.id.to_string())
		}));
		assert!(bounded.topology.iter().any(|row| {
			row.id == root.into()
				&& row
					.roles
					.contains(&super::BranchTopologyRole::MergeBase)
		}));
		assert_parent_rows_follow_children(&bounded.topology);
		assert_eq!(bounded.topology[0].id, tip.into());
		let last = bounded.topology.last().unwrap();
		assert_eq!(last.outside_parents.len(), 1);
		assert!(!bounded
			.topology
			.iter()
			.any(|row| row.id == last.outside_parents[0]));

		bounded_request.topology_limit = 250;
		let expanded =
			load_branch_view(&path, &bounded_request).unwrap();
		assert!(!expanded.topology_truncated);
		assert_eq!(expanded.topology.len(), 206);
		assert_eq!(expanded.commits.len(), 205);
		assert_eq!(
			expanded
				.topology
				.iter()
				.map(|row| row.id.to_string())
				.collect::<HashSet<_>>(),
			cli_rows.keys().cloned().collect::<HashSet<_>>()
		);
		assert_rows_match_cli_parents(&expanded.topology, &cli_rows);
		assert_parent_rows_follow_children(&expanded.topology);
		assert!(expanded.topology.iter().any(|row| {
			row.id == root.into()
				&& row
					.roles
					.contains(&super::BranchTopologyRole::MergeBase)
		}));
		assert!(expanded
			.topology
			.iter()
			.all(|row| row.outside_parents.is_empty()));
	}

	#[test]
	fn compares_remote_tracking_refs_as_stable_branch_tips() {
		let dir = tempfile::tempdir().unwrap();
		let repository = Repository::init(dir.path()).unwrap();
		let base = commit(
			&repository,
			"refs/heads/main",
			"base",
			None,
			Some(("base.txt", b"base")),
		);
		let remote = commit(
			&repository,
			"refs/remotes/origin/release/candidate",
			"remote topic",
			Some(base),
			Some(("remote.txt", b"remote")),
		);
		let view = load_branch_view(
			&RepoPath::from(dir.path().to_path_buf()),
			&request(
				"refs/remotes/origin/release/candidate",
				Some("refs/heads/main"),
				remote,
				Some(base),
				3,
			),
		)
		.unwrap();
		assert_eq!(
			view.request.branch_reference,
			"refs/remotes/origin/release/candidate"
		);
		assert_eq!((view.ahead, view.behind), (Some(1), Some(0)));
		assert!(view
			.files
			.iter()
			.any(|file| file.path == "remote.txt"));
	}

	#[test]
	fn unborn_head_and_gone_base_are_errors_instead_of_zero_counts() {
		let dir = tempfile::tempdir().unwrap();
		let repository = Repository::init(dir.path()).unwrap();
		let tip = commit(
			&repository,
			"refs/heads/topic",
			"topic root",
			None,
			Some(("topic.txt", b"topic")),
		);
		repository.set_head("refs/heads/unborn").unwrap();
		let path = RepoPath::from(dir.path().to_path_buf());
		assert!(load_branch_view(
			&path,
			&request("refs/heads/topic", None, tip, None, 1)
		)
		.is_err());
		assert!(load_branch_view(
			&path,
			&request(
				"refs/heads/topic",
				Some("refs/remotes/origin/gone"),
				tip,
				None,
				2
			)
		)
		.is_err());
	}
}
