//! A port of the client's WASD movement: line, Positioned tick, plans, sprint turn and roll
//! (docs/re/wasd-movement.md, every rule and address there). In grid cells, cell centres on whole
//! numbers. One frame: [`ClientMotion::positioned_tick`], then the frame's messages, then
//! [`ClientMotion::actor_tick`].

use crate::clientpath::{self, Params};
use crate::movement::Walkable;
use std::f32::consts::{PI, TAU};
use std::sync::Arc;

pub type Cell = (i32, i32);

/// `0x142193270`: input units to cells.
const PER_UNIT: f32 = 0.092;
/// `0x142193230`: cells to world units.
const UNITS_PER_CELL: f32 = 10.869565;
/// `FUN_141e42650`: seconds between plans of a player's Move.
const PLAN_PERIOD: f32 = 0.05;
/// `0x141dcc6a0`: goals at least this far (squared) are pulled in to 126 cells.
const GOAL_LIMIT_SQ: i64 = 0x4840;
const GOAL_CLAMP: f32 = 126.0;
/// `0x141dcc460`: the line of a turning sprint ends this many cells along the turned facing.
const SPRINT_TURN_AHEAD: f32 = 180.0;
/// `FUN_141b8c7f0`: a roll stops steering when its target is this close (`FUN_140132800`,
/// measured: between 1.41 and 2 by the `LINE` log).
const ROLL_STOP_DISTANCE: f32 = 1.6;
/// `FUN_141b8d1f0`: a roll's search looks at least this far ahead.
const ROLL_MIN_LOOKAHEAD: f32 = 10.0;
/// `SprintEnd` is `speed = 600` in `character.act` (`0x141e56ae0`).
const SPRINT_END_ANIM_SPEED: f32 = 600.0;
/// Pathfinding `+0x580` (`0x141dcc460`), measured 0.99 on the `LINE` log (2 boundary cells; 1.0
/// misses them); writer `0x141dcce80` unread.
const SPRINT_TURN_FACTOR: f32 = 0.99;
/// The stat `dodge_roll_sprint_turn_rate` (key `0x5794`, 5 in the game data), degrees per plan.
const SPRINT_TURN_RATE: f32 = 5.0;
/// `FUN_141b8c7f0`: roll action `+0x44`, the optional length of a sprinting roll's lines, in
/// cells. Measured (`LINE`: e = s + 300 along the heading); its writer is unread.
pub const ROLL_SPRINT_LINE: f32 = 300.0;
/// `offset_scale` of the class's `.ao` when the game data is not there.
pub const STAND_IN_OFFSET_SCALE: f32 = 0.9;
/// Where the character's dodge roll and sprint end curves are (`Character.act`). Characters are
/// unarmed for now; the weapon variants follow the `dodge_*_01` pattern.
const ROLL_AMD: &str = "art/models/charactersfour/animations/basedodgeroll/onerig.amd";
const SPRINT_AMD: &str = "art/models/charactersfour/animations/basesprint/onerig.amd";

/// A movement curve of an animation (`.amd`: duration and one position per frame).
#[derive(Debug, Clone, PartialEq)]
pub struct Curve {
    /// Seconds.
    pub length: f32,
    pub positions: Arc<[f32]>,
}

impl Curve {
    pub fn new(length: f32, positions: Vec<f32>) -> Self {
        Self {
            length,
            positions: positions.into(),
        }
    }

    /// The curve `name` of an `.amd` file's text (per animation: `"name"`, `once` or `loop`,
    /// duration in ms, a line, then `count v0 v1 ...`).
    pub fn from_amd(text: &str, name: &str) -> Option<Self> {
        let quoted = format!("\"{name}\"");
        let mut lines = text.lines().skip_while(|l| l.trim() != quoted);
        let ms: f32 = lines.nth(2)?.trim().parse().ok()?;
        let positions = lines
            .nth(1)?
            .split_whitespace()
            .skip(1)
            .map(|v| v.parse().ok())
            .collect::<Option<Vec<f32>>>()?;
        (positions.len() >= 2).then(|| Self::new(ms / 1000.0, positions))
    }

    /// `FUN_141ecccc0`.
    fn value(&self, t: f32) -> f32 {
        let n = self.positions.len();
        if n < 2 || t < 0.0 {
            return 0.0;
        }
        let loops = (t / self.length).floor();
        let frame = (t % self.length) / self.length * (n - 1) as f32;
        let i = (frame.floor() as usize).min(n - 1);
        let next = (i + 1).min(n - 1);
        let first = self.positions[0];
        let last = self.positions[n - 1];
        (self.positions[next] - self.positions[i]) * (frame - i as f32)
            + self.positions[i]
            + (last - first) * loops
    }
}

/// The curves of the character's dodge roll and sprint end (`basedodgeroll`, `basesprint`).
#[derive(Debug, Clone, PartialEq)]
pub struct MotionCurves {
    pub roll: Curve,
    pub roll_back: Curve,
    pub roll_sprint: Curve,
    pub roll_back_sprint: Curve,
    pub sprint_end: Curve,
}

impl MotionCurves {
    /// From the game's two `.amd` files (UTF-16 text).
    pub fn from_amd(roll_text: &str, sprint_text: &str) -> Option<Self> {
        Some(Self {
            roll: Curve::from_amd(roll_text, "dodge_unarmed_01")?,
            roll_back: Curve::from_amd(roll_text, "dodge_unarmed_bwd_01")?,
            roll_sprint: Curve::from_amd(roll_text, "dodge_unarmed_01_sprinting")?,
            roll_back_sprint: Curve::from_amd(roll_text, "dodge_unarmed_bwd_01_sprinting")?,
            sprint_end: Curve::from_amd(sprint_text, "sprint_end_unarmed_01")?,
        })
    }

    /// Reads the curves from the game data.
    pub fn load(data: &poe_data::GameData) -> poe_data::Result<Option<Self>> {
        let text = |path: &str| {
            data.files().read(path).map(|bytes| {
                let words: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16_lossy(&words).replace('\r', "")
            })
        };
        Ok(Self::from_amd(&text(ROLL_AMD)?, &text(SPRINT_AMD)?))
    }

