//! Correctness macros for typed Vorma declarations.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use primitives_rs::path_matcher::{
	FlatPatternLanguage, FlatPatternOptions, NestedPatternLanguage, NestedPatternOptions, Pattern,
	Segment,
};
use proc_macro2::Span;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Attribute, Ident, LitStr, Token, Visibility, parse_macro_input};
use vorma_path_wire::{DYNAMIC_PARAM_PREFIX, INDEX_SEGMENT, SPLAT_SEGMENT};

struct PatternDeclaration {
	attrs: Vec<Attribute>,
	visibility: Visibility,
	name: Ident,
	pattern: LitStr,
}

impl Parse for PatternDeclaration {
	fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
		let attrs = input.call(Attribute::parse_outer)?;
		let visibility = input.parse()?;
		input.parse::<Token![struct]>()?;
		let name = input.parse()?;
		input.parse::<Token![=]>()?;
		let pattern = input.parse()?;
		if input.peek(Token![;]) {
			input.parse::<Token![;]>()?;
		}
		Ok(Self {
			attrs,
			visibility,
			name,
			pattern,
		})
	}
}

/// Declares a view pattern and the struct of its params.
///
/// `vorma::view_pattern!(pub struct UserParams = "/users/:user_id");` checks the
/// pattern and generates `pub struct UserParams { pub user_id: String }` and
/// `UserParams::PATTERN`. For a pattern that ends in a splat, the struct also
/// has the field `splat_values: Vec<String>`. The struct derives `Clone`,
/// `Debug`, `PartialEq`, `Eq`, and `Hash`, and attributes written before
/// `struct`, such as doc comments and further derives, apply to it.
/// `vorma::PageTarget::new` accepts the struct.
///
/// Vorma decodes each matched segment before it reaches the struct, so a
/// param or a splat value can contain `/` (from `%2F`) and other characters
/// that a path segment would not spell. An application must not use a param or a
/// splat value as a file path without checking it.
#[proc_macro]
pub fn view_pattern(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
	let declaration = parse_macro_input!(input as PatternDeclaration);
	expand(declaration, PatternKind::View)
		.unwrap_or_else(syn::Error::into_compile_error)
		.into()
}

/// Declares an API pattern and the struct of its params.
///
/// `vorma::api_pattern!(pub struct UserApiParams = "/api/users/:user_id");`
/// checks the pattern and generates
/// `pub struct UserApiParams { pub user_id: String }` and
/// `UserApiParams::PATTERN`. For a pattern that ends in a splat, the
/// struct also has the field `splat_values: Vec<String>`. The struct derives
/// `Clone`, `Debug`, `PartialEq`, `Eq`, and `Hash`, and attributes written
/// before `struct`, such as doc comments and further derives, apply to it. One
/// struct serves every endpoint with the pattern, whatever its method.
/// `vorma::ApiEndpointTarget::new` accepts the struct.
///
/// Vorma decodes each matched segment before it reaches the struct, so a
/// param or a splat value can contain `/` (from `%2F`) and other characters
/// that a path segment would not spell. An application must not use a param or a
/// splat value as a file path without checking it.
#[proc_macro]
pub fn api_pattern(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
	let declaration = parse_macro_input!(input as PatternDeclaration);
	expand(declaration, PatternKind::ApiEndpoint)
		.unwrap_or_else(syn::Error::into_compile_error)
		.into()
}

#[derive(Clone, Copy)]
enum PatternKind {
	View,
	ApiEndpoint,
}

fn compile(pattern: &str, kind: PatternKind) -> Result<Pattern, String> {
	let wire = vorma_path_wire::canonicalize_pattern(pattern).map_err(|error| error.to_string())?;
	let compiled = match kind {
		PatternKind::View => NestedPatternLanguage::new(NestedPatternOptions {
			dynamic_param_prefix: DYNAMIC_PARAM_PREFIX,
			splat_segment_identifier: SPLAT_SEGMENT,
			explicit_index_segment_identifier: INDEX_SEGMENT.to_owned(),
		})
		.map_err(|error| error.to_string())?
		.compile_pattern(&wire)
		.map(|pattern| pattern.into_pattern()),
		PatternKind::ApiEndpoint => FlatPatternLanguage::new(FlatPatternOptions {
			dynamic_param_prefix: DYNAMIC_PARAM_PREFIX,
			splat_segment_identifier: SPLAT_SEGMENT,
		})
		.map_err(|error| error.to_string())?
		.compile_pattern(&wire)
		.map(|pattern| pattern.into_pattern()),
	};
	compiled.map_err(|error| error.to_string())
}

