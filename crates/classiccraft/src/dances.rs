//! WoW's race dances for Minecraft's Steve (2026-10-03, user: "/dance tauren" in Minecraft, the
//! Minecraft character dancing). Each race and gender's character model is read from the user's own
//! install, its `/dance` animation (`AnimationData` 69) posed bone by bone as benilla poses a rig
//! (a joint at each pivot, offset from its parent's, its keyed translation, rotation and scale),
//! and the pose turned into Steve's six parts at 30 frames a second:
//!
//! - the torso frame, from the shoulders (the arm roots) and hips (the leg roots), turns and moves
//!   the whole model (Minecraft's `root` part), against its frame in the Stand pose;
//! - each arm points from its shoulder to its hand, each leg from its hip to its foot (Steve has
//!   no elbows or knees: the limb takes where its end goes, the bend is lost);
//! - the head turns as the head bone does against the torso.
//!
//! Written to `~/.local/share/classiccraft/dances/<race>_<sex>.ccd`, where the Fabric mod
//! (`McwowDance`) reads them; nothing of the client's data leaves the user's machine. Generated
//! once (a missing file is made again).
//!
//! A race's dance is several variations, which WoW picks among by weight and repeats a rolled
//! number of times each (the user, 2026-10-03: "at least 2 animations that randomly interchange").
//!
//! File (little-endian): `CCDN`, u32 version (2), u32 variations, then per variation: u32 weight
//! (`frequency`), u32 min replay, u32 max replay, u32 frames, f32 fps, and per frame 17 f32: root
//! offset x, y, z (model pixels, Minecraft model axes: +x left, +y down, +z back), root rotation
//! x, y, z (radians, Minecraft's ZYX order), head x, y, z, right arm x, z, left arm x, z, right
//! leg x, z, left leg x, z.

use std::collections::HashMap;
use std::path::PathBuf;

use benilla_formats::{parse_m2_animations, parse_m2_attachments, parse_m2_skeleton};
use bevy::math::{Affine3A, Mat3, Quat, Vec3};
use bevy::prelude::*;

const MAGIC: &[u8; 4] = b"CCDN";
const VERSION: u32 = 2;
const FPS: f32 = 30.0;
const ANIM_STAND: u16 = 0;
const ANIM_DANCE: u16 = 69;
/// Attachment ids: right and left hand.
const ATTACH_HAND_R: u16 = 1;
const ATTACH_HAND_L: u16 = 2;
/// KeyBoneIDs: left and right upper arm, head.
const KEY_ARM_L: i16 = 0;
const KEY_ARM_R: i16 = 1;
const KEY_HEAD: i16 = 6;
const KEY_WAIST: i16 = 5;
/// Steve's hip height (model pixels): WoW's motion is scaled by it against the model's own.
const STEVE_HIP_PX: f32 = 12.0;

/// The races and their model folders, `ChrRaces` ids 1..8.
pub const RACES: [(u8, &str); 8] = [
    (1, "Human"),
    (2, "Orc"),
    (3, "Dwarf"),
    (4, "NightElf"),
    (5, "Scourge"),
    (6, "Tauren"),
    (7, "Gnome"),
    (8, "Troll"),
];

pub struct DancesPlugin;

impl Plugin for DancesPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            write_self.in_set(benilla_app::external::ExternalDriveSet::Report),
        );
        // Off the main thread: sixteen models, a second or two, once.
        std::thread::spawn(|| {
            if let Err(e) = generate_missing() {
                warn!("classiccraft: dances: {e}");
            }
        });
    }
}

/// Our character's race and gender into `dances/self` ("race sex"), for `/dance` with no race.
fn write_self(report: Res<benilla_app::external::SelfReport>, mut written: Local<Option<(u8, u8)>>) {
    let Some(rs) = report.race_sex.filter(|_| report.in_world) else {
        return;
    };
    if *written == Some(rs) {
        return;
    }
    *written = Some(rs);
    if let Some(dir) = dir() {
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = std::fs::write(dir.join("self"), format!("{} {}\n", rs.0, rs.1)) {
            warn!("classiccraft: dances/self: {e}");
        }
    }
}

pub fn dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/classiccraft/dances"))
}

