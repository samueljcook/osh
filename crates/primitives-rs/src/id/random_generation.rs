use std::{fmt, num::NonZeroUsize};

use zeroize::Zeroizing;

use crate::{
	english_words::ENGLISH_WORDS,
	random::secure_bytes::{RandomError, fill_random_bytes, fill_random_bytes_with_source},
};

/// A borrowed alphabet of 2–128 distinct ASCII characters.
///
/// Every character has the same probability. Character order affects deterministic
/// sampling but not the distribution. Parsing does not normalize or change case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RandomTextIdAlphabet<'a> {
	symbols: &'a str,
}

impl RandomTextIdAlphabet<'static> {
	/// The characters `0123456789abcdef`.
	pub const LOWERCASE_HEX: Self = Self {
		symbols: "0123456789abcdef",
	};
	/// The characters `0123456789abcdefghijklmnopqrstuvwxyz`.
	pub const LOWERCASE_ALPHANUMERIC: Self = Self {
		symbols: "0123456789abcdefghijklmnopqrstuvwxyz",
	};
	/// The characters `0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz`.
	pub const MIXED_CASE_ALPHANUMERIC: Self = Self {
		symbols: "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
	};
	/// The RFC 4648 Base64URL alphabet, without padding.
	pub const BASE64_URL: Self = Self {
		symbols: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_",
	};
}

impl<'a> RandomTextIdAlphabet<'a> {
	/// Parses distinct ASCII characters, including explicitly supplied whitespace
	/// or control characters. At least two characters are required for randomness.
	pub fn parse(symbols: &'a str) -> Result<Self, RandomTextIdAlphabetError> {
		if !(2..=128).contains(&symbols.len()) {
			return Err(RandomTextIdAlphabetError::InvalidLength);
		}
		let mut seen = [false; 128];
		for (index, byte) in symbols.bytes().enumerate() {
			if !byte.is_ascii() {
				return Err(RandomTextIdAlphabetError::NonAscii { index });
			}
			if seen[usize::from(byte)] {
				return Err(RandomTextIdAlphabetError::DuplicateCharacter { index });
			}
			seen[usize::from(byte)] = true;
		}
		Ok(Self { symbols })
	}

	/// Returns the alphabet's characters in sampling order.
	pub fn as_str(self) -> &'a str {
		self.symbols
	}
}

/// A malformed random-ID alphabet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RandomTextIdAlphabetError {
	/// The alphabet does not contain 2–128 bytes.
	InvalidLength,
	/// The byte at `index` is not ASCII.
	NonAscii {
		/// Byte position of the rejected character.
		index: usize,
	},
	/// The character at `index` already occurs earlier in the alphabet.
	DuplicateCharacter {
		/// Byte position of the repeated character.
		index: usize,
	},
}

impl fmt::Display for RandomTextIdAlphabetError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidLength => {
				f.write_str("a random-ID alphabet requires 2–128 ASCII characters")
			}
			Self::NonAscii { index } => {
				write!(f, "random-ID alphabet has a non-ASCII byte at {index}")
			}
			Self::DuplicateCharacter { index } => {
				write!(f, "random-ID alphabet repeats a character at {index}")
			}
		}
	}
}

impl std::error::Error for RandomTextIdAlphabetError {}

/// Explicit alphabet and output length for random text identifiers.
#[derive(Clone, Copy, Debug)]
pub struct RandomTextIdOptions<'a> {
	/// Distinct characters sampled with equal probability.
	pub alphabet: RandomTextIdAlphabet<'a>,
	/// Exact character count, which is also the byte count.
	pub length: NonZeroUsize,
}

/// Explicit word count for hyphen-separated English identifiers.
#[derive(Clone, Copy, Debug)]
pub struct RandomWordIdOptions {
	/// Exact number of independently sampled words.
	pub word_count: NonZeroUsize,
}

/// A failure to allocate or generate a random identifier.
#[derive(Debug)]
pub enum RandomIdError {
	/// The output size overflows or its allocation fails.
	AllocationFailed,
	/// The platform's secure random source failed.
	Random(RandomError),
}

impl fmt::Display for RandomIdError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::AllocationFailed => f.write_str("cannot allocate random-ID output"),
			Self::Random(error) => write!(f, "cannot generate random-ID entropy: {error}"),
		}
	}
}

impl std::error::Error for RandomIdError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Random(error) => Some(error),
			_ => None,
		}
	}
}

