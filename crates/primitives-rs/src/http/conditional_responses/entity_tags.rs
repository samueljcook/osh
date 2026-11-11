use http::{HeaderMap, HeaderValue, header::IF_NONE_MATCH};

use crate::http::entity_tag_syntax::{parse_entity_tag, trim_ows};

/// Compares all `If-None-Match` field lines with a current representation's ETag.
///
/// Comparison is weak, so `W/"v1"` matches `"v1"`. A sole `*` matches a
/// representation with a valid ETag. Commas inside tags are data. Invalid
/// syntax anywhere in the field prevents a match; empty list members are
/// ignored. This function allocates nothing and examines at most the supplied
/// header bytes.
///
/// Callers must establish that the representation exists, perform access and
/// higher-priority precondition checks first, and interpret a match according
/// to the request method: GET/HEAD use 304; other methods require a 412 before
/// performing the operation. This function does not perform those steps.
pub fn if_none_match_matches(request_headers: &HeaderMap, current_etag: &HeaderValue) -> bool {
	let Some((current, rest)) = parse_entity_tag(trim_ows(current_etag.as_bytes())) else {
		return false;
	};
	if !rest.is_empty() {
		return false;
	}
	let values = request_headers.get_all(IF_NONE_MATCH);
	let mut lines = values.iter();
	if let Some(first) = lines.next()
		&& trim_ows(first.as_bytes()) == b"*"
	{
		return lines.next().is_none();
	}
	let mut matched = false;
	for value in values {
		let mut remaining = trim_ows(value.as_bytes());
		while !remaining.is_empty() {
			if let Some(rest) = remaining.strip_prefix(b",") {
				remaining = trim_ows(rest);
				continue;
			}
			let Some((candidate, rest)) = parse_entity_tag(remaining) else {
				return false;
			};
			matched |= candidate.opaque_tag() == current.opaque_tag();
			remaining = trim_ows(rest);
			if !remaining.is_empty() {
				let Some(rest) = remaining.strip_prefix(b",") else {
					return false;
				};
				remaining = trim_ows(rest);
			}
		}
	}
	matched
}
