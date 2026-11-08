use crate::commands::{Id, Scope, SPECS};
use crate::{drafts, editor::Editor, i18n::t, logs, protocol::*, tr, update};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, io, path::PathBuf, time::Instant};
#[derive(Clone, Serialize, Deserialize)]
pub enum Prompt {
    AddName,
    AddPath(String),
    Config(String),
    Clone(String),
    Remove(String),
    ForceStart(String),
    Shutdown,
    Discard,
    Import,
    UpdatePackage,
    UpdateConfirm(crate::protocol::updates::UpdateRequest),
    ImportNode(crate::import::MergeMode),
    ImportRule(crate::import::MergeMode),
    ExportConfig {
        text: String,
    },
    ExportNode {
        text: String,
        share: bool,
    },
    ExportNodeDestination {
        text: String,
        share: bool,
        clipboard: bool,
    },
    ImportNodeSelection {
        name: String,
        path: PathBuf,
        candidates: crate::import::NodeImport,
        selected: Vec<usize>,
        cursor: usize,
        mode: crate::import::MergeMode,
    },
    ExportRule {
        text: String,
    },
    ExportTerminal {
        text: String,
    },
    ImportConfirm {
        text: String,
        notices: String,
    },
    Explain(String),
    CloseAll {
        name: String,
        pid: Option<u32>,
    },
    /// Linux only: a sudo-launched core takes over the TUN listener.
    ElevateTun {
        name: String,
        pid: Option<u32>,
    },
    /// Find on a list page; Enter jumps to the next match.
    Find(usize),
}
impl Prompt {
    pub fn picker(&self) -> bool {
        matches!(
            self,
            Self::ExportNodeDestination { .. } | Self::ImportNodeSelection { .. }
        )
    }
    pub fn confirmation(&self) -> bool {
        matches!(
            self,
            Self::Remove(_)
                | Self::ForceStart(_)
                | Self::Shutdown
                | Self::Discard
                | Self::ImportConfirm { .. }
                | Self::CloseAll { .. }
                | Self::ElevateTun { .. }
                | Self::UpdateConfirm(_)
        )
    }
    pub fn title(&self) -> &str {
        match self {
            Self::AddName => t("Name (1–24: a-z, 0-9, -_)"),
            Self::Find(_) => t("Find (Enter jumps to the next match)"),
            Self::AddPath(_) => t("Configuration path"),
            Self::Config(_) => t("Configuration path"),
            Self::Clone(_) => t("Copy to path"),
            Self::Remove(_) => t("Remove instance? Configuration is kept."),
            Self::ForceStart(_) => t("Force start and replace previous sessions?"),
            Self::Shutdown => t("Stop all instances?"),
            Self::Discard => t("Discard changes?"),
            Self::Import => t("Import config or proxy share links"),
            Self::UpdatePackage => t("Local update package path (.zip, .tar.gz or directory)"),
            Self::UpdateConfirm(_) => t("Apply this exact update target?"),
            Self::ImportNode(_) => t("Import node files, TOML or share links"),
            Self::ImportNodeSelection { .. } => t("Select nodes to import"),
            Self::ImportRule(_) => t("Import rules TOML"),
            Self::ExportConfig { .. } => t("Export configuration to path"),
            Self::ExportNode { .. } => t("Export nodes to file or folder (missing folders are created)"),
            Self::ExportNodeDestination { .. } => t("Export selected nodes"),
            Self::ExportRule { .. } => t("Export selected rule to path"),
            Self::ExportTerminal { .. } => t("Export terminal proxy commands to path"),
            Self::ImportConfirm { .. } => t("Replace editor draft?"),
            Self::Explain(_) => t("Enter host[:port] [tcp|udp] [process name or path]"),
            Self::CloseAll { .. } => {
                t("Close all connections? Applications reconnect on their own.")
            }
            Self::ElevateTun { .. } => {
                t("Enable TUN? sudo asks for your password; the current core keeps running until the replacement is ready.")
            }
        }
    }
}

