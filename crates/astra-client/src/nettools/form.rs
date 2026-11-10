//! Per-tool form fields and request construction.
use super::*;
use astra_model::nettool::AddressFamily;

impl Field {
    /// English keys are translated by the frontends with `t()`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Egress => "Egress",
            Self::Kind => "Test",
            Self::Target => "Target",
            Self::Record => "Record",
            Self::Server => "DNS server",
            Self::Resolver => "解析器（可选）",
            Self::Count => "Count",
            Self::HttpMethod => "HTTP 方法",
            Self::Hops => "最大跳数",
            Self::PhysicalInterface => "物理网卡",
            Self::Family => "地址族",
            Self::NatAdvanced => "类型识别",
            Self::Comparison => "对照出口",
            Self::CompareRounds => "两轮对照",
            Self::DualStack => "IPv4/IPv6 双栈",
            Self::IncludeNat => "包含 NAT 检测",
        }
    }
    pub fn editable(self) -> bool {
        matches!(
            self,
            Self::Target | Self::Server | Self::Resolver | Self::Count | Self::Hops
        )
    }
}

/// Egress kinds each tool can honour. Trace and physical ICMP cannot look
/// inside a proxy tunnel; NAT always uses its own physical interface.
pub fn egress_allowed(kind: Kind, egress: &Egress) -> bool {
    match kind {
        Kind::Trace => match egress {
            Egress::Interface(_) => true,
            Egress::Outbound(tag) => tag == "DIRECT",
            Egress::System => false,
        },
        Kind::Icmp => match egress {
            Egress::Outbound(tag) => tag == "DIRECT",
            _ => true,
        },
        Kind::Nat | Kind::FakeIp => matches!(egress, Egress::System),
        _ => true,
    }
}

