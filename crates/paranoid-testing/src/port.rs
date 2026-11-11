use crate::error::{PgTestHarnessError, PgTestHarnessPhase};
use primitives_rs::encoding::binary_to_text::ByteEncoding;
use primitives_rs::random::secure_bytes::{RandomError, generate_random_byte_array};
use std::fs::{File, OpenOptions, TryLockError};
use std::hash::BuildHasher;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
mod creation;
use creation::write_recorded_container;
pub(crate) use creation::{CreationTarget, RecordedContainer, RecordedContainers};

/// The file in which Linux gives the range of ports that it assigns automatically.
#[cfg(target_os = "linux")]
const AUTOMATIC_PORT_RANGE_FILE: &str = "/proc/sys/net/ipv4/ip_local_port_range";

/// The lowest port that a process can bind without privileges.
const FIRST_UNPRIVILEGED_PORT: u16 = 1024;

/// The name of every reservation file is this prefix, the port, and [`RESERVATION_FILE_SUFFIX`].
const RESERVATION_FILE_PREFIX: &str = "paranoid-ts-test-port-";
const RESERVATION_FILE_SUFFIX: &str = ".lock";

fn reservation_file_name(port: u16) -> String {
	format!("{RESERVATION_FILE_PREFIX}{port}{RESERVATION_FILE_SUFFIX}")
}

/// The port of the reservation file with this name, or `None` for any other name.
fn reservation_port(name: &str) -> Option<u16> {
	name.strip_prefix(RESERVATION_FILE_PREFIX)?
		.strip_suffix(RESERVATION_FILE_SUFFIX)?
		.parse()
		.ok()
}

fn is_reservation_file_name(name: &str) -> bool {
	reservation_port(name).is_some()
}

/// How many random bytes end a harness identifier. They make the identifier, and with it the
/// names of the harness's containers, differ from the identifier of every other harness, also
/// of a harness in another PID namespace that has the same process ID and the same ports.
const IDENTIFIER_RANDOM_BYTES: usize = 8;

/// The two ports that a harness identifier names.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IdentifierPorts {
	pub(crate) pg_port: u16,
	pub(crate) first_pgbouncer_port: u16,
}

/// Creates the identifier of a harness: the process ID, the PostgreSQL port, the first PgBouncer
/// port, and the lowercase hexadecimal form of random bytes, joined by `-`.
pub(crate) fn create_harness_identifier(ports: IdentifierPorts) -> Result<String, RandomError> {
	let random: [u8; IDENTIFIER_RANDOM_BYTES] = generate_random_byte_array()?;
	Ok(format!(
		"{}-{}-{}-{}",
		std::process::id(),
		ports.pg_port,
		ports.first_pgbouncer_port,
		ByteEncoding::HexLower.encode(&random)
	))
}

/// A loopback TCP port that no other harness holds, among the processes that share the system
/// temporary directory.
///
/// A file in that directory records the reservation, so concurrent test processes do not
/// choose the same port. The process holds an exclusive advisory lock on the file while it
/// holds the reservation, and the operating system releases that lock when the last process
/// that holds the file open ends, however it ends. A process started with `fork` and without
/// `exec` holds the file open too. Two processes with different temporary directories do not see
/// each other's files, so they can draw the same port.
///
/// The reservation ends on release or on drop, which remove the file. The exception is
/// [`leave_to_the_next_harness`](Self::leave_to_the_next_harness), which leaves the file
/// without its lock. A reservation file without a lock is therefore one that its harness left
/// on purpose or one of a process that has ended; see
/// [`take_unheld_reservations`](Self::take_unheld_reservations).
///
/// The reservation does not hold the port itself because the server in the container must
/// bind it. On Linux the harness draws the port from outside the range that the kernel assigns
/// automatically, so a socket that names no port, such as an outgoing connection or a listener
/// on port 0, cannot take the port before the server binds it. Only a socket that names the
/// port can. On another system, and on a Linux system with no unprivileged ports outside its automatic range,
/// the harness asks the kernel for a free port, and the kernel can assign
/// that port to another socket before the server binds it.
///
/// The reservation of the PostgreSQL port also records its harness and the state of each of the
/// harness's containers ([`record_harness`](Self::record_harness) and
/// [`record_container`](Self::record_container)). The open file of the reservation is the only
/// holder of its lock.
pub(crate) struct PortReservation {
	port: u16,
	held: Option<HeldReservationFile>,
}

/// A reservation file and the open handle that holds its lock. The handle appends every write
/// to the end of the file.
struct HeldReservationFile {
	path: PathBuf,
	file: File,
}

impl HeldReservationFile {
	/// Removes the file, unless another file has taken its path, which happens only when
	/// something outside the harness removed this file.
	fn remove(self) -> std::io::Result<()> {
		if !is_file_at_path(&self.file, &self.path) {
			return Ok(());
		}
		match std::fs::remove_file(&self.path) {
			Err(error) if error.kind() != ErrorKind::NotFound => Err(error),
			_ => Ok(()),
		}
	}
}

/// The container runtime that a record names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RecordedRuntime {
	/// The address at which the harness reached the runtime.
	Address(String),
	/// The value of `DOCKER_HOST` in the process of a record with a three-part identifier, or
	/// an empty text when that process had no `DOCKER_HOST`; that process found the runtime as
	/// a harness finds it from the same value.
	DockerHost(String),
}

