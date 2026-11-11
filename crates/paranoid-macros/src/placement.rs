//! Where a placeholder may stand in a template, and where a template must end.
//!
//! A placeholder stands only in SQL code, outside every string constant, quoted identifier,
//! dollar-quoted string, and comment, and only between two neighbors that keep its rendering a
//! token sequence of its own: on each side the start or the end of the template, an ASCII white
//! space character of SQL, or a character of [`ALLOWED_BEFORE`] or [`ALLOWED_AFTER`]. The
//! template itself ends in SQL code, so the text that follows the template in a composed
//! statement starts in SQL code too.
//!
//! A rendering starts with `(`, `"`, or `E'` and ends with `)`, `"`, or a letter, and it
//! contains no `$`, no line break, no `*/`, and no unbalanced quote. None of the allowed
//! neighbors can therefore join the rendering's first or last token, and the rendering leaves
//! the lexical context of the SQL code unchanged. The part of a rendering that varies with the
//! value stays inside one closed group: the parentheses of a number or a bool, the string
//! constant of a byte string, or the quoted identifier of a name. The text that follows the
//! varying part is the same for every value.

use proc_macro2::Span;

use crate::Piece;

/// The characters that may directly precede a placeholder.
///
/// - `(` and `,` are punctuation tokens of their own: a call, a group, a list.
/// - `[` is a punctuation token of its own: an array subscript.
/// - `=`, `<`, `>`, `+`, `-`, `*`, `/`, `%`, and `|` end an operator: a comparison, an
///   assignment, arithmetic, or `||`. PostgreSQL reads an operator as the longest run of
///   operator characters, and a rendering starts with no operator character, so the operator
///   ends with the template's text. A rendering also never starts with `-` or `*`, so `-` and
///   `/` cannot start a comment with it.
pub(crate) const ALLOWED_BEFORE: [char; 12] =
	['(', ',', '[', '=', '<', '>', '+', '-', '*', '/', '%', '|'];

/// The characters that may directly follow a placeholder.
///
/// - `(`, `)`, `,`, and `;` are punctuation tokens of their own: the start or the end of a
///   call's arguments, a group, or a type modifier, a list, the end of a statement.
/// - `[` and `]` are punctuation tokens of their own: an array subscript, an array slice, or
///   the brackets of an array type.
/// - `:` starts a cast (`::`) or separates the bounds of an array slice.
/// - `=`, `<`, `>`, `+`, `-`, `*`, `/`, `%`, and `|` start an operator. A rendering ends with no
///   operator character, so the operator starts with the template's text.
///
/// Each of these characters forms the same tokens after a rendering as after a space, which
/// the rule allows anywhere.
pub(crate) const ALLOWED_AFTER: [char; 16] = [
	'(', ')', ',', ';', '[', ']', ':', '=', '<', '>', '+', '-', '*', '/', '%', '|',
];

