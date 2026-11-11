//! Header reading and writing helpers for the CSRF layer.
//!
//! These helpers read the submitted token from request headers and
//! append `Set-Cookie` headers to a response without competing with a cookie
//! that the response already sets.

use super::*;

pub(super) fn submitted_token<'a>(
	headers: &'a HeaderMap,
	header_name: &HeaderName,
) -> Result<&'a str, Error> {
	let Some(value) = single_header_value(headers, header_name, "submitted token")? else {
		return Err(Error::TokenMissing);
	};
	let token = value.to_str().map_err(Error::SubmittedTokenHeaderDecode)?;
	if token.is_empty() {
		return Err(Error::TokenMissing);
	}
	if token.len() > MAX_CSRF_SUBMITTED_TOKEN_SIZE {
		return Err(Error::SubmittedTokenTooLarge {
			actual: token.len(),
			max: MAX_CSRF_SUBMITTED_TOKEN_SIZE,
		});
	}
	Ok(token)
}

/// Appends one `Set-Cookie` header for each cookie, in order, and marks the
/// response `Cache-Control: private, no-store`. An empty list changes nothing.
/// When the response already sets a cookie with the name of any listed cookie,
/// the response owns that cookie and this function appends no header.
pub fn append_set_cookie_headers(
	headers: &mut HeaderMap,
	cookies: &[Cookie<'_>],
) -> Result<(), Error> {
	if cookies.is_empty() {
		return Ok(());
	}
	headers.insert(
		http::header::CACHE_CONTROL,
		HeaderValue::from_static("private, no-store"),
	);
	if cookies
		.iter()
		.any(|cookie| response_already_sets_cookie(headers, cookie.name()))
	{
		return Ok(());
	}
	let values = cookies
		.iter()
		.map(crate::http::cookies::headers::to_set_cookie_header)
		.collect::<Result<Vec<_>, _>>()?;
	for value in values {
		headers.append(SET_COOKIE, value);
	}
	Ok(())
}

fn response_already_sets_cookie(headers: &HeaderMap, name: &str) -> bool {
	headers
		.get_all(SET_COOKIE)
		.iter()
		.any(|value| set_cookie_name(value.as_bytes()) == Some(name.as_bytes()))
}

fn set_cookie_name(value: &[u8]) -> Option<&[u8]> {
	let value = value
		.iter()
		.position(|byte| !matches!(byte, b' ' | b'\t'))
		.map(|start| &value[start..])?;
	let equals = value.iter().position(|byte| *byte == b'=')?;
	let name = value[..equals]
		.iter()
		.rposition(|byte| !matches!(byte, b' ' | b'\t'))
		.map(|end| &value[..=end])?;
	(!name.is_empty()).then_some(name)
}

pub(super) fn header_to_nonempty_str<'a>(
	headers: &'a HeaderMap,
	header_name: &HeaderName,
	label: &'static str,
) -> Result<Option<&'a str>, Error> {
	let Some(value) = single_header_value(headers, header_name, label)? else {
		return Ok(None);
	};
	let value = value
		.to_str()
		.map_err(|source| Error::HeaderDecode { label, source })?;
	if value.is_empty() {
		return Err(Error::EmptyHeader { label });
	}
	Ok(Some(value))
}

fn single_header_value<'a>(
	headers: &'a HeaderMap,
	header_name: &HeaderName,
	label: &'static str,
) -> Result<Option<&'a HeaderValue>, Error> {
	let mut values = headers.get_all(header_name).iter();
	let Some(value) = values.next() else {
		return Ok(None);
	};
	if values.next().is_some() {
		return Err(Error::DuplicateHeader { label });
	}
	Ok(Some(value))
}
