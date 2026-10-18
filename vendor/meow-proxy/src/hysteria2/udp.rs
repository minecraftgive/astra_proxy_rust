//! `UdpSession`: proxied UDP over hysteria2 QUIC datagrams, bridged to the
//! driver over channels. Sending forwards `(addr, data)` to the driver, which
//! fragments and encodes; receiving pulls decoded `UdpMessage`s the driver
//! routed here and reassembles fragments.

use super::driver::Cmd;
use super::proto::{UdpMessage, MAX_UDP_SIZE};
use super::{Error, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const MAX_PENDING_PACKETS: usize = 8;
const FRAGMENT_TTL: Duration = Duration::from_secs(10);

pub struct UdpSession {
    session_id: u32,
    cmd_tx: mpsc::Sender<Cmd>,
    packets: mpsc::Receiver<UdpMessage>,
    defragger: UdpDefragger,
}

impl UdpSession {
    pub(crate) fn new(
        session_id: u32,
        cmd_tx: mpsc::Sender<Cmd>,
        packets: mpsc::Receiver<UdpMessage>,
    ) -> Self {
        Self {
            session_id,
            cmd_tx,
            packets,
            defragger: UdpDefragger::new(),
        }
    }

    pub fn send(&self, data: &[u8], addr: &str) -> Result<()> {
        if data.len() > MAX_UDP_SIZE {
            return Err(Error::protocol("UDP payload is too large"));
        }
        self.cmd_tx
            .try_send(Cmd::SendUdp {
                session_id: self.session_id,
                addr: addr.to_string(),
                data: data.to_vec(),
            })
            .map_err(|_| Error::Closed)
    }

    pub async fn recv(&mut self) -> Result<(Vec<u8>, String)> {
        loop {
            let message = self.packets.recv().await.ok_or(Error::Closed)?;
            if let Some(message) = self.defragger.feed(message)? {
                return Ok((message.data, message.addr));
            }
        }
    }
}

impl Drop for UdpSession {
    fn drop(&mut self) {
        let _ = self.cmd_tx.try_send(Cmd::UnregisterUdp {
            session_id: self.session_id,
        });
    }
}

struct UdpDefragger {
    packets: HashMap<u16, FragmentPacket>,
}

impl UdpDefragger {
    fn new() -> Self {
        Self {
            packets: HashMap::new(),
        }
    }

    fn feed(&mut self, message: UdpMessage) -> Result<Option<UdpMessage>> {
        self.evict_stale();
        if message.data.len() > MAX_UDP_SIZE || message.frag_count == 0 {
            return Ok(None);
        }
        if message.frag_count == 1 {
            return Ok(Some(message));
        }
        if message.frag_id >= message.frag_count {
            return Ok(None);
        }

        let total = usize::from(message.frag_count);
        let index = usize::from(message.frag_id);
        if let Some(item) = self.packets.get(&message.packet_id) {
            if item.fragments.len() != total || item.addr != message.addr {
                self.packets.remove(&message.packet_id);
                return Ok(None);
            }
            if item.fragments[index].is_some() {
                return Ok(None);
            }
        } else if self.packets.len() >= MAX_PENDING_PACKETS {
            return Ok(None);
        }
        // Bound all incomplete payloads in this session, not just a completed
        // packet. Fragment loss or many packet IDs must not grow memory forever.
        let retained: usize = self.packets.values().map(|p| p.bytes).sum();
        if retained + message.data.len() > MAX_UDP_SIZE {
            self.packets.remove(&message.packet_id);
            return Ok(None);
        }
        let item = self
            .packets
            .entry(message.packet_id)
            .or_insert_with(|| FragmentPacket::new(total, message.addr.clone()));
        item.bytes += message.data.len();
        item.fragments[index] = Some(message);
        item.received += 1;

        if item.received != total {
            return Ok(None);
        }

        let packet_id = item.fragments[0]
            .as_ref()
            .expect("complete packet has first fragment")
            .packet_id;
        let item = self
            .packets
            .remove(&packet_id)
            .expect("fragment packet exists");
        reassemble(item).map(Some)
    }

    fn evict_stale(&mut self) {
        let now = Instant::now();
        self.packets
            .retain(|_, item| now.duration_since(item.created) <= FRAGMENT_TTL);
    }
}

struct FragmentPacket {
    created: Instant,
    fragments: Vec<Option<UdpMessage>>,
    received: usize,
    bytes: usize,
    addr: String,
}

impl FragmentPacket {
    fn new(total: usize, addr: String) -> Self {
        Self {
            created: Instant::now(),
            fragments: vec![None; total],
            received: 0,
            bytes: 0,
            addr,
        }
    }
}

fn reassemble(item: FragmentPacket) -> Result<UdpMessage> {
    let fragments: Vec<UdpMessage> = item
        .fragments
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| Error::protocol("incomplete UDP fragments"))?;
    let first = fragments
        .first()
        .ok_or_else(|| Error::protocol("empty UDP fragment set"))?
        .clone();
    let mut data = Vec::new();
    for fragment in &fragments {
        data.extend_from_slice(&fragment.data);
    }
    if data.len() > MAX_UDP_SIZE {
        return Err(Error::protocol("reassembled UDP payload is too large"));
    }
    Ok(UdpMessage {
        session_id: first.session_id,
        packet_id: first.packet_id,
        frag_id: 0,
        frag_count: 1,
        addr: first.addr,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment(packet_id: u16, frag_id: u8, size: usize) -> UdpMessage {
        UdpMessage {
            session_id: 1,
            packet_id,
            frag_id,
            frag_count: 2,
            addr: "localhost:53".into(),
            data: vec![frag_id; size],
        }
    }

    #[test]
    fn reassembly_bounds_duplicates_conflicts_and_expiry() {
        let mut d = UdpDefragger::new();
        let last = fragment(1, 1, 8192);
        assert!(d.feed(last.clone()).unwrap().is_none());
        assert!(d.feed(last).unwrap().is_none());
        assert_eq!(d.packets[&1].bytes, 8192);
        let full = d.feed(fragment(1, 0, 8192)).unwrap().unwrap();
        assert_eq!(full.data.len(), 16384);
        assert_eq!(&full.data[8192..], &[1; 8192]);
        assert!(d.packets.is_empty());

        d.feed(fragment(2, 0, MAX_UDP_SIZE)).unwrap();
        assert!(d.feed(fragment(2, 1, 1)).unwrap().is_none());
        assert!(d.packets.is_empty());
        for id in 0..20 {
            d.feed(fragment(id, 0, 1)).unwrap();
        }
        assert_eq!(d.packets.len(), MAX_PENDING_PACKETS);
        for item in d.packets.values_mut() {
            item.created = Instant::now() - FRAGMENT_TTL - Duration::from_secs(1);
        }
        d.feed(fragment(100, 0, 1)).unwrap();
        assert_eq!(d.packets.len(), 1);
        let mut conflicting = fragment(100, 1, 1);
        conflicting.addr = "other.example:53".into();
        assert!(d.feed(conflicting).unwrap().is_none());
        assert!(d.packets.is_empty());
        let mut empty = fragment(101, 0, 0);
        empty.frag_count = 1;
        assert!(d.feed(empty.clone()).unwrap().unwrap().data.is_empty());
        empty.frag_count = 0;
        assert!(d.feed(empty).unwrap().is_none());
    }

    #[test]
    fn defragger_reassembles_in_order() {
        let mut defragger = UdpDefragger::new();
        let first = UdpMessage {
            session_id: 1,
            packet_id: 2,
            frag_id: 0,
            frag_count: 2,
            addr: "127.0.0.1:53".into(),
            data: b"he".to_vec(),
        };
        let second = UdpMessage {
            frag_id: 1,
            data: b"llo".to_vec(),
            ..first.clone()
        };
        assert!(defragger.feed(first).unwrap().is_none());
        let complete = defragger.feed(second).unwrap().unwrap();
        assert_eq!(complete.data, b"hello");
    }
}
