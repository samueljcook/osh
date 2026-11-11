//! Circuit lifecycle owns its recovery deadline, epoch, and private probe reservation.

use super::{
	AcquisitionCapability, CircuitState, PrivateProbe, StateTimestampMicros, ThrottlerCodecError,
};

#[derive(Clone)]
pub(super) enum CircuitActivity {
	Closed {
		authority: IdleCircuitAuthority,
	},
	Open {
		recovery: RecoveryWindow,
		authority: OpenCircuitAuthority,
	},
}

#[derive(Clone)]
pub(super) enum RecoveryWindow {
	Ready,
	OpenedAt(StateTimestampMicros),
}

#[derive(Clone)]
pub(super) enum IdleCircuitAuthority {
	Uninitialized,
	Native(AcquisitionCapability),
}

#[derive(Clone)]
pub(super) enum OpenCircuitAuthority {
	Uninitialized,
	NativeIdle(AcquisitionCapability),
	NativeProbe {
		epoch: AcquisitionCapability,
		probe: PrivateProbe,
	},
}

pub(super) enum CircuitOwnershipView<'a> {
	Uninitialized,
	NativeIdle(&'a AcquisitionCapability),
	NativeProbe {
		epoch: &'a AcquisitionCapability,
		probe: &'a PrivateProbe,
	},
}

impl IdleCircuitAuthority {
	fn into_open(self) -> OpenCircuitAuthority {
		match self {
			Self::Uninitialized => OpenCircuitAuthority::Uninitialized,
			Self::Native(epoch) => OpenCircuitAuthority::NativeIdle(epoch),
		}
	}
}

impl OpenCircuitAuthority {
	fn into_idle(self) -> IdleCircuitAuthority {
		match self {
			Self::Uninitialized => IdleCircuitAuthority::Uninitialized,
			Self::NativeIdle(epoch) | Self::NativeProbe { epoch, .. } => {
				IdleCircuitAuthority::Native(epoch)
			}
		}
	}
}

impl CircuitActivity {
	pub(super) fn closed_uninitialized() -> Self {
		Self::Closed {
			authority: IdleCircuitAuthority::Uninitialized,
		}
	}

	pub(super) fn uninitialized(
		state: CircuitState,
		opening: Option<StateTimestampMicros>,
	) -> Result<Self, ThrottlerCodecError> {
		Self::from_idle(state, opening, IdleCircuitAuthority::Uninitialized)
	}

	pub(super) fn native(
		state: CircuitState,
		opening: Option<StateTimestampMicros>,
		epoch: AcquisitionCapability,
		probe: Option<PrivateProbe>,
	) -> Result<Self, ThrottlerCodecError> {
		match (state, probe) {
			(CircuitState::Closed, Some(_)) => Err(ThrottlerCodecError::Invalid {
				reason: "private probe requires an open circuit",
			}),
			(CircuitState::Open, Some(probe)) => Ok(Self::Open {
				recovery: Self::recovery(opening),
				authority: OpenCircuitAuthority::NativeProbe { epoch, probe },
			}),
			(state, None) => Self::from_idle(state, opening, IdleCircuitAuthority::Native(epoch)),
		}
	}

	fn from_idle(
		state: CircuitState,
		opening: Option<StateTimestampMicros>,
		authority: IdleCircuitAuthority,
	) -> Result<Self, ThrottlerCodecError> {
		match (state, opening) {
			(CircuitState::Closed, Some(_)) => Err(ThrottlerCodecError::Invalid {
				reason: "closed circuit must not retain an opening timestamp",
			}),
			(CircuitState::Closed, None) => Ok(Self::Closed { authority }),
			(CircuitState::Open, opening) => Ok(Self::Open {
				recovery: Self::recovery(opening),
				authority: authority.into_open(),
			}),
		}
	}

	fn recovery(opening: Option<StateTimestampMicros>) -> RecoveryWindow {
		match opening {
			None => RecoveryWindow::Ready,
			Some(opened_at) => RecoveryWindow::OpenedAt(opened_at),
		}
	}

