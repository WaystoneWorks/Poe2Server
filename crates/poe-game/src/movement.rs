//! Server-side movement of an entity, in grid cells (23 per tile; a cell is 250/23 world
//! units).
//!
//! The client runs in lockstep: it moves an entity only when the server tells it to (WASD
//! direction `0x64`, actions such as Move or DodgeRoll `0x1c8`), then simulates the movement
//! itself, frame by frame. When the server later sends a position that differs from the
//! client's, the client snaps to it. So the server has to simulate the same movement: same
//! speed, same frames, same obstacles.
//!
//! The client walks along paths from its pathfinder, for clicks, WASD (whose target is the
//! position plus `floor(input vector * 0.092)` cells), rolls and sprints, and plans again every
//! couple of frames. [`Pathfinder::Client`] follows paths from [`crate::clientpath`], a port of
//! that search; [`Pathfinder::Approx`] keeps the older approximation of [`crate::pathfinding`]
//! (A* plus wall sliding), which matches the client in the open and drifts near walls.

use crate::clientpath::{self, Params};
use crate::pathfinding::find_path;

/// Which grid cells an entity can stand on.
pub trait Walkable {
    fn walkable(&self, x: i32, y: i32) -> bool;

    /// Distance-to-wall value of a cell (0 = blocked; the client's grid has 0..5), used to
    /// find which way a wall runs.
    fn clearance(&self, x: i32, y: i32) -> u8 {
        self.walkable(x, y) as u8
    }

    /// The client's 4-bit cell value of layer 0 (0 wall, 1..4 near a wall, 5 clear), which the
    /// client's pathfinder compares with the entity's clearance level. Terrain without such
    /// values has walls (0) and clear cells (5).
    fn nibble(&self, x: i32, y: i32) -> u8 {
        if self.walkable(x, y) { 5 } else { 0 }
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

/// What the entity is doing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Motion {
    Idle,
    /// WASD: keep moving along this direction (client vector, any length).
    Direction {
        x: f32,
        y: f32,
    },
    /// Sprint (after a dodge roll while the key is held): like [`Motion::Direction`], at
    /// `scale` times the walking speed.
    Sprint {
        x: f32,
        y: f32,
        scale: f32,
    },
    /// The client's `(0, 0)` ended a WASD walk: it still walks this one frame along its current
    /// line (the action ends with the frame), then stands ([`Mover::stop_after_step`]).
    LastStep,
    /// Walk to a cell (click-to-move), then stop.
    MoveTo {
        x: f32,
        y: f32,
    },
    /// The entity keeps moving after a sprint ended without a new walk: the client's Move action
    /// is over, but its position still advances along the last direction, `step` cells in the
    /// first frame and `decay` less in each next one until it stops (docs/protocol/movement.md,
    /// "Coasting after a sprint"). Per frame like [`ROLL_STEPS`]: `frame` frames made so far.
    Coast {
        x: f32,
        y: f32,
        step: f32,
        decay: f32,
        frame: u32,
    },
    /// Dodge roll: `remaining` cells towards a direction, along a path, with the roll's speed
    /// profile ([`roll_speed`]); `elapsed` seconds since it started.
    Roll {
        x: f32,
        y: f32,
        remaining: f32,
        elapsed: f32,
        /// The sprint key was still down at `t0 + 15`: from there the client speeds up
        /// ([`ROLL_SPRINT_STEPS`]) instead of tapering off.
        sprint: bool,
    },
}

/// Seconds per frame of the roll table (the official 33 ms frames).
const ROLL_FRAME: f32 = 0.033;

/// Cells the client moves in each frame of a dodge roll (`t0 + 1` to `t0 + 30`), mean of the 6
/// standing rolls on open ground of the 2026-10-20 capture (`dodge_roll_travel_distance` 46:
/// the sum is 45.9, see docs/protocol/movement.md). The roll falls from 3.9 to 1.2 cells per
/// frame, holds about 0.94 and tapers to zero at `t0 + 31`.
pub const ROLL_STEPS: [f32; 30] = [
    3.81, 3.59, 3.37, 3.16, 2.95, 2.73, 2.49, 2.27, 2.02, 1.79, 1.55, 1.32, 1.20, 1.12, 1.06, 0.99,
    0.97, 0.94, 0.94, 0.94, 0.94, 0.95, 0.93, 0.85, 0.74, 0.68, 0.49, 0.31, 0.16, 0.04,
];

/// The steps of frames `t0 + 16` to `t0 + 30` of a roll that became a sprint (the key was still
/// down at `t0 + 15`), mean of the 7 sprint rolls of the 2026-10-20 capture: the client blends
/// into the sprint, so it speeds up from 0.9 to 1.7 cells per frame instead of tapering off, and
/// takes a catch-up step of about 2.4 in the last frame (median of 7: 2.0 to 3.4). From
/// `t0 + 31` on it sprints at 1.86 (the sprint motion).
pub const ROLL_SPRINT_STEPS: [f32; 15] = [
    0.91, 0.96, 1.02, 1.08, 1.09, 1.11, 1.20, 1.34, 1.36, 1.46, 1.49, 1.52, 1.54, 1.70, 2.40,
];

/// The vector a roll along the input `dir` runs: to the goal cell `distance` cells away along the
/// input's plan vector `floor(dir * 0.092)` (so `(-127, 127)` is `(-12, 11)`: 46 cells make
/// `(-34, 31)`, not a true diagonal). The client's plans in the roll keep this goal and its steps
/// keep this ratio (private 2026-11-02 f1641: goal `(702, 767)` from `(736, 736)`; official log,
/// same ratios).
pub fn roll_vector(dir: (i32, i32), distance: f32) -> (f32, f32) {
    let (fx, fy) = (
        (dir.0 as f32 * GOAL_PER_UNIT).floor(),
        (dir.1 as f32 * GOAL_PER_UNIT).floor(),
    );
    match unit(fx, fy) {
        Some((ux, uy)) => (
            (ux * distance + 0.5).floor(),
            (uy * distance + 0.5).floor(),
        ),
        None => (dir.0 as f32, dir.1 as f32),
    }
}

/// Index into [`ROLL_STEPS`] of the frame `t` seconds into the roll.
fn roll_frame(t: f32) -> usize {
    ((t / ROLL_FRAME) + 1e-3).floor().max(0.0) as usize
}

/// Cells per frame of frame `frame` (0 based) of a roll.
fn roll_step(frame: usize, sprint: bool) -> f32 {
    if sprint && frame >= 15 {
        ROLL_SPRINT_STEPS.get(frame - 15)
    } else {
        ROLL_STEPS.get(frame)
    }
    .copied()
    .unwrap_or(0.0)
}

/// Dodge roll speed (cells per second) `t` seconds into the roll: the per-frame steps of
/// [`ROLL_STEPS`] (0 once the table is over). The client drives it from the roll animation.
pub fn roll_speed(t: f32, sprint: bool) -> f32 {
    roll_step(roll_frame(t), sprint) / ROLL_FRAME
}

/// Which pathfinder plans the walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pathfinder {
    /// The port of the client's own search ([`crate::clientpath`]).
    #[default]
    Client,
    /// A* with line-of-sight smoothing and wall sliding ([`crate::pathfinding`]).
    Approx,
}

