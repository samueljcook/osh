//! Application SQL must not start or end a PostgreSQL transaction: a transaction handle owns
//! its transaction, and a pool must not keep a transaction open on a connection that it reuses.
//! This checks statement boundaries and lifetime-control heads, not business SQL validity.

use super::pg_lexer::{
	PgLexeme, PgLexer, PgLexerOptions, PgLiteralMode, is_identifier_byte, is_pg_ascii_whitespace,
};

/// These errors contain no SQL text, bound values, or database credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(super) enum TransactionSqlError {
	#[error(
		"application SQL cannot start or end a transaction; use the lifecycle methods of a Paranoid transaction"
	)]
	LifetimeControl,
	/// `EXECUTE` runs a prepared statement without exposing its text to this check, and a statement
	/// prepared on the connection outside the transaction can end the transaction.
	#[error("application SQL cannot run a prepared statement with EXECUTE")]
	PreparedExecution,
	#[error("SQL transaction-lifetime boundaries could not be established")]
	UnresolvedBoundary,
}

/// Checks the exact materialized SQL before any part of its request is sent.
/// An impossible literal interpretation cannot execute the batch, so it does not invalidate
/// another valid interpretation. A possible lifetime transition always rejects the request.
pub(super) fn check_transaction_lifetime(sql: &str) -> Result<(), TransactionSqlError> {
	let standard = check_interpretation(sql, false);
	let escaped = check_interpretation(sql, true);
	match (standard, escaped) {
		(
			Err(
				error @ (TransactionSqlError::LifetimeControl
				| TransactionSqlError::PreparedExecution),
			),
			_,
		)
		| (
			_,
			Err(
				error @ (TransactionSqlError::LifetimeControl
				| TransactionSqlError::PreparedExecution),
			),
		) => Err(error),
		(Ok(()), _) | (_, Ok(())) => Ok(()),
		(Err(error), Err(_)) => Err(error),
	}
}

fn check_interpretation(
	sql: &str,
	ordinary_backslash_escapes: bool,
) -> Result<(), TransactionSqlError> {
	let mut boundary = StatementBoundary::new();
	for lexeme in PgLexer::new(
		sql,
		PgLexerOptions {
			literal_mode: PgLiteralMode::Execution {
				ordinary_backslash_escapes,
			},
		},
	) {
		match lexeme.map_err(|_| TransactionSqlError::UnresolvedBoundary)? {
			PgLexeme::Protected(text) if text.starts_with("--") || text.starts_with("/*") => {}
			PgLexeme::Protected(_) => boundary.accept_other_token()?,
			PgLexeme::Code(code) => {
				let mut index = 0;
				let bytes = code.as_bytes();
				while index < bytes.len() {
					let start = index;
					let byte = bytes[index];
					index += 1;
					if is_identifier_byte(byte) {
						while index < bytes.len() && is_identifier_byte(bytes[index]) {
							index += 1;
						}
						boundary.accept_word(&code[start..index])?;
					} else if !is_pg_ascii_whitespace(byte) {
						boundary.accept_symbol(byte)?;
					}
				}
			}
		}
	}
	boundary.finish_statement()?;
	if boundary.atomic_bodies != 0 || boundary.parentheses != 0 {
		return Err(TransactionSqlError::UnresolvedBoundary);
	}
	Ok(())
}

#[derive(Clone, Copy)]
enum StatementHead {
	Empty,
	Start,
	Prepare,
	Rollback,
	RollbackNoise,
	Create,
	CreateOr,
	CreateOrReplace,
	RoutineDef,
	RoutineBegin,
	Other,
}

struct StatementBoundary {
	head: StatementHead,
	parentheses: usize,
	atomic_bodies: usize,
}

impl StatementBoundary {
	fn new() -> Self {
		Self {
			head: StatementHead::Empty,
			parentheses: 0,
			atomic_bodies: 0,
		}
	}

	fn accept_word(&mut self, word: &str) -> Result<(), TransactionSqlError> {
		// A routine body ends only where another statement could begin. CASE-expression
		// endings and CASE/END result labels occur within their statement, so they cannot
		// close this body or hide a following top-level lifetime-control statement.
		if self.atomic_bodies != 0
			&& matches!(self.head, StatementHead::Empty)
			&& self.parentheses == 0
			&& word.eq_ignore_ascii_case("END")
		{
			self.atomic_bodies -= 1;
			self.head = StatementHead::Other;
			return Ok(());
		}
		self.head = match self.head {
			StatementHead::Empty => {
				if word.eq_ignore_ascii_case("CREATE") {
					StatementHead::Create
				} else if self.atomic_bodies == 0 {
					if ["BEGIN", "COMMIT", "END", "ABORT"]
						.iter()
						.any(|keyword| word.eq_ignore_ascii_case(keyword))
					{
						return Err(TransactionSqlError::LifetimeControl);
					}
					if word.eq_ignore_ascii_case("EXECUTE") {
						return Err(TransactionSqlError::PreparedExecution);
					}
					if word.eq_ignore_ascii_case("START") {
						StatementHead::Start
					} else if word.eq_ignore_ascii_case("PREPARE") {
						StatementHead::Prepare
					} else if word.eq_ignore_ascii_case("ROLLBACK") {
						StatementHead::Rollback
					} else {
						StatementHead::Other
					}
				} else {
					StatementHead::Other
				}
			}
			StatementHead::Start | StatementHead::Prepare => {
				if word.eq_ignore_ascii_case("TRANSACTION") {
					return Err(TransactionSqlError::LifetimeControl);
				}
				StatementHead::Other
			}
			StatementHead::Rollback => {
				if word.eq_ignore_ascii_case("TO") {
					StatementHead::Other
				} else if word.eq_ignore_ascii_case("WORK")
					|| word.eq_ignore_ascii_case("TRANSACTION")
				{
					StatementHead::RollbackNoise
				} else {
					return Err(TransactionSqlError::LifetimeControl);
				}
			}
			StatementHead::RollbackNoise => {
				if !word.eq_ignore_ascii_case("TO") {
					return Err(TransactionSqlError::LifetimeControl);
				}
				StatementHead::Other
			}
			StatementHead::Create => {
				if word.eq_ignore_ascii_case("OR") {
					StatementHead::CreateOr
				} else {
					routine_def_head(word)
				}
			}
			StatementHead::CreateOr => {
				if word.eq_ignore_ascii_case("REPLACE") {
					StatementHead::CreateOrReplace
				} else {
					StatementHead::Other
				}
			}
			StatementHead::CreateOrReplace => routine_def_head(word),
			StatementHead::RoutineDef | StatementHead::RoutineBegin => {
				if self.parentheses == 0
					&& matches!(self.head, StatementHead::RoutineBegin)
					&& word.eq_ignore_ascii_case("ATOMIC")
				{
					self.atomic_bodies += 1;
					StatementHead::Empty
				} else if self.parentheses == 0 && word.eq_ignore_ascii_case("BEGIN") {
					StatementHead::RoutineBegin
				} else {
					StatementHead::RoutineDef
				}
			}
			StatementHead::Other => StatementHead::Other,
		};
		Ok(())
	}

