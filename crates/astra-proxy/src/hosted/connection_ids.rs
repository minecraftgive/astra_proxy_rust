//! Bounded UI connection identities, independent of worker and host restarts.
use std::collections::{HashMap, HashSet};

const MAX_CONNECTIONS: usize = 16 * 128;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(super) struct Target {
    pub boot: String,
    pub generation: u64,
    pub worker: u64,
}

pub(super) struct ConnectionIds {
    by_target: HashMap<Target, u64>,
    by_id: HashMap<u64, Target>,
    next: Option<u64>,
    multiplier: u64,
    mask: u64,
}
impl Default for ConnectionIds {
    fn default() -> Self {
        Self::with_seed(uuid::Uuid::new_v4().as_u128())
    }
}
impl ConnectionIds {
    fn with_seed(seed: u128) -> Self {
        Self {
            by_target: HashMap::new(),
            by_id: HashMap::new(),
            next: Some(1),
            multiplier: (seed >> 64) as u64 | 1,
            mask: seed as u64,
        }
    }
    fn allocate(&mut self) -> Option<u64> {
        loop {
            let sequence = self.next?;
            self.next = sequence.checked_add(1);
            // An odd multiplication, rotation and XOR form a bijection on
            // u64. No ID is reused within this host, even after pruning, so
            // retaining an unbounded set of previously issued IDs is needless.
            // The fresh random seed separates host boots; this is an opaque
            // identifier, not an authentication token.
            let id = sequence.wrapping_mul(self.multiplier).rotate_left(27) ^ self.mask;
            if id != 0 && !self.by_id.contains_key(&id) {
                return Some(id);
            }
        }
    }
    /// Keep only connections represented in the latest bounded worker snapshots.
    /// Returns false if input or the ID space cannot be fully represented.
    pub fn reconcile(
        &mut self,
        boot: &str,
        connections: impl IntoIterator<Item = (u64, u64)>,
    ) -> bool {
        let mut current = HashSet::new();
        let mut complete = true;
        for (generation, worker) in connections {
            let target = Target {
                boot: boot.into(),
                generation,
                worker,
            };
            if current.contains(&target) {
                continue;
            }
            if current.len() == MAX_CONNECTIONS {
                complete = false;
                break;
            }
            current.insert(target);
        }
        self.by_target.retain(|target, _| current.contains(target));
        self.by_id.retain(|_, target| current.contains(target));
        for target in current {
            if self.by_target.contains_key(&target) {
                continue;
            }
            if let Some(id) = self.allocate() {
                self.by_target.insert(target.clone(), id);
                self.by_id.insert(id, target);
            } else {
                complete = false;
            }
        }
        complete
    }
    pub fn id(&self, boot: &str, generation: u64, worker: u64) -> Option<u64> {
        self.by_target
            .get(&Target {
                boot: boot.into(),
                generation,
                worker,
            })
            .copied()
    }
    pub fn resolve(&self, id: u64) -> Option<&Target> {
        self.by_id.get(&id)
    }
}

#[cfg(test)]
#[path = "../test/hosted_connection_ids_test.rs"]
mod tests;
