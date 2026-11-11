use super::{
	EnvironmentVariableName, LocalEnvVault, ProfileEnvironment, SecretBytes, VaultEditSession,
	VaultError, VaultPasswordOptions,
};
use inquire::{InquireError, Password, PasswordDisplayMode, Select};
use primitives_rs::processes::{
	child_processes::{
		ChildProcess, ChildProcessOptions, ChildProcessStdio, ProcessCmd, ProcessEnvironment,
	},
	shutdown_signals::{
		ShutdownEvent, ShutdownSignal, ShutdownSignalError, ShutdownSignalListener,
	},
};
use std::{
	collections::BTreeMap,
	ffi::{OsStr, OsString},
	fmt,
	io::{self, Write},
	process::{ExitStatus, Stdio},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

/// Which ambient variables may enter a command before vault values are applied.
#[derive(Clone, Debug)]
pub enum CmdEnvironmentInheritance {
	/// Inherits all variables except vault-managed names.
	Inherit,
	/// Inherits only listed variables that exist, excluding vault-managed names.
	InheritSelected(Vec<EnvironmentVariableName>),
}

/// Environment and lifecycle choices for every command started by a wrapper.
pub struct VaultCmdOptions {
	/// Chooses ambient inheritance; profile values are applied afterward.
	pub environment_inheritance: CmdEnvironmentInheritance,
	/// Selects process ownership and termination deadlines. Use `ChildOnly` for
	/// commands that need the existing foreground terminal process group.
	pub process_options: ChildProcessOptions,
}

/// A completed wrapper action. Nonzero child exit status is an ordinary outcome.
#[derive(Debug)]
pub enum VaultCmdOutcome {
	/// The operator finished configuration; accepted edits have already been saved.
	Configured,
	/// All required entries exist; no decryption ran.
	ProfileValuesPresent,
	/// The child exited and its owner completed cleanup.
	ChildExited(ExitStatus),
	/// A signal or cancellation requested termination, and cleanup completed.
	Interrupted {
		event: ShutdownEvent,
		status: ExitStatus,
	},
}

/// Interactive configuration and command execution for one application-owned vault.
/// Prompts and password derivation block their calling thread. Child supervision
/// requires an active Tokio runtime with I/O and time enabled.
///
/// The password prompts pass the UTF-8 bytes of the typed text to the vault without
/// Unicode normalization. A new password must have at least the vault's minimum
/// length.
///
/// The `run` command decrypts the profile's values and passes them to the child as
/// environment variables. The operating system accepts environment variables only
/// as plain strings, so the values exist as plain strings in this process, which
/// keeps running while the child runs, and in the child's environment. Memory that
/// this process frees after building the environment is not cleared.
///
/// Signal handling has these limits:
///
/// - The runner registers handlers for SIGINT, SIGTERM, SIGHUP, and SIGQUIT
///   immediately before it starts the child. Before that, during prompts and
///   password derivation, these signals have their default actions unless the
///   application registered its own handlers. If a signal with its default
///   action ends the process while a prompt has set the terminal to raw mode,
///   the terminal stays in raw mode.
/// - When any of the four signals arrives after registration, the runner
///   requests graceful termination of the child according to the configured
///   termination options. The runner does not forward the received signal. With
///   `ChildOnly`, the child stays in the terminal's foreground process group, so
///   the terminal also delivers the SIGINT from Ctrl-C to the child, and the
///   runner then requests termination as well.
/// - The runner does not act on signals that arrive while it waits for a
///   terminating child, so a second Ctrl-C does not end the grace period early.
pub struct VaultCmdRunner<'a> {
	vault: &'a LocalEnvVault,
	options: VaultCmdOptions,
}

/// A command parsed from the wrapper's arguments before any action starts.
enum VaultCmd {
	Configure,
	Check {
		profile_name: String,
	},
	Run {
		profile_name: String,
		program: OsString,
		args: Vec<OsString>,
	},
}

impl VaultCmd {
	/// Parses arguments that start with the executable name.
	fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, VaultError> {
		let mut args = args.into_iter();
		args.next().ok_or(VaultError::InvalidCmd)?;
		let action = args.next().ok_or(VaultError::InvalidCmd)?;
		let cmd = match action.to_str() {
			Some("configure") => Self::Configure,
			Some("check") => Self::Check {
				profile_name: parse_profile_name(args.next())?,
			},
			Some("run") => {
				let profile_name = parse_profile_name(args.next())?;
				if args.next().as_deref() != Some(OsStr::new("--")) {
					return Err(VaultError::InvalidCmd);
				}
				let program = args
					.next()
					.filter(|program| !program.is_empty())
					.ok_or(VaultError::InvalidCmd)?;
				Self::Run {
					profile_name,
					program,
					args: args.by_ref().collect(),
				}
			}
			_ => return Err(VaultError::InvalidCmd),
		};
		if args.next().is_some() {
			return Err(VaultError::InvalidCmd);
		}
		Ok(cmd)
	}
}

