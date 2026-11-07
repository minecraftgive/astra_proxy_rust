use super::*;
#[test]
fn native_pages_follow_the_shared_navigation_order() {
    let titles: Vec<_> = PAGES.iter().map(|page| page.title).collect();
    assert_eq!(titles, astra_client::navigation::PAGES);
    assert!(titles.iter().all(|title| !title.is_empty()));
}
