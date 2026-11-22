//! The API for other plugins: one JSON object per plugin message to `holograms`, answered with
//! `{"ok": true, ...}` or `{"ok": false, "error": "..."}`.
//!
//! | Request | Does |
//! | --- | --- |
//! | `{"op":"version"}` | `{"ok":true,"api":1}` |
//! | `{"op":"create","key":"k","spec":{...},"persist":true}` | makes or replaces the plugin's hologram `k` |
//! | `{"op":"update","key":"k","spec":{...}}` | changes the fields the spec names |
//! | `{"op":"lines","key":"k","lines":["..."]}` | sets the text |
//! | `{"op":"move","key":"k","place":{...}}` | moves it |
//! | `{"op":"show","key":"k","player":"<uuid>"}`, `hide` | adds or takes a player of a `players` hologram |
//! | `{"op":"delete","key":"k"}` | removes it |
//! | `{"op":"get","key":"k"}`, `{"op":"list"}` | reads |
//!
//! Keys belong to the plugin that sends them: the sender's name comes from the server, so no
//! plugin can reach another's holograms. The same key replaces, never doubles.

use serde_json::{Map, Value, json};

use crate::model::valid_id;
use crate::values::{PlayerId, parse_uuid};

pub const API_VERSION: u32 = 1;
pub const MAX_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Version,
    Create {
        key: String,
        spec: Value,
        persist: bool,
    },
    Update {
        key: String,
        spec: Value,
    },
    Lines {
        key: String,
        lines: Value,
    },
    Move {
        key: String,
        place: Value,
    },
    Show {
        key: String,
        player: PlayerId,
        show: bool,
    },
    Delete {
        key: String,
    },
    Get {
        key: String,
    },
    List,
}

fn only(m: &Map<String, Value>, fields: &[&str]) -> Result<(), String> {
    match m.keys().find(|k| !fields.contains(&k.as_str())) {
        Some(k) => Err(format!("unknown field {k}")),
        None => Ok(()),
    }
}

fn key(m: &Map<String, Value>) -> Result<String, String> {
    let k = m.get("key").and_then(Value::as_str).ok_or("key: a text")?;
    if valid_id(k) {
        Ok(k.to_owned())
    } else {
        Err("key: 1 to 32 of a-z 0-9 _ -".into())
    }
}

fn object(m: &Map<String, Value>, field: &str) -> Result<Value, String> {
    match m.get(field) {
        Some(v @ Value::Object(_)) => Ok(v.clone()),
        _ => Err(format!("{field}: a JSON object")),
    }
}

/// Reads one request.
///
/// # Errors
/// What is wrong with it.
pub fn parse(message: &[u8]) -> Result<Request, String> {
    if message.len() > MAX_BYTES {
        return Err(format!("at most {MAX_BYTES} bytes per request"));
    }
    let value: Value =
        serde_json::from_slice(message).map_err(|_| "a request is one JSON object".to_owned())?;
    let Value::Object(m) = value else {
        return Err("a request is one JSON object".into());
    };
    let op = m.get("op").and_then(Value::as_str).ok_or("op: a text")?;
    Ok(match op {
        "version" => {
            only(&m, &["op"])?;
            Request::Version
        }
        "list" => {
            only(&m, &["op"])?;
            Request::List
        }
        "create" => {
            only(&m, &["op", "key", "spec", "persist"])?;
            let persist = match m.get("persist") {
                None => true,
                Some(v) => v.as_bool().ok_or("persist: true or false")?,
            };
            let spec = object(&m, "spec")?;
            for required in ["kind", "place"] {
                if spec.get(required).is_none() {
                    return Err(format!("spec.{required} is missing"));
                }
            }
            Request::Create {
                key: key(&m)?,
                spec,
                persist,
            }
        }
        "update" => {
            only(&m, &["op", "key", "spec"])?;
            Request::Update {
                key: key(&m)?,
                spec: object(&m, "spec")?,
            }
        }
        "lines" => {
            only(&m, &["op", "key", "lines"])?;
            let lines = m.get("lines").cloned().ok_or("lines: a list of texts")?;
            Request::Lines {
                key: key(&m)?,
                lines,
            }
        }
        "move" => {
            only(&m, &["op", "key", "place"])?;
            Request::Move {
                key: key(&m)?,
                place: object(&m, "place")?,
            }
        }
        "show" | "hide" => {
            only(&m, &["op", "key", "player"])?;
            let player = m
                .get("player")
                .and_then(Value::as_str)
                .and_then(parse_uuid)
                .ok_or("player: a UUID")?;
            Request::Show {
                key: key(&m)?,
                player,
                show: op == "show",
            }
        }
        "delete" | "get" => {
            only(&m, &["op", "key"])?;
            let key = key(&m)?;
            if op == "delete" {
                Request::Delete { key }
            } else {
                Request::Get { key }
            }
        }
        _ => return Err(format!("unknown op {op}")),
    })
}

