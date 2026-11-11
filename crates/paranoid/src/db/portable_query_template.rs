//! Splits a `portable_query` SQL template at its `$1`, `$2`, ... bind placeholders, then
//! rewrites the placeholders into pre-rendered literal fragments, so the finished statement
//! carries no bind parameters at all and can be sent as a single simple-protocol `Query`.
//!
//! The template is scanned once, byte-by-byte, keeping every span of SQL verbatim except
//! for placeholder tokens, which become the corresponding entry of `fragments` (1-indexed,
//! matching the order in which `.bind()` was called). Scanning must not mistake a `$`
//! that is part of a string literal, quoted identifier, or comment for a placeholder, so it
//! tracks those constructs precisely enough to skip over them:
//!
//! - `'...'` string literals, with the standard `''` doubled-quote escape. A backslash in an
//!   ordinary string is rejected so lexical boundaries cannot change with the session's
//!   `standard_conforming_strings` setting. Backslash escapes must use an explicit `E'...'`
//!   string, recognized by the literal `E`/`e` immediately preceding the opening quote.
//! - `"..."` quoted identifiers, with the standard `""` doubled-quote escape.
//! - `--` line comments, terminated by a newline or end of input.
//! - `/* ... */` block comments, which Postgres allows to nest, so nesting depth is tracked.
//! - Dollar-quoted strings with matching, case-sensitive delimiters.
//!
//! Dollar-quoted strings (`$$...$$` or `$tag$...$tag$`) and unquoted identifiers retain
//! their literal dollar signs. Only complete `$<digits>` parameter tokens outside
//! protected regions become bind placeholders.
//!
//! Byte-level scanning is safe here because every delimiter that this scanner recognizes (`'`,
//! `"`, `-`, `/`, `*`, `$`, digits, newline) is a single ASCII byte, and UTF-8 continuation
//! bytes (`0x80..=0xBF`) never equal an ASCII byte, so they can never be mistaken for a
//! delimiter mid-codepoint.

use super::PortableQueryTemplateError;
use super::pg_lexer::{
	PgLexError, PgLexeme, PgLexer, PgLexerOptions, PgLiteralMode, is_identifier_byte,
	is_identifier_start_byte,
};

/// A `portable_query` template split at its `$n` placeholders.
///
/// The parameter count is the highest placeholder index, matching the parameter count that
/// PostgreSQL infers for the extended protocol. Lower positions may remain unreferenced
/// (`$2` with two bound values is valid).
#[derive(Debug)]
pub(crate) struct PortableQueryTemplate {
	segments: Vec<Segment>,
	param_count: usize,
}

#[derive(Debug)]
enum Segment {
	Verbatim(String),
	/// A placeholder, as its zero-based position in the bound values.
	Param(usize),
}

impl PortableQueryTemplate {
	/// Splits `template` at its `$1`, `$2`, ... bind placeholders.
	pub(crate) fn parse(template: &str) -> Result<Self, PortableQueryTemplateError> {
		let mut segments = Vec::new();
		let mut param_count = 0_usize;

		let lexer = PgLexer::new(
			template,
			PgLexerOptions {
				literal_mode: PgLiteralMode::Portable,
			},
		);
		for lexeme in lexer {
			match lexeme.map_err(portable_lex_error)? {
				PgLexeme::Protected(text) => segments.push(Segment::Verbatim(text.to_owned())),
				PgLexeme::Code(text) => {
					parse_code_placeholders(text, &mut segments, &mut param_count)?;
				}
			}
		}

		Ok(Self {
			segments,
			param_count,
		})
	}

	/// The number of values that the template needs.
	pub(crate) fn param_count(&self) -> usize {
		self.param_count
	}

	/// The template as a statement: its text with each placeholder as `$n`, and no values.
	pub(crate) fn to_statement(&self) -> String {
		let mut out = String::new();
		for segment in &self.segments {
			match segment {
				Segment::Verbatim(text) => out.push_str(text),
				Segment::Param(position) => {
					out.push('$');
					out.push_str(&(position + 1).to_string());
				}
			}
		}
		out
	}

