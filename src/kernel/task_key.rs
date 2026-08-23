use std::fmt;

use serde::{Deserialize, Serialize};

use super::task_path::TaskPath;

#[derive(
  Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct TaskSpaceId(String);

impl TaskSpaceId {
  pub fn default_space() -> Self {
    Self(String::new())
  }

  pub fn dekit() -> Self {
    Self("dekit".to_string())
  }

  pub fn new(name: impl Into<String>) -> Result<Self, String> {
    let name = name.into();
    if name.is_empty() {
      return Err("space name is empty".to_string());
    }
    if name
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
      Ok(Self(name))
    } else {
      Err("space name contains an invalid character".to_string())
    }
  }

  pub fn is_default(&self) -> bool {
    self.0.is_empty()
  }

  pub fn is_reserved(&self) -> bool {
    self == &Self::dekit()
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }
}

impl Default for TaskSpaceId {
  fn default() -> Self {
    Self::default_space()
  }
}

impl fmt::Display for TaskSpaceId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

#[derive(
  Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct TaskKey {
  pub space: TaskSpaceId,
  pub path: TaskPath,
}

impl TaskKey {
  pub fn new(space: TaskSpaceId, path: TaskPath) -> Self {
    Self { space, path }
  }

  pub fn default_space(path: TaskPath) -> Self {
    Self::new(TaskSpaceId::default_space(), path)
  }
}

impl fmt::Display for TaskKey {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if self.space.is_default() {
      self.path.fmt(f)
    } else {
      write!(f, "@{}/{}", self.space, self.path)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn display_key() {
    let path = TaskPath::new("console").unwrap();
    assert_eq!(TaskKey::default_space(path.clone()).to_string(), "console");
    assert_eq!(
      TaskKey::new(TaskSpaceId::dekit(), path).to_string(),
      "@dekit/console"
    );
  }

  #[test]
  fn validate_space() {
    assert!(TaskSpaceId::new("worktree-a").is_ok());
    assert!(TaskSpaceId::new("").is_err());
    assert!(TaskSpaceId::new("bad/name").is_err());
    assert!(TaskSpaceId::new("*").is_err());
  }
}