    /// NOT game data: curves for running without game files (the unit and replay tests, a server
    /// with no client data), rebuilt from the per-frame steps measured on the client's `LINE` log
    /// (cells per 33 ms frame of a 46-cell roll; `sprint_end` from its total and a linear decay).
    /// Close to [`MotionCurves::load`], not equal: a real server loads the real ones.
    pub fn stand_in() -> Self {
        const ROLL: [f32; 31] = [
            3.7324, 3.5283, 3.3310, 3.1405, 2.9568, 2.7627, 2.5501, 2.3350, 2.1171, 1.8971, 1.6743,
            1.4492, 1.2353, 1.1218, 1.0497, 0.9889, 0.9394, 0.9010, 0.8740, 0.8581, 0.8536, 0.8601,
            0.8741, 0.8537, 0.7971, 0.7168, 0.60, 0.45, 0.30, 0.18, 0.0719,
        ];
        const SPRINT_TAIL: [f32; 15] = [
            0.9175, 0.9501, 1.0087, 1.0703, 1.1262, 1.1701, 1.2210, 1.3018, 1.3852, 1.4565, 1.4771,
            1.5311, 1.5200, 2.00, 2.37,
        ];
        const SPRINT_END_UNITS: f32 = 242.425;
        const SPRINT_END_SECONDS: f32 = 0.75;
        const UNITS_PER_ROLL_CELL: f32 = 500.0 / 46.0;
        let curve = |steps: &[f32], sign: f32| {
            let mut at = 0.0;
            let mut positions = vec![0.0];
            for step in steps {
                at += step * UNITS_PER_ROLL_CELL * sign;
                positions.push(at);
            }
            Curve::new(1.0, positions)
        };
        let sprinting: Vec<f32> = ROLL[..16].iter().chain(&SPRINT_TAIL).copied().collect();
        let end = (0..46)
            .map(|i| {
                let left = 1.0 - i as f32 / 45.0;
                SPRINT_END_UNITS * (1.0 - left * left)
            })
            .collect();
        Self {
            roll: curve(&ROLL, 1.0),
            roll_back: curve(&ROLL, -1.0),
            roll_sprint: curve(&sprinting, 1.0),
            roll_back_sprint: curve(&sprinting, -1.0),
            sprint_end: Curve::new(SPRINT_END_SECONDS, end),
        }
    }
}

/// `offset_scale` from the `.ao` of `metadata_id` (`Melee { offset_scale = 0.9 }`), the class's
/// own file or the ones it extends.
pub fn load_offset_scale(data: &poe_data::GameData, metadata_id: &str) -> Option<f32> {
    let mut path = metadata_id.to_owned();
    for _ in 0..16 {
        let bytes = data.files().read(&format!("{path}.ao").to_lowercase()).ok()?;
        let words: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let text = String::from_utf16_lossy(&words);
        let value = |key: &str| {
            text.lines().find_map(|l| {
                let (k, v) = l.split_once(|c: char| c == '=' || c.is_whitespace())?;
                (k.trim() == key).then(|| v.trim().trim_matches(['=', ' ', '"']).to_owned())
            })
        };
        if let Some(v) = value("offset_scale") {
            return v.parse().ok();
        }
        path = value("extends")?;
        if path == "nothing" {
            return None;
        }
    }
    None
}

/// A curve in use (Positioned `+0x2a8`, `L + 0xa0`).
#[derive(Debug, Clone, PartialEq)]
struct CurveRun {
    curve: Curve,
    /// `[8]` time.
    t: f32,
    /// `[7]` time from the last restart to the end.
    span: f32,
    /// `[1]` value at the last restart.
    from: f32,
    /// `[0]` value left from the last restart.
    range: f32,
    /// `[3]`, `[4]` values at the start and the end (fixed at load).
    first: f32,
    last: f32,
}

impl CurveRun {
    /// `FUN_141ecc540`.
    fn load(curve: Curve) -> Self {
        let first = curve.value(0.0);
        let last = curve.value(curve.length);
        let mut run = Self {
            span: 0.0,
            from: 0.0,
            range: 0.0,
            t: 0.0,
            first,
            last,
            curve,
        };
        run.restart(0.0);
        run
    }

    /// `FUN_141ecc710`.
    fn restart(&mut self, t: f32) {
        self.t = t;
        self.span = self.curve.length - t;
        self.from = self.curve.value(t);
        self.range = self.curve.value(self.curve.length) - self.from;
    }

    /// `FUN_141ecc780`: the share of the remaining range passed.
    fn advance(&mut self, dt: f32, rate: f32) -> f32 {
        let old = self.curve.value(self.t);
        self.t += self.span * rate * dt;
        if self.range == 0.0 {
            return 0.0;
        }
        (self.curve.value(self.t) - old) / self.range
    }

    /// Share of the curve passed, `0..=1` (`FUN_141b8cf70`).
    fn done(&self) -> f32 {
        ((self.curve.value(self.t) - self.first) / (self.last - self.first)).clamp(0.0, 1.0)
    }
}

/// The line a Positioned walks (`L = Positioned + 0x208`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Line {
    s: Cell,
    p: (f32, f32),
    q: Cell,
    e: Cell,
    flag: bool,
    swap: bool,
    sign: i32,
    slope: f32,
    end_major: i32,
    major: i32,
    minor: i32,
    minor_acc: f32,
    distance_mode: bool,
    len: f32,
    t: f32,
    inv_t: f32,
    /// `+0x78`, written by the roll step.
    rate: f32,
    elapsed: f32,
    distance: f32,
    thr: f32,
    acc: f32,
    pending: f32,
    step_time: f32,
    /// `+0x98`.
    pending_scale: f32,
    /// `+0x61`: the move ended (`0x64 (0, 0)`): the next line does not continue this one.
    ended: bool,
}

impl Line {
    fn cur(&self) -> Cell {
        if self.swap {
            (self.minor, self.major)
        } else {
            (self.major, self.minor)
        }
    }

    fn set_cur(&mut self, c: Cell) {
        (self.major, self.minor) = if self.swap { (c.1, c.0) } else { (c.0, c.1) };
    }

    /// `FUN_141ec83d0`.
    fn point(&self) -> (f32, f32) {
        let k = if self.distance_mode {
            if self.len > 0.0 { self.distance / self.len } else { 1.0 }
        } else if self.t > 0.0 {
            self.elapsed / self.t
        } else {
            1.0
        };
        let k = k.clamp(0.0, 1.0);
        (
            self.p.0 + (self.e.0 as f32 - self.p.0) * k,
            self.p.1 + (self.e.1 as f32 - self.p.1) * k,
        )
    }

    fn running(&self) -> bool {
        self.len != 0.0
    }

