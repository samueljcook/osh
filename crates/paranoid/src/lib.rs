//! Postgres-backed distributed systems primitives.
//!
//! An application opens its installation with [`installation::PgInstallation::open`] and starts
//! one [`runtime::PgRuntime`] in each process with [`runtime::PgRuntime::start`]. Every handle
//! comes from the runtime: Raw bytes and typed documents, Counters, leases, Mutexes, Crons, Once,
//! coalescing caches, Semaphores, Throttlers, the Queue, and Topics. An application can upgrade
//! an installation from published Paranoid v10 to this version in place: a deployment on this
//! version opens the installation alongside the running v10 deployment
//! ([`installation::PgOpenInstallationFields::v10`]), and [`runtime::PgRuntime::retire_v10`]
//! retires v10 access after the last v10 instance has stopped.
//!
//! The `uuid` feature adds a pooler-safe literal encoding for `uuid::Uuid` values.
//!
//! Cryptographic sealing and identifier generation come from the
//! `primitives-rs` package (`primitives_rs::crypto`, `primitives_rs::id`);
//! this crate composes them into database-backed machinery.
//!
//! # Tested PostgreSQL and PgBouncer versions
//!
//! Paranoid is verified against PostgreSQL 14, 15, 16, 17, and 18, connected directly and through
//! PgBouncer in transaction pooling, and its statement-pooling behavior is verified through
//! PgBouncer in statement pooling. The verified releases are PostgreSQL 14.24, 15.19, 16.15,
//! 17.11, and 18.6. No other PostgreSQL version was tested.
//!
//! Paranoid sends its own statements in the simple query protocol and turns the `sqlx` statement
//! cache off in its pools, so Paranoid does not depend on the `max_prepared_statements` setting of
//! a PgBouncer in front of the pools. The verified PgBouncer releases are 1.21.0, 1.23.1, and
//! 1.26.0 with PostgreSQL 16, with `max_prepared_statements` at 0 and at 200. Earlier verification
//! with the value at 200 also covered PgBouncer 1.22.0, 1.22.1, 1.23.0, 1.24.0, 1.24.1, and
//! 1.25.2. No other PgBouncer version was tested. The documentation of the [`db`] module describes
//! how an application's own `sqlx` queries behave through a pooler.
//!
//! # Obligations of an application
//!
//! Paranoid enforces what it can through types and operations. The obligations below remain with
//! the application. Each line names one obligation and links to the item that states it and
//! gives its reason.
//!
//! ## Connections and SQL
//!
//! - Connect Paranoid's operations to PostgreSQL directly or through a pooler in transaction
//!   pooling mode: [`db`](db#pooler-independence).
//! - Leave the `PGOPTIONS`, `PGSSLCERT`, `PGSSLKEY`, and `PGSSLROOTCERT` environment variables
//!   unset, and `PGPASSWORD` too when the database URL has no password:
//!   [`PoolConfig`](db::PoolConfig).
//! - Run extended-protocol SQL inside an owned transaction. A pool accepts simple-protocol
//!   queries and rejects extended-protocol queries before acquiring a connection:
//!   [`db`](db#pooler-independence).
//! - Keep a connection free for independent work while the application's transactions hold
//!   theirs: [`PoolConfig::max_connections`](db::PoolConfig::max_connections).
//! - Give the runtime separate application and control client pools, and leave backend
//!   capacity for control work through any pooler or database connection limit:
//!   [`PgRuntimeFields`](runtime::PgRuntimeFields).
//! - Pass one transaction to Paranoid's operations and to the application's SQL when they must
//!   commit together: [`db`](db#pools-and-transactions).
//! - Do not read a pool or transaction type as a statement about database privileges:
//!   [`WritePool`](db::WritePool).
//! - Keep untrusted input out of SQL text that passes through `sqlx::AssertSqlSafe`:
//!   [`db`](db#app-owned-sql).
//! - Run SQL that depends on session state inside a transaction, and use only the forms that end
//!   with the transaction: [`db`](db#app-owned-sql).
//! - Read `real` and `double precision` results exactly when `extra_float_digits` can be zero or
//!   negative: [`db`](db#app-owned-sql).
//! - Build dynamic SQL only from audited fragments, quoted identifiers, and typed literals:
//!   [`SimpleQuery`](db::SimpleQuery).
//! - Do not treat several pages of one listing as one snapshot:
//!   [`PgRaw::read_page`](kv::PgRaw::read_page) and
//!   [`PgQueueReader::list_jobs`](queue::PgQueueReader::list_jobs).
//! - Keep Queue payloads to at most 100 nested arrays or objects:
//!   [`PgQueue::enqueue`](queue::PgQueue::enqueue).
//!
//! ## The installation and its upgrades
//!
//! - Open the same schema in every process, and name the published v10 installation in the
//!   release that moves from it:
//!   [`PgOpenInstallationFields`](installation::PgOpenInstallationFields).
//! - Give the Queue's stale threshold the value that v10 workers use, and keep every worker's
//!   heartbeat interval below it in both releases:
//!   [`PgInitialInstallationOptions`](installation::PgInitialInstallationOptions).
//! - Start the retirement of v10 only after the last v10 instance has stopped because v10
//!   instances cannot register their presence:
//!   [`PgRuntime::retire_v10`](runtime::PgRuntime::retire_v10).
//! - Keep the v10 `protocol` and `schema_ledger` tables after v10 data retirement because
//!   every opening of the installation reads them:
//!   [`PgV10ImportPhase::DataRetired`](installation::PgV10ImportPhase::DataRetired).
//! - Keep the Tokio runtime that started a runtime alive until its shutdown finishes:
//!   [`PgRuntime`](runtime::PgRuntime).
//! - Declare every scope that the code uses, with the roles that it uses in each:
//!   [`AppDeclarations`](scope::AppDeclarations).
//!
//! ## Shared names and configuration
//!
//! - Use the same key texts in every process that shares state: [`fleet`] and [`kv::Key`].
//! - Give every handle of one Semaphore or Throttler the same settings, which every operation
//!   checks against the stored record: [`PgSemaphoreFields`](fleet::PgSemaphoreFields) and
//!   [`PgThrottlerFields`](fleet::PgThrottlerFields).
//!
//! ## Commits, claims, and expiry
//!
//! - Make a task safe to overlap with the next holder of its claim because a run does not stop
//!   its task when the claim ends: [`PgMutex`](fleet::PgMutex),
//!   [`PgSemaphore`](fleet::PgSemaphore), and [`PgThrottler`](fleet::PgThrottler).
//! - Send the fencing token to an external resource that must reject an expired holder:
//!   [`FencingToken`](fleet::FencingToken).
//! - Renew a lease claim before it stops being usable: [`PgLease`](fleet::PgLease).
//! - Do not treat a status or a holder read as permission to proceed:
//!   [`PgMutex::fetch_live_holder`](fleet::PgMutex::fetch_live_holder).
//! - Do not expect a run to return a rate-limit token: [`PgThrottler`](fleet::PgThrottler).
//! - Do not use a holder identifier or a worker owner ID as authority:
//!   [`HolderId`](fleet::HolderId) and [`WorkerOwnerId`](queue::WorkerOwnerId).
//!
//! ## Repeated work
//!
//! - Make the effects of a queue handler safe to repeat: [`TaskRegistry`](queue::TaskRegistry).
//! - Do not rely on a dedupe key to prevent a repeated effect: [`DedupeKey`](queue::DedupeKey).
//! - Make the effects of a Once task safe to repeat: [`PgOnce`](fleet::PgOnce).
//! - Make the effects of a cache computation safe to repeat:
//!   [`PgCoalescingCache`](fleet::PgCoalescingCache).
//! - Read Topic events in the transaction that processes them:
//!   [`PgTopicSubscription`](fleet::PgTopicSubscription).
//! - Call `stop_and_wait` or `wait` on a worker handle to observe its failure:
//!   [`WorkerHandle`](queue::WorkerHandle).
//!   Inspect [`WorkerRunLoopSummary::shutdown`](queue::WorkerRunLoopSummary::shutdown)
//!   to distinguish drained job tasks from an expired shutdown grace period.
//!
//! ## Cryptography and identifiers
//!
//! - Pass the same associated data to decryption as to encryption:
//!   [`EncryptInput`](primitives_rs::crypto::encryption::EncryptInput).
//! - List keys newest first, and keep an older key for as long as data under it is retained:
//!   [`Keyset`](primitives_rs::crypto::keysets::Keyset).
//! - Decrypt with a keyset that has the purpose string of the keyset that encrypted:
//!   [`Keyset`](primitives_rs::crypto::keysets::Keyset).
//! - Do not move an encrypted Raw value to another Raw scope or key without decrypting it under
//!   the source scope and key and re-encrypting it under the destination scope and key:
//!   [`PgEncryptedRaw`](kv::PgEncryptedRaw).
//! - Protect exported key bytes and key text as secrets:
//!   [`Key32`](primitives_rs::crypto::Key32).
//! - Clear copies taken from exposed secret bytes:
//!   [`SecretBytes`](primitives_rs::secret_bytes::SecretBytes).
//! - Sort sortable IDs by their bytes, and do not treat their text form as encryption:
//!   [`SortableId`](primitives_rs::id::time_sortable::SortableId).
//! - Expect generation at an explicit time to clamp a time outside the range of the ID:
//!   [`generate_at_unix_micros`](primitives_rs::id::time_sortable::SortableId::generate_at_unix_micros).
//!
//! ## Testing with the harness
//!
//! The `paranoid-testing` crate documents these obligations.
//!
//! - Run the harness on a container runtime that supports host networking.
//! - Configure explicit startup and cleanup budgets for harness-owned lifecycle work.
//! - Await explicit harness shutdown to observe cleanup completion and failure.
//! - Remove the containers that a killed test process leaves behind.
//! - Set every harness setting, and grant each scenario role the privileges that its test needs.
#![deny(missing_docs)]
#![deny(clippy::mod_module_files)]

