//! The text of a hologram line: a small markup that becomes a Minecraft text component.
//!
//! - **Colours:** `<red>`, `<gold>`, ... (the sixteen Minecraft colour names), `<#ff8800>`,
//!   `<color:#ff8800>`; `&0`..`&f` and `&#ff8800` as in chat plugins (a colour code ends the
//!   formats before it, as in the game's own codes).
//! - **Formats:** `<bold>` `<b>`, `<italic>` `<i>`, `<underlined>` `<u>`, `<strikethrough>` `<st>`,
//!   `<obfuscated>` `<obf>`; `&l`, `&o`, `&n`, `&m`, `&k`; `<reset>` and `&r` end everything.
//! - **Gradients:** `<gradient:#ff0000:#0000ff>text</gradient>` (two to eight colours) and
//!   `<rainbow>text</rainbow>`, spread over the characters of the text, values included.
//! - `</name>` closes the newest open tag of that name and every tag opened after it.
//! - **Values:** `{player}` (the player who looks), `{online}`, `{max_players}`, `{world}`,
//!   `{world_time}`, `{time}` (UTC), and `{plugin:key}`, a value another plugin pushes. A value is
//!   literal text: markup inside it is never read.
//! - `\<`, `\&`, `\{` and `\\` write the character itself. An unknown tag or value stays as
//!   written.
//!
//! Everything here is plain Rust: a line is parsed once into a [`Template`], which says which
//! values it uses ([`Template::refs`]) and is rendered to the component's JSON with the values
//! of one viewer.

use serde_json::{Map, Value, json};

/// The most characters one line may have as written.
pub const MAX_LINE_CHARS: usize = 512;
/// The most characters a whole hologram shows after the values are filled in.
pub const MAX_RENDERED_CHARS: usize = 4096;
/// The most characters one value adds.
pub const MAX_VALUE_CHARS: usize = 256;

/// The sixteen Minecraft colours: name, legacy code, RGB.
const NAMED: [(&str, char, u32); 16] = [
    ("black", '0', 0x00_0000),
    ("dark_blue", '1', 0x00_00AA),
    ("dark_green", '2', 0x00_AA00),
    ("dark_aqua", '3', 0x00_AAAA),
    ("dark_red", '4', 0xAA_0000),
    ("dark_purple", '5', 0xAA_00AA),
    ("gold", '6', 0xFF_AA00),
    ("gray", '7', 0xAA_AAAA),
    ("dark_gray", '8', 0x55_5555),
    ("blue", '9', 0x55_55FF),
    ("green", 'a', 0x55_FF55),
    ("aqua", 'b', 0x55_FFFF),
    ("red", 'c', 0xFF_5555),
    ("light_purple", 'd', 0xFF_55FF),
    ("yellow", 'e', 0xFF_FF55),
    ("white", 'f', 0xFF_FFFF),
];

/// A colour: one of the sixteen names (sent by name, as the game writes it) or any RGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Named(usize),
    Rgb(u32),
}

impl Color {
    #[must_use]
    pub const fn rgb(self) -> u32 {
        match self {
            Self::Named(i) => NAMED[i].2,
            Self::Rgb(v) => v,
        }
    }

    fn json(self) -> String {
        match self {
            Self::Named(i) => NAMED[i].0.to_owned(),
            Self::Rgb(v) => format!("#{v:06x}"),
        }
    }
}

/// Reads a colour name or `#rrggbb`.
#[must_use]
pub fn parse_color(text: &str) -> Option<Color> {
    let lower = text.trim().to_ascii_lowercase();
    if let Some(i) = NAMED.iter().position(|(n, _, _)| *n == lower) {
        return Some(Color::Named(i));
    }
    let hex = lower.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(Color::Rgb)
}

/// What paints a run of text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Paint {
    Solid(Color),
    /// A gradient or rainbow of this line, by its index in [`Line::gradients`].
    Spread(usize),
}

/// The style of a run of text. Only what is set is sent; everything else is the game's default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Style {
    paint: Option<Paint>,
    bold: bool,
    italic: bool,
    underlined: bool,
    strikethrough: bool,
    obfuscated: bool,
}

/// A value a line shows.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ref {
    /// The name of the player who looks at the hologram.
    Player,
    /// Players online.
    Online,
    MaxPlayers,
    /// The name of the hologram's world.
    World,
    /// The time of day in the hologram's world, `HH:MM`.
    WorldTime,
    /// The time now in UTC, `HH:MM`.
    Time,
    /// `{plugin:key}`: a value pushed by that plugin (both in lower case).
    Pushed(String, String),
}