	/// Substitutes `fragments` (in the order of the `.bind()` calls) for the placeholders,
	/// returning the finished, parameter-free SQL text. The fragment count must equal the
	/// parameter count.
	pub(crate) fn render(
		&self,
		fragments: &[String],
	) -> Result<String, PortableQueryTemplateError> {
		if fragments.len() != self.param_count {
			return Err(PortableQueryTemplateError::new(format!(
				"portable_query SQL template requires {} bind value(s), but {} `.bind()` call(s) were supplied",
				self.param_count,
				fragments.len()
			)));
		}
		let mut out = String::new();
		for segment in &self.segments {
			match segment {
				Segment::Verbatim(text) => out.push_str(text),
				Segment::Param(position) => out.push_str(&fragments[*position]),
			}
		}
		Ok(out)
	}
}

fn parse_code_placeholders(
	code: &str,
	segments: &mut Vec<Segment>,
	param_count: &mut usize,
) -> Result<(), PortableQueryTemplateError> {
	let bytes = code.as_bytes();
	let mut copied_through = 0_usize;
	let mut index = 0_usize;

	while index < bytes.len() {
		if bytes[index].is_ascii_digit() {
			index += 1;
			while bytes
				.get(index)
				.is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
			{
				index += 1;
			}
			continue;
		}
		if is_identifier_start_byte(bytes[index]) {
			index += 1;
			while bytes
				.get(index)
				.is_some_and(|byte| is_identifier_byte(*byte))
			{
				index += 1;
			}
			continue;
		}
		if bytes[index] != b'$' {
			index += 1;
			continue;
		}

		if copied_through < index {
			segments.push(Segment::Verbatim(code[copied_through..index].to_owned()));
		}
		let digits_start = index + 1;
		let mut digits_end = digits_start;
		while bytes.get(digits_end).is_some_and(u8::is_ascii_digit) {
			digits_end += 1;
		}
		if digits_end == digits_start {
			return Err(PortableQueryTemplateError::new(
				"unsupported `$` construct in portable_query SQL template: expected a `$<digits>` bind placeholder or a complete dollar-quoted string",
			));
		}

		if bytes
			.get(digits_end)
			.is_some_and(|byte| is_identifier_byte(*byte))
		{
			return Err(PortableQueryTemplateError::new(
				"a portable_query bind placeholder must not continue as an identifier",
			));
		}
		let digits = &code[digits_start..digits_end];
		let placeholder_index: usize = digits.parse().map_err(|_| {
			PortableQueryTemplateError::new(format!(
				"bind placeholder `${digits}` is not a valid placeholder index"
			))
		})?;
		if placeholder_index == 0 {
			return Err(PortableQueryTemplateError::new(
				"bind placeholder `$0` is not valid; portable_query placeholders are 1-indexed",
			));
		}
		*param_count = (*param_count).max(placeholder_index);
		segments.push(Segment::Param(placeholder_index - 1));
		index = digits_end;
		copied_through = digits_end;
	}

	if copied_through < code.len() {
		segments.push(Segment::Verbatim(code[copied_through..].to_owned()));
	}
	Ok(())
}

fn portable_lex_error(error: PgLexError) -> PortableQueryTemplateError {
	let reason = match error {
		PgLexError::UnterminatedStringLiteral => {
			"unterminated string literal in portable_query SQL template"
		}
		PgLexError::OrdinaryStringBackslash => {
			"backslash in an ordinary string literal in portable_query SQL template; use an explicit E-prefixed escape string so scanning is independent of standard_conforming_strings"
		}
		PgLexError::EscapeStringBackslashQuote => {
			"backslash-escaped apostrophe in an escape string in portable_query SQL template; write a doubled apostrophe because the backslash_quote setting decides whether PostgreSQL accepts the backslash form"
		}
		PgLexError::UnterminatedQuotedIdentifier => {
			"unterminated quoted identifier in portable_query SQL template"
		}
		PgLexError::TooManyNestedBlockComments => {
			"too many nested block comments in portable_query SQL template"
		}
		PgLexError::UnterminatedBlockComment => {
			"unterminated block comment in portable_query SQL template"
		}
		PgLexError::UnterminatedDollarQuotedString => {
			"unterminated dollar-quoted string in portable_query SQL template"
		}
		PgLexError::AmbiguousDollarAfterNumber => {
			"dollar-quoted string directly after a numeric token with letters or underscores in portable_query SQL template; put whitespace before the dollar sign because PostgreSQL versions lex this boundary differently"
		}
	};
	PortableQueryTemplateError::new(reason)
}

#[cfg(test)]
mod tests {
	use super::*;
	use proptest::prelude::*;