fn parse_profile_name(arg: Option<OsString>) -> Result<String, VaultError> {
	arg.and_then(|name| name.into_string().ok())
		.ok_or(VaultError::InvalidCmd)
}

/// An action in the configuration menu.
#[derive(Clone, Copy)]
enum ConfigureAction {
	SetValue,
	ReviewProfiles,
	RemoveUnusedValues,
	ChangePassword,
	Done,
}

impl ConfigureAction {
	const MENU: [Self; 5] = [
		Self::SetValue,
		Self::ReviewProfiles,
		Self::RemoveUnusedValues,
		Self::ChangePassword,
		Self::Done,
	];
}

impl fmt::Display for ConfigureAction {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::SetValue => "Set or update a value",
			Self::ReviewProfiles => "Review profiles",
			Self::RemoveUnusedValues => "Remove unused values",
			Self::ChangePassword => "Change password",
			Self::Done => "Done",
		})
	}
}

/// An item in the value-selection menu.
enum ValueMenuItem<'a> {
	Variable {
		name: &'a EnvironmentVariableName,
		is_stored: bool,
		profile_count: usize,
	},
	Back,
}

impl fmt::Display for ValueMenuItem<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Variable {
				name,
				is_stored,
				profile_count,
			} => write!(
				f,
				"{} — {}; required by {profile_count} profiles",
				name.as_str(),
				if *is_stored { "stored" } else { "missing" }
			),
			Self::Back => f.write_str("Back"),
		}
	}
}

/// An answer to the unused-value removal question.
#[derive(Clone, Copy)]
enum UnusedValueAction {
	Keep,
	Remove,
}

impl fmt::Display for UnusedValueAction {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Keep => "Keep values",
			Self::Remove => "Remove values",
		})
	}
}

impl<'a> VaultCmdRunner<'a> {
	/// Creates a wrapper without accessing the filesystem or terminal.
	pub fn new(vault: &'a LocalEnvVault, options: VaultCmdOptions) -> Self {
		Self { vault, options }
	}

	/// Parses arguments including the executable name, then performs one action.
	///
	/// Commands are `configure`, `check PROFILE`, and `run PROFILE -- COMMAND [ARG ...]`.
	/// Child arguments retain native encoding and are never parsed by a shell.
	/// Keyboard cancellation (Ctrl-C) at the configuration menu restores the prompt
	/// terminal and releases the editor. Esc or Ctrl-C at any prompt below the menu,
	/// and a value or new password that the vault rejects, return to the menu.
	/// The cancellation token controls command startup and asynchronous child execution;
	/// it does not interrupt a synchronous prompt or password derivation.
	pub async fn run_from_args<I, S>(
		&self,
		args: I,
		cancellation: &CancellationToken,
	) -> Result<VaultCmdOutcome, VaultError>
	where
		I: IntoIterator<Item = S>,
		S: Into<OsString>,
	{
		if cancellation.is_cancelled() {
			return Err(VaultError::Cancelled);
		}
		match VaultCmd::parse(args.into_iter().map(Into::into))? {
			VaultCmd::Configure => {
				self.configure()?;
				Ok(VaultCmdOutcome::Configured)
			}
			VaultCmd::Check { profile_name } => self.check(&profile_name),
			VaultCmd::Run {
				profile_name,
				program,
				args,
			} => self.run(&profile_name, program, args, cancellation).await,
		}
	}

	fn check(&self, profile_name: &str) -> Result<VaultCmdOutcome, VaultError> {
		let status = self.vault.check_profile_completeness(profile_name)?;
		for missing in &status.missing_variable_names {
			write_line(&format!("Missing {}", missing.as_str()))?;
		}
		if !status.missing_variable_names.is_empty() {
			return Err(VaultError::MissingValues);
		}
		write_line(&format!(
			"Profile {profile_name}: all required values are stored"
		))?;
		Ok(VaultCmdOutcome::ProfileValuesPresent)
	}

	async fn run(
		&self,
		profile_name: &str,
		program: OsString,
		args: Vec<OsString>,
		cancellation: &CancellationToken,
	) -> Result<VaultCmdOutcome, VaultError> {
		let (profile, record) = self.vault.read_profile_record(profile_name)?;
		let password = prompt_secret("Vault password:")?;
		let environment =
			self.vault
				.decrypt_profile_record(profile, record, password.expose_secret())?;
		drop(password);
		let command = ProcessCmd {
			program,
			args,
			current_dir: None,
			environment: environment
				.create_process_environment(&self.options.environment_inheritance),
		};
		drop(environment);
		run_child(command, self.options.process_options, cancellation).await
	}

