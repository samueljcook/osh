//! Static public assets: the response body that streams a file from disk, the
//! content type of a file name, and the answer to a range request.

use std::io;
use std::num::NonZeroUsize;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::HeaderMap;
use http_body::{Body, Frame, SizeHint};
use primitives_rs::http::file_bodies::{FileBody, FileBodyConfig};
use primitives_rs::http::media_types::find_media_type_by_extension;
use primitives_rs::http::range_requests::{
	ByteRange, RangeLimits, RangeRepresentation, RangeSelection, select_byte_ranges,
};
use tokio::fs::File;

/// The maximum number of bytes in one frame of a streamed file.
const MAX_FRAME_SIZE: NonZeroUsize = NonZeroUsize::new(64 * 1024).unwrap();

/// The content type of a file with an extension that has no registered media type.
const UNKNOWN_CONTENT_TYPE: &str = "application/octet-stream";

/// The body of a response from [`StaticPublicAssetsService`](crate::StaticPublicAssetsService).
///
/// A static public asset's body reads the file in frames while the host server
/// writes the response, so the file is never held whole in memory. Its size
/// hint is the exact number of bytes that remain. An I/O error ends the body
/// after one error frame and releases the file.
#[derive(Debug)]
pub struct StaticPublicAssetBody {
	state: AssetBodyState,
}

#[derive(Debug)]
enum AssetBodyState {
	Buffered(Bytes),
	/// The body reads a range of a file until the file body ends.
	File(FileBody),
	LengthOnly(u64),
	/// The body sent its buffered bytes.
	Ended,
}

impl StaticPublicAssetBody {
	/// The body of a `GET` response: the bytes of `range` of the file, read
	/// as the host polls the body.
	pub(crate) async fn from_file(file: File, range: FileRange) -> io::Result<Self> {
		let body = FileBody::from_file(
			file,
			FileBodyConfig {
				start: range.start,
				length: range.length,
				max_frame_size: MAX_FRAME_SIZE,
			},
		)
		.await?;
		Ok(Self {
			state: AssetBodyState::File(body),
		})
	}

	/// The body of a `HEAD` response: no frames, and the length that the
	/// `GET` response would have.
	pub(crate) fn from_length_only(length: u64) -> Self {
		Self {
			state: AssetBodyState::LengthOnly(length),
		}
	}

	/// A body that is already in memory.
	pub(crate) fn from_bytes(bytes: Bytes) -> Self {
		Self {
			state: AssetBodyState::Buffered(bytes),
		}
	}
}

impl Body for StaticPublicAssetBody {
	type Data = Bytes;
	type Error = io::Error;

	fn poll_frame(
		self: Pin<&mut Self>,
		ctx: &mut Context<'_>,
	) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
		let body = self.get_mut();
		match &mut body.state {
			AssetBodyState::Buffered(_) => {
				let AssetBodyState::Buffered(bytes) =
					std::mem::replace(&mut body.state, AssetBodyState::Ended)
				else {
					unreachable!("the body owns buffered bytes");
				};
				Poll::Ready(Some(Ok(Frame::data(bytes))))
			}
			AssetBodyState::File(file_body) => Pin::new(file_body).poll_frame(ctx),
			AssetBodyState::LengthOnly(_) | AssetBodyState::Ended => Poll::Ready(None),
		}
	}

	fn size_hint(&self) -> SizeHint {
		match &self.state {
			AssetBodyState::Buffered(bytes) => SizeHint::with_exact(bytes.len() as u64),
			AssetBodyState::File(file_body) => file_body.size_hint(),
			AssetBodyState::LengthOnly(length) => SizeHint::with_exact(*length),
			AssetBodyState::Ended => SizeHint::with_exact(0),
		}
	}
}

/// The content type of a file, from its name's extension.
pub(crate) fn read_content_type(file_name: &str) -> String {
	find_media_type_by_extension(Path::new(file_name)).map_or_else(
		|| UNKNOWN_CONTENT_TYPE.to_owned(),
		|media_type| media_type.as_str().to_owned(),
	)
}

/// A run of a file's bytes that a `GET` answer sends: `length` bytes from
/// offset `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileRange {
	pub(crate) start: u64,
	pub(crate) length: u64,
}

/// How a `GET` or `HEAD` request for a static public asset is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StaticPublicAssetAnswer {
	/// `200` with the whole file.
	Whole,
	/// `206` with one range of the file.
	Partial(ByteRange),
	/// `416`: the `Range` header selects no byte of a non-empty file, or it
	/// is invalid.
	RangeNotSatisfiable,
}

/// Decides the answer from the request's `Range` and `If-Range` headers.
///
/// A static public asset has no validator, so a request with `If-Range` cannot
/// confirm one, and RFC 9110 section 13.1.5 requires the whole file. A
/// `Range` header with a range unit other than `bytes` also gets the whole
/// file, because RFC 9110 section 14.2 requires an origin server to ignore it.
/// The layer sends at most one range, so a `Range` header with several
/// satisfiable ranges gets the whole file, which RFC 9110 section 14.2
/// permits, and so does a list of byte ranges for an empty file, which no
/// range overlaps. A `bytes` header that is not a list of byte ranges, and a
/// request with several `Range` header lines, get `416`, as Go's
/// `http.FileServer` answers a header that is not a list of byte ranges.
pub(crate) fn decide_static_public_asset_answer(
	headers: &HeaderMap,
	length: u64,
) -> StaticPublicAssetAnswer {
	let representation = RangeRepresentation { length, etag: None };
	let limits = RangeLimits {
		max_range_count: NonZeroUsize::MIN,
	};
	match select_byte_ranges(headers, representation, limits) {
		RangeSelection::Whole | RangeSelection::TooManyRanges | RangeSelection::UnsupportedUnit => {
			StaticPublicAssetAnswer::Whole
		}
		RangeSelection::Ranges(ranges) => match ranges.as_slice() {
			[range] => StaticPublicAssetAnswer::Partial(*range),
			_ => StaticPublicAssetAnswer::Whole,
		},
		RangeSelection::NotSatisfiable if length == 0 => StaticPublicAssetAnswer::Whole,
		RangeSelection::NotSatisfiable | RangeSelection::Invalid => {
			StaticPublicAssetAnswer::RangeNotSatisfiable
		}
	}
}
