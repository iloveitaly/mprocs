use anyhow::Result;

use crate::cfg::CfgObj;
use crate::command::Command;
use crate::console::action::Action;

#[derive(Clone)]
pub enum Hook {
  Command(Command),
  LegacyAction(Action),
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
