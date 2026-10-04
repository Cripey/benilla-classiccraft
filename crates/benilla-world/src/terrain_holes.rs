//! classiccraft (fork only): holes dug into WoW's terrain by the Minecraft bridge. Not a 1.12.1
//! behaviour. A crate on top fills [`TerrainHoles`] with the open Minecraft block columns (1.4667
//! yd squares on the fixed mapping `x = wowY/S`, `z = wowX/S`) in a window of chunks around the
//! player; `terrain.wgsl` discards terrain pixels over an open column, so the Minecraft blocks below
//! show through.

use bevy::prelude::*;
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::render_resource::{Buffer, BufferDescriptor, BufferUsages};
use bevy::render::renderer::{RenderDevice, RenderQueue};
use bevy::render::{Render, RenderApp, RenderSystems};

/// Chunks (16×16 columns) per window edge.
pub const WINDOW_CHUNKS: i32 = 32;
/// `u32` words of mask per chunk (256 columns).
pub const WORDS_PER_CHUNK: usize = 8;
const WORDS: usize = (WINDOW_CHUNKS * WINDOW_CHUNKS) as usize * WORDS_PER_CHUNK;
/// Header (origin chunk x, z, window edge, pad) then the words, as `terrain.wgsl`'s `TerrainHoles`.
const HEADER_WORDS: usize = 4;

/// How far the camera is under WoW's ground, 0 (at or above it) to 1 (2 yd or more under), set by
/// the crate on top: weather treats it as cover, as a cave - no storm fog, no precipitation.
#[derive(Resource, Default)]
pub struct CameraUnderground(pub f32);

/// The hole buffer every terrain material binds (binding 120).
#[derive(Resource, Clone, ExtractResource)]
pub struct TerrainHoleBuffer(pub Buffer);

/// The window's contents: origin chunk (window-local chunk `(i, j)` is chunk
/// `(origin.x + i, origin.y + j)`), and per chunk 8 words, column `z·16 + x` at bit `idx % 32` of
/// word `idx / 32`. Written by the crate on top; bump `generation` to upload.
#[derive(Resource, Clone, ExtractResource)]
pub struct TerrainHoles {
    pub origin: IVec2,
    pub words: Vec<u32>,
    pub generation: u64,
    /// The chunks whose mask the last [`Self::rebuild`] changed, for CPU consumers (ground clutter
    /// over a dug column, `clutter.rs`).
    pub changed: Vec<IVec2>,
}

impl Default for TerrainHoles {
    fn default() -> Self {
        Self {
            origin: IVec2::ZERO,
            words: vec![0; WORDS],
            generation: 0,
            changed: Vec::new(),
        }
    }
}

impl TerrainHoles {
    /// Rebuilds the window around `origin` from a chunk mask lookup.
    pub fn rebuild(
        &mut self,
        origin: IVec2,
        mask: impl Fn(IVec2) -> Option<[u32; WORDS_PER_CHUNK]>,
    ) {
        let (old_origin, old_words) = (self.origin, std::mem::take(&mut self.words));
        self.words = vec![0; WORDS];
        self.origin = origin;
        for j in 0..WINDOW_CHUNKS {
            for i in 0..WINDOW_CHUNKS {
                if let Some(m) = mask(origin + IVec2::new(i, j)) {
                    let at = (j * WINDOW_CHUNKS + i) as usize * WORDS_PER_CHUNK;
                    self.words[at..at + WORDS_PER_CHUNK].copy_from_slice(&m);
                }
            }
        }
        // Compared per chunk over both windows, so a recentre alone changes nothing.
        let old = |c: IVec2| chunk_mask(old_origin, &old_words, c);
        self.changed.clear();
        for base in [old_origin, origin] {
            for j in 0..WINDOW_CHUNKS {
                for i in 0..WINDOW_CHUNKS {
                    let c = base + IVec2::new(i, j);
                    if old(c) != chunk_mask(origin, &self.words, c) && !self.changed.contains(&c) {
                        self.changed.push(c);
                    }
                }
            }
        }
        self.generation += 1;
    }

    /// Whether the Minecraft column over a raw WoW position is open, as `terrain.wgsl`'s
    /// `in_open_column` decides it; `false` outside the window.
    pub fn is_open_wow(&self, wow_x: f32, wow_y: f32) -> bool {
        // Bevy (x, z) = (-wowY, -wowX); the shader's column is `floor(-world.xz / S)`.
        let col = IVec2::new(
            (wow_y / MC_BLOCK_YD).floor() as i32,
            (wow_x / MC_BLOCK_YD).floor() as i32,
        );
        let m = chunk_mask(self.origin, &self.words, col >> 4);
        let idx = ((col.y & 15) * 16 + (col.x & 15)) as usize;
        m[idx / 32] & (1 << (idx % 32)) != 0
    }