/// The client re-aims a WASD walk every 0.05 s while a key is held (player entities; `0.25` for
/// others): with 33 ms frames that is every second frame.
const AIM_PERIOD: f32 = 0.05;
/// Radians per second a sprint's heading turns towards the plan's first waypoint: the client
/// moves along this heading, not along the plan (official 2026-11-02 log: best fit about 0.086
/// rad per 33 ms frame, median error 0.006 rad; walking turns at once).
const SPRINT_TURN_RATE: f32 = 150.0 * std::f32::consts::PI / 180.0;
/// A WASD or sprint vector turns into a goal this many cells per input unit away
/// (`floor(v * 0.092)`).
const GOAL_PER_UNIT: f32 = 0.092;
/// Length of the sprint stream's direction (the `0x64` the client plans from).
const SPRINT_VECTOR_LENGTH: f32 = 550.0;
/// The search window reaches 136 cells; farther goals are pulled in to this length.
const GOAL_LIMIT_SQ: i32 = 0x4840;
const GOAL_CLAMP: f32 = 126.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Mover {
    /// Position in grid cells (cell centre = integer + 0.5).
    pub x: f32,
    pub y: f32,
    /// Walking speed in cells per second.
    pub speed: f32,
    /// The client's action speed factor (Pathfinding `+0x55c`): walks, WASD and sprints move at
    /// `speed * factor` while an attack slows the entity (see `locomotion::ActionSpeed`); rolls
    /// and coasts are not scaled.
    pub factor: f32,
    pub motion: Motion,
    /// Waypoints (cells) of the current walk.
    path: Vec<(i32, i32)>,
    /// Frames until the WASD path is planned again.
    replan_in: u32,
    /// Frames the entity stands still before the motion starts (the client's gap between a
    /// message that ends or restarts its walk and the first step).
    delay: u32,
    /// Which pathfinder plans the walks.
    pub pathfinder: Pathfinder,
    /// Clearance level of the entity's path searches: cells with a grid value up to this are
    /// walls (0: only the walls themselves; 4 while it sprints after a roll).
    pub level: i32,
    /// Seconds until a held WASD walk is aimed again.
    aim_timer: f32,
    /// The client's current line: its own grid cell (the one its plans start from and the
    /// `0x64` cell is compared with) steps along it, see [`GridLine`] and [`Mover::grid`].
    line: GridLine,
    /// Direction of the last step along a plan: the client's entity carries on in it when its
    /// walk ends (the coast after a sprint) and stops at the first wall.
    last_dir: Option<(f32, f32)>,
    coast_dir: Option<(f32, f32)>,
    /// Unit direction of the last frame's displacement.
    travel_dir: Option<(f32, f32)>,
    /// Heading of the running sprint in radians ([`SPRINT_TURN_RATE`]).
    sprint_heading: Option<f32>,
    /// The entity moved in the previous frame: its plans then test line of sight to the start
    /// from its exact position ([`clientpath::Params::moving`]).
    moved: bool,
}

/// Cells the pathfinder may expand per plan.
const PATH_BUDGET: usize = 8_000;
/// WASD plans towards this many cells along the input (the client aims about 180 cells
/// ahead; the first part of the path is what matters between re-plans).
const WASD_LOOKAHEAD: f32 = 40.0;
/// WASD paths are planned again every this many frames.
const REPLAN_FRAMES: u32 = 6;

