//! Transport decks for Minecraft (2026-10-02, the Deeprun Tram first): a boat, zeppelin, tram car
//! or lift is a moving platform Steve walks on. A transport's walkable collision is no part of the
//! static cells ([`crate::geom`], where a frozen copy stood invisible at its spawn spot); it goes
//! once as `MSG_DECK` in the deck's own frame, and its pose every frame in the deck table
//! (`protocol/mcwow_geom_protocol.h`, `MCWOW_GEOM_OFF_DECKS`). The mod moves those triangles and
//! carries Steve with them; while he stands on one it reports his pose relative to the deck (the
//! rider block), which [`compose_rider`] turns back into a world pose through benilla's live deck
//! pose, so the camera never trails a deck that moved since Minecraft's last tick.

use std::collections::HashMap;
use std::sync::atomic::{fence, Ordering};

use avian3d::prelude::{Collider, ColliderOf, CollisionLayers};
use benilla_app::external::{DeckTransport, ExternalDrive, ExternalDriveSet, SelfReport};
use benilla_world::collision::{ColliderEpoch, WorldCollision};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use crate::bridge::{mc_to_bevy, S};
use crate::geom::{Geom, DECK_SLOTS, OFF_DECKS, OFF_RIDER};

const MSG_DECK: u32 = 5;
const MSG_DECK_GONE: u32 = 6;
const KIND_MODEL: u32 = 0;
const TRI_WALKABLE: u32 = 1;
/// Walkable iff the normal is within 50° of up, as [`crate::geom`].
const GROUND_COS: f32 = 0.642_788;
/// Decks within this of the player (yd) are exported; a boat is ~60 yd long.
const RANGE: f32 = 250.0;

pub struct DecksPlugin;

/// How far [`compose_rider`] moved Steve's feet off Minecraft's world pose this frame (Bevy yd):
/// what is drawn at Minecraft's world position for him (his avatar) takes it too.
#[derive(Resource, Default)]
pub(crate) struct RiderShift(pub Vec3);

impl Plugin for DecksPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Decks>()
            .init_resource::<RiderShift>()
            .add_systems(Update, export_decks.in_set(ExternalDriveSet::Report))
            .add_systems(
                Update,
                compose_rider
                    .in_set(ExternalDriveSet::Supply)
                    .after(crate::bridge::supply_pose),
            );
    }
}

/// Which collider faces belong to a transport, for the cell export to leave out.
#[derive(SystemParam)]
pub(crate) struct OnDeck<'w, 's> {
    bodies: Query<'w, 's, &'static ColliderOf>,
    decks: Query<'w, 's, (), With<DeckTransport>>,
}

impl OnDeck<'_, '_> {
    pub(crate) fn is_deck(&self, collider: Entity) -> bool {
        self.decks.contains(collider)
            || self
                .bodies
                .get(collider)
                .is_ok_and(|of| self.decks.contains(of.body))
    }
}

#[derive(Resource, Default)]
struct Decks {
    /// The geometry epoch the sent set belongs to: a `MSG_CLEAR` drops every deck on the mod's side.
    epoch: u32,
    /// Guid -> (triangle count, hash) last sent.
    sent: HashMap<u64, (usize, u64)>,
    /// The collider set the sent decks were read from: a deck is read again only when it changes.
    colliders_at: u64,
    logged: usize,
}

/// A deck's origin (MC blocks, region-local) and yaw about +Y, from its transform. Bevy -> MC is a
/// half turn about Y and a scale, so a yaw about Y is the same angle in both.
fn mc_pose(t: &Transform) -> ([f32; 3], f32) {
    let p = t.translation;
    (
        [-p.x / S, p.y / S, -p.z / S],
        t.rotation.to_euler(EulerRot::YXZ).0,
    )
}

