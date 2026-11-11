//! Recorded imported reopening from the native locator and durable source descriptor.
//!
//! The provisional descriptor lookup finishes its owned transaction before canonical
//! source/control ordering begins. Data-retired validation accesses retained metadata only.

use std::{
	fmt,
	num::{NonZeroU32, NonZeroU64},
};

use primitives_rs::id::time_sortable::SortableId;

use super::super::{
	BackendOrigin, FleetScalarCode, ImportedPhase, InitialDocumentIndexes, InitialInstallationMode,
	InstallationControlComponent, InstallationControlFields, InstallationControlReader,
	InstallationControlState, NativeInstallationError, NativeMappingOrigin, NativeSourceGatePhase,
	ParticipantSchema, PgNativeAdministration, PgNativeStorageBindings, PgV10SourceDescriptor,
	PgV10SourceLocation, QueueRecoveryPolicy, RecordMetadata, RecordedInstallationIdentity,
	ScopeSelections, StateStorageEnvelope, UnexpectedV10Protocol, WritePool,
	acquire_transaction_lock, check_initial_policies, decode_recorded_control,
	descriptor_registration, initial_policy, recorded_imported_descriptor_format_only,
	run_transaction_read, run_transaction_write,
};
use crate::db::{DatabaseOperationKind, Tx, owned_portable_query_as};
use crate::queue::{QueueInitialSourcePoliciesFields, QueueInitialSourceSelection};

const DATABASE_ENCODING_LABEL: &str = "db.native_installation.read_database_encoding";
const DATABASE_ENCODING_SQL: &str = "SELECT pg_catalog.getdatabaseencoding()::pg_catalog.text";

/// The database encodings in which Paranoid can import and read v10 tables.
///
/// The v10 KV, Fleet state, and lease tables store keys as `text` with the collation "C" or
/// "POSIX", which orders keys by the bytes that the database stores. Moving the v10 records
/// pages through the keys in that order and requires their UTF-8 bytes to increase, and
/// Paranoid connects with `client_encoding = 'UTF8'`. Only `UTF8` and `SQL_ASCII` satisfy both
/// requirements:
///
/// - In `UTF8`, the stored bytes are validated UTF-8 bytes.
/// - In `SQL_ASCII`, PostgreSQL stores the bytes that each client sends without character-set
///   conversion, and `pg_catalog.convert_to(key, 'UTF8')` copies those raw bytes without UTF-8
///   validation. Rust `str::from_utf8` validates decoded keys, so a key with bytes that are not
///   valid UTF-8 fails key decoding with an error.
///
/// `LATIN1` orders characters U+0000 through U+00FF by code point, which matches UTF-8 order on
/// that subset, but `client_encoding = 'UTF8'` causes PostgreSQL to abort statements with
/// SQLSTATE `22P05` when any parameter contains a character above U+00FF. Other single-byte
/// encodings, such as `WIN1252`, also store characters in an order that differs from UTF-8:
/// `WIN1252` stores `€` as `0x80` and `é` as `0xE9`, so `WIN1252` orders `€` before `é`, whereas
/// UTF-8 orders `é` (`C3 A9`) before `€` (`E2 82 AC`).
const UTF8_ORDERED_ENCODINGS: [&str; 2] = ["UTF8", "SQL_ASCII"];

/// Fails unless the database's encoding is one of [`UTF8_ORDERED_ENCODINGS`]. A database's
/// encoding never changes, so one check in each import and in each opening of an installation
/// that still reads v10 data covers the runtime that starts from it.
async fn require_utf8_ordered_encoding(tx: &mut Tx<'_>) -> Result<(), NativeInstallationError> {
	tx.record_database_operation(
		DatabaseOperationKind::FetchOne,
		DATABASE_ENCODING_LABEL,
		Some(DATABASE_ENCODING_SQL),
	);
	let (encoding,) = owned_portable_query_as::<(String,)>(DATABASE_ENCODING_SQL)
		.fetch_one(tx.inner.as_mut())
		.await
		.map_err(|error| error.with_operation_label(DATABASE_ENCODING_LABEL))?;
	if UTF8_ORDERED_ENCODINGS.contains(&encoding.as_str()) {
		Ok(())
	} else {
		Err(NativeInstallationError::UnsupportedDatabaseEncoding { encoding })
	}
}

