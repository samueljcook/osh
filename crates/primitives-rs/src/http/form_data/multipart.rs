use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt};
use tempfile::{NamedTempFile, TempPath};
use tokio::io::AsyncWriteExt;

use super::content_type::FormEncoding;
use super::{
	FormContentType, FormData, FormEntry, FormLimit, FormLimitExceeded, FormLimits, FormValue,
	check_limit,
};

type BoxError = Box<dyn Error + Send + Sync>;

/// Limits and upload storage for decoding one request body.
#[derive(Clone, Debug)]
pub struct FormDecodeConfig {
	/// Bounds for encoded input, part count, text values, and files.
	pub limits: FormLimits,
	/// Directory for temporary uploads. `None` uses the OS temporary directory.
	/// Submitted filenames never determine filesystem paths.
	pub upload_dir: Option<PathBuf>,
}

/// One uploaded file with automatic temporary-file cleanup.
///
/// The decoder closes the temporary file after it writes the contents, so an
/// uploaded file holds no open file descriptor. Open `temp_path` to read the
/// contents. Dropping this value deletes its file. Moving it transfers
/// ownership, and `temp_path.persist(path)` retains the file by renaming it.
/// Deserialization transfers the file from [`FormData::into_deserialized`]. An
/// empty browser file input can submit an empty filename and a zero-byte file.
/// Both are preserved.
#[derive(Debug)]
pub struct UploadedFile {
	/// The submitted filename. Treat it as untrusted display metadata, not a path.
	pub file_name: String,
	/// The parsed part content type, when present and well-formed. This does not
	/// verify the uploaded contents.
	pub content_type: Option<String>,
	/// The path of the closed temporary file. Dropping it deletes the file.
	pub temp_path: TempPath,
}

#[cfg(feature = "tsgen")]
impl crate::tsgen::TsGen for UploadedFile {
	fn type_ref() -> crate::tsgen::TypeRef {
		crate::tsgen::TypeRef::Blob
	}
}

/// Why decoding a form body failed.
///
/// Parser errors contain no submitted names or values. Body and storage errors
/// retain their original causes for callers that need to classify failures.
#[derive(Debug)]
pub enum FormDecodeError {
	/// Multipart syntax is invalid, or a part has no field name.
	MalformedMultipart,
	/// An explicit input, part-count, text, or file limit was exceeded.
	LimitExceeded(FormLimitExceeded),
	/// Reading the input stream failed.
	BodyRead(BoxError),
	/// Creating or writing an upload's temporary file failed.
	Storage(io::Error),
}
impl std::fmt::Display for FormDecodeError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::MalformedMultipart => f.write_str("malformed multipart form body"),
			Self::LimitExceeded(error) => error.fmt(f),
			Self::BodyRead(_) => f.write_str("could not read the form body"),
			Self::Storage(_) => f.write_str("could not store an uploaded file"),
		}
	}
}
impl Error for FormDecodeError {
	fn source(&self) -> Option<&(dyn Error + 'static)> {
		match self {
			Self::LimitExceeded(error) => Some(error),
			Self::BodyRead(error) => Some(&**error),
			Self::Storage(error) => Some(error),
			Self::MalformedMultipart => None,
		}
	}
}
impl From<FormLimitExceeded> for FormDecodeError {
	fn from(error: FormLimitExceeded) -> Self {
		Self::LimitExceeded(error)
	}
}

/// Decodes URL-encoded or multipart form data from a byte stream on Tokio.
///
/// File parts stream to temporary files. Text values remain in memory. A part
/// is a file when its Content-Disposition has a `filename` or `filename*`
/// parameter, with the parameter name compared without regard to ASCII case,
/// even when the parameter's value is not valid UTF-8; other parts are text. A
/// file's name is its `filename` value. Invalid UTF-8 in that value becomes
/// U+FFFD, and a part with only a `filename*` parameter has an empty file name
/// because RFC 7578 forbids `filename*` in form data. Repeated and empty names
/// are preserved in submission order. A part without a name is invalid.
///
/// Multipart text uses its `charset` parameter as an Encoding Standard label
/// and UTF-8 when the parameter is absent. An unknown label makes the body
/// invalid. Decoding does not interpret a byte order mark, and invalid
/// sequences become U+FFFD.
///
/// Each file part creates one temporary file, which the decoder closes after
/// writing its contents. Only `limits.max_parts` and `limits.max_size` bound the
/// number of temporary files. Without `max_parts`, a file part with an empty
/// name, an empty filename, and empty contents takes fewer than 50 bytes of
/// input under a one-byte boundary, so a 1 MiB `max_size` permits more than
/// 20,000 temporary files in one form.
///
/// The complete stream is counted, including a multipart epilogue after the
/// closing boundary. The decoder stops at the first error. Dropping its future
/// cancels reading and drops any uploads. File creation already running on
/// Tokio's blocking pool finishes independently and drops its result.
///
/// The decoder limits parser read-ahead to chunks of 16 KiB. Multipart headers
/// and boundary preambles can still occupy up to `limits.max_size` in parser
/// memory; file and text limits do not limit these structural bytes. Input chunks
/// themselves are allocated by the caller. Request deadlines belong to the host.
pub async fn decode_form_data<S, E>(
	content_type: FormContentType,
	body: S,
	config: FormDecodeConfig,
) -> Result<FormData, FormDecodeError>
where
	S: Stream<Item = Result<Bytes, E>> + Send,
	E: Into<BoxError>,
{
	let mut body = BoundedBodyChunks {
		max_size: config.limits.max_size,
		state: BodyChunkState::Reading(BodyChunkSource {
			stream: Box::pin(body),
			pending: Bytes::new(),
			remaining: config.limits.max_size,
		}),
	};
	match content_type.0 {
		FormEncoding::UrlEncoded => {
			let mut bytes = BytesMut::new();
			while let Some(chunk) = body.next().await {
				bytes.extend_from_slice(&chunk?);
			}
			Ok(FormData::parse_url_encoded(&bytes, config.limits)?)
		}
		FormEncoding::Multipart { boundary } => {
			let form = read_multipart(&mut body, boundary, &config).await?;
			// Multipart ends at its closing boundary. Read the remaining transport
			// bytes so that epilogues cannot bypass the request-size limit.
			while let Some(chunk) = body.next().await {
				chunk?;
			}
			Ok(form)
		}
	}
}

