use super::*;
use tokio::sync::mpsc::channel;

#[tokio::test]
async fn accepted_socket_keeps_tcp_windows_with_smaller_staging_rings() {
    for staging in [None, Some(64 * 1024)] {
        let (packet_tx, packet_rx) = channel(1);
        let (ingress_tx, _ingress_rx) = channel(1);
        let (stream_tx, mut stream_rx) = unbounded_channel();
        let (socket_tx, mut socket_rx) = unbounded_channel();
        let mut packet = Vec::new();
        etherparse::PacketBuilder::ipv4([192, 0, 2, 1], [192, 0, 2, 2], 64)
            .tcp(12345, 443, 1, 65535)
            .syn()
            .write(&mut packet, &[])
            .unwrap();
        packet_tx.send(packet).await.unwrap();
        drop(packet_tx);
        TcpListenerRunner::handle_packet(
            Arc::new(Notify::new()),
            ingress_tx,
            Arc::new(AtomicBool::new(false)),
            packet_rx,
            stream_tx,
            socket_tx,
            256 * 1024,
            512 * 1024,
            staging,
            1,
        )
        .await
        .unwrap();
        let accepted = socket_rx.recv().await.unwrap();
        let stream = stream_rx.recv().await.unwrap();
        assert_eq!(accepted.socket.recv_capacity(), 256 * 1024);
        assert_eq!(accepted.socket.send_capacity(), 512 * 1024);
        let control = stream.control.lock();
        assert_eq!(
            control.recv_buffer.capacity(),
            staging.unwrap_or(256 * 1024) as usize
        );
        assert_eq!(
            control.send_buffer.capacity(),
            staging.unwrap_or(512 * 1024) as usize
        );
    }
}
