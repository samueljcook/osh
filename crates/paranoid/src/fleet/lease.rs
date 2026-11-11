use super::error::{ClaimError, OperationError, RandomnessError};
use std::time::Duration as StdDuration;

/// Test-only lease backing table name.
#[cfg(test)]
pub const TEST_LEASE_TABLE_NAME: &str = "__paranoid_lease_store";

/// Test-only lease fencing counter backing table name.
#[cfg(test)]
pub const TEST_LEASE_FENCING_COUNTER_TABLE_NAME: &str = "__paranoid_lease_fencing_counters";

/// Separator used when composing persisted lease keys from key parts.
pub const LEASE_KEY_SEPARATOR: &str = "::";

/// Maximum accepted persisted lease key length, in bytes.
pub const MAX_LEASE_KEY_LEN: usize = 2048;

/// Maximum accepted lease holder identifier length, in bytes.
pub const MAX_HOLDER_ID_LEN: usize = 512;

/// Minimum accepted positive lease duration.
pub const MIN_CLAIM_DURATION: StdDuration = StdDuration::from_secs(1);

const LEASE_TOKEN_LEN: usize = 32;

pub(crate) mod cleanup_target;
mod driver_io;
pub(crate) mod driver_timing;
pub(crate) mod engine;
mod error;
pub(crate) mod handle;
mod model;
pub(crate) mod owned_driver;
mod state;
pub(crate) mod v10;

pub(crate) use engine::{
	NativeLeaseBindings, NativeLeaseError, NativeLeaseSourceBindings, PgNativeLease,
};
pub use error::{
	ClaimDurationError, CoordinationKeyError, HolderIdError, HolderPrefixError, PersistedLeaseError,
};
pub(crate) use model::Token;
pub use model::{
	Claim, ClaimDuration, FencingToken, HolderId, HolderPrefix, HolderSnapshot, LeaseKey,
	MAX_HOLDER_PREFIX_LEN,
};
pub(crate) use state::{
	LeaseAcquisition, LeaseBindingError, LeaseComponent, TentativeLeaseAuthority,
};
#[cfg(test)]
use v10::schema::validate_distinct_table_names;
pub(crate) use v10::schema::validate_schema_in_current_transaction;
pub(crate) use v10::source_bridge::LeaseSourceInitialPolicyError;
pub(crate) use v10::source_bridge::{LeaseSourceBindingError, LeaseSourceBridge};
pub use v10::store::StoreConfig;

pub const LEASE_ACQUIRE_LABEL: &str = "lease.acquire";
pub const LEASE_RELEASE_LABEL: &str = "lease.release";
pub const LEASE_RELEASE_UNDELIVERED_LABEL: &str = "lease.release_undelivered";
pub const LEASE_RENEW_LABEL: &str = "lease.renew";
pub const LEASE_SCHEMA_VALIDATE_COLUMNS_LABEL: &str = "lease.schema.validate_columns";
pub const LEASE_SCHEMA_VALIDATE_KEY_CONFLICT_ARBITER_LABEL: &str =
	"lease.schema.validate_key_conflict_arbiter";
pub const LEASE_SCHEMA_VALIDATE_CHECK_CONSTRAINTS_LABEL: &str =
	"lease.schema.validate_check_constraints";
pub const LEASE_SCHEMA_VALIDATE_EXPIRES_AT_INDEX_LABEL: &str =
	"lease.schema.validate_expires_at_index";

#[cfg(test)]
mod tests {
	use super::*;
	use crate::db::{PgIdentifier, PgQualifiedTableName, PgSchemaName};

	#[test]
	fn lease_store_config_rejects_identical_state_and_fencing_counter_tables() {
		let first_table = PgQualifiedTableName::new(
			PgSchemaName::from_identifier_text("public").expect("valid public-qualified table"),
			PgIdentifier::new("__paranoid_same_lease_table").expect("valid public-qualified table"),
		);
		let second_table = PgQualifiedTableName::new(
			PgSchemaName::from_identifier_text("public").expect("valid public-qualified table"),
			PgIdentifier::new("__paranoid_same_lease_table").expect("valid public-qualified table"),
		);

		let config = StoreConfig::new_with_fencing_counter_table(
			first_table,
			second_table,
			crate::installation::protocol::protocol_for_tests(),
		);

		let error = validate_distinct_table_names(&config).expect_err("identical table names");
		assert!(
			error
				.to_string()
				.contains("lease state and fencing counter table names must be distinct")
		);
	}