#[derive(Clone, Copy)]
pub enum Operation {
    UpdateSubscriptions,
    Start,
    ForceStart,
    Stop,
    Restart,
    Reload,
    Edit,
    Logs,
    Bind,
    Config,
    Copy,
    Remove,
    Diagnostics,
    NetTools,
    Explain,
    ExportConfig,
    ExportTerminal,
}
impl Operation {
    pub fn label(self) -> &'static str {
        t(match self {
            Self::Start => "Start",
            Self::ForceStart => "Force start",
            Self::Stop => "Stop",
            Self::Restart => "Restart",
            Self::Reload => "Reload",
            Self::UpdateSubscriptions => "Update subscriptions",
            Self::Edit => "Edit",
            Self::Logs => "Logs",
            Self::Bind => "Startup bindings",
            Self::Config => "Configuration path",
            Self::Copy => "Copy configuration",
            Self::Remove => "Remove",
            Self::Diagnostics => "Connectivity / Exit IP",
            Self::NetTools => "Network tools",
            Self::Explain => "Route explanation",
            Self::ExportConfig => "Export config",
            Self::ExportTerminal => "Copy terminal proxy commands to clipboard · F4",
        })
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub enum Resume {
    Prompt(Prompt, String),
    Request(Action),
    Session,
    Editor,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    pub message: String,
    pub resume: Resume,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    #[serde(default)]
    pub gui_windows: Vec<usize>,
    #[serde(default)]
    pub session_version: u32,
    #[serde(default)]
    pub keymap_version: u32,
    #[serde(default)]
    pub keymap_notice_pending: bool,
    #[serde(default)]
    pub drafts: HashMap<String, drafts::Draft>,
    #[serde(default)]
    pub active_profile: Option<String>,
    #[serde(default)]
    pub searches: HashMap<usize, String>,
    #[serde(default)]
    pub input_cursor: usize,
    #[serde(default)]
    pub motion: crate::motion::MotionMode,
    #[serde(default)]
    pub vim_keys: bool,
    pub kind: Kind,
    #[serde(deserialize_with = "read_selections")]
    pub selections: [usize; 3],
    pub editor: Option<Editor>,
    pub prompt: Option<Prompt>,
    pub input: String,
    pub recovery: Option<Recovery>,
    #[serde(default = "launch_directory")]
    pub working_dir: PathBuf,
    #[serde(default)]
    pub log_settings: HashMap<String, logs::Settings>,
    #[serde(default)]
    pub active_log: Option<String>,
    #[serde(default)]
    pub external_draft: Option<crate::external_editor::Draft>,
}
pub fn launch_directory() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}
impl Default for Session {
    fn default() -> Self {
        Self {
            gui_windows: Vec::new(),
            session_version: 1,
            keymap_version: drafts::KEYMAP_VERSION,
            keymap_notice_pending: false,
            drafts: HashMap::new(),
            active_profile: None,
            searches: HashMap::new(),
            input_cursor: 0,
            motion: Default::default(),
            vim_keys: false,
            kind: Kind::Proxy,
            selections: [0; 3],
            editor: None,
            prompt: None,
            input: String::new(),
            recovery: None,
            working_dir: launch_directory(),
            log_settings: HashMap::new(),
            active_log: None,
            external_draft: None,
        }
    }
}
pub const SESSION_LIMIT: u64 = 4 * 1024 * 1024;
pub fn read_selections<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<[usize; 3], D::Error> {
    let old = Vec::<usize>::deserialize(deserializer)?;
    if !(2..=3).contains(&old.len()) {
        return Err(serde::de::Error::custom(
            "Invalid saved instance selections; original session is retained",
        ));
    }
    let mut result = [0; 3];
    result[..old.len()].copy_from_slice(&old);
    Ok(result)
}
pub fn kind_index(kind: Kind) -> usize {
    match kind {
        Kind::Proxy => 0,
        Kind::Rule => 1,
        Kind::Server => 2,
    }
}
pub fn next_kind(kind: Kind) -> Kind {
    match kind {
        Kind::Proxy => Kind::Rule,
        Kind::Rule => Kind::Server,
        Kind::Server => Kind::Proxy,
    }
}
impl Session {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        use std::io::Read;
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(SESSION_LIMIT + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= SESSION_LIMIT,
            "Saved session exceeds 4 MiB"
        );
        let mut session: Self = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            session.session_version <= 1,
            "Saved session version is newer than this TUI; original file is retained: {}",
            path.display()
        );
        anyhow::ensure!(
            session.keymap_version <= drafts::KEYMAP_VERSION,
            "Saved keymap version is newer than this TUI; original file is retained: {}",
            path.display()
        );
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let needs_migration = session.session_version == 0
            || session.keymap_version < drafts::KEYMAP_VERSION
            || value
                .get("selections")
                .and_then(|v| v.as_array())
                .is_some_and(|v| v.len() == 2);
        if needs_migration {
            use astra_platform::platform::OpenOptionsExt;
            use std::io::Write;
            let backup = path.with_extension("pre-v2.json");
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&backup)
            {
                Ok(mut output) => {
                    astra_platform::platform::set_private(&backup, 0o600)?;
                    output.write_all(&bytes)?;
                    output.sync_all()?;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    anyhow::ensure!(
                        std::fs::symlink_metadata(&backup)?.is_file(),
                        "Session migration backup is not a regular file: {}",
                        backup.display()
                    );
                }
                Err(error) => return Err(error.into()),
            }
        }
        session.session_version = session.session_version.max(1);
        if let Some(editor) = &mut session.editor {
            editor.normalize();
        }
        for draft in session.drafts.values_mut() {
            draft.editor.normalize();
        }
        if session.keymap_version < drafts::KEYMAP_VERSION {
            if let Some(editor) = &session.editor {
                session.drafts.insert(
                    format!("{}\n", editor.name),
                    drafts::Draft {
                        path: PathBuf::new(),
                        kind: session.kind,
                        editor: editor.clone(),
                        conflict: false,
                        observed_disk: None,
                    },
                );
            }
            session.input_cursor = session.input.chars().count();
            session.keymap_version = drafts::KEYMAP_VERSION;
            session.keymap_notice_pending = true;
        }
        Ok(session)
    }
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        let data = serde_json::to_vec(self)?;
        anyhow::ensure!(data.len() as u64 <= SESSION_LIMIT, "Session exceeds 4 MiB");
        Ok(astra_platform::storage::atomic_write_default(path, &data)?)
    }
}