impl Mover {
    /// An idle entity at the centre of a grid cell.
    pub fn at_cell((x, y): (i32, i32), speed: f32) -> Self {
        Self {
            x: x as f32 + 0.5,
            y: y as f32 + 0.5,
            speed,
            factor: 1.0,
            motion: Motion::Idle,
            path: Vec::new(),
            replan_in: 0,
            delay: 0,
            pathfinder: Pathfinder::Client,
            level: 0,
            aim_timer: 0.0,
            line: GridLine::at((x, y)),
            last_dir: None,
            coast_dir: None,
            moved: false,
            travel_dir: None,
            sprint_heading: None,
        }
    }

    /// An action's `0x1c8` places the client on its start cell (the grid it was sent) and the
    /// line starts anew there: the server's position follows.
    pub fn stand_on_grid(&mut self) {
        let cell = self.grid();
        self.x = cell.0 as f32 + 0.5;
        self.y = cell.1 as f32 + 0.5;
        self.line = GridLine::at(cell);
    }

    /// A WASD walk stops: one more frame along the current line, keeping path and line.
    pub fn stop_after_step(&mut self) {
        self.motion = Motion::LastStep;
    }

    /// Starts a motion (plans walks on the next frame).
    pub fn start(&mut self, motion: Motion) {
        if !matches!(motion, Motion::Sprint { .. }) {
            self.sprint_heading = None;
        }
        // A coast carries on in the direction of the last step of the walk it follows.
        self.coast_dir = if matches!(motion, Motion::Coast { .. }) {
            self.last_dir
        } else {
            None
        };
        // The coast is one Positioned move from the client's cell to the cell its root motion
        // ends in (`0x141f5f650`): one line.
        if let (Motion::Coast { step, decay, .. }, Some((ux, uy))) = (motion, self.coast_dir) {
            let frames = if decay > 0.0 { (step / decay).ceil().max(0.0) } else { 0.0 };
            let total = step * frames - decay * frames * (frames - 1.0) / 2.0;
            let s = self.grid();
            let e = (
                (s.0 as f32 + ux * total + 0.5).floor() as i32,
                (s.1 as f32 + uy * total + 0.5).floor() as i32,
            );
            self.line = GridLine::new(s, e, None);
        }
        self.motion = motion;
        self.path.clear();
        self.replan_in = 0;
        self.delay = 0;
        self.aim_timer = 0.0;
    }

    /// Starts a motion that first stands still for `frames` frames.
    pub fn start_after(&mut self, motion: Motion, frames: u32) {
        self.start(motion);
        self.delay = frames;
    }

    /// The running roll turns into a sprint roll (the key was still down at `t0 + 15`): the
    /// rest of it follows [`ROLL_SPRINT_STEPS`] and is not limited to the roll's distance.
    pub fn sprint_roll(&mut self) {
        if let Motion::Roll {
            remaining, sprint, ..
        } = &mut self.motion
        {
            *sprint = true;
            *remaining = f32::MAX;
        }
    }

    /// Waypoints (cells) left of the current walk.
    pub fn path(&self) -> &[(i32, i32)] {
        &self.path
    }

    /// Moves the entity to a cell centre and stops it.
    pub fn place(&mut self, (x, y): (i32, i32)) {
        self.x = x as f32 + 0.5;
        self.y = y as f32 + 0.5;
        self.line = GridLine::at((x, y));
        self.start(Motion::Idle);
    }

    /// The grid cell the position is in.
    pub fn cell(&self) -> (i32, i32) {
        (self.x.floor() as i32, self.y.floor() as i32)
    }

    /// The client's own grid cell (Positioned `+0x444/+0x448`): the cell the `0x64` must carry,
    /// since the client snaps to any other. It steps along the current line ([`GridLine`]) and
    /// is often not the cell of the position. Where the entity left its line (wall slides, the
    /// approximate pathfinder) the cell of the position is used.
    pub fn grid(&self) -> (i32, i32) {
        let g = self.line.cell((self.x - 0.5, self.y - 0.5));
        let off = (g.0 as f32 + 0.5 - self.x).abs().max((g.1 as f32 + 0.5 - self.y).abs());
        if off > 2.0 { self.cell() } else { g }
    }

    pub fn is_moving(&self) -> bool {
        self.motion != Motion::Idle
    }

    /// Advances one frame of `dt` seconds.
    pub fn advance(&mut self, dt: f32, terrain: &dyn Walkable) {
        let before = (self.x, self.y);
        self.advance_frame(dt, terrain);
        self.moved = (self.x, self.y) != before;
        if self.moved {
            let (dx, dy) = (self.x - before.0, self.y - before.1);
            let len = (dx * dx + dy * dy).sqrt();
            self.travel_dir = Some((dx / len, dy / len));
        }
    }

