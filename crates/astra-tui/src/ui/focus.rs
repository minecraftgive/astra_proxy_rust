//! Region focus is independent of list selection and the active profile.
use super::*;
use shell::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Region {
    #[default]
    Main,
    Secondary,
    Controls,
    Header,
    Tabs,
    Tasks,
}
#[derive(Default)]
pub(super) struct Focus {
    pub(super) region: Region,
    pub(super) cursor: usize,
}
impl Region {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Main => "Content",
            Self::Secondary => "Details / members",
            Self::Controls => "Controls",
            Self::Header => "Profile",
            Self::Tabs => "Pages",
            Self::Tasks => "Tasks",
        }
    }
}
impl Ui {
    pub(super) fn focus_border(&self, region: Region, fallback: Color) -> Style {
        Style::default().fg(
            if self.focus.region == region && self.palette.is_none() && self.prompt.is_none() {
                Color::Cyan
            } else {
                fallback
            },
        )
    }
    pub(super) fn focused_card<'a>(&self, title: &'a str, region: Region) -> Block<'a> {
        super::dashboard::card(title).border_style(self.focus_border(region, Color::DarkGray))
    }
    pub(super) fn focus_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        if self.live_detail_open()
            || self.prompt.is_some()
            || self.recovery.is_some()
            || self.menu.is_some()
            || self.binding_picker.is_some()
            || self.diagnostics.is_some()
            || self.explanation.is_some()
            || self.help_scroll.is_some()
            || self.palette.is_some()
            || self.connection_filtering
            || self.editor.as_ref().is_some_and(|e| !e.browsing())
            || self.viewer.as_ref().is_some_and(|v| !v.browsing())
        {
            return None;
        }
        let regions: &[Region] = if self.editor.is_some() {
            &[Region::Main, Region::Controls, Region::Tasks]
        } else if matches!(self.page, PROXIES | CONNECTIONS | OVERVIEW) {
            &[
                Region::Main,
                Region::Secondary,
                Region::Controls,
                Region::Header,
                Region::Tabs,
                Region::Tasks,
            ]
        } else if self.page == PROFILES {
            &[
                Region::Main,
                Region::Controls,
                Region::Header,
                Region::Tabs,
                Region::Tasks,
            ]
        } else {
            &[Region::Main, Region::Header, Region::Tabs, Region::Tasks]
        };
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            let index = regions
                .iter()
                .position(|r| *r == self.focus.region)
                .unwrap_or(0);
            let next = if key.code == KeyCode::BackTab {
                (index + regions.len() - 1) % regions.len()
            } else {
                (index + 1) % regions.len()
            };
            self.focus.region = regions[next];
            self.focus.cursor = if self.focus.region == Region::Tabs {
                self.page
            } else {
                0
            };
            if self.page == PROXIES {
                if self.focus.region == Region::Secondary {
                    self.open_runtime_group();
                } else {
                    self.runtime_picker = None;
                }
            }
            return Some(Input::Idle);
        }
        if self.focus.region == Region::Main {
            return None;
        }
        if key.code == KeyCode::Esc {
            self.focus = Default::default();
            self.runtime_picker = None;
            return Some(Input::Idle);
        }
        if self.focus.region == Region::Secondary {
            if self.page == PROXIES {
                return None;
            }
            return self.live_secondary_key(key);
        }
        let count = match self.focus.region {
            Region::Tabs => PAGES.len(),
            Region::Controls if self.editor.is_some() => 3,
            Region::Controls if self.page == PROFILES => 3,
            Region::Controls if self.page == OVERVIEW && self.active_proxy().is_some() => {
                if self.live().is_some() {
                    8
                } else {
                    2
                }
            }
            Region::Controls => 6,
            _ => 1,
        };
        self.focus.cursor = self.focus.cursor.min(count - 1);
        match key.code {
            KeyCode::Left | KeyCode::Up => {
                self.focus.cursor = self.focus.cursor.saturating_sub(1);
                Some(Input::Idle)
            }
            KeyCode::Right | KeyCode::Down => {
                self.focus.cursor = (self.focus.cursor + 1).min(count - 1);
                Some(Input::Idle)
            }
            KeyCode::Enter | KeyCode::Char(' ') => Some(match self.focus.region {
                Region::Tabs => {
                    let page = self.focus.cursor;
                    self.suspend_editor();
                    self.focus = Default::default();
                    self.navigate(page)
                }
                Region::Header => self.run_command(commands::Id::Profiles),
                Region::Tasks => self.run_command(commands::Id::Tasks),
                Region::Controls if self.editor.is_some() => {
                    self.run_command(match self.focus.cursor {
                        0 => commands::Id::Save,
                        1 => commands::Id::Import,
                        _ => commands::Id::Discard,
                    })
                }
                Region::Controls if self.page == PROFILES => {
                    self.switch_core(
                        [Kind::Proxy, Kind::Rule, Kind::Server][self.focus.cursor.min(2)],
                    );
                    Input::Idle
                }
                Region::Controls => {
                    use astra_model::runtime::Command;
                    use astra_model::runtime::Mode;
                    let runtime_count = if self.live().is_some() { 6 } else { 0 };
                    if self.page == OVERVIEW
                        && self.active_proxy().is_some()
                        && self.focus.cursor >= runtime_count
                    {
                        return Some(if self.focus.cursor == runtime_count {
                            self.active_operation(Operation::NetTools)
                        } else {
                            self.toggle_active()
                        });
                    }
                    let snapshot = self.live()?;
                    let command = match self.focus.cursor {
                        0 => Command::SetMode(Mode::Rule),
                        1 => Command::SetMode(Mode::Global),
                        2 => Command::SetMode(Mode::Direct),
                        3 => Command::SystemProxy(!snapshot.system_proxy),
                        4 => Command::Tun(!snapshot.tun),
                        _ => Command::AllowLan(!snapshot.allow_lan),
                    };
                    self.runtime_action(command)
                }
                _ => Input::Idle,
            }),
            _ => None,
        }
    }
    pub(super) fn focus_label(&self) -> String {
        match self.focus.region {
            Region::Tabs => format!(
                "{}: {}",
                t("Pages"),
                t(PAGES[self.focus.cursor.min(PAGES.len() - 1)])
            ),
            Region::Controls if self.editor.is_some() => {
                t(["Save draft", "Import", "Discard draft"][self.focus.cursor.min(2)]).into()
            }
            Region::Controls if self.page == PROFILES => {
                t(["Proxy profiles", "Rule cores", "Servers"][self.focus.cursor.min(2)]).into()
            }
            Region::Controls
                if self.page == OVERVIEW
                    && self.active_proxy().is_some()
                    && (self.live().is_none() || self.focus.cursor >= 6) =>
            {
                let runtime_count = if self.live().is_some() { 6 } else { 0 };
                t(if self.focus.cursor == runtime_count {
                    "Network tools"
                } else if self.active_proxy().is_some_and(|row| row.pid.is_some()) {
                    "Disconnect profile"
                } else {
                    "Connect profile"
                })
                .into()
            }
            Region::Controls => t([
                "Rule mode",
                "Global mode",
                "Direct mode",
                "System proxy",
                "TUN",
                "Allow LAN",
            ][self.focus.cursor.min(5)])
            .into(),
            other => t(other.label()).into(),
        }
    }
}
