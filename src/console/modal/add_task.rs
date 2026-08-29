use tui_input::Input;

use crate::console::action::Action;
use crate::console::{
  client::ClientId,
  keymap::Keymap,
  widgets::text_input::{render_text_input, to_input_request},
};
use crate::term::{
  Grid,
  attrs::Attrs,
  grid::{BorderType, Rect},
  key::{Key, KeyCode},
};

use super::modal::{Modal, ModalResult};

#[derive(Default)]
pub struct AddTaskModal {
  input: Input,
}

impl Modal for AddTaskModal {
  fn handle_key(&mut self, key: &Key, _client_id: ClientId) -> ModalResult {
    match key.code {
      KeyCode::Enter if key.mods.is_empty() => {
        return ModalResult::Run(Action::AddTask {
          cmd: self.input.value().to_string(),
          name: None,
        });
      }
      KeyCode::Esc if key.mods.is_empty() => return ModalResult::Close,
      _ => (),
    }
    if let Some(req) = to_input_request(key) {
      self.input.handle(req);
    }
    ModalResult::Keep
  }

  fn size(&self) -> (u16, u16) {
    (42, 3)
  }

  fn render(&mut self, grid: &mut Grid, _keymap: &Keymap) {
    let area = self.area(grid.area());
    grid.draw_block(area, &BorderType::Plain.chars(), Attrs::default());
    grid.draw_text(
      Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1),
      "Add task",
      Attrs::default(),
    );
    grid.cursor_pos = Some(render_text_input(&self.input, area.inner(1), grid));
  }
}