async fn read_multipart<S>(
	body: S,
	boundary: String,
	config: &FormDecodeConfig,
) -> Result<FormData, FormDecodeError>
where
	S: Stream<Item = Result<Bytes, FormDecodeError>> + Send,
{
	let mut multipart = multer::Multipart::new(body, boundary);
	let mut entries = Vec::new();
	while let Some(mut field) = multipart.next_field().await.map_err(from_multer)? {
		check_limit(
			entries.len() + 1,
			config.limits.max_parts,
			FormLimit::PartCount,
		)?;
		let name = field
			.name()
			.ok_or(FormDecodeError::MalformedMultipart)?
			.to_owned();
		let file_name = match field.file_name() {
			Some(file_name) => Some(file_name.to_owned()),
			None => undecoded_file_name(field.headers()),
		};
		let value = if let Some(file_name) = file_name {
			let content_type = field.content_type().map(ToString::to_string);
			let temp_path = store_file(&mut field, config).await?;
			FormValue::File(UploadedFile {
				file_name,
				content_type,
				temp_path,
			})
		} else {
			let encoding = match field
				.content_type()
				.and_then(|mime| mime.get_param(mime::CHARSET))
			{
				Some(charset) => encoding_rs::Encoding::for_label(charset.as_str().as_bytes())
					.ok_or(FormDecodeError::MalformedMultipart)?,
				None => encoding_rs::UTF_8,
			};
			let mut bytes = BytesMut::new();
			while let Some(chunk) = field.chunk().await.map_err(from_multer)? {
				check_limit(
					bytes.len().saturating_add(chunk.len()),
					config.limits.max_text_size,
					FormLimit::TextSize,
				)?;
				bytes.extend_from_slice(&chunk);
			}
			let text = encoding.decode_without_bom_handling(&bytes).0;
			check_limit(text.len(), config.limits.max_text_size, FormLimit::TextSize)?;
			FormValue::Text(text.into_owned())
		};
		entries.push(FormEntry { name, value });
	}
	Ok(FormData { entries })
}

/// The file name of a part with a `filename` value that multer does not return,
/// or `None` when the Content-Disposition has no `filename` or `filename*`
/// parameter. Multer matches only the lowercase name `filename`, and it returns
/// no name for a value that is not valid UTF-8 or when a `filename*` parameter
/// precedes `filename`.
fn undecoded_file_name(headers: &http::HeaderMap) -> Option<String> {
	let disposition = headers.get(http::header::CONTENT_DISPOSITION)?.as_bytes();
	match disposition_parameters(disposition)
		.find(|(name, _)| name.eq_ignore_ascii_case(b"filename"))
	{
		Some((_, value)) => Some(String::from_utf8_lossy(value).replace("\\\"", "\"")),
		None => disposition_parameters(disposition)
			.any(|(name, _)| name.eq_ignore_ascii_case(b"filename*"))
			.then(String::new),
	}
}

