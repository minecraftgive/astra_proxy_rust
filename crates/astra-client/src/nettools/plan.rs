//! Editable batch requests shared by native frontends. No diagnostic rules run here.
use super::*;
use astra_management::v2::{NetworkBatchProject, NetworkBatchRequest, NetworkDiagnosticsRequest};

#[derive(Default)]
pub struct Editor {
    pub projects: Vec<NetworkBatchProject>,
    pub comparison: bool,
    pub selected: usize,
    next_id: u64,
}

fn same_path(left: &NetworkDiagnosticsRequest, right: &NetworkDiagnosticsRequest) -> bool {
    left.egress == right.egress && left.resolver == right.resolver
}
fn label(request: &NetworkDiagnosticsRequest) -> String {
    // Probe URLs and resolver URLs can contain tokens. The editable form keeps
    // them, but list summaries, clipboard results and exports never echo them.
    let probe = match &request.probe {
        Probe::Dns { name, record, .. } => format!("DNS {record} {name}"),
        Probe::Http { method, .. } => format!("HTTP {method}"),
        Probe::HttpsIp { .. } => "HTTPS 公网 IP".into(),
        Probe::Nat { .. } => "NAT（运行前重新手选物理网卡）".into(),
        probe => probe.to_string(),
    };
    clean(&format!(
        "{probe} · {} · {} · 解析器{}",
        egress_label(&request.egress),
        match request.family {
            Some(astra_model::nettool::AddressFamily::Ipv4) => "IPv4",
            Some(astra_model::nettool::AddressFamily::Ipv6) => "IPv6",
            None => "自动地址族",
        },
        if request.resolver.is_some() {
            "已指定"
        } else {
            "按路径策略"
        }
    ))
}

