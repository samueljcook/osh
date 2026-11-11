use super::TerminationScope;
use process_wrap::tokio::{ChildWrapper, CommandWrap, JobObject, KillOnDrop};
use std::{
	io,
	process::{Command, ExitStatus},
};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};

type Spawned = (
	Backend,
	Option<ChildStdin>,
	Option<ChildStdout>,
	Option<ChildStderr>,
);
pub(super) struct Backend {
	child: Box<dyn ChildWrapper>,
	termination_scope: TerminationScope,
	id: u32,
}
impl Backend {
	pub(super) fn spawn(cmd: Command, termination_scope: TerminationScope) -> io::Result<Spawned> {
		let mut cmd = CommandWrap::from(tokio::process::Command::from(cmd));
		// This sets both the child kill policy and the job kill-on-close policy.
		cmd.wrap(KillOnDrop);
		if termination_scope == TerminationScope::ProcessGroup {
			cmd.wrap(JobObject);
		}
		let mut child = cmd.spawn()?;
		let id = child
			.id()
			.ok_or_else(|| io::Error::other("spawned process has no ID"))?;
		let stdin = child.stdin().take();
		let stdout = child.stdout().take();
		let stderr = child.stderr().take();
		Ok((
			Self {
				child,
				termination_scope,
				id,
			},
			stdin,
			stdout,
			stderr,
		))
	}
	pub(super) fn id(&self) -> u32 {
		self.id
	}
	pub(super) async fn wait_for_exit(&mut self) -> io::Result<ExitStatus> {
		// Observe the leader without waiting for the job's descendants.
		match self.termination_scope {
			TerminationScope::ProcessGroup => self.child.inner_mut().wait().await,
			TerminationScope::ChildOnly => self.child.wait().await,
		}
	}
	pub(super) fn force_termination(&mut self) -> io::Result<()> {
		self.child.start_kill()
	}
	pub(super) async fn reap(&mut self) -> io::Result<ExitStatus> {
		self.wait_for_exit().await
	}
}
