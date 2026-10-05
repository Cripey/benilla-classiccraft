//! Minecraft's own hand, held item, HUD and screens over benilla's picture: the mod renders them on
//! a transparent background into `protocol/mcwow_overlay_protocol.h`'s triple buffer every frame;
//! we take the newest slot and show it as a full-window UI image above the world and WoW's UI.
//! The same file carries the input ring ([`crate::input`]), so [`OverlayFile`] owns the mapping.
//!
//! The pixels never pass through the main world: the render world maps the file itself, takes the
//! frame and writes its changed bands straight into the overlay's GPU texture ([`upload_overlay`]).
//! A frame is up to 15 MB; copying it into an `Image` cost the main thread a full copy, the
//! un-premultiply, and Bevy's extract clone every frame (~25% of a frame, 2026-10-02 profile).

use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::{
    Extent3d, Origin3d, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureAspect,
    TextureDimension, TextureFormat,
};
use bevy::render::renderer::RenderQueue;
use bevy::render::texture::GpuImage;
use bevy::render::{Extract, ExtractSchedule, Render, RenderApp, RenderSystems};
use bevy::tasks::ComputeTaskPool;

use crate::bridge::Bridge;

const NAME: &str = "classiccraft_overlay_v1.shm";
const MAGIC: u32 = 0x6D63_6F31; // 'mco1'
const VERSION: u32 = 2;
const MAX_W: usize = 2560;
const MAX_H: usize = 1600;
const SLOT_BYTES: usize = MAX_W * MAX_H * 4;
const OFF_WRITER_PID: usize = 8;
const OFF_STATE: usize = 12;
const OFF_MC_FLAGS: usize = 36;
const MC_SCREEN: u32 = 1 << 1;
const OFF_SLOT_HDR: usize = 64;
const SLOT_HDR_BYTES: usize = 32;
const OFF_PIXELS: usize = 256;
const OFF_INPUT: usize = OFF_PIXELS + 3 * SLOT_BYTES;
const RING_HEAD: usize = 0;
const RING_DATA: usize = 128;
const RING_ENTRIES: u64 = 4096;
const TOTAL: usize = OFF_INPUT + RING_DATA + RING_ENTRIES as usize * 16;
const DIRTY: u32 = 1 << 2;
/// Slot flags: rows bottom-up (GL readback order), and `band_mask` valid.
const FLAG_BOTTOM_UP: u32 = 1;
const FLAG_BANDS: u32 = 1 << 1;
const BANDS: usize = 64;
/// Above the world and WoW's UI quads, below the cinematic (900) and the loading cover (1000).
const OVERLAY_Z: i32 = 850;

pub struct OverlayPlugin;

impl Plugin for OverlayPlugin {
    fn build(&self, app: &mut App) {
        let shared = SharedFrame::default();
        app.init_resource::<OverlayFile>()
            .init_resource::<OverlayView>()
            .insert_resource(shared.clone())
            .add_systems(Startup, spawn_overlay)
            .add_systems(Update, show_overlay);
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .insert_resource(OverlayUpload {
                shared,
                ..default()
            })
            .add_systems(ExtractSchedule, extract_overlay)
            .add_systems(
                Render,
                // After `prepare_assets::<GpuImage>`, so a resized texture is the one we write.
                upload_overlay.in_set(RenderSystems::PrepareResources),
            );
    }
}

/// Minecraft's overlay file, mapped once it exists.
#[derive(Resource, Default)]
pub struct OverlayFile {
    map: Option<crate::link::SharedMap>,
    retry_at: f32,
    /// The last frame's size: Minecraft's framebuffer, the space its cursor lives in.
    frame_size: Option<UVec2>,
}

impl OverlayFile {
    pub fn is_open(&self) -> bool {
        self.map.is_some()
    }

    /// A Minecraft screen (pause menu, inventory, chest...) is open: it wants a cursor.
    pub fn mc_screen_open(&self) -> bool {
        self.is_open() && self.u32(OFF_MC_FLAGS) & MC_SCREEN != 0
    }

