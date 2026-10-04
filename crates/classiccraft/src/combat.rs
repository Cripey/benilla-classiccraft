//! The combat crossover's client leg (server: the vmangos fork's `ClassicCraft.h`).
//!
//! - WoW creatures near us go to Minecraft as stand-ins (`protocol/mcwow_actors_protocol.h` v2), with
//!   a ring of the server's hits on what Minecraft owns (`SMSG_CC_DAMAGE`: the player, or a mob).
//! - Minecraft's hits on stand-ins (`REN_HIT`) go to the server as `CMSG_CC_HIT`, from the player or
//!   from the hitting mob's proxy; its mob list (`REN_MOBS`) becomes `CMSG_CC_ACTORS`, the proxies.
//! - Minecraft death (`REN_EVENT` 1) is `CMSG_CC_DIED`; Minecraft's respawn (`REN_RESPAWN`) is
//!   `CMSG_CC_RESPAWN`: the character comes back at Steve's bed or its hearthstone location.
//! - `CMSG_CC_HELLO` tells the server whether Minecraft owns our health (while it drives us).
//! - Kill XP waits in Minecraft's XP orbs (2026-10-02): `SMSG_CC_XP_DROP` goes into the same ring as
//!   kind [`RING_XP_DROP`]; a picked-up orb (`REN_XP_CLAIM`) is `CMSG_CC_XP_CLAIM`. The level bar
//!   (`PLAYER_XP`, `PLAYER_NEXT_LEVEL_XP`) rides in the header.
//! - Kills (2026-10-03): `SMSG_CC_KILL` goes into the ring as kind [`RING_KILL`] for the mod's loot.
//! - Ore veins (2026-10-04): `REN_HARVEST` is `CMSG_CC_HARVEST`; the answer `SMSG_CC_HARVEST` goes into
//!   the ring as kind [`RING_HARVEST`] for the mod to drop the ore.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::sync::atomic::{fence, Ordering};

use benilla_app::external::{
    ChatIn, ChatOut, CustomPacketIn, CustomPacketOut, ExternalDriveSet, NearbyUnits, SelfReport,
};
use bevy::prelude::*;
use memmap2::MmapMut;

use crate::bridge::{Bridge, S};

const PATH: &str = "/dev/shm/classiccraft_actors_v1.shm";
const MAGIC: u32 = 0x6D63_7761; // "mcwa"
const VERSION: u32 = 7; // 7: + the visible model's height and width per actor
const MAX_ACTORS: usize = 64;
const HEADER: usize = 64;
const ACTOR_BYTES: usize = 80;
const RING_HEAD: usize = HEADER + MAX_ACTORS * ACTOR_BYTES; // 5184
const RING_DATA: usize = RING_HEAD + 8;
const RING_ENTRIES: u64 = 256;
const RING_ENTRY: usize = 32;
/// v3 (2026-10-03): the text ring, the server's system chat lines (GM command replies) for
/// Minecraft's chat: u64 head, then TEXT_SLOTS of u16 length + UTF-8.
const TEXT_HEAD: usize = RING_DATA + RING_ENTRIES as usize * RING_ENTRY; // 12872
const TEXT_DATA: usize = TEXT_HEAD + 8;
const TEXT_SLOTS: u64 = 64;
const TEXT_SLOT: usize = 1024; // v4: NPC speech runs long
/// v5 (2026-10-03): the crosshair focus, what WoW thing a right-click in Minecraft would act on
/// (`external::CrosshairTarget`): u32 seq (odd while written), u32 kind (`CROSSHAIR_KINDS`), f32
/// distance (blocks), u32 unable, u64 guid, u32 name length, then the name (UTF-8, <= 96 bytes).
const FOCUS: usize = TEXT_DATA + TEXT_SLOTS as usize * TEXT_SLOT;
const FOCUS_NAME_MAX: usize = 96;
/// v6: our own state for Minecraft: u32 stand state (`SelfReport::stand_state`), 28 spare bytes.
const SELF: usize = FOCUS + 128;
const TOTAL: usize = SELF + 32;

