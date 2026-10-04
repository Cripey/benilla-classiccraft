//! Offline, on demand (2026-10-04, user: WoW weapon models as blocky 3D Minecraft weapons, specific
//! ones only): one WoW item display's weapon model as a Minecraft item model made of cubes, into the
//! LOCAL resource pack the mod loads (`~/.local/share/classiccraft/resourcepack`, or `$OUT`) - built
//! from the user's own install, never shipped.
//!
//! The M2's opaque and alpha-keyed batches (glow layers are additive and left out) are sampled into
//! a voxel grid, `voxels` cells along the weapon's long axis, each cell coloured by the texture under
//! it; the colours are quantised to a 16x16 palette texture and same-coloured cells merged into
//! boxes. The long axis becomes Minecraft's Y with the grip (the model origin) at the block's
//! centre, the middle axis X, the thin one Z (width and thickness centred; a mesh that is mostly
//! mirror-symmetric, >= 80% of vertices with a twin, is mirrored in cubes too). Also writes
//! `weapon_previews/<key>.png` (side view) next to the pack, for checking the look without the game.
//!
//! Usage: `cc_weapon <ItemDisplayInfo id> <key> [voxels, default 32]` (needs `$WOW_DATA`).
use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use bevy::math::{Affine3A, Quat, Vec3};

const WEAPON_DIR: &str = "Item\\ObjectComponents\\Weapon\\";
/// Model units the hand's hold point sits above the grip's middle (fitted by eye in game).
const HAND_BIAS: f32 = 1.5;

