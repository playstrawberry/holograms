//! A hologram as the plugin keeps it: what it shows, where, how it looks and who sees it.
//!
//! The same JSON describes a hologram in the save file and in the API for other plugins
//! ([`Hologram::to_json`], [`Hologram::from_json`]). Reading is strict: a field this version
//! does not know, a wrong type or a value out of its range is refused with the reason, so a
//! broken file or request never becomes a half-read hologram.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::markup::{self, MAX_LINE_CHARS};
use crate::values::{PlayerId, parse_uuid, uuid_text};

pub const MAX_LINES: usize = 32;
pub const MAX_VIEWERS: usize = 256;
/// Holograms in all, from commands and plugins.
pub const MAX_HOLOGRAMS: usize = 20_000;
/// Holograms of one plugin (through the API).
pub const MAX_PER_PLUGIN: usize = 5_000;
pub const MIN_REFRESH_TICKS: u32 = 5;
pub const MAX_REFRESH_TICKS: u32 = 72_000;
/// The farthest a display entity is sent to players, in blocks.
pub const MAX_RANGE: f32 = 160.0;
/// The view range the game counts in: 1.0 is 64 blocks.
pub const RANGE_UNIT: f32 = 64.0;
const MAX_COORD: f64 = 30_000_000.0;
const MAX_Y: f64 = 20_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Item,
    Block,
}

impl Kind {
    pub const ALL: [Self; 3] = [Self::Text, Self::Item, Self::Block];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Item => "item",
            Self::Block => "block",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Billboard {
    Fixed,
    Vertical,
    Horizontal,
    Center,
}

impl Billboard {
    pub const ALL: [Self; 4] = [Self::Fixed, Self::Vertical, Self::Horizontal, Self::Center];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Vertical => "vertical",
            Self::Horizontal => "horizontal",
            Self::Center => "center",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Center,
    Left,
    Right,
}

impl Align {
    pub const ALL: [Self; 3] = [Self::Center, Self::Left, Self::Right];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Center => "center",
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }
}

/// The box behind a text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Background {
    /// The game's own grey box.
    Default,
    /// No box.
    None,
    /// `#AARRGGBB`.
    Argb(u32),
}

impl Background {
    /// `default`, `none`, `#RRGGBB` (opaque) or `#AARRGGBB`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let lower = text.trim().to_ascii_lowercase();
        match lower.as_str() {
            "default" => return Some(Self::Default),
            "none" | "transparent" => return Some(Self::None),
            _ => {}
        }
        let hex = lower.strip_prefix('#')?;
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(hex, 16).ok()?;
        match hex.len() {
            6 => Some(Self::Argb(0xff00_0000 | value)),
            8 => Some(Self::Argb(value)),
            _ => None,
        }
    }

    #[must_use]
    pub fn text(self) -> String {
        match self {
            Self::Default => "default".to_owned(),
            Self::None => "none".to_owned(),
            Self::Argb(v) => format!("#{v:08x}"),
        }
    }
}

/// Who sees a hologram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Visibility {
    Everyone,
    /// Players with this permission node.
    Permission(String),
    /// These players only.
    Players(BTreeSet<PlayerId>),
}

impl Visibility {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Everyone => "everyone",
            Self::Permission(_) => "permission",
            Self::Players(_) => "players",
        }
    }
}

/// The node a permission hologram uses when none is given.
#[must_use]
pub fn default_view_node(id: &str) -> String {
    format!("holograms:view.{id}")
}

/// `a-z 0-9 _ . : -`, 1 to 128: a permission node.
#[must_use]
pub fn valid_node(node: &str) -> bool {
    (1..=128).contains(&node.len())
        && node.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b':' | b'-')
        })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub world: String,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
}

impl Place {
    /// Chunk coordinates.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn chunk(&self) -> (i32, i32) {
        ((self.x.floor() as i32) >> 4, (self.z.floor() as i32) >> 4)
    }

    fn check(&self) -> Result<(), String> {
        if self.world.is_empty() || self.world.chars().count() > 64 {
            return Err("place.world: 1 to 64 characters".into());
        }
        if ![self.x, self.y, self.z].iter().all(|v| v.is_finite())
            || self.x.abs() > MAX_COORD
            || self.z.abs() > MAX_COORD
            || self.y.abs() > MAX_Y
        {
            return Err("place: a position inside the world".into());
        }
        if !self.yaw.is_finite() || !self.pitch.is_finite() || self.pitch.abs() > 90.0 {
            return Err("place: yaw is any angle, pitch -90 to 90".into());
        }
        Ok(())
    }
}

