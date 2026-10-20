use crate::i18n::t;
use anyhow::{ensure, Context, Result};
use astra_ipc::platform::OpenOptionsExt;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Serialize, Deserialize)]
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
        crate::manager::private_dir(&directory)?;
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
mod tests {
    use super::*;
    #[test]
    fn editor_arguments_preserve_windows_paths_unicode_and_metacharacters_without_a_shell() {
        assert_eq!(
            arguments(r#""C:\Program Files\Editor\editor.exe" --wait "中文 空格""#).unwrap(),
            [r"C:\Program Files\Editor\editor.exe", "--wait", "中文 空格"]
        );
        assert_eq!(
            arguments(r#"["C:\\Program Files\\Editor\\editor.exe","--wait","$value;literal"]"#)
                .unwrap(),
            [
                r"C:\Program Files\Editor\editor.exe",
                "--wait",
                "$value;literal"
            ]
        );
        for text in ["", "[]", "['invalid']", "\"unclosed", "[\"\"]"] {
            assert!(arguments(text).is_err(), "{text}");
        }
    }
    #[test]
    fn editor_process_receives_literal_draft_path_and_failure_retains_the_file() {
        let home =
            std::env::temp_dir().join(format!("astra editor process 中文 {}", std::process::id()));
        crate::manager::private_dir(&home).unwrap();
        let draft = Draft::create(&home, "proxy".into(), "standalone=true\n".into()).unwrap();
        #[cfg(windows)]
        let (script, command) = {
            let script = home.join("editor helper.ps1");
            fs::write(&script,"param([string]$target)\n[System.IO.File]::WriteAllText($target, \"standalone=false`n\", [System.Text.UTF8Encoding]::new($false))\n").unwrap();
            let args = vec![
                "powershell.exe".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-File".into(),
                script.to_string_lossy().into_owned(),
            ];
            (script, args)
        };
        #[cfg(not(windows))]
        let (script, command) = {
            let script = home.join("editor helper.sh");
            fs::write(&script, "printf 'standalone=false\\n' > \"$1\"\n").unwrap();
            let args = vec!["/bin/sh".into(), script.to_string_lossy().into_owned()];
            (script, args)
        };
        launch(&command, &draft, &home).unwrap();
        assert_eq!(draft.read().unwrap(), "standalone=false\n");
        fs::write(&script, "exit 7\n").unwrap();
        assert!(launch(&command, &draft, &home).is_err());
        assert!(launch(&["astra-editor-that-does-not-exist".into()], &draft, &home).is_err());
        assert_eq!(draft.read().unwrap(), "standalone=false\n");
        fs::remove_file(script).unwrap();
        fs::remove_file(draft.path).unwrap();
        fs::remove_dir(home.join("external-drafts")).unwrap();
        fs::remove_dir(home).unwrap();
    }
    #[test]
    fn draft_io_is_bounded_and_does_not_change_the_saved_configuration() {
        let home = std::env::temp_dir().join(format!("astra-external-{}", std::process::id()));
        crate::manager::private_dir(&home).unwrap();
        let source = home.join("saved.toml");
        fs::write(&source, "original").unwrap();
        let draft = Draft::create(&home, "proxy".into(), "standalone=true\n".into()).unwrap();
        fs::write(&draft.path, "\u{feff}standalone=false\n").unwrap();
        assert_eq!(draft.read().unwrap(), "standalone=false\n");
        assert_eq!(fs::read_to_string(&source).unwrap(), "original");
        fs::write(&draft.path, vec![b'a'; 480 * 1024 + 1]).unwrap();
        assert!(draft.read().is_err());
        fs::remove_file(&draft.path).unwrap();
        fs::remove_file(source).unwrap();
        fs::remove_dir(home.join("external-drafts")).unwrap();
        fs::remove_dir(home).unwrap();
    }
}
