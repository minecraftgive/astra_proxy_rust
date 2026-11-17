use super::*;
use tokio::sync::mpsc::channel;

fn packet(sequence: u32, ack: Option<u32>, payload: &[u8]) -> Vec<u8> {
    let builder = etherparse::PacketBuilder::ipv4([192, 0, 2, 1], [192, 0, 2, 2], 64)
        .tcp(49500, 80, sequence, 256);
    let builder = match ack {
        Some(ack) => builder.ack(ack),
        None => builder
            .syn()
            .options(&[etherparse::TcpOptionElement::MaximumSegmentSize(32)])
            .unwrap(),
    };
    let mut bytes = Vec::new();
    builder.write(&mut bytes, payload).unwrap();
    bytes
}

fn reply(receiver: &mut Receiver<Vec<u8>>) -> (u32, u32, u16) {
    let bytes = receiver.try_recv().expect("expected TCP reply");
    let ip = IpPacket::new_checked(bytes.as_slice()).unwrap();
    let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
    assert!(tcp.ack());
    assert!(!tcp.rst());
    assert!(receiver.try_recv().is_err(), "unexpected extra TCP reply");
    (
        tcp.seq_number().0 as u32,
        tcp.ack_number().0 as u32,
        tcp.window_len(),
    )
}

// smoltcp #1079: receive capacity can grow before a window update is sent.
// Bytes beyond the advertised window must be clipped, even if they fit in
// the buffer. smoltcp 0.12.0 panics in last_scaled_window() on the final poll.
// Exercise real IP/TCP frames through the same device used by the TUN runner.
#[test]
fn tcp_rejects_bytes_beyond_advertised_window_without_panicking() {
    // Check normal sequence numbers and an actual u32 wrap during the exchange.
    for initial_seq in [1000u32, u32::MAX - 40] {
        let (out_tx, mut out_rx) = channel(16);
        let (mut device, ingress, _) = VirtualDevice::new(out_tx, 1500);
        let mut iface = TcpListener::create_interface(&mut device).unwrap();
        let now = Instant::now();
        let mut socket = TcpSocket::new(
            TcpSocketBuffer::new(vec![0; 64]),
            TcpSocketBuffer::new(vec![0; 64]),
        );
        socket.set_ack_delay(Some(Duration::from_millis(10)));
        socket.listen(80).unwrap();
        let mut sockets = SocketSet::new(vec![]);
        let handle = sockets.add(socket);

        ingress.try_send(packet(initial_seq, None, &[])).unwrap();
        iface.poll(now, &mut device, &mut sockets);
        let (server_seq, ack, window) = reply(&mut out_rx);
        let seq = initial_seq.wrapping_add(1);
        let server_ack = server_seq.wrapping_add(1);
        assert_eq!((ack, window), (seq, 64));
        ingress
            .try_send(packet(seq, Some(server_ack), &[]))
            .unwrap();
        iface.poll(now, &mut device, &mut sockets);
        assert_eq!(
            sockets.get::<TcpSocket>(handle).state(),
            TcpState::Established
        );
        assert!(out_rx.try_recv().is_err());

        ingress
            .try_send(packet(seq, Some(server_ack), &[0; 32]))
            .unwrap();
        iface.poll(now, &mut device, &mut sockets);
        assert!(out_rx.try_recv().is_err(), "first ACK should be delayed");
        ingress
            .try_send(packet(seq.wrapping_add(32), Some(server_ack), &[0; 1]))
            .unwrap();
        iface.poll(now, &mut device, &mut sockets);
        let (_, ack, window) = reply(&mut out_rx);
        assert_eq!((ack, window), (seq.wrapping_add(33), 31));

        // Free one byte without advertising the additional receive capacity.
        assert_eq!(
            sockets
                .get_mut::<TcpSocket>(handle)
                .recv_slice(&mut [0; 1])
                .unwrap(),
            1
        );
        iface.poll(now, &mut device, &mut sockets);
        assert!(
            out_rx.try_recv().is_err(),
            "one byte must not trigger a window update"
        );

        // Only 31 of these 32 bytes are inside the last advertised window.
        ingress
            .try_send(packet(seq.wrapping_add(33), Some(server_ack), &[0; 32]))
            .unwrap();
        iface.poll(now, &mut device, &mut sockets);
        let (_, ack, window) = reply(&mut out_rx);
        assert_eq!((ack, window), (seq.wrapping_add(64), 1));
        assert_eq!(sockets.get::<TcpSocket>(handle).recv_queue(), 63);

        // The clipped byte remains deliverable when retransmitted inside the
        // new window. Keeping the socket alive alone would miss silent loss.
        ingress
            .try_send(packet(seq.wrapping_add(64), Some(server_ack), &[7]))
            .unwrap();
        iface.poll(now, &mut device, &mut sockets);
        let socket = sockets.get_mut::<TcpSocket>(handle);
        let mut received = [0; 64];
        assert_eq!(socket.recv_slice(&mut received).unwrap(), 64);
        assert_eq!(&received[..63], &[0; 63]);
        assert_eq!(received[63], 7);
        assert_eq!(socket.state(), TcpState::Established);
    }
}
