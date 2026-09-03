use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::kernel::{
  kernel_message::{SpaceSelector, TaskSelector},
  task::TaskId,
  task_key::{TaskKey, TaskSpaceId},
  task_path::{TaskPath, is_valid_component_char},
};

/// `[runner::][@space/]selector`, or the exact runtime id `{id}`. See
/// TARGETS.md. Parsed eagerly at every boundary; the runner is resolved
/// by the client and never reaches a kernel.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Target {
  Id(TaskId),
  Select {
    runner: Option<Runner>,
    space: SpaceSelector,
    selector: Selector,
  },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Runner {
  /// Built-in (`host`, `project`, `cloud`) or user alias.
  Name(String),
  /// `/abs`, `~/rel`, or `./rel`: a local project runner.
  Path(String),
  /// `ssh://...`, `cloud://...`.
  Url(String),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Selector {
  /// A path or a glob; an exact path is a glob without wildcards.
  Glob(String),
  Tag(String),
}

#[derive(Debug, Eq, PartialEq)]
pub struct InvalidTarget(pub String);

impl fmt::Display for InvalidTarget {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "invalid target: {}", self.0)
  }
}

impl std::error::Error for InvalidTarget {}

impl Target {
  pub fn glob(pattern: &str) -> Self {
    Target::Select {
      runner: None,
      space: SpaceSelector::One(TaskSpaceId::default_space()),
      selector: Selector::Glob(pattern.to_string()),
    }
  }

  pub fn tag(tag: &str) -> Self {
    Target::Select {
      runner: None,
      space: SpaceSelector::One(TaskSpaceId::default_space()),
      selector: Selector::Tag(tag.to_string()),
    }
  }

  pub fn runner(&self) -> Option<&Runner> {
    match self {
      Target::Id(_) => None,
      Target::Select { runner, .. } => runner.as_ref(),
    }
  }

  /// The same target with its runner dropped, once the client has
  /// resolved which runner it is talking to.
  pub fn without_runner(self) -> Self {
    match self {
      Target::Id(id) => Target::Id(id),
      Target::Select {
        space, selector, ..
      } => Target::Select {
        runner: None,
        space,
        selector,
      },
    }
  }

  /// The kernel selector. A runner qualifier is an error here: kernels
  /// only see targets a client already routed to them.
  pub fn selector(&self) -> Result<TaskSelector, InvalidTarget> {
    if self.runner().is_some() {
      return Err(InvalidTarget(
        "runner qualifiers are resolved by the client".to_string(),
      ));
    }
    Ok(self.local_selector())
  }

  /// The selector part, ignoring the runner.
  fn local_selector(&self) -> TaskSelector {
    match self {
      Target::Id(id) => TaskSelector::Id(*id),
      Target::Select {
        space, selector, ..
      } => match selector {
        Selector::Glob(pattern) => {
          TaskSelector::Glob(space.clone(), pattern.clone())
        }
        Selector::Tag(tag) => TaskSelector::Tag(space.clone(), tag.clone()),
      },
    }
  }

  /// The exact key for a target that names one task in one space.
  pub fn key(&self) -> Result<TaskKey, InvalidTarget> {
    let Target::Select {
      runner: None,
      space: SpaceSelector::One(space),
      selector: Selector::Glob(pattern),
    } = self
    else {
      return Err(InvalidTarget("expected a single task path".to_string()));
    };
    let path = TaskPath::new(pattern.as_str())
      .map_err(|_| InvalidTarget("expected a single task path".to_string()))?;
    Ok(TaskKey::new(space.clone(), path))
  }
}

impl FromStr for Target {
  type Err = InvalidTarget;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    let (runner, reference) = match s.rsplit_once("::") {
      Some((runner, reference)) => (Some(parse_runner(runner)?), reference),
      None => (None, s),
    };
    let (space, selector) = match reference.strip_prefix('@') {
      Some(qualified) => {
        let Some((space, selector)) = qualified.split_once('/') else {
          return Err(InvalidTarget(
            "a space qualifier must be followed by '/'".to_string(),
          ));
        };
        let space = if space == "*" {
          SpaceSelector::Any
        } else {
          SpaceSelector::One(TaskSpaceId::new(space).map_err(InvalidTarget)?)
        };
        (space, selector)
      }
      None => (SpaceSelector::One(TaskSpaceId::default_space()), reference),
    };
    let selector = match selector.strip_prefix('+') {
      Some(tag) => {
        if tag.is_empty() || !tag.chars().all(is_valid_component_char) {
          return Err(InvalidTarget(format!("bad tag '+{}'", tag)));
        }
        Selector::Tag(tag.to_string())
      }
      None => {
        TaskPath::check_glob(selector)
          .map_err(|err| InvalidTarget(err.to_string()))?;
        Selector::Glob(selector.to_string())
      }
    };
    Ok(Target::Select {
      runner,
      space,
      selector,
    })
  }
}

