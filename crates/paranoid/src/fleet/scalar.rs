//! Actual immutable scalar code and injective resource identities, without admission authority.

use std::{fmt, sync::Arc};

use crate::installation::contract::{ComponentPolicy, StateContractError};
use crate::records::RecordKey;
use crate::runtime::participation::state::ParticipantComponentSupport;
use crate::upgrade::policy::CandidateComponentPolicy;

use super::cache::epoch_state::CacheEpochComponent;
use super::once::state::OnceCompletionComponent;
use super::scalar_codec::ScalarStateError;
use super::topic::state::TopicMetadataComponent;
use super::v10::scalar_source_bridge::ScalarFleetSourceBridge;
use super::v10::source_state_keys::FleetSourceKeyKind;
use super::{CoalescingCacheKey, OnceKey, SubscriptionKey, TopicKey};
use primitives_rs::bytes::append_length_prefixed;

/// Retained built-in Once, cache-epoch, Topic metadata and source-reader code.
///
/// The same actual codec owners are reused by installation, handles and target binding.
/// Construction installs no storage, selects no policy and activates no runtime use.
/// Application CacheEntry and TopicEvent payload codecs are declared separately.
pub struct FleetScalarCode {
	pub(crate) once: Arc<OnceCompletionComponent>,
	pub(crate) cache_epoch: Arc<CacheEpochComponent>,
	pub(crate) topic: Arc<TopicMetadataComponent>,
	pub(crate) source: Arc<ScalarFleetSourceBridge>,
}
/// Initial storage origin selects source support independently from active runtime use.
pub(crate) enum FleetScalarInitialOrigin {
	NativeFresh,
	ImportedCoexisting,
}
pub(crate) struct FleetScalarInitialPolicyFields {
	pub(crate) origin: FleetScalarInitialOrigin,
}
impl FleetScalarCode {
	/// Retains the library's actual built-in scalar and source-reader registries.
	///
	/// This constructs code only; storage identities, installation choices and active uses
	/// remain explicit inputs to their owning installation and runtime APIs.
	#[allow(
		clippy::new_without_default,
		reason = "fixed built-in code has one explicit constructor; installation and runtime choices are supplied separately"
	)]
	pub fn new() -> Self {
		Self {
			once: Arc::new(OnceCompletionComponent::new()),
			cache_epoch: Arc::new(CacheEpochComponent::new()),
			topic: Arc::new(TopicMetadataComponent::new()),
			source: Arc::new(ScalarFleetSourceBridge::new()),
		}
	}
	/// Uses the same retained actual owners for every initial scalar component.
	pub(crate) fn initial_policies(
		&self,
		fields: FleetScalarInitialPolicyFields,
	) -> Result<Vec<ComponentPolicy>, NativeFleetScalarCodeError> {
		let mut policies = vec![
			self.once.initial_policy()?,
			self.cache_epoch.initial_policy()?,
			self.topic.initial_policy()?,
		];
		match fields.origin {
			FleetScalarInitialOrigin::NativeFresh => {}
			FleetScalarInitialOrigin::ImportedCoexisting => {
				policies.push(self.source.initial_policy()?)
			}
		}
		Ok(policies)
	}
	pub(crate) fn support(&self) -> [ParticipantComponentSupport; 4] {
		[
			ParticipantComponentSupport::from_declaration(&self.once.declaration()),
			ParticipantComponentSupport::from_declaration(&self.cache_epoch.declaration()),
			ParticipantComponentSupport::from_declaration(&self.topic.declaration()),
			ParticipantComponentSupport::from_declaration(&self.source.declaration()),
		]
	}

	/// A known policy binds actual callable code; this establishes no current policy or IO right.
	pub(crate) fn candidate(
		&self,
		policy: &ComponentPolicy,
	) -> Result<Option<CandidateComponentPolicy>, NativeFleetScalarCodeError> {
		let once = self.once.declaration();
		let epoch = self.cache_epoch.declaration();
		let topic = self.topic.declaration();
		let source = self.source.declaration();
		let candidate = if policy.component_identifier() == once.identifier() {
			let bound = once.bind(policy)?;
			self.once.bind_schema(&bound)?;
			CandidateComponentPolicy::from_bound(&bound)
		} else if policy.component_identifier() == epoch.identifier() {
			let bound = epoch.bind(policy)?;
			self.cache_epoch.bind_schema(&bound)?;
			CandidateComponentPolicy::from_bound(&bound)
		} else if policy.component_identifier() == topic.identifier() {
			let bound = topic.bind(policy)?;
			self.topic.bind_sequence(&bound)?;
			self.topic.bind_cursor(&bound)?;
			CandidateComponentPolicy::from_bound(&bound)
		} else if policy.component_identifier() == source.identifier() {
			let bound = source.bind(policy)?;
			self.source.bind(&bound)?;
			CandidateComponentPolicy::from_bound(&bound)
		} else {
			return Ok(None);
		};
		Ok(Some(candidate))
	}
}
impl fmt::Debug for FleetScalarCode {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("FleetScalarCode { .. }")
	}
}