const ACTOR_ATTACKABLE: u32 = 0x1;
const ACTOR_DEAD: u32 = 0x2;
const ACTOR_TARGETS_ME: u32 = 0x4;

// benilla_protocol::messages::opcode (fork): CMSG_CC_* / SMSG_CC_DAMAGE.
const CMSG_CC_HELLO: u16 = 0x033C;
const CMSG_CC_HIT: u16 = 0x033D;
const CMSG_CC_ACTORS: u16 = 0x033E;
const CMSG_CC_DIED: u16 = 0x033F;
const SMSG_CC_DAMAGE: u16 = 0x0340;
const SMSG_CC_XP_DROP: u16 = 0x0341;
const CMSG_CC_XP_CLAIM: u16 = 0x0342;
/// Travel through a Minecraft waygate: u32 map, f32 x, y, z, o (WoW), u64 owner.
const CMSG_CC_WAYGATE: u16 = 0x0343;
/// A creature we killed, for Minecraft's loot (2026-10-03): guid victim, u32 entry, f32 x, y, z,
/// u32 level, rank, creature type, family, money (copper), flags, quest items bagged.
const SMSG_CC_KILL: u16 = 0x0345;
/// A ring entry's kind byte for an XP drop (0 and 1 are hits on the player and on a mob).
const RING_XP_DROP: u8 = 0x10;
/// ... and for a kill (`SMSG_CC_KILL`).
const RING_KILL: u8 = 0x11;
/// Release spirit, Minecraft's way: u32 kind (0 = hearthstone location, 1 = at), u32 map, f32 x, y, z, o.
const CMSG_CC_RESPAWN: u16 = 0x0346;
/// An ore vein mined with a Minecraft pickaxe (2026-10-04): u64 the vein's guid.
const CMSG_CC_HARVEST: u16 = 0x0347;
/// ... the server's answer: u64 vein, u32 entry, f32 x, y, z, u8 ok, u8 used up.
const SMSG_CC_HARVEST: u16 = 0x0348;
/// A ring entry's kind byte for a harvested vein (`SMSG_CC_HARVEST`).
const RING_HARVEST: u8 = 0x12;

/// The server's proxy creatures (`sql/custom/classiccraft_proxies.sql`): never mirrored back.
const PROXY_ENTRIES: std::ops::RangeInclusive<u32> = 990001..=990003;
/// The mod's stand-in hit flags (`McwowActorEntity.HIT_*`) and the server's (`ClassicCraft::HitFlags`).
const MC_HIT_PROJECTILE: u32 = 1;
const MC_HIT_CRITICAL: u32 = 2;
/// 2026-10-04: a damage-over-time tick, frost's slow, and the spell school in bits 8-10
/// (`SpellSchools`: 0 physical .. 6 arcane) - passed to the server as they are.
const MC_HIT_PERIODIC: u32 = 16;
const MC_HIT_SLOW: u32 = 32;
const HIT_SCHOOL_BITS: u32 = 0x7 << 8;
const CC_HIT_CRIT: u32 = 1;
const CC_HIT_PROJECTILE: u32 = 2;
const CC_HIT_PERIODIC: u32 = 4;
const CC_HIT_SLOW: u32 = 8;
/// How often the server is reminded who owns our health (s): it forgets on a relog or a restart.
const HELLO_PERIOD: f32 = 10.0;

/// A Minecraft mob near the player, from `REN_MOBS` (region-local Minecraft coordinates).
#[derive(Clone, Copy, Debug)]
pub struct McMob {
    pub id: u32,
    pub kind: u8,
    pub hp_pct: u8,
    pub pos: [f32; 3],
    pub yaw_deg: f32,
}

