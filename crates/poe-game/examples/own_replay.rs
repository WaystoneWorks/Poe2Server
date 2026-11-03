//! End-to-end offline check: the client's inputs of an official capture
//! (`scripts/re/movement_own_inputs.py`) go to our server's [`Locomotion`]; its messages go to a
//! separate client model ([`ClientSim`]) in the client's drain order. Reports whether a `0x64` of
//! ours would snap that client, our messages against the official ones, and the client's position
//! against the official client's log (`scripts/re/movement_replay_trace.py`).
//!
//! `cargo run -p poe-game --example own_replay -- INPUTS.json SEG.tsv GRIDS OFFSET [--show N]`
//! Always from the segment's start (the area's start).
//! `--free`: no placing on the client's log inside skills. The two models must then agree in every frame.
//! `OFFSET`: log frame minus capture frame of the segment, as the trace script prints it.

use poe_game::clientsim::ClientSim;
use poe_game::grid::Grid;
use poe_game::locomotion::Locomotion;
use poe_game::motion_data::hub_motion_or_panic;
use poe_game::movement::{
    ActionKind, Cell, Drain, Keyed, MoveEvent, MoveInput, MoveStat, MovementConfig, SkillIds,
    StepCtx, UseSkill, Walkable,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

const DT: f32 = 0.033;
const TRAVEL: f32 = 46.0;

fn i(v: &Value, k: &str) -> i32 {
    v[k].as_i64().unwrap_or_else(|| panic!("{k} in {v}")) as i32
}

fn input(v: &Value) -> Option<MoveInput> {
    Some(match v["op"].as_str().unwrap() {
        "wasd" if v["cursor"].as_bool() == Some(true) => MoveInput::CursorDirection { x: i(v, "x"), y: i(v, "y") },
        "wasd" => MoveInput::Direction { x: i(v, "x"), y: i(v, "y") },
        "use_skill" => MoveInput::UseSkill(UseSkill {
            skill: i(v, "skill") as u32,
            flags: i(v, "flags") as u16,
            unk: i(v, "unk") as u8,
            target_flags: i(v, "target_flags") as u8,
            entity: v.get("entity").map(|e| e.as_u64().unwrap() as u32),
            x: i(v, "x"),
            y: i(v, "y"),
        }),
        "update_target" => MoveInput::UpdateSkillTarget { x: i(v, "x"), y: i(v, "y"), skill: i(v, "skill") as u32 },
        "input_end" => MoveInput::SkillInputEnd,
        "sprint" => MoveInput::SprintInput { held: v["held"].as_bool().unwrap() },
        "released" => MoveInput::SkillReleased { skill: i(v, "skill") as u32, flag: i(v, "flag") as u8 },
        "stance_request" => MoveInput::StanceRequest { value: i(v, "value") as u8 },
        "aim_mode" => MoveInput::AimMode { mode: i(v, "mode") as u8 },
        _ => return None,
    })
}

/// What the client does with a message.
fn apply(sim: &mut ClientSim, e: &MoveEvent, t: &dyn Walkable, skills: &SkillIds) {
    match *e {
        MoveEvent::MoveAck { dir, cell } => sim.on_move_ack(dir, cell, t),
        MoveEvent::Teleport { cell } => {
            sim.on_move_ack((1, 0), cell, t);
            sim.on_move_ack((0, 0), cell, t);
        }
        MoveEvent::SprintState { held } => sim.on_sprint_key(held),
        MoveEvent::ActionStart { kind, cell, target, .. } => match kind {
            ActionKind::Roll => sim.on_start_roll(cell, target, TRAVEL, t),
            ActionKind::Move => sim.on_start_move(cell, target, t),
            ActionKind::Other => sim.on_start_other(cell, t),
        },
        MoveEvent::Stats(ref s) => {
            for &(stat, v) in s {
                match stat {
                    MoveStat::IsSprinting => sim.on_is_sprinting(v != 0),
                    MoveStat::MustSprint => sim.on_must_sprint(v != 0),
                    MoveStat::MovementVelocityPermyriad => sim.on_permyriad(v),
                    _ => {}
                }
            }
        }
        MoveEvent::TargetAck { skill, x, y } if skills.is_roll(skill) => sim.on_skill_target((x, y)),
        _ => {}
    }
}

/// Our frame on the client: each message at its key, the components' ticks in order.
fn client_frame(sim: &mut ClientSim, events: &[Keyed], t: &dyn Walkable, skills: &SkillIds, snaps: &mut Vec<(Cell, Cell)>) {
    let run = |sim: &mut ClientSim, pick: &dyn Fn(Drain) -> bool, snaps: &mut Vec<(Cell, Cell)>| {
        for k in events.iter().filter(|k| pick(k.key)) {
            if let MoveEvent::MoveAck { dir, cell } = k.event
                && dir != (0, 0)
                && cell != sim.grid()
            {
                snaps.push((sim.grid(), cell));
            }
            apply(sim, &k.event, t, skills);
        }
    };
    run(sim, &|k| k == Drain::Start, snaps);
    for comp in 0..=10u8 {
        // Later sequences of the Positioned component run after its tick.
        let after_tick = comp == 0;
        if !after_tick {
            run(sim, &|k| matches!(k, Drain::Component { comp: c, .. } if c == comp), snaps);
        }
        match comp {
            0 => sim.positioned_tick(DT, t),
            2 => sim.pathfinding_tick(DT, t),
            5 => sim.animated_tick(DT, t),
            9 => sim.actor_tick(DT, t),
            _ => {}
        }
        if after_tick {
            run(sim, &|k| matches!(k, Drain::Component { comp: 0, .. }), snaps);
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let show: i64 = a.iter().position(|x| x == "--show").map_or(12, |i| a[i + 1].parse().unwrap());
    let eps: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&a[1]).unwrap()).unwrap();
    let off: i64 = a[4].parse().unwrap();
    let grid = Grid::parse(&std::fs::read(&a[3]).unwrap()).expect("grid");
    let (curves, scale) = hub_motion_or_panic();
    let skills = SkillIds { move_skill: 0x2000_2909, roll_skill: 0x8000_0040, roll_skill_other_set: 0x8000_0041 };
    let mut cfg = MovementConfig::new(skills, [2758, 2737, 2853]);
    cfg.curves = Some(curves.clone());
    cfg.offset_scale = scale;
    cfg.sprint_clearance_level = 4;
    let resync = a.iter().any(|x| x == "--resync");
    // `--free`: never place a model on the log, also in skills: server and client models must still agree.
    let free = a.iter().any(|x| x == "--free");
    let mut rows: BTreeMap<i64, Vec<Vec<String>>> = BTreeMap::new();
    for l in std::fs::read_to_string(&a[2]).unwrap().lines() {
        let w: Vec<String> = l.split_whitespace().map(String::from).collect();
        rows.entry(w[0].parse().unwrap()).or_default().push(w[1..].to_vec());
    }
    let n = |s: &String| s.parse::<i32>().unwrap();
    let cell = |s: &String| {
        let (x, y) = s.split_once(',').unwrap();
        (x.parse::<i32>().unwrap(), y.parse::<i32>().unwrap())
    };
    let (first, last) = (*rows.keys().next().unwrap(), *rows.keys().last().unwrap());
    let mut shown = 0;
    let mut c: HashMap<&str, i64> = HashMap::new();
    {
        let logged_at = |f: i64| {
            rows.get(&f).and_then(|r| r.iter().find(|w| w[2] == "G")).map(|w| (n(&w[3]), n(&w[4])))
        };
        let start = (first..=last).find_map(logged_at).unwrap();
        let mut loco = Locomotion::new(start, &cfg);
        loco.set_stance(true);
        let mut client = ClientSim::new(start, cfg.walk_speed, Some(curves.clone()), scale);
        client.sprint_level = cfg.sprint_clearance_level;
        let (mut action, mut buff) = (32768_u16, 0_u16);
        let mut skill_until = -1_i64;
        // Nothing is compared before the log's first position.
        let mut started = false;
        for f in first..=last {
            let k = f - first + 1;
            for v in eps.iter().filter(|v| v["f"].as_i64() == Some(f - off)) {
                if let Some(m) = input(v) {
                    loco.input(m);
                }
            }
            // The plans the client logged hold entities our terrain does not have.
            let mut hints = Vec::new();
            for g in f - 2..=f + 2 {
                for r in rows.get(&g).into_iter().flatten().filter(|r| r[2] == "P").map(|r| &r[2..]) {
                    hints.push(((n(&r[3]), n(&r[4])), (n(&r[1]), n(&r[2])), r[5..].iter().map(&cell).collect()));
                }
            }
            loco.sim_mut().plan_hints.clone_from(&hints);
            client.plan_hints = hints;
            let mut ctx = StepCtx {
                frame: k as u64,
                dt: DT,
                terrain: &grid,
                config: &cfg,
                next_action: &mut action,
                next_buff: &mut buff,
            };
            let events = loco.tick(&mut ctx);
            let mut snaps = Vec::new();
            client_frame(&mut client, &events, &grid, &skills, &mut snaps);
            for (m, o) in snaps.iter().filter(|_| started) {
                *c.entry("snap").or_default() += 1;
                if shown < show {
                    shown += 1;
                    println!("f{f} OUR 0x64 SNAPS the client: client {m:?} message {o:?}");
                }
            }
            // The whole visible state: the first frames where it differs, with their messages.
            if started && free {
                let a = &*loco.sim_mut();
                let st = |m: &ClientSim| (m.grid(), m.point(), m.facing(), m.speed_factor(), m.is_rolling(), m.is_coasting(), m.has_move(), m.line_end());
                if st(a) != st(&client) && shown < show {
                    shown += 1;
                    println!("f{f} STATE\n  s {:?}\n  c {:?}\n  {:?}", st(a), st(&client), events.iter().map(|e| (e.key, format!("{:?}", e.event))).collect::<Vec<_>>());
                }
            }
            if started && client.grid() != loco.grid() {
                *c.entry("model_vs_client").or_default() += 1;
                if shown < show {
                    shown += 1;
                    println!("f{f} server model {:?} client model {:?} in_skill {}", loco.grid(), client.grid(), f < skill_until);
                }
            }
            if !started {
                if let Some(g) = logged_at(f) {
                    started = true;
                    if client.grid() != g {
                        client.place(g);
                    }
                    if loco.grid() != g {
                        loco.sim_mut().place(g);
                    }
                }
                continue;
            }
            let frame = rows.get(&f);
            // Our messages against the official ones.
            let mine: Vec<_> = events.iter().filter_map(|e| match e.event {
                MoveEvent::MoveAck { dir, cell } => Some((dir, cell)),
                _ => None,
            }).collect();
            let theirs: Vec<_> = frame.into_iter().flatten().filter(|w| w[2] == "M").map(|w| ((n(&w[3]), n(&w[4])), (n(&w[5]), n(&w[6])))).collect();
            if !mine.is_empty() || !theirs.is_empty() {
                let in_skill = f < skill_until;
                let key = if mine == theirs { "ack_same" } else if in_skill { "ack_skill" } else { "ack_differ" };
                *c.entry(key).or_default() += 1;
                if key == "ack_differ" && shown < show {
                    shown += 1;
                    println!("f{f} 0x64 ours {mine:?} official {theirs:?}");
                }
            }
            let (mine, theirs): (Vec<_>, Vec<_>) = (
                events.iter().filter_map(|e| match e.event {
                    MoveEvent::ActionStart { kind, cell, target, .. } => Some((kind == ActionKind::Roll, kind == ActionKind::Other, cell, target)),
                    _ => None,
                }).collect(),
                frame.into_iter().flatten().filter(|w| w[2] == "A").map(|w| (w[3] == "roll", w[3] == "other", (n(&w[4]), n(&w[5])), (n(&w[6]), n(&w[7])))).collect(),
            );
            if !mine.is_empty() || !theirs.is_empty() {
                let key = if mine == theirs { "start_same" } else if f < skill_until { "start_skill" } else { "start_differ" };
                *c.entry(key).or_default() += 1;
                if key == "start_differ" && shown < show {
                    shown += 1;
                    println!("f{f} 0x1c8 ours {mine:?} official {theirs:?}");
                }
            }
            // A skill the port does not move: the client's log leads, as in sim_replay.
            if theirs.iter().any(|t| t.1) || mine.iter().any(|t| t.1) {
                skill_until = f + 30;
            }
            if let Some(g) = logged_at(f) {
                if f < skill_until {
                    if !free {
                        client.place(g);
                        loco.sim_mut().place(g);
                    }
                    *c.entry("grid_in_skill").or_default() += 1;
                } else if client.grid() == g {
                    *c.entry("grid_same").or_default() += 1;
                } else {
                    *c.entry("grid_differ").or_default() += 1;
                    // `--resync`: every difference on its own, not what follows from an earlier one.
                    if resync {
                        client.place(g);
                        loco.sim_mut().place(g);
                    }
                    if shown < show {
                        shown += 1;
                        println!("f{f} client model {:?} official client {g:?}", client.grid());
                    }
                }
            }
        }
    }
    let mut keys: Vec<_> = c.into_iter().collect();
    keys.sort();
    println!("{keys:?}");
}
