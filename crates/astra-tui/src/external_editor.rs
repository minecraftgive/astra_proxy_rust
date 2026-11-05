use crate::i18n::t;
use anyhow::{ensure, Context, Result};
use astra_platform::platform::OpenOptionsExt;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub name: String,
    pub path: PathBuf,
    pub base: String,
}
impl Draft {
    pub fn create(home: &Path, name: String, text: String) -> Result<Self> {
        ensure!(
            text.len() <= 480 * 1024,
            t("External draft exceeds 480 KiB")
        );
        let directory = home.join("external-drafts");
        astra_platform::storage::private_dir(&directory)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = directory.join(format!("draft-{}-{nonce}.toml", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(Self {
            name,
            path,
            base: text,
        })
    }
    pub fn read(&self) -> Result<String> {
        let mut data = Vec::new();
        fs::File::open(&self.path)?
            .take(480 * 1024 + 1)
            .read_to_end(&mut data)?;
        ensure!(
            data.len() <= 480 * 1024,
            t("External draft exceeds 480 KiB")
        );
        Ok(String::from_utf8(data)
            .context(t("External draft must be UTF-8"))?
            .trim_start_matches('\u{feff}')
            .to_owned())
    }
}
/// No shell is involved. JSON argv also supports Windows paths and literal arguments.
pub fn arguments(value: &str) -> Result<Vec<String>> {
    let value = value.trim();
    let args: Vec<String> = if value.starts_with('[') {
        serde_json::from_str(value)
            .context(t("Invalid editor command; use a JSON argument list"))?
    } else if Path::new(value).is_file() {
        vec![value.into()]
    } else {
        let mut args = Vec::new();
        let mut current = String::new();
        let mut quote = None;
        let mut started = false;
        for character in value.chars() {
            if let Some(delimiter) = quote {
                if character == delimiter {
                    quote = None;
                } else {
                    current.push(character);
                }
                started = true;
            } else if matches!(character, '\'' | '"') {
                quote = Some(character);
                started = true;
            } else if character.is_whitespace() {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            } else {
                current.push(character);
                started = true;
            }
        }
        ensure!(quote.is_none(), t("Unclosed quote in editor command"));
        if started {
            args.push(current);
        }
        args
    };
    ensure!(
        !args.is_empty() && !args[0].is_empty() && args.iter().all(|a| !a.contains('\0')),
        t("Invalid editor command")
    );
    Ok(args)
}
pub fn command() -> Result<Vec<String>> {
    for key in ["ASTRA_EDITOR", "VISUAL", "EDITOR"] {
        if let Some(value) = std::env::var_os(key) {
            let value = value
                .to_str()
                .context(t("Editor command must be Unicode"))?;
            if !value.trim().is_empty() {
                return arguments(value);
            }
        }
    }
    Ok(vec![if cfg!(windows) { "notepad.exe" } else { "vi" }.into()])
}
pub fn launch(arguments: &[String], draft: &Draft, working_dir: &Path) -> Result<()> {
    let status = Command::new(&arguments[0])
        .args(&arguments[1..])
        .arg(&draft.path)
        .current_dir(working_dir)
        .status()
        .context(t("Cannot start external editor"))?;
    ensure!(
        status.success(),
        "{}: {status}",
        t("External editor exited unsuccessfully")
    );
    Ok(())
}

#[cfg(test)]
#[path = "test/external_editor_test.rs"]
mod tests;