/// Combat messages from the render ring ([`crate::render`]).
#[derive(Debug)]
pub enum McMsg {
    Hit {
        guid: u64,
        wow_damage: u32,
        flags: u32,
        attacker: u32,
    },
    Event(u32),
    Mobs(Vec<McMob>),
    /// An XP orb picked up: its share of a drop's kill XP.
    XpClaim { drop: u32, xp: u32 },
    /// A chat line typed in Minecraft for WoW (a `.` GM command).
    Chat(String),
    /// Minecraft's right-click on the crosshair's WoW target.
    Interact,
    /// A waygate travel: `CMSG_CC_WAYGATE`'s body as Minecraft built it.
    Waygate(Vec<u8>),
    /// Steve respawned after dying: `CMSG_CC_RESPAWN`'s body as Minecraft built it.
    Respawn(Vec<u8>),
    /// An ore vein mined with a Minecraft pickaxe: `CMSG_CC_HARVEST`'s body (the vein's guid).
    Harvest(Vec<u8>),
    /// A choice in an NPC window shown in Minecraft.
    Dialog(benilla_app::external_dialog::DialogIn),
}

#[derive(Resource, Default)]
pub struct McInbox(pub Vec<McMsg>);

pub struct CombatPlugin;

impl Plugin for CombatPlugin {
    fn build(&self, app: &mut App) {
        // The proxies stand in for mobs Minecraft already draws: no nameplate, no targeting.
        benilla_app::external::set_unselectable_entries(PROXY_ENTRIES.collect());
        app.init_resource::<McInbox>()
            .init_resource::<Combat>()
            .add_systems(
                Update,
                (hello, write_actors, relay_damage, relay_chat, write_focus, handle_inbox)
                    .chain()
                    .in_set(ExternalDriveSet::Report),
            );
    }
}

#[derive(Resource, Default)]
struct Combat {
    map: Option<MmapMut>,
    retry_at: f32,
    frame: u64,
    /// What the server was last told (`CMSG_CC_HELLO`) and when.
    hello: Option<bool>,
    hello_at: f32,
    /// Minecraft mob ids that have a proxy, so the vanished ones are removed.
    proxied: HashSet<u32>,
}

impl Combat {
    fn open(&mut self, now: f32) {
        if self.map.is_some() || now < self.retry_at {
            return;
        }
        self.retry_at = now + 5.0;
        let Ok(file) = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PATH)
        else {
            return;
        };
        if file.set_len(TOTAL as u64).is_err() {
            return;
        }
        // SAFETY: our file; the mod only reads it.
        let Ok(mut map) = (unsafe { MmapMut::map_mut(&file) }) else {
            return;
        };
        map[..TOTAL].fill(0);
        map[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        map[4..8].copy_from_slice(&VERSION.to_le_bytes());
        info!("classiccraft: combat link {PATH} ready");
        self.map = Some(map);
    }

    fn put<const N: usize>(&mut self, off: usize, bytes: [u8; N]) {
        if let Some(map) = self.map.as_mut() {
            map[off..off + N].copy_from_slice(&bytes);
        }
    }

    fn seq(&mut self, v: u32) {
        if let Some(map) = self.map.as_mut() {
            // SAFETY: the 4-aligned seqlock word at 8.
            unsafe { std::ptr::write_volatile(map.as_mut_ptr().add(8).cast::<u32>(), v) }
        }
    }
}

fn send(out: &mut MessageWriter<CustomPacketOut>, opcode: u16, body: Vec<u8>) {
    out.write(CustomPacketOut { opcode, body });
}

/// Minecraft owns our health exactly while it drives us; the server hears of every change and is
/// reminded periodically (it forgets on a relog or a restart).
fn hello(
    time: Res<Time>,
    bridge: Res<Bridge>,
    report: Res<SelfReport>,
    units: Res<NearbyUnits>,
    mut combat: ResMut<Combat>,
    mut out: MessageWriter<CustomPacketOut>,
) {
    // A dead or ghost character fights nothing: no forwarded damage, no proxies (ghost mode).
    let want = bridge.driving() && report.in_world && !units.self_dead && !units.self_ghost;
    let now = time.elapsed_secs();
    if combat.hello != Some(want) || (want && now - combat.hello_at > HELLO_PERIOD) {
        if combat.hello != Some(want) {
            info!(
                "classiccraft: Minecraft combat {}",
                if want { "on" } else { "off" }
            );
        }
        combat.hello = Some(want);
        combat.hello_at = now;
        send(&mut out, CMSG_CC_HELLO, vec![want as u8]);
        if !want {
            combat.proxied.clear(); // the server dropped every proxy with it
        }
    }
}

