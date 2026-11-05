use super::*;
#[test]
fn timed_out_child_is_reaped() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "command::tests::sleeping_child", "--ignored"]);
    let before = Instant::now();
    assert_eq!(
        output_with_timeout(&mut command, Duration::from_millis(100))
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    assert!(before.elapsed() < Duration::from_secs(3));
}
#[test]
#[ignore = "subprocess fixture"]
fn sleeping_child() {
    std::thread::sleep(Duration::from_secs(30));
}
