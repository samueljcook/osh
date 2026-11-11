use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy)]
#[repr(u8)]
enum StatementFailureState {
	NoFailureObserved,
	FailureObserved,
}

/// A failed statement requires the commit probe even after application savepoint cleanup.
#[derive(Debug)]
pub(super) struct StatementFailureTracker(AtomicU8);

impl StatementFailureTracker {
	pub(super) fn new() -> Self {
		Self(AtomicU8::new(
			StatementFailureState::NoFailureObserved as u8,
		))
	}

	pub(super) fn note_failure(&self) {
		self.0.store(
			StatementFailureState::FailureObserved as u8,
			Ordering::Relaxed,
		);
	}

	pub(super) fn has_observed_failure(&self) -> bool {
		match self.0.load(Ordering::Relaxed) {
			value if value == StatementFailureState::NoFailureObserved as u8 => false,
			value if value == StatementFailureState::FailureObserved as u8 => true,
			_ => unreachable!("statement failure states are NoFailureObserved and FailureObserved"),
		}
	}
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum StatementInterruptionState {
	Uninterrupted,
	Interrupted,
}

/// Interruption is irreversible because no later borrow can read the abandoned answer.
pub(super) struct StatementInterruption(AtomicU8);

impl StatementInterruption {
	pub(super) fn new() -> Self {
		Self(AtomicU8::new(
			StatementInterruptionState::Uninterrupted as u8,
		))
	}

	pub(super) fn mark_interrupted(&self) {
		self.0.store(
			StatementInterruptionState::Interrupted as u8,
			Ordering::Release,
		);
	}

	pub(super) fn was_interrupted(&self) -> bool {
		match self.0.load(Ordering::Acquire) {
			value if value == StatementInterruptionState::Uninterrupted as u8 => false,
			value if value == StatementInterruptionState::Interrupted as u8 => true,
			_ => unreachable!("statement interruption states are Uninterrupted and Interrupted"),
		}
	}
}
