use crate::{
	error::Result,
	hash,
	sync::{
		self, status::StatusType, RepoPath, ShowUntrackedFilesConfig,
	},
	AsyncGitNotification, StatusItem,
};
use crossbeam_channel::Sender;
use std::{
	hash::Hash,
	path::Path,
	sync::{
		atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
		Arc, Mutex,
	},
};

#[derive(Default, Hash, Clone)]
pub struct Status {
	pub items: Vec<StatusItem>,
}

///
#[derive(Default, Hash, Copy, Clone, PartialEq, Eq)]
pub struct StatusParams {
	status_type: StatusType,
	config: Option<ShowUntrackedFilesConfig>,
}

impl StatusParams {
	///
	pub const fn new(
		status_type: StatusType,
		config: Option<ShowUntrackedFilesConfig>,
	) -> Self {
		Self {
			status_type,
			config,
		}
	}
}

struct Request<R, A>(R, Option<A>);

struct StatusFetchState {
	current: Arc<Mutex<Request<u64, Status>>>,
	last: Arc<Mutex<Status>>,
	display_revision: Arc<AtomicU64>,
	complete: Arc<Mutex<Status>>,
	generation: Arc<AtomicU64>,
	sender: Sender<AsyncGitNotification>,
	#[cfg(test)]
	progress_gate: Option<ProgressGate>,
}

#[cfg(test)]
struct ProgressGate {
	reached: Sender<()>,
	resume: crossbeam_channel::Receiver<()>,
}

struct StatusFetchWork {
	repo: RepoPath,
	status_type: StatusType,
	config: Option<ShowUntrackedFilesConfig>,
	hash_request: u64,
	generation: u64,
	state: StatusFetchState,
}

fn overlay_status(
	previous: &[StatusItem],
	prefix: &[StatusItem],
) -> Vec<StatusItem> {
	let mut merged =
		Vec::with_capacity(previous.len() + prefix.len());
	let (mut old_index, mut new_index) = (0, 0);
	while old_index < previous.len() && new_index < prefix.len() {
		let old = &previous[old_index];
		let new = &prefix[new_index];
		match Path::new(&old.path).cmp(Path::new(&new.path)) {
			std::cmp::Ordering::Less => {
				merged.push(old.clone());
				old_index += 1;
			}
			std::cmp::Ordering::Greater => {
				merged.push(new.clone());
				new_index += 1;
			}
			std::cmp::Ordering::Equal => {
				merged.push(new.clone());
				old_index += 1;
				new_index += 1;
			}
		}
	}
	merged.extend(previous[old_index..].iter().cloned());
	merged.extend(prefix[new_index..].iter().cloned());
	merged
}

///
pub struct AsyncStatus {
	current: Arc<Mutex<Request<u64, Status>>>,
	/// Most recent data suitable for display. During a fetch this
	/// contains the progressively discovered prefix.
	last: Arc<Mutex<Status>>,
	/// Advances whenever the display snapshot changes. The status UI uses
	/// it to coalesce queued progress notifications without cloning/rebuilding
	/// the same large snapshot repeatedly.
	display_revision: Arc<AtomicU64>,
	/// Most recent complete snapshot, safe for bulk operations.
	complete: Arc<Mutex<Status>>,
	sender: Sender<AsyncGitNotification>,
	pending: Arc<AtomicUsize>,
	/// An explicit change arrived during a scan. The scan is allowed
	/// to finish, then its completion notification starts a fresh one.
	dirty: Arc<AtomicBool>,
	repo: RepoPath,
	/// Counter that increments after each completed fetch.
	generation: Arc<AtomicU64>,
	#[cfg(test)]
	progress_gate: Arc<Mutex<Option<ProgressGate>>>,
}

impl AsyncStatus {
	///
	pub fn new(
		repo: RepoPath,
		sender: Sender<AsyncGitNotification>,
	) -> Self {
		Self {
			repo,
			current: Arc::new(Mutex::new(Request(0, None))),
			last: Arc::new(Mutex::new(Status::default())),
			display_revision: Arc::new(AtomicU64::new(0)),
			complete: Arc::new(Mutex::new(Status::default())),
			sender,
			pending: Arc::new(AtomicUsize::new(0)),
			dirty: Arc::new(AtomicBool::new(false)),
			generation: Arc::new(AtomicU64::new(0)),
			#[cfg(test)]
			progress_gate: Arc::new(Mutex::new(None)),
		}
	}

