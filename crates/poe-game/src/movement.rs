//! The movement seam: what a movement simulation gets and gives, and the traits it implements.
//!
//! The server must run an exact port of the client's movement code (hub `docs/decisions.md`,
//! "Movement and world"). This module holds no simulation: it defines
//! - the inputs, decoded from the client's packets ([`MoveInput`]);
//! - the outputs, the server's movement messages in send order ([`MoveEvent`]);
//! - the per-character facts from game data ([`MovementConfig`]) and the per-frame facts from the
//!   world ([`StepCtx`]);
//! - [`MovementModel`] (makes one [`Mover`] per entity) and [`Mover`] (one entity's movement:
//!   input, teleport, one frame, and its cell for the rest of the server).
//!
//! The world ([`crate::world::World`]) owns the movers and the frame clock; the instance server
//! decodes packets into [`MoveInput`] and encodes [`MoveEvent`]s. Implementations:
//! [`crate::locomotion::Locomotion`] (the exact port) and [`crate::standstill::StandStill`] (the
//! stub: the player stands, only teleports move it).

use std::sync::Arc;

use crate::motion_data::MotionCurves;

pub type Cell = (i32, i32);

/// What the movement code needs of the terrain.
pub trait Walkable {
    fn walkable(&self, x: i32, y: i32) -> bool;

    /// Distance-to-wall value of a cell (0 = blocked; the client's grid has 0..5).
    fn clearance(&self, x: i32, y: i32) -> u8 {
        self.walkable(x, y) as u8
    }

    /// The client's 4-bit cell value of layer 0 (0 wall, 1..4 near a wall, 5 clear), which the
    /// client's pathfinder compares with the entity's clearance level. Terrain without such
    /// values has walls (0) and clear cells (5).
    fn nibble(&self, x: i32, y: i32) -> u8 {
        if self.walkable(x, y) { 5 } else { 0 }
    }

    /// The client's cell value on a layer (`FUN_141d0df50`, the walk mode picks the layer).
    fn layer(&self, _layer: usize, x: i32, y: i32) -> u8 {
        self.nibble(x, y)
    }

    /// Size of the terrain in cells, if it has one (cells outside are walls).
    fn bounds(&self) -> Option<(i32, i32)> {
        None
    }
}

/// Everything is walkable (no terrain data).
pub struct Open;

impl Walkable for Open {
    fn walkable(&self, _: i32, _: i32) -> bool {
        true
    }
}

/// Bit the server adds to the flags of a `StartAction` that an input started.
pub const ACTION_INPUT_FLAG: u16 = 0x10;

/// Skill ids the component recognises (from game data; everything else is a generic action).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillIds {
    /// Click-to-move (`ActionTypes` basic action Move).
    pub move_skill: u32,
    /// Dodge roll.
    pub roll_skill: u32,
    /// The dodge roll of the other weapon set: the Actor has one per set (`0x80000040` and
    /// `0x80000041`, slot byte `0x40`/`0x41`), and the client sends the active set's.
    pub roll_skill_other_set: u32,
}

impl SkillIds {
    /// One of the dodge rolls.
    pub fn is_roll(&self, skill: u32) -> bool {
        skill == self.roll_skill || skill == self.roll_skill_other_set
    }

    /// The weapon set a skill belongs to, from the official id layout: weapon skills
    /// (`0x8000_0000` bit) end in the slot byte `0x40 + set` (`0x80400040` main attack of set
    /// 0, `0x80410041` of set 1, rolls `0x80000040`/`0x80000041`). Other ids (basic actions,
    /// our server's own skill ids): none.
    pub fn weapon_set_of(skill: u32) -> Option<u8> {
        let slot = (skill & 0xff) as u8;
        (skill & 0x8000_0000 != 0 && (0x40..0x43).contains(&slot)).then(|| slot - 0x40)
    }
}


