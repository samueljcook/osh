use super::{TASK_PAUSE_KEY_PREFIX, TaskName};

#[doc(hidden)]
pub fn paused_task_key(task_name: &TaskName) -> String {
	format!("{TASK_PAUSE_KEY_PREFIX}{}", task_name.as_str())
}