	fn render(template: &str, fragments: &[&str]) -> Result<String, PortableQueryTemplateError> {
		let fragments: Vec<String> = fragments.iter().map(|value| (*value).to_owned()).collect();
		PortableQueryTemplate::parse(template)?.render(&fragments)
	}

	#[test]
	fn substitutes_placeholders_in_order() {
		assert_eq!(
			render("SELECT $1 WHERE x = $2", &["(1)::bigint", "E'a'"]).unwrap(),
			"SELECT (1)::bigint WHERE x = E'a'"
		);
	}

	#[test]
	fn substitutes_repeated_placeholder_multiple_times() {
		assert_eq!(
			render("SELECT $1, $1", &["(1)::bigint"]).unwrap(),
			"SELECT (1)::bigint, (1)::bigint"
		);
	}

	#[test]
	fn multi_digit_placeholder_indices_parse_correctly() {
		let fragments: Vec<&str> = (0..10).map(|_| "TRUE").collect();
		assert_eq!(render("SELECT $10", &fragments).unwrap(), "SELECT TRUE");
	}

	#[test]
	fn skips_dollar_inside_plain_string_literal() {
		assert_eq!(
			render("SELECT 'a$1b' WHERE x = $1", &["(1)::bigint"]).unwrap(),
			"SELECT 'a$1b' WHERE x = (1)::bigint"
		);
	}

	#[test]
	fn skips_doubled_single_quote_escape_in_plain_string() {
		assert_eq!(
			render("SELECT 'it''s $1 fine' WHERE x = $1", &["(1)::bigint"]).unwrap(),
			"SELECT 'it''s $1 fine' WHERE x = (1)::bigint"
		);
	}

	#[test]
	fn ordinary_string_backslash_is_rejected_independently_of_session_settings() {
		assert!(render(r"SELECT 'a\' WHERE x = $1", &["(1)::bigint"]).is_err());
		assert!(render(r"SELECT 'ordinary\backslash'", &[]).is_err());
	}

	#[test]
	fn escape_string_backslash_escapes_the_next_byte_except_an_apostrophe() {
		for prefix in ["E", "e"] {
			let escaped_backslash = format!(r"SELECT {prefix}'a\\'' $1 b' WHERE x = $1");
			assert_eq!(
				render(&escaped_backslash, &["(1)::bigint"]).unwrap(),
				format!(r"SELECT {prefix}'a\\'' $1 b' WHERE x = (1)::bigint")
			);
		}
	}

	#[test]
	fn backslash_escaped_apostrophes_are_rejected_for_either_escape_string_prefix() {
		for prefix in ["E", "e"] {
			let error = render(
				&format!(r"SELECT {prefix}'a\' $1 b' WHERE x = $1"),
				&["(1)::bigint"],
			)
			.unwrap_err();
			assert!(
				error.reason().contains("backslash_quote"),
				"{}",
				error.reason()
			);
		}
	}

	#[test]
	fn identifier_ending_in_e_before_a_string_is_not_an_escape_string() {
		assert_eq!(
			render("SELECT TABLE'a$1b' WHERE x = $1", &["(1)::bigint"]).unwrap(),
			"SELECT TABLE'a$1b' WHERE x = (1)::bigint"
		);
	}