impl View {
    pub fn fields(&self) -> Vec<Field> {
        use Field::*;
        match self.kind {
            super::Kind::FakeIp => vec![Kind, Target],
            super::Kind::Nat => vec![Kind, PhysicalInterface, Family, Target, NatAdvanced],
            super::Kind::Dns => vec![Kind, Egress, Target, Record, Server, Count],
            super::Kind::Tcp => vec![Kind, Egress, Target, Resolver, Count],
            super::Kind::Icmp => vec![Kind, Egress, Target, Count],
            super::Kind::Trace => vec![Kind, Egress, Target, Hops],
            super::Kind::Http => vec![Kind, Egress, Target, HttpMethod, Resolver],
            super::Kind::PublicIp => vec![Kind, Egress, Target, Resolver],
            super::Kind::FullCheck => {
                let mut fields = vec![
                    Kind,
                    Egress,
                    Comparison,
                    Resolver,
                    CompareRounds,
                    DualStack,
                    IncludeNat,
                ];
                if self.include_nat {
                    fields.extend([PhysicalInterface, Family]);
                }
                fields
            }
        }
    }
    pub fn field_hint(&self, field: Field) -> &'static str {
        match field {
            Field::Target => match self.kind {
                Kind::Dns => "example.com",
                Kind::Tcp => "host:443 或 host:80,443,8080（最多 16 个端口）",
                Kind::Http => "https://example.com/",
                Kind::PublicIp => "返回纯 IP 或 JSON ip/address/origin 字段的 HTTPS 服务",
                Kind::Nat => "1–4 个 STUN 服务，用逗号分隔",
                Kind::Icmp | Kind::Trace => "主机名或 IP",
                Kind::FakeIp => "最多 64 个 IP，空格或逗号分隔",
                Kind::FullCheck => "",
            },
            Field::Server => "udp://IP · tcp://IP · tls://名称#bootstrap=IP · https://名称/dns-query#bootstrap=IP",
            Field::Resolver => "可选；服务名需 #bootstrap=IP",
            Field::Count => "1–50",
            Field::Hops => "1–30",
            Field::PhysicalInterface => "仅目标 Agent 已确认连接的物理网卡",
            Field::Comparison => "可选；与主出口对比 DNS、证书和出口 IP",
            _ => "",
        }
    }
    /// Egress choices the current tool can use, as (index, label).
    pub fn allowed_egresses(&self) -> Vec<(usize, String)> {
        self.egresses
            .iter()
            .enumerate()
            .filter(|(_, e)| egress_allowed(self.kind, e))
            .map(|(i, e)| (i, egress_label(e)))
            .collect()
    }
    fn ensure_allowed_egress(&mut self) {
        if !egress_allowed(self.kind, &self.egresses[self.egress]) {
            if let Some((index, _)) = self.allowed_egresses().first() {
                self.egress = *index;
            }
        }
    }
    pub fn set_kind(&mut self, kind: Kind) {
        if kind == self.kind {
            return;
        }
        let host = if let Some((_, authority)) = self.target.split_once("://") {
            host_part(authority.split('/').next().unwrap_or("example.com")).to_owned()
        } else {
            host_part(&self.target).to_owned()
        };
        let host = if host.is_empty() || host.contains(',') {
            "example.com".to_owned()
        } else {
            host
        };
        self.target = match kind {
            Kind::Tcp => format!(
                "{}:443",
                if host.contains(':') {
                    format!("[{host}]")
                } else {
                    host
                }
            ),
            Kind::Dns | Kind::Icmp | Kind::Trace => host,
            Kind::Http => "https://example.com/".into(),
            Kind::PublicIp => full_check::PUBLIC_IP_URL.into(),
            Kind::Nat => "stun.cloudflare.com:3478, stun.l.google.com:19302".into(),
            Kind::FakeIp | Kind::FullCheck => String::new(),
        };
        if !matches!(kind, Kind::Icmp | Kind::Tcp) {
            self.count = 1;
        } else if self.count == 1 {
            self.count = 4;
        }
        if matches!(kind, Kind::Nat | Kind::FullCheck) {
            self.nat_interface = None;
            self.family = AddressFamily::Ipv4;
        }
        self.kind = kind;
        self.ensure_allowed_egress();
        if !self.fields().contains(&self.field) {
            self.field = Field::Kind;
        }
    }
    pub fn set_text(&mut self, field: Field, value: String) -> Result<(), String> {
        match field {
            Field::Target => self.target = value,
            Field::Server => self.server = value,
            Field::Resolver => self.resolver = value,
            Field::Record if RECORDS.contains(&value.as_str()) => self.record = value,
            Field::Count => self.count = number(&value, 1, MAX_COUNT)?,
            Field::Hops => self.trace_hops = number(&value, 1, 30)? as u8,
            _ => return Err(t("Invalid value").into()),
        }
        Ok(())
    }
    pub fn field_value(&self, field: Field) -> String {
        let on_off = |value: bool| if value { "开启" } else { "关闭" }.to_owned();
        match field {
            Field::Egress => egress_label(&self.egresses[self.egress]),
            Field::Kind => self.kind.label().into(),
            Field::Target => self.target.clone(),
            Field::Record => self.record.clone(),
            Field::Server => self.server.clone(),
            Field::Resolver => self.resolver.clone(),
            Field::Count => self.count.to_string(),
            Field::Hops => self.trace_hops.to_string(),
            Field::HttpMethod => self.http_method.clone(),
            Field::Family => match self.family {
                AddressFamily::Ipv4 => "IPv4".into(),
                AddressFamily::Ipv6 => "IPv6".into(),
            },
            Field::PhysicalInterface => self
                .nat_interface
                .as_ref()
                .map(|id| {
                    self.physical_choices()
                        .into_iter()
                        .find(|(key, _)| key == id)
                        .map(|(_, label)| label)
                        .unwrap_or_else(|| format!("{id}（不可用，请重新选择）"))
                })
                .unwrap_or_else(|| "请选择物理网卡".into()),
            Field::NatAdvanced => on_off(self.nat_advanced),
            Field::Comparison => self
                .comparison
                .map_or_else(|| "不对照".into(), |i| egress_label(&self.egresses[i])),
            Field::CompareRounds => on_off(self.compare_rounds),
            Field::DualStack => on_off(self.dual_stack),
            Field::IncludeNat => on_off(self.include_nat),
        }
    }
    pub fn physical_choices(&self) -> Vec<(String, String)> {
        let family = self.family;
        let Some(snapshot) = &self.context.network else {
            return vec![];
        };
        if snapshot["truncated"] == true {
            return vec![];
        }
        snapshot["interfaces"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|interface| {
                interface["connected"] == true
                    && interface["kind"] == "physical"
                    && interface["addresses"].as_array().is_some_and(|addresses| {
                        addresses.iter().any(|address| {
                            address["address"]
                                .as_str()
                                .and_then(|ip| ip.parse().ok())
                                .is_some_and(|ip| family.matches(ip))
                        })
                    })
            })
            .filter_map(|interface| {
                let id = interface["id"].as_str()?.to_owned();
                Some((
                    id.clone(),
                    format!("{} · {}", interface["name"].as_str().unwrap_or(&id), id),
                ))
            })
            .collect()
    }
    pub fn select_physical(&mut self, id: String) {
        if self
            .physical_choices()
            .iter()
            .any(|(candidate, _)| *candidate == id)
        {
            self.nat_interface = Some(id);
        }
    }
    fn resolver_option(&self) -> Option<String> {
        (!self.resolver.trim().is_empty()).then(|| self.resolver.trim().to_owned())
    }
    fn nat_request(&self, servers: &str) -> Result<astra_model::network_nat::Request, String> {
        let interface = self.nat_interface.clone().ok_or("请选择物理网卡")?;
        if !self
            .physical_choices()
            .iter()
            .any(|(id, _)| *id == interface)
        {
            return Err("所选物理网卡已失效或不支持所选地址族".into());
        }
        let agent_id = self
            .context
            .network
            .as_ref()
            .and_then(|snapshot| snapshot["agent_id"].as_str())
            .ok_or("无法确认目标 Agent")?
            .to_owned();
        let request = astra_model::network_nat::Request {
            agent_id,
            interface_id: interface,
            family: self.family,
            servers: servers
                .split([',', ';', '\n'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            advanced: self.nat_advanced,
        };
        request.validate()?;
        Ok(request)
    }
    fn check_interface(&self, egress: &Egress) -> Result<(), String> {
        if let Egress::Interface(selected) = egress {
            let connected = self
                .context
                .network
                .as_ref()
                .and_then(|state| state["interfaces"].as_array())
                .is_some_and(|interfaces| {
                    interfaces.iter().any(|interface| {
                        interface["id"].as_str() == Some(selected) && interface["connected"] == true
                    })
                });
            if !connected {
                return Err("所选网卡已不可用或无法确认，请重新选择出口".into());
            }
        }
        Ok(())
    }
    pub fn full_check_request(&self) -> Result<astra_management::v2::NetworkBatchRequest, String> {
        let primary = self.egresses[self.egress].clone();
        self.check_interface(&primary)?;
        let comparison = self.comparison.map(|i| self.egresses[i].clone());
        if let Some(egress) = &comparison {
            self.check_interface(egress)?;
        }
        let nat = if self.include_nat {
            Some(self.nat_request("stun.cloudflare.com:3478, stun.l.google.com:19302")?)
        } else {
            None
        };
        full_check::build(
            &primary,
            comparison.as_ref(),
            self.resolver_option(),
            &full_check::Options {
                compare_rounds: self.compare_rounds,
                dual_stack: self.dual_stack,
            },
            nat,
        )
    }
    pub fn request(&self) -> Result<astra_management::v2::NetworkDiagnosticsRequest, String> {
        let target = self.target.trim();
        if target.is_empty() {
            return Err(t("Enter a target").into());
        }
        let mut egress = self.egresses[self.egress].clone();
        if !egress_allowed(self.kind, &egress) {
            return Err("此工具不支持所选出口".into());
        }
        if self.kind != Kind::Nat {
            self.check_interface(&egress)?;
        }
        let probe = match self.kind {
            Kind::FakeIp | Kind::FullCheck => return Err("此工具不创建单项探测".into()),
            Kind::Dns => Probe::Dns {
                name: target.into(),
                record: self.record.trim().into(),
                server: self.server.trim().into(),
            },
            Kind::Icmp => Probe::Icmp {
                host: target.into(),
            },
            Kind::Tcp => {
                let (host, ports) = target_ports(target)?;
                if ports.len() == 1 {
                    Probe::Tcp {
                        host,
                        port: ports[0],
                    }
                } else {
                    Probe::TcpPorts { host, ports }
                }
            }
            Kind::Http => Probe::Http {
                url: target.into(),
                method: self.http_method.clone(),
                compare_bundled: false,
            },
            Kind::PublicIp => Probe::HttpsIp {
                url: target.into(),
                compare_bundled: false,
            },
            Kind::Nat => {
                let request = self.nat_request(target)?;
                egress = Egress::Interface(request.interface_id.clone());
                Probe::Nat { request }
            }
            Kind::Trace => Probe::Trace {
                host: target.into(),
                max_hops: self.trace_hops,
                probes_per_hop: 3,
            },
        };
        let mut request: astra_management::v2::NetworkDiagnosticsRequest = Request {
            config: String::new(),
            egress,
            probe,
            count: if matches!(self.kind, Kind::Icmp | Kind::Tcp | Kind::Dns) {
                self.count
            } else {
                1
            },
            interval_ms: if self.kind == Kind::Dns { 200 } else { 1000 },
            timeout_ms: match self.kind {
                Kind::Nat => 30_000,
                Kind::Trace => 1000,
                Kind::Http | Kind::PublicIp => 10_000,
                _ => 3000,
            },
        }
        .into();
        if matches!(self.kind, Kind::Tcp | Kind::Http | Kind::PublicIp) {
            request.resolver = self.resolver_option();
        }
        if self.kind == Kind::Nat {
            request.family = Some(self.family);
        }
        Ok(request)
    }
    pub fn text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            Field::Target => Some(&mut self.target),
            Field::Server => Some(&mut self.server),
            Field::Resolver => Some(&mut self.resolver),
            _ => None,
        }
    }
    pub fn cycle(&mut self, step: isize) {
        let wrap =
            |value: usize, len: usize| (value as isize + step).rem_euclid(len as isize) as usize;
        match self.field {
            Field::Egress => {
                self.egress_query.clear();
                let allowed = self.allowed_egresses();
                if !allowed.is_empty() {
                    let position = allowed
                        .iter()
                        .position(|(i, _)| *i == self.egress)
                        .unwrap_or(0);
                    self.egress = allowed[wrap(position, allowed.len())].0;
                }
            }
            Field::Kind => self.set_kind(
                Kind::ALL[wrap(
                    Kind::ALL
                        .iter()
                        .position(|kind| *kind == self.kind)
                        .unwrap_or(0),
                    Kind::ALL.len(),
                )],
            ),
            Field::Record => {
                self.record = RECORDS[wrap(
                    RECORDS.iter().position(|r| *r == self.record).unwrap_or(0),
                    RECORDS.len(),
                )]
                .into()
            }
            Field::Count => {
                self.count = (self.count as isize + step).clamp(1, MAX_COUNT as isize) as u32
            }
            Field::Hops => self.trace_hops = (self.trace_hops as isize + step).clamp(1, 30) as u8,
            Field::Family => {
                self.family = match self.family {
                    AddressFamily::Ipv4 => AddressFamily::Ipv6,
                    AddressFamily::Ipv6 => AddressFamily::Ipv4,
                };
                self.nat_interface = None;
            }
            Field::PhysicalInterface => {
                let choices = self.physical_choices();
                if !choices.is_empty() {
                    let i = self
                        .nat_interface
                        .as_ref()
                        .and_then(|id| choices.iter().position(|(candidate, _)| candidate == id));
                    self.nat_interface = Some(
                        choices[i.map_or(0, |index| wrap(index, choices.len()))]
                            .0
                            .clone(),
                    );
                }
            }
            Field::HttpMethod => {
                self.http_method = if self.http_method == "GET" {
                    "HEAD"
                } else {
                    "GET"
                }
                .into()
            }
            Field::Comparison => {
                // None, then every egress except the primary one.
                let mut choices: Vec<Option<usize>> = vec![None];
                choices.extend(
                    (0..self.egresses.len())
                        .filter(|i| *i != self.egress)
                        .map(Some),
                );
                let position = choices
                    .iter()
                    .position(|c| *c == self.comparison)
                    .unwrap_or(0);
                self.comparison = choices[wrap(position, choices.len())];
            }
            Field::NatAdvanced => self.nat_advanced = !self.nat_advanced,
            Field::CompareRounds => self.compare_rounds = !self.compare_rounds,
            Field::DualStack => self.dual_stack = !self.dual_stack,
            Field::IncludeNat => {
                self.include_nat = !self.include_nat;
                self.nat_interface = None;
            }
            _ => {}
        }
    }
}
fn number(value: &str, min: u32, max: u32) -> Result<u32, String> {
    value
        .parse::<u32>()
        .ok()
        .filter(|number| (min..=max).contains(number))
        .ok_or_else(|| format!("请输入 {min}–{max}"))
}
pub fn target_ports(target: &str) -> Result<(String, Vec<u16>), String> {
    let (host, ports) = target
        .rsplit_once(':')
        .filter(|(host, _)| !host.is_empty())
        .ok_or_else(|| "请输入 主机:端口，多个端口用逗号分隔".to_owned())?;
    let ports = ports
        .split(',')
        .map(|port| {
            port.trim()
                .parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| "端口须为 1–65535".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if ports.is_empty()
        || ports.len() > 16
        || ports.iter().collect::<std::collections::HashSet<_>>().len() != ports.len()
    {
        return Err("最多指定 16 个不同端口".into());
    }
    Ok((host.trim_matches(['[', ']']).to_owned(), ports))
}