/// The parameters after the disposition type, as raw names and values.
/// A quoted value excludes its quotes and keeps its backslash escapes. Text
/// without `=` between two semicolons is skipped.
fn disposition_parameters(disposition: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
	let parameters = disposition
		.iter()
		.position(|&byte| byte == b';')
		.map_or(&[][..], |index| &disposition[index + 1..]);
	let mut rest = parameters;
	std::iter::from_fn(move || {
		loop {
			rest = rest.trim_ascii_start();
			if rest.is_empty() {
				return None;
			}
			let name_end = rest
				.iter()
				.position(|&byte| byte == b'=' || byte == b';')
				.unwrap_or(rest.len());
			let name = rest[..name_end].trim_ascii();
			if rest.get(name_end) != Some(&b'=') {
				rest = rest.get(name_end + 1..).unwrap_or_default();
				continue;
			}
			let value_start = rest[name_end + 1..].trim_ascii_start();
			let (value, after_value) = match value_start {
				[b'"', quoted @ ..] => {
					let end = quoted_value_len(quoted);
					(&quoted[..end], quoted.get(end + 1..).unwrap_or_default())
				}
				_ => {
					let end = value_start
						.iter()
						.position(|&byte| byte == b';')
						.unwrap_or(value_start.len());
					(value_start[..end].trim_ascii(), &value_start[end..])
				}
			};
			rest = after_value
				.iter()
				.position(|&byte| byte == b';')
				.map_or(&[][..], |index| &after_value[index + 1..]);
			return Some((name, value));
		}
	})
}

/// The length of a quoted value before its closing quote, skipping each byte
/// after a backslash. An unterminated value extends to the end.
fn quoted_value_len(quoted: &[u8]) -> usize {
	let mut index = 0;
	while let Some(&byte) = quoted.get(index) {
		match byte {
			b'"' => return index,
			b'\\' => index += 2,
			_ => index += 1,
		}
	}
	quoted.len()
}

async fn store_file(
	field: &mut multer::Field<'_>,
	config: &FormDecodeConfig,
) -> Result<TempPath, FormDecodeError> {
	let dir = config.upload_dir.clone();
	let (handle, temp_path) = tokio::task::spawn_blocking(move || {
		let temp_file = match dir {
			Some(dir) => NamedTempFile::new_in(dir)?,
			None => NamedTempFile::new()?,
		};
		Ok::<_, io::Error>(temp_file.into_parts())
	})
	.await
	.map_err(io::Error::other)
	.and_then(|created| created)
	.map_err(FormDecodeError::Storage)?;
	let mut handle = tokio::fs::File::from_std(handle);
	let mut size = 0usize;
	while let Some(chunk) = field.chunk().await.map_err(from_multer)? {
		size = size.saturating_add(chunk.len());
		check_limit(size, config.limits.max_file_size, FormLimit::FileSize)?;
		handle
			.write_all(&chunk)
			.await
			.map_err(FormDecodeError::Storage)?;
	}
	handle.flush().await.map_err(FormDecodeError::Storage)?;
	// Closing the file here keeps a form's open descriptors independent of its
	// number of file parts.
	drop(handle.into_std().await);
	Ok(temp_path)
}

fn from_multer(error: multer::Error) -> FormDecodeError {
	match error {
		multer::Error::StreamReadFailed(error) => match error.downcast::<FormDecodeError>() {
			Ok(error) => *error,
			Err(error) => FormDecodeError::BodyRead(error),
		},
		_ => FormDecodeError::MalformedMultipart,
	}
}

// Multer reads until Pending before it parses. Yielding between bounded chunks
// prevents an immediately-ready source from accumulating whole file contents
// in its internal buffer before file streaming begins.
struct BoundedBodyChunks<S> {
	state: BodyChunkState<S>,
	max_size: usize,
}

struct BodyChunkSource<S> {
	stream: Pin<Box<S>>,
	pending: Bytes,
	remaining: usize,
}

enum BodyChunkState<S> {
	Reading(BodyChunkSource<S>),
	Yielding(BodyChunkSource<S>),
	Finished,
}
impl<S, E> Stream for BoundedBodyChunks<S>
where
	S: Stream<Item = Result<Bytes, E>>,
	E: Into<BoxError>,
{
	type Item = Result<Bytes, FormDecodeError>;
	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let this = self.get_mut();
		let mut source = match std::mem::replace(&mut this.state, BodyChunkState::Finished) {
			BodyChunkState::Finished => return Poll::Ready(None),
			BodyChunkState::Yielding(source) => {
				this.state = BodyChunkState::Reading(source);
				cx.waker().wake_by_ref();
				return Poll::Pending;
			}
			BodyChunkState::Reading(source) => source,
		};
		if source.pending.is_empty() {
			match source.stream.as_mut().poll_next(cx) {
				Poll::Pending => {
					this.state = BodyChunkState::Reading(source);
					return Poll::Pending;
				}
				Poll::Ready(None) => return Poll::Ready(None),
				Poll::Ready(Some(Err(error))) => {
					return Poll::Ready(Some(Err(FormDecodeError::BodyRead(error.into()))));
				}
				Poll::Ready(Some(Ok(chunk))) => {
					let Some(remaining) = source.remaining.checked_sub(chunk.len()) else {
						return Poll::Ready(Some(Err(FormLimitExceeded {
							resource: FormLimit::InputSize,
							max: this.max_size,
						}
						.into())));
					};
					source.remaining = remaining;
					source.pending = chunk;
				}
			}
		}
		let size = source.pending.len().min(16 * 1024);
		let chunk = source.pending.split_to(size);
		this.state = BodyChunkState::Yielding(source);
		Poll::Ready(Some(Ok(chunk)))
	}
}
