use super::*;
fn data(inverse: bool) -> Vec<u8> {
    GeoIpList {
        entry: vec![GeoIp {
            country_code: "telegram".into(),
            inverse_match: inverse,
            cidr: vec![
                Cidr {
                    ip: vec![149, 154, 160, 0],
                    prefix: 20,
                },
                Cidr {
                    ip: "2001:db8::".parse::<Ipv6Addr>().unwrap().octets().to_vec(),
                    prefix: 32,
                },
            ],
        }],
    }
    .encode_to_vec()
}
#[test]
fn intervals_merge_duplicates_overlap_adjacency_and_maximum_values() {
    assert_eq!(
        merge_ranges(
            vec![
                (10u32, 20),
                (0, 5),
                (6, 9),
                (12, 13),
                (10, 20),
                (u32::MAX, u32::MAX)
            ],
            |n| n.saturating_add(1)
        ),
        vec![(0, 20), (u32::MAX, u32::MAX)]
    );
    assert_eq!(
        merge_ranges(vec![(0u128, u128::MAX), (u128::MAX, u128::MAX)], |n| n
            .saturating_add(1)),
        vec![(0, u128::MAX)]
    );
    let ranges = [(0u32, 20), (u32::MAX, u32::MAX)];
    for (ip, expected) in [
        (0, true),
        (20, true),
        (21, false),
        (u32::MAX - 1, false),
        (u32::MAX, true),
    ] {
        assert_eq!(contains(&ranges, ip), expected);
    }
    assert!(!contains::<u32>(&[], 0));
}

#[test]
fn interval_index_agrees_with_prefix_matching_in_both_families() {
    for inverse in [false, true] {
        let cidrs: Vec<_> = (0..129u32)
            .map(|prefix| Cidr {
                ip: (u128::MAX - (1u128 << (prefix % 128)))
                    .to_be_bytes()
                    .to_vec(),
                prefix,
            })
            .chain((1..33u32).map(|prefix| {
                Cidr {
                    ip: (0x80ffffffu32 ^ (1 << (prefix % 32)))
                        .to_be_bytes()
                        .to_vec(),
                    prefix,
                }
            }))
            .collect();
        let db = Database::decode(
            &GeoIpList {
                entry: vec![GeoIp {
                    country_code: "test".into(),
                    cidr: cidrs.clone(),
                    inverse_match: inverse,
                }],
            }
            .encode_to_vec(),
        )
        .unwrap();
        for i in 0..1000u128 {
            for ip in [
                IpAddr::V4(Ipv4Addr::from((i as u32).wrapping_mul(2654435761))),
                IpAddr::V6(Ipv6Addr::from(i.wrapping_mul(u128::MAX / 997))),
            ] {
                let canonical = ip.to_canonical();
                let matched = cidrs.iter().any(|cidr| match canonical {
                    IpAddr::V4(ip) if cidr.ip.len() == 4 => {
                        u32::from(ip) & mask4(cidr.prefix as u8)
                            == u32::from_be_bytes(cidr.ip.clone().try_into().unwrap())
                                & mask4(cidr.prefix as u8)
                    }
                    IpAddr::V6(ip) if cidr.ip.len() == 16 => {
                        u128::from(ip) & mask6(cidr.prefix as u8)
                            == u128::from_be_bytes(cidr.ip.clone().try_into().unwrap())
                                & mask6(cidr.prefix as u8)
                    }
                    _ => false,
                });
                assert_eq!(db.matches("TEST", ip), matched != inverse, "{ip}");
            }
        }
    }
}

#[test]
fn categories_match_v4_v6_mapped_and_inverse() {
    for inverse in [false, true] {
        let db = Database::decode(&data(inverse)).unwrap();
        let group = db.group("TeLeGrAm").unwrap();
        for ip in ["149.154.160.1", "::ffff:149.154.160.1", "2001:db8::1"] {
            assert_eq!(group.contains(ip.parse().unwrap()), !inverse);
        }
        for ip in ["149.154.176.1", "2001:db9::1"] {
            assert_eq!(group.contains(ip.parse().unwrap()), inverse);
        }
        assert!(db.group("missing").is_err());
    }
}