/// Known-opened imported installation, without legacy Store dependencies.
/// Ordinary admission must still check the transaction-observed source gate and control phase.
pub(crate) struct PgRecordedImportedInstallation {
	source: PgV10SourceDescriptor,
	bindings: PgNativeStorageBindings,
	control_component: InstallationControlComponent,
	installation_id: SortableId,
	phase: ImportedPhase,
}

/// Explicit initial generation, code-bound coexistence policies, and point conflict budget.
pub(crate) struct PgImportedV10PreparationOptions<'code> {
	/// The document code, which builds the physical index of each query-ready index.
	pub(crate) documents: &'code crate::kv::document::schema::managed::ManagedDocumentRegistry,
	pub(crate) fleet_scalar_code: &'code FleetScalarCode,
	pub(crate) generation: NonZeroU64,
	pub(crate) scope_selections: ScopeSelections,
	pub(crate) queue_recovery: &'code QueueRecoveryPolicy,
	pub(crate) conflict_attempts: NonZeroU32,
	pub(crate) queue_source: QueueInitialSourceSelection<'code>,
	/// The v10 protocol tables other than the source that must not exist when import holds the
	/// installation lock.
	pub(crate) unexpected_v10_protocols: &'code [UnexpectedV10Protocol<'code>],
}

impl PgRecordedImportedInstallation {
	/// Validates the recorded published source, strictly creates additive native containers and
	/// the physical index of each query-ready index, and commits imported registration with
	/// coexistence policy and its additive inverse marker.
	pub(crate) async fn prepare_imported(
		control_pool: &WritePool,
		source: PgV10SourceLocation,
		bindings: PgNativeStorageBindings,
		options: PgImportedV10PreparationOptions<'_>,
	) -> Result<Self, NativeInstallationError> {
		let scope_policies = options
			.scope_selections
			.into_initial_policies()
			.map_err(initial_policy::InitialPolicyError::from)?;
		bindings.check_scope_policies(&scope_policies)?;
		source.check_native_separation(&bindings)?;
		let initial_indexes =
			InitialDocumentIndexes::new(&scope_policies, options.documents, &bindings)?;
		let installation_id = SortableId::generate().map_err(NativeInstallationError::Random)?;
		let control_component =
			InstallationControlComponent::new(bindings.control.identity().clone(), installation_id);
		let participant = ParticipantSchema::new();
		let declaration = PgNativeAdministration::declaration_from_schemas(
			&participant,
			&control_component,
			bindings.backend_schema(),
		);
		let mut component_policies =
			initial_policy::component_policies(initial_policy::InitialComponentPolicyFields {
				participant: &participant,
				control: &control_component,
				backend: bindings.backend_schema(),
				scalar_code: options.fleet_scalar_code,
				queue_recovery: options.queue_recovery,
				origin: NativeMappingOrigin::ImportedV10,
			})?;
		let queue_imports = options.queue_source.defs();
		let descriptor = run_transaction_write(
			control_pool,
			"db.native_installation.prepare_imported",
			async |tx| {
				acquire_transaction_lock(tx).await?;
				super::super::create_absent_schemas(tx, &bindings).await?;
				require_utf8_ordered_encoding(tx).await?;
				for unexpected in options.unexpected_v10_protocols {
					unexpected.require_absent(tx).await?;
				}
				let observed_gate = source.gate().pin_shared(tx).await?;
				if observed_gate != NativeSourceGatePhase::PublishedV10 {
					return Err(NativeInstallationError::SourcePhaseMismatch {
						recorded: ImportedPhase::Coexisting,
						observed: observed_gate,
					});
				}
				let source_layout = source.read_validated_layout(tx).await?;
				let descriptor = PgV10SourceDescriptor::from_location(source, source_layout);
				let policies = options
					.queue_source
					.initial_policies(QueueInitialSourcePoliciesFields {
						layout: descriptor.layout().queue_layout(),
					})
					.map_err(initial_policy::InitialPolicyError::from)?;
				component_policies.extend([policies.jobs, policies.auxiliary]);
				let (backend_registration, parsed_backend) = {
					let policy = component_policies
						.iter()
						.find(|policy| policy.component_identifier() == declaration.identifier())
						.ok_or(NativeInstallationError::AdministrativePolicyAbsent)?;
					let binding = declaration.bind(policy)?;
					let backend = binding.bind_schema(bindings.backend_schema().schema())?;
					bindings
						.registration(
							BackendOrigin::ImportedV10 {
								descriptor: Box::new(descriptor.clone()),
								phase: ImportedPhase::Coexisting,
							},
							&backend,
							&scope_policies,
							&initial_indexes,
						)?
						.into_parts()
				};
				let state = InstallationControlState::new(InstallationControlFields {
					installation_id,
					generation: options.generation,
					component_policies,
					scope_policies,
					backend_registration,
					pending_upgrade: None,
				})?;

				check_initial_policies(
					&state,
					&parsed_backend,
					&declaration,
					InitialInstallationMode::ImportedCoexisting {
						queue_layout: descriptor.layout().queue_layout(),
						queue_imports,
					},
					bindings.queue(),
					options.fleet_scalar_code,
				)?;
				let binding = declaration.bind(
					state
						.component(declaration.identifier())
						.expect("initial required administrative policy was checked"),
				)?;
				let participant_schema = binding.bind_schema(participant.schema())?;
				if participant_schema.policy().writer().envelope != StateStorageEnvelope::NativeJson
				{
					return Err(NativeInstallationError::NativeAdminWriterRequired);
				}
				let backend = binding.bind_schema(bindings.backend_schema().schema())?;
				bindings.backend_schema().require_bound_writer(&backend)?;
				let control_schema = binding.bind_schema(control_component.schema())?;
				control_component.require_bound_writer(&control_schema)?;
				super::association::claim_descriptor_import_association(
					tx,
					&descriptor,
					&bindings,
					installation_id,
				)
				.await?;
				for storage in bindings.unique_containers() {
					storage.create_fresh(tx).await?;
				}
				let queue = bindings.queue();
				queue
					.storage()
					.create_queue_selection_indexes(tx, queue.selection_indexes())
					.await?;
				for storage in bindings.raw_record_containers() {
					storage.create_record_expiry_index(tx).await?;
				}
				initial_indexes.create(tx).await?;
				let lock = bindings
					.control
					.lock_exclusive(tx, options.conflict_attempts)
					.await?;
				if lock.snapshot().record().is_some() {
					return Err(NativeInstallationError::ControlAlreadyPresent);
				}
				let record = control_component.encode_record(
					bindings.control.identity(),
					&state,
					&control_schema,
					RecordMetadata {
						expires_at: None,
						updated_at: lock.snapshot().observed_at(),
					},
				)?;
				lock.write(record).await?;
				Ok::<_, NativeInstallationError>(descriptor)
			},
		)
		.await?;
		Ok(Self {
			source: descriptor,
			bindings,
			control_component,
			installation_id,
			phase: ImportedPhase::Coexisting,
		})
	}

