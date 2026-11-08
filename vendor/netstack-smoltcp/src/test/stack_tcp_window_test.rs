use super::*;

// The configurable TCP window must (a) default to the historical ~320 KiB so
// existing callers are behavior-identical, and (b) carry an override verbatim
// into the builder fields that the socket-creation path
// (`TcpListener::new` → `create` → `handle_packet`) reads. Those fields are
// the only input to the per-socket `SocketBuffer` sizing, so asserting them
// is asserting the window smoltcp will allocate.
#[test]
fn tcp_window_defaults_to_historical_and_threads_overrides() {
    let d = StackBuilder::default();
    assert_eq!(d.tcp_recv_buffer_size, 0x3FFF * 20);
    assert_eq!(d.tcp_send_buffer_size, 0x3FFF * 20);

    let b = StackBuilder::default()
        .tcp_recv_buffer_size(1 << 20)
        .tcp_send_buffer_size(2 << 20);
    assert_eq!(b.tcp_recv_buffer_size, 1 << 20);
    assert_eq!(b.tcp_send_buffer_size, 2 << 20);
    // The mpsc-depth knob is independent and must not be conflated.
    assert_eq!(b.tcp_buffer_size, 512);
}

#[test]
fn staging_override_preserves_tcp_windows() {
    let b = StackBuilder::default()
        .tcp_staging_buffer_size(64 * 1024)
        .tcp_recv_buffer_size(256 * 1024)
        .tcp_send_buffer_size(256 * 1024);
    assert_eq!(b.tcp_staging_buffer_size, Some(64 * 1024));
    assert_eq!(b.tcp_recv_buffer_size, 256 * 1024);
    assert_eq!(b.tcp_send_buffer_size, 256 * 1024);
    assert_eq!(StackBuilder::default().tcp_staging_buffer_size, None);
    assert_eq!(
        StackBuilder::default()
            .tcp_staging_buffer_size(0)
            .tcp_staging_buffer_size,
        Some(1)
    );
}
