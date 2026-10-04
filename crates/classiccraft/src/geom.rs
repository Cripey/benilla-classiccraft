//! The collision export, in place of mcwow's `geom_server`: benilla's own loaded colliders (terrain,
//! WMO walk faces, doodads, GameObjects; the body's filter) cut into fixed 32 yd cells and streamed
//! to Minecraft over `protocol/mcwow_geom_protocol.h` v2, whose `McwowTriCollider` collides with
//! them. Each triangle goes to the one cell its centroid lies in; a cell is (re)sent when it comes
//! into range, when the collider set changes ([`ColliderEpoch`]) and its triangles differ, and after
//! the mod evicts it or asks for a refresh.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{fence, Ordering};

use benilla_app::external::{ExternalDriveSet, SelfReport};
use benilla_world::collision::{ColliderEpoch, WorldCollision};
use bevy::prelude::*;
use memmap2::MmapMut;

use crate::bridge::S;
use crate::holes::HoleMasks;
use benilla_assets::AdtTile;
use benilla_world::terrain_stream::{terrain_height_under_cached, TerrainStreamer};

const PATH: &str = "/dev/shm/classiccraft_geom_v1.shm";
const MAGIC: u32 = 0x6D63_6731; // 'mcg1'
const VERSION: u32 = 3;
const OFF_WRITER_PID: usize = 8;
const OFF_WRITER_HEARTBEAT: usize = 16;
const OFF_REFRESH_REQUEST: usize = 24;
const OFF_RING: usize = 64;
const RING_BYTES: usize = 16 * 1024 * 1024;
const RING_HEAD: usize = OFF_RING;
const RING_TAIL: usize = OFF_RING + 8;
const RING_DATA: usize = OFF_RING + 64;
const RING_DATA_BYTES: u64 = (RING_BYTES - 64) as u64;
const OFF_EVICT: usize = OFF_RING + RING_BYTES;
const EVICT_SLOTS: u64 = 4096;
/// v3: the transport deck table and the rider block ([`crate::decks`]).
pub(crate) const OFF_DECKS: usize = OFF_EVICT + 16 + EVICT_SLOTS as usize * 8;
pub(crate) const DECK_SLOTS: usize = 16;
pub(crate) const OFF_RIDER: usize = OFF_DECKS + 16 + DECK_SLOTS * 32;
const TOTAL: usize = OFF_RIDER + 64;

const MSG_PAD: u32 = 0;
const MSG_CLEAR: u32 = 1;
const MSG_BATCH: u32 = 2;
/// A book or plaque read from Minecraft's crosshair, for its book screen: u32 title length, title,
/// u32 pages, then per page u32 length + UTF-8 (`external::BookOut`).
const MSG_BOOK: u32 = 7;
/// An NPC window for Minecraft's screen (`external_dialog::DialogOut`, 2026-10-03): u64 npc, u32
/// kind, u32 quest id, strings (u32 length + UTF-8) npc name, title, text, objectives; u32 options
/// then (u32 icon, string label) each; three item lists (choices, rewards, required), each u32 n
/// then per item u32 id, count, quality, class, subclass, inventory type, item level, required
/// level, string name; u32 money, u32 completable, u32 XP.
const MSG_DIALOG: u32 = 8;
/// WoW's quest log for Minecraft's (`external_dialog::QuestLogOut`, 2026-10-03): u32 quests, per
/// quest u32 id, level, i32 complete, string title, u32 objectives then (u32 finished, string text)
/// each, strings description, objectives text, u32 reward money; u32 items, per item u32 bag, slot,
/// id, count, usable, string name.
const MSG_QUESTLOG: u32 = 9;
const KIND_MODEL: u32 = 0;
const TRI_WALKABLE: u32 = 1;

const CELL_YD: f32 = 32.0;
const CELL_ID_BIT: u64 = 1 << 62;
/// Cells kept around the player: a (2R+1)² square, 160 yd across at R = 2.
const RADIUS: i32 = 2;
/// Cells (re)computed per frame, so a burst of streaming never stalls one.
const BUDGET: usize = 4;
/// Walkable iff the normal is within 50° of up (`player::state::GROUND_COS`).
const GROUND_COS: f32 = 0.642_788;
/// The cell query's vertical reach either side of the feet (yd): the whole column in practice.
const COLUMN_HALF: f32 = 2000.0;