    /// `FUN_141ec86a0`.
    fn setup(&mut self, s: Cell, e: Cell, speed: f32, distance_mode: bool) {
        let sf = (s.0 as f32, s.1 as f32);
        let ef = (e.0 as f32, e.1 as f32);
        let mut running = !self.ended && self.running() && self.cur() == s;
        let mut p = sf;
        if running {
            let old = self.point();
            if old != ef && (sf.0 - old.0).powi(2) + (sf.1 - old.1).powi(2) < 4.0 {
                p = old;
            } else {
                running = false;
            }
        }
        let dir = (ef.0 - p.0, ef.1 - p.1);
        let (q, flag) = if running {
            let t = (dir.0 * (sf.0 - p.0) + dir.1 * (sf.1 - p.1)) / (dir.0 * dir.0 + dir.1 * dir.1);
            let q = (
                (p.0 + dir.0 * t + 0.5).floor() as i32,
                (p.1 + dir.1 * t + 0.5).floor() as i32,
            );
            (q, q != s)
        } else {
            (s, false)
        };
        let (dx, dy) = ((e.0 - q.0).abs(), (e.1 - q.1).abs());
        let swap = dx < dy || (dx == dy && running && dir.0.abs() < dir.1.abs());
        let pick = |v: (f32, f32)| if swap { (v.1, v.0) } else { v };
        let picki = |v: Cell| if swap { (v.1, v.0) } else { v };
        let (dir_major, dir_minor) = pick(dir);
        let (p_major, p_minor) = pick(p);
        let _ = p_major;
        let (e_major, _) = picki(e);
        let (q_major, _) = picki(q);
        let len = (dir.0 * dir.0 + dir.1 * dir.1).sqrt();
        let t = len / speed;
        let d = dir_major.abs();
        let thr = if distance_mode { len / d } else { t / d };
        let slope = dir_minor / d;
        let k = (e_major - q_major).abs() as f32 - d;
        let pending = self.pending;
        *self = Self {
            s,
            p,
            q,
            e,
            flag,
            swap,
            sign: (e_major - q_major).signum(),
            slope,
            end_major: e_major,
            major: 0,
            minor: 0,
            minor_acc: p_minor - slope * k + if slope > 0.0 { 0.49 } else { 0.5 },
            distance_mode,
            len,
            t,
            inv_t: 1.0 / t,
            rate: 0.0,
            elapsed: 0.0,
            distance: 0.0,
            thr,
            acc: thr * k,
            pending,
            step_time: t / d,
            pending_scale: 1.0,
            ended: false,
        };
        self.set_cur(s);
    }

    /// `FUN_141ec82e0`.
    fn advance(&mut self, dt: f32, curve: Option<&mut CurveRun>) {
        let mut x = dt + self.pending;
        self.pending = x;
        self.elapsed += x;
        if self.distance_mode {
            let rate = if self.rate > 0.0 { self.rate } else { self.inv_t };
            let share = curve.map_or(0.0, |c| c.advance(x, rate));
            self.pending_scale = if share > 0.0 { rate * self.pending / share } else { 1.0 };
            x = share * self.len;
            if self.rate > 0.0 {
                x *= self.inv_t / self.rate;
            }
        }
        self.pending = 0.0;
        self.acc += x;
        self.distance += x;
    }

    /// Whether the tick loop steps again.
    fn wants_step(&self) -> bool {
        self.thr != 0.0 && (self.thr <= self.acc || (self.flag && self.acc > 0.0))
    }

    /// `FUN_141ec8cb0`: one step; returns the cell and whether the line is done.
    fn step(&mut self) -> (Cell, bool) {
        if self.flag {
            let mut c = self.cur();
            c.0 += (self.q.0 - c.0).signum();
            c.1 += (self.q.1 - c.1).signum();
            self.set_cur(c);
            if c == self.q {
                self.flag = false;
                if self.major == self.end_major {
                    self.acc -= if self.distance_mode { self.len } else { self.t };
                }
            }
        } else {
            self.major += self.sign;
            self.acc -= self.thr;
            self.minor_acc += self.slope;
            self.minor = self.minor_acc.floor() as i32;
        }
        let cell = self.cur();
        let done = !self.flag && self.major == self.end_major;
        if done {
            self.finish();
        }
        (cell, done)
    }

    /// `FUN_141ec8f00`.
    fn finish(&mut self) {
        self.pending = if self.distance_mode && self.thr != 0.0 {
            self.pending_scale * self.step_time * self.acc / self.thr
        } else {
            self.acc
        }
        .max(0.0);
        self.thr = 0.0;
        self.len = 0.0;
    }

    /// `FUN_141de5f50` on the line.
    fn kill(&mut self) {
        self.finish();
        self.acc = 0.0;
        self.pending = 0.0;
    }
}

/// The fields of the DLL's `LINE` log. `p`, `q`, `e`, `cur` are `(major, minor)`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LoggedLine {
    pub s: Cell,
    pub p: (f32, f32),
    pub q: Cell,
    pub e: Cell,
    pub cur: Cell,
    pub flag: bool,
    pub swap: bool,
    pub sign: i32,
    pub slope: f32,
    pub minacc: f32,
    pub len: f32,
    pub t: f32,
    pub el: f32,
    pub thr: f32,
    pub acc: f32,
    pub pend: f32,
}

/// A running roll (`FUN_141b8c720`).
#[derive(Debug, Clone, PartialEq)]
struct Roll {
    /// `+0x40`, facing convention.
    heading: f32,
    /// `+0x4c`.
    total: i32,
    /// Animation time, seconds.
    elapsed: f32,
    /// `+0x3d` steering stays on until the goal is close.
    steering: bool,
    /// `+0x44`, `+0x48`: the optional length of the line, cells.
    extend: Option<f32>,
    /// The heading follows the keys, not the cursor.
    keys: bool,
    /// `+0x3f`: at least 90 degrees from the facing: `_bwd_` animation, orientation mode 2.
    backward: bool,
    /// Clearance level of the action's search (`+0x20`, `FUN_141b8d1f0`), fixed at its start: the
    /// sprint arriving during the roll does not change it.
    level: i32,
}

