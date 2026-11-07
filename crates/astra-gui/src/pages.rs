//! Native navigation titles; behavior lives in astra-client.
use astra_client::i18n::t;
pub struct Page {
    pub title: &'static str,
}
pub const PAGES: [Page; 7] = [
    Page { title: "Overview" },
    Page { title: "Proxies" },
    Page {
        title: "Connections",
    },
    Page { title: "Rules" },
    Page { title: "Logs" },
    Page { title: "Profiles" },
    Page { title: "Settings" },
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
}

#[cfg(test)]
#[path = "test/pages_test.rs"]
mod tests;