pub struct GeomPlugin;

impl Plugin for GeomPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Geom>()
            .add_systems(Update, stream_cells.in_set(ExternalDriveSet::Report))
            .add_systems(Update, (send_books, send_dialogs, send_quest_log).after(stream_cells));
    }
}

#[derive(Resource, Default)]
pub(crate) struct Geom {
    map: Option<MmapMut>,
    retry_at: f32,
    /// Bumped by every `MSG_CLEAR`: the mod has dropped everything sent before it.
    pub(crate) epoch: u32,
    /// Cell id -> (collider epoch it was computed at, triangle hash).
    sent: HashMap<u64, (u64, u64)>,
    map_id: Option<u32>,
    refresh_seen: u32,
    evict_tail: u64,
    logged_cells: usize,
}

fn cell_id(cx: i32, cy: i32) -> u64 {
    CELL_ID_BIT | (((cx + 32768) as u16 as u64) << 16) | ((cy + 32768) as u16 as u64)
}

impl Geom {
    fn open(&mut self, now: f32) {
        if self.map.is_some() || now < self.retry_at {
            return;
        }
        self.retry_at = now + 5.0;
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PATH)
        {
            Ok(f) => f,
            Err(e) => return warn!("classiccraft: cannot open {PATH}: {e}"),
        };
        if file.set_len(TOTAL as u64).is_err() {
            return;
        }
        // SAFETY: our file; the layout below is the protocol's, the mod reads it concurrently.
        let Ok(mut map) = (unsafe { MmapMut::map_mut(&file) }) else {
            return;
        };
        map[..TOTAL].fill(0);
        map[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        map[4..8].copy_from_slice(&VERSION.to_le_bytes());
        map[OFF_WRITER_PID..OFF_WRITER_PID + 4].copy_from_slice(&std::process::id().to_le_bytes());
        info!("classiccraft: geometry export {PATH} ready");
        self.map = Some(map);
        self.sent.clear();
        self.evict_tail = 0;
    }

    fn u32(&self, off: usize) -> u32 {
        let map = self.map.as_ref().unwrap();
        // SAFETY: in bounds; volatile, the mod writes it.
        unsafe { std::ptr::read_volatile(map.as_ptr().add(off).cast::<u32>()) }
    }
    fn u64(&self, off: usize) -> u64 {
        let map = self.map.as_ref().unwrap();
        // SAFETY: in bounds and 8-aligned in the layout.
        unsafe { std::ptr::read_volatile(map.as_ptr().add(off).cast::<u64>()) }
    }
    fn put_u64(&mut self, off: usize, v: u64) {
        let map = self.map.as_mut().unwrap();
        // SAFETY: as `u64`.
        unsafe { std::ptr::write_volatile(map.as_mut_ptr().add(off).cast::<u64>(), v) }
    }

    /// Whether the ring is open.
    pub(crate) fn is_open(&self) -> bool {
        self.map.is_some()
    }

    /// The deck table and rider block ([`OFF_DECKS`]..), for [`crate::decks`].
    pub(crate) fn decks_area(&mut self) -> Option<&mut [u8]> {
        self.map.as_mut().map(|m| &mut m[OFF_DECKS..TOTAL])
    }

