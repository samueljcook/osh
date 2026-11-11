//! Complete per-container native plans derived before any placed mutation is applied.

use std::collections::{BTreeMap, BTreeSet};

use crate::db::PgQualifiedTableName;
use crate::records::{InvalidGroupMutationError, RecordIdentity, RecordMutation};

use super::super::group::PreparedPgRecordGroupMutation;
use super::{HeldContainer, PgPlacedRecordIdentity, RecordSetRejectionError};

pub(super) struct PreparedRecordSetMutation {
	pub(super) changes: BTreeMap<PgQualifiedTableName, PreparedPgRecordGroupMutation>,
	pub(super) cleanup: BTreeMap<PgQualifiedTableName, BTreeSet<RecordIdentity>>,
}

impl PreparedRecordSetMutation {
	pub(super) fn prepare(
		held: &BTreeMap<PgQualifiedTableName, HeldContainer<'_>>,
		actions: Vec<(PgPlacedRecordIdentity, RecordMutation)>,
	) -> Result<Self, RecordSetRejectionError> {
		let mut by_container: BTreeMap<_, Vec<_>> = BTreeMap::new();
		for (identity, action) in actions {
			if !held.contains_key(identity.table()) {
				let source = InvalidGroupMutationError::UnknownIdentity {
					identity: identity.record().clone(),
				};
				return Err(RecordSetRejectionError::mutation(source));
			}
			by_container
				.entry(identity.table)
				.or_default()
				.push((identity.record, action));
		}
		let mut prepared_changes = BTreeMap::new();
		let mut prepared_cleanup = BTreeMap::new();
		for (table, container) in held {
			let changes = PreparedPgRecordGroupMutation::prepare_records(
				&container.records,
				by_container.remove(table).unwrap_or_default(),
			)
			.map_err(RecordSetRejectionError::mutation)?;
			let cleanup = changes.inserted_keep_reservations(&container.inserted_reservations());
			prepared_changes.insert(table.clone(), changes);
			prepared_cleanup.insert(table.clone(), cleanup);
		}
		Ok(Self {
			changes: prepared_changes,
			cleanup: prepared_cleanup,
		})
	}
}