struct Tex {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

impl Tex {
    fn sample(&self, u: f32, v: f32) -> [u8; 4] {
        let x = ((u.rem_euclid(1.0)) * self.w as f32) as u32 % self.w;
        let y = ((v.rem_euclid(1.0)) * self.h as f32) as u32 % self.h;
        let i = ((y * self.w + x) * 4) as usize;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        bail!("usage: cc_weapon <ItemDisplayInfo id> <key> [voxels]");
    }
    let display: u32 = args[0].parse()?;
    let key = &args[1];
    let res: usize = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(32);
    let out = std::env::var_os("OUT").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").expect("$HOME")).join(".local/share/classiccraft/resourcepack")
    });
    let data = benilla_formats::wow_data().context("no WoW install ($WOW_DATA)")?;
    let mut chain = benilla_formats::open_chain(&data)?;
    let catalog = benilla_formats::load_item_display_catalog(&mut chain)?;
    let d = catalog.get(display).with_context(|| format!("no ItemDisplayInfo {display}"))?;
    let model = d.model[0].clone().context("display has no model")?;
    let model_path = if model.contains('\\') { model.clone() } else { format!("{WEAPON_DIR}{model}") };
    let bytes = chain.read_file(&model_path).with_context(|| format!("reading {model_path}"))?;
    let format = benilla_m2::parse_m2(&mut Cursor::new(&bytes[..])).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let m = format.model();
    let skin = m.parse_embedded_skin(&bytes, 0).map_err(|e| anyhow::anyhow!("{e:?}"))?;

    // Textures by M2 texture index: hardcoded names, else the display's own texture.
    let mut textures: HashMap<usize, Tex> = HashMap::new();
    let mut tex_for = |chain: &mut benilla_formats::Chain, ti: usize| -> Option<()> {
        if textures.contains_key(&ti) {
            return Some(());
        }
        let t = m.textures.get(ti)?;
        let path = match t.texture_type {
            benilla_m2::M2TextureType::Hardcoded => t.filename.string.to_string_lossy().into_owned(),
            _ => format!("{WEAPON_DIR}{}.blp", d.model_texture[0].as_deref()?),
        };
        let bytes = chain.read_file(&path).ok()?;
        let (w, h, rgba) = benilla_formats::blp_to_rgba(&bytes).ok()?;
        textures.insert(ti, Tex { w, h, rgba });
        Some(())
    };

    // The model as it stands (2026-10-04: Atiesh's head and ribbons are bone-animated and sat in odd
    // places at the bind pose): each vertex skinned by the Stand sequence's first frame, else bind.
    let skeleton = benilla_formats::parse_m2_skeleton(&bytes).ok();
    let anims = benilla_formats::parse_m2_animations(&bytes);
    let stand = anims.iter().find(|a| a.anim_id == 0).or(anims.first());
    let world: Vec<Affine3A> = match (&skeleton, stand) {
        (Some(sk), Some(anim)) => pose(&sk.bones, anim, 0.0),
        _ => Vec::new(),
    };
    if std::env::var_os("CC_WEAPON_DEBUG").is_some() {
        let g = benilla_formats::parse_m2_global_sequence_bones(&bytes);
        eprintln!("bones {}, sequences {:?}, stand keyed bones {}, global-sequence bones {}",
            skeleton.as_ref().map_or(0, |s| s.bones.len()), anims.iter().map(|a| a.anim_id).collect::<Vec<_>>(),
            stand.map_or(0, |a| a.bones.len()), g.len());
        if let Some(sk) = &skeleton {
            for (i, b) in sk.bones.iter().enumerate() {
                let k = stand.and_then(|a| a.bones.iter().find(|k| k.bone as usize == i));
                eprintln!("  bone {i} parent {} pivot {:.2?} keys t{} r{} s{}{}", b.parent, b.pivot,
                    k.map_or(0, |k| k.translation.len()), k.map_or(0, |k| k.rotation.len()), k.map_or(0, |k| k.scale.len()),
                    k.and_then(|k| k.rotation.first()).map_or(String::new(), |r| format!(" r0 {:.2?}", r.1)));
            }
        }
    }
    let posed = |v: &benilla_m2::M2Vertex| -> [f32; 3] {
        let p = Vec3::new(v.position.x, v.position.y, v.position.z);
        let (Some(sk), false) = (&skeleton, world.is_empty()) else { return p.to_array() };
        let mut out = Vec3::ZERO;
        let mut total = 0.0;
        for k in 0..4 {
            let w = f32::from(v.bone_weights[k]) / 255.0;
            let b = v.bone_indices[k] as usize;
            if w <= 0.0 || b >= world.len() {
                continue;
            }
            out += w * world[b].transform_point3(p - Vec3::from(sk.bones[b].pivot));
            total += w;
        }
        if total > 0.0 { (out / total).to_array() } else { p.to_array() }
    };

    // Opaque / alpha-keyed triangles with their texture.
    let mut tris: Vec<([[f32; 3]; 3], [[f32; 2]; 3], usize)> = Vec::new();
    for b in skin.batches() {
        let blend = m.materials.get(b.material_index as usize).map_or(0, |mat| mat.blend_mode.bits());
        if blend > 1 {
            continue; // additive / blended glow layers
        }
        let Some(&ti) = m.raw_data.texture_lookup_table.get(b.texture_combo_index as usize) else { continue };
        if tex_for(&mut chain, ti as usize).is_none() {
            continue;
        }
        let Some(sec) = skin.submeshes().get(b.skin_section_index as usize) else { continue };
        let start = sec.triangle_start as usize;
        if std::env::var_os("CC_WEAPON_DEBUG").is_some() {
            let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
            for t in start..start + sec.triangle_count as usize {
                let v = posed(&m.vertices[skin.indices()[skin.triangles()[t] as usize] as usize]);
                for a in 0..3 {
                    lo[a] = lo[a].min(v[a]);
                    hi[a] = hi[a].max(v[a]);
                }
            }
            let t = &m.textures[ti as usize];
            eprintln!("batch section {} blend {blend} texture {:?} type {:?} tris {} lo {lo:.2?} hi {hi:.2?}",
                b.skin_section_index, t.filename.string, std::mem::discriminant(&t.texture_type), sec.triangle_count / 3);
        }
        for t in (start..start + sec.triangle_count as usize).step_by(3) {
            let mut p = [[0.0; 3]; 3];
            let mut uv = [[0.0; 2]; 3];
            for k in 0..3 {
                let idx = skin.indices()[skin.triangles()[t + k] as usize] as usize;
                let v = &m.vertices[idx];
                p[k] = posed(v);
                uv[k] = [v.tex_coords.x, v.tex_coords.y];
            }
            tris.push((p, uv, ti as usize));
        }
    }
    if tris.is_empty() {
        bail!("no opaque triangles in {model_path}");
    }

    // Axes: long -> Minecraft Y, middle -> X, thin -> Z; the grip (origin) at the bottom end.
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for (p, _, _) in &tris {
        for v in p {
            for a in 0..3 {
                lo[a] = lo[a].min(v[a]);
                hi[a] = hi[a].max(v[a]);
            }
        }
    }
    let ext = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
    let mut axes = [0usize, 1, 2];
    axes.sort_by(|&a, &b| ext[b].total_cmp(&ext[a]));
    let (long, mid, thin) = (axes[0], axes[1], axes[2]);
    let scale = (res as f32 - 1.0) / ext[long];
    let flip = (0.0 - lo[long]) * scale > res as f32 / 2.0; // origin in the upper half: blade points down
    // Width and thickness are centred on the model's middle, a whole number of cells either way, so
    // a mirror-symmetric weapon stays symmetric (user, 2026-10-04: anchored at the edge, one side
    // rounded a cell wider).
    let cells = |e: f32| ((e * scale / 2.0).ceil() as usize).max(1) * 2;
    let dims = [cells(ext[mid]), res, cells(ext[thin])];
    let centre = |a: usize| (lo[a] + hi[a]) / 2.0;
    let to_voxel = |v: &[f32; 3]| -> [f32; 3] {
        let mut y = (v[long] - lo[long]) * scale;
        if flip {
            y = res as f32 - 1.0 - y;
        }
        [
            (v[mid] - centre(mid)) * scale + dims[0] as f32 / 2.0,
            y,
            (v[thin] - centre(thin)) * scale + dims[2] as f32 / 2.0,
        ]
    };

    // Rasterise: each triangle sampled densely enough to touch every cell it crosses.
    let mut sum: HashMap<[usize; 3], [u32; 4]> = HashMap::new();
    let (mut kept, mut dropped) = (0usize, 0usize);
    for (p, uv, ti) in &tris {
        let tex = &textures[ti];
        let q = [to_voxel(&p[0]), to_voxel(&p[1]), to_voxel(&p[2])];
        let len = |a: [f32; 3], b: [f32; 3]| ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
        let n = (len(q[0], q[1]).max(len(q[1], q[2])).max(len(q[2], q[0])) * 2.0).ceil() as usize + 1;
        for i in 0..=n {
            for j in 0..=(n - i) {
                let (a, b) = (i as f32 / n as f32, j as f32 / n as f32);
                let c = 1.0 - a - b;
                let pos = [0, 1, 2].map(|k| a * q[0][k] + b * q[1][k] + c * q[2][k]);
                let (u, v) = (a * uv[0][0] + b * uv[1][0] + c * uv[2][0], a * uv[0][1] + b * uv[1][1] + c * uv[2][1]);
                let px = tex.sample(u, v);
                if px[3] < 128 {
                    dropped += 1;
                    continue;
                }
                kept += 1;
                let cell = [0, 1, 2].map(|k| (pos[k].max(0.0) as usize).min(dims[k] - 1));
                let s = sum.entry(cell).or_insert([0; 4]);
                for k in 0..3 {
                    s[k] += u32::from(px[k]);
                }
                s[3] += 1;
            }
        }
    }

    // Is WoW's mesh itself mirror-symmetric across the width? (vertices with a twin within 1% of
    // the model's length)
    let tol = ext[long] * 0.01;
    let pts: Vec<[f32; 3]> = tris.iter().flat_map(|(p, _, _)| p.iter().copied()).collect();
    let c_mid = (lo[mid] + hi[mid]) / 2.0;
    let twins = pts.iter().filter(|v| {
        let m = 2.0 * c_mid - v[mid];
        pts.iter().any(|w| (w[mid] - m).abs() < tol && (w[long] - v[long]).abs() < tol && (w[thin] - v[thin]).abs() < tol)
    }).count();
    println!("{key}: mesh mirror twins {:.0}% across the width", 100.0 * twins as f32 / pts.len().max(1) as f32);
    // A mostly symmetric mesh stays symmetric in cubes (user, 2026-10-04): a cell without a twin
    // across the width gets one in its colour; twins keep their own colours (painted details).
    let symmetric = twins as f32 >= 0.8 * pts.len() as f32;
    if symmetric {
        let lone: Vec<([usize; 3], [u32; 4])> = sum
            .iter()
            .filter(|(c, _)| !sum.contains_key(&[dims[0] - 1 - c[0], c[1], c[2]]))
            .map(|(c, s)| ([dims[0] - 1 - c[0], c[1], c[2]], *s))
            .collect();
        for (c, s) in lone {
            sum.insert(c, s);
        }
    }
    if std::env::var_os("CC_WEAPON_DEBUG").is_some() {
        eprintln!("samples kept {kept}, dropped for alpha {dropped}");
    }
    // Palette: colours quantised until they fit 16x16.
    let colour = |s: &[u32; 4]| [0, 1, 2].map(|k| (s[k] / s[3]) as u8);
    let mut step = 8u8;
    let (palette, index) = loop {
        let mut palette: Vec<[u8; 3]> = Vec::new();
        let mut index: HashMap<[usize; 3], usize> = HashMap::new();
        let mut seen: HashMap<[u8; 3], usize> = HashMap::new();
        for (cell, s) in &sum {
            let c = colour(s).map(|x| (x / step) * step + step / 2);
            let i = *seen.entry(c).or_insert_with(|| {
                palette.push(c);
                palette.len() - 1
            });
            index.insert(*cell, i);
        }
        if palette.len() <= 256 {
            break (palette, index);
        }
        step = step.saturating_mul(2).max(step + 8);
    };

    // Greedy merge: x runs, then whole rows up y, then slabs along z.
    let mut done: HashMap<[usize; 3], bool> = HashMap::new();
    let at = |c: [usize; 3]| index.get(&c).copied();
    let mut cells: Vec<[usize; 3]> = index.keys().copied().collect();
    cells.sort();
    let mut boxes: Vec<([usize; 3], [usize; 3], usize)> = Vec::new();
    for c0 in cells {
        if done.contains_key(&c0) {
            continue;
        }
        let col = at(c0).unwrap();
        let free = |c: [usize; 3], done: &HashMap<[usize; 3], bool>| at(c) == Some(col) && !done.contains_key(&c);
        let mut w = 1;
        while c0[0] + w < dims[0] && free([c0[0] + w, c0[1], c0[2]], &done) {
            w += 1;
        }
        let mut h = 1;
        'grow_y: while c0[1] + h < dims[1] {
            for x in 0..w {
                if !free([c0[0] + x, c0[1] + h, c0[2]], &done) {
                    break 'grow_y;
                }
            }
            h += 1;
        }
        let mut dz = 1;
        'grow_z: while c0[2] + dz < dims[2] {
            for x in 0..w {
                for y in 0..h {
                    if !free([c0[0] + x, c0[1] + y, c0[2] + dz], &done) {
                        break 'grow_z;
                    }
                }
            }
            dz += 1;
        }
        for x in 0..w {
            for y in 0..h {
                for z in 0..dz {
                    done.insert([c0[0] + x, c0[1] + y, c0[2] + z], true);
                }
            }
        }
        boxes.push((c0, [w, h, dz], col));
    }

    // Placement in the block's -16..32 space: grip at (8, 8), centred in X and Z.
    let grip_y = {
        let y = (0.0 - lo[long]) * scale;
        if flip { res as f32 - 1.0 - y } else { y }
    };
    let clampf = |v: f32, size: usize| v.max(-16.0).min(32.0 - size as f32);
    let off = [
        clampf(8.0 - dims[0] as f32 / 2.0, dims[0]),
        clampf(8.0 - grip_y, dims[1]),
        clampf(8.0 - dims[2] as f32 / 2.0, dims[2]),
    ];
    // The grip (user, 2026-10-04: Steve held the sword by the pommel): the narrowest run of rows
    // nearest WoW's own grip point (the model origin), in model units. A long weapon can't put it at
    // the block centre (the model must fit -16..32), so the hold transforms move it to the hand.
    let mut width = vec![0usize; dims[1]];
    for c in index.keys() {
        width[c[1]] = width[c[1]].max(index.keys().filter(|d| d[1] == c[1] && d[2] == c[2]).count());
    }
    // The handle: grown from WoW's grip point (the model origin) up and down while the rows stay
    // about as narrow as there - a sword's stops at the guard and the pommel, a staff's runs along
    // the shaft. (The narrowest rows anywhere found Atiesh's tip.)
    let origin = (grip_y.round().max(0.0) as usize).min(dims[1] - 1);
    let base = (origin.saturating_sub(1)..=(origin + 1).min(dims[1] - 1))
        .map(|y| width[y])
        .filter(|&w| w > 0)
        .min()
        .unwrap_or_else(|| width.iter().filter(|&&w| w > 0).min().copied().unwrap_or(1));
    let handle = |y: usize| width[y] > 0 && width[y] <= base + 1;
    let (mut a, mut b) = (origin, origin);
    while a > 0 && handle(a - 1) {
        a -= 1;
    }
    while b + 1 < dims[1] && handle(b + 1) {
        b += 1;
    }
    let grip = (a, b);
    // A long narrow run is a shaft (a staff, a polearm): WoW's own grip point on it, not its middle.
    let grip_centre = if grip.1 - grip.0 + 1 > 8 { grip_y + 0.5 } else { (grip.0 + grip.1 + 1) as f32 / 2.0 };
    let grip_model = off[1] + grip_centre + HAND_BIAS;
    let tex_id = format!("mcwow:item/wow/{key}");
    let mut elements = Vec::new();
    for (c, s, col) in &boxes {
        let (u, v) = ((col % 16) as f32, (col / 16) as f32);
        let uv = format!("[{:.2}, {:.2}, {:.2}, {:.2}]", u + 0.25, v + 0.25, u + 0.75, v + 0.75);
        let faces: Vec<String> = ["north", "east", "south", "west", "up", "down"]
            .iter()
            .map(|f| format!("\"{f}\": {{\"uv\": {uv}, \"texture\": \"#t\"}}"))
            .collect();
        elements.push(format!(
            "{{\"from\": [{:.2}, {:.2}, {:.2}], \"to\": [{:.2}, {:.2}, {:.2}], \"faces\": {{{}}}}}",
            off[0] + c[0] as f32, off[1] + c[1] as f32, off[2] + c[2] as f32,
            off[0] + (c[0] + s[0]) as f32, off[1] + (c[1] + s[1]) as f32, off[2] + (c[2] + s[2]) as f32,
            faces.join(", ")
        ));
    }
    // Hold transforms. Minecraft holds an item at its transform's origin, which is model point
    // (8, 8, 8) moved by the translation (pixels) after rotating and scaling the model about it:
    // a translation of R * (0, (8 - grip) * scale, 0) puts the grip there.
    let held = |rot: [f32; 3], scale: f32, base: [f32; 3]| -> String {
        let d = (8.0 - grip_model) * scale;
        let (x, y, z) = (rot[0].to_radians(), rot[1].to_radians(), rot[2].to_radians());
        // R = Rx * Ry * Rz (rotationXYZ) applied to (0, d, 0)
        let (vx, vy) = (-d * z.sin(), d * z.cos());
        let (vx, vz) = (vx * y.cos(), -vx * y.sin());
        let (vy, vz) = (vy * x.cos() - vz * x.sin(), vy * x.sin() + vz * x.cos());
        format!(
            "{{\"rotation\": [{}, {}, {}], \"translation\": [{:.2}, {:.2}, {:.2}], \"scale\": [{s}, {s}, {s}]}}",
            rot[0], rot[1], rot[2], base[0] + vx, base[1] + vy, base[2] + vz, s = scale
        )
    };
    let display = format!(
        r#""display": {{
    "thirdperson_righthand": {},
    "thirdperson_lefthand": {},
    "firstperson_righthand": {},
    "firstperson_lefthand": {},
    "gui": {{"rotation": [0, 90, -45], "translation": [0, 0, 0], "scale": [0.45, 0.45, 0.45]}},
    "ground": {{"rotation": [0, 0, 0], "translation": [0, 2, 0], "scale": [0.4, 0.4, 0.4]}},
    "fixed": {{"rotation": [0, 90, -45], "translation": [0, 0, 0], "scale": [0.55, 0.55, 0.55]}}
  }}"#,
        held([0.0, 90.0, 0.0], 0.85, [0.0, 0.0, 1.0]),
        held([0.0, -90.0, 0.0], 0.85, [0.0, 0.0, 1.0]),
        held([0.0, -90.0, 25.0], 0.68, [1.13, 3.2, 1.13]),
        held([0.0, 90.0, -25.0], 0.68, [1.13, 3.2, 1.13]),
    );
    let model_json = format!(
        "{{\n  \"texture_size\": [16, 16],\n  \"textures\": {{\"t\": \"{tex_id}\", \"particle\": \"{tex_id}\"}},\n  {display},\n  \"elements\": [\n    {}\n  ]\n}}\n",
        elements.join(",\n    ")
    );

    let assets = out.join("assets/mcwow");
    for d in ["models/item/wow", "textures/item/wow", "items/wow"] {
        std::fs::create_dir_all(assets.join(d))?;
    }
    std::fs::write(assets.join(format!("models/item/wow/{key}.json")), model_json)?;
    std::fs::write(
        assets.join(format!("items/wow/{key}.json")),
        format!("{{\"model\": {{\"type\": \"minecraft:model\", \"model\": \"mcwow:item/wow/{key}\"}}}}\n"),
    )?;
    let mut img = image::RgbaImage::new(16, 16);
    for (i, c) in palette.iter().enumerate() {
        img.put_pixel((i % 16) as u32, (i / 16) as u32, image::Rgba([c[0], c[1], c[2], 255]));
    }
    img.save(assets.join(format!("textures/item/wow/{key}.png")))?;
    let meta = out.join("pack.mcmeta");
    if !meta.exists() {
        std::fs::write(&meta, "{\"pack\": {\"description\": \"classiccraft: WoW models from your own install (local, not shared)\", \"min_format\": [97, 0], \"max_format\": [97, 1]}}\n")?;
    }

    // Preview: the side the X/Y plane shows (looking along Z), 8 px a cell, nearest cell wins.
    let px = 8u32;
    let mut prev = image::RgbaImage::from_pixel(dims[0] as u32 * px, dims[1] as u32 * px, image::Rgba([40, 40, 48, 255]));
    for x in 0..dims[0] {
        for y in 0..dims[1] {
            if let Some(z) = (0..dims[2]).find(|&z| index.contains_key(&[x, y, z])) {
                let c = palette[index[&[x, y, z]]];
                let shade = 1.0 - 0.15 * z as f32 / dims[2].max(1) as f32;
                let rgba = image::Rgba([0, 1, 2].map(|k| (c[k] as f32 * shade) as u8).into_iter().chain([255]).collect::<Vec<_>>().try_into().unwrap());
                for dx in 0..px {
                    for dy in 0..px {
                        prev.put_pixel(x as u32 * px + dx, (dims[1] - 1 - y) as u32 * px + dy, rgba);
                    }
                }
            }
        }
    }
    let previews = out.parent().unwrap_or(&out).join("weapon_previews");
    std::fs::create_dir_all(&previews)?;
    prev.save(previews.join(format!("{key}.png")))?;
    // How mirror-symmetric the shape came out across the width (cells without a twin).
    let unmatched = index.keys().filter(|c| !index.contains_key(&[dims[0] - 1 - c[0], c[1], c[2]])).count();
    println!(
        "{key}: {model_path} -> {} cells, {} boxes, {} colours, {}x{}x{} cells{}, {} cells without a mirror twin, grip at y {:.1}",
        index.len(), boxes.len(), palette.len(), dims[0], dims[1], dims[2], if flip { ", flipped" } else { "" }, unmatched,
        grip_model
    );
    Ok(())
}

