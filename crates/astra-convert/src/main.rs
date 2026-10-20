use anyhow::{bail, ensure, Context, Result};
use astra_convert::{subscription, Options};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const HELP: &str = "astra-convert — 转换 Clash/Mihomo YAML 或订阅为 Astra TOML

用法: astra-convert INPUT.yaml|URL|vless://... [选项]
  --subscribe URL       本地 YAML 绑定订阅 URL（URL 输入自动绑定）
  --include-config      订阅额外导入代理组和规则（默认仅节点）
  --interval SECONDS    节点更新间隔，默认 3600，最小 60
  --user-agent VALUE    订阅请求 User-Agent
  INPUT 为 - 时从标准输入读取
  -o, --output-dir DIR    输出目录（默认输入 YAML 所在目录；stdin 使用当前目录）
  --select GROUP=MEMBER  固定代理组成员，可重复
  --rule-socket PATH     两份配置共用的 UDS 路径
  --geoip-db PATH       GeoIP MMDB 或 geoip.dat 数据库路径
  --geosite-db PATH     GeoSite .dat 数据库路径
  --base-dir DIR        stdin 的本地规则集基准目录
  --strict              遇到不支持的规则/规则集即失败
  --check               仅转换检查，不写文件
  -h, --help            显示帮助

订阅默认仅输出 proxy.toml；--include-config 或普通文件转换另输出 rule.toml。已有文件不会覆盖。
未支持字段/协议/规则会跳过，转换摘要写入标准错误。";