/// The white space characters of PostgreSQL's lexer that every supported version reads as
/// white space. A non-ASCII space, such as a no-break space, is an identifier character for
/// PostgreSQL.
fn is_sql_white_space(character: char) -> bool {
	matches!(character, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

/// Whether PostgreSQL reads `character` as the start of an identifier or a keyword: an ASCII
/// letter, `_`, or any non-ASCII character.
fn is_identifier_start(character: char) -> bool {
	character.is_ascii_alphabetic() || character == '_' || !character.is_ascii()
}

/// Whether PostgreSQL reads `character` as part of an identifier or a keyword when it follows
/// one: an identifier start, an ASCII digit, or `$`.
fn is_identifier_character(character: char) -> bool {
	is_identifier_start(character) || character.is_ascii_digit() || character == '$'
}

/// What directly precedes or follows a placeholder.
enum Neighbor<'a> {
	TemplateBoundary,
	Character(char),
	Placeholder(&'a str),
}

impl Neighbor<'_> {
	fn is_allowed(&self, allowed: &[char]) -> bool {
		match self {
			Self::TemplateBoundary => true,
			Self::Character(character) => {
				is_sql_white_space(*character) || allowed.contains(character)
			}
			Self::Placeholder(_) => false,
		}
	}

	fn describe(&self) -> String {
		match self {
			Self::TemplateBoundary => "the template's boundary".to_owned(),
			Self::Character(character) => format!("{character:?}"),
			Self::Placeholder(name) => format!("the placeholder `{{{name}}}`"),
		}
	}
}

/// The token that the SQL code before a position ends with, as far as it decides what a `$` at
/// the position starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodeEnd {
	/// The start of the code, white space, punctuation, an operator, a closed string constant,
	/// quoted identifier, or dollar-quoted string, a comment, a lone `$`, or a rendering. A `$`
	/// starts a dollar quote or a parameter, or stands alone.
	Separator,
	/// An unquoted identifier or keyword. A `$` or a digit continues it.
	Identifier,
	/// A number or a parameter, of digits and periods. A `$` starts a dollar quote or a
	/// parameter, or stands alone.
	Number,
	/// A number that a letter or `_` directly follows, such as `1e5`, `0x1F`, or `1_000`.
	/// PostgreSQL 14 reads a `$` there as part of an identifier or as the start of a dollar
	/// quote, and later versions reject the number as trailing junk, so the lexer cannot know
	/// what a `$` there starts.
	NumberWithLetter,
}

impl CodeEnd {
	/// The token that the code ends with after `character`, which is neither the start of a
	/// quote or a comment nor a `$`.
	fn after(self, character: char) -> Self {
		match (self, character) {
			(Self::Identifier, character) if is_identifier_character(character) => Self::Identifier,
			(Self::Number, '0'..='9' | '.') => Self::Number,
			(Self::Number | Self::NumberWithLetter, character)
				if is_identifier_start(character) =>
			{
				Self::NumberWithLetter
			}
			(Self::NumberWithLetter, '0'..='9') => Self::NumberWithLetter,
			(_, '0'..='9') => Self::Number,
			(_, character) if is_identifier_start(character) => Self::Identifier,
			_ => Self::Separator,
		}
	}
}

/// The lexical context of the template's SQL at a position.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Context {
	/// SQL code.
	Code {
		end: CodeEnd,
	},
	/// A string constant. In one with the `E` prefix, a backslash escapes the next character.
	StringConstant {
		escapes_with_backslash: bool,
	},
	QuotedIdentifier,
	DollarQuoted {
		delimiter: String,
	},
	LineComment,
	BlockComment {
		depth: usize,
	},
}

impl Context {
	fn describe(&self) -> &'static str {
		match self {
			Self::Code { .. } => "SQL code",
			Self::StringConstant { .. } => "a string constant",
			Self::QuotedIdentifier => "a quoted identifier",
			Self::DollarQuoted { .. } => "a dollar-quoted string",
			Self::LineComment | Self::BlockComment { .. } => "a comment",
		}
	}
}

