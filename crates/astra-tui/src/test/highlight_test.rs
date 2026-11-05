use super::*;
fn colored(spans: &[Span]) -> Vec<(String, Option<Color>)> {
    spans
        .iter()
        .map(|s| (s.content.to_string(), s.style.fg))
        .collect()
}
fn text(spans: &[Span]) -> String {
    spans.iter().map(|s| s.content.as_ref()).collect()
}
#[test]
fn lists_keep_their_text_and_color_each_item() {
    let source = r#"["10.0.0.0/8", "+.example.com", "fc00::/7"]"#;
    let list = spans(source, Flavor::Auto);
    assert_eq!(text(&list), source);
    let colors = colored(&list);
    assert!(colors.contains(&("10.0.0.0".into(), Some(ADDRESS))));
    assert!(colors.contains(&("8".into(), Some(NUMBER))));
    assert!(colors.contains(&("+.".into(), Some(MARK))));
    assert!(colors.contains(&("fc00::".into(), Some(ADDRESS))));
    assert!(colors.contains(&("[".into(), Some(PUNCT))));
    // Unfinished input while typing still renders every character.
    let partial = r#"["a.example", "b.exa"#;
    assert_eq!(text(&spans(partial, Flavor::Auto)), partial);
    // Trailing TOML comments are dimmed and never parsed as data.
    let commented = r#"[]    # e.g. ["127.0.0.0/8"]"#;
    let colors = colored(&spans(commented, Flavor::Auto));
    assert_eq!(colors.last().unwrap().1, Some(Color::DarkGray));
    assert!(!colors.iter().any(|(_, c)| *c == Some(ADDRESS)));
    let number = colored(&spans("8      # 1..128", Flavor::Number));
    assert_eq!(number[0].1, Some(NUMBER));
    assert_eq!(number[1], ("# 1..128".into(), Some(Color::DarkGray)));
    // A # inside plain strings (URLs, regex) stays part of the value.
    assert_eq!(
        colored(&spans("https://a.example/#x", Flavor::Auto))[0].1,
        Some(MARK)
    );
    assert!(!colored(&spans("a #b", Flavor::Regex))
        .iter()
        .any(|(_, c)| *c == Some(Color::DarkGray)));
}
#[test]
fn addresses_urls_actions_and_matchers() {
    assert_eq!(
        colored(&item("127.0.0.1:7890", Flavor::Auto)),
        [
            ("127.0.0.1".into(), Some(ADDRESS)),
            (":".into(), Some(PUNCT)),
            ("7890".into(), Some(NUMBER))
        ]
    );
    assert_eq!(text(&spans("[::1]:443", Flavor::Auto)), "[::1]:443");
    assert_eq!(
        colored(&spans("[::1]:443", Flavor::Auto))[0].1,
        Some(ADDRESS)
    );
    let url = item(
        "https://user@sub.example.net:8443/api?token=1",
        Flavor::Auto,
    );
    assert_eq!(text(&url), "https://user@sub.example.net:8443/api?token=1");
    assert_eq!(colored(&url)[0], ("https://".into(), Some(MARK)));
    assert!(colored(&url).contains(&("8443".into(), Some(NUMBER))));
    assert_eq!(colored(&action("direct"))[0].1, Some(Color::Green));
    assert_eq!(colored(&action("block"))[0].1, Some(Color::Red));
    assert_eq!(text(&action("proxy:PROXY")), "proxy:PROXY");
    assert_eq!(
        colored(&geosite("cn@!ads"))[1],
        ("@!ads".into(), Some(MARK))
    );
    assert_eq!(
        colored(&regex(r"^ad\.example$"))
            .iter()
            .filter(|(_, color)| *color == Some(MARK))
            .count(),
        3
    );
    assert_eq!(text(&marks("*.cdn.*", &['*'], MARK)), "*.cdn.*");
    let path = item(r"C:\Program Files\Browser\browser.exe", Flavor::Auto);
    assert_eq!(text(&path), r"C:\Program Files\Browser\browser.exe");
    assert_eq!(flavor("value", false, Some("domain_regex")), Flavor::Regex);
    assert_eq!(flavor("default_action", false, None), Flavor::Action);
    assert_eq!(flavor("port", true, None), Flavor::Number);
}
#[test]
fn cursor_lands_between_characters_of_any_span() {
    let spans = item("127.0.0.1:80", Flavor::Auto);
    for cursor in 0..=12 {
        let with = with_cursor(spans.clone(), cursor);
        let text = text(&with);
        assert_eq!(text.chars().position(|c| c == '▏'), Some(cursor));
        assert_eq!(text.replace('▏', ""), "127.0.0.1:80");
    }
    assert_eq!(text(&with_cursor(vec![], 0)), "▏");
}
