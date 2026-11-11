//! Byte range requests (RFC 9110 section 14): what the `Range` and `If-Range`
//! fields of a `GET` request select from one representation.
//!
//! Enable the `range-requests` Cargo feature. [`select_byte_ranges`] reads the
//! request and reports what it selects. The server chooses its answer from the
//! [`RangeSelection`]: which status it sends, whether it sends several ranges
//! as `multipart/byteranges`, whether it ignores or refuses a field that it
//! cannot use, and its cache policy.
//!
//! A range request can also be conditional. RFC 9110 section 13.2.2 orders
//! the evaluation: `If-Match`, `If-Unmodified-Since`, `If-None-Match`, and
//! `If-Modified-Since` come before `If-Range`. A server that answers
//! `If-None-Match` with `conditional_responses::if_none_match_matches` passes
//! the same current `ETag` value to [`select_byte_ranges`] when it does not
//! answer the request with `304`.
//!
//! ```
//! use std::num::NonZeroUsize;
//! use http::{HeaderMap, HeaderValue, header::RANGE};
//! use primitives_rs::http::range_requests::{
//!     RangeLimits, RangeRepresentation, RangeSelection, select_byte_ranges,
//! };
//!
//! let mut headers = HeaderMap::new();
//! headers.insert(RANGE, HeaderValue::from_static("bytes=2-5"));
//! let representation = RangeRepresentation { length: 10, etag: None };
//! let limits = RangeLimits { max_range_count: NonZeroUsize::MIN };
//! let RangeSelection::Ranges(ranges) = select_byte_ranges(&headers, representation, limits) else {
//!     panic!("the range overlaps the representation");
//! };
//! assert_eq!(ranges[0].start(), 2);
//! assert_eq!(ranges[0].content_range(), "bytes 2-5/10");
//! ```

use std::num::{NonZeroU64, NonZeroUsize};

use http::header::{IF_RANGE, RANGE};
use http::{HeaderMap, HeaderValue};

use crate::http::entity_tag_syntax::{EntityTag, parse_entity_tag, trim_ows};

/// The representation that a range request reads.
#[derive(Clone, Copy, Debug)]
pub struct RangeRepresentation<'a> {
	/// The representation's length in bytes.
	pub length: u64,
	/// The representation's current entity tag, as its `ETag` field carries
	/// it, or `None` when the representation has no entity tag. An `If-Range`
	/// field can match only a strong entity tag.
	pub etag: Option<&'a HeaderValue>,
}

/// A satisfiable byte range of a representation: one or more bytes from
/// [`ByteRange::start`] that end at or before the representation's end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
	start: u64,
	length: NonZeroU64,
	representation_length: u64,
}

impl ByteRange {
	/// The offset of the range's first byte.
	pub fn start(&self) -> u64 {
		self.start
	}

	/// The number of bytes in the range.
	pub fn length(&self) -> NonZeroU64 {
		self.length
	}

	/// The `Content-Range` value of a `206` answer that carries this range:
	/// `bytes <first>-<last>/<representation length>`, with inclusive offsets.
	pub fn content_range(&self) -> HeaderValue {
		let last = self.start + (self.length.get() - 1);
		to_header_value(format!(
			"bytes {}-{last}/{}",
			self.start, self.representation_length
		))
	}
}

/// The `Content-Range` value of a `416` answer for a representation of
/// `representation_length` bytes: `bytes */<representation length>`.
pub fn unsatisfied_content_range(representation_length: u64) -> HeaderValue {
	to_header_value(format!("bytes */{representation_length}"))
}

fn to_header_value(text: String) -> HeaderValue {
	HeaderValue::try_from(text).expect("a Content-Range of digits and ASCII is a header value")
}