/// Writes every race and gender's dance file that is not there yet.
pub fn generate_missing() -> anyhow::Result<()> {
    let dir = dir().ok_or_else(|| anyhow::anyhow!("no $HOME"))?;
    std::fs::create_dir_all(&dir)?;
    let wanted: Vec<(u8, &str, u8)> = RACES
        .iter()
        .flat_map(|&(id, folder)| [(id, folder, 0u8), (id, folder, 1u8)])
        .filter(|&(id, _, sex)| !current(&dir.join(format!("{id}_{sex}.ccd"))))
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let data = benilla_formats::wow_data().ok_or_else(|| anyhow::anyhow!("no WoW install found"))?;
    let mut chain = benilla_formats::open_chain(&data)?;
    for (id, folder, sex) in wanted {
        let gender = if sex == 0 { "Male" } else { "Female" };
        let path = format!("character\\{folder}\\{gender}\\{folder}{gender}.m2").to_ascii_lowercase();
        match chain.read_file(&path).map_err(anyhow::Error::from).and_then(|b| extract(&b)) {
            Ok(variations) => {
                std::fs::write(dir.join(format!("{id}_{sex}.ccd")), encode(&variations))?;
                info!(
                    "classiccraft: dance {folder} {gender}: {} variations written",
                    variations.len()
                );
            }
            Err(e) => warn!("classiccraft: dance {folder} {gender}: {e}"),
        }
    }
    Ok(())
}

/// A file of this version (an older one is written again).
fn current(path: &std::path::Path) -> bool {
    std::fs::read(path).is_ok_and(|b| b.len() >= 8 && &b[0..4] == MAGIC && b[4..8] == VERSION.to_le_bytes())
}

/// One variation of a dance: WoW's weight and replay range, and Steve's frames.
pub struct Variation {
    pub weight: u32,
    pub min_replay: u32,
    pub max_replay: u32,
    pub frames: Vec<[f32; 17]>,
}

fn encode(variations: &[Variation]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(variations.len() as u32).to_le_bytes());
    for v in variations {
        for n in [v.weight, v.min_replay, v.max_replay, v.frames.len() as u32] {
            out.extend_from_slice(&n.to_le_bytes());
        }
        out.extend_from_slice(&FPS.to_le_bytes());
        for f in &v.frames {
            for x in f {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
    }
    out
}

/// One model's dance variations as Steve's frames.
pub fn extract(bytes: &[u8]) -> anyhow::Result<Vec<Variation>> {
    let skeleton = parse_m2_skeleton(bytes)?;
    let attachments = parse_m2_attachments(bytes)?;
    let anims = parse_m2_animations(bytes);
    let find = |id: u16| anims.iter().find(|a| a.anim_id == id);
    let stand = find(ANIM_STAND).ok_or_else(|| anyhow::anyhow!("no Stand"))?;
    let rig = Rig::new(&skeleton.bones, &attachments)?;
    let rest = rig.measure(&rig.pose(stand, 0.0));
    let variations: Vec<Variation> = anims
        .iter()
        .filter(|a| a.anim_id == ANIM_DANCE)
        .map(|dance| {
            let count = ((dance.duration * FPS).round() as usize).max(1);
            Variation {
                weight: u32::from(dance.frequency).max(1),
                min_replay: dance.min_replay,
                max_replay: dance.max_replay.max(dance.min_replay),
                frames: (0..count)
                    .map(|i| steve_frame(&rest, &rig.measure(&rig.pose(dance, i as f32 / FPS))))
                    .collect(),
            }
        })
        .collect();
    if variations.is_empty() {
        anyhow::bail!("no dance animation");
    }
    Ok(variations)
}

/// The bones the measure reads, found once on the model.
struct Rig<'a> {
    bones: &'a [benilla_formats::SkeletonBone],
    arm: [usize; 2],
    hand: [(usize, Vec3); 2],
    hip: [usize; 2],
    foot: [usize; 2],
    head: usize,
}

/// What a pose comes to, WoW model space.
struct Measure {
    /// The torso frame as Minecraft model axes (columns: +x left, +y down, +z back), WoW coords.
    frame: Mat3,
    hip_mid: Vec3,
    shoulder: [Vec3; 2],
    hand: [Vec3; 2],
    hip: [Vec3; 2],
    foot: [Vec3; 2],
    head: Mat3,
}