/// The Move slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walk {
    None,
    /// WASD: the goal comes from the axes at each plan.
    Axes,
    /// Click-to-move.
    Target(Cell),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientMotion {
    /// Positioned `+0x444`.
    grid: Cell,
    line: Line,
    /// Positioned `+0x44c`.
    facing: f32,
    /// Actor `+0x400`.
    axes: (i32, i32),
    /// Actor `+0x3dc`.
    plan_timer: f32,
    walk: Walk,
    /// A `0x64 (0, 0)` ended the walk: the line runs through the next frame's Positioned tick,
    /// then it stops. Actor ticks left.
    ending: u8,
    /// The walk's path in walking order, and the next entry.
    path: Vec<Cell>,
    next: usize,
    /// Pathfinding `+0x580`.
    turn_factor: f32,
    roll: Option<Roll>,
    curve: Option<CurveRun>,
    /// The animation's coast line runs ([`ClientMotion::start_coast`]).
    coasting: bool,
    pub curves: Option<Arc<MotionCurves>>,
    /// The class's `offset_scale` (`.ao`), scales root motion and the animation's rate.
    pub offset_scale: f32,
    /// Seconds of `SprintEnd` left. `Actor_StartWasdWalk` starts nothing while it plays.
    end_anim: f32,
    /// The rate of the `SprintEnd` that started in the last Actor tick: its move event comes in
    /// the next frame's Animated tick (`0x141da76c0`).
    coast_due: Option<f32>,
    /// The sprint part is on (`0x141b77110` ran, `0x141b773a0` not yet).
    sprint_on: bool,
    /// The sprint's plan came back empty: this frame's end check ends it with the Bonk.
    bonk_due: bool,
    /// The Bonk ran; the owner has not seen it yet ([`ClientMotion::take_bonked`]).
    bonked: bool,
    /// A slot-0 action (an attack) started in this frame: `0x141b773a0` plays no `SprintEnd`.
    slot0_started: bool,
    /// The buff `must_sprint`: key state does not end the sprint while it is on.
    pub must_sprint: bool,
    /// The speed when the frame started. A `SprintEnd` in a frame with a `0x64` plan runs at this
    /// rate, one without at the speed after the frame's stats (`LINE`: coast T 0.671 and 1.007,
    /// the reason is unread).
    frame_speed: f32,
    /// A `0x64` planned in this frame.
    planned: bool,
    /// [`Self::frame_speed`] applies. The official captures' client played every `SprintEnd` at
    /// the speed after the stats (coasts 30 frames), the private client's plan frames do not (the
    /// reason is unread), see `SprintConfig::release_ends_at_sprint_speed`.
    pub plan_frame_keeps_speed: bool,
    /// The server's echo of the sprint key's release came.
    key_released: bool,
    /// Cells per second: base speed, attack factor and the sprint's stats.
    pub speed: f32,
    pub factor: f32,
    pub scale: f32,
    /// The stat `is_sprinting`.
    pub is_sprinting: bool,
    /// Stat `dodge_roll_sprint_turn_rate`, degrees per plan.
    pub turn_rate: f32,
    /// The frame's Positioned tick made a step.
    stepped: bool,
    /// Clearance level of the walk's searches.
    pub level: i32,
    /// Plans made so far, for tests and logs.
    pub plans: u64,
}

fn round(x: f32) -> i32 {
    (x + 0.5).floor() as i32
}

fn floor_goal(v: i32) -> i32 {
    (v as f32 * PER_UNIT).floor() as i32
}

fn world(c: f32) -> f32 {
    (c + 0.5) * UNITS_PER_CELL
}

fn facing_of(dx: f32, dy: f32) -> f32 {
    (dy.atan2(dx) + PI / 2.0).rem_euclid(TAU)
}

impl ClientMotion {
    pub fn at_cell(cell: Cell, speed: f32) -> Self {
        Self {
            grid: cell,
            line: Line::default(),
            facing: 0.0,
            axes: (0, 0),
            plan_timer: 0.0,
            walk: Walk::None,
            ending: 0,
            path: Vec::new(),
            next: 0,
            turn_factor: SPRINT_TURN_FACTOR,
            roll: None,
            curve: None,
            coasting: false,
            curves: None,
            offset_scale: 1.0,
            end_anim: 0.0,
            coast_due: None,
            sprint_on: false,
            bonk_due: false,
            bonked: false,
            slot0_started: false,
            must_sprint: false,
            frame_speed: speed,
            planned: false,
            plan_frame_keeps_speed: true,
            key_released: false,
            speed,
            factor: 1.0,
            scale: 1.0,
            is_sprinting: false,
            turn_rate: SPRINT_TURN_RATE,
            stepped: false,
            level: 0,
            plans: 0,
        }
    }

    pub fn grid(&self) -> Cell {
        self.grid
    }

    /// The position on the current line, in cell-centred coordinates.
    pub fn point(&self) -> (f32, f32) {
        if self.line.running() {
            self.line.point()
        } else {
            (self.grid.0 as f32, self.grid.1 as f32)
        }
    }

    /// Line state for logs: time, elapsed, acc, length.
    pub fn line_state(&self) -> (f32, f32, f32, f32) {
        (self.line.t, self.line.elapsed, self.line.acc, self.line.len)
    }

    /// End cell of the running line.
    pub fn line_end(&self) -> Option<Cell> {
        self.line.running().then_some(self.line.e)
    }

    pub fn axes(&self) -> (i32, i32) {
        self.axes
    }

    pub fn is_walking(&self) -> bool {
        self.walk != Walk::None
    }

    pub fn is_rolling(&self) -> bool {
        self.roll.is_some()
    }

    /// A line runs.
    pub fn is_moving(&self) -> bool {
        self.line.running()
    }

    /// `SprintEnd`'s root move runs or is about to start.
    pub fn is_coasting(&self) -> bool {
        self.coasting || self.coast_due.is_some()
    }

    /// The position in cells: cell corners on whole numbers, as `(cell + 0.5)` for a centre.
    pub fn position(&self) -> (f32, f32) {
        let (x, y) = self.point();
        (x + 0.5, y + 0.5)
    }

    /// The waypoints left of a click's walk.
    pub fn path(&self) -> &[Cell] {
        self.path.get(self.next..).unwrap_or(&[])
    }

    /// Whether a sprint ended on a wall since the last call.
    pub fn take_bonked(&mut self) -> bool {
        std::mem::take(&mut self.bonked)
    }

    /// A slot-0 action starts (`0x141e4ea40`): it takes the place of a running roll.
    pub fn action_started(&mut self) {
        self.slot0_started = true;
        self.roll = None;
    }

    /// A new speed factor: a running walk's line is issued again, same end (`0x141de5190`, from
    /// the Actor tick's ramp, spec step 12).
    pub fn set_factor(&mut self, factor: f32) {
        if factor == self.factor {
            return;
        }
        self.factor = factor;
        if self.line.running() && self.roll.is_none() && !self.coasting {
            let e = self.line.e;
            let cur = self.line.cur();
            self.line.setup(cur, e, self.move_speed(), false);
        }
    }

    /// Places the entity (a teleport): every motion stops.
    pub fn place(&mut self, cell: Cell) {
        let curves = self.curves.take();
        *self = Self {
            curves,
            offset_scale: self.offset_scale,
            speed: self.speed,
            scale: self.scale,
            factor: self.factor,
            level: self.level,
            is_sprinting: self.is_sprinting,
            must_sprint: self.must_sprint,
            ..Self::at_cell(cell, self.speed)
        };
    }

    fn move_speed(&self) -> f32 {
        self.speed * self.factor * self.scale
    }

    /// Whether the entity may stand on `cell` (`FUN_141ddcfc0`, walkability only: level 0).
    fn free(terrain: &dyn Walkable, cell: Cell) -> bool {
        if let Some((w, h)) = terrain.bounds()
            && (cell.0 < 0 || cell.1 < 0 || cell.0 >= w || cell.1 >= h)
        {
            return false;
        }
        terrain.nibble(cell.0, cell.1) >= 1
    }

    // --- Positioned ---

    /// `FUN_141b87580`: a line runs and its current cell is the grid.
    fn on_line(&self) -> bool {
        self.line.running() && self.line.cur() == self.grid
    }

