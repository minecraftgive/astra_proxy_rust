use serde::Deserialize;
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutboundConfig {
    Native {
        tag: String,
        config: Box<crate::native::NodeConfig>,
    },
    Socks5 {
        tag: String,
        server: String,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    Http {
        tag: String,
        server: String,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    TrojanTls {
        tag: String,
        server: String,
        sni: String,
        password: String,
    },
    VlessTls {
        tag: String,
        server: String,
        sni: String,
        uuid: String,
    },
}
impl OutboundConfig {
    pub fn tag(&self) -> &str {
        match self {
            Self::Native { tag, .. }
            | Self::Http { tag, .. }
            | Self::Socks5 { tag, .. }
            | Self::TrojanTls { tag, .. }
            | Self::VlessTls { tag, .. } => tag,
        }
    }
}
