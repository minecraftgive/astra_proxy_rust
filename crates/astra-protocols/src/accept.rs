//! Authentication and decoded requests, independent of routing and lifecycle.
pub use astra_model::config::server::{Protocol, User};
use astra_model::Target;
use subtle::ConstantTimeEq;

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