    /// The frame's components before the Actor: Positioned, then Animated.
    pub fn positioned_tick(&mut self, dt: f32, terrain: &dyn Walkable) {
        self.frame_speed = self.move_speed();
        self.planned = false;
        self.step_line(dt, terrain);
        self.animated_tick(dt, terrain);
    }

    /// The part of the Animated tick that moves the entity: `SprintEnd`'s move event, its end.
    fn animated_tick(&mut self, dt: f32, terrain: &dyn Walkable) {
        if self.end_anim <= 0.0 {
            return;
        }
        if let Some(rate) = self.coast_due.take() {
            self.start_coast(rate, terrain);
        }
        self.end_anim = (self.end_anim - dt).max(0.0);
        if self.end_anim <= 0.0 && self.coasting {
            // `0x141da7dc0`: the animation changed, its move stops.
            self.line.kill();
            self.coasting = false;
            self.curve = None;
        }
    }

    /// `FUN_141ddc9d0`: advances the line and makes its steps.
    fn step_line(&mut self, dt: f32, terrain: &dyn Walkable) {
        self.stepped = false;
        if self.line.thr == 0.0 {
            return;
        }
        self.line.advance(dt, self.curve.as_mut());
        while self.line.wants_step() {
            let (cell, done) = self.line.step();
            self.stepped = true;
            if !Self::free(terrain, cell) {
                // Blocked: the cell is not applied; `FUN_141de5f50`, and no replan: the path ends
                // (`LINE`-less wall run: 21 still frames at both walls, a sprint's Bonk). A walk's
                // next Actor tick starts again from the keys.
                self.line.kill();
                if self.roll.is_none() {
                    self.path_failed();
                }
                return;
            }
            self.grid = cell;
            if done {
                self.line_done(terrain);
                if self.line.thr == 0.0 {
                    self.line.acc = 0.0;
                    self.line.pending = 0.0;
                    return;
                }
                if self.line.pending > 0.0 {
                    self.line.advance(0.0, self.curve.as_mut());
                }
            }
        }
    }

    /// `0x141dccdb0`, the Pathfinding's handler of a finished line.
    fn line_done(&mut self, terrain: &dyn Walkable) {
        if self.coasting {
            self.coasting = false;
            self.curve = None;
            return;
        }
        if self.roll.is_some() || self.walk == Walk::None {
            return;
        }
        if let Walk::Target(goal) = self.walk
            && self.grid == goal
        {
            self.end_path();
            return;
        }
        if self.next < self.path.len() {
            let i = self.next;
            self.next += 1;
            self.step_along(i, terrain);
        } else {
            self.plan(terrain);
        }
    }

    /// `0x141dccac0(pf, 0)`.
    fn end_path(&mut self) {
        self.walk = Walk::None;
        self.path.clear();
        self.next = 0;
    }

    /// `FUN_141de5f50`.
    pub fn stop_line(&mut self) {
        self.line.kill();
    }

    /// `0x141ddb6f0`: the client's snap on a `0x64` whose cell is not its grid.
    pub fn snap(&mut self, cell: Cell) {
        self.grid = cell;
        self.line.kill();
        self.line.set_cur(cell);
    }

    // --- Pathfinding ---

    /// `0x141dcc6a0`: a search from the grid to the walk's goal, then the first waypoint.
    fn plan(&mut self, terrain: &dyn Walkable) {
        let goal = match self.walk {
            Walk::Axes => (
                self.grid.0 + floor_goal(self.axes.0),
                self.grid.1 + floor_goal(self.axes.1),
            ),
            Walk::Target(goal) => goal,
            Walk::None => return,
        };
        self.plans += 1;
        let goal = self.clamp_goal(goal);
        let moving = self.on_line().then(|| self.line.point());
        let params = Params {
            moving,
            ..Params::default()
        };
        let plan = clientpath::plan(terrain, self.grid, goal, self.level, &params);
        self.path = plan.waypoints().collect();
        if self.path.is_empty() {
            self.path_failed();
            return;
        }
        self.next = 1;
        self.step_along(0, terrain);
    }

    fn clamp_goal(&self, goal: Cell) -> Cell {
        let (dx, dy) = ((goal.0 - self.grid.0) as i64, (goal.1 - self.grid.1) as i64);
        let d2 = dx * dx + dy * dy;
        if d2 < GOAL_LIMIT_SQ {
            return goal;
        }
        let k = GOAL_CLAMP / (d2 as f32).sqrt();
        (
            self.grid.0 + round(dx as f32 * k),
            self.grid.1 + round(dy as f32 * k),
        )
    }

    /// `Pathfinding_StepAlongPath`.
    fn step_along(&mut self, i: usize, terrain: &dyn Walkable) {
        let w = self.sprint_turn(self.path[i]);
        self.move_to(w, self.move_speed(), false, terrain);
    }

    /// `0x141dcc460`.
    fn sprint_turn(&self, w: Cell) -> Cell {
        if !self.is_sprinting {
            return w;
        }
        let (dx, dy) = (w.0 - self.grid.0, w.1 - self.grid.1);
        let (x, y) = (
            (dx as f32 + 0.5) * UNITS_PER_CELL,
            (dy as f32 + 0.5) * UNITS_PER_CELL,
        );
        let n = (x * x + y * y).sqrt();
        let d = (x / n, y / n);
        let f = (self.facing.sin(), -self.facing.cos());
        let c = d.0 * f.0 + d.1 * f.1;
        if c > 0.99 {
            return w;
        }
        let mut a = self.turn_rate * self.turn_factor * PI / 180.0;
        if c > -0.99 && c.acos() < a {
            return w;
        }
        if d.1 * f.0 - d.0 * f.1 < 0.0 {
            a = -a;
        }
        let r = (
            f.0 * a.cos() - f.1 * a.sin(),
            f.0 * a.sin() + f.1 * a.cos(),
        );
        (
            ((world(self.grid.0 as f32) + r.0 * SPRINT_TURN_AHEAD * UNITS_PER_CELL) * PER_UNIT).floor()
                as i32,
            ((world(self.grid.1 as f32) + r.1 * SPRINT_TURN_AHEAD * UNITS_PER_CELL) * PER_UNIT).floor()
                as i32,
        )
    }

    /// `Positioned_MoveToWaypoint` (`0x141de31a0`).
    fn move_to(&mut self, target: Cell, speed: f32, distance_mode: bool, terrain: &dyn Walkable) {
        if speed <= 0.0 {
            self.line.kill();
            return;
        }
        if target == self.grid {
            self.line_done_at_target(terrain);
            return;
        }
        self.facing = facing_of(
            (target.0 - self.grid.0) as f32,
            (target.1 - self.grid.1) as f32,
        );
        if self.roll.as_ref().is_some_and(|r| r.backward) {
            // Orientation mode 2 (`0x141de3afa`).
            self.facing = (self.facing - PI).rem_euclid(TAU);
        }
        self.line.setup(self.grid, target, speed, distance_mode);
    }