/// Checks that each placeholder stands in SQL code between two allowed neighbors, and that the
/// template ends in SQL code.
pub(crate) fn check_placeholder_placement(pieces: &[Piece], span: Span) -> syn::Result<()> {
	let mut context = Context::Code {
		end: CodeEnd::Separator,
	};
	for (index, piece) in pieces.iter().enumerate() {
		match piece {
			Piece::Text(text) => context = advance(context, text, span)?,
			Piece::Placeholder(name) => {
				if !matches!(context, Context::Code { .. }) {
					return Err(syn::Error::new(
						span,
						format!(
							"audited_sql! placeholder `{{{name}}}` is inside {}; a placeholder must stand in SQL code, outside string constants, quoted identifiers, dollar-quoted strings, and comments",
							context.describe()
						),
					));
				}
				let before = index
					.checked_sub(1)
					.map_or(Neighbor::TemplateBoundary, |previous| {
						match &pieces[previous] {
							Piece::Text(text) => text
								.chars()
								.next_back()
								.map_or(Neighbor::TemplateBoundary, Neighbor::Character),
							Piece::Placeholder(other) => Neighbor::Placeholder(other),
						}
					});
				let after = pieces
					.get(index + 1)
					.map_or(Neighbor::TemplateBoundary, |next| match next {
						Piece::Text(text) => text
							.chars()
							.next()
							.map_or(Neighbor::TemplateBoundary, Neighbor::Character),
						Piece::Placeholder(other) => Neighbor::Placeholder(other),
					});
				if !before.is_allowed(&ALLOWED_BEFORE) {
					return Err(syn::Error::new(
						span,
						format!(
							"audited_sql! placeholder `{{{name}}}` directly follows {}; put a space between them",
							before.describe()
						),
					));
				}
				if !after.is_allowed(&ALLOWED_AFTER) {
					return Err(syn::Error::new(
						span,
						format!(
							"audited_sql! placeholder `{{{name}}}` is directly followed by {}; put a space between them",
							after.describe()
						),
					));
				}
				// The rendering is a complete token sequence, so the code continues after it.
				context = Context::Code {
					end: CodeEnd::Separator,
				};
			}
		}
	}
	match context {
		Context::Code { .. } => Ok(()),
		open => Err(syn::Error::new(
			span,
			format!(
				"audited_sql! template ends inside {}; a template must end in SQL code, outside string constants, quoted identifiers, dollar-quoted strings, and comments, because text that follows the template would continue it",
				open.describe()
			),
		)),
	}
}