    /// Append one message; `false` (nothing written) if the reader has not freed the room yet.
    pub(crate) fn send(&mut self, kind: u32, payload: &[u8]) -> bool {
        let size = (8 + payload.len() as u64).next_multiple_of(8);
        let mut head = self.u64(RING_HEAD);
        let tail = self.u64(RING_TAIL);
        let pos = head % RING_DATA_BYTES;
        let pad = if pos + size > RING_DATA_BYTES {
            RING_DATA_BYTES - pos
        } else {
            0
        };
        if head + pad + size - tail > RING_DATA_BYTES {
            return false;
        }
        let map = self.map.as_mut().unwrap();
        if pad > 0 {
            let at = RING_DATA + pos as usize;
            map[at..at + 4].copy_from_slice(&MSG_PAD.to_le_bytes());
            head += pad;
        }
        let at = RING_DATA + (head % RING_DATA_BYTES) as usize;
        map[at..at + 4].copy_from_slice(&kind.to_le_bytes());
        map[at + 4..at + 8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        map[at + 8..at + 8 + payload.len()].copy_from_slice(payload);
        fence(Ordering::Release);
        self.put_u64(RING_HEAD, head + size);
        true
    }

    fn clear(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;
        if self.send(MSG_CLEAR, &epoch.to_le_bytes()) {
            self.sent.clear();
        }
    }
}

/// NPC windows, to Minecraft's own screens.
fn send_dialogs(
    mut dialogs: MessageReader<benilla_app::external_dialog::DialogOut>,
    mut geom: ResMut<Geom>,
) {
    fn put(s: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    fn items(list: &[benilla_app::external_dialog::DialogItem], out: &mut Vec<u8>) {
        out.extend_from_slice(&(list.len() as u32).to_le_bytes());
        for it in list {
            for v in [it.item_id, it.count, it.quality, it.class, it.subclass, it.inventory_type,
                it.item_level, it.required_level] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            put(&it.name, out);
        }
    }
    for d in dialogs.read() {
        let mut p = Vec::new();
        p.extend_from_slice(&d.npc.to_le_bytes());
        p.extend_from_slice(&u32::from(d.kind).to_le_bytes());
        p.extend_from_slice(&d.quest_id.to_le_bytes());
        for s in [&d.npc_name, &d.title, &d.text, &d.objectives] {
            put(s, &mut p);
        }
        p.extend_from_slice(&(d.options.len() as u32).to_le_bytes());
        for (icon, label) in &d.options {
            p.extend_from_slice(&icon.to_le_bytes());
            put(label, &mut p);
        }
        items(&d.choices, &mut p);
        items(&d.rewards, &mut p);
        items(&d.required, &mut p);
        p.extend_from_slice(&d.money.to_le_bytes());
        p.extend_from_slice(&u32::from(d.completable).to_le_bytes());
        p.extend_from_slice(&d.xp.to_le_bytes());
        if !geom.is_open() || !geom.send(MSG_DIALOG, &p) {
            warn!("classiccraft: dialog kind {} not sent (geometry ring closed or full)", d.kind);
        }
    }
}

/// WoW's quest log, to Minecraft's quest log screen.
fn send_quest_log(
    mut logs: MessageReader<benilla_app::external_dialog::QuestLogOut>,
    mut geom: ResMut<Geom>,
) {
    fn put(s: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    for log in logs.read() {
        let mut p = Vec::new();
        p.extend_from_slice(&(log.quests.len() as u32).to_le_bytes());
        for q in &log.quests {
            p.extend_from_slice(&q.quest_id.to_le_bytes());
            p.extend_from_slice(&q.level.to_le_bytes());
            p.extend_from_slice(&q.complete.to_le_bytes());
            put(&q.title, &mut p);
            p.extend_from_slice(&(q.objectives.len() as u32).to_le_bytes());
            for (done, text) in &q.objectives {
                p.extend_from_slice(&u32::from(*done).to_le_bytes());
                put(text, &mut p);
            }
            put(&q.description, &mut p);
            put(&q.objectives_text, &mut p);
            p.extend_from_slice(&q.reward_money.to_le_bytes());
        }
        p.extend_from_slice(&(log.items.len() as u32).to_le_bytes());
        for it in &log.items {
            for v in [it.bag, it.slot, it.item_id, it.count, u32::from(it.usable)] {
                p.extend_from_slice(&v.to_le_bytes());
            }
            put(&it.name, &mut p);
        }
        if !geom.is_open() || !geom.send(MSG_QUESTLOG, &p) {
            warn!("classiccraft: quest log not sent (geometry ring closed or full)");
        }
    }
}

/// Books read from the crosshair, to Minecraft's book screen.
fn send_books(
    mut books: MessageReader<benilla_app::external::BookOut>,
    mut geom: ResMut<Geom>,
) {
    for book in books.read() {
        let mut payload = Vec::new();
        let put = |s: &str, out: &mut Vec<u8>| {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        };
        put(&book.title, &mut payload);
        payload.extend_from_slice(&(book.pages.len() as u32).to_le_bytes());
        for page in &book.pages {
            put(page, &mut payload);
        }
        if !geom.is_open() || !geom.send(MSG_BOOK, &payload) {
            warn!("classiccraft: book {:?} not sent (geometry ring closed or full)", book.title);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn stream_cells(
    time: Res<Time>,
    report: Res<SelfReport>,
    collider_epoch: Res<ColliderEpoch>,
    collide: WorldCollision,
    mut geom: ResMut<Geom>,
    mut holes: ResMut<HoleMasks>,
    streamer: Res<TerrainStreamer>,
    tiles: Res<Assets<AdtTile>>,
    on_deck: crate::decks::OnDeck,
) {
    let geom = &mut *geom;
    geom.open(time.elapsed_secs());
    if geom.map.is_none() {
        return;
    }
    // Cells over a chunk whose holes changed are owed again (their terrain is clipped anew).
    for (hx, hz) in std::mem::take(&mut holes.dirty) {
        let (y0, y1) = (hx as f32 * 16.0 * S, (hx + 1) as f32 * 16.0 * S);
        let (x0, x1) = (hz as f32 * 16.0 * S, (hz + 1) as f32 * 16.0 * S);
        for cx in (x0 / CELL_YD).floor() as i32..=(x1 / CELL_YD).floor() as i32 {
            for cy in (y0 / CELL_YD).floor() as i32..=(y1 / CELL_YD).floor() as i32 {
                geom.sent.remove(&cell_id(cx, cy));
            }
        }
    }
    let ground = Ground {
        streamer: &streamer,
        tiles: &tiles,
        holes: &holes,
    };
    let hb = geom.u64(OFF_WRITER_HEARTBEAT).wrapping_add(1);
    geom.put_u64(OFF_WRITER_HEARTBEAT, hb);
    // A cinematic moves the streaming focus - and with it benilla's colliders - to its camera: the
    // cells around the held body went out nearly empty and Steve fell through them, then drove the
    // WoW body down with him (2026-10-04, race intro skipped). The mod keeps what it has until the
    // cinematic is over; the bridge then places Steve afresh.
    if benilla_app::external::cinematic() {
        return;
    }

    // The mod asks for everything again after a resync; a new map starts a new epoch.
    let refresh = geom.u32(OFF_REFRESH_REQUEST);
    if refresh != geom.refresh_seen {
        geom.refresh_seen = refresh;
        geom.clear();
    }
    if report.in_world && report.map != geom.map_id {
        geom.map_id = report.map;
        geom.clear();
    }
    // Cells the mod dropped are owed again when we pass by.
    let evict_head = geom.u64(OFF_EVICT);
    if evict_head.saturating_sub(geom.evict_tail) > EVICT_SLOTS {
        geom.sent.clear();
        geom.evict_tail = evict_head;
    }
    while geom.evict_tail < evict_head {
        let id = geom.u64(OFF_EVICT + 16 + ((geom.evict_tail % EVICT_SLOTS) * 8) as usize);
        geom.sent.remove(&id);
        geom.evict_tail += 1;
    }
    if !report.in_world {
        return;
    }

    let [wx, wy, wz] = report.wow_pos;
    let (pcx, pcy) = ((wx / CELL_YD).floor() as i32, (wy / CELL_YD).floor() as i32);
    let mut wanted: Vec<(i32, i32)> = Vec::new();
    for dx in -RADIUS..=RADIUS {
        for dy in -RADIUS..=RADIUS {
            wanted.push((pcx + dx, pcy + dy));
        }
    }
    // Nearest first: the cell underfoot before the far corners.
    wanted.sort_by_key(|(cx, cy)| (cx - pcx).abs().max((cy - pcy).abs()));
    let now_epoch = collider_epoch.get();
    let mut budget = BUDGET;
    for (cx, cy) in wanted {
        let id = cell_id(cx, cy);
        if geom.sent.get(&id).is_some_and(|&(e, _)| e == now_epoch) {
            continue;
        }
        if budget == 0 {
            break;
        }
        budget -= 1;
        let payload = cell_payload(&collide, &ground, &on_deck, id, cx, cy, wz);
        let hash = {
            let mut h = DefaultHasher::new();
            payload.hash(&mut h);
            h.finish()
        };
        if geom.sent.get(&id).is_some_and(|&(_, h)| h == hash) {
            geom.sent.insert(id, (now_epoch, hash)); // unchanged by the new colliders
            continue;
        }
        if !geom.send(MSG_BATCH, &payload) {
            break; // ring full: the mod is behind; try again next frame
        }
        geom.sent.insert(id, (now_epoch, hash));
    }
    if geom.sent.len() != geom.logged_cells {
        geom.logged_cells = geom.sent.len();
        debug!(
            "classiccraft: geometry cells current: {}",
            geom.logged_cells
        );
    }
}

/// One cell's `McwowGeomBatchHeader` + triangles, in Minecraft block space (region-local).
fn cell_payload(
    collide: &WorldCollision,
    ground: &Ground,
    on_deck: &crate::decks::OnDeck,
    id: u64,
    cx: i32,
    cy: i32,
    feet_z: f32,
) -> Vec<u8> {
    // The cell's WoW square [cx·C, cx·C + C) × [cy·C, cy·C + C) in Bevy (-wowY, wowZ, -wowX).
    let centre = Vec3::new(
        -(cy as f32 + 0.5) * CELL_YD,
        feet_z,
        -(cx as f32 + 0.5) * CELL_YD,
    );
    let half = Vec3::new(CELL_YD * 0.5, COLUMN_HALF, CELL_YD * 0.5);
    let faces = collide.faces_near_body(centre, half, usize::MAX);
    let mut tris: Vec<u8> = Vec::with_capacity(faces.len() * 40);
    let mut count = 0_u32;
    for f in &faces {
        // A transport's deck moves: it goes as a deck of its own (`crate::decks`), never into a
        // cell (frozen there, the trams stood invisible at their spawn spots, 2026-10-02).
        if on_deck.is_deck(f.entity) {
            continue;
        }
        let c = f.centroid();
        // WoW x = -bevy z, y = -bevy x: keep the triangle only in the cell its centroid is in.
        if ((-c.z) / CELL_YD).floor() as i32 != cx || ((-c.x) / CELL_YD).floor() as i32 != cy {
            continue;
        }
        let walkable = f.normal.normalize_or_zero().y > GROUND_COS;
        let flags = if walkable { TRI_WALKABLE } else { 0 };
        // WoW (x, y, z) = (-bevy z, -bevy x, bevy y).
        let wow = f.verts.map(|v| [-v.z, -v.x, v.y]);
        for tri in ground.cut_holes(wow) {
            for v in tri {
                // Minecraft (x, y, z) = (wowY, wowZ, wowX) / S.
                for comp in [v[1] / S, v[2] / S, v[0] / S] {
                    tris.extend_from_slice(&comp.to_le_bytes());
                }
            }
            tris.extend_from_slice(&flags.to_le_bytes());
            count += 1;
        }
    }
    let mut payload = Vec::with_capacity(16 + tris.len());
    payload.extend_from_slice(&id.to_le_bytes());
    payload.extend_from_slice(&count.to_le_bytes());
    payload.extend_from_slice(&KIND_MODEL.to_le_bytes());
    payload.extend_from_slice(&tris);
    payload
}

/// Dug holes in WoW's terrain, for the collision export: a terrain face over an open Minecraft
/// column loses that column's square, so bodies drop into the hole.
struct Ground<'a> {
    streamer: &'a TerrainStreamer,
    tiles: &'a Assets<AdtTile>,
    holes: &'a HoleMasks,
}

type Poly = Vec<[f32; 3]>;

impl Ground<'_> {
    /// A face (WoW coords) as the triangles left after cutting out the open columns it overlaps;
    /// itself when it is no terrain face or touches no hole.
    fn cut_holes(&self, tri: [[f32; 3]; 3]) -> Vec<[[f32; 3]; 3]> {
        let whole = vec![tri];
        if !self.holes.any() {
            return whole;
        }
        let lo = |k: usize| tri.iter().map(|v| v[k]).fold(f32::INFINITY, f32::min);
        let hi = |k: usize| tri.iter().map(|v| v[k]).fold(f32::NEG_INFINITY, f32::max);
        // Open columns under the face's footprint (column x = wowY/S, z = wowX/S).
        let mut squares = Vec::new();
        for mz in (lo(0) / S).floor() as i32..=(hi(0) / S).floor() as i32 {
            for mx in (lo(1) / S).floor() as i32..=(hi(1) / S).floor() as i32 {
                if self.holes.is_open(mx, mz) {
                    squares.push((
                        mz as f32 * S,
                        (mz + 1) as f32 * S,
                        mx as f32 * S,
                        (mx + 1) as f32 * S,
                    ));
                }
            }
        }
        if squares.is_empty() || !self.is_terrain(&tri) {
            return whole;
        }
        let mut pieces: Vec<Poly> = vec![tri.to_vec()];
        for (x0, x1, y0, y1) in squares {
            pieces = pieces
                .into_iter()
                .flat_map(|p| subtract_square(&p, x0, x1, y0, y1))
                .collect();
        }
        pieces
            .iter()
            .flat_map(|p| (1..p.len() - 1).map(move |i| [p[0], p[i], p[i + 1]]))
            .filter(|t| area2(t) > 1.0e-6)
            .collect()
    }

    /// A terrain face: every vertex on the terrain heightfield. Each is checked a hair inside the
    /// face: a terrain face's corners lie on MCNK edges, where the height lookup can round off both
    /// chunks and find nothing - the face then wasn't cut and a dug column kept its collision.
    fn is_terrain(&self, tri: &[[f32; 3]; 3]) -> bool {
        let mut cache = None;
        let c: [f32; 3] = std::array::from_fn(|k| (tri[0][k] + tri[1][k] + tri[2][k]) / 3.0);
        tri.iter().all(|v| {
            let v: [f32; 3] = std::array::from_fn(|k| v[k] + (c[k] - v[k]) * 0.01);
            let bevy = Vec3::new(-v[1], v[2], -v[0]);
            terrain_height_under_cached(self.streamer, self.tiles, bevy, &mut cache)
                .is_some_and(|z| (z - v[2]).abs() < 0.05)
        })
    }
}

/// Twice the horizontal area of a triangle (WoW x, y).
fn area2(t: &[[f32; 3]; 3]) -> f32 {
    ((t[1][0] - t[0][0]) * (t[2][1] - t[0][1]) - (t[2][0] - t[0][0]) * (t[1][1] - t[0][1])).abs()
}

/// The convex parts of `poly` outside the square `x0..x1 × y0..y1` (WoW x, y): left, right, and the
/// strips before and after it between them.
fn subtract_square(poly: &Poly, x0: f32, x1: f32, y0: f32, y1: f32) -> Vec<Poly> {
    let mut out = Vec::new();
    let mut keep = |p: Poly| {
        if p.len() >= 3 {
            out.push(p);
        }
    };
    keep(clip(poly, 0, x0, true));
    let rest = clip(poly, 0, x0, false);
    keep(clip(&rest, 0, x1, false));
    let mid = clip(&rest, 0, x1, true);
    keep(clip(&mid, 1, y0, true));
    keep(clip(&clip(&mid, 1, y0, false), 1, y1, false));
    out
}

/// Sutherland-Hodgman against one axis-aligned line: the part with `v[axis] <= at` (`below`) or
/// `>= at`, every coordinate (height too) interpolated along the cut edges.
fn clip(poly: &Poly, axis: usize, at: f32, below: bool) -> Poly {
    let inside = |v: &[f32; 3]| if below { v[axis] <= at } else { v[axis] >= at };
    let mut out = Vec::with_capacity(poly.len() + 2);
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
        if inside(&a) {
            out.push(a);
        }
        if inside(&a) != inside(&b) {
            let t = (at - a[axis]) / (b[axis] - a[axis]);
            out.push(std::array::from_fn(|k| a[k] + (b[k] - a[k]) * t));
        }
    }
    out
}