/// What a harness writes into the reservation file of its PostgreSQL port so that a later
/// harness can remove the containers that this one left.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HarnessRecord {
	/// The identifier of the harness, which is part of the name of each of its containers; see
	/// [`create_harness_identifier`].
	pub(crate) identifier: String,
	pub(crate) runtime: RecordedRuntime,
	pub(crate) containers: RecordedContainers,
}

impl HarnessRecord {
	/// The text of a new record. A harness writes only records with a four-part identifier.
	fn to_text(&self) -> String {
		let runtime = match &self.runtime {
			RecordedRuntime::Address(address) | RecordedRuntime::DockerHost(address) => address,
		};
		format!("{}\n{}", self.identifier, runtime)
	}

	/// Reads the record of the reservation file of `port`. Any process of the machine can
	/// write a file in the temporary directory, so a record counts only when its identifier
	/// has one of the two forms below and names `port` as its PostgreSQL port.
	///
	/// A record is the identifier, a line feed, the runtime, and then one line for each event
	/// of the containers. The parser reads two forms:
	///
	/// - The four-part identifier `<process ID>-<PostgreSQL port>-<first PgBouncer port>-<16
	///   lowercase hexadecimal digits>`, with the runtime's address. Each event line is
	///   `+<target>` (a container can exist under the target's name) or `-<target>` (none
	///   exists), where the target is 1 for PostgreSQL, 2 for the PgBouncer in transaction
	///   pooling, and 4 for the PgBouncer in statement pooling. A line of any other form makes
	///   every container possibly present.
	/// - The three-part identifier `<process ID>-<PostgreSQL port>-<first PgBouncer port>`, with
	///   the value of `DOCKER_HOST` or an empty text. In such a record, `-<target>` states only
	///   that a create request got an answer, which can have created the container, so the
	///   parser reads every container as possibly present and ignores the event lines.
	fn from_text(text: &str, port: u16) -> Option<Self> {
		let (identifier, tail) = text.split_once('\n')?;
		let mut lines = tail.split('\n');
		let runtime = lines.next()?.to_owned();
		let parts: Vec<&str> = identifier.split('-').collect();
		let is_number =
			|part: &&str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
		let is_random = |part: &str| {
			part.len() == 2 * IDENTIFIER_RANDOM_BYTES
				&& part
					.bytes()
					.all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
		};
		if parts.len() < 3 || !parts[..3].iter().all(is_number) || parts[1] != port.to_string() {
			return None;
		}
		let (runtime, containers) = match parts[3..] {
			[random] if is_random(random) => {
				let mut containers = RecordedContainers::absent();
				for event in lines {
					containers.read_event(event);
				}
				(RecordedRuntime::Address(runtime), containers)
			}
			[] => (
				RecordedRuntime::DockerHost(runtime),
				RecordedContainers::possibly_present(),
			),
			_ => return None,
		};
		Some(Self {
			identifier: identifier.to_owned(),
			runtime,
			containers,
		})
	}
}

/// The longest text that a harness writes into a reservation file is far below this many bytes.
const MAX_RESERVATION_TEXT_LEN: u64 = 4096;

impl PortReservation {
	pub(crate) fn reserve(attempts: u32) -> Result<Self, PgTestHarnessError> {
		Self::reserve_in(&std::env::temp_dir(), attempts, allocate_loopback_port)
	}

	/// Reserves a port that `allocate_port` draws, recording it in `lock_dir`.
	fn reserve_in(
		lock_dir: &Path,
		attempts: u32,
		mut allocate_port: impl FnMut() -> Result<u16, PgTestHarnessError>,
	) -> Result<Self, PgTestHarnessError> {
		for _ in 0..attempts {
			let port = allocate_port()?;
			let path = lock_dir.join(reservation_file_name(port));
			match create_locked_reservation_file(lock_dir, &path) {
				Ok(Some(file)) => {
					return Ok(Self {
						port,
						held: Some(HeldReservationFile { path, file }),
					});
				}
				// Another reservation has the port.
				Ok(None) => {}
				Err(error) => {
					return Err(port_reserve_error(
						"could not reserve a loopback TCP port",
						error,
					));
				}
			}
		}
		Err(PgTestHarnessError::new(
			PgTestHarnessPhase::PortReserve,
			"port reservation attempts were exhausted",
		))
	}

	pub(crate) fn port(&self) -> u16 {
		self.port
	}

	pub(crate) fn associate_with(
		&mut self,
		record: &HarnessRecord,
	) -> Result<(), PgTestHarnessError> {
		let held = self.held.as_mut().expect("the reservation is held");
		write!(held.file, "reserved-with:{}", record.to_text())
			.and_then(|()| held.file.flush())
			.map_err(|error| port_reserve_error("could not associate the reserved ports", error))
	}

	/// Appends the state of one of the harness's containers to the record of this reservation,
	/// which must be the reservation of the harness's PostgreSQL port.
	pub(crate) fn record_container(
		&self,
		target: CreationTarget,
		recorded: RecordedContainer,
	) -> std::io::Result<()> {
		let held = self
			.held
			.as_ref()
			.expect("a harness records its containers while it holds its reservation");
		write_recorded_container(&held.file, target, recorded)
	}

