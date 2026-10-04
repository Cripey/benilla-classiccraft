//! classiccraft: an external driver for our body. A crate on top (the Minecraft bridge) supplies
//! the pose each frame in [`ExternalDriveSet::Supply`]; [`super::controller::control`] then takes
//! the feet, facing, ground contact and camera from it instead of the keyboard mover and the camera
//! rig, and everything downstream (flags, gait, the movement stream, acks) runs unchanged, so the
//! server sees an ordinary mover. Not a 1.12.1 behaviour: this module exists only in the fork.
//!
//! The body yields to the server: when a server move (teleport, GM move, anticheat snap-back)
//! lands on `Player::pos` between two driven frames, the frame is not driven and
//! [`ExternalDrive::server_moves`] counts it, so the driver can re-place itself at the new spot.

use bevy::prelude::*;

use super::{mover, Player};

/// One frame of the external driver's pose, in Bevy coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExternalPose {
    /// The body's feet.
    pub feet: Vec3,
    /// The body's velocity (yd/s), for the jump and fall tails the movement stream sends.
    pub velocity: Vec3,
    /// The facing, `Player::face_yaw`'s convention (equal to the WoW orientation).
    pub face_yaw: f32,
    /// The camera's position.
    pub eye: Vec3,
    /// The camera's yaw (Bevy, around +Y) and pitch (radians, positive looks up).
    pub eye_yaw: f32,
    pub eye_pitch: f32,
    /// Standing on something this frame.
    pub grounded: bool,
    /// The driver's camera is first person: the body hides as in benilla's own zoom-to-zero.
    pub first_person: bool,
    /// The transport ([`DeckTransport`]) the driver stands aboard, as the driver decides it: the
    /// rider attach follows it.
    pub deck: Option<Entity>,
    /// Whether the driver's word on [`Self::deck`] was read this frame: then `None` means it left
    /// the deck, and the ride ends even in the air.
    pub deck_known: bool,
}

/// The external driver's mailbox. `pose` is rewritten by the driver every frame (or cleared);
/// the rest is written by `control`.
#[derive(Resource, Default, Debug)]
pub struct ExternalDrive {
    /// This frame's pose, or `None` to leave the body to benilla.
    pub pose: Option<ExternalPose>,
    /// The feet `control` last wrote from a pose, to tell a server move from our own.
    pub(super) applied: Option<Vec3>,
    /// Frames in which a server move preempted the external pose; the driver re-places on change.
    pub server_moves: u32,
    /// The pose drove the body this frame.
    pub drove: bool,
}

/// Ordering handles for the driver: supply the pose before `control`, read the result after.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExternalDriveSet {
    Supply,
    Report,
}

/// What benilla reports about our body for the driver, refreshed after `control` each frame.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct SelfReport {
    /// In world, the server has placed us, we drive our own body and no loading cover is up: the
    /// world, and with it the collision a driver needs, has streamed in.
    pub in_world: bool,
    /// Feet in WoW coordinates (yards).
    pub wow_pos: [f32; 3],
    /// The WoW orientation (radians).
    pub facing: f32,
    /// The map the world streams (`CurrentMap`), `None` before the first world entry.
    pub map: Option<u32>,
    /// Our character's guid, 0 before login.
    pub guid: u64,
    /// The settle hold after a teleport or login: the destination is still streaming in.
    pub settling: bool,
    /// The `TerrainType.dbc` id under our feet, as WoW's own footsteps pick it (a building's floor
    /// indoors, the ground texture outside); `None` for none. Minecraft's footsteps follow it.
    pub ground_terrain: Option<u32>,
    /// Our character's race and gender (`UNIT_FIELD_BYTES_0`), `None` before its fields arrive:
    /// Minecraft's `/dance` with no race dances as our own.
    pub race_sex: Option<(u8, u8)>,
    /// Our stand state (`UNIT_FIELD_BYTES_1` byte 0: 1 sit, 2/4/5/6 chair, ...), as the server
    /// echoed it: Minecraft's Steve sits while WoW's body does (a chair used from the crosshair).
    pub stand_state: u8,
}

/// classiccraft: a custom-opcode packet for the server (`benilla_protocol` `CMSG_CC_*`), body built
/// by the crate on top; queued to the write thread in order.
/// A transport (boat, zeppelin, tram car, lift), by the guid its riders' movement names: the
/// crate on top exports its deck to the external driver as a moving platform (fork only).
#[derive(Component, Clone, Copy, Debug)]
pub struct DeckTransport {
    pub guid: u64,
}

