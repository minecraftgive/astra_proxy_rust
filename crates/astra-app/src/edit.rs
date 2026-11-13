use anyhow::{bail, Context, Result};
use astra_model::document::PathPart as Part;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table, TableLike};
fn t(source: &str) -> &str {
    source
}
pub fn node_index(path: &[Part]) -> Option<usize> {
    match path {
        [Part::Key(key), Part::Index(index), ..] if key == "outbounds" => Some(*index),
        _ => None,
    }
}
pub fn item_index(path: &[Part]) -> Option<(&str, usize)> {
    match path {
        [Part::Key(key), Part::Index(index), ..] if key == "outbounds" || key == "rules" => {
            Some((key, *index))
        }
        _ => None,
    }
}
pub fn table_at<'a>(table: &'a dyn TableLike, path: &[Part]) -> Option<&'a dyn TableLike> {
    if path.is_empty() {
        return Some(table);
    }
    let Part::Key(key) = &path[0] else {
        return None;
    };
    let item = table.get(key)?;
    if let Some(Part::Index(index)) = path.get(1) {
        table_at(item.as_array_of_tables()?.get(*index)?, &path[2..])
    } else {
        table_at(item.as_table_like()?, &path[1..])
    }
}
pub fn table_at_mut<'a>(
    table: &'a mut dyn TableLike,
    path: &[Part],
) -> Result<&'a mut dyn TableLike> {
    if path.is_empty() {
        return Ok(table);
    }
    let Part::Key(key) = &path[0] else {
        bail!(t("无效的条目路径"));
    };
    if let Some(Part::Index(index)) = path.get(1) {
        let child = table
            .get_mut(key)
            .and_then(Item::as_array_of_tables_mut)
            .and_then(|a| a.get_mut(*index))
            .context(t("条目已不存在，请返回上一级"))?;
        table_at_mut(child, &path[2..])
    } else {
        if table.get(key).is_none() {
            table.insert(key, Item::Table(Table::new()));
        }
        let child = table
            .get_mut(key)
            .and_then(Item::as_table_like_mut)
            .context(t("该条目不是参数分组"))?;
        table_at_mut(child, &path[1..])
    }
}
pub fn scalar(item: &Item) -> String {
    if let Some(s) = item.as_str() {
        s.to_owned()
    } else {
        item.to_string().trim().to_owned()
    }
}
pub fn replace_field(table: &mut dyn TableLike, key: &str, mut item: Item) {
    if let Some(existing) = table.get_mut(key) {
        if let (Some(old), Some(new)) = (existing.as_value(), item.as_value_mut()) {
            *new.decor_mut() = old.decor().clone();
        }
        *existing = item;
    } else {
        table.insert(key, item);
    }
}
// Encoding respects parsed source positions, which must follow the new logical order
// after moving an array item (including all of that item's nested tables).
pub fn position_tables(table: &mut Table, position: &mut isize) {
    table.set_position(*position);
    *position += 1;
    for (_, item) in table.iter_mut() {
        if let Some(child) = item.as_table_mut() {
            position_tables(child, position);
        } else if let Some(array) = item.as_array_of_tables_mut() {
            for child in array.iter_mut() {
                position_tables(child, position);
            }
        }
    }
}
pub fn last_key(path: &[Part]) -> Option<&str> {
    path.iter().rev().find_map(|p| {
        if let Part::Key(k) = p {
            Some(k.as_str())
        } else {
            None
        }
    })
}
pub fn rule_set_name(path: &[Part]) -> Option<&str> {
    match path {
        [Part::Key(root), Part::Key(name), ..] if root == "rule_sets" => Some(name),
        _ => None,
    }
}
pub fn proxy_group_name(path: &[Part]) -> Option<&str> {
    match path {
        [Part::Key(root), Part::Key(name), ..] if root == "proxy_groups" => Some(name),
        _ => None,
    }
}
pub fn proxy_targets(doc: &DocumentMut) -> Vec<String> {
    let mut tags: Vec<String> = doc
        .get("outbounds")
        .and_then(Item::as_array_of_tables)
        .into_iter()
        .flat_map(|nodes| nodes.iter())
        .filter_map(|node| node.get("tag").and_then(Item::as_str))
        .map(str::to_owned)
        .collect();
    if let Some(groups) = doc.get("proxy_groups").and_then(Item::as_table_like) {
        tags.extend(groups.iter().map(|(name, _)| name.to_owned()));
    }
    tags.sort();
    if !tags.iter().any(|v| v == "PROXY") {
        tags.push("PROXY".into());
        tags.sort();
    }
    tags.dedup();
    tags
}
pub fn proxy_references(doc: &DocumentMut, target: &str) -> bool {
    doc.get("outbounds")
        .and_then(Item::as_array_of_tables)
        .is_some_and(|nodes| {
            nodes.iter().any(|node| {
                ["dialer_proxy", "dialer-proxy"]
                    .iter()
                    .any(|key| node.get(key).and_then(Item::as_str) == Some(target))
            })
        })
        || doc.get("standalone_outbound").and_then(Item::as_str) == Some(target)
        || doc
            .get("proxy_groups")
            .and_then(Item::as_table_like)
            .is_some_and(|groups| {
                groups.iter().any(|(_, group)| {
                    group
                        .as_table_like()
                        .and_then(|g| g.get("selected"))
                        .and_then(Item::as_str)
                        == Some(target)
                        || group
                            .as_table_like()
                            .and_then(|g| g.get("members"))
                            .and_then(Item::as_array)
                            .is_some_and(|a| a.iter().any(|m| m.as_str() == Some(target)))
                })
            })
}
pub fn rename_proxy_references(doc: &mut DocumentMut, previous: &str, next: &str) {
    if let Some(nodes) = doc
        .get_mut("outbounds")
        .and_then(Item::as_array_of_tables_mut)
    {
        for node in nodes.iter_mut() {
            for key in ["dialer_proxy", "dialer-proxy"] {
                if node.get(key).and_then(Item::as_str) == Some(previous) {
                    node.insert(key, value(next));
                }
            }
        }
    }

    if doc.get("standalone_outbound").and_then(Item::as_str) == Some(previous) {
        replace_field(doc.as_table_mut(), "standalone_outbound", value(next));
    }
    if let Some(groups) = doc
        .get_mut("proxy_groups")
        .and_then(Item::as_table_like_mut)
    {
        for (_, group) in groups.iter_mut() {
            if let Some(group) = group.as_table_like_mut() {
                if let Some(members) = group.get_mut("members").and_then(Item::as_array_mut) {
                    for member in members.iter_mut() {
                        if member.as_str() == Some(previous) {
                            let mut replacement = toml_edit::Value::from(next);
                            *replacement.decor_mut() = member.decor().clone();
                            *member = replacement;
                        }
                    }
                }
                if group.get("selected").and_then(Item::as_str) == Some(previous) {
                    replace_field(group, "selected", value(next));
                }
            }
        }
    }
}
pub fn rule_category(table: &Table) -> &'static str {
    match table.get("kind").and_then(Item::as_str).unwrap_or("") {
        "domain_exact" | "domain_suffix" | "domain_subdomain" | "domain_pattern"
        | "domain_keyword" | "domain_regex" => "Domain rules",
        "geosite" => "GEOSITE",
        "ip_cidr" | "geoip" => "IP rules",
        "process_name" | "process_path" => "Process rules",
        "rule_set" => "Rule set references",
        "port" | "network" => "Network / port rules",
        _ => "Other rules",
    }
}
pub fn rule_groups(array: &ArrayOfTables) -> Vec<(usize, usize, &'static str)> {
    let mut groups: Vec<(usize, usize, &'static str)> = vec![];
    for (index, table) in array.iter().enumerate() {
        let category = rule_category(table);
        if let Some(last) = groups.last_mut().filter(|last| last.2 == category) {
            last.1 = index;
        } else {
            groups.push((index, index, category));
        }
    }
    groups
}
pub fn rule_set_references(doc: &DocumentMut, name: Option<&str>) -> usize {
    doc.get("rules")
        .and_then(Item::as_array_of_tables)
        .map(|rules| {
            rules
                .iter()
                .filter(|rule| {
                    rule.get("kind").and_then(Item::as_str) == Some("rule_set")
                        && name.is_none_or(|name| {
                            rule.get("value").and_then(Item::as_str) == Some(name)
                        })
                })
                .count()
        })
        .unwrap_or(0)
}
pub fn collection<'a>(doc: &'a DocumentMut, path: &[Part]) -> Option<&'a ArrayOfTables> {
    let (Part::Key(key), parent) = path.split_last()? else {
        return None;
    };
    table_at(doc.as_table(), parent)?
        .get(key)?
        .as_array_of_tables()
}
pub fn collection_mut<'a>(
    doc: &'a mut DocumentMut,
    path: &[Part],
) -> Result<&'a mut ArrayOfTables> {
    let (Part::Key(key), parent) = path.split_last().context(t("没有选择条目列表"))? else {
        bail!(t("没有选择条目列表"));
    };
    let table = table_at_mut(doc.as_table_mut(), parent)?;
    if table.get(key).is_none() {
        table.insert(key, Item::ArrayOfTables(ArrayOfTables::new()));
    }
    table
        .get_mut(key)
        .and_then(Item::as_array_of_tables_mut)
        .context(t("该分组不是条目列表"))
}
pub fn native_config(protocol: &str) -> Table {
    let mut table = Table::new();
    table["protocol"] = value(protocol);
    table["server"] = value("node.example.com");
    table["port"] = value(443);
    if matches!(protocol, "vless" | "vmess" | "tuic") {
        table["uuid"] = value("00000000-0000-0000-0000-000000000001");
    }
    if matches!(protocol, "shadowsocks" | "trojan" | "hysteria2" | "tuic") {
        table["password"] = value("change-me");
    }
    if protocol == "shadowsocks" {
        table["cipher"] = value("aes-128-gcm");
    }
    if protocol != "shadowsocks" {
        let mut tls = Table::new();
        tls["server_name"] = value("node.example.com");
        table["tls"] = Item::Table(tls);
    }
    table
}
pub fn outbound_template(kind: &str, tag: &str) -> Table {
    let mut table = Table::new();
    table["tag"] = value(tag);
    if kind.starts_with("native") {
        table["kind"] = value("native");
        table["config"] = Item::Table(native_config(
            kind.strip_prefix("native:").unwrap_or("vless"),
        ));
    } else {
        table["kind"] = value(kind);
        table["server"] = value(if matches!(kind, "socks5" | "http") {
            "127.0.0.1:7890"
        } else {
            "node.example.com:443"
        });
        if kind == "trojan_tls" {
            table["sni"] = value("node.example.com");
            table["password"] = value("change-me");
        }
        if kind == "vless_tls" {
            table["sni"] = value("node.example.com");
            table["uuid"] = value("00000000-0000-0000-0000-000000000001");
        }
    }
    table
}

