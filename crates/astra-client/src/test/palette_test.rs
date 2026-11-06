use super::*;
use crate::protocol::{Instance, Row};
// Frontends own query/cursor state; the shared model receives only query and mode.
#[derive(Default)]
struct Palette {
    query: String,
    profiles: bool,
}
struct Ui {
    client: Client,
    palette: Option<Palette>,
}
impl std::ops::Deref for Ui {
    type Target = Client;
    fn deref(&self) -> &Client {
        &self.client
    }
}
impl std::ops::DerefMut for Ui {
    fn deref_mut(&mut self) -> &mut Client {
        &mut self.client
    }
}
impl Ui {
    fn new(session: Session) -> Self {
        Self {
            client: Client::from_session(session),
            palette: None,
        }
    }
    fn mode(&self) -> PaletteMode {
        if self.palette.as_ref().is_some_and(|p| p.profiles) {
            PaletteMode::Profiles(Kind::Proxy)
        } else {
            PaletteMode::Commands
        }
    }
    fn palette_entries(&self) -> Vec<Entry> {
        self.client.palette_entries_in(language(), self.mode())
    }
    fn palette_matches(&self) -> Vec<Entry> {
        self.client.palette_matches(
            self.mode(),
            self.palette.as_ref().map_or("", |p| p.query.as_str()),
        )
    }
    fn palette_run(&mut self, index: usize) -> Input {
        let entry = self.palette_matches().remove(index);
        self.client.run_palette_entry(entry)
    }
}
fn profile_ui() -> Ui {
    let mut ui = Ui::new(Session::default());
    for (name, kind) in [
        ("proxy-a", Kind::Proxy),
        ("proxy-b", Kind::Proxy),
        ("rules-a", Kind::Rule),
        ("rules-b", Kind::Rule),
    ] {
        ui.rows.push(Row {
            instance: Instance {
                name: name.into(),
                kind,
                config: format!("{name}.toml").into(),
            },
            pid: None,
            state: "Stopped".into(),
            uptime: 0,
            changed: false,
        });
    }
    ui.active_profile = Some("proxy-b".into());
    ui
}
#[test]
fn palette_searches_current_language_and_english() {
    for language in [
        Language::English,
        Language::Simplified,
        Language::Traditional,
    ] {
        crate::i18n::with_language(language, || {
            let mut ui = Ui::new(Session::default());
            ui.palette = Some(Palette::default());
            assert_eq!(ui.palette_matches().len(), ui.palette_entries().len());
            for source in [
                "Overview",
                "Full restart",
                "Command palette",
                "Import local update package",
            ] {
                for query in [t(source).to_owned(), source.to_uppercase()] {
                    ui.palette.as_mut().unwrap().query = query;
                    let entries = ui.palette_matches();
                    assert_eq!(entries.len(), 1, "{language:?}: {source}");
                    assert_eq!(entries[0].label, t(source));
                }
            }
            for query in [t("General"), "GENERAL"] {
                ui.palette.as_mut().unwrap().query = query.into();
                let entries = ui.palette_matches();
                assert!(!entries.is_empty());
                assert!(entries.iter().all(|entry| entry.scope == "General"));
            }
            for query in [
                format!("{} {}", t("Go to"), t("Overview")),
                "GO TO OVERVIEW".into(),
            ] {
                ui.palette.as_mut().unwrap().query = query;
                let entries = ui.palette_matches();
                assert_eq!(entries.len(), 1);
                assert!(matches!(entries[0].run, Intent::Page(OVERVIEW)));
            }
            ui.palette.as_mut().unwrap().query = "ctrl-k".into();
            assert_eq!(ui.palette_matches().len(), 1);
            ui.palette.as_mut().unwrap().query = "no matching command".into();
            assert!(ui.palette_matches().is_empty());
        });
    }
}
#[test]
fn palette_searches_dynamic_labels_in_both_languages() {
    for language in [
        Language::English,
        Language::Simplified,
        Language::Traditional,
    ] {
        crate::i18n::with_language(language, || {
            let mut ui = profile_ui();
            ui.rows[0].pid = Some(42);
            for profiles in [false, true] {
                ui.palette = Some(Palette {
                    profiles,
                    ..Default::default()
                });
                for (name, status) in [("proxy-a", "Running"), ("proxy-b", "Stopped")] {
                    for query in [
                        format!("{} {name} · {}", t("Profile"), t(status)),
                        format!("PROFILE {name} · {}", status.to_uppercase()),
                    ] {
                        ui.palette.as_mut().unwrap().query = query;
                        let entries = ui.palette_matches();
                        assert_eq!(entries.len(), 1);
                        assert_eq!(entries[0].label, format!("{name} · {}", t(status)));
                        assert!(
                            matches!(&entries[0].run, Intent::Activate { name: target, .. } if target == name)
                        );
                    }
                }
            }
            ui.palette = Some(Palette::default());
            for query in [
                format!("{} · proxy-b", t("Force start")),
                "FORCE START · proxy-b".into(),
            ] {
                ui.palette.as_mut().unwrap().query = query;
                let entries = ui.palette_matches();
                assert_eq!(entries.len(), 1);
                assert!(matches!(entries[0].run, Intent::ForceStart));
                assert_eq!(entries[0].label, format!("{} · proxy-b", t("Force start")));
            }
            ui.palette_run(0);
            assert!(matches!(&ui.prompt, Some(Prompt::ForceStart(name)) if name == "proxy-b"));
        });
    }
}
#[test]
fn force_start_palette_targets_the_selected_core_or_active_profile() {
    let mut ui = profile_ui();
    for (page, kind, expected) in [
        (PROFILES, Kind::Rule, "rules-a"),
        (PROFILES, Kind::Proxy, "proxy-a"),
        (OVERVIEW, Kind::Proxy, "proxy-b"),
    ] {
        ui.page = page;
        ui.kind = kind;
        ui.palette = Some(Palette::default());
        let entries = ui.palette_matches();
        let index = entries
            .iter()
            .position(|entry| matches!(entry.run, Intent::ForceStart))
            .unwrap();
        assert!(entries[index].label.ends_with(expected));
        assert!(matches!(ui.palette_run(index), Input::Idle));
        assert!(matches!(&ui.prompt, Some(Prompt::ForceStart(name)) if name == expected));
        assert!(!ui.confirmation_accept);
        ui.cancel_prompt();
    }
    ui.rows.iter_mut().for_each(|row| row.pid = Some(42));
    assert!(!ui
        .palette_entries()
        .iter()
        .any(|entry| matches!(entry.run, Intent::ForceStart)));
}
