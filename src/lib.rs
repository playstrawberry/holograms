//! holograms: text, item and block holograms for Strawberry servers.
//!
//! - **What** a hologram is: [`model`] (and [`store`], the save file). Text in a small markup
//!   ([`markup`]) with colours, gradients and formats, values other plugins push ([`values`],
//!   the `placeholders/1` contract) and built-in ones.
//! - **How** it is shown: the game's display entities ([`show`]), managed by [`runtime`]: one
//!   entity everybody sees, or entities for single players (chosen players, a permission, a text
//!   that differs per viewer). Nothing runs per tick when nothing changes.
//! - **Who** changes it: the `/holo` command (this file) and other plugins through plugin
//!   messages ([`api`]). Every message in English, French, Spanish and Turkish ([`lang`]).
//!
//! No lock is held while a call could come back into the plugin: the plugin listens to no event
//! that its own calls fire.

pub mod api;
pub mod edition;
pub mod lang;
pub mod markup;
pub mod model;
pub mod runtime;
pub mod show;
pub mod store;
pub mod values;

use std::sync::{LazyLock, Mutex, PoisonError};

use serde_json::{Value, json};
use strawberry_plugin_api::command::{
    Arg, ArgumentType, Command, CommandError, CommandNode, CommandSender, CommandSuggestion,
    CommandSuggestions, ConsumedArgs, StringType, SuggestionRequest,
};
use strawberry_plugin_api::command_wit::Number;
use strawberry_plugin_api::commands::{CommandHandler, CommandSuggestionHandler};
use strawberry_plugin_api::common::Hand;
use strawberry_plugin_api::events::{
    EventData, EventHandler, FailureVerdict, HandlerOptions, HandlerPriority,
    PlayerChangedWorldEvent, PlayerJoinEvent, PlayerLeaveEvent, WorldLoadEvent, WorldUnloadEvent,
};
use strawberry_plugin_api::logging::{LogLevel, log};
use strawberry_plugin_api::permission::{
    Permission, PermissionChild, PermissionDefault, PermissionLevel,
};
use strawberry_plugin_api::scheduler::SchedulerExt;
use strawberry_plugin_api::text::TextComponent;
use strawberry_plugin_api::{
    Context, Player, Plugin, PluginMetadata, Result, Server, permissions, register_plugin,
};

use model::{Align, Background, Billboard, Hologram, Kind, Place, Visibility};
use runtime::{State, Status, id_of};
use values::uuid_text;

const NODE_COMMAND: &str = "holograms:command";
const NODE_VIEW: &str = "holograms:view";
const NODE_CREATE: &str = "holograms:create";
const NODE_EDIT: &str = "holograms:edit";
const NODE_DELETE: &str = "holograms:delete";
const NODE_RELOAD: &str = "holograms:reload";
const NODE_ADMIN: &str = "holograms:admin";
/// A new hologram stands this far above the feet of the player who makes it.
const SPAWN_HEIGHT: f64 = 1.6;
/// The save file is written this many ticks after a change (several changes, one write).
const SAVE_DELAY: u64 = 40;
const LIST_PAGE: usize = 10;
const NEAR_DEFAULT: f64 = 16.0;
const NEAR_LINES: usize = 20;

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));
/// The server from `on_load`, for plugin messages (which carry none).
static SERVER: Mutex<Option<Server>> = Mutex::new(None);

/// Runs `f` on the plugin's state. Never called while it is already held.
pub(crate) fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let mut guard = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut guard)
}

fn with_server<T>(f: impl FnOnce(&Server) -> T) -> Option<T> {
    SERVER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(f)
}

// ----- saving -----