/// Ids typed in commands: 1 to 32 of `a-z 0-9 _ -`.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// The id of a hologram a plugin made: `<plugin>.<key>`.
#[must_use]
pub fn api_id(plugin: &str, key: &str) -> String {
    format!("{}.{key}", plugin.to_ascii_lowercase())
}

/// An id as stored: a command id, or `<plugin>.<key>` with a plugin name of 1 to 64 of
/// `a-z 0-9 _ - .`... and a key that is a command id.
#[must_use]
pub fn valid_stored_id(id: &str) -> bool {
    if valid_id(id) {
        return true;
    }
    id.rsplit_once('.').is_some_and(|(plugin, key)| {
        (1..=64).contains(&plugin.len())
            && plugin.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'_' | b'-' | b'.' | b' ')
            })
            && valid_id(key)
    })
}

/// `minecraft:stone`, `stone` (namespace `minecraft`), lower case.
#[must_use]
pub fn normalize_key(key: &str) -> Option<String> {
    let key = key.trim().to_ascii_lowercase();
    let full = if key.contains(':') {
        key
    } else {
        format!("minecraft:{key}")
    };
    let (ns, path) = full.split_once(':')?;
    let ok = |s: &str, slash: bool| {
        !s.is_empty()
            && s.len() <= 128
            && s.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'_' | b'-' | b'.')
                    || (slash && b == b'/')
            })
    };
    (ok(ns, false) && ok(path, true)).then_some(full)
}

/// A block with optional properties: `minecraft:oak_stairs[facing=east,half=top]`.
/// Returns the name and the properties.
#[must_use]
pub fn parse_block(text: &str) -> Option<(String, Vec<(String, String)>)> {
    let text = text.trim();
    let (name, props) = match text.split_once('[') {
        Some((name, rest)) => (name, rest.strip_suffix(']')?),
        None => (text, ""),
    };
    let name = normalize_key(name)?;
    let mut out = Vec::new();
    for pair in props.split(',').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=')?;
        let good = |s: &str| {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        };
        if !good(k.trim()) || !good(v.trim()) || out.len() >= 16 {
            return None;
        }
        out.push((k.trim().to_owned(), v.trim().to_owned()));
    }
    Some((name, out))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hologram {
    pub id: String,
    /// The plugin that made it through the API; `None` for one made with a command.
    pub owner: Option<String>,
    /// Kept in the save file. Only plugins make holograms that are not kept.
    pub persist: bool,
    pub kind: Kind,
    /// Text holograms: the lines, as written (markup).
    pub lines: Vec<String>,
    /// Item holograms: the item key.
    pub item: String,
    /// Block holograms: the block with its properties.
    pub block: String,
    pub place: Place,
    pub billboard: Billboard,
    pub scale: [f32; 3],
    pub offset: [f32; 3],
    /// Block light and sky light, 0 to 15; `None` for the light where it stands.
    pub brightness: Option<(u8, u8)>,
    /// Blocks.
    pub range: f32,
    pub shadow_radius: f32,
    pub shadow_strength: f32,
    pub background: Background,
    pub text_shadow: bool,
    pub see_through: bool,
    pub align: Align,
    /// Pixels before a line wraps.
    pub width: i32,
    /// Ticks between reading values that change on their own (`{world_time}`, `{time}`).
    pub refresh: Option<u32>,
    pub visibility: Visibility,
}

impl Hologram {
    /// A new hologram of `kind` at `place` with the defaults.
    #[must_use]
    pub fn new(id: &str, kind: Kind, place: Place) -> Self {
        Self {
            id: id.to_owned(),
            owner: None,
            persist: true,
            kind,
            lines: Vec::new(),
            item: "minecraft:diamond".to_owned(),
            block: "minecraft:grass_block".to_owned(),
            place,
            billboard: if kind == Kind::Block {
                Billboard::Fixed
            } else {
                Billboard::Center
            },
            scale: [1.0; 3],
            offset: [0.0; 3],
            brightness: None,
            range: RANGE_UNIT,
            shadow_radius: 0.0,
            shadow_strength: 1.0,
            background: Background::Default,
            text_shadow: false,
            see_through: false,
            align: Align::Center,
            width: 200,
            refresh: None,
            visibility: Visibility::Everyone,
        }
    }

