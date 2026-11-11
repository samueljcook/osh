//! Conservative HTML tag and attribute name syntax.

/// Report whether `name` is a conservatively valid HTML tag or attribute
/// name: an ASCII letter followed by ASCII alphanumerics, `:`, `-`, `_`, and
/// `.`.
pub fn is_valid_html_name(name: &str) -> bool {
	let mut bytes = name.bytes();
	bytes
		.next()
		.is_some_and(|first| first.is_ascii_alphabetic())
		&& bytes
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-' | b'_' | b'.'))
}