	/// Writes the record of the harness that holds this reservation into the reservation file.
	pub(crate) fn record_harness(
		&mut self,
		record: &HarnessRecord,
	) -> Result<(), PgTestHarnessError> {
		let held = self
			.held
			.as_mut()
			.expect("a harness writes its record before it releases the reservation");
		held.file
			.write_all(record.to_text().as_bytes())
			.and_then(|()| held.file.flush())
			.map_err(|error| {
				port_reserve_error(
					"could not record the harness in its port reservation",
					error,
				)
			})
	}

	/// Ends this process's hold on the reservation and leaves the reservation file so that
	/// the next harness that starts removes the containers that the file's record names and
	/// then the file. Until then the port stays reserved.
	///
	/// On a system other than Unix, automatic recovery cannot identify a replaced reservation
	/// file. The reservation remains rather than permitting reuse of an unresolved port.
	pub(crate) fn leave_to_the_next_harness(&mut self) {
		self.held.take();
	}

	pub(crate) fn release(&mut self) -> Result<(), PgTestHarnessError> {
		let Some(held) = self.held.take() else {
			return Ok(());
		};
		held.remove().map_err(|error| {
			PgTestHarnessError::with_source(
				PgTestHarnessPhase::PortRelease,
				"could not release the loopback TCP port reservation",
				error,
			)
		})
	}
}

impl Drop for PortReservation {
	fn drop(&mut self) {
		if let Some(held) = self.held.take() {
			let _ = held.remove();
		}
	}
}

/// Creates the reservation file at `path` and returns its locked handle, or `None` when a
/// reservation already has that path.
///
/// The file is created and locked under a name of its own and then linked to `path`, so every
/// reservation file that another process can see is already locked. A process that ends
/// between the two steps leaves the file under its own name, which reserves no port and which
/// no harness removes.
fn create_locked_reservation_file(lock_dir: &Path, path: &Path) -> std::io::Result<Option<File>> {
	let own_path = lock_dir.join(format!(
		"{RESERVATION_FILE_PREFIX}{}-{:016x}.unlinked",
		std::process::id(),
		std::hash::RandomState::new().hash_one(path)
	));
	let file = OpenOptions::new()
		.read(true)
		.append(true)
		.create_new(true)
		.open(&own_path)?;
	let linked = file
		.try_lock()
		.map_err(|error| match error {
			TryLockError::WouldBlock => std::io::Error::from(ErrorKind::WouldBlock),
			TryLockError::Error(error) => error,
		})
		.and_then(|()| std::fs::hard_link(&own_path, path));
	let removed = std::fs::remove_file(&own_path);
	match linked {
		Ok(()) => removed.map(|()| Some(file)),
		Err(error) if error.kind() == ErrorKind::AlreadyExists => removed.map(|()| None),
		Err(error) => Err(error),
	}
}

/// A reservation file that no process holds locked: its harness left it, or its process ended.
///
/// The value holds the file's lock, so no other harness handles the same reservation at the
/// same time.
pub(crate) struct UnheldReservation {
	held: HeldReservationFile,
	record: ReservationRecord,
}

enum ReservationRecord {
	Empty,
	Harness(HarnessRecord),
	Associated(HarnessRecord),
	Unknown,
}

impl UnheldReservation {
	/// The record of the harness that held the reservation, when the reservation is the one
	/// of a PostgreSQL port.
	pub(crate) fn harness_record(&self) -> Option<&HarnessRecord> {
		match &self.record {
			ReservationRecord::Harness(record) => Some(record),
			_ => None,
		}
	}

	pub(crate) fn can_remove_without_harness(&self) -> bool {
		let associated = match &self.record {
			ReservationRecord::Unknown => return false,
			ReservationRecord::Empty | ReservationRecord::Harness(_) => return true,
			ReservationRecord::Associated(record) => record,
		};
		let port = associated
			.identifier
			.split('-')
			.nth(1)
			.and_then(|value| value.parse().ok())
			.expect("the association has a parsed PostgreSQL port");
		let path = self.held.path.with_file_name(reservation_file_name(port));
		let file = match File::open(path) {
			Ok(file) if is_regular_file_of_the_same_owner(&file, &self.held.file) => file,
			Err(error) if error.kind() == ErrorKind::NotFound => return true,
			_ => return false,
		};
		let mut text = String::new();
		if file
			.take(MAX_RESERVATION_TEXT_LEN)
			.read_to_string(&mut text)
			.is_err()
		{
			return false;
		}
		HarnessRecord::from_text(&text, port).is_some_and(|record| {
			record.identifier != associated.identifier || record.runtime != associated.runtime
		})
	}

	pub(crate) fn is_associated(&self) -> bool {
		matches!(self.record, ReservationRecord::Associated(_))
	}

	/// Records that no container of `target` exists, after a removal by name that found it or
	/// found no container with its name.
	pub(crate) fn record_absent(&mut self, target: CreationTarget) -> std::io::Result<()> {
		write_recorded_container(&self.held.file, target, RecordedContainer::Absent)?;
		if let ReservationRecord::Harness(record) = &mut self.record {
			record.containers.set(target, RecordedContainer::Absent);
		}
		Ok(())
	}

	/// Removes the reservation file so that the port can be reserved again.
	pub(crate) fn remove(self) {
		let _ = self.held.remove();
	}
}

fn reservation_file_paths_in(lock_dir: &Path) -> Vec<PathBuf> {
	let Ok(entries) = std::fs::read_dir(lock_dir) else {
		return Vec::new();
	};
	entries
		.filter_map(|entry| {
			let entry = entry.ok()?;
			is_reservation_file_name(entry.file_name().to_str()?).then(|| entry.path())
		})
		.collect()
}

