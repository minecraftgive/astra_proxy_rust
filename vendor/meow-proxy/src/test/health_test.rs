use super::*;

#[test]
fn parsed_url_cases() {
    // (input, https, host, port, path)
    let accepted: &[(&str, bool, &str, u16, &str)] = &[
        (
            "https://www.gstatic.com/generate_204",
            true,
            "www.gstatic.com",
            443,
            "/generate_204",
        ),
        (
            "https://cp.cloudflare.com/generate_204",
            true,
            "cp.cloudflare.com",
            443,
            "/generate_204",
        ),
        ("http://example.com:8080", false, "example.com", 8080, "/"),
        ("http://[::1]:8080/x", false, "::1", 8080, "/x"),
    ];
    // CTL/whitespace bytes would inject raw request lines or headers
    // into the probe request (issue #648).
    let rejected: &[&str] = &[
        "ftp://x",
        "example.com",
        "http://victim/\r\nX-Injected: x",
        "http://victim/x y",
        "http://vic\ttim/",
        "http://vic\0tim/",
        "http://victim/\x0bz",
        "http://victim/\x7f",
        // CTL in the authority half, not just the path — these would
        // still parse (valid host + port) without the guard.
        "http://vic\ntim:8080/",
        "http://vic\rtim:8080/",
    ];

    // Collect instead of asserting inline so one bad input does not hide
    // the remaining cases.
    let mut failures = Vec::new();
    for (input, https, host, port, path) in accepted {
        match ParsedUrl::parse(input) {
            Some(p) => {
                let got = (p.https, p.host.as_str(), p.port, p.path.as_str());
                if got != (*https, *host, *port, *path) {
                    failures.push(format!(
                            "{input:?}: expected (https={https}, host={host:?}, port={port}, path={path:?}), got {got:?}"
                        ));
                }
            }
            None => failures.push(format!("{input:?}: expected a parse, got None")),
        }
    }
    for input in rejected {
        if let Some(p) = ParsedUrl::parse(input) {
            failures.push(format!("{input:?}: expected None, got {p:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "ParsedUrl::parse mismatches:\n{}",
        failures.join("\n")
    );
}

#[test]
fn parse_expected_cases() {
    // (label, input, expected): expected is Some(ranges) when the input
    // must parse to exactly those ranges, None when it must be rejected.
    type Case = (
        &'static str,
        Option<&'static str>,
        Option<&'static [(u16, u16)]>,
    );
    let cases: &[Case] = &[
        ("default: None is 2xx", None, Some(&[(200, 299)])),
        (
            "default: empty string is 2xx",
            Some(""),
            Some(&[(200, 299)]),
        ),
        (
            "mixed list of codes and ranges",
            Some("200,204-206,301"),
            Some(&[(200, 200), (204, 206), (301, 301)]),
        ),
        ("inverted range is rejected", Some("300-200"), None),
        ("garbage is rejected", Some("abc"), None),
    ];

    // Every case runs even if an earlier one fails, so one bad input does
    // not hide the rest.
    let mut failures = Vec::new();
    for (label, input, expected) in cases {
        match (parse_expected(*input), expected) {
            (Ok(got), Some(want)) => {
                if got.as_slice() != *want {
                    failures.push(format!(
                        "{label} ({input:?}): expected {want:?}, got {got:?}"
                    ));
                }
            }
            (Err(_), None) => {}
            (Ok(got), None) => {
                failures.push(format!(
                    "{label} ({input:?}): expected an error, got {got:?}"
                ));
            }
            (Err(e), Some(want)) => {
                failures.push(format!(
                    "{label} ({input:?}): expected {want:?}, got error {e:?}"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "parse_expected mismatches:\n{}",
        failures.join("\n")
    );
}
