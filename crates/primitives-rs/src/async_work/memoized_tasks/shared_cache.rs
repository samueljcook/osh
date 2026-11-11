use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::sync::Arc;

use tokio::time::Instant;

use super::runtime::RunId;
use super::task::{RetentionLimit, RunningEntry};

/// Running work and retained successful results for one task in one runtime.
///
/// Each lookup and each insertion first removes every result that has expired,
/// so an expired result leaves without a lookup of its own input. A result
/// enters and leaves the expiry index once, so the removal adds logarithmic
/// work for each result to the lookup or insertion that removes it.
pub(super) struct SharedCache<Input, Output, Failure> {
	values: HashMap<Input, SharedEntry<Output, Failure>>,
	by_use: BTreeMap<u64, Input>,
	by_expiry: BTreeMap<ExpiryKey, Input>,
	next_use: u64,
}

/// Orders results by expiry. The insertion's use number makes the key unique.
type ExpiryKey = (Instant, u64);

struct RetainedResult<Output> {
	value: Arc<Output>,
	expiry_key: Option<ExpiryKey>,
	used_at: u64,
}

/// A completed run retains its result before its run guard removes its running
/// registration, so a running registration and a retained result share an entry
/// from that retention until the guard drops.
enum SharedEntry<Output, Failure> {
	Running(RunningEntry<Output, Failure>),
	Retained(RetainedResult<Output>),
	RunningWithRetained {
		running: RunningEntry<Output, Failure>,
		retained: RetainedResult<Output>,
	},
}

impl<Output, Failure> SharedEntry<Output, Failure> {
	fn running(&self) -> Option<&RunningEntry<Output, Failure>> {
		match self {
			Self::Running(running) | Self::RunningWithRetained { running, .. } => Some(running),
			Self::Retained(_) => None,
		}
	}

	fn retained(&self) -> Option<&RetainedResult<Output>> {
		match self {
			Self::Retained(retained) | Self::RunningWithRetained { retained, .. } => Some(retained),
			Self::Running(_) => None,
		}
	}

	fn retained_mut(&mut self) -> Option<&mut RetainedResult<Output>> {
		match self {
			Self::Retained(retained) | Self::RunningWithRetained { retained, .. } => Some(retained),
			Self::Running(_) => None,
		}
	}
}

impl<Input: Clone + Eq + Hash, Output, Failure> SharedCache<Input, Output, Failure> {
	pub(crate) fn new() -> Self {
		Self {
			values: HashMap::new(),
			by_use: BTreeMap::new(),
			by_expiry: BTreeMap::new(),
			next_use: 0,
		}
	}

	pub(super) fn get_retained(
		&mut self,
		input: &Input,
		now: Instant,
	) -> (Option<Arc<Output>>, usize) {
		let expired = self.remove_expired(now);
		let Some(entry) = self
			.values
			.get_mut(input)
			.and_then(SharedEntry::retained_mut)
		else {
			return (None, expired);
		};
		self.by_use.remove(&entry.used_at);
		entry.used_at = self.next_use;
		self.by_use.insert(self.next_use, input.clone());
		self.next_use = self
			.next_use
			.checked_add(1)
			.expect("retention use identifiers exhausted");
		(Some(Arc::clone(&entry.value)), expired)
	}

	pub(super) fn retain_result(
		&mut self,
		run_id: RunId,
		insertion: RetainedResultInsertion<Input, Output>,
	) -> Option<(usize, usize)> {
		if !self
			.running(&insertion.input)
			.is_some_and(|running| running.run_id == run_id)
		{
			return None;
		}
		let RetainedResultInsertion {
			input,
			value,
			now,
			expires_at,
			limit,
		} = insertion;
		let expired = self.remove_expired(now);
		self.remove_retained(&input);
		let used_at = self.next_use;
		self.next_use = used_at
			.checked_add(1)
			.expect("retention use identifiers exhausted");
		let expiry_key = expires_at.map(|expiry| (expiry, used_at));
		if let Some(key) = expiry_key {
			self.by_expiry.insert(key, input.clone());
		}
		self.by_use.insert(used_at, input.clone());
		let retained = RetainedResult {
			value,
			expiry_key,
			used_at,
		};
		let entry = match self.values.remove(&input) {
			Some(SharedEntry::Running(running)) => {
				SharedEntry::RunningWithRetained { running, retained }
			}
			None => SharedEntry::Retained(retained),
			Some(SharedEntry::Retained(_) | SharedEntry::RunningWithRetained { .. }) => {
				unreachable!("the prior retained result was removed")
			}
		};
		self.values.insert(input, entry);
		let mut evicted = 0;
		if let RetentionLimit::Bounded(limit) = limit {
			while self.by_use.len() > limit.get() {
				evicted += 1;
				let (_, oldest) = self
					.by_use
					.pop_first()
					.expect("retained values have recency entries");
				self.remove_retained(&oldest);
			}
		}
		Some((expired, evicted))
	}

