//! What the plugin does with the world: which entities show which hologram, and who sees what.
//!
//! **Where players look.** The server keeps a chunk's entities in the world only while a player
//! watches the chunk: the first player to watch it brings back the entities saved with it, and
//! when the last one stops watching they are saved and taken out again. It does not tell plugins
//! when that happens, so the plugin follows the players: one light check every half second, and
//! only while players are in a world that has holograms, works out which hologram chunks each
//! player watches ([`inside`], the server's own rule). With no hologram or no player there, no
//! check runs at all.
//!
//! **Never a second entity.** The plugin makes an entity only in a chunk that has been watched
//! for at least one whole check, so the entities saved with it are back and can be seen. There
//! it takes up the entity the world kept for the hologram (tagged `holograms:id`), removes any
//! other copy, any entity of a hologram that no longer exists and any entity meant for some
//! players only that an older version let the world save; and only then makes what is missing.
//! A change to a hologram nobody watches waits until somebody does.
//!
//! **Kept by the world, or by the plugin.** A hologram everybody sees the same way is an
//! ordinary entity the world saves with its chunk. A hologram for some players only, one whose
//! text differs per viewer, or one a plugin does not keep, is made by the plugin while players
//! watch it, is never saved with its chunk (`set_persistent(false)`), and is removed when
//! the plugin stops.
//!
//! **Nothing sent when nothing changes.** Texts are sent again only when they changed; a task
//! for values that change on their own (`{world_time}`, `{time}`) runs only while such a
//! hologram is watched.
//!
//! **Many at once.** When players come near many holograms at once, the world's entities are
//! looked at once and the holograms wait in one queue; a task shows a share of them each tick,
//! about a millisecond's worth, those the world does not show by itself first, and stops when
//! the queue is empty.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use strawberry_plugin_api::logging::{LogLevel, log};
use strawberry_plugin_api::persistent_data::PersistentDataHolder;
use strawberry_plugin_api::scheduler::{self, cancel_task};
use strawberry_plugin_api::{Player, PlayerEntityVisibilityExt, Server, World};

use crate::markup::{Ref, Template};
use crate::model::{Hologram, Kind, Visibility};
use crate::show::{self, Mode, Shown};
use crate::values::{PlayerId, Values};

/// Entities made for single viewers, in all.
pub const MAX_COPIES: usize = 8192;
/// Ticks between two checks of where players look.
pub const SIGHT_PERIOD: u64 = 10;
/// The refresh interval when a hologram with a changing value does not name one.
pub const DEFAULT_REFRESH: u32 = 20;
/// Checks a chunk must be watched in a row, and be loaded by the server, before entities are made
/// in it: a player who has just joined or teleported watches chunks the server is still loading,
/// and an entity made there would wait for the chunk.
const SETTLED: u8 = 2;
/// Checks after which a watched chunk is looked at once more. A busy server can bring a chunk's
/// saved entities back later than [`SETTLED`] checks; a copy it brings back late (a hologram for
/// some players only that the world saved, a second copy) is removed then.
const LATE: [u8; 2] = [8, 32];
/// The count stops here.
const LAST: u8 = 32;

/// Which look at the world's entities.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// A chunk has just come into sight: copies for some players only go at once.
    First,
    /// A chunk has settled: the saved entities are taken up, what is missing waits to be made.
    Attach,
    /// Some seconds later: whatever the plugin does not hold there goes.
    Late,
}
/// A player's view distance is read again every this many checks.
const SETTINGS_EVERY: u64 = 40;
/// The shortest view distance the server gives a player, in chunks.
const MIN_VIEW: i32 = 2;
/// Holograms shown in one tick at most when players come near many at once; the rest follow in
/// the next ticks, so no single tick carries them all.
pub const ATTACH_BUDGET: usize = 100;
/// ... and about this much time per tick, so a slow server spreads them over more ticks.
const DRAIN_TIME: std::time::Duration = std::time::Duration::from_millis(1);
/// ... but at least this many per tick, so a slow server still shows them soon.
const ATTACH_MIN: usize = 2;
/// Texts sent again at one check at most for about this long; the rest at the next checks.
const ATTACH_TIME: std::time::Duration = std::time::Duration::from_millis(3);

/// A chunk of a world.
pub type ChunkKey = (String, i32, i32);

/// Whether the chunk `dx`, `dz` chunks from a player's chunk is in his view of `view` chunks:
/// the server's rule (a square of five chunks, then a circle).
#[must_use]
pub fn inside(view: i32, dx: i32, dz: i32) -> bool {
    let rel = |d: i32| i64::from((d.abs() - 2).max(0));
    let (x, z) = (rel(dx), rel(dz));
    x * x + z * z < i64::from(view) * i64::from(view)
}

/// A player the plugin knows is online.
pub struct Viewer {
    pub player: Player,
    pub name: String,
    pub world: String,
    /// His view distance in chunks, as the server gives it to him.
    view: i32,
    /// The hologram chunks of his world he watches.
    sees: BTreeSet<(i32, i32)>,
}

/// The entities showing one hologram while its chunk is watched.
pub struct Live {
    pub shared: Option<Shown>,
    pub copies: BTreeMap<PlayerId, Shown>,
    pub mode: Mode,
    pub per_viewer: bool,
}

/// What `/holo list` and `info` say about a hologram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Watched by a player; the plugin holds its entities.
    Shown,
    /// Nobody watches it; it shows when a player comes near.
    Waiting,
    /// Nobody watches it; the world keeps its entity.
    Kept,
    NoWorld,
}