fn write_file(path: &str, text: &str) -> std::result::Result<(), String> {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Marks the records changed; the file is written off the tick a little later.
fn mark_dirty(st: &mut State, server: &Server) {
    st.dirty = true;
    if !st.save_scheduled {
        st.save_scheduled = true;
        server.schedule_delayed_async_task(SAVE_DELAY, |_| save_now());
    }
}

fn save_now() {
    let text = with_state(|st| {
        st.save_scheduled = false;
        if !st.dirty || st.broken.is_some() {
            return None;
        }
        st.dirty = false;
        Some(store::encode(st.holos.values()))
    });
    if let Some(text) = text
        && let Err(e) = write_file(store::FILE, &text)
    {
        log(
            LogLevel::Error,
            &format!("holograms: could not save {}: {e}", store::FILE),
        );
        with_state(|st| st.dirty = true);
    }
}

/// Reads the save file: the holograms, or why it cannot be read.
fn read_file() -> std::result::Result<Vec<Hologram>, String> {
    match std::fs::read_to_string(store::FILE) {
        Ok(text) => store::decode(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.to_string()),
    }
}

// ----- events -----

/// Joins, quits and world changes only observe: they run off the event and cost it nothing.
const fn observer() -> HandlerOptions {
    HandlerOptions {
        priority: HandlerPriority::Monitor,
        blocking: false,
        ignore_cancelled: false,
        on_failure: FailureVerdict::Allow,
    }
}

struct Join;
impl EventHandler<PlayerJoinEvent> for Join {
    fn handle(
        &self,
        server: Server,
        event: EventData<PlayerJoinEvent>,
    ) -> EventData<PlayerJoinEvent> {
        // A handle of our own: the event's handle goes back with the event.
        if let Some(player) = server.get_player_by_uuid(event.player.get_id()) {
            with_state(|st| st.join(&server, player));
        }
        event
    }
}

struct Leave;
impl EventHandler<PlayerLeaveEvent> for Leave {
    fn handle(
        &self,
        server: Server,
        event: EventData<PlayerLeaveEvent>,
    ) -> EventData<PlayerLeaveEvent> {
        let pid = id_of(&event.player);
        with_state(|st| st.leave(&server, pid));
        event
    }
}

struct ChangedWorld;
impl EventHandler<PlayerChangedWorldEvent> for ChangedWorld {
    fn handle(
        &self,
        server: Server,
        event: EventData<PlayerChangedWorldEvent>,
    ) -> EventData<PlayerChangedWorldEvent> {
        if let Some(player) = server.get_player_by_uuid(event.player.get_id()) {
            with_state(|st| st.changed_world(player));
        }
        event
    }
}

struct WorldLoad;
impl EventHandler<WorldLoadEvent> for WorldLoad {
    fn handle(
        &self,
        server: Server,
        event: EventData<WorldLoadEvent>,
    ) -> EventData<WorldLoadEvent> {
        let _ = (&server, &event.target_world);
        with_state(State::world_loaded);
        event
    }
}

struct WorldUnload;
impl EventHandler<WorldUnloadEvent> for WorldUnload {
    fn handle(
        &self,
        _server: Server,
        event: EventData<WorldUnloadEvent>,
    ) -> EventData<WorldUnloadEvent> {
        let name = event.target_world.get_name();
        with_state(|st| st.world_unloading(&name));
        event
    }
}

// ----- plugin messages -----

fn handle_message(from: &str, message: &[u8]) -> Vec<u8> {
    let plugin = from.to_ascii_lowercase();
    if values::is_contract(message) {
        return handle_values(&plugin, message);
    }
    let request = match api::parse(message) {
        Ok(r) => r,
        Err(e) => return api::error(&e),
    };
    with_server(|server| with_state(|st| answer(st, server, &plugin, request)))
        .unwrap_or_else(|| api::error("the plugin is starting"))
}

fn handle_values(plugin: &str, message: &[u8]) -> Vec<u8> {
    let refuse = |reason: &str| format!("{}\terror\t{reason}", values::CONTRACT).into_bytes();
    let pushes = match values::parse(message) {
        Ok(values::Message::Alive) => return format!("{}\tok", values::CONTRACT).into_bytes(),
        Ok(values::Message::Push(p)) => p,
        Err(e) => return refuse(&e),
    };
    let result = with_server(|server| {
        with_state(|st| {
            let changed = st.values.apply(plugin, &pushes)?;
            if !changed.is_empty() {
                st.values_changed(server, plugin, &changed);
            }
            Ok::<(), String>(())
        })
    });
    match result {
        Some(Ok(())) => format!("{}\tok", values::CONTRACT).into_bytes(),
        Some(Err(e)) => refuse(&e),
        None => refuse("the plugin is starting"),
    }
}

fn owned_by(st: &State, plugin: &str, key: &str) -> std::result::Result<Hologram, Vec<u8>> {
    let id = model::api_id(plugin, key);
    st.holos
        .get(&id)
        .cloned()
        .ok_or_else(|| api::error("no such hologram"))
}

/// Puts a changed record in and shows it.
fn commit(st: &mut State, server: &Server, h: Hologram) {
    let id = h.id.clone();
    let old = st.put(h);
    st.changed(server, &id, old.as_ref());
    mark_dirty(st, server);
}

fn answer(st: &mut State, server: &Server, plugin: &str, request: api::Request) -> Vec<u8> {
    use api::Request as R;
    if st.broken.is_some() && !matches!(request, R::Version | R::List | R::Get { .. }) {
        return api::error(
            "the hologram file could not be read; nothing changes until it is fixed",
        );
    }
    match request {
        R::Version => api::ok(&[("api", json!(api::API_VERSION))]),
        R::List => {
            let prefix = format!("{plugin}.");
            let keys: Vec<&str> = st
                .holos
                .keys()
                .filter_map(|id| id.strip_prefix(&prefix))
                .collect();
            api::ok(&[("keys", json!(keys))])
        }
        R::Get { key } => match owned_by(st, plugin, &key) {
            Ok(h) => api::ok(&[("found", json!(true)), ("hologram", h.to_json())]),
            Err(_) => api::ok(&[("found", json!(false))]),
        },
        R::Create { key, spec, persist } => {
            let id = model::api_id(plugin, &key);
            let replaced = st.holos.contains_key(&id);
            if !replaced {
                if st.holos.len() >= model::MAX_HOLOGRAMS {
                    return api::error("the server holds the most holograms it keeps");
                }
                let prefix = format!("{plugin}.");
                if st.holos.keys().filter(|k| k.starts_with(&prefix)).count()
                    >= model::MAX_PER_PLUGIN
                {
                    return api::error("this plugin holds the most holograms it may");
                }
            }
            let kind = spec
                .get("kind")
                .and_then(Value::as_str)
                .and_then(Kind::from_name)
                .unwrap_or(Kind::Text);
            let mut h = Hologram::new(
                &id,
                kind,
                Place {
                    world: String::new(),
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                    yaw: 0.0,
                    pitch: 0.0,
                },
            );
            h.owner = Some(plugin.to_owned());
            h.persist = persist;
            if let Err(e) = h.apply_json(&spec, false) {
                return api::error(&e);
            }
            if let Err(e) = known_content(&h) {
                return api::error(&e);
            }
            if replaced && let Some(old) = st.take(&id) {
                // A new hologram under the same key: the old one's entities go first, so it
                // never doubles.
                st.detach(&id, true);
                st.put(h);
                st.changed(server, &id, Some(&old));
                mark_dirty(st, server);
            } else {
                commit(st, server, h);
            }
            api::ok(&[("id", json!(id)), ("replaced", json!(replaced))])
        }
        R::Update { key, spec } => {
            api_edit(st, server, plugin, &key, |h| h.apply_json(&spec, false))
        }
        R::Lines { key, lines } => api_edit(st, server, plugin, &key, |h| {
            h.apply_json(&json!({ "lines": lines }), false)
        }),
        R::Move { key, place } => api_edit(st, server, plugin, &key, |h| {
            h.apply_json(&json!({ "place": place }), false)
        }),
        R::Show { key, player, show } => api_edit(st, server, plugin, &key, |h| {
            let Visibility::Players(set) = &mut h.visibility else {
                return Err("the hologram's visibility is not players".into());
            };
            if show {
                set.insert(player);
            } else {
                set.remove(&player);
            }
            h.check()
        }),
        R::Delete { key } => {
            let id = model::api_id(plugin, &key);
            match st.take(&id) {
                Some(old) => {
                    st.removed(server, &old);
                    mark_dirty(st, server);
                    api::ok(&[("deleted", json!(true))])
                }
                None => api::ok(&[("deleted", json!(false))]),
            }
        }
    }
}

fn api_edit(
    st: &mut State,
    server: &Server,
    plugin: &str,
    key: &str,
    change: impl FnOnce(&mut Hologram) -> std::result::Result<(), String>,
) -> Vec<u8> {
    let mut h = match owned_by(st, plugin, key) {
        Ok(h) => h,
        Err(e) => return e,
    };
    if let Err(e) = change(&mut h).and_then(|()| known_content(&h)) {
        return api::error(&e);
    }
    commit(st, server, h);
    api::ok(&[])
}

/// The item or block the server must know.
fn known_content(h: &Hologram) -> std::result::Result<(), String> {
    match h.kind {
        Kind::Item if !show::item_exists(&h.item) => Err(format!("unknown item {}", h.item)),
        Kind::Block if show::block_state(&h.block).is_none() => {
            Err(format!("unknown block {}", h.block))
        }
        _ => Ok(()),
    }
}

// ----- the command -----

fn sender_language(sender: &CommandSender) -> &'static str {
    sender
        .as_player()
        .map_or("en", |p| lang::language(&p.get_locale()))
}

