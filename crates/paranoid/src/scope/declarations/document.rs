//! The roles that a document scope's code supports, and the roles that a runtime uses when it
//! starts.

use std::sync::Arc;

use crate::kv::document::schema::IndexContractIdentity;
use crate::kv::document::schema::managed::{TypedManagedDocument, TypedManagedDocumentFields};
use crate::runtime::participation::state::catalog::ParticipantUseReference;
use crate::scope::{DocumentRole, DocumentRoleSupport, ScopeIdentity};

use super::{
	AppDeclarationError, AppDeclarationInnerError, AppDocumentDeclarationFields, DocumentUseIntent,
	InitialUse,
};

/// One document scope that the app declares.
pub struct AppDocumentDeclaration {
	pub(super) scope: ScopeIdentity,
	pub(super) schema_identifier: String,
	pub(super) support: Vec<DocumentRoleSupport>,
	pub(super) initial_uses: Vec<ParticipantUseReference>,
	pub(super) managed: crate::kv::document::schema::managed::ManagedDocumentRegistration,
	/// The indexes that this declaration queries, which an upgrade makes query-ready.
	pub(super) query_indexes: Vec<IndexContractIdentity>,
}
impl AppDocumentDeclaration {
	/// Declares a document scope without database work. It fails when the code cannot provide
	/// a role that the intent marks as supported, or when the intent repeats a query.
	pub fn new<T, C, P>(
		fields: AppDocumentDeclarationFields<T, C, P>,
	) -> Result<Self, AppDeclarationError>
	where
		crate::kv::document::schema::owned::DocumentCode<T, C, P>: Send + Sync,
		T: 'static,
		C: Send + Sync + 'static,
		P: Send + Sync + 'static,
	{
		let registration = TypedManagedDocument::new(TypedManagedDocumentFields {
			scope: fields.scope.identity().clone(),
			code: Arc::clone(fields.scope.code()),
		})
		.into_registration();
		let query_indexes = fields
			.intent
			.queries
			.iter()
			.map(|query| query.index.clone())
			.collect();
		let scope = registration.scope().clone();
		let schema = registration.schema_identifier().to_owned();
		let derived = derive_document_support(DeriveDocumentSupportFields {
			scope: &scope,
			intent: fields.intent,
			role_support_fn: |role| {
				registration.support(role).map_err(|error| {
					AppDeclarationError::at_scope(
						&scope,
						AppDeclarationInnerError::DocumentDeclaration(error),
					)
				})
			},
			query_support_fn: |indexes: &[IndexContractIdentity]| {
				registration
					.support_reader_queries(indexes)
					.map_err(|error| {
						AppDeclarationError::at_scope(
							&scope,
							AppDeclarationInnerError::DocumentDeclaration(error),
						)
					})
			},
		})?;
		Ok(Self {
			scope,
			schema_identifier: schema,
			support: derived.support,
			initial_uses: derived.initial_uses,
			managed: registration,
			query_indexes,
		})
	}
}

struct DerivedDocumentSupport {
	support: Vec<DocumentRoleSupport>,
	initial_uses: Vec<ParticipantUseReference>,
}
struct DeriveDocumentSupportFields<'a, R, Q> {
	scope: &'a ScopeIdentity,
	intent: DocumentUseIntent,
	role_support_fn: R,
	query_support_fn: Q,
}
fn derive_document_support<R, Q>(
	fields: DeriveDocumentSupportFields<'_, R, Q>,
) -> Result<DerivedDocumentSupport, AppDeclarationError>
where
	R: FnMut(DocumentRole) -> Result<DocumentRoleSupport, AppDeclarationError>,
	Q: FnOnce(&[IndexContractIdentity]) -> Result<DocumentRoleSupport, AppDeclarationError>,
{
	let DeriveDocumentSupportFields {
		scope,
		intent,
		mut role_support_fn,
		query_support_fn,
	} = fields;
	for (index, query) in intent.queries.iter().enumerate() {
		if intent.queries[..index]
			.iter()
			.any(|prior| prior.index == query.index)
		{
			return Err(AppDeclarationError::at_scope(
				scope,
				AppDeclarationInnerError::DuplicateQuery { index },
			));
		}
	}
	let mut support = Vec::new();
	let mut initial_uses = Vec::new();
	if !intent.queries.is_empty() {
		let indexes = intent
			.queries
			.iter()
			.map(|query| query.index.clone())
			.collect::<Vec<_>>();
		support.push(query_support_fn(&indexes)?);
	} else if intent.point_reader.is_supported() {
		support.push(role_support_fn(DocumentRole::Reader)?);
	}
	if intent.producer.is_supported() {
		support.push(role_support_fn(DocumentRole::Producer)?);
	}
	if intent.maintenance.is_supported() {
		support.push(role_support_fn(DocumentRole::Maintenance)?);
	}
	for (role, requested) in [
		(DocumentRole::Reader, intent.point_reader.initial_use()),
		(DocumentRole::Producer, intent.producer.initial_use()),
		(DocumentRole::Maintenance, intent.maintenance.initial_use()),
	] {
		if requested == Some(InitialUse::Active) {
			initial_uses.push(ParticipantUseReference::Document {
				scope: scope.clone(),
				role,
			});
		}
	}
	for query in intent.queries {
		if query.initial_use == InitialUse::Active {
			initial_uses.push(ParticipantUseReference::DocumentQuery {
				scope: scope.clone(),
				index: query.index,
			});
		}
	}
	Ok(DerivedDocumentSupport {
		support,
		initial_uses,
	})
}
