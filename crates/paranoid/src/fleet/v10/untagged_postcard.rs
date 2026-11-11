//! Actual published Postcard readers for the state that pre.9 and pre.10 stored.
//!
//! Whole values retain the dependency's unused-suffix and collection semantics. The Topic
//! timestamp split returns its exact business payload remainder. No reader selects an
//! application schema or supplies native mutation authority.

use std::fmt;

use serde::de::DeserializeOwned;

use super::state_formats::OnceCompletion;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UntaggedFamily {
	Counter,
	CacheEpoch,
	Once,
	TopicSequence,
	SubscriptionCursor,
}

impl UntaggedFamily {
	fn name(self) -> &'static str {
		match self {
			Self::Counter => "fleet-counter",
			Self::CacheEpoch => "fleet-cache-epoch",
			Self::Once => "fleet-once",

			Self::TopicSequence => "fleet-topic-sequence",
			Self::SubscriptionCursor => "fleet-topic-cursor",
		}
	}
}

/// The actual family and dependency cause contain no stored body or fabricated byte offset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UntaggedPostcardError {
	family: UntaggedFamily,
	source: postcard::Error,
}

impl fmt::Display for UntaggedPostcardError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "invalid published {} Postcard value", self.family.name())
	}
}

impl std::error::Error for UntaggedPostcardError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&self.source)
	}
}

type Decoded<T> = Result<T, UntaggedPostcardError>;

fn decode_published<T: DeserializeOwned>(bytes: &[u8], family: UntaggedFamily) -> Decoded<T> {
	postcard::from_bytes(bytes).map_err(|source| UntaggedPostcardError { family, source })
}

pub(super) fn decode_untagged_counter(bytes: &[u8]) -> Decoded<i64> {
	decode_published(bytes, UntaggedFamily::Counter)
}

pub(super) fn decode_untagged_cache_epoch(bytes: &[u8]) -> Decoded<i64> {
	decode_published(bytes, UntaggedFamily::CacheEpoch)
}

pub(super) fn decode_untagged_topic_sequence(bytes: &[u8]) -> Decoded<i64> {
	decode_published(bytes, UntaggedFamily::TopicSequence)
}

pub(super) fn decode_untagged_subscription_cursor(bytes: &[u8]) -> Decoded<i64> {
	decode_published(bytes, UntaggedFamily::SubscriptionCursor)
}

pub(super) fn decode_untagged_once_completion(bytes: &[u8]) -> Decoded<OnceCompletion> {
	decode_published(bytes, UntaggedFamily::Once)
}
