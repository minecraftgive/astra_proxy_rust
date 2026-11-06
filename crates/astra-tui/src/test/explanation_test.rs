use super::*;
use astra_ipc::API_VERSION;
use astra_model::{control::RouteExplanation, RouteAction};
#[test]
fn active_route_explanation_renders_in_three_languages_and_small_windows() {
    use ratatui::{backend::TestBackend, Terminal};
    let view = View {
        report: RouteExplanation {
            api_version: API_VERSION,
            pid: 1,
            revision: 2,
            config: "rules.toml".into(),
            request: request("api.ip.sb").unwrap(),
            action: RouteAction::Proxy("explicit".into()),
            path: "rules[114] -> geosite(\"geolocation-!cn\")".into(),
            default_action: "proxy:default".into(),
            condition: Some(("domain_suffix".into(), "ip.sb".into())),
        },
        scroll: 0,
    };
    for language in [
        crate::i18n::Language::English,
        crate::i18n::Language::Simplified,
        crate::i18n::Language::Traditional,
    ] {
        crate::i18n::with_language(language, || {
            for (width, height) in [(100, 24), (32, 8)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| view.render(frame, frame.area()))
                    .unwrap();
            }
        });
    }
}
