use crate::{
	error::Result,
	graph::{GraphRow, GraphWalker},
	sync::{
		get_commits_info_from_repo, gix_repo, repo, CommitId,
		CommitInfo, LogWalker, LogWalkerWithoutFilter, RepoPath,
		SharedCommitFilterFn, WalkEntry,
	},
	AsyncGitNotification,
};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use git2::Repository;
use std::{
	collections::{HashMap, HashSet, VecDeque},
	sync::{
		atomic::{AtomicBool, AtomicUsize, Ordering},
		Arc, Mutex,
	},
	thread::{self, JoinHandle},
	time::{Duration, Instant},
};

const DEFAULT_PAGE_SIZE: usize = 128;
const WALK_CHUNK_SIZE: usize = 128;
const MAX_PENDING_INFO_IDS: usize = 128;
const COMMIT_INFO_CACHE_CAPACITY: usize = 512;
const MESSAGE_LENGTH_LIMIT: usize = 4096;
const HEAD_RECHECK_INTERVAL: Duration = Duration::from_secs(5);

///
#[derive(PartialEq, Eq, Debug)]
pub enum FetchStatus {
	/// Previous fetch still running.
	Pending,
	/// No additional range was required.
	NoChange,
	/// A new bounded range was requested.
	Started,
}

/// Snapshot of the lazy log walk. `has_more` remains true until the worker
/// actually reaches EOF; it never requires a full-history count first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogRangeStatus {
	pub generation: u64,
	pub loaded: usize,
	pub pending: bool,
	pub has_more: bool,
	pub complete: bool,
}

///
#[derive(Clone, Debug, Default)]
pub struct AsyncLogResult {
	pub commits: Vec<CommitId>,
	pub duration: Duration,
}

#[derive(Clone)]
struct GraphRequest {
	start: usize,
	len: usize,
	branch_tips: HashSet<CommitId>,
	stashes: HashSet<CommitId>,
	head_id: Option<CommitId>,
}

#[derive(Clone)]
struct GraphResult {
	generation: u64,
	request: GraphRequest,
	rows: Vec<GraphRow>,
}

impl GraphRequest {
	fn matches(&self, other: &Self) -> bool {
		self.start == other.start
			&& self.len == other.len
			&& self.branch_tips == other.branch_tips
			&& self.stashes == other.stashes
			&& self.head_id == other.head_id
	}
}

#[derive(Default)]
struct SharedState {
	result: AsyncLogResult,
	pending_walk_entries: Vec<WalkEntry>,
	graph_request: Option<GraphRequest>,
	graph_result: Option<GraphResult>,
	generation: u64,
	head: Option<CommitId>,
	complete: bool,
	pending: bool,
	walk_inflight: bool,
	delivered: usize,
	error: Option<String>,
	last_error: Option<String>,
	pending_info_ids: Vec<CommitId>,
	info_cache: HashMap<CommitId, CommitInfo>,
	info_lru: VecDeque<CommitId>,
}

#[derive(Clone, Copy)]
enum WorkerCommand {
	Wake,
	Shutdown,
}

enum Walker<'a> {
	Plain(LogWalkerWithoutFilter<'a>),
	Filtered(LogWalker<'a>),
}

impl Walker<'_> {
	fn read_up_to(
		&mut self,
		entries: &mut Vec<WalkEntry>,
		limit: usize,
	) -> Result<usize> {
		match self {
			Self::Plain(walker) => walker.read_up_to(entries, limit),
			Self::Filtered(walker) => {
				walker.read_up_to(entries, limit)
			}
		}
	}
}

/// Persistent, demand-driven commit history and graph cache.
pub struct AsyncLog {
	worker_sender: Sender<WorkerCommand>,
	worker: Mutex<Option<JoinHandle<()>>>,
	state: Arc<Mutex<SharedState>>,
	requested_end: Arc<AtomicUsize>,
	background: Arc<AtomicBool>,
	shutdown: Arc<AtomicBool>,
}

impl AsyncLog {
	/// Create a worker. Repository discovery and all object reads happen on
	/// its dedicated thread, never on the caller/UI thread.
	#[allow(clippy::needless_pass_by_value)] // Ownership moves to the worker thread.
	pub fn new(
		repo: RepoPath,
		sender: &Sender<AsyncGitNotification>,
		filter: Option<SharedCommitFilterFn>,
	) -> Self {
		let (worker_sender, worker_receiver) = bounded(1);
		let state = Arc::new(Mutex::new(SharedState::default()));
		let requested_end = Arc::new(AtomicUsize::new(0));
		let background = Arc::new(AtomicBool::new(false));
		let shutdown = Arc::new(AtomicBool::new(false));
		let worker = {
			let repo_path = repo;
			let state = Arc::clone(&state);
			let requested_end = Arc::clone(&requested_end);
			let background = Arc::clone(&background);
			let shutdown = Arc::clone(&shutdown);
			let notification_sender = sender.clone();
			let filter = filter.clone();
			thread::Builder::new()
				.name("gitui-log-walker".to_string())
				.spawn(move || {
					worker_loop(
						&repo_path,
						filter,
						worker_receiver,
						state,
						requested_end,
						background,
						shutdown,
						notification_sender,
					);
				})
				.ok()
		};
		Self {
			worker_sender,
			worker: Mutex::new(worker),
			state,
			requested_end,
			background,
			shutdown,
		}
	}

