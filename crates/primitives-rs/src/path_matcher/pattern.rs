use std::cmp::Ordering;
use std::collections::HashSet;
use std::fmt;

use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};

use super::language::{FlatPatternOptions, is_javascript_whitespace};

/// A checked pattern and its parsed segments.
///
/// A pattern is flat or nested, and only a flat pattern has a trailing-slash
/// marker. Only a pattern language's compilation constructs a pattern.
#[derive(Clone, Debug)]
pub struct Pattern(PatternKind);

/// The two kinds of pattern, each with its own data.
#[derive(Clone, Debug)]
enum PatternKind {
	Flat {
		pattern: String,
		segments: Vec<Segment>,
		has_trailing_slash: bool,
	},
	Nested {
		pattern: String,
		segments: Vec<Segment>,
	},
}

/// Serializes as the compiled-pattern format that the module documentation
/// describes: `kind`, `pattern`, `segments`, and, for a flat pattern only,
/// `hasTrailingSlash`.
impl Serialize for Pattern {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		match &self.0 {
			PatternKind::Flat {
				pattern,
				segments,
				has_trailing_slash,
			} => {
				let mut fields = serializer.serialize_struct("Pattern", 4)?;
				fields.serialize_field("kind", "flat")?;
				fields.serialize_field("pattern", pattern)?;
				fields.serialize_field("segments", segments)?;
				fields.serialize_field("hasTrailingSlash", has_trailing_slash)?;
				fields.end()
			}
			PatternKind::Nested { pattern, segments } => {
				let mut fields = serializer.serialize_struct("Pattern", 3)?;
				fields.serialize_field("kind", "nested")?;
				fields.serialize_field("pattern", pattern)?;
				fields.serialize_field("segments", segments)?;
				fields.end()
			}
		}
	}
}

/// One checked segment of a pattern.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(
	tag = "kind",
	rename_all = "camelCase",
	rename_all_fields = "camelCase"
)]
pub enum Segment {
	/// A segment that must match its literal text.
	Static {
		/// Literal segment text.
		value: String,
	},
	/// A segment that matches any one path segment and returns it as a param.
	Dynamic {
		/// Name under which the value is returned.
		param_name: String,
	},
	/// A final segment that matches every remaining path segment and returns each one as a splat value.
	Splat,
	/// A final segment, only in nested patterns, that makes the pattern the
	/// index pattern for the path that its preceding segments spell.
	Index,
}

/// A checked flat pattern, which [`FlatMatcher`](super::FlatMatcher) matches.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct FlatPattern(Pattern);

/// A checked nested pattern, which [`NestedMatcher`](super::NestedMatcher)
/// matches.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct NestedPattern(Pattern);

impl FlatPattern {
	pub(super) fn new(pattern: Pattern) -> Self {
		Self(pattern)
	}

	/// Returns the pattern.
	pub fn as_pattern(&self) -> &Pattern {
		&self.0
	}

	/// Returns the pattern, consuming its flat guarantee.
	pub fn into_pattern(self) -> Pattern {
		self.0
	}
}

impl NestedPattern {
	pub(super) fn new(pattern: Pattern) -> Self {
		Self(pattern)
	}

	/// Returns the pattern.
	pub fn as_pattern(&self) -> &Pattern {
		&self.0
	}

	/// Returns the pattern, consuming its nested guarantee.
	pub fn into_pattern(self) -> Pattern {
		self.0
	}
}

impl Pattern {
	/// Returns the text from which this pattern was compiled.
	pub fn as_str(&self) -> &str {
		match &self.0 {
			PatternKind::Flat { pattern, .. } | PatternKind::Nested { pattern, .. } => pattern,
		}
	}

	/// Returns the checked segments in path order.
	pub fn segments(&self) -> &[Segment] {
		match &self.0 {
			PatternKind::Flat { segments, .. } | PatternKind::Nested { segments, .. } => segments,
		}
	}

	/// Returns `true` when this is a flat pattern other than `/` that ends
	/// with a slash. A flat matcher selects such a pattern only for a path
	/// that ends with a slash. Returns `false` for every nested pattern.
	pub fn has_trailing_slash(&self) -> bool {
		match &self.0 {
			PatternKind::Flat {
				has_trailing_slash, ..
			} => *has_trailing_slash,
			PatternKind::Nested { .. } => false,
		}
	}

