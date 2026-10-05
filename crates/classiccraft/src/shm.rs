//! The main bridge file, `protocol/mcwow_protocol.h` v3, from the WoW side: benilla creates it,
//! publishes our body (`McwowWowPlayerState`) and reads Minecraft's camera (`McwowMcCameraState`).
//! Offsets are the header's documented byte layout; both slots are seqlocks (odd while writing).

use std::sync::atomic::{fence, Ordering};


pub const NAME: &str = "classiccraft_v1.shm";
const MAGIC: u32 = 0x6D63_7731; // 'mcw1'
const VERSION: u32 = 3;
const TOTAL_SIZE: usize = 260;

// McwowHeader
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_WOW_PID: usize = 8;
const OFF_MC_PID: usize = 12;
const OFF_WOW_HEARTBEAT: usize = 16;
const OFF_MC_HEARTBEAT: usize = 24;
// McwowWowPlayerState
const OFF_SEQ: usize = 32;
const OFF_X: usize = 36;
const OFF_FACING: usize = 48;
const OFF_WOW_TICK: usize = 52;
const OFF_GROUND_GRID: usize = 60;
const OFF_GROUND_MASK: usize = 160;
const OFF_MAP_ID: usize = 164;
const OFF_TELEPORT_SEQ: usize = 168;
const OFF_DIFFICULTY: usize = 172;
const OFF_PLAYER_GUID: usize = 176;
// McwowMcCameraState
const OFF_MC_SEQ: usize = 184;
const OFF_MC_X: usize = 188;
const OFF_MC_YAW: usize = 200;
const OFF_MC_PITCH: usize = 204;
const OFF_MC_FEET_X: usize = 216;
const OFF_MC_FIRST_PERSON: usize = 228;
const OFF_MC_TELEPORT_ACK: usize = 232;
const OFF_MC_ON_GROUND: usize = 236;
const OFF_MC_VEL_X: usize = 244;
const OFF_MC_FOV_DEG: usize = 256;

/// The 5x5 ground grid's radius in blocks; cell `i = (dx+2)*5 + (dz+2)`.
pub const GRID_RADIUS: i32 = 2;

/// Our body, as Minecraft reads it.
pub struct WowPlayer {
    pub pos: [f32; 3],
    pub facing: f32,
    /// Ground heights in Minecraft Y (blocks), and which cells hit.
    pub ground_grid: [f32; 25],
    pub ground_mask: u32,
    /// `u32::MAX` when not in world.
    pub map_id: u32,
    pub teleport_seq: u32,
    pub guid: u64,
    /// The `TerrainType.dbc` id underfoot (Minecraft's footsteps), `None` for none.
    pub ground_terrain: Option<u32>,
}

/// Minecraft's camera and body, raw (blocks, Y-up, degrees), region-local.
#[derive(Clone, Copy, Debug, Default)]
pub struct McCamera {
    pub eye: [f32; 3],
    pub yaw_deg: f32,
    pub pitch_deg: f32,
    pub feet: [f32; 3],
    pub first_person: bool,
    /// Steve is down a Minecraft mine (a separate dimension): his pose isn't WoW's (flag bit 1 of
    /// the first-person word).
    pub away: bool,
    pub teleport_ack: u32,
    pub on_ground: bool,
    /// Blocks per second.
    pub velocity: [f32; 3],
    pub fov_deg: f32,
}

pub struct Shm {
    map: crate::link::SharedMap,
}

impl Shm {
    /// Create or reuse the file; a file of another layout is wiped, as `bridge::Init` does.
    pub fn open() -> std::io::Result<Self> {
        // Every access below is bounds-checked against the fixed layout; concurrent writers are
        // the seqlock protocol's concern.
        let map = crate::link::open(NAME, TOTAL_SIZE, true)?;
        let mut shm = Self { map };
        if shm.u32(OFF_MAGIC) != MAGIC || shm.u32(OFF_VERSION) != VERSION {
            shm.map[..TOTAL_SIZE].fill(0);
        }
        shm.put_u32(OFF_MAGIC, MAGIC);
        shm.put_u32(OFF_VERSION, VERSION);
        shm.put_u32(OFF_WOW_PID, std::process::id());
        Ok(shm)
    }

    pub fn mc_pid(&self) -> u32 {
        self.u32(OFF_MC_PID)
    }

