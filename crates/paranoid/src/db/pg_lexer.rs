//! Shared lexical traversal for Paranoid-owned PostgreSQL text boundaries.
//!
//! This is deliberately not a SQL parser. It only separates executable SQL text from
//! quoted strings, quoted identifiers, and comments so callers can apply their own narrow
//! policy to unprotected text. Component migrations and portable queries also protect
//! dollar-quoted bodies from statement and placeholder processing.

#[derive(Clone, Copy)]
pub(crate) struct PgLexerOptions {
	pub(crate) literal_mode: PgLiteralMode,
}

/// Portable templates require unambiguous literals. Execution checks inspect both ambient
/// interpretations of ordinary strings without changing explicit escape-string semantics.
#[derive(Clone, Copy)]
pub(crate) enum PgLiteralMode {
	Portable,
	Execution { ordinary_backslash_escapes: bool },
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum PgLexeme<'sql> {
	Code(&'sql str),
	Protected(&'sql str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PgLexError {
	UnterminatedStringLiteral,
	OrdinaryStringBackslash,
	EscapeStringBackslashQuote,
	UnterminatedQuotedIdentifier,
	TooManyNestedBlockComments,
	UnterminatedBlockComment,
	UnterminatedDollarQuotedString,
	AmbiguousDollarAfterNumber,
}

/// This type records the token category at the end of scanned SQL code so that `PgLexer`
/// handles a following `$` byte consistently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodeEnd {
	/// The scan is at the start of the input, or after whitespace, punctuation, an operator, a
	/// closed protected token, or a lone `$`.
	Separator,
	/// The scan is at the end of an unquoted identifier or keyword, where a following `$` or
	/// ASCII digit continues that identifier.
	Identifier,
	/// The scan is at the end of a numeric literal of digits and periods or a `$n` parameter,
	/// where a following `$` can open a dollar-quoted string.
	Number,
	/// The scan is at the end of a numeric token that an identifier start byte directly follows,
	/// such as `1e5`, `0x1F`, or `1_000`. PostgreSQL 14 and later PostgreSQL versions lex a
	/// dollar-quote opener after such a token differently.
	NumberWithLetter,
}

impl CodeEnd {
	fn after_non_dollar_byte(self, byte: u8) -> Self {
		match (self, byte) {
			(Self::Identifier, byte) if is_identifier_byte(byte) => Self::Identifier,
			(Self::Number, b'0'..=b'9' | b'.') => Self::Number,
			(Self::Number | Self::NumberWithLetter, byte) if is_identifier_start_byte(byte) => {
				Self::NumberWithLetter
			}
			(Self::NumberWithLetter, b'0'..=b'9') => Self::NumberWithLetter,
			(_, b'0'..=b'9') => Self::Number,
			(_, byte) if is_identifier_start_byte(byte) => Self::Identifier,
			_ => Self::Separator,
		}
	}
}

pub(crate) struct PgLexer<'sql> {
	sql: &'sql str,
	state: PgLexerState,
	options: PgLexerOptions,
}

enum PgLexerState {
	Scanning { index: usize, code_end: CodeEnd },
	Finished,
}

impl<'sql> PgLexer<'sql> {
	pub(crate) fn new(sql: &'sql str, options: PgLexerOptions) -> Self {
		Self {
			sql,
			state: PgLexerState::Scanning {
				index: 0,
				code_end: CodeEnd::Separator,
			},
			options,
		}
	}

	fn next_lexeme(
		&self,
		start: usize,
		code_end: CodeEnd,
	) -> Result<(PgLexeme<'sql>, usize, CodeEnd), PgLexError> {
		let bytes = self.sql.as_bytes();

		match classify_byte(bytes, start, code_end) {
			ByteClassification::AmbiguousDollarAfterNumber => {
				Err(PgLexError::AmbiguousDollarAfterNumber)
			}
			ByteClassification::Protected(kind) => {
				let end = protected_end(bytes, start, kind, self.options.literal_mode)?;
				Ok((
					PgLexeme::Protected(&self.sql[start..end]),
					end,
					CodeEnd::Separator,
				))
			}
			ByteClassification::Code(mut next_end) => {
				let mut index = start + 1;
				while index < bytes.len() {
					match classify_byte(bytes, index, next_end) {
						ByteClassification::Code(updated_end) => {
							next_end = updated_end;
							index += 1;
						}
						ByteClassification::Protected(_)
						| ByteClassification::AmbiguousDollarAfterNumber => break,
					}
				}
				Ok((PgLexeme::Code(&self.sql[start..index]), index, next_end))
			}
		}
	}
}

