use super::*;
fn database() -> Database {
    Database::decode(include_bytes!(
        "../../../astra-rule/src/test/data/geosite.dat"
    ))
    .unwrap()
}
#[test]
fn selectors_preserve_group_names_and_filter_attributes() {
    let db = database();
    assert_eq!(db.expand("TeSt").unwrap().len(), 4);
    assert_eq!(db.expand("test@cn").unwrap().len(), 2);
    assert_eq!(db.expand("test@!cn").unwrap().len(), 2);
    assert_eq!(db.expand("test@cn@fast").unwrap().len(), 1);
    assert_eq!(db.expand("test@missing").unwrap().len(), 0);
    assert_eq!(
        db.expand("geolocation-!cn").unwrap()[0].value,
        "abroad.test"
    );
    assert!(db.expand("missing").is_err());
    for value in [
        "", "@cn", "test@", "test@!", "test a", "test@cn!", "test@@cn",
    ] {
        assert!(selector(value).is_err(), "{value}");
    }
}
#[test]
fn all_domain_types_and_invalid_database_are_handled() {
    assert_eq!(
        database()
            .expand("test")
            .unwrap()
            .iter()
            .map(|rule| rule.kind.as_str())
            .collect::<Vec<_>>(),
        [
            "domain_exact",
            "domain_suffix",
            "domain_keyword",
            "domain_regex"
        ]
    );
    assert!(Database::decode(b"invalid protobuf").is_err());
    assert!(Database::decode(b"").is_err());
    let empty = GeoSiteList {
        entry: vec![GeoSite {
            country_code: "empty".into(),
            domain: vec![],
        }],
    };
    assert!(Database::decode(&empty.encode_to_vec())
        .unwrap()
        .expand("empty")
        .unwrap()
        .is_empty());
}

#[test]
fn selected_groups_keep_attribute_semantics_and_report_missing_on_expansion() {
    let bytes = include_bytes!("../../../astra-rule/src/test/data/geosite.dat");
    let selected = Database::decode_selected(bytes, &["TeSt@cn@!missing".into()]).unwrap();
    assert_eq!(selected.groups.len(), 1);
    assert_eq!(
        selected.expand("test@cn").unwrap(),
        database().expand("test@cn").unwrap()
    );
    assert_eq!(
        selected.expand("test@!cn").unwrap(),
        database().expand("test@!cn").unwrap()
    );
    assert!(selected.expand("geolocation-!cn").is_err());
    assert!(Database::decode_selected(bytes, &["missing".into()])
        .unwrap()
        .expand("missing")
        .is_err());
    assert!(Database::decode_selected(bytes, &["test@".into()]).is_err());
    assert!(Database::decode_selected(bytes, &[])
        .unwrap()
        .groups
        .is_empty());
}

#[test]
fn unselected_groups_are_still_validated() {
    let valid = GeoSite {
        country_code: "selected".into(),
        domain: vec![Domain {
            kind: 3,
            value: "example.test".into(),
            attribute: vec![],
        }],
    };
    for invalid in [
        GeoSite {
            country_code: "".into(),
            domain: vec![],
        },
        GeoSite {
            country_code: "bad@name".into(),
            domain: vec![],
        },
        GeoSite {
            country_code: "unused".into(),
            domain: vec![Domain {
                kind: 256,
                value: "example.test".into(),
                attribute: vec![],
            }],
        },
        GeoSite {
            country_code: "unused".into(),
            domain: vec![Domain {
                kind: 3,
                value: "".into(),
                attribute: vec![],
            }],
        },
        GeoSite {
            country_code: "unused".into(),
            domain: vec![Domain {
                kind: 0,
                value: "a".repeat(254),
                attribute: vec![],
            }],
        },
    ] {
        let bytes = GeoSiteList {
            entry: vec![valid.clone(), invalid],
        }
        .encode_to_vec();
        assert!(Database::decode_selected(&bytes, &["selected".into()]).is_err());
    }
    let duplicate = GeoSiteList {
        entry: vec![
            valid,
            GeoSite {
                country_code: "unused".into(),
                domain: vec![],
            },
            GeoSite {
                country_code: "UNUSED".into(),
                domain: vec![],
            },
        ],
    }
    .encode_to_vec();
    assert!(Database::decode_selected(&duplicate, &["selected".into()]).is_err());
    // An unused group's malformed nested protobuf must not be skipped either.
    let mut invalid = GeoSite {
        country_code: "unused".into(),
        domain: vec![],
    }
    .encode_to_vec();
    invalid.extend_from_slice(&[0x12, 0x02, 0x12, 0xff]);
    let mut bytes = vec![0x0a, invalid.len() as u8];
    bytes.extend(invalid);
    assert!(Database::decode_selected(&bytes, &[]).is_err());
}