/// A `UseSkill` input (`0x1a5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UseSkill {
    pub skill: u32,
    pub flags: u16,
    pub unk: u8,
    pub target_flags: u8,
    /// Target entity (`target_flags & 8`); then `x`, `y` are absolute, else relative to the
    /// player's cell.
    pub entity: Option<u32>,
    pub x: i32,
    pub y: i32,
}

impl UseSkill {
    /// Bit of the target flags: this is a repeat of a held input.
    pub const HELD_REPEAT: u8 = 0x10;
}

/// What the client sends that moves the player, mirroring its packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveInput {
    /// `0x63` WASD vector (grid axes, length about 180; `(0, 0)` = stop).
    Direction { x: i32, y: i32 },
    /// `0x63` sent in click-to-move mode (the packet's third trailer byte is `1`): that client
    /// has no movement keys, so every vector is the cursor direction, also an exactly diagonal
    /// or axis-aligned one that [`MoveInput::Direction`] could take for a key.
    CursorDirection { x: i32, y: i32 },
    /// `0x1a5`.
    UseSkill(UseSkill),
    /// `0x1a8`, `x`, `y` relative to the player's cell.
    UpdateSkillTarget { x: i32, y: i32, skill: u32 },
    /// `0x1ac`: a held skill input ended.
    SkillInputEnd,
    /// `0x1ad`.
    SkillReleased { skill: u32, flag: u8 },
    /// `0x2e6`: the sprint key (Space).
    SprintInput { held: bool },
    /// `0x62` mode 2: the absolute cell under the cursor.
    Aim { x: i32, y: i32 },
    /// `0x62` mode 3 (hovered entity): not echoed.
    AimEntity { entity: u32 },
    /// `0x62` mode 0/1 (once at entry; mode 1 puts the character into combat stance).
    AimMode { mode: u8 },
    /// `0x200`: the stance request sent with an input-mode toggle and with the first skill use
    /// in mouse mode (value 1 click-to-move, 4 WASD/combat); echoed in the next stance delta.
    StanceRequest { value: u8 },
    /// `0xfe`: switch to weapon set `set` (client `0x1421150d0`). The lockstep client applies
    /// nothing itself and blocks skill use and swaps until the answer.
    WeaponSwap { set: u8 },
    /// `0x2e8`: the client's option `sprint_continues_with_move_input` (Player `+0x20a`).
    SprintOption { continues: bool },
}

/// Stats the component changes (game stat ids; the session resolves the network keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveStat {
    IsDodgeRolling,
    StunThresholdWhilePerformingAction,
    VirtualHeavyStunThreshold,
    VirtualLightStunThreshold,
    SprintBlendTimeOverrideMs,
    IsSprinting,
    MustSprint,
    AllHitsStunYou,
    VirtualSkillIgnoreNamelockingDynamically,
    BaseMoveSpeedFromSprint,
    MovementVelocityPermyriad,
    VirtualActionSpeed,
    MovementVelocityCap,
}

impl MoveStat {
    /// The stat's id in the game data (`Stats.dat` `Id`).
    pub fn id(self) -> &'static str {
        match self {
            Self::IsDodgeRolling => "is_dodge_rolling",
            Self::StunThresholdWhilePerformingAction => {
                "stun_threshold_+%_final_while_performing_action"
            }
            Self::VirtualHeavyStunThreshold => "virtual_heavy_stun_threshold_+%_final",
            Self::VirtualLightStunThreshold => "virtual_light_stun_threshold_+%_final",
            Self::SprintBlendTimeOverrideMs => "sprint_blend_time_override_ms",
            Self::IsSprinting => "is_sprinting",
            Self::MustSprint => "must_sprint",
            Self::AllHitsStunYou => "all_hits_stun_you",
            Self::VirtualSkillIgnoreNamelockingDynamically => {
                "virtual_skill_ignore_namelocking_dynamically"
            }
            Self::BaseMoveSpeedFromSprint => "base_move_speed_+%_from_sprint",
            Self::MovementVelocityPermyriad => "movement_velocity_+permyriad",
            Self::VirtualActionSpeed => "virtual_action_speed_+%",
            Self::MovementVelocityCap => "movement_velocity_cap",
        }
    }
}