pub struct Client {
    pub pending_proxy_selection: Option<crate::live::PendingProxySelection>,
    pub monitor: Option<crate::monitor::View>,
    /// Memory composition; present only while a debug view is registered.
    pub memory: Option<crate::memory::View>,
    pub marquee_hold: Option<(Instant, Instant, bool)>,
    pub terminals: crate::settings::TerminalState,
    #[cfg(windows)]
    pub service_change: Option<tokio::task::JoinHandle<std::result::Result<bool, String>>>,
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub startup_context: Option<crate::windows::Context>,
    pub navigation_stack: Vec<usize>,
    pub group_selection: usize,
    pub connection_selection: usize,
    pub traffic_expanded: bool,
    pub traffic_period: crate::traffic::Period,
    pub connection_filter: String,
    pub connection_filtering: bool,
    pub runtime_picker: Option<(String, usize)>,
    pub persistence_paused: bool,
    pub binding_picker: Option<BindingPicker>,
    pub last_find: String,
    pub live_state: crate::live::State,
    pub notice_since: std::cell::RefCell<HashMap<String, Instant>>,
    pub notice_expiry: std::cell::Cell<Option<Instant>>,
    pub installation: crate::installation::State,
    pub update_launch: crate::update::LaunchControl,
    pub maintenance: Box<crate::maintenance::State>,
    pub tasks: crate::tasks::Tasks,
    pub confirmation_accept: bool,
    pub prefilled_input: bool,
    pub program_info: crate::tasks::ProgramInformation,
    pub waker: crate::waker::Waker,
    pub tui_viewer: Option<logs::Viewer>,
    pub runtime_baseline: Option<(
        astra_model::runtime::Snapshot,
        astra_model::runtime::RuntimeTelemetry,
    )>,
    pub runtime_rates_valid: bool,
    pub runtime_received: Option<Instant>,
    pub runtime_telemetry: Option<astra_model::runtime::RuntimeTelemetry>,
    pub session: Session,
    pub update_status: Option<crate::protocol::updates::UpdateStatus>,
    pub update_view: Option<update::View>,
    pub page: usize,
    pub overlay_epoch: u64,
    pub runtime_snapshot: Option<astra_model::runtime::Snapshot>,
    pub runtime_instance: String,
    pub runtime_error: String,
    pub runtime_sample: Instant,
    pub rates: [u64; 2],
    pub direct_rates: [u64; 2],
    pub history: [std::collections::VecDeque<u64>; 2],
    pub rows: Vec<Row>,
    pub message: String,
    pub viewer: Option<logs::Viewer>,
    pub scroll: u16,
    pub refresh_paused: bool,
    pub details: HashMap<String, InstanceDetails>,
    pub details_supported: bool,
    pub diagnostics: Option<crate::diagnostics::View>,
    /// Exit IPs measured by node tests, keyed by (profile, outbound); shown in the header.
    pub exit_ips: HashMap<(String, String), std::net::IpAddr>,
    /// Background exit IP and TCP latency rounds for the header.
    pub exit_probe: crate::exit_probe::Prober,
    pub open_nettools: bool,
    pub explanation: Option<crate::explanation::View>,
    pub help_scroll: Option<u16>,
    pub help_override: Option<String>,
    pub operation_failed: bool,
    pub ready_editors: HashMap<String, Editor>,
    pub search_on_open: bool,
    pub preferences: crate::preferences::Preferences,
    pub preferences_path: Option<PathBuf>,
}
impl Client {
    pub fn new(kind: Kind) -> Self {
        Self::from_session(Session {
            kind,
            ..Default::default()
        })
    }
    pub fn from_session(session: Session) -> Self {
        let migration = session.keymap_notice_pending;
        Self {
            monitor: None,
            memory: None,
            pending_proxy_selection: None,
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            startup_context: None,
            #[cfg(windows)]
            service_change: None,
            terminals: Default::default(),
            marquee_hold: None,
            installation: Default::default(),
            update_launch: Default::default(),
            maintenance: Default::default(),
            navigation_stack: Vec::new(),
            group_selection: 0,
            connection_selection: 0,
            traffic_expanded: false,
            traffic_period: Default::default(),
            connection_filter: String::new(),
            connection_filtering: false,
            runtime_picker: None,
            persistence_paused: false,
            binding_picker: None,
            last_find: String::new(),
            live_state: Default::default(),
            notice_since: Default::default(),
            notice_expiry: Default::default(),

            confirmation_accept: false,
            prefilled_input: false,
            program_info: Default::default(),
            tasks: Default::default(),
            waker: Default::default(),
            tui_viewer: None,
            runtime_baseline: None,
            runtime_rates_valid: false,
            runtime_received: None,
            runtime_telemetry: None,
            session,
            update_status: None,
            update_view: None,
            page: 0,
            overlay_epoch: 0,
            runtime_snapshot: None,
            runtime_instance: String::new(),
            runtime_error: String::new(),
            runtime_sample: Instant::now(),
            rates: [0, 0],
            direct_rates: [0, 0],
            history: Default::default(),
            rows: vec![],
            message: String::new(),
            viewer: None,
            scroll: 0,
            refresh_paused: false,
            details: HashMap::new(),
            details_supported: false,
            diagnostics: None,
            exit_ips: HashMap::new(),
            exit_probe: Default::default(),
            open_nettools: false,
            explanation: None,
            help_scroll: migration.then_some(0),
            help_override: migration.then(|| t("Keyboard layout updated\n\nTab / Shift-Tab now moves focus. Enter activates the focused item.\nSpace pauses connections and logs, or toggles a checkbox.\nF2 / Ctrl-K opens every command. Q, R and 0 no longer run destructive actions.\nEsc, page changes and quitting retain configuration drafts.\n\nF1 opens the complete key map. Enter / Esc dismisses this notice.").into()),
            operation_failed: false,
            ready_editors: HashMap::new(),
            search_on_open: false,
            preferences: Default::default(),
            preferences_path: None,

        }
    }
    pub fn index(&self) -> usize {
        kind_index(self.kind)
    }
    pub fn rows(&self) -> Vec<&Row> {
        self.rows
            .iter()
            .filter(|r| r.instance.kind == self.kind)
            .collect()
    }
    pub fn selected(&self) -> Option<&Row> {
        self.rows().get(self.selections[self.index()]).copied()
    }
    pub fn update_rows(&mut self, reply: Reply) {
        let details = serde_json::from_str(&reply.text);
        self.details_supported = details.is_ok();
        self.details = details.unwrap_or_default();
        self.rows = reply.rows;
        if self.active_profile.is_none() {
            self.active_profile = self
                .rows
                .iter()
                .filter(|r| r.instance.kind == Kind::Proxy)
                .nth(self.selections[0])
                .or_else(|| self.rows.iter().find(|r| r.instance.kind == Kind::Proxy))
                .map(|r| r.instance.name.clone());
        }
        for (index, kind) in [Kind::Proxy, Kind::Rule, Kind::Server]
            .into_iter()
            .enumerate()
        {
            let count = self
                .rows
                .iter()
                .filter(|row| row.instance.kind == kind)
                .count();
            self.selections[index] = self.selections[index].min(count.saturating_sub(1));
        }
    }
    pub fn fail(&mut self, error: impl std::fmt::Display, resume: Resume) {
        self.operation_failed = true;
        self.recovery = Some(Recovery {
            message: error.to_string(),
            resume,
        });
        self.scroll = 0;
    }
}
impl std::ops::Deref for Client {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.session
    }
}
impl std::ops::DerefMut for Client {
    fn deref_mut(&mut self) -> &mut Session {
        &mut self.session
    }
}
impl Client {
    pub fn draft_key(&self, name: &str) -> (String, PathBuf) {
        let path = self
            .rows
            .iter()
            .find(|r| r.instance.name == name)
            .map(|r| r.instance.config.clone())
            .unwrap_or_default();
        let key = format!("{}\n{}", name, path.to_string_lossy());
        (key, path)
    }
    pub fn remember_draft(&mut self) {
        let Some(editor) = self.editor.clone() else {
            return;
        };
        let (key, path) = self.draft_key(&editor.name);
        let conflict = self.drafts.get(&key).is_some_and(|draft| draft.conflict);
        let observed_disk = self
            .drafts
            .get(&key)
            .and_then(|draft| draft.observed_disk.clone());
        let kind = self
            .rows
            .iter()
            .find(|r| r.instance.name == editor.name)
            .map_or(self.kind, |r| r.instance.kind);
        self.drafts.insert(
            key,
            drafts::Draft {
                path,
                kind,
                editor,
                conflict,
                observed_disk,
            },
        );
    }
    pub fn suspend_editor(&mut self) {
        if self.editor.is_some() {
            self.remember_draft();
            self.editor = None;
            self.message = t("Draft retained · reopen the configuration to continue").into();
        }
    }
    pub fn discard_current_draft(&mut self) {
        if let Some(editor) = self.editor.take() {
            let (key, _) = self.draft_key(&editor.name);
            self.drafts.remove(&key);
            self.ready_editors.remove(&editor.name);
        }
    }
    pub fn compare_draft(&mut self) {
        self.remember_draft();
        let Some(editor) = &self.editor else {
            return;
        };
        let (key, _) = self.draft_key(&editor.name);
        let Some(disk) = self
            .drafts
            .get(&key)
            .and_then(|draft| draft.observed_disk.as_ref())
        else {
            self.message = t("No external conflict was observed when this draft was opened").into();
            return;
        };
        let draft = editor.text();
        let disk_lines: Vec<_> = disk.lines().collect();
        let draft_lines: Vec<_> = draft.lines().collect();
        let mut text=format!("{} · {}\n{}\n\n",t("Draft conflict"),editor.name,t("Disk snapshot from the latest conflict check. - disk / + draft; no source file is changed."));
        let count = disk_lines.len().max(draft_lines.len());
        for index in 0..count.min(2000) {
            match (disk_lines.get(index), draft_lines.get(index)) {
                (Some(a), Some(b)) if a == b => text.push_str(&format!("  {a}\n")),
                (a, b) => {
                    if let Some(a) = a {
                        text.push_str(&format!("- {a}\n"));
                    }
                    if let Some(b) = b {
                        text.push_str(&format!("+ {b}\n"));
                    }
                }
            }
        }
        if count > 2000 {
            text.push_str(
                "\nPreview limited to 2000 lines; use the external editor for the complete file.\n",
            );
        }
        self.help_override = Some(
            text.chars()
                .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                .collect(),
        );
        self.help_scroll = Some(0);
    }
    pub fn active_proxy(&self) -> Option<&Row> {
        if self.kind == Kind::Server {
            return None;
        }
        if let Some(name) = &self.active_profile {
            self.rows
                .iter()
                .find(|r| r.instance.kind == Kind::Proxy && &r.instance.name == name)
        } else {
            self.rows
                .iter()
                .filter(|r| r.instance.kind == Kind::Proxy)
                .nth(self.selections[0])
        }
    }
    pub fn apply_pending_search(&mut self) {
        if !self.search_on_open {
            return;
        }
        if self.editor.as_ref().is_some_and(Editor::browsing) {
            self.search_on_open = false;
            if let Some(editor) = self.editor.as_mut() {
                editor.begin_search();
            }
        }
    }
}