	pub(super) fn compile(
		pattern: &str,
		options: FlatPatternOptions,
		index: Option<&str>,
	) -> Result<Self, PatternError> {
		if pattern.is_empty() {
			return Err(PatternError::EmptyPattern);
		}
		if !pattern.starts_with('/') {
			return Err(PatternError::RelativePattern(pattern.to_owned()));
		}
		if pattern.contains("//") {
			return Err(PatternError::RepeatedSlash(pattern.to_owned()));
		}
		if pattern.contains(is_javascript_whitespace) {
			return Err(PatternError::Whitespace(pattern.to_owned()));
		}
		let trailing = pattern.len() > 1 && pattern.ends_with('/');
		if index.is_some() && trailing {
			return Err(PatternError::NestedTrailingSlash(pattern.to_owned()));
		}
		let body = pattern[1..].strip_suffix('/').unwrap_or(&pattern[1..]);
		let values: Vec<_> = if body.is_empty() {
			Vec::new()
		} else {
			body.split('/').collect()
		};
		let mut segments = Vec::with_capacity(values.len());
		let mut names = HashSet::new();
		for (position, value) in values.iter().copied().enumerate() {
			let terminal = position + 1 == values.len();
			if Some(value) == index {
				if !terminal {
					return Err(PatternError::NonterminalIndex(pattern.to_owned()));
				}
				segments.push(Segment::Index);
			} else if value.starts_with(options.splat_segment_identifier)
				&& value.chars().count() == 1
			{
				if !terminal || trailing {
					return Err(PatternError::NonterminalSplat(pattern.to_owned()));
				}
				segments.push(Segment::Splat);
			} else if let Some(name) = value.strip_prefix(options.dynamic_param_prefix) {
				let mut bytes = name.bytes();
				if !bytes
					.next()
					.is_some_and(|first| first == b'_' || first.is_ascii_alphabetic())
					|| !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
				{
					return Err(PatternError::InvalidParamName {
						pattern: pattern.to_owned(),
						name: name.to_owned(),
					});
				}
				if !names.insert(name) {
					return Err(PatternError::RepeatedParamName {
						pattern: pattern.to_owned(),
						name: name.to_owned(),
					});
				}
				segments.push(Segment::Dynamic {
					param_name: name.to_owned(),
				});
			} else {
				segments.push(Segment::Static {
					value: value.to_owned(),
				});
			}
		}
		let pattern = pattern.to_owned();
		Ok(Self(match index {
			Some(_) => PatternKind::Nested { pattern, segments },
			None => PatternKind::Flat {
				pattern,
				segments,
				has_trailing_slash: trailing,
			},
		}))
	}
}

/// Orders two patterns that both match one path, such as the winners of two
/// matchers, by specificity.
///
/// The comparison skips index segments and compares the remaining segment
/// kinds in order. At the first position where the kinds differ, a static
/// segment ranks before a dynamic segment, and a dynamic segment ranks
/// before a splat. When one pattern's segment kinds are a prefix of the
/// other pattern's segment kinds, the shorter pattern ranks first, so `/a`
/// ranks before both `/a/*` and `/a/:x`. The comparison ignores static text,
/// parameter names, and trailing slashes. `Less` means that the left pattern
/// ranks first, and `Equal` means that both patterns have the same segment
/// kinds in the same order.
pub fn compare_matching_patterns(patterns: PatternComparison<'_>) -> Ordering {
	fn ranks(pattern: &Pattern) -> impl Iterator<Item = u8> {
		pattern
			.segments()
			.iter()
			.filter_map(|segment| match segment {
				Segment::Static { .. } => Some(0),
				Segment::Dynamic { .. } => Some(1),
				Segment::Splat => Some(2),
				Segment::Index => None,
			})
	}
	ranks(patterns.left).cmp(ranks(patterns.right))
}

/// Reports whether some path matches both patterns, each when it is the only
/// pattern of a matcher of its own kind. The answer is sound for use as a
/// reachability check: it is `false` only when no path can match both
/// patterns, and it is `true` whenever one does.
///
/// The check follows each kind's matching rules. A flat pattern without a
/// splat matches the segments that it spells, a flat pattern with a trailing
/// slash matches only a path that ends with a slash, and a flat pattern with
/// a splat as its last segment matches zero segments at the root or after a
/// trailing slash. A nested pattern ignores a trailing slash in the path, its
/// index segment spells no path segment, and a splat matches zero segments
/// only at the root. A dynamic segment and a splat match any segment text, so
/// a caller that rejects some matched values after matching, such as `.` and
/// `..`, must make sure that those values are not the only ones that overlap
/// the two patterns. That holds when no static segment of either pattern can
/// spell such a value.
pub fn patterns_can_match_one_path(patterns: PatternComparison<'_>) -> bool {
	let left = path_forms(patterns.left);
	let right = path_forms(patterns.right);
	left.iter()
		.any(|left| right.iter().any(|right| left.overlaps(right)))
}