impl<'sql> Iterator for PgLexer<'sql> {
	type Item = Result<PgLexeme<'sql>, PgLexError>;

	fn next(&mut self) -> Option<Self::Item> {
		let PgLexerState::Scanning { index, code_end } = self.state else {
			return None;
		};
		self.state = PgLexerState::Finished;
		if index >= self.sql.len() {
			return None;
		}
		Some(
			self.next_lexeme(index, code_end)
				.map(|(lexeme, next, next_end)| {
					if next < self.sql.len() {
						self.state = PgLexerState::Scanning {
							index: next,
							code_end: next_end,
						};
					}
					lexeme
				}),
		)
	}
}

#[derive(Clone, Copy)]
enum ProtectedKind {
	StringLiteral,
	QuotedIdentifier,
	LineComment,
	BlockComment,
	DollarQuotedString { delimiter_end: usize },
}

#[derive(Clone, Copy)]
enum ByteClassification {
	Code(CodeEnd),
	Protected(ProtectedKind),
	AmbiguousDollarAfterNumber,
}

fn classify_byte(bytes: &[u8], index: usize, code_end: CodeEnd) -> ByteClassification {
	match bytes[index] {
		b'\'' => ByteClassification::Protected(ProtectedKind::StringLiteral),
		b'"' => ByteClassification::Protected(ProtectedKind::QuotedIdentifier),
		b'-' if bytes.get(index + 1) == Some(&b'-') => {
			ByteClassification::Protected(ProtectedKind::LineComment)
		}
		b'/' if bytes.get(index + 1) == Some(&b'*') => {
			ByteClassification::Protected(ProtectedKind::BlockComment)
		}
		b'$' => match code_end {
			CodeEnd::Identifier => ByteClassification::Code(CodeEnd::Identifier),
			CodeEnd::NumberWithLetter => {
				if dollar_quote_delimiter_end(bytes, index).is_some() {
					ByteClassification::AmbiguousDollarAfterNumber
				} else {
					ByteClassification::Code(CodeEnd::NumberWithLetter)
				}
			}
			CodeEnd::Separator | CodeEnd::Number => {
				match dollar_quote_delimiter_end(bytes, index) {
					Some(delimiter_end) => {
						ByteClassification::Protected(ProtectedKind::DollarQuotedString {
							delimiter_end,
						})
					}
					None if bytes.get(index + 1).is_some_and(u8::is_ascii_digit) => {
						ByteClassification::Code(CodeEnd::Number)
					}
					None => ByteClassification::Code(CodeEnd::Separator),
				}
			}
		},
		byte => ByteClassification::Code(code_end.after_non_dollar_byte(byte)),
	}
}

fn protected_end(
	bytes: &[u8],
	start: usize,
	kind: ProtectedKind,
	literal_mode: PgLiteralMode,
) -> Result<usize, PgLexError> {
	match kind {
		ProtectedKind::StringLiteral => string_literal_end(bytes, start, literal_mode),
		ProtectedKind::QuotedIdentifier => quoted_identifier_end(bytes, start),
		ProtectedKind::LineComment => Ok(line_comment_end(bytes, start)),
		ProtectedKind::BlockComment => block_comment_end(bytes, start),
		ProtectedKind::DollarQuotedString { delimiter_end } => {
			dollar_quoted_string_end(bytes, start, delimiter_end)
		}
	}
}