#[allow(clippy::type_complexity)]
fn export_decks(
    report: Res<SelfReport>,
    collider_epoch: Res<ColliderEpoch>,
    mut geom: ResMut<Geom>,
    mut state: ResMut<Decks>,
    decks: Query<(Entity, &DeckTransport, &Transform)>,
    colliders: Query<(Entity, &Collider, &ColliderOf, Option<&CollisionLayers>)>,
    hierarchy: Query<(&Transform, Option<&ChildOf>)>,
) {
    if !geom.is_open() {
        return;
    }
    if state.epoch != geom.epoch {
        state.epoch = geom.epoch;
        state.sent.clear();
    }
    let [wx, wy, wz] = report.wow_pos;
    let here = Vec3::new(-wy, wz, -wx);
    let near: Vec<(Entity, u64, &Transform)> = if report.in_world {
        decks
            .iter()
            .filter(|(_, _, t)| t.translation.distance(here) < RANGE)
            .map(|(e, d, t)| (e, d.guid, t))
            .collect()
    } else {
        Vec::new()
    };
    // Each near deck's walkable collision, in its own frame: for decks not sent yet, and for all
    // when the collider set changed (a deck's colliders may attach a frame after it arms).
    let colliders_changed = state.colliders_at != collider_epoch.get();
    state.colliders_at = collider_epoch.get();
    let filter = WorldCollision::body_filter();
    let mut tris: HashMap<Entity, Vec<u8>> = near
        .iter()
        .filter(|(_, g, _)| colliders_changed || !state.sent.contains_key(g))
        .map(|(e, ..)| (*e, Vec::new()))
        .collect();
    let mut counts: HashMap<Entity, u32> = HashMap::new();
    let reading = !tris.is_empty();
    for (entity, collider, of, layers) in colliders.iter().filter(|_| reading) {
        let Some(buf) = tris.get_mut(&of.body) else {
            continue;
        };
        if !filter.test(entity, layers.copied().unwrap_or_default()) {
            continue; // a camera-only face
        }
        let Some(mesh) = collider.shape().as_trimesh() else {
            continue;
        };
        // The collider in the deck's frame through the model's own transforms, which never change.
        // Not avian's `ColliderTransform`: it is refreshed in the physics step against a deck the
        // transport tick already moved, so it drifted by a frame's travel (each car was re-sent
        // every frame, 2026-10-02). The deck's own scale stays in, its pose does not.
        let Some(to_deck) = deck_frame(entity, of.body, &hierarchy) else {
            continue;
        };
        for i in 0..mesh.num_triangles() {
            let tri = mesh.triangle(i as u32);
            let v = [tri.a, tri.b, tri.c].map(|p| to_deck.transform_point3(p));
            let n = (v[1] - v[0]).cross(v[2] - v[0]).normalize_or_zero();
            let flags = if n.y > GROUND_COS { TRI_WALKABLE } else { 0 };
            for p in v {
                for comp in [-p.x / S, p.y / S, -p.z / S] {
                    buf.extend_from_slice(&comp.to_le_bytes());
                }
            }
            buf.extend_from_slice(&flags.to_le_bytes());
            *counts.entry(of.body).or_default() += 1;
        }
    }
    // Send new or changed decks (its colliders may attach a frame after it arms).
    for (entity, guid, _) in &near {
        let Some(body) = tris.remove(entity) else {
            continue; // sent, and its colliders unchanged
        };
        let count = counts.get(entity).copied().unwrap_or(0);
        if count == 0 {
            continue;
        }
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            body.hash(&mut h);
            h.finish()
        };
        if state.sent.get(guid).is_some_and(|&(_, h)| h == hash) {
            continue;
        }
        let mut payload = Vec::with_capacity(16 + body.len());
        payload.extend_from_slice(&guid.to_le_bytes());
        payload.extend_from_slice(&count.to_le_bytes());
        payload.extend_from_slice(&KIND_MODEL.to_le_bytes());
        payload.extend_from_slice(&body);
        if geom.send(MSG_DECK, &payload) {
            // Its bounds in its own frame (MC blocks), to read the geometry against the model.
            let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
            for tri in body.chunks_exact(40) {
                for v in 0..9 {
                    let x = f32::from_le_bytes(tri[v * 4..v * 4 + 4].try_into().unwrap());
                    lo[v % 3] = lo[v % 3].min(x);
                    hi[v % 3] = hi[v % 3].max(x);
                }
            }
            info!(
                "classiccraft: deck {guid:#x} sent ({count} triangles, local {:.1?}..{:.1?})",
                lo, hi
            );
            state.sent.insert(*guid, (count as usize, hash));
        }
    }
    // Decks gone or out of range.
    let keep: Vec<u64> = near.iter().map(|(_, g, _)| *g).collect();
    let gone: Vec<u64> = state.sent.keys().copied().filter(|g| !keep.contains(g)).collect();
    for guid in gone {
        if geom.send(MSG_DECK_GONE, &guid.to_le_bytes()) {
            state.sent.remove(&guid);
        }
    }
    if state.sent.len() != state.logged {
        state.logged = state.sent.len();
        debug!("classiccraft: decks current: {}", state.logged);
    }
    // The pose table, seqlocked.
    let rows: Vec<(u64, [f32; 3], f32)> = near
        .iter()
        .filter(|(_, g, _)| state.sent.contains_key(g))
        .take(DECK_SLOTS)
        .map(|(_, g, t)| {
            let (o, yaw) = mc_pose(t);
            (*g, o, yaw)
        })
        .collect();
    let Some(area) = geom.decks_area() else {
        return;
    };
    let seq = u32::from_le_bytes(area[0..4].try_into().unwrap()).wrapping_add(1) | 1;
    area[0..4].copy_from_slice(&seq.to_le_bytes());
    fence(Ordering::Release);
    area[4..8].copy_from_slice(&(rows.len() as u32).to_le_bytes());
    for (i, (guid, o, yaw)) in rows.iter().enumerate() {
        let at = 16 + i * 32;
        area[at..at + 8].copy_from_slice(&guid.to_le_bytes());
        for (k, v) in [o[0], o[1], o[2], *yaw].iter().enumerate() {
            area[at + 8 + k * 4..at + 12 + k * 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    fence(Ordering::Release);
    area[0..4].copy_from_slice(&seq.wrapping_add(1).to_le_bytes());
}

/// `collider`'s frame in `deck`'s (scale kept, pose dropped), up the parent chain; `None` when the
/// chain does not reach it.
fn deck_frame(
    collider: Entity,
    deck: Entity,
    hierarchy: &Query<(&Transform, Option<&ChildOf>)>,
) -> Option<bevy::math::Affine3A> {
    let mut at = collider;
    let mut chain = bevy::math::Affine3A::IDENTITY;
    for _ in 0..8 {
        let (t, parent) = hierarchy.get(at).ok()?;
        if at == deck {
            return Some(bevy::math::Affine3A::from_scale(t.scale) * chain);
        }
        chain = t.compute_affine() * chain;
        at = parent?.parent();
    }
    None
}

/// While Steve stands on a deck, his pose from the rider block composed with the deck's live pose.
fn compose_rider(
    mut shift: ResMut<RiderShift>,
    mut geom: ResMut<Geom>,
    mut drive: ResMut<ExternalDrive>,
    decks: Query<(Entity, &DeckTransport, &Transform)>,
) {
    shift.0 = Vec3::ZERO;
    let Some(pose) = drive.pose.as_mut() else {
        return;
    };
    let Some(area) = geom.decks_area() else {
        return;
    };
    let r = OFF_RIDER - OFF_DECKS;
    let f = |o: usize| f32::from_le_bytes(area[r + o..r + o + 4].try_into().unwrap());
    let seq = u32::from_le_bytes(area[r..r + 4].try_into().unwrap());
    if seq & 1 != 0 {
        return; // mid-write: this frame keeps the world pose
    }
    fence(Ordering::Acquire);
    let guid = u64::from_le_bytes(area[r + 8..r + 16].try_into().unwrap());
    let eye = [f(16), f(20), f(24)];
    let feet = [f(28), f(32), f(36)];
    let yaw_deg = f(40);
    let has_pose = u32::from_le_bytes(area[r + 44..r + 48].try_into().unwrap()) & 1 != 0;
    fence(Ordering::Acquire);
    if u32::from_le_bytes(area[r..r + 4].try_into().unwrap()) != seq {
        return;
    }
    pose.deck_known = true;
    if guid == 0 {
        return; // not aboard
    }
    let Some((deck, _, t)) = decks.iter().find(|(_, d, _)| d.guid == guid) else {
        pose.deck_known = false; // a deck we no longer hold: no word either way
        return;
    };
    pose.deck = Some(deck);
    if !has_pose {
        return; // aboard, the deck out of the driver's sight: keep the world pose
    }
    let (o, yaw) = mc_pose(t);
    let (s, c) = yaw.sin_cos();
    let world = |l: [f32; 3]| [o[0] + l[0] * c + l[2] * s, o[1] + l[1], o[2] - l[0] * s + l[2] * c];
    let was = pose.feet;
    pose.feet = mc_to_bevy(world(feet));
    shift.0 = pose.feet - was;
    pose.eye = mc_to_bevy(world(eye));
    // As `bridge::pose_of`: Minecraft yaw clockwise, WoW's counter-clockwise.
    let yaw = -(yaw_deg - yaw.to_degrees()).to_radians();
    pose.face_yaw = yaw.rem_euclid(std::f32::consts::TAU);
    pose.eye_yaw = yaw;
}