	/// Request the half-open commit range beginning at `start`.
	/// The walker advances only as far as the largest requested endpoint.
	pub fn request_range(
		&self,
		start: usize,
		len: usize,
	) -> Result<LogRangeStatus> {
		self.background.store(false, Ordering::Release);
		let end = start.saturating_add(len);
		let previous =
			self.requested_end.fetch_max(end, Ordering::AcqRel);
		let mut state = self.state.lock()?;
		let loaded = state.result.commits.len();
		let needs_walk = end > loaded && !state.complete;
		let missing_ids = state
			.result
			.commits
			.iter()
			.skip(start.min(loaded))
			.take(end.saturating_sub(start))
			.filter(|id| !state.info_cache.contains_key(id))
			.take(
				MAX_PENDING_INFO_IDS
					.saturating_sub(state.pending_info_ids.len()),
			)
			.copied()
			.collect::<Vec<_>>();
		let has_missing_metadata = !missing_ids.is_empty();
		for id in missing_ids {
			if !state.pending_info_ids.contains(&id) {
				state.pending_info_ids.push(id);
			}
		}
		if needs_walk || has_missing_metadata {
			state.pending = true;
		}
		let should_wake =
			end > previous || needs_walk || has_missing_metadata;
		let status = status(&state);
		drop(state);
		if should_wake {
			self.wake();
		}
		Ok(status)
	}

	/// Compatibility helper for callers that request a loaded prefix.
	pub fn ensure_loaded(
		&self,
		end_exclusive: usize,
	) -> Result<LogRangeStatus> {
		self.request_range(0, end_exclusive)
	}

	/// Stop any unbounded history demand after a search is cancelled.
	/// The current page and pending viewport work remain available.
	pub fn cancel_pending_walk(&self) -> Result<()> {
		self.background.store(false, Ordering::Release);
		let mut state = self.state.lock()?;
		let graph_end =
			state.graph_request.as_ref().map_or(0, |request| {
				request.start.saturating_add(request.len)
			});
		let end = state
			.result
			.commits
			.len()
			.max(DEFAULT_PAGE_SIZE)
			.max(graph_end);
		self.requested_end.store(end, Ordering::Release);
		state.pending = state.walk_inflight
			|| (!state.complete && state.result.commits.len() < end)
			|| !state.pending_info_ids.is_empty()
			|| !state.pending_walk_entries.is_empty()
			|| state.graph_request.is_some();
		drop(state);
		self.wake();
		Ok(())
	}

	/// Current cache and worker state. This is a memory-only operation.
	pub fn range_status(&self) -> Result<LogRangeStatus> {
		let state = self.state.lock()?;
		Ok(status(&state))
	}

	/// Current history generation, incremented whenever the worker observes
	/// a different HEAD.
	pub fn current_generation(&self) -> Result<u64> {
		Ok(self.state.lock()?.generation)
	}

	/// Request metadata for exact commit IDs (including IDs from file-history
	/// or a filtered list that are not in the main HEAD walk).
	pub fn request_commit_info_ids(
		&self,
		ids: Vec<CommitId>,
	) -> Result<()> {
		self.background.store(false, Ordering::Release);
		let mut state = self.state.lock()?;
		for id in ids {
			if state.pending_info_ids.len() >= MAX_PENDING_INFO_IDS {
				break;
			}
			if !state.info_cache.contains_key(&id)
				&& !state.pending_info_ids.contains(&id)
			{
				state.pending_info_ids.push(id);
			}
		}
		let should_wake = !state.pending_info_ids.is_empty();
		if should_wake {
			state.pending = true;
		}
		drop(state);
		if should_wake {
			self.wake();
		}
		Ok(())
	}

	/// Return metadata only when every requested exact ID is cached. Results
	/// preserve the caller's order; this method never opens a repository.
	pub fn get_commit_info_range(
		&self,
		ids: &[CommitId],
	) -> Option<Vec<CommitInfo>> {
		let mut state = self.state.lock().ok()?;
		let result = ids
			.iter()
			.map(|id| state.info_cache.get(id).cloned())
			.collect::<Option<Vec<_>>>()?;
		for id in ids {
			touch_info_lru(&mut state, *id);
		}
		Some(result)
	}

	/// Queue a graph calculation for an immutable viewport snapshot.
	pub fn request_graph_range(
		&self,
		start: usize,
		len: usize,
		branch_tips: HashSet<CommitId>,
		stashes: HashSet<CommitId>,
		head_id: Option<CommitId>,
	) -> Result<()> {
		self.background.store(false, Ordering::Release);
		let request = GraphRequest {
			start,
			len,
			branch_tips,
			stashes,
			head_id,
		};
		let end = start.saturating_add(len);
		self.requested_end.fetch_max(end, Ordering::AcqRel);
		let mut state = self.state.lock()?;
		state.graph_request = Some(request);
		state.graph_result = None;
		state.pending = true;
		drop(state);
		self.wake();
		Ok(())
	}

