use super::TerminationScope;
use rustix::process::{
	Pid, Signal, WaitId, WaitIdOptions, kill_process, kill_process_group, waitid,
};
use std::{
	io,
	os::unix::process::{CommandExt, ExitStatusExt},
	process::{Child, Command, ExitStatus},
	sync::mpsc,
};
use tokio::{
	process::{ChildStderr, ChildStdin, ChildStdout},
	signal::unix::{Signal as SignalStream, SignalKind, signal},
	sync::oneshot,
};

type Spawned = (
	Backend,
	Option<ChildStdin>,
	Option<ChildStdout>,
	Option<ChildStderr>,
);

pub(super) struct Backend {
	ownership: ChildOwnership,
	id: Pid,
	termination_scope: TerminationScope,
	signal: SignalStream,
}

/// A child for a reaper thread to reap, and the sender for the reaping result.
type ReapRequest = (Child, oneshot::Sender<io::Result<ExitStatus>>);

/// Ownership of the direct child. Each child has a dedicated reaper thread from
/// before the child starts until that thread reaps the child.
enum ChildOwnership {
	/// This backend owns the unreaped child and the sender that transfers the
	/// child to its reaper thread.
	Owned {
		child: Child,
		reaper: mpsc::Sender<ReapRequest>,
	},
	/// The reaper thread owns the child and reaps it.
	TransferredToReaper,
}
impl Backend {
	pub(super) fn spawn(
		mut cmd: Command,
		termination_scope: TerminationScope,
	) -> io::Result<Spawned> {
		let signal = signal(SignalKind::child())?;
		let (reaper, receiver) = mpsc::channel::<ReapRequest>();
		// Allocate the reaper before spawning so thread allocation cannot orphan a child.
		std::thread::Builder::new()
			.name("process-reaper".into())
			.spawn(move || {
				if let Ok((mut child, result)) = receiver.recv() {
					let _ = result.send(child.wait());
				}
			})?;
		if termination_scope == TerminationScope::ProcessGroup {
			cmd.process_group(0);
		}
		let child = cmd.spawn()?;
		let id =
			Pid::from_raw(child.id() as i32).expect("a spawned child has a positive process ID");
		let mut backend = Self {
			ownership: ChildOwnership::Owned { child, reaper },
			id,
			termination_scope,
			signal,
		};
		let ChildOwnership::Owned { child, .. } = &mut backend.ownership else {
			unreachable!("a spawned child is owned before reaping");
		};
		let stdin = child.stdin.take().map(ChildStdin::from_std).transpose()?;
		let stdout = child.stdout.take().map(ChildStdout::from_std).transpose()?;
		let stderr = child.stderr.take().map(ChildStderr::from_std).transpose()?;
		Ok((backend, stdin, stdout, stderr))
	}
	pub(super) fn id(&self) -> u32 {
		self.id.as_raw_nonzero().get() as u32
	}
	pub(super) async fn wait_for_exit(&mut self) -> io::Result<ExitStatus> {
		assert!(
			matches!(self.ownership, ChildOwnership::Owned { .. }),
			"exit observation precedes transfer to the reaper"
		);
		loop {
			match waitid(
				WaitId::Pid(self.id),
				WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
			) {
				Ok(Some(status)) => {
					if let Some(code) = status.exit_status() {
						return Ok(ExitStatus::from_raw(code << 8));
					}
					if let Some(signal) = status.terminating_signal() {
						return Ok(ExitStatus::from_raw(
							signal | if status.dumped() { 0x80 } else { 0 },
						));
					}
				}
				Ok(None) => {}
				Err(rustix::io::Errno::INTR) => continue,
				Err(error) => return Err(error.into()),
			}
			self.signal
				.recv()
				.await
				.ok_or_else(|| io::Error::other("SIGCHLD listener closed"))?;
		}
	}
	fn send_signal(&self, signal: Signal) -> io::Result<()> {
		if matches!(self.ownership, ChildOwnership::TransferredToReaper) {
			return Ok(());
		}
		let result = match self.termination_scope {
			TerminationScope::ChildOnly => kill_process(self.id, signal),
			TerminationScope::ProcessGroup => kill_process_group(self.id, signal),
		};
		match result {
			Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
			Err(error) => Err(error.into()),
		}
	}
	pub(super) fn request_termination(&mut self) -> io::Result<()> {
		self.send_signal(Signal::TERM)
	}
	pub(super) fn force_termination(&mut self) -> io::Result<()> {
		let group_result = self.send_signal(Signal::KILL);
		if matches!(self.ownership, ChildOwnership::Owned { .. })
			&& self.termination_scope == TerminationScope::ProcessGroup
		{
			// The direct child remains owned even if it changes its process group.
			match kill_process(self.id, Signal::KILL) {
				Ok(()) | Err(rustix::io::Errno::SRCH) => {}
				Err(error) => return Err(error.into()),
			}
		}
		group_result
	}
	pub(super) async fn reap(&mut self) -> io::Result<ExitStatus> {
		let (result, reaped) = oneshot::channel();
		self.transfer_to_reaper(result);
		reaped
			.await
			.map_err(|_| io::Error::other("process reaper stopped"))?
	}
	fn transfer_to_reaper(&mut self, result: oneshot::Sender<io::Result<ExitStatus>>) {
		if let ChildOwnership::Owned { child, reaper } =
			std::mem::replace(&mut self.ownership, ChildOwnership::TransferredToReaper)
		{
			reaper
				.send((child, result))
				.expect("the reaper waits until its sender closes or supplies a child");
		}
	}
}
impl Drop for Backend {
	fn drop(&mut self) {
		// The child remains unreaped until this final signal, preventing process ID reuse.
		let _ = self.force_termination();
		// No caller waits for the reaping result after the backend drops.
		let (result, _) = oneshot::channel();
		self.transfer_to_reaper(result);
	}
}
