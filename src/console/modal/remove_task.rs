use crate::console::action::Action;
use crate::console::{client::ClientId, keymap::Keymap};
use crate::kernel::task::TaskId;
use crate::term::{
  Grid,
  attrs::Attrs,
  grid::BorderType,
  key::{Key, KeyCode},
};

use super::modal::{Modal, ModalResult};

pub struct RemoveTaskModal {
  pub id: TaskId,
}

impl Modal for RemoveTaskModal {
  fn handle_key(&mut self, key: &Key, _client_id: ClientId) -> ModalResult {
    if !key.mods.is_empty() {
      return ModalResult::Keep;
    }
    match key.code {
      KeyCode::Char('y') => {
        ModalResult::Run(Action::RemoveTask { id: self.id })
      }
      KeyCode::Char('n') | KeyCode::Esc => ModalResult::Close,
      _ => ModalResult::Keep,
    }
  }

  fn size(&self) -> (u16, u16) {
    (36, 3)
  }

  fn render(&mut self, grid: &mut Grid, _keymap: &Keymap) {
    let area = self.area(grid.area());
    grid.draw_block(area, &BorderType::Thick.chars(), Attrs::default());
    let inner = area.inner(1);
    grid.fill_area(inner, ' ', Attrs::default());
    grid.draw_text(inner, "Remove task? (y/n)", Attrs::default());
  }
}
