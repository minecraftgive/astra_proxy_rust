use super::*;
#[test]
fn parity_stubs_cover_pages_and_remaining_views() {
    assert_eq!(PAGES.len(), 7);
    for page in &PAGES {
        assert!(!page.title.is_empty());
        assert!(!page.pending.is_empty());
    }
    for panel in [
        Panel::Monitor,
        Panel::Diagnostics,
        Panel::NetTools,
        Panel::Updates,
        Panel::Editor,
        Panel::Recovery,
        Panel::GuiLog,
    ] {
        assert!(!panel.pending().is_empty());
    }
}
