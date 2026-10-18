use crate::{accept::Protocol, wire};
use anyhow::{ensure, Context, Result};
use astra_ipc::Target;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
type HeldFragment = (Vec<u8>, tokio::sync::OwnedSemaphorePermit);
pub struct Fragments {
    budget: Arc<tokio::sync::Semaphore>,
    entries: HashMap<
        (u32, u16),
        (
            Instant,
            u8,
            Option<Target>,
            Vec<Option<HeldFragment>>,
            usize,
        ),
    >,
}
impl Default for Fragments {
    fn default() -> Self {
        Self::new(Arc::new(tokio::sync::Semaphore::new(16 * 1024 * 1024)))
    }
}
impl Fragments {
    pub fn expire(&mut self) {
        self.entries
            .retain(|_, e| e.0.elapsed() < Duration::from_secs(5));
    }
    pub fn new(budget: Arc<tokio::sync::Semaphore>) -> Self {
        Self {
            budget,
            entries: HashMap::new(),
        }
    }
    pub(crate) fn insert(
        &mut self,
        id: u32,
        packet: u16,
        count: u8,
        index: u8,
        target: Option<Target>,
        body: &[u8],
    ) -> Result<Option<(u32, Target, Vec<u8>)>> {
        ensure!(
            count > 0 && index < count && body.len() <= 65507,
            "invalid UDP fragment"
        );
        self.entries
            .retain(|_, v| v.0.elapsed() < Duration::from_secs(5));
        let key = (id, packet);
        ensure!(
            self.entries.contains_key(&key) || self.entries.len() < 8,
            "fragment limit"
        );
        let entry = self.entries.entry(key).or_insert_with(|| {
            (
                Instant::now(),
                count,
                None,
                std::iter::repeat_with(|| None)
                    .take(count as usize)
                    .collect(),
                0,
            )
        });
        ensure!(entry.1 == count, "conflicting fragment count");
        if let Some(t) = target {
            if let Some(old) = &entry.2 {
                ensure!(
                    old.host == t.host && old.port == t.port,
                    "conflicting fragment address"
                );
            }
            entry.2 = Some(t)
        }
        let slot = &mut entry.3[index as usize];
        if let Some(old) = slot {
            ensure!(old.0.as_slice() == body, "conflicting fragment data");
            return Ok(None);
        }
        ensure!(entry.4 + body.len() <= 65507, "reassembled UDP limit");
        let permit = self
            .budget
            .clone()
            .try_acquire_many_owned(body.len().max(1) as u32)
            .context("fragment buffer budget exhausted")?;
        entry.4 += body.len();
        *slot = Some((body.to_vec(), permit));
        if entry.3.iter().all(Option::is_some) && entry.2.is_some() {
            let (_, _, target, parts, _) = self.entries.remove(&key).unwrap();
            return Ok(Some((
                id,
                target.unwrap(),
                parts.into_iter().flat_map(|part| part.unwrap().0).collect(),
            )));
        }
        Ok(None)
    }
}
pub async fn decode(
    bytes: &[u8],
    protocol: Protocol,
    fragments: &mut Fragments,
) -> Result<Option<(u32, Target, Vec<u8>)>> {
    match protocol {
        Protocol::Tuic => crate::tuic::decode_datagram(bytes, fragments).await,
        Protocol::Hysteria2 => crate::hysteria2::decode_datagram(bytes, fragments).await,
        _ => anyhow::bail!("non-QUIC datagram protocol"),
    }
}

pub fn encode(
    id: u32,
    packet: u16,
    target: &Target,
    payload: &[u8],
    protocol: Protocol,
    max: usize,
) -> Result<Vec<Vec<u8>>> {
    let mut address = vec![];
    if protocol == Protocol::Tuic {
        address = wire::encode(target, 2)?
    } else {
        let host = if target.host.contains(':') {
            format!("[{}]:{}", target.host, target.port)
        } else {
            format!("{}:{}", target.host, target.port)
        };
        wire::put_varint(host.len() as u64, &mut address)?;
        address.extend(host.as_bytes());
    }
    let header = if protocol == Protocol::Tuic { 10 } else { 8 };
    ensure!(max > header + address.len(), "QUIC datagram MTU");
    let size = max - header - address.len();
    let count = payload.len().max(1).div_ceil(size);
    ensure!(count <= 255, "UDP fragment count");
    let mut parts = vec![];
    for index in 0..count {
        let begin = index * size;
        let body = &payload[begin.min(payload.len())..((index + 1) * size).min(payload.len())];
        let mut out = vec![];
        if protocol == Protocol::Tuic {
            out.extend([5, 2]);
            out.extend((id as u16).to_be_bytes());
            out.extend(packet.to_be_bytes());
            out.extend([count as u8, index as u8]);
            out.extend((body.len() as u16).to_be_bytes());
        } else {
            out.extend(id.to_be_bytes());
            out.extend(packet.to_be_bytes());
            out.extend([index as u8, count as u8]);
        }
        if protocol == Protocol::Tuic && index > 0 {
            out.push(255)
        } else {
            out.extend(&address)
        }
        out.extend(body);
        parts.push(out);
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn quic_fragments_reorder_and_preserve_empty_datagrams() {
        for protocol in [Protocol::Tuic, Protocol::Hysteria2] {
            for size in [0, 1, 8192, 65507] {
                let target = Target {
                    host: "::1".into(),
                    port: 53,
                };
                let payload = vec![42; size];
                let parts = encode(7, 9, &target, &payload, protocol, 1200).unwrap();
                if protocol == Protocol::Tuic && parts.len() > 1 {
                    assert_eq!(parts[1][10], 255);
                }
                let mut state = Fragments::default();
                let mut result = None;
                for part in parts.into_iter().rev() {
                    if let Some(packet) = decode(&part, protocol, &mut state).await.unwrap() {
                        result = Some(packet);
                    }
                }
                let (id, to, data) = result.unwrap();
                assert_eq!(id, 7);
                assert_eq!(to, target);
                assert_eq!(data, payload);
                assert!(state.entries.is_empty());
            }
        }
    }
    #[test]
    fn fragment_state_rejects_conflicts_and_bounds_retained_payload() {
        let mut state = Fragments::default();
        let target = Target {
            host: "example.test".into(),
            port: 53,
        };
        assert!(state
            .insert(1, 1, 2, 0, Some(target.clone()), b"hello")
            .unwrap()
            .is_none());
        assert!(state.insert(1, 1, 2, 0, None, b"hello").unwrap().is_none());
        assert!(state.insert(1, 1, 2, 0, None, b"changed").is_err());
        assert!(state.insert(1, 1, 3, 1, None, b"x").is_err());
        assert!(state
            .insert(1, 2, 2, 0, Some(target), &vec![0; 65507])
            .unwrap()
            .is_none());
        assert!(state.insert(1, 2, 2, 1, None, b"x").is_err());
    }
}