/// The WoW creatures around us for Minecraft's stand-ins, under the seqlock.
fn write_actors(
    time: Res<Time>,
    report: Res<SelfReport>,
    units: Res<NearbyUnits>,
    mut combat: ResMut<Combat>,
    light: HandLight,
) {
    let combat = &mut *combat;
    combat.open(time.elapsed_secs());
    if combat.map.is_none() {
        return;
    }
    let seq = {
        let map = combat.map.as_ref().unwrap();
        u32::from_le_bytes(map[8..12].try_into().unwrap())
    };
    let seq = seq | 1; // odd while writing (also heals a writer that died mid-write)
    combat.seq(seq);
    fence(Ordering::Release);

    let list: Vec<_> = units
        .units
        .iter()
        .filter(|u| !PROXY_ENTRIES.contains(&u.entry))
        .take(MAX_ACTORS)
        .collect();
    combat.frame += 1;
    let frame = combat.frame;
    combat.put(12, (list.len() as u32).to_le_bytes());
    combat.put(16, report.guid.to_le_bytes());
    combat.put(24, units.self_level.to_le_bytes());
    combat.put(36, report.map.unwrap_or(u32::MAX).to_le_bytes());
    combat.put(40, frame.to_le_bytes());
    let death = u32::from(units.self_dead) | (u32::from(units.self_ghost) << 1);
    combat.put(56, death.to_le_bytes());
    // The light on Minecraft's first-person hand (Minecraft's lightmap, mod LightmapMixin): 28 the
    // ambient (+ WoW point lights at the camera), 32 the sun, RGB8 over 0..2; 60 marks them valid.
    let (ambient, sun, toward_sun) = light.at_camera();
    // The high bytes: the direction to the sun in Minecraft's axes, octahedral 8+8 bits.
    let [ox, oy] = oct_encode(toward_sun);
    combat.put(28, (pack_light(ambient) | (ox << 24)).to_le_bytes());
    combat.put(32, (pack_light(sun) | (oy << 24)).to_le_bytes());
    combat.put(60, HAND_LIGHT_VALID.to_le_bytes());
    // Since v2 the old hit fields (48, 52) carry the level bar.
    combat.put(48, units.self_xp.to_le_bytes());
    combat.put(52, units.self_next_xp.to_le_bytes());
    for (i, u) in list.iter().enumerate() {
        let o = HEADER + i * ACTOR_BYTES;
        let mut flags = 0;
        // A ghost attacks nothing (ghost mode).
        if u.attackable && !units.self_dead && !units.self_ghost {
            flags |= ACTOR_ATTACKABLE;
        }
        if u.dead {
            flags |= ACTOR_DEAD;
        }
        if u.target != 0 && u.target == report.guid {
            flags |= ACTOR_TARGETS_ME;
        }
        combat.put(o, u.guid.to_le_bytes());
        combat.put(o + 8, u.wow_pos[0].to_le_bytes());
        combat.put(o + 12, u.wow_pos[1].to_le_bytes());
        combat.put(o + 16, u.wow_pos[2].to_le_bytes());
        combat.put(o + 20, u.facing.to_le_bytes());
        combat.put(o + 24, u.entry.to_le_bytes());
        combat.put(o + 28, u.display_id.to_le_bytes());
        combat.put(o + 32, u.scale.to_le_bytes());
        combat.put(o + 36, u.bounding_radius.to_le_bytes());
        combat.put(o + 40, u.combat_reach.to_le_bytes());
        combat.put(o + 44, u.health.to_le_bytes());
        combat.put(o + 48, u.max_health.to_le_bytes());
        combat.put(o + 52, u.level.to_le_bytes());
        combat.put(o + 56, u.target.to_le_bytes());
        combat.put(o + 64, flags.to_le_bytes());
        combat.put(o + 68, u.unit_flags.to_le_bytes());
        combat.put(o + 72, u.model_height.to_le_bytes());
        combat.put(o + 76, u.model_width.to_le_bytes());
    }
    fence(Ordering::Release);
    combat.seq(seq.wrapping_add(1));
}