impl View {
    fn add_request(&mut self, request: NetworkDiagnosticsRequest) -> Result<(), String> {
        if self.plan.projects.len() >= 32 {
            return Err("诊断计划最多 32 个项目".into());
        }
        if matches!(request.probe, Probe::Nat { .. })
            && self
                .plan
                .projects
                .iter()
                .any(|p| matches!(p.request.probe, Probe::Nat { .. }))
        {
            return Err("诊断计划最多包含一个手动选择的 NAT 项目".into());
        }
        let path = self
            .plan
            .projects
            .iter()
            .find(|p| same_path(&p.request, &request))
            .map(|p| p.path_id.clone());
        let paths = self
            .plan
            .projects
            .iter()
            .map(|p| &p.path_id)
            .collect::<std::collections::HashSet<_>>();
        if path.is_none() && paths.len() >= 3 {
            return Err("最多选择 3 条出口/解析器路径；请先删除不需要的路径项目".into());
        }
        self.plan.next_id += 1;
        self.plan.projects.push(NetworkBatchProject {
            id: format!("project-{}", self.plan.next_id),
            path_id: path.unwrap_or_else(|| format!("path-{}", self.plan.next_id)),
            request,
        });
        self.plan.selected = self.plan.projects.len() - 1;
        Ok(())
    }
    pub fn add_project(&mut self) -> Result<(), String> {
        let mut request: NetworkDiagnosticsRequest = self.request()?.into();
        request.resolver =
            (!self.resolver.trim().is_empty()).then(|| self.resolver.trim().to_owned());
        request.family = self.family;
        self.add_request(request)?;
        self.note = Some("已加入诊断计划；在诊断分区编辑路径、启用对照并运行".into());
        Ok(())
    }
    /// User-triggered draft presets; adding them performs no network activity.
    pub fn add_presets(&mut self) -> Result<(), String> {
        if self.plan.projects.len() > 27 {
            return Err("加入五项预设将超过 32 个项目上限".into());
        }
        let probes = vec![
            Probe::Dns {
                name: "example.com".into(),
                record: "A".into(),
                server: "1.1.1.1".into(),
            },
            Probe::Tls {
                host: "example.com".into(),
                port: 443,
                server_name: None,
                compare_bundled: false,
            },
            Probe::Http {
                url: "https://example.com/".into(),
                method: "HEAD".into(),
                compare_bundled: false,
            },
            Probe::HttpsIp {
                url: "https://api.ipify.org?format=json".into(),
                compare_bundled: false,
            },
            Probe::Stun {
                server: "stun.cloudflare.com:3478".into(),
            },
        ];
        let mut requests = vec![];
        for probe in probes {
            let mut request: NetworkDiagnosticsRequest = Request {
                config: String::new(),
                egress: self.egresses[self.egress].clone(),
                probe,
                count: 1,
                interval_ms: 200,
                timeout_ms: 10_000,
            }
            .into();
            request.resolver =
                (!self.resolver.trim().is_empty()).then(|| self.resolver.trim().to_owned());
            request.family = self.family;
            requests.push(request);
        }
        let paths = self
            .plan
            .projects
            .iter()
            .map(|p| &p.path_id)
            .collect::<std::collections::HashSet<_>>();
        if paths.len() >= 3
            && !self
                .plan
                .projects
                .iter()
                .any(|p| same_path(&p.request, &requests[0]))
        {
            return Err("最多选择 3 条出口/解析器路径".into());
        }
        for request in requests {
            self.add_request(request)?;
        }
        self.note=Some("已加入 DNS、TLS、HTTP、HTTPS 出口与 STUN 五项预设；可逐项编辑或删除，点击运行才发送请求。".into());
        Ok(())
    }
    pub fn remove_project(&mut self, index: usize) {
        if index < self.plan.projects.len() {
            self.plan.projects.remove(index);
        }
        self.plan.selected = self
            .plan
            .selected
            .min(self.plan.projects.len().saturating_sub(1));
    }
    /// Explicitly copy ordinary projects onto the currently selected exit and resolver.
    pub fn copy_plan_path(&mut self) -> Result<(), String> {
        let egress = self.egresses[self.egress].clone();
        let resolver = (!self.resolver.trim().is_empty()).then(|| self.resolver.trim().to_owned());
        let mut candidates: Vec<NetworkDiagnosticsRequest> = vec![];
        for project in &self.plan.projects {
            if matches!(project.request.probe, Probe::Nat { .. }) {
                continue;
            }
            let mut request = project.request.clone();
            request.egress = egress.clone();
            request.resolver = resolver.clone();
            let equivalent = |other: &NetworkDiagnosticsRequest| {
                same_path(other, &request)
                    && other.probe == request.probe
                    && other.family == request.family
            };
            if !self.plan.projects.iter().any(|p| equivalent(&p.request))
                && !candidates.iter().any(equivalent)
            {
                candidates.push(request);
            }
        }
        if candidates.is_empty() {
            return Err("此出口已包含相同项目，或计划仅包含不能复制的 NAT 项目".into());
        }
        if self.plan.projects.len() + candidates.len() > 32 {
            return Err("复制后超过 32 个项目上限".into());
        }
        let paths = self
            .plan
            .projects
            .iter()
            .map(|p| &p.path_id)
            .collect::<std::collections::HashSet<_>>();
        if paths.len() >= 3
            && !self
                .plan
                .projects
                .iter()
                .any(|p| same_path(&p.request, &candidates[0]))
        {
            return Err("最多选择 3 条出口/解析器路径".into());
        }
        for request in candidates {
            self.add_request(request)?;
        }
        Ok(())
    }
    pub fn project_rows(&self) -> Vec<String> {
        self.plan
            .projects
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let status = self
                    .details
                    .iter()
                    .rev()
                    .find(|event| {
                        event["event"] == "project_status"
                            && event["project_id"] == p.id
                            && event["path_id"] == p.path_id
                    })
                    .map(|event| {
                        format!(
                            " · 第 {} 轮 {}：{}",
                            event["round"]
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| event["round"].to_string()),
                            event["state"].as_str().unwrap_or(""),
                            event["message"].as_str().unwrap_or("")
                        )
                    })
                    .unwrap_or_default();
                clean(&format!(
                    "{} {} · {} · {}{status}",
                    if i == self.plan.selected { ">" } else { " " },
                    p.id,
                    p.path_id,
                    label(&p.request)
                ))
            })
            .collect()
    }
    pub fn plan_lines(&self) -> Vec<String> {
        let paths = self
            .plan
            .projects
            .iter()
            .map(|p| &p.path_id)
            .collect::<std::collections::HashSet<_>>()
            .len();
        let mut lines = vec![format!("可编辑诊断计划 · {} 个项目 · {paths}/3 条路径 · DNS/TLS 对照{}", self.plan.projects.len(),if self.plan.comparison {"开启：每路径执行两轮"}else{"关闭：不追加对照流量"}),
            "Ctrl+P 加入当前工具 · Ctrl+R 加入五项预设 · ↑↓ 选项目 · E 编辑 · Delete 删除 · C 对照 · ←→ 选择新路径 · P 复制普通项目 · Enter 运行/取消".into(),
            format!("复制到当前出口：{}（使用工具箱当前解析器）", clean(&egress_label(&self.egresses[self.egress])))];
        lines.extend(self.project_rows());
        if self.plan.projects.is_empty() {
            lines.push("计划为空；先在工具箱编辑工具和出口，再加入项目。".into());
        }
        if self
            .plan
            .projects
            .iter()
            .any(|p| matches!(p.request.probe, Probe::Nat { .. }))
        {
            lines.push("NAT 会直接访问测试服务并暴露所选物理网络的公网出口；每次运行前需重新手动选择同一目标 Agent 物理网卡。".into());
        }
        lines
    }
    pub fn batch_request(&self) -> Result<NetworkBatchRequest, String> {
        if self.plan.projects.is_empty() {
            return Err("请先加入至少一个诊断项目".into());
        }
        let mut projects = self.plan.projects.clone();
        for project in &mut projects {
            if let Probe::Nat { request } = &project.request.probe {
                let agent = self
                    .context
                    .network
                    .as_ref()
                    .and_then(|n| n["agent_id"].as_str());
                if self.nat_interface.as_deref() != Some(request.interface_id.as_str())
                    || agent != Some(request.agent_id.as_str())
                    || self.family != Some(request.family)
                    || !self
                        .physical_choices()
                        .iter()
                        .any(|(id, _)| *id == request.interface_id)
                {
                    return Err(
                        "请回到 NAT 工具，重新手动选择计划中的目标 Agent 物理网卡及地址族后运行"
                            .into(),
                    );
                }
            }
            if self.plan.comparison
                && matches!(project.request.probe, Probe::Dns { .. } | Probe::Tls { .. })
            {
                project.request.count = 1;
            }
        }
        Ok(NetworkBatchRequest {
            projects,
            comparison: self.plan.comparison,
        })
    }
    pub fn run_plan(&mut self) {
        match self.batch_request() {
            Ok(request) => self.start_batch(request),
            Err(error) => self.error = Some(error),
        }
    }
    pub fn edit_project(&mut self, index: usize) -> Result<(), String> {
        let request = self
            .plan
            .projects
            .get(index)
            .ok_or("请选择项目")?
            .request
            .clone();
        self.section = Section::Tools;
        let authority = |host: &str, port: u16| {
            format!(
                "{}:{port}",
                if host.contains(':') {
                    format!("[{host}]")
                } else {
                    host.into()
                }
            )
        };
        let (kind, target) = match &request.probe {
            Probe::Dns {
                name,
                record,
                server,
            } => {
                self.record = record.clone();
                self.server = server.clone();
                (Kind::Dns, name.clone())
            }
            Probe::Tcp { host, port } => (Kind::Tcp, authority(host, *port)),
            Probe::TcpPorts { host, ports } => (
                Kind::Tcp,
                format!(
                    "{}:{}",
                    if host.contains(':') {
                        format!("[{host}]")
                    } else {
                        host.clone()
                    },
                    ports
                        .iter()
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            ),
            Probe::Icmp { host } => (Kind::Icmp, host.clone()),
            Probe::Tls {
                host,
                port,
                server_name,
                compare_bundled,
            } => {
                self.server_name = server_name.clone().unwrap_or_default();
                self.compare_trust = *compare_bundled;
                (Kind::Tls, authority(host, *port))
            }
            Probe::Http {
                url,
                method,
                compare_bundled,
            } => {
                self.http_method = method.clone();
                self.compare_trust = *compare_bundled;
                (Kind::Http, url.clone())
            }
            Probe::HttpsIp {
                url,
                compare_bundled,
            } => {
                self.compare_trust = *compare_bundled;
                (Kind::PublicIp, url.clone())
            }
            Probe::Stun { server } => (Kind::Stun, server.clone()),
            Probe::Nat { request } => {
                self.nat_interface = None;
                self.nat_advanced = request.advanced;
                (Kind::Nat, request.servers.join(", "))
            }
            Probe::Trace {
                host,
                max_hops,
                probes_per_hop,
            } => {
                self.trace_hops = *max_hops;
                self.trace_samples = *probes_per_hop;
                (Kind::Trace, host.clone())
            }
            Probe::Ipv6 { host, port } => (Kind::Ipv6, authority(host, *port)),
        };
        self.kind = kind;
        self.target = target;
        self.count = request.count;
        self.family = request.family;
        self.resolver = request.resolver.clone().unwrap_or_default();
        self.field = Field::Target;
        if let Some(index) = self.egresses.iter().position(|e| *e == request.egress) {
            self.egress = index;
        } else {
            self.egresses.push(request.egress.clone());
            self.egress = self.egresses.len() - 1;
        }
        self.remove_project(index);
        self.scroll = 0;
        self.note = Some("项目已移回工具箱；修改后再次加入诊断计划".into());
        Ok(())
    }
    pub(super) fn plan_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc if self.running() => self.stop(),
            KeyCode::Esc => return false,
            KeyCode::Enter if self.running() => self.stop(),
            KeyCode::Enter => self.run_plan(),
            KeyCode::Up => self.plan.selected = self.plan.selected.saturating_sub(1),
            KeyCode::Down => {
                self.plan.selected =
                    (self.plan.selected + 1).min(self.plan.projects.len().saturating_sub(1))
            }
            KeyCode::Delete => self.remove_project(self.plan.selected),
            KeyCode::Left | KeyCode::Right => {
                self.field = Field::Egress;
                self.cycle(if key.code == KeyCode::Left { -1 } else { 1 });
            }
            KeyCode::Char('c') => self.plan.comparison = !self.plan.comparison,
            KeyCode::Char('p') => self.error = self.copy_plan_path().err(),
            KeyCode::Char('e') => self.error = self.edit_project(self.plan.selected).err(),
            _ => {}
        }
        true
    }
}