    /// Target == cell: the done delegates run, no line.
    fn line_done_at_target(&mut self, terrain: &dyn Walkable) {
        if self.roll.is_some() {
            return;
        }
        if self.next < self.path.len() {
            let i = self.next;
            self.next += 1;
            self.step_along(i, terrain);
        } else {
            self.path_failed();
        }
    }

    /// `0x141dccac0(pf, 0)`: no way on. `FUN_141de4de0` stops the line; a sprint's end check then
    /// finds no Move and plays the Bonk.
    fn path_failed(&mut self) {
        self.end_path();
        self.line.kill();
        if self.sprint_on {
            self.bonk_due = true;
        }
    }

    // --- Actor ---

    /// The `0x64` vector (`0x1421497c0`, after its snap): each one is a plan.
    pub fn set_axes(&mut self, v: (i32, i32), terrain: &dyn Walkable) {
        if v == (0, 0) && self.axes == (0, 0) {
            return;
        }
        self.axes = v;
        if let Some(r) = &mut self.roll
            && v != (0, 0)
        {
            // The roll runs along the keys while one is held.
            r.heading = facing_of(floor_goal(v.0) as f32, floor_goal(v.1) as f32);
            r.keys = true;
        }
        self.start_wasd(terrain);
    }

    /// `Actor_StartWasdWalk` (`0x141e4fb40`).
    fn start_wasd(&mut self, terrain: &dyn Walkable) {
        if self.roll.is_some() {
            return;
        }
        if self.axes == (0, 0) {
            if self.walk == Walk::Axes {
                self.ending = 2;
                self.line.ended = true;
            }
            return;
        }
        if self.end_anim > 0.0 {
            return;
        }
        self.ending = 0;
        self.walk = Walk::Axes;
        self.plan_now(terrain);
        self.planned = true;
    }

    /// `FUN_141e42650`.
    fn plan_now(&mut self, terrain: &dyn Walkable) {
        self.plan_timer = PLAN_PERIOD;
        self.plan(terrain);
    }

    /// Click-to-move: a Move with a target.
    pub fn walk_to(&mut self, goal: Cell, terrain: &dyn Walkable) {
        self.walk = Walk::Target(goal);
        self.ending = 0;
        self.plan_now(terrain);
    }

    /// `0x141e42a00`.
    pub fn actor_tick(&mut self, dt: f32, terrain: &dyn Walkable) {
        if self.ending > 0 {
            self.ending -= 1;
            if self.ending == 0 {
                self.end_path();
                self.line.kill();
            }
        }
        if self.roll.is_some() {
            self.roll_tick(dt, terrain);
        } else if self.axes != (0, 0) {
            if self.walk == Walk::None {
                self.start_wasd(terrain);
            } else if self.walk == Walk::Axes && self.plan_timer <= 0.0 {
                self.plan_now(terrain);
            }
        } else if let Walk::Target(_) = self.walk
            && self.plan_timer <= 0.0
        {
            // Clicks plan only when their line ends: the timer is not used for them.
        }
        // `0x141b76da0`, a part of this tick: after the re-aim, before the timer.
        if self.bonk_due && self.roll.is_none() {
            self.bonk();
        } else if self.sprint_on
            && (!self.is_sprinting || (self.key_released && !self.must_sprint))
        {
            self.end_sprint();
        }
        self.key_released = false;
        self.slot0_started = false;
        self.plan_timer = (self.plan_timer - dt).max(0.0);
    }

    /// The stat `is_sprinting` changes.
    pub fn set_sprinting(&mut self, on: bool) {
        self.is_sprinting = on;
        if on {
            self.sprint_on = true;
        }
    }

    /// The server's echo of the sprint key's release (`0x141e5c1f0`), read by this frame's end check.
    pub fn sprint_key_released(&mut self) {
        self.key_released = true;
    }

    /// `0x141b773a0` with Bonk: `sprinting_end_*_bonk` views the `SprintEnd` clip at the same rate,
    /// without a move event: the entity stands until it ends.
    fn bonk(&mut self) {
        self.end_sprint();
        self.coast_due = None;
        self.bonk_due = false;
        self.bonked = true;
    }

    /// Ended by a wall: the client plays the Bonk, not a coast.
    pub fn is_bonking(&self) -> bool {
        self.end_anim > 0.0 && !self.coasting && self.coast_due.is_none()
    }

    /// `0x141b773a0`: ends Move, stops the entity, and with no action running plays `SprintEnd`.
    fn end_sprint(&mut self) {
        self.sprint_on = false;
        self.end_path();
        self.ending = 0;
        self.line.kill();
        if self.roll.is_some() || self.slot0_started {
            return;
        }
        if let Some(curve) = self.curves.as_ref().map(|c| &c.sprint_end) {
            // `0x141e56ae0`: movement speed in units over the animation's speed.
            let speed = if self.planned && self.plan_frame_keeps_speed {
                self.frame_speed
            } else {
                self.move_speed()
            };
            let rate = speed * UNITS_PER_CELL
                / (SPRINT_END_ANIM_SPEED * self.offset_scale);
            self.end_anim = curve.length / rate;
            self.coast_due = Some(rate);
        }
    }

    // --- Roll ---

    /// The `0x1c8` of a roll: `cell` is the message's start cell, `target` its target.
    pub fn start_roll(
        &mut self,
        cell: Cell,
        target: Cell,
        total: i32,
        dt: f32,
        terrain: &dyn Walkable,
    ) {
        // `LINE`: a roll into a line that made no step this frame has no first step of its own, and
        // so is one step shorter (reason unread: two rolls in the `stuck` run, frame after a walk's
        // first plan).
        let first_step = !self.line.running() || self.stepped;
        if cell != self.grid {
            self.snap(cell);
        }
        self.line.kill();
        self.end_path();
        self.ending = 0;
        self.coasting = false;
        let playing_end_anim = self.end_anim;
        self.end_anim = 0.0;
        self.coast_due = None;
        // `DodgeRoll_InitHeading` (`0x141b8bda0`).
        let heading = if self.axes != (0, 0) {
            facing_of(floor_goal(self.axes.0) as f32, floor_goal(self.axes.1) as f32)
        } else if target != self.grid {
            facing_of(
                (target.0 - self.grid.0) as f32,
                (target.1 - self.grid.1) as f32,
            )
        } else {
            self.facing
        };
        // The action start turns to its target, so a roll against the cursor runs backwards.
        let faces = if target != self.grid {
            facing_of(
                (target.0 - self.grid.0) as f32,
                (target.1 - self.grid.1) as f32,
            )
        } else {
            self.facing
        };
        let off = (heading - faces).rem_euclid(TAU);
        // Not while `SprintEnd` plays (`LINE`: one roll each way; the reason is unread).
        let backward = playing_end_anim <= 0.0 && off.min(TAU - off) >= PI / 2.0;
        self.facing = heading;
        self.curve = self.curves.as_ref().map(|c| {
            CurveRun::load(if backward { c.roll_back.clone() } else { c.roll.clone() })
        });
        self.roll = Some(Roll {
            heading,
            total,
            elapsed: 0.0,
            steering: true,
            extend: None,
            keys: self.axes != (0, 0),
            backward,
            level: self.level,
        });
        // The action start: a plan from the cell and one step (the second plan is the frame's
        // Actor tick).
        self.roll_step(terrain);
        if first_step {
            self.step_line(dt, terrain);
        } else {
            self.skip_first_step(dt);
        }
    }