    /// Checks every limit.
    ///
    /// # Errors
    /// The first field out of its range.
    pub fn check(&self) -> Result<(), String> {
        if !valid_stored_id(&self.id) {
            return Err("id: 1 to 32 of a-z 0-9 _ -".into());
        }
        if self.lines.len() > MAX_LINES {
            return Err(format!("lines: at most {MAX_LINES}"));
        }
        if self
            .lines
            .iter()
            .any(|l| l.chars().count() > MAX_LINE_CHARS || l.chars().any(char::is_control))
        {
            return Err(format!(
                "lines: one line each, at most {MAX_LINE_CHARS} characters"
            ));
        }
        if normalize_key(&self.item).as_deref() != Some(self.item.as_str()) {
            return Err("item: an item key such as minecraft:diamond".into());
        }
        if parse_block(&self.block).is_none() {
            return Err(
                "block: a block such as minecraft:stone or minecraft:oak_stairs[facing=east]"
                    .into(),
            );
        }
        self.place.check()?;
        if !in_range(&self.scale, 0.01, 64.0) {
            return Err("scale: 0.01 to 64".into());
        }
        if !in_range(&self.offset, -64.0, 64.0) {
            return Err("offset: -64 to 64".into());
        }
        if self.brightness.is_some_and(|(b, s)| b > 15 || s > 15) {
            return Err("brightness: 0 to 15".into());
        }
        if !in_range(&[self.range], 1.0, MAX_RANGE) {
            return Err(format!("range: 1 to {MAX_RANGE} blocks"));
        }
        if !in_range(&[self.shadow_radius], 0.0, 64.0)
            || !in_range(&[self.shadow_strength], 0.0, 1.0)
        {
            return Err("shadow_radius: 0 to 64; shadow_strength: 0 to 1".into());
        }
        if !(1..=4096).contains(&self.width) {
            return Err("width: 1 to 4096".into());
        }
        if self
            .refresh
            .is_some_and(|r| !(MIN_REFRESH_TICKS..=MAX_REFRESH_TICKS).contains(&r))
        {
            return Err(format!(
                "refresh: {MIN_REFRESH_TICKS} to {MAX_REFRESH_TICKS} ticks"
            ));
        }
        match &self.visibility {
            Visibility::Permission(node) if !valid_node(node) => {
                return Err("visibility.node: 1 to 128 of a-z 0-9 _ . : -".into());
            }
            Visibility::Players(p) if p.len() > MAX_VIEWERS => {
                return Err(format!("visibility.players: at most {MAX_VIEWERS}"));
            }
            _ => {}
        }
        Ok(())
    }

