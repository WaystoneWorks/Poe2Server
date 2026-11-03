//! The movement component of a player entity: server-authoritative movement for the client's
//! lockstep mode.
//!
//! The client starts nothing on its own. It sends its input (WASD vector, skill use, sprint
//! key, aim), the server answers each input in the next frame batch and simulates the same
//! movement itself so it knows where the player is. [`Locomotion`] is that server side for one
//! entity: [`MoveInput`] in, [`MoveEvent`] out, one [`Locomotion::tick`] per frame. The
//! sequences, frame offsets and counts are the official ones of
//! `docs/protocol/movement.md`; scenarios the captures
//! do not cover are marked `UNVERIFIED (gap #n)`, numbered as in the gap list there.
//!
//! No wire formats here: [`MoveEvent`] is semantic (the instance server maps it to packets),
//! and skill ids and buff rows come in through [`MovementConfig`].
//!
//! # Frame model
//! `tick` is one server frame (batch). Inside it, in this order:
//! 1. the entity moves one frame (so a movement started by a reply in frame `F` first moves in
//!    `F + 1`, as on the client),
//! 2. the queued inputs are handled in arrival order and answer directly (`r + 0`: the first
//!    tick after an input arrived),
//! 3. timeline events that are due run (roll end, sprint start/stages),
//! 4. the `MoveAck` for a direction change or the sprint stream (one per frame),
//! 5. the sprint end, if the key was released.

use crate::movement::{Motion, Mover, Pathfinder, Walkable};

pub type Cell = (i32, i32);

/// The client's time per simulation frame: its steps are `speed / 30` (1.233 cells at 37 cells
/// per second, measured), whatever `tick_ms` says.
pub const CLIENT_FRAME_SECS: f32 = 1.0 / 30.0;

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

/// Dodge roll tuning (official values as defaults).
#[derive(Debug, Clone, PartialEq)]
pub struct RollConfig {
    /// `dodge_roll_travel_distance`, cells.
    pub travel_distance: f32,
    /// Frames the roll action lasts (aim is not echoed, a new roll waits, WASD is acted on
    /// afterwards). Also the frame a roll that became a sprint ends.
    pub action_frames: u32,
    /// Frames after the roll start at which a queued attack cuts the roll short and starts
    /// (2026-10-20 capture: 24 in all 3 cases; the client's roll action ends then too).
    pub attack_cancel_frames: u32,
    /// Frames after the roll start at which a tap roll (key up before the sprint starts)
    /// clears `is_dodge_rolling`; the action itself goes on until `action_frames`.
    pub tap_end_frames: u32,
    /// Value of the three stun threshold stats while rolling.
    pub stun_threshold: i32,
}

/// Click-to-move tuning (official values as defaults).
#[derive(Debug, Clone, PartialEq)]
pub struct ClickConfig {
    /// While the click is held, the server re-issues the Move `StartAction` towards the latest
    /// target when this many frames passed since the hold's last Move start with no new
    /// press or repeat in between. The client repeats every 2 frames but skips a beat about
    /// every 15 (its own timing), which is where the official restarts come: all 52 restarts
    /// of the 5 held clicks in the 2026-10-20 and 2026-11-02 captures, 2 frames after the
    /// press' answer when the first repeat is late, never while a repeat keeps the pace.
    pub restart_frames: u32,
}

/// Sprint tuning (official values as defaults).
#[derive(Debug, Clone, PartialEq)]
pub struct SprintConfig {
    /// Buff rows added when the sprint starts (indices `n`, `n+1`, `n+2`): from game data.
    pub buff_rows: [u16; 3],
    /// Fade time of the first buff when the roll ends.
    pub buff_fade_secs: f32,
    /// Frames after the roll start at which Space still held starts the sprint.
    pub start_frames: u32,
    /// Frames after the roll start at which `must_sprint` goes off.
    pub must_sprint_end_frames: u32,
    /// Walking speed multiplier while sprinting.
    pub speed_scale: f32,
    /// Direction vectors in sprint replies are the input scaled to this length and truncated
    /// toward zero (official: 550; measured on 294 of 309 replies, the rest lag one input).
    pub dir_length: i32,
    /// Clearance level of the entity's path searches from the end of the roll until the sprint
    /// ends: the client sets the entity's size to the class' `Positioned.secondary_object_size`
    /// (4) then, so cells whose grid value is at most that count as walls and the sprint
    /// keeps 5 to 10 cells off walls (`docs/re/pathfinding.md`).
    pub clearance_level: i32,
    /// `base_move_speed_+%_from_sprint`.
    pub move_speed_percent: i32,
    /// `movement_velocity_+permyriad`.
    pub velocity_permyriad: i32,
    /// `sprint_blend_time_override_ms`.
    pub blend_time_override_ms: i32,
    /// After a sprint ends the client's Move action is over, and without a new walk the entity
    /// coasts along its last direction: the first step is `coast_start_scale` x the walking
    /// step of a client frame (`walk_speed` / 30), falling linearly to zero over `coast_frames`
    /// frames (2026-10-20 official sprint ends at f1795 and f4585 and six sessions of ours:
    /// steps 1.309 - 0.0444 n cells per frame, n = 0..29, 19.95 cells in all; walking is 1.233
    /// per frame).
    pub coast_start_scale: f32,
    pub coast_frames: f32,
    /// Frames between the sprint-off batch and the first coasting step (the batch's own frame
    /// still has the sprint step, the next one none).
    pub coast_delay_frames: u32,
    /// Frames between the sprint-off batch and the end of the client's `SprintEnd` animation.
    /// The sprint's end (`0x141b773a0`) ends the Move action, stops the entity and plays
    /// `SprintEnd` (animation `0x36a`; `SprintEndBonk` `0x36b` at a wall); while it plays
    /// `Actor_StartWasdWalk` (`0x141e4fb40`) starts no walk, neither for the unscaled `MoveAck`
    /// nor for a held or new key. The Actor tick calls it every frame, so a key held at the
    /// animation's end walks on at once: Move at batch + 31, first step one frame later (three
    /// runs of ours with W held since before the roll; 21 once with a stream, not explained).
    pub end_anim_frames: u32,
    /// The values above are the walking-speed case. A sprint ended by the release batch (the
    /// `SprintState` echo with the sprint-off stats) plays `SprintEnd` at sprint speed on the
    /// private client (`speed_scale` times faster: coast and animation 1/`speed_scale` as long,
    /// same distance); the official client did not (2026-10-20 captures, walking speed), most
    /// likely because of the end check's Player `+0x20a` branch (`0x141b76f01`). See
    /// `Locomotion::sprint_end_rate`.
    pub release_ends_at_sprint_speed: bool,
    /// A zero WASD vector this many frames after the sprint-off batch, after a sprint that
    /// followed the cursor (Space held, no key), means the keys were already up when the
    /// unscaled `MoveAck` reached the client, which then coasted instead of walking on (the
    /// client sends the zero vector 1 to 5 frames after the Space release, 4 to 5 with an
    /// exactly diagonal cursor). In click-to-move mode, where there are no keys, the sprint
    /// coasts at once and this is how long cursor vectors still in flight are ignored.
    pub release_window_frames: u32,
    /// A key-shaped `0x63` vector (axis or exact diagonal) counts as the cursor direction when
    /// the roll key's cursor offset points within this many degrees of it (the client's cursor
    /// vector and the roll target differ by up to about 6 degrees).
    pub cursor_match_degrees: f32,
}

/// How an attack slows walking (official values as defaults; `docs/protocol/movement.md`,
/// "Action speed factor"). The client ramps the Actor's speed factor (`+0x440` towards `+0x448`
/// at `+0x444` per second, `0x141e441ab`) from the running action's
/// `1 + movement_speed_+%_final_while_performing_action / 100 x (1 + virtual_movement_speed_penalty_% / 100)`
/// (`0x14197ea40`) at `movement_speed_acceleration_+%_per_second_while_performing_action / 100`
/// (`0x14197eb10`), and back to 1 once `movement_speed_while_performing_action_locked_duration_%`
/// of the action has passed (`0x141e43fdb`). The numbers are measured on the client's own steps
/// (2026-10-20 capture, 4 attacks while walking): the attack skill's stats come from the weapon,
/// which the server does not model yet.
#[derive(Debug, Clone, PartialEq)]
pub struct AttackSpeedConfig {
    /// The factor the attack slows walking to (measured 0.28-0.31).
    pub floor: f32,
    /// Ramp rate of the factor, per second (measured 0.050-0.054 per 33 ms frame).
    pub ramp_per_second: f32,
    /// Frames after the attack's `StartAction` at which the factor drops to `floor` at once
    /// (7 in all 4 cases; until then it ramps down). The client snaps at the attack
    /// animation's contact event (`0x141a3be21`), which the server does not model.
    pub snap_frames: u32,
    /// Frames after the attack's `StartAction` from which the factor ramps back to 1 at the
    /// same rate (the first higher step is the frame after; 12 in 3 of 4 cases, 16 once). The
    /// client ramps back once `movement_speed_while_performing_action_locked_duration_%` of
    /// the action has passed, or when the action ends (`0x141e43f51`, `0x141e455b8`).
    pub lock_frames: u32,
}

/// Everything the component needs to know about the game (shared by all entities of a world).
#[derive(Debug, Clone, PartialEq)]
pub struct MovementConfig {
    pub skills: SkillIds,
    pub roll: RollConfig,
    pub sprint: SprintConfig,
    pub click: ClickConfig,
    pub attack_speed: AttackSpeedConfig,
    /// Characters' walking speed, cells per second.
    pub walk_speed: f32,
    /// Which pathfinder plans walks, rolls and sprints.
    pub pathfinder: Pathfinder,
    /// Frames between the client entering aim mode (`AimMode(1)`) at entry and the stance
    /// change.
    pub stance_delay_frames: u32,
    /// The same after an area change: the client then sends `AimMode(0)` and `AimMode(1)` in
    /// one batch (2026-11-02 capture, 10 waypoint and transition loads: 3 frames 7 times, 2
    /// once, 4 three times; the 9-14 frames of a login are `stance_delay_frames`). The delay
    /// follows the area load on the official server.
    pub area_stance_delay_frames: u32,
    /// Frames between a later input-mode toggle (`AimMode(0|1)` with the stance request
    /// `0x200`, mouse mode and back) and the stance change, counted from the batch that
    /// answers the input (2026-10-20 capture: 0, the same batch).
    pub stance_toggle_frames: u32,
    /// `0x2a6` value sent with a change to combat stance (all eight sessions: 0.05).
    pub stance_lerp_combat: f32,
    /// `0x2a6` value sent with a change out of combat stance (all eight sessions: 0.2).
    pub stance_lerp_peaceful: f32,
}

impl MovementConfig {
    /// The official values of the captures; `buff_rows` are the three sprint buff rows
    /// from the buff definitions (game data).
    pub fn new(skills: SkillIds, buff_rows: [u16; 3]) -> Self {
        Self {
            skills,
            roll: RollConfig {
                travel_distance: 46.0,
                action_frames: 30,
                attack_cancel_frames: 24,
                tap_end_frames: 16,
                stun_threshold: 100,
            },
            click: ClickConfig {
                restart_frames: 2,
            },
            attack_speed: AttackSpeedConfig {
                floor: 0.3,
                ramp_per_second: 1.5,
                snap_frames: 7,
                lock_frames: 12,
            },
            sprint: SprintConfig {
                buff_rows,
                buff_fade_secs: 0.1,
                start_frames: 15,
                must_sprint_end_frames: 35,
                speed_scale: 1.5,
                dir_length: 550,
                clearance_level: 4,
                move_speed_percent: 50,
                velocity_permyriad: 5000,
                blend_time_override_ms: 1,
                coast_start_scale: 1.0614,
                coast_frames: 29.5,
                coast_delay_frames: 1,
                end_anim_frames: 31,
                release_ends_at_sprint_speed: true,
                release_window_frames: 6,
                cursor_match_degrees: 10.0,
            },
            walk_speed: 37.0,
            pathfinder: Pathfinder::Client,
            stance_delay_frames: 8,
            area_stance_delay_frames: 3,
            stance_toggle_frames: 0,
            stance_lerp_combat: 0.05,
            stance_lerp_peaceful: 0.2,
        }
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

/// How a walk is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Walk {
    /// WASD: keep going along this vector.
    Direction(Cell),
    /// Click-to-move: walk to a cell along an A* path ([`Locomotion::path`]).
    Target { goal: Cell },
}

/// A click that is being held (between the press and `SkillReleased` of the Move skill).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveHold {
    /// Absolute cell of the latest press or repeat.
    pub goal: Cell,
    /// Flags of the latest request (press `0x400`, repeats `0`): restarts carry them.
    pub flags: u16,
    /// Press counter; stale restarts of an earlier hold are ignored.
    seq: u64,
    /// Frame of the latest press or repeat. Repeats come every 2 frames; a hold that is
    /// quiet for longer than [`MoveHold::STALE_FRAMES`] is over (5 of 12 holds in the
    /// 2026-10-20 capture ended without a `SkillReleased`).
    last_input: u64,
    /// Frame of the hold's latest Move start (press, repeat or restart).
    last_start: u64,
    /// The latest three goals with the frame each was handled, newest first: the
    /// `StartAction` at `must_sprint` off takes an older one ([`MoveHold::settled_goal`]).
    history: [(u64, Cell); 3],
}

impl MoveHold {
    pub const STALE_FRAMES: u64 = 6;

    /// The newest goal handled at least 2 frames before `frame` (`older` = 0), or the one
    /// before that (`older` = 1). Official, a sprint that goes on with a held click
    /// (2026-10-20 f3779, f4141, f4252, f4522; 2026-10-20_21-51-49 f8873, f9154; 2026-11-02
    /// f7746): a repeat handled 0 or 1 frame before `must_sprint` off is not in its
    /// `StartAction` yet (`older` 0, 7 of 7). The one sprint that ended there by an earlier
    /// release (2026-10-20 f3643) took the goal before that (`older` 1).
    fn settled_goal(&self, frame: u64, older: usize) -> Cell {
        let i = self
            .history
            .iter()
            .position(|(f, _)| f + 2 <= frame)
            .unwrap_or(self.history.len() - 1);
        self.history[(i + older).min(self.history.len() - 1)].1
    }
}

/// A running sprint (the buffs are added at roll start + 15 frames).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SprintRun {
    /// Frame of the roll that started it (`t0`).
    pub start_frame: u64,
    /// Buff index `n`; the sprint uses `n`, `n + 1`, `n + 2`.
    pub buff_base: u16,
    /// A non-zero WASD vector arrived after the sprint started: one `MoveAck` per frame.
    pub streaming: bool,
}

/// A dodge roll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rolling {
    pub start_frame: u64,
    /// The sprint key is down.
    pub held: bool,
    /// Cursor offset the roll runs towards when no WASD key is held (relative to the cell it
    /// started from); steered by `UpdateSkillTarget`.
    pub offset: Cell,
    /// A WASD key was held when the roll was asked for: the roll runs along the WASD vector
    /// (the client ignores the cursor then) and follows its changes (2026-10-20 capture:
    /// 14 of 14 rolls with WASD held, direction changes 2 frames after the `0x63`).
    pub wasd_driven: bool,
    pub action: u16,
    /// The roll turned into a sprint (the key was held at `start_frame + 15`).
    pub sprint: Option<SprintRun>,
    /// A sprint was started during this roll (even if it has ended again).
    pub sprinted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveState {
    Idle,
    Walking(Walk),
    Rolling(Rolling),
    /// The roll is over and the sprint goes on, until the key is released or an attack.
    Sprinting(SprintRun),
}

/// The frame a sprint ended by the key release (or the roll's end after an earlier release) and
/// the direction it ran in.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SprintEnd {
    frame: u64,
    dir: Cell,
    /// The sprint's last vector was the cursor direction, not a key (see
    /// [`Locomotion::is_cursor_vector`]): the keys are up, so the client coasts.
    cursor: bool,
    /// The sprint coasted at once, without waiting for the client's zero vector: click-to-move
    /// mode, where there are no keys at all.
    coasting: bool,
    /// A held click's Move ran the sprint: its end answered with `(0, 0)` and the cursor
    /// vectors still in flight are not answered.
    click: bool,
}

/// Scheduled timeline events.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Due {
    /// Roll `roll` starts its sprint if the key is still down.
    SprintStart {
        roll: u64,
    },
    /// A tap roll clears `is_dodge_rolling` (skipped when the roll became a sprint).
    RollEndTap {
        roll: u64,
    },
    /// The roll action is over.
    RollEnd {
        roll: u64,
    },
    /// `must_sprint` off; the buff index is in [`Locomotion::must_sprint_off`].
    MustSprintOff,
    /// The reply of the frame after a sprint ended: the unscaled direction.
    UnscaledAck,
    Stance(bool),
    /// Starts [`Locomotion::deferred_roll`].
    StartDeferredRoll,
    /// The stun stats of a roll whose timeline runs one frame behind its `StartAction`.
    RollStunStats,
    /// The roll `roll` lets go of the player: a queued attack starts (a roll that is held into
    /// a sprint waits for the sprint's end).
    AttackWindow {
        roll: u64,
    },
    /// The client's `SprintEnd` animation of the sprint that ended in frame `end` is over: a
    /// held key walks on.
    SprintEndAnimOver {
        end: u64,
    },
    /// Re-issues the Move `StartAction` of a held click.
    MoveRestart {
        seq: u64,
    },
}

/// Per-frame facts the component gets from the world.
pub struct StepCtx<'a> {
    pub frame: u64,
    /// Seconds per frame.
    pub dt: f32,
    pub terrain: &'a dyn Walkable,
    pub config: &'a MovementConfig,
    /// World-wide action id counter (`StartAction` ids are shared by all entities).
    pub next_action: &'a mut u16,
    /// The entity's next free buff index.
    pub next_buff: &'a mut u16,
}

