use super::*;
#[test]
fn rows_and_hits_use_actual_offsets_and_full_rows() {
    let rows = RenderedRows::fixed(Rect::new(2, 3, 20, 5), 7, 10, 2);
    assert_eq!(rows.visible_capacity, 2);
    assert_eq!(
        rows.rows[1],
        RowGeometry {
            index: 8,
            area: Rect::new(2, 5, 20, 2)
        }
    );
    let mut hits = HitMap::default();
    hits.bind_rows(&rows, |index| index);
    assert_eq!(hits.hit(2, 6), Some(&8));
    assert_eq!(hits.hit(2, 7), None);
    hits.insert(Rect::new(2, 3, 4, 1), 99);
    assert_eq!(hits.hit(2, 3), Some(&99));
    hits.clear();
    assert!(hits.hit(2, 3).is_none());
    assert_eq!(
        RenderedRows::fixed(Rect::new(0, 0, 0, 10), 0, 10, 1).visible_capacity,
        0
    );
}