	/// Cache-only graph lookup. The old signature remains as a transition
	/// helper, but it queues work instead of decompressing a long prefix here.
	pub fn get_graph_rows(
		&self,
		commit_slice: &[CommitId],
		global_start: usize,
		branch_tips: &HashSet<CommitId>,
		stashes: &HashSet<CommitId>,
		head_id: Option<&CommitId>,
	) -> Option<Vec<GraphRow>> {
		let request = GraphRequest {
			start: global_start,
			len: commit_slice.len(),
			branch_tips: branch_tips.clone(),
			stashes: stashes.clone(),
			head_id: head_id.copied(),
		};
		let result = self.state.lock().ok()?.graph_result.clone();
		if let Some(result) = result {
			if result.generation == self.current_generation().ok()?
				&& result.request.matches(&request)
			{
				return Some(result.rows);
			}
		}
		let _ = self.request_graph_range(
			request.start,
			request.len,
			request.branch_tips,
			request.stashes,
			request.head_id,
		);
		None
	}

	/// Cache-only graph lookup for clients already using `request_graph_range`.
	pub fn get_cached_graph_rows(
		&self,
		start: usize,
		len: usize,
	) -> Option<Vec<GraphRow>> {
		let state = self.state.lock().ok()?;
		let result = state.graph_result.as_ref()?;
		(result.generation == state.generation
			&& result.request.start == start
			&& result.request.len == len)
			.then(|| result.rows.clone())
	}

	/// Loaded commit count; it is not a total-history count until `complete`.
	pub fn count(&self) -> Result<usize> {
		Ok(self.state.lock()?.result.commits.len())
	}

	/// IDs currently available in the requested prefix.
	pub fn get_slice(
		&self,
		start_index: usize,
		amount: usize,
	) -> Result<Vec<CommitId>> {
		let state = self.state.lock()?;
		let list = &state.result.commits;
		let min = start_index.min(list.len());
		let max = min.saturating_add(amount).min(list.len());
		Ok(list[min..max].to_vec())
	}

	/// IDs currently loaded. Call `request_range` to extend the prefix.
	pub fn get_items(&self) -> Result<Vec<CommitId>> {
		Ok(self.state.lock()?.result.commits.clone())
	}

	/// Compatibility for append-style consumers: return newly loaded IDs.
	pub fn extract_items(&self) -> Result<Vec<CommitId>> {
		let mut state = self.state.lock()?;
		let start = state.delivered.min(state.result.commits.len());
		let result = state.result.commits[start..].to_vec();
		state.delivered = state.result.commits.len();
		Ok(result)
	}

	/// Duration spent walking the most recently published page.
	pub fn get_last_duration(&self) -> Result<Duration> {
		Ok(self.state.lock()?.result.duration)
	}

	pub fn is_pending(&self) -> bool {
		self.state.lock().is_ok_and(|state| state.pending)
	}

	/// Take the most recent background worker error for display.
	pub fn take_error(&self) -> Result<Option<String>> {
		Ok(self.state.lock()?.error.take())
	}

	/// Called while the log is hidden. Pending work is retained as demand,
	/// while the worker stops consuming commits until the UI wakes it again.
	pub fn set_background(&self) {
		self.background.store(true, Ordering::Release);
		self.wake();
	}

	pub fn fetch(&self) -> Result<FetchStatus> {
		self.background.store(false, Ordering::Release);
		let was_pending = self.is_pending();
		let status = self.request_range(0, DEFAULT_PAGE_SIZE)?;
		if was_pending {
			Ok(FetchStatus::Pending)
		} else if status.loaded >= DEFAULT_PAGE_SIZE
			|| status.complete
		{
			Ok(FetchStatus::NoChange)
		} else {
			Ok(FetchStatus::Started)
		}
	}

	fn wake(&self) {
		match self.worker_sender.try_send(WorkerCommand::Wake) {
			Ok(()) | Err(TrySendError::Full(_)) => {}
			Err(TrySendError::Disconnected(_)) => {
				if let Ok(mut state) = self.state.lock() {
					state.pending = false;
					state.error =
						Some("log worker is unavailable".to_string());
				}
			}
		}
	}
}

impl Drop for AsyncLog {
	fn drop(&mut self) {
		self.shutdown.store(true, Ordering::Release);
		let _ = self.worker_sender.try_send(WorkerCommand::Shutdown);
		if let Ok(worker) = self.worker.get_mut() {
			let _ = worker.take();
		}
	}
}

const fn status(state: &SharedState) -> LogRangeStatus {
	LogRangeStatus {
		generation: state.generation,
		loaded: state.result.commits.len(),
		pending: state.pending,
		has_more: !state.complete,
		complete: state.complete,
	}
}

fn touch_info_lru(state: &mut SharedState, id: CommitId) {
	if let Some(index) =
		state.info_lru.iter().position(|cached| *cached == id)
	{
		state.info_lru.remove(index);
	}
	state.info_lru.push_back(id);
}

fn cache_info(state: &mut SharedState, info: CommitInfo) {
	let id = info.id;
	state.info_cache.insert(id, info);
	touch_info_lru(state, id);
	while state.info_lru.len() > COMMIT_INFO_CACHE_CAPACITY {
		if let Some(expired) = state.info_lru.pop_front() {
			state.info_cache.remove(&expired);
		}
	}
}

