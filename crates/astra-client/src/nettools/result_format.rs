//! Display backend evidence and conclusions; never derive diagnostic findings here.
use super::clean;
use serde_json::Value;
fn text(value: &Value) -> String {
    match value {
        Value::String(value) => clean(value),
        Value::Null => "未知".into(),
        value => clean(&value.to_string()),
    }
}
fn list(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}
fn flag(value: &Value) -> &'static str {
    if value == true {
        "是"
    } else if value == false {
        "否"
    } else {
        "未知"
    }
}
fn fields(value: &Value, keys: &[(&str, &str)]) -> Vec<String> {
    keys.iter()
        .filter(|(key, _)| !value[*key].is_null())
        .map(|(key, label)| format!("  {label}：{}", text(&value[*key])))
        .collect()
}
fn evidence(value: &Value) -> Vec<String> {
    list(value)
        .map(|e| {
            format!(
                "  证据：项目 {} · 路径 {} · 第 {} 轮 · 事件 {} · 采样 {} · 来源 {}",
                text(&e["project_id"]),
                text(&e["path_id"]),
                text(&e["round"]),
                text(&e["event_index"]),
                text(&e["sampled_at_ms"]),
                text(&e["source"])
            )
        })
        .collect()
}
pub(super) fn unwrap(event: &Value) -> &Value {
    let mut value = event;
    for _ in 0..8 {
        if value["event"] != "project" {
            break;
        }
        value = &value["detail"];
    }
    value
}
pub(super) fn analysis(event: &Value, topology: bool) -> Vec<String> {
    let event = unwrap(event);
    let mut lines = vec![];
    if event["event"] != "analysis" {
        return lines;
    }
    if topology {
        for path in list(&event["topology"]) {
            lines.push(format!(
                "路径 {}{}",
                text(&path["path_id"]),
                if path["truncated"] == true {
                    " · 已截断"
                } else {
                    ""
                }
            ));
            for (section, label) in [
                ("dns", "DNS 解析视图"),
                ("connection", "连接分段视图"),
                ("trace", "逐跳测量视图"),
            ] {
                lines.push(format!("[{label}]"));
                let mut any = false;
                for edge in list(&path["edges"]).filter(|edge| edge["section"] == section) {
                    any = true;
                    let name = |id: &Value| {
                        list(&path["nodes"])
                            .find(|node| node["id"] == *id)
                            .map_or_else(
                                || text(id),
                                |node| {
                                    format!("{} ({})", text(&node["label"]), text(&node["role"]))
                                },
                            )
                    };
                    let level = match edge["level"].as_str() {
                        Some("measured") => "实测",
                        Some("configuration") => "配置推断（非实测）",
                        _ => "未知",
                    };
                    lines.push(format!(
                        "{} → {} · {level} · {}",
                        name(&edge["from"]),
                        name(&edge["to"]),
                        text(&edge["description"])
                    ));
                    lines.extend(evidence(&edge["evidence"]));
                }
                if !any {
                    lines.push("尚无此类路径证据".into());
                }
            }
            for attachment in list(&path["attachments"]) {
                lines.push(format!(
                    "旁注（不计作逐跳设备）：{} · {}",
                    text(&attachment["kind"]),
                    text(&attachment["label"])
                ));
                lines.extend(evidence(&attachment["evidence"]));
            }
        }
        return lines;
    }
    lines.push(format!(
        "后端统一诊断 · 规则版本 {} · 对照请求 {} · {}{}",
        text(&event["rules_version"]),
        flag(&event["comparison_requested"]),
        text(&event["comparison_status"]),
        if event["incomplete"] == true {
            " · 证据不完整"
        } else {
            ""
        }
    ));
    if event["truncated"] == true {
        lines.push("诊断结论已截断；完整性不能保证".into());
    }
    for finding in list(&event["findings"]) {
        let severity = match finding["severity"].as_str() {
            Some("risk") => "风险",
            Some("needs_verification") => "待核实",
            _ => "信息",
        };
        let confidence = match finding["confidence"].as_str() {
            Some("high") => "高",
            Some("medium") => "中",
            _ => "未知",
        };
        lines.push(format!(
            "[{severity} · 置信度{confidence}] {} · {}",
            text(&finding["title"]),
            text(&finding["kind"])
        ));
        lines.push(format!("  说明：{}", text(&finding["explanation"])));
        for alternative in list(&finding["alternatives"]) {
            lines.push(format!("  其他可能解释：{}", text(alternative)));
        }
        lines.push(format!("  建议：{}", text(&finding["recommendation"])));
        lines.extend(evidence(&finding["evidence"]));
    }
    if list(&event["findings"]).next().is_none() {
        lines.push("本次已完成项目没有产生固定规则发现；未测项目不表示没有差异或风险。".into());
    }
    for visibility in list(&event["visibility"]) {
        lines.push(format!(
            "可观察信息 · {}：{}",
            text(&visibility["location"]),
            text(&visibility["information"])
        ));
    }
    for note in list(&event["notes"]) {
        lines.push(format!("说明：{}", text(note)));
    }
    lines
}
pub(super) fn event_lines(event: &Value) -> Option<Vec<String>> {
    let name = event["event"].as_str()?;
    let detail = &event["detail"];
    let seq = text(&event["seq"]);
    let mut lines = match name {
        "project" => {
            let project = &event["project"];
            let mut lines = vec![format!(
                "项目 {} · 路径 {} · 第 {} 轮 · 事件 {} · 采样 {}",
                text(&project["project_id"]),
                text(&project["path_id"]),
                text(&project["round"]),
                text(&project["index"]),
                text(&project["sampled_at_ms"])
            )];
            lines.extend(super::presentation::event_lines(detail));
            return Some(lines);
        }
        "started" => vec![format!(
            "{} · {} · {}",
            text(&event["probe"]),
            text(&event["egress"]),
            text(&event["note"])
        )],
        "attempt" => vec![format!(
            "第 {} 次 · 连接/探测 {} ms · {} · {}",
            text(&event["seq"]),
            text(&event["rtt_ms"]),
            text(&event["detail"]),
            text(&event["error"])
        )],
        "summary" => vec![format!(
            "汇总：发送 {} · 成功 {} · 丢失 {} · 最小/平均/最大 {} / {} / {} ms",
            text(&event["sent"]),
            text(&event["received"]),
            {
                let number = |v: &Value| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                        .unwrap_or(0)
                };
                number(&event["sent"])
                    .saturating_sub(number(&event["received"]))
                    .to_string()
            },
            text(&event["min_ms"]),
            text(&event["avg_ms"]),
            text(&event["max_ms"])
        )],
        "done" => vec!["项目完成".into()],
        "project_status" => vec![format!(
            "项目 {} · 路径 {} · 第 {} 轮 · {}：{}",
            text(&event["project_id"]),
            text(&event["path_id"]),
            text(&event["round"]),
            text(&event["state"]),
            text(&event["message"])
        )],
        "analysis" => return Some(analysis(event, false)),
        "connection" => vec![format!(
            "连接 #{seq} · {} · {} → {} · 网卡 {} · {}",
            text(&event["purpose"]),
            text(&event["host"]),
            if event["address"].is_null() {
                "目标 IP 未知（代理远端解析）".into()
            } else {
                text(&event["address"])
            },
            text(&event["interface"]),
            text(&event["address_source"])
        )],
        "resolution" => {
            let mut lines = vec![format!(
                "解析 #{seq} · {} · {} · {}",
                text(&event["purpose"]),
                text(&event["host"]),
                text(&event["method"])
            )];
            lines.extend(fields(
                event,
                &[
                    ("resolver", "解析器"),
                    ("addresses", "结果"),
                    ("interface", "网卡"),
                ],
            ));
            return Some(lines);
        }
        "tls" => match detail["kind"].as_str() {
            Some("trust") => {
                let mut lines = vec![format!(
                    "TLS #{seq} · 系统根证书快照 · {} · {} 个根",
                    text(&detail["source"]),
                    text(&detail["loaded_roots"])
                )];
                lines.push(text(&detail["policy"]));
                for error in list(&detail["load_errors"]) {
                    lines.push(format!("  信任库加载问题：{}", text(error)));
                }
                if detail["load_errors_truncated"] == true {
                    lines.push("信任库错误列表已截断".into());
                }
                lines
            }
            Some("handshake") => {
                let mut lines = vec![format!(
                    "TLS #{seq} · {} · 验证通过 {} · 握手 {} ms",
                    text(&detail["server_name"]),
                    flag(&detail["verified"]),
                    text(&detail["handshake_ms"])
                )];
                if detail["verification_attempted"] == false {
                    lines.push("尚未收到可验证的证书；没有证书证据。".into());
                }
                lines.extend(fields(
                    detail,
                    &[
                        ("protocol", "TLS 版本"),
                        ("cipher", "密码套件"),
                        ("alpn", "ALPN"),
                        ("system_chain_verified", "系统根证书链验证通过"),
                        ("verification_error", "验证器错误"),
                        ("handshake_error", "握手错误"),
                        ("received_certificates", "收到证书数"),
                        ("retained_certificates", "保留证书数"),
                        ("retained_der_bytes", "保留 DER 字节"),
                        ("truncated", "证据截断"),
                    ],
                ));
                if detail["bundled_comparison"].is_object() {
                    lines.push(format!(
                        "  内置根离线对照：{} · 通过 {} · {}",
                        text(&detail["bundled_comparison"]["source"]),
                        flag(&detail["bundled_comparison"]["verified"]),
                        text(&detail["bundled_comparison"]["error"])
                    ));
                    lines.push("  信任库差异本身不表示监听；可能是企业证书或自定义根。".into());
                }
                lines
            }
            Some("certificate") => {
                let mut lines = vec![format!(
                    "TLS #{seq} · 证书 {} · SHA-256 {}",
                    text(&detail["index"]),
                    text(&detail["sha256"])
                )];
                lines.extend(fields(
                    detail,
                    &[
                        ("subject", "主体"),
                        ("issuer", "签发者"),
                        ("not_before", "生效时间"),
                        ("not_after", "到期时间"),
                        ("subject_alt_names", "名称"),
                        ("der_bytes", "DER 字节"),
                        ("metadata_truncated", "元数据截断"),
                        ("parse_error", "显示解析错误"),
                    ],
                ));
                lines
            }
            Some("certificate_der") => vec![format!(
                "TLS #{seq} · 证书 {} DER 分片 {} / {} 字节（完整内容保留在 JSON 导出）",
                text(&detail["index"]),
                text(&detail["offset"]),
                text(&detail["total_bytes"])
            )],
            _ => return None,
        },
        "http" => match detail["kind"].as_str() {
            Some("response") => {
                let mut lines = vec![format!(
                    "HTTP #{seq} · 跳转 {} · {} {} · 状态 {}",
                    text(&detail["hop"]),
                    text(&detail["method"]),
                    text(&detail["service"]),
                    text(&detail["status"])
                )];
                lines.extend(fields(
                    detail,
                    &[
                        ("version", "HTTP 版本"),
                        ("timings", "阶段耗时（ms）"),
                        ("header_bytes", "响应头字节"),
                        ("preview_bytes", "预览字节"),
                        ("body_truncated", "正文截断"),
                        ("headers_redacted", "响应头脱敏"),
                        ("preview_redacted", "预览脱敏"),
                    ],
                ));
                lines
            }
            Some("headers") => {
                let mut lines = vec![format!(
                    "HTTP #{seq} · 响应头 · 状态 {}",
                    text(&detail["status"])
                )];
                for header in list(&detail["headers"]) {
                    lines.push(format!(
                        "  {}: {}",
                        text(&header["name"]),
                        text(&header["value"])
                    ));
                }
                lines
            }
            Some("preview") => {
                let mut lines = vec![format!(
                    "HTTP #{seq} · 正文预览（偏移 {}）",
                    text(&detail["offset"])
                )];
                lines.extend(
                    detail["text"]
                        .as_str()
                        .unwrap_or_default()
                        .lines()
                        .map(clean),
                );
                lines
            }
            Some("redirect") => vec![format!(
                "HTTP #{seq} · {} → {} · 跟随 {} · {}",
                text(&detail["from"]),
                text(&detail["to"]),
                flag(&detail["followed"]),
                text(&detail["reason"])
            )],
            Some("failure") => vec![format!(
                "HTTP #{seq} · {}：{}",
                text(&detail["phase"]),
                text(&detail["message"])
            )],
            _ => return None,
        },
        "public_ip" => vec![format!(
            "HTTPS 出口观察 #{seq} · {} · 服务 {}（与 STUN 分开，不作 NAT 分类）",
            text(&event["address"]),
            text(&event["service"])
        )],
        "tcp" => {
            let result = &event["result"];
            vec![format!(
                "TCP #{seq} · {}:{} · {} · 连接/握手 {} ms · {}",
                text(&result["host"]),
                text(&result["port"]),
                text(&result["outcome"]),
                text(&result["connect_ms"]),
                text(&result["message"])
            )]
        }
        "trace" => {
            let sample = &event["sample"];
            vec![
                format!(
                    "逐跳 · TTL {} · 第 {} 次 · {} · {} ms · {}",
                    text(&sample["hop"]),
                    text(&sample["probe"]),
                    text(&sample["address"]),
                    text(&sample["rtt_ms"]),
                    text(&sample["outcome"])
                ),
                if sample["outcome"] == "no_response" {
                    "无响应只表示本次没有响应；不推断设备缺失或异常监听。".into()
                } else {
                    text(&sample["message"])
                },
            ]
        }
        "trace_summary" => {
            let mut lines = vec![format!(
                "逐跳汇总 · {} · {} · 到达 {} · 完成 {}",
                text(&event["target"]),
                text(&event["family"]),
                flag(&event["reached"]),
                flag(&event["completed"])
            )];
            lines.extend(fields(
                event,
                &[
                    ("capability", "平台能力"),
                    ("physical", "物理路径证据"),
                    ("probes_sent", "实际已提交探测"),
                    ("message", "说明"),
                ],
            ));
            lines
        }
        "echo" => {
            let result = &event["observation"];
            vec![
                format!(
                    "ICMP #{seq} · {} · {} · 证明目标可达 {}",
                    text(&result["target"]),
                    text(&result["outcome"]),
                    flag(&result["proves_reachability"])
                ),
                text(&result["message"]),
                format!("物理路径：{}", text(&result["physical"])),
            ]
        }
        "stun" => {
            let mut lines = vec![format!(
                "STUN 出口观察 · {} · 代理会话 {}",
                text(&event["server"]),
                flag(&event["proxy_session"])
            )];
            lines.extend(observation(&event["observation"]));
            lines.push(text(&event["note"]));
            lines
        }
        "nat" => {
            if event["kind"] == "report" {
                let report = &event["report"];
                let mut lines = vec![format!(
                    "NAT · 物理网卡 {} · {} · 本地 {}",
                    text(&report["interface_name"]),
                    text(&report["family"]),
                    text(&report["local_endpoint"])
                )];
                lines.extend(fields(
                    report,
                    &[
                        ("mapping", "映射行为"),
                        ("filtering", "过滤行为"),
                        ("comparison", "多服务映射对照"),
                        ("traditional_type", "传统分类"),
                        ("advanced_capable", "已确认 RFC 5780"),
                        ("unstable", "映射不稳定"),
                        ("partial", "证据不完整"),
                        ("physical_path", "物理出口证据"),
                    ],
                ));
                for note in list(&report["notes"]) {
                    lines.push(text(note));
                }
                lines
            } else {
                observation(&event["observation"])
            }
        }
        "policy" => {
            let mut lines = vec!["配置策略证据（不等于实际链路测量）".into()];
            lines.extend(fields(
                event,
                &[
                    ("protocol", "协议"),
                    ("encrypted", "配置加密"),
                    ("certificate_verification", "配置证书验证"),
                    ("matched_running", "与运行版本匹配"),
                    ("leaf_tag", "固定成员"),
                ],
            ));
            lines
        }
        _ => return None,
    };
    lines.retain(|line| !line.is_empty());
    Some(lines)
}
fn observation(value: &Value) -> Vec<String> {
    let mut lines = vec![format!(
        "STUN · {} · {} → {} · 收到响应 {}",
        text(&value["phase"]),
        text(&value["server"]),
        text(&value["mapped_endpoint"]),
        flag(&value["response_received"])
    )];
    lines.extend(fields(
        value,
        &[
            ("local_endpoint", "本地端点"),
            ("source", "响应来源"),
            ("expected_source", "预期来源"),
            ("other_address", "替代地址"),
            ("response_origin", "响应原点"),
            ("attempts", "发送次数"),
            ("discarded_packets", "丢弃无效包"),
            ("elapsed_ms", "耗时 ms"),
            ("error", "说明"),
        ],
    ));
    lines
}
