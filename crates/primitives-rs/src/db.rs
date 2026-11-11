//! Database connections, transactions, and record facilities.

#[cfg(feature = "record-identity")]
pub mod records;

#[cfg(feature = "sqlite-transactions")]
pub mod sqlite;