	pub(crate) async fn open_recorded(
		control_pool: &crate::db::Pool,
		bindings: PgNativeStorageBindings,
		identity: RecordedInstallationIdentity,
	) -> Result<Self, NativeInstallationError> {
		let participant = ParticipantSchema::new();
		// This locator observation establishes no source admission or ordinary-data authority.
		let reader = InstallationControlReader::new(bindings.control.identity().clone());
		let (source, installation_id) = run_transaction_read(
			control_pool,
			"db.native_installation.discover_recorded_source_descriptor",
			async |tx| {
				let snapshot = bindings.control.read_pinned_shared(tx).await?;
				let (state, _) =
					decode_recorded_control(reader, &bindings, &snapshot, identity, &participant)?;
				let (source, _) = recorded_imported_descriptor_format_only(&state, &bindings)?;
				Ok::<_, NativeInstallationError>((source, state.installation_id()))
			},
		)
		.await?;

		// The first owned transaction has ended. All canonical validation starts source-first.
		let reader = InstallationControlReader::new(bindings.control.identity().clone());
		let (control_component, phase) = run_transaction_read(
			control_pool,
			"db.native_installation.open_recorded_imported",
			async |tx| {
				let gate = source.gate().pin_shared(tx).await?;
				let snapshot = bindings.control.read_pinned_shared(tx).await?;
				let (state, component) = decode_recorded_control(
					reader,
					&bindings,
					&snapshot,
					RecordedInstallationIdentity::Require(installation_id),
					&participant,
				)?;
				let declaration = PgNativeAdministration::declaration_from_schemas(
					&participant,
					&component,
					bindings.backend_schema(),
				);
				let binding = declaration.bind(
					state
						.component(declaration.identifier())
						.ok_or(NativeInstallationError::AdministrativePolicyAbsent)?,
				)?;
				let backend = binding.bind_schema(bindings.backend_schema().schema())?;
				let (observed_source, phase) =
					descriptor_registration(&state, &bindings, gate, &backend)?;
				if !source.matches_descriptor(&observed_source) {
					return Err(NativeInstallationError::SourceBindingFingerprintMismatch);
				}
				if phase != ImportedPhase::SourceDataRetired {
					require_utf8_ordered_encoding(tx).await?;
				}
				if phase == ImportedPhase::SourceDataRetired {
					source.validate_retained_protocol(tx).await?;
					source.validate_retained_layout(tx).await?;
				} else {
					source.validate_existing(tx).await?;
				}
				super::association::verify_descriptor_import_association(
					tx,
					&source,
					&bindings,
					installation_id,
				)
				.await?;
				for container in bindings.unique_containers() {
					container.validate_existing(tx).await?;
				}
				let queue = bindings.queue();
				queue
					.storage()
					.validate_queue_selection_indexes(tx, queue.selection_indexes())
					.await?;
				Ok::<_, NativeInstallationError>((component, phase))
			},
		)
		.await?;
		Ok(Self {
			source,
			bindings,
			control_component,
			installation_id,
			phase,
		})
	}

