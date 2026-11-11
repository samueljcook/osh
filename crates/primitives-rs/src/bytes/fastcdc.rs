//! Content-defined chunks with the FastCDC 2020 algorithm.
//!
//! Identical bytes and [`ChunkerOptions`] produce identical boundaries across
//! slice iteration and reader fragmentation. This module uses the Gear tables,
//! mask selection, seed transform, and boundary rules from `fastcdc` 5.0.0's
//! `v2020` implementation. Those boundaries are part of this API's contract;
//! changing them requires an explicitly different algorithm contract. Persist
//! [`ALGORITHM_ID`] and all options when later rechunking must reproduce
//! the same boundaries.
//!
//! Gear is a noncryptographic boundary calculation. This API does not expose
//! its fingerprint as a content identity. Content addressing and authentication
//! require separate cryptographic constructions; this module does not select
//! their keys, namespaces, or privacy policy.
//! Changing the seed changes boundaries; it provides no confidentiality or
//! authentication guarantee.
//!
//! ```
//! use primitives_rs::bytes::fastcdc::{Chunker, ChunkerOptions, Normalization, ReadOptions};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let chunker = Chunker::new(ChunkerOptions {
//!     min_size: 2_048,
//!     target_size: 8_192,
//!     max_size: 32_768,
//!     normalization: Normalization::Level1,
//!     seed: 0,
//! })?;
//! let input = b"application content";
//! let mut reader = chunker.reader(&input[..], ReadOptions { max_input_size: 1_000_000 })?;
//! while let Some(chunk) = reader.read_chunk()? {
//!     assert_eq!(chunk.offset, 0);
//!     assert_eq!(chunk.bytes, input);
//! }
//! # Ok(())
//! # }
//! ```

use std::{fmt, iter::FusedIterator};

mod reader;
pub use reader::{ChunkReader, ReadError, ReadOptions};

use ::fastcdc::v2020;
/// FastCDC's explicit normalization levels, from no normalization to level 3.
pub use ::fastcdc::v2020::Normalization;

/// Stable identity for this module's complete boundary algorithm.
///
/// Retain this value together with every [`ChunkerOptions`] field when stored
/// content must be rechunked identically. It identifies the boundary rules,
/// tables, and mask selection, not a manifest encoding or a cryptographic hash.
pub const ALGORITHM_ID: &str = "primitives-rs/fastcdc2020/v1";

/// Parameters that determine the chunk boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChunkerOptions {
	/// Minimum chunk size in bytes, except for the final chunk; even, 64..=1_048_576.
	pub min_size: usize,
	/// Nominal target size in bytes; even, 256..=4_194_304.
	///
	/// This is a boundary-search threshold and selects probability masks by
	/// rounding `log2(target_size)`. It does not guarantee an observed mean size.
	pub target_size: usize,
	/// Maximum chunk size in bytes; even, 1_024..=16_777_216.
	pub max_size: usize,
	/// Strength of the algorithm's bias toward the target chunk size.
	pub normalization: Normalization,
	/// XOR input for the Gear tables. Zero selects the unmodified tables.
	///
	/// This value is a boundary parameter, not a cryptographic key.
	pub seed: u64,
}

/// Chunk sizes do not satisfy the algorithm's accepted ranges and ordering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
	/// The minimum is odd or outside 64..=1_048_576.
	InvalidMinSize {
		/// The supplied value in bytes.
		value: usize,
	},
	/// The target is odd or outside 256..=4_194_304.
	InvalidTargetSize {
		/// The supplied value in bytes.
		value: usize,
	},
	/// The maximum is odd or outside 1_024..=16_777_216.
	InvalidMaxSize {
		/// The supplied value in bytes.
		value: usize,
	},
	/// Sizes do not satisfy `min_size <= target_size <= max_size`.
	InvalidSizeOrder,
}

impl fmt::Display for ConfigError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidMinSize { value } => write!(
				f,
				"minimum chunk size {value} must be even and within 64..=1048576"
			),
			Self::InvalidTargetSize { value } => write!(
				f,
				"target chunk size {value} must be even and within 256..=4194304"
			),
			Self::InvalidMaxSize { value } => write!(
				f,
				"maximum chunk size {value} must be even and within 1024..=16777216"
			),
			Self::InvalidSizeOrder => {
				f.write_str("chunk sizes must satisfy min_size <= target_size <= max_size")
			}
		}
	}
}
impl std::error::Error for ConfigError {}

/// Parsed FastCDC 2020 parameters and precomputed boundary tables.
///
/// Construction and slice iteration allocate no heap memory. Each reader owns
/// a separate buffer, so one chunker can serve independent concurrent readers.
pub struct Chunker {
	options: ChunkerOptions,
	gear: [u64; 256],
	shifted_gear: [u64; 256],
	strict_mask: u64,
	relaxed_mask: u64,
}

impl fmt::Debug for Chunker {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Chunker")
			.field("options", &self.options)
			.finish_non_exhaustive()
	}
}