impl PortReservation {
	/// Takes every reservation file in the system temporary directory that no process holds
	/// locked; see [`take_unheld_reservation`](Self::take_unheld_reservation). The scan reads
	/// the directory and opens, locks, and reads files, so the harness runs it outside the
	/// async runtime's threads.
	pub(crate) fn take_unheld_reservations(&self) -> Vec<UnheldReservation> {
		reservation_file_paths_in(&std::env::temp_dir())
			.iter()
			.filter_map(|path| self.take_unheld_reservation(path))
			.collect()
	}

	/// Takes the reservation file at `path` when no process holds it locked and the user who
	/// owns this reservation owns it.
	///
	/// Any user of the machine can put a file with a reservation's name into the temporary
	/// directory. A file of another user is left alone, and so is anything that is not a
	/// regular file.
	fn take_unheld_reservation(&self, path: &Path) -> Option<UnheldReservation> {
		let own = self.held.as_ref()?;
		let port = reservation_port(path.file_name()?.to_str()?)?;
		let file = OpenOptions::new().read(true).append(true).open(path).ok()?;
		if !is_regular_file_of_the_same_owner(&file, &own.file) {
			return None;
		}
		// A live harness holds the lock of its reservation.
		file.try_lock().ok()?;
		// Another harness can have removed the file, and a live harness can have reserved the
		// same port with a new file, between the open and the lock.
		if !is_file_at_path(&file, path) {
			return None;
		}
		let mut text = String::new();
		(&file)
			.take(MAX_RESERVATION_TEXT_LEN)
			.read_to_string(&mut text)
			.ok()?;
		let harness = HarnessRecord::from_text(&text, port);
		let associated_harness = text.strip_prefix("reserved-with:").and_then(|record| {
			let pg_port = record.split('-').nth(1)?.parse().ok()?;
			HarnessRecord::from_text(record, pg_port)
		});
		let record = match (harness, associated_harness) {
			(Some(record), _) => ReservationRecord::Harness(record),
			(None, Some(record)) => ReservationRecord::Associated(record),
			(None, None) if text.is_empty() => ReservationRecord::Empty,
			(None, None) => ReservationRecord::Unknown,
		};
		Some(UnheldReservation {
			held: HeldReservationFile {
				path: path.to_owned(),
				file,
			},
			record,
		})
	}
}

/// Whether `path` names the file that `file` has open.
#[cfg(unix)]
fn is_file_at_path(file: &File, path: &Path) -> bool {
	use std::os::unix::fs::MetadataExt;
	match (file.metadata(), std::fs::symlink_metadata(path)) {
		(Ok(open), Ok(named)) => open.dev() == named.dev() && open.ino() == named.ino(),
		_ => false,
	}
}

/// Whether `path` names the file that `file` has open. A system other than Unix gives the
/// harness no identity of a file to compare, so the answer there is that it does.
#[cfg(not(unix))]
fn is_file_at_path(_file: &File, _path: &Path) -> bool {
	true
}

/// Whether `file` is a regular file that the owner of `own` owns.
#[cfg(unix)]
fn is_regular_file_of_the_same_owner(file: &File, own: &File) -> bool {
	use std::os::unix::fs::MetadataExt;
	match (file.metadata(), own.metadata()) {
		(Ok(file), Ok(own)) => file.is_file() && file.uid() == own.uid(),
		_ => false,
	}
}

/// A system other than Unix gives the harness no owner to compare, and no harness takes a
/// reservation that another left there.
#[cfg(not(unix))]
fn is_regular_file_of_the_same_owner(_file: &File, _own: &File) -> bool {
	false
}

fn port_reserve_error(reason: &'static str, error: std::io::Error) -> PgTestHarnessError {
	PgTestHarnessError::with_source(PgTestHarnessPhase::PortReserve, reason, error)
}

/// Draws a loopback TCP port that no socket listens on at the moment of the draw.
fn allocate_loopback_port() -> Result<u16, PgTestHarnessError> {
	match read_automatic_port_range()?.and_then(AutomaticPortRange::other_ports) {
		Some(other_ports) => {
			// Each newly initialized `RandomState` has keys that differ between threads and
			// between calls, so harnesses of one process start their searches at different ports.
			let start = std::hash::RandomState::new().hash_one(std::process::id());
			other_ports
				.find_from(start, is_loopback_port_free)
				.map_err(|error| {
					port_reserve_error("could not test whether a loopback TCP port is free", error)
				})?
				.ok_or_else(|| {
					PgTestHarnessError::new(
						PgTestHarnessPhase::PortReserve,
						"every loopback TCP port outside the kernel's automatic range is in use",
					)
				})
		}
		None => allocate_loopback_port_from_the_kernel(),
	}
}

/// Asks the kernel for a free loopback TCP port. The kernel can assign the same port to another
/// socket as soon as this function returns.
fn allocate_loopback_port_from_the_kernel() -> Result<u16, PgTestHarnessError> {
	let allocate = || -> std::io::Result<u16> {
		Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
	};
	allocate().map_err(|error| port_reserve_error("could not allocate a loopback TCP port", error))
}

fn is_loopback_port_free(port: u16) -> std::io::Result<bool> {
	match TcpListener::bind(("127.0.0.1", port)) {
		Ok(_) => Ok(true),
		Err(error) if error.kind() == ErrorKind::AddrInUse => Ok(false),
		Err(error) => Err(error),
	}
}

