//! Checksummed English-word encoding of arbitrary bytes.
//!
//! This is the Primitives mnemonic byte format, not BIP39's wallet mnemonic or
//! seed derivation. It uses the ordered 2,048-word BIP39 English vocabulary.
//! The checksum detects accidental transcription errors; it does not authenticate
//! data. This module does not generate entropy, derive keys, or encrypt bytes.
//!
//! # Format
//!
//! Words are lowercase and separated by exactly one ASCII space, without leading
//! or trailing whitespace. The first word's index is `11 * version + padding`;
//! version is zero and padding is the number of zero bits appended to the data
//! (0 through 10). Data bytes are split MSB-first into 11-bit word indices.
//! Two final words contain a 22-bit checksum, most significant word first.
//! The checksum is the first 22 bits of BLAKE3 over the concatenation of
//! `b"mnemonic-codec/v1/checksum"`, the byte length as a big-endian u64, and
//! the bytes. Empty input has a header word and two checksum words.
//!
//! Decoding checks spelling, version, byte alignment, zero padding, checksum,
//! and an explicit output limit. Returned strings and vectors are ordinary owned
//! values. Applications must protect encodings of secret material as secrets.
use crate::{
	encoding::binary_to_text::{DecodeError, DecodeOptions},
	english_words::ENGLISH_WORDS,
};
use std::fmt;
use zeroize::{Zeroize, Zeroizing};

/// A failure to decode the Primitives mnemonic byte format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MnemonicDecodeError {
	/// A word is missing, a separator is invalid, or the data is not byte-aligned.
	InvalidFormat,
	/// A word is absent from the exact lowercase English vocabulary.
	UnknownWord {
		/// The zero-based word index.
		index: usize,
	},
	/// The header identifies an unsupported format version.
	UnsupportedVersion,
	/// The data's last word contains nonzero padding bits.
	NonZeroPadding,
	/// The checksum does not match the decoded bytes.
	ChecksumMismatch,
	/// Allocation or output capacity prevents decoding.
	Output(DecodeError),
}
impl fmt::Display for MnemonicDecodeError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidFormat => f.write_str("invalid mnemonic byte format"),
			Self::UnknownWord { index } => write!(f, "unknown mnemonic word at index {index}"),
			Self::UnsupportedVersion => f.write_str("unsupported mnemonic byte format version"),
			Self::NonZeroPadding => f.write_str("nonzero mnemonic padding bits"),
			Self::ChecksumMismatch => f.write_str("mnemonic checksum mismatch"),
			Self::Output(error) => error.fmt(f),
		}
	}
}
impl std::error::Error for MnemonicDecodeError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Output(error) => Some(error),
			_ => None,
		}
	}
}

/// Encodes arbitrary bytes as a canonical checksummed English-word string.
pub fn encode(bytes: &[u8]) -> String {
	let checksum = checksum(bytes);
	let mut size = 0_usize;
	for_each_word(bytes, checksum, |word| {
		size = size
			.checked_add(word.len() + 1)
			.expect("mnemonic text length exceeds capacity");
	});
	let mut result = String::with_capacity(size - 1);
	for_each_word(bytes, checksum, |word| {
		if !result.is_empty() {
			result.push(' ');
		}
		result.push_str(word);
	});
	result
}

