use std::fmt::Display;

use super::RepoPath;
use crate::{
	error::Result,
	sync::{
		commit_details::get_author_of_commit,
		repository::{gix_repo, repo},
	},
};
use git2::{Commit, Error, Oid};
use scopetime::scope_time;
use serde::{
	de::{Deserialize, Deserializer, Error as DeError},
	ser::Serialize,
	Serializer,
};
use smallvec::SmallVec;
use unicode_truncate::UnicodeTruncateStr;

/// identifies a single commit
#[derive(
	Debug, Copy, Clone, PartialEq, Eq, Hash, Ord, PartialOrd,
)]
pub struct CommitId(Oid);

impl Default for CommitId {
	fn default() -> Self {
		Self(Oid::ZERO_SHA1)
	}
}

impl CommitId {
	/// create new `CommitId`
	pub const fn new(id: Oid) -> Self {
		Self(id)
	}

	///
	pub(crate) const fn get_oid(self) -> Oid {
		self.0
	}

	/// 7 chars short hash
	pub fn get_short_string(&self) -> String {
		self.to_string().chars().take(7).collect()
	}

	/// Tries to retrieve the `CommitId` form the revision if exists in the given repository
	pub fn from_revision(
		repo_path: &RepoPath,
		revision: &str,
	) -> Result<Self> {
		scope_time!("CommitId::from_revision");

		let repo = repo(repo_path)?;

		let commit_obj = repo.revparse_single(revision)?;
		Ok(commit_obj.id().into())
	}

	/// Tries to convert a &str representation of a commit id into
	/// a `CommitId`
	pub fn from_str_unchecked(commit_id_str: &str) -> Result<Self> {
		match Oid::from_str(commit_id_str) {
			Err(e) => Err(crate::Error::Generic(format!(
				"Could not convert {}",
				e.message()
			))),
			Ok(v) => Ok(Self::new(v)),
		}
	}
}

impl Display for CommitId {
	fn fmt(
		&self,
		f: &mut std::fmt::Formatter<'_>,
	) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

/// Serialize as the full lowercase hex string (the `Display`
/// form), so persisted data stays human-inspectable.
impl Serialize for CommitId {
	fn serialize<S: Serializer>(
		&self,
		serializer: S,
	) -> std::result::Result<S::Ok, S::Error> {
		serializer.serialize_str(&self.to_string())
	}
}

/// Deserialize from a full-length hex string (40 or 64 hex
/// digits, per object format). Abbreviated prefixes are
/// rejected — an id in persisted data is an exact identity,
/// never a guess — and anything
/// [`CommitId::from_str_unchecked`] rejects (non-hex, wrong
/// length) is a deserialization error.
impl<'de> Deserialize<'de> for CommitId {
	fn deserialize<D: Deserializer<'de>>(
		deserializer: D,
	) -> std::result::Result<Self, D::Error> {
		let text = String::deserialize(deserializer)?;
		if text.len() != 40 && text.len() != 64 {
			return Err(D::Error::custom(
				"commit id must be a full-length hex \
				 object id, not an abbreviation",
			));
		}
		Self::from_str_unchecked(&text).map_err(D::Error::custom)
	}
}

impl From<CommitId> for Oid {
	fn from(id: CommitId) -> Self {
		id.0
	}
}

impl From<Oid> for CommitId {
	fn from(id: Oid) -> Self {
		Self::new(id)
	}
}

impl From<gix::Id<'_>> for CommitId {
	fn from(object_id: gix::Id<'_>) -> Self {
		#[allow(clippy::expect_used)]
		let oid = Oid::from_bytes(object_id.as_bytes()).expect("`Oid::from_bytes(object_id.as_bytes())` is expected to never fail");

		Self::new(oid)
	}
}

impl From<gix::ObjectId> for CommitId {
	fn from(object_id: gix::ObjectId) -> Self {
		#[allow(clippy::expect_used)]
		let oid = Oid::from_bytes(object_id.as_bytes()).expect("`Oid::from_bytes(object_id.as_bytes())` is expected to never fail");

		Self::new(oid)
	}
}