    /// Advances the curve as the first step would, without moving.
    fn skip_first_step(&mut self, dt: f32) {
        let rate = self.line.inv_t;
        if let Some(run) = &mut self.curve {
            run.advance(dt, rate);
        }
    }

    /// A new target for the running roll (`0x1cc`, the cursor): it steers when no key is held.
    pub fn steer_roll(&mut self, target: Cell) {
        if target == self.grid {
            return;
        }
        let d = (
            (target.0 - self.grid.0) as f32,
            (target.1 - self.grid.1) as f32,
        );
        if let Some(r) = &mut self.roll
            && !r.keys
        {
            r.heading = facing_of(d.0, d.1);
        }
    }

    /// The roll's animation changes to its sprint variant: same time, new curve (`FUN_141b8cd50`).
    pub fn set_roll_sprinting(&mut self) {
        let Some(curves) = self.curves.clone() else { return };
        if let (Some(run), Some(r)) = (&mut self.curve, &self.roll) {
            let t = r.elapsed;
            *run = CurveRun::load(if r.backward {
                curves.roll_back_sprint.clone()
            } else {
                curves.roll_sprint.clone()
            });
            run.restart(t);
        }
    }

    /// The sprinting roll's lines run this far (see [`Roll::extend`]).
    pub fn extend_roll(&mut self, cells: f32) {
        if let Some(r) = &mut self.roll {
            r.extend = Some(cells);
        }
    }

    /// The roll action ends.
    pub fn end_roll(&mut self) {
        self.roll = None;
    }

    fn roll_left(&self) -> f32 {
        match (&self.curve, &self.roll) {
            (Some(c), Some(r)) => (1.0 - c.done()) * r.total as f32,
            _ => 0.0,
        }
    }

    /// `FUN_141b8c720`.
    fn roll_tick(&mut self, dt: f32, terrain: &dyn Walkable) {
        if let Some(r) = &mut self.roll {
            r.elapsed += dt;
        }
        self.roll_step(terrain);
    }

    /// `FUN_141b8c7f0`.
    fn roll_step(&mut self, terrain: &dyn Walkable) {
        let left = self.roll_left();
        let Some(r) = self.roll.clone() else { return };
        if left <= 1.0 || !r.steering {
            return;
        }
        let along = |h: f32, d: f32| (d * h.sin(), -d * h.cos());
        let base = if self.on_line() {
            self.line.point()
        } else {
            (self.grid.0 as f32, self.grid.1 as f32)
        };
        // `FUN_141b8d1f0`.
        let v = along(r.heading, left.max(ROLL_MIN_LOOKAHEAD));
        let far = (round(base.0 + v.0), round(base.1 + v.1));
        let moving = self.on_line().then(|| self.line.point());
        let plan = clientpath::plan(
            terrain,
            self.grid,
            far,
            r.level,
            &Params {
                moving,
                ..Params::default()
            },
        );
        let goal = plan.waypoints().next().unwrap_or(self.grid);
        // `FUN_141b8d000`: the point `left` away, towards the waypoint.
        let to = (goal.0 as f32 - base.0, goal.1 as f32 - base.1);
        let h = to.1.atan2(to.0) + PI / 2.0;
        let w = along(h, left);
        let target = (round(base.0 + w.0), round(base.1 + w.1));
        let (gx, gy) = (
            (goal.0 - self.grid.0) as f32,
            (goal.1 - self.grid.1) as f32,
        );
        let (tx, ty) = (
            (target.0 - self.grid.0) as f32,
            (target.1 - self.grid.1) as f32,
        );
        // `FUN_141b8d770`, `FUN_141dde3f0` (`LINE`: the restart also runs when no line follows).
        let Some(run) = &mut self.curve else { return };
        let time_left = run.curve.length - r.elapsed;
        if time_left <= 0.0 {
            return;
        }
        let speed = left / time_left;
        run.restart(r.elapsed);
        if (tx * tx + ty * ty).sqrt() < ROLL_STOP_DISTANCE {
            return;
        }
        // `FUN_141b8fe20`: a wall within 2 cells of the start: no move this frame.
        if Self::blocked_within(terrain, self.grid, target, 2) {
            return;
        }
        // `FUN_141b90060`: the line ends before the first wall on the way.
        let target = Self::last_free(terrain, self.grid, target);
        if let Some(ahead) = r.extend {
            // The optional length (`+0x44`, `+0x48`): a long line along the waypoint's direction
            // (`LINE`: not the target's, they differ once `left` is under the look-ahead); the
            // distance per frame stays that of the real target (`+0x280` = speed / its distance).
            let d = (
                (target.0 - self.grid.0) as f32,
                (target.1 - self.grid.1) as f32,
            );
            let dist = (d.0 * d.0 + d.1 * d.1).sqrt();
            let v = along(gy.atan2(gx) + PI / 2.0, ahead);
            let far = (self.grid.0 + round(v.0), self.grid.1 + round(v.1));
            self.move_to(far, speed, true, terrain);
            if dist > 0.0 {
                self.line.rate = speed / dist;
            }
        } else {
            self.move_to(target, speed, true, terrain);
        }
    }

    /// A Bresenham walk from `from` to `to` (`FUN_141b90060`): calls `each` per cell after the
    /// first, until it returns false.
    fn walk_cells(from: Cell, to: Cell, mut each: impl FnMut(Cell) -> bool) {
        let swap = (to.0 - from.0).abs() < (to.1 - from.1).abs();
        let (a0, b0, a1, b1) = if swap {
            (from.1, from.0, to.1, to.0)
        } else {
            (from.0, from.1, to.0, to.1)
        };
        let (da, db) = ((a1 - a0).abs(), (b1 - b0).abs());
        let (sa, sb) = (if a0 < a1 { 1 } else { -1 }, (b1 - b0).signum());
        let mut err = -(da / 2);
        let (mut a, mut b) = (a0, b0);
        while a != a1 + sa {
            let c = if swap { (b, a) } else { (a, b) };
            if !each(c) {
                return;
            }
            a += sa;
            err += db;
            if err >= 1 {
                err -= da;
                b += sb;
            }
        }
    }