/// Generates an exact-length string by uniformly sampling the selected alphabet.
///
/// The identifier contains `length * log2(alphabet_size)` bits of entropy.
/// This API returns ordinary public text rather than zeroizing secret storage.
///
/// ```
/// use primitives_rs::id::random_text::{generate_random_text_id, RandomTextIdAlphabet, RandomTextIdOptions};
/// use std::num::NonZeroUsize;
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let id = generate_random_text_id(RandomTextIdOptions {
///     alphabet: RandomTextIdAlphabet::LOWERCASE_ALPHANUMERIC,
///     length: NonZeroUsize::new(24).unwrap(),
/// })?;
/// assert_eq!(id.len(), 24);
/// # Ok(()) }
/// ```
pub fn generate_random_text_id(options: RandomTextIdOptions<'_>) -> Result<String, RandomIdError> {
	generate_random_text_id_with_source(options, fill_random_bytes)
}

fn generate_random_text_id_with_source(
	options: RandomTextIdOptions<'_>,
	mut random_fill_fn: impl FnMut(&mut [u8]) -> Result<(), RandomError>,
) -> Result<String, RandomIdError> {
	let length = options.length.get();
	let mut output = allocate_output(length)?;
	let alphabet = options.alphabet.symbols.as_bytes();
	let accepted_values = (256 / alphabet.len()) * alphabet.len();
	let mut random = Zeroizing::new([0_u8; 256]);
	while output.len() < length {
		let count = (length - output.len()).min(random.len());
		fill_random_bytes_with_source(&mut random[..count], &mut random_fill_fn)
			.map_err(RandomIdError::Random)?;
		for &byte in &random[..count] {
			if usize::from(byte) < accepted_values {
				output.push(char::from(alphabet[usize::from(byte) % alphabet.len()]));
			}
		}
	}
	Ok(std::mem::take(&mut *output))
}

/// Generates lowercase English words separated by single hyphens.
///
/// Each word is sampled independently from the fixed 2,048-word BIP39 English
/// vocabulary, so each word contributes 11 bits of entropy. Words may repeat.
/// Three words give 33 bits; twelve give 132 bits. These are public identifiers
/// with no encoded payload or checksum, not the mnemonic byte-encoding format.
///
/// ```
/// use primitives_rs::id::random_words::{generate_random_word_id, RandomWordIdOptions};
/// use std::num::NonZeroUsize;
/// # fn main() -> Result<(), primitives_rs::id::RandomIdError> {
/// let id = generate_random_word_id(RandomWordIdOptions {
///     word_count: NonZeroUsize::new(4).unwrap(),
/// })?;
/// assert_eq!(id.split('-').count(), 4);
/// # Ok(()) }
/// ```
pub fn generate_random_word_id(options: RandomWordIdOptions) -> Result<String, RandomIdError> {
	generate_random_word_id_with_source(options, fill_random_bytes)
}

fn generate_random_word_id_with_source(
	options: RandomWordIdOptions,
	mut random_fill_fn: impl FnMut(&mut [u8]) -> Result<(), RandomError>,
) -> Result<String, RandomIdError> {
	// The longest vocabulary entry is eight ASCII bytes; separators add one.
	let capacity = options
		.word_count
		.get()
		.checked_mul(9)
		.and_then(|size| size.checked_sub(1))
		.ok_or(RandomIdError::AllocationFailed)?;
	let mut output = allocate_output(capacity)?;
	let mut random = Zeroizing::new([0_u8; 256]);
	let mut remaining = options.word_count.get();
	while remaining > 0 {
		let count = remaining.min(random.len() / 2);
		fill_random_bytes_with_source(&mut random[..count * 2], &mut random_fill_fn)
			.map_err(RandomIdError::Random)?;
		for pair in random[..count * 2].as_chunks::<2>().0 {
			// Every 11-bit index has exactly 32 preimages among 16-bit samples.
			let index = usize::from(u16::from_le_bytes([pair[0], pair[1]]) & 2047);
			if !output.is_empty() {
				output.push('-');
			}
			output.push_str(ENGLISH_WORDS[index]);
		}
		remaining -= count;
	}
	Ok(std::mem::take(&mut *output))
}

fn allocate_output(capacity: usize) -> Result<Zeroizing<String>, RandomIdError> {
	let mut output = String::new();
	output
		.try_reserve_exact(capacity)
		.map_err(|_| RandomIdError::AllocationFailed)?;
	Ok(Zeroizing::new(output))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::collections::HashMap;

	fn fill_sequence(bytes: &[u8]) -> impl FnMut(&mut [u8]) -> Result<(), RandomError> + '_ {
		let mut offset = 0;
		move |output| {
			for byte in output {
				*byte = bytes[offset % bytes.len()];
				offset += 1;
			}
			Ok(())
		}
	}

	#[test]
	fn parsed_alphabets_require_distinct_ascii_and_preserve_explicit_characters() {
		for text in ["", "a", "abcabc", "aé", "😃😄"] {
			assert!(RandomTextIdAlphabet::parse(text).is_err(), "{text:?}");
		}
		let ascii: String = (0_u8..128).map(char::from).collect();
		assert_eq!(RandomTextIdAlphabet::parse(&ascii).unwrap().as_str(), ascii);
		assert_eq!(
			RandomTextIdAlphabet::parse("a \n\t").unwrap().as_str(),
			"a \n\t"
		);
	}

	#[test]
	fn every_supported_alphabet_size_has_exactly_uniform_accepted_samples() {
		let samples: Vec<_> = (0_u8..=255).collect();
		for size in 2..=128 {
			let text: String = (0..size).map(|value| char::from(value as u8)).collect();
			let options = RandomTextIdOptions {
				alphabet: RandomTextIdAlphabet::parse(&text).unwrap(),
				length: NonZeroUsize::new(256 / size * size).unwrap(),
			};
			let output =
				generate_random_text_id_with_source(options, fill_sequence(&samples)).unwrap();
			let mut counts = vec![0; size];
			for byte in output.bytes() {
				counts[usize::from(byte)] += 1;
			}
			assert!(
				counts.iter().all(|&count| count == 256 / size),
				"size {size}"
			);
		}
	}

	#[test]
	fn rejection_sampling_consumes_following_entropy_without_modulo_bias() {
		for (alphabet, samples) in [
			(
				RandomTextIdAlphabet::LOWERCASE_ALPHANUMERIC,
				[252, 253, 35, 36, 0],
			),
			(
				RandomTextIdAlphabet::MIXED_CASE_ALPHANUMERIC,
				[248, 249, 61, 62, 0],
			),
		] {
			let output = generate_random_text_id_with_source(
				RandomTextIdOptions {
					alphabet,
					length: NonZeroUsize::new(3).unwrap(),
				},
				fill_sequence(&samples),
			)
			.unwrap();
			assert_eq!(output, "z00");
		}
	}

	#[test]
	fn word_sampling_matches_the_historical_vector_and_is_uniform_over_all_inputs() {
		let vector = generate_random_word_id_with_source(
			RandomWordIdOptions {
				word_count: NonZeroUsize::new(3).unwrap(),
			},
			fill_sequence(&[0, 0, 1, 0, 255, 255]),
		)
		.unwrap();
		assert_eq!(vector, "abandon-ability-zoo");
		let bytes: Vec<u8> = (0_u16..=u16::MAX).flat_map(u16::to_le_bytes).collect();
		let words = generate_random_word_id_with_source(
			RandomWordIdOptions {
				word_count: NonZeroUsize::new(65_536).unwrap(),
			},
			fill_sequence(&bytes),
		)
		.unwrap();
		let mut counts = HashMap::new();
		for word in words.split('-') {
			*counts.entry(word).or_insert(0) += 1;
		}
		assert_eq!(counts.len(), 2048);
		for word in ENGLISH_WORDS {
			assert_eq!(counts.get(word), Some(&32));
		}
		assert_eq!(ENGLISH_WORDS.iter().map(|word| word.len()).max(), Some(8));
	}

	#[test]
	fn allocation_overflow_does_not_request_entropy() {
		let mut requests = 0;
		let mut source = |_: &mut [u8]| {
			requests += 1;
			Ok(())
		};
		assert!(matches!(
			generate_random_text_id_with_source(
				RandomTextIdOptions {
					alphabet: RandomTextIdAlphabet::LOWERCASE_HEX,
					length: NonZeroUsize::MAX,
				},
				&mut source
			),
			Err(RandomIdError::AllocationFailed)
		));
		assert!(matches!(
			generate_random_word_id_with_source(
				RandomWordIdOptions {
					word_count: NonZeroUsize::MAX,
				},
				&mut source
			),
			Err(RandomIdError::AllocationFailed)
		));
		assert_eq!(requests, 0);
	}

	#[test]
	fn partial_entropy_failures_after_successful_chunks_return_no_identifier() {
		let mut source = fail_second_entropy_request();
		assert!(matches!(
			generate_random_text_id_with_source(
				RandomTextIdOptions {
					alphabet: RandomTextIdAlphabet::LOWERCASE_HEX,
					length: NonZeroUsize::new(300).unwrap(),
				},
				&mut source
			),
			Err(RandomIdError::Random(_))
		));
		let mut source = fail_second_entropy_request();
		assert!(matches!(
			generate_random_word_id_with_source(
				RandomWordIdOptions {
					word_count: NonZeroUsize::new(300).unwrap(),
				},
				&mut source
			),
			Err(RandomIdError::Random(_))
		));
	}

	fn fail_second_entropy_request() -> impl FnMut(&mut [u8]) -> Result<(), RandomError> {
		enum EntropyPhase {
			FirstRequest,
			FailNextRequest,
			AfterFailure,
		}
		let mut phase = EntropyPhase::FirstRequest;
		move |output| {
			output.fill(0xa5);
			match phase {
				EntropyPhase::FirstRequest => {
					phase = EntropyPhase::FailNextRequest;
					Ok(())
				}
				EntropyPhase::FailNextRequest => {
					phase = EntropyPhase::AfterFailure;
					Err(RandomError::new(getrandom::Error::UNSUPPORTED))
				}
				EntropyPhase::AfterFailure => Ok(()),
			}
		}
	}
}