	pub(crate) fn installation_id(&self) -> SortableId {
		self.installation_id
	}
	pub(crate) fn observed_phase(&self) -> ImportedPhase {
		self.phase
	}
	pub(crate) fn source_descriptor(&self) -> &PgV10SourceDescriptor {
		&self.source
	}
	pub(crate) fn bindings(&self) -> &PgNativeStorageBindings {
		&self.bindings
	}

	/// Consumption cannot fabricate verification; the wrapper has no fieldwise public constructor.
	pub(crate) fn into_parts(
		self,
	) -> (
		PgV10SourceDescriptor,
		PgNativeStorageBindings,
		InstallationControlComponent,
	) {
		(self.source, self.bindings, self.control_component)
	}
}

impl fmt::Debug for PgRecordedImportedInstallation {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("PgRecordedImportedInstallation")
			.field("observed_phase", &self.phase)
			.finish_non_exhaustive()
	}
}

#[cfg(test)]
mod tests {
	use super::UTF8_ORDERED_ENCODINGS;

	#[test]
	fn utf8_ordered_encodings_accept_only_utf8_and_sql_ascii() {
		assert_eq!(UTF8_ORDERED_ENCODINGS, ["UTF8", "SQL_ASCII"]);
		assert!(!UTF8_ORDERED_ENCODINGS.contains(&"LATIN1"));
	}
}