impl Ref {
    /// Whether the value changes without anybody telling the plugin, so it has to be read again
    /// from time to time (a refresh interval).
    #[must_use]
    pub const fn needs_refresh(&self) -> bool {
        matches!(self, Self::WorldTime | Self::Time)
    }

    fn builtin(name: &str) -> Option<Self> {
        Some(match name {
            "player" => Self::Player,
            "online" => Self::Online,
            "max_players" => Self::MaxPlayers,
            "world" => Self::World,
            "world_time" => Self::WorldTime,
            "time" => Self::Time,
            _ => return None,
        })
    }
}

/// `a-z 0-9 _ - .`, 1 to 64 characters: a pushed value's key, as the `placeholders/1` contract
/// allows it.
#[must_use]
pub fn valid_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
        })
}

#[derive(Clone, Debug, PartialEq)]
enum Piece {
    Text(String, Style),
    Value(Ref, Style),
}

#[derive(Clone, Debug, PartialEq)]
struct Gradient {
    stops: Vec<u32>,
    rainbow: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Line {
    pieces: Vec<Piece>,
    gradients: Vec<Gradient>,
}

/// A hologram's text, parsed once.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Template {
    lines: Vec<Line>,
}

/// One open tag.
#[derive(Clone, Debug)]
struct Open {
    /// The name `</name>` closes it with.
    name: String,
    /// Also closed by `</color>`.
    is_color: bool,
    style: Style,
}

/// The name a format tag is closed with, from any of its spellings.
fn format_name(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "bold" | "b" => "bold",
        "italic" | "i" | "em" => "italic",
        "underlined" | "u" => "underlined",
        "strikethrough" | "st" => "strikethrough",
        "obfuscated" | "obf" => "obfuscated",
        _ => return None,
    })
}

fn set_format(style: &mut Style, name: &str) {
    match name {
        "bold" => style.bold = true,
        "italic" => style.italic = true,
        "underlined" => style.underlined = true,
        "strikethrough" => style.strikethrough = true,
        _ => style.obfuscated = true,
    }
}

struct Parser {
    line: Line,
    stack: Vec<Open>,
    text: String,
    text_style: Style,
}

impl Parser {
    fn style(&self) -> Style {
        self.stack.last().map_or_else(Style::default, |o| o.style)
    }

    fn flush(&mut self) {
        if !self.text.is_empty() {
            let text = std::mem::take(&mut self.text);
            self.line.pieces.push(Piece::Text(text, self.text_style));
        }
    }

    fn push_char(&mut self, c: char) {
        let style = self.style();
        if style != self.text_style {
            self.flush();
            self.text_style = style;
        }
        self.text.push(c);
    }

    fn open(&mut self, name: &str, is_color: bool, change: impl FnOnce(&mut Style)) {
        let mut style = self.style();
        change(&mut style);
        self.stack.push(Open {
            name: name.to_owned(),
            is_color,
            style,
        });
    }

    /// `</name>`: closes the newest open tag of that name and all after it. False when none is
    /// open, so the text stays as written.
    fn close(&mut self, name: &str) -> bool {
        let found = self
            .stack
            .iter()
            .rposition(|o| o.name == name || (name == "color" && o.is_color));
        if let Some(i) = found {
            self.stack.truncate(i);
        }
        found.is_some()
    }

    fn gradient(&mut self, stops: Vec<u32>, rainbow: bool) -> Paint {
        self.line.gradients.push(Gradient { stops, rainbow });
        Paint::Spread(self.line.gradients.len() - 1)
    }