	fn configure(&self) -> Result<(), VaultError> {
		let (directory, lock) = self.vault.lock_directory()?;
		let mut editor = match self.vault.read_record(&directory) {
			Ok(record) => {
				let password = prompt_secret("Vault password:")?;
				self.vault
					.edit_locked(directory, lock, record, password.expose_secret())?
			}
			Err(VaultError::MissingVault) => {
				let password = prompt_new_password(&self.vault.password_options)?;
				self.vault
					.create_locked(directory, lock, password.expose_secret())?
			}
			Err(error) => return Err(error),
		};
		loop {
			match select_menu_option("Local environment vault", ConfigureAction::MENU.to_vec())? {
				None | Some(ConfigureAction::Done) => return Ok(()),
				Some(ConfigureAction::SetValue) => configure_value(&mut editor)?,
				Some(ConfigureAction::ReviewProfiles) => review_profiles(&editor)?,
				Some(ConfigureAction::RemoveUnusedValues) => remove_unused_values(&mut editor)?,
				Some(ConfigureAction::ChangePassword) => change_password(&mut editor)?,
			}
		}
	}
}

impl ProfileEnvironment {
	/// Snapshots the selected ambient environment, removes configured and stored
	/// vault names, and adds this profile's values. The parent environment is unchanged.
	/// The returned native strings are plaintext; create the child and drop this
	/// command configuration promptly, without logging or retaining it.
	pub fn create_process_environment(
		&self,
		inheritance: &CmdEnvironmentInheritance,
	) -> ProcessEnvironment {
		let mut environment: BTreeMap<OsString, OsString> = match inheritance {
			CmdEnvironmentInheritance::Inherit => std::env::vars_os().collect(),
			CmdEnvironmentInheritance::InheritSelected(names) => names
				.iter()
				.filter_map(|name| {
					std::env::var_os(name.as_str())
						.map(|value| (OsString::from(name.as_str()), value))
				})
				.collect(),
		};
		for name in &self.managed_variable_names {
			environment.remove(OsStr::new(name.as_str()));
		}
		for (name, value) in &self.values {
			let text = std::str::from_utf8(value.expose_secret())
				.expect("profile values were parsed as UTF-8");
			environment.insert(name.as_str().into(), text.into());
		}
		ProcessEnvironment::Replace(environment)
	}
}

async fn run_child(
	command: ProcessCmd,
	options: ChildProcessOptions,
	cancellation: &CancellationToken,
) -> Result<VaultCmdOutcome, VaultError> {
	if cancellation.is_cancelled() {
		return Err(VaultError::Cancelled);
	}
	let mut signals = ShutdownSignalListener::new(&[
		ShutdownSignal::Interrupt,
		ShutdownSignal::Terminate,
		ShutdownSignal::Hangup,
		ShutdownSignal::Quit,
	])
	.map_err(|error| match error {
		ShutdownSignalError::Registration(error) => VaultError::Io(error),
		ShutdownSignalError::EmptySignalList | ShutdownSignalError::DuplicateSignal(_) => {
			unreachable!("the vault requests a fixed list of distinct signals")
		}
	})?;
	let child = ChildProcess::spawn(
		&command,
		ChildProcessStdio {
			stdin: Stdio::inherit(),
			stdout: Stdio::inherit(),
			stderr: Stdio::inherit(),
		},
		options,
	)
	.map_err(VaultError::Process)?;
	drop(command);
	tokio::select! {
		biased;
		status = child.wait_for_completion() => Ok(VaultCmdOutcome::ChildExited(status.map_err(VaultError::Process)?)),
		event = signals.recv(cancellation) => {
			let event = event?;
			let status = child.terminate().await.map_err(VaultError::Process)?;
			Ok(VaultCmdOutcome::Interrupted { event, status })
		}
	}
}

