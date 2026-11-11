use super::ComponentSchemaVersion;
use crate::db::SchemaError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecordedComponentSchemaVersion {
	pub(crate) version: i32,
	pub(crate) fingerprint: String,
}

/// One version/fingerprint endpoint in a component schema migration chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComponentSchemaMigrationVersion<'a> {
	pub(crate) version: i32,
	pub(crate) fingerprint: &'a str,
}

/// One supported component schema migration step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComponentSchemaMigrationStep<'a> {
	/// The target that the step starts from.
	pub from: ComponentSchemaMigrationVersion<'a>,
	/// The target that the step advances to. It must have a larger version than `from`; that
	/// invariant is validated when a schema is planned, migrated, or validated.
	pub to: ComponentSchemaMigrationVersion<'a>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ComponentSchemaMigrationPlan<'a> {
	FreshInstall,
	AlreadyCurrent,
	AlreadyCompatible {
		recorded: RecordedComponentSchemaVersion,
	},
	Upgrade {
		from: RecordedComponentSchemaVersion,
		steps: Vec<ComponentSchemaMigrationStep<'a>>,
	},
}

impl<'a> ComponentSchemaMigrationVersion<'a> {
	/// Creates a migration target.
	///
	/// The version must be positive and the fingerprint must be non-empty; those
	/// invariants are validated when a schema is planned, migrated, or
	/// validated.
	pub const fn new(version: i32, fingerprint: &'a str) -> Self {
		Self {
			version,
			fingerprint,
		}
	}

	/// Returns the schema version for this target.
	pub const fn version(&self) -> i32 {
		self.version
	}

	/// Returns the schema fingerprint for this target.
	pub const fn fingerprint(&self) -> &'a str {
		self.fingerprint
	}
}

impl<'a> From<ComponentSchemaVersion<'a>> for ComponentSchemaMigrationVersion<'a> {
	fn from(version: ComponentSchemaVersion<'a>) -> Self {
		Self::new(version.version, version.fingerprint)
	}
}

pub(crate) fn plan_component_schema_migration<'a>(
	component_schema_version: ComponentSchemaVersion<'_>,
	recorded: Option<RecordedComponentSchemaVersion>,
	upgrade_steps: &'a [ComponentSchemaMigrationStep<'a>],
) -> Result<ComponentSchemaMigrationPlan<'a>, SchemaError> {
	let current = ComponentSchemaMigrationVersion::from(component_schema_version);
	validate_migration_target(
		component_schema_version.component,
		component_schema_version.instance_key,
		"current target",
		current,
	)?;
	validate_upgrade_steps(
		component_schema_version.component,
		component_schema_version.instance_key,
		current,
		upgrade_steps,
	)?;

	let Some(recorded) = recorded else {
		return Ok(ComponentSchemaMigrationPlan::FreshInstall);
	};

	if recorded.version == current.version {
		if recorded.fingerprint == current.fingerprint {
			return Ok(ComponentSchemaMigrationPlan::AlreadyCurrent);
		}
		return Err(SchemaError::mismatch(format!(
			"schema ledger row for component {:?} instance {:?} recorded fingerprint {:?}, expected {:?}",
			component_schema_version.component,
			component_schema_version.instance_key,
			recorded.fingerprint,
			current.fingerprint
		)));
	}

	if recorded.version > current.version {
		return Err(SchemaError::mismatch(format!(
			"schema ledger row for component {:?} instance {:?} recorded version {}, which is newer than supported version {}",
			component_schema_version.component,
			component_schema_version.instance_key,
			recorded.version,
			current.version
		)));
	}

	let mut selected_steps = Vec::new();
	let mut cursor_version = recorded.version;
	let mut cursor_fingerprint = recorded.fingerprint.as_str();

	loop {
		let matching_steps = upgrade_steps
			.iter()
			.copied()
			.filter(|step| {
				step.from.version == cursor_version && step.from.fingerprint == cursor_fingerprint
			})
			.collect::<Vec<_>>();
		let [step] = matching_steps.as_slice() else {
			let reason = if matching_steps.is_empty() {
				format!(
					"schema ledger row for component {:?} instance {:?} recorded unsupported version {} fingerprint {:?}; no migration step reaches supported version {} fingerprint {:?}",
					component_schema_version.component,
					component_schema_version.instance_key,
					cursor_version,
					cursor_fingerprint,
					current.version,
					current.fingerprint
				)
			} else {
				format!(
					"schema migration chain for component {:?} instance {:?} has multiple steps from version {} fingerprint {:?}",
					component_schema_version.component,
					component_schema_version.instance_key,
					cursor_version,
					cursor_fingerprint
				)
			};
			return Err(SchemaError::mismatch(reason));
		};

		selected_steps.push(*step);
		if step.to.version == current.version && step.to.fingerprint == current.fingerprint {
			return Ok(ComponentSchemaMigrationPlan::Upgrade {
				from: recorded,
				steps: selected_steps,
			});
		}

		if step.to.version >= current.version {
			return Err(SchemaError::mismatch(format!(
				"schema migration chain for component {:?} instance {:?} reached version {} fingerprint {:?}, not supported version {} fingerprint {:?}",
				component_schema_version.component,
				component_schema_version.instance_key,
				step.to.version,
				step.to.fingerprint,
				current.version,
				current.fingerprint
			)));
		}

		cursor_version = step.to.version;
		cursor_fingerprint = step.to.fingerprint;
	}
}