impl Client {
    pub fn with_waker(mut self, waker: crate::waker::Waker) -> Self {
        self.waker = waker;
        self
    }
    pub fn reset_runtime_identity(&mut self) {
        self.pending_proxy_selection = None;
        self.live_state = Default::default();
        self.connection_selection = 0;
        self.runtime_instance.clear();
        self.runtime_snapshot = None;
        self.runtime_telemetry = None;
        self.runtime_baseline = None;
        self.runtime_rates_valid = false;
        self.runtime_received = None;
        self.runtime_sample = Instant::now();
        self.runtime_error.clear();
        self.history = Default::default();
        self.rates = [0; 2];
        self.direct_rates = [0; 2];
    }
    pub fn runtime_disconnected(&mut self, error: String) {
        self.runtime_rates_valid = false;
        self.runtime_baseline = None;
        self.runtime_error = error;
    }
    pub fn accept_runtime(&mut self, name: String, snapshot: astra_model::runtime::Snapshot) {
        self.accept_runtime_view(
            name,
            astra_model::runtime::RuntimeView {
                snapshot,
                telemetry: None,
            },
        );
    }
    pub fn accept_runtime_view(&mut self, name: String, view: astra_model::runtime::RuntimeView) {
        use astra_model::runtime::{Coverage, RuntimeView};
        let RuntimeView {
            snapshot,
            telemetry,
        } = view;
        let identity_changed = self.runtime_instance != name
            || self
                .runtime_snapshot
                .as_ref()
                .is_some_and(|old| old.pid != snapshot.pid)
            || telemetry
                .as_ref()
                .zip(self.runtime_telemetry.as_ref())
                .is_some_and(|(new, old)| new.host_boot_id != old.host_boot_id);
        if identity_changed {
            if self.runtime_snapshot.is_some() && self.runtime_instance == name {
                self.message =
                    t("Host restarted; previous connection targets and paused data were cleared")
                        .into();
                self.tasks.notice_message(&self.message);
            }
            self.reset_runtime_identity();
        }
        // An immediate command result is not a sample from the host clock.
        if telemetry.is_none() && self.runtime_telemetry.is_some() && !identity_changed {
            self.reconcile_proxy_selection(&name, &snapshot);
            self.track_new_connections(&snapshot);
            self.runtime_instance = name;
            self.runtime_snapshot = Some(snapshot);
            self.sync_connection_selection();
            return;
        }
        if let Some(meta) = telemetry {
            if self
                .runtime_baseline
                .as_ref()
                .is_some_and(|(_, old)| meta.sample_sequence < old.sample_sequence)
            {
                return;
            }
            let valid = meta.coverage == Coverage::Complete && meta.sample_age_ms <= 3000;
            let fresh = self
                .runtime_baseline
                .as_ref()
                .is_none_or(|(_, old)| old.sample_sequence != meta.sample_sequence);
            if fresh {
                self.runtime_rates_valid = false;
                let previous = self
                    .runtime_baseline
                    .as_ref()
                    .filter(|(_, old)| {
                        old.host_boot_id == meta.host_boot_id && old.coverage == Coverage::Complete
                    })
                    .cloned();
                if let Some((old, old_meta)) = previous.filter(|_| valid) {
                    let elapsed = meta
                        .sample_elapsed_ms
                        .saturating_sub(old_meta.sample_elapsed_ms)
                        as f64
                        / 1000.0;
                    if elapsed > 0.0 {
                        self.set_runtime_rates(&old, &snapshot, elapsed);
                        self.runtime_rates_valid = true;
                        self.record_runtime_rates();
                    }
                }
                self.runtime_baseline = Some((snapshot.clone(), meta.clone()));
            }
            if !valid {
                self.runtime_rates_valid = false;
            }
            self.runtime_telemetry = Some(meta);
        } else {
            self.accept_legacy_rates(&name, &snapshot);
        }
        self.runtime_received = Some(Instant::now());
        self.runtime_sample = Instant::now();
        self.reconcile_proxy_selection(&name, &snapshot);
        self.track_new_connections(&snapshot);
        self.runtime_instance = name;
        self.runtime_snapshot = Some(snapshot);
        self.group_selection = self
            .group_selection
            .min(self.live().map_or(0, |s| s.groups.len().saturating_sub(1)));
        if self.live_state.selected.is_none() && self.live_state.selected_target.is_none() {
            self.select_connection(0);
        }
        self.sync_connection_selection();
        self.runtime_error.clear();
    }
    pub fn runtime_rates_available(&self) -> bool {
        self.runtime_snapshot.is_some()
            && self.runtime_rates_valid
            && self.runtime_error.is_empty()
            && self
                .runtime_received
                .is_some_and(|at| at.elapsed() <= std::time::Duration::from_secs(3))
            && self.runtime_telemetry.as_ref().is_none_or(|meta| {
                meta.sample_age_ms.saturating_add(
                    self.runtime_received
                        .map_or(0, |at| at.elapsed().as_millis() as u64),
                ) <= 3000
            })
    }
    pub fn set_runtime_rates(
        &mut self,
        old: &astra_model::runtime::Snapshot,
        snapshot: &astra_model::runtime::Snapshot,
        elapsed: f64,
    ) {
        self.rates = [
            (snapshot.proxy_upload.saturating_sub(old.proxy_upload) as f64 / elapsed) as u64,
            (snapshot.proxy_download.saturating_sub(old.proxy_download) as f64 / elapsed) as u64,
        ];
        self.direct_rates = [
            (snapshot
                .upload
                .saturating_sub(snapshot.proxy_upload)
                .saturating_sub(old.upload.saturating_sub(old.proxy_upload)) as f64
                / elapsed) as u64,
            (snapshot
                .download
                .saturating_sub(snapshot.proxy_download)
                .saturating_sub(old.download.saturating_sub(old.proxy_download))
                as f64
                / elapsed) as u64,
        ];
    }
    pub fn record_runtime_rates(&mut self) {
        for index in 0..2 {
            self.history[index].push_back(self.rates[index]);
            if self.history[index].len() > 64 {
                self.history[index].pop_front();
            }
        }
    }
    pub fn accept_legacy_rates(&mut self, name: &str, snapshot: &astra_model::runtime::Snapshot) {
        let previous = self
            .runtime_snapshot
            .as_ref()
            .filter(|old| name == self.runtime_instance && old.pid == snapshot.pid);
        let elapsed = self.runtime_sample.elapsed().as_secs_f64();
        self.direct_rates = if let Some(old) = previous {
            if elapsed > 0.1 {
                [
                    (snapshot
                        .upload
                        .saturating_sub(snapshot.proxy_upload)
                        .saturating_sub(old.upload.saturating_sub(old.proxy_upload))
                        as f64
                        / elapsed) as u64,
                    (snapshot
                        .download
                        .saturating_sub(snapshot.proxy_download)
                        .saturating_sub(old.download.saturating_sub(old.proxy_download))
                        as f64
                        / elapsed) as u64,
                ]
            } else {
                self.direct_rates
            }
        } else {
            self.history = Default::default();
            [0, 0]
        };
        self.rates = if let Some(old) = previous {
            if elapsed > 0.1 {
                [
                    (snapshot.proxy_upload.saturating_sub(old.proxy_upload) as f64 / elapsed)
                        as u64,
                    (snapshot.proxy_download.saturating_sub(old.proxy_download) as f64 / elapsed)
                        as u64,
                ]
            } else {
                self.rates
            }
        } else {
            self.history = Default::default();
            [0, 0]
        };
        for index in 0..2 {
            self.history[index].push_back(self.rates[index]);
            if self.history[index].len() > 64 {
                self.history[index].pop_front();
            }
        }
        self.runtime_sample = Instant::now();
        self.runtime_rates_valid = previous.is_some();
    }
}

