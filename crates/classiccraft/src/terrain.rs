//! The terrain export for breakable WoW ground (Phase 1): for every Minecraft chunk near the body,
//! each block column's WoW ground as the mod needs it to fill real blocks under the WoW surface.
//! Sent over the geometry ring (`protocol/mcwow_geom_protocol.h`, `MSG_TERRAIN`), one 16×16-column
//! Minecraft chunk per message, once per ring epoch.
//!
//! Per column (Minecraft block coordinates, region-local, `x = wowY/S`, `z = wowX/S`):
//! - `surface`: the lowest of 3×3 terrain samples across the column, so a block topped at or under
//!   it never pokes through WoW's ground between the samples on a slope; NaN in a terrain hole (a
//!   cave mouth) or off the map;
//! - `fill_top`: `surface`, lowered under any collision face well below the terrain (a cellar, mine
//!   or tunnel of a WMO), so the fill never intrudes into one;
//! - the surface material from the dominant ground texture, and the `AreaTable` id;
//! - the WoW terrain liquid (MCLQ) over the column's centre: its surface and kind, which the mod
//!   fills with water blocks where it is above the ground (outdoor water, 2026-10-02).
//!
//! A chunk waits until every tile under it has been resident a few seconds and the collider set
//! has settled, so WMO interiors are known before the mod fills (and keeps) the ground there.

use std::collections::{HashMap, HashSet};

use benilla_app::external::{ExternalDriveSet, SelfReport};
use benilla_assets::AdtTile;
use benilla_formats::{world_to_tile, ChunkMesh, LiquidKind};
use benilla_world::collision::{ColliderEpoch, WorldCollision};
use benilla_world::liquid::{LiquidSource, WaterChunkInfo};
use benilla_world::terrain_stream::TerrainStreamer;
use bevy::prelude::*;

use crate::bridge::S;
use crate::geom::Geom;

const MSG_TERRAIN: u32 = 4;
/// Minecraft chunks (16 blocks) around the body to describe.
const RADIUS: i32 = 6;
/// Chunks computed per frame.
const BUDGET: usize = 2;
/// Seconds a tile must have been resident, and the collider set unchanged, before its ground is
/// sent: the mod fills once and keeps it.
const TILE_SETTLE: f32 = 3.0;
const COLLIDER_SETTLE: f32 = 1.0;
/// A collision face this far (yd) under the terrain belongs to a structure below the ground.
const UNDERGROUND_YD: f32 = 0.75;
/// Blocks of air kept between a structure's lowest face and the fill under it.
const STRUCTURE_MARGIN: f32 = 0.5;
const COLUMN_HALF: f32 = 2000.0;
/// Yards a no-ground sample is retried to each side (MCNK edge rounding).
const NUDGE: f32 = 0.01;

/// Surface materials (`McwowTerrainFill.MAT_*` in the mod).
const MAT_GRASS: u8 = 1;
const MAT_DIRT: u8 = 2;
const MAT_SAND: u8 = 3;
const MAT_SNOW: u8 = 4;
const MAT_STONE: u8 = 5;
const MAT_GRAVEL: u8 = 6;
const MAT_MUD: u8 = 7;

/// Liquid kinds (`McwowTerrainFill.LIQ_*` in the mod), in the column's flags byte.
const LIQ_WATER: u8 = 1;
const LIQ_OCEAN: u8 = 2;
const LIQ_MAGMA: u8 = 3;
const LIQ_SLIME: u8 = 4;

pub struct TerrainPlugin;

impl Plugin for TerrainPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Terrain>().add_systems(
            Update,
            stream_terrain
                .after(crate::geom::stream_cells)
                .in_set(ExternalDriveSet::Report),
        );
    }
}

#[derive(Resource, Default)]
struct Terrain {
    /// Chunks sent in the current ring epoch.
    sent: HashSet<(i32, i32)>,
    epoch: u32,
    /// When each tile's asset was first seen resident.
    tile_seen: HashMap<(i32, i32), f32>,
    collider_epoch: u64,
    collider_changed_at: f32,
    /// Ground texture -> material, and the names no rule matched (logged once).
    materials: HashMap<String, u8>,
    logged: usize,
}

enum Sample {
    /// A tile under it is not resident (or not settled) yet: try the chunk again later.
    Pending,
    /// No terrain: a hole or off the map.
    None,
    Ground(f32),
}

struct Ground<'a> {
    streamer: &'a TerrainStreamer,
    tiles: &'a Assets<AdtTile>,
    tile_seen: &'a mut HashMap<(i32, i32), f32>,
    now: f32,
}