#[derive(bevy::ecs::message::Message, Clone, Debug)]
pub struct CustomPacketOut {
    pub opcode: u16,
    pub body: Vec<u8>,
}

/// classiccraft: a custom-opcode packet from the server (`SMSG_CC_*`), body raw.
#[derive(bevy::ecs::message::Message, Clone, Debug)]
pub struct CustomPacketIn {
    pub opcode: u16,
    pub body: Vec<u8>,
}

fn send_custom(mut out: MessageReader<CustomPacketOut>, net: Option<Res<crate::net::NetCommands>>) {
    let Some(net) = net else {
        out.clear();
        return;
    };
    for p in out.read() {
        let _ = net.0.send(crate::net::ClientCommand::ClassicCraft {
            opcode: p.opcode,
            body: p.body.clone(),
        });
    }
}

/// classiccraft: a chat line typed in the external driver (a `.` GM command, 2026-10-03), sent as
/// WoW's own edit box sends a plain line: `Say`, which the server parses commands from.
#[derive(bevy::ecs::message::Message, Clone, Debug)]
pub struct ChatOut {
    pub text: String,
}

/// classiccraft: a system chat line from the server (GM command replies, notices), for the
/// external driver to show.
#[derive(bevy::ecs::message::Message, Clone, Debug)]
pub struct ChatIn {
    pub text: String,
}

/// classiccraft: NPC lines (say, yell, emote, whisper) as WoW's chat shows them, `$`-macros expanded
/// and languages garbled, queued for [`ChatIn`] (2026-10-03: town criers and quest NPCs in
/// Minecraft's chat). Coloured with Minecraft's `§` codes as WoW colours them.
static NPC_LINES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn npc_line(chat_type: u8, sender: Option<&str>, text: &str) {
    use benilla_protocol::messages as m;
    let who = sender.unwrap_or("Someone");
    let line = match chat_type {
        m::CHAT_MSG_MONSTER_SAY => format!("§f{who} says: {text}"),
        m::CHAT_MSG_MONSTER_YELL => format!("§c{who} yells: {text}"),
        m::CHAT_MSG_MONSTER_WHISPER | m::CHAT_MSG_RAID_BOSS_WHISPER => {
            format!("§d{who} whispers: {text}")
        }
        // An emote names its speaker where `%s` stands, else leads with it.
        m::CHAT_MSG_MONSTER_EMOTE | m::CHAT_MSG_RAID_BOSS_EMOTE if text.contains("%s") => {
            format!("§6{}", text.replace("%s", who))
        }
        m::CHAT_MSG_MONSTER_EMOTE | m::CHAT_MSG_RAID_BOSS_EMOTE => format!("§6{who} {text}"),
        _ => return,
    };
    if let Ok(mut q) = NPC_LINES.lock() {
        if q.len() < 64 {
            q.push(line);
        }
    }
}

fn send_npc_lines(mut out: MessageWriter<ChatIn>) {
    let lines = NPC_LINES.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
    for text in lines {
        out.write(ChatIn { text });
    }
}

fn send_chat(mut out: MessageReader<ChatOut>, net: Option<Res<crate::net::NetCommands>>) {
    let Some(net) = net else {
        out.clear();
        return;
    };
    for line in out.read() {
        let _ = net.0.send(crate::net::ClientCommand::Chat {
            kind: crate::net::ChatKind::Say,
            target: None,
            text: line.text.clone(),
            language: None,
        });
    }
}

fn receive_chat(In(ev): In<benilla_protocol::SessionEvent>, mut out: MessageWriter<ChatIn>) {
    if let benilla_protocol::SessionEvent::Chat(m) = ev {
        if m.chat_type == benilla_protocol::messages::CHAT_MSG_SYSTEM && !m.is_addon() {
            out.write(ChatIn { text: m.text });
        }
    }
}

// ---- classiccraft: interacting from the external driver's crosshair (2026-10-03) ----
//
// The foundation for talking to NPCs and using objects from Minecraft mode (quests, vendors,
// gossip, books, chairs later): benilla picks what is under the screen centre - the driver's
// crosshair, its camera being ours - with WoW's own pick (`target::hover`), publishes it
// ([`CrosshairTarget`]), and a driver's right-click ([`CrosshairUse`]) runs WoW's own right-click
// on it (`target::crosshair`), so the server answers as it would a mouse click.

