//! Minecraft's world drawn inside benilla's: the mod streams geometry over
//! `protocol/mcwow_render_protocol.h` (the block atlas, 16³ block sections, entity textures, this
//! frame's entities and avatar, the selection box), and we draw it as ordinary world models with
//! benilla's own [`WowModelMaterial`], so WoW's depth hides it behind trees and walls and WoW's
//! sun, ambient and fog light it. While we read the ring, Minecraft stops drawing its own world
//! into the overlay, which then holds only the hand and HUD.
//!
//! Minecraft space to Bevy: `(-x, y, -z)·S` (see [`crate::bridge`]); a rotation, so winding holds.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::sync::atomic::{fence, Ordering};

use benilla_assets::materials::WowModelMaterial;
use benilla_formats::{FogPolicy, ModelBlend};
use benilla_world::lighting::{ExternalLight, SharedLightBuffer, WorldPointLight};
use benilla_world::model_render::{model_material, MaterialCache, ShadeSel};
use bevy::asset::RenderAssetUsages;
use bevy::camera::primitives::MeshAabb;
use bevy::camera::visibility::{NoAutoAabb, NoFrustumCulling};
use bevy::image::ImageSampler;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::{
    Extent3d, Origin3d, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureAspect,
    TextureDimension, TextureFormat,
};
use bevy::render::renderer::RenderQueue;
use bevy::render::texture::GpuImage;
use bevy::render::{Render, RenderApp, RenderSystems};
use memmap2::MmapMut;

use crate::bridge::S;

const PATH: &str = "/dev/shm/classiccraft_render_v1.shm";
const MAGIC: u32 = 0x6D63_7772; // "mcwr"
const VERSION: u32 = 1;
const OFF_WRITER_PID: usize = 8;
const OFF_READER_PID: usize = 12;
const OFF_READER_HEARTBEAT: usize = 16;
const OFF_RESEND: usize = 24;
const RING_OFF: usize = 256;
const RING_BYTES: usize = 64 << 20;
const RING_HEAD: usize = RING_OFF;
const RING_TAIL: usize = RING_OFF + 0x40;
const RING_DATA: usize = RING_OFF + 0x80;
const RING_DATA_BYTES: u64 = (RING_BYTES - 0x80) as u64;
const TOTAL: usize = RING_OFF + RING_BYTES;

const REN_PAD: u32 = 0;
const REN_ATLAS: u32 = 1;
const REN_SECTION: u32 = 2;
const REN_CLEAR_ALL: u32 = 3;
const REN_TEXTURE: u32 = 4;
const REN_AVATAR: u32 = 5;
const REN_SCENE: u32 = 6;
const REN_ATLAS_REGION: u32 = 7;
const REN_SELECTION: u32 = 8;
const REN_LIGHTS: u32 = 9;
const REN_HIT: u32 = 10;
const REN_EVENT: u32 = 11;
const REN_MOBS: u32 = 12;
/// Breakable terrain: one chunk's open columns, `i32 cx, cz` (region-local) and 8 `u32` of mask.
const REN_HOLES: u32 = 13;
/// An XP orb picked up (leveling, 2026-10-02): `u32 drop id, u32 XP`.
const REN_XP_CLAIM: u32 = 14;
/// A chat line typed in Minecraft for WoW (a `.` GM command): u32 length, UTF-8.
const REN_CHAT: u32 = 15;
/// Minecraft's right-click on the crosshair's WoW target (no payload).
const REN_INTERACT: u32 = 17;
/// A waygate travel: u32 map, f32 x, y, z, o (WoW yards, radians), u64 owner guid (28 bytes).
const REN_WAYGATE: u32 = 18;
/// A choice in an NPC window (2026-10-03): u64 npc, u8 kind, u8 action, u16 0, u32 arg
/// (`benilla_app::external_dialog::DialogIn`).
const REN_DIALOG: u32 = 19;
/// Steve respawned after a death in a WoW map (2026-10-04): u32 kind (0 = at the hearthstone
/// location, 1 = at the given spot: his bed), u32 map, f32 x, y, z, o (WoW) - `CMSG_CC_RESPAWN`'s body.
const REN_RESPAWN: u32 = 20;
/// An ore vein mined with a Minecraft pickaxe (2026-10-04): u64 the vein's guid - `CMSG_CC_HARVEST`'s body.
const REN_HARVEST: u32 = 21;
/// A light-level-15 block's reach (yd): Minecraft light fades over 15 blocks.
const MC_LIGHT_RANGE: f32 = 15.0 * S;