/// Every bone's model-space transform at `t` s into `anim`, joint at its pivot (as the classiccraft
/// crate's dances.rs poses characters): a point relative to a bone's pivot maps to the model.
fn pose(bones: &[benilla_formats::SkeletonBone], anim: &benilla_formats::ModelAnimation, t: f32) -> Vec<Affine3A> {
    let keys: HashMap<u16, &benilla_formats::BoneKeys> = anim.bones.iter().map(|k| (k.bone, k)).collect();
    let mut world: Vec<Option<Affine3A>> = vec![None; bones.len()];
    fn solve(
        i: usize,
        bones: &[benilla_formats::SkeletonBone],
        keys: &HashMap<u16, &benilla_formats::BoneKeys>,
        t: f32,
        world: &mut Vec<Option<Affine3A>>,
        depth: usize,
    ) -> Affine3A {
        if let Some(w) = world[i] {
            return w;
        }
        let b = &bones[i];
        let parent = usize::try_from(b.parent).ok().filter(|&p| p < bones.len() && p != i && depth < 64);
        let base = parent.map_or(Vec3::ZERO, |p| Vec3::from(bones[p].pivot));
        let k = keys.get(&(i as u16));
        let trans = k.and_then(|k| sample(&k.translation, t)).map_or(Vec3::ZERO, Vec3::from);
        let rot = k.and_then(|k| sample_rot(&k.rotation, t)).unwrap_or(Quat::IDENTITY);
        let scale = k.and_then(|k| sample(&k.scale, t)).map_or(Vec3::ONE, Vec3::from);
        let local = Affine3A::from_scale_rotation_translation(scale, rot, Vec3::from(b.pivot) - base + trans);
        let w = match parent {
            Some(p) => solve(p, bones, keys, t, world, depth + 1) * local,
            None => local,
        };
        world[i] = Some(w);
        w
    }
    (0..bones.len()).map(|i| solve(i, bones, &keys, t, &mut world, 0)).collect()
}