/// Header slot 60's marker: the hand light fields (28, 32) are set.
const HAND_LIGHT_VALID: u32 = 0x4C49_4748; // "LIGH"

/// WoW's light where the camera is, for Minecraft's first-person hand and held item, which
/// Minecraft draws itself (the overlay) and lit by its own lightmap (2026-10-02, user's choice).
#[derive(bevy::ecs::system::SystemParam)]
struct HandLight<'w, 's> {
    lighting: Option<Res<'w, benilla_world::lighting::WowLighting>>,
    camera: Query<'w, 's, &'static GlobalTransform, With<benilla_world::view::WorldCamera>>,
    points: Query<
        'w,
        's,
        (&'static benilla_world::lighting::WorldPointLight, &'static GlobalTransform),
        Without<benilla_world::lighting::ExternalLight>,
    >,
}

impl HandLight<'_, '_> {
    /// `(ambient, sun)`: WoW's ambient plus its nearest three point lights (`wow_model.wgsl`'s
    /// falloff, half facing), and its sun colour. Minecraft's torches light the hand themselves.
    fn at_camera(&self) -> ([f32; 3], [f32; 3], Vec3) {
        let Some(l) = self.lighting.as_deref() else {
            return ([1.0; 3], [0.0; 3], Vec3::Y);
        };
        let mut ambient = l.ambient;
        if let Some(cam) = self.camera.iter().next() {
            let eye = cam.translation();
            let mut near: Vec<(f32, [f32; 3])> = self
                .points
                .iter()
                .filter_map(|(p, gt)| {
                    let d = gt.translation().distance(eye);
                    (d <= p.range).then(|| {
                        let s = p.intensity / (4.0 * std::f32::consts::PI);
                        (d, [p.color[0] * s, p.color[1] * s, p.color[2] * s])
                    })
                })
                .collect();
            near.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (d, rgb) in near.into_iter().take(3) {
                let atten = 0.5 / (0.7 * d + 0.03 * d * d).max(0.25);
                for k in 0..3 {
                    ambient[k] += rgb[k] * atten;
                }
            }
        }
        // `sun_dir` is where the light travels (Bevy); Minecraft = (-x, y, -z).
        let toward = -l.sun_dir;
        (ambient, l.diffuse, Vec3::new(-toward.x, toward.y, -toward.z).normalize_or(Vec3::Y))
    }
}

/// A unit vector as two bytes (octahedral, y up): `LightmapMixin` decodes it.
fn oct_encode(n: Vec3) -> [u32; 2] {
    let s = n.x.abs() + n.y.abs() + n.z.abs();
    let (mut px, mut pz) = (n.x / s, n.z / s);
    if n.y < 0.0 {
        let (ax, az) = (px.abs(), pz.abs());
        px = (1.0 - az) * px.signum();
        pz = (1.0 - ax) * pz.signum();
    }
    let q = |v: f32| ((v * 0.5 + 0.5).clamp(0.0, 1.0) * 255.0).round() as u32;
    [q(px), q(pz)]
}

/// RGB 0..2 as RGB8 (`0x00BBGGRR`).
fn pack_light(c: [f32; 3]) -> u32 {
    let q = |v: f32| ((v / 2.0).clamp(0.0, 1.0) * 255.0).round() as u32;
    q(c[0]) | (q(c[1]) << 8) | (q(c[2]) << 16)
}