/// Bytes of ring consumed per frame at most, so a world load spreads over frames.
const FRAME_BUDGET: u64 = 24 << 20;
const VERTEX_BYTES: usize = 32;
const V_TRANSLUCENT: u32 = 1 << 1;

pub struct RenderLinkPlugin;

impl Plugin for RenderLinkPlugin {
    fn build(&self, app: &mut App) {
        let uploads = AtlasUploads::default();
        app.insert_resource(RenderLink {
            atlas_uploads: uploads.clone(),
            ..default()
        })
        .add_systems(
            Update,
            (consume_ring, shift_avatar, draw_selection)
                .chain()
                .after(benilla_app::external::ExternalDriveSet::Supply),
        );
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app.insert_resource(uploads).add_systems(
                Render,
                // After `prepare_assets::<GpuImage>`, so a new atlas has its texture.
                upload_atlas_regions.in_set(RenderSystems::PrepareResources),
            );
        }
    }
}

#[derive(Default)]
struct Section {
    /// Opaque/cutout and translucent parts, either may be absent.
    parts: Vec<Entity>,
}

/// See [`RenderLink::scene_at`].
const SCENE_STALE: f32 = 0.5;

/// One frame's dynamic mesh (entities or the avatar): reused entities, meshes rewritten in place.
#[derive(Default)]
struct Dynamic {
    parts: Vec<(Entity, Handle<Mesh>)>,
}

#[derive(Resource, Default)]
struct RenderLink {
    map: Option<MmapMut>,
    retry_at: f32,
    writer_pid: u32,
    atlas: Option<Handle<Image>>,
    textures: HashMap<u32, Handle<Image>>,
    sections: HashMap<[i32; 3], Section>,
    /// A section's light-emitting blocks, as benilla point lights (WoW's own lantern lights).
    lights: HashMap<[i32; 3], Vec<Entity>>,
    scene: Dynamic,
    /// When the last scene message arrived (s): Minecraft sends one every frame, so a scene left
    /// without one for [`SCENE_STALE`] is hidden rather than shown frozen (2026-10-03: picked-up
    /// drops stayed on screen, not spinning).
    scene_at: f32,
    scene_stale: bool,
    avatar: Dynamic,
    /// The avatar's origin as Minecraft last sent it (Bevy), before [`shift_avatar`].
    avatar_origin: Vec3,
    materials: MaterialCache,
    selection: Option<(Vec3, Vec3)>,
    logged_atlas: bool,
    atlas_uploads: AtlasUploads,
}

impl RenderLink {
    fn open(&mut self, now: f32) {
        if self.map.is_some() || now < self.retry_at {
            return;
        }
        self.retry_at = now + 2.0;
        let Ok(file) = OpenOptions::new().read(true).write(true).open(PATH) else {
            return;
        };
        if file.metadata().map(|m| m.len()).unwrap_or(0) < TOTAL as u64 {
            return;
        }
        // SAFETY: Minecraft's render file; we write only the reader's header words and the tail.
        let Ok(map) = (unsafe { MmapMut::map_mut(&file) }) else {
            return;
        };
        let word = |off: usize| u32::from_le_bytes(map[off..off + 4].try_into().unwrap());
        if word(0) != MAGIC || word(4) != VERSION {
            return;
        }
        info!("classiccraft: render link attached to {PATH}");
        self.map = Some(map);
        self.writer_pid = 0;
    }

    fn u32(&self, off: usize) -> u32 {
        let map = self.map.as_ref().unwrap();
        // SAFETY: in bounds; volatile, Minecraft writes it.
        unsafe { std::ptr::read_volatile(map.as_ptr().add(off).cast::<u32>()) }
    }
    fn u64(&self, off: usize) -> u64 {
        let map = self.map.as_ref().unwrap();
        // SAFETY: in bounds and 8-aligned.
        unsafe { std::ptr::read_volatile(map.as_ptr().add(off).cast::<u64>()) }
    }
    fn put_u32(&mut self, off: usize, v: u32) {
        let map = self.map.as_mut().unwrap();
        // SAFETY: as `u32`.
        unsafe { std::ptr::write_volatile(map.as_mut_ptr().add(off).cast::<u32>(), v) }
    }
    fn put_u64(&mut self, off: usize, v: u64) {
        let map = self.map.as_mut().unwrap();
        // SAFETY: as `u64`.
        unsafe { std::ptr::write_volatile(map.as_mut_ptr().add(off).cast::<u64>(), v) }
    }
}