/// The range of ports that the kernel assigns automatically, or `None` on a system where the
/// harness has no means to read it.
#[cfg(target_os = "linux")]
fn read_automatic_port_range() -> Result<Option<AutomaticPortRange>, PgTestHarnessError> {
	let text = std::fs::read_to_string(AUTOMATIC_PORT_RANGE_FILE).map_err(|error| {
		port_reserve_error(
			"could not read the kernel's automatic port range from /proc/sys/net/ipv4/ip_local_port_range",
			error,
		)
	})?;
	AutomaticPortRange::parse(&text).map(Some).ok_or_else(|| {
		PgTestHarnessError::new(
			PgTestHarnessPhase::PortReserve,
			"/proc/sys/net/ipv4/ip_local_port_range does not hold two port numbers in ascending order",
		)
	})
}

/// The range of ports that the kernel assigns automatically, or `None` on a system where the
/// harness has no means to read it.
#[cfg(not(target_os = "linux"))]
fn read_automatic_port_range() -> Result<Option<AutomaticPortRange>, PgTestHarnessError> {
	Ok(None)
}

/// The ports that the kernel assigns to a socket that names no port, from `first` to `last`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AutomaticPortRange {
	first: u16,
	last: u16,
}

impl AutomaticPortRange {
	/// Parses two port numbers in ascending order, separated by whitespace, which is the form
	/// of `/proc/sys/net/ipv4/ip_local_port_range`.
	#[cfg_attr(not(target_os = "linux"), expect(dead_code))]
	fn parse(text: &str) -> Option<Self> {
		let mut numbers = text.split_whitespace().map(str::parse::<u16>);
		let (Some(Ok(first)), Some(Ok(last)), None) =
			(numbers.next(), numbers.next(), numbers.next())
		else {
			return None;
		};
		(first <= last).then_some(Self { first, last })
	}

	/// The unprivileged ports that the kernel never assigns automatically: the ports above the
	/// range, or the ports below it when none is above it. `None` when the range leaves neither.
	fn other_ports(self) -> Option<OtherPorts> {
		if self.last < u16::MAX {
			Some(OtherPorts {
				first: self.last + 1,
				last: u16::MAX,
			})
		} else if self.first > FIRST_UNPRIVILEGED_PORT {
			Some(OtherPorts {
				first: FIRST_UNPRIVILEGED_PORT,
				last: self.first - 1,
			})
		} else {
			None
		}
	}
}

/// Unprivileged ports outside the kernel's automatic range, from `first` to `last`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OtherPorts {
	first: u16,
	last: u16,
}

impl OtherPorts {
	/// The first port for which `is_free_fn` returns true, testing each port once, from the port
	/// that `start` selects upward and then from `first`. `None` when no port is free.
	fn find_from<E>(
		self,
		start: u64,
		mut is_free_fn: impl FnMut(u16) -> Result<bool, E>,
	) -> Result<Option<u16>, E> {
		let count = u64::from(self.last - self.first) + 1;
		for offset in 0..count {
			let index = (start % count + offset) % count;
			let port = self.first
				+ u16::try_from(index).expect("an index below the port count fits a port number");
			if is_free_fn(port)? {
				return Ok(Some(port));
			}
		}
		Ok(None)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::atomic::{AtomicU32, Ordering};

	static SCRATCH_DIR_COUNT: AtomicU32 = AtomicU32::new(0);

	/// A scratch directory under the system temporary directory, removed on drop.
	struct ScratchDir(PathBuf);

	impl ScratchDir {
		fn new() -> Self {
			let path = std::env::temp_dir().join(format!(
				"paranoid-testing-port-test-{}-{}",
				std::process::id(),
				SCRATCH_DIR_COUNT.fetch_add(1, Ordering::Relaxed)
			));
			std::fs::create_dir(&path).expect("the scratch directory is new");
			Self(path)
		}

		fn path(&self) -> &Path {
			&self.0
		}
	}

	impl Drop for ScratchDir {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}

	fn lock_path(dir: &Path, port: u16) -> PathBuf {
		dir.join(reservation_file_name(port))
	}

	/// Writes a reservation file that no process holds locked, as a process that ended without
	/// releasing its reservation leaves it.
	fn leave_reservation(dir: &Path, port: u16, text: &str) -> PathBuf {
		let path = lock_path(dir, port);
		std::fs::write(&path, text).expect("write the reservation file");
		path
	}

	fn draw_from(ports: &[u16]) -> impl FnMut() -> Result<u16, PgTestHarnessError> + '_ {
		let mut ports = ports.iter().copied();
		move || Ok(ports.next().expect("the test supplies enough ports"))
	}

	#[test]
	fn the_automatic_port_range_parses_only_two_ascending_port_numbers() {
		assert_eq!(
			AutomaticPortRange::parse("32768\t60999\n"),
			Some(AutomaticPortRange {
				first: 32768,
				last: 60999
			})
		);
		for malformed in [
			"",
			"32768",
			"60999 32768",
			"32768 60999 1",
			"32768 70000",
			"a b",
		] {
			assert_eq!(AutomaticPortRange::parse(malformed), None, "{malformed}");
		}
	}

