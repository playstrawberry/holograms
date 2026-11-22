//! Values other plugins push for `{plugin:key}`: the `placeholders/1` text contract, the same
//! one the HUD of user_interface and Tab and Scoreboard read, so a plugin sends one message to
//! all of them.
//!
//! ```text
//! placeholders/1
//! set<TAB><uuid or *><TAB><key><TAB><value>
//! clear<TAB><uuid or *>[<TAB><key>]
//! ```
//!
//! `icon` lines are for the HUD; they are accepted and not used here. A message that breaks a
//! rule changes nothing. The sender's name comes from the server, so a plugin only ever sets its
//! own keys. A player's value wins over the one for `*`; a player's values go when they leave.

use std::collections::BTreeMap;

pub const CONTRACT: &str = "placeholders/1";
pub const MAX_LINES: usize = 1024;
pub const MAX_BYTES: usize = 64 * 1024;
pub const MAX_VALUE_CHARS: usize = 256;
pub const MAX_KEYS_PER_PLUGIN: usize = 64;
pub const MAX_PLUGINS: usize = 16;
/// Values for single players under one key.
pub const MAX_PLAYERS_PER_KEY: usize = 4096;

/// A player's UUID as one number.
pub type PlayerId = u128;

/// Reads a UUID with or without dashes.
#[must_use]
pub fn parse_uuid(text: &str) -> Option<PlayerId> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    let dashes_ok = text.len() == 32
        || (text.len() == 36
            && text
                .char_indices()
                .all(|(i, c)| matches!(i, 8 | 13 | 18 | 23) == (c == '-')));
    if hex.len() != 32 || !dashes_ok || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok()
}