impl Client {
    pub fn command_available(&self, id: Id) -> Option<&'static str> {
        let scope = SPECS
            .iter()
            .find(|spec| spec.id == id)
            .map_or(Scope::Global, |spec| spec.scope);
        if scope == Scope::Editor && self.editor.is_none() {
            return Some("Open a configuration first");
        }
        if scope == Scope::Live && !matches!(self.page, 0 | 2 | 4) {
            return Some("Open connections or logs first");
        }
        match id {
            Id::CopyTerminal if self.prompt.is_some() => {
                Some("Finish or cancel the current prompt first")
            }
            Id::CopyTerminal if self.terminal_export_profile().is_none() => Some("No profile yet"),
            Id::ReadExternal | Id::Import
                if self
                    .editor
                    .as_ref()
                    .is_some_and(|editor| !editor.browsing()) =>
            {
                Some("Confirm or cancel the field before importing changes")
            }
            Id::Save
                if self
                    .update_status
                    .as_ref()
                    .and_then(|status| status.operation.as_ref())
                    .is_some_and(|operation| !operation.phase.terminal()) =>
            {
                Some("Update running or requires recovery; draft retained, retry save afterward")
            }
            Id::Profiles if self.prompt.is_some() => {
                Some("Finish or cancel the current prompt first")
            }
            _ => None,
        }
    }
    pub fn terminal_export_profile(&self) -> Option<&Row> {
        if self.page == 5 && self.kind == Kind::Proxy {
            self.selected()
        } else {
            self.active_proxy()
        }
    }
}