#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
fn worker_loop(
	repo_path: &RepoPath,
	filter: Option<SharedCommitFilterFn>,
	receiver: Receiver<WorkerCommand>,
	state: Arc<Mutex<SharedState>>,
	requested_end: Arc<AtomicUsize>,
	background: Arc<AtomicBool>,
	shutdown: Arc<AtomicBool>,
	notification_sender: Sender<AsyncGitNotification>,
) {
	loop {
		if shutdown.load(Ordering::Acquire) {
			return;
		}
		match receiver.recv() {
			Ok(WorkerCommand::Wake) => {}
			Ok(WorkerCommand::Shutdown) | Err(_) => return,
		}
		loop {
			match run_session(
				repo_path,
				filter.as_ref(),
				&receiver,
				&state,
				&requested_end,
				&background,
				&shutdown,
				&notification_sender,
			) {
				Ok(
					SessionExit::HeadChanged | SessionExit::Restart,
				) => {}
				Ok(SessionExit::Shutdown) => return,
				Err(error) => {
					record_worker_error(error.to_string(), &state);
					if !notify(&notification_sender, &state) {
						return;
					}
					break;
				}
			}
		}
	}
}

enum SessionExit {
	HeadChanged,
	Restart,
	Shutdown,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_session(
	repo_path: &RepoPath,
	filter: Option<&SharedCommitFilterFn>,
	receiver: &Receiver<WorkerCommand>,
	state: &Mutex<SharedState>,
	requested_end: &AtomicUsize,
	background: &AtomicBool,
	shutdown: &AtomicBool,
	notification_sender: &Sender<AsyncGitNotification>,
) -> Result<SessionExit> {
	let repository = repo(repo_path)?;
	let session_head = read_head(&repository);
	let mut graph_timing = Instant::now();
	let mut gix_repository = if filter.is_none() {
		Some(gix_repo(repo_path)?)
	} else {
		None
	};
	let mut walker =
		match (filter, gix_repository.as_mut(), session_head) {
			(Some(filter), _, Some(_)) => Some(Walker::Filtered(
				LogWalker::new(&repository, WALK_CHUNK_SIZE)?
					.filter(Some(Arc::clone(filter))),
			)),
			(None, Some(gix), Some(_)) => Some(Walker::Plain(
				LogWalkerWithoutFilter::new(gix, WALK_CHUNK_SIZE)?,
			)),
			(None, None, Some(_)) | (_, _, None) => None,
		};
	let generation_changed = {
		let mut current = state.lock()?;
		if current.generation == 0 || current.head != session_head {
			let had_generation = current.generation != 0;
			current.generation = current.generation.saturating_add(1);
			current.head = session_head;
			current.result = AsyncLogResult::default();
			current.pending_walk_entries.clear();
			current.walk_inflight = false;
			current.graph_request = None;
			current.graph_result = None;
			current.delivered = 0;
			current.complete = walker.is_none();
			if had_generation {
				requested_end
					.store(DEFAULT_PAGE_SIZE, Ordering::Release);
			}
			true
		} else {
			false
		}
	};
	if generation_changed && !notify(notification_sender, state) {
		return Ok(SessionExit::Shutdown);
	}
	let mut graph_walker = GraphWalker::new();

	'session: loop {
		if shutdown.load(Ordering::Acquire) {
			return Ok(SessionExit::Shutdown);
		}
		match receiver.try_recv() {
			Ok(WorkerCommand::Shutdown) => {
				return Ok(SessionExit::Shutdown);
			}
			Ok(WorkerCommand::Wake) => {
				if read_head(&repository) != session_head {
					return Ok(SessionExit::HeadChanged);
				}
			}
			Err(_) => {}
		}
		let (
			requested,
			loaded,
			pending_info_ids,
			pending_walk_entries,
			graph_request,
			is_complete,
		) = {
			let current = state.lock()?;
			(
				requested_end.load(Ordering::Acquire),
				current.result.commits.len(),
				current.pending_info_ids.clone(),
				current.pending_walk_entries.clone(),
				current.graph_request.clone(),
				current.complete,
			)
		};

		if background.load(Ordering::Acquire) {
			let mut current = state.lock()?;
			let was_pending = current.pending;
			current.pending = current.walk_inflight
				|| !current.pending_info_ids.is_empty()
				|| !current.pending_walk_entries.is_empty();
			let settled = was_pending && !current.pending;
			drop(current);
			if settled && !notify(notification_sender, state) {
				return Ok(SessionExit::Shutdown);
			}
			match receiver.recv_timeout(HEAD_RECHECK_INTERVAL) {
				Ok(WorkerCommand::Shutdown)
				| Err(
					crossbeam_channel::RecvTimeoutError::Disconnected,
				) => {
					return Ok(SessionExit::Shutdown);
				}
				Ok(WorkerCommand::Wake) => continue,
				Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
					if read_head(&repository) != session_head {
						return Ok(SessionExit::HeadChanged);
					}
				}
			}
			continue;
		}

		let need_page_metadata = !pending_walk_entries.is_empty();
		let need_walk =
			!need_page_metadata && !is_complete && loaded < requested;
		let need_metadata = !pending_info_ids.is_empty();
		let need_graph = if filter.is_some() {
			false
		} else if let Some(request) = graph_request.as_ref() {
			if request.start.saturating_add(request.len) <= loaded {
				let current = state.lock()?;
				!current.graph_result.as_ref().is_some_and(|result| {
					result.generation == current.generation
						&& result.request.matches(request)
				})
			} else {
				false
			}
		} else {
			false
		};