    fn advance_frame(&mut self, dt: f32, terrain: &dyn Walkable) {
        if self.delay > 0 && self.motion != Motion::Idle {
            self.delay -= 1;
            return;
        }
        match self.motion {
            Motion::Idle => {}
            Motion::LastStep => {
                self.follow(self.speed * self.factor * dt);
                self.motion = Motion::Idle;
            }
            Motion::Coast {
                x,
                y,
                step,
                decay,
                frame,
            } => {
                let len = step - decay * frame as f32;
                match unit(x, y) {
                    Some(_) if len > 0.0 && self.coast_dir.is_some() => {
                        let (ux, uy) = self.coast_dir.unwrap();
                        let (nx, ny) = (self.x + ux * len, self.y + uy * len);
                        self.motion = if terrain.walkable(nx.floor() as i32, ny.floor() as i32) {
                            self.x = nx;
                            self.y = ny;
                            Motion::Coast {
                                x,
                                y,
                                step,
                                decay,
                                frame: frame + 1,
                            }
                        } else {
                            Motion::Idle
                        };
                    }
                    Some((ux, uy)) if len > 0.0 => {
                        self.slide(ux, uy, len, terrain);
                        self.motion = Motion::Coast {
                            x,
                            y,
                            step,
                            decay,
                            frame: frame + 1,
                        };
                    }
                    _ => self.motion = Motion::Idle,
                }
            }
            Motion::Direction { x, y } => {
                if self.pathfinder == Pathfinder::Client {
                    self.walk_aimed(x, y, 1.0, dt, terrain, false);
                } else {
                    self.walk_towards(x, y, 1.0, dt, terrain);
                }
            }
            Motion::Sprint { x, y, scale } => {
                if self.pathfinder == Pathfinder::Client {
                    self.walk_aimed(x, y, scale, dt, terrain, true);
                } else {
                    self.walk_towards(x, y, scale, dt, terrain);
                }
            }
            Motion::MoveTo { x, y } => {
                if self.path.is_empty() && self.replan_in == 0 {
                    let goal = (x.floor() as i32, y.floor() as i32);
                    self.path = if self.pathfinder == Pathfinder::Client {
                        self.waypoints_to(terrain, goal)
                    } else {
                        find_path(terrain, self.cell(), goal, PATH_BUDGET)
                    };
                    // Plan once per click.
                    self.replan_in = u32::MAX;
                }
                if !self.follow(self.speed * self.factor * dt) {
                    // The client plans again where a plan ended short of the goal (a goal
                    // beyond the search window, or inside a wall) and stops when that plan
                    // has nowhere to go.
                    let goal = (x.floor() as i32, y.floor() as i32);
                    if self.pathfinder == Pathfinder::Client && self.cell() != goal {
                        self.path = self.waypoints_to(terrain, goal);
                        if !self.path.is_empty() {
                            return;
                        }
                    }
                    self.start(Motion::Idle);
                }
            }
            Motion::Roll {
                x,
                y,
                remaining,
                elapsed,
                sprint,
            } => {
                let step = (roll_speed(elapsed, sprint) * dt).min(remaining);
                let moved = match unit(x, y) {
                    Some((ux, uy)) if self.pathfinder == Pathfinder::Client => {
                        self.roll_planned(ux, uy, remaining, step, terrain)
                    }
                    Some((ux, uy)) => self.slide(ux, uy, step, terrain),
                    None => 0.0,
                };
                // A blocked frame costs no distance: the roll goes on with the profile (the
                // client's roll animation runs its 30 frames even against a wall, and a later
                // steer can free it).
                let remaining = remaining - moved;
                // The last table step is the last frame of the roll.
                let last = roll_frame(elapsed) + 1 >= ROLL_STEPS.len();
                self.motion = if remaining > 1e-3 && !last {
                    Motion::Roll {
                        x,
                        y,
                        remaining,
                        elapsed: elapsed + dt,
                        sprint,
                    }
                } else {
                    Motion::Idle
                };
            }
        }
    }

    /// Moves `len` cells along a unit vector; when the next cell is blocked, moves along the
    /// wall instead (the direction minus its component into the wall, the wall's normal
    /// taken from the clearance gradient). Returns the distance moved.
    fn slide(&mut self, ux: f32, uy: f32, len: f32, terrain: &dyn Walkable) -> f32 {
        let free = |x: f32, y: f32| terrain.walkable(x.floor() as i32, y.floor() as i32);
        let (nx, ny) = (self.x + ux * len, self.y + uy * len);
        if free(nx, ny) {
            self.x = nx;
            self.y = ny;
            return len;
        }
        let (cx, cy) = self.cell();
        let c = |x: i32, y: i32| terrain.clearance(x, y) as f32;
        // Gradient over a 5-cell span (points away from the wall).
        let gx = c(cx + 2, cy) - c(cx - 2, cy);
        let gy = c(cx, cy + 2) - c(cx, cy - 2);
        let mut tangent = None;
        if let Some((gx, gy)) = unit(gx, gy) {
            let into = ux * gx + uy * gy;
            if into < 0.0 {
                tangent = unit(ux - into * gx, uy - into * gy);
            }
        }
        let candidates = tangent
            .into_iter()
            .chain([(ux.signum(), 0.0), (0.0, uy.signum())]);
        for (tx, ty) in candidates {
            if tx == 0.0 && ty == 0.0 || tx * ux + ty * uy <= 0.0 {
                continue;
            }
            // Along the wall only the part of the motion that goes that way.
            let along = len * (tx * ux + ty * uy);
            let (nx, ny) = (self.x + tx * along, self.y + ty * along);
            if free(nx, ny) {
                self.x = nx;
                self.y = ny;
                return along;
            }
        }
        0.0
    }