/// The context after `text`, which starts in `context`.
fn advance(mut context: Context, text: &str, span: Span) -> syn::Result<Context> {
	let characters: Vec<char> = text.chars().collect();
	let mut index = 0;
	while index < characters.len() {
		let character = characters[index];
		let next = characters.get(index + 1).copied();
		let mut consumed = 1;
		context = match context {
			Context::Code { end } => match (character, next) {
				('-', Some('-')) => {
					consumed = 2;
					Context::LineComment
				}
				('/', Some('*')) => {
					consumed = 2;
					Context::BlockComment { depth: 1 }
				}
				('\'', _) => {
					let prefix = string_prefix(&characters[..index]);
					if prefix == StringPrefix::Plain
						&& string_has_backslash(&characters[index + 1..])
					{
						return Err(syn::Error::new(
							span,
							"audited_sql! template has a backslash in a string constant without the E prefix, which PostgreSQL reads differently under each setting of standard_conforming_strings; write the constant with the E prefix",
						));
					}
					Context::StringConstant {
						escapes_with_backslash: prefix == StringPrefix::Escape,
					}
				}
				('"', _) => Context::QuotedIdentifier,
				('$', _) => match end {
					CodeEnd::Identifier => Context::Code {
						end: CodeEnd::Identifier,
					},
					CodeEnd::NumberWithLetter => {
						return Err(syn::Error::new(
							span,
							"audited_sql! template has a `$` directly after a number that a letter or `_` follows, which PostgreSQL versions read differently; put a space before the `$`",
						));
					}
					CodeEnd::Separator | CodeEnd::Number => {
						match dollar_quote_delimiter(&characters[index..]) {
							Some(delimiter) => {
								consumed = delimiter.chars().count();
								Context::DollarQuoted { delimiter }
							}
							// A parameter, such as `$1`, is a number as far as a following `$`
							// is concerned.
							None if next.is_some_and(|next| next.is_ascii_digit()) => {
								Context::Code {
									end: CodeEnd::Number,
								}
							}
							None => Context::Code {
								end: CodeEnd::Separator,
							},
						}
					}
				},
				_ => Context::Code {
					end: end.after(character),
				},
			},
			Context::StringConstant {
				escapes_with_backslash,
			} => match (character, next) {
				('\\', Some('\'')) if escapes_with_backslash => {
					return Err(syn::Error::new(
						span,
						"audited_sql! template has a backslash-escaped single quote in an E-prefixed string constant, which PostgreSQL rejects when backslash_quote is off; write two single quotes instead",
					));
				}
				('\\', Some(_)) if escapes_with_backslash => {
					consumed = 2;
					Context::StringConstant {
						escapes_with_backslash,
					}
				}
				('\'', Some('\'')) => {
					consumed = 2;
					Context::StringConstant {
						escapes_with_backslash,
					}
				}
				('\'', _) => Context::Code {
					end: CodeEnd::Separator,
				},
				_ => Context::StringConstant {
					escapes_with_backslash,
				},
			},
			Context::QuotedIdentifier => match (character, next) {
				('"', Some('"')) => {
					consumed = 2;
					Context::QuotedIdentifier
				}
				('"', _) => Context::Code {
					end: CodeEnd::Separator,
				},
				_ => Context::QuotedIdentifier,
			},
			Context::DollarQuoted { delimiter } => {
				if characters[index..]
					.iter()
					.copied()
					.take(delimiter.chars().count())
					.eq(delimiter.chars())
				{
					consumed = delimiter.chars().count();
					Context::Code {
						end: CodeEnd::Separator,
					}
				} else {
					Context::DollarQuoted { delimiter }
				}
			}
			Context::LineComment => match character {
				'\n' | '\r' => Context::Code {
					end: CodeEnd::Separator,
				},
				_ => Context::LineComment,
			},
			Context::BlockComment { depth } => match (character, next) {
				('/', Some('*')) => {
					consumed = 2;
					Context::BlockComment { depth: depth + 1 }
				}
				('*', Some('/')) if depth == 1 => {
					consumed = 2;
					Context::Code {
						end: CodeEnd::Separator,
					}
				}
				('*', Some('/')) => {
					consumed = 2;
					Context::BlockComment { depth: depth - 1 }
				}
				_ => Context::BlockComment { depth },
			},
		};
		index += consumed;
	}
	Ok(context)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StringPrefix {
	/// No prefix, or `B`, `X`, or `N`: the constant ends at its first single quote that no
	/// second single quote follows, unless `standard_conforming_strings` is off and a backslash
	/// precedes that quote.
	Plain,
	/// `E`: a backslash escapes the next character.
	Escape,
	/// `U&`: a backslash starts a Unicode escape and cannot escape a quote.
	Unicode,
}

/// Returns the prefix of a string constant with an opening quote that follows `code`.
fn string_prefix(code: &[char]) -> StringPrefix {
	let token_starts_at = |prefix_len: usize| {
		code.len()
			.checked_sub(prefix_len + 1)
			.is_none_or(|before| !is_identifier_character(code[before]))
	};
	match code {
		[.., 'E' | 'e'] if token_starts_at(1) => StringPrefix::Escape,
		[.., 'U' | 'u', '&'] if token_starts_at(2) => StringPrefix::Unicode,
		_ => StringPrefix::Plain,
	}
}

/// Returns whether the plain string constant starting at `content` contains a backslash before
/// its closing quote, or anywhere in the rest of the text when the constant does not close.
fn string_has_backslash(content: &[char]) -> bool {
	let mut index = 0;
	while index < content.len() {
		match (content[index], content.get(index + 1)) {
			('\'', Some('\'')) => index += 2,
			('\'', _) => return false,
			('\\', _) => return true,
			_ => index += 1,
		}
	}
	false
}

/// The dollar-quote delimiter (`$$` or `$tag$`) at the start of `characters`, if one starts
/// there. A tag starts with a letter, `_`, or a non-ASCII character and continues with those or
/// digits.
fn dollar_quote_delimiter(characters: &[char]) -> Option<String> {
	let tag_len = characters[1..]
		.iter()
		.enumerate()
		.take_while(|(position, character)| {
			is_identifier_start(**character) || (*position > 0 && character.is_ascii_digit())
		})
		.count();
	(characters.get(1 + tag_len) == Some(&'$')).then(|| characters[..tag_len + 2].iter().collect())
}