/// The `sqlx` crate that Paranoid's handles use. Paranoid's [`db::Pool`], [`db::WritePool`],
/// [`db::Tx`], and [`db::WriteTx`] implement [`sqlx::Executor`], and an application that
/// uses this re-export builds its queries against the same `sqlx` version.
pub use sqlx;

/// Builds [`db::AuditedSql`] from a string literal template with named placeholders.
///
/// ```text
/// audited_sql!(
///     "CREATE TABLE IF NOT EXISTS {items} ({kind} text NOT NULL)",
///     items = &items_table,
///     kind = &kind_column,
/// )
/// ```
///
/// The template must be a string literal. A placeholder is `{name}`, and `{{` and `}}` write
/// a literal brace. Each placeholder uses the argument of the same name; an argument named with
/// a raw identifier, such as `r#type`, has the name without `r#`. The argument's type implements
/// [`db::AuditedSqlArgument`]: a quoted identifier ([`db::PgIdentifier`], [`db::PgSchemaName`],
/// or [`db::PgQualifiedTableName`]) or a typed literal that Paranoid's encoders render (`bool`,
/// `i16`, `i32`, `i64`, `f64`, or bytes). A placeholder may appear more than once.
///
/// A placeholder must stand in SQL code, outside every string constant, quoted identifier,
/// dollar-quoted string, and comment of the template, and the template must end in SQL code.
/// On each side a placeholder must touch the start or the end of the template, an ASCII white
/// space character of SQL (space, tab, line feed, carriage return, or form feed), or one of
/// these characters:
///
/// - before a placeholder: `(` `,` `[` `=` `<` `>` `+` `-` `*` `/` `%` `|`
/// - after a placeholder: `(` `)` `,` `;` `[` `]` `:` `=` `<` `>` `+` `-` `*` `/` `%` `|`
///
/// Every other neighbor, such as a letter, a digit, `_`, `$`, `.`, a quote, a non-ASCII
/// character, or another placeholder, fails to compile with a message that names it. A
/// template also fails to compile when it ends inside a string constant, a quoted identifier,
/// a dollar-quoted string, or a comment; when a string constant without the `E` prefix
/// contains a backslash because PostgreSQL finds the end of such a constant differently under
/// each setting of `standard_conforming_strings`; and when a `$` directly follows a number that
/// a letter or `_` follows, such as `1e5$`, because PostgreSQL versions read that `$`
/// differently.
///
/// Each rendering starts with `(`, `"`, or `E'` and ends with `)`, `"`, or a letter. The part
/// of a rendering that varies with the value stays inside one closed group: a `bool` inside
/// parentheses (`(TRUE)` or `(FALSE)`), an integer's sign and digits inside parentheses before
/// a fixed cast, a float inside one parenthesized expression, a byte string's hexadecimal
/// digits inside one string constant before a fixed cast, and an identifier's characters inside
/// one quoted identifier. The text after that group is the same for every value, so with these
/// rules the tokens of the statement outside the group are the same for every value, and the
/// value of an argument never changes the statement's structure. A literal argument renders as
/// an expression: where PostgreSQL's grammar takes a column label or a keyword instead, such as
/// after `.` or `AS`, it is a syntax error for every value.
///
/// The expansion is a complete fragment. Code that joins it with other SQL text at run time
/// must separate the two pieces (for example, [`db::SimpleQuery`] separates pieces with one
/// space) because the macro checks only the neighbors inside the template.
///
/// The macro also fails to compile when the first argument is not a string literal, when the
/// template is empty, when the template has a malformed or positional placeholder, when a
/// placeholder has no argument, when an argument is not used by the template, or when two
/// arguments have the same name. An argument of another type fails at its expression. The
/// expansion has type [`db::AuditedSql`] and reaches this crate through `$crate`, so it also
/// works in a crate that renames the dependency.
#[macro_export]
macro_rules! audited_sql {
	($($input:tt)*) => {
		$crate::__private::audited_sql!($crate, $($input)*)
	};
}