    /// Walks along a direction (WASD, sprint), planning towards a point ahead of the entity
    /// every few frames.
    fn walk_towards(&mut self, x: f32, y: f32, scale: f32, dt: f32, terrain: &dyn Walkable) {
        if x == 0.0 && y == 0.0 {
            self.motion = Motion::Idle;
            return;
        }
        if self.replan_in == 0 || self.path.is_empty() {
            let (cx, cy) = self.cell();
            let len = (x * x + y * y).sqrt();
            let goal = (
                cx + (x / len * WASD_LOOKAHEAD).round() as i32,
                cy + (y / len * WASD_LOOKAHEAD).round() as i32,
            );
            self.path = find_path(terrain, (cx, cy), goal, PATH_BUDGET);
            self.replan_in = REPLAN_FRAMES;
        }
        self.replan_in -= 1;
        self.follow(self.speed * self.factor * scale * dt);
    }

    /// The cell the client plans from: its own grid cell ([`Mover::grid`]; with the earlier
    /// approximation `trunc(position - 0.5 - line start)`, 66 % of 643 logged plans started in
    /// it against 45 % for the cell of the position). Where that cell is a wall, the cell of the
    /// position or its free neighbour is used.
    fn planning_cell(&self, terrain: &dyn Walkable) -> (i32, i32) {
        let lagging = self.grid();
        let blocked = |c: (i32, i32)| terrain.nibble(c.0, c.1) == 0;
        if !blocked(lagging) {
            return lagging;
        }
        let here = self.cell();
        if !blocked(here) {
            return here;
        }
        (-1..=1)
            .flat_map(|dy| (-1..=1).map(move |dx| (here.0 + dx, here.1 + dy)))
            .filter(|&c| !blocked(c))
            .min_by(|a, b| {
                let d = |c: (i32, i32)| (c.0 as f32 + 0.5 - self.x).powi(2) + (c.1 as f32 + 0.5 - self.y).powi(2);
                d(*a).total_cmp(&d(*b))
            })
            .unwrap_or(here)
    }

    /// Moving along a line whose cell is `start`: line of sight to the start is tested from the
    /// exact position (`FUN_141b87580`), in the client's cell-centred coordinates.
    fn moving_from(&self, start: (i32, i32)) -> Option<(f32, f32)> {
        (self.moved && start == self.grid()).then_some((self.x - 0.5, self.y - 0.5))
    }

    /// The goal of a plan from `start`: one beyond the search window is pulled in to 126 cells
    /// along the same line.
    fn clamp_goal(start: (i32, i32), goal: (i32, i32)) -> (i32, i32) {
        let (dx, dy) = (goal.0 as i64 - start.0 as i64, goal.1 as i64 - start.1 as i64);
        if dx * dx + dy * dy < GOAL_LIMIT_SQ as i64 {
            return goal;
        }
        let k = GOAL_CLAMP / ((dx * dx + dy * dy) as f32).sqrt();
        (
            (start.0 as i64 + (dx as f32 * k + 0.5).floor() as i64) as i32,
            (start.1 as i64 + (dy as f32 * k + 0.5).floor() as i64) as i32,
        )
    }

    /// The direction a sprint plans along: the stream's vector of length 550, truncated toward
    /// zero.
    fn sprint_vector(x: f32, y: f32) -> (f32, f32) {
        let len = ((x as f64).powi(2) + (y as f64).powi(2)).sqrt();
        let k = SPRINT_VECTOR_LENGTH as f64 / len;
        ((x as f64 * k) as i32 as f32, (y as f64 * k) as i32 as f32)
    }

    /// The running sprint has nowhere to go from here: the client's next plan is empty, so its
    /// Move action ends this frame and it ends the sprint itself (`0x141b76da0`: no action left
    /// -> `SprintEndBonk`). Changes nothing.
    pub fn sprint_blocked(&self, terrain: &dyn Walkable) -> bool {
        let Motion::Sprint { x, y, .. } = self.motion else {
            return false;
        };
        if self.pathfinder != Pathfinder::Client || self.delay > 0 || (x == 0.0 && y == 0.0) {
            return false;
        }
        let (vx, vy) = Self::sprint_vector(x, y);
        let start = self.planning_cell(terrain);
        let goal = Self::clamp_goal(
            start,
            (
                start.0 + (vx * GOAL_PER_UNIT).floor() as i32,
                start.1 + (vy * GOAL_PER_UNIT).floor() as i32,
            ),
        );
        let params = Params {
            moving: self.moving_from(start),
            ..Params::default()
        };
        clientpath::plan(terrain, start, goal, self.level, &params)
            .waypoints()
            .all(|w| w == start)
    }

