use crate::{
	error::Result,
	hash,
	sync::{
		self, commit_files::OldNew, diff::DiffOptions, CommitId,
		RepoPath,
	},
	AsyncGitNotification, FileDiff,
};
use crossbeam_channel::Sender;
use std::{
	hash::Hash,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc, Mutex,
	},
};

///
#[derive(Debug, Hash, Clone, PartialEq, Eq)]
pub enum DiffType {
	/// diff two commits
	Commits(OldNew<CommitId>),
	/// diff in a given commit
	Commit(CommitId),
	/// diff against staged file
	Stage,
	/// diff against file in workdir
	WorkDir,
}

///
#[derive(Debug, Hash, Clone, PartialEq, Eq)]
pub struct DiffParams {
	/// path to the file to diff
	pub path: String,
	/// what kind of diff
	pub diff_type: DiffType,
	/// diff options
	pub options: DiffOptions,
}

struct Request<R, A>(R, Option<A>);

#[derive(Default, Clone)]
struct LastResult<P, R> {
	params: P,
	result: R,
}

///
pub struct AsyncDiff {
	current: Arc<Mutex<Request<Option<u64>, FileDiff>>>,
	last: Arc<Mutex<Option<LastResult<DiffParams, FileDiff>>>>,
	sender: Sender<AsyncGitNotification>,
	pending: Arc<AtomicUsize>,
	repo: RepoPath,
}

impl AsyncDiff {
	///
	pub fn new(
		repo: RepoPath,
		sender: &Sender<AsyncGitNotification>,
	) -> Self {
		Self {
			repo,
			current: Arc::new(Mutex::new(Request(None, None))),
			last: Arc::new(Mutex::new(None)),
			sender: sender.clone(),
			pending: Arc::new(AtomicUsize::new(0)),
		}
	}

	///
	pub fn last(&self) -> Result<Option<(DiffParams, FileDiff)>> {
		let last = self.last.lock()?;

		Ok(last.clone().map(|res| (res.params, res.result)))
	}

	///
	pub fn refresh(&self) -> Result<()> {
		if let Ok(Some(param)) = self.get_last_param() {
			self.clear_current()?;
			self.request(param)?;
		}
		Ok(())
	}

	///
	pub fn is_pending(&self) -> bool {
		self.pending.load(Ordering::Relaxed) > 0
	}

	///
	pub fn request(
		&self,
		params: DiffParams,
	) -> Result<Option<FileDiff>> {
		log::trace!("request {params:?}");

		let hash = hash(&params);

		{
			let mut current = self.current.lock()?;

			if current.0 == Some(hash) {
				return Ok(current.1.clone());
			}

			current.0 = Some(hash);
			current.1 = None;
		}

		let arc_current = Arc::clone(&self.current);
		let arc_last = Arc::clone(&self.last);
		let sender = self.sender.clone();
		let arc_pending = Arc::clone(&self.pending);
		let repo = self.repo.clone();

		self.pending.fetch_add(1, Ordering::Relaxed);

		rayon_core::spawn(move || {
			let notify = Self::get_diff_helper(
				&repo,
				params,
				&arc_last,
				&arc_current,
				hash,
			);

			let notify = match notify {
				Err(e) => {
					log::error!("get_diff_helper error: {e}");
					if let Err(clear_error) =
						Self::clear_current_if_hash(
							&arc_current,
							hash,
						) {
						log::error!(
							"could not clear failed diff request: {clear_error}"
						);
					}
					false
				}
				Ok(notify) => notify,
			};

			Self::finish_request(&sender, &arc_pending, notify);
		});

		Ok(None)
	}

