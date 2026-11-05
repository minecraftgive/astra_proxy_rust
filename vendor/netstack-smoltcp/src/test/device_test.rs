use super::*;

use tokio::sync::mpsc::channel;

/// Regression test for the receive() ordering bug: reserving out_buf
/// capacity before dequeuing in_buf must not lose the in_buf packet, and
/// must not clear in_buf_avail while a real packet is still queued.
#[test]
fn receive_keeps_in_buf_packet_when_out_buf_is_full() {
    let (out_tx, mut out_rx) = channel::<AnyIpPktFrame>(1);
    let (mut device, ingress_tx, in_buf_avail) = VirtualDevice::new(out_tx.clone(), 1500);

    // Saturate the bounded out_buf (capacity 1) so try_reserve() fails.
    out_tx.try_send(vec![0u8; 4]).unwrap();

    // Queue an in_buf packet and mark it available, as handle_packet would.
    ingress_tx.try_send(b"payload".to_vec()).unwrap();
    in_buf_avail.store(true, Ordering::Release);

    // out_buf is full: receive() must yield nothing this poll...
    assert!(device.receive(Instant::from_millis(0)).is_none());
    // ...without discarding the queued in_buf packet or claiming there's no more work.
    assert!(
        in_buf_avail.load(Ordering::Acquire),
        "in_buf_avail must stay true: the in_buf packet is still pending, \
             only out_buf capacity was the blocker"
    );

    // Free up out_buf capacity.
    out_rx.try_recv().unwrap();

    // The packet must still be there, untouched, once out_buf has room again.
    let (rx_token, _tx_token) = device
        .receive(Instant::from_millis(1))
        .expect("in_buf packet must not have been dropped while out_buf was full");
    assert_eq!(rx_token.consume(|buf| buf.to_vec()), b"payload");
}