    /// Minecraft's framebuffer size, once a frame has arrived.
    pub fn frame_size(&self) -> Option<UVec2> {
        self.frame_size
    }

    pub fn open(&mut self, now: f32) {
        if self.map.is_some() || now < self.retry_at {
            return;
        }
        self.retry_at = now + 2.0;
        // Minecraft's overlay file; we touch the reader's header words, the triple-buffer
        // state word (atomically) and the input ring, as the protocol assigns them.
        let Ok(map) = crate::link::open(NAME, TOTAL, false) else {
            return; // Minecraft creates it; not running yet
        };
        let word = |off: usize| u32::from_le_bytes(map[off..off + 4].try_into().unwrap());
        if word(0) != MAGIC || word(4) != VERSION {
            return;
        }
        info!("classiccraft: overlay link attached to {}", crate::link::describe(NAME));
        self.map = Some(map);
    }

    pub fn u32(&self, off: usize) -> u32 {
        let map = self.map.as_ref().expect("open");
        // SAFETY: in bounds; volatile, Minecraft writes it.
        unsafe { std::ptr::read_volatile(map.as_ptr().add(off).cast::<u32>()) }
    }

    pub fn put_u32(&mut self, off: usize, v: u32) {
        if let Some(map) = self.map.as_mut() {
            // SAFETY: in bounds; volatile, Minecraft reads it concurrently.
            unsafe { std::ptr::write_volatile(map.as_mut_ptr().add(off).cast::<u32>(), v) }
        }
    }

    /// One input event into the ring (we are its only writer).
    pub fn push(&mut self, kind: u16, code: u16, a: i32, b: i32) {
        let Some(map) = self.map.as_mut() else {
            return;
        };
        let base = map.as_mut_ptr();
        // SAFETY: the ring lies inside the mapped file.
        unsafe {
            let head_ptr = base.add(OFF_INPUT + RING_HEAD).cast::<u64>();
            let head = std::ptr::read_volatile(head_ptr);
            let e = base.add(OFF_INPUT + RING_DATA + ((head & (RING_ENTRIES - 1)) * 16) as usize);
            std::ptr::write_unaligned(e.cast::<u16>(), kind);
            std::ptr::write_unaligned(e.add(2).cast::<u16>(), code);
            std::ptr::write_unaligned(e.add(4).cast::<i32>(), a);
            std::ptr::write_unaligned(e.add(8).cast::<i32>(), b);
            std::ptr::write_unaligned(e.add(12).cast::<i32>(), 0);
            fence(Ordering::Release);
            std::ptr::write_volatile(head_ptr, head + 1);
        }
    }
}

/// The newest frame's shape, published by the render world for the main world (the overlay node's
/// size and flip, and the cursor space in [`crate::input`]); a frame behind, which is fine.
#[derive(Resource, Clone, Default)]
struct SharedFrame(Arc<Mutex<Option<FrameShape>>>);

#[derive(Clone, Copy, PartialEq)]
struct FrameShape {
    size: UVec2,
    bottom_up: bool,
}

#[derive(Resource, Default)]
struct OverlayView {
    node: Option<Entity>,
    image: Handle<Image>,
}

#[derive(Component)]
struct OverlayNode;

fn spawn_overlay(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut view: ResMut<OverlayView>,
) {
    view.image = images.add(blank(1, 1));
    let node = commands
        .spawn((
            OverlayNode,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                top: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
            ImageNode::new(view.image.clone()),
            GlobalZIndex(OVERLAY_Z),
            Pickable::IGNORE,
            Visibility::Hidden,
        ))
        .id();
    view.node = Some(node);
}

/// Minecraft's framebuffer bytes are sRGB-encoded; an sRGB texture lets benilla's gamma UI shader
/// (`ui_node_gamma.wgsl`: sampler decode, then `linear_to_srgb`) hand the same bytes back. No
/// pixel data: the render world writes the texture ([`upload_overlay`]), and an image without data
/// is never re-extracted (wgpu zero-fills a new texture: transparent).
fn blank(w: u32, h: u32) -> Image {
    Image::new_uninit(
        Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
    )
}