	///
	pub fn last(&self) -> Result<Status> {
		let last = self.last.lock()?;
		Ok(last.clone())
	}

	/// Return a new display snapshot only when it differs from the supplied
	/// revision. The snapshot and revision are read consistently so queued
	/// progress events can share the newest state without repeated clones.
	pub fn last_if_changed(
		&self,
		since_revision: u64,
	) -> Result<Option<(u64, Status)>> {
		loop {
			let revision =
				self.display_revision.load(Ordering::Acquire);
			if revision == since_revision {
				return Ok(None);
			}

			let last = self.last.lock()?;
			if self.display_revision.load(Ordering::Acquire)
				== revision
			{
				return Ok(Some((revision, last.clone())));
			}
		}
	}

	/// Return the most recent complete repository snapshot.
	/// Unlike [`Self::last`], this never exposes a partial scan.
	pub fn complete(&self) -> Result<Status> {
		let complete = self.complete.lock()?;
		Ok(complete.clone())
	}

	/// Mark the cached request stale. An active scan is allowed to
	/// finish; the next fetch then starts against a new generation.
	pub fn invalidate(&self) {
		if self.is_pending() {
			self.dirty.store(true, Ordering::Relaxed);
		} else {
			self.generation.fetch_add(1, Ordering::Relaxed);
		}
	}

	/// Refresh the cache on a periodic poll only when idle. A long
	/// running scan must not be invalidated by each timer tick.
	pub fn invalidate_if_idle(&self) {
		if !self.is_pending() {
			self.invalidate();
		}
	}

	///
	pub fn is_pending(&self) -> bool {
		self.pending.load(Ordering::Relaxed) > 0
	}

	/// Whether an explicit change arrived during the current scan
	/// and a complete snapshot is waiting to be refreshed.
	pub fn is_dirty(&self) -> bool {
		self.dirty.load(Ordering::Relaxed)
	}

	///
	pub fn fetch(
		&self,
		params: &StatusParams,
	) -> Result<Option<Status>> {
		if self.is_pending() {
			log::trace!("request blocked, still pending");
			return Ok(None);
		}
		if self.dirty.swap(false, Ordering::Relaxed) {
			self.generation.fetch_add(1, Ordering::Relaxed);
		}

		let generation = self.generation.load(Ordering::Relaxed);
		let hash_request = hash(&(params, generation));

		log::trace!(
			"request: [hash: {}] (type: {:?}, gen: {})",
			hash_request,
			params.status_type,
			generation,
		);

		{
			let mut current = self.current.lock()?;

			if current.0 == hash_request {
				return Ok(current.1.clone());
			}

			current.0 = hash_request;
			current.1 = None;
		}

		let state = StatusFetchState {
			current: Arc::clone(&self.current),
			last: Arc::clone(&self.last),
			display_revision: Arc::clone(&self.display_revision),
			complete: Arc::clone(&self.complete),
			generation: Arc::clone(&self.generation),
			sender: self.sender.clone(),
			#[cfg(test)]
			progress_gate: self.progress_gate.lock()?.take(),
		};
		let arc_pending = Arc::clone(&self.pending);
		let work = StatusFetchWork {
			repo: self.repo.clone(),
			status_type: params.status_type,
			config: params.config,
			hash_request,
			generation,
			state,
		};
		let sender = self.sender.clone();

		self.pending.fetch_add(1, Ordering::Relaxed);

		rayon_core::spawn(move || {
			let error_generation = Arc::clone(&work.state.generation);
			let request_generation = work.generation;
			if let Err(e) = Self::fetch_helper(work) {
				log::error!("fetch_helper: {e}");
				if error_generation.load(Ordering::Relaxed)
					== request_generation
				{
					error_generation.fetch_add(1, Ordering::Relaxed);
				}
			}

			arc_pending.fetch_sub(1, Ordering::Relaxed);

			if let Err(e) = sender.send(AsyncGitNotification::Status)
			{
				log::error!("send status error: {e}");
			}
		});

		Ok(None)
	}

