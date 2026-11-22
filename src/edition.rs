//! What Bedrock players see of holograms.
//!
//! Holograms are the game's display entities. Today the server shows no display entity to
//! Bedrock players, so they see no hologram at all, and `/holo list` and `/holo info` say so.
//! When the server shows display entities to Bedrock players, this switch turns the note off;
//! nothing else in the plugin changes: the same entities, the same per-player visibility.
//!
//! What Bedrock players will see then (the server's plan): a text hologram as a name tag with
//! its lines and colours, turning to the player as name tags do; background, see-through and
//! text shadow stay Java-only. Item and block holograms show their item or block, scaled and
//! turned.

/// Whether Bedrock players see display entities on this server.
pub const BEDROCK_SEES_DISPLAYS: bool = false;
