use crate::console::{
  keymap::KeymapGroup, task_view::TaskView, widgets::list::ListState,
};
use crate::kernel::task::TaskId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
  Tasks,
  Term,
  TermZoom,
}

impl Scope {
  pub fn toggle(&self) -> Self {
    match self {
      Scope::Tasks => Scope::Term,
      Scope::Term => Scope::Tasks,
      Scope::TermZoom => Scope::Tasks,
    }
  }

  pub fn is_zoomed(&self) -> bool {
    match self {
      Scope::Tasks => false,
      Scope::Term => false,
      Scope::TermZoom => true,
    }
  }

  pub fn is_term(&self) -> bool {
    match self {
      Scope::Tasks => false,
      Scope::Term => true,
      Scope::TermZoom => true,
    }
  }
}

pub struct State {
  pub scope: Scope,
  pub tasks: Vec<TaskView>,
  pub tasks_list: ListState,
  pub hide_keymap_window: bool,
  pub quitting: bool,
}

impl State {
  pub fn selected(&self) -> usize {
    self.tasks_list.selected()
  }

  pub fn select(&mut self, index: usize) {
    self.tasks_list.select(index, self.tasks.len());
  }

  pub fn current_task(&self) -> Option<&TaskView> {
    self.tasks.get(self.tasks_list.selected())
  }

  pub fn task_mut(&mut self, id: TaskId) -> Option<&mut TaskView> {
    self.tasks.iter_mut().find(|t| t.id == id)
  }

  pub fn keymap_group(&self) -> KeymapGroup {
    match self.scope {
      Scope::Tasks => KeymapGroup::Tasks,
      Scope::Term | Scope::TermZoom => match self.current_task() {
        Some(task) if task.present.is_some() => KeymapGroup::Copy,
        _ => KeymapGroup::Term,
      },
    }
  }
}