/// The movement component.
#[derive(Debug, Clone)]
pub struct Locomotion {
    mover: Mover,
    state: MoveState,
    wasd: Cell,
    held: bool,
    /// A direction input arrived this frame: answer it.
    ack_wanted: bool,
    inbox: Vec<MoveInput>,
    due: Vec<(u64, Due)>,
    /// Buff index of the `must_sprint` buff while it is on.
    must_sprint_off: Option<u16>,
    combat_stance: bool,
    stance_scheduled: bool,
    /// The stance at entry has been set; later `AimMode` inputs are input-mode toggles.
    stance_entered: bool,
    /// Frame of an `AimMode(0)` before the entry stance (an area change sends 0 then 1).
    entry_mode0_frame: Option<u64>,
    /// The active weapon set.
    weapon_set: u8,
    /// An attack with a weapon swap ends the sprint after this frame's `MoveAck` (still
    /// scaled); `Some(streaming)`.
    attack_ends_sprint_after_ack: Option<bool>,
    /// The value of the last `0x200` not yet answered.
    stance_request: Option<u8>,
    /// The last `AimMode` asked for combat stance (mode 1).
    aim_combat: bool,
    teleport: Option<Cell>,
    /// A roll asked for while the previous roll's action still runs: it starts when that ends.
    queued_roll: Option<(UseSkill, Cell)>,
    /// Events that follow this frame's `MoveAck`.
    deferred: Vec<MoveEvent>,
    /// The sprint key is released in the batch being handled (set by `tick`).
    batch_release: bool,
    /// A roll whose key was already up when it was handled: acked now, started next frame.
    deferred_roll: Option<(UseSkill, Cell)>,
    /// The last attack asked for during a roll (acked already).
    queued_attack: Option<(UseSkill, Cell)>,
    /// The Move click being held.
    move_hold: Option<MoveHold>,
    hold_seq: u64,
    /// The sprint key was released before `must_sprint` went off: the sprint ends then.
    release_pending: bool,
    /// ... and it is `t0 + 35` now: end it after this frame's `MoveAck`.
    release_due: bool,
    /// An attack that starts after this frame's `MoveAck` (see `MustSprintOff`).
    attack_after_ack: Option<(UseSkill, Cell)>,
    /// A held click's Move that starts after this frame's `MoveAck` (see `MustSprintOff`).
    move_after_ack: Option<Cell>,
    /// A held click's Move runs during the sprint (started at or after `must_sprint` off): the
    /// sprint's end needs no `StartAction` of its own.
    sprint_move: bool,
    /// Frame of the latest Move `StartAction` answering a click input.
    move_start_frame: Option<u64>,
    /// The next roll started is one deferred by a click's Move: its timeline runs one frame
    /// behind its `StartAction` (see `on_use_skill`).
    roll_late: bool,
    /// The sprint that just ended by key release, see [`SprintEnd`].
    sprint_end: Option<SprintEnd>,
    /// The client's `SprintEnd` animation runs until this frame: no WASD walk starts before it
    /// (see [`SprintConfig::end_anim_frames`]).
    sprint_anim_until: Option<u64>,
    /// Play rate of the client's `SprintEnd` relative to the walking-speed case (see
    /// [`Locomotion::sprint_end_rate`]): scales the coast and the animation's length.
    sprint_end_rate: f32,
    /// The latest `0x63` was sent in click-to-move mode ([`MoveInput::CursorDirection`]).
    cursor_mode: bool,
    /// The cursor's offset from the player, as the roll key's target last said (`0x1a5`,
    /// `0x1a8`): the direction the client sends as `0x63` with Space held and no key.
    roll_cursor: Cell,
    /// The direction of the unscaled `MoveAck` due in the frame after a sprint ended: it is
    /// still sent first when the client's zero vector arrives in that very frame.
    unscaled_dir: Option<Cell>,
    /// The direction the entity last moved or aimed in: the client rolls along it when the
    /// roll has no target (cursor on the player) and no key is held.
    facing: Cell,
    /// Start frame of the attack whose speed factor is still in effect ([`AttackSpeedConfig`]).
    attack_slow: Option<u64>,
}

impl Locomotion {
    /// An idle entity at `cell`, walking `speed` cells per second.
    pub fn new(cell: Cell, speed: f32) -> Self {
        Self {
            mover: Mover::at_cell(cell, speed),
            state: MoveState::Idle,
            wasd: (0, 0),
            held: false,
            ack_wanted: false,
            inbox: Vec::new(),
            due: Vec::new(),
            must_sprint_off: None,
            combat_stance: false,
            stance_scheduled: false,
            stance_entered: false,
            entry_mode0_frame: None,
            weapon_set: 0,
            attack_ends_sprint_after_ack: None,
            stance_request: None,
            aim_combat: true,
            teleport: None,
            queued_roll: None,
            deferred: Vec::new(),
            batch_release: false,
            deferred_roll: None,
            queued_attack: None,
            move_hold: None,
            hold_seq: 0,
            attack_after_ack: None,
            move_after_ack: None,
            sprint_move: false,
            move_start_frame: None,
            roll_late: false,
            release_pending: false,
            release_due: false,
            sprint_end: None,
            sprint_anim_until: None,
            sprint_end_rate: 1.0,
            cursor_mode: false,
            roll_cursor: (0, 0),
            unscaled_dir: None,
            facing: (0, 0),
            attack_slow: None,
        }
    }

    /// Queues an input; it is handled in the next [`tick`](Self::tick).
    pub fn input(&mut self, input: MoveInput) {
        self.inbox.push(input);
    }

    /// Places the entity at `cell` in the next tick and stops it.
    pub fn teleport(&mut self, cell: Cell) {
        self.teleport = Some(cell);
    }

    pub fn state(&self) -> &MoveState {
        &self.state
    }

    /// The grid cell the entity stands in.
    pub fn cell(&self) -> Cell {
        self.mover.cell()
    }

    /// The client's own grid cell as the server models it ([`crate::movement::Mover::grid`]).
    pub fn grid(&self) -> Cell {
        self.mover.grid()
    }

    /// Exact position in cells.
    pub fn position(&self) -> (f32, f32) {
        (self.mover.x, self.mover.y)
    }

    /// The latest WASD vector.
    pub fn wasd(&self) -> Cell {
        self.wasd
    }

    /// The sprint key is down.
    pub fn sprint_held(&self) -> bool {
        self.held
    }

    /// Sets the stance directly (a player whose entry stance is already done, e.g. restored
    /// or in tests); later `AimMode` inputs are then input-mode toggles.
    pub fn set_stance(&mut self, combat: bool) {
        self.combat_stance = combat;
        self.stance_entered = true;
    }

    /// Sets the active weapon set (restored state, tests).
    pub fn set_weapon_set(&mut self, set: u8) {
        self.weapon_set = set;
    }

    pub fn in_combat_stance(&self) -> bool {
        self.combat_stance
    }

    /// Waypoints left of a click-to-move walk (cells).
    pub fn path(&self) -> &[Cell] {
        self.mover.path()
    }

    pub fn sprint(&self) -> Option<&SprintRun> {
        match &self.state {
            MoveState::Rolling(r) => r.sprint.as_ref(),
            MoveState::Sprinting(s) => Some(s),
            _ => None,
        }
    }