fn main() {
    if let Err(err) = run() {
        eprintln!("astra-convert: {err:#}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut input = None;
    let mut output: Option<PathBuf> = None;
    let mut options = Options::default();
    let mut check = false;
    let mut include_config = false;
    let mut subscribe = None;
    let mut interval = 3600;
    let mut user_agent = subscription::Subscription::default().user_agent;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(());
            }
            "--check" => check = true,
            "--include-config" => include_config = true,
            "--subscribe" => subscribe = Some(args.next().context("缺少订阅 URL")?),
            "--interval" => interval = args.next().context("缺少间隔")?.parse()?,
            "--user-agent" => user_agent = args.next().context("缺少 User-Agent")?,
            "--strict" => options.strict_rules = true,
            "--geosite-db" => {
                options.geosite_database = Some(
                    fs::canonicalize(args.next().context("缺少 GeoSite 路径")?)?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            "--geoip-db" => {
                options.geoip_database = Some(
                    fs::canonicalize(args.next().context("缺少 GeoIP 数据库路径")?)?
                        .to_string_lossy()
                        .into_owned(),
                )
            }
            "--base-dir" => {
                options.base_dir = fs::canonicalize(args.next().context("缺少规则集目录")?)?
            }
            "-o" | "--output-dir" => output = Some(args.next().context("缺少输出目录")?.into()),
            "--rule-socket" => options.rule_socket = Some(args.next().context("缺少 socket 路径")?),
            "--select" => {
                let value = args.next().context("缺少 GROUP=MEMBER")?;
                let (group, member) = value
                    .split_once('=')
                    .context("--select 需要 GROUP=MEMBER")?;
                ensure!(
                    !group.is_empty() && !member.is_empty(),
                    "代理组和成员不可为空"
                );
                ensure!(
                    options
                        .selections
                        .insert(group.into(), member.into())
                        .is_none(),
                    "代理组重复选择"
                );
            }
            "-" => {
                ensure!(input.is_none(), "只能指定一个输入文件");
                input = Some(arg);
            }
            _ if arg.starts_with('-') => bail!("未知参数 {arg}"),
            _ => {
                ensure!(input.is_none(), "只能指定一个输入文件");
                input = Some(arg);
            }
        }
    }
    let input = input.context("请指定 mihomo YAML 文件；使用 --help 查看用法")?;
    let share = astra_convert::share::is_share(&input);
    let remote = input.starts_with("https://") || input.starts_with("http://");
    if remote {
        ensure!(subscribe.is_none(), "URL 输入不可再指定 --subscribe");
        subscribe = Some(input.clone());
    }
    if let Some(url) = &subscribe {
        subscription::Subscription {
            url: url.clone(),
            interval,
            user_agent: user_agent.clone(),
            ..Default::default()
        }
        .validate()?;
    }
    let text = if share {
        input.clone()
    } else if remote {
        subscription::download(&input, &user_agent)?
    } else if input == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    } else {
        fs::read_to_string(&input).context("无法读取输入文件")?
    };
    if options.base_dir.as_os_str().is_empty() {
        options.base_dir = if input == "-" || remote || share {
            std::env::current_dir()?
        } else {
            fs::canonicalize(&input)?
                .parent()
                .context("input has no parent")?
                .to_path_buf()
        };
    }
    let output = match output {
        Some(output) => output,
        None if input == "-" || remote || share => std::env::current_dir()?,
        None => std::path::absolute(&input)?
            .parent()
            .context("input has no parent")?
            .to_path_buf(),
    };
    let source_label = if share {
        "<node-links>".to_owned()
    } else if remote {
        "<subscription>".to_owned()
    } else if input == "-" {
        "<stdin>".to_owned()
    } else {
        std::path::absolute(&input)?.display().to_string()
    };
    let nodes_only =
        share || astra_convert::share::is_share(&text) || (subscribe.is_some() && !include_config);
    let mut result = if share || astra_convert::share::is_share(&text) {
        astra_convert::share::convert_links(&text, &options)
    } else if nodes_only {
        subscription::convert_nodes(&text, &options, &user_agent, false)
    } else {
        subscription::convert_clash(&text, &options, &user_agent)
    }
    .with_context(|| format!("输入 {source_label}"))?;
    if let Some(url) = subscribe {
        ensure!(result.nodes > 0, "订阅无可用节点");
        let mut proxy: toml::Table = toml::from_str(&result.proxy)?;
        let settings = subscription::Subscription {
            url,
            interval,
            user_agent,
            import_groups: include_config,
            managed_nodes: proxy["outbounds"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v["tag"].as_str().map(str::to_owned))
                .collect(),
            managed_groups: if nodes_only {
                vec![]
            } else {
                proxy["proxy_groups"]
                    .as_table()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect()
            },
        };
        proxy.insert("subscription".into(), toml::Value::try_from(settings)?);
        result.proxy = toml::to_string_pretty(&proxy)?;
    }
    eprintln!("── {source_label} ──");
    for warning in &result.warnings {
        eprintln!("提示: {warning}");
    }
    if !check {
        fs::create_dir_all(&output).context("无法创建输出目录")?;
        if nodes_only {
            let mut file = create(&output.join("proxy.toml"))?;
            file.write_all(result.proxy.as_bytes())?;
            file.sync_all()?;
        } else {
            write_pair(&output, &result.proxy, &result.rules)?;
        }
        if nodes_only {
            eprintln!(
                "已写入 {}（仅订阅节点）",
                output.join("proxy.toml").display()
            );
        } else {
            eprintln!(
                "已写入 {} 和 {}",
                output.join("proxy.toml").display(),
                output.join("rule.toml").display()
            );
        }
    }
    eprintln!(
        "转换{}：{} 个节点，{} 条规则，{} 条提示",
        if check { "检查通过" } else { "完成" },
        result.nodes,
        result.rule_count,
        result.warnings.len()
    );
    Ok(())
}

fn create(path: &Path) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("无法创建 {}（已有文件不会覆盖）", path.display()))
}
fn write_pair(dir: &Path, proxy: &str, rules: &str) -> Result<()> {
    let proxy_path = dir.join("proxy.toml");
    let rule_path = dir.join("rule.toml");
    let mut p = create(&proxy_path)?;
    let mut r = match create(&rule_path) {
        Ok(r) => r,
        Err(err) => {
            drop(p);
            let _ = fs::remove_file(&proxy_path);
            return Err(err);
        }
    };
    let written = (|| -> Result<()> {
        p.write_all(proxy.as_bytes())?;
        r.write_all(rules.as_bytes())?;
        p.sync_all()?;
        r.sync_all()?;
        Ok(())
    })();
    drop(p);
    drop(r);
    if written.is_err() {
        let _ = fs::remove_file(proxy_path);
        let _ = fs::remove_file(rule_path);
    }
    written
}