/// Little-endian reads over a message payload.
struct Rd<'a>(&'a [u8]);
impl Rd<'_> {
    fn u32(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.0[o..o + 4].try_into().unwrap())
    }
    fn i32(&self, o: usize) -> i32 {
        self.u32(o) as i32
    }
    fn f32(&self, o: usize) -> f32 {
        f32::from_bits(self.u32(o))
    }
    fn f64(&self, o: usize) -> f64 {
        f64::from_le_bytes(self.0[o..o + 8].try_into().unwrap())
    }
}

enum Msg {
    Atlas {
        w: u32,
        h: u32,
        pixels: Vec<u8>,
    },
    AtlasRegion {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        pixels: Vec<u8>,
    },
    Section {
        at: [i32; 3],
        verts: Vec<u8>,
    },
    ClearAll,
    Texture {
        id: u32,
        w: u32,
        h: u32,
        pixels: Vec<u8>,
    },
    Mesh {
        avatar: bool,
        origin: [f64; 3],
        batches: Vec<[u32; 4]>,
        verts: Vec<u8>,
    },
    Selection(Option<(Vec3, Vec3)>),
    /// A section's emitters: block position (Minecraft, region-local), level 1-15, RGB8.
    Lights {
        at: [i32; 3],
        lights: Vec<([i32; 3], u8, [u8; 3])>,
    },
    Combat(crate::combat::McMsg),
    Holes {
        chunk: (i32, i32),
        mask: [u32; 8],
    },
}

#[allow(clippy::too_many_arguments)]
fn consume_ring(
    mut commands: Commands,
    time: Res<Time>,
    mut link: ResMut<RenderLink>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<WowModelMaterial>>,
    light: Res<SharedLightBuffer>,
    mut vis: Query<&mut Visibility>,
    mut inbox: ResMut<crate::combat::McInbox>,
    mut holes: ResMut<crate::holes::HoleMasks>,
) {
    consume_ring_inner(
        &mut commands,
        &time,
        &mut link,
        &mut images,
        &mut meshes,
        &mut materials,
        &light,
        &mut vis,
        &mut inbox,
        &mut holes,
    );
}