    /// The JSON written to the save file and answered to `get`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        if let Some(owner) = &self.owner {
            m.insert("owner".into(), json!(owner));
        }
        m.insert("kind".into(), json!(self.kind.name()));
        match self.kind {
            Kind::Text => {
                m.insert("lines".into(), json!(self.lines));
            }
            Kind::Item => {
                m.insert("item".into(), json!(self.item));
            }
            Kind::Block => {
                m.insert("block".into(), json!(self.block));
            }
        }
        m.insert(
            "place".into(),
            json!({"world": self.place.world, "x": self.place.x, "y": self.place.y, "z": self.place.z,
                   "yaw": self.place.yaw, "pitch": self.place.pitch}),
        );
        m.insert("billboard".into(), json!(self.billboard.name()));
        m.insert("scale".into(), json!(self.scale));
        m.insert("offset".into(), json!(self.offset));
        m.insert(
            "brightness".into(),
            self.brightness
                .map_or(Value::Null, |(b, s)| json!({"block": b, "sky": s})),
        );
        m.insert("range".into(), json!(self.range));
        m.insert("shadow_radius".into(), json!(self.shadow_radius));
        m.insert("shadow_strength".into(), json!(self.shadow_strength));
        if self.kind == Kind::Text {
            m.insert("background".into(), json!(self.background.text()));
            m.insert("text_shadow".into(), json!(self.text_shadow));
            m.insert("see_through".into(), json!(self.see_through));
            m.insert("align".into(), json!(self.align.name()));
            m.insert("width".into(), json!(self.width));
            m.insert(
                "refresh".into(),
                self.refresh.map_or(Value::Null, |r| json!(r)),
            );
        }
        m.insert(
            "visibility".into(),
            match &self.visibility {
                Visibility::Everyone => json!({"mode": "everyone"}),
                Visibility::Permission(node) => json!({"mode": "permission", "node": node}),
                Visibility::Players(p) => json!({"mode": "players",
                    "players": p.iter().map(|id| uuid_text(*id)).collect::<Vec<_>>()}),
            },
        );
        Value::Object(m)
    }

    /// Reads a hologram from JSON on top of `self`: fields that are absent keep their value.
    /// `id`, `owner` are only read when `stored` (the save file); a request names the id
    /// itself.
    ///
    /// # Errors
    /// The first field that is unknown, of the wrong type or out of its range.
    pub fn apply_json(&mut self, value: &Value, stored: bool) -> Result<(), String> {
        let Value::Object(m) = value else {
            return Err("a hologram is a JSON object".into());
        };
        for (k, v) in m {
            match k.as_str() {
                "id" if stored => self.id = string(v, "id")?,
                "owner" if stored => self.owner = Some(string(v, "owner")?),
                "kind" => {
                    let name = string(v, "kind")?;
                    let kind = Kind::from_name(&name).ok_or("kind: text, item or block")?;
                    if kind != self.kind && kind == Kind::Block && !m.contains_key("billboard") {
                        self.billboard = Billboard::Fixed;
                    }
                    self.kind = kind;
                }
                "lines" => {
                    let Value::Array(a) = v else {
                        return Err("lines: a list of texts".into());
                    };
                    if a.len() > MAX_LINES {
                        return Err(format!("lines: at most {MAX_LINES}"));
                    }
                    self.lines = a
                        .iter()
                        .map(|l| string(l, "lines").map(|s| markup::clean_line(&s)))
                        .collect::<Result<_, _>>()?;
                }
                "item" => {
                    self.item = normalize_key(&string(v, "item")?)
                        .ok_or("item: an item key such as minecraft:diamond")?;
                }
                "block" => {
                    let text = string(v, "block")?;
                    let (name, props) =
                        parse_block(&text).ok_or("block: a block such as minecraft:stone")?;
                    self.block = block_text(&name, &props);
                }
                "place" => self.place = place(v)?,
                "billboard" => {
                    self.billboard = Billboard::from_name(&string(v, "billboard")?)
                        .ok_or("billboard: fixed, center, vertical or horizontal")?;
                }
                "scale" => self.scale = vec3(v, "scale", true)?,
                "offset" => self.offset = vec3(v, "offset", false)?,
                "brightness" => {
                    self.brightness = match v {
                        Value::Null => None,
                        Value::Object(b) => {
                            if b.keys().any(|k| k != "block" && k != "sky") {
                                return Err("brightness: block and sky".into());
                            }
                            let level = |name: &str| {
                                b.get(name)
                                    .and_then(Value::as_u64)
                                    .and_then(|n| u8::try_from(n).ok())
                                    .filter(|n| *n <= 15)
                                    .ok_or_else(|| format!("brightness.{name}: 0 to 15"))
                            };
                            Some((level("block")?, level("sky")?))
                        }
                        _ => return Err("brightness: null or {block, sky}".into()),
                    };
                }
                "range" => self.range = number(v, "range")?,
                "shadow_radius" => self.shadow_radius = number(v, "shadow_radius")?,
                "shadow_strength" => self.shadow_strength = number(v, "shadow_strength")?,
                "background" => {
                    self.background = Background::parse(&string(v, "background")?)
                        .ok_or("background: default, none, #RRGGBB or #AARRGGBB")?;
                }
                "text_shadow" => self.text_shadow = boolean(v, "text_shadow")?,
                "see_through" => self.see_through = boolean(v, "see_through")?,
                "align" => {
                    self.align = Align::from_name(&string(v, "align")?)
                        .ok_or("align: left, center or right")?;
                }
                "width" => {
                    self.width = v
                        .as_i64()
                        .and_then(|n| i32::try_from(n).ok())
                        .ok_or("width: a whole number")?;
                }
                "refresh" => {
                    self.refresh = match v {
                        Value::Null => None,
                        _ => Some(
                            v.as_u64()
                                .and_then(|n| u32::try_from(n).ok())
                                .ok_or("refresh: null or ticks")?,
                        ),
                    };
                }
                "visibility" => self.visibility = visibility(v, &self.id)?,
                other => return Err(format!("unknown field {other}")),
            }
        }
        self.check()
    }

    /// The hologram's text, parsed.
    #[must_use]
    pub fn template(&self) -> markup::Template {
        markup::Template::parse(&self.lines)
    }
}

