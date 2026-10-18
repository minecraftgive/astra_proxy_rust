use crate::{runtime::Current, App};
use anyhow::{bail, Result};
use astra_ipc::{Inbound, Target};
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

pub fn tcp_listener(address: &str, transparent: bool) -> Result<TcpListener> {
    #[cfg(target_os = "linux")]
    {
        linux::tcp_listener(address.parse()?, transparent)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (address, transparent);
        bail!("TPROXY/REDIRECT require Linux")
    }
}
pub async fn tcp(mut stream: TcpStream, app: Arc<App>, inbound: Inbound) -> Result<()> {
    #[cfg(target_os = "linux")]
    let destination = if inbound == Inbound::Tproxy {
        stream.local_addr()?
    } else {
        linux::original_destination(&stream)?
    };
    #[cfg(not(target_os = "linux"))]
    let destination = {
        let _ = inbound;
        stream.local_addr()?
    };
    let target = Target {
        host: destination.ip().to_string(),
        port: destination.port(),
    };
    let action = app.route(&target, inbound).await?;
    let mut remote = app.connect(&target, &action).await?;
    tokio::io::copy_bidirectional(&mut stream, &mut remote).await?;
    Ok(())
}
pub struct PreparedUdp {
    #[cfg(target_os = "linux")]
    socket: tokio::net::UdpSocket,
}
pub struct UdpGuard {
    task: JoinHandle<()>,
}
impl UdpGuard {
    pub fn finished(&self) -> bool {
        self.task.is_finished()
    }
}
impl Drop for UdpGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub fn prepare_udp(address: SocketAddr) -> Result<PreparedUdp> {
    #[cfg(target_os = "linux")]
    {
        Ok(PreparedUdp {
            socket: linux::udp_socket(address, true)?,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = address;
        bail!("TPROXY UDP requires Linux")
    }
}
impl PreparedUdp {
    pub fn start(self, current: Current) -> UdpGuard {
        UdpGuard {
            task: tokio::spawn(async move {
                #[cfg(target_os = "linux")]
                if let Err(error) = linux::udp(self.socket, current).await {
                    tracing::error!(%error, "TPROXY UDP failed");
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = (self, current);
                }
            }),
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use anyhow::{ensure, Context};
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    use std::{io, mem, os::fd::AsRawFd};
    use tokio::{io::Interest, net::UdpSocket, sync::mpsc};

    fn set(fd: i32, level: i32, name: i32, value: i32) -> io::Result<()> {
        // SAFETY: valid fd, level-specific integer option, correctly sized pointer.
        let result = unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                &value as *const _ as *const _,
                mem::size_of_val(&value) as _,
            )
        };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    fn socket(address: SocketAddr, udp: bool, transparent: bool) -> Result<Socket> {
        let socket = Socket::new(
            Domain::for_address(address),
            if udp { Type::DGRAM } else { Type::STREAM },
            Some(if udp { Protocol::UDP } else { Protocol::TCP }),
        )?;
        socket.set_nonblocking(true)?;
        if address.is_ipv6() {
            socket.set_only_v6(true)?;
        }
        socket.set_reuse_address(true)?;
        if transparent {
            let (level, option) = if address.is_ipv4() {
                (libc::SOL_IP, libc::IP_TRANSPARENT)
            } else {
                (libc::SOL_IPV6, libc::IPV6_TRANSPARENT)
            };
            set(socket.as_raw_fd(), level, option, 1)
                .context("IP_TRANSPARENT needs CAP_NET_ADMIN or CAP_NET_RAW")?;
        }
        socket.bind(&address.into())?;
        Ok(socket)
    }
    pub fn tcp_listener(address: SocketAddr, transparent: bool) -> Result<TcpListener> {
        let socket = socket(address, false, transparent)?;
        socket.listen(1024)?;
        Ok(TcpListener::from_std(socket.into())?)
    }
    pub fn udp_socket(address: SocketAddr, receive_original: bool) -> Result<UdpSocket> {
        let socket = socket(address, true, true)?;
        if receive_original {
            let (level, option) = if address.is_ipv4() {
                (libc::SOL_IP, libc::IP_RECVORIGDSTADDR)
            } else {
                (libc::SOL_IPV6, libc::IPV6_RECVORIGDSTADDR)
            };
            set(socket.as_raw_fd(), level, option, 1)?;
        }
        Ok(UdpSocket::from_std(socket.into())?)
    }
    unsafe fn address(
        storage: *const libc::sockaddr,
        length: libc::socklen_t,
    ) -> Result<SocketAddr> {
        // SAFETY: caller passes initialized sockaddr with its actual length.
        let value = unsafe {
            SockAddr::try_init(|target, size| {
                ensure_size(length as usize, mem::size_of::<libc::sockaddr_storage>())?;
                std::ptr::copy_nonoverlapping(
                    storage as *const u8,
                    target as *mut u8,
                    length as usize,
                );
                *size = length;
                Ok(())
            })
        }?;
        value
            .1
            .as_socket()
            .context("unsupported transparent address family")
    }
    fn ensure_size(len: usize, max: usize) -> io::Result<()> {
        if len > max {
            Err(io::Error::other("invalid sockaddr length"))
        } else {
            Ok(())
        }
    }
    pub fn original_destination(stream: &TcpStream) -> Result<SocketAddr> {
        let ipv4 = stream.local_addr()?.is_ipv4();
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut length = mem::size_of_val(&storage) as libc::socklen_t;
        // SO_ORIGINAL_DST / IP6T_SO_ORIGINAL_DST are both 80.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                if ipv4 { libc::SOL_IP } else { libc::SOL_IPV6 },
                80,
                &mut storage as *mut _ as *mut _,
                &mut length,
            )
        };
        ensure!(
            result == 0,
            "SO_ORIGINAL_DST: {}",
            io::Error::last_os_error()
        );
        unsafe { address(&storage as *const _ as *const _, length) }
    }
    fn receive(socket: &UdpSocket, data: &mut [u8]) -> io::Result<(usize, SocketAddr, SocketAddr)> {
        let mut source: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut control = [0usize; 32];
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr() as *mut _,
            iov_len: data.len(),
        };
        let mut message: libc::msghdr = unsafe { mem::zeroed() };
        message.msg_name = &mut source as *mut _ as *mut _;
        message.msg_namelen = mem::size_of_val(&source) as _;
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr() as *mut _;
        message.msg_controllen = mem::size_of_val(&control);
        let count = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, libc::MSG_DONTWAIT) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            return Err(io::Error::other("truncated transparent UDP datagram"));
        }
        let from = unsafe { address(&source as *const _ as *const _, message.msg_namelen) }
            .map_err(io::Error::other)?;
        let mut destination = None;
        unsafe {
            let mut header = libc::CMSG_FIRSTHDR(&message);
            while !header.is_null() {
                let (level, kind) = ((*header).cmsg_level, (*header).cmsg_type);
                let size = if level == libc::SOL_IP && kind == libc::IP_ORIGDSTADDR {
                    mem::size_of::<libc::sockaddr_in>()
                } else if level == libc::SOL_IPV6 && kind == libc::IPV6_ORIGDSTADDR {
                    mem::size_of::<libc::sockaddr_in6>()
                } else {
                    0
                };
                if size > 0 && (*header).cmsg_len >= libc::CMSG_LEN(size as _) as usize {
                    destination = Some(
                        address(libc::CMSG_DATA(header) as *const _, size as _)
                            .map_err(io::Error::other)?,
                    );
                }
                header = libc::CMSG_NXTHDR(&message, header);
            }
        }
        Ok((
            count as usize,
            from,
            destination.ok_or_else(|| io::Error::other("missing UDP original destination"))?,
        ))
    }
    pub async fn udp(socket: UdpSocket, current: Current) -> Result<()> {
        let (input, rx) = mpsc::channel(256);
        let (output, mut replies) = mpsc::channel::<crate::packet::Datagram>(256);
        let mut jobs = tokio::task::JoinSet::new();
        jobs.spawn(crate::packet::dispatch(
            rx,
            output,
            current,
            Inbound::Tproxy,
        ));
        let receive = async {
            let mut packet = vec![0; 65536];
            loop {
                let (size, source, destination) = socket
                    .async_io(Interest::READABLE, || receive(&socket, &mut packet))
                    .await?;
                let _ = input.try_send((packet[..size].to_vec(), source, destination));
            }
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };
        let send = async {
            while let Some((body, source, destination)) = replies.recv().await {
                // A non-local source socket preserves the original destination in
                // replies; unlike NAT REDIRECT, TPROXY has no reverse NAT state.
                let result = async {
                    let reply = udp_socket(source, false)?;
                    reply.send_to(&body, destination).await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if let Err(error) = result {
                    tracing::debug!(%error, %source, %destination, "transparent UDP reply dropped");
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::try_join!(receive, send)?;
        Ok(())
    }
}
