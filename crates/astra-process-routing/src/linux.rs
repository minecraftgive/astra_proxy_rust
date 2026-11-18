//! cgroup v2 classification. The Agent owns the links independently of proxy workers.
use anyhow::{ensure, Context, Result};
use aya::{
    maps::{HashMap, MapData},
    programs::{CgroupAttachMode, CgroupSockAddr},
    Ebpf, Pod,
};
use std::{fs::File, num::NonZeroU32, os::unix::fs::MetadataExt, path::Path};

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
#[repr(C)]
struct Component {
    suffix: u32,
    name: [u8; 256],
}
// repr(C), no padding or invalid bit patterns; matches capture.bpf.c.
unsafe impl Pod for Component {}

#[derive(Clone, Copy)]
#[repr(C)]
struct Executable {
    root_inode: u64,
    root_device: u32,
    path_id: u32,
}
unsafe impl Pod for Executable {}

#[derive(Clone, Copy)]
pub enum Disposition {
    Block,
    /// The matching TPROXY listener, policy route and fail-closed filter must
    /// be installed before publishing this mark.
    Transparent(NonZeroU32),
}

pub struct Capture {
    components: HashMap<MapData, Component, u32>,
    bindings: HashMap<MapData, Executable, u32>,
    forwarders: HashMap<MapData, u64, u32>,
    cgroup: std::path::PathBuf,
    root_inode: u64,
    root_device: u32,
    next_id: u32,
    users: std::collections::HashMap<Component, usize>,
    // Dropping this object detaches only the links we created.
    _links: Ebpf,
}

impl Capture {
    pub fn attach(object: &[u8], cgroup: &Path) -> Result<Self> {
        let root = std::fs::metadata("/")?;
        // Kernel dev_t uses 12 major bits and 20 minor bits. libc's st_dev
        // is the userspace encoding of those fields.
        let root_device = (libc::major(root.dev()) << 20) | libc::minor(root.dev());
        let directory = File::open(cgroup)?;
        // The kernel owns SK_STORAGE; userspace only needs typed access to the two hash maps.
        let mut bpf = aya::EbpfLoader::new()
            .allow_unsupported_maps()
            .load(object)?;
        let components =
            HashMap::try_from(bpf.take_map("components").context("missing path trie")?)?;
        let bindings = HashMap::try_from(bpf.take_map("bindings").context("missing bindings")?)?;
        let forwarders = HashMap::try_from(
            bpf.take_map("forwarders")
                .context("missing worker exclusions")?,
        )?;
        for name in ["connect4", "connect6", "sendmsg4", "sendmsg6"] {
            let program: &mut CgroupSockAddr = bpf
                .program_mut(name)
                .context("missing capture hook")?
                .try_into()?;
            program
                .load()
                .with_context(|| format!("kernel rejected {name}"))?;
            program
                .attach(&directory, CgroupAttachMode::Single)
                .with_context(|| format!("attach {name} to {}", cgroup.display()))?;
        }
        Ok(Self {
            components,
            bindings,
            forwarders,
            cgroup: cgroup.canonicalize()?,
            root_inode: root.ino(),
            root_device,
            next_id: 1,
            users: Default::default(),
            _links: bpf,
        })
    }

    /// Exempt exactly one supervisor-owned child cgroup. Place the core and
    /// forwarding workers there before they create outbound sockets.
    pub fn exclude_forwarders(&mut self, directory: &Path) -> Result<u64> {
        let path = directory.canonicalize()?;
        ensure!(
            path != self.cgroup
                && path.starts_with(&self.cgroup)
                && path.join("cgroup.procs").is_file(),
            "expected a forwarding cgroup beneath the captured subtree"
        );
        let id = path.metadata()?.ino();
        self.forwarders.insert(id, 1, 0)?;
        Ok(id)
    }

    pub fn remove_forwarder_exclusion(&mut self, id: u64) -> Result<()> {
        Ok(self.forwarders.remove(&id)?)
    }

    /// Add/replace one exact executable path atomically at the terminal trie node.
    /// A file that does not yet exist can use its future canonical path.
    pub fn set(&mut self, path: &Path, disposition: Disposition) -> Result<()> {
        let mut nodes = Vec::new();
        let result = (|| {
            let key = self.key(path, true, &mut nodes)?;
            let inserting = match self.bindings.get(&key, 0) {
                Ok(_) => false,
                Err(aya::maps::MapError::KeyNotFound) => true,
                Err(error) => return Err(error.into()),
            };
            let mark = match disposition {
                Disposition::Block => 0,
                Disposition::Transparent(mark) => mark.get(),
            };
            self.bindings.insert(key, mark, 0)?;
            if inserting {
                for node in &nodes {
                    *self.users.entry(*node).or_default() += 1;
                }
            }
            Ok(())
        })();
        if result.is_err() {
            self.prune(&nodes)?;
        }
        result
    }

    pub fn remove(&mut self, path: &Path) -> Result<()> {
        let mut nodes = Vec::new();
        let key = self.key(path, false, &mut nodes)?;
        self.bindings.remove(&key)?;
        for node in &nodes {
            if let std::collections::hash_map::Entry::Occupied(mut count) = self.users.entry(*node)
            {
                *count.get_mut() -= 1;
                if *count.get() == 0 {
                    count.remove();
                }
            }
        }
        self.prune(&nodes)
    }

    fn prune(&mut self, nodes: &[Component]) -> Result<()> {
        for node in nodes.iter().rev() {
            if !self.users.contains_key(node) {
                self.components.remove(node)?;
            }
        }
        Ok(())
    }

    fn key(&mut self, path: &Path, create: bool, nodes: &mut Vec<Component>) -> Result<Executable> {
        use std::os::unix::ffi::OsStrExt;
        ensure!(
            path.is_absolute() && path.file_name().is_some(),
            "expected absolute executable path"
        );
        let bytes = path.as_os_str().as_bytes();
        ensure!(
            bytes.len() < 4096 && !bytes.contains(&0),
            "invalid executable path"
        );
        // Reject lexical aliases. The caller resolves symlinks on the target host.
        let parts = bytes[1..].split(|b| *b == b'/');
        ensure!(
            parts
                .clone()
                .all(|s| !s.is_empty() && s != b".." && s != b"." && s.len() <= 255),
            "expected normalized executable path"
        );
        let mut suffix = 0;
        for part in parts.rev() {
            let mut key = Component {
                suffix,
                name: [0; 256],
            };
            key.name[..part.len()].copy_from_slice(part);
            suffix = match self.components.get(&key, 0) {
                Ok(id) => id,
                Err(aya::maps::MapError::KeyNotFound) if create => {
                    let id = self.next_id;
                    let next_id = id.checked_add(1).context("path trie exhausted")?;
                    self.components.insert(key, id, 1 /* BPF_NOEXIST */)?;
                    self.next_id = next_id;
                    id
                }
                Err(error) => return Err(error.into()),
            };
            nodes.push(key);
        }
        Ok(Executable {
            root_inode: self.root_inode,
            root_device: self.root_device,
            path_id: suffix,
        })
    }
}

#[cfg(test)]
#[path = "test/linux_capture_test.rs"]
mod tests;