impl Client {
    pub fn prepare_external(
        &mut self,
        home: &std::path::Path,
    ) -> Result<(crate::external_editor::Draft, PathBuf)> {
        let editor = self
            .editor
            .as_mut()
            .context(t("Open a configuration first"))?;
        let text = editor.prepare_save()?;
        let name = editor.name.clone();
        if let Some(previous) = &self.external_draft {
            if previous.name == name && previous.base != text && previous.path.is_file() {
                anyhow::ensure!(
                    previous.read()? == previous.base,
                    "{}: {}",
                    t("TUI draft changed; external file is kept for manual import"),
                    previous.path.display()
                );
            }
        }
        let draft = match &self.external_draft {
            Some(draft) if draft.name == name && draft.base == text && draft.path.is_file() => {
                draft.clone()
            }
            _ => crate::external_editor::Draft::create(home, name.clone(), text)?,
        };
        self.external_draft = Some(draft.clone());
        self.message = tr!(
            "External draft: {} · F2 → Read external draft · Ctrl-S save",
            draft.path.display()
        );
        let directory = self
            .rows
            .iter()
            .find(|row| row.instance.name == name)
            .and_then(|row| row.instance.config.parent())
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| self.working_dir.clone());
        Ok((draft, directory))
    }
    pub fn read_external(&mut self) -> Result<()> {
        let draft = self
            .external_draft
            .clone()
            .context(t("No external draft; use F2 → Open external editor"))?;
        let text = draft.read()?;
        let editor = self
            .editor
            .as_mut()
            .context(t("Open a configuration first"))?;
        anyhow::ensure!(
            editor.name == draft.name,
            t("External draft belongs to another instance")
        );
        anyhow::ensure!(
            editor.text() == draft.base || editor.text() == text,
            t("TUI draft changed; external file is kept for manual import")
        );
        let path = draft.path.clone();
        if editor.text() != text {
            editor.import(text.clone())?;
        }
        self.external_draft.as_mut().unwrap().base = text;
        self.message = tr!(
            "External draft: {} · F2 → Read external draft · Ctrl-S save",
            path.display()
        );
        Ok(())
    }
    pub fn prompt(&mut self, prompt: Prompt) {
        self.confirmation_accept = false;
        if matches!(prompt, Prompt::ExportNode { .. }) {
            match astra_platform::platform::documents_dir() {
                Ok(directory) => self.working_dir = directory,
                Err(error) => {
                    self.fail(
                        format!("Locate Documents directory: {error}"),
                        Resume::Editor,
                    );
                    return;
                }
            }
        } else if matches!(
            prompt,
            Prompt::AddName
                | Prompt::Config(_)
                | Prompt::Clone(_)
                | Prompt::ImportNode(_)
                | Prompt::ImportRule(_)
                | Prompt::UpdatePackage
                | Prompt::ExportConfig { .. }
                | Prompt::ExportRule { .. }
                | Prompt::ExportTerminal { .. }
        ) {
            self.working_dir = launch_directory();
        }
        self.input = match &prompt {
            Prompt::Config(name) => self
                .rows
                .iter()
                .find(|row| &row.instance.name == name)
                .map(|row| row.instance.config.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Prompt::Clone(name) => self
                .rows
                .iter()
                .find(|row| &row.instance.name == name)
                .map(|row| {
                    let source = &row.instance.config;
                    let stem = source.file_stem().unwrap_or_default().to_string_lossy();
                    let mut target = source.with_file_name(format!("{stem}-copy.toml"));
                    for index in 2..=100 {
                        if !target.exists() {
                            break;
                        }
                        target = source.with_file_name(format!("{stem}-copy-{index}.toml"));
                    }
                    target.to_string_lossy().into_owned()
                })
                .unwrap_or_default(),
            Prompt::ExportNode { share, .. } => self
                .working_dir
                .join(if *share { "nodes.txt" } else { "nodes.toml" })
                .to_string_lossy()
                .into_owned(),
            Prompt::ExportConfig { .. }
            | Prompt::ExportRule { .. }
            | Prompt::ExportTerminal { .. } => String::new(),
            Prompt::Find(page) => self.searches.get(page).cloned().unwrap_or_default(),
            _ => String::new(),
        };
        self.prefilled_input = !self.input.is_empty();
        self.input_cursor = self.input.chars().count();
        self.prompt = Some(prompt);
    }
    pub fn paste_prompt(&mut self, text: &str) {
        if self.prompt.as_ref().is_some_and(Prompt::picker) {
            return;
        }
        if self.prefilled_input {
            self.input.clear();
            self.input_cursor = 0;
            self.prefilled_input = false;
        }
        let multiline = matches!(self.prompt, Some(Prompt::ImportNode(_)))
            || matches!(self.prompt, Some(Prompt::Import)) && astra_convert::share::is_share(text);
        let inserted: String = text
            .chars()
            .filter(|character| {
                !character.is_control() || (multiline && matches!(*character, '\n' | '\r' | '\t'))
            })
            .collect();
        let byte = self.prompt_byte();
        self.input.insert_str(byte, &inserted);
        self.input_cursor += inserted.chars().count();
    }
    pub fn prompt_byte(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.input_cursor)
            .map_or(self.input.len(), |(index, _)| index)
    }
}