#[derive(Default)]
pub struct State {
    pub holos: BTreeMap<String, Hologram>,
    templates: BTreeMap<String, Template>,
    by_chunk: BTreeMap<ChunkKey, BTreeSet<String>>,
    /// Chunks that may still hold an entity of a hologram that was removed or moved away while
    /// nobody watched them; looked at when they are watched next.
    graveyard: BTreeSet<ChunkKey>,
    /// How many checks in a row each followed chunk has been watched (0 to [`SETTLED`]).
    watched: BTreeMap<ChunkKey, u8>,
    pub live: BTreeMap<String, Live>,
    pub values: Values,
    pub viewers: BTreeMap<PlayerId, Viewer>,
    sight_task: Option<u32>,
    /// The task that shows waiting holograms, each tick while any wait.
    drain_task: Option<u32>,
    checks: u64,
    refresh_task: Option<(u32, u32)>,
    refresh_due: BTreeMap<String, u64>,
    /// Ticks counted by the refresh task.
    ticks: u64,
    pub dirty: bool,
    pub save_scheduled: bool,
    /// Why the save file could not be read; nothing is changed or written while it is set.
    pub broken: Option<String>,
    pub max_players: u32,
    /// Texts sent to entities since the plugin loaded.
    pub sends: u64,
    /// Passes over a world's entities since the plugin loaded.
    pub scans: u64,
    /// Holograms of settled chunks waiting to be shown, with the entity the world kept for each;
    /// a share is shown at each check ([`ATTACH_BUDGET`]).
    waiting: VecDeque<(String, Option<Shown>)>,
    /// `{online}` changed: its holograms are sent again at the next check, once.
    online_dirty: bool,
    /// Watched holograms whose text is to be sent again, a share at each check.
    stale: BTreeSet<String>,
    /// The shown holograms changed since the refresh task was last set up.
    refresh_dirty: bool,
}

/// A player's UUID as one number.
#[must_use]
pub fn id_of(player: &Player) -> PlayerId {
    let u = player.get_id();
    (u128::from(u.high) << 64) | u128::from(u.low)
}

fn chunk_key(h: &Hologram) -> ChunkKey {
    let (x, z) = h.place.chunk();
    (h.place.world.clone(), x, z)
}

fn allowed(h: &Hologram, viewer: &Viewer, id: PlayerId) -> bool {
    match &h.visibility {
        Visibility::Everyone => true,
        Visibility::Permission(node) => viewer.player.has_permission(node),
        Visibility::Players(p) => p.contains(&id),
    }
}

fn time_text(minutes_of_day: u64) -> String {
    format!(
        "{:02}:{:02}",
        (minutes_of_day / 60) % 24,
        minutes_of_day % 60
    )
}

#[allow(clippy::cast_possible_truncation)]
fn chunk_of(pos: (f64, f64, f64)) -> (i32, i32) {
    ((pos.0.floor() as i32) >> 4, (pos.2.floor() as i32) >> 4)
}

/// A player's view distance as the server gives it: what his client asks, at least
/// [`MIN_VIEW`], at most the server's. A Bedrock player's is not known here, so the shortest is
/// taken: the plugin then believes a chunk watched only where it surely is.
fn view_of(player: &Player, server_view: i32) -> i32 {
    let max = server_view.max(MIN_VIEW);
    player.as_java().map_or(MIN_VIEW, |j| {
        i32::from(j.get_settings().view_distance).clamp(MIN_VIEW, max)
    })
}

/// Whether an entity stands away from the hologram's place.
fn moved(shown: &Shown, h: &Hologram) -> bool {
    let (x, y, z) = shown.entity().get_position();
    (x - h.place.x).abs() > 1e-4 || (y - h.place.y).abs() > 1e-4 || (z - h.place.z).abs() > 1e-4
}

impl State {
    // ----- records -----

    /// Puts a hologram in (new, or replacing the one with its id) and returns the old one.
    pub fn put(&mut self, h: Hologram) -> Option<Hologram> {
        let id = h.id.clone();
        let key = chunk_key(&h);
        self.templates.insert(id.clone(), h.template());
        let old = self.holos.insert(id.clone(), h);
        if let Some(o) = &old {
            self.unindex(&id, &chunk_key(o));
        }
        self.by_chunk.entry(key.clone()).or_default().insert(id);
        self.watched.entry(key).or_insert(0);
        old
    }

    /// Takes a hologram out of the records.
    pub fn take(&mut self, id: &str) -> Option<Hologram> {
        let old = self.holos.remove(id)?;
        self.templates.remove(id);
        self.unindex(id, &chunk_key(&old));
        Some(old)
    }

    fn unindex(&mut self, id: &str, key: &ChunkKey) {
        if let Some(set) = self.by_chunk.get_mut(key) {
            set.remove(id);
            if set.is_empty() {
                self.by_chunk.remove(key);
                if !self.graveyard.contains(key) {
                    self.watched.remove(key);
                }
            }
        }
    }

    fn bury(&mut self, key: ChunkKey) {
        self.watched.entry(key.clone()).or_insert(0);
        self.graveyard.insert(key);
    }

    fn template(&self, id: &str) -> Option<&Template> {
        self.templates.get(id)
    }

    fn refs(&self, id: &str) -> Vec<Ref> {
        self.template(id).map(Template::refs).unwrap_or_default()
    }

    /// Whether the hologram's text differs from one viewer to another.
    #[must_use]
    pub fn per_viewer(&self, id: &str) -> bool {
        self.holos.get(id).is_some_and(|h| h.kind == Kind::Text)
            && self.refs(id).iter().any(|r| match r {
                Ref::Player => true,
                Ref::Pushed(p, k) => self.values.per_player(p, k),
                _ => false,
            })
    }

    /// `shared` for a kept hologram everybody sees the same; `session` otherwise.
    #[must_use]
    pub fn mode_of(&self, id: &str) -> Mode {
        match self.holos.get(id) {
            Some(h)
                if h.persist && h.visibility == Visibility::Everyone && !self.per_viewer(id) =>
            {
                Mode::Shared
            }
            _ => Mode::Session,
        }
    }

    fn settled(&self, key: &ChunkKey) -> bool {
        self.watched.get(key).is_some_and(|n| *n >= SETTLED)
    }

    #[must_use]
    pub fn status(&self, id: &str, world_loaded: bool) -> Status {
        if !world_loaded {
            Status::NoWorld
        } else if self.live.contains_key(id) {
            Status::Shown
        } else if self.mode_of(id) == Mode::Shared {
            Status::Kept
        } else {
            Status::Waiting
        }
    }

