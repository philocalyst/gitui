/*!
The tabs module contains a struct for each of the tabs visible in the
ui:

- [`Status`]: Stage changes, push, pull
- [`Revlog`]: Revision log (think git log)
- [`FilesTab`]: See content of any file at HEAD. Blame
- [`Stashing`]: Managing one stash
- [`StashList`]: Managing all stashes
- [`RebaseTab`]: The interactive-rebase workspace (plan rail,
  inspector, composer)

Many of the tabs can expand to show more details. This is done via
Enter or right-arrow. To close again, press ESC.
*/

mod branches;
mod files;
mod rebase;
mod rebase_backend;
mod revlog;
mod stashing;
mod stashlist;
mod status;

pub use branches::Branches;
pub use files::FilesTab;
// TODO(app wiring): these become live once app.rs constructs
// the rebase tab against its service.
#[allow(unused_imports)]
pub use rebase::RebaseTab;
#[allow(unused_imports)]
pub use rebase_backend::{RebaseBackend, SharedBackend};
pub use revlog::Revlog;
pub use stashing::{Stashing, StashingOptions};
pub use stashlist::StashList;
pub use status::Status;