	pub(crate) fn remove(&mut self, input: &Input) {
		self.remove_retained(input);
		self.values.remove(input);
	}

	fn remove_retained(&mut self, input: &Input) {
		if self
			.values
			.get(input)
			.and_then(SharedEntry::retained)
			.is_none()
		{
			return;
		}
		let retained = match self.values.remove(input) {
			None | Some(SharedEntry::Running(_)) => {
				unreachable!("the selected entry owns a retained result")
			}
			Some(SharedEntry::Retained(retained)) => retained,
			Some(SharedEntry::RunningWithRetained { running, retained }) => {
				self.values
					.insert(input.clone(), SharedEntry::Running(running));
				retained
			}
		};
		self.by_use.remove(&retained.used_at);
		if let Some(key) = retained.expiry_key {
			self.by_expiry.remove(&key);
		}
	}

	pub(crate) fn remove_where(&mut self, mut predicate_fn: impl FnMut(&Input) -> bool) {
		let running: Vec<_> = self
			.values
			.iter()
			.filter_map(|(input, entry)| {
				let running = entry.running()?;
				predicate_fn(input).then(|| (input.clone(), running.run_id))
			})
			.collect();
		for (input, run_id) in running {
			self.remove_running(&input, run_id);
		}
		let retained: Vec<_> = self
			.values
			.iter()
			.filter_map(|(input, entry)| {
				entry.retained()?;
				predicate_fn(input).then(|| input.clone())
			})
			.collect();
		for input in retained {
			self.remove_retained(&input);
		}
	}

	pub(super) fn running(&self, input: &Input) -> Option<&RunningEntry<Output, Failure>> {
		self.values.get(input).and_then(SharedEntry::running)
	}

	pub(super) fn start_run(&mut self, input: Input, running: RunningEntry<Output, Failure>) {
		let entry = match self.values.remove(&input) {
			None | Some(SharedEntry::Running(_)) => SharedEntry::Running(running),
			// A task that retains results looks up its retained result before it
			// starts a run, and a task that does not retain results has none.
			Some(SharedEntry::Retained(_) | SharedEntry::RunningWithRetained { .. }) => {
				unreachable!("a shared run starts only for an input without a retained result")
			}
		};
		self.values.insert(input, entry);
	}

	pub(super) fn remove_running(&mut self, input: &Input, run_id: RunId) {
		if !self
			.running(input)
			.is_some_and(|entry| entry.run_id == run_id)
		{
			return;
		}
		match self
			.values
			.remove(input)
			.expect("the selected running entry exists")
		{
			SharedEntry::Running(_) => {}
			SharedEntry::RunningWithRetained { retained, .. } => {
				self.values
					.insert(input.clone(), SharedEntry::Retained(retained));
			}
			SharedEntry::Retained(_) => unreachable!("the selected entry owns a run"),
		}
	}

	pub(crate) fn clear(&mut self) {
		self.values.clear();
		self.by_use.clear();
		self.by_expiry.clear();
	}

	/// Removes every result that expires at or before `now`.
	fn remove_expired(&mut self, now: Instant) -> usize {
		let mut count = 0;
		while let Some(entry) = self.by_expiry.first_entry()
			&& entry.key().0 <= now
		{
			let (_, input) = entry.remove_entry();
			count += 1;
			self.remove_retained(&input);
		}
		count
	}
}

/// A result to retain, and the clock reading and limit that apply to it.
pub(crate) struct RetainedResultInsertion<Input, Output> {
	pub(crate) input: Input,
	pub(crate) value: Arc<Output>,
	pub(crate) now: Instant,
	/// `None` when the result never expires.
	pub(crate) expires_at: Option<Instant>,
	pub(crate) limit: RetentionLimit,
}
