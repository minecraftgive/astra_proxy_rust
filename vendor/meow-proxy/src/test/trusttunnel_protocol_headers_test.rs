use super::*;

#[test]
fn user_agent_follows_the_specs_per_stream_shape() {
    assert_eq!(user_agent("ios", "AdGuard", "_check"), "ios");
    assert_eq!(user_agent("ios", "AdGuard", "_udp2"), "ios _udp2");
    assert_eq!(user_agent("ios", "AdGuard", "_icmp"), "ios _icmp");
    assert_eq!(
        user_agent("ios", "AdGuard", "example.test:443"),
        "ios AdGuard"
    );
}

#[test]
fn a_literal_value_renders_itself() {
    let template = Template::parse("keep-alive").unwrap();
    assert_eq!(template.render(), "keep-alive");
    assert_eq!(template.render(), "keep-alive");
}

#[test]
fn a_fixed_length_placeholder_rerolls_every_render() {
    let template = Template::parse("<random-string(24)>").unwrap();
    let first = template.render();
    let second = template.render();
    assert_eq!(first.len(), 24);
    assert_eq!(second.len(), 24);
    // A constant padding value is just a constant to fingerprint, so the
    // re-roll is the feature, not an implementation detail.
    assert_ne!(first, second, "24 random characters collided twice");
    assert!(
        first.bytes().all(|b| ALPHABET.contains(&b)),
        "{first} left the URL-safe alphabet"
    );
}

/// Rendering draws entropy a stack buffer at a time, so a run longer than
/// one draw has to come out exactly as long — and still inside the
/// alphabet.
#[test]
fn a_placeholder_longer_than_one_random_draw_renders_exactly() {
    let length = RANDOM_CHUNK * 3 + 7;
    let template = Template::parse(&format!("<random-string({length})>")).unwrap();
    let rendered = template.render();
    assert_eq!(rendered.len(), length);
    assert!(
        rendered.bytes().all(|b| ALPHABET.contains(&b)),
        "{rendered}"
    );
    assert_ne!(rendered, template.render());
}

#[test]
fn a_range_placeholder_stays_inside_its_range() {
    let template = Template::parse("p=<random-string(4-9)>;q").unwrap();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..256 {
        let rendered = template.render();
        let body = rendered
            .strip_prefix("p=")
            .and_then(|rest| rest.strip_suffix(";q"))
            .expect(&rendered);
        assert!((4..=9).contains(&body.len()), "{rendered}");
        seen.insert(body.len());
    }
    assert!(seen.len() > 1, "the length never varied: {seen:?}");
}

#[test]
fn several_placeholders_in_one_value_are_independent() {
    // Separated by a character the alphabet cannot produce: `-` and `_`
    // are both URL-safe, so splitting on either would land inside a run
    // roughly two renders in five.
    let template = Template::parse("<random-string(16)>.<random-string(16)>").unwrap();
    let rendered = template.render();
    let (left, right) = rendered.split_once('.').unwrap();
    assert_eq!((left.len(), right.len()), (16, 16));
    assert_ne!(left, right);
}

#[test]
fn malformed_placeholders_are_rejected_not_passed_through() {
    for broken in [
        "<random-string(32>",
        "<random-string()>",
        "<random-string(0)>",
        "<random-string(9-4)>",
        "<random-string(abc)>",
        "<random-string(4097)>",
        "",
    ] {
        assert!(
            Template::parse(broken).is_err(),
            "'{broken}' must not parse"
        );
    }
    // Eight is the cap, nine is one too many.
    let nine = "<random-string(1)>".repeat(9);
    assert!(Template::parse(&nine).is_err());
    assert!(Template::parse(&"<random-string(1)>".repeat(8)).is_ok());
}

#[test]
fn a_value_that_cannot_be_sent_is_rejected_at_parse_time() {
    assert!(Template::parse("line\r\nInjected: yes").is_err());
    assert!(Template::parse("nul\0byte").is_err());
    // Two 4 KiB runs plus literals exceed the per-value budget.
    assert!(Template::parse("<random-string(4096)><random-string(4096)>x").is_err());
}

#[test]
fn reserved_and_malformed_names_are_rejected() {
    let set = |name: &str| {
        ExtraHeaders::new(&[(name.to_string(), "value".to_string())], 8)
            .err()
            .unwrap_or_default()
    };
    assert!(set("proxy-authorization").contains("set by the adapter itself"));
    assert!(set(":method").contains("is not a header name"));
    assert!(set("bad header").contains("is not a header name"));
    assert!(ExtraHeaders::new(&[], 8).unwrap().iter().next().is_none());
}