static CROSSHAIR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The crate on top: whether the pick follows the screen centre (the driver drives and the WoW UI
/// is not in use), not the mouse cursor.
pub fn set_crosshair(on: bool) {
    CROSSHAIR.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn crosshair() -> bool {
    CROSSHAIR.load(std::sync::atomic::Ordering::Relaxed)
}

static CINEMATIC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A cinematic is on screen (set every frame by `cinematic.rs`): the driver hands the keyboard and
/// mouse back to WoW meanwhile, so the cinematic frame's ESC skip works.
pub(crate) fn set_cinematic(on: bool) {
    CINEMATIC.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether a cinematic is on screen.
pub fn cinematic() -> bool {
    CINEMATIC.load(std::sync::atomic::Ordering::Relaxed)
}

/// Where the world pick aims: the screen centre in crosshair mode, else the mouse cursor.
pub(crate) fn pick_point(window: &Window) -> Option<Vec2> {
    if crosshair() {
        Some(Vec2::new(window.width() * 0.5, window.height() * 0.5))
    } else {
        window.cursor_position()
    }
}

/// What WoW object or unit is under the crosshair this frame, and what a right-click would do.
#[derive(Resource, Default, Clone, Debug, PartialEq)]
pub struct CrosshairTarget {
    pub guid: u64,
    /// WoW's cursor over it ([`CROSSHAIR_KINDS`]); 0: nothing to interact with.
    pub kind: u8,
    pub name: String,
    /// Along the view ray, yards.
    pub distance: f32,
    /// Out of reach (WoW's grayed cursor).
    pub unable: bool,
}

/// The cursor kinds a [`CrosshairTarget`] names, by index (0 = none).
pub const CROSSHAIR_KINDS: [&str; 16] = [
    "", "attack", "speak", "loot", "interact", "buy", "read", "trainer", "taxi", "skin", "mail",
    "mine", "herbs", "picklock", "repair", "cast",
];

/// The driver's right-click on [`CrosshairTarget`], taken by `target::crosshair` this frame.
#[derive(Resource, Default)]
pub struct CrosshairUse {
    pub pending: bool,
}

/// A book, plaque or letter read from the crosshair, for the driver to show (title, pages, `$`
/// tokens expanded).
#[derive(bevy::ecs::message::Message, Clone, Debug)]
pub struct BookOut {
    pub title: String,
    pub pages: Vec<String>,
}

fn receive_custom(
    In(ev): In<benilla_protocol::SessionEvent>,
    mut out: MessageWriter<CustomPacketIn>,
) {
    if let benilla_protocol::SessionEvent::ClassicCraft { opcode, body } = ev {
        out.write(CustomPacketIn { opcode, body });
    }
}

/// classiccraft: one WoW creature near us, as the Minecraft side needs it for a hittable stand-in.
#[derive(Clone, Copy, Debug)]
pub struct UnitSnapshot {
    pub guid: u64,
    /// Feet, WoW coordinates (yards).
    pub wow_pos: [f32; 3],
    pub facing: f32,
    pub entry: u32,
    pub display_id: u32,
    pub scale: f32,
    pub bounding_radius: f32,
    pub combat_reach: f32,
    pub health: u32,
    pub max_health: u32,
    pub level: u32,
    pub target: u64,
    pub unit_flags: u32,
    /// `CanAttack(player, unit)`, the reference's attackability (`target::can_attack`).
    pub attackable: bool,
    pub dead: bool,
}

/// classiccraft: the creatures within [`UNIT_RANGE`] of us, nearest first, refreshed each frame,
/// and our own state the Minecraft side mirrors.
#[derive(Resource, Default, Debug, Clone)]
pub struct NearbyUnits {
    pub units: Vec<UnitSnapshot>,
    pub self_level: u32,
    /// `PLAYER_XP` / `PLAYER_NEXT_LEVEL_XP`: the level bar (Minecraft's HUD).
    pub self_xp: u32,
    pub self_next_xp: u32,
    pub self_dead: bool,
    pub self_ghost: bool,
}

/// The stand-in radius (yd).
pub const UNIT_RANGE: f32 = 60.0;

#[allow(clippy::type_complexity)]
fn publish_units(
    player: Res<Player>,
    units: Query<(
        &crate::net::Guid,
        &crate::net::NetEntity,
        &crate::net::ObjectStore,
        &GlobalTransform,
    )>,
    me: Query<&crate::net::ObjectStore, With<crate::net::SelfPlayer>>,
    factions: Option<Res<crate::target::Factions>>,
    reputations: Res<crate::net::Reputations>,
    mut out: ResMut<NearbyUnits>,
) {
    let me = me.iter().next();
    out.units.clear();
    out.self_level = me.and_then(|m| m.0.unit_level()).unwrap_or(0);
    out.self_xp = me.and_then(|m| m.0.player_xp()).unwrap_or(0);
    out.self_next_xp = me.and_then(|m| m.0.player_next_level_xp()).unwrap_or(0);
    out.self_dead = me.is_some_and(|m| m.0.unit_is_dead());
    out.self_ghost = me.is_some_and(|m| m.0.player_is_ghost());
    if !player.active {
        return;
    }
    for (guid, net, store, gt) in &units {
        if net.kind != benilla_protocol::EntityKind::Unit {
            continue;
        }
        let pos = gt.translation();
        if pos.distance(player.pos) > UNIT_RANGE {
            continue;
        }
        let f = &store.0;
        let fwd = gt.forward();
        out.units.push(UnitSnapshot {
            guid: guid.0,
            wow_pos: benilla_assets::coords::bevy_to_wow(pos),
            // Bevy forward (-sin f, 0, -cos f) for WoW facing f.
            facing: (-fwd.x).atan2(-fwd.z),
            entry: f.object_entry().unwrap_or(0),
            display_id: f.unit_displayid().unwrap_or(0).max(0) as u32,
            scale: f.object_scale_x().unwrap_or(1.0),
            bounding_radius: f.unit_bounding_radius(),
            combat_reach: f.unit_combat_reach(),
            health: f.unit_health().unwrap_or(0),
            max_health: f.unit_max_health().unwrap_or(0),
            level: f.unit_level().unwrap_or(0),
            target: f.unit_target().unwrap_or(0),
            unit_flags: f.unit_flags(),
            attackable: crate::target::can_attack(
                Some(store),
                factions.as_deref(),
                &reputations,
                me,
            ),
            dead: f.unit_is_dead(),
        });
    }
    let here = player.pos;
    out.units.sort_by(|a, b| {
        let d = |u: &UnitSnapshot| {
            benilla_assets::coords::wow_to_bevy(u.wow_pos).distance_squared(here)
        };
        d(a).total_cmp(&d(b))
    });
}

/// classiccraft: creature entries this client treats as `UNIT_FLAG_NOT_SELECTABLE` (the bridge's
/// server-side stand-ins for Minecraft mobs), so they get no nameplate and cannot be targeted. Set
/// once by the crate on top; the server keeps them selectable, since WoW AI must still attack them.
static UNSELECTABLE_ENTRIES: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();

/// classiccraft: see [`UNSELECTABLE_ENTRIES`]. Later calls are ignored.
pub fn set_unselectable_entries(entries: Vec<u32>) {
    let _ = UNSELECTABLE_ENTRIES.set(entries);
}

/// classiccraft: ORs `NOT_SELECTABLE` (flags bit 25) into a creature's incoming create or delta
/// when its entry is listed, before it reaches the store, so no reader ever sees it selectable.
pub(crate) fn mask_unselectable(guid: u64, fields: &mut benilla_protocol::messages::ObjectFields) {
    use benilla_protocol::field::FIELD_UNIT_FLAGS;
    const NOT_SELECTABLE: u32 = 1 << 25;
    let Some(entries) = UNSELECTABLE_ENTRIES.get() else {
        return;
    };
    if benilla_protocol::guid::high(guid) != benilla_protocol::guid::HIGH_UNIT
        || !benilla_protocol::guid::entry(guid).is_some_and(|e| entries.contains(&e))
    {
        return;
    }
    // A create reads an absent flags field as 0; a delta without one leaves the stored value.
    let flags = fields
        .raw_fields()
        .find(|&(i, _)| i == FIELD_UNIT_FLAGS)
        .map(|(_, v)| v)
        .or_else(|| fields.created_as().map(|_| 0));
    if let Some(flags) = flags {
        fields.merge(benilla_protocol::messages::ObjectFields::from_pairs(&[(
            FIELD_UNIT_FLAGS,
            flags | NOT_SELECTABLE,
        )]));
    }
}

pub(super) fn plugin(app: &mut App) {
    use crate::net::handlers::NetHandlerApp;
    app.init_resource::<NearbyUnits>()
        .add_systems(Update, publish_units.in_set(ExternalDriveSet::Report));
    app.add_message::<CustomPacketOut>()
        .add_message::<CustomPacketIn>()
        .add_systems(Update, send_custom)
        .net_handler(
            benilla_protocol::SessionEventKind::ClassicCraft,
            receive_custom,
        );
    app.init_resource::<CrosshairTarget>()
        .init_resource::<CrosshairUse>()
        .add_message::<BookOut>();
    super::external_dialog::plugin(app);
    app.add_message::<ChatOut>()
        .add_message::<ChatIn>()
        .add_systems(Update, (send_chat, send_npc_lines))
        .net_handler(benilla_protocol::SessionEventKind::Chat, receive_chat);
    app.init_resource::<ExternalDrive>()
        .init_resource::<SelfReport>()
        .configure_sets(
            Update,
            (
                ExternalDriveSet::Supply.before(super::PlayerControlSet),
                ExternalDriveSet::Report.after(super::PlayerControlSet),
            ),
        )
        .add_systems(Update, publish_report.in_set(ExternalDriveSet::Report))
        .add_systems(
            Update,
            publish_ground_terrain
                .after(publish_report)
                .in_set(ExternalDriveSet::Report),
        );
}

/// classiccraft: the surface under our feet for Minecraft's footsteps, through the same chain as
/// WoW's own (`sound::footsteps`).
fn publish_ground_terrain(
    player: Res<Player>,
    me: Query<Entity, With<crate::net::SelfPlayer>>,
    footsteps: Option<Res<crate::sound::footsteps::Footsteps>>,
    world: benilla_world::world_point::WorldPoint,
    objects: benilla_world::object_surface::ObjectUnderfoot,
    models: Query<(&ChildOf, &benilla_world::interact::WorldObject)>,
    mut report: ResMut<SelfReport>,
    mut logged_deck: Local<Option<Entity>>,
) {
    // Our own unit, as WoW's footsteps ask (`Subject::Unit`): its WMO room claim puts a building's
    // floor under us. `Subject::Player` carries no room there and always read the ground beneath
    // the building (2026-10-02).
    let Some(me) = me.iter().next() else {
        report.ground_terrain = None;
        return;
    };
    let who = benilla_world::world_point::Subject::Unit(me);
    let authored = footsteps.and_then(|f| world.terrain_type(&f.0, who, player.pos));
    // Many building floors name no footstep (`TerrainType` 10, None, or none at all): WoW plays
    // nothing there. Their texture's name decides instead (wood, stone, carpet...).
    let texture = matches!(authored, None | Some(TERRAIN_NONE))
        .then(|| world.floor_texture(who, player.pos))
        .flatten();
    let room = world.room_group(who);
    // Aboard a transport: its deck, by its model's name. A ray from the feet misses a moving deck
    // (its collider trails it a frame) and read the terrain or sea under it (2026-10-03).
    if let Some(deck) = player.ride_entity() {
        if let Some((_, model)) = models.iter().find(|(of, _)| of.parent() == deck) {
            report.ground_terrain = Some(terrain_of_doodad(&model.label));
            if logged_deck.replace(deck) != Some(deck) {
                info!(
                    "classiccraft: footsteps aboard {} -> terrain {:?}",
                    model.label, report.ground_terrain
                );
            }
            return;
        }
    }
    // Outdoors, a bridge, dock, porch or step: WoW's claim covers interior groups only and reads
    // the terrain under it (2026-10-02, user: find the object, name its sound).
    let object = room.is_none().then(|| objects.object_under(player.pos)).flatten();
    report.ground_terrain = match &object {
        Some(o) => Some(terrain_of_object(o)),
        None => texture.as_deref().map(terrain_of_texture).or(authored),
    };
}

/// `TerrainType.dbc` 10, "None": silent.
const TERRAIN_NONE: u32 = 10;
/// Not a `TerrainType` row: carpet, rugs, cloth (Minecraft's wool steps).
pub const TERRAIN_CLOTH: u32 = 11;

/// A building floor's footstep by its texture's name: the `TerrainType` its words suggest, stone
/// for the rest (most interior floors without one are stone or tile).
fn terrain_of_texture(texture: &str) -> u32 {
    terrain_of_words(texture).unwrap_or(TERRAIN_STONE)
}

const TERRAIN_DIRT: u32 = 0;
const TERRAIN_STONE: u32 = 2;
const TERRAIN_WOOD: u32 = 4;

/// The `TerrainType` a texture or model name's words suggest, if any.
fn terrain_of_words(name: &str) -> Option<u32> {
    let name = name.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| name.contains(w));
    if has(&["carpet", "rug", "cloth", "fabric", "tapestry"]) {
        Some(TERRAIN_CLOTH)
    } else if has(&["wood", "plank", "board", "timber", "log"]) {
        Some(TERRAIN_WOOD)
    } else if has(&["metal", "iron", "steel", "grate", "bronze", "copper"]) {
        Some(1)
    } else if has(&["dirt", "mud", "earth"]) {
        Some(TERRAIN_DIRT)
    } else if has(&["hay", "straw", "grass"]) {
        Some(5)
    } else if has(&["sand"]) {
        Some(7)
    } else if has(&["snow", "ice"]) {
        Some(3)
    } else if has(&["stone", "rock", "brick", "cobble", "marble", "tile"]) {
        Some(TERRAIN_STONE)
    } else {
        None
    }
}

/// A doodad by its model's file name (its folder names zones, "Duskwood" or "Ironforge", which
/// would read as wood or metal): stone words first, as "DuskwoodRock" is a rock; then wooden
/// things; else the texture words; else wood (most things stood on outdoors are steps, docks and
/// crates).
fn terrain_of_doodad(model: &str) -> u32 {
    let file = model
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| file.contains(w));
    if has(&[
        "rock", "stone", "boulder", "brick", "cobble", "marble", "statue", "ruin", "pillar",
        "column", "tomb", "grave", "obelisk", "monument",
    ]) {
        TERRAIN_STONE
    } else if has(&["subway", "tram"]) || has(&["elevator"]) && !has(&["thunderbluff"]) {
        // Transports: the Deeprun Tram's cars and the Undercity's lifts are metal; Thunder
        // Bluff's lifts are wooden platforms.
        1
    } else if has(&["zeppelin", "transport"]) {
        TERRAIN_WOOD // ship and zeppelin decks
    } else if has(&[
        "dock", "pier", "bridge", "porch", "stair", "step", "fence", "crate", "barrel", "box",
        "cart", "wagon", "ship", "boat", "stump", "tree", "trunk", "table", "bench", "chair", "bed",
        "shelf", "ladder", "scaffold", "raft", "platform",
    ]) {
        TERRAIN_WOOD
    } else {
        terrain_of_words(&file).unwrap_or(TERRAIN_WOOD)
    }
}

/// An object outdoors (classiccraft): a building's face by its texture as indoors (stone when its
/// name has no material word), else its authored ground type (exterior materials mostly leave it
/// 0 or None, which no reference code ever read), else stone; never the building's name ("snow_inn"
/// made the Kharanos inn's floor snow); a doodad by its model's name.
fn terrain_of_object(object: &benilla_world::object_surface::ObjectSurface) -> u32 {
    use benilla_world::object_surface::ObjectSurface;
    match object {
        ObjectSurface::Doodad { model } => terrain_of_doodad(model),
        ObjectSurface::Wmo {
            ground_type,
            texture,
            ..
        } => texture
            .as_deref()
            .map(terrain_of_texture)
            .or(ground_type.filter(|g| !matches!(*g, TERRAIN_DIRT | TERRAIN_NONE)))
            .unwrap_or(TERRAIN_STONE),
    }
}

fn publish_report(
    player: Res<Player>,
    guid: Res<crate::net::SelfGuid>,
    map: Option<Res<benilla_world::world_map::CurrentMap>>,
    loading: Res<crate::loading_screen::LoadingScreen>,
    me: Query<&crate::net::ObjectStore, With<crate::net::SelfPlayer>>,
    mut report: ResMut<SelfReport>,
) {
    *report = SelfReport {
        in_world: player.active
            && player.foreign_mover.is_none()
            && !player.detached
            // A cover only for terrain streaming in under the body keeps us in world: the driver
            // owns the body (an elytra flight outran the streamer; the cover handed the body to
            // benilla, which dropped it, and its end re-placed Steve there, 2026-10-02).
            && (!loading.covering() || loading.streaming_only()),
        wow_pos: benilla_assets::coords::bevy_to_wow(player.pos),
        facing: player.face_yaw,
        map: map.map(|m| m.0),
        guid: guid.0.unwrap_or(0),
        settling: player.settling,
        ground_terrain: report.ground_terrain,
        race_sex: me
            .iter()
            .next()
            .and_then(|s| Some((s.0.unit_race()?, s.0.unit_gender()?))),
        stand_state: me.iter().next().map_or(0, |s| s.0.unit_stand_state()),
    };
}

/// The pose for this frame, or `None` when the body is benilla's: no pose, or a server move landed
/// on `Player::pos` since we last drove it (counted for the driver's re-placement).
pub(super) fn take_pose(drive: &mut ExternalDrive, player: &Player) -> Option<ExternalPose> {
    drive.drove = false;
    let Some(pose) = drive.pose else {
        // An undriven frame: benilla's own mover may move the body, which is no server move.
        drive.applied = None;
        return None;
    };
    if let Some(applied) = drive.applied {
        if player.pos.distance(applied) > 0.01 {
            drive.applied = None;
            drive.server_moves = drive.server_moves.wrapping_add(1);
            return None;
        }
    }
    Some(pose)
}

/// The external pose in place of [`mover::step`]: writes the body and reports the step's verdict.
/// A jump is a take-off from the ground with an upward velocity; the settle hold freezes the body
/// as the mover's does.
pub(super) fn step(
    drive: &mut ExternalDrive,
    player: &mut Player,
    pose: ExternalPose,
) -> mover::Outcome {
    let was_grounded = player.airborne_since.is_none();
    if player.settling {
        drive.applied = Some(player.pos);
        return mover::Outcome {
            held: true,
            grounded: true,
            jumped: false,
            knocked: false,
            air_nudged: false,
            ground: None,
        };
    }
    // The driver left the deck, in the air too: a jump or glide off it must not ride on with the
    // transport (flying behind a boat, the server carried the rider across with it, 2026-10-03).
    if pose.deck_known && pose.deck.is_none() && player.ride.take().is_some() {
        info!("transport: deboard (the external driver left the deck)");
    }
    player.pos = pose.feet;
    player.horiz_vel = Vec3::new(pose.velocity.x, 0.0, pose.velocity.z);
    player.vel_y = if pose.grounded { 0.0 } else { pose.velocity.y };
    drive.applied = Some(pose.feet);
    drive.drove = true;
    // The deck the driver stands aboard, for the transport attach (`ride::update_attachment`): we
    // board it and the movement carries the deck-local pose (2026-10-02, the Deeprun Tram). The
    // driver's word, not a ray: avian's colliders on a moving car trail it by a frame, and a ray
    // from the feet missed it (deboard and board ~20 times a second the whole ride).
    let ground = pose.deck;
    mover::Outcome {
        held: false,
        grounded: pose.grounded,
        jumped: was_grounded && !pose.grounded && pose.velocity.y > 1.0,
        knocked: false,
        air_nudged: false,
        ground,
    }
}

/// The net forward and strafe axes of the pose's horizontal motion against its facing, as the
/// keyboard would have netted them: a direction counts past ~22° off the axis (cos 67.5°).
pub(super) fn axes_of(pose: &ExternalPose) -> (i32, i32) {
    let horiz = Vec3::new(pose.velocity.x, 0.0, pose.velocity.z);
    let speed = horiz.length();
    if speed < 0.3 {
        return (0, 0);
    }
    let rot = Quat::from_rotation_y(pose.face_yaw);
    let fwd = (rot * Vec3::NEG_Z).dot(horiz) / speed;
    let right = (rot * Vec3::X).dot(horiz) / speed;
    let axis = |v: f32| {
        if v > 0.38 {
            1
        } else if v < -0.38 {
            -1
        } else {
            0
        }
    };
    (axis(fwd), axis(right))
}

/// The camera at the pose's eye, looking along its yaw and pitch.
pub(super) fn seat_camera(pose: &ExternalPose, cam_t: &mut Transform) {
    cam_t.translation = pose.eye;
    cam_t.rotation = Quat::from_rotation_y(pose.eye_yaw) * Quat::from_rotation_x(pose.eye_pitch);
}

/// A server-driven ride (a taxi, a charge, a knockback spline) owns the body: the camera rides it,
/// looking where the driver looks, first person at the eye or third person behind it, and the
/// next driven frame re-places the driver at wherever the ride ended.
pub(super) fn seat_riding_camera(
    drive: &mut ExternalDrive,
    pose: &ExternalPose,
    head: Vec3,
    cam_t: &mut Transform,
) {
    drive.drove = false;
    // Not a position the server could match: the first driven frame after the ride counts as a
    // server move and re-places the driver.
    drive.applied = Some(Vec3::splat(f32::MAX));
    let rotation = Quat::from_rotation_y(pose.eye_yaw) * Quat::from_rotation_x(pose.eye_pitch);
    // Minecraft's third-person camera sits 4 blocks behind the eye.
    let back = if pose.first_person { 0.0 } else { 4.0 * 1.4667 };
    cam_t.translation = head + rotation * Vec3::Z * back;
    cam_t.rotation = rotation;
}

#[cfg(test)]
mod tests {
    use benilla_protocol::field::FIELD_UNIT_FLAGS;
    use benilla_protocol::messages::{ObjectFields, ObjectType};

    use super::*;

    /// A creature guid as vmangos builds it (`ObjectGuid.h:123`).
    fn creature(entry: u32, counter: u32) -> u64 {
        (u64::from(benilla_protocol::guid::HIGH_UNIT) << 48)
            | (u64::from(entry) << 24)
            | u64::from(counter)
    }

    #[test]
    fn listed_creatures_arrive_unselectable_and_others_untouched() {
        set_unselectable_entries(vec![990003]);
        // A create with no flags field reads 0, so the bit lands.
        let mut create = ObjectFields::default().into_created(ObjectType::Unit);
        mask_unselectable(creature(990003, 7), &mut create);
        assert_eq!(create.unit_flags(), 1 << 25);
        // A delta carrying flags keeps its own bits and gains ours.
        let mut delta = ObjectFields::from_pairs(&[(FIELD_UNIT_FLAGS, 0x8)]);
        mask_unselectable(creature(990003, 7), &mut delta);
        assert_eq!(delta.unit_flags(), 0x8 | 1 << 25);
        // A delta without flags stays without, so the stored value survives.
        let mut other = ObjectFields::from_pairs(&[(FIELD_UNIT_FLAGS + 1, 5)]);
        mask_unselectable(creature(990003, 7), &mut other);
        assert!(other.raw_fields().all(|(i, _)| i != FIELD_UNIT_FLAGS));
        // Any other creature is left alone.
        let mut boar = ObjectFields::from_pairs(&[(FIELD_UNIT_FLAGS, 0x8)]);
        mask_unselectable(creature(1125, 7), &mut boar);
        assert_eq!(boar.unit_flags(), 0x8);
    }

    #[test]
    fn objects_outdoors_sound_by_name_not_by_zone_folder() {
        use benilla_world::object_surface::ObjectSurface;
        let doodad = |m: &str| terrain_of_object(&ObjectSurface::Doodad { model: m.into() });
        // The zone folder says "wood"; the rock is stone.
        assert_eq!(doodad("World\\Azeroth\\Duskwood\\PassiveDoodads\\Rocks\\DuskwoodRock01.m2"), TERRAIN_STONE);
        assert_eq!(doodad("World/Generic/Human/Passive Doodads/Docks/HumanDock01.m2"), TERRAIN_WOOD);
        // Unknown things stood on outdoors: wood.
        assert_eq!(doodad("World/Generic/Gnome/Passive Doodads/Gizmo01.m2"), TERRAIN_WOOD);
        let wmo = |texture: Option<&str>, ground_type: Option<u32>| {
            terrain_of_object(&ObjectSurface::Wmo {
                model: "world/wmo/khazmodan/buildings/dwarven_inn/snow_inn/snow_inn.wmo".into(),
                ground_type,
                texture: texture.map(str::to_owned),
            })
        };
        assert_eq!(wmo(Some("Dungeons/Textures/Floor/WoodPlanks.blp"), Some(0)), TERRAIN_WOOD);
        // A texture without a material word is stone, as indoors; the "snow" in the name is not.
        assert_eq!(wmo(Some("DUNGEONS\\TEXTURES\\FLOOR\\MM_DWRF_SFLOOR_04.BLP"), Some(10)), TERRAIN_STONE);
        // An unread exterior ground type (0, Dirt) is no answer.
        assert_eq!(wmo(None, Some(TERRAIN_DIRT)), TERRAIN_STONE);
        assert_eq!(wmo(None, Some(1)), 1);
    }
}
