use super::*;
use std::{fmt, marker::PhantomData, str::FromStr};

/// A client-readable cookie with a typed textual value and validated scope.
#[derive(Debug)]
pub struct PlaintextCookieDefinition<T> {
	definition: CookieDefinition,
	value: PhantomData<fn() -> T>,
}
impl<T> PlaintextCookieDefinition<T> {
	pub(super) fn new(definition: CookieDefinition) -> Self {
		Self {
			definition,
			value: PhantomData,
		}
	}
	/// Returns the shared creation/deletion definition.
	pub fn definition(&self) -> &CookieDefinition {
		&self.definition
	}
}
impl<T: fmt::Display + FromStr> PlaintextCookieDefinition<T> {
	/// Renders a bounded value and checks that its type accepts that text.
	///
	/// The returned cookie's `Debug` output includes its value. Do not log it.
	pub fn create_cookie(&self, value: &T) -> Result<Cookie<'static>, CookieError> {
		use fmt::Write;
		struct BoundedText(String);
		impl fmt::Write for BoundedText {
			fn write_str(&mut self, text: &str) -> fmt::Result {
				if self.0.len().saturating_add(text.len()) > MAX_SET_COOKIE_SIZE {
					return Err(fmt::Error);
				}
				self.0.push_str(text);
				Ok(())
			}
		}
		let mut text = BoundedText(String::new());
		write!(&mut text, "{value}").map_err(|_| CookieError::InvalidValue)?;
		text.0.parse::<T>().map_err(|_| CookieError::InvalidValue)?;
		self.definition.create_cookie(text.0)
	}
	/// Parses raw cookie text without percent decoding or exposing parser errors.
	pub fn decode_value(&self, value: &str) -> Result<T, CookieError> {
		if value.len() > MAX_SET_COOKIE_SIZE {
			return Err(CookieError::ValueTooLarge);
		}
		if !value.bytes().all(is_value_byte) {
			return Err(CookieError::InvalidValue);
		}
		value.parse().map_err(|_| CookieError::InvalidValue)
	}
	/// Reads a unique typed value; absence is distinct from invalid input.
	pub fn read_value(&self, headers: &HeaderMap) -> Result<Option<T>, CookieError> {
		self.definition
			.read_value(headers)?
			.map(|value| self.decode_value(value))
			.transpose()
	}
}
impl<T> Clone for PlaintextCookieDefinition<T> {
	fn clone(&self) -> Self {
		Self::new(self.definition.clone())
	}
}