/// A raw identifier for a parameter name so that a name such as `type` or
/// `gen` remains a field. Names that cannot be raw identifiers are refused,
/// and so is `splat_values`, which names the splat field.
fn to_field_ident(name: &str, span: Span) -> syn::Result<Ident> {
	if name == "splat_values" {
		return Err(syn::Error::new(
			span,
			"the parameter name `splat_values` is reserved for the splat field; rename the parameter",
		));
	}
	let refused = matches!(name, "self" | "Self" | "super" | "crate" | "_");
	if refused || syn::parse_str::<Ident>(&format!("r#{name}")).is_err() {
		return Err(syn::Error::new(
			span,
			format!("the parameter name {name:?} cannot be a Rust field name"),
		));
	}
	Ok(Ident::new_raw(name, span))
}

fn expand(
	declaration: PatternDeclaration,
	kind: PatternKind,
) -> syn::Result<proc_macro2::TokenStream> {
	let PatternDeclaration {
		attrs,
		visibility,
		name,
		pattern,
	} = declaration;
	let span = pattern.span();
	let compiled =
		compile(&pattern.value(), kind).map_err(|message| syn::Error::new(span, message))?;
	let mut param_names = Vec::new();
	let mut param_fields = Vec::new();
	let mut has_splat = false;
	for segment in compiled.segments() {
		match segment {
			Segment::Dynamic { param_name } => {
				param_fields.push(to_field_ident(param_name, span)?);
				param_names.push(param_name.clone());
			}
			Segment::Splat => has_splat = true,
			Segment::Static { .. } | Segment::Index => {}
		}
	}
	let (params_trait, pattern_description) = match kind {
		PatternKind::View => (quote!(::vorma::ViewPatternParams), "view pattern"),
		PatternKind::ApiEndpoint => (quote!(::vorma::ApiPatternParams), "API pattern"),
	};
	let splat_field = has_splat.then(|| {
		quote! {
			#[doc = "The path segments that the splat matched, decoded. A value can contain `/`, so it is not safe to use as a file path without checking."]
			pub splat_values: ::std::vec::Vec<::std::string::String>,
		}
	});
	let take_splat = has_splat.then(|| quote!(splat_values: decoded_match.take_splat_values(),));
	let give_splat = if has_splat {
		quote!(self.splat_values)
	} else {
		quote!(::std::vec::Vec::new())
	};
	let into_decoded_match = quote! {
		fn into_decoded_match(self) -> ::vorma::__private::DecodedMatch {
			::vorma::__private::DecodedMatch::new(
				::std::vec![#((::std::string::String::from(#param_names), self.#param_fields),)*],
				#give_splat,
			)
		}
	};
	Ok(quote! {
		#[derive(
			::core::clone::Clone,
			::core::fmt::Debug,
			::core::cmp::PartialEq,
			::core::cmp::Eq,
			::core::hash::Hash,
		)]
		#[doc = ::core::concat!("The params of the ", #pattern_description, " `", #pattern, "`.")]
		#(#attrs)*
		#visibility struct #name {
			#(
				#[doc = ::core::concat!("The decoded `", #param_names, "` parameter. It can contain `/`, so it is not safe to use as a file path without checking.")]
				pub #param_fields: ::std::string::String,
			)*
			#splat_field
		}

		impl #name {
			#[doc = ::core::concat!("The pattern `", #pattern, "`.")]
			pub const PATTERN: &'static str = #pattern;
		}

		impl #params_trait for #name {
			const PATTERN: &'static str = #pattern;

			fn from_decoded_match(
				mut decoded_match: ::vorma::__private::DecodedMatch,
			) -> ::std::result::Result<Self, ::vorma::MissingParamError> {
				let _ = &mut decoded_match;
				::std::result::Result::Ok(Self {
					#(#param_fields: decoded_match.take_param(#param_names)?,)*
					#take_splat
				})
			}

			#into_decoded_match
		}
	})
}

/// Derive the TypeScript generation trait for a serializable Rust type.
///
/// This is the `primitives-rs` `TsGen` derive with generated code that reaches
/// the runtime through `::vorma::tsgen`, so an application that depends only on
/// `vorma` can use it. `vorma` re-exports it as `vorma::TsGen`, together with
/// the trait. The derive reads the `serde` attributes of the type and supports
/// the same shapes and attributes as `primitives_rs::tsgen::TsGen`, which
/// documents them.
#[proc_macro_derive(TsGen, attributes(serde, tsgen))]
pub fn derive_ts_gen(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
	let input = parse_macro_input!(input as syn::DeriveInput);
	primitives_rs_macros_core::expand_ts_gen(input, &quote! { ::vorma::tsgen })
		.unwrap_or_else(syn::Error::into_compile_error)
		.into()
}
