//! Authentication and decoded requests, independent of routing and lifecycle.
use astra_ipc::Target;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub name: String,
    pub uuid: Option<uuid::Uuid>,
    pub password: Option<String>,
}

pub enum Request {
    Tcp(Target),
    Udp(Target),
    TrojanUdp,
    SocksUdp,
    MuxCool,
}
pub fn secret(a: &[u8], b: &[u8]) -> bool {
    bool::from(a.ct_eq(b))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Socks5,
    Http,
    Shadowsocks,
    Vmess,
    Vless,
    Trojan,
    Hysteria2,
    Tuic,
}
impl Protocol {
    pub fn quic(self) -> bool {
        matches!(self, Self::Hysteria2 | Self::Tuic)
    }
}
