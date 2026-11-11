//! Layout-specific initial policies of the v10 Queue source readers, without source authority.

use super::{QueueSourceImportBindingError, QueueSourceImportDefs};
use crate::installation::contract::{ComponentPolicy, StateContractError};
use crate::queue::PublishedV10QueueLayout;

/// Borrows the reader registry that an imported installation records, without source
/// observation.
pub(crate) struct QueueInitialSourceSelection<'registry> {
	defs: &'registry QueueSourceImportDefs,
}

pub(crate) struct QueueInitialSourcePoliciesFields {
	pub(crate) layout: PublishedV10QueueLayout,
}

/// Named policy parts are comparison metadata, without source admission or persistence rights.
pub(crate) struct QueueInitialSourcePolicies {
	pub(crate) jobs: ComponentPolicy,
	pub(crate) auxiliary: ComponentPolicy,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueueInitialSourcePolicyError {
	#[error("initial Queue source policy does not bind its declarations")]
	Contract(#[from] StateContractError),
	#[error("initial Queue source policies do not select the layout-compatible pair")]
	Binding(#[from] QueueSourceImportBindingError),
}

impl QueueSourceImportDefs {
	pub(crate) fn initial_source_selection(&self) -> QueueInitialSourceSelection<'_> {
		QueueInitialSourceSelection { defs: self }
	}
}

impl<'registry> QueueInitialSourceSelection<'registry> {
	/// Borrows the same retained registry for later actual opening validation.
	pub(crate) fn defs(&self) -> &'registry QueueSourceImportDefs {
		self.defs
	}

	/// Derives and checks the selected pair against supplied observed layout comparison data.
	/// The owning imported opener must supply its actual validated layout before persistence.
	pub(crate) fn initial_policies(
		&self,
		fields: QueueInitialSourcePoliciesFields,
	) -> Result<QueueInitialSourcePolicies, QueueInitialSourcePolicyError> {
		let jobs = self.defs.def.jobs.initial_policy(fields.layout)?;
		let auxiliary = self.defs.def.auxiliary.initial_policy()?;
		let job_declaration = self.defs.job_declaration();
		let auxiliary_declaration = self.defs.auxiliary_declaration();
		self.defs.require_bound_pair(
			&job_declaration.bind(&jobs)?,
			&auxiliary_declaration.bind(&auxiliary)?,
			fields.layout,
		)?;
		Ok(QueueInitialSourcePolicies { jobs, auxiliary })
	}
}