impl<'a> Rig<'a> {
    fn new(
        bones: &'a [benilla_formats::SkeletonBone],
        attachments: &[benilla_formats::M2Attachment],
    ) -> anyhow::Result<Self> {
        let key = |k: i16| {
            bones
                .iter()
                .position(|b| b.key_bone == k)
                .ok_or_else(|| anyhow::anyhow!("no key bone {k}"))
        };
        let attach = |id: u16| {
            attachments
                .iter()
                .find(|a| a.id == id)
                .map(|a| (a.bone as usize, Vec3::from(a.position)))
                .ok_or_else(|| anyhow::anyhow!("no attachment {id}"))
        };
        let pivot = |i: usize| Vec3::from(bones[i].pivot);
        let chain = |mut i: usize| {
            let mut out = vec![i];
            while let Some(p) = usize::try_from(bones[i].parent).ok().filter(|&p| p < bones.len()) {
                if out.contains(&p) {
                    break;
                }
                out.push(p);
                i = p;
            }
            out
        };
        // Each foot: the lowest bone on its side (WoW +y is the model's left) under the waist
        // (the models keep parentless bones on the floor that are no leg); its hip: the top of its
        // chain, the bone just under the one both legs share.
        let waist = key(KEY_WAIST)?;
        let side_lowest = |left: bool| {
            (0..bones.len())
                .filter(|&i| (pivot(i).y > 0.0) == left && pivot(i).y.abs() > 0.02)
                .filter(|&i| chain(i).contains(&waist))
                .min_by(|&a, &b| pivot(a).z.total_cmp(&pivot(b).z))
                .ok_or_else(|| anyhow::anyhow!("no foot"))
        };
        let (foot_l, foot_r) = (side_lowest(true)?, side_lowest(false)?);
        let (chain_l, chain_r) = (chain(foot_l), chain(foot_r));
        let hip_of = |own: &[usize], other: &[usize]| {
            own.iter()
                .position(|b| other.contains(b))
                .and_then(|shared| shared.checked_sub(1).map(|k| own[k]))
                .ok_or_else(|| anyhow::anyhow!("legs share no bone"))
        };
        Ok(Rig {
            bones,
            arm: [key(KEY_ARM_R)?, key(KEY_ARM_L)?],
            hand: [attach(ATTACH_HAND_R)?, attach(ATTACH_HAND_L)?],
            hip: [hip_of(&chain_r, &chain_l)?, hip_of(&chain_l, &chain_r)?],
            foot: [foot_r, foot_l],
            head: key(KEY_HEAD)?,
        })
    }

    /// Every bone's world transform at `t` seconds into `anim` (WoW model space).
    fn pose(&self, anim: &benilla_formats::ModelAnimation, t: f32) -> Vec<Affine3A> {
        let keys: HashMap<u16, &benilla_formats::BoneKeys> =
            anim.bones.iter().map(|k| (k.bone, k)).collect();
        let mut world: Vec<Option<Affine3A>> = vec![None; self.bones.len()];
        fn solve(
            i: usize,
            bones: &[benilla_formats::SkeletonBone],
            keys: &HashMap<u16, &benilla_formats::BoneKeys>,
            t: f32,
            world: &mut Vec<Option<Affine3A>>,
        ) -> Affine3A {
            if let Some(w) = world[i] {
                return w;
            }
            let b = &bones[i];
            let parent = usize::try_from(b.parent).ok().filter(|&p| p < bones.len() && p != i);
            let base = parent.map_or(Vec3::ZERO, |p| Vec3::from(bones[p].pivot));
            let k = keys.get(&(i as u16));
            let trans = k.map_or(Vec3::ZERO, |k| sample_vec(&k.translation, t, Vec3::ZERO));
            let rot = k.map_or(Quat::IDENTITY, |k| sample_quat(&k.rotation, t));
            let scale = k.map_or(Vec3::ONE, |k| sample_vec(&k.scale, t, Vec3::ONE));
            let local = Affine3A::from_scale_rotation_translation(
                scale,
                rot,
                Vec3::from(b.pivot) - base + trans,
            );
            let w = match parent {
                Some(p) => solve(p, bones, keys, t, world) * local,
                None => local,
            };
            world[i] = Some(w);
            w
        }
        (0..self.bones.len())
            .map(|i| solve(i, self.bones, &keys, t, &mut world))
            .collect()
    }

    fn measure(&self, w: &[Affine3A]) -> Measure {
        let joint = |i: usize| -> Vec3 { w[i].translation.into() };
        let at = |(bone, p): (usize, Vec3)| {
            w[bone].transform_point3(p - Vec3::from(self.bones[bone].pivot))
        };
        let shoulder = [joint(self.arm[0]), joint(self.arm[1])];
        let hip = [joint(self.hip[0]), joint(self.hip[1])];
        let hip_mid = (hip[0] + hip[1]) * 0.5;
        let up = ((shoulder[0] + shoulder[1]) * 0.5 - hip_mid).normalize_or(Vec3::Z);
        let across = shoulder[0] - shoulder[1]; // left to right
        let right = (across - up * across.dot(up)).normalize_or(Vec3::NEG_Y);
        let forward = up.cross(right);
        Measure {
            // Minecraft model axes: +x left, +y down, +z back.
            frame: Mat3::from_cols(-right, -up, -forward),
            hip_mid,
            shoulder,
            hand: [at(self.hand[0]), at(self.hand[1])],
            hip,
            foot: [joint(self.foot[0]), joint(self.foot[1])],
            head: Mat3::from_quat(w[self.head].to_scale_rotation_translation().1),
        }
    }
}