impl From<gix::Commit<'_>> for CommitId {
	fn from(commit: gix::Commit<'_>) -> Self {
		#[allow(clippy::expect_used)]
		let oid = Oid::from_bytes(commit.id().as_bytes()).expect("`Oid::from_bytes(commit.id().as_bytes())` is expected to never fail");

		Self::new(oid)
	}
}

impl From<CommitId> for gix::ObjectId {
	fn from(id: CommitId) -> Self {
		Self::from_bytes_or_panic(id.0.as_bytes())
	}
}

///
#[derive(Debug, Clone)]
pub struct CommitInfo {
	///
	pub message: String,
	///
	pub time: i64,
	///
	pub author: String,
	///
	pub id: CommitId,
	///
	pub parents: SmallVec<[CommitId; 2]>,
}

///
pub fn get_commits_info(
	repo_path: &RepoPath,
	ids: &[CommitId],
	message_length_limit: usize,
) -> Result<Vec<CommitInfo>> {
	scope_time!("get_commits_info");

	let repo = repo(repo_path)?;
	get_commits_info_from_repo(&repo, ids, message_length_limit)
}

/// Decode commit presentation metadata using an already-open repository.
/// Background walkers use this to avoid reopening the repository for every
/// requested viewport page.
pub fn get_commits_info_from_repo(
	repo: &git2::Repository,
	ids: &[CommitId],
	message_length_limit: usize,
) -> Result<Vec<CommitInfo>> {
	let mailmap = repo.mailmap()?;

	let commits = ids
		.iter()
		.map(|id| repo.find_commit((*id).into()))
		.collect::<std::result::Result<Vec<Commit>, Error>>()?
		.into_iter();

	let res = commits
		.map(|c: Commit| {
			let message = get_message(&c, Some(message_length_limit));
			let author = get_author_of_commit(&c, &mailmap)
				.name()
				.ok()
				.map_or_else(
					|| String::from("<unknown>"),
					String::from,
				);
			CommitInfo {
				message,
				author,
				time: c.time().seconds(),
				id: CommitId(c.id()),
				parents: c
					.parents()
					.take(2)
					.map(|p| CommitId(p.id()))
					.collect(),
			}
		})
		.collect::<Vec<_>>();

	Ok(res)
}

///
/// The first parent of a commit; `None` for a root commit.
/// Merge commits ignore their second parent here (a linear
/// "rewrite from this commit" scope follows the first parent).
#[must_use]
pub fn commit_parent(
	repo_path: &RepoPath,
	commit: CommitId,
) -> Option<CommitId> {
	let repo = repo(repo_path).ok()?;
	let commit = repo.find_commit(commit.get_oid()).ok()?;
	commit.parent(0).ok().map(|p| CommitId::new(p.id()))
}

///
pub fn get_commit_info(
	repo_path: &RepoPath,
	commit_id: &CommitId,
) -> Result<CommitInfo> {
	scope_time!("get_commit_info");

	let repo: gix::Repository = gix_repo(repo_path)?;
	let mailmap = repo.open_mailmap();

	let commit = repo.find_commit(*commit_id)?;
	let commit_ref = commit.decode()?;

	let message = gix_get_message(&commit_ref, None);

	let author = commit_ref.author()?;

	let author = mailmap.try_resolve(author).map_or_else(
		|| author.name.into(),
		|signature| signature.name,
	);

	Ok(CommitInfo {
		message,
		author: author.to_string(),
		time: commit_ref.time()?.seconds,
		id: commit.id().detach().into(),
		parents: commit
			.parent_ids()
			.take(2)
			.map(Into::into)
			.collect(),
	})
}

/// if `message_limit` is set the message will be
/// limited to the first line and truncated to fit
pub fn get_message(
	c: &git2::Commit,
	message_limit: Option<usize>,
) -> String {
	let msg = String::from_utf8_lossy(c.message_bytes());
	let msg = msg.trim();

	message_limit.map_or_else(
		|| msg.to_string(),
		|limit| {
			let msg = msg.lines().next().unwrap_or_default();
			msg.unicode_truncate(limit).0.to_string()
		},
	)
}