fn validate_upgrade_steps(
	component: &str,
	instance_key: &str,
	current: ComponentSchemaMigrationVersion<'_>,
	upgrade_steps: &[ComponentSchemaMigrationStep<'_>],
) -> Result<(), SchemaError> {
	for step in upgrade_steps {
		validate_migration_target(component, instance_key, "step source", step.from)?;
		validate_migration_target(component, instance_key, "step target", step.to)?;
		if step.to.version <= step.from.version {
			return Err(SchemaError::mismatch(format!(
				"schema migration step for component {component:?} instance {instance_key:?} must advance versions, got {} -> {}",
				step.from.version, step.to.version
			)));
		}
		if step.to.version > current.version {
			return Err(SchemaError::mismatch(format!(
				"schema migration step for component {component:?} instance {instance_key:?} targets future version {}, supported version is {}",
				step.to.version, current.version
			)));
		}
	}
	Ok(())
}

fn validate_migration_target(
	component: &str,
	instance_key: &str,
	label: &str,
	target: ComponentSchemaMigrationVersion<'_>,
) -> Result<(), SchemaError> {
	if target.version <= 0 {
		return Err(SchemaError::mismatch(format!(
			"schema migration {label} for component {component:?} instance {instance_key:?} must have a positive version, got {}",
			target.version
		)));
	}
	if target.fingerprint.is_empty() {
		return Err(SchemaError::mismatch(format!(
			"schema migration {label} for component {component:?} instance {instance_key:?} must have a non-empty fingerprint"
		)));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	const COMPONENT: &str = "test";
	const INSTANCE: &str = "table=test";

	fn current() -> ComponentSchemaVersion<'static> {
		ComponentSchemaVersion {
			component: COMPONENT,
			instance_key: INSTANCE,
			version: 3,
			fingerprint: "current",
		}
	}

	fn recorded(version: i32, fingerprint: &str) -> RecordedComponentSchemaVersion {
		RecordedComponentSchemaVersion {
			version,
			fingerprint: fingerprint.to_owned(),
		}
	}

	#[test]
	fn planner_classifies_missing_ledger_as_fresh_install() {
		assert_eq!(
			plan_component_schema_migration(current(), None, &[]).expect("plan"),
			ComponentSchemaMigrationPlan::FreshInstall
		);
	}

	#[test]
	fn planner_classifies_matching_ledger_as_current() {
		assert_eq!(
			plan_component_schema_migration(current(), Some(recorded(3, "current")), &[])
				.expect("plan"),
			ComponentSchemaMigrationPlan::AlreadyCurrent
		);
	}

	#[test]
	fn planner_rejects_same_version_with_different_fingerprint() {
		let err = plan_component_schema_migration(current(), Some(recorded(3, "other")), &[])
			.expect_err("conflicting fingerprint");
		assert!(
			err.to_string().contains("recorded fingerprint"),
			"error = {err:?}"
		);
	}

	#[test]
	fn planner_rejects_future_recorded_version() {
		let err = plan_component_schema_migration(current(), Some(recorded(4, "future")), &[])
			.expect_err("future version");
		assert!(
			err.to_string().contains("newer than supported"),
			"error = {err:?}"
		);
	}

	#[test]
	fn planner_rejects_older_version_without_supported_step() {
		let err = plan_component_schema_migration(current(), Some(recorded(1, "old")), &[])
			.expect_err("unsupported stale version");
		assert!(
			err.to_string().contains("unsupported version"),
			"error = {err:?}"
		);
	}

	#[test]
	fn planner_builds_ordered_upgrade_chain() {
		let steps = [
			ComponentSchemaMigrationStep {
				from: ComponentSchemaMigrationVersion::new(1, "v1"),
				to: ComponentSchemaMigrationVersion::new(2, "v2"),
			},
			ComponentSchemaMigrationStep {
				from: ComponentSchemaMigrationVersion::new(2, "v2"),
				to: ComponentSchemaMigrationVersion::new(3, "current"),
			},
		];

		assert_eq!(
			plan_component_schema_migration(current(), Some(recorded(1, "v1")), &steps)
				.expect("upgrade plan"),
			ComponentSchemaMigrationPlan::Upgrade {
				from: recorded(1, "v1"),
				steps: steps.to_vec(),
			}
		);
	}

	#[test]
	fn planner_rejects_chain_that_reaches_wrong_current_fingerprint() {
		let steps = [ComponentSchemaMigrationStep {
			from: ComponentSchemaMigrationVersion::new(1, "v1"),
			to: ComponentSchemaMigrationVersion::new(3, "wrong"),
		}];

		let err = plan_component_schema_migration(current(), Some(recorded(1, "v1")), &steps)
			.expect_err("wrong final fingerprint");
		assert!(
			err.to_string().contains("not supported version"),
			"error = {err:?}"
		);
	}

	fn step(
		from: (i32, &'static str),
		to: (i32, &'static str),
	) -> ComponentSchemaMigrationStep<'static> {
		ComponentSchemaMigrationStep {
			from: ComponentSchemaMigrationVersion::new(from.0, from.1),
			to: ComponentSchemaMigrationVersion::new(to.0, to.1),
		}
	}

	fn rejection(
		steps: &[ComponentSchemaMigrationStep<'_>],
		recorded_row: RecordedComponentSchemaVersion,
	) -> String {
		plan_component_schema_migration(current(), Some(recorded_row), steps)
			.expect_err("rejected plan")
			.to_string()
	}

	#[test]
	fn planner_rejects_a_recorded_fingerprint_that_no_step_starts_from() {
		let steps = [step((1, "v1"), (2, "v2")), step((2, "v2"), (3, "current"))];
		assert!(rejection(&steps, recorded(1, "unknown")).contains("unsupported version"));
		assert!(rejection(&[], recorded(1, "old")).contains("unsupported version"));
	}

	#[test]
	fn planner_rejects_two_steps_from_the_same_version_and_fingerprint() {
		let steps = [
			step((1, "v1"), (2, "v2")),
			step((1, "v1"), (2, "v2")),
			step((2, "v2"), (3, "current")),
		];
		assert!(rejection(&steps, recorded(1, "v1")).contains("multiple steps"));
	}

	#[test]
	fn planner_rejects_a_chain_whose_last_step_has_the_wrong_final_fingerprint() {
		let steps = [step((1, "v1"), (2, "v2")), step((2, "v2"), (3, "wrong"))];
		assert!(rejection(&steps, recorded(1, "v1")).contains("not supported version"));
	}

	#[test]
	fn planner_rejects_recorded_rows_that_the_ledger_cannot_hold() {
		let steps = [step((1, "v1"), (3, "current"))];
		for malformed in [recorded(0, "x"), recorded(1, "")] {
			assert!(plan_component_schema_migration(current(), Some(malformed), &steps).is_err());
		}
	}

	#[test]
	fn planner_rejects_steps_that_do_not_advance_or_that_target_a_future_version() {
		for steps in [
			[step((2, "v2"), (1, "v1"))],
			[step((2, "v2"), (2, "other"))],
			[step((3, "current"), (4, "future"))],
			[step((0, "zero"), (1, "v1"))],
			[step((1, ""), (2, "v2"))],
		] {
			assert!(plan_component_schema_migration(current(), None, &steps).is_err());
		}
		assert!(
			plan_component_schema_migration(
				ComponentSchemaVersion {
					version: 0,
					..current()
				},
				None,
				&[]
			)
			.is_err()
		);
	}
}
