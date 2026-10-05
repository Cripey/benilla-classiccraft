//! The Minecraft bridge: publishes our body to Minecraft and drives the body and camera from
//! Minecraft's pose ([`benilla_app::external`]), over `protocol/mcwow_protocol.h` v3.
//!
//! Placement handshake (as mcwow's): whenever WoW decides where we are (world entry, a map
//! change, a server move, the bridge switched on), `teleport_seq` advances; Minecraft moves its
//! player to the published spot and echoes it as `teleport_ack`, and only then does its pose drive
//! the body. Coordinates are mcwow's fixed mapping, 1 block = [`S`] yd:
//! `mcX = wowY/S, mcY = wowZ/S, mcZ = wowX/S`, which in Bevy space is `(-mcX, mcY, -mcZ)·S`.

use benilla_app::external::{CustomPacketOut, ExternalDrive, ExternalDriveSet, ExternalPose, SelfReport};

/// Steve went down a Minecraft mine (1) or came back (0).
const CMSG_CC_MINE: u16 = 0x0344;
use benilla_world::collision::WorldCollision;
use bevy::prelude::*;

use crate::shm::{McCamera, Shm, WowPlayer, GRID_RADIUS};

/// WoW yards per Minecraft block (`MCWOW_MC_BLOCKS_TO_WOW_YARDS`).
pub const S: f32 = 1.4667;
/// Minecraft's pose is stale once its heartbeat has not moved for this long (s).
const MC_STALE_SECS: f32 = 1.0;
/// The ground probe starts this far above the feet (yd) and reaches this far down.
const PROBE_RISE: f32 = 4.0;
const PROBE_REACH: f32 = 60.0;

pub struct BridgePlugin;

impl Plugin for BridgePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Bridge {
            enabled: std::env::var("CLASSICCRAFT_BRIDGE").as_deref() != Ok("0"),
            // A per-session start, so a stale ack Minecraft still holds from our last run never
            // matches our first placement (which once skipped placing Steve over cleared geometry).
            teleport_seq: session_seed(),
            ..default()
        })
        .add_systems(Update, supply_pose.in_set(ExternalDriveSet::Supply))
        .add_systems(
            Update,
            (publish_body, apply_fov).in_set(ExternalDriveSet::Report),
        );
    }
}

#[derive(Resource, Default)]
pub(crate) struct Bridge {
    shm: Option<Shm>,
    /// When to retry opening the file after a failure (`Time::elapsed_secs`).
    retry_at: f32,
    /// Numpad+ toggles; on by default (`CLASSICCRAFT_BRIDGE=0` starts it off).
    enabled: bool,
    teleport_seq: u32,
    seen_in_world: bool,
    /// A cinematic was on screen last frame (`external::cinematic`).
    seen_cinematic: bool,
    seen_map: Option<u32>,
    seen_server_moves: u32,
    mc_heartbeat: u64,
    mc_heartbeat_at: f32,
    /// Minecraft's rendered vertical FOV (degrees) for the frame we drove, if we did.
    fov_deg: Option<f32>,
    /// We changed the world camera's FOV and owe benilla's back when we stop.
    fov_applied: bool,
    /// Last logged state, so transitions log once.
    logged: Option<State>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Off,
    NotInWorld,
    NoMinecraft,
    Placing,
    Driving,
    /// Steve is down a Minecraft mine: Minecraft draws its own world (full-window overlay), the WoW
    /// body waits parked at the entrance (the server hides it, `CMSG_CC_MINE`).
    Away,
}

impl Bridge {
    /// Numpad+: the whole bridge on or off.
    pub(crate) fn toggle(&mut self) {
        self.enabled = !self.enabled;
        info!(
            "classiccraft: bridge {}",
            if self.enabled { "on" } else { "off" }
        );
        if self.enabled {
            self.place("bridge switched on");
        }
    }

    /// Minecraft is linked and owns the body (or is being placed): its input should flow.
    pub(crate) fn driving(&self) -> bool {
        matches!(self.logged, Some(State::Placing | State::Driving | State::Away))
    }

