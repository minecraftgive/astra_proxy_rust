use crate::net::{App, Hub};
use anyhow::Result;
use astra_protocols::shadowsocks::{from_ss, to_ss};
use std::{sync::Arc, time::Duration};
pub async fn ss_udp(
    socket: shadowsocks::relay::udprelay::proxy_socket::ProxySocket<shadowsocks::net::UdpSocket>,
    app: Arc<App>,
    aead2022: bool,
) -> Result<()> {
    use shadowsocks::relay::udprelay::options::UdpSocketControlData;
    let mut hub = Hub::new(app.clone());
    let mut buf = vec![0; 65536];
    let mut clients = std::collections::HashMap::new();
    let mut next = 0u32;
    loop {
        tokio::select! {
            received=socket.recv_from_with_ctrl(&mut buf)=>{let Ok((n,peer,target,_,ctrl))=received else{continue};let ctrl=ctrl.unwrap_or_default();let key=(peer,ctrl.client_session_id);if aead2022&&clients.keys().any(|(old_peer,id)|*id==ctrl.client_session_id&&*old_peer!=peer){continue}let now=std::time::Instant::now();clients.retain(|_,v:&mut (u32,UdpSocketControlData,std::time::Instant,u64,u64)|v.2.elapsed()<Duration::from_secs(app.config.idle_timeout_secs.max(if aead2022{60}else{1})));if !clients.contains_key(&key)&&clients.len()>=app.config.max_udp_flows{continue}let entry=clients.entry(key).or_insert_with(||{next=next.wrapping_add(1);let mut response=ctrl.clone();response.server_session_id=uuid::Uuid::new_v4().as_u64_pair().0;(next,response,now,0,0)});
                if aead2022 {let id=ctrl.packet_id;if id>entry.3{let shift=id-entry.3;entry.4=if shift>=64{1}else{(entry.4<<shift)|1};entry.3=id;}else{let shift=entry.3-id;if shift>=64||entry.4&(1<<shift)!=0{continue}entry.4|=1<<shift;}}
                entry.2=now;let Ok(p)=app.packet(entry.0,from_ss(target),buf[..n].to_vec())else{continue};let _=hub.send.try_send(p);
            },
            Some(p)=hub.receive.recv()=>{if let Some(((peer,_),entry))=clients.iter_mut().find(|(_,v)|v.0==p.association){entry.1.packet_id=entry.1.packet_id.wrapping_add(1);let _=socket.send_to_with_ctrl(*peer,&to_ss(&p.target),&entry.1,&p.payload).await;}},
        }
    }
}