    pub fn mc_heartbeat(&self) -> u64 {
        self.u64(OFF_MC_HEARTBEAT)
    }

    /// One frame of ours: the heartbeat and the player slot.
    pub fn publish(&mut self, p: &WowPlayer) {
        let hb = self.u64(OFF_WOW_HEARTBEAT).wrapping_add(1);
        self.put_u64(OFF_WOW_HEARTBEAT, hb);
        let mut seq = self.u32(OFF_SEQ);
        if seq & 1 != 0 {
            seq = seq.wrapping_add(1); // a writer died mid-write
        }
        self.put_u32(OFF_SEQ, seq.wrapping_add(1));
        fence(Ordering::Release);
        for (i, v) in p.pos.iter().enumerate() {
            self.put_f32(OFF_X + 4 * i, *v);
        }
        self.put_f32(OFF_FACING, p.facing);
        self.put_u64(OFF_WOW_TICK, hb);
        for (i, v) in p.ground_grid.iter().enumerate() {
            self.put_f32(OFF_GROUND_GRID + 4 * i, *v);
        }
        self.put_u32(OFF_GROUND_MASK, p.ground_mask);
        self.put_u32(OFF_MAP_ID, p.map_id);
        self.put_u32(OFF_TELEPORT_SEQ, p.teleport_seq);
        // Vanilla has no instance difficulty (low byte 0); bits 8-15 carry the WoW terrain type
        // under our feet + 1 (0 none), for Minecraft's footsteps (2026-10-02).
        self.put_u32(OFF_DIFFICULTY, p.ground_terrain.map_or(0, |t| (t.min(254) + 1) << 8));
        self.put_u64(OFF_PLAYER_GUID, p.guid);
        fence(Ordering::Release);
        self.put_u32(OFF_SEQ, seq.wrapping_add(2));
    }

    /// Minecraft's camera slot, or `None` if never written or every try raced the writer.
    pub fn read_mc(&self) -> Option<McCamera> {
        for _ in 0..50 {
            let s1 = self.u32(OFF_MC_SEQ);
            if s1 & 1 != 0 {
                continue;
            }
            fence(Ordering::Acquire);
            let f3 = |off: usize| [self.f32(off), self.f32(off + 4), self.f32(off + 8)];
            let cam = McCamera {
                eye: f3(OFF_MC_X),
                yaw_deg: self.f32(OFF_MC_YAW),
                pitch_deg: self.f32(OFF_MC_PITCH),
                feet: f3(OFF_MC_FEET_X),
                first_person: self.u32(OFF_MC_FIRST_PERSON) & 1 != 0,
                away: self.u32(OFF_MC_FIRST_PERSON) & 2 != 0,
                teleport_ack: self.u32(OFF_MC_TELEPORT_ACK),
                on_ground: self.u32(OFF_MC_ON_GROUND) != 0,
                velocity: f3(OFF_MC_VEL_X),
                fov_deg: self.f32(OFF_MC_FOV_DEG),
            };
            fence(Ordering::Acquire);
            if self.u32(OFF_MC_SEQ) == s1 {
                return (s1 != 0).then_some(cam);
            }
        }
        None
    }

    fn u32(&self, off: usize) -> u32 {
        // SAFETY: in bounds of the fixed layout; volatile, since the other process writes it.
        unsafe { std::ptr::read_volatile(self.map.as_ptr().add(off).cast::<u32>()) }
    }
    fn u64(&self, off: usize) -> u64 {
        // SAFETY: as `u32`; x86 tolerates the layout's unaligned u64s.
        unsafe { std::ptr::read_unaligned(self.map.as_ptr().add(off).cast::<u64>()) }
    }
    fn f32(&self, off: usize) -> f32 {
        f32::from_bits(self.u32(off))
    }
    fn put_u32(&mut self, off: usize, v: u32) {
        // SAFETY: as `u32`.
        unsafe { std::ptr::write_volatile(self.map.as_mut_ptr().add(off).cast::<u32>(), v) }
    }
    fn put_u64(&mut self, off: usize, v: u64) {
        // SAFETY: as `u64`.
        unsafe { std::ptr::write_unaligned(self.map.as_mut_ptr().add(off).cast::<u64>(), v) }
    }
    fn put_f32(&mut self, off: usize, v: f32) {
        self.put_u32(off, v.to_bits());
    }
}
