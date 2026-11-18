#[test]
fn discovers_this_executable_on_the_target_host() {
    let pid = std::process::id();
    let entries = super::running_processes();
    let program = entries.iter().find(|row| row.pid == pid).unwrap();
    assert_eq!(
        std::fs::canonicalize(&program.path).unwrap(),
        std::env::current_exe().unwrap().canonicalize().unwrap()
    );
    assert!(super::search_processes(&program.name)
        .0
        .iter()
        .any(|row| row.pid == pid));
    assert!(
        super::search_processes("Astra path that cannot exist on this host / nonexistent")
            .0
            .is_empty()
    );
}
