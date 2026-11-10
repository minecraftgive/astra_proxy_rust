use super::*;
#[test]
fn twenty_cycles_preserve_wrapped_bytes_and_reclaim_only_when_empty() {
    let mut buffer = Staging::new(65536, true);
    for _ in 0..20 {
        let bytes: Vec<_> = (0..65536).map(|i| (i % 251) as u8).collect();
        let mut sent = 0;
        while sent < bytes.len() {
            sent += buffer.enqueue_slice(&bytes[sent..]);
        }
        assert_eq!(buffer.ring.capacity(), 65536);
        buffer.idle(Instant::now() + Duration::from_secs(31));
        assert_eq!(buffer.ring.capacity(), 65536);
        let mut output = vec![0; bytes.len()];
        assert_eq!(buffer.dequeue_slice(&mut output), bytes.len());
        assert_eq!(bytes, output);
        buffer.idle(Instant::now() + Duration::from_secs(31));
        assert_eq!(buffer.ring.capacity(), 4096);
        buffer.enqueue_slice(&[7; 4000]);
        buffer.dequeue_slice(&mut [0; 3000]);
        buffer.enqueue_slice(&[8; 3096]);
        buffer.enqueue_slice(&[9; 100]);
        let mut wrapped = vec![0; 4196];
        assert_eq!(buffer.dequeue_slice(&mut wrapped), 4196);
        assert_eq!(wrapped, [&[7; 1000][..], &[8; 3096], &[9; 100]].concat());
        buffer.idle(Instant::now() + Duration::from_secs(31));
    }
}
