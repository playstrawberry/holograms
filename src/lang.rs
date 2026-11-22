//! The plugin's own texts in English, French, Spanish and Turkish (`lang/<code>.lang`, `key=text`
//! lines; `{0}`, `{1}`, ... are filled in). A player reads his client's language; any other
//! language, and the console, read English.

use std::collections::BTreeMap;
use std::sync::LazyLock;

const FILES: [(&str, &str); 4] = [
    ("en", include_str!("../lang/en.lang")),
    ("fr", include_str!("../lang/fr.lang")),
    ("es", include_str!("../lang/es.lang")),
    ("tr", include_str!("../lang/tr.lang")),
];

type Table = BTreeMap<&'static str, &'static str>;

fn table(file: &'static str) -> Table {
    file.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect()
}

static TABLES: LazyLock<Vec<(&'static str, Table)>> = LazyLock::new(|| {
    FILES
        .iter()
        .map(|(code, file)| (*code, table(file)))
        .collect()
});

/// The language of a locale as Minecraft writes it (`fr_fr`, `tr_tr`, `es_mx`).
#[must_use]
pub fn language(locale: &str) -> &'static str {
    let code = locale.get(..2).unwrap_or("").to_ascii_lowercase();
    FILES
        .iter()
        .map(|(c, _)| *c)
        .find(|c| *c == code)
        .unwrap_or("en")
}

/// The text of `key` in `language` with `args` in place of `{0}`, `{1}`, ...; English when
/// the language lacks the key, the key itself when English lacks it too.
#[must_use]
pub fn text(language: &str, key: &str, args: &[&str]) -> String {
    let find = |code: &str| {
        TABLES
            .iter()
            .find(|(c, _)| *c == code)
            .and_then(|(_, t)| t.get(key).copied())
    };
    let raw = find(language).or_else(|| find("en")).unwrap_or(key);
    let mut out = raw.to_owned();
    for (i, arg) in args.iter().enumerate() {
        out = out.replace(&format!("{{{i}}}"), arg);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_has_every_key_with_the_same_placeholders() {
        let english = table(FILES[0].1);
        for (code, file) in FILES {
            let t = table(file);
            let lines = file.lines().filter(|l| !l.trim().is_empty()).count();
            assert_eq!(
                lines,
                t.len(),
                "{code}: a line without a key or a key twice"
            );
            assert_eq!(
                t.keys().collect::<Vec<_>>(),
                english.keys().collect::<Vec<_>>(),
                "{code} has other keys than en"
            );
            for (key, value) in &t {
                for slot in 0..8 {
                    let slot = format!("{{{slot}}}");
                    assert_eq!(
                        value.contains(&slot),
                        english[key].contains(&slot),
                        "{code} {key} {slot}"
                    );
                }
            }
        }
    }

    #[test]
    fn locales_pick_their_language_and_others_read_english() {
        assert_eq!(language("fr_fr"), "fr");
        assert_eq!(language("tr_tr"), "tr");
        assert_eq!(language("es_mx"), "es");
        assert_eq!(language("de_de"), "en");
        assert_eq!(language(""), "en");
        assert_eq!(
            text("tr", "created", &["shop"]),
            "shop hologramı oluşturuldu."
        );
        assert_eq!(text("de", "deleted", &["a"]), "Hologram a deleted.");
        assert_eq!(text("en", "no_such_key", &[]), "no_such_key");
    }
}