    /// The client's plan from the entity's cell to `goal`.
    fn plan_client(&mut self, terrain: &dyn Walkable, goal: (i32, i32)) -> clientpath::Plan {
        let start = self.planning_cell(terrain);
        let moving = self.moving_from(start);
        let goal = Self::clamp_goal(start, goal);
        // A line that starts standing runs from the centre of its start cell, wherever the
        // position had got to: the client's phase restarts here, so the server's does too.
        if moving.is_none() && !self.moved {
            self.x = start.0 as f32 + 0.5;
            self.y = start.1 as f32 + 0.5;
        }
        let params = Params {
            moving,
            ..Params::default()
        };
        let plan = clientpath::plan(terrain, start, goal, self.level, &params);
        // The walk's first line runs from the start cell to the first waypoint
        // (`Positioned_MoveToWaypoint` -> `FUN_141ec86a0`).
        self.line = match plan.waypoints().next() {
            Some(first) => GridLine::new(start, first, moving),
            None => GridLine::at(start),
        };
        plan
    }

    /// Waypoints of the client's plan from the entity's cell to `goal`, in walking order.
    fn waypoints_to(&mut self, terrain: &dyn Walkable, goal: (i32, i32)) -> Vec<(i32, i32)> {
        self.plan_client(terrain, goal).waypoints().collect()
    }

    /// WASD and sprint like the client: the goal is the cell plus `floor(vector * 0.092)`, planned
    /// again every [`AIM_PERIOD`] (every frame while a sprint's `0x64` stream runs, and whenever
    /// the walk has ended), and the entity walks the first legs of the plan.
    fn walk_aimed(
        &mut self,
        x: f32,
        y: f32,
        scale: f32,
        dt: f32,
        terrain: &dyn Walkable,
        sprint: bool,
    ) {
        if x == 0.0 && y == 0.0 {
            self.motion = Motion::Idle;
            return;
        }
        self.aim_timer -= dt;
        if sprint || self.path.is_empty() || self.aim_timer <= 0.0 {
            let (vx, vy) = if sprint { Self::sprint_vector(x, y) } else { (x, y) };
            let cell = self.planning_cell(terrain);
            let moving = self.moving_from(cell);
            let goal = (
                cell.0 + (vx * GOAL_PER_UNIT).floor() as i32,
                cell.1 + (vy * GOAL_PER_UNIT).floor() as i32,
            );
            self.path = self.waypoints_to(terrain, goal);
            self.aim_timer = AIM_PERIOD;
            if sprint {
                self.turn_sprint(cell, moving, scale, dt, terrain);
            }
        }
        self.follow(self.speed * self.factor * scale * dt);
    }

    /// A sprint's heading turns towards the plan's first waypoint at [`SPRINT_TURN_RATE`]; while it
    /// is still turning the entity and its grid line follow the heading, so the walk's path is
    /// replaced by the point ahead on it (unless that step would enter a wall).
    fn turn_sprint(
        &mut self,
        cell: (i32, i32),
        moving: Option<(f32, f32)>,
        scale: f32,
        dt: f32,
        terrain: &dyn Walkable,
    ) {
        let Some(&first) = self.path.first() else {
            return;
        };
        let target = ((first.1 - cell.1) as f32).atan2((first.0 - cell.0) as f32);
        let heading = self
            .sprint_heading
            .or(self.travel_dir.map(|(x, y)| y.atan2(x)))
            .unwrap_or(target);
        let tau = std::f32::consts::TAU;
        let error = (target - heading + std::f32::consts::PI).rem_euclid(tau) - std::f32::consts::PI;
        let max = SPRINT_TURN_RATE * dt;
        let heading = heading + error.clamp(-max, max);
        self.sprint_heading = Some(heading);
        if error.abs() <= max {
            return;
        }
        let (ux, uy) = (heading.cos(), heading.sin());
        let step = self.speed * self.factor * scale * dt;
        if !terrain.walkable(
            (self.x + ux * step).floor() as i32,
            (self.y + uy * step).floor() as i32,
        ) {
            self.sprint_heading = Some(target);
            return;
        }
        let ahead = SPRINT_VECTOR_LENGTH * GOAL_PER_UNIT;
        let end = (
            cell.0 + (ux * ahead + 0.5).floor() as i32,
            cell.1 + (uy * ahead + 0.5).floor() as i32,
        );
        self.path = vec![end];
        self.line = GridLine::new(cell, end, moving);
    }

    /// One frame of a roll towards `(ux, uy)` with `remaining` cells to go: plan to the point
    /// that far ahead and walk `step` cells of the plan. Returns the distance moved.
    fn roll_planned(
        &mut self,
        ux: f32,
        uy: f32,
        remaining: f32,
        step: f32,
        terrain: &dyn Walkable,
    ) -> f32 {
        let cell = self.planning_cell(terrain);
        let ahead = remaining.min(GOAL_CLAMP);
        let goal = (
            cell.0 + (ux * ahead + 0.5).floor() as i32,
            cell.1 + (uy * ahead + 0.5).floor() as i32,
        );
        let plan = self.plan_client(terrain, goal);
        // Open ground: one straight leg to the goal (or the goal is this very cell). The roll
        // then keeps its exact heading and distance (the goal cell's centre is up to half a cell
        // off the ideal point).
        if goal == cell || plan.reached && plan.path.len() == 2 {
            self.x += ux * step;
            self.y += uy * step;
            return step;
        }
        self.path = plan.waypoints().collect();
        let before = (self.x, self.y);
        self.follow(step);
        self.path.clear();
        ((self.x - before.0).powi(2) + (self.y - before.1).powi(2)).sqrt()
    }