    /// Handles `<tag>` (without the brackets). False for a tag that is not ours.
    fn tag(&mut self, tag: &str) -> bool {
        let lower = tag.to_ascii_lowercase();
        if let Some(name) = lower.strip_prefix('/') {
            let name = format_name(name).unwrap_or(name);
            let name = if name.starts_with("gradient") {
                "gradient"
            } else {
                name
            };
            return self.close(name);
        }
        if lower == "reset" {
            self.stack.clear();
            return true;
        }
        if let Some(name) = format_name(&lower) {
            self.open(name, false, |s| set_format(s, name));
            return true;
        }
        if lower == "rainbow" {
            let paint = self.gradient(Vec::new(), true);
            self.open("rainbow", false, |s| s.paint = Some(paint));
            return true;
        }
        if let Some(rest) = lower.strip_prefix("gradient:") {
            let stops: Option<Vec<u32>> = rest
                .split(':')
                .map(|c| parse_color(c).map(Color::rgb))
                .collect();
            return match stops {
                Some(stops) if (2..=8).contains(&stops.len()) => {
                    let paint = self.gradient(stops, false);
                    self.open("gradient", false, |s| s.paint = Some(paint));
                    true
                }
                _ => false,
            };
        }
        let color_text = lower
            .strip_prefix("color:")
            .or_else(|| lower.strip_prefix("c:"))
            .unwrap_or(&lower);
        if let Some(color) = parse_color(color_text) {
            self.open(&lower, true, |s| s.paint = Some(Paint::Solid(color)));
            return true;
        }
        false
    }

    /// `&x` codes. Returns how many characters after `&` were used (0: not a code).
    fn legacy(&mut self, rest: &[char]) -> usize {
        let Some(&code) = rest.first() else {
            return 0;
        };
        let code = code.to_ascii_lowercase();
        if code == '#' && rest.len() >= 7 {
            let hex: String = rest[1..7].iter().collect();
            if let Some(color) = parse_color(&format!("#{hex}")) {
                self.stack.clear();
                self.open("color", true, |s| s.paint = Some(Paint::Solid(color)));
                return 7;
            }
            return 0;
        }
        if let Some(i) = NAMED.iter().position(|(_, c, _)| *c == code) {
            self.stack.clear();
            self.open(NAMED[i].0, true, |s| {
                s.paint = Some(Paint::Solid(Color::Named(i)))
            });
            return 1;
        }
        let name = match code {
            'l' => "bold",
            'o' => "italic",
            'n' => "underlined",
            'm' => "strikethrough",
            'k' => "obfuscated",
            'r' => {
                self.stack.clear();
                return 1;
            }
            _ => return 0,
        };
        self.open(name, false, |s| set_format(s, name));
        1
    }

    /// `{name}` or `{plugin:key}`. Returns how many characters after `{` were used.
    fn value(&mut self, rest: &[char]) -> usize {
        let Some(end) = rest.iter().take(130).position(|c| *c == '}') else {
            return 0;
        };
        let inner: String = rest[..end].iter().collect();
        let found = Ref::builtin(&inner).or_else(|| {
            let (plugin, key) = inner.split_once(':')?;
            let plugin_ok = (1..=64).contains(&plugin.len())
                && plugin.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
                });
            (plugin_ok && valid_key(key)).then(|| Ref::Pushed(plugin.to_owned(), key.to_owned()))
        });
        let Some(found) = found else {
            return 0;
        };
        self.flush();
        self.text_style = self.style();
        self.line.pieces.push(Piece::Value(found, self.style()));
        end + 1
    }
}

fn parse_line(text: &str) -> Line {
    let chars: Vec<char> = text.chars().collect();
    let mut p = Parser {
        line: Line::default(),
        stack: Vec::new(),
        text: String::new(),
        text_style: Style::default(),
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if matches!(chars.get(i + 1), Some('<' | '&' | '{' | '\\')) => {
                p.push_char(chars[i + 1]);
                i += 2;
            }
            '<' => {
                let end = chars[i + 1..].iter().take(80).position(|c| *c == '>');
                let handled = end.is_some_and(|end| {
                    let tag: String = chars[i + 1..i + 1 + end].iter().collect();
                    !tag.is_empty() && p.tag(&tag)
                });
                if handled {
                    i += end.unwrap_or(0) + 2;
                } else {
                    p.push_char('<');
                    i += 1;
                }
            }
            '&' => {
                let used = p.legacy(&chars[i + 1..]);
                if used == 0 {
                    p.push_char('&');
                }
                i += 1 + used;
            }
            '{' => {
                let used = p.value(&chars[i + 1..]);
                if used == 0 {
                    p.push_char('{');
                }
                i += 1 + used;
            }
            _ => {
                p.push_char(c);
                i += 1;
            }
        }
    }
    p.flush();
    p.line
}

/// Turns typed text into one safe line: control characters become spaces, at most
/// [`MAX_LINE_CHARS`] characters are kept.
#[must_use]
pub fn clean_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_LINE_CHARS)
        .collect()
}

