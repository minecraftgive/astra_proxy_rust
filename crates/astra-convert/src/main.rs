use anyhow::{bail, ensure, Context, Result};
use astra_convert::{convert, Options};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const HELP: &str = "astra-convert — 离线转换 mihomo YAML 为 Astra TOML

用法: astra-convert INPUT.yaml [选项]
  INPUT 为 - 时从标准输入读取
  -o, --output-dir DIR    输出目录（默认 converted）
  --select GROUP=MEMBER  固定代理组成员，可重复
  --rule-socket PATH     两份配置共用的 UDS 路径
  --check               仅转换检查，不写文件
  -h, --help            显示帮助

输出 proxy.toml 和 rule.toml；已有文件不会覆盖。
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
    let mut output = PathBuf::from("converted");
    let mut options = Options::default();
    let mut check = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(());
            }
            "--check" => check = true,
            "-o" | "--output-dir" => output = args.next().context("缺少输出目录")?.into(),
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
    let text = if input == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    } else {
        fs::read_to_string(&input).context("无法读取输入文件")?
    };
    let result = convert(&text, &options)?;
    for warning in &result.warnings {
        eprintln!("提示: {warning}");
    }
    if !check {
        fs::create_dir_all(&output).context("无法创建输出目录")?;
        write_pair(&output, &result.proxy, &result.rules)?;
        eprintln!(
            "已写入 {} 和 {}",
            output.join("proxy.toml").display(),
            output.join("rule.toml").display()
        );
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
