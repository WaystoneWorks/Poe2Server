//! Offline check of `clientsim` against an official capture: replays what the official server sent
//! (`scripts/re/movement_replay_trace.py`) and compares the model's grid with the client's logged
//! grid per frame; a `0x64` cell the model does not hold would snap our client.
//!
//! `cargo run -p poe-game --example sim_replay -- TRACE GRIDS [--from F] [--to F] [--show N]
//! [--objects FILE] [--no-hints] [--first]`
//! Private sessions (`scripts/re/first_divergence.py`): `--objects` adds the area's blocking
//! objects, `--no-hints` drops the client's logged plans (the search runs
//! on its own), `--first` stops at the first frame whose grid differs and prints that frame's
//! context.

use poe_game::clientsim::ClientSim;
use poe_game::clientpath::Obstacle;
use poe_game::grid::Grid;
use poe_game::movement::{Walkable, WithObstacles};
use poe_game::motion_data::hub_motion_or_panic;
use std::collections::BTreeMap;

const DT: f32 = 0.033;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let opt = |k: &str, d: i64| {
        a.iter().position(|x| x == k).map_or(d, |i| a[i + 1].parse().unwrap())
    };
    let (from, to, show) = (opt("--from", 0), opt("--to", i64::MAX), opt("--show", 12));
    let grid = Grid::parse(&std::fs::read(&a[2]).unwrap()).expect("grid");
    // The area's objects (`x y reach targeting obstacle type`, `instance-server/examples/replay_terrain.rs`):
    // the blocking ones.
    let obstacles: Vec<Obstacle> = a.iter().position(|x| x == "--objects").map_or_else(Vec::new, |i| {
        std::fs::read_to_string(&a[i + 1]).unwrap().lines().filter_map(|l| {
            let v: Vec<i32> = l.split_whitespace().take(5).map(|w| w.parse().unwrap()).collect();
            (v[4] >= 0).then_some(Obstacle { cell: (v[0], v[1]), size: v[4] })
        }).collect()
    });
    let terrain = WithObstacles { terrain: &grid, obstacles: &obstacles };
    let tw: &dyn Walkable = &terrain;
    let hints = !a.iter().any(|x| x == "--no-hints");
    let first_only = a.iter().any(|x| x == "--first");
    let (curves, scale) = hub_motion_or_panic();
    let mut rows: BTreeMap<i64, Vec<Vec<String>>> = BTreeMap::new();
    for l in std::fs::read_to_string(&a[1]).unwrap().lines() {
        let w: Vec<String> = l.split_whitespace().map(String::from).collect();
        let f: i64 = w[0].parse().unwrap();
        if f >= from && f <= to {
            rows.entry(f).or_default().push(w[1..].to_vec());
        }
    }
    let n = |s: &String| s.parse::<i32>().unwrap();
    let start = rows.values().flatten().find(|r| r[2] == "G").map(|r| (n(&r[3]), n(&r[4]))).unwrap();
    let mut sim = ClientSim::new(start, 37.0, Some(curves), scale);
    let (first, last) = (*rows.keys().next().unwrap(), *rows.keys().last().unwrap());
    let (mut same, mut diff, mut snaps, mut acks, mut shown) = (0, 0, 0, 0, 0);
    let mut logged = start;
    let mut lines = (0, 0);
    let cell = |s: &String| {
        let (x, y) = s.split_once(',').unwrap();
        (x.parse::<i32>().unwrap(), y.parse::<i32>().unwrap())
    };
    let mut drift = 0;
    let mut starts = 0;
    let mut client_snaps = 0;
    let mut first_div: Option<i64> = None;
    let dist = |a: (i32, i32), b: (i32, i32)| (((a.0 - b.0).pow(2) + (a.1 - b.1).pow(2)) as f32).sqrt();
    // A skill the port does not move (out of scope, the server sends none yet): while it and its
    // recovery run (a Move then walks slowed, Actor `+0x430`), the replay takes the client's logged
    // cell, until the server's next roll or 30 frames after the skill's start.
    let mut in_skill = false;
    let mut skill_start = -1_000_000_i64;
    let mut skill_frames = 0;
    for f in first..=last {
        let f_ = f;
        sim.plan_hints.clear();
        for g in (f - 2..=f + 2).filter(|_| hints) {
            for r in rows.get(&g).into_iter().flatten().filter(|r| r[2] == "P").map(|r| &r[2..]) {
                let path = r[5..].iter().map(&cell).collect();
                sim.plan_hints.push(((n(&r[3]), n(&r[4])), (n(&r[1]), n(&r[2])), path));
            }
        }
        let empty = Vec::new();
        let frame = rows.get(&f).unwrap_or(&empty);
        // The drain positions: -1 before all entities, then each component before its tick.
        for comp in [-1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10] {
            for w in frame.iter().filter(|w| w[0] == comp.to_string()) {
                let r = &w[2..];
                match r[0].as_str() {
                    "M" => {
                        let (dir, cell) = ((n(&r[1]), n(&r[2])), (n(&r[3]), n(&r[4])));
                        if dir != (0, 0) {
                            acks += 1;
                            if cell != sim.grid() && !in_skill {
                                // The client's line restarts on the integer cell when it snapped too.
                                let both = frame.iter().map(|w| &w[2..]).any(|r| {
                                    r[0] == "L" && r[7].ends_with(".0000") && r[8].ends_with(".0000") && (n(&r[1]), n(&r[2])) == cell
                                });
                                snaps += 1;
                                if both {
                                    client_snaps += 1;
                                }
                                if shown < show {
                                    shown += 1;
                                    println!("f{f} SNAP model {:?} 0x64 {:?} dir {:?} client also {both}", sim.grid(), cell, dir);
                                }
                            }
                        }
                        sim.on_move_ack(dir, cell, tw);
                    }
                    "A" => {
                        let (s, t) = ((n(&r[2]), n(&r[3])), (n(&r[4]), n(&r[5])));
                        // The server's own position: what its model must hold when it starts an action.
                        if s != sim.grid() {
                            starts += 1;
                            if a.iter().any(|x| x == "--starts") {
                                println!("f{f} A {} server start {s:?} model {:?} |{:.1}|", r[1], sim.grid(), (((s.0 - sim.grid().0).pow(2) + (s.1 - sim.grid().1).pow(2)) as f32).sqrt());
                            }
                        }
                        match r[1].as_str() {
                            "move" => {
                                in_skill = f - skill_start < 30;
                                sim.on_start_move(s, t, tw)
                            }
                            "interact" => {
                                in_skill = false;
                                sim.on_start_interaction(s, tw)
                            }
                            "roll" => {
                                in_skill = false;
                                sim.on_start_roll(s, t, 46.0, tw)
                            }
                            _ => {
                                in_skill = true;
                                skill_start = f;
                                sim.on_start_other(s, tw)
                            }
                        }
                    }
                    "S" => match r[1].as_str() {
                        "is_sprinting" => sim.on_is_sprinting(r[2] != "0"),
                        "must_sprint" => sim.on_must_sprint(r[2] != "0"),
                        "permyriad" => sim.on_permyriad(n(&r[2])),
                        _ => {}
                    },
                    "K" => sim.on_sprint_key(r[1] != "0"),
                    "T" => sim.on_skill_target((n(&r[1]), n(&r[2]))),
                    _ => {}
                }
            }
            match comp {
                0 => sim.positioned_tick(DT, tw),
                2 => sim.pathfinding_tick(DT, tw),
                5 => sim.animated_tick(DT, tw),
                9 => sim.actor_tick(DT, tw),
                _ => {}
            }
        }
        let mut seen = false;
        for r in frame.iter().map(|w| &w[2..]) {
            if in_skill && r[0] == "L" {
                continue;
            }
            match r[0].as_str() {
                "G" => {
                    logged = (n(&r[1]), n(&r[2]));
                    seen = true;
                }
                "L" => {
                    let e = (n(&r[5]), n(&r[6]));
                    if sim.line_end() == Some(e) {
                        lines.0 += 1;
                        // The line's float state: a drift here changes a later rounding.
                        let f = |s: &String| s.parse::<f32>().unwrap();
                        let (p, len, acc) = sim.line_state();
                        let d = (p.0 - f(&r[7])).abs().max((p.1 - f(&r[8])).abs()).max((len - f(&r[9])).abs()).max((acc - f(&r[10])).abs());
                        if d > 0.002 {
                            drift += 1;
                            if a.iter().any(|x| x == "--drift") && shown < show {
                                shown += 1;
                                println!("f{f_} drift p {p:?} len {len} acc {acc} client {} {} {} {}", r[7], r[8], r[9], r[10], f_ = f_);
                            }
                        }
                    } else {
                        lines.1 += 1;
                        if shown < show {
                            shown += 1;
                            println!("f{f} line end model {:?} client {e:?} grid {:?}", sim.line_end(), sim.grid());
                        }
                    }
                }
                _ => {}
            }
        }
        if in_skill {
            skill_frames += 1;
            if seen {
                sim.place(logged);
            }
            continue;
        }
        // The log skips some frames: only a logged grid counts.
        if seen {
            if sim.grid() == logged {
                same += 1;
            } else {
                diff += 1;
                first_div.get_or_insert(f);
                if first_only {
                    println!("FIRST DIVERGENCE f{f}: model {:?} client {:?} (|{:.1}| cells); model line end {:?} axes {:?} roll {} coast {}", sim.grid(), logged, dist(sim.grid(), logged), sim.line_end(), sim.axes(), sim.is_rolling(), sim.is_coasting());
                    println!("rows of frames f{}..f{f} (frame drain-slot order kind args; G logged grid, L client line, P client plan, M 0x64, A 0x1c8, S stat, K sprint key, T target, I client 0x63):", f - 6);
                    for g in f - 6..=f {
                        for w in rows.get(&g).into_iter().flatten() {
                            println!("  f{g} {}", w.join(" "));
                        }
                    }
                    break;
                }
                if shown < show {
                    shown += 1;
                    println!("f{f} model {:?} client {:?} end {:?} axes {:?} roll {} coast {}", sim.grid(), logged, sim.line_end(), sim.axes(), sim.is_rolling(), sim.is_coasting());
                }
            }
        }
    }
    println!(
        "first divergence {}; frames {first}..{last}: grid same {same} differ {diff}; line ends same {} differ {} (float drift on {drift}); 0x64 {acks}, would snap {snaps} (client too {client_snaps}); action starts off {starts}; frames in a skill (not compared) {skill_frames}",
        first_div.map_or("none".to_string(), |f| format!("f{f}")),
        lines.0, lines.1
    );
}