/// What kind of action a [`MoveEvent::ActionStart`] starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Move,
    Roll,
    /// Any other skill (an attack).
    Other,
}

/// What the server tells the client about this entity's movement, in the order it is sent.
/// The instance server maps each variant to its packet (the opcode in the comment names the
/// official message; see docs/protocol/movement.md).
#[derive(Debug, Clone, PartialEq)]
pub enum MoveEvent {
    /// `0x64` MoveDirectionUpdate: direction (scaled while sprinting) and the server's cell.
    MoveAck { dir: Cell, cell: Cell },
    /// `0x1c8` StartAction. `action` comes from the world-wide action counter.
    ActionStart {
        kind: ActionKind,
        skill: u32,
        /// The request's flags plus [`ACTION_INPUT_FLAG`] for a start driven by the player's
        /// input (`0x410`, `0x450` in mouse mode, `0x10` for a held repeat); a restart the
        /// server issues by itself carries the last request's flags without it.
        flags: u16,
        action: u16,
        cell: Cell,
        target: Cell,
        entity: Option<u32>,
        unk: u8,
    },
    /// `0x1ca`, no body (with every roll start and when an attack ends a sprint).
    ActionEvent,
    /// `0x1d3` StatsUpdate (set 0).
    Stats(Vec<(MoveStat, i32)>),
    /// `0x1ae` UseSkillAck: the request echoed with the absolute target; the session sets the
    /// "confirmed" target flag (`0x04`).
    SkillAck {
        skill: u32,
        flags: u16,
        unk: u8,
        target_flags: u8,
        entity: Option<u32>,
        x: i32,
        y: i32,
    },
    /// `0x1af` UpdateSkillTargetAck: absolute target.
    TargetAck { skill: u32, x: i32, y: i32 },
    /// `0x1b1` SkillInputEndAck.
    InputEndAck,
    /// `0x1b2` SkillReleasedAck.
    ReleasedAck { skill: u32, flag: u8 },
    /// `0x2e7` SprintState: the echo of the sprint key.
    SprintState { held: bool },
    /// `0x1eb` BuffAdded; `fade` is the optional fade time (flag `0x02`).
    BuffAdded {
        index: u16,
        row: u16,
        fade: Option<f32>,
    },
    /// `0x1ec` BuffRemoved.
    BuffRemoved { index: u16 },
    /// `0x1dc` SetAimTarget kind 0: the cursor cell echoed.
    AimEcho { cell: Cell },
    /// `0x2f9` ComponentDelta of the Player component: the combat stance (`0x0400`) and, when
    /// the client asked with `0x200`, its value (`0x1000`).
    StanceChanged { combat: bool, request: Option<u8> },
    /// `0x2a6`: sent before every stance delta (0.05 to combat, 0.2 out of it).
    OrientationLerp { value: f32 },
    /// `0x1d5` SetWeaponSet. `requested`: the answer to [`MoveInput::WeaponSwap`] in the batch
    /// answering it, followed by the session message `0xff` WeaponSetSwapDone (3 of 3 official
    /// swaps, standing). Not requested: a skill of the other set was used, the server swaps
    /// by itself (2026-11-02 capture, 2 of 2: no `0xff`, see [`SkillIds::weapon_set_of`]).
    WeaponSet { set: u8, requested: bool },
    /// The entity was placed by the server (admin command, transfer). The client only snaps
    /// when the distance is 46 cells or more, and only for a `0x64` with a non-zero direction.
    Teleport { cell: Cell },
}


