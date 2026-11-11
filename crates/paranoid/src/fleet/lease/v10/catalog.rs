use crate::db::{PgIdentifier, migration_object_identifier};
use crate::fleet::lease::{LEASE_TOKEN_LEN, MAX_HOLDER_ID_LEN, MAX_LEASE_KEY_LEN, StoreConfig};

const INDEX_KIND: &str = "idx";
const BYTEWISE_TEXT_COLLATIONS: [&str; 2] = ["C", "POSIX"];
const NO_COLLATIONS: [&str; 0] = [];

pub(super) const LEASE_STATE_COLUMNS: [LeaseColumn; 6] = [
	LeaseColumn::LeaseKey,
	LeaseColumn::HolderId,
	LeaseColumn::FencingToken,
	LeaseColumn::LeaseToken,
	LeaseColumn::ExpiresAt,
	LeaseColumn::UpdatedAt,
];
pub(super) const LEASE_FENCING_COUNTER_COLUMNS: [LeaseColumn; 3] = [
	LeaseColumn::LeaseKey,
	LeaseColumn::LastFencingToken,
	LeaseColumn::UpdatedAt,
];
pub(super) const LEASE_STATE_CHECKED_COLUMNS: [LeaseColumn; 4] = [
	LeaseColumn::LeaseKey,
	LeaseColumn::HolderId,
	LeaseColumn::FencingToken,
	LeaseColumn::LeaseToken,
];
pub(super) const LEASE_FENCING_COUNTER_CHECKED_COLUMNS: [LeaseColumn; 2] =
	[LeaseColumn::LeaseKey, LeaseColumn::LastFencingToken];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeaseColumn {
	LeaseKey,
	HolderId,
	FencingToken,
	LeaseToken,
	ExpiresAt,
	UpdatedAt,
	LastFencingToken,
}

impl LeaseColumn {
	pub(crate) const fn name(self) -> &'static str {
		match self {
			Self::LeaseKey => "key",
			Self::HolderId => "holder_id",
			Self::FencingToken => "fencing_token",
			Self::LeaseToken => "lease_token",
			Self::ExpiresAt => "expires_at",
			Self::UpdatedAt => "updated_at",
			Self::LastFencingToken => "last_fencing_token",
		}
	}

	pub(super) const fn validation_type(self) -> &'static str {
		match self {
			Self::LeaseKey | Self::HolderId => "text",
			Self::FencingToken | Self::LastFencingToken => "bigint",
			Self::LeaseToken => "bytea",
			Self::ExpiresAt | Self::UpdatedAt => "timestamp with time zone",
		}
	}

	pub(super) fn allowed_collations(self) -> &'static [&'static str] {
		match self {
			Self::LeaseKey | Self::HolderId => &BYTEWISE_TEXT_COLLATIONS,
			Self::FencingToken
			| Self::LeaseToken
			| Self::ExpiresAt
			| Self::UpdatedAt
			| Self::LastFencingToken => &NO_COLLATIONS,
		}
	}

	pub(super) fn normalized_check_constraint(self) -> Option<String> {
		match self.check_constraint() {
			Some(LeaseCheckConstraint::NonEmptyMaxOctetLength(max_octets)) => {
				let column = self.name();
				Some(format!(
					"(octet_length({column})>0)AND(octet_length({column})<={max_octets})"
				))
			}
			Some(LeaseCheckConstraint::Positive) => Some(format!("{}>0", self.name())),
			Some(LeaseCheckConstraint::ExactOctetLength(octets)) => {
				Some(format!("octet_length({})={octets}", self.name()))
			}
			None => None,
		}
	}

	pub(super) fn human_check_constraint(self) -> Option<String> {
		match self.check_constraint() {
			Some(LeaseCheckConstraint::NonEmptyMaxOctetLength(max_octets)) => {
				let column = self.name();
				Some(format!(
					"pg_catalog.octet_length({column}) OPERATOR(pg_catalog.>) 0 AND pg_catalog.octet_length({column}) OPERATOR(pg_catalog.<=) {max_octets}"
				))
			}
			Some(LeaseCheckConstraint::Positive) => {
				Some(format!("{} OPERATOR(pg_catalog.>) 0", self.name()))
			}
			Some(LeaseCheckConstraint::ExactOctetLength(octets)) => Some(format!(
				"pg_catalog.octet_length({}) OPERATOR(pg_catalog.=) {octets}",
				self.name()
			)),
			None => None,
		}
	}

	pub(super) fn qualified(self, table_alias: &str) -> String {
		format!("{table_alias}.{}", self.name())
	}

	fn check_constraint(self) -> Option<LeaseCheckConstraint> {
		match self {
			Self::LeaseKey => Some(LeaseCheckConstraint::NonEmptyMaxOctetLength(
				MAX_LEASE_KEY_LEN,
			)),
			Self::HolderId => Some(LeaseCheckConstraint::NonEmptyMaxOctetLength(
				MAX_HOLDER_ID_LEN,
			)),
			Self::FencingToken | Self::LastFencingToken => Some(LeaseCheckConstraint::Positive),
			Self::LeaseToken => Some(LeaseCheckConstraint::ExactOctetLength(LEASE_TOKEN_LEN)),
			Self::ExpiresAt | Self::UpdatedAt => None,
		}
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LeaseCheckConstraint {
	NonEmptyMaxOctetLength(usize),
	Positive,
	ExactOctetLength(usize),
}

pub(super) fn expires_at_unix_micros_expression(table_alias: Option<&str>) -> String {
	let expires_at = table_alias.map_or_else(
		|| LeaseColumn::ExpiresAt.name().to_owned(),
		|alias| LeaseColumn::ExpiresAt.qualified(alias),
	);
	format!(
		"(pg_catalog.floor(EXTRACT(EPOCH FROM {expires_at}) OPERATOR(pg_catalog.*) 1000000)::bigint)"
	)
}

pub(super) fn migration_index_identifier(
	config: &StoreConfig,
	suffix: &'static str,
) -> PgIdentifier {
	migration_object_identifier(INDEX_KIND, &config.claim_table, suffix)
}