fn text(sender: &CommandSender, key: &str, args: &[&str]) -> String {
    lang::text(sender_language(sender), key, args)
}

fn say(sender: &CommandSender, key: &str, args: &[&str]) {
    sender.send_message(TextComponent::text(&text(sender, key, args)));
}

fn fail(sender: &CommandSender, key: &str, args: &[&str]) -> CommandError {
    CommandError::CommandFailed(TextComponent::text(&text(sender, key, args)))
}

fn allowed(
    sender: &CommandSender,
    server: &Server,
    node: &str,
) -> std::result::Result<(), CommandError> {
    if sender.is_console()
        || sender.has_permission(server, node)
        || sender.has_permission(server, NODE_ADMIN)
    {
        Ok(())
    } else {
        Err(CommandError::CommandFailed(TextComponent::translate(
            "commands.help.failed",
            vec![],
        )))
    }
}

fn word(args: &ConsumedArgs, key: &str) -> String {
    match args.get_value(key) {
        Arg::Simple(s) | Arg::Msg(s) | Arg::Item(s) | Arg::Block(s) | Arg::ResourceLocation(s) => s,
        _ => String::new(),
    }
}

#[allow(clippy::cast_precision_loss)]
fn num(args: &ConsumedArgs, key: &str) -> Option<f64> {
    match args.get_value(key) {
        Arg::Num(Ok(n)) => Some(match n {
            Number::Float64(v) => v,
            Number::Float32(v) => f64::from(v),
            Number::Int32(v) => f64::from(v),
            Number::Int64(v) => v as f64,
        }),
        _ => None,
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn count(args: &ConsumedArgs, key: &str) -> Option<usize> {
    num(args, key).filter(|n| *n >= 0.0).map(|n| n as usize)
}

#[allow(clippy::cast_possible_truncation)]
fn float(args: &ConsumedArgs, key: &str) -> Option<f32> {
    num(args, key).map(|n| n as f32)
}

fn flag(args: &ConsumedArgs, key: &str) -> Option<bool> {
    match args.get_value(key) {
        Arg::Bool(b) => Some(b),
        _ => None,
    }
}

fn players(args: &ConsumedArgs, key: &str) -> Vec<Player> {
    match args.get_value(key) {
        Arg::Players(p) => p,
        _ => Vec::new(),
    }
}

/// Where the sender stands: (world, position above the feet, yaw).
fn sender_place(sender: &CommandSender) -> Option<Place> {
    let p = sender.as_player()?;
    let (x, y, z) = p.get_position();
    Some(Place {
        world: p.get_world().get_name(),
        x,
        y: y + SPAWN_HEIGHT,
        z,
        yaw: p.get_yaw(),
        pitch: 0.0,
    })
}

fn yes_no(sender: &CommandSender, v: bool) -> String {
    text(sender, if v { "yes" } else { "no" }, &[])
}

fn fmt(v: f64) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" {
        "0".to_owned()
    } else {
        s.to_owned()
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Usage,
    CreateText,
    CreateItem,
    CreateBlock,
    Delete,
    List,
    Near,
    Info,
    Copy,
    Move,
    Reload,
    Status,
    SetLine,
    AddLine,
    InsertLine,
    RemoveLine,
    Billboard(Billboard),
    Scale,
    Rotation,
    Offset,
    Brightness,
    BrightnessDefault,
    Range,
    Shadow,
    Background,
    TextShadow,
    SeeThrough,
    Align(Align),
    Width,
    Refresh,
    RefreshOff,
    Item,
    Block,
    Everyone,
    Permission,
    Players,
    ViewerAdd,
    ViewerRemove,
}

struct Cmd(Op);

impl CommandHandler for Cmd {
    fn handle(
        &self,
        sender: CommandSender,
        server: Server,
        args: ConsumedArgs,
    ) -> std::result::Result<i32, CommandError> {
        run(self.0, &sender, &server, &args)
    }
}

/// Hologram ids for tab completion.
struct Ids;

impl CommandSuggestionHandler for Ids {
    fn suggest(
        &self,
        _sender: CommandSender,
        _server: Server,
        request: SuggestionRequest,
    ) -> CommandSuggestions {
        let prefix = request.remaining.to_ascii_lowercase();
        let values = with_state(|st| {
            st.holos
                .keys()
                .filter(|id| id.starts_with(&prefix))
                .take(100)
                .map(|id| CommandSuggestion {
                    value: id.clone(),
                    tooltip: None,
                })
                .collect()
        });
        CommandSuggestions {
            start: request.start,
            length: u32::try_from(request.remaining.len()).unwrap_or(0),
            values,
        }
    }
}

fn run(
    op: Op,
    sender: &CommandSender,
    server: &Server,
    args: &ConsumedArgs,
) -> std::result::Result<i32, CommandError> {
    match op {
        Op::Usage => {
            say(sender, "usage", &[]);
            Ok(1)
        }
        Op::CreateText | Op::CreateItem | Op::CreateBlock => create(op, sender, server, args),
        Op::Delete => delete(sender, server, args),
        Op::List => list(sender, server, args),
        Op::Near => near(sender, server, args),
        Op::Info => info(sender, server, args),
        Op::Copy => copy(sender, server, args),
        Op::Reload => reload(sender, server),
        Op::Status => {
            allowed(sender, server, NODE_VIEW)?;
            let (numbers, sends) = with_state(|st| (st.summary(), st.sends));
            let [holos, live, copies, chunks, scans] = numbers.map(|n| n.to_string());
            say(
                sender,
                "status",
                &[&holos, &live, &copies, &chunks, &scans, &sends.to_string()],
            );
            Ok(1)
        }
        Op::Move => {
            let place = match args.get_value("pos") {
                Arg::Pos3d((x, y, z)) => {
                    let world = sender
                        .as_player()
                        .map(|p| p.get_world().get_name())
                        .or_else(|| sender.world().map(|w| w.get_name()));
                    world.map(|world| Place {
                        world,
                        x,
                        y,
                        z,
                        yaw: 0.0,
                        pitch: 0.0,
                    })
                }
                _ => sender_place(sender),
            };
            let Some(place) = place else {
                return Err(fail(sender, "need_player", &[]));
            };
            edit(sender, server, args, NODE_EDIT, "moved", |h| {
                let yaw = if args_has_pos(args) {
                    h.place.yaw
                } else {
                    place.yaw
                };
                h.place = Place {
                    yaw,
                    pitch: h.place.pitch,
                    ..place.clone()
                };
                Ok(())
            })
        }
        _ => edit_op(op, sender, server, args),
    }
}

fn args_has_pos(args: &ConsumedArgs) -> bool {
    matches!(args.get_value("pos"), Arg::Pos3d(_))
}

/// A refusal of an edit: a language key and its arguments.
type Refusal = (&'static str, Vec<String>);

fn edit(
    sender: &CommandSender,
    server: &Server,
    args: &ConsumedArgs,
    node: &str,
    done: &str,
    change: impl FnOnce(&mut Hologram) -> std::result::Result<(), Refusal>,
) -> std::result::Result<i32, CommandError> {
    allowed(sender, server, node)?;
    let id = word(args, "id").to_ascii_lowercase();
    let result = with_state(|st| {
        if let Some(reason) = &st.broken {
            return Err(("file_broken", vec![reason.clone()]));
        }
        let Some(mut h) = st.holos.get(&id).cloned() else {
            return Err(("no_such", vec![id.clone()]));
        };
        if let Some(owner) = &h.owner {
            return Err(("owned", vec![id.clone(), owner.clone()]));
        }
        change(&mut h)?;
        if h.check().is_err() {
            return Err(("bad_value", vec![id.clone()]));
        }
        commit(st, server, h);
        Ok(())
    });
    match result {
        Ok(()) => {
            say(sender, done, &[&id]);
            Ok(1)
        }
        Err((key, a)) => {
            let a: Vec<&str> = a.iter().map(String::as_str).collect();
            Err(fail(sender, key, &a))
        }
    }
}

fn refusal(key: &'static str, args: &[&str]) -> Refusal {
    (key, args.iter().map(|s| (*s).to_owned()).collect())
}

fn only(h: &Hologram, kind: Kind) -> std::result::Result<(), Refusal> {
    if h.kind == kind {
        return Ok(());
    }
    Err(refusal(
        match kind {
            Kind::Text => "only_text",
            Kind::Item => "only_item",
            Kind::Block => "only_block",
        },
        &[],
    ))
}

fn line_text(args: &ConsumedArgs) -> String {
    markup::clean_line(&word(args, "text"))
}

#[allow(clippy::too_many_lines)]
fn edit_op(
    op: Op,
    sender: &CommandSender,
    server: &Server,
    args: &ConsumedArgs,
) -> std::result::Result<i32, CommandError> {
    // What needs the server or the sender is read before the records are locked.
    let item = match op {
        Op::Item => {
            let typed = word(args, "item");
            let key = if typed.is_empty() {
                sender
                    .as_player()
                    .and_then(|p| p.get_item_in_hand(Hand::Right))
                    .and_then(|s| model::normalize_key(&s.get_registry_key()))
            } else {
                model::normalize_key(&typed)
            };
            match key.filter(|k| show::item_exists(k) && k.as_str() != "minecraft:air") {
                Some(k) => Some(k),
                None => return Err(fail(sender, "bad_item", &[&typed])),
            }
        }
        _ => None,
    };
    let block = match op {
        Op::Block => {
            let typed = word(args, "block");
            match model::parse_block(&typed).filter(|_| show::block_state(&typed).is_some()) {
                Some((name, props)) => Some(model::block_text(&name, &props)),
                None => return Err(fail(sender, "bad_block", &[&typed])),
            }
        }
        _ => None,
    };
    let new_players: Vec<(values::PlayerId, String)> = match op {
        Op::ViewerAdd | Op::ViewerRemove => players(args, "players")
            .iter()
            .map(|p| (id_of(p), p.get_name()))
            .collect(),
        _ => Vec::new(),
    };
    let id = word(args, "id").to_ascii_lowercase();
    let result = edit(sender, server, args, NODE_EDIT, "changed", |h| {
        match op {
            Op::SetLine | Op::InsertLine | Op::RemoveLine | Op::AddLine => {
                only(h, Kind::Text)?;
                let n = count(args, "line").unwrap_or(0);
                let len = h.lines.len();
                match op {
                    Op::AddLine | Op::InsertLine if len >= model::MAX_LINES => {
                        return Err(refusal("max_lines", &[&model::MAX_LINES.to_string()]));
                    }
                    Op::AddLine => h.lines.push(line_text(args)),
                    Op::InsertLine if (1..=len + 1).contains(&n) => {
                        h.lines.insert(n - 1, line_text(args))
                    }
                    Op::SetLine if (1..=len).contains(&n) => h.lines[n - 1] = line_text(args),
                    Op::SetLine if n == len + 1 && len < model::MAX_LINES => {
                        h.lines.push(line_text(args))
                    }
                    Op::RemoveLine if (1..=len).contains(&n) => {
                        h.lines.remove(n - 1);
                    }
                    _ => return Err(refusal("no_line", &[&h.id, &n.to_string()])),
                }
            }
            Op::Billboard(b) => h.billboard = b,
            Op::Scale => {
                let x = float(args, "x").unwrap_or(1.0);
                h.scale = match (float(args, "y"), float(args, "z")) {
                    (Some(y), Some(z)) => [x, y, z],
                    _ => [x; 3],
                };
            }
            Op::Rotation => {
                h.place.yaw = float(args, "yaw").unwrap_or(0.0);
                h.place.pitch = float(args, "pitch").unwrap_or(0.0);
            }
            Op::Offset => {
                h.offset = [
                    float(args, "x").unwrap_or(0.0),
                    float(args, "y").unwrap_or(0.0),
                    float(args, "z").unwrap_or(0.0),
                ];
            }
            Op::Brightness => {
                let level = |k: &str| {
                    count(args, k)
                        .and_then(|n| u8::try_from(n).ok())
                        .unwrap_or(15)
                };
                h.brightness = Some((level("block"), level("sky")));
            }
            Op::BrightnessDefault => h.brightness = None,
            Op::Range => h.range = float(args, "blocks").unwrap_or(model::RANGE_UNIT),
            Op::Shadow => {
                h.shadow_radius = float(args, "radius").unwrap_or(0.0);
                if let Some(s) = float(args, "strength") {
                    h.shadow_strength = s;
                }
            }
            Op::Background => {
                only(h, Kind::Text)?;
                h.background = Background::parse(&word(args, "color"))
                    .ok_or_else(|| refusal("bad_background", &[]))?;
            }
            Op::TextShadow => {
                only(h, Kind::Text)?;
                h.text_shadow = flag(args, "value").unwrap_or(false);
            }
            Op::SeeThrough => {
                only(h, Kind::Text)?;
                h.see_through = flag(args, "value").unwrap_or(false);
            }
            Op::Align(a) => {
                only(h, Kind::Text)?;
                h.align = a;
            }
            Op::Width => {
                only(h, Kind::Text)?;
                h.width = count(args, "pixels")
                    .and_then(|n| i32::try_from(n).ok())
                    .unwrap_or(200);
            }
            Op::Refresh => {
                h.refresh = count(args, "ticks").and_then(|n| u32::try_from(n).ok());
            }
            Op::RefreshOff => h.refresh = None,
            Op::Item => {
                only(h, Kind::Item)?;
                if let Some(k) = item {
                    h.item = k;
                }
            }
            Op::Block => {
                only(h, Kind::Block)?;
                if let Some(b) = block {
                    h.block = b;
                }
            }
            Op::Everyone => h.visibility = Visibility::Everyone,
            Op::Permission => {
                let typed = word(args, "node").to_ascii_lowercase();
                let node = if typed.is_empty() {
                    model::default_view_node(&h.id)
                } else {
                    typed
                };
                h.visibility = Visibility::Permission(node);
            }
            Op::Players if !matches!(h.visibility, Visibility::Players(_)) => {
                h.visibility = Visibility::Players(std::collections::BTreeSet::new());
            }
            Op::ViewerAdd | Op::ViewerRemove => {
                let Visibility::Players(set) = &mut h.visibility else {
                    return Err(refusal("not_players", &[&h.id]));
                };
                for (pid, _) in &new_players {
                    if matches!(op, Op::ViewerAdd) {
                        set.insert(*pid);
                    } else {
                        set.remove(pid);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    });
    if result.is_ok() && matches!(op, Op::ViewerAdd | Op::ViewerRemove) {
        let key = if matches!(op, Op::ViewerAdd) {
            "viewer_added"
        } else {
            "viewer_removed"
        };
        for (_, name) in &new_players {
            say(sender, key, &[name, &id]);
        }
    }
    result
}

fn create(
    op: Op,
    sender: &CommandSender,
    server: &Server,
    args: &ConsumedArgs,
) -> std::result::Result<i32, CommandError> {
    allowed(sender, server, NODE_CREATE)?;
    let id = word(args, "id");
    if !model::valid_id(&id) {
        return Err(fail(sender, "bad_id", &[]));
    }
    let Some(place) = sender_place(sender) else {
        return Err(fail(sender, "need_player", &[]));
    };
    let kind = match op {
        Op::CreateItem => Kind::Item,
        Op::CreateBlock => Kind::Block,
        _ => Kind::Text,
    };
    let mut h = Hologram::new(&id, kind, place);
    match kind {
        Kind::Text => {
            let line = line_text(args);
            if line.trim().is_empty() {
                return Err(fail(sender, "bad_value", &[&id]));
            }
            h.lines = vec![line];
        }
        Kind::Item => {
            let typed = word(args, "item");
            let key = if typed.is_empty() {
                sender
                    .as_player()
                    .and_then(|p| p.get_item_in_hand(Hand::Right))
                    .and_then(|s| model::normalize_key(&s.get_registry_key()))
                    .filter(|k| k.as_str() != "minecraft:air")
                    .unwrap_or_else(|| h.item.clone())
            } else {
                model::normalize_key(&typed).unwrap_or_default()
            };
            if !show::item_exists(&key) {
                return Err(fail(sender, "bad_item", &[&typed]));
            }
            h.item = key;
        }
        Kind::Block => {
            let typed = word(args, "block");
            if !typed.is_empty() {
                match model::parse_block(&typed).filter(|_| show::block_state(&typed).is_some()) {
                    Some((name, props)) => h.block = model::block_text(&name, &props),
                    None => return Err(fail(sender, "bad_block", &[&typed])),
                }
            }
        }
    }
    if h.check().is_err() {
        return Err(fail(sender, "bad_value", &[&id]));
    }
    let result = with_state(|st| {
        if let Some(reason) = &st.broken {
            return Err(("file_broken", vec![reason.clone()]));
        }
        if st.holos.contains_key(&id) {
            return Err(("exists", vec![id.clone()]));
        }
        if st.holos.len() >= model::MAX_HOLOGRAMS {
            return Err(("limit", vec![model::MAX_HOLOGRAMS.to_string()]));
        }
        commit(st, server, h);
        Ok(())
    });
    match result {
        Ok(()) => {
            say(sender, "created", &[&id]);
            Ok(1)
        }
        Err((key, a)) => {
            let a: Vec<&str> = a.iter().map(String::as_str).collect();
            Err(fail(sender, key, &a))
        }
    }
}

fn delete(
    sender: &CommandSender,
    server: &Server,
    args: &ConsumedArgs,
) -> std::result::Result<i32, CommandError> {
    allowed(sender, server, NODE_DELETE)?;
    let id = word(args, "id").to_ascii_lowercase();
    let result = with_state(|st| {
        if let Some(reason) = &st.broken {
            return Err(("file_broken", vec![reason.clone()]));
        }
        let Some(old) = st.take(&id) else {
            return Err(("no_such", vec![id.clone()]));
        };
        st.removed(server, &old);
        mark_dirty(st, server);
        Ok(())
    });
    match result {
        Ok(()) => {
            say(sender, "deleted", &[&id]);
            Ok(1)
        }
        Err((key, a)) => {
            let a: Vec<&str> = a.iter().map(String::as_str).collect();
            Err(fail(sender, key, &a))
        }
    }
}

fn copy(
    sender: &CommandSender,
    server: &Server,
    args: &ConsumedArgs,
) -> std::result::Result<i32, CommandError> {
    allowed(sender, server, NODE_CREATE)?;
    let id = word(args, "id").to_ascii_lowercase();
    let new_id = word(args, "new");
    if !model::valid_id(&new_id) {
        return Err(fail(sender, "bad_id", &[]));
    }
    let here = sender_place(sender);
    let result = with_state(|st| {
        if let Some(reason) = &st.broken {
            return Err(("file_broken", vec![reason.clone()]));
        }
        let Some(mut h) = st.holos.get(&id).cloned() else {
            return Err(("no_such", vec![id.clone()]));
        };
        if st.holos.contains_key(&new_id) {
            return Err(("exists", vec![new_id.clone()]));
        }
        if st.holos.len() >= model::MAX_HOLOGRAMS {
            return Err(("limit", vec![model::MAX_HOLOGRAMS.to_string()]));
        }
        h.id.clone_from(&new_id);
        h.owner = None;
        h.persist = true;
        if let Visibility::Permission(node) = &h.visibility
            && *node == model::default_view_node(&id)
        {
            h.visibility = Visibility::Permission(model::default_view_node(&new_id));
        }
        if let Some(place) = here {
            h.place = place;
        }
        commit(st, server, h);
        Ok(())
    });
    match result {
        Ok(()) => {
            say(sender, "copied", &[&id, &new_id]);
            Ok(1)
        }
        Err((key, a)) => {
            let a: Vec<&str> = a.iter().map(String::as_str).collect();
            Err(fail(sender, key, &a))
        }
    }
}

fn status_text(sender: &CommandSender, status: Status) -> String {
    text(
        sender,
        match status {
            Status::Shown => "state_shown",
            Status::Waiting => "state_waiting",
            Status::Kept => "state_kept",
            Status::NoWorld => "state_no_world",
        },
        &[],
    )
}

fn bedrock_note(sender: &CommandSender) {
    if !edition::BEDROCK_SEES_DISPLAYS {
        say(sender, "bedrock_note", &[]);
    }
}

fn list(
    sender: &CommandSender,
    server: &Server,
    args: &ConsumedArgs,
) -> std::result::Result<i32, CommandError> {
    allowed(sender, server, NODE_VIEW)?;
    let page = count(args, "page").unwrap_or(1).max(1);
    let rows = with_state(|st| {
        if let Some(reason) = &st.broken {
            return Err(reason.clone());
        }
        let total = st.holos.len();
        let pages = total.div_ceil(LIST_PAGE).max(1);
        let page = page.min(pages);
        let mut worlds: std::collections::BTreeMap<String, bool> =
            std::collections::BTreeMap::new();
        let rows: Vec<(Hologram, Status)> = st
            .holos
            .values()
            .skip((page - 1) * LIST_PAGE)
            .take(LIST_PAGE)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| {
                let loaded = *worlds
                    .entry(h.place.world.clone())
                    .or_insert_with(|| server.get_world_by_name(&h.place.world).is_some());
                let status = st.status(&h.id, loaded);
                (h, status)
            })
            .collect();
        Ok((total, page, pages, rows))
    });
    let (total, page, pages, rows) = match rows {
        Ok(r) => r,
        Err(reason) => return Err(fail(sender, "file_broken", &[&reason])),
    };
    if total == 0 {
        say(sender, "list_empty", &[]);
        return Ok(0);
    }
    say(
        sender,
        "list_header",
        &[&total.to_string(), &page.to_string(), &pages.to_string()],
    );
    for (h, status) in &rows {
        say(
            sender,
            "list_line",
            &[
                &h.id,
                h.kind.name(),
                &h.place.world,
                &fmt(h.place.x),
                &fmt(h.place.y),
                &fmt(h.place.z),
                &status_text(sender, *status),
            ],
        );
    }
    bedrock_note(sender);
    Ok(i32::try_from(rows.len()).unwrap_or(i32::MAX))
}

fn near(
    sender: &CommandSender,
    server: &Server,
    args: &ConsumedArgs,
) -> std::result::Result<i32, CommandError> {
    allowed(sender, server, NODE_VIEW)?;
    let Some(player) = sender.as_player() else {
        return Err(fail(sender, "need_player", &[]));
    };
    let radius = num(args, "radius").unwrap_or(NEAR_DEFAULT);
    let world = player.get_world().get_name();
    let at = player.get_position();
    let found = with_state(|st| {
        st.near(&world, at, radius)
            .into_iter()
            .take(NEAR_LINES)
            .map(|(id, d)| {
                let kind = st.holos.get(&id).map_or("text", |h| h.kind.name());
                (id, kind, d)
            })
            .collect::<Vec<_>>()
    });
    let r = fmt(radius);
    if found.is_empty() {
        say(sender, "near_none", &[&r]);
        return Ok(0);
    }
    say(sender, "near_header", &[&r]);
    for (id, kind, d) in &found {
        say(sender, "near_line", &[id, kind, &fmt(*d)]);
    }
    Ok(i32::try_from(found.len()).unwrap_or(i32::MAX))
}

fn info(
    sender: &CommandSender,
    server: &Server,
    args: &ConsumedArgs,
) -> std::result::Result<i32, CommandError> {
    allowed(sender, server, NODE_VIEW)?;
    let id = word(args, "id").to_ascii_lowercase();
    let loaded_world = |h: &Hologram| server.get_world_by_name(&h.place.world).is_some();
    let found = with_state(|st| {
        st.holos.get(&id).cloned().map(|h| {
            let loaded = loaded_world(&h);
            let status = st.status(&id, loaded);
            (h, status)
        })
    });
    let Some((h, status)) = found else {
        return Err(fail(sender, "no_such", &[&id]));
    };
    let p = &h.place;
    say(sender, "info_title", &[&h.id, h.kind.name()]);
    say(
        sender,
        "info_place",
        &[
            &p.world,
            &fmt(p.x),
            &fmt(p.y),
            &fmt(p.z),
            &fmt(f64::from(p.yaw)),
            &fmt(f64::from(p.pitch)),
        ],
    );
    let v3 = |v: [f32; 3]| {
        if v[0] == v[1] && v[1] == v[2] {
            fmt(f64::from(v[0]))
        } else {
            format!(
                "{} {} {}",
                fmt(f64::from(v[0])),
                fmt(f64::from(v[1])),
                fmt(f64::from(v[2]))
            )
        }
    };
    say(
        sender,
        "info_look",
        &[
            h.billboard.name(),
            &v3(h.scale),
            &v3(h.offset),
            &fmt(f64::from(h.range)),
        ],
    );
    match h.kind {
        Kind::Text => {
            for (n, line) in h.template().plain().iter().enumerate() {
                say(sender, "info_line", &[&(n + 1).to_string(), line]);
            }
            say(
                sender,
                "info_text_look",
                &[
                    &h.background.text(),
                    &yes_no(sender, h.text_shadow),
                    &yes_no(sender, h.see_through),
                    h.align.name(),
                    &h.width.to_string(),
                ],
            );
            if let Some(p) = sender.as_player() {
                let pid = id_of(&p);
                if let Some(seen) = with_state(|st| st.preview(server, &h.id, Some(pid))) {
                    say(sender, "info_preview", &[&seen]);
                }
            }
        }
        Kind::Item => say(sender, "info_item", &[&h.item]),
        Kind::Block => say(sender, "info_block", &[&h.block]),
    }
    match &h.visibility {
        Visibility::Everyone => say(sender, "info_everyone", &[]),
        Visibility::Permission(node) => say(sender, "info_permission", &[node]),
        Visibility::Players(set) => {
            let names: Vec<String> = set
                .iter()
                .map(|pid| {
                    let u = strawberry_plugin_api::wit::strawberry::plugin::uuid::Uuid {
                        high: u64::try_from(*pid >> 64).unwrap_or(0),
                        low: u64::try_from(*pid & u128::from(u64::MAX)).unwrap_or(0),
                    };
                    server
                        .get_player_by_uuid(u)
                        .map_or_else(|| uuid_text(*pid), |p| p.get_name())
                })
                .collect();
            say(
                sender,
                "info_players",
                &[&set.len().to_string(), &names.join(", ")],
            );
        }
    }
    if let Some(owner) = &h.owner {
        say(sender, "info_owner", &[owner]);
    }
    if let Some(r) = h.refresh {
        say(sender, "info_refresh", &[&r.to_string()]);
    }
    say(sender, "info_state", &[&status_text(sender, status)]);
    bedrock_note(sender);
    Ok(1)
}

fn reload(sender: &CommandSender, server: &Server) -> std::result::Result<i32, CommandError> {
    allowed(sender, server, NODE_RELOAD)?;
    match read_file() {
        Ok(list) => {
            let n = list.len();
            with_state(|st| {
                st.broken = None;
                st.reload(server, list);
            });
            say(sender, "reloaded", &[&n.to_string()]);
            Ok(1)
        }
        Err(reason) => {
            with_state(|st| st.broken = Some(reason.clone()));
            Err(fail(sender, "file_broken", &[&reason]))
        }
    }
}

fn lit(name: &str) -> CommandNode {
    CommandNode::literal(name)
}

fn arg(name: &str, t: &ArgumentType) -> CommandNode {
    CommandNode::argument(name, t)
}

fn id_arg() -> CommandNode {
    arg("id", &ArgumentType::String(StringType::SingleWord)).suggest(Ids)
}

fn greedy(name: &str) -> CommandNode {
    arg(name, &ArgumentType::String(StringType::Greedy))
}

fn double(name: &str, lo: f64, hi: f64) -> CommandNode {
    arg(name, &ArgumentType::Double((Some(lo), Some(hi))))
}

fn int(name: &str, lo: i32, hi: i32) -> CommandNode {
    arg(name, &ArgumentType::Integer((Some(lo), Some(hi))))
}

fn run_op(op: Op) -> Cmd {
    Cmd(op)
}

#[allow(clippy::too_many_lines)]
fn holo_command() -> Command {
    let max_lines = i32::try_from(model::MAX_LINES).unwrap_or(32);
    let edit = id_arg()
        .then(
            lit("line")
                .then(int("line", 1, max_lines).then(greedy("text").execute(run_op(Op::SetLine)))),
        )
        .then(lit("addline").then(greedy("text").execute(run_op(Op::AddLine))))
        .then(
            lit("insertline").then(
                int("line", 1, max_lines).then(greedy("text").execute(run_op(Op::InsertLine))),
            ),
        )
        .then(lit("removeline").then(int("line", 1, max_lines).execute(run_op(Op::RemoveLine))))
        .then(Billboard::ALL.iter().fold(lit("billboard"), |n, b| {
            n.then(lit(b.name()).execute(run_op(Op::Billboard(*b))))
        }))
        .then(
            lit("scale").then(double("x", 0.01, 64.0).execute(run_op(Op::Scale)).then(
                double("y", 0.01, 64.0).then(double("z", 0.01, 64.0).execute(run_op(Op::Scale))),
            )),
        )
        .then(
            lit("rotation").then(
                double("yaw", -360.0, 360.0)
                    .execute(run_op(Op::Rotation))
                    .then(double("pitch", -90.0, 90.0).execute(run_op(Op::Rotation))),
            ),
        )
        .then(lit("offset").then(double("x", -64.0, 64.0).then(
            double("y", -64.0, 64.0).then(double("z", -64.0, 64.0).execute(run_op(Op::Offset))),
        )))
        .then(
            lit("brightness")
                .then(lit("default").execute(run_op(Op::BrightnessDefault)))
                .then(int("block", 0, 15).then(int("sky", 0, 15).execute(run_op(Op::Brightness)))),
        )
        .then(
            lit("range").then(
                double("blocks", 1.0, f64::from(model::MAX_RANGE)).execute(run_op(Op::Range)),
            ),
        )
        .then(
            lit("shadow").then(
                double("radius", 0.0, 64.0)
                    .execute(run_op(Op::Shadow))
                    .then(double("strength", 0.0, 1.0).execute(run_op(Op::Shadow))),
            ),
        )
        .then(lit("background").then(greedy("color").execute(run_op(Op::Background))))
        .then(
            lit("textshadow")
                .then(arg("value", &ArgumentType::Bool).execute(run_op(Op::TextShadow))),
        )
        .then(
            lit("seethrough")
                .then(arg("value", &ArgumentType::Bool).execute(run_op(Op::SeeThrough))),
        )
        .then(Align::ALL.iter().fold(lit("align"), |n, a| {
            n.then(lit(a.name()).execute(run_op(Op::Align(*a))))
        }))
        .then(lit("width").then(int("pixels", 1, 4096).execute(run_op(Op::Width))))
        .then(
            lit("refresh")
                .then(lit("off").execute(run_op(Op::RefreshOff)))
                .then(
                    int(
                        "ticks",
                        i32::try_from(model::MIN_REFRESH_TICKS).unwrap_or(5),
                        i32::try_from(model::MAX_REFRESH_TICKS).unwrap_or(72_000),
                    )
                    .execute(run_op(Op::Refresh)),
                ),
        )
        .then(
            lit("item")
                .execute(run_op(Op::Item))
                .then(greedy("item").execute(run_op(Op::Item))),
        )
        .then(lit("block").then(greedy("block").execute(run_op(Op::Block))))
        .then(
            lit("visibility")
                .then(lit("everyone").execute(run_op(Op::Everyone)))
                .then(
                    lit("permission")
                        .execute(run_op(Op::Permission))
                        .then(greedy("node").execute(run_op(Op::Permission))),
                )
                .then(lit("players").execute(run_op(Op::Players))),
        )
        .then(
            lit("viewer")
                .then(
                    lit("add").then(
                        arg("players", &ArgumentType::Players).execute(run_op(Op::ViewerAdd)),
                    ),
                )
                .then(lit("remove").then(
                    arg("players", &ArgumentType::Players).execute(run_op(Op::ViewerRemove)),
                )),
        );
    let create = lit("create")
        .then(
            lit("text").then(
                arg("id", &ArgumentType::String(StringType::SingleWord))
                    .then(greedy("text").execute(run_op(Op::CreateText))),
            ),
        )
        .then(
            lit("item").then(
                arg("id", &ArgumentType::String(StringType::SingleWord))
                    .execute(run_op(Op::CreateItem))
                    .then(greedy("item").execute(run_op(Op::CreateItem))),
            ),
        )
        .then(
            lit("block").then(
                arg("id", &ArgumentType::String(StringType::SingleWord))
                    .execute(run_op(Op::CreateBlock))
                    .then(greedy("block").execute(run_op(Op::CreateBlock))),
            ),
        )
        .then(
            arg("id", &ArgumentType::String(StringType::SingleWord))
                .then(greedy("text").execute(run_op(Op::CreateText))),
        );
    Command::new(
        &[
            "holo".to_owned(),
            "hologram".to_owned(),
            "holograms".to_owned(),
        ],
        "Text, item and block holograms.",
    )
    .execute(run_op(Op::Usage))
    .then(create)
    .then(lit("edit").then(edit))
    .then(
        lit("move").then(
            id_arg()
                .execute(run_op(Op::Move))
                .then(arg("pos", &ArgumentType::Position3d).execute(run_op(Op::Move))),
        ),
    )
    .then(lit("delete").then(id_arg().execute(run_op(Op::Delete))))
    .then(
        lit("list")
            .execute(run_op(Op::List))
            .then(int("page", 1, 100_000).execute(run_op(Op::List))),
    )
    .then(
        lit("near")
            .execute(run_op(Op::Near))
            .then(double("radius", 1.0, 512.0).execute(run_op(Op::Near))),
    )
    .then(lit("info").then(id_arg().execute(run_op(Op::Info))))
    .then(lit("copy").then(
        id_arg().then(
            arg("new", &ArgumentType::String(StringType::SingleWord)).execute(run_op(Op::Copy)),
        ),
    ))
    .then(lit("reload").execute(run_op(Op::Reload)))
    .then(lit("status").execute(run_op(Op::Status)))
}

// ----- the plugin -----

struct Holograms;

impl Plugin for Holograms {
    fn new() -> Self {
        Self
    }

    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "holograms".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            authors: vec!["Strawberry Interactive LLC".into()],
            // /plugins shows it on hover; metadata cannot use the language files.
            description: "Text, item and block holograms with live text and per-player visibility."
                .into(),
            dependencies: Vec::new(),
            permissions: vec![
                permissions::FS_READ_DATA.into(),
                permissions::FS_WRITE_DATA.into(),
            ],
        }
    }

    fn on_load(&self, context: Context) -> Result<()> {
        let op2 = || PermissionDefault::Op(PermissionLevel::Two);
        let nodes = [
            (NODE_COMMAND, "Use /holo"),
            (NODE_VIEW, "List holograms and see their details"),
            (NODE_CREATE, "Create and copy holograms"),
            (NODE_EDIT, "Change and move holograms"),
            (NODE_DELETE, "Delete holograms"),
            (NODE_RELOAD, "Read the hologram file again"),
        ];
        for (node, description) in nodes {
            context.register_permission(&Permission {
                node: node.into(),
                description: description.into(),
                default: op2(),
                children: Vec::new(),
            })?;
        }
        context.register_permission(&Permission {
            node: NODE_ADMIN.into(),
            description: "Everything /holo does".into(),
            default: op2(),
            children: nodes
                .iter()
                .map(|(node, _)| PermissionChild {
                    node: (*node).into(),
                    value: true,
                })
                .collect(),
        })?;
        context.register_command(holo_command(), NODE_COMMAND);
        context.register_event_handler_with_options::<PlayerJoinEvent, _>(Join, observer())?;
        context.register_event_handler_with_options::<PlayerLeaveEvent, _>(Leave, observer())?;
        context.register_event_handler_with_options::<PlayerChangedWorldEvent, _>(
            ChangedWorld,
            observer(),
        )?;
        context.register_event_handler_with_options::<WorldLoadEvent, _>(WorldLoad, observer())?;
        context
            .register_event_handler_with_options::<WorldUnloadEvent, _>(WorldUnload, observer())?;
        let loaded = read_file();
        with_state(|st| match loaded {
            Ok(list) => {
                for h in list {
                    st.put(h);
                }
            }
            Err(reason) => {
                log(
                    LogLevel::Error,
                    &format!(
                        "holograms: {} could not be read ({reason}); no hologram is shown and nothing is written until it is fixed and /holo reload runs",
                        store::FILE
                    ),
                );
                st.broken = Some(reason);
            }
        });
        let server = context.get_server();
        // The holograms are checked once the server runs its first tick.
        server.schedule_delayed_task(1, |server| with_state(|st| st.start(&server)));
        *SERVER.lock().unwrap_or_else(PoisonError::into_inner) = Some(server);
        Ok(())
    }

    fn on_unload(&self, _context: Context) -> Result<()> {
        with_state(State::stop);
        save_now();
        Ok(())
    }

    fn handle_ipc_message(
        &self,
        from: strawberry_plugin_api::wit::PluginId,
        payload: strawberry_plugin_api::wit::IpcMessage,
    ) -> std::result::Result<strawberry_plugin_api::wit::IpcMessage, String> {
        Ok(handle_message(&from, &payload))
    }
}

register_plugin!(Holograms);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_metadata_asks_only_for_its_data_folder() {
        let m = Holograms::new().metadata();
        assert_eq!(m.name, "holograms");
        assert_eq!(
            m.permissions,
            vec![
                permissions::FS_READ_DATA.to_string(),
                permissions::FS_WRITE_DATA.to_string()
            ]
        );
        assert!(m.dependencies.is_empty());
    }

    #[test]
    fn observers_never_block() {
        let o = observer();
        assert!(!o.blocking);
        assert!(matches!(o.on_failure, FailureVerdict::Allow));
    }

    #[test]
    fn numbers_read_short() {
        assert_eq!(fmt(1.0), "1");
        assert_eq!(fmt(-0.001), "0");
        assert_eq!(fmt(2.5), "2.5");
        assert_eq!(fmt(64.126), "64.13");
    }
}