	fn accept_other_token(&mut self) -> Result<(), TransactionSqlError> {
		self.finish_statement()?;
		self.head = match self.head {
			StatementHead::RoutineDef | StatementHead::RoutineBegin => StatementHead::RoutineDef,
			_ => StatementHead::Other,
		};
		Ok(())
	}

	fn accept_symbol(&mut self, symbol: u8) -> Result<(), TransactionSqlError> {
		match symbol {
			b';' if self.parentheses == 0 => {
				self.finish_statement()?;
				self.head = StatementHead::Empty;
			}
			b'(' => {
				self.parentheses += 1;
				self.accept_other_token()?;
			}
			b')' => {
				self.parentheses = self
					.parentheses
					.checked_sub(1)
					.ok_or(TransactionSqlError::UnresolvedBoundary)?;
				self.accept_other_token()?;
			}
			_ => {
				self.accept_other_token()?;
			}
		}
		Ok(())
	}

	fn finish_statement(&self) -> Result<(), TransactionSqlError> {
		if matches!(
			self.head,
			StatementHead::Rollback | StatementHead::RollbackNoise
		) {
			return Err(TransactionSqlError::LifetimeControl);
		}
		Ok(())
	}
}

fn routine_def_head(word: &str) -> StatementHead {
	if word.eq_ignore_ascii_case("FUNCTION") || word.eq_ignore_ascii_case("PROCEDURE") {
		StatementHead::RoutineDef
	} else {
		StatementHead::Other
	}
}

#[cfg(test)]
mod tests {
	use super::{TransactionSqlError, check_transaction_lifetime};

	#[test]
	fn rejects_lifetime_control_and_prepared_execution() {
		for sql in [
			"COMMIT",
			"select 1; commit",
			"ROLLBACK",
			"START TRANSACTION",
			"\x0bBEGIN",
			"\x0bCOMMIT",
			"SELECT 1;\x0bROLLBACK",
			"\x0bPREPARE\x0bTRANSACTION 'two_phase'",
		] {
			assert_eq!(
				check_transaction_lifetime(sql),
				Err(TransactionSqlError::LifetimeControl),
				"{sql}"
			);
		}
		for sql in [
			"EXECUTE sqlx_s_1",
			"select 1; execute plan(1)",
			"Execute plan",
			"\x0bEXECUTE plan",
		] {
			assert_eq!(
				check_transaction_lifetime(sql),
				Err(TransactionSqlError::PreparedExecution),
				"{sql}"
			);
		}
		assert_eq!(
			check_transaction_lifetime("\x0bSAVEPOINT a;\x0bROLLBACK TO SAVEPOINT a"),
			Ok(())
		);
	}

	#[test]
	fn accepts_data_statements_and_savepoints() {
		for sql in [
			"SELECT 1",
			"SELECT 'EXECUTE plan'",
			"SAVEPOINT a; ROLLBACK TO SAVEPOINT a",
			"DO $$ BEGIN EXECUTE 'SELECT 1'; END $$",
			"SELECT $$a$$$$; COMMIT; $$",
			"SELECT 1$$ $2 $$",
			"SELECT $1$t$ $2 $t$",
			"SELECT 1.5$$ $2 $$",
		] {
			assert_eq!(check_transaction_lifetime(sql), Ok(()), "{sql}");
		}
	}

	#[test]
	fn detects_lifetime_control_after_adjacent_dollar_quotes_and_rejects_ambiguous_numeric_dollars()
	{
		for sql in [
			"SELECT $$a$$$t$ $$ $t$; COMMIT; -- $$",
			"SELECT 1$t$ $$ $t$; COMMIT; -- $$",
			"SELECT $1$t$ $$ $t$; COMMIT; -- $$",
			"SELECT 1.5$t$ $$ $t$; COMMIT; -- $$",
		] {
			assert_eq!(
				check_transaction_lifetime(sql),
				Err(TransactionSqlError::LifetimeControl),
				"{sql}"
			);
		}
		for sql in ["SELECT 1e5$$x$$", "SELECT 0x1F$$x$$"] {
			assert_eq!(
				check_transaction_lifetime(sql),
				Err(TransactionSqlError::UnresolvedBoundary),
				"{sql}"
			);
		}
	}
}