#[allow(clippy::too_many_arguments)]
fn consume_ring_inner(
    commands: &mut Commands,
    time: &Time,
    link: &mut RenderLink,
    images: &mut Assets<Image>,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<WowModelMaterial>,
    light: &SharedLightBuffer,
    vis: &mut Query<&mut Visibility>,
    inbox: &mut crate::combat::McInbox,
    holes: &mut crate::holes::HoleMasks,
) {
    let link = &mut *link;
    link.open(time.elapsed_secs());
    if link.map.is_none() {
        return;
    }
    // A new Minecraft: drop what the old one sent and ask the new one for everything.
    let pid = link.u32(OFF_WRITER_PID);
    if pid != link.writer_pid {
        link.writer_pid = pid;
        clear_all(commands, link);
        holes.clear();
        let head = link.u64(RING_HEAD);
        link.put_u64(RING_TAIL, head);
        let r = link.u32(OFF_RESEND).wrapping_add(1);
        link.put_u32(OFF_RESEND, r);
        link.put_u32(OFF_READER_PID, std::process::id());
    }
    let hb = link.u64(OFF_READER_HEARTBEAT).wrapping_add(1);
    link.put_u64(OFF_READER_HEARTBEAT, hb);

    // Copy this frame's messages out of the ring, then free the space.
    let mut msgs = Vec::new();
    {
        let map = link.map.as_ref().unwrap();
        // SAFETY: as `RenderLink::u64`.
        let rd64 =
            |off: usize| unsafe { std::ptr::read_volatile(map.as_ptr().add(off).cast::<u64>()) };
        let head = rd64(RING_HEAD);
        fence(Ordering::Acquire);
        let mut tail = rd64(RING_TAIL);
        let start = tail;
        while tail < head && tail - start < FRAME_BUDGET {
            let pos = (tail % RING_DATA_BYTES) as usize;
            let base = RING_DATA + pos;
            let kind = u32::from_le_bytes(map[base..base + 4].try_into().unwrap());
            if kind == REN_PAD {
                tail += RING_DATA_BYTES - pos as u64;
                continue;
            }
            let len = u32::from_le_bytes(map[base + 4..base + 8].try_into().unwrap()) as usize;
            if pos + 8 + len > RING_DATA_BYTES as usize {
                warn!("classiccraft: render ring message overruns the ring; resyncing");
                tail = head;
                break;
            }
            let p = Rd(&map[base + 8..base + 8 + len]);
            if let Some(m) = parse(kind, &p) {
                msgs.push(m);
            }
            tail += (8 + len as u64).next_multiple_of(8);
        }
        link.put_u64(RING_TAIL, tail);
    }

    for m in msgs {
        match m {
            Msg::Atlas { w, h, pixels } => {
                let image = rgba_image(w, h, pixels);
                match &link.atlas {
                    Some(handle) => {
                        if let Some(img) = images.get_mut(handle) {
                            *img = image;
                        }
                    }
                    None => link.atlas = Some(images.add(image)),
                }
                if !link.logged_atlas {
                    link.logged_atlas = true;
                    info!("classiccraft: Minecraft block atlas {w}x{h}");
                }
            }
            Msg::AtlasRegion { x, y, w, h, pixels } => {
                // Straight into the GPU texture ([`upload_atlas_regions`]): editing the 21 MB atlas
                // asset re-created its texture every tick, and a block section's material kept the
                // old one until the section was resent - water animated every few seconds.
                if let Some(atlas) = &link.atlas {
                    let mut q = link.atlas_uploads.0.lock().unwrap();
                    if q.len() >= MAX_PENDING_REGIONS {
                        q.remove(0);
                    }
                    q.push(AtlasRegion {
                        image: atlas.id(),
                        x,
                        y,
                        w,
                        h,
                        pixels,
                    });
                }
            }
            Msg::Texture { id, w, h, pixels } => {
                let image = rgba_image(w, h, pixels);
                match link.textures.get(&id) {
                    Some(handle) => {
                        if let Some(img) = images.get_mut(handle) {
                            *img = image;
                        }
                    }
                    None => {
                        let handle = images.add(image);
                        link.textures.insert(id, handle);
                    }
                }
            }
            Msg::ClearAll => {
                clear_all(commands, link);
                holes.clear();
            }
            Msg::Holes { chunk, mask } => holes.set(chunk, mask),
            Msg::Section { at, verts } => {
                if let Some(old) = link.sections.remove(&at) {
                    for e in old.parts {
                        commands.entity(e).despawn();
                    }
                }
                let Some(atlas) = link.atlas.clone() else {
                    continue;
                };
                if verts.is_empty() {
                    continue;
                }
                let origin = mc_to_bevy([
                    at[0] as f64 * 16.0,
                    at[1] as f64 * 16.0,
                    at[2] as f64 * 16.0,
                ]);
                let mut section = Section::default();
                for translucent in [false, true] {
                    let Some(mesh) =
                        build_mesh(&verts, |f| (f & V_TRANSLUCENT != 0) == translucent)
                    else {
                        continue;
                    };
                    // Bounds once, here: Bevy's `calculate_bounds` re-ran `compute_aabb` over
                    // thousands of these sections every frame (~30% of the main thread with the
                    // terrain fill, 2026-10-02), though a section's mesh never changes.
                    let aabb = mesh.compute_aabb().unwrap_or_default();
                    let mat = material(link, materials, light, Some(atlas.clone()), translucent, false, false);
                    let e = commands
                        .spawn((
                            aabb,
                            NoAutoAabb,
                            Mesh3d(meshes.add(mesh)),
                            MeshMaterial3d(mat),
                            Transform::from_translation(origin),
                        ))
                        .id();
                    section.parts.push(e);
                }
                link.sections.insert(at, section);
            }
            Msg::Mesh {
                avatar,
                origin,
                batches,
                verts,
            } => {
                let origin = mc_to_bevy(origin);
                if avatar {
                    link.avatar_origin = origin;
                } else {
                    link.scene_at = time.elapsed_secs();
                    link.scene_stale = false;
                }
                let mut used = 0;
                for [texture, first, count, flags] in batches {
                    let (first, count) = (first as usize, count as usize);
                    if count == 0 || (first + count) * VERTEX_BYTES > verts.len() {
                        continue;
                    }
                    let tex = if texture == 0 {
                        link.atlas.clone()
                    } else {
                        link.textures.get(&texture).cloned()
                    };
                    let Some(tex) = tex else {
                        continue;
                    };
                    let slice = &verts[first * VERTEX_BYTES..(first + count) * VERTEX_BYTES];
                    let Some(mesh) = build_mesh(slice, |_| true) else {
                        continue;
                    };
                    let translucent = flags & 1 != 0;
                    // Batch flag 2: a no-cull render type (an elytra's wings), drawn from both sides.
                    let two_sided = flags & 2 != 0;
                    let mat = material(link, materials, light, Some(tex), translucent, two_sided, false);
                    let dynamic = if avatar {
                        &mut link.avatar
                    } else {
                        &mut link.scene
                    };
                    if let Some((e, handle)) = dynamic.parts.get(used).cloned() {
                        if let Some(m) = meshes.get_mut(&handle) {
                            *m = mesh;
                        }
                        commands.entity(e).insert((
                            MeshMaterial3d(mat),
                            Transform::from_translation(origin),
                            Visibility::Inherited,
                        ));
                    } else {
                        let handle = meshes.add(mesh);
                        let e = commands
                            .spawn((
                                Mesh3d(handle.clone()),
                                MeshMaterial3d(mat),
                                Transform::from_translation(origin),
                                NoFrustumCulling,
                            ))
                            .id();
                        dynamic.parts.push((e, handle));
                    }
                    used += 1;
                }
                // Hide the parts this frame did not use.
                let dynamic = if avatar { &link.avatar } else { &link.scene };
                for (e, _) in dynamic.parts.iter().skip(used) {
                    if let Ok(mut v) = vis.get_mut(*e) {
                        v.set_if_neq(Visibility::Hidden);
                    }
                }
            }
            Msg::Selection(sel) => link.selection = sel,
            Msg::Combat(m) => inbox.0.push(m),
            Msg::Lights { at, lights } => {
                for e in link.lights.remove(&at).unwrap_or_default() {
                    commands.entity(e).despawn();
                }
                let spawned: Vec<Entity> = lights
                    .into_iter()
                    .map(|(b, level, rgb)| {
                        let centre =
                            mc_to_bevy([b[0] as f64 + 0.5, b[1] as f64 + 0.5, b[2] as f64 + 0.5]);
                        let lin = |c: u8| Color::srgb_u8(c, c, c).to_linear().red;
                        commands
                            .spawn((
                                WorldPointLight {
                                    color: [lin(rgb[0]), lin(rgb[1]), lin(rgb[2])],
                                    // benilla's convention: 4π × the authored intensity.
                                    intensity: 4.0 * std::f32::consts::PI * f32::from(level) / 15.0,
                                    range: MC_LIGHT_RANGE.max(f32::from(level) * S),
                                },
                                ExternalLight,
                                Transform::from_translation(centre),
                            ))
                            .id()
                    })
                    .collect();
                if !spawned.is_empty() {
                    link.lights.insert(at, spawned);
                }
            }
        }
    }
    // No scene for a while (Minecraft stopped sending it): hide it rather than show it frozen.
    if !link.scene_stale && time.elapsed_secs() - link.scene_at > SCENE_STALE {
        link.scene_stale = true;
        let shown = link.scene.parts.len();
        for (e, _) in &link.scene.parts {
            if let Ok(mut v) = vis.get_mut(*e) {
                v.set_if_neq(Visibility::Hidden);
            }
        }
        if shown > 0 {
            info!("classiccraft: no Minecraft scene for {SCENE_STALE} s - entities hidden");
        }
    }
}

