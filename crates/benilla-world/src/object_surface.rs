//! classiccraft (fork only): which placed object a body stands on, for footsteps on bridges, docks,
//! porches and steps outdoors. WoW's own answer ([`crate::surface`]) covers a building's interior
//! groups only, and doodad hulls weld per tile into one collider, so outside both read the terrain
//! under the object. Here a short down-ray asks the body's own world what it hits, and the hit
//! collider's [`SurfaceSource`] names the object: the weld's hull whose bounds hold the hit point,
//! or a WMO's walking face and its MOPY material.

use std::sync::Arc;

use bevy::ecs::system::SystemParam;
use bevy::math::Affine3A;
use bevy::prelude::*;

use benilla_assets::coords::bevy_to_wow;
use benilla_assets::WmoModel;

use crate::collision::WorldCollision;

/// How far over the feet the down-ray starts (yd): a foot sunk a little into a step still finds it.
const RAY_LIFT: f32 = 0.5;
/// How far under the feet a surface still counts as stood on (yd).
const RAY_REACH: f32 = 0.6;
/// The slack around a hull's bounds and a face's height (yd): the hit point lies on the surface.
const SLACK: f32 = 0.05;

/// What a world collider is made of, for [`ObjectUnderfoot`]; terrain and GameObjects carry none.
#[derive(Component, Clone)]
pub enum SurfaceSource {
    /// Doodad hulls (one, or a weld of many): each hull's world bounds and model path.
    Hulls(Arc<[NamedHull]>),
    /// A WMO's walking collider.
    Wmo {
        handle: Handle<WmoModel>,
        /// Placement transform, Bevy model→world, as [`crate::wmo_portal::WmoPortalInstance`].
        world_from_local: Affine3A,
        model: Arc<str>,
    },
}

/// One doodad hull inside a [`SurfaceSource::Hulls`]: its world AABB (Bevy space) and model path.
#[derive(Clone)]
pub struct NamedHull {
    pub min: Vec3,
    pub max: Vec3,
    pub model: Arc<str>,
}

