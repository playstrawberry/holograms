//! The save file, `plugins/data/holograms/holograms.json`:
//!
//! ```json
//! {"format": "holograms/1", "holograms": [{"id": "shop", "kind": "text", "lines": ["<gold>Shop"],
//!   "place": {"world": "world", "x": 0.5, "y": 70, "z": 0.5, "yaw": 0, "pitch": 0}, ...}]}
//! ```
//!
//! It holds what the holograms are, never the entities that show them: the plugin makes those
//! from the file, so the world's saved copies and the file can never become two holograms.
//! A file that cannot be read is refused whole and left untouched; nothing is written over it
//! until it is fixed.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::model::{Hologram, Kind, Place};

pub const FILE: &str = "data/holograms.json";
pub const FORMAT: &str = "holograms/1";
/// The largest save file read.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

/// The text of the save file for these holograms (those not kept are left out).
#[must_use]
pub fn encode<'a>(holograms: impl Iterator<Item = &'a Hologram>) -> String {
    let list: Vec<Value> = holograms
        .filter(|h| h.persist)
        .map(Hologram::to_json)
        .collect();
    let mut text = serde_json::to_string_pretty(&json!({"format": FORMAT, "holograms": list}))
        .unwrap_or_default();
    text.push('\n');
    text
}

/// Reads the save file.
///
/// # Errors
/// What is wrong, naming the hologram; nothing is read then.
pub fn decode(text: &str) -> Result<Vec<Hologram>, String> {
    if text.len() > MAX_BYTES {
        return Err(format!("the file is larger than {MAX_BYTES} bytes"));
    }
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let Value::Object(top) = &value else {
        return Err("the file is one JSON object".into());
    };
    if top.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(format!("\"format\" must be \"{FORMAT}\""));
    }
    if let Some(k) = top.keys().find(|k| *k != "format" && *k != "holograms") {
        return Err(format!("unknown field {k}"));
    }
    let Some(Value::Array(list)) = top.get("holograms") else {
        return Err("\"holograms\" is a list".into());
    };
    if list.len() > crate::model::MAX_HOLOGRAMS {
        return Err(format!("at most {} holograms", crate::model::MAX_HOLOGRAMS));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(list.len());
    for (n, item) in list.iter().enumerate() {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(|| format!("number {}", n + 1), str::to_owned);
        let read = || -> Result<Hologram, String> {
            for required in ["id", "kind", "place"] {
                if item.get(required).is_none() {
                    return Err(format!("{required} is missing"));
                }
            }
            let mut h = Hologram::new(
                "pending",
                Kind::Text,
                Place {
                    world: "world".into(),
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                    yaw: 0.0,
                    pitch: 0.0,
                },
            );
            h.apply_json(item, true)?;
            Ok(h)
        };
        let h = read().map_err(|e| format!("hologram {id}: {e}"))?;
        if !seen.insert(h.id.clone()) {
            return Err(format!("hologram {id} is in the file twice"));
        }
        out.push(h);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Background, Visibility};

    fn sample() -> Vec<Hologram> {
        let place = Place {
            world: "world".into(),
            x: 1.5,
            y: 64.0,
            z: -2.5,
            yaw: 45.0,
            pitch: 0.0,
        };
        let mut a = Hologram::new("shop", Kind::Text, place.clone());
        a.lines = vec!["<gold>Shop".into(), "&7{online} online".into()];
        a.background = Background::None;
        a.visibility = Visibility::Permission("holograms:view.shop".into());
        let mut b = Hologram::new("gem", Kind::Item, place.clone());
        b.item = "minecraft:emerald".into();
        let mut c = Hologram::new("p.board", Kind::Block, place.clone());
        c.owner = Some("p".into());
        let mut gone = Hologram::new("p.temp", Kind::Text, place);
        gone.owner = Some("p".into());
        gone.persist = false;
        vec![a, b, c, gone]
    }

    #[test]
    fn the_file_keeps_every_kept_hologram() {
        let all = sample();
        let back = decode(&encode(all.iter())).unwrap();
        assert_eq!(back, all[..3].to_vec());
        assert!(decode("").unwrap().is_empty());
        assert!(decode(&encode(std::iter::empty())).unwrap().is_empty());
    }

    #[test]
    fn a_broken_file_is_refused_whole_with_the_reason() {
        let good = encode(sample().iter());
        let twice = good.replace("\"gem\"", "\"shop\"");
        assert!(decode(&twice).unwrap_err().contains("twice"));
        let bad_line = good.replace("\"minecraft:emerald\"", "\"Emer ald\"");
        assert!(decode(&bad_line).unwrap_err().contains("hologram gem"));
        assert!(decode("garbage\n").unwrap_err().contains("not JSON"));
        assert!(decode("{\"format\":\"holograms/2\",\"holograms\":[]}").is_err());
        assert!(
            decode("{\"format\":\"holograms/1\",\"holograms\":[{\"id\":\"a\",\"kind\":\"text\"}]}")
                .unwrap_err()
                .contains("place is missing")
        );
        assert!(decode("{\"format\":\"holograms/1\",\"holograms\":[],\"extra\":1}").is_err());
    }
}
