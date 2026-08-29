use std::ops::Range;

use crate::term::grid::Rect;

#[derive(Default)]
pub struct ListState {
  selected: usize,
  top: usize,
  count: usize,
  height: usize,
}

impl ListState {
  pub fn fit(&mut self, area: Rect, count: usize) {
    self.count = count;
    self.height = area.height as usize;
    self.clamp();
  }

  pub fn select(&mut self, index: usize, count: usize) {
    self.count = count;
    self.selected = index;
    self.clamp();
  }

  // Keep `selected` in range and scroll `top` so it is visible.
  fn clamp(&mut self) {
    self.selected = self.selected.min(self.count.saturating_sub(1));
    self.top = self.top.min(self.selected);
    self.top = self
      .top
      .max((self.selected + 1).saturating_sub(self.height));
    self.top = self.top.min(self.count.saturating_sub(self.height));
  }

  pub fn visible_range(&self) -> Range<usize> {
    self.top..(self.top + self.height).min(self.count)
  }

  /// Item index at a visible row.
  pub fn index_at(&self, row: usize) -> Option<usize> {
    let index = self.top + row;
    (row < self.height && index < self.count).then_some(index)
  }

  pub fn selected(&self) -> usize {
    self.selected
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn scrolls_to_keep_selection_visible() {
    let area = Rect::new(0, 0, 10, 3);
    let mut list = ListState::default();
    list.fit(area, 10);
    assert_eq!(list.visible_range(), 0..3);

    list.select(5, 10);
    assert_eq!(list.visible_range(), 3..6);
    assert_eq!(list.index_at(0), Some(3));
    assert_eq!(list.index_at(2), Some(5));
    assert_eq!(list.index_at(3), None);

    list.select(4, 10);
    assert_eq!(list.visible_range(), 3..6, "stays put while visible");

    list.select(2, 10);
    assert_eq!(list.visible_range(), 2..5);

    list.select(100, 10);
    assert_eq!(list.selected(), 9);
    assert_eq!(list.visible_range(), 7..10);

    list.fit(area, 0);
    assert_eq!(list.selected(), 0);
    assert_eq!(list.index_at(0), None);
  }

  #[test]
  fn selects_before_first_fit() {
    let mut list = ListState::default();
    list.select(2, 5);
    assert_eq!(list.selected(), 2);
    list.fit(Rect::new(0, 0, 10, 3), 5);
    assert_eq!(list.selected(), 2);
    assert!(list.visible_range().contains(&2));
  }
}