	#[test]
	fn the_other_ports_are_above_the_automatic_range_or_else_below_it() {
		let range = |first, last| AutomaticPortRange { first, last };
		assert_eq!(
			range(32768, 60999).other_ports(),
			Some(OtherPorts {
				first: 61000,
				last: 65535
			})
		);
		assert_eq!(
			range(32768, 65535).other_ports(),
			Some(OtherPorts {
				first: 1024,
				last: 32767
			})
		);
		assert_eq!(range(1024, 65535).other_ports(), None);
		assert_eq!(range(1, 65535).other_ports(), None);
	}

	#[test]
	fn the_search_tests_each_port_once_from_its_start_and_wraps() {
		let ports = OtherPorts {
			first: 61000,
			last: 61003,
		};
		let mut tested = Vec::new();
		let found = ports.find_from(6, |port| {
			tested.push(port);
			Ok::<_, std::convert::Infallible>(port == 61001)
		});
		assert_eq!(found, Ok(Some(61001)));
		assert_eq!(tested, [61002, 61003, 61000, 61001]);

		let mut tested = Vec::new();
		let found = ports.find_from(u64::MAX, |port| {
			tested.push(port);
			Ok::<_, std::convert::Infallible>(false)
		});
		assert_eq!(found, Ok(None));
		assert_eq!(tested.len(), 4);

		let failed = ports.find_from(0, |_| Err("the test fails"));
		assert_eq!(failed, Err("the test fails"));
	}

	#[test]
	fn a_port_with_a_listener_is_not_free() {
		let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
		let port = listener.local_addr().unwrap().port();
		assert!(!is_loopback_port_free(port).unwrap());
	}

	#[cfg(target_os = "linux")]
	#[test]
	fn a_drawn_port_is_outside_the_automatic_range_of_this_system() {
		let range = read_automatic_port_range()
			.unwrap()
			.expect("Linux gives the range");
		if range.other_ports().is_some() {
			let port = allocate_loopback_port().unwrap();
			assert!(port < range.first || port > range.last, "{port}");
			assert!(port >= FIRST_UNPRIVILEGED_PORT, "{port}");
		}
	}

	#[test]
	fn two_reservations_hold_distinct_ports_and_end_on_release_or_drop() {
		let outer = PortReservation::reserve(100).unwrap();
		let mut inner = PortReservation::reserve(100).unwrap();
		assert_ne!(outer.port(), inner.port());
		let outer_path = lock_path(&std::env::temp_dir(), outer.port());
		let inner_path = lock_path(&std::env::temp_dir(), inner.port());
		assert!(outer_path.is_file());
		assert!(inner_path.is_file());
		inner.release().unwrap();
		assert!(!inner_path.exists());
		assert!(outer_path.is_file());
		drop(outer);
		assert!(!outer_path.exists());
		inner.release().unwrap();
	}

	#[test]
	fn collisions_retry_within_the_attempt_limit() {
		let dir = ScratchDir::new();
		// A reservation file and a directory of the same name both hold their ports.
		leave_reservation(dir.path(), 1001, "");
		std::fs::create_dir(lock_path(dir.path(), 1002)).unwrap();
		let reservation =
			PortReservation::reserve_in(dir.path(), 3, draw_from(&[1001, 1002, 1003])).unwrap();
		assert_eq!(reservation.port(), 1003);
		assert!(lock_path(dir.path(), 1003).is_file());
		drop(reservation);
		assert!(!lock_path(dir.path(), 1003).exists());
	}

	#[test]
	fn exhausted_attempts_fail_without_taking_a_port() {
		let dir = ScratchDir::new();
		leave_reservation(dir.path(), 1001, "");
		leave_reservation(dir.path(), 1002, "");
		let error = PortReservation::reserve_in(dir.path(), 2, draw_from(&[1001, 1002, 1003]))
			.err()
			.expect("both attempts collide");
		assert_eq!(error.phase(), PgTestHarnessPhase::PortReserve);
		assert_eq!(error.reason(), "port reservation attempts were exhausted");
		assert!(!lock_path(dir.path(), 1003).exists());
	}

	#[test]
	fn an_error_other_than_a_collision_stops_the_search() {
		let dir = ScratchDir::new();
		let missing = dir.path().join("missing");
		let mut draws = 0;
		let error = PortReservation::reserve_in(&missing, 2, || {
			draws += 1;
			Ok(1001)
		})
		.err()
		.expect("the lock directory does not exist");
		assert_eq!(error.phase(), PgTestHarnessPhase::PortReserve);
		assert_eq!(error.reason(), "could not reserve a loopback TCP port");
		assert!(std::error::Error::source(&error).is_some());
		assert_eq!(draws, 1);
	}

	#[cfg(unix)]
	#[test]
	fn a_reservation_that_cannot_be_removed_reports_the_release_failure_and_a_removed_one_succeeds()
	{
		use std::os::unix::fs::PermissionsExt;
		let dir = ScratchDir::new();
		let mut reservation =
			PortReservation::reserve_in(dir.path(), 1, draw_from(&[1001])).unwrap();
		let path = lock_path(dir.path(), 1001);
		// Without write permission, file deletion from this directory fails.
		std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
		let released = reservation.release();
		std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
		let error = released.expect_err("directory permissions prohibit file deletion");
		assert_eq!(error.phase(), PgTestHarnessPhase::PortRelease);
		assert_eq!(
			error.reason(),
			"could not release the loopback TCP port reservation"
		);
		assert!(path.is_file());

		let mut removed = PortReservation::reserve_in(dir.path(), 1, draw_from(&[1002])).unwrap();
		std::fs::remove_file(lock_path(dir.path(), 1002)).unwrap();
		removed.release().unwrap();
	}