    fn place(&mut self, why: &str) {
        self.teleport_seq = self.teleport_seq.wrapping_add(1);
        info!("classiccraft: placement #{} ({why})", self.teleport_seq);
    }
}

fn session_seed() -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() ^ d.as_secs() as u32);
    (nanos ^ std::process::id().rotate_left(16)) & 0x7fff_0000
}

/// Minecraft position (blocks, region-local) to Bevy (yd).
pub(crate) fn mc_to_bevy(p: [f32; 3]) -> Vec3 {
    Vec3::new(-p[0] * S, p[1] * S, -p[2] * S)
}

fn pose_of(cam: &McCamera) -> ExternalPose {
    // Minecraft yaw grows clockwise seen from above, WoW's facing counter-clockwise; pitch is
    // positive looking down in Minecraft, up in Bevy.
    let yaw = -cam.yaw_deg.to_radians();
    ExternalPose {
        feet: mc_to_bevy(cam.feet),
        velocity: mc_to_bevy(cam.velocity),
        face_yaw: yaw.rem_euclid(std::f32::consts::TAU),
        eye: mc_to_bevy(cam.eye),
        eye_yaw: yaw,
        eye_pitch: -cam.pitch_deg.clamp(-89.0, 89.0).to_radians(),
        grounded: cam.on_ground,
        first_person: cam.first_person,
        deck: None, // `decks::compose_rider` fills it aboard
        deck_known: false,
    }
}

pub(crate) fn supply_pose(
    time: Res<Time>,
    report: Res<SelfReport>,
    mut drive: ResMut<ExternalDrive>,
    mut bridge: ResMut<Bridge>,
    mut out: MessageWriter<CustomPacketOut>,
) {
    let bridge = &mut *bridge;
    let now = time.elapsed_secs();
    drive.pose = None;
    if bridge.shm.is_none() && now >= bridge.retry_at {
        match Shm::open() {
            Ok(shm) => {
                info!("classiccraft: bridge file {} ready", crate::link::describe(crate::shm::NAME));
                bridge.shm = Some(shm);
            }
            Err(e) => {
                warn!("classiccraft: cannot open {}: {e}", crate::link::describe(crate::shm::NAME));
                bridge.retry_at = now + 5.0;
            }
        }
    }

    // What WoW decided since last frame.
    if report.in_world && !bridge.seen_in_world {
        bridge.place("entered the world");
    }
    bridge.seen_in_world = report.in_world;
    if report.in_world && report.map != bridge.seen_map {
        if bridge.seen_map.is_some() {
            bridge.place("map changed");
        }
        bridge.seen_map = report.map;
    }
    // A cinematic over (watched or skipped): Steve back onto the body, held until the ground is in.
    // Meanwhile the body was held and the geometry export frozen (geom.rs); Minecraft's pose from
    // before must not drive the body (2026-10-04: driving straight on, a Steve who had fallen
    // during the race intro took the WoW body under the map).
    let cinematic = benilla_app::external::cinematic();
    if bridge.seen_cinematic && !cinematic {
        bridge.place("cinematic over");
    }
    bridge.seen_cinematic = cinematic;
    if drive.server_moves != bridge.seen_server_moves {
        bridge.seen_server_moves = drive.server_moves;
        bridge.place("the server moved us");
    }

    let Some(shm) = bridge.shm.as_ref() else {
        return;
    };
    let hb = shm.mc_heartbeat();
    let cam = shm.read_mc();
    let mc_pid = shm.mc_pid();
    if hb != bridge.mc_heartbeat {
        bridge.mc_heartbeat = hb;
        bridge.mc_heartbeat_at = now;
    }
    let mc_alive = hb != 0 && now - bridge.mc_heartbeat_at < MC_STALE_SECS;

    let state = if !bridge.enabled {
        State::Off
    } else if !report.in_world {
        State::NotInWorld
    } else if !mc_alive {
        State::NoMinecraft
    } else if cam.is_some_and(|c| c.away) {
        State::Away
    } else if cam.is_none_or(|c| c.teleport_ack != bridge.teleport_seq) || report.settling || cinematic {
        State::Placing
    } else {
        State::Driving
    };
    let mut state = state;
    if bridge.logged != Some(state) {
        let was_away = bridge.logged == Some(State::Away);
        if state == State::Away || was_away {
            out.write(CustomPacketOut {
                opcode: CMSG_CC_MINE,
                body: vec![u8::from(state == State::Away)],
            });
        }
        if was_away {
            // Back from the mine: Minecraft puts Steve back where the WoW body waited.
            bridge.place("back from a mine");
            if state == State::Driving {
                state = State::Placing; // never drive with a pose from before the placement
            }
        }
        bridge.logged = Some(state);
        info!(
            "classiccraft: bridge {state:?} (seq {}, minecraft pid {})",
            bridge.teleport_seq,
            mc_pid
        );
    }
    bridge.fov_deg = None;
    if state == State::Driving {
        drive.pose = cam.as_ref().map(pose_of);
        // From 1°: a spyglass zooms to a tenth of the FOV (~7°), and a 10° floor sent WoW's camera
        // back out the moment the zoom finished (2026-10-03).
        bridge.fov_deg = cam.map(|c| c.fov_deg).filter(|f| (1.0..170.0).contains(f));
    }
}

