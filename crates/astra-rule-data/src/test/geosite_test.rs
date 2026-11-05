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
