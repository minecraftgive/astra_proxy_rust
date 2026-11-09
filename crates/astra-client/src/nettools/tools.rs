use super::*;
use astra_model::nettool::AddressFamily;

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Self::Egress => "Egress",
            Self::Kind => "Test",
            Self::Target => "Target",
            Self::Record => "Record",
            Self::Server => "DNS server",
            Self::Resolver => "解析器（可选）",
            Self::Count => "Count",
            Self::Family => "地址族",
            Self::PhysicalInterface => "物理网卡（必选）",
            Self::HttpMethod => "HTTP 方法",
            Self::ServerName => "TLS 名称（可选）",
            Self::TrustComparison => "离线根证书对照",
            Self::NatAdvanced => "NAT 类型识别",
            Self::Hops => "最大跳数",
            Self::Samples => "每跳次数",
        }
    }
    pub fn editable(self) -> bool {
        matches!(
            self,
            Self::Target
                | Self::Server
                | Self::Resolver
                | Self::Count
                | Self::ServerName
                | Self::Hops
                | Self::Samples
        )
    }
}

impl View {
    pub fn fields(&self) -> Vec<Field> {
        use Field::*;
        if self.section != Section::Tools {
            return vec![];
        }
        match self.kind {
            super::Kind::FakeIp | super::Kind::Report => vec![Kind, Target],
            super::Kind::Ipv6 => vec![Egress, Kind, Target, Resolver, Count],
            super::Kind::Nat => vec![
                Kind,
                PhysicalInterface,
                Family,
                Target,
                Resolver,
                NatAdvanced,
            ],
            super::Kind::Dns => vec![
                Egress, Kind, Target, Record, Server, Resolver, Family, Count,
            ],
            super::Kind::Tls => vec![
                Egress,
                Kind,
                Target,
                ServerName,
                Resolver,
                Family,
                TrustComparison,
                Count,
            ],
            super::Kind::Http => vec![
                Egress,
                Kind,
                Target,
                HttpMethod,
                Resolver,
                Family,
                TrustComparison,
                Count,
            ],
            super::Kind::PublicIp => vec![
                Egress,
                Kind,
                Target,
                Resolver,
                Family,
                TrustComparison,
                Count,
            ],
            super::Kind::Trace => vec![Egress, Kind, Target, Resolver, Family, Hops, Samples],
            _ => vec![Egress, Kind, Target, Resolver, Family, Count],
        }
    }
    pub fn field_hint(&self, field: Field) -> &'static str {
        match field {
            Field::Target=>match self.kind {
                Kind::Dns=>"example.com",Kind::Tcp=>"host:443,8443（最多 16 个端口）",Kind::Tls|Kind::Ipv6=>"host:443 或 [IPv6]:443",
                Kind::Http=>"https://example.com/",Kind::PublicIp=>"HTTPS 服务：纯 IP 或 JSON 的 ip/address/origin 字段",
                Kind::Stun=>"stun.cloudflare.com:3478",Kind::Nat=>"1–4 个 STUN 服务，用逗号分隔",Kind::Icmp|Kind::Trace=>"主机名或 IP",
                Kind::FakeIp=>"最多 64 个 IP，空格或逗号分隔",Kind::Report=>"原任务标识；仅恢复观察，不重新执行",
            },
            Field::Server=>"udp://IP · tcp://IP · tls://名称#bootstrap=IP · https://名称/dns-query#bootstrap=IP",
            Field::Resolver=>"可选；受约束路径不回退系统 DNS，服务名需 #bootstrap=IP",
            Field::ServerName=>"与证书匹配的名称；可与连接目标 IP 分开",
            Field::Count=>"1–50",Field::Hops=>"1–30",Field::Samples=>"1–3",
            Field::PhysicalInterface=>"仅目标 Agent 已确认连接的物理网卡，必须手动选择",
            _=>"",
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
        self.target = match kind {
            Kind::Tcp | Kind::Tls | Kind::Ipv6 => format!(
                "{}:443",
                if host.contains(':') {
                    format!("[{host}]")
                } else if host.is_empty() {
                    "example.com".into()
                } else {
                    host.clone()
                }
            ),
            Kind::Dns | Kind::Icmp | Kind::Trace => {
                if host.is_empty() {
                    "example.com".into()
                } else {
                    host
                }
            }
            Kind::Http => "https://example.com/".into(),
            Kind::PublicIp => "https://api.ipify.org?format=json".into(),
            Kind::Stun => "stun.cloudflare.com:3478".into(),
            Kind::Nat => "stun.cloudflare.com:3478, stun.l.google.com:19302".into(),
            Kind::FakeIp => String::new(),
            Kind::Report => self.operation_id.clone().unwrap_or_default(),
        };
        if matches!(
            kind,
            Kind::Dns
                | Kind::Tls
                | Kind::Http
                | Kind::PublicIp
                | Kind::Stun
                | Kind::Nat
                | Kind::Trace
        ) {
            self.count = 1;
        }
        if kind == Kind::Nat {
            self.nat_interface = None;
            self.family = Some(AddressFamily::Ipv4);
        } else if kind == Kind::Ipv6 {
            self.family = Some(AddressFamily::Ipv6);
        }
        self.kind = kind;
        if !self.fields().contains(&self.field) {
            self.field = Field::Kind;
        }
    }
    pub fn set_text(&mut self, field: Field, value: String) -> Result<(), String> {
        match field {
            Field::Target => self.target = value,
            Field::Server => self.server = value,
            Field::Resolver => self.resolver = value,
            Field::ServerName => self.server_name = value,
            Field::Record if RECORDS.contains(&value.as_str()) => self.record = value,
            Field::Count => self.count = number(&value, 1, MAX_COUNT)?,
            Field::Hops => self.trace_hops = number(&value, 1, 30)? as u8,
            Field::Samples => self.trace_samples = number(&value, 1, 3)? as u8,
            _ => return Err(t("Invalid value").into()),
        }
        Ok(())
    }
    pub fn field_value(&self, field: Field) -> String {
        match field {
            Field::Egress => egress_label(&self.egresses[self.egress]),
            Field::Kind => t(self.kind.label()).into(),
            Field::Target => self.target.clone(),
            Field::Record => self.record.clone(),
            Field::Server => self.server.clone(),
            Field::Resolver => self.resolver.clone(),
            Field::Count => self.count.to_string(),
            Field::ServerName => self.server_name.clone(),
            Field::Hops => self.trace_hops.to_string(),
            Field::Samples => self.trace_samples.to_string(),
            Field::Family => match self.family {
                None => "自动".into(),
                Some(AddressFamily::Ipv4) => "IPv4".into(),
                Some(AddressFamily::Ipv6) => "IPv6".into(),
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
            Field::HttpMethod => self.http_method.clone(),
            Field::TrustComparison => if self.compare_trust {
                "开启（离线，不增加请求）"
            } else {
                "关闭"
            }
            .into(),
            Field::NatAdvanced => if self.nat_advanced {
                "开启（需服务端支持）"
            } else {
                "基础 STUN"
            }
            .into(),
        }
    }
    pub fn physical_choices(&self) -> Vec<(String, String)> {
        let family = self.family.unwrap_or(AddressFamily::Ipv4);
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
    pub fn request(&self) -> Result<Request, String> {
        let target = self.target.trim();
        if target.is_empty() {
            return Err(t("Enter a target").into());
        }
        let mut egress = self.egresses[self.egress].clone();
        if self.kind != Kind::Nat {
            if let Egress::Interface(selected) = &egress {
                if !self
                    .context
                    .network
                    .as_ref()
                    .and_then(|state| state["interfaces"].as_array())
                    .is_some_and(|interfaces| {
                        interfaces.iter().any(|interface| {
                            interface["id"].as_str() == Some(selected)
                                && interface["connected"] == true
                        })
                    })
                {
                    return Err("所选目标 Agent 网卡已不可用或无法确认，请重新选择出口".into());
                }
            }
        }
        let probe = match self.kind {
            Kind::FakeIp | Kind::Report => return Err("只读查询不创建主动探测".into()),
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
            Kind::Tls | Kind::Ipv6 => {
                let (host, ports) = target_ports(target)?;
                if ports.len() != 1 {
                    return Err("此工具只接受一个端口".into());
                }
                if self.kind == Kind::Tls {
                    Probe::Tls {
                        host,
                        port: ports[0],
                        server_name: (!self.server_name.trim().is_empty())
                            .then(|| self.server_name.trim().into()),
                        compare_bundled: self.compare_trust,
                    }
                } else {
                    Probe::Ipv6 {
                        host,
                        port: ports[0],
                    }
                }
            }
            Kind::Http => Probe::Http {
                url: target.into(),
                method: self.http_method.clone(),
                compare_bundled: self.compare_trust,
            },
            Kind::PublicIp => Probe::HttpsIp {
                url: target.into(),
                compare_bundled: self.compare_trust,
            },
            Kind::Stun => Probe::Stun {
                server: target.into(),
            },
            Kind::Nat => {
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
                egress = Egress::Interface(interface.clone());
                let request = astra_model::network_nat::Request {
                    agent_id,
                    interface_id: interface,
                    family: self.family.unwrap_or(AddressFamily::Ipv4),
                    servers: target
                        .split([',', ';', '\n'])
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect(),
                    advanced: self.nat_advanced,
                };
                request.validate()?;
                Probe::Nat { request }
            }
            Kind::Trace => Probe::Trace {
                host: target.into(),
                max_hops: self.trace_hops,
                probes_per_hop: self.trace_samples,
            },
        };
        Ok(Request {
            config: String::new(),
            egress,
            probe,
            count: if matches!(self.kind, Kind::Nat | Kind::Trace) {
                1
            } else {
                self.count
            },
            interval_ms: if self.kind == Kind::Dns { 200 } else { 1000 },
            timeout_ms: match self.kind {
                Kind::Nat => 30_000,
                Kind::Trace => 1000,
                Kind::Http | Kind::Tls | Kind::PublicIp => 10_000,
                _ => 3000,
            },
        })
    }
    pub fn text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            Field::Target => Some(&mut self.target),
            Field::Record => Some(&mut self.record),
            Field::Server => Some(&mut self.server),
            Field::Resolver => Some(&mut self.resolver),
            Field::ServerName => Some(&mut self.server_name),
            _ => None,
        }
    }
    pub fn cycle(&mut self, step: isize) {
        let wrap =
            |value: usize, len: usize| (value as isize + step).rem_euclid(len as isize) as usize;
        match self.field {
            Field::Egress => {
                self.egress_query.clear();
                self.egress = wrap(self.egress, self.egresses.len());
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
            Field::Samples => {
                self.trace_samples = (self.trace_samples as isize + step).clamp(1, 3) as u8
            }
            Field::Family => {
                let choices = if self.kind == Kind::Nat {
                    vec![Some(AddressFamily::Ipv4), Some(AddressFamily::Ipv6)]
                } else {
                    vec![None, Some(AddressFamily::Ipv4), Some(AddressFamily::Ipv6)]
                };
                self.family = choices[wrap(
                    choices
                        .iter()
                        .position(|value| *value == self.family)
                        .unwrap_or(0),
                    choices.len(),
                )];
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
            Field::TrustComparison => self.compare_trust = !self.compare_trust,
            Field::NatAdvanced => self.nat_advanced = !self.nat_advanced,
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
fn target_ports(target: &str) -> Result<(String, Vec<u16>), String> {
    let (host, ports) = target
        .rsplit_once(':')
        .filter(|(host, _)| !host.is_empty())
        .ok_or_else(|| "请输入主机:端口，多个端口用逗号分隔".to_owned())?;
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
