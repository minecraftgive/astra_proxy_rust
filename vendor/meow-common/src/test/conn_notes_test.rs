use super::*;

#[tokio::test]
async fn notes_go_to_the_scoped_log_only() {
    let notes = ConnNotes::new();
    conn_note!(Dns, "outside any scope");
    ConnNotes::scope(Some(notes.clone()), async {
        assert!(recording());
        conn_note!(Rule, "matched {}", "MATCH");
    })
    .await;
    ConnNotes::scope(None, async {
        assert!(!recording());
        conn_note!(Rule, "disabled");
    })
    .await;
    assert_eq!(notes.texts(), ["matched MATCH"]);
}

#[tokio::test]
async fn arguments_are_not_evaluated_without_a_scope() {
    let mut evaluated = false;
    let mut probe = || {
        evaluated = true;
        1
    };
    conn_note!(Dns, "{}", probe());
    assert!(!evaluated);
}

#[test]
fn a_full_log_keeps_the_newest_note_and_counts_the_rest() {
    let notes = ConnNotes::new();
    for i in 0..MAX_NOTES + 3 {
        notes.push(NoteKind::Dns, format_args!("note {i}"));
    }
    let texts = notes.texts();
    assert_eq!(texts.len(), MAX_NOTES);
    assert_eq!(texts[MAX_NOTES - 2], format!("note {}", MAX_NOTES - 2));
    assert_eq!(texts[MAX_NOTES - 1], format!("note {}", MAX_NOTES + 2));

    let json = serde_json::to_value(&notes).unwrap();
    let array = json.as_array().unwrap();
    assert_eq!(array.len(), MAX_NOTES + 1);
    assert_eq!(array[MAX_NOTES - 1]["text"], "3 more notes omitted");
    assert_eq!(array[MAX_NOTES]["text"], format!("note {}", MAX_NOTES + 2));
}

#[test]
fn serializes_kind_text_and_a_microsecond_utc_time() {
    let notes = ConnNotes::new();
    notes.push(NoteKind::Dial, format_args!("connected"));
    let json = serde_json::to_value(&notes).unwrap();
    let note = &json[0];
    assert_eq!(note["kind"], "dial");
    assert_eq!(note["text"], "connected");
    let time = note["time"].as_str().unwrap();
    assert!(time.ends_with('Z'), "{time}");
    if let Some((_, fraction)) = time.split_once('.') {
        assert!(fraction.len() <= "000000Z".len(), "{time}");
    }
}

#[test]
fn long_text_is_cut_at_a_char_boundary() {
    let notes = ConnNotes::new();
    let long = "é".repeat(MAX_NOTE_LEN);
    notes.push(NoteKind::Dns, format_args!("{long}"));
    let text = &notes.texts()[0];
    assert!(text.len() <= MAX_NOTE_LEN + '…'.len_utf8());
    assert!(text.ends_with('…'));
}
