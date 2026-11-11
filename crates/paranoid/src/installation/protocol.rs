use crate::db::{
	AdmissionError, DatabaseError, DatabaseOperationKind, PgQualifiedTableName, ProtocolError, Tx,
	owned_portable_query_as,
};
use std::borrow::Cow;
use std::sync::Arc;

const MAX_PROTOCOL_FINGERPRINT_LEN: usize = 256;

/// Compatibility epoch of the state that pre.9 and pre.10 instances write.
pub(crate) const LEGACY_PROTOCOL: ProtocolDef =
	ProtocolDef::new(1, "paranoid.protocol.pre9-state.v1");

/// Epoch and immutable definition identifier stored in the protocol relation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProtocolDef {
	pub(crate) epoch: i32,
	pub(crate) fingerprint: Cow<'static, str>,
}

impl ProtocolDef {
	pub(crate) const fn new(epoch: i32, fingerprint: &'static str) -> Self {
		Self {
			epoch,
			fingerprint: Cow::Borrowed(fingerprint),
		}
	}

	fn validate_domain(&self) -> Result<(), ProtocolError> {
		if self.epoch < 1
			|| self.fingerprint.is_empty()
			|| self.fingerprint.len() > MAX_PROTOCOL_FINGERPRINT_LEN
			|| self.fingerprint.contains('\0')
		{
			return Err(ProtocolError::Config {
				reason: "protocol definition is outside its declared domain",
			});
		}
		Ok(())
	}
}

/// Lock that a protocol read holds on the protocol relation until the transaction ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProtocolLock {
	/// Admission and preparation exclude activation but not each other.
	Shared,
	/// Activation excludes every admitted transaction.
	Exclusive,
}

/// Declared direct transitions into one target definition.
///
/// A source is accepted for upgrade only when it is declared. Nothing infers a
/// path through intermediate epochs, and accepting a source for upgrade does
/// not admit ordinary operations under it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProtocolUpgradePolicy {
	target: ProtocolDef,
	sources: Vec<ProtocolDef>,
}

/// Result of comparing the active definition with an upgrade policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProtocolUpgradePlan {
	/// The active definition is the target.
	AlreadyCurrent,
	/// The active definition is a declared source of the target.
	Upgrade,
}

impl ProtocolUpgradePolicy {
	/// Sources must be unique epochs earlier than the target.
	pub(crate) fn new(
		target: ProtocolDef,
		sources: Vec<ProtocolDef>,
	) -> Result<Self, ProtocolError> {
		target.validate_domain()?;
		for (index, source) in sources.iter().enumerate() {
			source.validate_domain()?;
			if source.epoch >= target.epoch
				|| sources[..index]
					.iter()
					.any(|earlier| earlier.epoch == source.epoch)
			{
				return Err(ProtocolError::Config {
					reason: "upgrade sources must be unique earlier epochs",
				});
			}
		}
		Ok(Self { target, sources })
	}

	pub(crate) fn target(&self) -> &ProtocolDef {
		&self.target
	}

	pub(crate) fn plan(&self, active: &ProtocolDef) -> Result<ProtocolUpgradePlan, ProtocolError> {
		if active.epoch == self.target.epoch {
			check_protocol_def(active, &self.target)?;
			return Ok(ProtocolUpgradePlan::AlreadyCurrent);
		}
		match self
			.sources
			.iter()
			.find(|source| source.epoch == active.epoch)
		{
			Some(source) => {
				check_protocol_def(active, source)?;
				Ok(ProtocolUpgradePlan::Upgrade)
			}
			None => Err(ProtocolError::Incompatible {
				active_epoch: active.epoch,
				supported_epoch: self.target.epoch,
				reason: "unsupported epoch",
			}),
		}
	}
}

fn check_protocol_def(active: &ProtocolDef, expected: &ProtocolDef) -> Result<(), ProtocolError> {
	if active == expected {
		return Ok(());
	}
	Err(ProtocolError::Incompatible {
		active_epoch: active.epoch,
		supported_epoch: expected.epoch,
		reason: if active.epoch == expected.epoch {
			"fingerprint mismatch"
		} else {
			"unsupported epoch"
		},
	})
}

/// Identity of the transaction admission boundary shared by one Paranoid installation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Protocol {
	table_name: PgQualifiedTableName,
	admission_sql: Arc<str>,
	activation_lock_sql: Arc<str>,
}

/// Protocol bound to the relation of a fixed test installation.
#[cfg(test)]
pub(crate) fn protocol_for_tests() -> Protocol {
	let schema_name = crate::db::PgSchemaName::from_identifier_text("__paranoid_test_admission")
		.expect("test protocol schema");
	let table =
		crate::db::PgIdentifier::new(crate::installation::v10::BOOTSTRAP_PROTOCOL_TABLE_NAME)
			.expect("bootstrap protocol table name");
	Protocol::new(PgQualifiedTableName::new(schema_name, table))
}

impl Protocol {
	pub(crate) fn new(table_name: PgQualifiedTableName) -> Self {
		let table = table_name.quoted();
		let admission_sql = format!(
			"LOCK TABLE ONLY {table} IN ACCESS SHARE MODE; \
             SELECT singleton, epoch, fingerprint, \
             pg_catalog.current_setting('transaction_isolation') \
             FROM ONLY {table} LIMIT 2"
		)
		.into();
		let activation_lock_sql = format!(
			"LOCK TABLE ONLY {table} IN ACCESS EXCLUSIVE MODE; \
             SELECT singleton, epoch, fingerprint, \
             pg_catalog.current_setting('transaction_isolation') \
             FROM ONLY {table} LIMIT 2"
		)
		.into();
		Self {
			table_name,
			admission_sql,
			activation_lock_sql,
		}
	}