/// A vector key track at `t` (linear between keys, clamped at the ends).
fn sample(keys: &[(f32, [f32; 3])], t: f32) -> Option<[f32; 3]> {
    let (first, last) = (keys.first()?, keys.last()?);
    if t <= first.0 {
        return Some(first.1);
    }
    if t >= last.0 {
        return Some(last.1);
    }
    let i = keys.partition_point(|k| k.0 <= t);
    let (a, b) = (keys[i - 1], keys[i]);
    let f = if b.0 > a.0 { (t - a.0) / (b.0 - a.0) } else { 0.0 };
    Some([0, 1, 2].map(|k| a.1[k] + (b.1[k] - a.1[k]) * f))
}

/// A rotation key track at `t` (slerp between keys).
fn sample_rot(keys: &[(f32, [f32; 4])], t: f32) -> Option<Quat> {
    let (first, last) = (keys.first()?, keys.last()?);
    let q = |k: [f32; 4]| Quat::from_array(k).normalize();
    if t <= first.0 {
        return Some(q(first.1));
    }
    if t >= last.0 {
        return Some(q(last.1));
    }
    let i = keys.partition_point(|k| k.0 <= t);
    let (a, b) = (keys[i - 1], keys[i]);
    let f = if b.0 > a.0 { (t - a.0) / (b.0 - a.0) } else { 0.0 };
    Some(q(a.1).slerp(q(b.1), f))
}
