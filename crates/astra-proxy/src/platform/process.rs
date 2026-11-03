//! Resolve local socket ownership outside the async I/O threads.
use astra_model::Network;
use astra_model::ProcessInfo;
use std::{
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

#[derive(Default)]
struct Workers(arc_swap::ArcSwap<std::collections::HashSet<u32>>);
impl Workers {
    fn register(&self, pid: u32) {
        self.0.rcu(|previous| {
            let mut next = (**previous).clone();
            next.insert(pid);
            Arc::new(next)
        });
    }
    fn unregister(&self, pid: u32) {
        self.0.rcu(|previous| {
            let mut next = (**previous).clone();
            next.remove(&pid);
            Arc::new(next)
        });
    }
    fn contains(&self, pid: u32) -> bool {
        self.0.load().contains(&pid)
    }
}
fn workers() -> &'static Workers {
    // Publish whole exclusion sets. A panic while preparing an update must not
    // erase existing workers and allow their traffic to be captured recursively.
    static WORKERS: OnceLock<Workers> = OnceLock::new();
    WORKERS.get_or_init(Default::default)
}
pub(crate) fn register_worker(pid: u32) {
    workers().register(pid);
}
pub(crate) fn unregister_worker(pid: u32) {
    workers().unregister(pid);
}
pub(crate) fn own_process(pid: u32) -> bool {
    pid == std::process::id() || workers().contains(pid)
}

pub(crate) async fn lookup(
    network: Network,
    source: SocketAddr,
    destination: SocketAddr,
) -> Option<ProcessInfo> {
    bounded_lookup(network, source, destination, resolve).await
}

/// Runs for packets in the TUN reader, so on Windows it only consults the
/// shared owner-table snapshot (refreshed at most every 100 ms) and never
/// forces a fresh dump per packet. A socket opened since the last snapshot
/// can be missed; routing then rejects that recaptured connection after
/// its own, fresh lookup.
pub(crate) async fn is_self(network: Network, source: SocketAddr, destination: SocketAddr) -> bool {
    bounded_lookup(
        network,
        source,
        destination,
        |network, source, destination| {
            #[cfg(windows)]
            let pid = windows::owner_pid_snapshot(network, source, destination);
            #[cfg(not(windows))]
            let pid = resolve(network, source, destination).map(|process| process.pid);
            pid.map(own_process)
        },
    )
    .await
    .unwrap_or(false)
}

async fn bounded_lookup<T: Send + 'static>(
    network: Network,
    source: SocketAddr,
    destination: SocketAddr,
    resolve: impl FnOnce(Network, SocketAddr, SocketAddr) -> Option<T> + Send + 'static,
) -> Option<T> {
    static LIMIT: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let limit = LIMIT
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(16)))
        .clone();
    // A busy or inaccessible process table must not stall every new connection.
    tokio::time::timeout(Duration::from_millis(250), async move {
        let permit = limit.acquire_owned().await.ok()?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // UDP owner tables expose only a local bind. Never attribute a
            // remote client's coincidentally equal source port to a local app.
            if network == Network::Udp
                && (source.ip().is_unspecified()
                    || std::net::UdpSocket::bind(SocketAddr::new(source.ip(), 0)).is_err())
            {
                return None;
            }
            resolve(network, source, destination)
        })
        .await
        .ok()
        .flatten()
    })
    .await
    .ok()
    .flatten()
}

#[cfg(any(windows, target_os = "linux"))]
fn info(pid: u32, path: String) -> Option<ProcessInfo> {
    let name = path.rsplit(['/', '\\']).next()?.to_owned();
    if name.is_empty() || name.len() > 256 || path.len() > 4096 {
        return None;
    }
    Some(ProcessInfo { pid, name, path })
}