/// What the `Range` and `If-Range` fields of a request select from a
/// representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RangeSelection {
	/// The request has no `Range` field, or it has an `If-Range` field that
	/// does not match the representation's current entity tag. RFC 9110
	/// requires the whole representation in both cases.
	Whole,
	/// The ranges that overlap the representation, at least one and at most
	/// [`RangeLimits::max_range_count`], each cut at the representation's end.
	/// The ranges keep the order of the field: they are neither merged nor
	/// sorted, and they can overlap or repeat. A server that sends each range
	/// can therefore be made to send the representation many times in one
	/// response, as `bytes=0-,0-,0-` selects the whole representation three
	/// times; RFC 9110 section 14.2 names many small or overlapping ranges as
	/// signs of a denial-of-service attack. A server can send one range with
	/// `206`, several ranges as `multipart/byteranges` with `206`, or the
	/// whole representation with `200`.
	Ranges(Vec<ByteRange>),
	/// The `Range` field is a valid list of byte ranges, and more of them
	/// overlap the representation than [`RangeLimits::max_range_count`]. A
	/// server can send the whole representation with `200`, which RFC 9110
	/// section 14.2 permits, or refuse the request.
	TooManyRanges,
	/// The `Range` field is a valid list of byte ranges, and none of them
	/// overlaps the representation. Every valid list of byte ranges selects
	/// this case for a zero-length representation. A server can answer `416`
	/// with [`unsatisfied_content_range`] or ignore the field.
	NotSatisfiable,
	/// The `Range` field names a range unit other than `bytes`. RFC 9110
	/// section 14.2 requires an origin server to ignore the field.
	UnsupportedUnit,
	/// The `Range` field is not a valid ranges specifier, or the request has
	/// several `Range` field lines, which RFC 9110 does not allow because the
	/// field is not a list. RFC 9110 section 14.2 permits a server to ignore an
	/// invalid field or to refuse the request.
	Invalid,
}

/// The caller's limits on what a request can select.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeLimits {
	/// The most ranges that overlap the representation that a selection
	/// returns. A valid field that selects more is
	/// [`RangeSelection::TooManyRanges`]. A server that sends at most one
	/// range in an answer sets this limit to one.
	pub max_range_count: NonZeroUsize,
}

/// Reads a request's `Range` and `If-Range` fields against a representation.
///
/// When the request has an `If-Range` field, the ranges apply only when the
/// field holds one strong entity tag that equals the representation's strong
/// current entity tag, byte for byte. An `If-Range` field with a weak entity
/// tag or with several field lines does not match. An `If-Range` field with a
/// date does not match either, because this function compares no dates. A
/// request with an `If-Range` field that does not match selects the whole
/// representation, whatever its `Range` field holds, as RFC 9110 section
/// 13.1.5 requires.
///
/// The `Range` field must have one field line; several lines are
/// [`RangeSelection::Invalid`]. The field's unit is `bytes`, compared without
/// regard to ASCII case and followed directly by `=`. The ranges follow as a
/// comma-separated list in which empty items are ignored. A range is
/// `first-last` or `first-` with decimal offsets, or `-suffix` for the last
/// `suffix` bytes; spaces and tabs around a range and around its `-` are
/// ignored. A range with a last offset before its first offset makes the
/// whole field invalid. A range that starts at or after the representation's
/// end, or a suffix of zero bytes, does not overlap the representation, and a
/// range or a suffix that reaches past the end is cut at the end. An offset or
/// a suffix that does not fit in a `u64` makes the field invalid. The whole
/// field is checked before the count of overlapping ranges is compared with
/// `limits`, so an invalid field is [`RangeSelection::Invalid`] however many
/// ranges it selects. The function keeps at most
/// [`RangeLimits::max_range_count`] ranges in memory, and its time is in
/// proportion to the length of the field.
pub fn select_byte_ranges(
	request_headers: &HeaderMap,
	representation: RangeRepresentation<'_>,
	limits: RangeLimits,
) -> RangeSelection {
	let mut range_lines = request_headers.get_all(RANGE).iter();
	let Some(range) = range_lines.next() else {
		return RangeSelection::Whole;
	};
	if request_headers.contains_key(IF_RANGE)
		&& !if_range_matches(request_headers, representation.etag)
	{
		return RangeSelection::Whole;
	}
	if range_lines.next().is_some() {
		return RangeSelection::Invalid;
	}
	let Ok(range) = range.to_str() else {
		return RangeSelection::Invalid;
	};
	let Some((unit, range_set)) = range.split_once('=') else {
		return RangeSelection::Invalid;
	};
	if !unit.eq_ignore_ascii_case("bytes") {
		return if !unit.is_empty() && unit.bytes().all(is_token_byte) {
			RangeSelection::UnsupportedUnit
		} else {
			RangeSelection::Invalid
		};
	}
	parse_byte_range_set(range_set, representation.length, limits)
}

/// Whether the request's `If-Range` field holds one strong entity tag equal to
/// the current strong entity tag.
fn if_range_matches(request_headers: &HeaderMap, current_etag: Option<&HeaderValue>) -> bool {
	let Some(current_etag) = current_etag else {
		return false;
	};
	let mut lines = request_headers.get_all(IF_RANGE).iter();
	let (Some(if_range), None) = (lines.next(), lines.next()) else {
		return false;
	};
	match (read_strong_tag(if_range), read_strong_tag(current_etag)) {
		(Some(requested), Some(current)) => requested == current,
		_ => false,
	}
}