fn for_each_word(bytes: &[u8], checksum: u32, mut visit_fn: impl FnMut(&'static str)) {
	let padding = (11 - (bytes.len() % 11) * 8 % 11) % 11;
	visit_fn(ENGLISH_WORDS[padding]);
	let mut accumulator = Zeroizing::new(0_u32);
	let mut bits = 0;
	for &byte in bytes {
		*accumulator = (*accumulator << 8) | u32::from(byte);
		bits += 8;
		if bits >= 11 {
			bits -= 11;
			visit_fn(ENGLISH_WORDS[((*accumulator >> bits) & 2047) as usize]);
		}
	}
	if bits != 0 {
		visit_fn(ENGLISH_WORDS[((*accumulator << (11 - bits)) & 2047) as usize]);
	}
	visit_fn(ENGLISH_WORDS[(checksum >> 11) as usize]);
	visit_fn(ENGLISH_WORDS[(checksum & 2047) as usize]);
}

/// Decodes a mnemonic with an explicit maximum result size in bytes.
pub fn decode(text: &str, options: DecodeOptions) -> Result<Vec<u8>, MnemonicDecodeError> {
	let size = decoded_size(text, options.max_decoded_size)?;
	let mut output = Vec::new();
	output
		.try_reserve_exact(size)
		.map_err(|_| MnemonicDecodeError::Output(DecodeError::AllocationFailed))?;
	output.resize(size, 0);
	decode_into(text, &mut output)?;
	Ok(output)
}

/// Decodes into a caller-owned buffer and returns the number of written bytes.
/// The entire supplied buffer is zeroized on failure. Bytes after the result
/// are unchanged on success.
pub fn decode_into(text: &str, output: &mut [u8]) -> Result<usize, MnemonicDecodeError> {
	let result = decode_into_inner(text, output);
	if result.is_err() {
		output.zeroize();
	}
	result
}

fn decoded_size(text: &str, limit: usize) -> Result<usize, MnemonicDecodeError> {
	let max_text_size = limit
		.saturating_mul(8)
		.div_ceil(11)
		.saturating_add(3)
		.saturating_mul(9)
		.saturating_sub(1);
	if text.len() > max_text_size {
		return Err(MnemonicDecodeError::Output(DecodeError::OutputTooSmall));
	}
	let mut words = text.split(' ');
	let header = word_index(
		words
			.next()
			.expect("str::split yields at least one item, even for empty text"),
		0,
	)?;
	if header >= 11 {
		return Err(MnemonicDecodeError::UnsupportedVersion);
	}
	let data_words = words
		.count()
		.checked_sub(2)
		.ok_or(MnemonicDecodeError::InvalidFormat)?;
	let bits = data_words
		.checked_mul(11)
		.and_then(|n| n.checked_sub(header))
		.ok_or(MnemonicDecodeError::InvalidFormat)?;
	if bits % 8 != 0 || (bits == 0 && header != 0) {
		return Err(MnemonicDecodeError::InvalidFormat);
	}
	if bits / 8 > limit {
		return Err(MnemonicDecodeError::Output(DecodeError::OutputTooSmall));
	}
	Ok(bits / 8)
}

fn decode_into_inner(text: &str, output: &mut [u8]) -> Result<usize, MnemonicDecodeError> {
	let size = decoded_size(text, output.len())?;
	let word_count = text.split(' ').count();
	let mut accumulator = Zeroizing::new(0_u32);
	let mut bits = 0;
	let mut written = 0;
	let mut supplied_checksum = 0;
	for (index, word) in text.split(' ').enumerate().skip(1) {
		let value = word_index(word, index)? as u32;
		if index >= word_count - 2 {
			supplied_checksum = (supplied_checksum << 11) | value;
			continue;
		}
		*accumulator = (*accumulator << 11) | value;
		bits += 11;
		while bits >= 8 && written < size {
			bits -= 8;
			output[written] = (*accumulator >> bits) as u8;
			written += 1;
		}
		*accumulator &= (1 << bits) - 1;
	}
	if *accumulator != 0 {
		return Err(MnemonicDecodeError::NonZeroPadding);
	}
	if checksum(&output[..size]) != supplied_checksum {
		return Err(MnemonicDecodeError::ChecksumMismatch);
	}
	Ok(size)
}
fn word_index(word: &str, index: usize) -> Result<usize, MnemonicDecodeError> {
	if word.is_empty() {
		return Err(MnemonicDecodeError::InvalidFormat);
	}
	ENGLISH_WORDS
		.binary_search(&word)
		.map_err(|_| MnemonicDecodeError::UnknownWord { index })
}
fn checksum(bytes: &[u8]) -> u32 {
	let mut hasher = blake3::Hasher::new();
	hasher.update(b"mnemonic-codec/v1/checksum");
	hasher.update(&(bytes.len() as u64).to_be_bytes());
	hasher.update(bytes);
	let mut hash = hasher.finalize();
	let digest = hash.as_bytes();
	let result =
		((u32::from(digest[0]) << 16) | (u32::from(digest[1]) << 8) | u32::from(digest[2])) >> 2;
	hasher.zeroize();
	hash.zeroize();
	result
}