fn show_overlay(
    bridge: Res<Bridge>,
    mut file: ResMut<OverlayFile>,
    view: Res<OverlayView>,
    shared: Res<SharedFrame>,
    mut images: ResMut<Assets<Image>>,
    mut node: Query<(&mut ImageNode, &mut Visibility), With<OverlayNode>>,
) {
    let Ok((mut image_node, mut vis)) = node.single_mut() else {
        return;
    };
    let shown = bridge.driving() && file.is_open();
    vis.set_if_neq(if shown {
        Visibility::Visible
    } else {
        Visibility::Hidden
    });
    let Some(shape) = *shared.0.lock().unwrap() else {
        return;
    };
    file.frame_size = Some(shape.size);
    if image_node.flip_y != shape.bottom_up {
        image_node.flip_y = shape.bottom_up;
    }
    // A new size makes a new texture; until it is prepared the render world drops frames.
    let size = images.get(&view.image).map(|i| i.size());
    if size.is_some_and(|s| s != shape.size) {
        if let Some(image) = images.get_mut(&view.image) {
            *image = blank(shape.size.x, shape.size.y);
        }
    }
}

/// The render world's side of the overlay: its own mapping of the file, the triple buffer's
/// reader state, and the texture's current content.
#[derive(Resource, Default)]
struct OverlayUpload {
    map: Option<crate::link::SharedMap>,
    retry_at: Option<Instant>,
    /// Our front slot of the triple buffer, 2 until we take one (reset when the writer changes).
    front: usize,
    writer_pid: u32,
    shared: SharedFrame,
    /// From the main world this frame: the overlay is on screen, and its image.
    shown: bool,
    image: AssetId<Image>,
    /// Bands the texture holds something in (all, when unknown: a hidden spell or a new texture).
    texture_bands: u64,
    texture_size: UVec2,
    /// Un-premultiplied rows waiting for the queue, one whole frame at most.
    scratch: Vec<u8>,
}

fn extract_overlay(
    mut up: ResMut<OverlayUpload>,
    bridge: Extract<Res<Bridge>>,
    file: Extract<Res<OverlayFile>>,
    view: Extract<Res<OverlayView>>,
) {
    up.shown = bridge.driving() && file.is_open();
    up.image = view.image.id();
}

impl OverlayUpload {
    fn open(&mut self) {
        if self.map.is_some() || self.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        self.retry_at = Some(Instant::now() + Duration::from_secs(2));
        // Minecraft's overlay file; this side touches only the triple-buffer state word
        // (atomically) and reads the slot it owns, as the protocol assigns them.
        let Ok(map) = crate::link::open(NAME, TOTAL, false) else {
            return; // Minecraft creates it; not running yet
        };
        let word = |off: usize| u32::from_le_bytes(map[off..off + 4].try_into().unwrap());
        if word(0) != MAGIC || word(4) != VERSION {
            return;
        }
        self.map = Some(map);
        self.front = 2;
        self.writer_pid = 0;
    }
}