	#[test]
	fn skips_dollar_inside_quoted_identifier() {
		assert_eq!(
			render(r#"SELECT "a$1b" WHERE x = $1"#, &["(1)::bigint"]).unwrap(),
			r#"SELECT "a$1b" WHERE x = (1)::bigint"#
		);
	}

	#[test]
	fn skips_doubled_double_quote_escape_in_identifier() {
		assert_eq!(
			render(r#"SELECT "a""$1""b" WHERE x = $1"#, &["(1)::bigint"]).unwrap(),
			r#"SELECT "a""$1""b" WHERE x = (1)::bigint"#
		);
	}

	#[test]
	fn skips_dollar_inside_line_comment() {
		assert_eq!(
			render("SELECT $1 -- a $2 comment\nWHERE x = $1", &["(1)::bigint"]).unwrap(),
			"SELECT (1)::bigint -- a $2 comment\nWHERE x = (1)::bigint"
		);
	}

	#[test]
	fn carriage_return_ends_a_line_comment_before_the_next_placeholder() {
		assert_eq!(
			render("SELECT $1 -- a $2 comment\rWHERE x = $1", &["(1)::bigint"]).unwrap(),
			"SELECT (1)::bigint -- a $2 comment\rWHERE x = (1)::bigint"
		);
	}

	#[test]
	fn line_comment_without_trailing_newline_is_terminated_by_end_of_input() {
		assert_eq!(
			render("SELECT $1 -- a $2 comment", &["(1)::bigint"]).unwrap(),
			"SELECT (1)::bigint -- a $2 comment"
		);
	}

	#[test]
	fn skips_dollar_inside_block_comment() {
		assert_eq!(
			render(
				"SELECT $1 /* a $2 comment */ WHERE x = $1",
				&["(1)::bigint"]
			)
			.unwrap(),
			"SELECT (1)::bigint /* a $2 comment */ WHERE x = (1)::bigint"
		);
	}

	#[test]
	fn nested_block_comments_track_depth() {
		assert_eq!(
			render(
				"SELECT $1 /* outer /* inner $2 */ still comment */ x",
				&["(1)::bigint"]
			)
			.unwrap(),
			"SELECT (1)::bigint /* outer /* inner $2 */ still comment */ x"
		);
	}

	#[test]
	fn unterminated_string_literal_is_rejected() {
		assert!(render("SELECT 'unterminated", &[]).is_err());
	}

	#[test]
	fn unterminated_quoted_identifier_is_rejected() {
		assert!(render(r#"SELECT "unterminated"#, &[]).is_err());
	}

	#[test]
	fn unterminated_block_comment_is_rejected() {
		assert!(render("SELECT /* unterminated", &[]).is_err());
	}

	#[test]
	fn dollar_quoted_strings_preserve_literal_placeholders_and_delimiters() {
		for literal in [
			"$$literal $1$$",
			"$tag$literal $2 $other$ $tag$",
			"$λ$literal $3$λ$",
		] {
			let template = format!("SELECT {literal}, $1");
			assert_eq!(
				render(&template, &["TRUE"]).unwrap(),
				format!("SELECT {literal}, TRUE")
			);
		}
	}

	#[test]
	fn unquoted_identifiers_preserve_dollars_and_decimal_suffixes() {
		for identifier in ["price$1", "_price$2", "café$3", "table$tag$", "table$$"] {
			let template = format!("SELECT {identifier}, $1 FROM prices");
			assert_eq!(
				render(&template, &["TRUE"]).unwrap(),
				format!("SELECT {identifier}, TRUE FROM prices")
			);
		}
	}

	#[test]
	fn numeric_tokens_and_dollar_quotes_adjacent_to_placeholders_parse_cleanly() {
		assert_eq!(render("SELECT 1e5$1", &[" + 1"]).unwrap(), "SELECT 1e5 + 1");
		assert_eq!(
			render("SELECT 0x1a$1", &[" + 1"]).unwrap(),
			"SELECT 0x1a + 1"
		);
		assert_eq!(
			render("SELECT 1_000$1", &[" + 1"]).unwrap(),
			"SELECT 1_000 + 1"
		);
		assert_eq!(render("SELECT 1$$ $2 $$", &[]).unwrap(), "SELECT 1$$ $2 $$");
		assert_eq!(
			render("SELECT $1$t$ $2 $t$", &["(1)::bigint"]).unwrap(),
			"SELECT (1)::bigint$t$ $2 $t$"
		);
		for ambiguous in ["SELECT 1e5$$x$$", "SELECT 0x1F$$x$$"] {
			assert!(render(ambiguous, &[]).is_err(), "{ambiguous}");
		}
	}

	#[test]
	fn a_placeholder_cannot_continue_as_an_identifier() {
		for template in [
			"SELECT $1name",
			"SELECT $1_name",
			"SELECT $1é",
			"SELECT $1$2",
		] {
			assert!(render(template, &["TRUE"]).is_err(), "{template}");
		}
	}

	#[test]
	fn placeholder_index_beyond_bound_values_is_rejected() {
		assert!(render("SELECT $1", &[]).is_err());
		assert!(render("SELECT $2", &["(1)::bigint"]).is_err());
	}

	#[test]
	fn zero_placeholder_index_is_rejected() {
		assert!(render("SELECT $0", &["(1)::bigint"]).is_err());
	}

	#[test]
	fn more_bound_values_than_the_highest_placeholder_is_rejected() {
		assert!(render("SELECT $1", &["(1)::bigint", "(2)::bigint"]).is_err());
		assert!(render("SELECT 1", &["(1)::bigint"]).is_err());
	}

	#[test]
	fn an_unreferenced_lower_param_is_permitted() {
		assert_eq!(
			render("SELECT $2", &["(1)::bigint", "(2)::bigint"]).unwrap(),
			"SELECT (2)::bigint"
		);
	}

	#[test]
	fn no_placeholders_returns_template_unchanged() {
		assert_eq!(render("SELECT 1 FROM t", &[]).unwrap(), "SELECT 1 FROM t");
	}

	#[test]
	fn multibyte_characters_outside_delimiters_round_trip() {
		assert_eq!(
			render("SELECT $1 -- café\nWHERE name = 'café'", &["(1)::bigint"]).unwrap(),
			"SELECT (1)::bigint -- café\nWHERE name = 'café'"
		);
	}

	#[test]
	fn malformed_retained_inputs_are_rejected() {
		for (template, fragments) in [
			("SELECT $$unterminated", &[][..]),
			("SELECT $0", &["(1)::integer"][..]),
			("SELECT 'unterminated", &[][..]),
			("SELECT \"unterminated", &[][..]),
			("SELECT /* unterminated", &[][..]),
			("SELECT $999999999999999999999999999999999999", &[][..]),
		] {
			assert!(render(template, fragments).is_err(), "{template}");
		}
	}

	#[test]
	fn mixed_protected_and_executable_spans_render_byte_for_byte() {
		assert_eq!(
			render(
				"SELECT $1 /* $2 */ -- $3\rWHERE value = $1",
				&["(1)::integer"]
			)
			.unwrap(),
			"SELECT (1)::integer /* $2 */ -- $3\rWHERE value = (1)::integer"
		);
	}

	#[test]
	fn corpus_templates_render_or_fail_as_the_published_corpus_expects() {
		assert_eq!(
			render("SELECT $1, $2 WHERE x = $3", &["a", "b", "c"]).unwrap(),
			"SELECT a, b WHERE x = c"
		);
		assert_eq!(
			render(
				"SELECT $1 -- comment with $2 inside\n , 'lit$3eral'",
				&["a"]
			)
			.unwrap(),
			"SELECT a -- comment with $2 inside\n , 'lit$3eral'"
		);
		assert!(render("SELECT $$unterminated", &["a", "b", "c"]).is_err());
		assert_eq!(render("", &[]).unwrap(), "");
	}

	/// One part of a generated template.
	#[derive(Clone, Debug)]
	enum Part {
		Plain(String),
		String(String),
		EscapeString(String),
		QuotedIdentifier(String),
		LineComment(String),
		BlockComment(String),
		NestedBlockComment(String, String),
		Placeholder(usize),
	}

	fn part() -> impl Strategy<Value = Part> {
		prop_oneof![
			"[a-zA-Z0-9 ]{0,6}".prop_map(Part::Plain),
			any::<String>().prop_map(Part::String),
			any::<String>().prop_map(Part::EscapeString),
			any::<String>().prop_map(Part::QuotedIdentifier),
			"[^\r\n]{0,12}".prop_map(Part::LineComment),
			"[^/*]{0,12}".prop_map(Part::BlockComment),
			("[^/*]{0,6}", "[^/*]{0,6}")
				.prop_map(|(outer, inner)| Part::NestedBlockComment(outer, inner)),
			(0_usize..9).prop_map(Part::Placeholder),
		]
	}

	proptest! {
		#![proptest_config(ProptestConfig {
			cases: 512,
			failure_persistence: None,
			..ProptestConfig::default()
		})]

		#[test]
		fn any_template_and_fragments_give_a_result_without_panicking(
			template in any::<String>(),
			fragments in prop::collection::vec(any::<String>(), 0..8),
		) {
			let fragments = fragments.iter().map(String::as_str).collect::<Vec<_>>();
			let _ = render(&template, &fragments);
		}

		#[test]
		fn well_formed_placeholders_keep_their_order_and_count(
			chunks in prop::collection::vec(prop::sample::select(vec![
				"SELECT * FROM users WHERE id = ",
				"INSERT INTO table (a, b) VALUES (",
				"UPDATE t SET x = ",
				"DELETE FROM t WHERE key = ",
				"/* block comment */ ",
				"-- line comment\n ",
				"'string literal with spaces' ",
				"\"quoted_identifier\" ",
			]), 1..5),
			params in prop::collection::vec(any::<String>(), 1..3),
		) {
			let mut template = String::new();
			let mut expected = String::new();
			for (index, param) in params.iter().enumerate() {
				let chunk = chunks[index % chunks.len()];
				template.push_str(&format!("{chunk}${} ", index + 1));
				expected.push_str(&format!("{chunk}{param} "));
			}
			let fragments = params.iter().map(String::as_str).collect::<Vec<_>>();
			prop_assert_eq!(render(&template, &fragments).unwrap(), expected);
		}

		#[test]
		fn dollar_quoted_strings_preserve_their_body_for_every_tag(
			tag in "[a-zA-Z_][a-zA-Z0-9_]{0,12}",
			body in "[^$]{0,30}",
		) {
			let template = format!("SELECT ${tag}${body} $2 ${tag}$ WHERE id = $1");
			prop_assert_eq!(render(&template, &["param1"]).unwrap(), format!("SELECT ${tag}${body} $2 ${tag}$ WHERE id = param1"));
		}

		#[test]
		fn placeholders_inside_strings_are_not_placeholders(
			inner in "[^\\\\\0]{0,50}",
			fake_index in 1_u32..100,
		) {
			let escaped = inner.replace('\'', "''");
			let literal = format!("'prefix ${fake_index} {escaped}'");
			let template = format!("SELECT {literal}, $1");
			prop_assert_eq!(
				render(&template, &["real_param"]).unwrap(),
				format!("SELECT {literal}, real_param")
			);
		}

		#[test]
		fn placeholders_inside_block_comments_are_not_placeholders(
			content in "[^/*]{0,30}",
			fake_index in 1_u32..50,
		) {
			let comment = format!("/* comment ${fake_index} /* nested */ {content} */");
			let template = format!("SELECT {comment} $1");
			prop_assert_eq!(
				render(&template, &["valid_arg"]).unwrap(),
				format!("SELECT {comment} valid_arg")
			);
		}

		#[test]
		fn every_segment_kind_keeps_protected_placeholders_and_delimiters(
			parts in prop::collection::vec(part(), 0..12),
			fragments in prop::collection::vec("frag[a-z0-9]{0,4}", 9),
		) {
			let mut template = String::new();
			let mut expected = String::new();
			let mut highest = 0;
			let ordinary_backslash = parts.iter().any(|part| {
				matches!(part, Part::String(text) if text.contains('\\'))
			});
			for part in &parts {
				let segment = match part {
					Part::Plain(text) => text.clone(),
					Part::String(text) => {
						format!("'{}'", text.replace('\'', "''"))
					}
					Part::EscapeString(text) => {
						format!("E'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
					}
					Part::QuotedIdentifier(text) => format!("\"{}\"", text.replace('"', "\"\"")),
					Part::LineComment(text) => format!("-- {text}\n"),
					Part::BlockComment(text) => format!("/*{text}*/"),
					Part::NestedBlockComment(outer, inner) => {
						format!("/*{outer} /*{inner}*/ {outer}*/")
					}
					Part::Placeholder(index) => {
						template.push_str(&format!(" ${}", index + 1));
						expected.push_str(&format!(" {}", fragments[*index]));
						highest = highest.max(index + 1);
						continue;
					}
				};
				template.push_str(&format!(" {segment}"));
				expected.push_str(&format!(" {segment}"));
			}
			let supplied = fragments[..highest].iter().map(String::as_str).collect::<Vec<_>>();
			let rendered = render(&template, &supplied);
			if ordinary_backslash {
				prop_assert!(rendered.is_err());
			} else {
				prop_assert_eq!(rendered.unwrap(), expected);
			}
		}

		#[test]
		fn malformed_templates_are_always_rejected(
			selector in 0_usize..6,
			rest in any::<String>(),
		) {
			let template = [
				format!("SELECT '{}", rest.replace('\'', "")),
				format!("SELECT /*{}", rest.replace(['/', '*'], "")),
				format!("SELECT \"{}", rest.replace('"', "")),
				"SELECT $$unterminated".to_owned(),
				"SELECT $0".to_owned(),
				"SELECT $999999999999999999999999999999".to_owned(),
			][selector].clone();
			prop_assert!(render(&template, &["only_fragment"]).is_err());
		}
	}
}
