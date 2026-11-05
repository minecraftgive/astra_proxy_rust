//! Display-cell operations preserve grapheme clusters and use Ratatui's width model.
use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;

pub fn width(text: &str) -> usize {
    Line::from(text).width()
}

pub fn truncate_cells(text: &str, columns: usize, ellipsis: &str) -> String {
    if width(text) <= columns {
        return text.to_owned();
    }
    let suffix = if width(ellipsis) <= columns {
        ellipsis
    } else {
        ""
    };
    let limit = columns.saturating_sub(width(suffix));
    let mut output = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let cells = width(grapheme);
        if used + cells > limit {
            break;
        }
        output.push_str(grapheme);
        used += cells;
    }
    output.push_str(suffix);
    output
}

pub fn pad_cells(text: &str, columns: usize) -> String {
    format!("{text}{}", " ".repeat(columns.saturating_sub(width(text))))
}

pub fn fit_column(text: &str, columns: usize) -> String {
    pad_cells(&truncate_cells(text, columns, "…"), columns)
}

#[cfg(test)]
#[path = "test/text_test.rs"]
mod tests;
