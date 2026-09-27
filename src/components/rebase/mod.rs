//! Components of the rebase workspace tab.
//!
//! * [`rail`] — the plan rows (the tab's spine)
//! * [`inspector`] — the phase-dependent right-hand panel

pub mod inspector;
pub mod rail;

pub use inspector::Inspector;
pub use rail::{rail_items, Rail};
