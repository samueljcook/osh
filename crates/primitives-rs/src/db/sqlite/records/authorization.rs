use rusqlite::hooks::{AuthAction, AuthContext};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Authorization state lasts for the enclosing transaction, including after a
/// record view is dropped. No prepared statement escapes a transaction method.
pub(in crate::db::sqlite) struct TableAuthorization {
	state: AtomicU8,
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum TableAuthorizationState {
	Unprotected,
	Protected,
	LibraryAccess,
}

impl TableAuthorization {
	pub(in crate::db::sqlite) fn new() -> Self {
		Self {
			state: AtomicU8::new(TableAuthorizationState::Unprotected as u8),
		}
	}

	pub(in crate::db::sqlite) fn reset(&self) {
		assert!(
			!matches!(self.state(), TableAuthorizationState::LibraryAccess),
			"record access ends before transaction authorization resets"
		);
		self.state.store(
			TableAuthorizationState::Unprotected as u8,
			Ordering::Relaxed,
		);
	}

	pub(super) fn protect(&self) {
		assert!(
			!matches!(self.state(), TableAuthorizationState::LibraryAccess),
			"record protection is selected outside a library access scope"
		);
		self.state
			.store(TableAuthorizationState::Protected as u8, Ordering::Relaxed);
	}

	fn state(&self) -> TableAuthorizationState {
		match self.state.load(Ordering::Relaxed) {
			value if value == TableAuthorizationState::Unprotected as u8 => {
				TableAuthorizationState::Unprotected
			}
			value if value == TableAuthorizationState::Protected as u8 => {
				TableAuthorizationState::Protected
			}
			value if value == TableAuthorizationState::LibraryAccess as u8 => {
				TableAuthorizationState::LibraryAccess
			}
			_ => unreachable!(
				"record authorization states are Unprotected, Protected, and LibraryAccess"
			),
		}
	}

	pub(in crate::db::sqlite) fn allows(&self, context: AuthContext<'_>) -> bool {
		let library_access = match self.state() {
			TableAuthorizationState::Unprotected => return true,
			TableAuthorizationState::Protected => false,
			TableAuthorizationState::LibraryAccess => true,
		};
		let main = context
			.database_name
			.is_some_and(|name| name.eq_ignore_ascii_case("main"));
		let protected = match context.action {
			AuthAction::Insert { table_name }
			| AuthAction::Delete { table_name }
			| AuthAction::Update { table_name, .. }
			| AuthAction::CreateTable { table_name }
			| AuthAction::DropTable { table_name }
			| AuthAction::CreateVtable { table_name, .. }
			| AuthAction::DropVtable { table_name, .. } => main && is_owned_name(table_name),
			AuthAction::CreateIndex {
				table_name,
				index_name,
			}
			| AuthAction::DropIndex {
				table_name,
				index_name,
			} => main && (is_owned_name(table_name) || is_owned_name(index_name)),
			AuthAction::CreateTrigger {
				table_name,
				trigger_name,
			}
			| AuthAction::DropTrigger {
				table_name,
				trigger_name,
			} => main && (is_owned_name(table_name) || is_owned_name(trigger_name)),
			// SQLite reports the trigger's schema, not the target table's schema.
			// Both names are therefore excluded as TEMP trigger targets.
			AuthAction::CreateTempTrigger { table_name, .. }
			| AuthAction::DropTempTrigger { table_name, .. } => is_owned_name(table_name),
			AuthAction::CreateView { view_name } | AuthAction::DropView { view_name } => {
				main && is_owned_name(view_name)
			}
			AuthAction::AlterTable {
				database_name,
				table_name,
			} => database_name.eq_ignore_ascii_case("main") && is_owned_name(table_name),
			_ => false,
		};
		// Application triggers can use application tables, including during a
		// foreign-key cascade. They cannot inherit direct access to owned tables.
		!protected || (library_access && context.accessor.is_none())
	}
}

fn is_owned_name(name: &str) -> bool {
	name.eq_ignore_ascii_case(super::schema::STORE_TABLE)
		|| name.eq_ignore_ascii_case(super::schema::RECORD_TABLE)
}

pub(super) struct TableAccessGuard(Arc<TableAuthorization>);

impl TableAccessGuard {
	pub(super) fn new(authorization: &Arc<TableAuthorization>) -> Self {
		assert!(
			authorization
				.state
				.compare_exchange(
					TableAuthorizationState::Protected as u8,
					TableAuthorizationState::LibraryAccess as u8,
					Ordering::Relaxed,
					Ordering::Relaxed
				)
				.is_ok(),
			"owned record access requires protected tables and must not nest"
		);
		Self(Arc::clone(authorization))
	}
}

impl Drop for TableAccessGuard {
	fn drop(&mut self) {
		self.0
			.state
			.store(TableAuthorizationState::Protected as u8, Ordering::Relaxed);
	}
}
