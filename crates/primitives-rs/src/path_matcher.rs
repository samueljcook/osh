//! Checked pattern languages and immutable flat and nested matchers.
//!
//! Construct a language with explicit options and compile patterns with it:
//! [`FlatPatternLanguage::compile_pattern`] returns a [`FlatPattern`], and
//! [`NestedPatternLanguage::compile_pattern`] returns a [`NestedPattern`].
//! [`FlatMatcher::new`] and [`NestedMatcher::new`] construct an immutable
//! matcher from compiled patterns and their values, so a matcher's patterns
//! are always of its own kind. A language checks its options when it is
//! constructed and checks each pattern when it compiles the pattern. A
//! matcher's constructor returns [`DuplicatePositionError`], with every
//! pattern that matches the same paths as an earlier pattern, when two
//! patterns match the same paths.
//!
//! Before matching, a matcher trims leading and trailing JavaScript
//! whitespace from the path, treats a path without a leading slash as if it
//! had one, and collapses repeated slashes. A nested matcher ignores a
//! trailing slash. A flat matcher treats `/a/` and `/a` as different
//! patterns: a path with a trailing slash can match either pattern and
//! prefers `/a/`, and a path without a trailing slash matches only `/a`.
//!
//! A matcher compares the path's segments with static segments as raw text,
//! case-sensitively, and returns parameter and splat values as raw text. It
//! does not percent-decode segments, resolve `.` or `..` segments, or remove
//! a query or a fragment, so the caller passes only the path and decodes the
//! values that it needs.
//!
//! At each path segment, matching prefers a static segment, then a dynamic
//! segment, then a splat, and backtracks when a preferred branch cannot
//! match the rest of the path. After the matcher matches the path's last
//! segment, or immediately for a path with no segments, it selects a pattern
//! that ends at that position. A flat matcher selects the first registered
//! pattern in this list:
//!
//! 1. The pattern with a trailing slash, when the path has a trailing slash.
//! 2. The pattern with no trailing slash.
//! 3. The pattern that ends with a splat, when the position is the root or
//!    the path has a trailing slash. The splat matches no segments.
//!
//! A nested matcher selects the first registered pattern in this list:
//!
//! 1. The pattern that ends with an index segment.
//! 2. The pattern that ends with a splat, when the position is the root. The
//!    splat matches no segments.
//! 3. The pattern with no index segment or splat.
//!
//! When no listed pattern is registered at that position, the matcher
//! backtracks. A nested match also contains the selected pattern's
//! ancestors, outermost first. They are the registered patterns with no
//! index segment or splat that end at the positions that the matcher passed
//! through to reach the selected pattern, including the selected pattern's
//! own position when the selected pattern ends with an index segment or
//! splat.
//!
//! [`Pattern`] serializes as a JSON object with a `kind` field (`"flat"` or
//! `"nested"`), a `pattern` field that contains the compiled text, a
//! `segments` array, and, for a flat pattern, a `hasTrailingSlash` field.
//! Each segment is `{"kind": "static", "value": ...}`,
//! `{"kind": "dynamic", "paramName": ...}`, `{"kind": "splat"}`, or
//! `{"kind": "index"}`. This is the compiled-pattern format that a
//! matcher of another language can read from the same JSON, such as a
//! TypeScript matcher.

mod language;
mod pattern;
mod tree;

pub use language::{
	FlatPatternLanguage, FlatPatternOptions, NestedPatternLanguage, NestedPatternOptions,
	PatternLanguageError,
};
pub use pattern::{
	FlatPattern, NestedPattern, Pattern, PatternComparison, PatternError, Segment,
	compare_matching_patterns, patterns_can_match_one_path,
};
pub use tree::{
	DuplicatePosition, DuplicatePositionError, FlatMatch, FlatMatcher, NestedMatch, NestedMatcher,
};