    /// Walks `len` cells along the path (waypoints are cell centres). Returns whether there is
    /// path left.
    fn follow(&mut self, mut len: f32) -> bool {
        while len > 0.0 {
            let Some(&(wx, wy)) = self.path.first() else {
                return false;
            };
            let (tx, ty) = (wx as f32 + 0.5, wy as f32 + 0.5);
            let (dx, dy) = (tx - self.x, ty - self.y);
            let dist = (dx * dx + dy * dy).sqrt();
            if dist > 1e-4 {
                self.last_dir = Some((dx / dist, dy / dist));
            }
            if dist <= len {
                self.x = tx;
                self.y = ty;
                len -= dist;
                self.path.remove(0);
                // The next line starts on the waypoint reached (the old one has ended there).
                self.line = match self.path.first() {
                    Some(&next) => GridLine::new((wx, wy), next, None),
                    None => GridLine::at((wx, wy)),
                };
            } else {
                self.x += dx / dist * len;
                self.y += dy / dist * len;
                len = 0.0;
            }
        }
        !self.path.is_empty()
    }
}

/// The line the client's grid cell steps along while its entity walks one leg (Positioned
/// `+0x208`, set up by `FUN_141ec86a0`, stepped by `FUN_141ec8cb0` from the Positioned tick
/// `FUN_141ddc9d0`). In cell-centred coordinates (cell centre = integer):
/// - The line runs from a point `p` to the end cell `e`. `p` is the start cell `s`, or, when the
///   entity was moving along its previous line, still in `s`, and is less than 2 cells from it
///   (`|s - p|^2 < 4`), the exact position; the start is then `q`, the projection of `s` onto
///   the line, rounded.
/// - The major axis is the one with the longer integer distance from `q` to `e` (x on a tie,
///   or the longer float distance when starting from the position).
/// - An accumulator starts at `|e - q| - |e - p|` (major axis, in cells) and grows with the
///   distance walked; each whole cell steps the major coordinate one cell towards `e` (the
///   first step first walks `s` to `q`) and the minor one to `floor(minor)`, where `minor`
///   starts at `p_minor - slope * offset + c` (`slope = (e - p)_minor / |e - p|_major`,
///   `c = 0.49` for a positive slope, else `0.5`) and grows by `slope` per step.
///
/// Closed form used here: `k = floor(|e - q| - |e - pos|)` (major axis) whole steps; the
/// cell is `s` while `k = 0`, else `(q + k * sign, floor(minor + k * slope))`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct GridLine {
    s: (i32, i32),
    q: (i32, i32),
    e: (i32, i32),
    y_major: bool,
    slope: f32,
    minor0: f32,
}

impl GridLine {
    /// No line: the grid stays on `cell`.
    fn at(cell: (i32, i32)) -> Self {
        Self {
            s: cell,
            q: cell,
            e: cell,
            y_major: false,
            slope: 0.0,
            minor0: 0.0,
        }
    }

    /// A line from cell `s` to cell `e`; `pos` is the exact position (cell-centred) when the
    /// entity moved along its previous line (the moving variant).
    fn new(s: (i32, i32), e: (i32, i32), pos: Option<(f32, f32)>) -> Self {
        let sf = (s.0 as f32, s.1 as f32);
        let ef = (e.0 as f32, e.1 as f32);
        let p = match pos {
            Some(p) if p != ef && (sf.0 - p.0).powi(2) + (sf.1 - p.1).powi(2) < 4.0 => p,
            _ => sf,
        };
        let d = (ef.0 - p.0, ef.1 - p.1);
        let q = if p == sf {
            s
        } else {
            let len2 = d.0 * d.0 + d.1 * d.1;
            let t = if len2 > 0.0 {
                ((sf.0 - p.0) * d.0 + (sf.1 - p.1) * d.1) / len2
            } else {
                0.0
            };
            (
                (d.0 * t + p.0 + 0.5).floor() as i32,
                (d.1 * t + p.1 + 0.5).floor() as i32,
            )
        };
        let (ix, iy) = ((e.0 - q.0).abs(), (e.1 - q.1).abs());
        let y_major = ix < iy || (p != sf && ix == iy && d.0.abs() < d.1.abs());
        let (p_maj, p_min, e_min, q_maj, e_maj) = if y_major {
            (p.1, p.0, ef.0, q.1, e.1)
        } else {
            (p.0, p.1, ef.1, q.0, e.0)
        };
        let df = (e_maj as f32 - p_maj).abs();
        if df <= 0.0 {
            return Self::at(e);
        }
        let slope = (e_min - p_min) / df;
        let offset = (e_maj - q_maj).abs() as f32 - df;
        let c = if slope > 0.0 { 0.49 } else { 0.5 };
        Self {
            s,
            q,
            e,
            y_major,
            slope,
            minor0: p_min - slope * offset + c,
        }
    }