/// Take Minecraft's newest frame, if it published one, and write the bands that changed into the
/// overlay texture: those with content, un-premultiplied, and those that just emptied, as zeros.
/// The triple buffer's reader side: `state` swapped for our front slot whenever DIRTY is set.
/// Drained even while hidden, so the first shown frame is current.
fn upload_overlay(
    mut up: ResMut<OverlayUpload>,
    gpu_images: Res<RenderAssets<GpuImage>>,
    queue: Res<RenderQueue>,
) {
    up.open();
    let up = &mut *up;
    let Some(map) = up.map.as_mut() else {
        return;
    };
    let word = |map: &crate::link::SharedMap, off: usize| u32::from_le_bytes(map[off..off + 4].try_into().unwrap());
    let pid = word(map, OFF_WRITER_PID);
    if pid != up.writer_pid {
        up.writer_pid = pid;
        up.front = 2;
    }
    // SAFETY: `state` is a 4-aligned u32 in the mapping that both sides only touch atomically.
    let state = unsafe { &*map.as_ptr().add(OFF_STATE).cast::<AtomicU32>() };
    if state.load(Ordering::Acquire) & DIRTY == 0 {
        return;
    }
    up.front = (state.swap(up.front as u32, Ordering::AcqRel) & 3) as usize;
    let hdr = OFF_SLOT_HDR + up.front * SLOT_HDR_BYTES;
    let (w, h, flags) = (
        word(map, hdr) as usize,
        word(map, hdr + 4) as usize,
        word(map, hdr + 8),
    );
    if w == 0 || h == 0 || w > MAX_W || h > MAX_H {
        return;
    }
    let bands = if flags & FLAG_BANDS != 0 {
        u64::from_le_bytes(map[hdr + 24..hdr + 32].try_into().unwrap())
    } else {
        u64::MAX
    };
    let size = UVec2::new(w as u32, h as u32);
    *up.shared.0.lock().unwrap() = Some(FrameShape {
        size,
        bottom_up: flags & FLAG_BOTTOM_UP != 0,
    });
    let gpu = gpu_images
        .get(up.image)
        .filter(|g| g.size.width == size.x && g.size.height == size.y);
    let (true, Some(gpu)) = (up.shown, gpu) else {
        up.texture_bands = u64::MAX; // stale once we stop writing it
        return;
    };
    if up.texture_size != size {
        up.texture_size = size;
        up.texture_bands = u64::MAX;
    }
    let write = bands | up.texture_bands;
    up.texture_bands = bands;
    if write == 0 {
        return;
    }
    let start = OFF_PIXELS + up.front * SLOT_BYTES;
    let pixels = &map[start..start + w * h * 4];
    up.scratch.resize(w * h * 4, 0);
    // Bands in parallel: a 2560x1440 frame with the hand in view is ~5 MB to un-premultiply.
    let rows = |band: usize| (band * h / BANDS, (band + 1) * h / BANDS);
    ComputeTaskPool::get().scope(|s| {
        // Bands tile the rows in order, so each split takes exactly rows r0..r1.
        let mut rest = up.scratch.as_mut_slice();
        for band in 0..BANDS {
            let (r0, r1) = rows(band);
            let (dst, tail) = std::mem::take(&mut rest).split_at_mut((r1 - r0) * w * 4);
            rest = tail;
            if write & (1 << band) == 0 {
                continue;
            }
            if bands & (1 << band) == 0 {
                s.spawn(async move { dst.fill(0) });
                continue;
            }
            let src = &pixels[r0 * w * 4..r1 * w * 4];
            s.spawn(async move { unpremultiply(dst, src) });
        }
    });
    // One queue write per run of consecutive written bands.
    let mut band = 0;
    while band < BANDS {
        if write & (1 << band) == 0 {
            band += 1;
            continue;
        }
        let first = band;
        while band < BANDS && write & (1 << band) != 0 {
            band += 1;
        }
        let (r0, r1) = (rows(first).0, rows(band - 1).1);
        if r1 == r0 {
            continue;
        }
        queue.write_texture(
            TexelCopyTextureInfo {
                texture: &gpu.texture,
                mip_level: 0,
                origin: Origin3d {
                    x: 0,
                    y: r0 as u32,
                    z: 0,
                },
                aspect: TextureAspect::All,
            },
            &up.scratch[r0 * w * 4..r1 * w * 4],
            TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w as u32 * 4),
                rows_per_image: Some((r1 - r0) as u32),
            },
            Extent3d {
                width: w as u32,
                height: (r1 - r0) as u32,
                depth_or_array_layers: 1,
            },
        );
    }
}

/// Minecraft's pixels are premultiplied; benilla's UI blends straight alpha.
fn unpremultiply(dst: &mut [u8], src: &[u8]) {
    dst.copy_from_slice(src);
    for px in dst.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}