/// A UUID in its usual text form.
#[must_use]
pub fn uuid_text(id: PlayerId) -> String {
    let h = format!("{id:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Everyone,
    Player(PlayerId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Push {
    Set(Target, String, String),
    Clear(Target, Option<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// `placeholders/1<TAB>alive`: answered with ok.
    Alive,
    Push(Vec<Push>),
}

/// Whether a plugin message is this text contract (the hologram API is JSON).
#[must_use]
pub fn is_contract(message: &[u8]) -> bool {
    message.starts_with(CONTRACT.as_bytes())
}

fn target(field: &str) -> Result<Target, String> {
    if field == "*" {
        return Ok(Target::Everyone);
    }
    parse_uuid(field)
        .map(Target::Player)
        .ok_or_else(|| "a target is a UUID or *".to_owned())
}

fn key(field: &str) -> Result<String, String> {
    if crate::markup::valid_key(field) {
        Ok(field.to_owned())
    } else {
        Err("a key is 1 to 64 of a-z 0-9 _ - .".to_owned())
    }
}

/// Reads one message.
///
/// # Errors
/// The reason, when any line breaks a rule.
pub fn parse(message: &[u8]) -> Result<Message, String> {
    if message.len() > MAX_BYTES {
        return Err(format!("at most {MAX_BYTES} bytes per message"));
    }
    let text = std::str::from_utf8(message).map_err(|_| "a message is UTF-8 text".to_owned())?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    let mut lines = text.split('\n');
    let first = lines.next().map(|l| l.trim_end_matches('\r'));
    if first == Some("placeholders/1\talive") {
        return Ok(Message::Alive);
    }
    if first != Some(CONTRACT) {
        return Err("unknown contract".to_owned());
    }
    let mut pushes = Vec::new();
    for (n, line) in lines.enumerate() {
        if n >= MAX_LINES {
            return Err(format!("at most {MAX_LINES} lines per message"));
        }
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        let push = match (f.first().copied(), f.len()) {
            (Some("set"), 4) => {
                if f[3].chars().count() > MAX_VALUE_CHARS {
                    return Err(format!("a value has at most {MAX_VALUE_CHARS} characters"));
                }
                Push::Set(target(f[1])?, key(f[2])?, f[3].to_owned())
            }
            // The HUD's images: valid in this contract, nothing to show on a hologram.
            (Some("icon"), 4) => {
                target(f[1])?;
                key(f[2])?;
                continue;
            }
            (Some("clear"), 2) => Push::Clear(target(f[1])?, None),
            (Some("clear"), 3) => Push::Clear(target(f[1])?, Some(key(f[2])?)),
            _ => {
                return Err(format!(
                    "line {} is not set, icon or clear with its fields",
                    n + 2
                ));
            }
        };
        pushes.push(push);
    }
    Ok(Message::Push(pushes))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Entry {
    everyone: Option<String>,
    players: BTreeMap<PlayerId, String>,
}

impl Entry {
    fn is_empty(&self) -> bool {
        self.everyone.is_none() && self.players.is_empty()
    }
}

/// The values every plugin pushed, by plugin (lower case) and key.
#[derive(Clone, Debug, Default)]
pub struct Values {
    plugins: BTreeMap<String, BTreeMap<String, Entry>>,
}

impl Values {
    /// Applies one message of `plugin`, whole or not at all. Returns the keys whose values
    /// changed.
    ///
    /// # Errors
    /// The reason a limit would be passed; nothing changed then.
    pub fn apply(&mut self, plugin: &str, pushes: &[Push]) -> Result<Vec<String>, String> {
        let plugin = plugin.to_ascii_lowercase();
        let mut mine = self.plugins.get(&plugin).cloned().unwrap_or_default();
        for push in pushes {
            match push {
                Push::Set(t, k, v) => {
                    let entry = mine.entry(k.clone()).or_default();
                    match t {
                        Target::Everyone => entry.everyone = Some(v.clone()),
                        Target::Player(p) => {
                            entry.players.insert(*p, v.clone());
                        }
                    }
                }
                Push::Clear(t, k) => {
                    let keys: Vec<String> = match k {
                        Some(k) => vec![k.clone()],
                        None => mine.keys().cloned().collect(),
                    };
                    for k in keys {
                        if let Some(entry) = mine.get_mut(&k) {
                            match t {
                                // `clear * [key]`: every value of the key, the players' too.
                                Target::Everyone => *entry = Entry::default(),
                                Target::Player(p) => {
                                    entry.players.remove(p);
                                }
                            }
                        }
                    }
                }
            }
        }
        mine.retain(|_, e| !e.is_empty());
        if mine.len() > MAX_KEYS_PER_PLUGIN {
            return Err(format!("at most {MAX_KEYS_PER_PLUGIN} keys per plugin"));
        }
        if mine.values().any(|e| e.players.len() > MAX_PLAYERS_PER_KEY) {
            return Err(format!("at most {MAX_PLAYERS_PER_KEY} players per key"));
        }
        let before = self.plugins.get(&plugin);
        if before.is_none() && !mine.is_empty() && self.plugins.len() >= MAX_PLUGINS {
            return Err(format!("at most {MAX_PLUGINS} plugins push values"));
        }
        let empty = BTreeMap::new();
        let old = before.unwrap_or(&empty);
        let mut changed: Vec<String> = old
            .keys()
            .chain(mine.keys())
            .filter(|k| old.get(*k) != mine.get(*k))
            .cloned()
            .collect();
        changed.sort();
        changed.dedup();
        if mine.is_empty() {
            self.plugins.remove(&plugin);
        } else {
            self.plugins.insert(plugin, mine);
        }
        Ok(changed)
    }

    /// The value `viewer` sees for `{plugin:key}`: his own, else the one for everyone.
    #[must_use]
    pub fn get(&self, plugin: &str, key: &str, viewer: Option<PlayerId>) -> Option<&str> {
        let entry = self.plugins.get(plugin)?.get(key)?;
        viewer
            .and_then(|v| entry.players.get(&v))
            .or(entry.everyone.as_ref())
            .map(String::as_str)
    }

    /// Whether `{plugin:key}` has a value for some single player, so it differs per viewer.
    #[must_use]
    pub fn per_player(&self, plugin: &str, key: &str) -> bool {
        self.plugins
            .get(plugin)
            .and_then(|m| m.get(key))
            .is_some_and(|e| !e.players.is_empty())
    }

    /// Forgets a player's values (he left). Returns the `(plugin, key)` pairs that changed.
    pub fn leave(&mut self, player: PlayerId) -> Vec<(String, String)> {
        let mut changed = Vec::new();
        for (plugin, keys) in &mut self.plugins {
            for (key, entry) in keys.iter_mut() {
                if entry.players.remove(&player).is_some() {
                    changed.push((plugin.clone(), key.clone()));
                }
            }
            keys.retain(|_, e| !e.is_empty());
        }
        self.plugins.retain(|_, k| !k.is_empty());
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEVE: &str = "8667ba71-b85a-4004-af54-457a9734eed7";

    fn steve() -> PlayerId {
        parse_uuid(STEVE).unwrap()
    }

    fn pushes(text: &str) -> Vec<Push> {
        match parse(text.as_bytes()).unwrap() {
            Message::Push(p) => p,
            Message::Alive => panic!("alive"),
        }
    }

    #[test]
    fn uuids_read_with_and_without_dashes() {
        assert_eq!(parse_uuid(&STEVE.replace('-', "")), Some(steve()));
        assert_eq!(uuid_text(steve()), STEVE);
        assert_eq!(parse_uuid("8667ba71b85a-4004-af54-457a9734eed7a"), None);
        assert_eq!(parse_uuid("nobody"), None);
        // Text that is not ASCII is refused, never cut in the middle of a character.
        assert_eq!(parse_uuid("000000000000000é000000000000000"), None);
        assert_eq!(parse_uuid("8667ba71-b85a-4004-af54-457a9734eedé"), None);
    }

    #[test]
    fn a_batch_is_read_and_icons_are_accepted() {
        let p = pushes(&format!(
            "placeholders/1\nset\t{STEVE}\tmoney\t1,234\nicon\t*\tworld\tmoon\nclear\t{STEVE}\tmoney\nclear\t*\n"
        ));
        assert_eq!(
            p,
            vec![
                Push::Set(Target::Player(steve()), "money".into(), "1,234".into()),
                Push::Clear(Target::Player(steve()), Some("money".into())),
                Push::Clear(Target::Everyone, None),
            ]
        );
        assert_eq!(parse(b"placeholders/1\talive"), Ok(Message::Alive));
    }

    #[test]
    fn broken_messages_are_refused_whole() {
        for bad in [
            format!("placeholders/1\nset\t{STEVE}\tmoney"),
            "placeholders/1\nset\tnobody\tmoney\t1".to_owned(),
            "placeholders/1\nset\t*\tBad Key\t1".to_owned(),
            "placeholders/1\nfly\t*".to_owned(),
            "placeholders/2\nclear\t*".to_owned(),
            format!(
                "placeholders/1\nset\t*\tk\t{}",
                "v".repeat(MAX_VALUE_CHARS + 1)
            ),
        ] {
            assert!(parse(bad.as_bytes()).is_err(), "{bad}");
        }
        let many: String = std::iter::once("placeholders/1".to_owned())
            .chain((0..=MAX_LINES).map(|_| "clear\t*".to_owned()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse(many.as_bytes()).is_err());
        assert!(parse(&[0xff]).is_err());
    }

    #[test]
    fn a_players_value_wins_and_goes_when_he_leaves() {
        let mut v = Values::default();
        let changed = v
            .apply(
                "Shop",
                &pushes(&format!(
                    "placeholders/1\nset\t*\tmoney\t0\nset\t{STEVE}\tmoney\t42"
                )),
            )
            .unwrap();
        assert_eq!(changed, vec!["money".to_owned()]);
        assert_eq!(v.get("shop", "money", Some(steve())), Some("42"));
        assert_eq!(v.get("shop", "money", Some(1)), Some("0"));
        assert_eq!(v.get("shop", "money", None), Some("0"));
        assert!(v.per_player("shop", "money"));
        assert_eq!(
            v.leave(steve()),
            vec![("shop".to_owned(), "money".to_owned())]
        );
        assert!(!v.per_player("shop", "money"));
        // The same value again changes nothing.
        assert!(
            v.apply("shop", &pushes("placeholders/1\nset\t*\tmoney\t0"))
                .unwrap()
                .is_empty()
        );
        // clear * takes every value of the key, the players' too.
        v.apply(
            "shop",
            &pushes(&format!("placeholders/1\nset\t{STEVE}\tmoney\t7")),
        )
        .unwrap();
        v.apply("shop", &pushes("placeholders/1\nclear\t*\tmoney"))
            .unwrap();
        assert_eq!(v.get("shop", "money", Some(steve())), None);
    }

    #[test]
    fn limits_refuse_the_whole_message() {
        let mut v = Values::default();
        let mut msg = String::from("placeholders/1");
        for i in 0..=MAX_KEYS_PER_PLUGIN {
            msg.push_str(&format!("\nset\t*\tk{i}\tv"));
        }
        assert!(v.apply("a", &pushes(&msg)).is_err());
        assert_eq!(v.get("a", "k0", None), None);
        for i in 0..MAX_PLUGINS {
            v.apply(&format!("p{i}"), &pushes("placeholders/1\nset\t*\tk\tv"))
                .unwrap();
        }
        assert!(
            v.apply("late", &pushes("placeholders/1\nset\t*\tk\tv"))
                .is_err()
        );
    }
}