    /// Ids in a world within `radius` of a point, nearest first, with their distance.
    #[must_use]
    pub fn near(&self, world: &str, at: (f64, f64, f64), radius: f64) -> Vec<(String, f64)> {
        let mut out: Vec<(String, f64)> = self
            .holos
            .values()
            .filter(|h| h.place.world == world)
            .map(|h| {
                let d = ((h.place.x - at.0).powi(2)
                    + (h.place.y - at.1).powi(2)
                    + (h.place.z - at.2).powi(2))
                .sqrt();
                (h.id.clone(), d)
            })
            .filter(|(_, d)| *d <= radius)
            .collect();
        out.sort_by(|a, b| a.1.total_cmp(&b.1));
        out
    }

    // ----- rendering -----

    /// The text component's JSON of a hologram for one viewer (`None`: the one everybody sees).
    fn render(&self, world: Option<&World>, id: &str, viewer: Option<PlayerId>) -> Option<String> {
        let h = self.holos.get(id)?;
        if h.kind != Kind::Text {
            return None;
        }
        let t = self.template(id)?;
        let refs = t.refs();
        let world_time = if refs.contains(&Ref::WorldTime) {
            world.map(|w| (w.get_time_of_day() + 6000) % 24000 * 1440 / 24000)
        } else {
            None
        };
        let online = self.viewers.len();
        let max = self.max_players;
        let name = viewer
            .and_then(|v| self.viewers.get(&v))
            .map(|v| v.name.clone());
        let lookup = |r: &Ref| -> Option<String> {
            match r {
                Ref::Player => name.clone(),
                Ref::Online => Some(online.to_string()),
                Ref::MaxPlayers => Some(max.to_string()),
                Ref::World => Some(h.place.world.clone()),
                Ref::WorldTime => world_time.map(time_text),
                Ref::Time => std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| time_text(d.as_secs() / 60 % 1440)),
                Ref::Pushed(p, k) => self.values.get(p, k, viewer).map(str::to_owned),
            }
        };
        Some(t.render(&lookup))
    }

    // ----- sight -----

    fn followed_worlds(&self) -> BTreeSet<&str> {
        self.watched.keys().map(|k| k.0.as_str()).collect()
    }

    /// Starts the check of where players look when it has something to do, or stops it.
    pub fn ensure_sight(&mut self) {
        let worlds = self.followed_worlds();
        let needed = self
            .viewers
            .values()
            .any(|v| worlds.contains(v.world.as_str()));
        match (needed, self.sight_task) {
            (true, None) => {
                self.sight_task = Some(scheduler::schedule_repeating_task(
                    SIGHT_PERIOD,
                    SIGHT_PERIOD,
                    |server| crate::with_state(|st| st.check(&server)),
                ));
            }
            (false, Some(task)) => {
                cancel_task(task);
                self.sight_task = None;
                // Nobody watches anything now: what a check would see, done at once, so the next
                // player to come is treated as the first to watch.
                let watched: Vec<ChunkKey> = self
                    .watched
                    .iter()
                    .filter(|(_, n)| **n > 0)
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in watched {
                    self.watched.insert(key.clone(), 0);
                    self.out_of_sight(&key);
                }
                // A chunk with no hologram left is followed no longer.
                let (by_chunk, graveyard) = (&self.by_chunk, &self.graveyard);
                self.watched
                    .retain(|k, _| by_chunk.contains_key(k) || graveyard.contains(k));
                self.waiting.clear();
                self.stale.clear();
                self.online_dirty = false;
            }
            _ => {}
        }
    }

    /// The followed chunks of `world` a player at `at` with `view` watches.
    fn seen_from(&self, world: &str, at: (i32, i32), view: i32) -> BTreeSet<(i32, i32)> {
        let reach = view + 2;
        let lo = (world.to_owned(), at.0 - reach, i32::MIN);
        let hi = (world.to_owned(), at.0 + reach, i32::MAX);
        self.watched
            .range(lo..=hi)
            .map(|((_, x, z), _)| (*x, *z))
            .filter(|(x, z)| inside(view, x - at.0, z - at.1))
            .collect()
    }

    /// Whether a player watches `key` right now (positions read again).
    fn watched_now(&mut self, server: &Server, key: &ChunkKey) -> bool {
        let server_view = i32::from(server.get_view_distance());
        self.viewers
            .values_mut()
            .filter(|v| v.world == key.0)
            .any(|v| {
                if v.view == 0 {
                    v.view = view_of(&v.player, server_view);
                }
                let at = chunk_of(v.player.get_position());
                inside(v.view, key.1 - at.0, key.2 - at.1)
            })
    }

    /// Every [`SIGHT_PERIOD`] ticks while players are in a world with holograms.
    pub fn check(&mut self, server: &Server) {
        self.checks += 1;
        let server_view = i32::from(server.get_view_distance());
        let reread = self.checks % SETTINGS_EVERY == 1;
        let worlds: BTreeSet<String> = self
            .followed_worlds()
            .into_iter()
            .map(str::to_owned)
            .collect();
        // Where each player is and what he watches.
        let mut seen: BTreeMap<PlayerId, BTreeSet<(i32, i32)>> = BTreeMap::new();
        let ids: Vec<PlayerId> = self.viewers.keys().copied().collect();
        for pid in ids {
            let Some(v) = self.viewers.get_mut(&pid) else {
                continue;
            };
            if !worlds.contains(&v.world) {
                continue;
            }
            if reread || v.view == 0 {
                v.view = view_of(&v.player, server_view);
            }
            let at = chunk_of(v.player.get_position());
            let (world, view) = (v.world.clone(), v.view);
            seen.insert(pid, self.seen_from(&world, at, view));
        }
        let mut in_sight: BTreeSet<ChunkKey> = BTreeSet::new();
        for (pid, chunks) in &seen {
            if let Some(v) = self.viewers.get(pid) {
                for (x, z) in chunks {
                    in_sight.insert((v.world.clone(), *x, *z));
                }
            }
        }
        // Chunks that come into sight, settle, or go out of it.
        let mut first: BTreeMap<String, BTreeSet<(i32, i32)>> = BTreeMap::new();
        let mut arriving: BTreeMap<String, BTreeSet<(i32, i32)>> = BTreeMap::new();
        let mut late: BTreeMap<String, BTreeSet<(i32, i32)>> = BTreeMap::new();
        let mut done: Vec<ChunkKey> = Vec::new();
        let mut gone: Vec<ChunkKey> = Vec::new();
        let handles: BTreeMap<String, Option<World>> = in_sight
            .iter()
            .map(|k| k.0.clone())
            .collect::<BTreeSet<String>>()
            .into_iter()
            .map(|name| {
                let w = server.get_world_by_name(&name);
                (name, w)
            })
            .collect();
        for (key, n) in &mut self.watched {
            if in_sight.contains(key) {
                // One call for a chunk about to settle: the server must have it loaded.
                let waits = *n + 1 == SETTLED
                    && !handles
                        .get(&key.0)
                        .and_then(Option::as_ref)
                        .is_some_and(|w| w.is_chunk_loaded(key.1, key.2));
                if *n < LAST && !waits {
                    *n += 1;
                    let to = match *n {
                        n if n < SETTLED => Some(&mut first),
                        SETTLED => Some(&mut arriving),
                        n if LATE.contains(&n) => Some(&mut late),
                        _ => None,
                    };
                    if let Some(to) = to {
                        to.entry(key.0.clone()).or_default().insert((key.1, key.2));
                    }
                    if *n == LAST {
                        done.push(key.clone());
                    }
                }
            } else if *n > 0 {
                gone.push(key.clone());
                *n = 0;
            }
        }
        for key in gone {
            self.out_of_sight(&key);
        }
        // Copies for single viewers follow each player in the chunks already settled.
        let pids: Vec<PlayerId> = seen.keys().copied().collect();
        for pid in pids {
            let new = seen.remove(&pid).unwrap_or_default();
            let Some(v) = self.viewers.get_mut(&pid) else {
                continue;
            };
            let old = std::mem::replace(&mut v.sees, new.clone());
            let world = v.world.clone();
            for c in old.difference(&new) {
                self.drop_copies(&(world.clone(), c.0, c.1), pid);
            }
            let added: Vec<(i32, i32)> = new.difference(&old).copied().collect();
            for c in added {
                let key = (world.clone(), c.0, c.1);
                let settled_before =
                    self.settled(&key) && !arriving.get(&world).is_some_and(|a| a.contains(&c));
                if settled_before {
                    self.add_copies(server, &key, pid);
                }
            }
        }
        // Where a hologram was deleted while nobody watched: its entity goes at once. (Entities
        // for some players only are never saved, so nothing else is looked for here.)
        for (world, chunks) in first {
            let wanted: BTreeSet<(i32, i32)> = chunks
                .into_iter()
                .filter(|c| self.graveyard.contains(&(world.clone(), c.0, c.1)))
                .collect();
            if !wanted.is_empty()
                && let Some(w) = server.get_world_by_name(&world)
            {
                self.reconcile(&w, &world, &wanted, Pass::First);
            }
        }
        if std::mem::take(&mut self.online_dirty) {
            self.refresh_online(server);
        }
        let stale: Vec<String> = std::mem::take(&mut self.stale).into_iter().collect();
        self.refresh_many(server, stale);
        for (world, chunks) in arriving {
            if let Some(w) = server.get_world_by_name(&world) {
                self.reconcile(&w, &world, &chunks, Pass::Attach);
            }
        }
        for (world, chunks) in late {
            if let Some(w) = server.get_world_by_name(&world) {
                self.reconcile(&w, &world, &chunks, Pass::Late);
            }
        }
        // A chunk with no hologram (one was removed there) is followed no longer once looked at.
        for key in done {
            if !self.by_chunk.contains_key(&key) {
                self.watched.remove(&key);
            }
        }
        self.ensure_drain();
        self.ensure_sight();
        if std::mem::take(&mut self.refresh_dirty) {
            self.update_refresh();
        }
    }

    /// Nobody watches `key` any more: the world takes its entities. The plugin lets go of
    /// them, and removes those for some players only that are still there.
    fn out_of_sight(&mut self, key: &ChunkKey) {
        let ids: Vec<String> = self
            .by_chunk
            .get(key)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        // Not shown yet: the world keeps its entity.
        self.waiting.retain(|(w, _)| !ids.contains(w));
        for id in ids {
            let remove = self.live.get(&id).is_some_and(|l| l.mode == Mode::Session);
            self.detach(&id, remove);
        }
    }

    // ----- making, taking up and removing entities -----

    /// Looks at the plugin's entities in `world`: an entity of a hologram that no longer exists
    /// goes, so does one of a watched hologram the plugin does not hold, and one meant for some
    /// players only. With `attach`, the chunks in `chunks` are settled: the entity the world
    /// kept for each hologram there is taken up, and what is missing is made.
    fn reconcile(&mut self, world: &World, name: &str, chunks: &BTreeSet<(i32, i32)>, pass: Pass) {
        let attach = pass == Pass::Attach;
        self.scans += 1;
        let mut held: BTreeSet<u32> = self
            .live
            .values()
            .flat_map(|l| l.shared.iter().chain(l.copies.values()))
            .map(Shown::entity_id)
            .collect();
        held.extend(
            self.waiting
                .iter()
                .filter_map(|(_, s)| s.as_ref())
                .map(Shown::entity_id),
        );
        let mut adopted: BTreeMap<String, Shown> = BTreeMap::new();
        for entity in world.get_entities() {
            // One call for every entity that is not ours.
            let Some(id) = entity.get_string(show::NAMESPACE, "id") else {
                continue;
            };
            if held.contains(&entity.get_id()) {
                continue;
            }
            let Some(h) = self.holos.get(&id) else {
                entity.remove();
                continue;
            };
            let home = chunk_key(h);
            let at = chunk_of(entity.get_position());
            let in_home = home.0 == name && (home.1, home.2) == at;
            if !in_home {
                // A copy left where the hologram was before it moved.
                if self.settled(&(name.to_owned(), at.0, at.1)) || chunks.contains(&at) {
                    entity.remove();
                }
                continue;
            }
            if !chunks.contains(&at) {
                if self.settled(&home) {
                    // Its chunk is watched and the plugin holds what it needs there.
                    entity.remove();
                }
                continue;
            }
            let mode = show::read_tag(&entity).map_or(Mode::Session, |(_, m)| m);
            let take = attach
                && mode == Mode::Shared
                && self.mode_of(&id) == Mode::Shared
                && !adopted.contains_key(&id)
                && entity.get_type() == show::entity_type(h.kind);
            if take {
                if let Some(s) = Shown::adopt(entity, h.kind) {
                    adopted.insert(id, s);
                }
            } else if mode == Mode::Session || pass != Pass::First {
                entity.remove();
            }
        }
        for c in chunks {
            self.graveyard.remove(&(name.to_owned(), c.0, c.1));
        }
        if !attach {
            return;
        }
        let ids: Vec<String> = chunks
            .iter()
            .flat_map(|c| {
                self.by_chunk
                    .get(&(name.to_owned(), c.0, c.1))
                    .into_iter()
                    .flatten()
                    .cloned()
            })
            .collect();
        let queued: BTreeSet<String> = self.waiting.iter().map(|(w, _)| w.clone()).collect();
        for id in ids {
            if self.live.contains_key(&id) || queued.contains(&id) {
                continue;
            }
            let a = adopted.remove(&id);
            // Those the world does not show by itself first.
            if a.is_some() {
                self.waiting.push_back((id, a));
            } else {
                self.waiting.push_front((id, a));
            }
        }
        // A second entity of a hologram the plugin already holds.
        for (_, extra) in adopted {
            extra.remove();
        }
    }

    /// Starts the task that shows waiting holograms, if any wait.
    fn ensure_drain(&mut self) {
        if !self.waiting.is_empty() && self.drain_task.is_none() {
            self.drain_task = Some(scheduler::schedule_repeating_task(1, 1, |server| {
                crate::with_state(|st| st.drain_tick(&server));
            }));
        }
    }

    /// Each tick while holograms wait: a share of them, then the task ends when none is left.
    fn drain_tick(&mut self, server: &Server) {
        self.drain(server);
        if self.waiting.is_empty()
            && let Some(task) = self.drain_task.take()
        {
            cancel_task(task);
        }
    }

    /// Shows waiting holograms: at most [`ATTACH_BUDGET`] and about [`DRAIN_TIME`], at least
    /// [`ATTACH_MIN`].
    fn drain(&mut self, server: &Server) {
        let started = std::time::Instant::now();
        let mut made = 0;
        let mut worlds: BTreeMap<String, Option<World>> = BTreeMap::new();
        while let Some((id, adopted)) = self.waiting.pop_front() {
            let Some(key) = self.holos.get(&id).map(chunk_key) else {
                if let Some(a) = adopted {
                    a.remove();
                }
                continue;
            };
            if self.live.contains_key(&id) {
                if let Some(a) = adopted {
                    a.remove();
                }
                continue;
            }
            if !self.settled(&key) {
                // Out of sight since: the world keeps what it has.
                continue;
            }
            if !worlds.contains_key(&key.0) {
                worlds.insert(key.0.clone(), server.get_world_by_name(&key.0));
            }
            let Some(Some(world)) = worlds.get(&key.0) else {
                continue;
            };
            self.attach(server, world, &id, adopted);
            made += 1;
            if made >= ATTACH_BUDGET || (made >= ATTACH_MIN && started.elapsed() > DRAIN_TIME) {
                break;
            }
        }
    }

    /// Shows a hologram in a watched chunk, with `adopted`, the entity the world kept for it.
    fn attach(&mut self, server: &Server, world: &World, id: &str, adopted: Option<Shown>) {
        self.detach(id, true);
        let Some(h) = self.holos.get(id).cloned() else {
            if let Some(a) = adopted {
                a.remove();
            }
            return;
        };
        let key = chunk_key(&h);
        let mode = self.mode_of(id);
        let per_viewer = self.per_viewer(id);
        let mut live = Live {
            shared: None,
            copies: BTreeMap::new(),
            mode,
            per_viewer,
        };
        if per_viewer {
            if let Some(a) = adopted {
                a.remove();
            }
            let targets: Vec<PlayerId> = self
                .viewers
                .iter()
                .filter(|(pid, v)| {
                    v.world == key.0 && v.sees.contains(&(key.1, key.2)) && allowed(&h, v, **pid)
                })
                .map(|(pid, _)| *pid)
                .collect();
            for pid in targets {
                if let Some(c) = self.spawn_copy(world, &h, pid, live.copies.len()) {
                    live.copies.insert(pid, c);
                }
            }
        } else {
            let shown = match adopted {
                Some(s) => {
                    if moved(&s, &h)
                        && let Some(w) = server.get_world_by_name(&h.place.world)
                    {
                        s.place(&h, w);
                    }
                    Some(s)
                }
                None => Shown::spawn(world, &h, mode, None),
            };
            if let Some(mut s) = shown {
                s.look(&h);
                let text = self.render(Some(world), id, None);
                if s.content(&h, text.as_deref()) {
                    self.sends += 1;
                }
                if mode == Mode::Session {
                    for (pid, v) in &self.viewers {
                        if v.world == key.0 && allowed(&h, v, *pid) {
                            v.player.show(s.entity());
                        }
                    }
                }
                live.shared = Some(s);
            }
        }
        self.live.insert(id.to_owned(), live);
        self.refresh_dirty = true;
    }

    fn copies(&self) -> usize {
        self.live.values().map(|l| l.copies.len()).sum()
    }

    /// An entity of a per-viewer hologram for one player, shown to him only.
    fn spawn_copy(
        &mut self,
        world: &World,
        h: &Hologram,
        pid: PlayerId,
        building: usize,
    ) -> Option<Shown> {
        if self.copies() + building >= MAX_COPIES {
            log(
                LogLevel::Warn,
                &format!(
                    "holograms: {MAX_COPIES} entities for single players reached; {} is not shown to everyone",
                    h.id
                ),
            );
            return None;
        }
        let mut c = Shown::spawn(world, h, Mode::Session, Some(pid))?;
        c.look(h);
        let text = self.render(Some(world), &h.id, Some(pid));
        if c.content(h, text.as_deref()) {
            self.sends += 1;
        }
        if let Some(v) = self.viewers.get(&pid) {
            v.player.show(c.entity());
        }
        Some(c)
    }

    /// A player watches a settled chunk now: his copies of its per-viewer holograms.
    fn add_copies(&mut self, server: &Server, key: &ChunkKey, pid: PlayerId) {
        let ids: Vec<String> = self
            .by_chunk
            .get(key)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        let Some(world) = server.get_world_by_name(&key.0) else {
            return;
        };
        for id in ids {
            let Some(h) = self.holos.get(&id).cloned() else {
                continue;
            };
            let wants = self
                .live
                .get(&id)
                .is_some_and(|l| l.per_viewer && !l.copies.contains_key(&pid))
                && self.viewers.get(&pid).is_some_and(|v| allowed(&h, v, pid));
            if !wants {
                continue;
            }
            if let Some(c) = self.spawn_copy(&world, &h, pid, 0)
                && let Some(l) = self.live.get_mut(&id)
            {
                l.copies.insert(pid, c);
            }
        }
    }

    /// A player no longer watches `key`: his copies there go.
    fn drop_copies(&mut self, key: &ChunkKey, pid: PlayerId) {
        let ids: Vec<String> = self
            .by_chunk
            .get(key)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        for id in ids {
            if let Some(l) = self.live.get_mut(&id)
                && let Some(c) = l.copies.remove(&pid)
            {
                c.remove();
            }
        }
    }

    /// Stops holding a hologram's entities; `remove` takes them out of the world too.
    pub fn detach(&mut self, id: &str, remove: bool) {
        if let Some(pos) = self.waiting.iter().position(|(w, _)| w == id)
            && let Some((_, Some(a))) = self.waiting.remove(pos)
            && remove
        {
            a.remove();
        }
        let Some(live) = self.live.remove(id) else {
            return;
        };
        self.refresh_dirty = true;
        if remove {
            if let Some(s) = live.shared {
                s.remove();
            }
            for (_, c) in live.copies {
                c.remove();
            }
        }
    }

    /// Shows a hologram now if its chunk is watched and settled; else when it is.
    fn attach_now(&mut self, server: &Server, id: &str, fresh: bool) {
        let Some(key) = self.holos.get(id).map(chunk_key) else {
            return;
        };
        // A new hologram can have no copy in the world, so its chunk need not have settled.
        let ready = (self.settled(&key) || (fresh && !self.graveyard.contains(&key)))
            && self.watched_now(server, &key);
        if !ready {
            if self.settled(&key) {
                // Players left since the last check: the next check sees the chunk again.
                self.watched.insert(key.clone(), 0);
                self.out_of_sight(&key);
            }
            return;
        }
        if let Some(world) = server.get_world_by_name(&key.0) {
            if fresh && !self.settled(&key) {
                // Watched now: that counts as one check, so leaving before the next is noticed.
                let n = self.watched.entry(key.clone()).or_insert(0);
                *n = (*n).max(1);
                // Not settled: only this new hologram is made; the rest waits for the check.
                let viewers: Vec<PlayerId> = self.viewers.keys().copied().collect();
                let server_view = i32::from(server.get_view_distance());
                for pid in viewers {
                    if let Some(v) = self.viewers.get_mut(&pid)
                        && v.world == key.0
                    {
                        if v.view == 0 {
                            v.view = view_of(&v.player, server_view);
                        }
                        let at = chunk_of(v.player.get_position());
                        if inside(v.view, key.1 - at.0, key.2 - at.1) {
                            v.sees.insert((key.1, key.2));
                        }
                    }
                }
            }
            self.attach(server, &world, id, None);
        }
    }

    // ----- changes -----

    /// After a hologram's record changed (or it is new): changes its entities in place when
    /// that is enough, else makes them again where players watch.
    pub fn changed(&mut self, server: &Server, id: &str, old: Option<&Hologram>) {
        let Some(new) = self.holos.get(id).cloned() else {
            return;
        };
        let new_key = chunk_key(&new);
        let old_key = old.map(chunk_key);
        let moved_chunk = old_key.as_ref().is_some_and(|k| *k != new_key);
        if let Some(live) = self.live.get(id) {
            let same = old.is_some_and(|o| {
                o.kind == new.kind && (o.visibility == new.visibility || !live.per_viewer)
            }) && !moved_chunk
                && live.mode == self.mode_of(id)
                && live.per_viewer == self.per_viewer(id);
            if same {
                self.apply_in_place(server, id, old);
                self.update_refresh();
                return;
            }
            self.detach(id, true);
        }
        if let Some(k) = old_key.clone()
            && moved_chunk
            && !self.watched_now(server, &k)
        {
            // Its entity may still be in the old chunk, which nobody watches.
            self.bury(k);
        }
        self.attach_now(server, id, old.is_none());
        self.ensure_sight();
        self.update_refresh();
    }

    /// A hologram is gone from the records: its entities go too.
    pub fn removed(&mut self, server: &Server, old: &Hologram) {
        let key = chunk_key(old);
        self.detach(&old.id, true);
        if !self.watched_now(server, &key) {
            // Nobody watches it: the world keeps its entity, which goes when somebody does.
            self.bury(key);
        }
        self.ensure_sight();
        self.update_refresh();
    }

    fn apply_in_place(&mut self, server: &Server, id: &str, old: Option<&Hologram>) {
        let Some(h) = self.holos.get(id).cloned() else {
            return;
        };
        let world = server.get_world_by_name(&h.place.world);
        let place_changed = old.is_some_and(|o| o.place != h.place);
        let visibility_changed = old.is_some_and(|o| o.visibility != h.visibility);
        let viewers: Vec<Option<PlayerId>> = match self.live.get(id) {
            Some(l) => l
                .shared
                .iter()
                .map(|_| None)
                .chain(l.copies.keys().map(|p| Some(*p)))
                .collect(),
            None => return,
        };
        let texts: Vec<Option<String>> = viewers
            .iter()
            .map(|v| self.render(world.as_ref(), id, *v))
            .collect();
        let mut sends = 0;
        if let Some(live) = self.live.get_mut(id) {
            let shown = live.shared.iter_mut().chain(live.copies.values_mut());
            for (s, text) in shown.zip(texts) {
                if place_changed && let Some(w) = server.get_world_by_name(&h.place.world) {
                    s.place(&h, w);
                }
                s.look(&h);
                if s.content(&h, text.as_deref()) {
                    sends += 1;
                }
            }
        }
        self.sends += sends;
        if visibility_changed
            && let Some(live) = self.live.get(id)
            && live.mode == Mode::Session
            && let Some(s) = &live.shared
        {
            for (pid, v) in &self.viewers {
                if v.world != h.place.world {
                    continue;
                }
                if allowed(&h, v, *pid) {
                    v.player.show(s.entity());
                } else {
                    v.player.hide(s.entity());
                }
            }
        }
    }

    /// Sends the text of a watched hologram again where it changed.
    pub fn refresh_text(&mut self, server: &Server, id: &str) {
        if let Some(l) = self.live.get(id)
            && l.per_viewer != self.per_viewer(id)
        {
            // Its values now differ per viewer (or no longer): one entity becomes many, or back.
            self.detach(id, true);
            self.attach_now(server, id, false);
            return;
        }
        let Some(h) = self.holos.get(id).cloned() else {
            return;
        };
        if h.kind != Kind::Text {
            return;
        }
        let world = server.get_world_by_name(&h.place.world);
        let viewers: Vec<Option<PlayerId>> = match self.live.get(id) {
            Some(l) => l
                .shared
                .iter()
                .map(|_| None)
                .chain(l.copies.keys().map(|p| Some(*p)))
                .collect(),
            None => return,
        };
        let texts: Vec<Option<String>> = viewers
            .iter()
            .map(|v| self.render(world.as_ref(), id, *v))
            .collect();
        let mut sends = 0;
        if let Some(live) = self.live.get_mut(id) {
            for (s, text) in live
                .shared
                .iter_mut()
                .chain(live.copies.values_mut())
                .zip(texts)
            {
                if s.content(&h, text.as_deref()) {
                    sends += 1;
                }
            }
        }
        self.sends += sends;
    }

    /// Values a plugin pushed changed: the watched holograms that show them are sent again.
    pub fn values_changed(&mut self, server: &Server, plugin: &str, keys: &[String]) {
        let ids: Vec<String> = self
            .holos
            .keys()
            .filter(|id| {
                self.refs(id)
                    .iter()
                    .any(|r| matches!(r, Ref::Pushed(p, k) if p == plugin && keys.contains(k)))
            })
            .cloned()
            .collect();
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| self.live.contains_key(id))
            .collect();
        self.refresh_many(server, ids);
    }

    /// Sends texts again for at most [`ATTACH_TIME`] now; the rest at the next checks, so a value
    /// a thousand holograms show never holds one tick.
    fn refresh_many(&mut self, server: &Server, ids: Vec<String>) {
        let started = std::time::Instant::now();
        let mut done = 0;
        for id in ids {
            if done > 0 && started.elapsed() > ATTACH_TIME && self.sight_task.is_some() {
                self.stale.insert(id);
                continue;
            }
            self.refresh_text(server, &id);
            done += 1;
        }
    }

    /// `{online}` changed: sent at the next check, once for many joins; at once when no check
    /// runs.
    fn online_changed(&mut self, server: &Server) {
        if self.sight_task.is_some() {
            self.online_dirty = true;
        } else {
            self.refresh_online(server);
        }
    }

    fn refresh_online(&mut self, server: &Server) {
        let ids: Vec<String> = self
            .live
            .keys()
            .filter(|id| self.refs(id).contains(&Ref::Online))
            .cloned()
            .collect();
        self.refresh_many(server, ids);
    }

    // ----- players -----

    fn add_viewer(&mut self, player: Player) -> PlayerId {
        let pid = id_of(&player);
        let name = player.get_name();
        let world = player.get_world().get_name();
        self.viewers.insert(
            pid,
            Viewer {
                player,
                name,
                world,
                view: 0,
                sees: BTreeSet::new(),
            },
        );
        pid
    }

    /// A player joined.
    pub fn join(&mut self, server: &Server, player: Player) {
        self.add_viewer(player);
        self.online_changed(server);
        self.ensure_sight();
    }

    /// A player left.
    pub fn leave(&mut self, server: &Server, pid: PlayerId) {
        self.leave_world(pid);
        self.viewers.remove(&pid);
        for (plugin, key) in self.values.leave(pid) {
            self.values_changed(server, &plugin, &[key]);
        }
        self.online_changed(server);
        self.ensure_sight();
    }

    /// A player is in another world now.
    pub fn changed_world(&mut self, player: Player) {
        let pid = id_of(&player);
        self.leave_world(pid);
        self.add_viewer(player);
        self.ensure_sight();
    }

    fn leave_world(&mut self, pid: PlayerId) {
        for live in self.live.values_mut() {
            if let Some(c) = live.copies.remove(&pid) {
                c.remove();
            }
        }
        if let Some(v) = self.viewers.get_mut(&pid) {
            v.sees.clear();
        }
    }

    // ----- refresh -----

    /// Starts, changes or stops the one refresh task, for the watched holograms that show a
    /// value that changes on its own or name a refresh interval.
    pub fn update_refresh(&mut self) {
        let wanted: Vec<(String, u32)> = self
            .live
            .keys()
            .filter_map(|id| {
                let h = self.holos.get(id)?;
                (h.refresh.is_some() || self.refs(id).iter().any(Ref::needs_refresh))
                    .then(|| (id.clone(), h.refresh.unwrap_or(DEFAULT_REFRESH)))
            })
            .collect();
        self.refresh_due
            .retain(|id, _| wanted.iter().any(|(w, _)| w == id));
        for (id, period) in &wanted {
            let due = self.ticks + u64::from(*period);
            self.refresh_due.entry(id.clone()).or_insert(due);
        }
        let period = wanted.iter().map(|(_, p)| *p).min();
        match (period, self.refresh_task) {
            (None, Some((task, _))) => {
                cancel_task(task);
                self.refresh_task = None;
            }
            (Some(p), Some((task, current))) if current != p => {
                cancel_task(task);
                self.refresh_task = Some((start_refresh(p), p));
            }
            (Some(p), None) => self.refresh_task = Some((start_refresh(p), p)),
            _ => {}
        }
    }

    fn refresh_tick(&mut self, server: &Server) {
        let Some((_, period)) = self.refresh_task else {
            return;
        };
        self.ticks += u64::from(period);
        let now = self.ticks;
        let due: Vec<String> = self
            .refresh_due
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &due {
            let p = self
                .holos
                .get(id)
                .and_then(|h| h.refresh)
                .unwrap_or(DEFAULT_REFRESH);
            self.refresh_due.insert(id.clone(), now + u64::from(p));
        }
        self.refresh_many(server, due);
    }

    // ----- start, reload, stop -----

    /// At start: the players already online. Every hologram is looked at once a player watches
    /// its chunk.
    pub fn start(&mut self, server: &Server) {
        self.max_players = server.get_max_players();
        for player in server.get_all_players() {
            self.add_viewer(player);
        }
        self.ensure_sight();
    }

    /// A world loaded: its holograms are looked at when players watch them.
    pub fn world_loaded(&mut self) {
        self.ensure_sight();
    }

    /// A world is unloading: the plugin lets go of what it holds there.
    pub fn world_unloading(&mut self, name: &str) {
        let keys: Vec<ChunkKey> = self
            .watched
            .keys()
            .filter(|k| k.0 == name)
            .cloned()
            .collect();
        for key in keys {
            self.out_of_sight(&key);
            self.watched.insert(key, 0);
        }
    }

    /// Replaces every hologram with those read from the file again.
    pub fn reload(&mut self, server: &Server, list: Vec<Hologram>) {
        let ids: Vec<String> = self.live.keys().cloned().collect();
        for id in &ids {
            let session = self
                .live
                .get(id)
                .is_some_and(|l| l.mode == Mode::Session || l.per_viewer);
            self.detach(id, session);
        }
        let old: Vec<Hologram> = self.holos.values().cloned().collect();
        self.waiting.clear();
        self.holos.clear();
        self.templates.clear();
        self.by_chunk.clear();
        let graves: Vec<ChunkKey> = self.graveyard.iter().cloned().collect();
        self.watched.clear();
        for key in graves {
            self.watched.insert(key, 0);
        }
        for h in list {
            self.put(h);
        }
        for o in old {
            if !self.holos.contains_key(&o.id) {
                self.bury(chunk_key(&o));
            }
        }
        for v in self.viewers.values_mut() {
            v.sees.clear();
        }
        // Every followed chunk is looked at again when it has been watched for a check.
        let _ = server;
        self.ensure_sight();
        self.update_refresh();
    }

    /// The plugin stops: the entities for some players only are removed, those everybody sees
    /// are left to the world, the tasks end.
    pub fn stop(&mut self) {
        for (_, live) in std::mem::take(&mut self.live) {
            if live.mode == Mode::Session
                && let Some(s) = live.shared
            {
                s.remove();
            }
            for (_, c) in live.copies {
                c.remove();
            }
        }
        self.waiting.clear();
        self.stale.clear();
        if let Some(task) = self.sight_task.take() {
            cancel_task(task);
        }
        if let Some(task) = self.drain_task.take() {
            cancel_task(task);
        }
        if let Some((task, _)) = self.refresh_task.take() {
            cancel_task(task);
        }
    }

    /// What `viewer` reads on a text hologram, without styles.
    #[must_use]
    pub fn preview(&self, server: &Server, id: &str, viewer: Option<PlayerId>) -> Option<String> {
        let world = self
            .holos
            .get(id)
            .and_then(|h| server.get_world_by_name(&h.place.world));
        self.render(world.as_ref(), id, viewer)
            .map(|json| crate::markup::plain_of(&json))
    }

    /// For `/holo status`: holograms, those watched now, entities for single players, chunks
    /// followed, passes over the worlds' entities.
    #[must_use]
    pub fn summary(&self) -> [u64; 5] {
        let n = |v: usize| u64::try_from(v).unwrap_or(u64::MAX);
        [
            n(self.holos.len()),
            n(self.live.len()),
            n(self.copies()),
            n(self.watched.len()),
            self.scans,
        ]
    }
}