fn parse(kind: u32, p: &Rd) -> Option<Msg> {
    let len = p.0.len();
    match kind {
        REN_ATLAS if len >= 8 => {
            let (w, h) = (p.u32(0), p.u32(4));
            let n = (w * h * 4) as usize;
            (len >= 8 + n).then(|| Msg::Atlas {
                w,
                h,
                pixels: p.0[8..8 + n].to_vec(),
            })
        }
        REN_ATLAS_REGION if len >= 16 => {
            let (x, y, w, h) = (p.u32(0), p.u32(4), p.u32(8), p.u32(12));
            let n = (w * h * 4) as usize;
            (len >= 16 + n).then(|| Msg::AtlasRegion {
                x,
                y,
                w,
                h,
                pixels: p.0[16..16 + n].to_vec(),
            })
        }
        REN_SECTION if len >= 16 => {
            let at = [p.i32(0), p.i32(4), p.i32(8)];
            let n = p.u32(12) as usize * VERTEX_BYTES;
            (len >= 16 + n).then(|| Msg::Section {
                at,
                verts: p.0[16..16 + n].to_vec(),
            })
        }
        REN_CLEAR_ALL => Some(Msg::ClearAll),
        REN_TEXTURE if len >= 16 => {
            let (id, w, h) = (p.u32(0), p.u32(4), p.u32(8));
            let n = (w * h * 4) as usize;
            (len >= 16 + n).then(|| Msg::Texture {
                id,
                w,
                h,
                pixels: p.0[16..16 + n].to_vec(),
            })
        }
        REN_AVATAR | REN_SCENE if len >= 32 => {
            let origin = [p.f64(0), p.f64(8), p.f64(16)];
            let (nb, nv) = (p.u32(24) as usize, p.u32(28) as usize);
            let vstart = 32 + nb * 16;
            if len < vstart + nv * VERTEX_BYTES {
                return None;
            }
            let batches = (0..nb)
                .map(|i| {
                    let o = 32 + i * 16;
                    [p.u32(o), p.u32(o + 4), p.u32(o + 8), p.u32(o + 12)]
                })
                .collect();
            Some(Msg::Mesh {
                avatar: kind == REN_AVATAR,
                origin,
                batches,
                verts: p.0[vstart..vstart + nv * VERTEX_BYTES].to_vec(),
            })
        }
        REN_LIGHTS if len >= 16 => {
            let at = [p.i32(0), p.i32(4), p.i32(8)];
            let n = p.u32(12) as usize;
            if len < 16 + n * 8 {
                return None;
            }
            let lights = (0..n)
                .map(|i| {
                    let o = 16 + i * 8;
                    let b = &p.0[o..o + 8];
                    let block = [
                        at[0] * 16 + b[0] as i32,
                        at[1] * 16 + b[1] as i32,
                        at[2] * 16 + b[2] as i32,
                    ];
                    (block, b[3], [b[4], b[5], b[6]])
                })
                .collect();
            Some(Msg::Lights { at, lights })
        }
        REN_SELECTION if len >= 32 => Some(Msg::Selection((p.u32(0) != 0).then(|| {
            let a = mc_to_bevy([p.f32(8) as f64, p.f32(12) as f64, p.f32(16) as f64]);
            let b = mc_to_bevy([p.f32(20) as f64, p.f32(24) as f64, p.f32(28) as f64]);
            (a.min(b), a.max(b))
        }))),
        REN_HIT if len >= 24 => Some(Msg::Combat(crate::combat::McMsg::Hit {
            guid: u64::from_le_bytes(p.0[0..8].try_into().unwrap()),
            wow_damage: p.u32(8),
            flags: p.u32(12),
            attacker: p.u32(16),
        })),
        REN_HOLES if len >= 40 => Some(Msg::Holes {
            chunk: (p.i32(0), p.i32(4)),
            mask: std::array::from_fn(|k| p.u32(8 + k * 4)),
        }),
        REN_EVENT if len >= 4 => Some(Msg::Combat(crate::combat::McMsg::Event(p.u32(0)))),
        REN_INTERACT => Some(Msg::Combat(crate::combat::McMsg::Interact)),
        REN_WAYGATE if len >= 28 => Some(Msg::Combat(crate::combat::McMsg::Waygate(p.0[..28].to_vec()))),
        REN_RESPAWN if len >= 24 => Some(Msg::Combat(crate::combat::McMsg::Respawn(p.0[..24].to_vec()))),
        REN_HARVEST if len >= 8 => Some(Msg::Combat(crate::combat::McMsg::Harvest(p.0[..8].to_vec()))),
        REN_DIALOG if len >= 16 => Some(Msg::Combat(crate::combat::McMsg::Dialog(
            benilla_app::external_dialog::DialogIn {
                npc: u64::from_le_bytes(p.0[0..8].try_into().unwrap()),
                kind: p.0[8],
                action: p.0[9],
                arg: p.u32(12),
            },
        ))),
        REN_CHAT if len >= 4 && len >= 4 + p.u32(0) as usize => {
            let n = p.u32(0) as usize;
            Some(Msg::Combat(crate::combat::McMsg::Chat(
                String::from_utf8_lossy(&p.0[4..4 + n]).into_owned(),
            )))
        }
        REN_XP_CLAIM if len >= 8 => Some(Msg::Combat(crate::combat::McMsg::XpClaim {
            drop: p.u32(0),
            xp: p.u32(4),
        })),
        REN_MOBS if len >= 8 => {
            let n = p.u32(0) as usize;
            if len < 8 + n * 24 {
                return None;
            }
            let mobs = (0..n)
                .map(|i| {
                    let o = 8 + i * 24;
                    crate::combat::McMob {
                        id: p.u32(o),
                        kind: p.0[o + 4],
                        hp_pct: p.0[o + 5],
                        pos: [p.f32(o + 8), p.f32(o + 12), p.f32(o + 16)],
                        yaw_deg: p.f32(o + 20),
                    }
                })
                .collect();
            Some(Msg::Combat(crate::combat::McMsg::Mobs(mobs)))
        }
        _ => None,
    }
}