    /// `FUN_141b8fe20`: a wall within `cells` cells of `from`.
    fn blocked_within(terrain: &dyn Walkable, from: Cell, to: Cell, cells: i32) -> bool {
        let mut left = cells;
        let mut blocked = false;
        Self::walk_cells(from, to, |c| {
            if !Self::free(terrain, c) {
                blocked = true;
                return false;
            }
            left -= 1;
            left > 0
        });
        blocked
    }

    /// The last free cell of the walk from `from` to `to` (`FUN_141b90060`).
    fn last_free(terrain: &dyn Walkable, from: Cell, to: Cell) -> Cell {
        let mut last = from;
        Self::walk_cells(from, to, |c| {
            if !Self::free(terrain, c) {
                return false;
            }
            last = c;
            true
        });
        last
    }

    // --- Coast ---

    /// `0x141f5f650`: the `SprintEnd` animation's root motion: one line from the grid along the
    /// facing, the curve's displacement x `offset_scale` long, over the animation's time at `rate`.
    /// The speed comes from the rounded target (`LINE`: T stays length / rate).
    fn start_coast(&mut self, rate: f32, terrain: &dyn Walkable) {
        let Some(curve) = self.curves.as_ref().map(|c| c.sprint_end.clone()) else { return };
        let units = (curve.value(curve.length) - curve.value(0.0)) * self.offset_scale;
        let cells = units / UNITS_PER_CELL;
        let f = (self.facing.sin(), -self.facing.cos());
        let d = (round(f.0 * cells), round(f.1 * cells));
        let target = (self.grid.0 + d.0, self.grid.1 + d.1);
        let time = curve.length / rate;
        let len = ((d.0 * d.0 + d.1 * d.1) as f32).sqrt();
        self.line.kill();
        self.curve = Some(CurveRun::load(curve));
        self.coasting = true;
        self.move_to(target, len / time, true, terrain);
    }

    /// Replaces the line with a state logged by the client (`LINE`), for the offline comparison.
    pub fn load_line(&mut self, l: LoggedLine) {
        let major_minor = |v: Cell| if l.swap { (v.1, v.0) } else { v };
        let d = if l.t > 0.0 { l.el / l.t } else { 0.0 };
        self.grid = major_minor(l.cur);
        let e = major_minor(l.e);
        if e != l.s {
            self.facing = facing_of((e.0 - l.s.0) as f32, (e.1 - l.s.1) as f32);
        }
        let distance_mode = self.line.distance_mode && self.line.running();
        let rate = self.line.rate;
        self.line = Line {
            s: l.s,
            p: if l.swap { (l.p.1, l.p.0) } else { l.p },
            q: major_minor(l.q),
            e: major_minor(l.e),
            flag: l.flag,
            swap: l.swap,
            sign: l.sign,
            slope: l.slope,
            end_major: l.e.0,
            major: l.cur.0,
            minor: l.cur.1,
            minor_acc: l.minacc,
            distance_mode,
            len: l.len,
            t: l.t,
            inv_t: 1.0 / l.t,
            rate: if distance_mode { rate } else { 0.0 },
            elapsed: l.el,
            distance: d * l.len,
            thr: l.thr,
            acc: l.acc,
            pending: l.pend,
            step_time: l.t / (l.e.0 as f32 - l.p.0).abs(),
            pending_scale: 1.0,
            ended: false,
        };
    }

    /// The client has no line (`LINE` not logged): the entity stands on `cell`.
    pub fn load_no_line(&mut self, cell: Cell) {
        self.grid = cell;
        self.line = Line {
            pending: self.line.pending,
            ..Line::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMD: &str = "version 5\n2\n\"other\"\nonce\n500\nx\n2 0 1\ny\n\"dodge_unarmed_01\"\nonce\n1000\nmeta\n3 0 400 500\nsegments\n";

    #[test]
    fn a_curve_comes_from_its_amd_block() {
        let c = Curve::from_amd(AMD, "dodge_unarmed_01").unwrap();
        assert_eq!(c.length, 1.0);
        assert_eq!(&*c.positions, &[0.0, 400.0, 500.0]);
        assert_eq!(c.value(0.25), 200.0);
        assert!(Curve::from_amd(AMD, "missing").is_none());
    }

    #[test]
    fn the_stand_in_curves_have_the_measured_ends() {
        let c = MotionCurves::stand_in();
        assert!((c.roll.value(1.0) - 500.0).abs() < 10.0);
        assert!((c.roll_back.value(1.0) + 500.0).abs() < 10.0);
        assert!(c.roll_sprint.value(1.0) > c.roll.value(1.0));
        assert!((c.sprint_end.value(0.75) - 242.425).abs() < 0.01);
    }

    #[test]
    fn a_blocked_sprint_step_bonks_once_with_no_coast() {
        struct Wall;
        impl Walkable for Wall {
            fn walkable(&self, x: i32, _: i32) -> bool {
                x < 60
            }
        }
        let mut m = ClientMotion::at_cell((50, 50), 37.0);
        m.curves = Some(Arc::new(MotionCurves::stand_in()));
        m.offset_scale = STAND_IN_OFFSET_SCALE;
        m.scale = 1.5;
        m.set_sprinting(true);
        let mut bonks = 0;
        for _ in 0..30 {
            m.positioned_tick(0.033, &Wall);
            m.set_axes((550, 0), &Wall);
            m.actor_tick(0.033, &Wall);
            bonks += m.take_bonked() as u32;
            if bonks > 0 {
                break;
            }
        }
        assert_eq!(bonks, 1);
        assert_eq!(m.grid().0, 59);
        assert!(m.is_bonking());
        m.set_axes((550, 0), &Wall);
        for _ in 0..10 {
            m.positioned_tick(0.033, &Wall);
            m.actor_tick(0.033, &Wall);
            assert_eq!(m.grid().0, 59, "it stands during the Bonk");
        }
    }

    #[test]
    fn a_roll_runs_its_distance_and_a_walk_follows_the_keys() {
        let open = crate::movement::Open;
        let mut m = ClientMotion::at_cell((100, 100), 37.0);
        m.curves = Some(Arc::new(MotionCurves::stand_in()));
        m.start_roll((100, 100), (146, 100), 46, 0.033, &open);
        for _ in 0..30 {
            m.positioned_tick(0.033, &open);
            m.actor_tick(0.033, &open);
        }
        let rolled = m.grid().0 - 100;
        assert!((43..=47).contains(&rolled), "{rolled}");
        m.end_roll();
        m.positioned_tick(0.033, &open);
        m.set_axes((0, 180), &open);
        m.actor_tick(0.033, &open);
        let at = m.grid();
        for _ in 0..10 {
            m.positioned_tick(0.033, &open);
            m.actor_tick(0.033, &open);
        }
        assert_eq!(m.grid().0, at.0);
        assert!(m.grid().1 > at.1);
    }
}
