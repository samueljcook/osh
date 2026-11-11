//! Exact phase-obsolete source bridge use reconciliation inside ordinary participant renewal.
//!
//! The parent supplies the route from its freshly pinned and verified control observation,
//! actual selected administrative binding, and canonical live participant authentication.
//! No source phase, control policy, role enrollment, or arbitrary use change is exposed.

use crate::installation::contract::{BoundComponentContract, BoundStateSchema};
use crate::records::StateTimestampMicros;
use crate::runtime::participation::state::{
	ParticipantAuthority, ParticipantState, ParticipationSemantics,
};

use super::NativeParticipationError;
use crate::runtime::admission::bindings::InstallationStorageRoute;

pub(super) fn reconcile_retired_source_uses(
	state: &mut ParticipantState,
	authority: &ParticipantAuthority,
	route: InstallationStorageRoute,
	administration: &BoundComponentContract<'_, '_, ParticipationSemantics>,
	schema: &BoundStateSchema<
		'_,
		ParticipantState,
		crate::runtime::participation::state::ParticipantStateError,
	>,
	observed_at: StateTimestampMicros,
) -> Result<(), NativeParticipationError> {
	if route == InstallationStorageRoute::NativeImported {
		administration.transitions().reconcile_retired_source_uses(
			state,
			authority,
			schema,
			observed_at,
		)?;
	}
	Ok(())
}