fn mc_to_bevy(p: [f64; 3]) -> Vec3 {
    Vec3::new(
        (-p[0] * S as f64) as f32,
        (p[1] * S as f64) as f32,
        (-p[2] * S as f64) as f32,
    )
}

fn clear_all(commands: &mut Commands, link: &mut RenderLink) {
    for (_, ls) in link.lights.drain() {
        for e in ls {
            commands.entity(e).despawn();
        }
    }
    for (_, s) in link.sections.drain() {
        for e in s.parts {
            commands.entity(e).despawn();
        }
    }
    for (e, _) in link
        .scene
        .parts
        .drain(..)
        .chain(link.avatar.parts.drain(..))
    {
        commands.entity(e).despawn();
    }
}

/// Gamma bytes, as benilla's world art (`blp.rs` `GAMMA_BYTES`); nearest filtering for pixel art.
fn rgba_image(w: u32, h: u32, pixels: Vec<u8>) -> Image {
    let mut image = Image::new(
        Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        pixels,
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
    );
    image.sampler = ImageSampler::nearest();
    image
}

/// Animated atlas frames waiting for the render world: shared between both worlds.
#[derive(Resource, Clone, Default)]
struct AtlasUploads(std::sync::Arc<std::sync::Mutex<Vec<AtlasRegion>>>);