/// Native key construction uses complete parsed resource names rather than source prefixes.
pub(crate) enum NativeFleetScalarResource<'a> {
	Once(&'a OnceKey),
	CacheEpoch(&'a CoalescingCacheKey),
	TopicSequence(&'a TopicKey),
	SubscriptionCursor {
		topic: &'a TopicKey,
		subscription: &'a SubscriptionKey,
	},
}
impl<'a> NativeFleetScalarResource<'a> {
	/// Uses the actual retained source constructors and the recorded imported Fleet root.
	pub(crate) fn source_key(
		&self,
		root: &super::RootKey,
	) -> Result<crate::kv::Key, crate::kv::KeyError> {
		use super::namespace_keys::{
			build_coalescing_cache_epoch_prefix, build_once_completion_prefix,
			build_subscription_cursor_prefix, build_topic_sequence_prefix,
		};
		let (prefix, suffix) = match self {
			Self::Once(key) => (
				build_once_completion_prefix(root, key)?,
				Some(super::constants::FLEET_ONCE_DONE_KEY_PART),
			),
			Self::CacheEpoch(key) => (build_coalescing_cache_epoch_prefix(root, key)?, None),
			Self::TopicSequence(key) => (build_topic_sequence_prefix(root, key)?, None),
			Self::SubscriptionCursor {
				topic,
				subscription,
			} => (
				build_subscription_cursor_prefix(root, topic, subscription)?,
				None,
			),
		};
		crate::kv::Key::from_prefix_and_parts(&prefix, suffix)
	}
	pub(crate) fn from_source(kind: &'a FleetSourceKeyKind) -> Option<Self> {
		Some(match kind {
			FleetSourceKeyKind::OnceCompletion(key) => Self::Once(key),
			FleetSourceKeyKind::CacheEpoch(key) => Self::CacheEpoch(key),
			FleetSourceKeyKind::TopicSequence(key) => Self::TopicSequence(key),
			FleetSourceKeyKind::SubscriptionCursor {
				topic,
				subscription,
			} => Self::SubscriptionCursor {
				topic,
				subscription,
			},
			_ => return None,
		})
	}
	pub(crate) fn record_key(&self) -> RecordKey {
		let mut bytes = Vec::new();
		append_length_prefixed(&mut bytes, b"paranoid.fleet.scalar-resource.v1");
		match self {
			Self::Once(key) => {
				append_length_prefixed(&mut bytes, b"once-completion");
				append_length_prefixed(&mut bytes, key.as_str().as_bytes());
			}
			Self::CacheEpoch(key) => {
				append_length_prefixed(&mut bytes, b"cache-epoch");
				append_length_prefixed(&mut bytes, key.as_str().as_bytes());
			}
			Self::TopicSequence(key) => {
				append_length_prefixed(&mut bytes, b"topic-sequence");
				append_length_prefixed(&mut bytes, key.as_str().as_bytes());
			}
			Self::SubscriptionCursor {
				topic,
				subscription,
			} => {
				append_length_prefixed(&mut bytes, b"subscription-cursor");
				append_length_prefixed(&mut bytes, topic.as_str().as_bytes());
				append_length_prefixed(&mut bytes, subscription.as_str().as_bytes());
			}
		}
		RecordKey::new(bytes)
	}
}
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeFleetScalarCodeError {
	#[error("Fleet scalar policy cannot bind its declaration")]
	Contract(#[from] StateContractError),
	#[error("Fleet scalar policy cannot bind its native schema or source bridge")]
	State(#[from] ScalarStateError),
}