		if !need_walk
			&& !need_page_metadata
			&& !need_metadata
			&& !need_graph
		{
			let mut current = state.lock()?;
			let was_pending = current.pending;
			let latest_end = requested_end.load(Ordering::Acquire);
			let walk_pending = !current.complete
				&& current.result.commits.len() < latest_end;
			let graph_pending = filter.is_none()
				&& current.graph_request.as_ref().is_some_and(
					|request| {
						request.start.saturating_add(request.len)
							<= current.result.commits.len()
							&& !current
								.graph_result
								.as_ref()
								.is_some_and(|result| {
									result.generation
										== current.generation && result
										.request
										.matches(request)
								})
					},
				);
			current.pending = current.walk_inflight
				|| walk_pending
				|| !current.pending_info_ids.is_empty()
				|| !current.pending_walk_entries.is_empty()
				|| graph_pending;
			let settled = was_pending && !current.pending;
			drop(current);
			if settled && !notify(notification_sender, state) {
				return Ok(SessionExit::Shutdown);
			}
			match receiver.recv_timeout(HEAD_RECHECK_INTERVAL) {
				Ok(WorkerCommand::Shutdown)
				| Err(
					crossbeam_channel::RecvTimeoutError::Disconnected,
				) => return Ok(SessionExit::Shutdown),
				Ok(WorkerCommand::Wake)
				| Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
					if read_head(&repository) != session_head {
						return Ok(SessionExit::HeadChanged);
					}
				}
			}
			continue;
		}

		if need_walk || need_page_metadata {
			let mut entries = if need_page_metadata {
				pending_walk_entries
			} else {
				Vec::with_capacity(
					requested
						.saturating_sub(loaded)
						.min(WALK_CHUNK_SIZE),
				)
			};
			if need_walk {
				// Demand may have been cancelled after the loop snapshot. Recheck
				// it while holding the same lock used by cancellation, and mark
				// this bounded page in flight before releasing that lock.
				let amount = {
					let mut current = state.lock()?;
					let current_loaded = current.result.commits.len();
					let current_requested =
						requested_end.load(Ordering::Acquire);
					if current.complete
						|| current_loaded >= current_requested
						|| background.load(Ordering::Acquire)
					{
						0
					} else {
						current.walk_inflight = true;
						current.pending = true;
						current_requested
							.saturating_sub(current_loaded)
							.min(WALK_CHUNK_SIZE)
					}
				};
				if amount == 0 {
					continue 'session;
				}
				let visited = match walker
					.as_mut()
					.map_or(Ok(0), |walker| {
						walker.read_up_to(&mut entries, amount)
					}) {
					Ok(visited) => visited,
					Err(error) => {
						reset_after_walk_error(state, session_head)?;
						record_worker_error(error.to_string(), state);
						if !notify(notification_sender, state) {
							return Ok(SessionExit::Shutdown);
						}
						match receiver.recv() {
							Ok(WorkerCommand::Wake) => {
								return Ok(SessionExit::Restart);
							}
							Ok(WorkerCommand::Shutdown) | Err(_) => {
								return Ok(SessionExit::Shutdown);
							}
						}
					}
				};
				if visited == 0 {
					let mut current = state.lock()?;
					current.complete = true;
					current.walk_inflight = false;
					continue;
				}
				if entries.is_empty() && filter.is_some() {
					// A filtered walker may visit excluded commits while producing
					// no visible rows; continue within current demand.
					state.lock()?.walk_inflight = false;
					continue;
				}
			}
			{
				let ids = entries
					.iter()
					.map(|(id, _)| *id)
					.collect::<Vec<_>>();
				let metadata = match get_commits_info_from_repo(
					&repository,
					&ids,
					MESSAGE_LENGTH_LIMIT,
				) {
					Ok(metadata) => metadata,
					Err(error) => {
						if let Ok(mut current) = state.lock() {
							current.pending_walk_entries = entries;
							current.walk_inflight = false;
						}
						record_worker_error(error.to_string(), state);
						if !notify(notification_sender, state) {
							return Ok(SessionExit::Shutdown);
						}
						match receiver.recv() {
							Ok(WorkerCommand::Wake) => {
								continue 'session;
							}
							Ok(WorkerCommand::Shutdown) | Err(_) => {
								return Ok(SessionExit::Shutdown);
							}
						}
					}
				};
				if filter.is_none() {
					for (id, parents) in &entries {
						graph_walker.process(*id, parents);
					}
				}
				let mut current = state.lock()?;
				current.pending_walk_entries.clear();
				current.walk_inflight = false;
				current.result.commits.extend(ids);
				for info in metadata {
					cache_info(&mut current, info);
				}
				current.result.duration = graph_timing.elapsed();
				graph_timing = Instant::now();
				current.last_error = None;
				current.error = None;
			}
			if !notify(notification_sender, state) {
				return Ok(SessionExit::Shutdown);
			}
		}

		if need_metadata {
			let ids = pending_info_ids
				.into_iter()
				.filter(|id| {
					state.lock().is_ok_and(|current| {
						!current.info_cache.contains_key(id)
					})
				})
				.collect::<Vec<_>>();
			if !ids.is_empty() {
				let metadata = match get_commits_info_from_repo(
					&repository,
					&ids,
					MESSAGE_LENGTH_LIMIT,
				) {
					Ok(metadata) => metadata,
					Err(error) => {
						record_worker_error(error.to_string(), state);
						if !notify(notification_sender, state) {
							return Ok(SessionExit::Shutdown);
						}
						match receiver.recv() {
							Ok(WorkerCommand::Wake) => {
								continue 'session;
							}
							Ok(WorkerCommand::Shutdown) | Err(_) => {
								return Ok(SessionExit::Shutdown);
							}
						}
					}
				};
				let mut current = state.lock()?;
				for info in metadata {
					cache_info(&mut current, info);
				}
				current
					.pending_info_ids
					.retain(|id| !ids.contains(id));
				current.last_error = None;
				current.error = None;
			}
			if !notify(notification_sender, state) {
				return Ok(SessionExit::Shutdown);
			}
		}

		if need_graph {
			let Some(request) = graph_request else {
				continue;
			};
			let end = request.start.saturating_add(request.len);
			if filter.is_some() {
				continue;
			}
			let (generation, commits) = {
				let current = state.lock()?;
				if end > current.result.commits.len() {
					continue;
				}
				(
					current.generation,
					current.result.commits[request.start..end]
						.to_vec(),
				)
			};
			let rows = graph_walker.compute_rows(
				&commits,
				request.start,
				&request.branch_tips,
				&request.stashes,
				request.head_id.as_ref(),
			);
			let mut current = state.lock()?;
			if current.generation == generation
				&& current
					.graph_request
					.as_ref()
					.is_some_and(|latest| latest.matches(&request))
			{
				current.graph_result = Some(GraphResult {
					generation,
					request,
					rows,
				});
			}
			drop(current);
			if !notify(notification_sender, state) {
				return Ok(SessionExit::Shutdown);
			}
		}
	}
}

