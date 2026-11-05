#[test]
fn local_adapters_are_sorted_unique_and_excludable() {
    let names = super::names(None).unwrap();
    assert!(
        !names.is_empty(),
        "expected at least one local network adapter"
    );
    assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(!super::names(Some(&names[0])).unwrap().contains(&names[0]));
    println!("Local adapters: {names:?}");
    let connected = super::connected().unwrap();
    assert!(connected.iter().all(|name| names.contains(name)));
    println!("Connected adapters: {connected:?}");
}