impl Template {
    /// Parses the lines of a hologram.
    #[must_use]
    pub fn parse(lines: &[String]) -> Self {
        Self {
            lines: lines.iter().map(|l| parse_line(l)).collect(),
        }
    }

    /// Every value the text uses, each once.
    #[must_use]
    pub fn refs(&self) -> Vec<Ref> {
        let mut out: Vec<Ref> = self
            .lines
            .iter()
            .flat_map(|l| &l.pieces)
            .filter_map(|p| match p {
                Piece::Value(r, _) => Some(r.clone()),
                Piece::Text(..) => None,
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The text without markup, values shown as `{name}`: for `/holo list` and `/holo info`.
    #[must_use]
    pub fn plain(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|l| {
                l.pieces
                    .iter()
                    .map(|p| match p {
                        Piece::Text(t, _) => t.clone(),
                        Piece::Value(Ref::Pushed(plugin, key), _) => format!("{{{plugin}:{key}}}"),
                        Piece::Value(r, _) => format!("{{{}}}", builtin_name(r)),
                    })
                    .collect()
            })
            .collect()
    }

    /// The JSON of the text component for one viewer. `value` answers a value, `None` when it
    /// is not known (shown as nothing).
    #[must_use]
    pub fn render(&self, value: &dyn Fn(&Ref) -> Option<String>) -> String {
        let mut extra: Vec<Value> = Vec::new();
        let mut budget = MAX_RENDERED_CHARS;
        for (n, line) in self.lines.iter().enumerate() {
            if n > 0 {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                push_segment(&mut extra, "\n".to_owned(), None, Style::default());
            }
            render_line(line, value, &mut budget, &mut extra);
        }
        let mut root = Map::new();
        root.insert("text".into(), json!(""));
        if !extra.is_empty() {
            root.insert("extra".into(), Value::Array(extra));
        }
        Value::Object(root).to_string()
    }
}

/// The words of a component [`Template::render`] made, without styles; lines joined with ` / `.
#[must_use]
pub fn plain_of(json_text: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(json_text) else {
        return String::new();
    };
    let mut out = String::new();
    if let Some(Value::Array(extra)) = value.get("extra") {
        for e in extra {
            if let Some(t) = e.get("text").and_then(Value::as_str) {
                out.push_str(t);
            }
        }
    }
    out.replace('\n', " / ")
}

fn builtin_name(r: &Ref) -> &'static str {
    match r {
        Ref::Player => "player",
        Ref::Online => "online",
        Ref::MaxPlayers => "max_players",
        Ref::World => "world",
        Ref::WorldTime => "world_time",
        Ref::Time => "time",
        Ref::Pushed(..) => "",
    }
}

/// A value as literal text: one line, bounded.
fn clean_value(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_VALUE_CHARS)
        .collect()
}

fn render_line(
    line: &Line,
    value: &dyn Fn(&Ref) -> Option<String>,
    budget: &mut usize,
    extra: &mut Vec<Value>,
) {
    // The runs with the values filled in, cut to the budget.
    let mut runs: Vec<(String, Style)> = Vec::new();
    for piece in &line.pieces {
        if *budget == 0 {
            break;
        }
        let (text, style) = match piece {
            Piece::Text(t, s) => (t.clone(), *s),
            Piece::Value(r, s) => (value(r).map(|v| clean_value(&v)).unwrap_or_default(), *s),
        };
        let text: String = text.chars().take(*budget).collect();
        *budget -= text.chars().count();
        if !text.is_empty() {
            runs.push((text, style));
        }
    }
    // Characters per gradient, for spreading its colours.
    let mut sizes = vec![0usize; line.gradients.len()];
    for (text, style) in &runs {
        if let Some(Paint::Spread(g)) = style.paint {
            sizes[g] += text.chars().count();
        }
    }
    let mut seen = vec![0usize; line.gradients.len()];
    for (text, style) in runs {
        match style.paint {
            Some(Paint::Spread(g)) => {
                for c in text.chars() {
                    let rgb = spread(&line.gradients[g], seen[g], sizes[g]);
                    seen[g] += 1;
                    push_segment(extra, c.to_string(), Some(Color::Rgb(rgb)), style);
                }
            }
            Some(Paint::Solid(color)) => push_segment(extra, text, Some(color), style),
            None => push_segment(extra, text, None, style),
        }
    }
}

