//! Explicit framework views. These lists track every remaining parity requirement.
use astra_client::i18n::t;

pub struct Page {
    pub title: &'static str,
    pub subtitle: &'static str,
    pub pending: &'static [&'static str],
}
pub const PAGES: [Page; 7] = [
    Page {
        title: "Overview",
        subtitle: "Runtime and traffic",
        pending: &[
            "Header card, alerts and connection toggle",
            "Mode, system proxy, TUN and LAN controls",
            "Connection sparkline, proxy groups and expandable traffic panel",
            "Pause live display",
        ],
    },
    Page {
        title: "Proxies",
        subtitle: "Groups and outbound selection",
        pending: &[
            "Group list and member grid with delay chips",
            "Test group and find members",
        ],
    },
    Page {
        title: "Connections",
        subtitle: "Active network sessions",
        pending: &[
            "Virtualized table grouped by destination",
            "Filter and connection details",
            "Close selected connection and confirm close all",
            "Pause live display",
        ],
    },
    Page {
        title: "Rules",
        subtitle: "Routing and explanation",
        pending: &[
            "Rule core status",
            "Route test dialog and explanation view",
            "Edit and search rules",
        ],
    },
    Page {
        title: "Logs",
        subtitle: "Live events and diagnostics",
        pending: &[
            "Virtualized log viewer and source switch",
            "Search n/N, filter, display and capture levels",
            "Memory limit, wrap, follow, reconnect and horizontal scroll",
        ],
    },
    Page {
        title: "Profiles",
        subtitle: "Proxy, rule and server configurations",
        pending: &[
            "Proxy / Rule / Server tabs and profile details",
            "Create, clone, start, stop, restart and force start",
            "Reload, edit, logs, test, bindings, remove, import, export and validate",
            "Operations context menu and confirmation dialogs",
        ],
    },
    Page {
        title: "Settings",
        subtitle: "Appearance, startup and installation",
        pending: &[
            "Language, accent, colour intensity, motion, Vim keys, mouse and start page",
            "Terminal picker and GUI / TUI login startup",
            "Silent startup and Windows privilege service",
            "Installation, secure askpass / UAC and update self-restart",
            "Hidden Ctrl+Home marquee banner",
        ],
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Help,
    Tasks,
    Monitor,
    Diagnostics,
    NetTools,
    Updates,
    Editor,
    Recovery,
    GuiLog,
}
impl Panel {
    pub fn title(self) -> &'static str {
        t(match self {
            Self::Help => "Keyboard help",
            Self::Tasks => "Task details",
            Self::Monitor => "Monitor",
            Self::Diagnostics => "Diagnostics",
            Self::NetTools => "Network tools",
            Self::Updates => "Updates",
            Self::Editor => "Configuration editor",
            Self::Recovery => "Recovery",
            Self::GuiLog => "GUI log",
        })
    }
    pub fn pending(self) -> &'static [&'static str] {
        match self {
            Self::Help => &[],
            Self::Tasks => &["Task event details and abort"],
            Self::Monitor => &["CPU, memory and network charts", "Pause and clear history"],
            Self::Diagnostics => &["Checklist runner, streaming results and abort"],
            Self::NetTools => &["Egress, kind, target, record, server and count form"],
            Self::Updates => &[
                "Import package, status, activate confirmation, rollback and recover",
                "Restart GUI after update",
            ],
            Self::Editor => &[
                "Schema tree: fold, mark, reorder, rename, duplicate and delete with undo",
                "Schema form, member picker and new outbound wizard",
                "Raw TOML code editor and semantic highlighting",
                "Drafts, conflict banner, disk comparison and Ctrl-S save",
                "Import drafts, nodes and rules; export TOML and share links",
                "External editor and shared Vim bindings zM / zR / v / K / J",
            ],
            Self::Recovery => &["Full-window recovery view with back and continue"],
            Self::GuiLog => &["TUI / GUI log source, filter, follow and search"],
        }
    }
}

#[cfg(test)]
#[path = "test/pages_test.rs"]
mod tests;
