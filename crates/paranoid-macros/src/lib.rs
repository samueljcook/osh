//! `paranoid` procedural macros.
//!
//! Application code should use `paranoid::audited_sql!`; this crate is an implementation
//! detail.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote, quote_spanned};
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{Expr, Ident, LitStr, Path, Token, parse_macro_input};

mod placement;

/// The expansion of `paranoid::audited_sql!`, which documents the template and its arguments.
///
/// The input is the path of the `paranoid` crate, a comma, and the input of
/// `paranoid::audited_sql!`. That macro passes `$crate` as the path, so the expansion reaches
/// the crate also when a consumer renames the dependency.
#[proc_macro]
pub fn audited_sql(input: TokenStream) -> TokenStream {
	let input = parse_macro_input!(input as AuditedSqlInput);
	match input.expand() {
		Ok(tokens) => tokens.into(),
		Err(error) => error.to_compile_error().into(),
	}
}

struct AuditedSqlInput {
	crate_path: Path,
	template: LitStr,
	arguments: Vec<Argument>,
}

struct Argument {
	name: Ident,
	value: Expr,
}

/// A part of a template: text, in which `{{` and `}}` are already single braces, or a
/// placeholder's name. The parser never produces an empty text.
pub(crate) enum Piece {
	Text(String),
	Placeholder(String),
}

impl Parse for AuditedSqlInput {
	fn parse(input: ParseStream) -> syn::Result<Self> {
		let crate_path = input.call(Path::parse_mod_style)?;
		input.parse::<Token![,]>()?;
		let template: LitStr = input.parse().map_err(|error| {
            syn::Error::new(
                error.span(),
                "audited_sql! requires a string literal as its first argument so that the SQL structure is fixed at compile time",
            )
        })?;
		let mut arguments = Vec::new();
		while !input.is_empty() {
			input.parse::<Token![,]>()?;
			if input.is_empty() {
				break;
			}
			let name: Ident = input.parse()?;
			input.parse::<Token![=]>()?;
			let value: Expr = input.parse()?;
			arguments.push(Argument { name, value });
		}
		Ok(Self {
			crate_path,
			template,
			arguments,
		})
	}
}