/// The colour of character `i` of `n` in a gradient or rainbow.
fn spread(g: &Gradient, i: usize, n: usize) -> u32 {
    #[allow(clippy::cast_precision_loss)]
    let t = if n <= 1 {
        0.0
    } else {
        i as f64 / (n - 1) as f64
    };
    if g.rainbow {
        return hue(t * 300.0);
    }
    let last = g.stops.len() - 1;
    #[allow(clippy::cast_precision_loss)]
    let pos = t * last as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let k = (pos.floor() as usize).min(last.saturating_sub(1));
    #[allow(clippy::cast_precision_loss)]
    let local = pos - k as f64;
    mix(g.stops[k], g.stops[(k + 1).min(last)], local)
}

fn mix(a: u32, b: u32, t: f64) -> u32 {
    let channel = |shift: u32| {
        let x = f64::from((a >> shift) & 0xff);
        let y = f64::from((b >> shift) & 0xff);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let v = (x + (y - x) * t).round().clamp(0.0, 255.0) as u32;
        v << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// A fully saturated colour of hue `h` degrees.
fn hue(h: f64) -> u32 {
    let x = 1.0 - ((h / 60.0) % 2.0 - 1.0).abs();
    let (r, g, b) = match (h / 60.0).floor() as i64 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let byte = |v: f64| (v * 255.0).round() as u32;
    (byte(r) << 16) | (byte(g) << 8) | byte(b)
}

/// Appends a run, joined to the one before when both look the same.
fn push_segment(extra: &mut Vec<Value>, text: String, color: Option<Color>, style: Style) {
    let mut obj = Map::new();
    if let Some(color) = color {
        obj.insert("color".into(), json!(color.json()));
    }
    for (set, name) in [
        (style.bold, "bold"),
        (style.italic, "italic"),
        (style.underlined, "underlined"),
        (style.strikethrough, "strikethrough"),
        (style.obfuscated, "obfuscated"),
    ] {
        if set {
            obj.insert(name.into(), json!(true));
        }
    }
    if let Some(Value::Object(last)) = extra.last_mut() {
        let same = last.len() == obj.len() + 1 && obj.iter().all(|(k, v)| last.get(k) == Some(v));
        if same && let Some(Value::String(t)) = last.get_mut("text") {
            t.push_str(&text);
            return;
        }
    }
    obj.insert("text".into(), json!(text));
    extra.push(Value::Object(obj));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(line: &str) -> Template {
        Template::parse(&[line.to_owned()])
    }

    fn render(line: &str) -> Value {
        serde_json::from_str(&one(line).render(&|_| None)).unwrap()
    }

    fn render_with(line: &str, f: &dyn Fn(&Ref) -> Option<String>) -> Value {
        serde_json::from_str(&one(line).render(f)).unwrap()
    }

    #[test]
    fn plain_text_is_one_child_and_empty_text_has_none() {
        assert_eq!(
            render("Hello"),
            json!({"text":"","extra":[{"text":"Hello"}]})
        );
        assert_eq!(render(""), json!({"text":""}));
    }

    #[test]
    fn tags_and_codes_make_colours_and_formats() {
        assert_eq!(
            render("<red>Shop</red> <bold>open"),
            json!({"text":"","extra":[
                {"text":"Shop","color":"red"},
                {"text":" "},
                {"text":"open","bold":true}
            ]})
        );
        assert_eq!(
            render("&6&lGold &rplain &#00ff88hex"),
            json!({"text":"","extra":[
                {"text":"Gold ","color":"gold","bold":true},
                {"text":"plain "},
                {"text":"hex","color":"#00ff88"}
            ]})
        );
        assert_eq!(
            render("<#123abc><i>a</i>b</#123abc>c"),
            json!({"text":"","extra":[
                {"text":"a","color":"#123abc","italic":true},
                {"text":"b","color":"#123abc"},
                {"text":"c"}
            ]})
        );
        // A colour code ends the formats before it, as the game's own codes do.
        assert_eq!(
            render("&lA&cB"),
            json!({"text":"","extra":[{"text":"A","bold":true},{"text":"B","color":"red"}]})
        );
    }

    #[test]
    fn closing_a_tag_closes_the_ones_opened_after_it() {
        assert_eq!(
            render("<blue>a<u>b</blue>c"),
            json!({"text":"","extra":[
                {"text":"a","color":"blue"},
                {"text":"b","color":"blue","underlined":true},
                {"text":"c"}
            ]})
        );
    }

    #[test]
    fn unknown_tags_escapes_and_lone_signs_stay_as_written() {
        assert_eq!(
            render("<nope>x</b> 5 < 6 & \\<red> \\{player} {x} {bad key:k} a&zb"),
            json!({"text":"","extra":[{"text":"<nope>x</b> 5 < 6 & <red> {player} {x} {bad key:k} a&zb"}]})
        );
    }

    #[test]
    fn gradients_spread_over_the_characters() {
        let v = render("<gradient:#000000:#ffffff>abc</gradient>!");
        assert_eq!(
            v,
            json!({"text":"","extra":[
                {"text":"a","color":"#000000"},
                {"text":"b","color":"#808080"},
                {"text":"c","color":"#ffffff"},
                {"text":"!"}
            ]})
        );
        let r = render("<rainbow>ab</rainbow>");
        assert_eq!(r["extra"][0]["color"], json!("#ff0000"));
        assert_eq!(r["extra"].as_array().unwrap().len(), 2);
        // Too few colours: not a gradient.
        assert_eq!(
            render("<gradient:#000000>x"),
            json!({"text":"","extra":[{"text":"<gradient:#000000>x"}]})
        );
    }

    #[test]
    fn a_gradient_spreads_over_a_value_too() {
        let v = render_with("<gradient:red:blue>{online}</gradient>", &|r| {
            (r == &Ref::Online).then(|| "123".to_owned())
        });
        let colors: Vec<_> = v["extra"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["color"].clone())
            .collect();
        assert_eq!(
            colors,
            vec![json!("#ff5555"), json!("#aa55aa"), json!("#5555ff")]
        );
    }

    #[test]
    fn values_are_literal_text_with_the_style_around_them() {
        let t = one("<gold>Hi {player}, {shop:balance} coins {missing:x}");
        assert_eq!(
            t.refs(),
            vec![
                Ref::Player,
                Ref::Pushed("missing".into(), "x".into()),
                Ref::Pushed("shop".into(), "balance".into())
            ]
        );
        let v: Value = serde_json::from_str(&t.render(&|r| match r {
            Ref::Player => Some("<red>Alex&l".into()),
            Ref::Pushed(_, k) if k == "balance" => Some("1,234\nnew".into()),
            _ => None,
        }))
        .unwrap();
        assert_eq!(
            v,
            json!({"text":"","extra":[{"text":"Hi <red>Alex&l, 1,234 new coins ","color":"gold"}]})
        );
    }

    #[test]
    fn lines_are_joined_with_line_breaks_and_the_total_is_bounded() {
        let t = Template::parse(&["<red>a".to_owned(), "b".to_owned()]);
        assert_eq!(
            serde_json::from_str::<Value>(&t.render(&|_| None)).unwrap(),
            json!({"text":"","extra":[{"text":"a","color":"red"},{"text":"\nb"}]})
        );
        let long = "x".repeat(MAX_LINE_CHARS);
        let lines: Vec<String> = (0..20).map(|_| long.clone()).collect();
        let v: Value = serde_json::from_str(&Template::parse(&lines).render(&|_| None)).unwrap();
        let shown: usize = v["extra"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["text"].as_str().unwrap().chars().count())
            .sum();
        assert_eq!(shown, MAX_RENDERED_CHARS);
    }

    #[test]
    fn plain_text_and_refresh_needs() {
        assert_eq!(
            plain_of(
                &Template::parse(&["<red>a{online}".to_owned(), "&lb".to_owned()])
                    .render(&|_| Some("7".into()))
            ),
            "a7 / b"
        );
        let t = one("&cTime {world_time} {a:b}");
        assert_eq!(t.plain(), vec!["Time {world_time} {a:b}".to_owned()]);
        assert!(t.refs().iter().any(Ref::needs_refresh));
        assert!(!one("{online}").refs().iter().any(Ref::needs_refresh));
        assert_eq!(clean_line("a\tb\nc"), "a b c");
        assert_eq!(clean_line(&"y".repeat(600)).len(), MAX_LINE_CHARS);
    }

    #[test]
    fn colours_parse_by_name_and_hex() {
        assert_eq!(parse_color("Gold"), Some(Color::Named(6)));
        assert_eq!(parse_color("#FF8800"), Some(Color::Rgb(0xff8800)));
        assert_eq!(parse_color("#ff88"), None);
        assert_eq!(parse_color("pink"), None);
    }
}