	pub(super) fn state(&self) -> CircuitState {
		match self {
			Self::Closed { .. } => CircuitState::Closed,
			Self::Open { .. } => CircuitState::Open,
		}
	}

	pub(super) fn opening(&self) -> Option<StateTimestampMicros> {
		match self {
			Self::Closed { .. }
			| Self::Open {
				recovery: RecoveryWindow::Ready,
				..
			} => None,
			Self::Open {
				recovery: RecoveryWindow::OpenedAt(opened_at),
				..
			} => Some(*opened_at),
		}
	}

	pub(super) fn ownership(&self) -> CircuitOwnershipView<'_> {
		match self {
			Self::Closed { authority } => match authority {
				IdleCircuitAuthority::Uninitialized => CircuitOwnershipView::Uninitialized,
				IdleCircuitAuthority::Native(epoch) => CircuitOwnershipView::NativeIdle(epoch),
			},
			Self::Open { authority, .. } => match authority {
				OpenCircuitAuthority::Uninitialized => CircuitOwnershipView::Uninitialized,
				OpenCircuitAuthority::NativeIdle(epoch) => CircuitOwnershipView::NativeIdle(epoch),
				OpenCircuitAuthority::NativeProbe { epoch, probe } => {
					CircuitOwnershipView::NativeProbe { epoch, probe }
				}
			},
		}
	}

	pub(super) fn native_epoch(&self) -> Option<&AcquisitionCapability> {
		match self.ownership() {
			CircuitOwnershipView::NativeIdle(epoch)
			| CircuitOwnershipView::NativeProbe { epoch, .. } => Some(epoch),
			CircuitOwnershipView::Uninitialized => None,
		}
	}

	pub(super) fn native_probe(&self) -> Option<(&AcquisitionCapability, &PrivateProbe)> {
		match self {
			Self::Open {
				authority: OpenCircuitAuthority::NativeProbe { epoch, probe },
				..
			} => Some((epoch, probe)),
			_ => None,
		}
	}

	pub(super) fn native_probe_mut(&mut self) -> Option<&mut PrivateProbe> {
		match self {
			Self::Open {
				authority: OpenCircuitAuthority::NativeProbe { probe, .. },
				..
			} => Some(probe),
			_ => None,
		}
	}

	pub(super) fn retain_native_epoch(&mut self, epoch: AcquisitionCapability) {
		match self {
			Self::Closed { authority } => *authority = IdleCircuitAuthority::Native(epoch),
			Self::Open { authority, .. } => *authority = OpenCircuitAuthority::NativeIdle(epoch),
		}
	}

	pub(super) fn reserve_probe(&mut self, epoch: AcquisitionCapability, probe: PrivateProbe) {
		let Self::Open { authority, .. } = self else {
			unreachable!("only the open circuit gate admits a probe");
		};
		*authority = OpenCircuitAuthority::NativeProbe { epoch, probe };
	}

	pub(super) fn open(&mut self, now: StateTimestampMicros) {
		let prior = std::mem::replace(self, Self::closed_uninitialized());
		let authority = match prior {
			Self::Closed { authority } => authority.into_open(),
			Self::Open { authority, .. } => authority,
		};
		*self = Self::Open {
			recovery: RecoveryWindow::OpenedAt(now),
			authority,
		};
	}

	pub(super) fn close(&mut self) {
		let prior = std::mem::replace(self, Self::closed_uninitialized());
		let authority = match prior {
			Self::Closed { authority } => authority,
			Self::Open { authority, .. } => authority.into_idle(),
		};
		*self = Self::Closed { authority };
	}

	pub(super) fn reserved_until(&self) -> Option<StateTimestampMicros> {
		match self {
			Self::Open {
				authority: OpenCircuitAuthority::NativeProbe { probe, .. },
				..
			} => Some(probe.expires_at),
			_ => None,
		}
	}
}