/// The opaque tag of a field value that is one strong entity tag.
fn read_strong_tag(value: &HeaderValue) -> Option<&[u8]> {
	match parse_entity_tag(trim_ows(value.as_bytes())) {
		Some((tag @ EntityTag::Strong(_), [])) => Some(tag.opaque_tag()),
		Some(_) | None => None,
	}
}

/// The selection of a `bytes` range set against a representation of
/// `representation_length` bytes.
fn parse_byte_range_set(
	range_set: &str,
	representation_length: u64,
	limits: RangeLimits,
) -> RangeSelection {
	let mut scan = RangeSetScan::Empty;
	for spec in range_set
		.split(',')
		.map(trim_ows_text)
		.filter(|spec| !spec.is_empty())
	{
		scan = match read_byte_range(spec, representation_length) {
			RangeSpec::Invalid => return RangeSelection::Invalid,
			RangeSpec::Unsatisfiable => scan.add_unsatisfiable_range(),
			RangeSpec::Satisfiable(range) => scan.add_satisfiable_range(range, limits),
		};
	}
	match scan {
		RangeSetScan::Empty => RangeSelection::Invalid,
		RangeSetScan::OnlyUnsatisfiable => RangeSelection::NotSatisfiable,
		RangeSetScan::Satisfiable(ranges) => RangeSelection::Ranges(ranges),
		RangeSetScan::TooMany => RangeSelection::TooManyRanges,
	}
}

/// What the ranges of a range set select so far, while the set is valid.
enum RangeSetScan {
	/// The set has no range so far.
	Empty,
	/// Every range so far starts at or after the representation's end.
	OnlyUnsatisfiable,
	/// The ranges so far that overlap the representation, at least one and at
	/// most the maximum count.
	Satisfiable(Vec<ByteRange>),
	/// More ranges so far overlap the representation than the maximum count.
	TooMany,
}

impl RangeSetScan {
	fn add_unsatisfiable_range(self) -> Self {
		match self {
			Self::Empty => Self::OnlyUnsatisfiable,
			scan @ (Self::OnlyUnsatisfiable | Self::Satisfiable(_) | Self::TooMany) => scan,
		}
	}

	fn add_satisfiable_range(self, range: ByteRange, limits: RangeLimits) -> Self {
		match self {
			Self::Empty | Self::OnlyUnsatisfiable => Self::Satisfiable(vec![range]),
			Self::Satisfiable(mut ranges) if ranges.len() < limits.max_range_count.get() => {
				ranges.push(range);
				Self::Satisfiable(ranges)
			}
			Self::Satisfiable(_) | Self::TooMany => Self::TooMany,
		}
	}
}

/// One range of a `bytes` range set, read against a representation.
enum RangeSpec {
	Satisfiable(ByteRange),
	/// The range starts at or after the representation's end, or it is a
	/// suffix of zero bytes.
	Unsatisfiable,
	Invalid,
}

/// Reads one non-empty item of a `bytes` range set.
fn read_byte_range(spec: &str, representation_length: u64) -> RangeSpec {
	let Some((first, last)) = spec.split_once('-') else {
		return RangeSpec::Invalid;
	};
	let (first, last) = (trim_ows_text(first), trim_ows_text(last));
	let (start, end) = if first.is_empty() {
		// A suffix range: the last `last` bytes.
		let Some(suffix_length) = parse_decimal(last) else {
			return RangeSpec::Invalid;
		};
		if suffix_length == 0 || representation_length == 0 {
			return RangeSpec::Unsatisfiable;
		}
		let suffix_length = suffix_length.min(representation_length);
		(
			representation_length - suffix_length,
			representation_length - 1,
		)
	} else {
		let Some(start) = parse_decimal(first) else {
			return RangeSpec::Invalid;
		};
		let end = if last.is_empty() {
			None
		} else {
			match parse_decimal(last) {
				Some(end) if end >= start => Some(end),
				Some(_) | None => return RangeSpec::Invalid,
			}
		};
		if start >= representation_length {
			return RangeSpec::Unsatisfiable;
		}
		let end = end.map_or(representation_length - 1, |end| {
			end.min(representation_length - 1)
		});
		(start, end)
	};
	RangeSpec::Satisfiable(ByteRange {
		start,
		length: NonZeroU64::new(end - start + 1)
			.expect("a range's last offset is at or after its first"),
		representation_length,
	})
}

/// A non-negative decimal integer of digits only, or `None` for other text or
/// a number that does not fit in a `u64`.
fn parse_decimal(text: &str) -> Option<u64> {
	if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
		return None;
	}
	text.parse().ok()
}

fn trim_ows_text(text: &str) -> &str {
	text.trim_matches([' ', '\t'])
}

fn is_token_byte(byte: u8) -> bool {
	byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}
