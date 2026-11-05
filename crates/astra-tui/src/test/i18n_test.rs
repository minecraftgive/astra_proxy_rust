use super::*;
#[test]
fn regional_locales_and_fallback() {
    for locale in ["zh-Hant", "zh_TW.UTF-8", "zh-HK", "zh-MO"] {
        assert_eq!(Language::resolve(locale), Language::Traditional);
    }
    for locale in ["zh-CN", "zh_Hans", "zh"] {
        assert_eq!(Language::resolve(locale), Language::Simplified);
    }
    for locale in ["en-US", "C", "de_DE.UTF-8"] {
        assert_eq!(Language::resolve(locale), Language::English);
    }
}
#[test]
fn indexed_translations_match_every_catalog_entry_and_preserve_unknown_text() {
    for &(source, english, simplified, traditional) in CATALOG {
        assert_eq!(translate(source, Language::English), english);
        assert_eq!(translate(source, Language::Simplified), simplified);
        assert_eq!(translate(source, Language::Traditional), traditional);
    }
    assert_eq!(
        translate("unknown translation key", Language::Simplified),
        "unknown translation key"
    );
}
#[test]
fn translations_keep_all_placeholders() {
    let mut keys = std::collections::HashSet::new();
    for (source, english, simplified, traditional) in CATALOG {
        assert!(keys.insert(source), "Duplicate translation: {source}");
        let count = source.matches("{}").count();
        for translation in [english, simplified, traditional] {
            assert_eq!(translation.matches("{}").count(), count, "{source}");
        }
    }
}
