//! Turns that let one acquisition at a time of each record begin a transaction in one runtime.
//!
//! Every acquisition of a Throttler, Semaphore, or lease locks its key's record in the database,
//! so the acquisitions of one key in one runtime run one after another in any case. Before an
//! acquisition begins its transaction, it waits for the record's turn, in the order of arrival.
//! A waiting acquisition holds no connection and no transaction, so a burst of acquisitions on
//! one key holds one connection at a time, and the burst does not delay the runtime's other
//! work on the same pool.

use std::collections::{BTreeMap, VecDeque};

use tokio::sync::oneshot;

use super::{Arc, RecordIdentity, RuntimeAdmissionTracker, SharedAdmission};

/// The waiting acquisitions of each record with an acquired turn. A record has an entry exactly
/// while one acquisition holds its turn.
#[derive(Default)]
pub(super) struct RecordTurnQueues(BTreeMap<RecordIdentity, VecDeque<oneshot::Sender<()>>>);

impl SharedAdmission {
	fn lock_record_turns(&self) -> std::sync::MutexGuard<'_, RecordTurnQueues> {
		self.record_turns
			.lock()
			.expect("record turn queues are not poisoned")
	}

	/// Hands the turn of `record` to the longest waiting acquisition that still waits, or ends the
	/// turn when none waits.
	fn pass_record_turn(&self, record: &RecordIdentity) {
		let mut queues = self.lock_record_turns();
		let waiters = queues
			.0
			.get_mut(record)
			.expect("a held turn retains its queue");
		while let Some(waiter) = waiters.pop_front() {
			if waiter.send(()).is_ok() {
				return;
			}
		}
		queues.0.remove(record);
	}
}

impl RuntimeAdmissionTracker {
	/// Waits for the turn of `record` and returns it. The turn ends when the returned value is
	/// dropped. Cancelling the wait leaves the order of the other waiting acquisitions unchanged.
	pub(crate) async fn take_record_turn(&self, record: &RecordIdentity) -> RecordTurn {
		let receiver = {
			let mut queues = self.0.lock_record_turns();
			match queues.0.get_mut(record) {
				Some(waiters) => {
					let (sender, receiver) = oneshot::channel();
					waiters.push_back(sender);
					receiver
				}
				None => {
					queues.0.insert(record.clone(), VecDeque::new());
					return RecordTurn {
						shared: Arc::clone(&self.0),
						record: record.clone(),
					};
				}
			}
		};
		let mut wait = RecordTurnWait {
			shared: Arc::clone(&self.0),
			record: record.clone(),
			state: RecordTurnWaitState::Waiting(receiver),
		};
		match &mut wait.state {
			RecordTurnWaitState::Waiting(receiver) => receiver
				.await
				.expect("only a handover removes a waiting acquisition's sender"),
			RecordTurnWaitState::Claimed => {
				unreachable!("a waiting acquisition retains its receiver")
			}
		}
		wait.state = RecordTurnWaitState::Claimed;
		RecordTurn {
			shared: Arc::clone(&self.0),
			record: record.clone(),
		}
	}
}

/// The turn of one record. Dropping it hands the turn to the next waiting acquisition.
pub(crate) struct RecordTurn {
	shared: Arc<SharedAdmission>,
	record: RecordIdentity,
}

impl Drop for RecordTurn {
	fn drop(&mut self) {
		self.shared.pass_record_turn(&self.record);
	}
}

/// A wait for a record's turn. Dropping it after the turn was handed to it, but before the
/// waiting acquisition took the turn, hands the turn on.
struct RecordTurnWait {
	shared: Arc<SharedAdmission>,
	record: RecordIdentity,
	state: RecordTurnWaitState,
}

enum RecordTurnWaitState {
	Waiting(oneshot::Receiver<()>),
	Claimed,
}

impl Drop for RecordTurnWait {
	fn drop(&mut self) {
		if let RecordTurnWaitState::Waiting(mut receiver) =
			std::mem::replace(&mut self.state, RecordTurnWaitState::Claimed)
		{
			receiver.close();
			if receiver.try_recv().is_ok() {
				self.shared.pass_record_turn(&self.record);
			}
		}
	}
}
