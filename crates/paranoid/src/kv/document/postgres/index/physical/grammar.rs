//! Closed catalog expression comparison for compound document index format one.

use crate::db::bytea::decode_text_output;
use crate::kv::document::schema::IndexScalarType;
use crate::records::postgres::projection;

use super::{ExpectedIndexField, PgDocumentIndexExpectedCatalog};

#[derive(Debug, thiserror::Error)]
pub(crate) enum PgDocumentIndexExpressionError {
	#[error("catalog expression has invalid lexical structure")]
	Lexical,
	#[error("catalog expression is not the required structure")]
	Structure,
	#[error("catalog expression constant is not its required typed value")]
	Constant,
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
	Name(String),
	Text(Vec<u8>),
	Symbol(char),
	Cast,
}
struct Tokens {
	tokens: Vec<Token>,
	closes: Vec<Option<usize>>,
}
#[derive(Clone, Copy)]
struct View<'a> {
	owner: &'a Tokens,
	start: usize,
	end: usize,
}
impl Tokens {
	fn parse(sql: &str, ordinary_escapes: bool) -> Result<Self, PgDocumentIndexExpressionError> {
		let bytes = sql.as_bytes();
		let mut tokens = Vec::new();
		let mut i = 0;
		while i < bytes.len() {
			let byte = bytes[i];
			if byte.is_ascii_whitespace() {
				i += 1;
				continue;
			}
			if byte == b'\'' || byte == b'"' {
				let quote = byte;
				let value = parse_quoted(bytes, &mut i, quote, ordinary_escapes)?;
				tokens.push(if quote == b'\'' {
					Token::Text(value)
				} else {
					Token::Name(
						String::from_utf8(value)
							.map_err(|_| PgDocumentIndexExpressionError::Lexical)?,
					)
				});
				continue;
			}
			if byte.is_ascii_alphabetic() || byte == b'_' {
				let start = i;
				i += 1;
				while bytes
					.get(i)
					.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$')
				{
					i += 1;
				}
				tokens.push(Token::Name(sql[start..i].to_ascii_lowercase()));
				continue;
			}
			if byte == b':' && bytes.get(i + 1) == Some(&b':') {
				tokens.push(Token::Cast);
				i += 2;
				continue;
			}
			if b"().[],=".contains(&byte) {
				tokens.push(Token::Symbol(char::from(byte)));
				i += 1;
				continue;
			}
			return Err(PgDocumentIndexExpressionError::Lexical);
		}
		let mut closes = vec![None; tokens.len()];
		let mut stack = Vec::new();
		for (position, token) in tokens.iter().enumerate() {
			match token {
				Token::Symbol('(' | '[') => stack.push(position),
				Token::Symbol(')' | ']') => {
					let open = stack.pop().ok_or(PgDocumentIndexExpressionError::Lexical)?;
					if !matches!(
						(&tokens[open], token),
						(Token::Symbol('('), Token::Symbol(')'))
							| (Token::Symbol('['), Token::Symbol(']'))
					) {
						return Err(PgDocumentIndexExpressionError::Lexical);
					}
					closes[open] = Some(position);
				}
				_ => {}
			}
		}
		if !stack.is_empty() {
			return Err(PgDocumentIndexExpressionError::Lexical);
		}
		Ok(Self { tokens, closes })
	}
	fn view(&self) -> View<'_> {
		View {
			owner: self,
			start: 0,
			end: self.tokens.len(),
		}
	}
}
fn parse_quoted(
	bytes: &[u8],
	position: &mut usize,
	quote: u8,
	ordinary_escapes: bool,
) -> Result<Vec<u8>, PgDocumentIndexExpressionError> {
	*position += 1;
	let mut value = Vec::new();
	loop {
		let byte = *bytes
			.get(*position)
			.ok_or(PgDocumentIndexExpressionError::Lexical)?;
		if byte == quote {
			*position += 1;
			if bytes.get(*position) != Some(&quote) {
				return Ok(value);
			}
			value.push(quote);
			*position += 1;
		} else if quote == b'\'' && ordinary_escapes && byte == b'\\' {
			// The deparser doubles each literal backslash under this observed setting.
			if bytes.get(*position + 1) != Some(&b'\\') {
				return Err(PgDocumentIndexExpressionError::Lexical);
			}
			value.push(b'\\');
			*position += 2;
		} else {
			value.push(byte);
			*position += 1;
		}
	}
}