	#[test]
	fn a_release_removes_no_file_of_another_reservation() {
		let dir = ScratchDir::new();
		let path = lock_path(dir.path(), 1001);
		let mut first = PortReservation::reserve_in(dir.path(), 1, draw_from(&[1001])).unwrap();
		// Something outside the harness removes the file, and another reservation takes the port.
		std::fs::remove_file(&path).unwrap();
		let second = PortReservation::reserve_in(dir.path(), 1, draw_from(&[1001])).unwrap();
		first.release().unwrap();
		assert!(path.is_file());
		drop(second);
		assert!(!path.exists());
	}

	#[test]
	fn a_reservation_leaves_no_other_file_in_the_lock_directory() {
		let dir = ScratchDir::new();
		let reservation = PortReservation::reserve_in(dir.path(), 1, draw_from(&[1001])).unwrap();
		let names: Vec<String> = std::fs::read_dir(dir.path())
			.unwrap()
			.map(|entry| entry.unwrap().file_name().into_string().unwrap())
			.collect();
		assert_eq!(names, [reservation_file_name(1001)]);
		drop(reservation);
		assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
	}

	/// An identifier in the form that a harness writes, with the random part of the test.
	fn identifier(numbers: &str) -> String {
		format!("{numbers}-0123456789abcdef")
	}

	fn record(numbers: &str) -> HarnessRecord {
		HarnessRecord {
			identifier: identifier(numbers),
			runtime: RecordedRuntime::Address("unix:///run/test.sock".to_owned()),
			containers: RecordedContainers::absent(),
		}
	}

	#[test]
	fn a_created_identifier_has_the_form_that_a_record_accepts() {
		let ports = IdentifierPorts {
			pg_port: 1001,
			first_pgbouncer_port: 1002,
		};
		let created = create_harness_identifier(ports).unwrap();
		let other = create_harness_identifier(ports).unwrap();
		assert_ne!(created, other);
		let text = format!("{created}\nunix:///run/test.sock");
		assert_eq!(
			HarnessRecord::from_text(&text, 1001).map(|record| record.identifier),
			Some(created)
		);
	}

	#[test]
	fn recorded_containers_are_appended_to_the_reservation_of_the_harness() {
		let dir = ScratchDir::new();
		let mut reservation =
			PortReservation::reserve_in(dir.path(), 1, draw_from(&[1001])).unwrap();
		reservation.record_harness(&record("77-1001-1002")).unwrap();
		for (target, recorded) in [
			(
				CreationTarget::PgBouncerTransaction,
				RecordedContainer::PossiblyPresent,
			),
			(
				CreationTarget::PgBouncerStatement,
				RecordedContainer::PossiblyPresent,
			),
			(
				CreationTarget::PgBouncerTransaction,
				RecordedContainer::Absent,
			),
		] {
			reservation.record_container(target, recorded).unwrap();
		}
		let text = std::fs::read_to_string(lock_path(dir.path(), 1001)).unwrap();
		let containers = HarnessRecord::from_text(&text, 1001).unwrap().containers;
		assert_eq!(
			CreationTarget::ALL.map(|target| containers.get(target)),
			[
				RecordedContainer::Absent,
				RecordedContainer::Absent,
				RecordedContainer::PossiblyPresent,
			]
		);
	}

	#[test]
	fn malformed_container_events_make_every_container_possibly_present() {
		let record = HarnessRecord::from_text(
			&format!(
				"{}\nruntime\n+1\n-1\ninvalid\n-2",
				identifier("77-1001-1002")
			),
			1001,
		)
		.unwrap();
		assert_eq!(
			CreationTarget::ALL.map(|target| record.containers.get(target)),
			[
				RecordedContainer::PossiblyPresent,
				RecordedContainer::Absent,
				RecordedContainer::PossiblyPresent,
			]
		);
	}