impl<'a> Ground<'a> {
    fn tile(&mut self, wow_x: f32, wow_y: f32) -> Option<&'a AdtTile> {
        let (tx, ty) = world_to_tile(wow_x, wow_y);
        let key = (tx as i32, ty as i32);
        let adt = self.tiles.get(self.streamer.tile_handle(key)?)?;
        let seen = *self.tile_seen.entry(key).or_insert(self.now);
        (self.now - seen >= TILE_SETTLE).then_some(adt)
    }

    fn chunk(&mut self, wow_x: f32, wow_y: f32) -> Result<Option<&'a ChunkMesh>, ()> {
        let adt = self.tile(wow_x, wow_y).ok_or(())?;
        // MCNKs are stored in file order, row-major by (index_y, index_x); scan only on a miss.
        let (gx, gy) = benilla_formats::world_to_chunk(wow_x, wow_y);
        let guess = adt.chunks.get((gy % 16 * 16 + gx % 16) as usize);
        if let Some(c) = guess.filter(|c| footprint(c, wow_x, wow_y).is_some()) {
            return Ok(Some(c));
        }
        Ok(adt
            .chunks
            .iter()
            .find(|c| footprint(c, wow_x, wow_y).is_some()))
    }

    fn height(&mut self, wow_x: f32, wow_y: f32) -> Sample {
        let sample = self.height_exact(wow_x, wow_y);
        if !matches!(sample, Sample::None) {
            return sample;
        }
        // A point on the edge between two MCNKs can round off both footprints: try a hair to each
        // side before calling it no ground (a column was left unfilled that way, its WoW ground
        // then never dug away, 2026-10-02). A real terrain hole misses on every side.
        for (dx, dy) in [(NUDGE, 0.0), (-NUDGE, 0.0), (0.0, NUDGE), (0.0, -NUDGE)] {
            if let s @ (Sample::Ground(_) | Sample::Pending) =
                self.height_exact(wow_x + dx, wow_y + dy)
            {
                return s;
            }
        }
        Sample::None
    }

    fn height_exact(&mut self, wow_x: f32, wow_y: f32) -> Sample {
        match self.chunk(wow_x, wow_y) {
            Err(()) => Sample::Pending,
            Ok(None) => Sample::None,
            Ok(Some(c)) => match c.height_at([wow_x, wow_y, 0.0]) {
                Some(z) => Sample::Ground(z),
                None => Sample::None,
            },
        }
    }
}

/// The MCLQ liquid at a raw WoW position: the lowest surface where several stack (a river mouth's
/// stream over its sea), as benilla's own `liquid_at` picks, and its kind. `surfaces` caches each
/// MCNK's queryable surfaces for the payload being built.
fn liquid_at(
    c: &ChunkMesh,
    surfaces: &mut HashMap<*const ChunkMesh, Vec<(WaterChunkInfo, LiquidKind)>>,
    wow_x: f32,
    wow_y: f32,
) -> Option<(f32, LiquidKind)> {
    if c.liquids.is_empty() {
        return None;
    }
    let infos = surfaces.entry(c as *const ChunkMesh).or_insert_with(|| {
        c.liquids
            .iter()
            .map(|lq| {
                let info = WaterChunkInfo::new(
                    LiquidSource::AdtChunk,
                    lq.kind,
                    [lq.grid[0] as usize, lq.grid[1] as usize],
                    lq.positions.clone(),
                    lq.wet.clone(),
                );
                (info, lq.kind)
            })
            .collect()
    });
    infos
        .iter()
        .filter_map(|(w, kind)| Some((w.surface_z_at(wow_x, wow_y)?, *kind)))
        .min_by(|a, b| a.0.total_cmp(&b.0))
}

/// `(south, east)` fractions 0..1 of a raw WoW position across a chunk, `None` off it.
fn footprint(c: &ChunkMesh, wow_x: f32, wow_y: f32) -> Option<(f32, f32)> {
    let nw = *c.positions.first()?;
    let south = (nw[0] - wow_x) / benilla_formats::TILE_SIZE * 16.0;
    let east = (nw[1] - wow_y) / benilla_formats::TILE_SIZE * 16.0;
    ((0.0..=1.0).contains(&south) && (0.0..=1.0).contains(&east)).then_some((south, east))
}