fn start_refresh(period: u32) -> u32 {
    let p = u64::from(period);
    scheduler::schedule_repeating_task(p, p, |server| {
        crate::with_state(|st| st.refresh_tick(&server));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_rule_is_the_servers() {
        // The five chunks around the player are always in view.
        for d in -2..=2 {
            assert!(inside(2, d, d));
        }
        assert!(!inside(2, 4, 0));
        assert!(inside(2, 3, 0));
        assert!(inside(10, 11, 0));
        assert!(!inside(10, 12, 0));
        assert!(!inside(10, 10, 10));
        assert!(inside(10, 9, 9));
    }

    fn hologram(id: &str) -> Hologram {
        Hologram::new(
            id,
            Kind::Text,
            crate::model::Place {
                world: "world".into(),
                x: 0.5,
                y: 64.0,
                z: 0.5,
                yaw: 0.0,
                pitch: 0.0,
            },
        )
    }

    /// Every entity of a hologram a plugin does not keep (`persist: false`), of one for some
    /// players only and of a text per viewer is a session entity, which is spawned
    /// non-persistent; only a kept hologram everybody sees the same is saved by the world.
    #[test]
    fn entities_that_are_not_kept_are_never_saved() {
        let mut st = State::default();
        st.put(hologram("kept"));
        let mut gone = hologram("api");
        gone.persist = false;
        st.put(gone);
        let mut few = hologram("few");
        few.visibility = Visibility::Players([7].into_iter().collect());
        st.put(few);
        let mut perm = hologram("perm");
        perm.visibility = Visibility::Permission("a.b".into());
        st.put(perm);
        let mut each = hologram("each");
        each.lines = vec!["Hello {player}".into()];
        st.put(each);
        assert_eq!(st.mode_of("kept"), Mode::Shared);
        assert!(st.mode_of("kept").persistent());
        for id in ["api", "few", "perm", "each"] {
            assert_eq!(st.mode_of(id), Mode::Session, "{id}");
            assert!(!st.mode_of(id).persistent(), "{id}");
        }
        // The spawn marks them in the tick it makes them, right after the spawn.
        // Either line ending: a Windows checkout holds the source with CRLF.
        let src = include_str!("show.rs").replace("\r\n", "\n");
        let spawn = src.find("world.spawn_entity(").expect("the spawn");
        let after = &src[spawn..spawn + 260];
        assert!(
            after.contains("if !mode.persistent() {
            entity.set_persistent(false);"),
            "{after}"
        );
    }
}
