use std::fmt;

use super::pattern::{FlatPattern, NestedPattern, Pattern, PatternError};

/// Syntax for flat patterns.
#[derive(Clone, Copy, Debug)]
pub struct FlatPatternOptions {
	/// Prefix of a named dynamic segment.
	pub dynamic_param_prefix: char,
	/// Character that spells a complete splat segment.
	pub splat_segment_identifier: char,
}

/// Syntax for nested patterns.
#[derive(Clone, Debug)]
pub struct NestedPatternOptions {
	/// Prefix of a named dynamic segment.
	pub dynamic_param_prefix: char,
	/// Character that spells a complete splat segment.
	pub splat_segment_identifier: char,
	/// Complete final segment that makes a nested pattern the index pattern
	/// for the path that its preceding segments spell.
	pub explicit_index_segment_identifier: String,
}

/// Checked flat-pattern syntax.
#[derive(Clone, Copy, Debug)]
pub struct FlatPatternLanguage {
	pub(super) options: FlatPatternOptions,
}

impl FlatPatternLanguage {
	/// Checks the markers and constructs the language.
	pub fn new(options: FlatPatternOptions) -> Result<Self, PatternLanguageError> {
		check_markers(options)?;
		Ok(Self { options })
	}

	/// Parses and checks one flat pattern.
	pub fn compile_pattern(&self, pattern: &str) -> Result<FlatPattern, PatternError> {
		Pattern::compile(pattern, self.options, None).map(FlatPattern::new)
	}
}

/// Checked nested-pattern syntax.
#[derive(Clone, Debug)]
pub struct NestedPatternLanguage {
	pub(super) options: NestedPatternOptions,
}

impl NestedPatternLanguage {
	/// Checks the markers and index identifier and constructs the language.
	pub fn new(options: NestedPatternOptions) -> Result<Self, PatternLanguageError> {
		check_markers(FlatPatternOptions {
			dynamic_param_prefix: options.dynamic_param_prefix,
			splat_segment_identifier: options.splat_segment_identifier,
		})?;
		let index = &options.explicit_index_segment_identifier;
		if index.is_empty()
			|| index.contains('/')
			|| index.contains(is_javascript_whitespace)
			|| index.starts_with(options.dynamic_param_prefix)
			|| index == &options.splat_segment_identifier.to_string()
		{
			return Err(PatternLanguageError::InvalidExplicitIndexSegmentIdentifier(
				index.clone(),
			));
		}
		Ok(Self { options })
	}

	/// Parses and checks one nested pattern.
	pub fn compile_pattern(&self, pattern: &str) -> Result<NestedPattern, PatternError> {
		Pattern::compile(
			pattern,
			FlatPatternOptions {
				dynamic_param_prefix: self.options.dynamic_param_prefix,
				splat_segment_identifier: self.options.splat_segment_identifier,
			},
			Some(&self.options.explicit_index_segment_identifier),
		)
		.map(NestedPattern::new)
	}
}

/// A marker or index identifier does not describe valid pattern syntax.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternLanguageError {
	/// The dynamic parameter prefix is a slash or JavaScript whitespace.
	InvalidDynamicParamPrefix(char),
	/// The splat segment identifier is a slash or JavaScript whitespace.
	InvalidSplatSegmentIdentifier(char),
	/// The explicit index segment identifier is empty, contains a slash or
	/// JavaScript whitespace, or would be parsed as a dynamic segment or splat.
	InvalidExplicitIndexSegmentIdentifier(String),
}

impl fmt::Display for PatternLanguageError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidDynamicParamPrefix(marker) => {
				write!(formatter, "invalid dynamic parameter prefix {marker:?}")
			}
			Self::InvalidSplatSegmentIdentifier(marker) => {
				write!(formatter, "invalid splat segment identifier {marker:?}")
			}
			Self::InvalidExplicitIndexSegmentIdentifier(index) => {
				write!(
					formatter,
					"invalid explicit index segment identifier {index:?}"
				)
			}
		}
	}
}

impl std::error::Error for PatternLanguageError {}

fn check_markers(options: FlatPatternOptions) -> Result<(), PatternLanguageError> {
	if options.dynamic_param_prefix == '/' || is_javascript_whitespace(options.dynamic_param_prefix)
	{
		return Err(PatternLanguageError::InvalidDynamicParamPrefix(
			options.dynamic_param_prefix,
		));
	}
	if options.splat_segment_identifier == '/'
		|| is_javascript_whitespace(options.splat_segment_identifier)
	{
		return Err(PatternLanguageError::InvalidSplatSegmentIdentifier(
			options.splat_segment_identifier,
		));
	}
	Ok(())
}

pub(super) fn is_javascript_whitespace(character: char) -> bool {
	matches!(character, '\u{0009}'..='\u{000D}' | '\u{0020}' | '\u{00A0}' | '\u{1680}'
		| '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}'
		| '\u{205F}' | '\u{3000}' | '\u{FEFF}')
}
