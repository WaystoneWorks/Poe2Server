//! `plan_cmp SEG.tsv GRID.bin`: the pathfinder port against every `PATHPLAN` row of a replay segment
//! (`P` rows of `own_replay`'s input): counts, and the rows no clearance level reproduces.
use poe_game::{
    clientpath::{Params, plan},
    grid::Grid,
};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let grid = Grid::parse(&std::fs::read(&a[2]).unwrap()).expect("grid");
    let text = std::fs::read_to_string(&a[1]).unwrap();
    let mut line: Option<(f32, f32, (i32, i32))> = None;
    let (mut same, mut diff, mut same4) = (0, 0, 0);
    let show: usize = a.get(3).map_or(40, |s| s.parse().unwrap());
    for row in text.lines() {
        let r: Vec<&str> = row.split(' ').collect();
        if r.len() > 12 && r[3] == "L" {
            let i = |k: usize| r[k].parse::<i32>().unwrap();
            line = Some((r[10].parse().unwrap(), r[11].parse().unwrap(), (i(6), i(7))));
        }
        if r.len() > 8 && r[3] == "P" {
            let i = |k: usize| r[k].parse::<i32>().unwrap();
            let (goal, start) = ((i(4), i(5)), (i(6), i(7)));
            let want: Vec<(i32, i32)> = r[8..]
                .iter()
                .map(|p| {
                    let (x, y) = p.split_once(',').unwrap();
                    (x.parse().unwrap(), y.parse().unwrap())
                })
                .collect();
            let moving = line.filter(|l| l.2 == start).map(|l| (l.0, l.1));
            let got = |lvl: i32, m: Option<(f32, f32)>| plan(&grid, start, goal, lvl, &Params { moving: m, ..Params::default() }).path;
            let ok0 = [moving, None].iter().any(|m| got(0, *m) == want);
            let ok4 = [moving, None].iter().any(|m| got(4, *m) == want);
            if ok0 {
                same += 1;
            } else if ok4 {
                same4 += 1;
            } else {
                diff += 1;
                if diff <= show {
                    println!("f{} goal {:?} start {:?} moving {:?}\n  client {:?}\n  port   {:?}", r[0], goal, start, moving, want, got(0, moving));
                }
            }
        }
    }
    println!("level0 {same} level4 {same4} neither {diff}");
}