/// Where in the client's frame a message runs: its header order key (`+0x14`, `+0x18`, `+0x1a`).
/// The client drains the head of its frame queue at each position of `0x142130360` (hub
/// `docs/re/wasd-movement.md`, "Frame order"), so a frame's messages must go out sorted by this key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Drain {
    /// Order key 0: before all entities.
    Start,
    /// Keyed to a component of the entity: before its tick, later ones as `seq` is reached (the
    /// frame's phase counter starts at 0 per component).
    Component { comp: u8, seq: u16 },
    /// Order key `0xffffffff`: after all entities.
    End,
}

impl Drain {
    pub const fn at(comp: u8, seq: u16) -> Self {
        Self::Component { comp, seq }
    }

    /// Where an event runs unless its sender says otherwise.
    pub fn default_for(event: &MoveEvent) -> Self {
        match event {
            MoveEvent::MoveAck { .. } | MoveEvent::Teleport { .. } => Self::at(9, 0),
            MoveEvent::StanceChanged { .. } => Self::End,
            _ => Self::Start,
        }
    }
}

/// A [`MoveEvent`] with its place in the client's frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Keyed {
    pub key: Drain,
    pub event: MoveEvent,
}

impl From<MoveEvent> for Keyed {
    fn from(event: MoveEvent) -> Self {
        Self {
            key: Drain::default_for(&event),
            event,
        }
    }
}

/// A character's movement facts from game data, set at player setup (players of different
/// classes share one world, so each entity carries its own).
#[derive(Debug, Clone, PartialEq)]
pub struct MovementConfig {
    pub skills: SkillIds,
    /// Walking speed, cells per second (the class's `Pathfinding.base_speed`).
    pub walk_speed: f32,
    /// The character's stats that change its speed.
    pub speed_stats: SpeedStats,
    /// The class's `dodge_roll_sprint_turn_rate`, degrees per second.
    pub sprint_turn_rate: f32,
    /// The roll skill's `dodge_roll_base_travel_distance`, cells.
    pub roll_travel_distance: f32,
    /// The class's `sprint_movement_speed_+%` (`None`: not in its stats).
    pub sprint_bonus_percent: Option<f32>,
    /// The larger of the class's `Positioned` `object_size` and `secondary_object_size`: the
    /// entity's path clearance from the end of a roll until the sprint ends.
    pub sprint_clearance_level: i32,
    /// `BuffDefinitions` rows of the three buffs a sprint adds (`[movement] sprint_buff_rows`).
    pub sprint_buff_rows: [u16; 3],
    /// The roll and sprint-end curves (required at player setup; `None` in tests).
    pub curves: Option<Arc<MotionCurves>>,
    /// The class's `offset_scale` (`.ao`), scales root motion and the animation's rate.
    pub offset_scale: f32,
    /// Frames between the client entering aim mode at login and the stance change (official: 8).
    pub stance_delay_frames: u32,
    /// The same after an area change (official: 3).
    pub area_stance_delay_frames: u32,
}

impl MovementConfig {
    /// A character with these skills walking at 37 cells per second, without curves.
    pub fn new(skills: SkillIds, sprint_buff_rows: [u16; 3]) -> Self {
        Self {
            skills,
            walk_speed: 37.0,
            speed_stats: SpeedStats::default(),
            sprint_turn_rate: 150.0,
            roll_travel_distance: 46.0,
            sprint_bonus_percent: Some(50.0),
            sprint_clearance_level: 0,
            sprint_buff_rows,
            curves: None,
            offset_scale: 1.0,
            stance_delay_frames: 8,
            area_stance_delay_frames: 3,
        }
    }
}

/// The character's stats that change its speed. The client reads only the results
/// (`Pathfinding_GetSpeed` `0x141dccfa0`: `movement_velocity_+permyriad`, `virtual_action_speed_+%`,
/// `movement_velocity_cap`), which the server computes and sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpeedStats {
    /// The sum of the increases (`base_movement_velocity_+%` and the like); adds to the sprint's.
    pub increased_percent: i32,
    /// `combined_movement_velocity_+%_final`.
    pub final_percent: i32,
    /// `virtual_action_speed_+%`.
    pub action_speed_percent: i32,
    /// `movement_velocity_cap`; 0: none.
    pub velocity_cap: i32,
}

