use super::*;
#[test]
fn partial_adapter_setup_is_recoverable_but_external_changes_are_preserved() {
    let before = AdapterValues {
        parameters: vec![InterfaceParameters {
            family: AF_INET,
            mtu: 1500,
            metric: 25,
            automatic: true,
        }],
        addresses: vec![AdapterAddress {
            address: "192.0.2.1".parse().unwrap(),
            prefix: 24,
            properties: None,
        }],
    };
    let expected = AdapterValues {
        parameters: vec![InterfaceParameters {
            family: AF_INET,
            mtu: 1400,
            metric: 5,
            automatic: false,
        }],
        addresses: vec![AdapterAddress {
            address: "198.18.0.1".parse().unwrap(),
            prefix: 30,
            properties: None,
        }],
    };
    let snapshot = AdapterSnapshot {
        index: 1,
        guid: [0; 16],
        before: before.clone(),
        expected: expected.clone(),
    };
    let mut partial = before.clone();
    partial.parameters[0].mtu = 1400;
    validate_adapter_restoration(&snapshot, &partial).unwrap();
    partial.addresses.clear();
    validate_adapter_restoration(&snapshot, &partial).unwrap();
    validate_adapter_restoration(&snapshot, &expected).unwrap();
    partial.parameters[0].metric = 17;
    assert!(validate_adapter_restoration(&snapshot, &partial).is_err());
    partial = before.clone();
    partial.addresses.push(AdapterAddress {
        address: "203.0.113.9".parse().unwrap(),
        prefix: 24,
        properties: None,
    });
    assert!(validate_adapter_restoration(&snapshot, &partial).is_err());
}
