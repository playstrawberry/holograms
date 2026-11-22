//! The display entities that show a hologram: the only module that makes, changes and removes
//! them.
//!
//! Every entity this plugin makes carries two values of its own (`holograms:id`, the hologram's
//! id, and `holograms:mode`), so the plugin can tell its entities in the world from any other:
//!
//! - `shared`: the one entity of a hologram everybody sees. The world keeps it with its chunk,
//!   and the plugin takes it up again after a restart instead of making a second one.
//! - `session`: an entity for some players only (a hologram for chosen players, one whose text
//!   differs per viewer, one a plugin did not ask to keep). It is never taken up again: the
//!   plugin removes it when it stops, and removes any that a crash left behind.

use strawberry_plugin_api::persistent_data::PersistentDataHolder;
use strawberry_plugin_api::text::TextComponent;
use strawberry_plugin_api::wit::strawberry::plugin::world::resolve_block_state;
use strawberry_plugin_api::{
    BillboardMode, BlockDisplayEntity, EntityType, ItemDisplayEntity, ItemStack, TextAlignment,
    TextDisplayEntity, TransformationBuilder, World, world::Entity,
};

use crate::model::{Align, Background, Billboard, Hologram, Kind, RANGE_UNIT, parse_block};
use crate::values::PlayerId;

/// The namespace of the values the plugin puts on its entities.
pub const NAMESPACE: &str = "holograms";
const ID_KEY: &str = "id";
const MODE_KEY: &str = "mode";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Shared,
    Session,
}

impl Mode {
    /// Whether the world saves the entity with its chunk: only a kept hologram everybody sees
    /// the same. A session entity (a hologram that is not kept, one for some players only, a text
    /// per viewer) is never saved (Paper's `Entity#setPersistent(false)`).
    #[must_use]
    pub const fn persistent(self) -> bool {
        matches!(self, Self::Shared)
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Session => "session",
        }
    }
}

/// Whether an entity type is one of the three display entities.
#[must_use]
pub fn is_display(t: EntityType) -> bool {
    matches!(
        t,
        EntityType::TextDisplay | EntityType::ItemDisplay | EntityType::BlockDisplay
    )
}

/// The entity type that shows a kind of hologram.
#[must_use]
pub const fn entity_type(kind: Kind) -> EntityType {
    match kind {
        Kind::Text => EntityType::TextDisplay,
        Kind::Item => EntityType::ItemDisplay,
        Kind::Block => EntityType::BlockDisplay,
    }
}

/// The plugin's values on an entity: its hologram id and mode.
#[must_use]
pub fn read_tag(entity: &Entity) -> Option<(String, Mode)> {
    let id = entity.get_string(NAMESPACE, ID_KEY)?;
    let mode = match entity.get_string(NAMESPACE, MODE_KEY).as_deref() {
        Some("shared") => Mode::Shared,
        _ => Mode::Session,
    };
    Some((id, mode))
}

/// The state id of a block written as `name[props]`, if the server knows it.
#[must_use]
pub fn block_state(text: &str) -> Option<u16> {
    let (name, props) = parse_block(text)?;
    resolve_block_state(&name, &props)
}

/// Whether the server knows an item key (`minecraft:` and a vanilla item). The server answers an
/// unknown key with air, and names items without their namespace.
#[must_use]
pub fn item_exists(key: &str) -> bool {
    let got = ItemStack::new(key, 1).get_registry_key();
    let got = if got.contains(':') {
        got
    } else {
        format!("minecraft:{got}")
    };
    got != "minecraft:air" && got == key
}

enum Display {
    Text(TextDisplayEntity),
    Item(ItemDisplayEntity),
    Block(BlockDisplayEntity),
}

/// One entity showing a hologram, to everyone or to one viewer.
pub struct Shown {
    entity: Entity,
    display: Display,
    /// What its content was last set to (the text's JSON, the item, the block), so the same
    /// content is never sent twice.
    sent: Option<String>,
    /// The player it is for, for a per-viewer copy.
    pub viewer: Option<PlayerId>,
}

impl Shown {
    /// Takes up an entity of the world as the hologram's.
    #[must_use]
    pub fn adopt(entity: Entity, kind: Kind) -> Option<Self> {
        let display = match kind {
            Kind::Text => Display::Text(TextDisplayEntity::from_entity(&entity)?),
            Kind::Item => Display::Item(ItemDisplayEntity::from_entity(&entity)?),
            Kind::Block => Display::Block(BlockDisplayEntity::from_entity(&entity)?),
        };
        Some(Self {
            entity,
            display,
            sent: None,
            viewer: None,
        })
    }