impl NamedHull {
    /// The hull of `verts` (world space) for `model`.
    pub fn of(model: Arc<str>, verts: &[Vec3]) -> Self {
        let (min, max) = verts.iter().fold(
            (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
            |(lo, hi), v| (lo.min(*v), hi.max(*v)),
        );
        Self { min, max, model }
    }

    fn holds(&self, p: Vec3) -> bool {
        p.cmpge(self.min - SLACK).all() && p.cmple(self.max + SLACK).all()
    }

    fn volume(&self) -> f32 {
        (self.max - self.min).max(Vec3::ZERO).element_product()
    }
}

/// The object under a body's feet.
#[derive(Debug, Clone)]
pub enum ObjectSurface {
    /// A doodad (M2): its model path.
    Doodad { model: Arc<str> },
    /// A building's walking face: its MOMT ground type and texture when the face has a material.
    Wmo {
        model: Arc<str>,
        ground_type: Option<u32>,
        texture: Option<String>,
    },
}

/// The world asked what object lies under a point.
#[derive(SystemParam)]
pub struct ObjectUnderfoot<'w, 's> {
    collision: WorldCollision<'w, 's>,
    sources: Query<'w, 's, &'static SurfaceSource>,
    wmos: Res<'w, Assets<WmoModel>>,
}

impl ObjectUnderfoot<'_, '_> {
    /// The object whose surface lies within reach under `feet` (Bevy space); `None` over terrain,
    /// over a GameObject, or in the air.
    pub fn object_under(&self, feet: Vec3) -> Option<ObjectSurface> {
        let origin = feet + Vec3::Y * RAY_LIFT;
        let hit = self
            .collision
            .ray_body(origin, Dir3::NEG_Y, RAY_LIFT + RAY_REACH)?;
        let point = origin - Vec3::Y * hit.distance;
        match self.sources.get(hit.entity).ok()? {
            SurfaceSource::Hulls(hulls) => hulls
                .iter()
                .filter(|h| h.holds(point))
                .min_by(|a, b| a.volume().total_cmp(&b.volume()))
                .map(|h| ObjectSurface::Doodad {
                    model: h.model.clone(),
                }),
            SurfaceSource::Wmo {
                handle,
                world_from_local,
                model,
            } => {
                let wmo = self.wmos.get(handle)?;
                let local = bevy_to_wow(world_from_local.inverse().transform_point3(point));
                let (group, material) = wmo_face_material(wmo, local)?;
                let material = Some(material).filter(|&m| m != 0xFF);
                let ground_type = material
                    .and_then(|m| wmo.material_ground_type.get(usize::from(m)))
                    .copied();
                // A collision-only face (a ramp over stairs, 2026-10-02 Kharanos inn steps) has no
                // material: the visible face under it in the same group names the floor.
                let texture = match material {
                    Some(m) => wmo
                        .material_texture
                        .get(usize::from(m))
                        .filter(|t| !t.is_empty())
                        .cloned(),
                    None => wmo_render_texture(wmo, group, local),
                };
                Some(ObjectSurface::Wmo {
                    model: model.clone(),
                    ground_type,
                    texture,
                })
            }
        }
    }
}

/// The group and MOPY material (`0xFF` collision-only) of the walking face through `p` (WMO-local
/// WoW space): the face whose height over `p`'s column is nearest `p`, within [`SLACK`].
fn wmo_face_material(wmo: &WmoModel, p: [f32; 3]) -> Option<(usize, u8)> {
    let mut best: Option<(f32, usize, u8)> = None;
    for (gi, tris) in wmo.group_collision_tris.iter().enumerate() {
        let Some(mats) = wmo
            .group_collision_materials
            .get(gi)
            .filter(|m| m.len() == tris.len())
        else {
            continue;
        };
        if let Some(Some((min, max))) = wmo.group_collision_bounds.get(gi) {
            if p[0] < min[0] - SLACK
                || p[0] > max[0] + SLACK
                || p[1] < min[1] - SLACK
                || p[1] > max[1] + SLACK
                || p[2] < min[2] - SLACK
                || p[2] > max[2] + SLACK
            {
                continue;
            }
        }
        for (tri, &mat) in tris.iter().zip(mats) {
            let Some(z) = height_over(*tri, p) else { continue };
            let dz = (z - p[2]).abs();
            if dz <= SLACK && best.is_none_or(|(d, ..)| dz < d) {
                best = Some((dz, gi, mat));
            }
        }
    }
    best.map(|(_, g, m)| (g, m))
}

/// How far under a collision-only face its visible floor may lie (yd): a ramp over steps rides
/// their nosings, a step's tread lies up to a riser below.
const RENDER_BELOW: f32 = 0.8;
/// How far over it (yd): the ramp may cut under a tread's back edge.
const RENDER_ABOVE: f32 = 0.3;

/// The texture of `group`'s visible face nearest over or under `p` (WMO-local WoW space).
fn wmo_render_texture(wmo: &WmoModel, group: usize, p: [f32; 3]) -> Option<String> {
    let mut best: Option<(f32, &str)> = None;
    for (sub, &g) in wmo.submeshes.iter().zip(&wmo.submesh_group) {
        if usize::from(g) != group {
            continue;
        }
        let geo = &sub.geometry;
        let Some(texture) = geo.texture.as_deref() else { continue };
        for t in geo.indices.as_chunks::<3>().0 {
            let (Some(&a), Some(&b), Some(&c)) = (
                geo.positions.get(t[0] as usize),
                geo.positions.get(t[1] as usize),
                geo.positions.get(t[2] as usize),
            ) else {
                continue;
            };
            let Some(z) = height_over([a, b, c], p) else { continue };
            let dz = z - p[2];
            if (-RENDER_BELOW..=RENDER_ABOVE).contains(&dz) && best.is_none_or(|(d, _)| dz.abs() < d) {
                best = Some((dz.abs(), texture));
            }
        }
    }
    best.map(|(_, t)| t.to_owned())
}

/// The height of triangle `[a, b, c]` over `p`'s column, when the column passes through it; `None`
/// for a wall, which has no height over a column.
fn height_over([a, b, c]: [[f32; 3]; 3], p: [f32; 3]) -> Option<f32> {
    let det = (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1]);
    if det.abs() < 1e-9 {
        return None;
    }
    let wb = ((p[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (p[1] - a[1])) / det;
    let wc = ((b[0] - a[0]) * (p[1] - a[1]) - (p[0] - a[0]) * (b[1] - a[1])) / det;
    let wa = 1.0 - wb - wc;
    (wa >= -1e-4 && wb >= -1e-4 && wc >= -1e-4).then(|| wa * a[2] + wb * b[2] + wc * c[2])
}