/// The server's hits on what Minecraft owns, into the ring the mod drains.
fn relay_damage(mut packets: MessageReader<CustomPacketIn>, mut combat: ResMut<Combat>) {
    for p in packets.read() {
        let xp_drop = p.opcode == SMSG_CC_XP_DROP && p.body.len() >= 36;
        let kill = p.opcode == SMSG_CC_KILL && p.body.len() >= 52;
        let harvest = p.opcode == SMSG_CC_HARVEST && p.body.len() >= 26;
        if !xp_drop && !kill && !harvest && (p.opcode != SMSG_CC_DAMAGE || p.body.len() < 29) {
            continue;
        }
        let b = &p.body;
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let Some(map) = combat.map.as_mut() else {
            continue;
        };
        // SAFETY: the ring head, 8-aligned in our mapping.
        let head = unsafe { std::ptr::read_volatile(map.as_ptr().add(RING_HEAD).cast::<u64>()) };
        let o = RING_DATA + (head % RING_ENTRIES) as usize * RING_ENTRY;
        map[o..o + RING_ENTRY].fill(0);
        if harvest {
            info!(
                "classiccraft: vein {} harvested ({}) to Minecraft",
                u32_at(8),
                if b[24] != 0 { "ok" } else { "refused" }
            );
            // Entry: kind, u8 ok, u8 used up, u8 0, u32 entry, u64 vein, f32 x, y, z (WoW yards).
            map[o] = RING_HARVEST;
            map[o + 1] = b[24];
            map[o + 2] = b[25];
            map[o + 4..o + 8].copy_from_slice(&b[8..12]);
            map[o + 8..o + 16].copy_from_slice(&b[0..8]);
            map[o + 16..o + 28].copy_from_slice(&b[12..24]);
        } else if kill {
            info!(
                "classiccraft: kill of entry {} (level {}, {} copper) to Minecraft",
                u32_at(8),
                u32_at(24),
                u32_at(40)
            );
            // Entry: kind, u8 rank, u8 level, u8 creature type, u32 entry, u32 money, f32 x, y, z
            // (WoW yards), u8 family, u8 flags, u8 quest items bagged.
            map[o] = RING_KILL;
            map[o + 1] = u32_at(28).min(255) as u8;
            map[o + 2] = u32_at(24).min(255) as u8;
            map[o + 3] = u32_at(32).min(255) as u8;
            map[o + 4..o + 8].copy_from_slice(&b[8..12]);
            map[o + 8..o + 12].copy_from_slice(&b[40..44]);
            map[o + 12..o + 24].copy_from_slice(&b[12..24]);
            map[o + 24] = u32_at(36).min(255) as u8;
            map[o + 25] = u32_at(44).min(255) as u8;
            map[o + 26] = u32_at(48).min(255) as u8;
        } else if xp_drop {
            info!(
                "classiccraft: kill XP drop {} ({} XP) to Minecraft",
                u32_at(0),
                u32_at(24)
            );
            // Body: u32 drop id, guid victim, f32 x, y, z, u32 XP, u32 level, u32 rank. Entry: kind,
            // u8 rank, u16 level, u32 drop id, u32 XP, f32 x, y, z (WoW yards).
            map[o] = RING_XP_DROP;
            map[o + 1] = u32_at(32).min(255) as u8;
            map[o + 2..o + 4].copy_from_slice(&(u32_at(28).min(u32::from(u16::MAX)) as u16).to_le_bytes());
            map[o + 4..o + 8].copy_from_slice(&b[0..4]);
            map[o + 8..o + 12].copy_from_slice(&b[24..28]);
            map[o + 12..o + 24].copy_from_slice(&b[12..24]);
        } else {
            map[o] = b[0]; // victim kind
            map[o + 4..o + 8].copy_from_slice(&b[1..5]); // mc id
            map[o + 8..o + 16].copy_from_slice(&b[5..13]); // attacker guid
            map[o + 16..o + 20].copy_from_slice(&u32_at(13).to_le_bytes()); // WoW damage
            map[o + 20..o + 24].copy_from_slice(&u32_at(17).to_le_bytes()); // attacker level
            map[o + 24..o + 28].copy_from_slice(&u32_at(21).to_le_bytes()); // flags
            map[o + 28..o + 32].copy_from_slice(&u32_at(25).to_le_bytes()); // school
        }
        fence(Ordering::Release);
        // SAFETY: as above.
        unsafe {
            std::ptr::write_volatile(map.as_mut_ptr().add(RING_HEAD).cast::<u64>(), head + 1)
        };
    }
}