struct AtlasRegion {
    image: AssetId<Image>,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    pixels: Vec<u8>,
}

/// More than a few frames' worth behind (no atlas texture yet): the oldest go.
const MAX_PENDING_REGIONS: usize = 1024;

/// Writes Minecraft's animated texture frames (water, lava, fire...) into the atlas texture; a
/// region whose texture is not prepared yet waits for the next frame.
fn upload_atlas_regions(
    uploads: Res<AtlasUploads>,
    gpu_images: Res<RenderAssets<GpuImage>>,
    queue: Res<RenderQueue>,
) {
    let mut pending = uploads.0.lock().unwrap();
    pending.retain(|r| {
        let Some(gpu) = gpu_images.get(r.image) else {
            return true;
        };
        let fits = r.x + r.w <= gpu.size.width
            && r.y + r.h <= gpu.size.height
            && r.pixels.len() >= (r.w * r.h * 4) as usize;
        if fits {
            queue.write_texture(
                TexelCopyTextureInfo {
                    texture: &gpu.texture,
                    mip_level: 0,
                    origin: Origin3d { x: r.x, y: r.y, z: 0 },
                    aspect: TextureAspect::All,
                },
                &r.pixels[..(r.w * r.h * 4) as usize],
                TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(r.w * 4),
                    rows_per_image: Some(r.h),
                },
                Extent3d {
                    width: r.w,
                    height: r.h,
                    depth_or_array_layers: 1,
                },
            );
        }
        false
    });
}

fn material(
    link: &mut RenderLink,
    materials: &mut Assets<WowModelMaterial>,
    light: &SharedLightBuffer,
    texture: Option<Handle<Image>>,
    translucent: bool,
    two_sided: bool,
    no_depth_write: bool,
) -> Handle<WowModelMaterial> {
    let handle = base_material(link, materials, light, texture, translucent, two_sided, no_depth_write);
    // Per-vertex point-light anchor (`wow_model.wgsl`, `clutter_fade.z` bit 14, fork only): a
    // section's torches light the blocks around them, not just those near its corner.
    benilla_world::model_render::lazy::with_material_mut(materials, handle.id(), |m| {
        let markers = m.extension.clutter_fade.z as u32 | VERTEX_LIGHT_ANCHOR;
        m.extension.clutter_fade.z = markers as f32;
    });
    handle
}

/// `clutter_fade.z` bit 14: anchor point-light selection at each vertex.
const VERTEX_LIGHT_ANCHOR: u32 = 1 << 14;

fn base_material(
    link: &mut RenderLink,
    materials: &mut Assets<WowModelMaterial>,
    light: &SharedLightBuffer,
    texture: Option<Handle<Image>>,
    translucent: bool,
    two_sided: bool,
    no_depth_write: bool,
) -> Handle<WowModelMaterial> {
    model_material(
        &mut link.materials,
        materials,
        texture,
        if translucent {
            ModelBlend::Blend
        } else {
            ModelBlend::AlphaTest
        },
        // One-sided: Minecraft sends each visible side as its own face (a plant's cross is four),
        // relying on back-face culling; two-sided drew both at one depth and they z-fought. Only
        // entity parts of Minecraft's no-cull render types are two-sided.
        two_sided,
        false, // an M2, not a WMO: the vertex colour (tint · AO) folds into the albedo
        false,
        false,
        false,
        false,
        // Depth on for every Minecraft mesh: a clear pixel is discarded instead (`wow_model.wgsl`,
        // bit 14) - without depth a translucent skin drew its head behind its body (2026-10-02).
        no_depth_write,
        false,
        FogPolicy::Scene,
        false,
        ShadeSel::Lit,
        0,
        None,
        None,
        None,
        None,
        false,
        false,
        &light.0,
        None,
    )
}