	fn get_diff_helper(
		repo_path: &RepoPath,
		params: DiffParams,
		arc_last: &Arc<
			Mutex<Option<LastResult<DiffParams, FileDiff>>>,
		>,
		arc_current: &Arc<Mutex<Request<Option<u64>, FileDiff>>>,
		hash: u64,
	) -> Result<bool> {
		let res = match params.diff_type {
			DiffType::Stage => sync::diff::get_diff(
				repo_path,
				&params.path,
				true,
				Some(params.options),
			)?,
			DiffType::WorkDir => sync::diff::get_diff(
				repo_path,
				&params.path,
				false,
				Some(params.options),
			)?,
			DiffType::Commit(id) => sync::diff::get_diff_commit(
				repo_path,
				id,
				params.path.clone(),
				Some(params.options),
			)?,
			DiffType::Commits(ids) => sync::diff::get_diff_commits(
				repo_path,
				ids,
				params.path.clone(),
				Some(params.options),
			)?,
		};

		let mut notify = false;
		{
			let mut current = arc_current.lock()?;
			if current.0 == Some(hash) {
				current.1 = Some(res.clone());
				notify = true;
			}
		}

		{
			let mut last = arc_last.lock()?;
			*last = Some(LastResult {
				result: res,
				params,
			});
		}

		Ok(notify)
	}

	fn get_last_param(&self) -> Result<Option<DiffParams>> {
		Ok(self.last.lock()?.clone().map(|e| e.params))
	}

	fn clear_current(&self) -> Result<()> {
		let mut current = self.current.lock()?;
		current.0 = None;
		current.1 = None;
		Ok(())
	}

	fn clear_current_if_hash(
		current: &Arc<Mutex<Request<Option<u64>, FileDiff>>>,
		hash: u64,
	) -> Result<()> {
		let mut current = current.lock()?;
		if current.0 == Some(hash) {
			current.0 = None;
			current.1 = None;
		}
		Ok(())
	}

	fn finish_request(
		sender: &Sender<AsyncGitNotification>,
		pending: &AtomicUsize,
		notify: bool,
	) {
		pending.fetch_sub(1, Ordering::Relaxed);
		let notification = if notify {
			AsyncGitNotification::Diff
		} else {
			AsyncGitNotification::FinishUnchanged
		};
		if sender.send(notification).is_err() {
			log::debug!(
				"diff notification receiver closed before completion"
			);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	#[test]
	fn closed_receiver_is_a_cancelled_completion_and_clears_pending()
	{
		let (sender, receiver) = crossbeam_channel::unbounded();
		drop(receiver);
		let pending = AtomicUsize::new(1);

		AsyncDiff::finish_request(&sender, &pending, true);

		assert_eq!(pending.load(Ordering::Relaxed), 0);
	}

	#[test]
	fn failed_request_invalidation_does_not_clear_a_newer_request() {
		let current = Arc::new(Mutex::new(Request(Some(17), None)));
		AsyncDiff::clear_current_if_hash(&current, 16).unwrap();
		assert_eq!(current.lock().unwrap().0, Some(17));

		AsyncDiff::clear_current_if_hash(&current, 17).unwrap();
		assert_eq!(current.lock().unwrap().0, None);
	}

	#[test]
	fn failed_request_completes_unchanged_and_retries_only_when_requested(
	) {
		let directory = tempfile::tempdir().unwrap();
		let repo_path: RepoPath =
			directory.path().to_str().unwrap().into();
		let (sender, receiver) = crossbeam_channel::unbounded();
		let diff = AsyncDiff::new(repo_path, &sender);
		let params = DiffParams {
			path: String::from("missing.txt"),
			diff_type: DiffType::WorkDir,
			options: DiffOptions::default(),
		};

		assert!(diff.request(params.clone()).unwrap().is_none());
		assert_eq!(
			receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
			AsyncGitNotification::FinishUnchanged
		);
		assert!(!diff.is_pending());
		assert!(receiver.try_recv().is_err());

		// The failed request was invalidated, so an explicit retry starts
		// one new worker and emits one completion without an automatic loop.
		assert!(diff.request(params).unwrap().is_none());
		assert_eq!(
			receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
			AsyncGitNotification::FinishUnchanged
		);
		assert!(!diff.is_pending());
		assert!(receiver.try_recv().is_err());
	}
}
