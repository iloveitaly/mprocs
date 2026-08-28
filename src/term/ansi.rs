use super::{attrs::Attrs, screen::Screen, vt::emit};

/// Render the screen contents as ANSI-styled text, one line per row,
/// trailing whitespace trimmed.
pub fn render_screen_ansi(screen: &Screen) -> String {
  let size = screen.size();
  let mut out: Vec<u8> = Vec::new();
  let mut brush = Attrs::default();
  let mut line: Vec<u8> = Vec::new();

  for row in 0..size.height {
    if row > 0 {
      out.extend_from_slice(b"\r\n");
    }
    line.clear();
    let mut line_brush = brush;

    for col in 0..size.width {
      let cell = match screen.cell(row, col) {
        Some(c) => c,
        None => continue,
      };
      let attrs = *cell.attrs();
      emit::sgr(&mut line, line_brush, attrs);
      line_brush = attrs;

      let c = if cell.width() > 0 {
        cell.contents()
      } else {
        " "
      };
      line.extend_from_slice(c.as_bytes());
    }

    // Trim trailing spaces from each line
    let trimmed =
      line.len() - line.iter().rev().take_while(|b| **b == b' ').count();
    out.extend_from_slice(&line[..trimmed]);
    brush = line_brush;
  }

  // Reset attributes at the end
  if brush != Attrs::default() {
    out.extend_from_slice(emit::SGR_RESET.as_bytes());
  }

  String::from_utf8(out).expect("emitted ANSI is valid utf-8")
}
