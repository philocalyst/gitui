//! Decoupling seam between the rebase workspace UI and
//! [`crate::rebase_service::RebaseService`].
//!
//! The tab and its components are written against
//! [`RebaseBackend`], a thin trait over the service contract.
//! This keeps the UI snapshot-testable against an in-memory
//! fake; the delegate at the bottom is a straight delegation
//! (with two v1 defaults: `start` uses `EmptyPolicy::Stop` and
//! no `--update-refs`, and `continue` passes no message
//! override — the composer resolves messages instead). Draft
//! creation, adoption, and refresh are driven by the app
//! directly on the service, so they are not part of the tab's
//! contract.

use anyhow::Result;
use asyncgit::rebase::{
	engine::EmptyPolicy, journal::Session, plan::Edit, policy::Policy,
};
use std::cell::RefCell;
use std::rc::Rc;

pub trait RebaseBackend {
	/// The folded session state.
	fn session(&self) -> &Session;

	/// The complete projection for the current moment.
	fn policy(&self) -> Policy;

	/// Whether a mutating command is currently in flight.
	fn is_busy(&self) -> bool;

	/// Apply one plan edit (`SetAction`, `Move`, `InsertBreak`,
	/// `InsertExec`, `SetMessage`).
	fn apply_edit(&mut self, edit: Edit) -> Result<()>;

	/// Undo the last plan edit.
	fn undo(&mut self) -> Result<()>;

	/// Redo an undone plan edit.
	fn redo(&mut self) -> Result<()>;

	/// Start the reviewed rebase.
	fn start(&mut self) -> Result<()>;

	/// Continue the stopped rebase.
	fn r#continue(&mut self) -> Result<()>;

	/// Skip the stopped step.
	fn skip(&mut self) -> Result<()>;

	/// Abort the rebase.
	fn abort(&mut self) -> Result<()>;

	/// Resolve a pending message composition.
	fn resolve_message(&mut self, message: String) -> Result<()>;

	/// Cancel a pending message composition.
	fn cancel_message(&mut self) -> Result<()>;

	/// Close the session workspace.
	fn close(&mut self) -> Result<()>;

	/// The last snapshot, when the backend can provide one.
	/// Used for conflict shapes in the inspector; the UI
	/// degrades to plain paths when `None` (the default).
	fn snapshot(
		&self,
	) -> Option<asyncgit::rebase::snapshot::Snapshot> {
		None
	}
}

/// Shared handle to the backend the tab renders.
pub type SharedBackend = Rc<RefCell<dyn RebaseBackend>>;

impl RebaseBackend for crate::rebase_service::RebaseService {
	fn session(&self) -> &Session {
		Self::session(self)
	}

	fn policy(&self) -> Policy {
		Self::policy(self)
	}

	fn is_busy(&self) -> bool {
		Self::is_busy(self)
	}

	fn apply_edit(&mut self, edit: Edit) -> Result<()> {
		Self::apply_edit(self, edit).map_err(anyhow::Error::msg)
	}

	fn undo(&mut self) -> Result<()> {
		Self::undo(self).map_err(anyhow::Error::msg)
	}

	fn redo(&mut self) -> Result<()> {
		Self::redo(self).map_err(anyhow::Error::msg)
	}

	fn start(&mut self) -> Result<()> {
		Self::start(self, EmptyPolicy::Stop, Vec::new());
		Ok(())
	}

	fn r#continue(&mut self) -> Result<()> {
		Self::r#continue(self, None);
		Ok(())
	}

	fn skip(&mut self) -> Result<()> {
		Self::skip(self);
		Ok(())
	}

	fn abort(&mut self) -> Result<()> {
		Self::abort(self);
		Ok(())
	}

	fn resolve_message(&mut self, message: String) -> Result<()> {
		Self::resolve_message(self, message);
		Ok(())
	}

	fn cancel_message(&mut self) -> Result<()> {
		Self::cancel_message(self);
		Ok(())
	}

	fn close(&mut self) -> Result<()> {
		Self::close(self);
		Ok(())
	}

	fn snapshot(
		&self,
	) -> Option<asyncgit::rebase::snapshot::Snapshot> {
		Some(Self::snapshot(self).clone())
	}
}
