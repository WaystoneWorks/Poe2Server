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
    let start = rows.values().flatten().find(|r| r[0] == "G").map(|r| (n(&r[1]), n(&r[2]))).unwrap();
    let mut sim = ClientSim::new(start, 37.0, Some(curves), scale);
    let (first, last) = (*rows.keys().next().unwrap(), *rows.keys().last().unwrap());
    let (mut same, mut diff, mut snaps, mut acks, mut shown) = (0, 0, 0, 0, 0);
    let mut logged = start;
    let mut lines = (0, 0);
    let mut pct = 0;
    let cell = |s: &String| {
        let (x, y) = s.split_once(',').unwrap();
        (x.parse::<i32>().unwrap(), y.parse::<i32>().unwrap())
    };
    for f in first..=last {
        sim.plan_hints.clear();
        for g in f - 2..=f + 2 {
            for r in rows.get(&g).into_iter().flatten().filter(|r| r[0] == "P") {
                let path = r[5..].iter().map(&cell).collect();
                sim.plan_hints.push(((n(&r[3]), n(&r[4])), (n(&r[1]), n(&r[2])), path));
            }
        }
        sim.frame_start(DT, &grid);
        let empty = Vec::new();
        let frame = rows.get(&f).unwrap_or(&empty);
        for r in frame {
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
                    "permyriad" => {
                        pct = n(&r[2]);
                        sim.on_permyriad(pct);
                    }
                    _ => {}
                },
                "K" => sim.on_sprint_key(r[1] != "0"),
                _ => {}
            }
        }
        sim.actor_tick(DT, &grid);
        let mut seen = false;
        for r in frame {
            match r[0].as_str() {
                "G" => {
                    logged = (n(&r[1]), n(&r[2]));
                    seen = true;
                }
                "L" => {
                    let e = (n(&r[5]), n(&r[6]));
                    if sim.line_end() == Some(e) { lines.0 += 1 } else { lines.1 += 1 }
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
    let _ = pct;
    println!(
        "frames {first}..{last}: grid same {same} differ {diff}; line ends same {} differ {}; 0x64 {acks}, would snap {snaps}",
        lines.0, lines.1
    );
}