/// Model decisions are shared; window/overlay presentation is returned to the frontend.
#[derive(Clone)]
pub enum Input {
    Idle,
    Quit,
    Run(Action, Option<Box<(Prompt, String)>>),
    ExternalEditor,
    Frontend(Id),
    OpenWindow(crate::windows::Launch),
    Install(crate::installation::Request),
}
impl Client {
    pub fn run_command(&mut self, id: Id) -> Input {
        if let Some(reason) = self.command_available(id) {
            self.message = t(reason).into();
            return Input::Idle;
        }
        match id {
            Id::Save => {
                let editor = self.editor.as_mut().expect("availability checks editor");
                match editor.prepare_save() {
                    Ok(text) => Input::Run(
                        Action::Save {
                            name: editor.name.clone(),
                            original: editor.original.clone(),
                            text,
                        },
                        None,
                    ),
                    Err(error) => {
                        self.fail(format!("{error:#}"), Resume::Editor);
                        Input::Idle
                    }
                }
            }
            Id::Undo | Id::Redo => {
                if let Some(editor) = self.editor.as_mut().filter(|editor| editor.browsing()) {
                    if let Err(error) = editor.restore(id == Id::Redo) {
                        self.fail(format!("{error:#}"), Resume::Editor);
                    }
                }
                Input::Idle
            }
            Id::Quit => {
                self.remember_draft();
                Input::Quit
            }
            Id::Shutdown => {
                self.prompt(Prompt::Shutdown);
                Input::Idle
            }
            Id::Import => {
                self.prompt(Prompt::Import);
                Input::Idle
            }
            Id::Discard => {
                self.prompt(Prompt::Discard);
                Input::Idle
            }
            Id::CompareDraft => {
                self.compare_draft();
                Input::Idle
            }
            Id::ExternalEditor => Input::ExternalEditor,
            Id::ReadExternal => {
                if let Err(error) = self.read_external() {
                    self.fail(format!("{error:#}"), Resume::Editor);
                }
                Input::Idle
            }
            Id::CopyTerminal => self.copy_terminal_commands(),
            Id::Search => self.search_current(),
            Id::NextMatch | Id::PreviousMatch => self.find_next(id == Id::PreviousMatch),
            Id::Window => self.detach_current(),
            Id::Pause => {
                if let Some(viewer) = &mut self.viewer {
                    viewer.toggle_pause();
                    Input::Idle
                } else {
                    self.toggle_live_pause()
                }
            }
            Id::VimKeys => {
                if self.change_preferences(|p| p.vim_keys = !p.vim_keys) {
                    self.message = if self.vim_keys {
                        "Vim aliases enabled"
                    } else {
                        "Vim aliases disabled"
                    }
                    .into();
                }
                Input::Idle
            }
            Id::MotionFull | Id::MotionReduced | Id::MotionOff => {
                let mode = match id {
                    Id::MotionFull => crate::motion::MotionMode::Normal,
                    Id::MotionReduced => crate::motion::MotionMode::Reduced,
                    _ => crate::motion::MotionMode::Off,
                };
                self.change_preferences(|p| p.motion = mode);
                Input::Idle
            }
            Id::Settings => self.navigate(crate::navigation::SETTINGS),
            Id::Palette
            | Id::Help
            | Id::Profiles
            | Id::Tasks
            | Id::Logs
            | Id::Monitor
            | Id::Debug => Input::Frontend(id),
        }
    }
}

#[cfg(test)]
#[path = "test/state_runtime_test.rs"]
mod runtime_tests;

pub struct BindingPicker {
    pub name: String,
    pub choices: Vec<(String, Kind)>,
    pub selected: usize,
    pub targets: Vec<String>,
}
