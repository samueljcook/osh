//! Public IDs with explicit generation and representation contracts.
mod random_generation;
/// Random text and word generation failures.
pub use random_generation::RandomIdError;
pub mod time_sortable;
/// Uniformly sampled text IDs from an explicit ASCII alphabet.
pub mod random_text {
	pub use super::random_generation::{
		RandomTextIdAlphabet, RandomTextIdAlphabetError, RandomTextIdOptions,
		generate_random_text_id,
	};
}
/// Uniformly sampled, hyphen-separated English-word IDs.
pub mod random_words {
	pub use super::random_generation::{RandomWordIdOptions, generate_random_word_id};
}
