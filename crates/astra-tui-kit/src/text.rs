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
mod tests {
    use super::*;

    #[test]
    fn unicode_columns_fit_without_splitting_clusters() {
        for text in ["中文节点", "e\u{301}cole", "👩‍💻abc", "🇭🇰 香港", "plain"] {
            for columns in 0..15 {
                assert!(width(&truncate_cells(text, columns, "…")) <= columns);
                assert_eq!(width(&fit_column(text, columns)), columns);
            }
        }
        assert_eq!(truncate_cells("e\u{301}abcdef", 2, "…"), "e\u{301}…");
        assert_eq!(truncate_cells("中文", 1, "…"), "…");
        assert_eq!(truncate_cells("abc", 0, "…"), "");
    }
}
