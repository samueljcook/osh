//! Verified source placement retained until startup observes its current admitted route.

use crate::installation::creation::v10::recorded_source::PgRecordedImportedInstallation;
use crate::queue::v10::source_group_storage::PgQueueSourceGroupStorage;
use crate::queue::{QueueSourceImportDefs, engine::NativeQueueSourceBindings};
use std::sync::Arc;

/// This value retains physical verification, without requiring source interpretation before
/// startup observes the current route. No source IO or bridge policy is inferred here.
pub(crate) struct NativeQueueSourcePreparation {
	storage: PgQueueSourceGroupStorage,
	imports: Arc<QueueSourceImportDefs>,
}
impl NativeQueueSourcePreparation {
	pub(in crate::runtime::v10) fn bind(self) -> Arc<NativeQueueSourceBindings> {
		Arc::new(NativeQueueSourceBindings::new(self.storage, self.imports))
	}
}

/// Returns `None` when the installation has no v10 Queue storage to read, for example because
/// it had retired its v10 data before it was opened.
pub(crate) fn prepare_recorded_queue_source(
	installation: &PgRecordedImportedInstallation,
	imports: Arc<QueueSourceImportDefs>,
) -> Option<NativeQueueSourcePreparation> {
	PgQueueSourceGroupStorage::from_verified_recorded(installation)
		.map(|storage| NativeQueueSourcePreparation { storage, imports })
}