#[test]
fn selected_categories_keep_matching_semantics_without_other_indexes() {
    for inverse in [false, true] {
        let mut list = GeoIpList::decode(data(inverse).as_slice()).unwrap();
        let mut unused = list.entry[0].clone();
        unused.country_code = "unused".into();
        list.entry.push(unused);
        let bytes = list.encode_to_vec();
        let full = Database::decode(&bytes).unwrap();
        let selected =
            Database::decode_selected(&bytes, &["TeLeGrAm".into(), "telegram".into()]).unwrap();
        assert_eq!(selected.groups.len(), 1);
        assert!(selected.group("unused").is_err());
        for ip in [
            "149.154.160.1",
            "::ffff:149.154.160.1",
            "2001:db8::1",
            "149.154.176.1",
            "2001:db9::1",
        ] {
            let ip = ip.parse().unwrap();
            assert_eq!(
                selected.matches("TELEGRAM", ip),
                full.matches("TELEGRAM", ip)
            );
        }
        assert!(Database::decode_selected(&bytes, &["missing".into()]).is_err());
        assert!(Database::decode_selected(&bytes, &["bad/name".into()]).is_err());
        assert!(Database::decode_selected(&bytes, &[])
            .unwrap()
            .groups
            .is_empty());
    }
}

#[test]
fn selected_loading_still_validates_unselected_groups() {
    let original = GeoIpList::decode(data(false).as_slice()).unwrap();
    for (name, ip, prefix) in [
        ("TELEGRAM", vec![1, 2, 3, 4], 24), // duplicate, including case
        ("bad/name", vec![1, 2, 3, 4], 24),
        ("unused", vec![1, 2, 3, 4], 33),
        ("unused", vec![0; 16], 129),
        ("unused", vec![1, 2, 3], 24),
    ] {
        let mut list = original.clone();
        list.entry.push(GeoIp {
            country_code: name.into(),
            inverse_match: false,
            cidr: vec![Cidr { ip, prefix }],
        });
        assert!(
            Database::decode_selected(&list.encode_to_vec(), &["telegram".into()]).is_err(),
            "{name}, prefix {prefix}"
        );
    }
}

#[test]
fn streamed_messages_handle_unknown_fields_order_and_truncation() {
    // CIDR precedes metadata, and unknown fields occur at each level.
    let mut cidr = Cidr {
        ip: vec![10, 0, 0, 0],
        prefix: 8,
    }
    .encode_to_vec();
    cidr.extend_from_slice(&[0x48, 1]);
    let mut group = vec![0x12, cidr.len() as u8];
    group.extend(cidr);
    group.extend_from_slice(&[0x0a, 2, b'C', b'N', 0x18, 1, 0x48, 1]);
    let mut bytes = vec![0x0a, group.len() as u8];
    bytes.extend(group);
    let end = bytes.len();
    bytes.extend_from_slice(&[0x48, 1]);
    let db = Database::decode_selected(&bytes, &["cn".into()]).unwrap();
    assert!(!db.matches("CN", "10.1.2.3".parse().unwrap()));
    assert!(db.matches("CN", "11.1.2.3".parse().unwrap()));
    for len in 0..end {
        assert!(Database::decode_selected(&bytes[..len], &["cn".into()]).is_err());
    }
    for bytes in [vec![0x08, 1], vec![0x0a, 0xff], vec![0x0a, 2, 0x10, 1]] {
        assert!(Database::decode_selected(&bytes, &[]).is_err());
    }
}
#[test]
fn corrupt_databases_and_selectors_are_rejected() {
    for bytes in [
        vec![],
        b"not a database".to_vec(),
        GeoIpList {
            entry: vec![GeoIp {
                country_code: "cn".into(),
                cidr: vec![Cidr {
                    ip: vec![1, 2, 3, 4],
                    prefix: 33,
                }],
                inverse_match: false,
            }],
        }
        .encode_to_vec(),
    ] {
        assert!(Database::decode(&bytes).is_err());
    }
    for name in ["", "CN,x", "bad name", "../cn", "cn@foo"] {
        assert!(category(name).is_err());
    }
}
