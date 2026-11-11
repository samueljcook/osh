//! Canonical text encodings of arbitrary bytes.
//!
//! ```
//! use primitives_rs::encoding::binary_to_text::{ByteEncoding, DecodeOptions};
//!
//! let bytes = ByteEncoding::Base64UrlUnpadded.decode(
//!     "aGVsbG8",
//!     DecodeOptions { max_decoded_size: 32 },
//! )?;
//! assert_eq!(ByteEncoding::HexLower.encode(&bytes), "68656c6c6f");
//! # Ok::<(), primitives_rs::encoding::binary_to_text::DecodeError>(())
//! ```
//!
//! Decoders reject whitespace, prefixes, mixed alphabets, incorrect padding,
//! and nonzero unused bits. Limits count decoded bytes. `decode_into` uses a
//! caller-owned buffer and clears that buffer on every failure, including a
//! capacity error. Encoded strings and decoded vectors are ordinary owned
//! values; callers encoding secrets must protect those values as secrets.
//!
//! UTF-8 conversion uses Rust's native `str::as_bytes`, `str::from_utf8`, and
//! `String::from_utf8_lossy`. The strict parser preserves an initial U+FEFF
//! character and rejects malformed UTF-8. The lossy parser replaces malformed
//! sequences with U+FFFD. Rust strings cannot contain unpaired surrogates.

use data_encoding::{DecodeKind, Encoding, Specification};
use std::{fmt, sync::LazyLock};
use zeroize::{Zeroize, Zeroizing};

/// A canonical byte representation. Each variant defines one accepted spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ByteEncoding {
	/// Lowercase hexadecimal without a prefix.
	HexLower,
	/// Uppercase hexadecimal without a prefix.
	HexUpper,
	/// RFC 4648 Base64 with required `=` padding.
	Base64,
	/// RFC 4648 Base64 without padding.
	Base64Unpadded,
	/// RFC 4648 URL-safe Base64 with required `=` padding.
	Base64Url,
	/// RFC 4648 URL-safe Base64 without padding.
	Base64UrlUnpadded,
	/// RFC 4648 uppercase Base32 with required `=` padding.
	Base32,
	/// RFC 4648 uppercase Base32 without padding.
	Base32Unpadded,
	/// The RFC 4648 Base32 alphabet in lowercase, without padding.
	Base32LowerUnpadded,
	/// Uppercase Crockford symbols in MSB-first byte groups, without padding,
	/// aliases, separators, or a check symbol. This is a byte encoding, not
	/// Crockford's integer notation; unused bits at the right must be zero.
	CrockfordBase32,
	/// Bitcoin's Base58 alphabet, preserving each leading zero byte as `1`.
	/// This format does not include a Base58Check checksum.
	Base58Bitcoin,
}

/// An explicit allocation limit for a decoding operation.
#[derive(Clone, Copy, Debug)]
pub struct DecodeOptions {
	/// The maximum permitted decoded length in bytes.
	pub max_decoded_size: usize,
}

/// A decoding failure. Errors never contain the input or decoded content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeError {
	/// The text length cannot encode whole bytes.
	InvalidLength,
	/// A byte does not belong to the selected alphabet.
	InvalidSymbol {
		/// The zero-based byte offset in the encoded input.
		index: usize,
	},
	/// The final symbol has nonzero unused bits.
	NonZeroTrailingBits {
		/// The zero-based byte offset of the final symbol.
		index: usize,
	},
	/// Padding is missing, misplaced, or excessive.
	InvalidPadding,
	/// The result exceeds the caller's allocation limit or buffer capacity.
	OutputTooSmall,
	/// The allocator could not reserve the result buffer.
	AllocationFailed,
}

impl fmt::Display for DecodeError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidLength => f.write_str("invalid encoded length"),
			Self::InvalidSymbol { index } => write!(f, "invalid encoding symbol at byte {index}"),
			Self::NonZeroTrailingBits { index } => write!(f, "nonzero unused bits at byte {index}"),
			Self::InvalidPadding => f.write_str("invalid encoding padding"),
			Self::OutputTooSmall => f.write_str("decoded bytes exceed the output limit"),
			Self::AllocationFailed => f.write_str("could not allocate decoded bytes"),
		}
	}
}
impl std::error::Error for DecodeError {}

static HEXLOWER: Encoding = data_encoding::HEXLOWER;
static HEXUPPER: Encoding = data_encoding::HEXUPPER;
static BASE64: Encoding = data_encoding::BASE64;
static BASE64URL: Encoding = data_encoding::BASE64URL;
static BASE32: Encoding = data_encoding::BASE32;
static BASE64_NOPAD: Encoding = data_encoding::BASE64_NOPAD;
static BASE64URL_NOPAD: Encoding = data_encoding::BASE64URL_NOPAD;
static BASE32_NOPAD: Encoding = data_encoding::BASE32_NOPAD;

static BASE32_LOWER: LazyLock<Encoding> =
	LazyLock::new(|| make_encoding("abcdefghijklmnopqrstuvwxyz234567"));
static CROCKFORD_BASE32: LazyLock<Encoding> =
	LazyLock::new(|| make_encoding("0123456789ABCDEFGHJKMNPQRSTVWXYZ"));

fn make_encoding(symbols: &str) -> Encoding {
	let mut spec = Specification::new();
	spec.symbols.push_str(symbols);
	spec.encoding().expect("the fixed byte alphabet is valid")
}

impl ByteEncoding {
	/// Encodes bytes with this variant's canonical spelling.
	pub fn encode(self, bytes: &[u8]) -> String {
		match self {
			Self::Base58Bitcoin => bs58::encode(bytes).into_string(),
			_ => self.encoding().encode(bytes),
		}
	}

