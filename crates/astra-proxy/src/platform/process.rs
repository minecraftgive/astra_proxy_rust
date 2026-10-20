//! Resolve local socket ownership outside the async I/O threads.
use astra_ipc::{Network, ProcessInfo};
use std::{
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

pub(crate) async fn lookup(
    network: Network,
    source: SocketAddr,
    destination: SocketAddr,
) -> Option<ProcessInfo> {
    static LIMIT: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let limit = LIMIT
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
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

    fn table<T: Copy>(network: Network, ipv4: bool) -> Option<Vec<T>> {
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
            let count = data[0] as usize;
            if count > (size as usize).saturating_sub(4) / size_of::<T>() {
                return None;
            }
            let pointer = unsafe { data.as_ptr().cast::<u8>().add(4) };
            return Some(
                (0..count)
                    .map(|i| unsafe {
                        pointer.add(i * size_of::<T>()).cast::<T>().read_unaligned()
                    })
                    .collect(),
            );
        }
        None
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
    pub(super) fn resolve(
        network: Network,
        source: SocketAddr,
        destination: SocketAddr,
    ) -> Option<ProcessInfo> {
        let source = SocketAddr::new(source.ip().to_canonical(), source.port());
        let destination = SocketAddr::new(destination.ip().to_canonical(), destination.port());
        let pid = match (network, source.is_ipv4()) {
            (Network::Tcp, true) => unique(
                table::<MIB_TCPROW_OWNER_PID>(network, true)?
                    .into_iter()
                    .filter(|row| {
                        same(v4(row.dwLocalAddr, row.dwLocalPort), source)
                            && same(v4(row.dwRemoteAddr, row.dwRemotePort), destination)
                    })
                    .map(|row| row.dwOwningPid),
            ),
            (Network::Tcp, false) => unique(
                table::<MIB_TCP6ROW_OWNER_PID>(network, false)?
                    .into_iter()
                    .filter(|row| {
                        same(v6(row.ucLocalAddr, row.dwLocalPort), source)
                            && same(v6(row.ucRemoteAddr, row.dwRemotePort), destination)
                    })
                    .map(|row| row.dwOwningPid),
            ),
            (Network::Udp, true) => {
                let rows = table::<MIB_UDPROW_OWNER_PID>(network, true)?;
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
                let rows = table::<MIB_UDP6ROW_OWNER_PID>(network, false)?;
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
        }?;
        unsafe {
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
            info(pid, String::from_utf16(&path[..length as usize]).ok()?)
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        collections::HashSet,
        fs,
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
    };
    fn address(value: &str) -> Option<SocketAddr> {
        let (ip, port) = value.split_once(':')?;
        let ip = match ip.len() {
            8 => IpAddr::V4(Ipv4Addr::from(
                u32::from_str_radix(ip, 16).ok()?.to_ne_bytes(),
            )),
            32 => {
                let mut bytes = [0; 16];
                for (i, chunk) in bytes.chunks_exact_mut(4).enumerate() {
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
    pub(super) fn resolve(
        network: Network,
        source: SocketAddr,
        destination: SocketAddr,
    ) -> Option<ProcessInfo> {
        let source = SocketAddr::new(source.ip().to_canonical(), source.port());
        let destination = SocketAddr::new(destination.ip().to_canonical(), destination.port());
        let base = match network {
            Network::Tcp => "tcp",
            Network::Udp => "udp",
        };
        let mut inodes = HashSet::new();
        for suffix in ["", "6"] {
            let Ok(text) = fs::read_to_string(format!("/proc/net/{base}{suffix}")) else {
                continue;
            };
            for line in text.lines().skip(1) {
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.len() < 10 {
                    continue;
                }
                let (Some(local), Some(remote)) = (address(fields[1]), address(fields[2])) else {
                    continue;
                };
                let local_matches = local == source
                    || (network == Network::Udp
                        && local.ip().is_unspecified()
                        && local.port() == source.port());
                if local_matches
                    && (remote == destination
                        || (network == Network::Udp && remote.ip().is_unspecified()))
                    && fields[9] != "0"
                {
                    inodes.insert(format!("socket:[{}]", fields[9]));
                }
            }
        }
        if inodes.is_empty() {
            return None;
        }
        let mut owner = None;
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
            let owns = fds.flatten().any(|fd| {
                fs::read_link(fd.path())
                    .ok()
                    .is_some_and(|path| inodes.contains(path.to_string_lossy().as_ref()))
            });
            if owns {
                if owner.is_some_and(|other| other != pid) {
                    return None;
                }
                owner = Some(pid);
            }
        }
        let pid = owner?;
        info(
            pid,
            fs::read_link(format!("/proc/{pid}/exe"))
                .ok()?
                .to_str()?
                .to_owned(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