    fn sprint_mut(&mut self) -> Option<&mut SprintRun> {
        match &mut self.state {
            MoveState::Rolling(r) => r.sprint.as_mut(),
            MoveState::Sprinting(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_rolling(&self) -> bool {
        matches!(self.state, MoveState::Rolling(_))
    }

    /// One server frame; see the module docs for the order.
    /// A frame while the world is paused: only the stance timer runs (the official entry
    /// stance goes out while the client's post-load pause is on, five sessions).
    pub fn tick_paused(&mut self, ctx: &mut StepCtx) -> Vec<MoveEvent> {
        let mut out = Vec::new();
        let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.due)
            .into_iter()
            .partition(|(f, d)| *f <= ctx.frame && matches!(d, Due::Stance(_)));
        self.due = later;
        let mut unscaled = false;
        for (_, due) in now {
            self.fire(due, ctx, &mut out, &mut unscaled);
        }
        out
    }

    pub fn tick(&mut self, ctx: &mut StepCtx) -> Vec<MoveEvent> {
        let mut out = Vec::new();
        // 1. Move. The client's pathing size grows while the sprint runs after the roll.
        self.mover.pathfinder = ctx.config.pathfinder;
        self.mover.level = if matches!(self.state, MoveState::Sprinting(_)) {
            ctx.config.sprint.clearance_level
        } else {
            0
        };
        self.update_speed_factor(ctx);
        self.mover.advance(ctx.dt, ctx.terrain);
        if matches!(self.state, MoveState::Walking(Walk::Target { .. })) && !self.mover.is_moving()
        {
            self.state = MoveState::Idle;
        }
        if let Some(cell) = self.teleport.take() {
            self.mover.place(cell);
            self.state = MoveState::Idle;
            self.wasd = (0, 0);
            self.ack_wanted = false;
            out.push(MoveEvent::Teleport { cell });
        }
        // 2. Inputs.
        let mut end_sprint = false;
        let inbox = std::mem::take(&mut self.inbox);
        self.batch_release = inbox
            .iter()
            .any(|i| matches!(i, MoveInput::SprintInput { held: false }));
        for input in inbox {
            self.handle(input, ctx, &mut out, &mut end_sprint);
        }
        // 3. Timeline.
        let mut unscaled_ack = false;
        let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.due)
            .into_iter()
            .partition(|(f, _)| *f <= ctx.frame);
        self.due = later;
        for (_, due) in now {
            self.fire(due, ctx, &mut out, &mut unscaled_ack);
        }
        // 4. MoveAck, then what follows it.
        self.move_ack(unscaled_ack, ctx, &mut out);
        out.append(&mut self.deferred);
        if let Some(streaming) = self.attack_ends_sprint_after_ack.take() {
            // Official f11798: `0x64 (0, 180)` unscaled at r+1.
            if streaming && self.wasd != (0, 0) {
                self.due.push((ctx.frame + 1, Due::UnscaledAck));
                self.unscaled_dir = Some(self.wasd);
            }
            self.finish_sprint();
        }
        if let Some(target) = self.move_after_ack.take() {
            out.push(MoveEvent::ActionStart {
                kind: ActionKind::Move,
                skill: ctx.config.skills.move_skill,
                flags: ACTION_INPUT_FLAG,
                action: Self::alloc_action(ctx),
                cell: self.mover.cell(),
                target,
                entity: None,
                unk: 0xff,
            });
        }
        if let Some((req, target)) = self.attack_after_ack.take() {
            self.start_attack(req, target, false, ctx, &mut out);
            out.append(&mut self.deferred);
        }
        // 5. Sprint end. `end_sprint` alone: this batch also carries the `SprintState` echo.
        let in_release_batch = end_sprint;
        end_sprint |= std::mem::take(&mut self.release_due);
        if end_sprint {
            self.sprint_end_rate = Self::sprint_end_rate(in_release_batch, ctx);
            self.end_sprint(ctx, &mut out);
        }
        out
    }

    // ---- inputs

    fn handle(
        &mut self,
        input: MoveInput,
        ctx: &mut StepCtx,
        out: &mut Vec<MoveEvent>,
        end_sprint: &mut bool,
    ) {
        match input {
            MoveInput::Direction { x, y } => self.on_direction((x, y), false, ctx),
            MoveInput::CursorDirection { x, y } => self.on_direction((x, y), true, ctx),
            MoveInput::UseSkill(req) => self.on_use_skill(req, ctx, out),
            MoveInput::UpdateSkillTarget { x, y, skill } => {
                let (cx, cy) = self.mover.cell();
                let abs = (cx + x, cy + y);
                self.steer(skill, (x, y), abs, ctx);
                out.push(MoveEvent::TargetAck {
                    skill,
                    x: abs.0,
                    y: abs.1,
                });
            }
            MoveInput::SkillInputEnd => out.push(MoveEvent::InputEndAck),
            MoveInput::SkillReleased { skill, flag } => {
                if skill == ctx.config.skills.move_skill {
                    self.move_hold = None;
                }
                // In click-to-move mode the roll key is released with `SkillReleased` (no
                // `SkillInputEnd`), which is answered with both acks (2026-10-20 capture).
                if ctx.config.skills.is_roll(skill) {
                    out.push(MoveEvent::InputEndAck);
                }
                out.push(MoveEvent::ReleasedAck { skill, flag });
            }
            MoveInput::SprintInput { held } => {
                self.held = held;
                if let MoveState::Rolling(r) = &mut self.state {
                    r.held = held;
                }
                out.push(MoveEvent::SprintState { held });
                if !held && let Some(run) = self.sprint().copied() {
                    // A sprint cannot end before `must_sprint` goes off at t0 + 35: a key
                    // released earlier (2026-10-20 capture: at t0 + 19) is answered at once
                    // (`1b1`, `2e7`) and the sprint ends at t0 + 35.
                    let end = run.start_frame + ctx.config.sprint.must_sprint_end_frames as u64;
                    if ctx.frame < end {
                        self.release_pending = true;
                    } else {
                        *end_sprint = true;
                    }
                } else if held && self.sprint().is_some() {
                    // Pressed again before t0 + 35: the earlier release no longer ends the sprint
                    // (2026-11-02 f2178: released at t0 + 22, pressed at t0 + 30, the roll press
                    // only acked, the sprint runs on to the next release).
                    self.release_pending = false;
                }
            }
            MoveInput::Aim { x, y } => {
                // Never echoed while rolling or sprinting (all sessions).
                if !self.is_rolling() && self.sprint().is_none() && self.deferred_roll.is_none() {
                    out.push(MoveEvent::AimEcho { cell: (x, y) });
                }
            }
            MoveInput::AimEntity { .. } => {}
            MoveInput::StanceRequest { value } => self.stance_request = Some(value),
            // The client cannot use a skill or swap again until this answer (session +0x7a90,
            // cleared by 0xff), so every request is answered. Whether a swap during an action
            // or a walk changes it is not captured (gap #7).
            MoveInput::WeaponSwap { set } => {
                self.weapon_set = set;
                out.push(MoveEvent::WeaponSet {
                    set,
                    requested: true,
                })
            }
            MoveInput::AimMode { mode } => {
                let combat = mode == 1;
                self.aim_combat = combat;
                if !self.stance_entered {
                    // At entry the stance follows the first mode 1 in the batch 8 frames after
                    // the one answering it (captures: 9 frames after the input's frame; 14 in the
                    // 2026-10-20 session: it depends on the area load). After an area change
                    // (mode 0 then 1 in one batch) 3 frames.
                    if !combat {
                        self.entry_mode0_frame = Some(ctx.frame);
                    } else if !self.stance_scheduled {
                        self.stance_scheduled = true;
                        let delay = if self.entry_mode0_frame == Some(ctx.frame) {
                            ctx.config.area_stance_delay_frames
                        } else {
                            ctx.config.stance_delay_frames
                        };
                        self.due.push((ctx.frame + delay as u64, Due::Stance(true)));
                    }
                } else if combat != self.combat_stance || self.stance_request.is_some() {
                    // Input mode toggled (with the stance request `0x200`): 2 of 2 in the
                    // 2026-10-20 capture, in the batch answering the input, in both directions,
                    // while standing.
                    // UNVERIFIED (gap #7): while walking or rolling.
                    self.due.push((
                        ctx.frame + ctx.config.stance_toggle_frames as u64,
                        Due::Stance(combat),
                    ));
                }
            }
        }
    }

    fn on_direction(&mut self, dir: Cell, cursor: bool, ctx: &StepCtx) {
        let window = ctx.config.sprint.release_window_frames as u64;
        let click_end = self
            .sprint_end
            .is_some_and(|e| e.click && ctx.frame <= e.frame + window);
        if (cursor && matches!(self.state, MoveState::Walking(Walk::Target { .. })))
            || (click_end && (cursor || dir == (0, 0)))
        {
            // A held click walks, or its Move ran the sprint that just ended: cursor vectors
            // and the client's zero vector are not answered (2026-10-20 f3644,
            // 2026-10-20_21-51-49 f8952, 2026-11-02 f7911).
            return;
        }
        self.wasd = dir;
        self.cursor_mode = cursor;
        self.ack_wanted = true;
        let moving = dir != (0, 0);
        if !cursor
            && self
                .sprint_anim_until
                .is_some_and(|until| ctx.frame < until)
        {
            // The client's `SprintEnd` animation still plays: `Actor_StartWasdWalk` starts no
            // walk and the coast goes on; the key, if still down at its end, walks on then.
            if moving {
                self.facing = dir;
            }
            return;
        }
        if moving {
            self.facing = dir;
            // Cursor vectors still in flight when the sprint ended keep its direction up to
            // date; a key vector is a real new walk. In click-to-move mode every vector is a
            // cursor one, for a limited time (a sprint that coasts at once ignores them).
            let window = ctx.config.sprint.release_window_frames as u64;
            let cursor_vector = cursor || self.is_cursor_vector(dir, ctx);
            match &mut self.sprint_end {
                Some(end) if end.coasting => {
                    if cursor && ctx.frame <= end.frame + window {
                        end.dir = dir;
                        return;
                    }
                    self.sprint_end = None;
                }
                Some(end) if cursor_vector => end.dir = dir,
                _ => self.sprint_end = None,
            }
        } else if let Some(end) = self.sprint_end.take()
            && ctx.frame <= end.frame + ctx.config.sprint.release_window_frames as u64
            && end.cursor
            && matches!(self.state, MoveState::Walking(Walk::Direction(_)))
        {
            // The sprint followed the cursor and the keys were already up when the unscaled
            // `MoveAck` of its end reached the client: that never restarted its walk, it
            // coasts (see `end_sprint`). Carry on from the step the client is at.
            self.state = MoveState::Idle;
            self.start_coast(
                end.dir,
                end.frame + 1 + ctx.config.sprint.coast_delay_frames as u64,
                ctx,
            );
            return;
        }
        if !moving && matches!(self.mover.motion, Motion::Coast { .. }) {
            // The client's walk is over already; a stop does not cut the coasting short.
            return;
        }
        if let Some(run) = self.sprint_mut() {
            // The stream starts with the first vector after the sprint started. A zero vector
            // is answered once. The official client never sends one while a sprint runs (it
            // sends the cursor direction); UNVERIFIED (gap #4) what the stream does if it did.
            run.streaming = moving;
        }
        match self.state {
            // A roll along the WASD keys turns with them; the walk after it is picked up when
            // the roll ends. A roll towards the cursor keeps steering by the cursor
            // (UNVERIFIED: WASD pressed during a roll that started standing).
            MoveState::Rolling(r) => {
                if r.wasd_driven
                    && moving
                    && let Motion::Roll { x, y, .. } = &mut self.mover.motion
                {
                    *x = dir.0 as f32;
                    *y = dir.1 as f32;
                }
            }
            MoveState::Sprinting(_) => self.mover.start(if moving {
                Motion::Sprint {
                    x: dir.0 as f32,
                    y: dir.1 as f32,
                    scale: ctx.config.sprint.speed_scale,
                }
            } else {
                Motion::Idle
            }),
            MoveState::Idle | MoveState::Walking(Walk::Direction(_)) => {
                if moving {
                    self.state = MoveState::Walking(Walk::Direction(dir));
                    self.mover.start(Motion::Direction {
                        x: dir.0 as f32,
                        y: dir.1 as f32,
                    });
                } else {
                    self.state = MoveState::Idle;
                    self.mover.stop_after_step();
                }
            }
            MoveState::Walking(Walk::Target { .. }) => {
                // UNVERIFIED (gap #2): WASD takes over a click walk; a stop leaves it running.
                if moving {
                    self.state = MoveState::Walking(Walk::Direction(dir));
                    self.mover.start(Motion::Direction {
                        x: dir.0 as f32,
                        y: dir.1 as f32,
                    });
                }
            }
        }
    }

    fn target_of(&self, req: &UseSkill) -> Cell {
        match req.entity {
            Some(_) => (req.x, req.y),
            None => (self.mover.cell().0 + req.x, self.mover.cell().1 + req.y),
        }
    }

    fn skill_ack(req: &UseSkill, target: Cell) -> MoveEvent {
        MoveEvent::SkillAck {
            skill: req.skill,
            flags: req.flags,
            unk: req.unk,
            target_flags: req.target_flags,
            entity: req.entity,
            x: target.0,
            y: target.1,
        }
    }

    fn on_use_skill(&mut self, req: UseSkill, ctx: &mut StepCtx, out: &mut Vec<MoveEvent>) {
        let target = self.target_of(&req);
        let busy = matches!(self.state, MoveState::Rolling(_) | MoveState::Sprinting(_));
        if ctx.config.skills.is_roll(req.skill) {
            match self.state {
                // Chained rolls (2026-10-20: presses 28 and 29 frames after the previous start,
                // plus the 2026-10-14 case at 27): acked at once, the roll itself starts when
                // the previous action ends (`max(r+1, P+30)`). A press 9 frames in that was
                // released again was dropped: this keeps the last press (single slot);
                // UNVERIFIED (gap #3) whether a released key drops it.
                MoveState::Rolling(_) if req.target_flags & UseSkill::HELD_REPEAT == 0 => {
                    out.push(Self::skill_ack(&req, target));
                    self.queued_roll = Some((req, target));
                }
                // Held-key repeats of the roll skill (target flags 0x13) are acked and nothing
                // else happens. UNVERIFIED (gap #3): a roll while sprinting is treated alike.
                MoveState::Rolling(_) | MoveState::Sprinting(_) => {
                    out.push(Self::skill_ack(&req, target));
                }
                // A held repeat never starts a roll: after an attack ended the sprint with the
                // key still down, the repeats are only acked (2026-11-02, f11801, f12757).
                _ if req.target_flags & UseSkill::HELD_REPEAT != 0 => {
                    out.push(Self::skill_ack(&req, target));
                }
                // Press and release handled together (inputs queued while paused): the
                // answers come in input order and the roll itself one batch later (2026-10-20
                // capture, scheduled headers `(5, 8)`, `(5, 17)`, `(5, 25)`).
                // A Move started by a click input of this batch: the roll is acked now and its
                // `StartAction` comes one batch later, its stun stats and whole timeline (sprint,
                // tap end, roll end, `must_sprint` off) one more frame behind (2026-10-20
                // f3712, f4485; 2026-11-02 f8037, f8075).
                _ if self.move_start_frame == Some(ctx.frame) => {
                    out.push(Self::skill_ack(&req, target));
                    self.deferred_roll = Some((req, target));
                    self.roll_late = true;
                    self.due.push((ctx.frame + 1, Due::StartDeferredRoll));
                }
                _ if self.batch_release && self.held => {
                    out.push(Self::skill_ack(&req, target));
                    self.deferred_roll = Some((req, target));
                    self.due.push((ctx.frame + 1, Due::StartDeferredRoll));
                }
                _ => self.start_roll(req, target, true, ctx, out),
            }
        } else if req.skill == ctx.config.skills.move_skill {
            if busy {
                // Verified (2026-10-20, 1 roll and 3 sprints): acked at once, nothing moves;
                // the held click's walk resumes with a `StartAction` when the roll ends.
                // Once `must_sprint` is off a running sprint follows the click: every press
                // or repeat starts a Move (flags + 0x10, before its ack) and the sprint goes
                // on (8 of 8 sprints with a held click past t0 + 35, 2026-10-20 and 2026-11-02).
                self.note_hold(&req, target, ctx);
                if matches!(self.state, MoveState::Sprinting(_)) && self.must_sprint_off.is_none()
                {
                    self.sprint_move = true;
                    out.push(MoveEvent::ActionStart {
                        kind: ActionKind::Move,
                        skill: req.skill,
                        flags: req.flags | ACTION_INPUT_FLAG,
                        action: Self::alloc_action(ctx),
                        cell: self.mover.cell(),
                        target,
                        entity: req.entity,
                        unk: req.unk,
                    });
                }
                out.push(Self::skill_ack(&req, target));
            } else if self.in_click_sprint_end(ctx.frame) {
                // The client plays `SprintEnd` after a sprint its click ran: acked only.
                self.note_hold(&req, target, ctx);
                out.push(Self::skill_ack(&req, target));
            } else {
                self.start_walk(req, target, ctx, out);
            }
        } else {
            self.start_other(req, target, ctx, out);
        }
    }

    fn alloc_action(ctx: &mut StepCtx) -> u16 {
        let id = *ctx.next_action;
        *ctx.next_action = ctx.next_action.wrapping_add(1);
        id
    }

    /// Starts a roll towards the absolute `target`. `ack` is false for a queued roll, whose
    /// `UseSkillAck` was sent when it was asked for.
    fn start_roll(
        &mut self,
        req: UseSkill,
        target: Cell,
        ack: bool,
        ctx: &mut StepCtx,
        out: &mut Vec<MoveEvent>,
    ) {
        let cell = self.mover.cell();
        let offset = (target.0 - cell.0, target.1 - cell.1);
        let action = Self::alloc_action(ctx);
        let late = std::mem::take(&mut self.roll_late);
        let cfg = ctx.config;
        // Verified (2026-10-20): a standing roll gets the same batch and ends in Idle with no
        // `MoveAck`. The client rolls along the WASD vector when a key is held, else towards
        // the cursor (`target`, steered every 2 frames); `target` is the cursor either way.
        // The client's roll heading (`0x141b76da0` neighbourhood, `0x141b8bda0`): the held input
        // vector, else the direction to the action's target, and the entity's facing when the
        // target is the player's own cell.
        self.roll_cursor = offset;
        // The roll is the running action now: its own factor (1, no ramp) replaces an
        // attack's. UNVERIFIED: the one attack a roll cut short (f4021) shows only the roll's
        // own steps, and the walk after it was at full speed.
        self.attack_slow = None;
        self.mover.factor = 1.0;
        let wasd_driven = self.wasd != (0, 0);
        let heading = if wasd_driven {
            self.wasd
        } else if offset != (0, 0) {
            offset
        } else {
            self.facing
        };
        if heading != (0, 0) {
            self.facing = heading;
        }
        self.sprint_move = false;
        self.sprint_end = None;
        self.sprint_anim_until = None;
        self.mover.start(if heading == (0, 0) {
            Motion::Idle
        } else {
            Motion::Roll {
                x: heading.0 as f32,
                y: heading.1 as f32,
                remaining: cfg.roll.travel_distance,
                elapsed: 0.0,
                sprint: false,
            }
        });
        let t0 = ctx.frame + late as u64;
        self.state = MoveState::Rolling(Rolling {
            start_frame: t0,
            held: self.held,
            offset,
            wasd_driven,
            action,
            sprint: None,
            sprinted: false,
        });
        out.push(MoveEvent::ActionStart {
            kind: ActionKind::Roll,
            skill: req.skill,
            flags: req.flags | ACTION_INPUT_FLAG,
            action,
            cell,
            target,
            entity: req.entity,
            unk: req.unk,
        });
        out.push(MoveEvent::ActionEvent);
        out.push(MoveEvent::Stats(vec![(MoveStat::IsDodgeRolling, 1)]));
        if ack {
            out.push(Self::skill_ack(&req, target));
        }
        if late {
            self.due.push((t0, Due::RollStunStats));
        } else {
            out.push(MoveEvent::Stats(stun_stats(cfg.roll.stun_threshold)));
        }
        let at = |frames: u32| t0 + frames as u64;
        self.due
            .push((at(cfg.sprint.start_frames), Due::SprintStart { roll: t0 }));
        self.due
            .push((at(cfg.roll.tap_end_frames), Due::RollEndTap { roll: t0 }));
        self.due
            .push((at(cfg.roll.action_frames), Due::RollEnd { roll: t0 }));
    }

    /// Click-to-move, verified on 2026-10-20 (6 single clicks, 4 holds): the press is answered
    /// with `StartAction` Move (request flags + 0x10, start = the server's cell, target =
    /// cell + offset) and a `UseSkillAck`; every held repeat (target flags 0x10) the same with
    /// flags 0x10, so a repeat re-aims the walk; the release with a `SkillReleasedAck`.
    fn start_walk(
        &mut self,
        req: UseSkill,
        target: Cell,
        ctx: &mut StepCtx,
        out: &mut Vec<MoveEvent>,
    ) {
        let cell = self.mover.cell();
        if target != cell {
            self.facing = (target.0 - cell.0, target.1 - cell.1);
        }
        self.sprint_end = None;
        self.sprint_anim_until = None;
        self.state = MoveState::Walking(Walk::Target { goal: target });
        self.mover.start(Motion::MoveTo {
            x: target.0 as f32 + 0.5,
            y: target.1 as f32 + 0.5,
        });
        self.note_hold(&req, target, ctx);
        self.move_start_frame = Some(ctx.frame);
        out.push(MoveEvent::ActionStart {
            kind: ActionKind::Move,
            skill: req.skill,
            flags: req.flags | ACTION_INPUT_FLAG,
            action: Self::alloc_action(ctx),
            cell,
            target,
            entity: req.entity,
            unk: req.unk,
        });
        out.push(Self::skill_ack(&req, target));
    }

    /// The client plays `SprintEnd` after a sprint a held click took part in: Move requests
    /// are only acked and no restart comes.
    fn in_click_sprint_end(&self, frame: u64) -> bool {
        self.sprint_anim_until.is_some_and(|until| frame < until)
            && self.sprint_end.is_some_and(|e| e.click)
    }

    /// The held click, if its input is recent.
    fn live_hold(&self, frame: u64) -> Option<MoveHold> {
        self.move_hold
            .filter(|h| frame - h.last_input <= MoveHold::STALE_FRAMES)
    }

    /// Remembers the click as held until its release; every press or repeat restarts the
    /// restart timer.
    fn note_hold(&mut self, req: &UseSkill, target: Cell, ctx: &StepCtx) {
        let repeat = req.target_flags & UseSkill::HELD_REPEAT != 0;
        match &mut self.move_hold {
            Some(h) if repeat => {
                h.history = [(ctx.frame, target), h.history[0], h.history[1]];
                h.goal = target;
                h.flags = req.flags;
                h.last_input = ctx.frame;
                h.last_start = ctx.frame;
            }
            _ => {
                self.hold_seq += 1;
                self.move_hold = Some(MoveHold {
                    goal: target,
                    flags: req.flags,
                    seq: self.hold_seq,
                    last_input: ctx.frame,
                    last_start: ctx.frame,
                    history: [(ctx.frame, target); 3],
                });
            }
        }
        self.due.push((
            ctx.frame + ctx.config.click.restart_frames as u64,
            Due::MoveRestart { seq: self.hold_seq },
        ));
    }

    fn start_other(
        &mut self,
        req: UseSkill,
        target: Cell,
        ctx: &mut StepCtx,
        out: &mut Vec<MoveEvent>,
    ) {
        // An attack during a roll is acked at once and the last one asked for starts when the
        // roll lets go: `attack_cancel_frames` after the roll started (the roll is cut short
        // there), or, when Space was held past +15, with the end of the sprint at +35 (2026-10-20
        // capture: 3 of 3 and 1 of 1). Attacks end a running sprint; they do not stop walking but
        // slow it (`update_speed_factor`; UNVERIFIED (gap #6) for a plain WASD walk).
        if let MoveState::Rolling(r) = self.state {
            out.push(Self::skill_ack(&req, target));
            // A held repeat does not replace the request (the attack on a hovered monster
            // kept its entity target in the capture).
            if req.target_flags & UseSkill::HELD_REPEAT == 0 || self.queued_attack.is_none() {
                self.queued_attack = Some((req, target));
            }
            let at =
                (r.start_frame + ctx.config.roll.attack_cancel_frames as u64).max(ctx.frame + 1);
            self.due.push((
                at,
                Due::AttackWindow {
                    roll: r.start_frame,
                },
            ));
            return;
        }
        self.start_attack(req, target, true, ctx, out);
    }

    /// The client's action speed factor for this frame's step: after an attack's `StartAction`
    /// in frame `s` it ramps towards `floor` from frame `s + 1`, is `floor` from `s + snap`, and
    /// ramps back to 1 from `s + lock + 1` ([`AttackSpeedConfig`]).
    fn update_speed_factor(&mut self, ctx: &StepCtx) {
        let Some(start) = self.attack_slow else {
            return;
        };
        let cfg = &ctx.config.attack_speed;
        let n = ctx.frame.saturating_sub(start);
        if n == 0 {
            return;
        }
        let rate = cfg.ramp_per_second * ctx.dt;
        let f = &mut self.mover.factor;
        if n <= cfg.lock_frames as u64 {
            *f = if n >= cfg.snap_frames as u64 {
                cfg.floor
            } else {
                (*f - rate).max(cfg.floor)
            };
        } else {
            *f = (*f + rate).min(1.0);
            if *f >= 1.0 {
                self.attack_slow = None;
            }
        }
    }

    /// Starts an attack now; `ack` is false for a queued one, whose ack went out when it was
    /// asked for.
    fn start_attack(
        &mut self,
        req: UseSkill,
        target: Cell,
        ack: bool,
        ctx: &mut StepCtx,
        out: &mut Vec<MoveEvent>,
    ) {
        let cell = self.mover.cell();
        self.attack_slow = Some(ctx.frame);
        out.push(MoveEvent::ActionStart {
            kind: ActionKind::Other,
            skill: req.skill,
            flags: req.flags | ACTION_INPUT_FLAG,
            action: Self::alloc_action(ctx),
            cell,
            target,
            entity: req.entity,
            unk: req.unk,
        });
        // A skill of the other weapon set: the server swaps by itself (2026-11-02, f5464 and
        // f11798): the ack at once, then after the frame's `MoveAck` the `0x1d5`, the sprint-off
        // (the `MoveAck` was still scaled; the unscaled one follows at r+1) and the `0x1ca`.
        if let Some(set) = SkillIds::weapon_set_of(req.skill).filter(|s| *s != self.weapon_set) {
            self.weapon_set = set;
            if ack {
                out.push(Self::skill_ack(&req, target));
            }
            self.deferred.push(MoveEvent::WeaponSet {
                set,
                requested: false,
            });
            if let Some(run) = self.sprint().copied() {
                if let Some(n) = self.must_sprint_off.take() {
                    self.deferred.push(MoveEvent::BuffRemoved { index: n });
                    self.deferred
                        .push(MoveEvent::Stats(vec![(MoveStat::MustSprint, 0)]));
                }
                self.deferred.push(MoveEvent::BuffRemoved {
                    index: run.buff_base + 1,
                });
                self.deferred.push(MoveEvent::Stats(sprint_off_stats()));
                self.deferred.push(MoveEvent::ActionEvent);
                self.deferred.push(MoveEvent::BuffRemoved {
                    index: run.buff_base + 2,
                });
                self.deferred
                    .push(MoveEvent::Stats(vec![(MoveStat::AllHitsStunYou, 0)]));
                self.attack_ends_sprint_after_ack = Some(run.streaming);
            } else {
                self.deferred.push(MoveEvent::ActionEvent);
            }
            return;
        }
        // Every action start but Move's has a `0x1ca` (all eight sessions: 0x80400040 147 of
        // 147, 0x2000c266 31 of 31, 0x85000040 47 of 47; skills with their own stats or buffs
        // send those first), then the ack.
        let Some(run) = self.sprint().copied() else {
            out.push(MoveEvent::ActionEvent);
            if ack {
                out.push(Self::skill_ack(&req, target));
            }
            return;
        };
        // The attack ends the sprint: (`must_sprint` off, if it still runs,) `0x1c8`, the first
        // half of the sprint-off, `0x1ca`, the ack, then after the frame's `MoveAck` (unscaled)
        // the second half.
        if let Some(n) = self.must_sprint_off.take() {
            let start = out.pop().expect("ActionStart");
            out.push(MoveEvent::BuffRemoved { index: n });
            out.push(MoveEvent::Stats(vec![(MoveStat::MustSprint, 0)]));
            out.push(start);
        }
        out.push(MoveEvent::BuffRemoved {
            index: run.buff_base + 1,
        });
        out.push(MoveEvent::Stats(sprint_off_stats()));
        out.push(MoveEvent::ActionEvent);
        if ack {
            out.push(Self::skill_ack(&req, target));
        }
        self.deferred.push(MoveEvent::BuffRemoved {
            index: run.buff_base + 2,
        });
        self.deferred
            .push(MoveEvent::Stats(vec![(MoveStat::AllHitsStunYou, 0)]));
        self.ack_wanted |= run.streaming && self.wasd != (0, 0);
        self.finish_sprint();
    }

    /// Re-aims the running action after a `UpdateSkillTarget`.
    fn steer(&mut self, skill: u32, offset: Cell, abs: Cell, ctx: &StepCtx) {
        if ctx.config.skills.is_roll(skill) {
            self.roll_cursor = offset;
        }
        match &mut self.state {
            MoveState::Rolling(r) if ctx.config.skills.is_roll(skill) => {
                if offset != (0, 0) && !r.wasd_driven {
                    r.offset = offset;
                    if let Motion::Roll { x, y, .. } = &mut self.mover.motion {
                        *x = offset.0 as f32;
                        *y = offset.1 as f32;
                    }
                }
            }
            MoveState::Walking(Walk::Target { goal }) if skill == ctx.config.skills.move_skill => {
                *goal = abs;
                self.mover.start(Motion::MoveTo {
                    x: abs.0 as f32 + 0.5,
                    y: abs.1 as f32 + 0.5,
                });
            }
            _ => {}
        }
    }

    // ---- timeline

    fn fire(&mut self, due: Due, ctx: &mut StepCtx, out: &mut Vec<MoveEvent>, unscaled: &mut bool) {
        let cfg = ctx.config;
        match due {
            Due::SprintStart { roll } => {
                let MoveState::Rolling(r) = &mut self.state else {
                    return;
                };
                if r.start_frame != roll || !r.held || r.sprint.is_some() {
                    return;
                }
                let n = *ctx.next_buff;
                *ctx.next_buff += 3;
                // A vector handled in the frame the sprint starts opens the stream already
                // (2026-10-14_22-23-02 f308: scaled in the sprint-start batch).
                let streaming = self.ack_wanted && self.wasd != (0, 0);
                r.sprint = Some(SprintRun {
                    start_frame: roll,
                    buff_base: n,
                    streaming,
                });
                r.sprinted = true;
                self.mover.sprint_roll();
                for (i, row) in cfg.sprint.buff_rows.iter().enumerate() {
                    out.push(MoveEvent::BuffAdded {
                        index: n + i as u16,
                        row: *row,
                        fade: None,
                    });
                }
                out.push(MoveEvent::Stats(vec![
                    (
                        MoveStat::SprintBlendTimeOverrideMs,
                        cfg.sprint.blend_time_override_ms,
                    ),
                    (MoveStat::IsSprinting, 1),
                    (MoveStat::MustSprint, 1),
                    (MoveStat::AllHitsStunYou, 1),
                    (MoveStat::VirtualSkillIgnoreNamelockingDynamically, 1),
                ]));
                self.must_sprint_off = Some(n);
                self.due.push((
                    roll + cfg.sprint.must_sprint_end_frames as u64,
                    Due::MustSprintOff,
                ));
            }
            Due::RollEndTap { roll } => {
                if let MoveState::Rolling(r) = self.state
                    && r.start_frame == roll
                    && !r.sprinted
                {
                    // `is_dodge_rolling` goes off; the action itself runs on to `action_frames`.
                    roll_end_stats(out);
                }
            }
            Due::RollEnd { roll } => {
                let MoveState::Rolling(r) = self.state else {
                    return;
                };
                if r.start_frame != roll {
                    return;
                }
                // A tap roll cleared its stats earlier; one that sprinted does it now.
                if r.sprinted {
                    roll_end_stats(out);
                }
                if let Some(run) = r.sprint {
                    out.push(MoveEvent::Stats(vec![
                        (
                            MoveStat::BaseMoveSpeedFromSprint,
                            cfg.sprint.move_speed_percent,
                        ),
                        (
                            MoveStat::MovementVelocityPermyriad,
                            cfg.sprint.velocity_permyriad,
                        ),
                    ]));
                    out.push(MoveEvent::BuffAdded {
                        index: run.buff_base + 1,
                        row: cfg.sprint.buff_rows[1],
                        fade: None,
                    });
                    out.push(MoveEvent::BuffAdded {
                        index: run.buff_base,
                        row: cfg.sprint.buff_rows[0],
                        fade: Some(cfg.sprint.buff_fade_secs),
                    });
                    self.state = MoveState::Sprinting(run);
                    self.start_sprint_motion(ctx);
                } else if let Some(h) = self.live_hold(ctx.frame)
                    && self.queued_roll.is_none()
                {
                    // A held click (unless a queued roll starts now: 2026-10-20 f3744): the walk resumes with a `StartAction` towards the latest
                    // target (2026-10-20 capture, scheduled header `(0, 8)`).
                    self.state = MoveState::Walking(Walk::Target { goal: h.goal });
                    self.mover.start(Motion::MoveTo {
                        x: h.goal.0 as f32 + 0.5,
                        y: h.goal.1 as f32 + 0.5,
                    });
                    out.push(MoveEvent::ActionStart {
                        kind: ActionKind::Move,
                        skill: cfg.skills.move_skill,
                        flags: h.flags & !ACTION_INPUT_FLAG,
                        action: Self::alloc_action(ctx),
                        cell: self.mover.cell(),
                        target: h.goal,
                        entity: None,
                        unk: 0xff,
                    });
                } else {
                    // The client resumes walking by itself after a roll; no message.
                    self.resume_walking();
                }
                if let Some((req, target)) = self.queued_roll.take() {
                    self.start_roll(req, target, false, ctx, out);
                }
            }
            Due::MustSprintOff => {
                if let Some(n) = self.must_sprint_off.take() {
                    out.push(MoveEvent::BuffRemoved { index: n });
                    out.push(MoveEvent::Stats(vec![(MoveStat::MustSprint, 0)]));
                }
                // An attack asked for during the roll ends the sprint now, after this frame's
                // `MoveAck` (official order: `must_sprint` off, `MoveAck`, `StartAction`, ...).
                if self.sprint().is_some() {
                    // A held click takes the sprint over: a Move towards its goal after the
                    // `MoveAck` (official order: `must_sprint` off, `MoveAck`, `StartAction`
                    // flags 0x10), the sprint goes on (2026-10-20 f3779, 2026-11-02 f7746).
                    if let Some(h) = self.live_hold(ctx.frame)
                        && matches!(self.state, MoveState::Sprinting(_))
                        && !self.release_pending
                    {
                        self.move_after_ack = Some(h.settled_goal(ctx.frame, 0));
                        self.sprint_move = true;
                    }
                    self.attack_after_ack = self.queued_attack.take();
                    if self.release_pending {
                        self.release_pending = false;
                        self.release_due = true;
                    }
                }
            }
            Due::AttackWindow { roll } => {
                let MoveState::Rolling(r) = self.state else {
                    return;
                };
                // A roll that sprinted hands the attack to the sprint's end (see
                // `MustSprintOff`) or to its own end.
                if r.start_frame != roll || r.sprinted {
                    return;
                }
                if let Some((req, target)) = self.queued_attack.take() {
                    self.resume_walking();
                    self.start_attack(req, target, false, ctx, out);
                }
            }
            Due::MoveRestart { seq } => {
                let Some(h) = self.move_hold else {
                    return;
                };
                if h.seq != seq {
                    return;
                }
                if ctx.frame - h.last_input > MoveHold::STALE_FRAMES {
                    self.move_hold = None;
                    return;
                }
                // A newer press or repeat restarted the timer.
                if ctx.frame < h.last_start + cfg.click.restart_frames as u64 {
                    return;
                }
                self.due.push((
                    ctx.frame + cfg.click.restart_frames as u64,
                    Due::MoveRestart { seq },
                ));
                // Only while the walk runs: a roll or sprint holds the restart back, and the
                // roll's end issues one.
                if matches!(self.state, MoveState::Walking(Walk::Target { .. }))
                    && !self.in_click_sprint_end(ctx.frame)
                {
                    out.push(MoveEvent::ActionStart {
                        kind: ActionKind::Move,
                        skill: cfg.skills.move_skill,
                        flags: h.flags & !ACTION_INPUT_FLAG,
                        action: Self::alloc_action(ctx),
                        cell: self.mover.cell(),
                        target: h.goal,
                        entity: None,
                        unk: 0xff,
                    });
                    if let Some(m) = &mut self.move_hold {
                        m.last_start = ctx.frame;
                    }
                }
            }
            Due::StartDeferredRoll => {
                if let Some((req, target)) = self.deferred_roll.take() {
                    if self.is_rolling() {
                        self.queued_roll = Some((req, target));
                    } else {
                        self.start_roll(req, target, false, ctx, out);
                    }
                }
            }
            Due::UnscaledAck => *unscaled = true,
            Due::RollStunStats => {
                out.push(MoveEvent::Stats(stun_stats(cfg.roll.stun_threshold)));
            }
            Due::SprintEndAnimOver { end } => {
                if self.sprint_anim_until != Some(end + self.end_anim_frames(ctx) as u64) {
                    return;
                }
                self.sprint_anim_until = None;
                // The Actor tick's `Actor_StartWasdWalk` call finds the key down (WASD mode
                // only: click-to-move mode has no keys).
                if self.wasd != (0, 0) && !self.cursor_mode && matches!(self.state, MoveState::Idle)
                {
                    self.sprint_end = None;
                    self.resume_walking();
                }
            }
            Due::Stance(combat) => {
                let entered = std::mem::replace(&mut self.stance_entered, true);
                // Before entry a request waits: 2026-10-14 (four sessions, `0x200` value 1 and
                // mode 0 right after mode 1 at f1) answers it one frame after the entry stance.
                let request = if entered {
                    self.stance_request.take()
                } else {
                    None
                };
                if self.combat_stance != combat || request.is_some() {
                    self.combat_stance = combat;
                    let value = if combat {
                        ctx.config.stance_lerp_combat
                    } else {
                        ctx.config.stance_lerp_peaceful
                    };
                    out.push(MoveEvent::OrientationLerp { value });
                    out.push(MoveEvent::StanceChanged { combat, request });
                }
                if !entered && self.stance_request.is_some() {
                    self.due.push((ctx.frame + 1, Due::Stance(self.aim_combat)));
                }
            }
        }
    }

    /// Walks in the held direction, or stops.
    fn resume_walking(&mut self) {
        if self.wasd != (0, 0) {
            self.state = MoveState::Walking(Walk::Direction(self.wasd));
            self.mover.start(Motion::Direction {
                x: self.wasd.0 as f32,
                y: self.wasd.1 as f32,
            });
        } else {
            self.state = MoveState::Idle;
            self.mover.start(Motion::Idle);
        }
    }

    /// The roll that became a sprint is over: sprint in the held direction.
    fn start_sprint_motion(&mut self, ctx: &StepCtx) {
        self.mover.start(if self.wasd != (0, 0) {
            Motion::Sprint {
                x: self.wasd.0 as f32,
                y: self.wasd.1 as f32,
                scale: ctx.config.sprint.speed_scale,
            }
        } else {
            Motion::Idle
        });
    }

    // ---- replies of the frame

    fn move_ack(&mut self, unscaled_due: bool, ctx: &StepCtx, out: &mut Vec<MoveEvent>) {
        // The client's own grid cell: it snaps to any other cell in a non-zero `0x64`
        // (lockstep), which is the hitch at every WASD start and turn and every frame of a
        // sprint stream when the cell of the position is sent instead.
        let cell = self.mover.grid();
        let wasd = self.wasd;
        let scaled = scale_to(wasd, ctx.config.sprint.dir_length);
        let unscaled = self.unscaled_dir.take();
        if unscaled_due
            && wasd == (0, 0)
            && let Some(dir) = unscaled
        {
            // The client's zero vector arrived in the very frame the unscaled `MoveAck` of the
            // sprint's end is due. That ack still goes out first, as in every official sprint
            // (a `(0, 0)` in its place stopped the client's coast in game: 22 cells of drift);
            // the zero's own ack follows in the next frame.
            out.push(MoveEvent::MoveAck { dir, cell });
            return;
        }
        match self.sprint() {
            Some(run) if run.streaming => out.push(MoveEvent::MoveAck { dir: scaled, cell }),
            _ if self.ack_wanted || unscaled_due => {
                out.push(MoveEvent::MoveAck { dir: wasd, cell })
            }
            _ => {}
        }
        self.ack_wanted = false;
    }

    /// The sprint key went up: sprint-off buffs and stats after this frame's `MoveAck`, then
    /// back to walking. The stream's last message was scaled and the next frame sends the
    /// unscaled direction.
    fn end_sprint(&mut self, ctx: &mut StepCtx, out: &mut Vec<MoveEvent>) {
        let frame = ctx.frame;
        let Some(run) = self.sprint().copied() else {
            return;
        };
        self.release_pending = false;
        let sprint_move = std::mem::take(&mut self.sprint_move);
        if sprint_move && matches!(self.state, MoveState::Sprinting(_)) {
            // The click's Move ran the sprint and the key went up (2026-10-20_21-51-49 f8952,
            // 2026-11-02 f7910): no `StartAction`, `(0, 0)` in the next frame, and the client
            // plays `SprintEnd`: presses and repeats are only acked until it is over (a press
            // 21 frames after the batch, f8973) and no restart comes.
            let dir = self.wasd;
            self.wasd = (0, 0);
            self.due.push((frame + 1, Due::UnscaledAck));
            out.push(MoveEvent::BuffRemoved {
                index: run.buff_base + 1,
            });
            out.push(MoveEvent::Stats(sprint_off_stats()));
            out.push(MoveEvent::BuffRemoved {
                index: run.buff_base + 2,
            });
            out.push(MoveEvent::Stats(vec![(MoveStat::AllHitsStunYou, 0)]));
            self.state = MoveState::Idle;
            self.sprint_end = Some(SprintEnd {
                frame,
                dir,
                cursor: true,
                coasting: true,
                click: true,
            });
            let cfg = &ctx.config.sprint;
            self.start_coast(dir, frame + 1 + cfg.coast_delay_frames as u64, ctx);
            self.sprint_anim_until = Some(frame + self.end_anim_frames(ctx) as u64);
            return;
        }
        if let Some(h) = self.live_hold(ctx.frame)
            && matches!(self.state, MoveState::Sprinting(_))
        {
            // A held click: the walk goes on. Ended at t0 + 35 by an earlier release, with a
            // `StartAction` before the sprint-off buffs (2026-10-20 f3643); a sprint the click
            // already steers keeps its Move (2026-10-20_21-51-49 f8952, 2026-11-02 f7910).
            let goal = h.settled_goal(ctx.frame, 1);
            self.state = MoveState::Walking(Walk::Target { goal });
            self.mover.start(Motion::MoveTo {
                x: goal.0 as f32 + 0.5,
                y: goal.1 as f32 + 0.5,
            });
            // The direction is over: the next frame's `MoveAck` is `(0, 0)` (3 of 3, with or
            // without the client's own zero vector) and later cursor vectors are not answered.
            self.wasd = (0, 0);
            self.due.push((frame + 1, Due::UnscaledAck));
            out.push(MoveEvent::ActionStart {
                kind: ActionKind::Move,
                skill: ctx.config.skills.move_skill,
                flags: h.flags & !ACTION_INPUT_FLAG,
                action: Self::alloc_action(ctx),
                cell: self.mover.cell(),
                target: goal,
                entity: None,
                unk: 0xff,
            });
            // The client plays `SprintEnd` all the same: the next repeat is only acked
            // (f3645, then a roll).
            self.sprint_end = Some(SprintEnd {
                frame,
                dir: (0, 0),
                cursor: true,
                coasting: false,
                click: true,
            });
            self.sprint_anim_until = Some(frame + self.end_anim_frames(ctx) as u64);
        }
        out.push(MoveEvent::BuffRemoved {
            index: run.buff_base + 1,
        });
        out.push(MoveEvent::Stats(sprint_off_stats()));
        out.push(MoveEvent::BuffRemoved {
            index: run.buff_base + 2,
        });
        out.push(MoveEvent::Stats(vec![(MoveStat::AllHitsStunYou, 0)]));
        // The stream's last `0x64` carried the scaled vector: the next frame sends the unscaled
        // one (official sprints with a stream, 6 of 6: `0x64` unscaled at r+1). Without a
        // stream the client's direction (Actor +0x400) was never scaled, so that `0x64` would
        // change nothing but could snap: in lockstep the Actor handler (`0x1421497c0`) places
        // the entity at any non-zero `0x64` whose cell differs from its grid (+0x444)
        // (`0x141ddb6f0`, which stops the `SprintEnd` root move via `0x141de5f50`). The
        // official server's cell equals the client's; ours can be a cell off after the roll,
        // so it sends none (2026-11-02: one cell off, the client stood instead of coasting).
        if self.wasd != (0, 0) && run.streaming {
            self.due.push((frame + 1, Due::UnscaledAck));
            self.unscaled_dir = Some(self.wasd);
        }
        self.finish_sprint_by_release(ctx);
    }

    /// The sprint ended by the key release: walks on, or coasts.
    fn finish_sprint_by_release(&mut self, ctx: &StepCtx) {
        if !matches!(self.state, MoveState::Sprinting(_)) {
            self.finish_sprint();
            return;
        }
        let cfg = &ctx.config.sprint;
        if self.wasd != (0, 0) && self.cursor_mode {
            // Click-to-move mode has no movement keys: the vector is the cursor direction
            // whatever its angle (an exactly diagonal one looks like a key's) and the client's
            // unscaled-ack restart finds no key down. It coasts, without waiting for the
            // client's zero vector (4 to 5 frames after the batch with an exactly diagonal
            // cursor, in game 2026-10-21); that and the cursor vectors still in flight are
            // ignored for the release window.
            let dir = self.wasd;
            self.state = MoveState::Idle;
            self.sprint_end = Some(SprintEnd {
                frame: ctx.frame,
                dir,
                cursor: true,
                coasting: true,
                click: false,
            });
            self.start_coast(dir, ctx.frame + 1 + cfg.coast_delay_frames as u64, ctx);
        } else if self.wasd != (0, 0) || self.facing != (0, 0) {
            // The client plays `SprintEnd` and coasts whether or not a key is held; the
            // unscaled `MoveAck` of the next frame restarts nothing (`Actor_StartWasdWalk`
            // refuses during the animation). A key still down at its end walks on.
            let dir = if self.wasd != (0, 0) {
                self.wasd
            } else {
                self.facing
            };
            self.state = MoveState::Idle;
            self.sprint_end = Some(SprintEnd {
                frame: ctx.frame,
                dir,
                cursor: self.is_cursor_vector(dir, ctx),
                coasting: false,
                click: false,
            });
            // The coast is `SprintEnd`'s root motion, with or without a stream.
            self.start_coast(dir, ctx.frame + 1 + cfg.coast_delay_frames as u64, ctx);
            self.sprint_anim_until = Some(ctx.frame + self.end_anim_frames(ctx) as u64);
        } else {
            self.finish_sprint();
        }
        if let Some(until) = self.sprint_anim_until
            && until == ctx.frame + self.end_anim_frames(ctx) as u64
            && !self.cursor_mode
        {
            self.due
                .push((until, Due::SprintEndAnimOver { end: ctx.frame }));
        }
    }

    /// Whether the `0x63` vector `v` is the cursor direction (Space held, no key) and not a
    /// key: a free angle, or a key's vector that the cursor offset of the roll key points along
    /// within `cursor_match_degrees` (an exactly diagonal cursor makes the same `(127, 127)` as
    /// W, in WASD mode and with the same trailer bytes; the cursor offset tells them apart).
    fn is_cursor_vector(&self, v: Cell, ctx: &StepCtx) -> bool {
        if !is_key_vector(v) {
            return true;
        }
        let (a, b) = (v, self.roll_cursor);
        let (la, lb) = (
            ((a.0 as f64).powi(2) + (a.1 as f64).powi(2)).sqrt(),
            ((b.0 as f64).powi(2) + (b.1 as f64).powi(2)).sqrt(),
        );
        if la == 0.0 || lb == 0.0 {
            return false;
        }
        let cos = (a.0 as f64 * b.0 as f64 + a.1 as f64 * b.1 as f64) / (la * lb);
        cos >= (ctx.config.sprint.cursor_match_degrees as f64)
            .to_radians()
            .cos()
    }

    /// Play rate of the client's `SprintEnd` over the walking-speed case. `SprintEnd` is a
    /// movement-speed animation (`sprinting_end_*_01 : speed = 600` in `character.act`):
    /// `0x141e57b80` plays it at rate = movement speed (`0x141e4db00`, `Pathfinding_GetSpeed`
    /// x 10.869565 units per cell) / (600 x `dexfour.ao` `offset_scale` 0.9 x Positioned +0x474 x
    /// action speed) (`0x141e56ae0`), read when the sprint's end check (`0x141b76da0`) fires,
    /// and the root move (`0x141f5f650`) lasts 0.75 s / rate. Traced (2026-11-02, private):
    /// walking speed 402.17 -> 0.745 (30 frames), sprint speed 603.26 -> 1.117 (20 frames).
    /// A release batch (`SprintState` echo with the sprint-off stats, after `must_sprint` went
    /// off) ends the sprint as the echo is applied, before the stats: sprint speed. A sprint
    /// ended at t0 + 35 by an earlier release, or by an attack, sees the stats first.
    fn sprint_end_rate(in_release_batch: bool, ctx: &StepCtx) -> f32 {
        if in_release_batch && ctx.config.sprint.release_ends_at_sprint_speed {
            ctx.config.sprint.speed_scale
        } else {
            1.0
        }
    }

    /// Frames between the sprint-off batch and the end of `SprintEnd` at the current rate.
    fn end_anim_frames(&self, ctx: &StepCtx) -> u32 {
        (ctx.config.sprint.end_anim_frames as f32 / self.sprint_end_rate).round() as u32
    }

    /// Coasts along `dir`; the client's first coasting step is in frame `first_step`. Steps of
    /// frames before the next advance (`ctx.frame + 1`) that the entity has not made yet (it
    /// was waiting for a restart that never came) are made now.
    fn start_coast(&mut self, dir: Cell, first_step: u64, ctx: &StepCtx) {
        let cfg = &ctx.config.sprint;
        // `SprintEnd` played `rate` times faster: same root displacement in 1/rate the time.
        let rate = self.sprint_end_rate;
        let step = self.mover.speed * CLIENT_FRAME_SECS * cfg.coast_start_scale * rate;
        let decay = step / (cfg.coast_frames / rate);
        let next = ctx.frame + 1;
        let delay = first_step.saturating_sub(next) as u32;
        self.mover.start_after(
            Motion::Coast {
                x: dir.0 as f32,
                y: dir.1 as f32,
                step,
                decay,
                frame: 0,
            },
            delay,
        );
        for _ in first_step..next {
            self.mover.advance(ctx.dt, ctx.terrain);
        }
    }

    /// Leaves the sprint: a sprinting entity walks on; a rolling one finishes its roll.
    fn finish_sprint(&mut self) {
        self.sprint_move = false;
        match &mut self.state {
            MoveState::Rolling(r) => r.sprint = None,
            MoveState::Sprinting(_) => self.resume_walking(),
            _ => {}
        }
    }
}

/// A vector the keyboard can make (one axis, or both with the same magnitude), as opposed to
/// the free angle of the cursor direction the client sends with Space held and no key.
fn is_key_vector(v: Cell) -> bool {
    v.0 == 0 || v.1 == 0 || v.0.abs() == v.1.abs()
}

/// `v` scaled to length `len` and truncated toward zero (`(0, 0)` stays).
fn scale_to(v: Cell, len: i32) -> Cell {
    let l = ((v.0 as f64).powi(2) + (v.1 as f64).powi(2)).sqrt();
    if l == 0.0 {
        return (0, 0);
    }
    let k = len as f64 / l;
    ((v.0 as f64 * k) as i32, (v.1 as f64 * k) as i32)
}

fn stun_stats(value: i32) -> Vec<(MoveStat, i32)> {
    vec![
        (MoveStat::StunThresholdWhilePerformingAction, value),
        (MoveStat::VirtualHeavyStunThreshold, value),
        (MoveStat::VirtualLightStunThreshold, value),
    ]
}

fn roll_end_stats(out: &mut Vec<MoveEvent>) {
    let mut stats = stun_stats(0);
    stats.push((MoveStat::IsDodgeRolling, 0));
    out.push(MoveEvent::Stats(stats));
}

fn sprint_off_stats() -> Vec<(MoveStat, i32)> {
    vec![
        (MoveStat::BaseMoveSpeedFromSprint, 0),
        (MoveStat::MovementVelocityPermyriad, 0),
        (MoveStat::SprintBlendTimeOverrideMs, 0),
        (MoveStat::IsSprinting, 0),
        (MoveStat::VirtualSkillIgnoreNamelockingDynamically, 0),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{grid::VecGrid, movement::Open};
    use std::collections::BTreeMap;

    const MOVE: u32 = 0x2000_2909;
    const ROLL: u32 = 0x8000_0040;
    const ATTACK: u32 = 0x8040_0040;

    /// One entity and its frame clock.
    struct Rig {
        loco: Locomotion,
        cfg: MovementConfig,
        grid: Box<dyn Walkable>,
        frame: u64,
        next_action: u16,
        next_buff: u16,
    }

    impl Rig {
        fn new(cell: Cell) -> Self {
            Self::on(cell, Box::new(Open))
        }

        fn on(cell: Cell, grid: Box<dyn Walkable>) -> Self {
            let cfg = MovementConfig::new(
                SkillIds {
                    move_skill: MOVE,
                    roll_skill: ROLL,
                    roll_skill_other_set: u32::MAX,
                },
                [100, 101, 102],
            );
            Self {
                loco: Locomotion::new(cell, cfg.walk_speed),
                cfg,
                grid,
                frame: 0,
                next_action: 5000,
                next_buff: 0,
            }
        }

        /// Submits `inputs` and ticks once.
        fn tick(&mut self, inputs: &[MoveInput]) -> Vec<MoveEvent> {
            for i in inputs {
                self.loco.input(*i);
            }
            self.frame += 1;
            let mut ctx = StepCtx {
                frame: self.frame,
                dt: 0.033,
                terrain: &*self.grid,
                config: &self.cfg,
                next_action: &mut self.next_action,
                next_buff: &mut self.next_buff,
            };
            self.loco.tick(&mut ctx)
        }

        fn mover_motion(&self) -> Motion {
            self.loco.mover.motion
        }

        /// Ticks `n` frames without input, collecting `(frame, event)`.
        fn run(&mut self, n: u64) -> Vec<(u64, MoveEvent)> {
            let mut all = Vec::new();
            for _ in 0..n {
                let ev = self.tick(&[]);
                all.extend(ev.into_iter().map(|e| (self.frame, e)));
            }
            all
        }

        /// Ticks `n` times with the inputs `f(frame)` gives for each frame.
        fn run_with(&mut self, n: u64, mut f: impl FnMut(u64) -> Vec<MoveInput>) -> Vec<(u64, MoveEvent)> {
            let mut all = Vec::new();
            for _ in 0..n {
                let inputs = f(self.frame + 1);
                let ev = self.tick(&inputs);
                all.extend(ev.into_iter().map(|e| (self.frame, e)));
            }
            all
        }
    }

    fn dir(x: i32, y: i32) -> MoveInput {
        MoveInput::Direction { x, y }
    }

    fn skill(skill: u32, x: i32, y: i32) -> UseSkill {
        UseSkill {
            skill,
            flags: 0x400,
            unk: 0xff,
            target_flags: 0,
            entity: None,
            x,
            y,
        }
    }

    fn roll(x: i32, y: i32) -> MoveInput {
        MoveInput::UseSkill(skill(ROLL, x, y))
    }

    fn space(held: bool) -> MoveInput {
        MoveInput::SprintInput { held }
    }

    fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
        ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
    }

    #[test]
    fn wasd_is_acked_in_the_next_frame_and_moves_from_the_one_after() {
        let mut r = Rig::new((100, 100));
        let start = r.loco.position();
        assert_eq!(
            r.tick(&[dir(180, 0)]),
            vec![MoveEvent::MoveAck {
                dir: (180, 0),
                cell: (100, 100)
            }]
        );
        assert_eq!(
            r.loco.state(),
            &MoveState::Walking(Walk::Direction((180, 0)))
        );
        // The client turns when it receives the reply; the server moves from the next frame.
        assert_eq!(r.loco.position(), start);
        r.tick(&[]);
        assert!((dist(r.loco.position(), start) - 37.0 * 0.033).abs() < 1e-3);
        // Same vector again: acked again, state kept.
        assert_eq!(r.tick(&[dir(180, 0)]).len(), 1);
        // Turn.
        let ev = r.tick(&[dir(0, -180)]);
        assert!(matches!(
            &ev[..],
            [MoveEvent::MoveAck { dir: (0, -180), .. }]
        ));
    }

    #[test]
    fn wasd_stop_goes_idle() {
        let mut r = Rig::new((10, 10));
        r.tick(&[dir(127, 127)]);
        r.run(5);
        let ev = r.tick(&[dir(0, 0)]);
        assert_eq!(
            ev,
            vec![MoveEvent::MoveAck {
                dir: (0, 0),
                cell: r.loco.cell()
            }]
        );
        assert_eq!(r.loco.state(), &MoveState::Idle);
        // The client walks one more frame after the `(0, 0)`.
        let before = r.loco.position();
        r.run(1);
        let at = r.loco.position();
        assert!(dist(before, at) > 1.0);
        r.run(5);
        assert_eq!(r.loco.position(), at);
        assert!(r.run(5).is_empty());
    }

    #[test]
    fn walking_stops_at_a_wall() {
        let grid = VecGrid::from_rows(&["..........", "..........", "##########"]);
        let mut r = Rig::on((2, 1), Box::new(grid));
        r.tick(&[dir(0, 180)]);
        r.run(20);
        assert_eq!(r.loco.cell().1, 1);
    }

    // ---- 2026-10-20 capture (WP2.1b)

    #[test]
    fn a_roll_runs_along_the_wasd_keys_not_towards_the_cursor() {
        // 14 of 14 rolls with WASD held went along the key vector while the cursor offset
        // (and so the StartAction target) pointed elsewhere.
        let mut r = Rig::new((100, 100));
        r.tick(&[dir(180, 0)]);
        r.run(3);
        let start = r.loco.position();
        let ev = r.tick(&[roll(0, 30)]);
        assert!(
            matches!(&ev[0], MoveEvent::ActionStart { target, .. } if target.1 > r.loco.cell().1 + 20),
            "the target stays the cursor"
        );
        r.run(31);
        let end = r.loco.position();
        assert!(end.0 - start.0 > 40.0, "{start:?} -> {end:?}");
        assert!((end.1 - start.1).abs() < 2.0, "{start:?} -> {end:?}");
    }

    #[test]
    fn a_standing_roll_runs_towards_the_cursor_and_steers() {
        let mut r = Rig::new((100, 100));
        let start = r.loco.position();
        r.tick(&[roll(30, 0)]);
        r.run(8);
        r.tick(&[MoveInput::UpdateSkillTarget {
            x: 0,
            y: 30,
            skill: ROLL,
        }]);
        r.run(30);
        let end = r.loco.position();
        assert!(
            end.0 - start.0 > 5.0 && end.1 - start.1 > 5.0,
            "{start:?} -> {end:?}"
        );
        assert!(dist(start, end) > 28.0, "a bent path of 46 cells");
    }

    #[test]
    fn a_roll_along_the_keys_turns_with_them() {
        // Official: a WASD change during the roll turns the roll (2 frames later on the client).
        let mut r = Rig::new((100, 100));
        r.tick(&[dir(180, 0)]);
        r.run(2);
        r.tick(&[roll(0, 30)]);
        r.run(5);
        let mid = r.loco.position();
        r.tick(&[dir(0, 180)]);
        r.run(30);
        let end = r.loco.position();
        assert!(end.1 - mid.1 > 10.0, "{mid:?} -> {end:?}");
    }

    #[test]
    fn sprint_directions_are_scaled_to_length_550_and_truncated() {
        let mut r = sprinting((94, -153));
        let ev = r.tick(&[dir(94, -153)]);
        assert!(
            matches!(
                &ev[..],
                [MoveEvent::MoveAck {
                    dir: (287, -468),
                    ..
                }]
            ),
            "{ev:?}"
        );
        let ev = r.tick(&[dir(93, -154)]);
        assert!(
            matches!(
                &ev[..],
                [MoveEvent::MoveAck {
                    dir: (284, -470),
                    ..
                }]
            ),
            "{ev:?}"
        );
    }

    #[test]
    fn a_weapon_swap_is_answered_in_the_same_batch() {
        // 2026-10-20_18-41-14: 0xfe {1, 1} and {1, 0} (standing) -> 0x1d5 + 0xff in r+0.
        let mut r = Rig::new((50, 50));
        assert_eq!(
            r.tick(&[MoveInput::WeaponSwap { set: 1 }]),
            vec![MoveEvent::WeaponSet {
                set: 1,
                requested: true
            }]
        );
        assert_eq!(
            r.tick(&[MoveInput::WeaponSwap { set: 0 }]),
            vec![MoveEvent::WeaponSet {
                set: 0,
                requested: true
            }]
        );
        assert_eq!(r.tick(&[]), vec![]);
    }

    #[test]
    fn the_input_mode_toggle_changes_the_stance_in_the_same_batch() {
        let mut r = Rig::new((50, 50));
        r.loco.set_stance(true);
        assert_eq!(
            r.tick(&[
                MoveInput::StanceRequest { value: 1 },
                MoveInput::AimMode { mode: 0 }
            ]),
            vec![
                MoveEvent::OrientationLerp { value: 0.2 },
                MoveEvent::StanceChanged {
                    combat: false,
                    request: Some(1)
                }
            ]
        );
        assert!(!r.loco.in_combat_stance());
        assert_eq!(
            r.tick(&[
                MoveInput::StanceRequest { value: 4 },
                MoveInput::AimMode { mode: 1 }
            ]),
            vec![
                MoveEvent::OrientationLerp { value: 0.05 },
                MoveEvent::StanceChanged {
                    combat: true,
                    request: Some(4)
                }
            ]
        );
        // No change and no request: no event.
        assert!(r.tick(&[MoveInput::AimMode { mode: 1 }]).is_empty());
        // A request alone in the same stance is still answered (2026-10-14: the first skill
        // use in mouse mode can come when the stance is already combat).
        assert_eq!(
            r.tick(&[
                MoveInput::StanceRequest { value: 4 },
                MoveInput::AimMode { mode: 1 }
            ]),
            vec![
                MoveEvent::OrientationLerp { value: 0.05 },
                MoveEvent::StanceChanged {
                    combat: true,
                    request: Some(4)
                }
            ]
        );
    }

    /// 2026-10-14 entry: mode 1, `0x200` value 1 and mode 0 at f1; the entry stance (combat)
    /// 9 capture frames later (8 after the input's tick), then the request one frame after it (stance 0, value 1).
    #[test]
    fn a_request_before_entry_is_answered_after_the_entry_stance() {
        let mut r = Rig::new((10, 10));
        r.tick(&[
            MoveInput::AimMode { mode: 1 },
            MoveInput::StanceRequest { value: 1 },
            MoveInput::AimMode { mode: 0 },
        ]);
        let ev = r.run(12);
        assert_eq!(
            ev,
            vec![
                (9, MoveEvent::OrientationLerp { value: 0.05 }),
                (
                    9,
                    MoveEvent::StanceChanged {
                        combat: true,
                        request: None
                    }
                ),
                (10, MoveEvent::OrientationLerp { value: 0.2 }),
                (
                    10,
                    MoveEvent::StanceChanged {
                        combat: false,
                        request: Some(1)
                    }
                ),
            ]
        );
    }

    #[test]
    fn a_roll_pressed_and_released_in_one_batch_starts_one_batch_later() {
        let mut r = Rig::new((50, 50));
        let ev = r.tick(&[
            space(true),
            roll(10, 0),
            MoveInput::SkillInputEnd,
            space(false),
        ]);
        assert!(matches!(
            &ev[..],
            [
                MoveEvent::SprintState { held: true },
                MoveEvent::SkillAck { .. },
                MoveEvent::InputEndAck,
                MoveEvent::SprintState { held: false }
            ]
        ));
        let ev = r.tick(&[]);
        assert!(matches!(
            &ev[..],
            [
                MoveEvent::ActionStart { .. },
                MoveEvent::ActionEvent,
                MoveEvent::Stats(_),
                MoveEvent::Stats(_)
            ]
        ));
        assert!(r.loco.is_rolling());
    }

    #[test]
    fn an_attack_during_a_tap_roll_starts_at_t0_24() {
        let mut r = Rig::new((50, 50));
        r.tick(&[space(true), roll(10, 0)]);
        r.run(5);
        r.tick(&[MoveInput::SkillInputEnd, space(false)]);
        let mut atk = skill(ATTACK, 20, 0);
        atk.flags = 0x400;
        let ev = r.tick(&[MoveInput::UseSkill(atk)]);
        assert!(matches!(
            &ev[..],
            [MoveEvent::SkillAck { skill: ATTACK, .. }]
        ));
        // t0 = frame 1: the attack starts in frame 25, without a second ack.
        let ev = r.run(30);
        let start = ev
            .iter()
            .find(|(_, e)| matches!(e, MoveEvent::ActionStart { skill: ATTACK, .. }))
            .expect("attack start");
        assert_eq!(start.0, 25);
        assert!(
            !ev.iter()
                .any(|(_, e)| matches!(e, MoveEvent::SkillAck { .. }))
        );
        assert!(
            ev.iter()
                .any(|(f, e)| *f == 25 && matches!(e, MoveEvent::ActionEvent))
        );
        assert!(!r.loco.is_rolling());
    }

    #[test]
    fn a_key_released_before_must_sprint_ends_the_sprint_at_t0_35() {
        // Official (Space up at t0 + 19): `1b1`/`2e7` at once, the stream goes on scaled, the
        // sprint-off events come with `must_sprint` off at t0 + 35.
        let mut r = Rig::new((50, 50));
        r.tick(&[space(true), roll(10, 0)]);
        r.run(18);
        r.tick(&[dir(0, -180)]);
        let ev = r.tick(&[MoveInput::SkillInputEnd, space(false)]);
        assert!(ev.iter().any(|e| matches!(e, MoveEvent::InputEndAck)));
        assert!(!ev.iter().any(|e| matches!(e, MoveEvent::Stats(s) if s.iter().any(|(k, _)| *k == MoveStat::IsSprinting))));
        assert!(r.loco.sprint().is_some());
        let ev = r.run(20);
        let off = ev
            .iter()
            .find(|(_, e)| matches!(e, MoveEvent::Stats(s) if s.iter().any(|(k, v)| *k == MoveStat::IsSprinting && *v == 0)))
            .expect("sprint off");
        assert_eq!(off.0, 36, "t0 + 35 with t0 = frame 1");
        assert!(r.loco.sprint().is_none());
    }

    #[test]
    fn a_held_click_restarts_its_move_when_a_repeat_is_late() {
        let mut r = Rig::new((10, 10));
        r.tick(&[MoveInput::UseSkill(skill(MOVE, 60, 0))]);
        let mut held = skill(MOVE, 60, 0);
        held.flags = 0;
        held.target_flags = UseSkill::HELD_REPEAT;
        let mut frames = Vec::new();
        // The client repeats every 2 frames while the button is down, but skips a beat now
        // and then (the first repeat after the press here, and the one at 14 comes at 15).
        let repeats = [4, 6, 8, 10, 12, 15, 17, 19, 21, 23, 25];
        for _ in 0..30 {
            let inputs = if repeats.contains(&(r.frame + 1)) {
                vec![MoveInput::UseSkill(held)]
            } else {
                vec![]
            };
            for e in r.tick(&inputs) {
                // The restarts carry the latest request's flags without 0x10.
                if matches!(e, MoveEvent::ActionStart { flags, .. } if flags & ACTION_INPUT_FLAG == 0)
                {
                    frames.push(r.frame);
                }
            }
        }
        // 2 frames after the press' start (frame 1) and after the repeat at 12; after the last
        // repeat (25) every 2 frames until the hold is stale (6 frames).
        assert_eq!(frames, [3, 14, 27, 29, 31]);
        // Released: no more restarts.
        r.tick(&[MoveInput::SkillReleased {
            skill: MOVE,
            flag: 0,
        }]);
        assert!(
            !r.run(40)
                .iter()
                .any(|(_, e)| matches!(e, MoveEvent::ActionStart { .. }))
        );
    }

    fn held(x: i32, y: i32) -> MoveInput {
        let mut h = skill(MOVE, x, y);
        h.flags = 0;
        h.target_flags = UseSkill::HELD_REPEAT;
        MoveInput::UseSkill(h)
    }

    fn move_starts(ev: &[MoveEvent]) -> Vec<(u16, Cell)> {
        ev.iter()
            .filter_map(|e| match e {
                MoveEvent::ActionStart {
                    kind: ActionKind::Move,
                    flags,
                    target,
                    ..
                } => Some((*flags, *target)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_held_click_takes_over_the_sprint_when_must_sprint_goes_off() {
        // Official (8 of 8 sprints with a held click past t0 + 35): at `must_sprint` off a
        // Move (flags 0x10) towards the goal of a repeat at least 2 frames old, after the
        // `MoveAck`; every later repeat starts a Move; the sprint goes on.
        let mut r = Rig::new((50, 50));
        r.tick(&[space(true), roll(10, 0)]); // t0 = 1
        r.run(18);
        r.tick(&[cursor(180, 0)]);
        let mut acks = BTreeMap::new();
        while r.frame < 35 {
            let inputs = if r.frame % 2 == 1 { vec![held(30, r.frame as i32)] } else { vec![] };
            for e in r.tick(&inputs) {
                assert!(move_starts(std::slice::from_ref(&e)).is_empty(), "only acked before t0 + 35");
                if let MoveEvent::SkillAck { skill: MOVE, x, y, .. } = e {
                    acks.insert(r.frame, (x, y));
                }
            }
        }
        // Frame 36 = t0 + 35; its repeat is handled first and only acked.
        let ev = r.tick(&[held(30, 99)]);
        let ack = ev.iter().position(|e| matches!(e, MoveEvent::MoveAck { .. })).unwrap();
        let start = ev
            .iter()
            .position(|e| matches!(e, MoveEvent::ActionStart { kind: ActionKind::Move, .. }))
            .expect("Move at must_sprint off");
        assert!(start > ack);
        assert_eq!(move_starts(&ev), [(ACTION_INPUT_FLAG, acks[&34])]);
        assert!(matches!(r.loco.state(), MoveState::Sprinting(_)));
        r.tick(&[]);
        let ev = r.tick(&[held(30, 0)]);
        assert_eq!(move_starts(&ev).len(), 1, "a repeat starts a Move");
        assert!(matches!(r.loco.state(), MoveState::Sprinting(_)));
        // Space up: sprint-off with no `StartAction`, `(0, 0)` in the next frame, then the
        // repeats are only acked while `SprintEnd` plays and nothing restarts.
        let ev = r.tick(&[MoveInput::SkillInputEnd, space(false), cursor(180, 0)]);
        assert!(move_starts(&ev).is_empty());
        assert!(ev.iter().any(|e| matches!(e, MoveEvent::Stats(s) if s.contains(&(MoveStat::IsSprinting, 0)))));
        let ev = r.tick(&[cursor(180, 2)]);
        let dirs: Vec<_> = ev
            .iter()
            .filter_map(|e| match e {
                MoveEvent::MoveAck { dir, .. } => Some(*dir),
                _ => None,
            })
            .collect();
        assert_eq!(dirs, [(0, 0)]);
        let mut first = true;
        for (_, e) in r.run_with(10, |f| {
            let zero = std::mem::take(&mut first);
            match (f % 2 == 0, zero) {
                (_, true) => vec![dir(0, 0)],
                (true, _) => vec![held(30, 0)],
                _ => vec![],
            }
        }) {
            assert!(!matches!(e, MoveEvent::ActionStart { .. } | MoveEvent::MoveAck { .. }), "{e:?}");
        }
    }

    #[test]
    fn a_roll_pressed_with_a_repeat_that_starts_a_move_starts_one_batch_later() {
        // Official (2026-10-20 f3712, f4485; 2026-11-02 f8037, f8075): the ack now, the
        // `StartAction` in the next batch without an ack, the stun stats and the timeline one
        // frame later still.
        let mut r = Rig::new((50, 50));
        r.tick(&[MoveInput::UseSkill(skill(MOVE, 30, 0))]);
        r.tick(&[]);
        let ev = r.tick(&[held(30, 0), space(true), roll(30, 0)]);
        assert_eq!(move_starts(&ev).len(), 1);
        assert!(ev.iter().any(|e| matches!(e, MoveEvent::SkillAck { skill: ROLL, .. })));
        assert!(!ev.iter().any(|e| matches!(e, MoveEvent::ActionStart { kind: ActionKind::Roll, .. })));
        let ev = r.tick(&[]); // frame 4
        assert!(ev.iter().any(|e| matches!(e, MoveEvent::ActionStart { kind: ActionKind::Roll, .. })));
        assert!(!ev.iter().any(|e| matches!(e, MoveEvent::SkillAck { .. })));
        let stun = |ev: &[MoveEvent]| {
            ev.iter().any(|e| matches!(e, MoveEvent::Stats(s) if s.iter().any(|(k, v)| *k == MoveStat::VirtualLightStunThreshold && *v != 0)))
        };
        assert!(!stun(&ev));
        assert!(stun(&r.tick(&[]))); // frame 5 = the timeline's t0
        let ev = r.run(16);
        let sprint = ev
            .iter()
            .find(|(_, e)| matches!(e, MoveEvent::BuffAdded { .. }))
            .expect("sprint start");
        assert_eq!(sprint.0, 20, "t0 + 15 with t0 = 5");
    }

    #[test]
    fn space_pressed_again_before_t0_35_keeps_the_sprint_running() {
        // Official 2026-11-02 f2178: released at t0 + 22, pressed again (with the roll key) at
        // t0 + 30, handled at t0 + 31: the roll press is only acked, `must_sprint` goes off at t0 + 35 without the
        // sprint-off, and the next release ends it in its own batch.
        let mut r = Rig::new((50, 50));
        r.tick(&[space(true), roll(10, 0)]); // t0 = 1
        r.run(18);
        r.tick(&[dir(127, 127)]);
        r.run(2);
        r.tick(&[MoveInput::SkillInputEnd, space(false)]); // frame 23
        r.run(8);
        let ev = r.tick(&[space(true), roll(10, 0)]); // frame 32, handled while sprinting
        assert!(!ev.iter().any(|e| matches!(e, MoveEvent::ActionStart { .. })));
        for (_, e) in r.run(10) {
            assert!(!matches!(e, MoveEvent::Stats(s) if s.contains(&(MoveStat::IsSprinting, 0))));
        }
        assert!(r.loco.sprint().is_some());
        let ev = r.tick(&[MoveInput::SkillInputEnd, space(false)]);
        assert!(ev.iter().any(|e| matches!(e, MoveEvent::Stats(s) if s.contains(&(MoveStat::IsSprinting, 0)))));
    }

    #[test]
    fn a_hold_without_repeats_expires_without_a_release() {
        let mut r = Rig::new((10, 10));
        r.tick(&[MoveInput::UseSkill(skill(MOVE, 200, 0))]);
        // Nothing follows (the capture's holds that ended with a roll or attack): restarts
        // every 2 frames until the hold is stale (6 frames), then none, and a later roll
        // does not resume the walk.
        let ev = r.run(40);
        let starts: Vec<u64> = ev
            .iter()
            .filter(|(_, e)| matches!(e, MoveEvent::ActionStart { .. }))
            .map(|(f, _)| *f)
            .collect();
        assert_eq!(starts, [3, 5, 7]);
        r.tick(&[roll(0, 30)]);
        let ev = r.run(31);
        assert!(!ev.iter().any(|(_, e)| matches!(
            e,
            MoveEvent::ActionStart {
                kind: ActionKind::Move,
                ..
            }
        )));
    }

    #[test]
    fn releasing_the_roll_key_with_skill_released_answers_both_acks() {
        let mut r = Rig::new((50, 50));
        let ev = r.tick(&[MoveInput::SkillReleased {
            skill: ROLL,
            flag: 0,
        }]);
        assert_eq!(
            ev,
            vec![
                MoveEvent::InputEndAck,
                MoveEvent::ReleasedAck {
                    skill: ROLL,
                    flag: 0
                }
            ]
        );
    }

    #[test]
    fn standing_roll_batch_and_timeline() {
        let mut r = Rig::new((50, 50));
        let ev = r.tick(&[space(true), roll(15, -35)]);
        let kinds: Vec<_> = ev
            .iter()
            .map(|e| match e {
                MoveEvent::SprintState { .. } => "sprint",
                MoveEvent::ActionStart { .. } => "start",
                MoveEvent::ActionEvent => "event",
                MoveEvent::Stats(_) => "stats",
                MoveEvent::SkillAck { .. } => "ack",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["sprint", "start", "event", "stats", "ack", "stats"]);
        let MoveEvent::ActionStart {
            cell,
            target,
            kind,
            action,
            skill,
            ..
        } = &ev[1]
        else {
            panic!()
        };
        assert_eq!(
            (*cell, *target, *kind, *skill),
            ((50, 50), (65, 15), ActionKind::Roll, ROLL)
        );
        assert_eq!(*action, 5000);
        assert!(matches!(
            &ev[4],
            MoveEvent::SkillAck {
                x: 65,
                y: 15,
                flags: 0x400,
                unk: 0xff,
                ..
            }
        ));
        assert!(r.loco.is_rolling());
        // Space is still down at +15: the sprint starts there.
        let later = r.run(15);
        let at15: Vec<_> = later.iter().filter(|(f, _)| *f == 16).collect();
        assert_eq!(at15.len(), 4, "{at15:?}");
        assert!(matches!(
            at15[0].1,
            MoveEvent::BuffAdded {
                index: 0,
                row: 100,
                fade: None
            }
        ));
        assert!(matches!(
            at15[2].1,
            MoveEvent::BuffAdded {
                index: 2,
                row: 102,
                ..
            }
        ));
        assert!(
            matches!(&at15[3].1, MoveEvent::Stats(s) if s.contains(&(MoveStat::IsSprinting, 1)))
        );
        // Nothing at +16 (tap stats are skipped once the roll became a sprint).
        assert!(r.run(1).is_empty());
    }

    #[test]
    fn tap_roll_ends_its_stats_at_16_and_the_action_at_30() {
        let mut r = Rig::new((50, 50));
        r.tick(&[roll(30, 0)]);
        r.tick(&[MoveInput::SkillInputEnd, space(false)]);
        let ev = r.run(40);
        let frames: Vec<u64> = ev.iter().map(|(f, _)| *f).collect();
        // Roll batch at frame 1: tap stats at 1 + 16, nothing else.
        assert_eq!(frames, [17]);
        assert!(matches!(&ev[0].1, MoveEvent::Stats(s)
            if s.len() == 4 && s[3] == (MoveStat::IsDodgeRolling, 0)));
        assert_eq!(r.loco.state(), &MoveState::Idle);
        // About 46 cells to the right (the speed profile may stop it a little short).
        assert!(r.loco.position().0 > 50.0 + 40.0, "{:?}", r.loco.position());
        assert!((r.loco.position().1 - 50.5).abs() < 0.1);
    }

    #[test]
    fn roll_state_lasts_30_frames_then_walking_resumes() {
        let mut r = Rig::new((50, 50));
        r.tick(&[dir(0, 180)]);
        r.tick(&[roll(30, 0)]);
        // A WASD change during the roll is acked at once and acted on after the roll.
        let ev = r.tick(&[dir(180, 0)]);
        assert!(matches!(
            &ev[..],
            [MoveEvent::MoveAck { dir: (180, 0), .. }]
        ));
        assert!(r.loco.is_rolling());
        r.run(28);
        assert!(r.loco.is_rolling(), "t0 + 29");
        r.run(1);
        assert_eq!(
            r.loco.state(),
            &MoveState::Walking(Walk::Direction((180, 0)))
        );
    }

    #[test]
    fn roll_steering_moves_the_target_and_is_acked() {
        let mut r = Rig::new((50, 50));
        r.tick(&[roll(30, 0)]);
        let ev = r.tick(&[MoveInput::UpdateSkillTarget {
            x: 0,
            y: 30,
            skill: ROLL,
        }]);
        // Relative to the server's cell at the time.
        let cell = r.loco.cell();
        assert_eq!(
            ev,
            vec![MoveEvent::TargetAck {
                skill: ROLL,
                x: cell.0,
                y: cell.1 + 30
            }]
        );
        r.run(30);
        let (x, y) = r.loco.position();
        assert!(y > 50.0 + 20.0 && x < 50.0 + 20.0, "{x} {y}");
    }

    #[test]
    fn no_aim_echo_while_rolling_or_sprinting() {
        let mut r = Rig::new((50, 50));
        let aim = MoveInput::Aim { x: 60, y: 61 };
        assert_eq!(r.tick(&[aim]), vec![MoveEvent::AimEcho { cell: (60, 61) }]);
        assert!(r.tick(&[AimEntityInput()]).is_empty());
        r.tick(&[space(true), roll(10, 0)]);
        assert!(r.tick(&[aim]).is_empty());
        // Sprint, after the roll action and `must_sprint` (a key released earlier ends the
        // sprint at t0 + 35): still no echo.
        r.run(36);
        assert!(r.loco.sprint().is_some());
        assert!(r.tick(&[aim]).is_empty());
        r.tick(&[MoveInput::SkillInputEnd, space(false)]);
        assert_eq!(r.tick(&[aim]), vec![MoveEvent::AimEcho { cell: (60, 61) }]);
    }

    #[allow(non_snake_case)]
    fn AimEntityInput() -> MoveInput {
        MoveInput::AimEntity { entity: 7 }
    }

    /// Roll with Space held until the sprint runs and a vector is set.
    fn sprinting(wasd: Cell) -> Rig {
        let mut r = Rig::new((50, 50));
        r.tick(&[space(true), roll(10, 0)]);
        r.run(36); // past `must_sprint` off at t0 + 35
        assert!(matches!(r.loco.state(), MoveState::Sprinting(_)));
        if wasd != (0, 0) {
            r.tick(&[dir(wasd.0, wasd.1)]);
        }
        r
    }

    #[test]
    fn sprint_stream_is_one_scaled_move_ack_per_frame() {
        let mut r = sprinting((127, -127));
        // The reply to the vector is the stream's first message, scaled with integer division.
        // (`sprinting` consumed it; the next frames follow.)
        for _ in 0..5 {
            let ev = r.tick(&[]);
            assert!(
                matches!(
                    &ev[..],
                    [MoveEvent::MoveAck {
                        dir: (388, -388),
                        ..
                    }]
                ),
                "{ev:?}"
            );
        }
        // A new vector changes the next message.
        let ev = r.tick(&[dir(-53, 172)]);
        assert!(matches!(
            &ev[..],
            [MoveEvent::MoveAck {
                dir: (-161, 525),
                ..
            }]
        ));
        // Speed: 1.5 x walking.
        let p = r.loco.position();
        r.tick(&[]);
        let step = dist(p, r.loco.position());
        assert!((step - 37.0 * 1.5 * 0.033).abs() < 0.01, "{step}");
    }

    #[test]
    fn a_vector_during_the_roll_before_the_sprint_is_not_scaled() {
        let mut r = Rig::new((50, 50));
        r.tick(&[space(true), roll(10, 0)]);
        let ev = r.tick(&[dir(127, 127)]);
        assert!(matches!(
            &ev[..],
            [MoveEvent::MoveAck {
                dir: (127, 127),
                ..
            }]
        ));
        // No stream before the sprint starts (t0 + 15).
        assert!(r.run(13).is_empty());
        // The first vector after the start begins it, still during the roll.
        r.run(1);
        let ev = r.tick(&[dir(127, 127)]);
        assert!(matches!(
            &ev[..],
            [MoveEvent::MoveAck {
                dir: (388, 388),
                ..
            }]
        ));
        assert!(r.loco.is_rolling());
        assert_eq!(r.tick(&[]).len(), 1);
    }

    #[test]
    fn sprint_timeline_t0_15_30_35() {
        let mut r = Rig::new((50, 50));
        r.tick(&[space(true), roll(10, 0)]);
        let ev = r.run(40);
        let at = |f: u64| {
            ev.iter()
                .filter(|(g, _)| *g == f)
                .map(|(_, e)| e)
                .collect::<Vec<_>>()
        };
        assert_eq!(at(16).len(), 4);
        assert!(at(30).is_empty() && at(31).len() == 4, "{:?}", at(31));
        // t0 + 30: roll-end stats, sprint speed stats, buff n + 1 again, buff n with a fade.
        assert!(matches!(
            at(31)[2],
            MoveEvent::BuffAdded {
                index: 1,
                row: 101,
                fade: None
            }
        ));
        assert!(
            matches!(at(31)[3], MoveEvent::BuffAdded { index: 0, row: 100, fade: Some(f) } if (*f - 0.1).abs() < 1e-6)
        );
        // t0 + 35: `must_sprint` off.
        assert!(matches!(
            at(36)[..],
            [MoveEvent::BuffRemoved { index: 0 }, MoveEvent::Stats(_)]
        ));
    }

    #[test]
    fn sprint_release_sequence_and_unscaled_follow_up() {
        let mut r = sprinting((0, -180));
        r.run(3);
        let ev = r.tick(&[MoveInput::SkillInputEnd, space(false)]);
        let names: Vec<&str> = ev
            .iter()
            .map(|e| match e {
                MoveEvent::InputEndAck => "1b1",
                MoveEvent::SprintState { held: false } => "2e7",
                MoveEvent::MoveAck { dir: (0, -550), .. } => "64 scaled",
                MoveEvent::BuffRemoved { index: 1 } => "1ec n+1",
                MoveEvent::BuffRemoved { index: 2 } => "1ec n+2",
                MoveEvent::Stats(s) if s.len() == 5 => "off",
                MoveEvent::Stats(s) if s == &[(MoveStat::AllHitsStunYou, 0)] => "all_hits",
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            names,
            [
                "1b1",
                "2e7",
                "64 scaled",
                "1ec n+1",
                "off",
                "1ec n+2",
                "all_hits"
            ]
        );
        // The client coasts through its `SprintEnd` animation (no walk before its end).
        assert_eq!(r.loco.state(), &MoveState::Idle);
        assert!(matches!(r.mover_motion(), Motion::Coast { .. }));
        let ev = r.tick(&[]);
        assert!(matches!(
            &ev[..],
            [MoveEvent::MoveAck { dir: (0, -180), .. }]
        ));
        assert!(r.run(5).is_empty());
    }

    #[test]
    fn a_zero_vector_during_the_sprint_stops_the_stream() {
        // UNVERIFIED (gap #4): the official client never sends a zero vector in a sprint (it
        // sends the cursor direction), so this is only a guard.
        let mut r = sprinting((0, -180));
        let ev = r.tick(&[dir(0, 0)]);
        assert!(matches!(&ev[..], [MoveEvent::MoveAck { dir: (0, 0), .. }]));
        assert!(r.run(3).is_empty());
        assert!(matches!(r.loco.state(), MoveState::Sprinting(_)));
        // Releasing now ends the sprint without a `MoveAck`.
        let ev = r.tick(&[MoveInput::SkillInputEnd, space(false)]);
        assert!(!ev.iter().any(|e| matches!(e, MoveEvent::MoveAck { .. })));
        assert_eq!(r.loco.state(), &MoveState::Idle);
    }

    /// Client speed factors (step / 1.2333) after the Melee start at f4005 of the 2026-10-20
    /// capture (held-click walk, attack at frame 0), frames 1..=16; frame 6 is a partial step.
    const CAPTURE_ATTACK_FACTORS: [f32; 16] = [
        0.958, 0.912, 0.850, 0.803, 0.748, 0.607, 0.306, 0.303, 0.306, 0.304, 0.305, 0.306, 0.357,
        0.411, 0.468, 0.518,
    ];

    #[test]
    fn an_attack_while_walking_slows_the_walk_like_the_client() {
        let mut r = Rig::new((50, 50));
        r.tick(&[dir(180, 0)]);
        r.run(5);
        let full = step(&mut r, &[]);
        assert!((full - 37.0 * 0.033).abs() < 1e-3, "{full}");
        r.tick(&[MoveInput::UseSkill(skill(ATTACK, 20, 0))]);
        let (mut ours, mut theirs) = (0.0, 0.0);
        for (n, want) in CAPTURE_ATTACK_FACTORS.iter().enumerate() {
            let got = step(&mut r, &[]) / full;
            // Frame 6 is the client's partial step into the drop.
            let tol = if n + 1 == 6 { 0.1 } else { 0.03 };
            assert!((got - want).abs() < tol, "frame {}: {got} vs {want}", n + 1);
            ours += got * full;
            theirs += want * full;
        }
        assert!((ours - theirs).abs() < 0.3, "{ours} vs {theirs} cells");
        assert!(matches!(r.loco.state, MoveState::Walking(_)));
    }

    #[test]
    fn the_walk_is_back_to_full_speed_after_the_ramp() {
        let mut r = Rig::new((50, 50));
        r.tick(&[dir(180, 0), MoveInput::UseSkill(skill(ATTACK, 20, 0))]);
        // 12 locked frames, then 14 frames from 0.3 to 1 at 0.0495 per frame.
        r.run(26);
        assert!(r.loco.mover.factor < 1.0);
        r.run(1);
        assert_eq!(r.loco.mover.factor, 1.0);
        assert!(r.loco.attack_slow.is_none());
        let s = step(&mut r, &[]);
        assert!((s - 37.0 * 0.033).abs() < 1e-3, "{s}");
    }

    #[test]
    fn a_walk_started_during_the_attack_is_slow_from_its_first_step() {
        let mut r = Rig::new((50, 50));
        r.tick(&[MoveInput::UseSkill(skill(ATTACK, 20, 0))]);
        r.run(7);
        r.tick(&[dir(180, 0)]);
        let s = step(&mut r, &[]);
        assert!((s - 0.3 * 37.0 * 0.033).abs() < 1e-3, "{s}");
    }

    #[test]
    fn a_new_attack_restarts_the_slow() {
        let mut r = Rig::new((50, 50));
        r.tick(&[dir(180, 0), MoveInput::UseSkill(skill(ATTACK, 20, 0))]);
        r.run(20);
        let before = r.loco.mover.factor;
        assert!(before > 0.3 && before < 1.0, "{before}");
        r.tick(&[MoveInput::UseSkill(skill(ATTACK, 20, 0))]);
        r.run(6);
        assert!(r.loco.mover.factor < before);
        r.run(1);
        assert_eq!(r.loco.mover.factor, 0.3);
    }

    #[test]
    fn a_roll_ends_the_attack_slow() {
        let mut r = Rig::new((50, 50));
        r.tick(&[dir(180, 0), MoveInput::UseSkill(skill(ATTACK, 20, 0))]);
        r.run(8);
        assert_eq!(r.loco.mover.factor, 0.3);
        r.tick(&[space(true), roll(10, 0)]);
        assert!(r.loco.is_rolling());
        assert_eq!(r.loco.mover.factor, 1.0);
        assert!(r.loco.attack_slow.is_none());
    }

    #[test]
    fn an_attack_ends_the_sprint() {
        let mut r = sprinting((0, -180));
        r.run(5);
        let ev = r.tick(&[MoveInput::UseSkill(skill(ATTACK, 5, 5))]);
        let names: Vec<&str> = ev
            .iter()
            .map(|e| match e {
                MoveEvent::ActionStart {
                    kind: ActionKind::Other,
                    ..
                } => "1c8",
                MoveEvent::BuffRemoved { index: 1 } => "1ec n+1",
                MoveEvent::BuffRemoved { index: 2 } => "1ec n+2",
                MoveEvent::Stats(s) if s.len() == 5 => "off",
                MoveEvent::Stats(_) => "all_hits",
                MoveEvent::ActionEvent => "1ca",
                MoveEvent::SkillAck { .. } => "1ae",
                MoveEvent::MoveAck { dir: (0, -180), .. } => "64 unscaled",
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            names,
            [
                "1c8",
                "1ec n+1",
                "off",
                "1ca",
                "1ae",
                "64 unscaled",
                "1ec n+2",
                "all_hits"
            ]
        );
        assert_eq!(
            r.loco.state(),
            &MoveState::Walking(Walk::Direction((0, -180)))
        );
        // The key release now only gets its echo.
        let ev = r.tick(&[MoveInput::SkillInputEnd, space(false)]);
        assert_eq!(
            ev,
            vec![
                MoveEvent::InputEndAck,
                MoveEvent::SprintState { held: false }
            ]
        );
    }

    #[test]
    fn an_attack_just_after_the_roll_also_ends_must_sprint() {
        let mut r = Rig::new((50, 50));
        r.tick(&[space(true), roll(10, 0)]);
        r.run(31); // frame 32 = t0 + 31: sprinting, `must_sprint` still on
        assert!(matches!(r.loco.state(), MoveState::Sprinting(_)));
        let ev = r.tick(&[MoveInput::UseSkill(skill(ATTACK, 5, 5))]);
        assert!(matches!(
            &ev[..3],
            [
                MoveEvent::BuffRemoved { index: 0 },
                MoveEvent::Stats(_),
                MoveEvent::ActionStart { .. }
            ]
        ));
        // The t0 + 35 timer finds nothing left to do.
        assert!(r.run(5).is_empty());
    }

    #[test]
    fn a_roll_asked_for_during_a_roll_waits_for_its_end() {
        // Verified 2026-10-14 (27 frames) and 2026-10-20 (28, 29 frames).
        let mut r = Rig::new((50, 50));
        r.tick(&[roll(30, 0)]);
        r.run(26);
        let ev = r.tick(&[roll(0, 30)]);
        assert!(matches!(
            &ev[..],
            [MoveEvent::SkillAck {
                skill: ROLL,
                y: 80,
                ..
            }]
        ));
        let ev = r.run(3);
        assert_eq!(ev.len(), 4, "{ev:?}");
        assert_eq!(ev[0].0, 31, "t0 + 30");
        assert!(matches!(
            ev[0].1,
            MoveEvent::ActionStart {
                target: (_, 80),
                ..
            }
        ));
        assert!(matches!(ev[1].1, MoveEvent::ActionEvent));
        assert!(r.loco.is_rolling());
    }

    #[test]
    fn held_repeats_of_the_roll_skill_are_only_acked() {
        let mut r = Rig::new((50, 50));
        r.tick(&[space(true), roll(10, 0)]);
        r.run(31);
        let mut again = skill(ROLL, 10, 0);
        again.target_flags = 0x13;
        again.flags = 0;
        let ev = r.tick(&[MoveInput::UseSkill(again)]);
        assert!(matches!(
            &ev[..],
            [MoveEvent::SkillAck {
                flags: 0,
                target_flags: 0x13,
                ..
            }]
        ));
        assert!(matches!(r.loco.state(), MoveState::Sprinting(_)));
    }

    #[test]
    fn click_to_move_walks_around_a_wall_and_stops() {
        // Verified replies (2026-10-20); the walk itself uses the A* (gap #2: clicks on walls).
        let grid = VecGrid::from_rows(&[
            "....#....",
            "....#....",
            "....#....",
            "....#....",
            ".........",
        ]);
        let mut r = Rig::on((1, 0), Box::new(grid));
        let ev = r.tick(&[MoveInput::UseSkill(skill(MOVE, 6, 0))]);
        assert!(matches!(
            &ev[..],
            [
                MoveEvent::ActionStart {
                    kind: ActionKind::Move,
                    cell: (1, 0),
                    target: (7, 0),
                    ..
                },
                MoveEvent::SkillAck { x: 7, y: 0, .. }
            ]
        ));
        assert_eq!(
            r.loco.state(),
            &MoveState::Walking(Walk::Target { goal: (7, 0) })
        );
        let mut went_down = false;
        for _ in 0..60 {
            r.tick(&[]);
            went_down |= r.loco.cell().1 >= 3;
            assert_ne!(r.loco.cell(), (4, 0), "through the wall");
        }
        assert!(went_down);
        assert_eq!(r.loco.cell(), (7, 0));
        assert_eq!(r.loco.state(), &MoveState::Idle);
    }

    #[test]
    fn held_click_repeats_and_reaims() {
        let mut r = Rig::new((10, 10));
        r.tick(&[MoveInput::UseSkill(skill(MOVE, 20, 0))]);
        let mut held = skill(MOVE, 0, 20);
        held.target_flags = UseSkill::HELD_REPEAT;
        let ev = r.tick(&[MoveInput::UseSkill(held)]);
        // A repeat is answered like the press: a StartAction (request flags + 0x10) and the ack with
        // the new absolute target (2026-10-20 capture).
        assert!(
            matches!(
                &ev[..],
                [
                    MoveEvent::ActionStart {
                        kind: ActionKind::Move,
                        flags: 0x410,
                        ..
                    },
                    MoveEvent::SkillAck { y: 30, .. }
                ]
            ),
            "{ev:?}"
        );
        let ev = r.tick(&[MoveInput::UpdateSkillTarget {
            x: 5,
            y: 5,
            skill: MOVE,
        }]);
        // (The held click's periodic restart comes in the same frame.)
        let ev: Vec<_> = ev
            .into_iter()
            .filter(|e| !matches!(e, MoveEvent::ActionStart { .. }))
            .collect();
        let cell = r.loco.cell();
        assert_eq!(
            ev,
            vec![MoveEvent::TargetAck {
                skill: MOVE,
                x: cell.0 + 5,
                y: cell.1 + 5
            }]
        );
        assert!(
            matches!(r.loco.state(), MoveState::Walking(Walk::Target { goal }) if *goal == (cell.0 + 5, cell.1 + 5))
        );
        assert_eq!(
            r.tick(&[
                MoveInput::SkillReleased {
                    skill: MOVE,
                    flag: 0
                },
                MoveInput::SkillInputEnd
            ]),
            vec![
                MoveEvent::ReleasedAck {
                    skill: MOVE,
                    flag: 0
                },
                MoveEvent::InputEndAck
            ]
        );
    }

    #[test]
    fn wasd_takes_over_a_click_walk() {
        let mut r = Rig::new((10, 10));
        r.tick(&[MoveInput::UseSkill(skill(MOVE, 20, 0))]);
        r.tick(&[dir(0, 180)]);
        assert_eq!(
            r.loco.state(),
            &MoveState::Walking(Walk::Direction((0, 180)))
        );
    }

    #[test]
    fn entity_target_skills_use_absolute_cells() {
        let mut r = Rig::new((10, 10));
        let mut s = skill(ATTACK, 40, 41);
        s.entity = Some(77);
        s.target_flags = 8;
        let ev = r.tick(&[MoveInput::UseSkill(s)]);
        assert!(matches!(
            &ev[..],
            [
                MoveEvent::ActionStart {
                    kind: ActionKind::Other,
                    target: (40, 41),
                    entity: Some(77),
                    ..
                },
                MoveEvent::ActionEvent,
                MoveEvent::SkillAck {
                    x: 40,
                    y: 41,
                    entity: Some(77),
                    target_flags: 8,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn stance_changes_8_frames_after_aim_mode_one() {
        let mut r = Rig::new((10, 10));
        r.tick(&[MoveInput::AimMode { mode: 0 }]);
        r.tick(&[MoveInput::AimMode { mode: 1 }]);
        let ev = r.run(12);
        assert_eq!(
            ev,
            vec![
                (10, MoveEvent::OrientationLerp { value: 0.05 }),
                (
                    10,
                    MoveEvent::StanceChanged {
                        combat: true,
                        request: None
                    }
                )
            ]
        );
        assert!(r.loco.in_combat_stance());
        // Once only.
        r.tick(&[MoveInput::AimMode { mode: 1 }]);
        assert!(r.run(12).is_empty());
    }

    #[test]
    fn after_an_area_change_the_stance_comes_3_frames_after_the_mode_pair() {
        let mut r = Rig::new((10, 10));
        r.tick(&[MoveInput::AimMode { mode: 0 }, MoveInput::AimMode { mode: 1 }]);
        let ev = r.run(12);
        assert_eq!(
            ev,
            vec![
                (4, MoveEvent::OrientationLerp { value: 0.05 }),
                (
                    4,
                    MoveEvent::StanceChanged {
                        combat: true,
                        request: None
                    }
                )
            ]
        );
    }

    #[test]
    fn teleport_places_and_reports() {
        let mut r = Rig::new((10, 10));
        r.tick(&[dir(180, 0)]);
        r.loco.teleport((200, 300));
        let ev = r.tick(&[]);
        assert_eq!(ev, vec![MoveEvent::Teleport { cell: (200, 300) }]);
        assert_eq!(r.loco.cell(), (200, 300));
        assert_eq!(r.loco.state(), &MoveState::Idle);
    }

    #[test]
    fn sprint_buffs_come_from_the_config() {
        let mut r = Rig::new((0, 0));
        r.cfg.sprint.buff_rows = [7, 8, 9];
        r.next_buff = 4;
        r.tick(&[space(true), roll(10, 0)]);
        let ev = r.run(15);
        let rows: Vec<_> = ev
            .iter()
            .filter_map(|(_, e)| match e {
                MoveEvent::BuffAdded { index, row, .. } => Some((*index, *row)),
                _ => None,
            })
            .collect();
        assert_eq!(rows, [(4, 7), (5, 8), (6, 9)]);
        assert_eq!(r.next_buff, 7);
    }

    // ---- gap #11: after a sprint ends the client's walk is over (2026-10-20 captures) ----------

    /// Ticks once with `inputs` and returns the distance the entity moved in that frame.
    fn step(r: &mut Rig, inputs: &[MoveInput]) -> f32 {
        let before = r.loco.position();
        r.tick(inputs);
        dist(before, r.loco.position())
    }

    /// WASD held since before the roll (no `0x63` after the sprint started: no `MoveAck`
    /// stream), then Space released: tick `F`. Returns the rig after that tick.
    fn sprinting_with_a_key_held(key: Cell) -> Rig {
        let mut r = Rig::new((50, 50));
        r.tick(&[dir(key.0, key.1)]);
        r.run(2);
        r.tick(&[space(true), roll(10, 0)]);
        r.run(36);
        assert!(matches!(r.loco.state(), MoveState::Sprinting(s) if !s.streaming));
        r
    }

    #[test]
    fn a_streamless_sprint_ended_with_a_key_held_coasts_then_walks_on_after_the_sprint_end_animation() {
        let mut r = sprinting_with_a_key_held((180, 0));
        // F: the sprint-off batch; the client's Move action ends with it and `SprintEnd`
        // plays. No stream ran, so the client's direction was never scaled and no unscaled
        // `MoveAck` follows (its cell could only snap the client out of the coast). The
        // release batch ends the sprint before its stats apply: `SprintEnd` at sprint speed
        // (rate 1.117, 21 frames; traced 2026-11-02).
        let ev = r.tick(&[MoveInput::SkillInputEnd, space(false)]);
        assert!(!ev.iter().any(|e| matches!(e, MoveEvent::MoveAck { .. })));
        assert!(step(&mut r, &[]) < 1e-3); // F + 1: no step
        assert!(!r.run(19).iter().any(|(_, e)| matches!(e, MoveEvent::MoveAck { .. })));
        // F + 2 .. F + 20: the `SprintEnd` root motion, about 20 cells.
        assert_eq!(r.loco.state(), &MoveState::Idle);
        // F + 21: the animation is over and W is still down: Move, first step in F + 22.
        assert!(step(&mut r, &[]) < 0.1);
        let walk = r.cfg.walk_speed * 0.033;
        assert!((step(&mut r, &[]) - walk).abs() < 1e-3);
        assert_eq!(
            r.loco.state(),
            &MoveState::Walking(Walk::Direction((180, 0)))
        );
    }

    #[test]
    fn a_release_before_must_sprint_goes_off_ends_the_sprint_at_walking_speed() {
        // Space released at t0 + 25 (W held): the sprint ends at t0 + 35 with `must_sprint`
        // off and the sprint-off stats, no `SprintState` echo in that batch: the client's end
        // check sees the walking speed, `SprintEnd` plays at rate 0.745 (31 frames; p5 runs).
        let mut r = Rig::new((50, 50));
        r.tick(&[dir(180, 0)]);
        r.run(2);
        r.tick(&[space(true), roll(10, 0)]); // t0
        r.run(24);
        r.tick(&[MoveInput::SkillInputEnd, space(false)]); // t0 + 25
        r.run(9); // t0 + 34
        assert!(matches!(r.loco.state(), MoveState::Sprinting(_)));
        r.tick(&[]); // F = t0 + 35
        let f = r.loco.position();
        r.run(29); // F + 29: still coasting, no walk
        assert_eq!(r.loco.state(), &MoveState::Idle);
        let coast = dist(f, r.loco.position());
        assert!((coast - 19.4).abs() < 1.0, "coast {coast}");
        r.run(1); // F + 30
        assert_eq!(r.loco.state(), &MoveState::Idle);
        r.tick(&[]); // F + 31: Move
        let walk = r.cfg.walk_speed * 0.033;
        assert!((step(&mut r, &[]) - walk).abs() < 1e-3);
    }

    #[test]
    fn a_streamless_sprint_coasts_like_one_with_a_stream() {
        let mut r = sprinting_with_a_key_held((180, 0));
        let start = r.loco.position();
        r.tick(&[MoveInput::SkillInputEnd, space(false)]); // F
        let f = r.loco.position();
        r.run(20); // F + 20, before the walk goes on
        let coast = dist(f, r.loco.position());
        assert!((coast - 19.4).abs() < 1.0, "coast {coast}");
        assert!(dist(start, f) > 1.0, "F keeps the sprint step");
    }

    #[test]
    fn a_key_released_during_the_sprint_end_animation_does_not_walk_on() {
        let mut r = sprinting_with_a_key_held((180, 0));
        r.tick(&[MoveInput::SkillInputEnd, space(false)]); // F
        r.run(10);
        r.tick(&[dir(0, 0)]);
        r.run(30);
        assert_eq!(r.loco.state(), &MoveState::Idle);
        let here = r.loco.position();
        r.run(5);
        assert_eq!(r.loco.position(), here);
    }

    /// The cursor-direction sprint of the official capture (Space only): the keys are up when
    /// the unscaled `MoveAck` reaches the client, which coasts instead of walking on.
    fn sprint_following_the_cursor_then_released() -> Rig {
        let mut r = sprinting((100, -150));
        r.run(30); // the heading has turned to the vector (`SPRINT_TURN_RATE`)
        r.tick(&[MoveInput::SkillInputEnd, space(false)]); // F
        r.tick(&[dir(101, -149)]); // F + 1: a cursor vector still in flight
        r
    }

    #[test]
    fn a_sprint_ended_after_the_cursor_direction_coasts_to_a_stop() {
        let mut r = sprinting_with_cursor_release_at_f2();
        let start = r.loco.position();
        // Ended by the release batch: `SprintEnd` at sprint speed, the same ~20 cells in
        // 1/1.5 the time: 1.964 falling by 0.0998 per frame, 20 steps (private client,
        // 2026-11-02, rate 1.117); the first one is made by the tick that saw the zero vector.
        let mut steps = Vec::new();
        for _ in 0..40 {
            steps.push(step(&mut r, &[]));
        }
        assert!(
            (steps[0] - 1.864).abs() < 0.01 && (steps[1] - 1.764).abs() < 0.01,
            "{steps:?}"
        );
        assert!(steps.windows(2).take(18).all(|w| w[1] < w[0]), "{steps:?}");
        assert!(steps[18] < 0.1 && steps[19] == 0.0, "{steps:?}");
        let total = dist(start, r.loco.position());
        assert!((total + 1.964 - 20.3).abs() < 0.5, "{total}");
        // Along the cursor direction, and over: no message, idle.
        let d = (r.loco.position().0 - start.0, r.loco.position().1 - start.1);
        // The client's line goes to a whole goal cell (floor(vector * 0.092) away), so the coast is
        // up to two degrees off the cursor direction.
        assert!(
            (d.0 / total - 100.0 / 180.03).abs() < 0.03,
            "{}",
            d.0 / total
        );
        assert_eq!(r.loco.state(), &MoveState::Idle);
        assert!(r.run(5).is_empty());
    }

    /// F + 2: the zero vector the client sends 2 frames after the Space release.
    fn sprinting_with_cursor_release_at_f2() -> Rig {
        let mut r = sprint_following_the_cursor_then_released();
        let ev = r.tick(&[dir(0, 0)]);
        assert!(matches!(&ev[..], [MoveEvent::MoveAck { dir: (0, 0), .. }]));
        r
    }

    #[test]
    fn a_zero_vector_while_coasting_does_not_cut_it_short() {
        let mut r = sprinting_with_cursor_release_at_f2();
        r.run(8);
        let before = r.loco.position();
        r.tick(&[dir(0, 0)]);
        r.run(3);
        assert!(dist(before, r.loco.position()) > 3.0);
        assert!(matches!(r.mover_motion(), Motion::Coast { .. }));
    }

    #[test]
    fn a_new_direction_during_the_coast_walks_on_when_the_animation_ends() {
        let mut r = sprinting_with_cursor_release_at_f2(); // F + 2
        r.run(8);
        r.tick(&[dir(180, 0)]); // F + 11: the coast goes on
        assert!(matches!(r.mover_motion(), Motion::Coast { .. }));
        r.run(9); // F + 20
        assert!(matches!(r.mover_motion(), Motion::Coast { .. }));
        r.tick(&[]); // F + 21: Move (`SprintEnd` at sprint speed, 21 frames)
        let walk = r.cfg.walk_speed * 0.033;
        assert!((step(&mut r, &[]) - walk).abs() < 1e-3);
        assert_eq!(
            r.loco.state(),
            &MoveState::Walking(Walk::Direction((180, 0)))
        );
    }

    #[test]
    fn a_key_released_just_after_the_sprint_does_not_stop_the_coast() {
        // The client's Move action is over and `SprintEnd` plays: a key release has nothing
        // to stop (sprint-w game test cases 2 and 3: both coast to a stop).
        let mut r = sprinting((0, -180));
        r.run(3);
        r.tick(&[MoveInput::SkillInputEnd, space(false)]);
        r.tick(&[]);
        r.tick(&[dir(0, 0)]);
        let here = r.loco.position();
        r.run(10);
        assert!(dist(here, r.loco.position()) > 8.0);
    }

    // ---- problem 1: an exactly diagonal cursor (movement.md, "In-game check, 2026-10-21") ------

    fn cursor(x: i32, y: i32) -> MoveInput {
        MoveInput::CursorDirection { x, y }
    }

    /// A click-to-move sprint (Space only) whose cursor is exactly on the diagonal: the vector
    /// `(127, 127)` looks like a key's, but this client has no keys.
    fn click_mode_sprint(vector: Cell) -> Rig {
        let mut r = Rig::new((50, 50));
        r.tick(&[space(true), roll(10, 0)]);
        r.run(36);
        assert!(matches!(r.loco.state(), MoveState::Sprinting(_)));
        r.tick(&[cursor(vector.0, vector.1)]);
        r.run(30); // the heading has turned to the vector (`SPRINT_TURN_RATE`)
        r
    }

    #[test]
    fn a_click_mode_sprint_on_an_exact_diagonal_coasts_whenever_the_zero_comes() {
        // In game the client's zero vector came 4 to 5 frames after the sprint-off batch.
        for zero_at in [None, Some(1), Some(2), Some(5), Some(8)] {
            let mut r = click_mode_sprint((127, 127));
            r.tick(&[MoveInput::SkillInputEnd, space(false)]); // F
            let start = r.loco.position();
            let mut total = 0.0;
            for f in 1..=45 {
                let before = r.loco.position();
                let inputs = if zero_at == Some(f) {
                    vec![cursor(0, 0)]
                } else {
                    vec![]
                };
                r.tick(&inputs);
                total += dist(before, r.loco.position());
            }
            // The coast of the client: 19.95 cells less the step the batch's frame makes.
            assert!((total - 19.95).abs() < 0.8, "{zero_at:?}: {total}");
            let d = (r.loco.position().0 - start.0, r.loco.position().1 - start.1);
            // The coast runs on towards the plan's goal cell, so it can be a degree or two off the
            // exact diagonal.
            assert!((d.0 - d.1).abs() < 0.6 && d.0 > 10.0, "{zero_at:?}: {d:?}");
            assert_eq!(r.loco.state(), &MoveState::Idle);
        }
    }

    #[test]
    fn a_click_mode_sprint_ignores_cursor_vectors_in_flight() {
        let mut r = click_mode_sprint((127, 127));
        r.tick(&[MoveInput::SkillInputEnd, space(false)]); // F
        r.tick(&[cursor(127, 127)]); // F + 1
        r.tick(&[cursor(126, 128)]); // F + 2
        r.tick(&[cursor(0, 0)]); // F + 3
        r.run(2);
        // Still on the coast (not the walk a new direction would start): the steps fall.
        let a = step(&mut r, &[]);
        let b = step(&mut r, &[]);
        assert!(a > 1.1 && b < a, "{a} {b}");
        assert!(matches!(r.mover_motion(), Motion::Coast { .. }));
    }

    #[test]
    fn a_new_click_mode_vector_long_after_the_sprint_walks_on() {
        let mut r = click_mode_sprint((127, 127));
        r.tick(&[MoveInput::SkillInputEnd, space(false)]);
        r.run(10);
        r.tick(&[cursor(180, 0)]);
        let walk = r.cfg.walk_speed * 0.033;
        assert!((step(&mut r, &[]) - walk).abs() < 1e-3);
    }

    #[test]
    fn the_unscaled_ack_still_comes_first_when_the_zero_arrives_in_the_same_frame() {
        // The click-mode case and the free-angle one of WASD mode: in game the zero came in
        // F + 1 about half of the time and was answered with `(0, 0)` instead (22 cells off).
        for r in [click_mode_sprint((127, 127)), sprinting((100, -150))] {
            let mut r = r;
            r.tick(&[MoveInput::SkillInputEnd, space(false)]); // F
            let zero = if matches!(r.mover_motion(), Motion::Coast { .. }) {
                cursor(0, 0)
            } else {
                dir(0, 0)
            };
            let ev = r.tick(&[zero]); // F + 1
            assert!(
                matches!(&ev[..], [MoveEvent::MoveAck { dir, .. }] if *dir != (0, 0)),
                "{ev:?}"
            );
            let ev = r.tick(&[]); // F + 2: the zero's own ack
            assert!(
                matches!(&ev[..], [MoveEvent::MoveAck { dir: (0, 0), .. }]),
                "{ev:?}"
            );
            let start = r.loco.position();
            r.run(40);
            assert!(dist(start, r.loco.position()) > 15.0);
        }
    }

    /// WASD mode (`Direction`, trailer byte 0), Space held with the cursor at `cursor` (the
    /// roll key's target offset) and the `0x63` vector `vector` streaming; Space up (`F`), the
    /// zero vector in `F + 1`. Returns how far the entity went after the zero.
    fn wasd_sprint_released_with_zero_in_f1(cursor: Cell, vector: Cell) -> f32 {
        let mut r = Rig::new((50, 50));
        r.tick(&[space(true), roll(cursor.0, cursor.1)]);
        r.run(36);
        r.tick(&[dir(vector.0, vector.1)]);
        r.run(3);
        r.tick(&[MoveInput::SkillInputEnd, space(false)]); // F
        r.tick(&[dir(0, 0)]); // F + 1
        let start = r.loco.position();
        r.run(40);
        dist(start, r.loco.position())
    }

    #[test]
    fn an_exact_diagonal_vector_along_the_cursor_is_a_cursor_sprint_in_wasd_mode_too() {
        // The vector is W's `(127, 127)`, with the same trailer bytes; the cursor tells them
        // apart: here it points the same way (in game 2026-10-21: 20 cells of drift when the
        // server stopped at the zero and the client coasted).
        assert!(wasd_sprint_released_with_zero_in_f1((30, 30), (127, 127)) > 15.0);
        // Within `cursor_match_degrees` (10): a cursor 5 degrees off the diagonal.
        assert!(wasd_sprint_released_with_zero_in_f1((30, 36), (127, 127)) > 15.0);
        // An axis vector along the cursor as well.
        assert!(wasd_sprint_released_with_zero_in_f1((40, 2), (180, 0)) > 15.0);
    }

    #[test]
    fn a_key_vector_released_at_the_sprint_end_coasts_as_well() {
        // W pressed while sprinting towards another cursor: the client coasts all the same
        // (`SprintEnd`), the release has no walk to stop.
        assert!(wasd_sprint_released_with_zero_in_f1((-30, 30), (127, 127)) > 15.0);
        assert!(wasd_sprint_released_with_zero_in_f1((20, 50), (127, 127)) > 15.0);
        assert!(wasd_sprint_released_with_zero_in_f1((0, 0), (180, 0)) > 15.0);
    }

    #[test]
    fn a_free_angle_zero_four_to_five_frames_after_the_sprint_off_still_coasts() {
        let mut r = sprinting((100, -150));
        r.run(3);
        r.tick(&[MoveInput::SkillInputEnd, space(false)]); // F
        r.tick(&[dir(101, -149)]);
        r.run(2);
        r.tick(&[dir(0, 0)]); // F + 4
        let start = r.loco.position();
        r.run(40);
        assert!(dist(start, r.loco.position()) > 12.0);
    }

    #[test]
    fn a_sprint_ended_by_the_key_with_no_direction_stays_put() {
        let mut r = Rig::new((50, 50));
        r.tick(&[space(true), roll(0, 0)]);
        r.run(40);
        r.tick(&[MoveInput::SkillInputEnd, space(false)]);
        let here = r.loco.position();
        r.run(40);
        assert_eq!(r.loco.position(), here);
    }

    // ---- gap #12: the roll heading ---------------------------------------------------------

    #[test]
    fn a_roll_with_the_cursor_on_the_player_goes_along_the_facing() {
        // The client takes the entity's facing when the target is its own cell
        // (`0x141b8bda0`); the roll used to be dropped.
        let mut r = Rig::new((50, 50));
        r.tick(&[dir(0, -180)]);
        r.run(5);
        r.tick(&[dir(0, 0)]);
        r.tick(&[space(true), roll(0, 0)]);
        r.run(10);
        let (x, y) = r.loco.position();
        assert!((x - 50.5).abs() < 0.01 && y < 50.5 - 20.0, "{x},{y}");
    }

    #[test]
    fn a_roll_in_click_mode_follows_the_cursor_not_the_click_walk() {
        // A click walk towards the south-west, then a roll with the cursor to the north: the
        // roll runs along the cursor offset (the Move target is not its heading).
        let mut r = Rig::new((50, 50));
        r.tick(&[MoveInput::UseSkill(UseSkill {
            target_flags: 0,
            ..skill(MOVE, -40, 40)
        })]);
        r.run(4);
        let (x0, y0) = r.loco.position();
        r.tick(&[space(true), roll(0, -30)]);
        r.run(10);
        let (x, y) = r.loco.position();
        assert!(
            (x - x0).abs() < 2.0 && y0 - y > 20.0,
            "{x0},{y0} -> {x},{y}"
        );
    }
}
