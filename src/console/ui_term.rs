use crate::console::state::State;
use crate::term::{
  Color, Grid, Screen,
  attrs::Attrs,
  grid::{BorderType, Pos, Rect},
};

pub fn render_term(area: Rect, grid: &mut Grid, state: &State) {
  if area.width < 3 || area.height < 3 {
    return;
  }

  let active = state.scope.is_term();

  let Some(task) = state.current_task() else {
    return;
  };

  let border = if active {
    BorderType::Thick
  } else {
    BorderType::Plain
  };
  grid.draw_block(area, &border.chars(), Attrs::default());

  let handle = task.present.as_ref().unwrap_or(&task.vt);
  let Ok(screen) = handle.read() else {
    return;
  };
  let screen = &*screen;

  let mut top_line = Rect {
    x: area.x + 1,
    y: area.y,
    width: area.width - 2,
    height: 1,
  };
  let r =
    grid.draw_text(top_line, "Terminal", Attrs::default().set_bold(active));
  top_line = top_line.move_left(r.width as i32);
  let title = screen.title();
  if !title.is_empty() {
    let r = grid.draw_text(top_line, " ", Attrs::default());
    top_line = top_line.move_left(r.width as i32);
    let _r =
      grid.draw_text(top_line, title, Attrs::default().fg(Color::BRIGHT_BLACK));
  }

  let inner = area.inner(1);
  render_screen(screen, inner, grid);

  if active && !screen.hide_cursor() {
    let (row, col) = screen.cursor_position();
    grid.cursor_pos = Some(Pos {
      col: inner.x + col,
      row: inner.y + row,
    });
    grid.cursor_style = screen.cursor_style();
  }
}

fn render_screen(screen: &Screen, area: Rect, grid: &mut Grid) {
  for row in 0..area.height {
    for col in 0..area.width {
      let Some(to_cell) = grid.drawing_cell_mut(Pos {
        col: area.x + col,
        row: area.y + row,
      }) else {
        continue;
      };
      if let Some(cell) = screen.cell(row, col) {
        *to_cell = cell.clone();
        if !cell.has_contents() {
          to_cell.set_str(" ");
        }
      }
    }
  }
}