impl Chunker {
	/// Parses all size constraints before preparing the boundary tables.
	pub fn new(options: ChunkerOptions) -> Result<Self, ConfigError> {
		if !(v2020::MINIMUM_MIN..=v2020::MINIMUM_MAX).contains(&options.min_size)
			|| !options.min_size.is_multiple_of(2)
		{
			return Err(ConfigError::InvalidMinSize {
				value: options.min_size,
			});
		}
		if !(v2020::AVERAGE_MIN..=v2020::AVERAGE_MAX).contains(&options.target_size)
			|| !options.target_size.is_multiple_of(2)
		{
			return Err(ConfigError::InvalidTargetSize {
				value: options.target_size,
			});
		}
		if !(v2020::MAXIMUM_MIN..=v2020::MAXIMUM_MAX).contains(&options.max_size)
			|| !options.max_size.is_multiple_of(2)
		{
			return Err(ConfigError::InvalidMaxSize {
				value: options.max_size,
			});
		}
		if options.min_size > options.target_size || options.target_size > options.max_size {
			return Err(ConfigError::InvalidSizeOrder);
		}
		// Round log2(target_size) using integers so platform math libraries
		// cannot affect persisted boundaries. The midpoint is sqrt(2) * 2^bits;
		// squaring both positive sides gives an exact comparison in this range.
		let target = options.target_size as u64;
		let lower_bits = target.ilog2();
		let bits = lower_bits + u32::from(target * target >= 1 << (2 * lower_bits + 1));
		let level = options.normalization.bits();
		let strict_mask = v2020::MASKS[(bits + level) as usize];
		let relaxed_mask = v2020::MASKS[(bits - level) as usize];
		// A zero seed borrows the dependency's static tables without allocating.
		let (base_gear, base_shifted_gear) = v2020::get_gear_with_seed(0);
		let gear = std::array::from_fn(|index| base_gear[index] ^ options.seed);
		let shifted_gear =
			std::array::from_fn(|index| base_shifted_gear[index] ^ (options.seed << 1));
		Ok(Self {
			options,
			gear,
			shifted_gear,
			strict_mask,
			relaxed_mask,
		})
	}

	/// Returns every parameter that determines the chunk boundaries.
	pub fn options(&self) -> ChunkerOptions {
		self.options
	}

	/// Iterates over borrowed chunks of one complete input, without allocation.
	///
	/// Each call begins a separate input at offset zero. Do not call this method
	/// separately on transport fragments that belong to the same input.
	pub fn chunks<'c, 'a>(&'c self, bytes: &'a [u8]) -> Chunks<'c, 'a> {
		Chunks {
			chunker: self,
			state: if bytes.is_empty() {
				SliceChunkState::Finished
			} else {
				SliceChunkState::Reading {
					remaining: bytes,
					offset: 0,
				}
			},
		}
	}

	/// Creates a reader with one fallibly allocated maximum-chunk-size buffer.
	///
	/// The reader returns borrowed chunks and imposes the explicit total input
	/// bound independently of the chunk-size bounds.
	pub fn reader<R: std::io::Read>(
		&self,
		reader: R,
		options: ReadOptions,
	) -> Result<ChunkReader<'_, R>, ReadError> {
		ChunkReader::new(self, reader, options)
	}

	fn boundary(&self, bytes: &[u8]) -> usize {
		v2020::cut_gear(
			bytes,
			self.options.min_size,
			self.options.target_size,
			self.options.max_size,
			self.strict_mask,
			self.relaxed_mask,
			self.strict_mask << 1,
			self.relaxed_mask << 1,
			&self.gear,
			&self.shifted_gear,
		)
		.1
	}
}

/// A borrowed chunk and its byte offset from the start of this input.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct Chunk<'a> {
	/// The first byte's zero-based offset in the input.
	pub offset: u64,
	/// The complete nonempty chunk contents.
	pub bytes: &'a [u8],
}
impl fmt::Debug for Chunk<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Chunk")
			.field("offset", &self.offset)
			.field("size", &self.bytes.len())
			.finish()
	}
}

/// Allocation-free iteration over one complete byte slice.
pub struct Chunks<'c, 'a> {
	chunker: &'c Chunker,
	state: SliceChunkState<'a>,
}

enum SliceChunkState<'a> {
	Reading { remaining: &'a [u8], offset: u64 },
	Finished,
}
impl<'a> Iterator for Chunks<'_, 'a> {
	type Item = Chunk<'a>;
	fn next(&mut self) -> Option<Self::Item> {
		let SliceChunkState::Reading { remaining, offset } = self.state else {
			return None;
		};
		let size = self.chunker.boundary(remaining);
		let (bytes, remaining) = remaining.split_at(size);
		let chunk = Chunk { offset, bytes };
		self.state = if remaining.is_empty() {
			SliceChunkState::Finished
		} else {
			SliceChunkState::Reading {
				remaining,
				offset: offset + size as u64,
			}
		};
		Some(chunk)
	}
}
impl FusedIterator for Chunks<'_, '_> {}