/// A triangle-list mesh from `McwowRenVertex`es whose flags pass `keep`, per whole triangle.
fn build_mesh(verts: &[u8], keep: impl Fn(u32) -> bool) -> Option<Mesh> {
    let n = verts.len() / VERTEX_BYTES;
    let v = Rd(verts);
    let (mut pos, mut nor, mut uv, mut col) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut block = Vec::new();
    for t in 0..n / 3 {
        let base = t * 3 * VERTEX_BYTES;
        if !keep(v.u32(base + 28)) {
            continue;
        }
        let p: [Vec3; 3] = std::array::from_fn(|k| {
            let o = base + k * VERTEX_BYTES;
            Vec3::new(-v.f32(o) * S, v.f32(o + 4) * S, -v.f32(o + 8) * S)
        });
        let face_n = (p[1] - p[0]).cross(p[2] - p[0]).normalize_or(Vec3::Y);
        for (k, pk) in p.iter().enumerate() {
            let o = base + k * VERTEX_BYTES;
            let flags = v.u32(o + 28);
            pos.push(pk.to_array());
            nor.push(
                direction_normal((flags >> 4) & 7)
                    .unwrap_or(face_n)
                    .to_array(),
            );
            uv.push([v.f32(o + 12), v.f32(o + 16)]);
            let rgba = &verts[o + 20..o + 24];
            let light = v.u32(o + 24);
            // Block light rides `uv_b.x`, added as Minecraft's warm torch light, and sky light
            // `uv_b.y`, which scales only WoW's sun and ambient (roofs and caves stay dark under
            // WoW's sun) - fork shader, `wow_model.wgsl`. Both on Minecraft's brightness curve.
            // Sky light used to dim the albedo itself, so a torch deep down lit near-black.
            block.push([
                brightness(light & 0xF),
                brightness((light >> 8) & 0xF).max(0.04),
            ]);
            col.push([
                rgba[0] as f32 / 255.0,
                rgba[1] as f32 / 255.0,
                rgba[2] as f32 / 255.0,
                rgba[3] as f32 / 255.0,
            ]);
        }
    }
    if pos.is_empty() {
        return None;
    }
    let count = pos.len() as u32;
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, nor);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, col);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, block);
    mesh.insert_indices(Indices::U32((0..count).collect()));
    Some(mesh)
}

/// Minecraft's light level 0-15 to brightness (its lightmap curve with no ambient).
fn brightness(level: u32) -> f32 {
    let f = level.min(15) as f32 / 15.0;
    f / (4.0 - 3.0 * f)
}

/// Minecraft `Direction` ordinal + 1 (DOWN, UP, NORTH, SOUTH, WEST, EAST) to a Bevy normal.
fn direction_normal(code: u32) -> Option<Vec3> {
    let mc = match code {
        1 => Vec3::NEG_Y,
        2 => Vec3::Y,
        3 => Vec3::NEG_Z,
        4 => Vec3::Z,
        5 => Vec3::NEG_X,
        6 => Vec3::X,
        _ => return None,
    };
    Some(Vec3::new(-mc.x, mc.y, -mc.z))
}

/// The targeted block's outline, depth-tested like the reference selection box.
/// Aboard a moving deck the camera is composed with benilla's live deck pose
/// ([`crate::decks::RiderShift`]); Steve's own mesh, at Minecraft's world position, trailed it by up to
/// a tick and flickered in third person (2026-10-02). It takes the same shift.
fn shift_avatar(
    link: Res<RenderLink>,
    shift: Res<crate::decks::RiderShift>,
    mut transforms: Query<&mut Transform>,
) {
    let at = link.avatar_origin + shift.0;
    for (e, _) in &link.avatar.parts {
        if let Ok(mut t) = transforms.get_mut(*e) {
            if t.translation != at {
                t.translation = at;
            }
        }
    }
}

fn draw_selection(link: Res<RenderLink>, mut gizmos: Gizmos) {
    if let Some((min, max)) = link.selection {
        let centre = (min + max) * 0.5;
        let size = (max - min) * 1.002;
        gizmos.cube(
            Transform::from_translation(centre).with_scale(size),
            Color::srgba(0.0, 0.0, 0.0, 0.6),
        );
    }
}

