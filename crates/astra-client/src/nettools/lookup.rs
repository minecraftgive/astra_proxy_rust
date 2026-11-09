use super::*;
use astra_management::v2 as v;
use std::{net::IpAddr, time::Duration};

pub(super) fn addresses(input: &str) -> Result<Vec<String>, String> {
    let values = input
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<IpAddr>().map(|ip| ip.to_string()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "请输入 IP 地址；不接受域名或地址段".to_owned())?;
    if values.is_empty() || values.len() > 64 {
        return Err("请输入 1–64 个 IP 地址，使用空格或逗号分隔".into());
    }
    Ok(values)
}

impl View {
    pub(super) fn start_fake_ip(&mut self) {
        let addresses = match addresses(&self.target) {
            Ok(addresses) => addresses,
            Err(message) => {
                self.error = Some(message);
                return;
            }
        };
        self.details.clear();
        self.attempts.clear();
        self.summary = None;
        self.error = None;
        self.operation_id = None;
        self.report_state = None;
        self.probe_path = None;
        self.path_stale = false;
        self.captured_runtime = self.context.runtime.clone();
        self.captured_core = self.context.core.clone();
        self.note = Some("读取运行核心已有映射；未命中不按地址段推断 Fake-IP".into());
        let client = self.client.clone();
        let instance = self.name.clone();
        let waker = self.waker.clone();
        let (sender, receiver) = mpsc::channel(4);
        let (cancel, mut control) = tokio::sync::watch::channel(false);
        self.task = Some(tokio::spawn(async move {
            let result = tokio::select! {
                result = tokio::time::timeout(Duration::from_secs(8), lookup(&client, instance, addresses)) => result.unwrap_or_else(|_| Err(anyhow::anyhow!("通信失败：映射读取超时"))),
                _ = async { let _ = control.wait_for(|stop| *stop).await; } => {
                    let _ = sender.send(jobs::Update::Notice("已停止读取核心映射".into())).await;
                    let _ = sender.send(jobs::Update::Finished).await;
                    waker.wake();
                    return;
                }
            };
            let update = match result {
                Ok(reply) => jobs::Update::FakeIp(reply),
                Err(error) => jobs::Update::Error(format!("{error:#}")),
            };
            let _ = sender.send(update).await;
            let _ = sender.send(jobs::Update::Finished).await;
            waker.wake();
        }));
        self.cancel = Some(cancel);
        self.updates = Some(receiver);
    }
}

async fn lookup(
    client: &crate::network_diagnostics::Client,
    instance: String,
    addresses: Vec<String>,
) -> anyhow::Result<v::FakeIpLookupReply> {
    let v::ResponseBody::Hello(hello) = client
        .request(v::RequestBody::Query(v::Query::Hello))
        .await?
    else {
        anyhow::bail!("协议错误：Agent 身份响应类型不匹配");
    };
    anyhow::ensure!(
        hello.version == v::VERSION && hello.role == "agent",
        "协议错误：目标不是受支持的 Agent"
    );
    anyhow::ensure!(
        hello.capabilities.iter().any(|c| c == "core_network_v1"),
        "此 Agent 不支持读取核心映射"
    );
    let v::ResponseBody::NetworkCore(core) = client
        .request(v::RequestBody::Query(v::Query::NetworkCore {
            instance: instance.clone(),
        }))
        .await?
    else {
        anyhow::bail!("协议错误：核心身份响应类型不匹配");
    };
    anyhow::ensure!(
        core.instance == instance && core.schema_version == 1,
        "协议错误：核心身份来源不匹配"
    );
    anyhow::ensure!(
        core.status == v::CoreQueryStatus::Ok,
        "核心查询 {:?}：{}",
        core.status,
        core.message.as_deref().unwrap_or("无法读取核心身份")
    );
    let identity = core
        .identity
        .ok_or_else(|| anyhow::anyhow!("协议错误：核心身份缺失"))?;
    let capabilities = identity
        .capabilities
        .ok_or_else(|| anyhow::anyhow!("能力无法确认：核心未声明能力；未发送映射查询"))?;
    anyhow::ensure!(
        capabilities.iter().any(|c| c == "fake_ip_lookup_v1"),
        "核心不支持 Fake-IP 查询；未发送新命令"
    );
    let expected = addresses.clone();
    let v::ResponseBody::FakeIpLookup(reply) = client
        .request(v::RequestBody::Query(v::Query::FakeIpLookup {
            instance: instance.clone(),
            addresses: addresses
                .into_iter()
                .map(|s| s.parse())
                .collect::<Result<_, _>>()?,
        }))
        .await?
    else {
        anyhow::bail!("协议错误：Fake-IP 响应类型不匹配");
    };
    anyhow::ensure!(
        reply.instance == instance && reply.schema_version == 1,
        "协议错误：Fake-IP 响应来源不匹配"
    );
    if reply.status == v::CoreQueryStatus::Ok {
        anyhow::ensure!(
            reply.mappings.len() == expected.len()
                && reply
                    .mappings
                    .iter()
                    .zip(&expected)
                    .all(|(mapping, address)| mapping.address == *address),
            "协议错误：Fake-IP 响应地址与请求不一致"
        );
        anyhow::ensure!(
            reply.boot_id.as_deref() == Some(identity.boot_id.as_str()),
            "核心在查询期间已重启，请重新查询映射"
        );
    }
    Ok(reply)
}