/// `headers: {connection: keep-alive}` is exactly what an operator adds
/// to look like a browser — and exactly what makes an HTTP/2 or HTTP/3
/// receiver treat the CONNECT as malformed and reset the stream, which
/// this client would report as a dial failure against a working node. The
/// name has to be refused where the operator can still see it.
#[test]
fn headers_that_no_h2_or_h3_request_may_carry_are_rejected() {
    for name in UNSENDABLE {
        let error = ExtraHeaders::new(&[((*name).to_string(), "x".to_string())], 8)
            .err()
            .unwrap_or_else(|| panic!("'{name}' must not be accepted"));
        assert!(error.contains("malformed"), "{name}: {error}");
    }
    // Case is irrelevant — `HeaderName` normalises before the check.
    assert!(ExtraHeaders::new(&[("Connection".into(), "close".into())], 8).is_err());
    // Nothing adjacent got caught in the net.
    assert!(ExtraHeaders::new(&[("accept-encoding".into(), "gzip".into())], 8).is_ok());
}

/// Two spellings of one header name would be sent twice, which is its own
/// distinguishing mark — and for `user-agent` it would also fight the
/// override the knob exists to allow.
#[test]
fn a_header_declared_twice_is_rejected() {
    let error = ExtraHeaders::new(
        &[
            ("User-Agent".into(), "first".into()),
            ("user-agent".into(), "second".into()),
        ],
        8,
    )
    .unwrap_err();
    assert!(error.contains("declared twice"), "{error}");
}

#[test]
fn the_entry_count_is_bounded() {
    let many: Vec<_> = (0..5)
        .map(|i| (format!("x-{i}"), "v".to_string()))
        .collect();
    assert!(ExtraHeaders::new(&many, 5).is_ok());
    assert!(ExtraHeaders::new(&many, 4).err().unwrap().contains("max 4"));
}

#[test]
fn a_configured_user_agent_is_recorded_as_an_override() {
    let plain = ExtraHeaders::new(&[("x-padding".into(), "<random-string(8)>".into())], 8).unwrap();
    assert!(!plain.overrides_user_agent());
    let overridden =
        ExtraHeaders::new(&[("User-Agent".into(), "ios AdGuard/2.0".into())], 8).unwrap();
    assert!(overridden.overrides_user_agent());
    // Header names normalize to lowercase, so the wire form is stable
    // whatever case the YAML used.
    assert_eq!(overridden.iter().next().unwrap().0.as_str(), "user-agent");
}

#[test]
fn tokens_are_validated_for_the_field_they_land_in() {
    assert!(validate_token("platform", "ios", false).is_ok());
    assert!(validate_token("platform", "i os", false).is_err());
    assert!(validate_token("app-name", "AdGuard VPN", true).is_ok());
    assert!(validate_token("app-name", "", true).is_err());
    assert!(validate_token("app-name", &"x".repeat(65), true).is_err());
    assert!(validate_token("app-name", "bad\nname", true).is_err());
    assert!(validate_token("platform", "\tios", false).is_err());
}

/// RFC 9113 §8.2.1: a value that starts or ends with SP/HTAB is malformed,
/// so the endpoint would reset the CONNECT. `app-name` ends the
/// `user-agent`, so its edges are the value's edge.
#[test]
fn app_name_whitespace_is_inner_only() {
    for padded in ["AdGuard ", " AdGuard", "AdGuard\t", "\tAdGuard"] {
        let error = validate_token("app-name", padded, true).unwrap_err();
        assert!(
            error.contains("cannot start or end with whitespace"),
            "{padded:?} -> {error}"
        );
    }
    assert!(validate_token("app-name", "Ad\tGuard VPN", true).is_ok());
}

#[test]
fn header_values_cannot_render_edge_whitespace() {
    for (raw, edge) in [
        (" abc", "start"),
        ("\tabc", "start"),
        ("abc ", "end"),
        ("abc\t", "end"),
        (" ", "start"),
        // A placeholder that may render empty exposes its literal
        // neighbour — and two of them in a row expose the next one.
        ("<random-string(0-4)> abc", "start"),
        ("abc <random-string(0-4)>", "end"),
        ("<random-string(0-2)><random-string(0-2)>\tabc", "start"),
    ] {
        let error = Template::parse(raw).unwrap_err();
        assert!(
            error.contains(&format!("cannot {edge} with a space or tab")),
            "{raw:?} -> {error}"
        );
    }
    // Inner whitespace is fine, and so is an edge literal shielded by a
    // run that always renders at least one character.
    for raw in [
        "a b",
        "a\tb",
        "abc <random-string(1-4)>",
        "<random-string(2)> abc",
        "<random-string(0-4)>",
    ] {
        let template = Template::parse(raw).unwrap_or_else(|e| panic!("{raw:?}: {e}"));
        let rendered = template.render();
        assert!(!rendered.starts_with(WHITESPACE), "{rendered:?}");
        assert!(!rendered.ends_with(WHITESPACE), "{rendered:?}");
    }
}