fn string_literal_end(
	bytes: &[u8],
	start: usize,
	literal_mode: PgLiteralMode,
) -> Result<usize, PgLexError> {
	let is_escape_string = start >= 1
		&& matches!(bytes[start - 1], b'E' | b'e')
		&& (start < 2 || !is_identifier_byte(bytes[start - 2]));
	let backslash_escapes = match literal_mode {
		PgLiteralMode::Portable => is_escape_string,
		PgLiteralMode::Execution {
			ordinary_backslash_escapes,
		} => {
			// Bit, hex, and Unicode string literals do not use ordinary-string escape rules.
			let prefixed_without_escapes = (start >= 1
				&& matches!(bytes[start - 1], b'B' | b'b' | b'X' | b'x')
				&& (start < 2 || !is_identifier_byte(bytes[start - 2])))
				|| (start >= 2
					&& matches!(bytes[start - 2], b'U' | b'u')
					&& bytes[start - 1] == b'&'
					&& (start < 3 || !is_identifier_byte(bytes[start - 3])));
			is_escape_string || (!prefixed_without_escapes && ordinary_backslash_escapes)
		}
	};
	let mut index = start + 1;

	while index < bytes.len() {
		match bytes[index] {
			b'\\'
				if matches!(literal_mode, PgLiteralMode::Portable)
					&& is_escape_string
					&& bytes.get(index + 1) == Some(&b'\'') =>
			{
				return Err(PgLexError::EscapeStringBackslashQuote);
			}
			b'\\' if backslash_escapes => index = (index + 2).min(bytes.len()),
			b'\\' if matches!(literal_mode, PgLiteralMode::Portable) => {
				return Err(PgLexError::OrdinaryStringBackslash);
			}
			b'\'' if bytes.get(index + 1) == Some(&b'\'') => index += 2,
			b'\'' => {
				if matches!(literal_mode, PgLiteralMode::Execution { .. })
					&& let Some(continued) = continued_string_start(bytes, index + 1)
				{
					index = continued + 1;
				} else {
					return Ok(index + 1);
				}
			}
			_ => index += 1,
		}
	}

	Err(PgLexError::UnterminatedStringLiteral)
}

/// PostgreSQL continues a string across whitespace containing a newline, preserving its
/// original escape mode. Line comments are whitespace here; block comments are not.
fn continued_string_start(bytes: &[u8], mut index: usize) -> Option<usize> {
	enum Continuation {
		SameLine,
		AfterNewline,
	}
	let mut continuation = Continuation::SameLine;
	loop {
		match bytes.get(index) {
			Some(b'\n' | b'\r') => {
				continuation = Continuation::AfterNewline;
				index += 1;
			}
			Some(b' ' | b'\t' | 0x0b | 0x0c) => index += 1,
			Some(b'-') if bytes.get(index + 1) == Some(&b'-') => {
				index = line_comment_end(bytes, index)
			}
			Some(b'\'') if matches!(continuation, Continuation::AfterNewline) => {
				return Some(index);
			}
			_ => return None,
		}
	}
}

fn quoted_identifier_end(bytes: &[u8], start: usize) -> Result<usize, PgLexError> {
	let mut index = start + 1;
	while index < bytes.len() {
		match bytes[index] {
			b'"' if bytes.get(index + 1) == Some(&b'"') => index += 2,
			b'"' => return Ok(index + 1),
			_ => index += 1,
		}
	}
	Err(PgLexError::UnterminatedQuotedIdentifier)
}

fn line_comment_end(bytes: &[u8], start: usize) -> usize {
	let mut index = start + 2;
	while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
		index += 1;
	}
	index
}

fn block_comment_end(bytes: &[u8], start: usize) -> Result<usize, PgLexError> {
	let mut index = start + 2;
	let mut depth = 1_usize;
	while depth > 0 {
		if bytes.get(index) == Some(&b'/') && bytes.get(index + 1) == Some(&b'*') {
			depth = depth
				.checked_add(1)
				.ok_or(PgLexError::TooManyNestedBlockComments)?;
			index += 2;
		} else if bytes.get(index) == Some(&b'*') && bytes.get(index + 1) == Some(&b'/') {
			depth -= 1;
			index += 2;
		} else if index < bytes.len() {
			index += 1;
		} else {
			return Err(PgLexError::UnterminatedBlockComment);
		}
	}
	Ok(index)
}

