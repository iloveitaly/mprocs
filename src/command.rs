use std::fmt;

use futures::future::try_join_all;
use serde::{Deserialize, Serialize};

use crate::kernel::{
  kernel_message::{KernelCommand, TaskContext, TaskSelector},
  task::TaskId,
  task_key::TaskSpaceId,
  task_path::TaskPath,
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum Command {
  Batch { commands: Vec<Command> },
  Quit,
  Start { target: Target },
  Stop { target: Target },
  Down { target: Target },
  Kill { target: Target },
  Veto { target: Target },
  Restart { target: Target },
  ForceRestart { target: Target },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Target {
  Selector(String),
  Id { id: TaskId },
  All { all: TaskSpaceId },
}

impl Target {
  pub fn id(id: TaskId) -> Self {
    Self::Id { id }
  }

  fn selector(&self) -> Result<TaskSelector, CommandError> {
    match self {
      Target::Id { id } => Ok(TaskSelector::Id(*id)),
      Target::All { all } => Ok(TaskSelector::All(all.clone())),
      Target::Selector(target) => parse_selector(target),
    }
  }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CommandResult {
  None,
  Matched(usize),
}

#[derive(Debug)]
pub enum CommandError {
  InvalidTarget(String),
  KernelClosed,
}

impl fmt::Display for CommandError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      CommandError::InvalidTarget(message) => f.write_str(message),
      CommandError::KernelClosed => {
        f.write_str("kernel closed the reply channel")
      }
    }
  }
}

impl std::error::Error for CommandError {}

pub async fn execute(
  pc: &TaskContext,
  command: &Command,
) -> Result<CommandResult, CommandError> {
  let mut replies = Vec::new();
  dispatch(pc, command, true, &mut replies)?;
  let outcomes = try_join_all(replies)
    .await
    .map_err(|_| CommandError::KernelClosed)?;
  if matches!(command, Command::Batch { .. }) {
    Ok(CommandResult::None)
  } else {
    Ok(
      outcomes
        .into_iter()
        .next()
        .map(CommandResult::Matched)
        .unwrap_or(CommandResult::None),
    )
  }
}

pub fn issue(pc: &TaskContext, command: &Command) -> Result<(), CommandError> {
  dispatch(pc, command, false, &mut Vec::new())
}

fn dispatch(
  pc: &TaskContext,
  command: &Command,
  reply: bool,
  replies: &mut Vec<tokio::sync::oneshot::Receiver<usize>>,
) -> Result<(), CommandError> {
  match command {
    Command::Batch { commands } => {
      for command in commands {
        dispatch(pc, command, reply, replies)?;
      }
    }
    Command::Quit => pc.send(KernelCommand::Quit),
    Command::Start { target } => {
      act(pc, target, KernelCommand::Start, reply, replies)?;
    }
    Command::Stop { target } => {
      act(pc, target, KernelCommand::Stop, reply, replies)?;
    }
    Command::Down { target } => {
      act(pc, target, KernelCommand::Down, reply, replies)?;
    }
    Command::Kill { target } => {
      act(pc, target, KernelCommand::Kill, reply, replies)?;
    }
    Command::Veto { target } => {
      act(pc, target, KernelCommand::Veto, reply, replies)?;
    }
    Command::Restart { target } => {
      act(pc, target, KernelCommand::Restart, reply, replies)?;
    }
    Command::ForceRestart { target } => {
      act(pc, target, KernelCommand::ForceRestart, reply, replies)?;
    }
  }
  Ok(())
}

fn act(
  pc: &TaskContext,
  target: &Target,
  make: impl FnOnce(
    TaskSelector,
    Option<tokio::sync::oneshot::Sender<usize>>,
  ) -> KernelCommand,
  reply: bool,
  replies: &mut Vec<tokio::sync::oneshot::Receiver<usize>>,
) -> Result<(), CommandError> {
  let selector = target.selector()?;
  let ack = if reply {
    let (tx, rx) = tokio::sync::oneshot::channel();
    replies.push(rx);
    Some(tx)
  } else {
    None
  };
  pc.send(make(selector, ack));
  Ok(())
}

fn parse_selector(target: &str) -> Result<TaskSelector, CommandError> {
  let (space, selector) = match target.strip_prefix('@') {
    Some(qualified) => {
      let Some((space, selector)) = qualified.split_once('/') else {
        return Err(CommandError::InvalidTarget(
          "qualified target must contain '/'".to_string(),
        ));
      };
      let space =
        TaskSpaceId::new(space).map_err(CommandError::InvalidTarget)?;
      (space, selector)
    }
    None => (TaskSpaceId::default_space(), target),
  };

  if let Some(tag) = selector.strip_prefix('+') {
    if tag.is_empty() {
      return Err(CommandError::InvalidTarget("tag is empty".to_string()));
    }
    return Ok(TaskSelector::Tag(space, tag.to_string()));
  }
  TaskPath::check_glob(selector)
    .map_err(|err| CommandError::InvalidTarget(err.to_string()))?;
  Ok(TaskSelector::Glob(space, selector.to_string()))
}

#[cfg(test)]
mod tests {
  use crate::kernel::{
    kernel::Kernel,
    task::{TargetTask, TaskDef},
  };

  use super::*;

  #[test]
  fn serde_round_trip() {
    let command = Command::Batch {
      commands: vec![
        Command::Start {
          target: Target::Selector("+dev".to_string()),
        },
        Command::Stop {
          target: Target::id(TaskId(7)),
        },
      ],
    };
    let yaml = serde_yaml::to_string(&command).unwrap();
    assert_eq!(serde_yaml::from_str::<Command>(&yaml).unwrap(), command);
  }

  #[test]
  fn stable_json_shape() {
    assert_eq!(
      serde_json::to_string(&Command::Start {
        target: Target::Selector("+dev".to_string()),
      })
      .unwrap(),
      r#"{"command":"start","target":"+dev"}"#
    );
    assert_eq!(
      serde_json::to_string(&Command::ForceRestart {
        target: Target::id(TaskId(7)),
      })
      .unwrap(),
      r#"{"command":"force-restart","target":{"id":7}}"#
    );
  }

  #[test]
  fn parses_space_and_tag() {
    assert!(matches!(
      Target::Selector("@dekit/+ui".to_string()).selector(),
      Ok(TaskSelector::Tag(space, tag))
        if space == TaskSpaceId::dekit() && tag == "ui"
    ));
  }

  #[tokio::test]
  async fn executes_selector_command() {
    let kernel = Kernel::new();
    let pc = kernel.context();
    pc.register(
      TaskDef {
        path: Some(TaskPath::new("api").unwrap()),
        ..TaskDef::default()
      },
      Box::new(|_| Box::new(TargetTask)),
    );
    let handle = tokio::spawn(kernel.run());

    let result = execute(
      &pc,
      &Command::Start {
        target: Target::Selector("api".to_string()),
      },
    )
    .await
    .unwrap();
    assert_eq!(result, CommandResult::Matched(1));

    pc.send(KernelCommand::Quit);
    handle.await.unwrap();
  }
}