fn reset_after_walk_error(
	state: &Mutex<SharedState>,
	head: Option<CommitId>,
) -> Result<()> {
	let mut current = state.lock()?;
	current.generation = current.generation.saturating_add(1);
	current.head = head;
	current.result = AsyncLogResult::default();
	current.pending_walk_entries.clear();
	current.walk_inflight = false;
	current.graph_result = None;
	current.delivered = 0;
	current.complete = head.is_none();
	current.pending = false;
	current.error = None;
	current.last_error = None;
	Ok(())
}

fn read_head(repository: &Repository) -> Option<CommitId> {
	repository
		.head()
		.ok()
		.and_then(|head| head.peel_to_commit().ok())
		.map(|commit| CommitId::new(commit.id()))
}

fn record_worker_error(error: String, state: &Mutex<SharedState>) {
	if let Ok(mut current) = state.lock() {
		if current.last_error.as_ref() != Some(&error) {
			current.last_error = Some(error.clone());
			current.error = Some(error);
		}
		current.pending = false;
		current.walk_inflight = false;
	}
}

fn notify(
	sender: &Sender<AsyncGitNotification>,
	state: &Mutex<SharedState>,
) -> bool {
	if let Err(error) = sender.send(AsyncGitNotification::Log) {
		log::trace!("revlog notification receiver closed: {error}");
		if let Ok(mut current) = state.lock() {
			current.pending = false;
			current.walk_inflight = false;
		}
		false
	} else {
		true
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sync::{
		commit, stage_add_file, tests::repo_init, RepoPath,
	};
	use crossbeam_channel::unbounded;
	use std::{
		fs::{self, File},
		io::Write,
		path::Path,
	};
	use tempfile::TempDir;

	#[test]
	fn requested_ranges_are_demand_driven_and_metadata_is_cached() {
		let (_td, repository) = repo_init().unwrap();
		let root = repository.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();
		for name in ["one", "two", "three"] {
			File::create(root.join("file"))
				.unwrap()
				.write_all(name.as_bytes())
				.unwrap();
			stage_add_file(&repo_path, Path::new("file")).unwrap();
			commit(&repo_path, name).unwrap();
		}
		let (sender, receiver) = unbounded();
		let log = AsyncLog::new(repo_path, &sender, None);
		log.request_range(0, 2).unwrap();
		wait_until(&log, &receiver, |status| status.loaded >= 2);
		let status = log.range_status().unwrap();
		assert_eq!(status.loaded, 2);
		assert!(!status.complete);
		let ids = log.get_slice(0, 2).unwrap();
		assert_eq!(log.get_commit_info_range(&ids).unwrap().len(), 2);
		log.request_range(2, 3).unwrap();
		wait_until(&log, &receiver, |status| status.complete);
		assert_eq!(log.range_status().unwrap().loaded, 4);
	}

	#[test]
	fn settled_log_status_is_notified_after_eof() {
		let (_td, repository) = repo_init().unwrap();
		let root = repository.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();
		let (sender, receiver) = unbounded();
		let log = AsyncLog::new(repo_path, &sender, None);
		log.request_range(0, usize::MAX).unwrap();

		let deadline = Instant::now() + Duration::from_secs(5);
		loop {
			let remaining =
				deadline.saturating_duration_since(Instant::now());
			assert!(
				!remaining.is_zero(),
				"worker settled without notifying the receiver"
			);
			receiver
				.recv_timeout(remaining)
				.expect("worker did not send a log notification");
			let status = log.range_status().unwrap();
			if status.complete && !status.pending {
				break;
			}
		}
		assert!(log.range_status().unwrap().complete);
	}

	#[test]
	fn unborn_repository_reaches_eof_without_a_walk() {
		let (_td, repository) =
			crate::sync::tests::repo_init_empty().unwrap();
		let root = repository.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();
		let (sender, _receiver) = unbounded();
		let log = AsyncLog::new(repo_path, &sender, None);
		log.request_range(0, 10).unwrap();
		let deadline = Instant::now() + Duration::from_secs(5);
		while !log.range_status().unwrap().complete {
			assert!(
				Instant::now() < deadline,
				"worker did not settle"
			);
			thread::yield_now();
		}
		assert_eq!(log.count().unwrap(), 0);
	}

	#[test]
	fn info_request_can_load_ids_outside_head_walk() {
		let (_td, repository) = repo_init().unwrap();
		let root = repository.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();
		let id = CommitId::new(
			repository.head().unwrap().target().unwrap(),
		);
		let (sender, receiver) = unbounded();
		let log = AsyncLog::new(repo_path, &sender, None);
		log.request_commit_info_ids(vec![id]).unwrap();
		wait_until(&log, &receiver, |_| {
			log.get_commit_info_range(&[id]).is_some()
		});
		assert_eq!(log.get_slice(0, 10).unwrap().len(), 0);
	}

	#[test]
	fn hiding_and_showing_resumes_the_same_walk_cursor() {
		let (_td, repository) = repo_init().unwrap();
		let root = repository.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();
		for index in 0..4 {
			File::create(root.join("file"))
				.unwrap()
				.write_all(index.to_string().as_bytes())
				.unwrap();
			stage_add_file(&repo_path, Path::new("file")).unwrap();
			commit(&repo_path, &format!("commit-{index}")).unwrap();
		}
		let (sender, receiver) = unbounded();
		let log = AsyncLog::new(repo_path, &sender, None);
		log.request_range(0, 2).unwrap();
		wait_until(&log, &receiver, |status| status.loaded >= 2);
		let generation = log.current_generation().unwrap();
		log.set_background();
		let deadline = Instant::now() + Duration::from_secs(5);
		while log.is_pending() {
			assert!(
				Instant::now() < deadline,
				"worker did not pause"
			);
			thread::yield_now();
		}
		log.request_range(2, 4).unwrap();
		wait_until(&log, &receiver, |status| status.complete);
		let ids = log.get_items().unwrap();
		assert_eq!(ids.len(), 5);
		assert_eq!(
			ids.iter().copied().collect::<HashSet<_>>().len(),
			5
		);
		assert_eq!(log.current_generation().unwrap(), generation);
	}

	#[test]
	fn head_change_resets_loaded_ids_and_increments_generation() {
		let (_td, repository) = repo_init().unwrap();
		let root = repository.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();
		let (sender, receiver) = unbounded();
		let log = AsyncLog::new(repo_path.clone(), &sender, None);
		log.request_range(0, 1).unwrap();
		wait_until(&log, &receiver, |status| status.loaded >= 1);
		let old_generation = log.current_generation().unwrap();
		File::create(root.join("later"))
			.unwrap()
			.write_all(b"later")
			.unwrap();
		stage_add_file(&repo_path, Path::new("later")).unwrap();
		let new_id = commit(&repo_path, "later").unwrap();
		log.request_range(0, 2).unwrap();
		wait_until(&log, &receiver, |status| {
			status.generation > old_generation && status.loaded >= 2
		});
		let ids = log.get_items().unwrap();
		assert_eq!(ids[0], new_id.into());
		assert_eq!(
			ids.iter().copied().collect::<HashSet<_>>().len(),
			2
		);
	}

	fn wait_until(
		log: &AsyncLog,
		receiver: &crossbeam_channel::Receiver<AsyncGitNotification>,
		mut done: impl FnMut(LogRangeStatus) -> bool,
	) {
		let deadline = Instant::now() + Duration::from_secs(5);
		loop {
			if done(log.range_status().unwrap()) {
				return;
			}
			let remaining =
				deadline.saturating_duration_since(Instant::now());
			assert!(
				!remaining.is_zero(),
				"worker timed out: status={:?}, error={:?}",
				log.range_status(),
				log.take_error()
			);
			let _ = receiver.recv_timeout(remaining);
		}
	}

	#[test]
	fn no_repo_open_occurs_during_memory_only_reads() {
		let (_td, repository) = repo_init().unwrap();
		let root = repository.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();
		let (sender, _receiver) = unbounded();
		let log = AsyncLog::new(repo_path, &sender, None);
		assert!(log.get_commit_info_range(&[]).unwrap().is_empty());
		assert!(log
			.get_graph_rows(
				&[],
				0,
				&HashSet::new(),
				&HashSet::new(),
				None
			)
			.is_none());
	}

	#[test]
	fn missing_repository_error_is_retryable() {
		let temporary = TempDir::new().unwrap();
		let repo_path: RepoPath =
			temporary.path().to_str().unwrap().into();
		let (sender, receiver) = unbounded();
		let log = AsyncLog::new(repo_path, &sender, None);
		log.request_range(0, 1).unwrap();
		let deadline = Instant::now() + Duration::from_secs(5);
		while log.is_pending() {
			assert!(Instant::now() < deadline, "worker did not stop");
			let _ = receiver.recv_timeout(Duration::from_millis(50));
		}
		assert!(log.take_error().unwrap().is_some());
		log.request_range(0, 1).unwrap();
	}

	#[test]
	fn closed_notification_receiver_stops_worker_and_clears_pending()
	{
		let (_td, repository) = repo_init().unwrap();
		let root = repository.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();
		let (sender, receiver) = unbounded();
		let log = AsyncLog::new(repo_path, &sender, None);
		drop(receiver);
		log.request_range(0, 1).unwrap();
		let deadline = Instant::now() + Duration::from_secs(5);
		while log.is_pending() {
			assert!(Instant::now() < deadline, "worker did not stop");
			thread::yield_now();
		}
		assert_eq!(log.range_status().unwrap().loaded, 0);
	}

	#[test]
	fn evicted_metadata_is_rehydrated_for_exact_ids() {
		let (_td, repository) = repo_init().unwrap();
		let root = repository.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();
		for index in 0..COMMIT_INFO_CACHE_CAPACITY + 1 {
			File::create(root.join("file"))
				.unwrap()
				.write_all(index.to_string().as_bytes())
				.unwrap();
			stage_add_file(&repo_path, Path::new("file")).unwrap();
			commit(&repo_path, &format!("commit-{index}")).unwrap();
		}
		let (sender, receiver) = unbounded();
		let log = AsyncLog::new(repo_path, &sender, None);
		log.request_range(0, usize::MAX).unwrap();
		wait_until(&log, &receiver, |status| status.loaded >= 128);
		log.cancel_pending_walk().unwrap();
		let loaded_at_cancel = log.count().unwrap();
		wait_until(&log, &receiver, |status| !status.pending);
		let loaded_after_cancel = log.count().unwrap();
		assert!(
			loaded_after_cancel <= loaded_at_cancel + WALK_CHUNK_SIZE
		);
		assert_eq!(log.count().unwrap(), loaded_after_cancel);
		let cancelled_status = log.range_status().unwrap();
		assert_eq!(cancelled_status.loaded, loaded_after_cancel);
		assert_eq!(
			cancelled_status.complete,
			!cancelled_status.has_more
		);
		log.request_range(0, COMMIT_INFO_CACHE_CAPACITY + 3)
			.unwrap();
		wait_until(&log, &receiver, |status| status.complete);
		let ids = log.get_items().unwrap();
		let first_id = ids[0];
		assert!(log.get_commit_info_range(&[first_id]).is_none());
		log.request_range(0, usize::MAX).unwrap();
		let queued_ids =
			log.state.lock().unwrap().pending_info_ids.len();
		assert!(queued_ids <= MAX_PENDING_INFO_IDS);
		log.request_commit_info_ids(vec![first_id]).unwrap();
		wait_until(&log, &receiver, |_| {
			log.get_commit_info_range(&[first_id]).is_some()
		});
		assert_eq!(log.get_items().unwrap(), ids);
	}

	#[test]
	fn walk_error_restarts_generation_without_skipping_or_duplicating(
	) {
		let (_td, repository) = repo_init().unwrap();
		let root = repository.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();
		for index in 0..2 {
			File::create(root.join("file"))
				.unwrap()
				.write_all(index.to_string().as_bytes())
				.unwrap();
			stage_add_file(&repo_path, Path::new("file")).unwrap();
			commit(&repo_path, &format!("commit-{index}")).unwrap();
		}
		let head =
			repository.head().unwrap().peel_to_commit().unwrap();
		let expected_ids = vec![
			head.id().into(),
			head.parent(0).unwrap().id().into(),
			head.parent(0).unwrap().parent(0).unwrap().id().into(),
		];
		let oldest = expected_ids[2];
		let object_id = git2::Oid::from(oldest).to_string();
		let object_path = repository
			.path()
			.join("objects")
			.join(&object_id[..2])
			.join(&object_id[2..]);
		let original_object = fs::read(&object_path).unwrap();
		let original_permissions =
			fs::metadata(&object_path).unwrap().permissions();
		let mut writable_permissions = original_permissions.clone();
		writable_permissions.set_readonly(false);
		fs::set_permissions(&object_path, writable_permissions)
			.unwrap();
		fs::write(&object_path, b"corrupt object").unwrap();
		let (sender, receiver) = unbounded();
		let log = AsyncLog::new(repo_path, &sender, None);
		let initial_generation = log.current_generation().unwrap();
		log.request_range(0, 3).unwrap();
		let deadline = Instant::now() + Duration::from_secs(5);
		loop {
			let status = log.range_status().unwrap();
			if status.generation >= initial_generation + 2
				&& !status.pending
			{
				break;
			}
			assert!(
				Instant::now() < deadline,
				"walker did not reset"
			);
			let _ = receiver.recv_timeout(Duration::from_millis(50));
		}
		assert!(log.take_error().unwrap().is_some());
		fs::write(&object_path, original_object).unwrap();
		fs::set_permissions(&object_path, original_permissions)
			.unwrap();
		log.request_range(0, 3).unwrap();
		wait_until(&log, &receiver, |status| {
			status.generation >= initial_generation + 2
				&& status.loaded >= 3
		});
		let actual_ids = log.get_items().unwrap();
		assert_eq!(
			actual_ids.iter().copied().collect::<HashSet<_>>(),
			expected_ids.iter().copied().collect::<HashSet<_>>()
		);
		assert_eq!(
			actual_ids.iter().copied().collect::<HashSet<_>>().len(),
			3
		);
	}
}