impl SpeedStats {
    /// `movement_velocity_+permyriad` with this sprint bonus: the official server's whole percents,
    /// `(1 + (increased + sprint) / 100) * (1 + final / 100) - 1`. All official values fit: final -4
    /// walks at -400 and sprints at 4400, final -3 at -300 and 4600 (1.455), increased 10 with final
    /// -4 at 600 and 5400. These cannot tell rounding from rounding up.
    pub fn permyriad(&self, sprint_percent: i32) -> i32 {
        let p = (100 + self.increased_percent + sprint_percent) * (100 + self.final_percent) - 10000;
        (p + 50).div_euclid(100) * 100
    }

    /// Nothing changes the speed: the official sprint stats carry `base_move_speed_+%_from_sprint`
    /// and the blend-time stat only then (64 of 64, and none of the 15 others).
    pub fn is_plain(&self) -> bool {
        self.increased_percent == 0 && self.final_percent == 0
    }
}

/// Per-frame facts a mover gets from the world.
pub struct StepCtx<'a> {
    /// The frame (batch) being produced.
    pub frame: u64,
    /// Seconds per frame (`tick_ms / 1000`).
    pub dt: f32,
    pub terrain: &'a dyn Walkable,
    pub config: &'a MovementConfig,
    /// World-wide action id counter (`StartAction` ids are shared by all entities).
    pub next_action: &'a mut u16,
    /// The entity's next free buff index.
    pub next_buff: &'a mut u16,
}

/// One entity's movement.
pub trait Mover: Send + std::fmt::Debug {
    /// Client input; answered by the next [`tick`](Self::tick).
    fn input(&mut self, input: MoveInput);

    /// The server places the entity (admin command, spawn fix-up); the next tick reports it
    /// with [`MoveEvent::Teleport`].
    fn teleport(&mut self, cell: Cell);

    /// One frame: what the clients must be told, in send order for equal keys.
    fn tick(&mut self, ctx: &mut StepCtx) -> Vec<Keyed>;

    /// The character's speed stats changed (gear, a buff, an admin command): the next tick sends the
    /// results to the clients and moves by them.
    fn set_speed_stats(&mut self, _stats: SpeedStats) {}

    /// One frame while the world is paused (solo escape menu): nothing moves, inputs wait.
    fn tick_paused(&mut self, _ctx: &mut StepCtx) -> Vec<Keyed> {
        Vec::new()
    }

    /// The entity's cell for the rest of the server (spawn, transfer, leave, remote blobs).
    fn cell(&self) -> Cell;

    /// The exact position in cells (position sync, drift checks, mods).
    fn position(&self) -> (f32, f32) {
        let (x, y) = self.cell();
        (x as f32, y as f32)
    }

    /// The facing the client model holds (radians, Positioned `+0x44c`; tests).
    fn facing(&self) -> f32 {
        0.0
    }

    /// The client grid cell the model holds for the entity (movement trace `pos` lines).
    fn grid(&self) -> Cell {
        self.cell()
    }

    /// Sets the stance directly (a player whose entry stance is already done); later aim-mode
    /// inputs are then input-mode toggles.
    fn set_stance(&mut self, _combat: bool) {}

    /// Sets the active weapon set (restored state).
    fn set_weapon_set(&mut self, _set: u8) {}

    /// Whether the character is in combat stance (remote blobs of late joiners).
    fn in_combat_stance(&self) -> bool {
        false
    }

    /// State for the DLL's debug link, as `(key, value)`; `state` is the overlay's headline.
    fn debug_fields(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }
}

/// A movement implementation: makes one [`Mover`] per entity.
pub trait MovementModel: Send + Sync {
    /// Name for logs.
    fn name(&self) -> &'static str;

    /// A mover for an entity standing on `cell`.
    fn spawn(&self, cell: Cell, config: &MovementConfig) -> Box<dyn Mover>;
}