	/// Encodes bytes directly into a zeroizing string without leaving an unzeroized heap copy.
	///
	/// Fixed-radix encodings (`HexLower`, `Base32*`, `Base64*`) encode in constant time;
	/// [`Self::Base58Bitcoin`] runs in time that varies with input magnitude and leading zero bytes.
	pub fn encode_zeroizing(self, bytes: &[u8]) -> Zeroizing<String> {
		match self {
			Self::Base58Bitcoin => {
				let max_len = bytes.len() + bytes.len().div_ceil(2);
				let mut buf = Zeroizing::new(vec![0_u8; max_len]);
				let written = bs58::encode(bytes)
					.onto(&mut buf[..])
					.expect("preallocated buffer fits Base58 output");
				buf.truncate(written);
				Zeroizing::new(
					String::from_utf8(std::mem::take(&mut *buf)).expect("Base58 output is ASCII"),
				)
			}
			_ => {
				let encoding = self.encoding();
				let mut output =
					Zeroizing::new(String::with_capacity(encoding.encode_len(bytes.len())));
				encoding.encode_append(bytes, &mut output);
				output
			}
		}
	}

	/// Decodes canonical text, allocating at most `max_decoded_size` bytes.
	/// The limit is checked before allocation and before Base58's quadratic work.
	pub fn decode(self, text: &str, options: DecodeOptions) -> Result<Vec<u8>, DecodeError> {
		let size = self.buffer_size(text, options.max_decoded_size)?;
		let mut output = Vec::new();
		output
			.try_reserve_exact(size)
			.map_err(|_| DecodeError::AllocationFailed)?;
		output.resize(size, 0);
		let written = self.decode_into(text, &mut output)?;
		output.truncate(written);
		Ok(output)
	}

	/// Decodes into a caller-owned buffer and returns the number of written bytes.
	/// Bytes beyond the returned length are unchanged on success. The entire
	/// supplied buffer is zeroized on failure; no partial result is returned.
	pub fn decode_into(self, text: &str, output: &mut [u8]) -> Result<usize, DecodeError> {
		let result = self.decode_into_inner(text, output);
		if result.is_err() {
			output.zeroize();
		}
		result
	}

	fn decode_into_inner(self, text: &str, output: &mut [u8]) -> Result<usize, DecodeError> {
		let size = self.buffer_size(text, output.len())?;
		if self == Self::Base58Bitcoin {
			return bs58::decode(text)
				.onto(output)
				.map_err(|error| match error {
					bs58::decode::Error::InvalidCharacter { index, .. }
					| bs58::decode::Error::NonAsciiCharacter { index } => DecodeError::InvalidSymbol { index },
					_ => DecodeError::OutputTooSmall,
				});
		}
		let unpadded = text.trim_end_matches('=');
		self.unpadded_encoding()
			.decode_mut(unpadded.as_bytes(), &mut output[..size])
			.map_err(|partial| map_decode_error(partial.error))
	}

	fn buffer_size(self, text: &str, limit: usize) -> Result<usize, DecodeError> {
		if self == Self::Base58Bitcoin {
			// 1.38 exceeds log(256)/log(58), including leading zero bytes.
			let max_text_size = (limit / 100)
				.saturating_mul(138)
				.saturating_add((limit % 100) * 138 / 100)
				.saturating_add(1);
			if text.len() > max_text_size {
				return Err(DecodeError::OutputTooSmall);
			}
			return Ok(text.len().min(limit));
		}
		let unpadded = text.trim_end_matches('=');
		let padding_size = text.len() - unpadded.len();
		let block_size = match self {
			Self::Base64 | Self::Base64Url => 4,
			Self::Base32 => 8,
			_ => 1,
		};
		if padding_size != (block_size - unpadded.len() % block_size) % block_size
			|| unpadded.contains('=')
		{
			return Err(DecodeError::InvalidPadding);
		}
		// data-encoding requires this bound for its internal size arithmetic.
		if unpadded.len() > usize::MAX / 8 {
			return Err(DecodeError::InvalidLength);
		}
		let size = self
			.unpadded_encoding()
			.decode_len(unpadded.len())
			.map_err(map_decode_error)?;
		if size > limit {
			return Err(DecodeError::OutputTooSmall);
		}
		Ok(size)
	}

	fn encoding(self) -> &'static Encoding {
		match self {
			Self::Base64 => &BASE64,
			Self::Base64Url => &BASE64URL,
			Self::Base32 => &BASE32,
			_ => self.unpadded_encoding(),
		}
	}

	fn unpadded_encoding(self) -> &'static Encoding {
		match self {
			Self::HexLower => &HEXLOWER,
			Self::HexUpper => &HEXUPPER,
			Self::Base64 | Self::Base64Unpadded => &BASE64_NOPAD,
			Self::Base64Url | Self::Base64UrlUnpadded => &BASE64URL_NOPAD,
			Self::Base32 | Self::Base32Unpadded => &BASE32_NOPAD,
			Self::Base32LowerUnpadded => &BASE32_LOWER,
			Self::CrockfordBase32 => &CROCKFORD_BASE32,
			Self::Base58Bitcoin => unreachable!("Base58 uses its own integer codec"),
		}
	}
}

fn map_decode_error(error: data_encoding::DecodeError) -> DecodeError {
	match error.kind {
		DecodeKind::Length => DecodeError::InvalidLength,
		DecodeKind::Symbol => DecodeError::InvalidSymbol {
			index: error.position,
		},
		DecodeKind::Trailing => DecodeError::NonZeroTrailingBits {
			index: error.position,
		},
		DecodeKind::Padding => DecodeError::InvalidPadding,
	}
}