	pub(crate) fn table_name(&self) -> &PgQualifiedTableName {
		&self.table_name
	}

	/// Locks the protocol relation, then reads its one control record in the same transaction.
	pub(crate) async fn read_def(
		&self,
		tx: &mut Tx<'_>,
		lock: ProtocolLock,
		label: &'static str,
	) -> Result<ProtocolDef, AdmissionError> {
		let statement = match lock {
			ProtocolLock::Shared => &self.admission_sql,
			ProtocolLock::Exclusive => &self.activation_lock_sql,
		};
		tx.record_database_operation(DatabaseOperationKind::FetchAll, label, Some(statement));
		let rows = owned_portable_query_as::<(i32, i32, String, String)>(sqlx::AssertSqlSafe(
			statement.as_ref(),
		))
		.fetch_all(tx.inner.as_mut())
		.await
		.map_err(|error| protocol_query_error(label, &self.table_name, error))?;
		let [(singleton, epoch, fingerprint, isolation)] = rows.as_slice() else {
			return Err(ProtocolError::State {
				reason: "expected exactly one protocol control record",
			}
			.into());
		};
		if *singleton != 1
			|| *epoch < 1
			|| fingerprint.is_empty()
			|| fingerprint.len() > MAX_PROTOCOL_FINGERPRINT_LEN
		{
			return Err(ProtocolError::State {
				reason: "protocol control record is outside its declared domain",
			}
			.into());
		}
		if isolation != "read committed" {
			return Err(ProtocolError::State {
				reason: "protocol admission requires a READ COMMITTED transaction",
			}
			.into());
		}
		Ok(ProtocolDef {
			epoch: *epoch,
			fingerprint: Cow::Owned(fingerprint.clone()),
		})
	}
}

fn protocol_query_error(
	label: &'static str,
	table_name: &PgQualifiedTableName,
	error: DatabaseError,
) -> AdmissionError {
	if matches!(
		error.driver_error(),
		sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_) | sqlx::Error::ColumnNotFound(_)
	) {
		return ProtocolError::State {
			reason: "protocol control record cannot be decoded",
		}
		.into();
	}
	let code = error.sql_state().map(|sql_state| sql_state.as_str());
	// The statement names only the protocol relation, so a missing schema (3F000) and a missing
	// relation (42P01) both mean that the relation does not exist.
	if matches!(code, Some("3F000" | "42P01")) {
		ProtocolError::RelationAbsent {
			relation: table_name.quoted().to_string(),
		}
		.into()
	} else if matches!(code, Some("42703" | "42809")) {
		ProtocolError::State {
			reason: "protocol relation has an incompatible shape",
		}
		.into()
	} else {
		error.with_operation_label(label).into()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const SOURCE: ProtocolDef = ProtocolDef::new(2, "source");
	const TARGET: ProtocolDef = ProtocolDef::new(3, "target");

	fn policy() -> ProtocolUpgradePolicy {
		ProtocolUpgradePolicy::new(TARGET, vec![SOURCE]).expect("policy")
	}

	fn reason(error: ProtocolError) -> &'static str {
		match error {
			ProtocolError::Incompatible { reason, .. } => reason,
			other => panic!("expected an incompatibility, got {other:?}"),
		}
	}

	#[test]
	fn a_declared_source_plans_an_upgrade_and_the_target_is_already_current() {
		assert_eq!(policy().target(), &TARGET);
		assert_eq!(
			policy().plan(&SOURCE).unwrap(),
			ProtocolUpgradePlan::Upgrade
		);
		assert_eq!(
			policy().plan(&TARGET).unwrap(),
			ProtocolUpgradePlan::AlreadyCurrent
		);
	}

	#[test]
	fn an_unknown_epoch_and_a_fingerprint_mismatch_are_distinct_incompatibilities() {
		assert_eq!(
			reason(policy().plan(&ProtocolDef::new(1, "unknown")).unwrap_err()),
			"unsupported epoch"
		);
		for active in [ProtocolDef::new(2, "wrong"), ProtocolDef::new(3, "wrong")] {
			assert_eq!(
				reason(policy().plan(&active).unwrap_err()),
				"fingerprint mismatch"
			);
		}
	}

	#[test]
	fn defs_outside_their_domain_are_rejected() {
		let too_long = "x".repeat(257);
		for def in [
			ProtocolDef::new(0, "target"),
			ProtocolDef::new(-1, "target"),
			ProtocolDef::new(3, ""),
			ProtocolDef::new(3, "nul\0fingerprint"),
			ProtocolDef {
				epoch: 3,
				fingerprint: Cow::Owned(too_long),
			},
		] {
			assert!(
				matches!(
					ProtocolUpgradePolicy::new(def.clone(), vec![]),
					Err(ProtocolError::Config { .. })
				),
				"{def:?}"
			);
		}
		assert!(ProtocolUpgradePolicy::new(ProtocolDef::new(i32::MAX, "target"), vec![]).is_ok());
	}

	#[test]
	fn sources_must_be_unique_epochs_earlier_than_the_target() {
		for sources in [
			vec![SOURCE, ProtocolDef::new(2, "other")],
			vec![TARGET],
			vec![ProtocolDef::new(4, "future")],
		] {
			assert!(
				matches!(
					ProtocolUpgradePolicy::new(TARGET, sources.clone()),
					Err(ProtocolError::Config { .. })
				),
				"{sources:?}"
			);
		}
	}
}