    /// Makes a new entity for `h` in `world`, tagged with `mode`. A session entity is never saved
    /// with its chunk, and hidden from everybody until it is shown to its players; both are set
    /// in the tick it is made, so no client is ever sent it first and no save ever holds it.
    #[must_use]
    pub fn spawn(
        world: &World,
        h: &Hologram,
        mode: Mode,
        viewer: Option<PlayerId>,
    ) -> Option<Self> {
        let p = &h.place;
        let entity = world.spawn_entity(entity_type(h.kind), (p.x, p.y, p.z));
        if !mode.persistent() {
            entity.set_persistent(false);
            entity.set_visible_by_default(false);
        }
        entity.set_string(NAMESPACE, ID_KEY, &h.id);
        entity.set_string(NAMESPACE, MODE_KEY, mode.name());
        let mut shown = Self::adopt(entity, h.kind)?;
        shown.viewer = viewer;
        Some(shown)
    }

    /// The entity's id, as the player visibility calls take it.
    #[must_use]
    pub fn entity_id(&self) -> u32 {
        self.entity.get_id()
    }

    #[must_use]
    pub fn entity(&self) -> &Entity {
        &self.entity
    }

    /// Whether the entity is of `kind`.
    #[must_use]
    pub const fn is_kind(&self, kind: Kind) -> bool {
        matches!(
            (&self.display, kind),
            (Display::Text(_), Kind::Text)
                | (Display::Item(_), Kind::Item)
                | (Display::Block(_), Kind::Block)
        )
    }

    /// Puts the entity at the hologram's place, turned its way.
    pub fn place(&self, h: &Hologram, world: World) {
        let p = &h.place;
        self.entity.teleport((p.x, p.y, p.z), world);
        self.entity.set_rotation(p.yaw, p.pitch);
    }

    /// Everything about how it looks, but the content.
    pub fn look(&self, h: &Hologram) {
        let d = match &self.display {
            Display::Text(t) => {
                t.set_line_width(h.width);
                let (background, default) = match h.background {
                    Background::Default => (0x4000_0000_u32, false),
                    Background::None => (0, false),
                    Background::Argb(v) => (v, false),
                };
                t.set_default_background(default);
                t.set_background(argb(background));
                t.set_shadow(h.text_shadow);
                t.set_see_through(h.see_through);
                t.set_alignment(match h.align {
                    Align::Center => TextAlignment::Center,
                    Align::Left => TextAlignment::Left,
                    Align::Right => TextAlignment::Right,
                });
                t.get_display()
            }
            Display::Item(i) => i.get_display(),
            Display::Block(b) => b.get_display(),
        };
        d.set_billboard(match h.billboard {
            Billboard::Fixed => BillboardMode::Fixed,
            Billboard::Vertical => BillboardMode::Vertical,
            Billboard::Horizontal => BillboardMode::Horizontal,
            Billboard::Center => BillboardMode::Center,
        });
        let [sx, sy, sz] = h.scale;
        let [mut ox, oy, mut oz] = h.offset;
        if h.kind == Kind::Block {
            // A block is drawn from its corner; this puts its middle on the place.
            ox -= 0.5 * sx;
            oz -= 0.5 * sz;
        }
        d.set_transformation(
            TransformationBuilder::new()
                .translation(ox, oy, oz)
                .scale(sx, sy, sz)
                .build(),
        );
        d.set_view_range(h.range / RANGE_UNIT);
        d.set_shadow_radius(h.shadow_radius);
        d.set_shadow_strength(h.shadow_strength);
        d.set_brightness(h.brightness.map_or(-1, |(block, sky)| {
            (i32::from(block) << 4) | (i32::from(sky) << 20)
        }));
        self.entity.set_rotation(h.place.yaw, h.place.pitch);
    }

    /// Sets the content: `text` (a text component's JSON) for a text hologram, the item or the
    /// block for the others. Returns whether anything was sent.
    pub fn content(&mut self, h: &Hologram, text: Option<&str>) -> bool {
        let want = match self.display {
            Display::Text(_) => text.unwrap_or("{\"text\":\"\"}").to_owned(),
            Display::Item(_) => h.item.clone(),
            Display::Block(_) => h.block.clone(),
        };
        if self.sent.as_deref() == Some(want.as_str()) {
            return false;
        }
        match &self.display {
            Display::Text(t) => {
                t.set_text(
                    TextComponent::from_json(&want).unwrap_or_else(|_| TextComponent::text("")),
                );
            }
            Display::Item(i) => i.set_item(Some(ItemStack::new(&h.item, 1))),
            Display::Block(b) => {
                if let Some(state) = block_state(&h.block) {
                    b.set_block_state_id(state);
                }
            }
        }
        self.sent = Some(want);
        true
    }

    /// Forgets what was sent, so the next [`Shown::content`] sends it again.
    pub fn resend(&mut self) {
        self.sent = None;
    }

    /// Removes the entity from the world.
    pub fn remove(self) {
        self.entity.remove();
    }
}

/// A colour as the game's signed ARGB.
#[allow(clippy::cast_possible_wrap)]
const fn argb(color: u32) -> i32 {
    color as i32
}
