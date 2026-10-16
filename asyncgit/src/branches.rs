use crate::{
	asyncjob::{AsyncJob, RunParams},
	error::Result,
	sync::{
		branch::{get_branches_info, get_branches_info_with_head},
		BranchInfo, RepoPath,
	},
	AsyncGitNotification,
};
use std::sync::{Arc, Mutex};

/// A background snapshot of the repository's current HEAD identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadIdentity {
	/// Symbolic target when HEAD names a branch; `None` for detached HEAD.
	pub reference: Option<String>,
	/// Commit currently named by HEAD, or `None` for an unborn branch.
	pub target: Option<crate::sync::CommitId>,
	/// Whether HEAD is detached.
	pub detached: bool,
}

/// Branch references and their optional local-branch HEAD snapshot.
pub struct BranchListSnapshot {
	/// Branch list result.
	pub branches: Result<Vec<BranchInfo>>,
	/// HEAD identity returned for local-branch scans.
	pub head: Option<Result<HeadIdentity>>,
}

enum JobState {
	Request {
		local_branches: bool,
		repo: RepoPath,
	},
	Response {
		branches: Result<Vec<BranchInfo>>,
		head: Option<Result<HeadIdentity>>,
	},
}

///
#[derive(Clone, Default)]
pub struct AsyncBranchesJob {
	state: Arc<Mutex<Option<JobState>>>,
}

///
impl AsyncBranchesJob {
	///
	pub fn new(repo: RepoPath, local_branches: bool) -> Self {
		Self {
			state: Arc::new(Mutex::new(Some(JobState::Request {
				repo,
				local_branches,
			}))),
		}
	}

	///
	pub fn result(&self) -> Option<Result<Vec<BranchInfo>>> {
		self.result_with_head().map(|snapshot| snapshot.branches)
	}

	/// Take the completed branch list and, for local jobs, its HEAD snapshot.
	pub fn result_with_head(&self) -> Option<BranchListSnapshot> {
		if let Ok(mut state) = self.state.lock() {
			if let Some(current) = state.take() {
				return match current {
					request @ JobState::Request { .. } => {
						*state = Some(request);
						None
					}
					JobState::Response { branches, head } => {
						Some(BranchListSnapshot { branches, head })
					}
				};
			}
		}

		None
	}
}

impl AsyncJob for AsyncBranchesJob {
	type Notification = AsyncGitNotification;
	type Progress = ();

	fn run(
		&mut self,
		_params: RunParams<Self::Notification, Self::Progress>,
	) -> Result<Self::Notification> {
		let request =
			self.state.lock().ok().and_then(|mut state| match state
				.take()?
			{
				JobState::Request {
					local_branches,
					repo,
				} => Some((local_branches, repo)),
				response @ JobState::Response { .. } => {
					*state = Some(response);
					None
				}
			});
		if let Some((local_branches, repo)) = request {
			let (branches, head) = if local_branches {
				match get_branches_info_with_head(&repo) {
					Ok((branches, head)) => {
						(Ok(branches), Some(Ok(head)))
					}
					Err(error) => (Err(error), None),
				}
			} else {
				(get_branches_info(&repo, false), None)
			};
			if let Ok(mut state) = self.state.lock() {
				*state = Some(JobState::Response { branches, head });
			}
		}

		Ok(AsyncGitNotification::Branches)
	}
}