/// if `message_limit` is set the message will be
/// limited to the first line and truncated to fit
pub fn gix_get_message(
	commit_ref: &gix::objs::CommitRef,
	message_limit: Option<usize>,
) -> String {
	let message = commit_ref.message.to_string();
	let message = message.trim();

	message_limit.map_or_else(
		|| message.to_string(),
		|limit| {
			let message = message.lines().next().unwrap_or_default();
			message.unicode_truncate(limit).0.to_string()
		},
	)
}

#[cfg(test)]
mod tests {
	use super::get_commits_info;
	use crate::{
		error::Result,
		sync::{
			commit, stage_add_file, tests::repo_init_empty,
			utils::get_head_repo, CommitId, RepoPath,
		},
	};
	use std::{fs::File, io::Write, path::Path};

	#[test]
	fn test_log() -> Result<()> {
		let file_path = Path::new("foo");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(repo_path, file_path).unwrap();
		let c1 = commit(repo_path, "commit1").unwrap();
		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(repo_path, file_path).unwrap();
		let c2 = commit(repo_path, "commit2").unwrap();

		let res = get_commits_info(repo_path, &[c2, c1], 50).unwrap();

		assert_eq!(res.len(), 2);
		assert_eq!(res[0].message.as_str(), "commit2");
		assert_eq!(res[0].author.as_str(), "name");
		assert_eq!(res[1].message.as_str(), "commit1");

		File::create(root.join(".mailmap"))?
			.write_all(b"new name <newemail> <email>")?;
		let res = get_commits_info(repo_path, &[c2], 50).unwrap();

		assert_eq!(res[0].author.as_str(), "new name");

		Ok(())
	}

	#[test]
	fn test_log_first_msg_line() -> Result<()> {
		let file_path = Path::new("foo");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(repo_path, file_path).unwrap();
		let c1 = commit(repo_path, "subject\nbody").unwrap();

		let res = get_commits_info(repo_path, &[c1], 50).unwrap();

		assert_eq!(res.len(), 1);
		assert_eq!(res[0].message.as_str(), "subject");

		Ok(())
	}

	#[test]
	fn test_invalid_utf8() -> Result<()> {
		let file_path = Path::new("foo");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"a")?;
		stage_add_file(repo_path, file_path).unwrap();

		let msg = invalidstring::invalid_utf8("test msg");
		commit(repo_path, msg.as_str()).unwrap();

		let res = get_commits_info(
			repo_path,
			&[get_head_repo(&repo).unwrap()],
			50,
		)
		.unwrap();

		assert_eq!(res.len(), 1);
		dbg!(&res[0].message);
		assert!(res[0].message.starts_with("test msg"));

		Ok(())
	}

	#[test]
	fn test_get_commit_from_revision() -> Result<()> {
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		let foo_file = Path::new("foo");
		File::create(root.join(foo_file))?.write_all(b"a")?;
		stage_add_file(repo_path, foo_file).unwrap();
		let c1 = commit(repo_path, "subject: foo\nbody").unwrap();
		let c1_rev = c1.get_short_string();

		assert_eq!(
			CommitId::from_revision(repo_path, c1_rev.as_str())
				.unwrap(),
			c1
		);

		const FOREIGN_HASH: &str =
			"d6d7d55cb6e4ba7301d6a11a657aab4211e5777e";
		assert!(
			CommitId::from_revision(repo_path, FOREIGN_HASH).is_err()
		);

		Ok(())
	}

	/// The full hex string a `CommitId` serializes to must
	/// deserialize back to the same id.
	#[test]
	fn test_serde_hex_round_trip() {
		const HEX: &str = "d6d7d55cb6e4ba7301d6a11a657aab4211e5777e";
		let id = CommitId::from_str_unchecked(HEX).unwrap();

		let text = ron::to_string(&id).unwrap();
		assert_eq!(text, format!("\"{HEX}\""));

		let back: CommitId = ron::from_str(&text).unwrap();
		assert_eq!(back, id);
	}

	/// Short or malformed strings must fail deserialization:
	/// ids are never truncated or zero-filled silently.
	#[test]
	fn test_serde_rejects_short_strings() {
		for bad in [
			"d6d7d55",                                  // abbreviated
			"",                                         // empty
			"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz", // not hex
		] {
			assert!(
				ron::from_str::<CommitId>(&format!("\"{bad}\""))
					.is_err(),
				"deserialization accepted {bad:?}"
			);
		}
	}
}