/// `{"ok": true, ...fields}`.
#[must_use]
pub fn ok(fields: &[(&str, Value)]) -> Vec<u8> {
    let mut m = Map::new();
    m.insert("ok".into(), json!(true));
    for (k, v) in fields {
        m.insert((*k).to_owned(), v.clone());
    }
    Value::Object(m).to_string().into_bytes()
}

/// `{"ok": false, "error": reason}`.
#[must_use]
pub fn error(reason: &str) -> Vec<u8> {
    json!({"ok": false, "error": reason})
        .to_string()
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEVE: &str = "8667ba71-b85a-4004-af54-457a9734eed7";

    #[test]
    fn every_request_is_read() {
        assert_eq!(parse(br#"{"op":"version"}"#), Ok(Request::Version));
        assert_eq!(parse(br#"{"op":"list"}"#), Ok(Request::List));
        let create = parse(
            br#"{"op":"create","key":"board","persist":false,"spec":{"kind":"text","place":{"world":"world","x":0,"y":64,"z":0},"lines":["a"]}}"#,
        )
        .unwrap();
        assert!(
            matches!(create, Request::Create { ref key, persist: false, .. } if key == "board")
        );
        assert!(matches!(
            parse(br#"{"op":"update","key":"board","spec":{"scale":2}}"#),
            Ok(Request::Update { .. })
        ));
        assert!(matches!(
            parse(br#"{"op":"lines","key":"board","lines":["x"]}"#),
            Ok(Request::Lines { .. })
        ));
        assert!(matches!(
            parse(br#"{"op":"move","key":"board","place":{"world":"w","x":1,"y":2,"z":3}}"#),
            Ok(Request::Move { .. })
        ));
        assert_eq!(
            parse(format!(r#"{{"op":"hide","key":"board","player":"{STEVE}"}}"#).as_bytes()),
            Ok(Request::Show {
                key: "board".into(),
                player: parse_uuid(STEVE).unwrap(),
                show: false
            })
        );
        assert_eq!(
            parse(br#"{"op":"delete","key":"board"}"#),
            Ok(Request::Delete {
                key: "board".into()
            })
        );
        assert_eq!(
            parse(br#"{"op":"get","key":"board"}"#),
            Ok(Request::Get {
                key: "board".into()
            })
        );
    }

    #[test]
    fn bad_requests_say_why() {
        for (bad, why) in [
            (&br#"not json"#[..], "JSON object"),
            (br#"[1]"#, "JSON object"),
            (br#"{"op":"dance"}"#, "unknown op"),
            (br#"{"op":"version","x":1}"#, "unknown field x"),
            (
                br#"{"op":"create","key":"b","spec":{"kind":"text"}}"#,
                "spec.place",
            ),
            (
                br#"{"op":"create","key":"Bad Key","spec":{"kind":"text","place":{}}}"#,
                "key:",
            ),
            (br#"{"op":"show","key":"b","player":"nobody"}"#, "player"),
            (br#"{"op":"move","key":"b","place":5}"#, "place"),
        ] {
            let e = parse(bad).unwrap_err();
            assert!(e.contains(why), "{e} for {}", String::from_utf8_lossy(bad));
        }
        assert!(parse(&vec![b' '; MAX_BYTES + 1]).is_err());
    }

    #[test]
    fn answers_are_json() {
        let v: Value = serde_json::from_slice(&ok(&[("id", json!("p.board"))])).unwrap();
        assert_eq!(v, json!({"ok": true, "id": "p.board"}));
        let v: Value = serde_json::from_slice(&error("no")).unwrap();
        assert_eq!(v, json!({"ok": false, "error": "no"}));
    }
}