/// A block and its properties as written.
#[must_use]
pub fn block_text(name: &str, props: &[(String, String)]) -> String {
    if props.is_empty() {
        return name.to_owned();
    }
    let inner: Vec<String> = props.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("{name}[{}]", inner.join(","))
}

fn in_range(v: &[f32], lo: f32, hi: f32) -> bool {
    v.iter().all(|x| x.is_finite() && *x >= lo && *x <= hi)
}

fn string(v: &Value, field: &str) -> Result<String, String> {
    v.as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{field}: a text"))
}

fn boolean(v: &Value, field: &str) -> Result<bool, String> {
    v.as_bool().ok_or_else(|| format!("{field}: true or false"))
}

#[allow(clippy::cast_possible_truncation)]
fn number(v: &Value, field: &str) -> Result<f32, String> {
    v.as_f64()
        .filter(|n| n.is_finite())
        .map(|n| n as f32)
        .ok_or_else(|| format!("{field}: a number"))
}

fn vec3(v: &Value, field: &str, uniform_ok: bool) -> Result<[f32; 3], String> {
    if uniform_ok && v.is_number() {
        let n = number(v, field)?;
        return Ok([n; 3]);
    }
    let Value::Array(a) = v else {
        return Err(format!("{field}: [x, y, z]"));
    };
    if a.len() != 3 {
        return Err(format!("{field}: [x, y, z]"));
    }
    Ok([
        number(&a[0], field)?,
        number(&a[1], field)?,
        number(&a[2], field)?,
    ])
}

fn place(v: &Value) -> Result<Place, String> {
    let Value::Object(m) = v else {
        return Err("place: {world, x, y, z}".into());
    };
    if let Some(k) = m
        .keys()
        .find(|k| !matches!(k.as_str(), "world" | "x" | "y" | "z" | "yaw" | "pitch"))
    {
        return Err(format!("place: unknown field {k}"));
    }
    let coord = |name: &str| {
        m.get(name)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("place.{name}: a number"))
    };
    let angle = |name: &str| match m.get(name) {
        None => Ok(0.0),
        Some(v) => number(v, &format!("place.{name}")),
    };
    let p = Place {
        world: m
            .get("world")
            .and_then(Value::as_str)
            .ok_or("place.world: a text")?
            .to_owned(),
        x: coord("x")?,
        y: coord("y")?,
        z: coord("z")?,
        yaw: angle("yaw")?,
        pitch: angle("pitch")?,
    };
    p.check()?;
    Ok(p)
}