fn configure_value(editor: &mut VaultEditSession<'_>) -> Result<(), VaultError> {
	let vault = editor.vault;
	let mut items: Vec<_> = vault
		.required_variable_names
		.iter()
		.map(|name| ValueMenuItem::Variable {
			name,
			is_stored: editor.record.body().entries.contains_key(name.as_str()),
			profile_count: vault
				.profiles
				.values()
				.filter(|profile| profile.required_variable_names.contains(name))
				.count(),
		})
		.collect();
	items.push(ValueMenuItem::Back);
	let name = match select_submenu_option("Select a value", items)? {
		Some(ValueMenuItem::Variable { name, .. }) => name,
		Some(ValueMenuItem::Back) | None => return Ok(()),
	};
	let value = match prompt_secret(&format!("{}:", name.as_str())) {
		Ok(value) => value,
		Err(VaultError::Cancelled) => {
			return write_line(&format!("{} was not changed", name.as_str()));
		}
		Err(error) => return Err(error),
	};
	match editor.set_value(name, value.expose_secret()) {
		Ok(()) => write_line(&format!(
			"Stored {} ({})",
			name.as_str(),
			if value.is_empty() {
				"empty"
			} else {
				"nonempty"
			}
		)),
		Err(error @ VaultError::InvalidValue) => {
			write_line(&format!("{} was not changed: {error}", name.as_str()))
		}
		Err(error) => Err(error),
	}
}
fn change_password(editor: &mut VaultEditSession<'_>) -> Result<(), VaultError> {
	let password = match prompt_new_password(&editor.vault.password_options) {
		Ok(password) => password,
		Err(VaultError::Cancelled) => return write_line("The password was not changed"),
		Err(
			error @ (VaultError::PasswordTooShort
			| VaultError::PasswordTooLong
			| VaultError::PasswordConfirmationMismatch),
		) => return write_line(&format!("The password was not changed: {error}")),
		Err(error) => return Err(error),
	};
	editor.change_password(password.expose_secret())?;
	write_line("Password changed")
}
fn review_profiles(editor: &VaultEditSession<'_>) -> Result<(), VaultError> {
	for profile in editor.vault.profiles() {
		write_line(&format!("Profile {}", profile.name()))?;
		let status = editor.check_profile_completeness(profile.name())?;
		for name in status.stored_variable_names {
			write_line(&format!("  {}: stored", name.as_str()))?;
		}
		for name in status.missing_variable_names {
			write_line(&format!("  {}: missing", name.as_str()))?;
		}
	}
	Ok(())
}
fn remove_unused_values(editor: &mut VaultEditSession<'_>) -> Result<(), VaultError> {
	let unused = editor.unused_variable_names();
	if unused.is_empty() {
		return write_line("There are no unused values");
	}
	for name in &unused {
		write_line(&format!("Unused: {}", name.as_str()))?;
	}
	match select_submenu_option(
		"Remove these unused values?",
		vec![UnusedValueAction::Keep, UnusedValueAction::Remove],
	)? {
		Some(UnusedValueAction::Remove) => {
			let removed = editor.remove_unused_values()?;
			write_line(&format!("Removed {} unused values", removed.len()))
		}
		Some(UnusedValueAction::Keep) | None => Ok(()),
	}
}
fn prompt_secret(prompt: &str) -> Result<SecretBytes, VaultError> {
	let text = Zeroizing::new(
		Password::new(prompt)
			.without_confirmation()
			.with_display_mode(PasswordDisplayMode::Hidden)
			.with_render_config(terminal_render_config())
			.prompt()
			.map_err(prompt_error)?,
	);
	SecretBytes::from_slice(text.as_bytes()).map_err(|_| VaultError::Crypto)
}
fn prompt_new_password(options: &VaultPasswordOptions) -> Result<SecretBytes, VaultError> {
	let password = prompt_secret("New vault password:")?;
	options.check_new_password(password.expose_secret())?;
	let confirmation = prompt_secret("Confirm vault password:")?;
	if password.expose_secret() != confirmation.expose_secret() {
		return Err(VaultError::PasswordConfirmationMismatch);
	}
	Ok(password)
}
/// Returns `None` for Esc and `VaultError::Cancelled` for Ctrl-C.
fn select_menu_option<T: fmt::Display>(
	prompt: &str,
	options: Vec<T>,
) -> Result<Option<T>, VaultError> {
	Select::new(prompt, options)
		.with_render_config(terminal_render_config())
		.prompt_skippable()
		.map_err(prompt_error)
}
/// Returns `None` for Esc and for Ctrl-C, so that both return to the menu.
fn select_submenu_option<T: fmt::Display>(
	prompt: &str,
	options: Vec<T>,
) -> Result<Option<T>, VaultError> {
	match select_menu_option(prompt, options) {
		Err(VaultError::Cancelled) => Ok(None),
		result => result,
	}
}
fn terminal_render_config() -> inquire::ui::RenderConfig<'static> {
	if std::env::var_os("NO_COLOR").is_some() {
		inquire::ui::RenderConfig::empty()
	} else {
		inquire::ui::RenderConfig::default()
	}
}
fn prompt_error(error: InquireError) -> VaultError {
	match error {
		InquireError::OperationCanceled | InquireError::OperationInterrupted => {
			VaultError::Cancelled
		}
		_ => VaultError::Io(io::Error::other("terminal prompt failed")),
	}
}
fn write_line(line: &str) -> Result<(), VaultError> {
	let mut output = io::stdout().lock();
	writeln!(output, "{line}")?;
	output.flush()?;
	Ok(())
}
