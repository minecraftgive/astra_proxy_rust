use ratatui::{
    layout::Rect,
    widgets::{Paragraph, Wrap},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowGeometry {
    pub index: usize,
    pub area: Rect,
}

#[derive(Clone, Debug, Default)]
pub struct RenderedRows {
    pub offset: usize,
    pub visible_capacity: usize,
    pub rows: Vec<RowGeometry>,
}

impl RenderedRows {
    /// `content` is the actual body after borders and headers were removed.
    /// Uses the offset returned by Ratatui's stateful renderer.
    pub fn fixed(content: Rect, offset: usize, count: usize, height: u16) -> Self {
        let capacity = if content.width == 0 || height == 0 {
            0
        } else {
            content.height as usize / height as usize
        };
        let rows = (0..capacity.min(count.saturating_sub(offset)))
            .map(|visible| RowGeometry {
                index: offset + visible,
                area: Rect::new(
                    content.x,
                    content.y.saturating_add(visible as u16 * height),
                    content.width,
                    height,
                ),
            })
            .collect();
        Self {
            offset,
            visible_capacity: capacity,
            rows,
        }
    }
}

pub struct HitMap<A> {
    entries: Vec<(Rect, A)>,
}

impl<A> Default for HitMap<A> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<A> HitMap<A> {
    pub fn clear(&mut self) {
        self.entries.clear();
    }
    pub fn insert(&mut self, area: Rect, action: A) {
        if area.width > 0 && area.height > 0 {
            self.entries.push((area, action));
        }
    }
    pub fn hit(&self, x: u16, y: u16) -> Option<&A> {
        self.entries
            .iter()
            .rev()
            .find(|(area, _)| area.contains((x, y).into()))
            .map(|(_, action)| action)
    }
    pub fn bind_rows(&mut self, rows: &RenderedRows, action: impl Fn(usize) -> A) {
        for row in &rows.rows {
            self.insert(row.area, action(row.index));
        }
    }
}

impl<A> std::ops::Deref for HitMap<A> {
    type Target = [(Rect, A)];
    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

pub fn content_popup(area: Rect, text: &str) -> Rect {
    let preferred = (ratatui::text::Text::from(text).width() + 4).clamp(28, 76) as u16;
    let width = area
        .width
        .saturating_sub(4)
        .min(preferred)
        .max(1)
        .min(area.width);
    let height = (Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .line_count(width.saturating_sub(2).max(1)) as u16
        + 2)
    .min(19)
    .min(area.height.saturating_sub(2).max(1))
    .min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
#[path = "test/layout_test.rs"]
mod tests;