impl<'a> View<'a> {
	fn strip(mut self) -> Self {
		while self.owner.tokens.get(self.start) == Some(&Token::Symbol('('))
			&& self.owner.closes[self.start] == self.end.checked_sub(1)
		{
			self.start += 1;
			self.end -= 1;
		}
		self
	}
	fn part(self, start: usize, end: usize) -> Self {
		Self {
			owner: self.owner,
			start,
			end,
		}
	}
	fn slice(self) -> &'a [Token] {
		&self.owner.tokens[self.start..self.end]
	}
	fn top(self, target: &Token) -> Vec<usize> {
		let mut found = Vec::new();
		let mut i = self.start;
		while i < self.end {
			if &self.owner.tokens[i] == target {
				found.push(i);
			}
			i = self.owner.closes[i].map_or(i + 1, |end| end + 1);
		}
		found
	}
	fn split(self, token: &Token) -> Vec<Self> {
		let mut start = self.start;
		let mut result = Vec::new();
		for end in self.top(token) {
			result.push(self.part(start, end));
			start = end + 1;
		}
		result.push(self.part(start, self.end));
		result
	}
}
fn matches_name(view: View<'_>, expected: &str, qualified: bool) -> bool {
	let view = view.strip();
	match view.slice() {
		[Token::Name(actual)] => actual == expected,
		[Token::Name(schema), Token::Symbol('.'), Token::Name(actual)] => {
			qualified && schema == "pg_catalog" && actual == expected
		}
		_ => false,
	}
}
fn cast<'a>(view: View<'a>, aliases: &[&str]) -> Option<View<'a>> {
	let view = view.strip();
	let positions = view.top(&Token::Cast);
	let [position] = positions.as_slice() else {
		return None;
	};
	let kind = view.part(*position + 1, view.end);
	aliases
		.iter()
		.any(|alias| matches_name(kind, alias, true))
		.then(|| view.part(view.start, *position).strip())
}
fn matches_text(view: View<'_>, expected: &[u8]) -> bool {
	let view = cast(view, &["text"]).unwrap_or(view).strip();
	matches!(view.slice(), [Token::Text(value)] if value == expected)
}
fn matches_bytea(view: View<'_>, expected: &[u8]) -> bool {
	let Some(value) = cast(view, &["bytea"]) else {
		return false;
	};
	let [Token::Text(value)] = value.slice() else {
		return false;
	};
	decode_text_output(value).is_ok_and(|actual| actual == expected)
}
fn call<'a>(view: View<'a>, expected: &str) -> Option<Vec<View<'a>>> {
	let view = view.strip();
	let open = view.top(&Token::Symbol('('));
	let [position] = open.as_slice() else {
		return None;
	};
	if !matches_name(view.part(view.start, *position), expected, true)
		|| view.owner.closes[*position] != view.end.checked_sub(1)
	{
		return None;
	}
	Some(
		view.part(*position + 1, view.end - 1)
			.split(&Token::Symbol(',')),
	)
}
fn matches_encode_key(view: View<'_>, key: &[u8]) -> bool {
	let Some(args) = call(view, "encode") else {
		return false;
	};
	matches!(args.as_slice(), [bytes, format] if matches_bytea(*bytes, key) && matches_text(*format, b"hex"))
}
fn matches_lookup(view: View<'_>, key: &[u8], value: bool) -> bool {
	let Some(args) = call(
		view,
		if value {
			"jsonb_extract_path_text"
		} else {
			"jsonb_extract_path"
		},
	) else {
		return false;
	};
	let Some(first) = args.first() else {
		return false;
	};
	if !matches_name(*first, "index_fields", false) {
		return false;
	}
	let direct = &args[1..];
	if !value && matches!(direct, [single] if matches_encode_key(*single, key)) {
		return true;
	}
	let paths;
	let path = if let [array] = direct {
		let array = array.strip();
		let tokens = array.slice();
		let prefix =
			usize::from(matches!(tokens.first(), Some(Token::Name(word)) if word == "variadic"));
		let start = array.start + prefix;
		if !matches!(array.owner.tokens.get(start), Some(Token::Name(word)) if word == "array")
			|| array.owner.tokens.get(start + 1) != Some(&Token::Symbol('['))
			|| array.owner.closes[start + 1] != array.end.checked_sub(1)
		{
			return false;
		}
		paths = array
			.part(start + 2, array.end - 1)
			.split(&Token::Symbol(','));
		paths.as_slice()
	} else {
		direct
	};
	if value {
		matches!(path, [field, val] if matches_encode_key(*field, key) && matches_text(*val, b"value"))
	} else {
		matches!(path, [field] if matches_encode_key(*field, key))
	}
}
fn matches_field(view: View<'_>, expected: &ExpectedIndexField) -> bool {
	match expected.kind {
		IndexScalarType::Boolean => cast(view, &["bool", "boolean"]).is_some_and(|v| matches_lookup(v, &expected.projection_key, true)),
		IndexScalarType::SignedInteger | IndexScalarType::Timestamp => cast(view, &["int8", "bigint"]).is_some_and(|v| matches_lookup(v, &expected.projection_key, true)),
		IndexScalarType::BytewiseText => matches_lookup(view, &expected.projection_key, true),
		IndexScalarType::Bytes => call(view, "decode").is_some_and(|args| matches!(args.as_slice(), [v, format] if matches_lookup(*v, &expected.projection_key, true) && matches_text(*format, b"hex"))),
	}
}
fn equality(view: View<'_>) -> Option<(View<'_>, View<'_>)> {
	let view = view.strip();
	let equals = view.top(&Token::Symbol('='));
	if let [position] = equals.as_slice() {
		return Some((
			view.part(view.start, *position),
			view.part(*position + 1, view.end),
		));
	}
	let operators = view.top(&Token::Name("operator".to_owned()));
	let [position] = operators.as_slice() else {
		return None;
	};
	let end = view.owner.closes.get(*position + 1).copied().flatten()?;
	if view.owner.tokens.get(*position + 1) != Some(&Token::Symbol('('))
		|| !matches!(&view.owner.tokens[*position + 2..end], [Token::Name(schema), Token::Symbol('.'), Token::Symbol('=')] if schema == "pg_catalog")
	{
		return None;
	}
	Some((
		view.part(view.start, *position),
		view.part(end + 1, view.end),
	))
}
fn conjuncts(view: View<'_>) -> Vec<View<'_>> {
	let mut pending = vec![view];
	let mut result = Vec::new();
	while let Some(next) = pending.pop() {
		let next = next.strip();
		let parts = next.split(&Token::Name("and".to_owned()));
		if parts.len() == 1 {
			result.push(next);
		} else {
			pending.extend(parts.into_iter().rev());
		}
	}
	result
}

pub(super) fn check_expressions(
	sql: Option<&str>,
	expected: &PgDocumentIndexExpectedCatalog,
	ordinary_escapes: bool,
) -> Result<(), PgDocumentIndexExpressionError> {
	if expected.fields.is_empty() {
		return if sql.is_none() {
			Ok(())
		} else {
			Err(PgDocumentIndexExpressionError::Structure)
		};
	}
	let tokens = Tokens::parse(
		sql.ok_or(PgDocumentIndexExpressionError::Structure)?,
		ordinary_escapes,
	)?;
	let expressions = tokens.view().split(&Token::Symbol(','));
	if expressions.len() != expected.fields.len()
		|| !expressions
			.iter()
			.zip(&expected.fields)
			.all(|(actual, expected)| matches_field(*actual, expected))
	{
		return Err(PgDocumentIndexExpressionError::Structure);
	}
	Ok(())
}
pub(super) fn check_predicate(
	sql: &str,
	expected: &PgDocumentIndexExpectedCatalog,
	ordinary_escapes: bool,
) -> Result<(), PgDocumentIndexExpressionError> {
	let tokens = Tokens::parse(sql, ordinary_escapes)?;
	let parts = conjuncts(tokens.view());
	let [namespace, body, coverage] = parts.as_slice() else {
		return Err(PgDocumentIndexExpressionError::Structure);
	};
	let Some((left, right)) = equality(*namespace) else {
		return Err(PgDocumentIndexExpressionError::Structure);
	};
	if !matches_name(left, "namespace", false)
		|| !matches_bytea(right, expected.descriptor.scope().namespace().as_bytes())
	{
		return Err(PgDocumentIndexExpressionError::Constant);
	}
	if !matches!(body.strip().slice(), [Token::Name(body), Token::Name(is), Token::Name(not), Token::Name(null)] if body == "body" && is == "is" && not == "not" && null == "null")
	{
		return Err(PgDocumentIndexExpressionError::Structure);
	}
	let Some((left, right)) = equality(*coverage) else {
		return Err(PgDocumentIndexExpressionError::Structure);
	};
	if !matches_lookup(left, &expected.coverage_key, false) {
		return Err(PgDocumentIndexExpressionError::Structure);
	}
	let json = cast(right, &["jsonb"]).ok_or(PgDocumentIndexExpressionError::Structure)?;
	let [Token::Text(value)] = json.slice() else {
		return Err(PgDocumentIndexExpressionError::Structure);
	};
	if !std::str::from_utf8(value).is_ok_and(projection::is_true_coverage_json) {
		return Err(PgDocumentIndexExpressionError::Constant);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn quoted_tokens_require_a_complete_delimiter() {
		for (sql, ordinary_escapes) in [
			("'unfinished", false),
			("\"unfinished", false),
			("'doubled''", false),
			("\"doubled\"\"", false),
			("'trailing\\", true),
			("'single\\escape'", true),
		] {
			assert!(
				matches!(
					Tokens::parse(sql, ordinary_escapes),
					Err(PgDocumentIndexExpressionError::Lexical)
				),
				"{sql:?}"
			);
		}
	}

	#[test]
	fn quoted_tokens_preserve_doubled_delimiters_and_observed_backslash_rules() {
		let tokens = Tokens::parse("'a''b', \"a\"\"b\"", false).unwrap();
		assert_eq!(
			tokens.tokens,
			[
				Token::Text(b"a'b".to_vec()),
				Token::Symbol(','),
				Token::Name("a\"b".to_owned())
			]
		);
		for (sql, ordinary_escapes) in [("'a\\b'", false), ("'a\\\\b'", true)] {
			assert_eq!(
				Tokens::parse(sql, ordinary_escapes).unwrap().tokens,
				[Token::Text(b"a\\b".to_vec())]
			);
		}
	}
}