/// What a right-click in Minecraft would act on, for its hint and its click (the focus block).
fn write_focus(
    target: Res<benilla_app::external::CrosshairTarget>,
    report: Res<SelfReport>,
    mut combat: ResMut<Combat>,
) {
    let Some(map) = combat.map.as_mut() else {
        return;
    };
    // Our stand state: a single word, no seqlock needed.
    map[SELF..SELF + 4].copy_from_slice(&u32::from(report.stand_state).to_le_bytes());
    let seq = u32::from_le_bytes(map[FOCUS..FOCUS + 4].try_into().unwrap());
    map[FOCUS..FOCUS + 4].copy_from_slice(&(seq.wrapping_add(1) | 1).to_le_bytes());
    fence(Ordering::Release);
    let mut n = target.name.len().min(FOCUS_NAME_MAX);
    while !target.name.is_char_boundary(n) {
        n -= 1;
    }
    map[FOCUS + 4..FOCUS + 8].copy_from_slice(&u32::from(target.kind).to_le_bytes());
    map[FOCUS + 8..FOCUS + 12].copy_from_slice(&(target.distance / S).to_le_bytes());
    map[FOCUS + 12..FOCUS + 16].copy_from_slice(&u32::from(target.unable).to_le_bytes());
    map[FOCUS + 16..FOCUS + 24].copy_from_slice(&target.guid.to_le_bytes());
    map[FOCUS + 24..FOCUS + 28].copy_from_slice(&(n as u32).to_le_bytes());
    map[FOCUS + 28..FOCUS + 28 + n].copy_from_slice(&target.name.as_bytes()[..n]);
    fence(Ordering::Release);
    map[FOCUS..FOCUS + 4].copy_from_slice(&(seq.wrapping_add(1) | 1).wrapping_add(1).to_le_bytes());
}

/// The server's system lines (GM command replies) into the text ring, for Minecraft's chat.
fn relay_chat(mut lines: MessageReader<ChatIn>, mut combat: ResMut<Combat>) {
    for line in lines.read() {
        let Some(map) = combat.map.as_mut() else {
            continue;
        };
        // Cut at a char boundary to fit the slot.
        let mut n = line.text.len().min(TEXT_SLOT - 2);
        while !line.text.is_char_boundary(n) {
            n -= 1;
        }
        // SAFETY: the text ring head, 8-aligned in our mapping.
        let head = unsafe { std::ptr::read_volatile(map.as_ptr().add(TEXT_HEAD).cast::<u64>()) };
        let o = TEXT_DATA + (head % TEXT_SLOTS) as usize * TEXT_SLOT;
        map[o..o + 2].copy_from_slice(&(n as u16).to_le_bytes());
        map[o + 2..o + 2 + n].copy_from_slice(&line.text.as_bytes()[..n]);
        fence(Ordering::Release);
        // SAFETY: as above.
        unsafe { std::ptr::write_volatile(map.as_mut_ptr().add(TEXT_HEAD).cast::<u64>(), head + 1) };
    }
}