	#[test]
	fn lease_key_from_parts_validates_and_joins_key_parts() {
		let key = LeaseKey::from_parts(["fleet", "leader"]).expect("key");
		assert_eq!(key.as_str(), "fleet::leader::");

		assert!(matches!(
			LeaseKey::from_parts::<&str, _>([]),
			Err(CoordinationKeyError::Empty)
		));
		assert!(matches!(
			LeaseKey::from_parts([""]),
			Err(CoordinationKeyError::EmptyPart)
		));
		assert!(matches!(
			LeaseKey::from_parts(["has:colon"]),
			Err(CoordinationKeyError::PartContainsSeparatorByte)
		));
		assert!(matches!(
			LeaseKey::from_parts(["has\0null"]),
			Err(CoordinationKeyError::PartContainsNullByte)
		));
	}

	#[test]
	fn lease_holder_id_validates_length_and_null_bytes() {
		assert!(HolderId::new("worker-1").is_ok());
		assert!(matches!(HolderId::new(""), Err(HolderIdError::Empty)));
		assert!(matches!(
			HolderId::new("bad\0holder"),
			Err(HolderIdError::ContainsNullByte)
		));
		assert!(matches!(
			HolderId::new("a".repeat(MAX_HOLDER_ID_LEN + 1)),
			Err(HolderIdError::TooLong { .. })
		));
	}

	#[test]
	fn lease_duration_rejects_zero_too_small_and_too_large_values() {
		assert!(matches!(
			ClaimDuration::new(StdDuration::ZERO),
			Err(ClaimDurationError::Zero)
		));
		assert!(matches!(
			ClaimDuration::new(StdDuration::from_millis(999)),
			Err(ClaimDurationError::BelowMinimum { .. })
		));
		assert!(matches!(
			ClaimDuration::new(StdDuration::from_micros(i64::MAX as u64 + 1)),
			Err(ClaimDurationError::TooLarge)
		));
		assert!(ClaimDuration::new(MIN_CLAIM_DURATION).is_ok());
		assert_eq!(
			ClaimDuration::new(StdDuration::from_secs(1) + StdDuration::from_nanos(1))
				.expect("duration")
				.positive_micros(),
			1_000_001
		);
	}

	#[test]
	fn a_persisted_fencing_token_is_positive() {
		for value in [i64::MIN, -1, 0] {
			assert!(matches!(
				FencingToken::from_i64(value),
				Err(PersistedLeaseError::FencingToken { value: rejected }) if rejected == value
			));
		}
		assert_eq!(FencingToken::from_i64(7).expect("token").as_i64(), 7);
		assert_eq!(
			FencingToken::from_i64(i64::MAX).expect("token").as_i64(),
			i64::MAX
		);
	}

	#[test]
	fn a_persisted_claim_token_has_exactly_the_token_length() {
		for length in [0, 31, 33] {
			assert!(matches!(
				Token::from_persisted_bytes(vec![1; length]),
				Err(PersistedLeaseError::TokenLength { actual, expected })
					if actual == length && expected == LEASE_TOKEN_LEN
			));
		}
		let token = Token::from_persisted_bytes(vec![9; LEASE_TOKEN_LEN]).expect("token");
		assert_eq!(token.as_bytes(), &[9; LEASE_TOKEN_LEN]);
		assert!(!format!("{token:?}").contains('9'));
		assert_ne!(
			Token::generate().expect("token"),
			Token::generate().expect("token")
		);
	}

	#[test]
	fn holder_prefixes_leave_room_for_the_separator_and_a_sortable_identifier() {
		assert!(HolderPrefix::new("api-7f3c").is_ok());
		assert!(matches!(
			HolderPrefix::new(""),
			Err(HolderPrefixError::Empty)
		));
		assert!(matches!(
			HolderPrefix::new("bad\0prefix"),
			Err(HolderPrefixError::ContainsNullByte)
		));
		assert!(HolderPrefix::new("a".repeat(MAX_HOLDER_PREFIX_LEN)).is_ok());
		assert!(matches!(
			HolderPrefix::new("a".repeat(MAX_HOLDER_PREFIX_LEN + 1)),
			Err(HolderPrefixError::TooLong { .. })
		));
		let longest = HolderPrefix::new("a".repeat(MAX_HOLDER_PREFIX_LEN)).expect("prefix");
		assert_eq!(
			longest.generate_holder_id().expect("holder").as_str().len(),
			MAX_HOLDER_ID_LEN
		);
	}

	#[test]
	fn a_generated_holder_is_the_prefix_a_separator_and_a_new_sortable_identifier() {
		let prefix = HolderPrefix::new("api:7f3c").expect("prefix");
		let first = prefix.generate_holder_id().expect("holder");
		let second = prefix.generate_holder_id().expect("holder");
		for holder in [&first, &second] {
			let identifier = holder
				.as_str()
				.strip_prefix("api:7f3c:")
				.expect("the holder starts with the prefix and a separator");
			assert_eq!(
				identifier.len(),
				primitives_rs::id::time_sortable::SORTABLE_ID_TEXT_LEN
			);
			assert!(
				identifier
					.bytes()
					.all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte)),
				"{identifier}"
			);
		}
		assert_ne!(first, second);
	}
}