fn publish_body(report: Res<SelfReport>, collide: WorldCollision, mut bridge: ResMut<Bridge>) {
    let teleport_seq = bridge.teleport_seq;
    let Some(shm) = bridge.shm.as_mut() else {
        return;
    };
    let [wx, wy, wz] = report.wow_pos;
    let (mut grid, mut mask) = ([0.0_f32; 25], 0_u32);
    if report.in_world {
        // One downward probe per Minecraft column around ours, at the column's centre.
        let (mc_x, mc_z) = ((wy / S).floor(), (wx / S).floor());
        let feet_y = wz;
        for dx in -GRID_RADIUS..=GRID_RADIUS {
            for dz in -GRID_RADIUS..=GRID_RADIUS {
                let i = ((dx + GRID_RADIUS) * 5 + (dz + GRID_RADIUS)) as usize;
                let cx = mc_x + dx as f32 + 0.5;
                let cz = mc_z + dz as f32 + 0.5;
                let origin = Vec3::new(-cx * S, feet_y + PROBE_RISE, -cz * S);
                if let Some(hit) = collide.ray_body(origin, Dir3::NEG_Y, PROBE_REACH) {
                    grid[i] = (origin.y - hit.distance) / S;
                    mask |= 1 << i;
                }
            }
        }
    }
    shm.publish(&WowPlayer {
        pos: report.wow_pos,
        facing: report.facing,
        ground_grid: grid,
        ground_mask: mask,
        map_id: if report.in_world {
            report.map.unwrap_or(u32::MAX)
        } else {
            u32::MAX
        },
        teleport_seq,
        guid: report.guid,
        ground_terrain: report.ground_terrain,
    });
}

/// The world camera takes Minecraft's vertical FOV while Minecraft drives, so the two pictures
/// share one projection; benilla's own comes back when it stops.
fn apply_fov(
    drive: Res<ExternalDrive>,
    mut bridge: ResMut<Bridge>,
    mut projection: Query<&mut Projection, With<benilla_world::view::WorldCamera>>,
) {
    let Ok(mut projection) = projection.single_mut() else {
        return;
    };
    let Projection::Perspective(p) = &mut *projection else {
        return;
    };
    match bridge.fov_deg.filter(|_| drive.drove) {
        Some(fov) => {
            p.fov = fov.to_radians();
            bridge.fov_applied = true;
        }
        None if bridge.fov_applied => {
            p.fov = benilla_world::view::CAM_FOVY;
            bridge.fov_applied = false;
        }
        None => {}
    }
}