/// The base model axes (Minecraft's +x left, +y down, +z back) in WoW model coords (+x forward,
/// +y left, +z up).
fn base() -> Mat3 {
    Mat3::from_cols(Vec3::Y, Vec3::NEG_Z, Vec3::NEG_X)
}

/// One frame of Steve against the Stand pose.
fn steve_frame(rest: &Measure, m: &Measure) -> [f32; 17] {
    let b = base();
    // The torso's turn since Stand, in model axes, on the root.
    let root = b.transpose() * m.frame * rest.frame.transpose() * b;
    let scale = STEVE_HIP_PX / rest.hip_mid.z.max(0.1);
    let offset = b.transpose() * (m.hip_mid - rest.hip_mid) * scale;
    // The head against the torso, as it turned since Stand.
    let head = m.frame.transpose() * m.head * rest.head.transpose() * rest.frame;
    // A limb from its root to its end, in its torso's own model axes, and the turn it made since
    // Stand, put on Steve's limb hanging straight down (+y): Stand is Steve's own stance, so a
    // troll's hunch or a tauren's wide stance is not copied, only the dance's motion. ModelPart
    // turns a limb Rz·Rx, so (x, z) solve for the direction.
    let dir = |meas: &Measure, from: Vec3, to: Vec3| {
        (meas.frame.transpose() * (to - from)).normalize_or(Vec3::Y)
    };
    let limb = |rest_dir: Vec3, now: Vec3| {
        let d = Quat::from_rotation_arc(rest_dir, now) * Vec3::Y;
        [d.z.clamp(-1.0, 1.0).asin(), (-d.x).atan2(d.y)]
    };
    let [rx, ry, rz] = zyx(root);
    let [hx, hy, hz] = zyx(head);
    let ar = limb(dir(rest, rest.shoulder[0], rest.hand[0]), dir(m, m.shoulder[0], m.hand[0]));
    let al = limb(dir(rest, rest.shoulder[1], rest.hand[1]), dir(m, m.shoulder[1], m.hand[1]));
    let lr = limb(dir(rest, rest.hip[0], rest.foot[0]), dir(m, m.hip[0], m.foot[0]));
    let ll = limb(dir(rest, rest.hip[1], rest.foot[1]), dir(m, m.hip[1], m.foot[1]));
    [
        offset.x, offset.y, offset.z, rx, ry, rz, hx, hy, hz, ar[0], ar[1], al[0], al[1], lr[0],
        lr[1], ll[0], ll[1],
    ]
}

/// The (x, y, z) angles of `r = Rz(z)·Ry(y)·Rx(x)`, ModelPart's order.
fn zyx(r: Mat3) -> [f32; 3] {
    // Row-major reads of the column-major matrix: r[row][col] = r.col(col)[row].
    let at = |row: usize, col: usize| r.col(col)[row];
    let y = (-at(2, 0)).clamp(-1.0, 1.0).asin();
    let x = at(2, 1).atan2(at(2, 2));
    let z = at(1, 0).atan2(at(0, 0));
    [x, y, z]
}

fn sample_vec(keys: &[(f32, [f32; 3])], t: f32, empty: Vec3) -> Vec3 {
    sample(keys, t, Vec3::from, |a, b, f| a.lerp(b, f)).unwrap_or(empty)
}

fn sample_quat(keys: &[(f32, [f32; 4])], t: f32) -> Quat {
    sample(keys, t, |q| Quat::from_array(q).normalize(), |a, b, f| a.slerp(b, f))
        .unwrap_or(Quat::IDENTITY)
}

fn sample<K: Copy, V: Copy>(
    keys: &[(f32, K)],
    t: f32,
    conv: impl Fn(K) -> V,
    mix: impl Fn(V, V, f32) -> V,
) -> Option<V> {
    let (first, last) = (keys.first()?, keys.last()?);
    if t <= first.0 {
        return Some(conv(first.1));
    }
    if t >= last.0 {
        return Some(conv(last.1));
    }
    let i = keys.partition_point(|k| k.0 <= t);
    let (a, b) = (keys[i - 1], keys[i]);
    let f = if b.0 > a.0 { (t - a.0) / (b.0 - a.0) } else { 0.0 };
    Some(mix(conv(a.1), conv(b.1), f))
}