    /// A chunk's raw WoW footprint, `(min, max)` as `[wow_x, wow_y]`.
    pub fn chunk_wow_rect(chunk: IVec2) -> ([f32; 2], [f32; 2]) {
        let span = 16.0 * MC_BLOCK_YD;
        (
            [chunk.y as f32 * span, chunk.x as f32 * span],
            [(chunk.y + 1) as f32 * span, (chunk.x + 1) as f32 * span],
        )
    }
}

/// Yards per Minecraft block (the bridge's scale; `terrain.wgsl` hardcodes it too).
const MC_BLOCK_YD: f32 = 1.4667;

/// One chunk's mask in a window at `origin`, all zero outside it.
fn chunk_mask(origin: IVec2, words: &[u32], chunk: IVec2) -> [u32; WORDS_PER_CHUNK] {
    let local = chunk - origin;
    if local.x < 0 || local.y < 0 || local.x >= WINDOW_CHUNKS || local.y >= WINDOW_CHUNKS {
        return [0; WORDS_PER_CHUNK];
    }
    let at = (local.y * WINDOW_CHUNKS + local.x) as usize * WORDS_PER_CHUNK;
    words[at..at + WORDS_PER_CHUNK].try_into().unwrap()
}

/// Creates the buffer, empty (no holes), from the main-world `RenderDevice` at startup.
pub fn new_terrain_hole_buffer(device: &RenderDevice) -> TerrainHoleBuffer {
    TerrainHoleBuffer(device.create_buffer(&BufferDescriptor {
        label: Some("classiccraft_terrain_holes"),
        size: ((HEADER_WORDS + WORDS) * 4) as u64,
        usage: BufferUsages::STORAGE | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    }))
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<TerrainHoles>()
        .init_resource::<CameraUnderground>()
        .add_plugins(ExtractResourcePlugin::<TerrainHoles>::default())
        .add_plugins(ExtractResourcePlugin::<TerrainHoleBuffer>::default());
    if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
        render_app.add_systems(Render, upload.in_set(RenderSystems::PrepareResources));
    }
}

/// Render world: writes the window when its generation moved (and once, zeroed, at the start).
fn upload(
    queue: Res<RenderQueue>,
    buffer: Option<Res<TerrainHoleBuffer>>,
    holes: Option<Res<TerrainHoles>>,
    mut uploaded: Local<Option<u64>>,
) {
    let (Some(buffer), Some(holes)) = (buffer, holes) else {
        return;
    };
    if *uploaded == Some(holes.generation) {
        return;
    }
    let mut data = Vec::with_capacity(HEADER_WORDS + holes.words.len());
    data.extend_from_slice(&[
        holes.origin.x as u32,
        holes.origin.y as u32,
        WINDOW_CHUNKS as u32,
        0,
    ]);
    data.extend_from_slice(&holes.words);
    queue.write_buffer(&buffer.0, 0, bytemuck::cast_slice(&data));
    *uploaded = Some(holes.generation);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One open column at Minecraft (x, z) = (37, -5), in a window around it.
    fn one_hole(origin: IVec2) -> TerrainHoles {
        let mut h = TerrainHoles::default();
        h.rebuild(origin, |c| {
            (c == IVec2::new(2, -1)).then(|| {
                let mut m = [0; WORDS_PER_CHUNK];
                let idx = (((-5i32) & 15) * 16 + (37 & 15)) as usize;
                m[idx / 32] |= 1 << (idx % 32);
                m
            })
        });
        h
    }

    #[test]
    fn the_cpu_lookup_matches_the_shaders_column() {
        let h = one_hole(IVec2::new(-10, -10));
        // Column (37, -5) spans wowY [37, 38)·S and wowX [-5, -4)·S.
        let (wow_x, wow_y) = (-4.5 * MC_BLOCK_YD, 37.5 * MC_BLOCK_YD);
        assert!(h.is_open_wow(wow_x, wow_y));
        assert!(!h.is_open_wow(wow_x + MC_BLOCK_YD, wow_y));
        assert!(!h.is_open_wow(wow_x, wow_y + MC_BLOCK_YD));
        let (lo, hi) = TerrainHoles::chunk_wow_rect(IVec2::new(2, -1));
        assert!(lo[0] <= wow_x && wow_x < hi[0] && lo[1] <= wow_y && wow_y < hi[1]);
    }

    #[test]
    fn a_recentre_alone_changes_nothing() {
        let mut h = one_hole(IVec2::new(-10, -10));
        assert_eq!(h.changed, vec![IVec2::new(2, -1)]);
        let mask = chunk_mask(h.origin, &h.words, IVec2::new(2, -1));
        h.rebuild(IVec2::new(-5, -12), |c| (c == IVec2::new(2, -1)).then_some(mask));
        assert!(h.changed.is_empty());
        h.rebuild(IVec2::new(-5, -12), |_| None);
        assert_eq!(h.changed, vec![IVec2::new(2, -1)]);
    }
}