pub fn parse_runner(runner: &str) -> Result<Runner, InvalidTarget> {
  if runner.is_empty() {
    return Err(InvalidTarget("runner is empty".to_string()));
  }
  if runner == "*" {
    return Err(InvalidTarget(
      "fleet-wide targeting ('*::') is reserved".to_string(),
    ));
  }
  if runner.contains('#') {
    return Err(InvalidTarget(
      "sub-world selectors ('runner#sub') are reserved".to_string(),
    ));
  }
  if runner.starts_with("git+") {
    return Err(InvalidTarget(
      "repo runners ('git+') are reserved".to_string(),
    ));
  }
  if runner.starts_with('/')
    || runner.starts_with("~/")
    || runner.starts_with("./")
  {
    return Ok(Runner::Path(runner.to_string()));
  }
  if runner.contains("://") {
    return Ok(Runner::Url(runner.to_string()));
  }
  if runner
    .chars()
    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
  {
    return Ok(Runner::Name(runner.to_string()));
  }
  Err(InvalidTarget(format!("bad runner '{}'", runner)))
}

impl fmt::Display for Runner {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Runner::Name(s) | Runner::Path(s) | Runner::Url(s) => f.write_str(s),
    }
  }
}

impl fmt::Display for Target {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if let Some(runner) = self.runner() {
      write!(f, "{}::", runner)?;
    }
    self.local_selector().fmt(f)
  }
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum Wire {
  Select(String),
  Id { id: TaskId },
}

impl Serialize for Target {
  fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    match self {
      Target::Id(id) => Wire::Id { id: *id }.serialize(serializer),
      Target::Select { .. } => {
        Wire::Select(self.to_string()).serialize(serializer)
      }
    }
  }
}

impl<'de> Deserialize<'de> for Target {
  fn deserialize<D: Deserializer<'de>>(
    deserializer: D,
  ) -> Result<Self, D::Error> {
    match Wire::deserialize(deserializer)? {
      Wire::Id { id } => Ok(Target::Id(id)),
      Wire::Select(s) => s.parse().map_err(serde::de::Error::custom),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parse(s: &str) -> Target {
    s.parse().unwrap_or_else(|err| panic!("{s}: {err}"))
  }

  #[test]
  fn parses_examples_from_targets_md() {
    for s in [
      "tmp/proc1",
      "+dev",
      "host::emacs-server",
      "project::@dekit/console",
      "prod::+ci",
      "~/dev/proj::web/*",
      "cloud://pvolok/proj::tmp/proc1",
      "ssh://ubuntu@host/~/dev/proj::@dekit/console",
      "@*/app",
      "**",
    ] {
      assert_eq!(parse(s).to_string(), s);
    }
  }

  #[test]
  fn splits_runner_at_last_separator() {
    let target = parse("ssh://[::1]:2222/~/x::web");
    assert_eq!(
      target.runner(),
      Some(&Runner::Url("ssh://[::1]:2222/~/x".to_string()))
    );
  }

  #[test]
  fn runner_kinds() {
    assert_eq!(
      parse("host::a").runner(),
      Some(&Runner::Name("host".into()))
    );
    assert_eq!(parse("/p::a").runner(), Some(&Runner::Path("/p".into())));
    assert_eq!(parse("./p::a").runner(), Some(&Runner::Path("./p".into())));
    assert_eq!(
      parse("cloud://o/p::a").runner(),
      Some(&Runner::Url("cloud://o/p".into()))
    );
  }

  #[test]
  fn reserved_syntax_is_an_error() {
    for s in [
      "*::web",
      "proj#main::web",
      "#main::web",
      "git+https://h/o/r::@fs/x",
      "::web",
      "@dekit",
      "@dekit/",
      "+",
      "+bad tag",
      "web*",
      "a b",
    ] {
      assert!(s.parse::<Target>().is_err(), "{s} should not parse");
    }
  }

  #[test]
  fn selector_and_key() {
    assert!(matches!(
      parse("@dekit/+ui").selector(),
      Ok(TaskSelector::Tag(SpaceSelector::One(space), tag))
        if space == TaskSpaceId::dekit() && tag == "ui"
    ));
    assert!(matches!(
      parse("@*/app").selector(),
      Ok(TaskSelector::Glob(SpaceSelector::Any, pattern)) if pattern == "app"
    ));
    assert!(parse("host::a").selector().is_err());
    assert_eq!(
      parse("@dekit/console").key().unwrap(),
      TaskKey::new(TaskSpaceId::dekit(), TaskPath::new("console").unwrap())
    );
    assert!(parse("web/*").key().is_err());
    assert!(parse("+dev").key().is_err());
    assert!(parse("@*/app").key().is_err());
  }

  #[test]
  fn serde_forms() {
    let select = parse("prod::@dekit/+ui");
    assert_eq!(
      serde_json::to_string(&select).unwrap(),
      r#""prod::@dekit/+ui""#
    );
    assert_eq!(
      serde_json::from_str::<Target>(r#""prod::@dekit/+ui""#).unwrap(),
      select
    );
    assert_eq!(
      serde_json::to_string(&Target::Id(TaskId(7))).unwrap(),
      r#"{"id":7}"#
    );
    assert_eq!(
      serde_json::from_str::<Target>(r#"{"id":7}"#).unwrap(),
      Target::Id(TaskId(7))
    );
    assert!(serde_json::from_str::<Target>(r#""*::web""#).is_err());
  }
}
