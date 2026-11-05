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