	#[cfg(unix)]
	#[test]
	fn a_harness_takes_only_a_reservation_file_that_no_process_holds_locked() {
		let dir = ScratchDir::new();
		// A live reservation with its harness's record, a live reservation without one, two
		// reservations that their harnesses left, a directory and a FIFO with a reservation's
		// name, and a file with another name.
		let mut live = PortReservation::reserve_in(dir.path(), 1, draw_from(&[1001])).unwrap();
		live.record_harness(&record("77-1001-1002")).unwrap();
		let other_live = PortReservation::reserve_in(dir.path(), 1, draw_from(&[1002])).unwrap();
		let left_with_record = leave_reservation(
			dir.path(),
			2001,
			&format!("{}\nunix:///run/test.sock", identifier("88-2001-2002")),
		);
		let left_without_record = leave_reservation(dir.path(), 2002, "");
		std::fs::create_dir(lock_path(dir.path(), 3001)).unwrap();
		let fifo = lock_path(dir.path(), 3002);
		assert!(
			std::process::Command::new("mkfifo")
				.arg(&fifo)
				.status()
				.expect("run mkfifo")
				.success()
		);
		std::fs::write(dir.path().join("paranoid-ts-test-port-x.lock"), "99-1-2\nr").unwrap();
		assert_eq!(
			std::fs::read_to_string(lock_path(dir.path(), 1001)).unwrap(),
			format!("{}\nunix:///run/test.sock", identifier("77-1001-1002"))
		);

		let mut paths = reservation_file_paths_in(dir.path());
		paths.sort();
		assert_eq!(
			paths,
			[1001, 1002, 2001, 2002, 3001, 3002].map(|port| lock_path(dir.path(), port))
		);
		let mut taken = Vec::new();
		for path in &paths {
			if let Some(reservation) = live.take_unheld_reservation(path) {
				// While one harness has taken a reservation, no other harness takes it.
				assert!(other_live.take_unheld_reservation(path).is_none());
				taken.push((path.clone(), reservation.harness_record().cloned()));
				reservation.remove();
			}
		}
		assert_eq!(
			taken,
			[
				(left_with_record.clone(), Some(record("88-2001-2002"))),
				(left_without_record.clone(), None),
			]
		);
		assert!(!left_with_record.exists());
		assert!(!left_without_record.exists());
		assert!(lock_path(dir.path(), 1001).is_file());
		assert!(lock_path(dir.path(), 1002).is_file());
		assert!(lock_path(dir.path(), 3001).is_dir());
		assert!(fifo.exists());

		// A reservation that its harness leaves is taken afterward, and one that its harness
		// releases is gone.
		let mut left = PortReservation::reserve_in(dir.path(), 1, draw_from(&[2003])).unwrap();
		left.record_harness(&record("88-2003-2004")).unwrap();
		left.record_container(CreationTarget::Postgres, RecordedContainer::PossiblyPresent)
			.unwrap();
		assert!(
			live.take_unheld_reservation(&lock_path(dir.path(), 2003))
				.is_none()
		);
		left.leave_to_the_next_harness();
		let mut reservation = live
			.take_unheld_reservation(&lock_path(dir.path(), 2003))
			.expect("the reservation that its harness left");
		let mut expected = record("88-2003-2004");
		expected
			.containers
			.set(CreationTarget::Postgres, RecordedContainer::PossiblyPresent);
		assert_eq!(reservation.harness_record(), Some(&expected));
		// A later harness records each removal in the file it has taken.
		reservation.record_absent(CreationTarget::Postgres).unwrap();
		assert_eq!(reservation.harness_record(), Some(&record("88-2003-2004")));
		let text = std::fs::read_to_string(lock_path(dir.path(), 2003)).unwrap();
		assert_eq!(
			HarnessRecord::from_text(&text, 2003).unwrap().containers,
			RecordedContainers::absent()
		);
		reservation.remove();
		let mut released = other_live;
		released.release().unwrap();
		assert!(
			live.take_unheld_reservation(&lock_path(dir.path(), 1002))
				.is_none()
		);
		PortReservation::reserve_in(dir.path(), 1, draw_from(&[2001])).unwrap();
	}

	#[test]
	fn the_text_of_a_reservation_file_is_a_record_only_in_the_form_that_its_harness_writes() {
		assert_eq!(
			HarnessRecord::from_text(&record("3024270-63441-64515").to_text(), 63441),
			Some(record("3024270-63441-64515"))
		);
		assert_eq!(
			HarnessRecord::from_text(&format!("{}\n", identifier("1-63441-2")), 63441),
			Some(HarnessRecord {
				identifier: identifier("1-63441-2"),
				runtime: RecordedRuntime::Address(String::new()),
				containers: RecordedContainers::absent(),
			})
		);
		for text in [
			"",
			"3024270-63441-64515-0123456789abcdef",
			"3024270-63441-64515",
			"3024270\nr",
			"3024270-63441\nr",
			"3024270-63441-64515-0123456789abcdef-1\nr",
			"3024270-63441--0123456789abcdef\nr",
			"3024270-x-64515-0123456789abcdef\nr",
			"3024270-63441-64515-0123456789ABCDEF\nr",
			"3024270-63441-64515-0123456789abcde\nr",
			"3024270-63441-64515-0123456789abcdefa\nr",
			"../1-63441-3-0123456789abcdef\nr",
			// The record of another harness: its PostgreSQL port is not the file's port.
			"3024270-64515-63441-0123456789abcdef\nr",
		] {
			assert_eq!(HarnessRecord::from_text(text, 63441), None, "{text:?}");
		}
	}

	#[test]
	fn a_record_with_a_three_part_identifier_names_every_container_as_possibly_present() {
		for (text, runtime) in [
			(
				"77-1001-1002\nunix:///run/test.sock",
				"unix:///run/test.sock",
			),
			("77-1001-1002\n\n+1\n-1\n+2\n-2\n+4\n-4", ""),
		] {
			assert_eq!(
				HarnessRecord::from_text(text, 1001),
				Some(HarnessRecord {
					identifier: "77-1001-1002".to_owned(),
					runtime: RecordedRuntime::DockerHost(runtime.to_owned()),
					containers: RecordedContainers::possibly_present(),
				}),
				"{text:?}"
			);
		}
		assert_eq!(HarnessRecord::from_text("77-1002-1001\nr", 1001), None);
	}

	#[cfg(unix)]
	#[test]
	fn a_reservation_file_with_a_long_text_is_read_only_in_part() {
		let dir = ScratchDir::new();
		let own = PortReservation::reserve_in(dir.path(), 1, draw_from(&[1001])).unwrap();
		let path = leave_reservation(dir.path(), 2001, &"7".repeat(1 << 20));
		let reservation = own
			.take_unheld_reservation(&path)
			.expect("the file that no process holds");
		assert_eq!(reservation.harness_record(), None);
		reservation.remove();
		assert!(!path.exists());
	}
}
