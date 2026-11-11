use super::*;

/// Request cookie pairs from Cookie fields. Values are borrowed and omitted from Debug.
///
/// Parsing reads each field's bytes, splits the field at every `;`, trims SP
/// and HTAB around each pair, and splits each pair at its first `=`. A pair
/// without `=` and a pair with a name that is not an HTTP token match no lookup.
/// Those pairs, and pairs with a value outside the cookie value grammar,
/// never make parsing fail and never affect a lookup of another name. The value
/// grammar allows an empty value and RFC 6265 cookie-octets, optionally inside
/// one pair of double quotes, which lookups remove. Non-ASCII bytes, controls,
/// spaces, commas, backslashes, and inner double quotes are outside it. No
/// value is percent-decoded.
pub struct RequestCookies<'a> {
	pairs: Vec<RequestCookiePair<'a>>,
}

/// A pair with a valid name. Pairs without a valid name match no lookup, so
/// parsing does not retain them.
struct RequestCookiePair<'a> {
	name: &'a str,
	value: RequestCookieValue<'a>,
}

enum RequestCookieValue<'a> {
	/// A value inside the cookie value grammar, without its surrounding quotes.
	Valid(&'a str),
	/// A value outside the cookie value grammar.
	Invalid,
}

impl<'a> RequestCookies<'a> {
	/// Parses all Cookie fields with a shared aggregate input bound. The bound
	/// counts every field's bytes plus one byte per field.
	pub fn parse(headers: &'a HeaderMap) -> Result<Self, CookieError> {
		let mut result = Self { pairs: Vec::new() };
		let mut size = 0usize;
		for value in headers.get_all(header::COOKIE) {
			size = size
				.saturating_add(value.as_bytes().len())
				.saturating_add(1);
			if size > MAX_COOKIE_HEADER_SIZE {
				return Err(CookieError::HeaderTooLarge);
			}
			result.append(value.as_bytes());
		}
		Ok(result)
	}
	/// Parses one raw Cookie field without percent decoding its values.
	pub fn parse_header(value: &'a str) -> Result<Self, CookieError> {
		if value.len().saturating_add(1) > MAX_COOKIE_HEADER_SIZE {
			return Err(CookieError::HeaderTooLarge);
		}
		let mut result = Self { pairs: Vec::new() };
		result.append(value.as_bytes());
		Ok(result)
	}
	/// Returns the value of the only pair with exactly this name, or `None` when
	/// no pair has the name.
	///
	/// Two or more pairs with the name return [`CookieError::DuplicateCookie`],
	/// even when their values match. One pair with the name and a value outside
	/// the cookie value grammar returns [`CookieError::MalformedHeader`]. Pairs
	/// with other names never cause an error.
	pub fn value(&self, name: &str) -> Result<Option<&'a str>, CookieError> {
		validate_name(name)?;
		let mut matching = self.pairs.iter().filter(|pair| pair.name == name);
		let Some(pair) = matching.next() else {
			return Ok(None);
		};
		if matching.next().is_some() {
			return Err(CookieError::DuplicateCookie);
		}
		match pair.value {
			RequestCookieValue::Valid(value) => Ok(Some(value)),
			RequestCookieValue::Invalid => Err(CookieError::MalformedHeader),
		}
	}
	/// Iterates the pairs that have names and values inside the cookie grammar,
	/// in field order and including duplicate names. It skips every other pair.
	pub fn iter(&self) -> impl Iterator<Item = (&'a str, &'a str)> + '_ {
		self.pairs.iter().filter_map(|pair| match pair.value {
			RequestCookieValue::Valid(value) => Some((pair.name, value)),
			RequestCookieValue::Invalid => None,
		})
	}
	fn append(&mut self, field: &'a [u8]) {
		for pair in field.split(|&byte| byte == b';') {
			let pair = trim_spaces_and_tabs(pair);
			let Some(equals) = pair.iter().position(|&byte| byte == b'=') else {
				continue;
			};
			let Some(name) = std::str::from_utf8(&pair[..equals])
				.ok()
				.filter(|name| validate_name(name).is_ok())
			else {
				continue;
			};
			self.pairs.push(RequestCookiePair {
				name,
				value: parse_value(&pair[equals + 1..]),
			});
		}
	}
}
impl fmt::Debug for RequestCookies<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RequestCookies")
			.field("pair_count", &self.pairs.len())
			.finish()
	}
}

fn trim_spaces_and_tabs(bytes: &[u8]) -> &[u8] {
	let is_kept = |byte: &u8| !matches!(byte, b' ' | b'\t');
	let start = bytes.iter().position(is_kept).unwrap_or(bytes.len());
	let end = bytes
		.iter()
		.rposition(is_kept)
		.map_or(start, |index| index + 1);
	&bytes[start..end]
}

fn parse_value(bytes: &[u8]) -> RequestCookieValue<'_> {
	let unquoted = match bytes {
		[b'"', inner @ .., b'"'] => inner,
		_ => bytes,
	};
	match std::str::from_utf8(unquoted) {
		Ok(value) if value.bytes().all(is_value_byte) => RequestCookieValue::Valid(value),
		_ => RequestCookieValue::Invalid,
	}
}