/// Minecraft's hits, deaths and mobs, to the server.
fn handle_inbox(
    bridge: Res<Bridge>,
    report: Res<SelfReport>,
    units: Res<NearbyUnits>,
    mut inbox: ResMut<McInbox>,
    mut combat: ResMut<Combat>,
    mut out: MessageWriter<CustomPacketOut>,
    mut chat: MessageWriter<ChatOut>,
    mut crosshair_use: ResMut<benilla_app::external::CrosshairUse>,
    mut dialog: MessageWriter<benilla_app::external_dialog::DialogIn>,
) {
    let mut msgs = std::mem::take(&mut inbox.0);
    if !(bridge.driving() && report.in_world) {
        // A respawn must reach the server (the character stays dead otherwise): kept until in world.
        msgs.retain(|m| matches!(m, McMsg::Respawn(_)));
        if !report.in_world {
            inbox.0 = msgs;
            return;
        }
    }
    let ghost = units.self_dead || units.self_ghost;
    for m in msgs {
        // Ghost mode: no hits and no proxies, only the death/respawn events get through.
        // A ghost talks to the spirit healer too.
        if ghost && !matches!(m, McMsg::Event(_) | McMsg::Respawn(_) | McMsg::Chat(_) | McMsg::Interact | McMsg::Dialog(_)) {
            continue;
        }
        match m {
            McMsg::Interact => crosshair_use.pending = true,
            McMsg::Dialog(d) => {
                dialog.write(d);
            }
            McMsg::Harvest(body) => {
                info!("classiccraft: ore vein mined in Minecraft, to the server");
                send(&mut out, CMSG_CC_HARVEST, body);
            }
            McMsg::Waygate(body) => {
                info!("classiccraft: waygate travel to the server");
                send(&mut out, CMSG_CC_WAYGATE, body);
            }
            McMsg::Chat(text) => {
                info!("classiccraft: chat from Minecraft: {text}");
                chat.write(ChatOut { text });
            }
            McMsg::Hit {
                guid,
                wow_damage,
                flags,
                attacker,
            } => {
                let mut cc_flags = 0;
                if flags & MC_HIT_CRITICAL != 0 {
                    cc_flags |= CC_HIT_CRIT;
                }
                if flags & MC_HIT_PROJECTILE != 0 {
                    cc_flags |= CC_HIT_PROJECTILE;
                }
                if flags & MC_HIT_PERIODIC != 0 {
                    cc_flags |= CC_HIT_PERIODIC;
                }
                if flags & MC_HIT_SLOW != 0 {
                    cc_flags |= CC_HIT_SLOW;
                }
                cc_flags |= flags & HIT_SCHOOL_BITS;
                let mut body = Vec::with_capacity(21);
                body.push(u8::from(attacker != 0));
                body.extend_from_slice(&attacker.to_le_bytes());
                body.extend_from_slice(&guid.to_le_bytes());
                body.extend_from_slice(&wow_damage.to_le_bytes());
                body.extend_from_slice(&cc_flags.to_le_bytes());
                send(&mut out, CMSG_CC_HIT, body);
            }
            McMsg::Event(1) => {
                info!("classiccraft: Steve died: the WoW character dies too");
                send(&mut out, CMSG_CC_DIED, Vec::new());
            }
            McMsg::Respawn(body) => {
                info!(
                    "classiccraft: Steve respawned: the WoW character comes back {}",
                    if body[0] == 1 { "at his bed" } else { "at its hearthstone location" }
                );
                send(&mut out, CMSG_CC_RESPAWN, body);
            }
            McMsg::Event(_) => {}
            McMsg::XpClaim { drop, xp } => {
                let mut body = drop.to_le_bytes().to_vec();
                body.extend_from_slice(&xp.to_le_bytes());
                send(&mut out, CMSG_CC_XP_CLAIM, body);
            }
            McMsg::Mobs(mobs) => {
                let now: HashSet<u32> = mobs.iter().map(|m| m.id).collect();
                let mut body = Vec::new();
                let mut count: u16 = 0;
                let entry =
                    |body: &mut Vec<u8>, id: u32, kind: u8, pos: [f32; 3], o: f32, hp: u8| {
                        body.extend_from_slice(&id.to_le_bytes());
                        body.push(kind);
                        for v in pos {
                            body.extend_from_slice(&v.to_le_bytes());
                        }
                        body.extend_from_slice(&o.to_le_bytes());
                        body.push(hp);
                    };
                for m in &mobs {
                    // Fixed mapping: WoW x = mcZ·S, y = mcX·S, z = mcY·S; facing = -yaw.
                    let wow = [m.pos[2] * S, m.pos[0] * S, m.pos[1] * S];
                    let o = (-m.yaw_deg.to_radians()).rem_euclid(std::f32::consts::TAU);
                    entry(&mut body, m.id, m.kind, wow, o, m.hp_pct);
                    count += 1;
                }
                for gone in combat.proxied.difference(&now) {
                    entry(&mut body, *gone, 0, [0.0; 3], 0.0, 0);
                    count += 1;
                }
                combat.proxied = now;
                if count > 0 {
                    let mut packet = count.to_le_bytes().to_vec();
                    packet.extend_from_slice(&body);
                    send(&mut out, CMSG_CC_ACTORS, packet);
                }
            }
        }
    }
}
