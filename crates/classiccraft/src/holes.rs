//! Holes dug into WoW's terrain (breakable terrain, Phase 2): the open Minecraft block columns per
//! chunk, as the mod reports them (`REN_HOLES`, a column is open when its top filled block is gone).
//! They drive the terrain shader's discard window ([`TerrainHoles`]) and the collision export, which
//! clips the open columns out of WoW's terrain triangles ([`crate::geom`]).

use std::collections::HashMap;

use benilla_app::external::{ExternalDriveSet, SelfReport};
use benilla_assets::AdtTile;
use benilla_world::terrain_holes::{
    CameraUnderground, TerrainHoles, WINDOW_CHUNKS, WORDS_PER_CHUNK,
};
use benilla_world::terrain_stream::{terrain_height_under, TerrainStreamer};
use benilla_world::view::WorldCamera;
use bevy::prelude::*;

use crate::bridge::S;

pub struct HolesPlugin;

impl Plugin for HolesPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HoleMasks>().add_systems(
            Update,
            (update_window, update_underground).in_set(ExternalDriveSet::Report),
        );
    }
}

/// Open columns per Minecraft chunk (region-local), bit `z·16 + x`.
#[derive(Resource, Default)]
pub(crate) struct HoleMasks {
    masks: HashMap<(i32, i32), [u32; WORDS_PER_CHUNK]>,
    /// Chunks whose mask changed since the collision export last looked.
    pub(crate) dirty: Vec<(i32, i32)>,
    /// Bumped on every change, for the shader window.
    version: u64,
}

impl HoleMasks {
    pub(crate) fn set(&mut self, chunk: (i32, i32), mask: [u32; WORDS_PER_CHUNK]) {
        let old = if mask.iter().all(|&w| w == 0) {
            self.masks.remove(&chunk)
        } else {
            self.masks.insert(chunk, mask)
        };
        if old.unwrap_or_default() != mask {
            self.dirty.push(chunk);
            self.version += 1;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.dirty.extend(self.masks.keys().copied());
        self.masks.clear();
        self.version += 1;
    }

    /// Whether the Minecraft column `(x, z)` (region-local blocks) is open.
    pub(crate) fn is_open(&self, x: i32, z: i32) -> bool {
        let Some(m) = self.masks.get(&(x.div_euclid(16), z.div_euclid(16))) else {
            return false;
        };
        let i = (z.rem_euclid(16) * 16 + x.rem_euclid(16)) as usize;
        m[i / 32] & (1 << (i % 32)) != 0
    }

    pub(crate) fn any(&self) -> bool {
        !self.masks.is_empty()
    }
}

/// Keeps the shader's window centred on the body's chunk and current with the masks.
fn update_window(
    report: Res<SelfReport>,
    masks: Res<HoleMasks>,
    mut holes: ResMut<TerrainHoles>,
    mut shown: Local<(IVec2, u64)>,
) {
    if !report.in_world {
        return;
    }
    let [wx, wy, _] = report.wow_pos;
    let here = IVec2::new(
        (wy / S / 16.0).floor() as i32,
        (wx / S / 16.0).floor() as i32,
    );
    let origin = here - IVec2::splat(WINDOW_CHUNKS / 2);
    // Recentre only when the body nears the window's edge, so walking rarely rebuilds it.
    let drift = (here - (shown.0 + IVec2::splat(WINDOW_CHUNKS / 2)))
        .abs()
        .max_element();
    let recentre = drift > WINDOW_CHUNKS / 4;
    if !recentre && shown.1 == masks.version {
        return;
    }
    let origin = if recentre || shown.1 == 0 {
        origin
    } else {
        shown.0
    };
    holes.rebuild(origin, |c| masks.masks.get(&(c.x, c.y)).copied());
    *shown = (origin, masks.version);
}

/// Yards under WoW's ground over which weather fades out for the camera (`CameraUnderground`).
const COVER_YD: f32 = 2.0;

/// How far the camera is under WoW's ground (its heightfield, holes or not): in a dug hole, weather
/// treats it as under cover, as in a cave.
fn update_underground(
    camera: Query<&GlobalTransform, With<WorldCamera>>,
    streamer: Res<TerrainStreamer>,
    tiles: Res<Assets<AdtTile>>,
    mut underground: ResMut<CameraUnderground>,
) {
    let Ok(cam) = camera.single() else {
        return;
    };
    let eye = cam.translation();
    let depth = terrain_height_under(&streamer, &tiles, eye).map_or(0.0, |ground| ground - eye.y);
    let cover = (depth / COVER_YD).clamp(0.0, 1.0);
    if underground.0 != cover {
        underground.0 = cover;
    }
}
