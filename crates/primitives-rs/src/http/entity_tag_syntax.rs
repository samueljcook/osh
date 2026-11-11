//! The entity-tag syntax of RFC 9110 section 8.8.3, which `If-None-Match`
//! and `If-Range` share.

/// One entity tag, with its opaque tag without quotes.
pub(super) enum EntityTag<'a> {
	/// A tag with the `W/` prefix.
	Weak(&'a [u8]),
	/// A tag without the `W/` prefix.
	Strong(&'a [u8]),
}

impl<'a> EntityTag<'a> {
	/// The tag between its quotes.
	pub(super) fn opaque_tag(&self) -> &'a [u8] {
		match self {
			Self::Weak(opaque_tag) | Self::Strong(opaque_tag) => opaque_tag,
		}
	}
}

/// Reads one entity tag at the start of `input` and returns it with the bytes
/// that follow it, or `None` when `input` does not start with an entity tag.
pub(super) fn parse_entity_tag(input: &[u8]) -> Option<(EntityTag<'_>, &[u8])> {
	let weak_input = input.strip_prefix(b"W/");
	let quoted = weak_input.unwrap_or(input).strip_prefix(b"\"")?;
	for (index, &byte) in quoted.iter().enumerate() {
		if byte == b'"' {
			let opaque_tag = &quoted[..index];
			let tag = match weak_input {
				Some(_) => EntityTag::Weak(opaque_tag),
				None => EntityTag::Strong(opaque_tag),
			};
			return Some((tag, &quoted[index + 1..]));
		}
		if !(byte == 0x21 || (0x23..=0x7e).contains(&byte) || byte >= 0x80) {
			return None;
		}
	}
	None
}

/// `input` without the spaces and tabs at its start and end.
pub(super) fn trim_ows(input: &[u8]) -> &[u8] {
	let start = input
		.iter()
		.position(|byte| !matches!(byte, b' ' | b'\t'))
		.unwrap_or(input.len());
	let end = input
		.iter()
		.rposition(|byte| !matches!(byte, b' ' | b'\t'))
		.map_or(start, |index| index + 1);
	&input[start..end]
}
