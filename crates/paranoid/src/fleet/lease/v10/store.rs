use crate::db::PgQualifiedTableName;

/// Schema configuration for the Postgres-backed lease primitive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreConfig {
	/// Shared installation protocol identity.
	pub protocol: crate::installation::Protocol,
	/// Backing table for lease rows.
	pub claim_table: PgQualifiedTableName,
	/// Backing table for durable per-key fencing counters.
	pub fencing_counter_table: PgQualifiedTableName,
}

#[cfg(test)]
impl Default for StoreConfig {
	fn default() -> Self {
		Self {
			protocol: crate::installation::protocol::protocol_for_tests(),
			claim_table: crate::db::table_name_for_tests(
				crate::fleet::lease::TEST_LEASE_TABLE_NAME,
			),
			fencing_counter_table: crate::db::table_name_for_tests(
				crate::fleet::lease::TEST_LEASE_FENCING_COUNTER_TABLE_NAME,
			),
		}
	}
}

impl StoreConfig {
	/// Creates a lease store config with explicit validated table names.
	pub(crate) fn new_with_fencing_counter_table(
		claim_table: PgQualifiedTableName,
		fencing_counter_table: PgQualifiedTableName,
		protocol: crate::installation::Protocol,
	) -> Self {
		Self {
			claim_table,
			fencing_counter_table,
			protocol,
		}
	}
}