/// One shape of path that a pattern matches: paths with segments that match
/// `prefix` and have exactly the prefix's length or, for an open form,
/// more segments. A trailing slash adds no segment, and a path matches a
/// pattern without a trailing slash whether or not it ends with a slash, so
/// the slash decides no overlap.
struct PathForm<'a> {
	/// Static and dynamic segments, in order.
	prefix: &'a [Segment],
	/// Whether the form also matches paths with more segments than the
	/// prefix.
	open: bool,
}

impl PathForm<'_> {
	fn overlaps(&self, other: &PathForm<'_>) -> bool {
		let shared = self.prefix.len().min(other.prefix.len());
		let segments_agree = self.prefix[..shared]
			.iter()
			.zip(&other.prefix[..shared])
			.all(|pair| match pair {
				(Segment::Static { value: left }, Segment::Static { value: right }) => {
					left == right
				}
				_ => true,
			});
		let lengths_agree = match (self.open, other.open) {
			(true, true) => true,
			(true, false) => other.prefix.len() > self.prefix.len(),
			(false, true) => self.prefix.len() > other.prefix.len(),
			(false, false) => self.prefix.len() == other.prefix.len(),
		};
		segments_agree && lengths_agree
	}
}

fn path_forms(pattern: &Pattern) -> Vec<PathForm<'_>> {
	let (prefix, ends_with_splat) = match pattern.segments().split_last() {
		Some((Segment::Splat, prefix)) => (prefix, true),
		Some((Segment::Index, prefix)) => (prefix, false),
		_ => (pattern.segments(), false),
	};
	if !ends_with_splat {
		return vec![PathForm {
			prefix,
			open: false,
		}];
	}
	let mut forms = vec![PathForm { prefix, open: true }];
	// A splat also matches zero segments: at the root in both kinds, and in
	// a flat pattern after the trailing slash of a path that has segments.
	if prefix.is_empty() || matches!(pattern.0, PatternKind::Flat { .. }) {
		forms.push(PathForm {
			prefix,
			open: false,
		});
	}
	forms
}

/// A pattern cannot be compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternError {
	/// The pattern is empty.
	EmptyPattern,
	/// The pattern does not start with `/`.
	RelativePattern(String),
	/// The pattern contains an empty middle segment.
	RepeatedSlash(String),
	/// The pattern contains JavaScript whitespace.
	Whitespace(String),
	/// A nested pattern other than `/` ends with a slash.
	NestedTrailingSlash(String),
	/// A splat is followed by another segment or a slash.
	NonterminalSplat(String),
	/// An index marker is followed by another segment.
	NonterminalIndex(String),
	/// A parameter name is not an ASCII identifier.
	InvalidParamName {
		/// Rejected pattern.
		pattern: String,
		/// Rejected parameter name.
		name: String,
	},
	/// Two parameters have the same name.
	RepeatedParamName {
		/// Rejected pattern.
		pattern: String,
		/// Repeated parameter name.
		name: String,
	},
}

impl fmt::Display for PatternError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::EmptyPattern => formatter.write_str("the pattern is empty"),
			Self::RelativePattern(pattern) => {
				write!(formatter, "pattern {pattern:?} must begin with /")
			}
			Self::RepeatedSlash(pattern) => {
				write!(formatter, "pattern {pattern:?} contains an empty segment")
			}
			Self::Whitespace(pattern) => {
				write!(formatter, "pattern {pattern:?} contains whitespace")
			}
			Self::NestedTrailingSlash(pattern) => {
				write!(formatter, "nested pattern {pattern:?} cannot end with /")
			}
			Self::NonterminalSplat(pattern) => write!(
				formatter,
				"pattern {pattern:?} has a splat that is not its last segment"
			),
			Self::NonterminalIndex(pattern) => write!(
				formatter,
				"pattern {pattern:?} has an index segment that is not its last segment"
			),
			Self::InvalidParamName { pattern, name } => write!(
				formatter,
				"pattern {pattern:?} names a dynamic parameter {name:?}, which is not an identifier"
			),
			Self::RepeatedParamName { pattern, name } => write!(
				formatter,
				"pattern {pattern:?} names dynamic parameter {name:?} more than once"
			),
		}
	}
}

impl std::error::Error for PatternError {}

/// Two patterns supplied to a comparison of them.
pub struct PatternComparison<'a> {
	/// First comparison operand.
	pub left: &'a Pattern,
	/// Second comparison operand.
	pub right: &'a Pattern,
}
