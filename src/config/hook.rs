use anyhow::Result;

use crate::cfg::CfgObj;
use crate::command::{Command, issue};
use crate::console::action::Action;
use crate::kernel::kernel_message::{TaskContext, TaskSelector, TaskSender};

#[derive(Clone)]
pub enum Hook {
  Command(Command),
  LegacyAction(Action),
}

impl Hook {
  pub fn run(&self, pc: &TaskContext, console: &TaskSender) {
    match self {
      Hook::Command(command) => {
        if let Err(err) = issue(pc, command) {
          log::error!("Hook command failed: {err}");
        }
      }
      Hook::LegacyAction(action) => console.send(action.clone()),
    }
  }
}

/// Runs `hook` each time the selected set goes from active to idle.
pub fn watch_idle(
  pc: &TaskContext,
  selector: TaskSelector,
  hook: Hook,
  console: TaskSender,
) {
  let mut watch = pc.watch_active(selector);
  let pc = pc.clone();
  tokio::spawn(async move {
    while let Some(active) = watch.recv().await {
      if !active {
        hook.run(&pc, &console);
      }
    }
  });
}

pub(crate) fn hook_from_cfg(
  obj: &CfgObj<'_>,
  key: &str,
) -> Result<Option<Hook>> {
  match obj.get(key) {
    Some(node) => {
      let command: Command = serde_yaml::from_value(node.raw().clone())?;
      Ok(Some(Hook::Command(command)))
    }
    None => Ok(None),
  }
}