/// Support for the expansion of [`audited_sql!`]. Not part of the API.
#[doc(hidden)]
pub mod __private {
	pub use crate::db::audited_sql::AuditedSqlBuilder;
	pub use paranoid_macros::audited_sql;

	/// Returns `argument`. The expansion of [`audited_sql!`] passes each argument through this
	/// function with the span of the argument's expression so that the compiler reports an
	/// argument of another type at that expression.
	pub fn audited_sql_argument<A: crate::db::AuditedSqlArgument + ?Sized>(argument: &A) -> &A {
		argument
	}
}

/// Internal items that only Paranoid's own integration tests use. No stability promise attaches
/// to this module.
#[cfg(feature = "__internal")]
#[doc(hidden)]
#[allow(missing_docs)]
pub mod __internal {
	pub mod db {
		pub mod catalog {
			pub use crate::db::catalog::*;
		}
		pub mod identifier {
			pub use crate::db::identifier::*;
		}
		pub mod pool {
			pub use crate::db::pool::*;
		}
		pub mod sql_state {
			pub use crate::db::sql_state::*;
		}
	}
	pub mod fleet {
		pub mod acquire_retry_jitter {
			pub use crate::fleet::acquire_retry_jitter::*;
		}
		pub mod throttler {
			pub mod arithmetic {
				pub use crate::fleet::throttler::arithmetic::*;
			}
		}
	}
	pub mod installation {
		pub mod schema_ledger {
			pub use crate::installation::schema_ledger::*;
		}
	}
	pub mod queue {
		pub mod claim_token {
			pub use crate::queue::claim_token::*;
		}
		pub mod constants {
			pub use crate::queue::constants::*;
		}
		pub mod job_model {
			pub use crate::queue::job_model::*;
		}
		pub mod operations {
			pub mod worker_transitions {
				pub use crate::queue::operations::worker_transitions::*;
			}
		}
		pub mod pause {
			pub use crate::queue::pause::*;
		}
		pub mod runtime_helpers {
			pub use crate::queue::runtime_helpers::*;
		}
		pub mod v10 {
			pub mod schema_model {
				pub use crate::queue::v10::schema_model::*;
			}
		}
		pub mod validation {
			pub use crate::queue::validation::*;
		}
		pub mod worker {
			pub mod job {
				pub use crate::queue::worker::job::*;
			}
			pub mod model {
				pub use crate::queue::worker::model::*;
			}
			pub mod run_loop {
				pub use crate::queue::worker::run_loop::*;
			}
		}
	}
}

pub mod db;
pub mod fleet;
pub mod installation;
pub mod kv;
pub mod queue;
mod records;
pub mod runtime;
pub mod scope;
pub mod upgrade;
