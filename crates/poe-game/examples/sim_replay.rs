//! Offline check of `clientsim` against an official capture: replays what the official server sent
//! (`scripts/re/movement_replay_trace.py`) and compares the model's grid with the client's logged
//! grid per frame; a `0x64` cell the model does not hold would snap our client.
//!
//! `cargo run -p poe-game --example sim_replay -- TRACE GRIDS [--from F] [--to F] [--show N]`

use poe_game::clientsim::ClientSim;
use poe_game::grid::Grid;
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
    for f in first..=last {
        let f_ = f;
        sim.plan_hints.clear();
        for g in f - 2..=f + 2 {
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
                            if cell != sim.grid() {
                                snaps += 1;
                                if shown < show {
                                    shown += 1;
                                    println!("f{f} SNAP model {:?} 0x64 {:?} dir {:?}", sim.grid(), cell, dir);
                                }
                            }
                        }
                        sim.on_move_ack(dir, cell, &grid);
                    }
                    "A" => {
                        let (s, t) = ((n(&r[2]), n(&r[3])), (n(&r[4]), n(&r[5])));
                        match r[1].as_str() {
                            "move" => sim.on_start_move(s, t, &grid),
                            "roll" => sim.on_start_roll(s, t, 46.0, &grid),
                            _ => sim.on_start_other(s),
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
                0 => sim.positioned_tick(DT, &grid),
                2 => sim.pathfinding_tick(DT, &grid),
                5 => sim.animated_tick(DT, &grid),
                9 => sim.actor_tick(DT, &grid),
                _ => {}
            }
        }
        if a.iter().any(|x| x == "--state") {
            println!("f{f} grid {:?} {}", sim.grid(), sim.debug_state());
        }
        let mut seen = false;
        for r in frame.iter().map(|w| &w[2..]) {
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
        if seen || sim.grid() != logged {
            if sim.grid() == logged {
                same += 1;
            } else {
                diff += 1;
                if shown < show {
                    shown += 1;
                    println!("f{f} model {:?} client {:?} end {:?} axes {:?} roll {} coast {}", sim.grid(), logged, sim.line_end(), sim.axes(), sim.is_rolling(), sim.is_coasting());
                }
            }
        }
    }
    println!(
        "frames {first}..{last}: grid same {same} differ {diff}; line ends same {} differ {} (float drift on {drift}); 0x64 {acks}, would snap {snaps}",
        lines.0, lines.1
    );
}