    /// The grid cell for the position `pos` (cell-centred) along this line.
    fn cell(&self, pos: (f32, f32)) -> (i32, i32) {
        if self.q == self.e {
            return if self.s == self.e { self.e } else { self.s };
        }
        let (pos_maj, q_maj, e_maj) = if self.y_major {
            (pos.1, self.q.1, self.e.1)
        } else {
            (pos.0, self.q.0, self.e.0)
        };
        let total = (e_maj - q_maj).abs();
        let rem = (e_maj as f32 - pos_maj).abs();
        let k = ((total as f32 - rem + 1e-4).floor() as i32).clamp(0, total);
        if k == 0 {
            return self.s;
        }
        let maj = q_maj + k * (e_maj - q_maj).signum();
        let min = (self.minor0 + k as f32 * self.slope).floor() as i32;
        if self.y_major { (min, maj) } else { (maj, min) }
    }
}

fn unit(x: f32, y: f32) -> Option<(f32, f32)> {
    let len = (x * x + y * y).sqrt();
    (len > 0.0).then(|| (x / len, y / len))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Wall;
    impl Walkable for Wall {
        /// Cells with x >= 10 are blocked.
        fn walkable(&self, x: i32, _: i32) -> bool {
            x < 10
        }
    }

    #[test]
    fn the_grid_steps_along_its_line_behind_the_position() {
        // Official 2026-11-02 f503: plan from (282, 252) to (269, 281), y major.
        let line = GridLine::new((282, 252), (269, 281), None);
        // Less than a whole cell along: still the start cell (the position's cell is (281, 252)).
        assert_eq!(line.cell((281.6, 252.9)), (282, 252));
        // One whole cell: the minor axis follows the line (floor(282 - 13/29 + 0.5) = 282),
        // not the position.
        assert_eq!(line.cell((281.5, 253.2)), (282, 253));
        assert_eq!(line.cell((269.0, 281.0)), (269, 281));
        // A new line started from the exact position: the start cell until a whole cell is done.
        let moving = GridLine::new((272, 282), (296, 291), Some((271.31, 281.87)));
        assert_eq!(moving.cell((271.9, 282.1)), (272, 282));
        // Idle: the cell itself.
        assert_eq!(GridLine::at((5, 7)).cell((9.0, 9.0)), (5, 7));
    }

    #[test]
    fn walks_in_a_direction() {
        let mut m = Mover::at_cell((0, 0), 37.0);
        m.start(Motion::Direction { x: 180.0, y: 0.0 });
        for _ in 0..30 {
            m.advance(0.033, &Open);
        }
        assert!((m.x - (0.5 + 37.0 * 0.99)).abs() < 1e-3);
        assert_eq!(m.cell().1, 0);
    }

    #[test]
    fn move_to_stops_at_the_target() {
        let mut m = Mover::at_cell((0, 0), 37.0);
        m.start(Motion::MoveTo { x: 5.5, y: 5.5 });
        for _ in 0..30 {
            m.advance(0.033, &Open);
        }
        assert_eq!(m.cell(), (5, 5));
        assert_eq!(m.motion, Motion::Idle);
    }

    #[test]
    fn walks_along_a_wall() {
        let mut m = Mover::at_cell((8, 0), 37.0);
        m.start(Motion::Direction { x: 127.0, y: 127.0 });
        for _ in 0..30 {
            m.advance(0.033, &Wall);
        }
        assert!(m.x < 10.0);
        assert!(m.y > 10.0);
    }

    #[test]
    fn roll_covers_its_length() {
        let mut m = Mover::at_cell((0, 0), 37.0);
        m.start(Motion::Roll {
            x: 0.0,
            y: 1.0,
            remaining: 46.0,
            elapsed: 0.0,
            sprint: false,
        });
        let mut frames = 0;
        while m.is_moving() {
            m.advance(0.033, &Open);
            frames += 1;
        }
        // The official profile covers 45.9 of the 46 cells in 30 frames.
        assert!(
            (m.y - (0.5 + ROLL_STEPS.iter().sum::<f32>())).abs() < 0.05,
            "{}",
            m.y
        );
        assert_eq!(frames, 30);
    }

    #[test]
    fn a_sprint_roll_speeds_up_after_frame_15() {
        let mut m = Mover::at_cell((0, 0), 37.0);
        m.start(Motion::Roll {
            x: 1.0,
            y: 0.0,
            remaining: 46.0,
            elapsed: 0.0,
            sprint: false,
        });
        let mut last = m.x;
        for i in 0..30 {
            if i == 15 {
                m.sprint_roll();
            }
            m.advance(0.033, &Open);
            let expect = if i < 15 {
                ROLL_STEPS[i]
            } else {
                ROLL_SPRINT_STEPS[i - 15]
            };
            assert!((m.x - last - expect).abs() < 1e-3, "frame {}", i + 1);
            last = m.x;
        }
        assert!(!m.is_moving());
        // About 55 cells in the 30 frames, against 46 for a tap roll.
        assert!(m.x > 0.5 + 54.0 && m.x < 0.5 + 56.0, "{}", m.x);
    }

    #[test]
    fn roll_follows_the_official_steps() {
        let mut m = Mover::at_cell((0, 0), 37.0);
        m.start(Motion::Roll {
            x: 1.0,
            y: 0.0,
            remaining: 46.0,
            elapsed: 0.0,
            sprint: false,
        });
        let mut last = m.x;
        for (i, step) in ROLL_STEPS.iter().enumerate() {
            m.advance(0.033, &Open);
            assert!((m.x - last - step).abs() < 1e-3, "frame {}", i + 1);
            last = m.x;
        }
        assert!(!m.is_moving());
    }
}