	fn fetch_helper(work: StatusFetchWork) -> Result<()> {
		let StatusFetchWork {
			repo,
			status_type,
			config,
			hash_request,
			generation,
			state,
		} = work;
		#[cfg(test)]
		let mut progress_gate = state.progress_gate;
		let items = sync::status::get_status_with_progress(
			&repo,
			status_type,
			config,
			|items| {
				if state.generation.load(Ordering::Relaxed)
					!= generation
				{
					return;
				}
				// Keep the previous complete snapshot visible until this
				// scan finishes. Overlay discovered paths so refreshing a
				// large tree does not make existing selections disappear
				// while the prefix is still being read.
				let snapshot = state.complete.lock().map_or_else(
					|_| Status {
						items: items.to_vec(),
					},
					|complete| Status {
						items: overlay_status(&complete.items, items),
					},
				);
				if let Ok(mut last) = state.last.lock() {
					*last = snapshot;
					state
						.display_revision
						.fetch_add(1, Ordering::Release);
				}
				if let Err(error) =
					state.sender.send(AsyncGitNotification::Status)
				{
					log::error!(
						"send status progress error: {error}"
					);
				}
				#[cfg(test)]
				if let Some(gate) = progress_gate.take() {
					let _ = gate.reached.send(());
					let _ = gate.resume.recv();
				}
			},
		)?;
		let res = Status { items };
		log::trace!(
			"status fetched: {hash_request} (type: {status_type:?})",
		);

		if state.generation.load(Ordering::Relaxed) != generation {
			return Ok(());
		}

		{
			let mut current = state.current.lock()?;
			if current.0 == hash_request {
				current.1 = Some(res.clone());
			}
		}

		{
			let mut last = state.last.lock()?;
			*last = res.clone();
			state.display_revision.fetch_add(1, Ordering::Release);
		}

		{
			let mut complete = state.complete.lock()?;
			*complete = res;
		}

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sync::{tests::repo_init, RepoPath};
	use crate::StatusItemType;
	use crossbeam_channel::{bounded, unbounded};
	use std::{fs, path::Path};

	#[test]
	fn status_publishes_sorted_prefixes_before_the_full_scan() {
		let (_temp, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath = root.to_str().unwrap().into();
		for index in 0..300 {
			fs::write(
				root.join(format!("file-{index:03}")),
				"change",
			)
			.unwrap();
		}

		let mut saw_prefix = false;
		let items = sync::status::get_status_with_progress(
			&repo_path,
			StatusType::WorkingDir,
			None,
			|prefix| {
				if prefix.len() < 300 {
					saw_prefix = true;
				}
				assert!(prefix.windows(2).all(|pair| {
					Path::new(&pair[0].path)
						<= Path::new(&pair[1].path)
				}));
			},
		)
		.unwrap();

		assert!(
			saw_prefix,
			"a prefix must be available before completion"
		);
		assert_eq!(items.len(), 300);
	}

	#[test]
	fn async_status_exposes_partial_display_while_authoritative_scan_is_paused(
	) {
		let (_temp, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		for index in 0..300 {
			fs::write(
				root.join(format!("file-{index:03}")),
				"change",
			)
			.unwrap();
		}
		let repo_path: RepoPath = root.to_str().unwrap().into();
		let (sender, receiver) = unbounded();
		let status = AsyncStatus::new(repo_path, sender);
		let (reached, wait) = bounded(1);
		let (resume, continue_scan) = bounded(1);
		*status.progress_gate.lock().unwrap() = Some(ProgressGate {
			reached,
			resume: continue_scan,
		});

		status
			.fetch(&StatusParams::new(StatusType::WorkingDir, None))
			.unwrap();
		wait.recv_timeout(std::time::Duration::from_secs(5))
			.unwrap();

		assert!(status.is_pending());
		assert!(status.complete().unwrap().items.is_empty());
		let (revision, display) =
			status.last_if_changed(0).unwrap().unwrap();
		assert_eq!(display.items.len(), 128);
		assert!(status.last_if_changed(revision).unwrap().is_none());

		resume.send(()).unwrap();
		let deadline = std::time::Instant::now()
			+ std::time::Duration::from_secs(5);
		while status.is_pending() {
			assert!(std::time::Instant::now() < deadline);
			let _ = receiver
				.recv_timeout(std::time::Duration::from_millis(50));
		}
		assert_eq!(status.complete().unwrap().items.len(), 300);
	}

	#[test]
	fn progress_overlay_merges_sorted_snapshots_and_replaces_matching_paths(
	) {
		let previous = vec![
			StatusItem {
				path: String::from("a"),
				status: StatusItemType::Modified,
			},
			StatusItem {
				path: String::from("c"),
				status: StatusItemType::Modified,
			},
			StatusItem {
				path: String::from("e"),
				status: StatusItemType::Modified,
			},
		];
		let prefix = vec![
			StatusItem {
				path: String::from("b"),
				status: StatusItemType::New,
			},
			StatusItem {
				path: String::from("c"),
				status: StatusItemType::Deleted,
			},
		];

		let merged = overlay_status(&previous, &prefix);
		assert_eq!(
			merged
				.iter()
				.map(|item| item.path.as_str())
				.collect::<Vec<_>>(),
			["a", "b", "c", "e"]
		);
		assert_eq!(merged[2].status, StatusItemType::Deleted);
	}

	#[test]
	fn stale_status_generation_cannot_replace_display_or_complete_state(
	) {
		let (_temp, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		fs::write(root.join("untracked"), "change").unwrap();
		let repo_path: RepoPath = root.to_str().unwrap().into();
		let current = Arc::new(Mutex::new(Request(17, None)));
		let last = Arc::new(Mutex::new(Status::default()));
		let display_revision = Arc::new(AtomicU64::new(0));
		let complete = Arc::new(Mutex::new(Status::default()));
		let generation = Arc::new(AtomicU64::new(2));
		let (sender, _receiver) = unbounded();

		AsyncStatus::fetch_helper(StatusFetchWork {
			repo: repo_path,
			status_type: StatusType::WorkingDir,
			config: None,
			hash_request: 17,
			generation: 1,
			state: StatusFetchState {
				current: Arc::clone(&current),
				last: Arc::clone(&last),
				display_revision: Arc::clone(&display_revision),
				complete: Arc::clone(&complete),
				generation: Arc::clone(&generation),
				sender,
				#[cfg(test)]
				progress_gate: None,
			},
		})
		.unwrap();

		assert!(last.lock().unwrap().items.is_empty());
		assert_eq!(display_revision.load(Ordering::Relaxed), 0);
		assert!(complete.lock().unwrap().items.is_empty());
		assert!(current.lock().unwrap().1.is_none());
	}

	#[test]
	fn invalidation_during_a_scan_queues_one_fresh_follow_up() {
		let (_temp, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		for index in 0..1_000 {
			fs::write(
				root.join(format!("file-{index:04}")),
				"change",
			)
			.unwrap();
		}
		let repo_path: RepoPath = root.to_str().unwrap().into();
		let (sender, receiver) = unbounded();
		let status = AsyncStatus::new(repo_path, sender);
		let params = StatusParams::new(StatusType::WorkingDir, None);

		assert!(status.fetch(&params).unwrap().is_none());
		assert!(status.is_pending());
		let generation = status.generation.load(Ordering::Relaxed);
		status.invalidate();
		assert_eq!(
			status.generation.load(Ordering::Relaxed),
			generation
		);
		assert!(status.dirty.load(Ordering::Relaxed));

		let deadline = std::time::Instant::now()
			+ std::time::Duration::from_secs(10);
		while status.is_pending() {
			assert!(
				std::time::Instant::now() < deadline,
				"the initial status scan should finish"
			);
			let _ = receiver
				.recv_timeout(std::time::Duration::from_millis(50));
		}

		assert!(status.dirty.load(Ordering::Relaxed));
		assert_eq!(status.complete().unwrap().items.len(), 1_000);
		let (revision, display) =
			status.last_if_changed(0).unwrap().expect(
				"completed snapshot advances its display revision",
			);
		assert_eq!(display.items.len(), 1_000);
		assert!(status.last_if_changed(revision).unwrap().is_none());
		fs::write(root.join("late-change"), "change").unwrap();
		assert!(status.fetch(&params).unwrap().is_none());
		assert_eq!(
			status.generation.load(Ordering::Relaxed),
			generation + 1
		);
		assert!(!status.dirty.load(Ordering::Relaxed));

		while status.is_pending() {
			assert!(
				std::time::Instant::now() < deadline,
				"the follow-up status scan should finish"
			);
			let _ = receiver
				.recv_timeout(std::time::Duration::from_millis(50));
		}
		assert_eq!(status.complete().unwrap().items.len(), 1_001);
		let (next_revision, display) = status
			.last_if_changed(revision)
			.unwrap()
			.expect("refreshed data advances the display revision");
		assert!(next_revision > revision);
		assert_eq!(display.items.len(), 1_001);
	}
}
