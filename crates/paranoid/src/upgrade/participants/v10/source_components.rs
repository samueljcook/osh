//! Fixed physical-import component withdrawal after actual durable source-data retirement.

use crate::fleet::scalar_codec::ScalarStateError;
use crate::installation::contract::StateContractError;
use crate::queue::QueueSourceImportBindingError;

use super::super::NativeUpgradeParticipantsError;

pub(in crate::upgrade::participants) mod preparation;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceComponentWithdrawalError {
	#[error("source component withdrawal requires the NativeImported route")]
	NativeImportedRequired,
	#[error("source component withdrawal cannot omit an unsupported current component")]
	UnsupportedOmission { component: String },
	#[error("source component withdrawal needs importer code")]
	CodeMissing { component: String },
	#[error("source component withdrawal requires the current and omitted Queue source pair")]
	QueuePairRequired,
	#[error("source component withdrawal selected component binding failed")]
	Contract {
		component: String,
		#[source]
		source: StateContractError,
	},
	#[error("source component withdrawal Queue source definition binding failed")]
	Queue(#[source] Box<QueueSourceImportBindingError>),
	#[error("source component withdrawal scalar source binding failed")]
	Scalar(#[source] ScalarStateError),
}
impl From<SourceComponentWithdrawalError> for NativeUpgradeParticipantsError {
	fn from(source: SourceComponentWithdrawalError) -> Self {
		Self::SourceComponents(Box::new(source))
	}
}