fn dollar_quoted_string_end(
	bytes: &[u8],
	start: usize,
	delimiter_end: usize,
) -> Result<usize, PgLexError> {
	let delimiter = &bytes[start..delimiter_end];
	let body_start = delimiter_end;
	let relative_end = bytes[body_start..]
		.windows(delimiter.len())
		.position(|window| window == delimiter)
		.ok_or(PgLexError::UnterminatedDollarQuotedString)?;
	Ok(body_start + relative_end + delimiter.len())
}

fn dollar_quote_delimiter_end(bytes: &[u8], start: usize) -> Option<usize> {
	debug_assert_eq!(bytes.get(start), Some(&b'$'));

	let mut index = start + 1;
	match bytes.get(index).copied()? {
		b'$' => return Some(index + 1),
		byte if is_identifier_start_byte(byte) => index += 1,
		_ => return None,
	}

	while let Some(byte) = bytes.get(index).copied() {
		match byte {
			b'$' => return Some(index + 1),
			byte if is_identifier_byte(byte) => index += 1,
			_ => return None,
		}
	}
	None
}

pub(super) fn is_identifier_byte(byte: u8) -> bool {
	byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') || byte >= 0x80
}

pub(super) fn is_identifier_start_byte(byte: u8) -> bool {
	byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

/// Returns whether PostgreSQL 14 through 18 lex `byte` as ASCII whitespace in SQL code,
/// including vertical tab (`0x0b`), which PostgreSQL 16 through 18 treat as whitespace.
pub(super) const fn is_pg_ascii_whitespace(byte: u8) -> bool {
	matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

#[cfg(test)]
mod tests {
	use super::*;
	use proptest::prelude::*;

	const COMPONENT_OPTIONS: PgLexerOptions = PgLexerOptions {
		literal_mode: PgLiteralMode::Portable,
	};

	#[test]
	fn lexer_remains_finished_after_exhaustion_or_a_protected_token_error() {
		for sql in [
			"",
			"SELECT 1",
			"SELECT '",
			"SELECT /*",
			"SELECT $body$",
			"SELECT \"",
			"SELECT 1e5$$x$$",
		] {
			let mut lexer = PgLexer::new(sql, COMPONENT_OPTIONS);
			let result = lexer.by_ref().collect::<Result<Vec<_>, _>>();
			assert_eq!(
				result.is_err(),
				sql.starts_with("SELECT ") && sql != "SELECT 1"
			);
			assert_eq!(lexer.next(), None);
			assert_eq!(lexer.next(), None);
		}
	}

	#[test]
	fn recognizes_only_lexically_valid_dollar_quote_delimiter_tags() {
		assert_eq!(dollar_quote_delimiter_end(b"$$", 0), Some(2));
		assert_eq!(dollar_quote_delimiter_end(b"$body$", 0), Some(6));
		assert_eq!(dollar_quote_delimiter_end(b"($body$", 1), Some(7));

		assert_eq!(dollar_quote_delimiter_end(b"$1$", 0), None);
		assert_eq!(dollar_quote_delimiter_end(b"$-bad$", 0), None);
	}

	#[test]
	fn line_comments_end_at_lf_or_cr() {
		let lexemes = PgLexer::new("-- one\r$1\n-- two\n$2", COMPONENT_OPTIONS)
			.collect::<Result<Vec<_>, _>>()
			.expect("lex SQL");
		assert_eq!(
			lexemes,
			vec![
				PgLexeme::Protected("-- one"),
				PgLexeme::Code("\r$1\n"),
				PgLexeme::Protected("-- two"),
				PgLexeme::Code("\n$2"),
			]
		);
	}

	#[test]
	fn recognizes_valid_dollar_quote_openers_in_a_whole_string() {
		let lexemes = PgLexer::new(
			"$$body$$ $body$body$body$ ($body$body$body$)",
			COMPONENT_OPTIONS,
		)
		.collect::<Result<Vec<_>, _>>()
		.expect("lex SQL");
		assert_eq!(
			lexemes,
			vec![
				PgLexeme::Protected("$$body$$"),
				PgLexeme::Code(" "),
				PgLexeme::Protected("$body$body$body$"),
				PgLexeme::Code(" ("),
				PgLexeme::Protected("$body$body$body$"),
				PgLexeme::Code(")"),
			]
		);
		for sql in [
			"prefix$body$",
			"café$body$",
			"prefix$$",
			"a1$$",
			"$1$",
			"$-bad$",
		] {
			let lexemes = PgLexer::new(sql, COMPONENT_OPTIONS)
				.collect::<Result<Vec<_>, _>>()
				.expect("lex SQL");
			assert_eq!(lexemes, vec![PgLexeme::Code(sql)], "{sql}");
		}
	}

	#[test]
	fn recognizes_dollar_quotes_after_closing_delimiters_numbers_and_parameters() {
		for (sql, expected) in [
			(
				"SELECT $$a$$$$; COMMIT; $$",
				vec![
					PgLexeme::Code("SELECT "),
					PgLexeme::Protected("$$a$$"),
					PgLexeme::Protected("$$; COMMIT; $$"),
				],
			),
			(
				"SELECT 1$$ $2 $$",
				vec![PgLexeme::Code("SELECT 1"), PgLexeme::Protected("$$ $2 $$")],
			),
			(
				"SELECT $1$t$ $2 $t$",
				vec![
					PgLexeme::Code("SELECT $1"),
					PgLexeme::Protected("$t$ $2 $t$"),
				],
			),
			(
				"SELECT 1.5$$ $2 $$",
				vec![
					PgLexeme::Code("SELECT 1.5"),
					PgLexeme::Protected("$$ $2 $$"),
				],
			),
		] {
			let lexemes = PgLexer::new(sql, COMPONENT_OPTIONS)
				.collect::<Result<Vec<_>, _>>()
				.expect("lex SQL");
			assert_eq!(lexemes, expected, "{sql}");
		}
	}

	#[test]
	fn rejects_dollar_quotes_after_numeric_tokens_with_letters_or_underscores() {
		for sql in [
			"SELECT 1e5$$x$$",
			"SELECT 0x1F$$x$$",
			"SELECT 1_000$$x$$",
			"SELECT 0x1a$1$$x$$",
		] {
			let result = PgLexer::new(sql, COMPONENT_OPTIONS).collect::<Result<Vec<_>, _>>();
			assert_eq!(result, Err(PgLexError::AmbiguousDollarAfterNumber), "{sql}");
		}
	}

	#[test]
	fn backslash_escaped_apostrophes_in_escape_strings_are_rejected() {
		for sql in [r"SELECT E'can\'t'", r"SELECT e'can\'t'"] {
			let result = PgLexer::new(sql, COMPONENT_OPTIONS).collect::<Result<Vec<_>, _>>();
			assert_eq!(result, Err(PgLexError::EscapeStringBackslashQuote), "{sql}");
		}
	}

	proptest! {
		#![proptest_config(ProptestConfig {
			cases: 256,
			failure_persistence: None,
			..ProptestConfig::default()
		})]

		#[test]
		fn successful_lexing_partitions_arbitrary_utf8_without_modification(sql in any::<String>()) {
			for options in [
				COMPONENT_OPTIONS,
				PgLexerOptions {
					literal_mode: PgLiteralMode::Execution { ordinary_backslash_escapes: false },
				},
				PgLexerOptions {
					literal_mode: PgLiteralMode::Execution { ordinary_backslash_escapes: true },
				},
			] {
				let reconstructed = PgLexer::new(&sql, options).try_fold(
					String::new(),
					|mut reconstructed, lexeme| {
						let (PgLexeme::Code(text) | PgLexeme::Protected(text)) = lexeme?;
						reconstructed.push_str(text);
						Ok::<_, PgLexError>(reconstructed)
					},
				);
				if let Ok(reconstructed) = reconstructed {
					prop_assert_eq!(reconstructed.as_str(), sql.as_str());
				}
			}
		}
	}
}
