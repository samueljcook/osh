//! Context-specific HTML text escaping.

/// Escape `value` for use as HTML element text content: `&`, `<`, and `>`
/// become character references.
pub fn escape_text_content(value: &str) -> String {
	let mut out = String::with_capacity(value.len());
	for ch in value.chars() {
		match ch {
			'&' => out.push_str("&amp;"),
			'<' => out.push_str("&lt;"),
			'>' => out.push_str("&gt;"),
			ch => out.push(ch),
		}
	}
	out
}

/// Escape `value` for use inside a double-quoted HTML attribute value: `&`,
/// `"`, `<`, and `>` become character references.
pub fn escape_attr_value(value: &str) -> String {
	let mut out = String::with_capacity(value.len());
	for ch in value.chars() {
		match ch {
			'&' => out.push_str("&amp;"),
			'"' => out.push_str("&quot;"),
			'<' => out.push_str("&lt;"),
			'>' => out.push_str("&gt;"),
			ch => out.push(ch),
		}
	}
	out
}

/// Escapes CSS text for use as the content of a `<style>` element.
///
/// The result contains no case-insensitive `</style`, so the HTML parser
/// cannot see an end tag inside the content, and the content ends only at the
/// element's own end tag. In every case-insensitive `</style`, the `s` becomes
/// the CSS escape `\73 `, and an `S` becomes `\53 `; the `<`, the `/`, and the
/// letters `tyle` keep their spelling. CSS reads an escaped letter as the
/// letter itself in strings, in `url()` values, and in identifiers, so the
/// stylesheet has the same tokens and the same meaning as `value`. Only the
/// text of a comment changes. Every other character is unchanged, and text
/// without `</style` is returned unchanged, so escaping escaped text changes
/// nothing.
///
/// The function does not sanitize CSS: the stylesheet keeps every rule of
/// `value`.
pub fn escape_style_text_content(value: &str) -> String {
	const END_TAG_START: &str = "</style";
	let mut out = String::with_capacity(value.len());
	let mut last = 0;
	for (index, _) in value.match_indices("</") {
		let Some(candidate) = value.get(index..index + END_TAG_START.len()) else {
			continue;
		};
		if !candidate.eq_ignore_ascii_case(END_TAG_START) {
			continue;
		}
		let letter_index = index + "</".len();
		out.push_str(&value[last..letter_index]);
		out.push_str(if value.as_bytes()[letter_index] == b'S' {
			"\\53 "
		} else {
			"\\73 "
		});
		last = letter_index + 1;
	}
	out.push_str(&value[last..]);
	out
}

/// Encode `value` as a JSON string literal that is safe to embed inside an
/// HTML `<script>` element: standard JSON escaping, plus `&`, `<`, `>`,
/// U+2028, and U+2029 as `\u`-escapes so the literal can never form
/// `</script>`, `<!--`, or a raw line separator.
pub fn encode_json_string_for_html_script(value: &str) -> String {
	let mut out = String::with_capacity(value.len() + 2);
	out.push('"');
	for ch in value.chars() {
		match ch {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\u{0008}' => out.push_str("\\b"),
			'\u{000C}' => out.push_str("\\f"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			'&' => out.push_str("\\u0026"),
			'<' => out.push_str("\\u003c"),
			'>' => out.push_str("\\u003e"),
			'\u{2028}' => out.push_str("\\u2028"),
			'\u{2029}' => out.push_str("\\u2029"),
			ch if (ch as u32) < 0x20 => {
				out.push_str(&format!("\\u{:04x}", ch as u32));
			}
			ch => out.push(ch),
		}
	}
	out.push('"');
	out
}

/// Escape a complete serialized-JSON text for embedding inside an HTML
/// `<script>` element: `&`, `<`, `>`, U+2028, and U+2029 become
/// `\u`-escapes.
///
/// In valid JSON these characters only ever appear inside string values,
/// never in structural tokens, so a full-text pass is equivalent to
/// per-string escaping. Idempotent for text already using the `\u00..`
/// forms: substitutions target the raw characters, never their escaped
/// spellings. Reach for [`crate::html::escaping::encode_json_string_for_html_script`]
/// instead when encoding one string value rather than a serialized
/// document.
pub fn escape_json_text_for_html_script(json_text: &str) -> String {
	if !json_text
		.chars()
		.any(|ch| matches!(ch, '&' | '<' | '>' | '\u{2028}' | '\u{2029}'))
	{
		return json_text.to_owned();
	}
	let mut out = String::with_capacity(json_text.len() + 16);
	for ch in json_text.chars() {
		match ch {
			'&' => out.push_str("\\u0026"),
			'<' => out.push_str("\\u003c"),
			'>' => out.push_str("\\u003e"),
			'\u{2028}' => out.push_str("\\u2028"),
			'\u{2029}' => out.push_str("\\u2029"),
			ch => out.push(ch),
		}
	}
	out
}
