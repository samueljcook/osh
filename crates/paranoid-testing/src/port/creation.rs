//! The record of the harness's containers in the reservation file of its PostgreSQL port.

use std::fs::File;
use std::io::Write;

/// One of the harness's three containers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CreationTarget {
	Postgres,
	PgBouncerTransaction,
	PgBouncerStatement,
}

impl CreationTarget {
	pub(crate) const ALL: [Self; 3] = [
		Self::Postgres,
		Self::PgBouncerTransaction,
		Self::PgBouncerStatement,
	];

	fn record_code(self) -> u8 {
		match self {
			Self::Postgres => 1,
			Self::PgBouncerTransaction => 2,
			Self::PgBouncerStatement => 4,
		}
	}

	fn from_record_code(code: &str) -> Option<Self> {
		match code.parse::<u8>().ok()? {
			1 => Some(Self::Postgres),
			2 => Some(Self::PgBouncerTransaction),
			4 => Some(Self::PgBouncerStatement),
			_ => None,
		}
	}

	fn index(self) -> usize {
		match self {
			Self::Postgres => 0,
			Self::PgBouncerTransaction => 1,
			Self::PgBouncerStatement => 2,
		}
	}
}

/// What a reservation record states about the container of one target. It is the part of the
/// container's lifecycle that a later harness needs: whether a container can exist under the
/// target's name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecordedContainer {
	/// No container exists under the target's name: the harness has not asked the runtime to
	/// create one, the runtime rejected the request, or the harness removed the container.
	Absent,
	/// A container can exist under the target's name: the harness sent a create request, and it
	/// has not removed a container since.
	PossiblyPresent,
}

impl RecordedContainer {
	fn record_kind(self) -> u8 {
		match self {
			Self::Absent => b'-',
			Self::PossiblyPresent => b'+',
		}
	}
}

/// The recorded state of each of the three containers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecordedContainers([RecordedContainer; 3]);

impl RecordedContainers {
	pub(crate) fn absent() -> Self {
		Self([RecordedContainer::Absent; 3])
	}

	pub(crate) fn possibly_present() -> Self {
		Self([RecordedContainer::PossiblyPresent; 3])
	}

	pub(crate) fn get(self, target: CreationTarget) -> RecordedContainer {
		self.0[target.index()]
	}

	pub(crate) fn set(&mut self, target: CreationTarget, recorded: RecordedContainer) {
		self.0[target.index()] = recorded;
	}

	/// Applies one line of the record. A line that the harness does not write makes every
	/// container possibly present so that a later harness removes each of them by name.
	pub(super) fn read_event(&mut self, event: &str) {
		let target = event.get(1..).and_then(CreationTarget::from_record_code);
		match (event.as_bytes().first(), target) {
			(Some(b'+'), Some(target)) => self.set(target, RecordedContainer::PossiblyPresent),
			(Some(b'-'), Some(target)) => self.set(target, RecordedContainer::Absent),
			_ => *self = Self::possibly_present(),
		}
	}
}

/// Appends the state of one container to a reservation file. The file is open for appending, so
/// the line lands at the end of the file in one write.
pub(super) fn write_recorded_container(
	mut file: &File,
	target: CreationTarget,
	recorded: RecordedContainer,
) -> std::io::Result<()> {
	let event = format!(
		"\n{}{}",
		char::from(recorded.record_kind()),
		target.record_code()
	);
	file.write_all(event.as_bytes())
}