fn visibility(v: &Value, id: &str) -> Result<Visibility, String> {
    let Value::Object(m) = v else {
        return Err("visibility: {mode, ...}".into());
    };
    let mode = m
        .get("mode")
        .and_then(Value::as_str)
        .ok_or("visibility.mode: a text")?;
    let allowed: &[&str] = match mode {
        "everyone" => &["mode"],
        "permission" => &["mode", "node"],
        "players" => &["mode", "players"],
        _ => return Err("visibility.mode: everyone, permission or players".into()),
    };
    if let Some(k) = m.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!("visibility: unknown field {k}"));
    }
    Ok(match mode {
        "everyone" => Visibility::Everyone,
        "permission" => Visibility::Permission(match m.get("node") {
            None | Some(Value::Null) => default_view_node(id),
            Some(n) => string(n, "visibility.node")?,
        }),
        _ => {
            let Some(Value::Array(a)) = m.get("players") else {
                return Err("visibility.players: a list of UUIDs".into());
            };
            if a.len() > MAX_VIEWERS {
                return Err(format!("visibility.players: at most {MAX_VIEWERS}"));
            }
            Visibility::Players(
                a.iter()
                    .map(|p| {
                        p.as_str()
                            .and_then(parse_uuid)
                            .ok_or_else(|| "visibility.players: UUIDs".to_owned())
                    })
                    .collect::<Result<_, _>>()?,
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn here() -> Place {
        Place {
            world: "world".into(),
            x: 10.5,
            y: 70.0,
            z: -3.5,
            yaw: 90.0,
            pitch: 0.0,
        }
    }

    #[test]
    fn a_hologram_survives_its_json() {
        let mut h = Hologram::new("shop", Kind::Text, here());
        h.lines = vec!["<gold>Shop".into(), "{online} online".into()];
        h.background = Background::Argb(0x8000_00ff);
        h.brightness = Some((15, 7));
        h.refresh = Some(20);
        h.visibility = Visibility::Players([7u128, 9].into_iter().collect());
        let mut back = Hologram::new("x", Kind::Item, here());
        back.apply_json(&h.to_json(), true).unwrap();
        assert_eq!(back, h);
        let mut b = Hologram::new("stairs", Kind::Block, here());
        b.block = "minecraft:oak_stairs[facing=east]".into();
        b.owner = Some("shop-plugin".into());
        let mut back = Hologram::new("x", Kind::Text, here());
        back.apply_json(&b.to_json(), true).unwrap();
        assert_eq!(back, b);
        assert_eq!(back.billboard, Billboard::Fixed);
    }

    #[test]
    fn requests_change_only_what_they_name_and_are_strict() {
        let mut h = Hologram::new("shop", Kind::Text, here());
        h.apply_json(
            &json!({"lines": ["a\tb"], "scale": 2, "billboard": "vertical"}),
            false,
        )
        .unwrap();
        assert_eq!(h.lines, vec!["a b".to_owned()]);
        assert_eq!(h.scale, [2.0; 3]);
        assert_eq!(h.billboard, Billboard::Vertical);
        assert_eq!(h.place, here());
        for bad in [
            json!({"colour": "red"}),
            json!({"id": "other"}),
            json!({"scale": 0}),
            json!({"scale": [1, 2]}),
            json!({"range": 1000}),
            json!({"refresh": 1}),
            json!({"background": "#12345"}),
            json!({"brightness": {"block": 16, "sky": 0}}),
            json!({"place": {"world": "w", "x": 1, "y": 2}}),
            json!({"place": {"world": "w", "x": 1e9, "y": 2, "z": 3}}),
            json!({"visibility": {"mode": "players", "players": ["nobody"]}}),
            json!({"visibility": {"mode": "secret"}}),
            json!({"lines": (0..=MAX_LINES).map(|_| "x").collect::<Vec<_>>()}),
            json!({"item": "Not An Item"}),
            json!({"block": "minecraft:stone[facing]"}),
        ] {
            let mut copy = h.clone();
            assert!(copy.apply_json(&bad, false).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_place_must_be_a_finite_position() {
        let mut h = Hologram::new("p", Kind::Text, here());
        h.place.x = f64::NAN;
        assert!(h.check().is_err());
        h.place.x = f64::INFINITY;
        assert!(h.check().is_err());
        h.place = here();
        h.place.pitch = 91.0;
        assert!(h.check().is_err());
        h.place = here();
        assert!(h.check().is_ok());
    }

    #[test]
    fn permission_visibility_gets_a_node_per_hologram() {
        let mut h = Hologram::new("vip", Kind::Text, here());
        h.apply_json(&json!({"visibility": {"mode": "permission"}}), false)
            .unwrap();
        assert_eq!(
            h.visibility,
            Visibility::Permission("holograms:view.vip".into())
        );
    }

    #[test]
    fn ids_keys_blocks_and_backgrounds() {
        assert!(
            valid_id("shop_1-a")
                && !valid_id("Shop")
                && !valid_id("")
                && !valid_id(&"a".repeat(33))
        );
        assert!(valid_stored_id("my-plugin.board") && !valid_stored_id("my-plugin.Board"));
        assert_eq!(
            normalize_key("Diamond").as_deref(),
            Some("minecraft:diamond")
        );
        assert_eq!(
            normalize_key("pack:item/sword").as_deref(),
            Some("pack:item/sword")
        );
        assert_eq!(normalize_key("a b"), None);
        assert_eq!(
            parse_block("oak_stairs[facing=east,half=top]"),
            Some((
                "minecraft:oak_stairs".into(),
                vec![
                    ("facing".into(), "east".into()),
                    ("half".into(), "top".into())
                ]
            ))
        );
        assert_eq!(parse_block("stone[facing"), None);
        assert_eq!(
            Background::parse("#ff0000"),
            Some(Background::Argb(0xffff_0000))
        );
        assert_eq!(
            Background::parse("#40000000"),
            Some(Background::Argb(0x4000_0000))
        );
        assert_eq!(Background::parse("none"), Some(Background::None));
        assert_eq!(here().chunk(), (0, -1));
    }
}