impl AuditedSqlInput {
	fn expand(&self) -> syn::Result<TokenStream2> {
		let pieces = parse_template(&self.template.value(), self.template.span())?;
		placement::check_placeholder_placement(&pieces, self.template.span())?;
		self.check_arguments(&pieces)?;

		let builder = Ident::new("audited_sql_builder", Span::mixed_site());
		let crate_path = &self.crate_path;
		// The tokens of each check carry the span of the argument's expression so that an
		// argument of a type that is not an argument type is reported there. The temporaries of
		// a `match` scrutinee live until the end of the `match`.
		let checked_arguments = self.arguments.iter().map(|argument| {
			let value = &argument.value;
			quote_spanned! {value.span()=>
				#crate_path::__private::audited_sql_argument(&(#value))
			}
		});
		let argument_bindings = self
			.arguments
			.iter()
			.map(|argument| argument_binding(&argument.name));
		let steps = pieces.iter().map(|piece| match piece {
			Piece::Text(text) => quote! { #builder.push_text(#text); },
			Piece::Placeholder(name) => {
				let argument = self
					.argument(name)
					.expect("check_arguments found an argument for each placeholder");
				let binding = argument_binding(&argument.name);
				quote! { #builder.push_argument(#binding); }
			}
		});

		Ok(quote! {
			match (#(#checked_arguments,)*) {
				(#(#argument_bindings,)*) => {
					let mut #builder = #crate_path::__private::AuditedSqlBuilder::new();
					#(#steps)*
					#builder.finish()
				}
			}
		})
	}

	/// The argument named `name`, compared without the `r#` of a raw identifier.
	fn argument(&self, name: &str) -> Option<&Argument> {
		self.arguments
			.iter()
			.find(|argument| argument.name.unraw() == name)
	}

	fn check_arguments(&self, pieces: &[Piece]) -> syn::Result<()> {
		let span = self.template.span();
		for (index, argument) in self.arguments.iter().enumerate() {
			let name = argument.name.unraw().to_string();
			if self.arguments[..index]
				.iter()
				.any(|earlier| earlier.name.unraw() == argument.name.unraw())
			{
				return Err(syn::Error::new(
					argument.name.span(),
					format!("audited_sql! argument `{name}` is given more than once"),
				));
			}
			let used = pieces.iter().any(
				|piece| matches!(piece, Piece::Placeholder(placeholder) if *placeholder == name),
			);
			if !used {
				return Err(syn::Error::new(
					argument.name.span(),
					format!("audited_sql! argument `{name}` is not used by the template"),
				));
			}
		}
		for piece in pieces {
			if let Piece::Placeholder(name) = piece
				&& self.argument(name).is_none()
			{
				return Err(syn::Error::new(
					span,
					format!("audited_sql! placeholder `{{{name}}}` has no argument named `{name}`"),
				));
			}
		}
		Ok(())
	}
}

/// The local binding of an argument, with mixed-site hygiene so that it cannot collide with a
/// name of the caller.
fn argument_binding(name: &Ident) -> Ident {
	format_ident!("audited_sql_argument_{}", name, span = Span::mixed_site())
}

fn parse_template(template: &str, span: Span) -> syn::Result<Vec<Piece>> {
	if template.is_empty() {
		return Err(syn::Error::new(
			span,
			"audited_sql! template is empty; write the SQL statement in the template",
		));
	}
	let mut pieces = Vec::new();
	let mut text = String::new();
	let mut characters = template.chars().peekable();
	while let Some(character) = characters.next() {
		match character {
			'{' if characters.peek() == Some(&'{') => {
				characters.next();
				text.push('{');
			}
			'}' if characters.peek() == Some(&'}') => {
				characters.next();
				text.push('}');
			}
			'}' => {
				return Err(syn::Error::new(
					span,
					"audited_sql! template has a `}` without a matching `{`; write `}}` for a literal brace",
				));
			}
			'{' => {
				let mut name = String::new();
				loop {
					match characters.next() {
						Some('}') => break,
						Some(next) => name.push(next),
						None => {
							return Err(syn::Error::new(
								span,
								"audited_sql! template has a `{` without a closing `}`; write `{{` for a literal brace",
							));
						}
					}
				}
				if !is_placeholder_name(&name) {
					return Err(syn::Error::new(
						span,
						format!(
							"audited_sql! placeholder `{{{name}}}` is not a name; placeholders are `{{name}}` with a name of ASCII letters, digits, and underscores that does not start with a digit"
						),
					));
				}
				if !text.is_empty() {
					pieces.push(Piece::Text(std::mem::take(&mut text)));
				}
				pieces.push(Piece::Placeholder(name));
			}
			other => text.push(other),
		}
	}
	if !text.is_empty() {
		pieces.push(Piece::Text(text));
	}
	Ok(pieces)
}

fn is_placeholder_name(name: &str) -> bool {
	let mut characters = name.chars();
	characters
		.next()
		.is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
		&& characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn expansion(input: &str) -> syn::Result<String> {
		syn::parse_str::<AuditedSqlInput>(&format!("::paranoid, {input}"))?
			.expand()
			.map(|tokens| tokens.to_string())
	}

	fn error_message(input: &str) -> String {
		expansion(input)
			.expect_err("the input does not expand")
			.to_string()
	}

	#[test]
	fn a_static_template_expands() {
		assert!(expansion(r#""SELECT 1""#).is_ok());
		assert!(expansion(r#""SELECT {{1}}""#).is_ok());
		assert!(expansion(r#""SELECT {a}, {a}", a = &x"#).is_ok());
		assert!(expansion(r#""SELECT {a}", a = &x,"#).is_ok());
		assert!(expansion(r#""SELECT ({a})+{b}, {{ {a} }}", a = &x, b = &y"#).is_ok());
	}

	#[test]
	fn the_expansion_reaches_the_builder_through_the_given_crate_path() {
		let tokens = syn::parse_str::<AuditedSqlInput>(r#"crate::renamed, "SELECT 1""#)
			.unwrap()
			.expand()
			.unwrap()
			.to_string();
		assert!(
			tokens.contains("crate :: renamed :: __private :: AuditedSqlBuilder"),
			"{tokens}"
		);
	}

	#[test]
	fn a_raw_identifier_argument_matches_the_placeholder_of_its_name() {
		assert!(expansion(r#""SELECT {type}", r#type = &x"#).is_ok());
		assert!(
			error_message(r#""SELECT {type}", r#type = &x, type_ = &y"#)
				.contains("argument `type_` is not used")
		);
		assert!(
			error_message(r#""SELECT {type}", r#type = &x, r#type = &y"#)
				.contains("argument `type` is given more than once")
		);
	}

	#[test]
	fn an_empty_template_is_rejected() {
		assert!(error_message(r#""""#).contains("template is empty"));
	}

	#[test]
	fn a_placeholder_needs_an_allowed_neighbor_on_each_side() {
		for input in [
			r#""{a}""#,
			r#""SELECT {a}""#,
			r#""SELECT\t{a}\n""#,
			r#""f({a},{a})""#,
			r#""x[{a}:1]""#,
			r#""x={a};""#,
			r#""x<{a}>y""#,
			r#""x+{a}-y""#,
			r#""x*{a}/y""#,
			r#""x%{a}%y""#,
			r#""x||{a}||y""#,
			r#""x-{a}::pg_catalog.text""#,
			r#""f({a}(1))""#,
			r#""SELECT {a}[1]""#,
		] {
			assert!(
				expansion(&format!("{input}, a = &x")).is_ok(),
				"{input}: {}",
				error_message(&format!("{input}, a = &x"))
			);
		}
		for (input, message) in [
			(
				r#""SELECT {a}{b}", a = &x, b = &y"#,
				"`{a}` is directly followed by the placeholder `{b}`",
			),
			(r#""SELECT flag_{a}", a = &x"#, "`{a}` directly follows '_'"),
			(
				r#""SELECT {a}_flag", a = &x"#,
				"`{a}` is directly followed by '_'",
			),
			(r#""SELECT €{a}", a = &x"#, "`{a}` directly follows '€'"),
			(
				"\"SELECT \u{a0}{a}\", a = &x",
				"`{a}` directly follows '\\u{a0}'",
			),
			(r#""SELECT s.{a}", a = &x"#, "`{a}` directly follows '.'"),
			(
				r#""SELECT {a}.t", a = &x"#,
				"`{a}` is directly followed by '.'",
			),
			(r#""SELECT f{a}", a = &x"#, "`{a}` directly follows 'f'"),
			(
				r#""SELECT {a}$1", a = &x"#,
				"`{a}` is directly followed by '$'",
			),
			(r#""SELECT ({a})", a = &x"#, ""),
			(r#""SELECT {{{a}}}", a = &x"#, "`{a}` directly follows '{'"),
		] {
			if message.is_empty() {
				assert!(expansion(input).is_ok(), "{input}");
			} else {
				assert!(
					error_message(input).contains(message),
					"{input}: {}",
					error_message(input)
				);
			}
		}
	}

	#[test]
	fn a_placeholder_inside_a_constant_an_identifier_or_a_comment_is_rejected() {
		for (input, context) in [
			(r#""SELECT ' {a} '", a = &x"#, "a string constant"),
			(r#""SELECT E' \\\\ '' {a} '", a = &x"#, "a string constant"),
			(r#""SELECT ' '' {a} '", a = &x"#, "a string constant"),
			(r#""SELECT \" {a} \"", a = &x"#, "a quoted identifier"),
			(r#""SELECT $$ {a} $$", a = &x"#, "a dollar-quoted string"),
			(
				r#""SELECT $body$ $$ {a} $body$", a = &x"#,
				"a dollar-quoted string",
			),
			("\"SELECT 1 -- {a}\n\", a = &x", "a comment"),
			(r#""SELECT 1 /* /* */ {a} */", a = &x"#, "a comment"),
		] {
			assert!(
				error_message(input).contains(&format!("`{{a}}` is inside {context}")),
				"{input}: {}",
				error_message(input)
			);
		}
		for input in [
			r#""SELECT ' ' || {a}", a = &x"#,
			r#""SELECT E'\\\\' || {a}", a = &x"#,
			r#""SELECT E'''' || {a}", a = &x"#,
			r#""SELECT \"x\" || {a}", a = &x"#,
			r#""SELECT $$ x $$ || {a}", a = &x"#,
			r#""SELECT a$b$c || {a}", a = &x"#,
			r#""SELECT $1 || {a}", a = &x"#,
			"\"SELECT 1 -- x\n|| {a}\", a = &x",
			r#""SELECT 1 /* x */ + {a}", a = &x"#,
		] {
			assert!(
				expansion(input).is_ok(),
				"{input}: {}",
				error_message(input)
			);
		}
	}

	#[test]
	fn a_template_that_ends_inside_a_constant_an_identifier_or_a_comment_is_rejected() {
		for (input, context) in [
			(r#""SELECT * FROM t WHERE name = '""#, "a string constant"),
			(r#""SELECT E'x\\\\""#, "a string constant"),
			(r#""SELECT {a}, 'x", a = &x"#, "a string constant"),
			(r#""SELECT \"x""#, "a quoted identifier"),
			(r#""SELECT $$x""#, "a dollar-quoted string"),
			(r#""SELECT $t$ x $$""#, "a dollar-quoted string"),
			(r#""SELECT 1 -- x""#, "a comment"),
			(r#""SELECT 1 /* /* */""#, "a comment"),
		] {
			assert!(
				error_message(input).contains(&format!("template ends inside {context}")),
				"{input}: {}",
				error_message(input)
			);
		}
		for input in [
			"\"SELECT 1 -- x\n\"",
			r#""SELECT 'x'""#,
			r#""SELECT \"x\"""#,
			r#""SELECT $t$ x $t$""#,
			r#""SELECT 1 /* /* */ */""#,
		] {
			assert!(
				expansion(input).is_ok(),
				"{input}: {}",
				error_message(input)
			);
		}
	}

	#[test]
	fn a_dollar_quote_after_a_closing_delimiter_a_number_or_a_parameter_is_found() {
		for input in [
			r#""SELECT $$a$$$$ {a} $$", a = &x"#,
			r#""SELECT 1$$ {a} $$", a = &x"#,
			r#""SELECT $1$t$ {a} $t$", a = &x"#,
			r#""SELECT 1.5$$ {a} $$", a = &x"#,
		] {
			assert!(
				error_message(input).contains("`{a}` is inside a dollar-quoted string"),
				"{input}: {}",
				error_message(input)
			);
		}
		for input in [r#""SELECT 1e5$$ x $$""#, r#""SELECT 0x1F$ x""#] {
			assert!(
				error_message(input).contains("`$` directly after a number"),
				"{input}: {}",
				error_message(input)
			);
		}
		for input in [
			r#""SELECT a1$$ || {a}", a = &x"#,
			r#""SELECT 1e+5 || $$ x $$ || {a}", a = &x"#,
		] {
			assert!(
				expansion(input).is_ok(),
				"{input}: {}",
				error_message(input)
			);
		}
	}

	#[test]
	fn a_backslash_in_a_string_constant_without_the_escape_prefix_is_rejected() {
		assert!(error_message(r#""SELECT 'a\\b'""#).contains("backslash in a string constant"));
		assert!(expansion(r#""SELECT E'a\\b'""#).is_ok());
		assert!(expansion(r#""SELECT U&'d\\0061t'""#).is_ok());
	}

	#[test]
	fn a_backslash_escaped_single_quote_in_an_escape_string_is_rejected() {
		for input in [
			r#""SELECT E'\\''""#,
			r#""SELECT e'\\''""#,
			r#""SELECT E' \\' {a} '", a = &x"#,
		] {
			assert!(
				error_message(input).contains("backslash_quote"),
				"{input}: {}",
				error_message(input)
			);
		}
		for input in [r#""SELECT E''''""#, r#""SELECT E'\\\\'""#] {
			assert!(
				expansion(input).is_ok(),
				"{input}: {}",
				error_message(input)
			);
		}
	}

	#[test]
	fn a_template_that_is_not_a_string_literal_is_rejected() {
		for input in [
			r#"format!("SELECT 1")"#,
			"query",
			"1",
			r#"concat!("SELECT ", "1")"#,
		] {
			assert!(
				error_message(input).contains("string literal"),
				"{input}: {}",
				error_message(input)
			);
		}
	}

	#[test]
	fn malformed_and_positional_placeholders_are_rejected() {
		assert!(error_message(r#""SELECT {a""#).contains("without a closing"));
		assert!(error_message(r#""SELECT a}""#).contains("without a matching"));
		for input in [
			r#""SELECT {}""#,
			r#""SELECT {0}""#,
			r#""SELECT {1a}""#,
			r#""SELECT {a b}""#,
		] {
			assert!(
				error_message(input).contains("is not a name"),
				"{input}: {}",
				error_message(input)
			);
		}
	}

	#[test]
	fn placeholders_and_arguments_must_correspond() {
		assert!(error_message(r#""SELECT {a}""#).contains("has no argument named `a`"));
		assert!(error_message(r#""SELECT 1", a = &x"#).contains("is not used by the template"));
		assert!(
			error_message(r#""SELECT {a}", a = &x, a = &y"#).contains("is given more than once")
		);
	}
}