/// The ground texture with the most weight at a position: the layers blend in order, each over
/// the ones before it by its alpha (R/G/B of the combined map for layers 1-3).
fn dominant_texture(c: &ChunkMesh, south: f32, east: f32) -> Option<&str> {
    let n = c.layer_textures.len();
    if n == 0 {
        return c.base_texture.as_deref();
    }
    let mut weights = [0.0_f32; 4];
    weights[0] = 1.0;
    if let Some(alpha) = &c.alpha_map {
        let size = benilla_formats::ALPHA_MAP_SIZE as usize;
        let row = ((south * size as f32) as usize).min(size - 1);
        let col = ((east * size as f32) as usize).min(size - 1);
        let texel = &alpha[(row * size + col) * 4..][..4];
        for layer in 1..n.min(4) {
            let a = f32::from(texel[layer - 1]) / 255.0;
            for w in weights.iter_mut().take(layer) {
                *w *= 1.0 - a;
            }
            weights[layer] = a;
        }
    }
    let best = (0..n.min(4)).max_by(|&a, &b| weights[a].total_cmp(&weights[b]))?;
    c.layer_textures.get(best).map(String::as_str)
}

/// A ground texture's Minecraft surface material, by the words in its file name.
fn classify(texture: &str) -> Option<u8> {
    let name = texture.to_ascii_lowercase();
    let name = name.rsplit(['\\', '/']).next().unwrap_or(&name);
    let has = |words: &[&str]| words.iter().any(|w| name.contains(w));
    if has(&["snow", "ice", "frost"]) {
        Some(MAT_SNOW)
    } else if has(&["sand", "beach", "desert", "dune"]) {
        Some(MAT_SAND)
    } else if has(&["mud", "swamp", "bog", "marsh"]) {
        Some(MAT_MUD)
    } else if has(&["gravel", "pebble"]) {
        Some(MAT_GRAVEL)
    } else if has(&[
        "grass", "moss", "needle", "leaf", "leaves", "fern", "flower", "meadow", "weed",
    ]) {
        Some(MAT_GRASS)
    } else if has(&["rock", "stone", "cliff", "crag", "mountain", "boulder"]) {
        Some(MAT_STONE)
    } else if has(&["dirt", "soil", "path", "road", "trail", "ground"]) {
        Some(MAT_DIRT)
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_terrain(
    time: Res<Time>,
    report: Res<SelfReport>,
    collider_epoch: Res<ColliderEpoch>,
    streamer: Res<TerrainStreamer>,
    tiles: Res<Assets<AdtTile>>,
    collide: WorldCollision,
    mut geom: ResMut<Geom>,
    mut terrain: ResMut<Terrain>,
) {
    let terrain = &mut *terrain;
    let now = time.elapsed_secs();
    if collider_epoch.get() != terrain.collider_epoch {
        terrain.collider_epoch = collider_epoch.get();
        terrain.collider_changed_at = now;
    }
    if geom.epoch != terrain.epoch {
        terrain.epoch = geom.epoch;
        terrain.sent.clear();
        terrain.tile_seen.clear();
    }
    if !geom.is_open() || !report.in_world || now - terrain.collider_changed_at < COLLIDER_SETTLE {
        return;
    }
    let [wx, wy, wz] = report.wow_pos;
    let (pcx, pcz) = (
        (wy / S / 16.0).floor() as i32,
        (wx / S / 16.0).floor() as i32,
    );
    let mut wanted: Vec<(i32, i32)> = Vec::new();
    for dx in -RADIUS..=RADIUS {
        for dz in -RADIUS..=RADIUS {
            if !terrain.sent.contains(&(pcx + dx, pcz + dz)) {
                wanted.push((pcx + dx, pcz + dz));
            }
        }
    }
    wanted.sort_by_key(|(cx, cz)| (cx - pcx).abs().max((cz - pcz).abs()));
    let mut ground = Ground {
        streamer: &streamer,
        tiles: &tiles,
        tile_seen: &mut terrain.tile_seen,
        now,
    };
    let mut budget = BUDGET;
    for (cx, cz) in wanted {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let Some(payload) =
            chunk_payload(&mut ground, &collide, &mut terrain.materials, cx, cz, wz)
        else {
            continue; // a tile under it is still arriving
        };
        if !geom.send(MSG_TERRAIN, &payload) {
            break; // ring full; next frame
        }
        terrain.sent.insert((cx, cz));
    }
    if terrain.sent.len() / 50 != terrain.logged / 50 {
        terrain.logged = terrain.sent.len();
        info!("classiccraft: terrain chunks sent: {}", terrain.logged);
    }
}

/// One Minecraft chunk's `MSG_TERRAIN` payload: `i32 cx, i32 cz`, then 256 columns (`z·16 + x`)
/// of `f32 surface, f32 fill_top` (Minecraft y), `u8 material, u8 liquid kind, u16 area,
/// f32 liquid surface` (Minecraft y, NaN for none).
fn chunk_payload(
    ground: &mut Ground,
    collide: &WorldCollision,
    materials: &mut HashMap<String, u8>,
    cx: i32,
    cz: i32,
    feet_z: f32,
) -> Option<Vec<u8>> {
    let mut surface = [f32::NAN; 256];
    let mut material = [0_u8; 256];
    let mut area = [0_u16; 256];
    let mut liquid = [f32::NAN; 256];
    let mut liquid_kind = [0_u8; 256];
    let mut surfaces = HashMap::new();
    for lz in 0..16 {
        for lx in 0..16 {
            let (mx, mz) = ((cx * 16 + lx) as f32, (cz * 16 + lz) as f32);
            let i = (lz * 16 + lx) as usize;
            let mut low = f32::INFINITY;
            let mut hole = false;
            for fx in [0.02, 0.5, 0.98] {
                for fy in [0.02, 0.5, 0.98] {
                    match ground.height((mz + fx) * S, (mx + fy) * S) {
                        Sample::Pending => return None,
                        Sample::None => hole = true,
                        Sample::Ground(z) => low = low.min(z),
                    }
                }
            }
            if hole {
                continue;
            }
            surface[i] = low / S;
            let (wow_x, wow_y) = ((mz + 0.5) * S, (mx + 0.5) * S);
            if let Ok(Some(c)) = ground.chunk(wow_x, wow_y) {
                area[i] = c.area_id.min(u32::from(u16::MAX)) as u16;
                if let Some((z, kind)) = liquid_at(c, &mut surfaces, wow_x, wow_y) {
                    liquid[i] = z / S;
                    liquid_kind[i] = match kind {
                        LiquidKind::Still | LiquidKind::Rapids => LIQ_WATER,
                        LiquidKind::Ocean => LIQ_OCEAN,
                        LiquidKind::Magma => LIQ_MAGMA,
                        LiquidKind::Slime => LIQ_SLIME,
                    };
                }
                if let Some((south, east)) = footprint(c, wow_x, wow_y) {
                    if let Some(tex) = dominant_texture(c, south, east) {
                        material[i] = *materials.entry(tex.to_owned()).or_insert_with(|| {
                            classify(tex).unwrap_or_else(|| {
                                info!("classiccraft: ground texture {tex:?} has no material rule: dirt");
                                MAT_DIRT
                            })
                        });
                    }
                }
            }
        }
    }

    // Structures under the ground: no fill above their lowest face.
    let mut fill_top = surface;
    let centre = Vec3::new(
        -(cx as f32 * 16.0 + 8.0) * S,
        feet_z,
        -(cz as f32 * 16.0 + 8.0) * S,
    );
    let half = Vec3::new(8.0 * S, COLUMN_HALF, 8.0 * S);
    for f in collide.faces_near_body(centre, half, usize::MAX) {
        // Bevy (x, y, z) = (-wowY, wowZ, -wowX).
        let wow: [[f32; 3]; 3] = f.verts.map(|v| [-v.z, -v.x, v.y]);
        let c = [
            (wow[0][0] + wow[1][0] + wow[2][0]) / 3.0,
            (wow[0][1] + wow[1][1] + wow[2][1]) / 3.0,
            (wow[0][2] + wow[1][2] + wow[2][2]) / 3.0,
        ];
        let underground = match ground.height(c[0], c[1]) {
            Sample::Pending => continue,
            Sample::None => true,
            Sample::Ground(z) => c[2] < z - UNDERGROUND_YD,
        };
        if !underground {
            continue;
        }
        let lo = |k: usize| wow.iter().map(|v| v[k]).fold(f32::INFINITY, f32::min);
        let hi = |k: usize| wow.iter().map(|v| v[k]).fold(f32::NEG_INFINITY, f32::max);
        let limit = lo(2) / S - STRUCTURE_MARGIN;
        let (x0, x1) = (
            (lo(1) / S).floor() as i32 - cx * 16,
            (hi(1) / S).floor() as i32 - cx * 16,
        );
        let (z0, z1) = (
            (lo(0) / S).floor() as i32 - cz * 16,
            (hi(0) / S).floor() as i32 - cz * 16,
        );
        for lz in z0.max(0)..=z1.min(15) {
            for lx in x0.max(0)..=x1.min(15) {
                let top = &mut fill_top[(lz * 16 + lx) as usize];
                if !top.is_nan() {
                    *top = top.min(limit);
                }
            }
        }
    }

    let mut payload = Vec::with_capacity(8 + 256 * 16);
    payload.extend_from_slice(&cx.to_le_bytes());
    payload.extend_from_slice(&cz.to_le_bytes());
    for i in 0..256 {
        payload.extend_from_slice(&surface[i].to_le_bytes());
        payload.extend_from_slice(&fill_top[i].to_le_bytes());
        payload.push(material[i]);
        payload.push(liquid_kind[i]);
        payload.extend_from_slice(&area[i].to_le_bytes());
        payload.extend_from_slice(&liquid[i].to_le_bytes());
    }
    Some(payload)
}
