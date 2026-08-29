use std::collections::HashMap;

use crate::console::action::Action;
use crate::term::key::Key;

#[derive(Default)]
pub struct Bindings {
  pub by_key: HashMap<Key, Action>,
  pub by_action: HashMap<Action, Key>,
}

#[derive(Default)]
pub struct Keymap {
  tasks: Bindings,
  term: Bindings,
  copy: Bindings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeymapGroup {
  Tasks,
  Term,
  Copy,
}

impl Keymap {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn group(&self, group: KeymapGroup) -> &Bindings {
    match group {
      KeymapGroup::Tasks => &self.tasks,
      KeymapGroup::Term => &self.term,
      KeymapGroup::Copy => &self.copy,
    }
  }

  pub fn bind(&mut self, group: KeymapGroup, key: Key, action: Action) {
    let bindings = match group {
      KeymapGroup::Tasks => &mut self.tasks,
      KeymapGroup::Term => &mut self.term,
      KeymapGroup::Copy => &mut self.copy,
    };
    bindings.by_key.insert(key, action.clone());
    bindings.by_action.insert(action, key);
  }

  pub fn action(&self, group: KeymapGroup, key: &Key) -> Option<&Action> {
    self.group(group).by_key.get(key)
  }

  pub fn key(&self, group: KeymapGroup, action: &Action) -> Option<&Key> {
    self.group(group).by_action.get(action)
  }
}
