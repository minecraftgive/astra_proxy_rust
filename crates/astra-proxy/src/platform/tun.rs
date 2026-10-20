use crate::{mode_config::Tun, runtime::Current};
use anyhow::{ensure, Context, Result};
use astra_ipc::{Inbound, Target};
use futures_util::{SinkExt, StreamExt};
#[cfg(not(windows))]
use std::process::Command;
use std::sync::Arc;
use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
};

pub struct Prepared {
    device: Arc<tun_rs::AsyncDevice>,
    config: Tun,
    routes: Routes,
}
pub struct Guard {
    task: Option<JoinHandle<()>>,
    _device: Arc<tun_rs::AsyncDevice>,
    routes: Routes,
    config: Tun,
}
impl Guard {
    pub fn finished(&self) -> bool {
        self.task.as_ref().is_some_and(|task| task.is_finished())
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
#[derive(Default)]
struct Routes {
    owned: Vec<Arc<OwnedRoute>>,
}
#[derive(Clone, PartialEq, Eq)]
struct RouteSpec {
    cidr: String,
    interface: Option<String>,
    gateway: Option<std::net::IpAddr>,
}
pub(crate) struct OwnedRoute {
    spec: RouteSpec,
    #[cfg(not(windows))]
    undo: Vec<String>,
    #[cfg(windows)]
    _route: super::windows_network::Route,
}
#[cfg(not(windows))]
impl Drop for OwnedRoute {
    fn drop(&mut self) {
        if let Err(error) = command(&self.undo) {
            tracing::error!(%error, "failed to remove owned TUN route");
        }
    }
}
#[cfg(not(windows))]
fn command(args: &[String]) -> Result<()> {
    let output = Command::new(&args[0]).args(&args[1..]).output()?;
    ensure!(
        output.status.success(),
        "route command failed: {}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    Ok(())
}
#[cfg(not(windows))]
fn route(
    cidr: &str,
    interface: Option<&str>,
    gateway: Option<std::net::IpAddr>,
    add: bool,
) -> Vec<String> {
    let ipv6 = cidr.contains(':');
    let mut args: Vec<String> = if cfg!(target_os = "macos") {
        vec![
            "/sbin/route".into(),
            "-n".into(),
            if add { "add" } else { "delete" }.into(),
            if ipv6 { "-inet6" } else { "-inet" }.into(),
            "-net".into(),
            cidr.into(),
        ]
    } else {
        vec![
            "ip".into(),
            if ipv6 { "-6" } else { "-4" }.into(),
            "route".into(),
            if add { "add" } else { "del" }.into(),
            cidr.into(),
        ]
    };
    if let Some(gateway) = gateway {
        if !cfg!(target_os = "macos") {
            args.push("via".into());
        }
        args.push(gateway.to_string());
    }
    // macOS -interface marks an on-link route; it must not be combined with a
    // gateway route. The existing connected route determines gateway egress.
    if let Some(interface) = interface.filter(|_| !cfg!(target_os = "macos") || gateway.is_none()) {
        args.push(
            if cfg!(target_os = "macos") {
                "-interface"
            } else {
                "dev"
            }
            .into(),
        );
        args.push(interface.into());
    }
    args
}
impl Routes {
    fn add(
        &mut self,
        cidr: &str,
        interface: Option<&str>,
        gateway: Option<std::net::IpAddr>,
        previous: Option<&Guard>,
    ) -> Result<()> {
        let network: ipnet::IpNet = cidr.parse()?;
        let spec = RouteSpec {
            cidr: network.trunc().to_string(),
            interface: interface.map(str::to_owned),
            gateway,
        };
        if let Some(existing) =
            previous.and_then(|guard| guard.routes.owned.iter().find(|owned| owned.spec == spec))
        {
            self.owned.push(existing.clone());
            return Ok(());
        }
        #[cfg(windows)]
        let owned = OwnedRoute {
            _route: super::windows_network::Route::create(
                &spec.cidr,
                interface.context("Windows route requires an interface")?,
                gateway,
            )?,
            spec,
        };
        #[cfg(not(windows))]
        let owned = {
            command(&route(&spec.cidr, interface, gateway, true))?;
            OwnedRoute {
                undo: route(&spec.cidr, interface, gateway, false),
                spec,
            }
        };
        self.owned.push(Arc::new(owned));
        Ok(())
    }
}
pub async fn prepare(config: Tun, previous: Option<&Guard>) -> Result<Prepared> {
    #[cfg(windows)]
    ensure!(astra_ipc::elevation::is_elevated()?,
        "Enabling Windows TUN requires administrator permission; restart this proxy instance to request UAC elevation (the current configuration is retained)");
    // Reuse the same native device when its identity/address is unchanged. Routes
    // are reference-counted so a candidate can share existing owned routes safely.
    let existing_device = previous
        .filter(|guard| {
            guard.config.name == config.name
                && guard.config.address == config.address
                && guard.config.prefix == config.prefix
                && guard.config.ipv6_address == config.ipv6_address
                && guard.config.ipv6_prefix == config.ipv6_prefix
        })
        .map(|guard| guard._device.clone());
    let device_config = config.clone();
    let device = tokio::task::spawn_blocking(move || {
        if let Some(device) = existing_device {
            return Ok::<_, anyhow::Error>(device);
        }
        #[cfg(not(windows))]
        let mut builder = tun_rs::DeviceBuilder::new()
            .ipv4(device_config.address, device_config.prefix, None)
            .mtu(device_config.mtu);
        #[cfg(not(windows))]
        if let Some(address) = device_config.ipv6_address {
            builder = builder.ipv6(address, device_config.ipv6_prefix);
        }
        #[cfg(windows)]
        {
            let library = std::env::var_os("ASTRA_WINTUN_DLL").map(std::path::PathBuf::from)
                .unwrap_or(std::env::current_exe()?.with_file_name("wintun.dll"));
            let library = std::fs::canonicalize(library).context("Windows TUN requires a trusted, matching-architecture wintun.dll beside astra-proxy.exe, or ASTRA_WINTUN_DLL")?;
            // Acquire the Wintun session before changing any adapter settings.
            // tun-rs opens a matching named Wintun adapter, or creates a new one.
            let mut builder = tun_rs::DeviceBuilder::new()
                .wintun_file(library.to_str().context("Wintun DLL path must be UTF-8")?.to_owned())
                .delete_driver(false);
            if let Some(name) = &device_config.name {
                builder = builder.name(name);
            }
            let existing = device_config.name.as_deref().is_some_and(|name|
                super::windows_network::interface_index(name).is_ok());
            let device = builder.build_async().with_context(|| {
                if existing {
                    format!("open existing Windows TUN {:?}; the adapter must be Wintun and available for a new session", device_config.name.as_deref().unwrap_or_default())
                } else {
                    "create Windows TUN device (requires administrator and Wintun)".to_owned()
                }
            })?;
            device.set_mtu(device_config.mtu).context("configure Windows TUN MTU")?;
            device.set_metric(5).context("configure Windows TUN metric")?;
            device.set_network_address(device_config.address, device_config.prefix, None)
                .context("configure Windows TUN IPv4 address")?;
            if let Some(address) = device_config.ipv6_address {
                device.add_address_v6(address, device_config.ipv6_prefix)
                    .context("configure Windows TUN IPv6 address")?;
            }
            tracing::info!(name = %device.name()?, reused = existing, "Windows TUN session opened");
            Ok(Arc::new(device))
        }
        #[cfg(not(windows))]
        {
            if let Some(name) = &device_config.name {
                builder = builder.name(name);
            }
            Ok(Arc::new(builder.build_async().context(
                "create native TUN device (Unix requires root/CAP_NET_ADMIN)",
            )?))
        }
    })
    .await??;
    let name = device.name()?;
    #[cfg(windows)]
    if let Some(interface) = &config.interface {
        ensure!(
            super::windows_network::interface_index(interface)? != device.if_index()?,
            "tun.interface must be the original physical egress, not the TUN adapter"
        );
    }
    let mut routes = Routes::default();
    for cidr in &config.bypass {
        routes.add(
            cidr,
            config.interface.as_deref(),
            config.gateway_for(cidr),
            previous,
        )?;
    }
    for cidr in &config.routes {
        routes.add(cidr, Some(&name), None, previous)?;
    }
    ensure!(
        device.mtu()? == config.mtu,
        "changing TUN MTU requires a new interface name (other settings and nodes can hot reload)"
    );
    tracing::info!(%name, "native TUN device prepared");
    Ok(Prepared {
        device,
        config,
        routes,
    })
}
impl Prepared {
    pub(crate) fn bypass_leases(&self) -> Vec<Arc<OwnedRoute>> {
        self.routes
            .owned
            .iter()
            .filter(|owned| {
                self.config.bypass.iter().any(|cidr| {
                    cidr.parse::<ipnet::IpNet>().is_ok_and(|net| {
                        owned.spec
                            == RouteSpec {
                                cidr: net.trunc().to_string(),
                                interface: self.config.interface.clone(),
                                gateway: self.config.gateway_for(cidr),
                            }
                    })
                })
            })
            .cloned()
            .collect()
    }
    pub fn start(self, current: Current, mut previous: Option<Guard>) -> Guard {
        if let Some(old) = previous
            .as_mut()
            .filter(|old| Arc::ptr_eq(&old._device, &self.device))
        {
            return Guard {
                task: old.task.take(),
                _device: self.device,
                routes: self.routes,
                config: self.config,
            };
        }
        let device = self.device.clone();
        let config = self.config.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = run(device, self.config.mtu, current).await {
                tracing::error!(%error, "TUN processing failed");
            }
        });
        Guard {
            task: Some(task),
            _device: self.device,
            routes: self.routes,
            config,
        }
    }
}
async fn run(device: Arc<tun_rs::AsyncDevice>, mtu: u16, current: Current) -> Result<()> {
    let (stack, runner, udp, tcp) = netstack_smoltcp::StackBuilder::default()
        .enable_tcp(true)
        .enable_udp(true)
        .enable_icmp(true)
        .stack_buffer_size(256)
        .tcp_buffer_size(256)
        .udp_buffer_size(256)
        .tcp_recv_buffer_size(256 * 1024)
        .tcp_send_buffer_size(256 * 1024)
        .mtu(mtu as usize)
        .build()?;
    let (mut sink, mut packets) = stack.split();
    let mut jobs = JoinSet::new();
    if let Some(runner) = runner {
        jobs.spawn(async move {
            runner.await?;
            Ok(())
        });
    }
    let reader_device = device.clone();
    let reader_current = current.clone();
    let (raw_send, mut raw_receive) = mpsc::channel::<Vec<u8>>(256);
    jobs.spawn(async move {
        let mut packet = vec![0; 65536];
        let mut warned_scope = false;
        let mut self_exclusion = super::tun_ingress::SelfExclusion::default();
        loop {
            let count = reader_device.recv(&mut packet).await?;
            // Consult the active generation: same-device reload keeps this reader.
            let has_egress_interface = reader_current
                .load()
                .config
                .tun
                .as_ref()
                .is_some_and(|tun| tun.interface.is_some());
            if !super::tun_ingress::accepts(&packet[..count], has_egress_interface) {
                if !warned_scope {
                    tracing::warn!("TUN dropped invalid or unscoped IPv6 link-local traffic; configure tun.interface with the physical egress to forward link-local traffic safely");
                    warned_scope = true;
                }
                continue;
            }
            if self_exclusion.excludes(&packet[..count]).await {
                tracing::debug!(target: "astra_route", phase = "self_excluded", "TUN excluded a proxy-core packet before stack allocation");
                continue;
            }
            sink.send(packet[..count].to_vec()).await?;
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    });
    jobs.spawn(async move {
        loop {
            // UDP replies are already complete host-bound IP packets. Writing
            // them back into the stack treats them as new inbound requests.
            let packet = tokio::select! {
                packet = packets.next() => packet.context("TUN output closed")??,
                Some(packet) = raw_receive.recv() => packet,
            };
            ensure!(
                device.send(&packet).await? == packet.len(),
                "partial TUN write"
            );
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    });
    jobs.spawn(tcp_dispatch(
        tcp.context("TCP stack disabled")?,
        current.clone(),
    ));
    jobs.spawn(udp_dispatch(
        udp.context("UDP stack disabled")?,
        raw_send,
        current,
    ));
    // Dropping JoinSet cancels the stack, dispatchers and their bounded workers.
    jobs.join_next().await.context("TUN stack exited")???;
    anyhow::bail!("TUN stack task exited unexpectedly")
}
async fn tcp_dispatch(mut listener: netstack_smoltcp::TcpListener, current: Current) -> Result<()> {
    let mut streams = JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = streams.join_next(), if !streams.is_empty() => {}
            item = listener.next() => {
                let Some((mut stream, source, destination)) = item else { break; };
                let app = current.load_full();
                // Avoid unbounded tasks waiting for permits under SYN floods.
                let Ok(permit) = app.permits.clone().try_acquire_owned() else { continue; };
                streams.spawn(async move {
                    let _permit = permit;
                    let target = Target {host: destination.ip().to_string(), port: destination.port()};
                    let result: Result<()> = async {
                        let action = app.route_connection(&target, Inbound::Tun, astra_ipc::Network::Tcp, Some(source), Some(destination)).await?;
                        let mut remote = app.connect_tracked(&target, &action, Inbound::Tun, Some(source)).await?;
                        tokio::io::copy_bidirectional(&mut stream, &mut remote).await?;
                        Ok(())
                    }.await;
                    if let Err(error) = result { tracing::debug!(%error, %source, %destination, "TUN TCP ended"); }
                });
            }
        }
    }
    Ok(())
}
async fn udp_dispatch(
    socket: netstack_smoltcp::UdpSocket,
    raw: mpsc::Sender<Vec<u8>>,
    current: Current,
) -> Result<()> {
    let (mut read, _write) = socket.split();
    let (input, receive) = mpsc::channel(256);
    let (output, mut replies) = mpsc::channel(256);
    let mut workers = JoinSet::new();
    workers.spawn(crate::packet::dispatch(
        receive,
        output,
        current,
        Inbound::Tun,
    ));
    let receive = async {
        while let Some(message) = read.next().await {
            let _ = input.try_send(message);
        }
        Ok::<_, anyhow::Error>(())
    };
    let send = async {
        while let Some((body, source, destination)) = replies.recv().await {
            raw.send(udp_packet(&body, source, destination)?).await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::try_join!(receive, send)?;
    Ok(())
}
fn udp_packet(
    body: &[u8],
    source: std::net::SocketAddr,
    destination: std::net::SocketAddr,
) -> Result<Vec<u8>> {
    use std::net::IpAddr;
    let builder = match (source.ip(), destination.ip()) {
        (IpAddr::V4(src), IpAddr::V4(dst)) => {
            etherparse::PacketBuilder::ipv4(src.octets(), dst.octets(), 64)
                .udp(source.port(), destination.port())
        }
        (IpAddr::V6(src), IpAddr::V6(dst)) => {
            etherparse::PacketBuilder::ipv6(src.octets(), dst.octets(), 64)
                .udp(source.port(), destination.port())
        }
        _ => anyhow::bail!("UDP source/destination IP family mismatch"),
    };
    let mut packet = Vec::with_capacity(builder.size(body.len()));
    builder.write(&mut packet, body)?;
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(not(windows))]
    fn gateway_routes_do_not_become_on_link_routes_on_macos() {
        let args = route(
            "203.0.113.8/32",
            Some("en0"),
            Some("192.168.1.1".parse().unwrap()),
            true,
        );
        if cfg!(target_os = "macos") {
            assert!(!args.iter().any(|value| value == "-interface"));
        }
        let capture = route("0.0.0.0/1", Some("utun20"), None, true);
        assert!(capture.iter().any(|value| value
            == if cfg!(target_os = "macos") {
                "-interface"
            } else {
                "dev"
            }));
    }
    #[test]
    fn udp_packets_preserve_empty_payload_and_ip_family() {
        for (source, destination) in [
            ("198.18.0.2:1234", "1.1.1.1:53"),
            ("[fd00::2]:1234", "[2606:4700:4700::1111]:53"),
        ] {
            for body in [&b""[..], &b"dns-query"[..]] {
                let packet =
                    udp_packet(body, source.parse().unwrap(), destination.parse().unwrap())
                        .unwrap();
                let parsed = etherparse::SlicedPacket::from_ip(&packet).unwrap();
                let Some(etherparse::TransportSlice::Udp(udp)) = parsed.transport else {
                    panic!("UDP required")
                };
                assert_eq!(udp.source_port(), 1234);
                assert_eq!(udp.destination_port(), 53);
                assert_eq!(udp.payload(), body);
            }
        }
    }
    async fn proxy() -> (Current, JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let mut jobs = JoinSet::new();
            loop {
                tokio::select! {
                    Some(_) = jobs.join_next(), if !jobs.is_empty() => {},
                    client = server.accept() => {
                        let (mut client, _) = client.unwrap();
                        jobs.spawn(async move {
                            let mut greeting = [0; 3]; client.read_exact(&mut greeting).await.unwrap();
                            client.write_all(&[5,0]).await.unwrap();
                            let mut header = [0;4];client.read_exact(&mut header).await.unwrap();
                            assert_eq!(header[3],1);
                            let mut address=[0;6];client.read_exact(&mut address).await.unwrap();
                            if header[1] == 1 {
                                assert_eq!(&address[..4], &[203,0,113,8]);
                                client.write_all(&[5,0,0,1,0,0,0,0,0,0]).await.unwrap();
                                let (mut read, mut write)=client.into_split();
                                let _=tokio::io::copy(&mut read,&mut write).await;
                            } else {
                                assert_eq!(header[1],3);
                                let udp=tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                                let mut reply=vec![5,0,0,1,127,0,0,1];reply.extend_from_slice(&udp.local_addr().unwrap().port().to_be_bytes());
                                client.write_all(&reply).await.unwrap();
                                let mut packet=vec![0;65536];let mut byte=[0];
                                loop {
                                    tokio::select! {
                                        _=client.read(&mut byte)=>break,
                                        data=udp.recv_from(&mut packet)=>{let (n,peer)=data.unwrap();udp.send_to(&packet[..n],peer).await.unwrap();}
                                    }
                                }
                            }
                        });
                    }
                }
            }
        });
        let path =
            std::env::temp_dir().join(format!("astra-stack-{}-{port}.toml", std::process::id()));
        std::fs::write(&path,format!("standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='selected'\nserver='127.0.0.1:{port}'\n")).unwrap();
        let app = crate::App::load(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        (Arc::new(arc_swap::ArcSwap::from_pointee(app)), task)
    }
    #[tokio::test]
    async fn raw_udp_stack_uses_selected_proxy_including_empty_packets() {
        let (current, proxy) = proxy().await;
        let (mut stack, _, udp, _) = netstack_smoltcp::StackBuilder::default()
            .enable_udp(true)
            .enable_tcp(false)
            .enable_icmp(false)
            .build()
            .unwrap();
        let (send, mut replies) = mpsc::channel(4);
        let dispatch = tokio::spawn(udp_dispatch(udp.unwrap(), send, current));
        let source = "198.18.0.2:3456".parse().unwrap();
        let destination = "203.0.113.8:53".parse().unwrap();
        for payload in [vec![], b"dns-query".to_vec(), vec![42; 1200]] {
            stack
                .send(udp_packet(&payload, source, destination).unwrap())
                .await
                .unwrap();
            let reply = tokio::time::timeout(std::time::Duration::from_secs(3), replies.recv())
                .await
                .unwrap()
                .unwrap();
            let parsed = etherparse::SlicedPacket::from_ip(&reply).unwrap();
            let Some(etherparse::TransportSlice::Udp(packet)) = parsed.transport else {
                panic!("expected UDP")
            };
            assert_eq!(packet.source_port(), 53);
            assert_eq!(packet.destination_port(), 3456);
            assert_eq!(packet.payload(), payload);
        }
        dispatch.abort();
        proxy.abort();
    }
    fn tcp_packet(sequence: u32, ack: Option<u32>, body: &[u8]) -> Vec<u8> {
        let builder = etherparse::PacketBuilder::ipv4([198, 18, 0, 2], [203, 0, 113, 8], 64)
            .tcp(3456, 443, sequence, 65535);
        let builder = if let Some(ack) = ack {
            builder.ack(ack)
        } else {
            builder.syn()
        };
        let mut packet = vec![];
        builder.write(&mut packet, body).unwrap();
        packet
    }
    #[tokio::test]
    async fn raw_tcp_stack_handshake_and_payload_use_selected_proxy() {
        let (current, proxy) = proxy().await;
        let (stack, runner, _, tcp) = netstack_smoltcp::StackBuilder::default()
            .enable_tcp(true)
            .enable_udp(false)
            .build()
            .unwrap();
        let mut jobs = JoinSet::new();
        jobs.spawn(async move {
            runner.unwrap().await.unwrap();
        });
        jobs.spawn(async move {
            tcp_dispatch(tcp.unwrap(), current).await.unwrap();
        });
        let (mut sink, mut packets) = stack.split();
        sink.send(tcp_packet(1000, None, &[])).await.unwrap();
        let syn_ack = tokio::time::timeout(std::time::Duration::from_secs(3), packets.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let parsed = etherparse::SlicedPacket::from_ip(&syn_ack).unwrap();
        let Some(etherparse::TransportSlice::Tcp(tcp)) = parsed.transport else {
            panic!("expected SYN ACK")
        };
        assert!(tcp.syn() && tcp.ack());
        assert_eq!(tcp.acknowledgment_number(), 1001);
        let acknowledgment = tcp.sequence_number().wrapping_add(1);
        sink.send(tcp_packet(1001, Some(acknowledgment), &[]))
            .await
            .unwrap();
        sink.send(tcp_packet(1001, Some(acknowledgment), b"tun-echo"))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let packet = packets.next().await.unwrap().unwrap();
                let parsed = etherparse::SlicedPacket::from_ip(&packet).unwrap();
                if let Some(etherparse::TransportSlice::Tcp(tcp)) = parsed.transport {
                    if !tcp.payload().is_empty() {
                        assert_eq!(tcp.payload(), b"tun-echo");
                        break;
                    }
                }
            }
        })
        .await
        .unwrap();
        proxy.abort();
    }
}