#[cfg(windows)]
fn resolve(network: Network, source: SocketAddr, destination: SocketAddr) -> Option<ProcessInfo> {
    windows::resolve(network, source, destination)
}
#[cfg(target_os = "linux")]
fn resolve(network: Network, source: SocketAddr, destination: SocketAddr) -> Option<ProcessInfo> {
    linux::resolve(network, source, destination)
}
#[cfg(not(any(windows, target_os = "linux")))]
fn resolve(
    _network: Network,
    _source: SocketAddr,
    _destination: SocketAddr,
) -> Option<ProcessInfo> {
    None
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        mem::size_of,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER},
        NetworkManagement::IpHelper::*,
        Networking::WinSock::{AF_INET, AF_INET6},
        System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };

    /// Owner tables are shared for a short window, so a burst of new
    /// connections costs one table dump instead of one per connection.
    /// Lookups that find no match re-query with `fresh = true`.
    const SNAPSHOT_TTL: Duration = Duration::from_millis(100);
    type Snapshot = (std::time::Instant, Arc<(Vec<u32>, u32)>);
    static SNAPSHOTS: std::sync::Mutex<[Option<Snapshot>; 4]> =
        std::sync::Mutex::new([None, None, None, None]);

    fn raw(network: Network, ipv4: bool, fresh: bool) -> Option<Arc<(Vec<u32>, u32)>> {
        let slot = usize::from(network == Network::Udp) * 2 + usize::from(ipv4);
        if !fresh {
            if let Some((at, data)) = &SNAPSHOTS.lock().ok()?[slot] {
                if at.elapsed() < SNAPSHOT_TTL {
                    return Some(data.clone());
                }
            }
        }
        let family = if ipv4 { AF_INET } else { AF_INET6 } as u32;
        let query = |pointer, size| unsafe {
            match network {
                Network::Tcp => {
                    GetExtendedTcpTable(pointer, size, 0, family, TCP_TABLE_OWNER_PID_ALL, 0)
                }
                Network::Udp => {
                    GetExtendedUdpTable(pointer, size, 0, family, UDP_TABLE_OWNER_PID, 0)
                }
            }
        };
        let mut size = 0;
        if query(std::ptr::null_mut(), &mut size) != ERROR_INSUFFICIENT_BUFFER {
            return None;
        }
        for _ in 0..3 {
            if !(4..=16 * 1024 * 1024).contains(&size) {
                return None;
            }
            // DWORD alignment satisfies all four owner-PID table layouts.
            let mut data = vec![0u32; (size as usize).div_ceil(4)];
            let result = query(data.as_mut_ptr().cast(), &mut size);
            if result == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            if result != 0 || size as usize > data.len() * 4 {
                return None;
            }
            let data = Arc::new((data, size));
            if let Ok(mut snapshots) = SNAPSHOTS.lock() {
                snapshots[slot] = Some((std::time::Instant::now(), data.clone()));
            }
            return Some(data);
        }
        None
    }

    fn table<T: Copy>(network: Network, ipv4: bool, fresh: bool) -> Option<Vec<T>> {
        let raw = raw(network, ipv4, fresh)?;
        let (data, size) = &*raw;
        let count = data[0] as usize;
        if count > (*size as usize).saturating_sub(4) / size_of::<T>() {
            return None;
        }
        let pointer = unsafe { data.as_ptr().cast::<u8>().add(4) };
        Some(
            (0..count)
                .map(|i| unsafe { pointer.add(i * size_of::<T>()).cast::<T>().read_unaligned() })
                .collect(),
        )
    }
    fn v4(address: u32, port: u32) -> SocketAddr {
        SocketAddr::new(
            IpAddr::V4(Ipv4Addr::from(address.to_ne_bytes())),
            u16::from_be(port as u16),
        )
    }
    fn v6(address: [u8; 16], port: u32) -> SocketAddr {
        SocketAddr::new(
            IpAddr::V6(Ipv6Addr::from(address)),
            u16::from_be(port as u16),
        )
    }
    fn same(left: SocketAddr, right: SocketAddr) -> bool {
        left.port() == right.port() && left.ip().to_canonical() == right.ip().to_canonical()
    }
    fn unique(pids: impl IntoIterator<Item = u32>) -> Option<u32> {
        let mut pids = pids.into_iter();
        let first = pids.next()?;
        (first != 0 && pids.all(|pid| pid == first)).then_some(first)
    }
    pub(super) fn owner_pid(
        network: Network,
        source: SocketAddr,
        destination: SocketAddr,
    ) -> Option<u32> {
        owner_pid_in(network, source, destination, false)
            .or_else(|| owner_pid_in(network, source, destination, true))
    }
    pub(super) fn owner_pid_snapshot(
        network: Network,
        source: SocketAddr,
        destination: SocketAddr,
    ) -> Option<u32> {
        owner_pid_in(network, source, destination, false)
    }
    fn owner_pid_in(
        network: Network,
        source: SocketAddr,
        destination: SocketAddr,
        fresh: bool,
    ) -> Option<u32> {
        let source = SocketAddr::new(source.ip().to_canonical(), source.port());
        let destination = SocketAddr::new(destination.ip().to_canonical(), destination.port());
        match (network, source.is_ipv4()) {
            (Network::Tcp, true) => unique(
                table::<MIB_TCPROW_OWNER_PID>(network, true, fresh)?
                    .into_iter()
                    .filter(|row| {
                        same(v4(row.dwLocalAddr, row.dwLocalPort), source)
                            && same(v4(row.dwRemoteAddr, row.dwRemotePort), destination)
                    })
                    .map(|row| row.dwOwningPid),
            ),
            (Network::Tcp, false) => unique(
                table::<MIB_TCP6ROW_OWNER_PID>(network, false, fresh)?
                    .into_iter()
                    .filter(|row| {
                        same(v6(row.ucLocalAddr, row.dwLocalPort), source)
                            && same(v6(row.ucRemoteAddr, row.dwRemotePort), destination)
                    })
                    .map(|row| row.dwOwningPid),
            ),
            (Network::Udp, true) => {
                let rows = table::<MIB_UDPROW_OWNER_PID>(network, true, fresh)?;
                let candidates: Vec<_> = rows
                    .iter()
                    .filter(|row| {
                        u16::from_be(row.dwLocalPort as u16) == source.port()
                            && (row.dwLocalAddr == 0
                                || same(v4(row.dwLocalAddr, row.dwLocalPort), source))
                    })
                    .map(|row| row.dwOwningPid)
                    .collect();
                unique(candidates)
            }
            (Network::Udp, false) => {
                let rows = table::<MIB_UDP6ROW_OWNER_PID>(network, false, fresh)?;
                unique(
                    rows.iter()
                        .filter(|row| {
                            u16::from_be(row.dwLocalPort as u16) == source.port()
                                && (row.ucLocalAddr == [0; 16]
                                    || same(v6(row.ucLocalAddr, row.dwLocalPort), source))
                        })
                        .map(|row| row.dwOwningPid),
                )
            }
        }
    }

    pub(super) fn resolve(
        network: Network,
        source: SocketAddr,
        destination: SocketAddr,
    ) -> Option<ProcessInfo> {
        let pid = owner_pid(network, source, destination)?;
        // Image paths are cached briefly; PID reuse inside the window is
        // far less likely than the cost of OpenProcess on every connection.
        type PathCache = std::collections::HashMap<u32, (std::time::Instant, String)>;
        static PATHS: std::sync::Mutex<Option<PathCache>> = std::sync::Mutex::new(None);
        if let Some((at, path)) = PATHS
            .lock()
            .ok()?
            .get_or_insert_with(Default::default)
            .get(&pid)
        {
            if at.elapsed() < Duration::from_secs(2) {
                return info(pid, path.clone());
            }
        }
        let path = unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let mut path = vec![0u16; 32768];
            let mut length = path.len() as u32;
            let result = QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut length);
            CloseHandle(handle);
            if result == 0 {
                return None;
            }
            String::from_utf16(&path[..length as usize]).ok()?
        };
        if let Ok(mut paths) = PATHS.lock() {
            let paths = paths.get_or_insert_with(Default::default);
            if paths.len() >= 1024 {
                paths.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(2));
            }
            paths.insert(pid, (std::time::Instant::now(), path.clone()));
        }
        info(pid, path)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        collections::HashSet,
        fs,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
        time::Instant,
    };
    fn address(value: &str) -> Option<SocketAddr> {
        let (ip, port) = value.split_once(':')?;
        let ip = match ip.len() {
            8 => IpAddr::V4(Ipv4Addr::from(
                u32::from_str_radix(ip, 16).ok()?.to_ne_bytes(),
            )),
            32 => {
                let mut bytes = [0; 16];
                for (i, chunk) in bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    chunk.copy_from_slice(
                        &u32::from_str_radix(&ip[i * 8..i * 8 + 8], 16)
                            .ok()?
                            .to_ne_bytes(),
                    );
                }
                IpAddr::V6(Ipv6Addr::from(bytes))
            }
            _ => return None,
        };
        Some(SocketAddr::new(
            ip.to_canonical(),
            u16::from_str_radix(port, 16).ok()?,
        ))
    }
    const SNAPSHOT_TTL: Duration = Duration::from_millis(100);
    const OWNER_LIMIT: usize = 65_536;
    // After an oversized owner table, use targeted scans for a while instead
    // of rebuilding a table that will overflow again.
    const OVERFLOW_RETRY: Duration = Duration::from_secs(10);
    #[derive(Clone)]
    struct Socket {
        local: SocketAddr,
        remote: SocketAddr,
        inode: u64,
    }
    type SocketTable = std::collections::HashMap<u16, Vec<Socket>>;
    type SocketSnapshot = (Instant, Arc<SocketTable>);
    static TABLES: std::sync::Mutex<[Option<SocketSnapshot>; 2]> =
        std::sync::Mutex::new([None, None]);

    /// `newer_than` asks for a snapshot taken after that instant. Callers that
    /// queued behind a refresh which started after their own lookup reuse it
    /// instead of rescanning /proc once each.
    fn reusable(at: Instant, newer_than: Option<Instant>) -> bool {
        match newer_than {
            Some(start) => at >= start,
            None => at.elapsed() < SNAPSHOT_TTL,
        }
    }
    fn parse_socket(line: &str) -> Option<Socket> {
        let mut fields = line.split_whitespace();
        let local = address(fields.nth(1)?)?;
        let remote = address(fields.next()?)?;
        let inode = fields.nth(6)?.parse().ok()?;
        (inode != 0).then_some(Socket {
            local,
            remote,
            inode,
        })
    }
    fn socket_table(network: Network, newer_than: Option<Instant>) -> Option<Arc<SocketTable>> {
        let index = usize::from(network == Network::Udp);
        let mut tables = TABLES.lock().ok()?;
        if let Some((at, table)) = &tables[index] {
            if reusable(*at, newer_than) {
                return Some(table.clone());
            }
        }
        let base = if network == Network::Tcp {
            "tcp"
        } else {
            "udp"
        };
        let mut table = SocketTable::new();
        for suffix in ["", "6"] {
            let Ok(text) = fs::read_to_string(format!("/proc/net/{base}{suffix}")) else {
                continue;
            };
            for socket in text.lines().skip(1).filter_map(parse_socket) {
                table.entry(socket.local.port()).or_default().push(socket);
            }
        }
        let table = Arc::new(table);
        tables[index] = Some((Instant::now(), table.clone()));
        Some(table)
    }
    fn matching_inodes(
        table: &SocketTable,
        network: Network,
        source: SocketAddr,
        destination: SocketAddr,
    ) -> HashSet<u64> {
        table
            .get(&source.port())
            .into_iter()
            .flatten()
            .filter(|socket| {
                (socket.local == source
                    || (network == Network::Udp && socket.local.ip().is_unspecified()))
                    && (socket.remote == destination
                        || (network == Network::Udp && socket.remote.ip().is_unspecified()))
            })
            .map(|socket| socket.inode)
            .collect()
    }
    // A snapshot maps every visible socket inode to its unique owner. Scanning
    // once per short window also amortizes bursts of different new connections.
    // None records ambiguity (forked/shared descriptors), never a first-owner win.
    type Owners = std::collections::HashMap<u64, Option<u32>>;
    #[derive(Clone)]
    enum OwnerSnapshot {
        Table(Arc<Owners>),
        /// The host has more socket descriptors than `OWNER_LIMIT`.
        Overflow,
    }
    static OWNERS: std::sync::Mutex<Option<(Instant, OwnerSnapshot)>> = std::sync::Mutex::new(None);
    fn add_owner(owners: &mut Owners, inode: u64, pid: u32) {
        owners
            .entry(inode)
            .and_modify(|owner| {
                if *owner != Some(pid) {
                    *owner = None;
                }
            })
            .or_insert(Some(pid));
    }
    /// Visits every socket descriptor as `(inode, pid)` until `visit` returns false.
    fn scan_socket_owners(mut visit: impl FnMut(u64, u32) -> bool) -> Option<()> {
        for entry in fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                let Ok(path) = fs::read_link(fd.path()) else {
                    continue;
                };
                let Some(inode) = path
                    .to_str()
                    .and_then(|path| path.strip_prefix("socket:["))
                    .and_then(|path| path.strip_suffix(']'))
                    .and_then(|inode| inode.parse().ok())
                else {
                    continue;
                };
                if !visit(inode, pid) {
                    return None;
                }
            }
        }
        Some(())
    }
    fn owner_table(newer_than: Option<Instant>) -> Option<OwnerSnapshot> {
        let mut snapshot = OWNERS.lock().ok()?;
        match &*snapshot {
            Some((at, OwnerSnapshot::Overflow)) if at.elapsed() < OVERFLOW_RETRY => {
                return Some(OwnerSnapshot::Overflow);
            }
            Some((at, owners @ OwnerSnapshot::Table(_))) if reusable(*at, newer_than) => {
                return Some(owners.clone());
            }
            _ => {}
        }
        let mut owners = Owners::new();
        // Do not keep a partially scanned ownership table: a later process
        // could share an inode already seen.
        let owners = match scan_socket_owners(|inode, pid| {
            add_owner(&mut owners, inode, pid);
            owners.len() <= OWNER_LIMIT
        }) {
            Some(()) => OwnerSnapshot::Table(Arc::new(owners)),
            None => OwnerSnapshot::Overflow,
        };
        *snapshot = Some((Instant::now(), owners.clone()));
        Some(owners)
    }
    /// Uncached lookup that records only the requested inodes, for hosts whose
    /// full ownership table exceeds `OWNER_LIMIT`.
    fn scan_owner(inodes: &HashSet<u64>) -> Option<u32> {
        let mut owners = Owners::new();
        scan_socket_owners(|inode, pid| {
            if inodes.contains(&inode) {
                add_owner(&mut owners, inode, pid);
            }
            true
        })?;
        unique_owner(&owners, inodes)
    }
    fn unique_owner(owners: &Owners, inodes: &HashSet<u64>) -> Option<u32> {
        let mut owner = None;
        for inode in inodes {
            // An unowned/inaccessible inode contributes no visible owner, as
            // before; an explicitly ambiguous inode must fail closed.
            if let Some(pid) = owners.get(inode) {
                let pid = (*pid)?;
                if owner.is_some_and(|other| other != pid) {
                    return None;
                }
                owner = Some(pid);
            }
        }
        owner
    }
    pub(super) fn resolve(
        network: Network,
        source: SocketAddr,
        destination: SocketAddr,
    ) -> Option<ProcessInfo> {
        let started = Instant::now();
        let source = SocketAddr::new(source.ip().to_canonical(), source.port());
        let destination = SocketAddr::new(destination.ip().to_canonical(), destination.port());
        let mut inodes = matching_inodes(
            socket_table(network, None)?.as_ref(),
            network,
            source,
            destination,
        );
        if inodes.is_empty() {
            // Fresh sockets must not wait for the snapshot to expire.
            inodes = matching_inodes(
                socket_table(network, Some(started))?.as_ref(),
                network,
                source,
                destination,
            );
        }
        if inodes.is_empty() {
            return None;
        }
        let mut owners = owner_table(None)?;
        if let OwnerSnapshot::Table(table) = &owners {
            if inodes.iter().any(|inode| !table.contains_key(inode)) {
                owners = owner_table(Some(started))?;
            }
        }
        let pid = match owners {
            OwnerSnapshot::Table(table) => unique_owner(&table, &inodes),
            OwnerSnapshot::Overflow => scan_owner(&inodes),
        }?;
        info(
            pid,
            fs::read_link(format!("/proc/{pid}/exe"))
                .ok()?
                .to_str()?
                .to_owned(),
        )
    }
    #[cfg(test)]
    mod cache_tests {
        use super::*;
        #[test]
        fn parse_proc_columns_without_allocating_fields() {
            let socket = parse_socket(" 0: 0100007F:1234 00000000:0000 07 00000000:00000000 00:00000000 00000000 1000 0 42 2").unwrap();
            assert_eq!(socket.local, "127.0.0.1:4660".parse().unwrap());
            assert_eq!(socket.inode, 42);
            assert!(parse_socket("short row").is_none());
        }
        #[test]
        fn shared_inodes_and_multiple_owners_remain_ambiguous() {
            let mut owners = Owners::new();
            add_owner(&mut owners, 1, 10);
            add_owner(&mut owners, 1, 10);
            assert_eq!(unique_owner(&owners, &HashSet::from([1])), Some(10));
            add_owner(&mut owners, 2, 20);
            assert_eq!(unique_owner(&owners, &HashSet::from([1, 2])), None);
            add_owner(&mut owners, 1, 20);
            add_owner(&mut owners, 1, 10);
            assert_eq!(unique_owner(&owners, &HashSet::from([1])), None);
        }
        #[test]
        fn forced_refresh_reuses_snapshots_taken_after_the_lookup_started() {
            let start = Instant::now();
            let before = start - Duration::from_millis(1);
            assert!(reusable(start, Some(start)));
            assert!(!reusable(before, Some(start)));
            assert!(reusable(before, None));
            assert!(!reusable(start - SNAPSHOT_TTL, None));
        }
        #[test]
        fn overflow_fallback_scans_only_requested_inodes() {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let local = listener.local_addr().unwrap();
            let table = socket_table(Network::Tcp, Some(Instant::now())).unwrap();
            let inodes: HashSet<u64> = table[&local.port()]
                .iter()
                .filter(|socket| socket.local == local)
                .map(|socket| socket.inode)
                .collect();
            assert_eq!(scan_owner(&inodes), Some(std::process::id()));
            assert_eq!(scan_owner(&HashSet::from([u64::MAX])), None);
        }
        #[test]
        fn wildcard_udp_matching_keeps_remote_semantics() {
            let socket = Socket {
                local: "0.0.0.0:1234".parse().unwrap(),
                remote: "0.0.0.0:0".parse().unwrap(),
                inode: 42,
            };
            let table = SocketTable::from([(1234, vec![socket])]);
            let source = "127.0.0.1:1234".parse().unwrap();
            let destination = "203.0.113.1:443".parse().unwrap();
            assert_eq!(
                matching_inodes(&table, Network::Udp, source, destination),
                HashSet::from([42])
            );
            assert!(matching_inodes(&table, Network::Tcp, source, destination).is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_exclusions_survive_interrupted_update_and_unwind_cleanup() {
        let workers = Workers::default();
        workers.register(101);
        workers.register(202);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            workers
                .0
                .rcu(|previous| -> Arc<std::collections::HashSet<u32>> {
                    let mut next = (**previous).clone();
                    next.clear();
                    panic!("interrupted worker exclusion update");
                });
        }))
        .is_err());
        assert!(workers.contains(101));
        assert!(workers.contains(202));
        struct Unregister<'a>(&'a Workers);
        impl Drop for Unregister<'_> {
            fn drop(&mut self) {
                self.0.unregister(101);
            }
        }
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _cleanup = Unregister(&workers);
            panic!("preparation failed");
        }))
        .is_err());
        assert!(!workers.contains(101));
        assert!(workers.contains(202));
        workers.register(303);
        assert!(workers.contains(202));
        assert!(workers.contains(303));
    }
    #[tokio::test]
    async fn ipv6_tcp_and_udp_socket_owners_are_attributed() {
        if !cfg!(any(windows, target_os = "linux")) {
            return;
        }
        let Ok(listener) = tokio::net::TcpListener::bind("[::1]:0").await else {
            return;
        };
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (_server, _) = listener.accept().await.unwrap();
        let owner = lookup(
            Network::Tcp,
            client.local_addr().unwrap(),
            listener.local_addr().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(owner.pid, std::process::id());
        let udp = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
        let source = SocketAddr::new("::1".parse().unwrap(), udp.local_addr().unwrap().port());
        let owner = lookup(Network::Udp, source, listener.local_addr().unwrap())
            .await
            .unwrap();
        assert_eq!(owner.pid, std::process::id());
    }
    #[tokio::test]
    async fn local_tcp_and_udp_socket_owners_are_attributed() {
        if !cfg!(any(windows, target_os = "linux")) {
            return;
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let owner = lookup(
            Network::Tcp,
            client.local_addr().unwrap(),
            server.local_addr().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(owner.pid, std::process::id());
        assert_eq!(
            owner.name,
            std::env::current_exe()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
        );
        let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let owner = lookup(
            Network::Udp,
            udp.local_addr().unwrap(),
            listener.local_addr().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(owner.pid, std::process::id());
        assert!(lookup(
            Network::Udp,
            SocketAddr::new(
                "203.0.113.1".parse().unwrap(),
                udp.local_addr().unwrap().port()
            ),
            listener.local_addr().unwrap()
        )
        .await
        .is_none());
        assert!(lookup(
            Network::Tcp,
            client.local_addr().unwrap(),
            "203.0.113.1:80".parse().unwrap()
        )
        .await
        .is_none());
    }
}