pub fn parse_editor_document(
    text: &str,
    is_proxy: bool,
    t: fn(&str) -> &str,
) -> Result<DocumentMut> {
    let mut doc: DocumentMut = text
        .parse()
        .context(t("配置格式无法解析。原草稿已保留，请修复配置后重新打开"))?;
    // Both [[rules]] and rules = [{...}] are valid core configurations.
    for key in ["rules", "outbounds"] {
        if let Some(array) = doc.get(key).and_then(Item::as_array) {
            let mut tables = ArrayOfTables::new();
            for item in array.iter() {
                tables.push(
                    item.as_inline_table()
                        .context(t("列表中需要参数条目"))?
                        .clone()
                        .into_table(),
                );
            }
            doc[key] = Item::ArrayOfTables(tables);
        }
    }
    if is_proxy {
        let groups = table_at_mut(doc.as_table_mut(), &[Part::Key("proxy_groups".into())])?;
        if groups.get("PROXY").is_none() {
            let mut group = Table::new();
            group["mode"] = value("select");
            group["include_all"] = value(true);
            groups.insert("PROXY", Item::Table(group));
        }
        // DIRECT/REJECT are built-in groups whose own member is the terminal outbound.
        for name in ["DIRECT", "REJECT"] {
            if groups.get(name).is_none() {
                let mut group = Table::new();
                group["mode"] = value("select");
                group["members"] = value(toml_edit::Array::from_iter([name]));
                groups.insert(name, Item::Table(group));
            }
        }
    }
    if is_proxy {
        let mut data: toml::Table = toml::from_str(&doc.to_string())?;
        astra_convert::subscription::prepare_groups(&mut data)?;
        if let Some(groups) = data.get("proxy_groups").and_then(|v| v.as_table()) {
            for (name, group) in groups
                .iter()
                .filter(|(_, g)| g.get("subscription").is_some())
            {
                let snippet: DocumentMut = toml::to_string(group)?.parse()?;
                let table = table_at_mut(doc.as_table_mut(), &[Part::Key("proxy_groups".into())])?;
                if table.get(name).is_none() {
                    table.insert(name, Item::Table(snippet.as_table().clone()));
                }
            }
        }
    }
    Ok(doc)
}
