use super::{Claim, FencingToken, HolderId, LeaseKey, Token, state::LeaseAcquisition};

/// Sealed exact cleanup material. It cannot become usable claim authority.
pub(crate) struct LeaseCleanupTarget {
	key: LeaseKey,
	holder_id: HolderId,
	authority: CleanupAuthority,
}

enum CleanupAuthority {
	UnreturnedAcquisition(Token),
	Confirmed {
		fence: FencingToken,
		tokens: CleanupTokens,
	},
}

enum CleanupTokens {
	Single(Token),
	Rotation { prior: Token, proposed: Token },
}

impl LeaseCleanupTarget {
	/// Records actual generated proposal material before any acquisition statement.
	pub(super) fn acquisition(key: &LeaseKey, proposal: &LeaseAcquisition) -> Self {
		Self {
			key: key.clone(),
			holder_id: proposal.holder_id.clone(),
			authority: CleanupAuthority::UnreturnedAcquisition(proposal.lease_token.clone()),
		}
	}

	pub(super) fn confirmed(claim: &Claim) -> Self {
		Self {
			key: claim.key.clone(),
			holder_id: claim.holder_id.clone(),
			authority: CleanupAuthority::Confirmed {
				fence: claim.fencing_token,
				tokens: CleanupTokens::Single(claim.lease_token.clone()),
			},
		}
	}

	/// Retains both sides of one actual tenure's proposed renewal before its SQL dispatch.
	pub(super) fn rotation(claim: &Claim, proposed: &Token) -> Self {
		Self {
			key: claim.key.clone(),
			holder_id: claim.holder_id.clone(),
			authority: CleanupAuthority::Confirmed {
				fence: claim.fencing_token,
				tokens: CleanupTokens::Rotation {
					prior: claim.lease_token.clone(),
					proposed: proposed.clone(),
				},
			},
		}
	}

	pub(super) fn key(&self) -> &LeaseKey {
		&self.key
	}
	pub(super) fn holder_id(&self) -> &HolderId {
		&self.holder_id
	}
	pub(super) fn fencing_token(&self) -> Option<FencingToken> {
		match self.authority {
			CleanupAuthority::UnreturnedAcquisition(_) => None,
			CleanupAuthority::Confirmed { fence, .. } => Some(fence),
		}
	}
	pub(super) fn first_token(&self) -> &Token {
		match &self.authority {
			CleanupAuthority::UnreturnedAcquisition(token)
			| CleanupAuthority::Confirmed {
				tokens: CleanupTokens::Single(token),
				..
			} => token,
			CleanupAuthority::Confirmed {
				tokens: CleanupTokens::Rotation { prior, .. },
				..
			} => prior,
		}
	}
	pub(super) fn second_token(&self) -> Option<&Token> {
		match &self.authority {
			CleanupAuthority::UnreturnedAcquisition(_)
			| CleanupAuthority::Confirmed {
				tokens: CleanupTokens::Single(_),
				..
			} => None,
			CleanupAuthority::Confirmed {
				tokens: CleanupTokens::Rotation { proposed, .. },
				..
			} => Some(proposed),
		}
	}

	pub(super) fn matches(
		&self,
		key: &LeaseKey,
		holder: &HolderId,
		fence: FencingToken,
		token: &Token,
	) -> bool {
		let mut token_matches = self.first_token() == token;
		if let Some(second) = self.second_token() {
			token_matches |= second == token;
		}
		key == &self.key
			&& holder == &self.holder_id
			&& self
				.fencing_token()
				.is_none_or(|expected| expected == fence)
			&& token_matches
	}
}

impl std::fmt::Debug for LeaseCleanupTarget {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("LeaseCleanupTargets")
			.field("key", &self.key)
			.field("fencing_token", &self.fencing_token())
			.finish_non_exhaustive()
	}
}
