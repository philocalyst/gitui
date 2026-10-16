use crate::keys::GituiKeyEvent;
use crate::strings::order;

///
#[derive(Clone)]
pub struct CommandText {
	/// Executable binding, shared by hints, help and the contextual palette.
	pub binding: Option<GituiKeyEvent>,
	///
	pub name: String,
	///
	pub desc: &'static str,
	///
	pub group: &'static str,
	///
	pub hide_help: bool,
}

// Presentation ordering is independent of the executable binding.
impl Ord for CommandText {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		(&self.name, self.desc, self.group, self.hide_help).cmp(&(
			&other.name,
			other.desc,
			other.group,
			other.hide_help,
		))
	}
}
impl PartialOrd for CommandText {
	fn partial_cmp(
		&self,
		other: &Self,
	) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}
impl PartialEq for CommandText {
	fn eq(&self, other: &Self) -> bool {
		self.cmp(other).is_eq()
	}
}
impl Eq for CommandText {}

impl CommandText {
	///
	pub const fn new(
		name: String,
		desc: &'static str,
		group: &'static str,
	) -> Self {
		Self {
			binding: None,
			name,
			desc,
			group,
			hide_help: false,
		}
	}
	pub const fn with_binding(
		mut self,
		binding: GituiKeyEvent,
	) -> Self {
		self.binding = Some(binding);
		self
	}
	///
	pub const fn hide_help(self) -> Self {
		let mut tmp = self;
		tmp.hide_help = true;
		tmp
	}
}

///
pub struct CommandInfo {
	///
	pub text: CommandText,
	/// available but not active in the context
	pub enabled: bool,
	/// will show up in the quick bar
	pub quick_bar: bool,

	/// available in current app state
	pub available: bool,
	/// used to order commands in quickbar
	pub order: i8,
}

impl CommandInfo {
	///
	pub const fn new(
		text: CommandText,
		enabled: bool,
		available: bool,
	) -> Self {
		Self {
			text,
			enabled,
			quick_bar: true,
			available,
			order: order::AVERAGE,
		}
	}

	///
	pub const fn order(self, order: i8) -> Self {
		let mut res = self;
		res.order = order;
		res
	}

	///
	pub const fn hidden(self) -> Self {
		let mut res = self;
		res.quick_bar = false;
		res
	}

	///
	pub const fn show_in_quickbar(&self) -> bool {
		self.quick_bar && self.available
	}
}
