//! The pathfinder port against the paths the client planned (`path_log`, `PATHPLAN` lines of
//! the DLL's log, fixture `tests/fixtures/clientpath_plans.json`: start, goal, parameters, the
//! entity's clearance level and the waypoints the client returned).
//!
//! The terrain is the client's own cell grid of the test area (`terrain_grid_<tile>.bin`,
//! exported by the DLL into the git-ignored `dumps/`), so the test skips when the file is not
//! there.

use poe_game::{
    clientpath::{Params, plan},
    grid::Grid,
};
use serde_json::Value;
use std::path::PathBuf;

/// The grid from `dumps/` of this repo or, in the hub checkout (`modules/server`), the hub's.
fn grid(tile: u64) -> Option<Grid> {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let name = format!("terrain_grid_{tile}.bin");
    ["../..", "../../../.."]
        .iter()
        .flat_map(|r| {
            ["terrain/grids-keep", "terrain/grids"].map(|d| {
                here.join(r)
                    .join("dumps/0.5.5f")
                    .join(d)
                    .join(&name)
            })
        })
        .find_map(|p| std::fs::read(p).ok())
        .and_then(|d| Grid::parse(&d))
}

fn cell(v: &Value) -> (i32, i32) {
    (v[0].as_i64().unwrap() as i32, v[1].as_i64().unwrap() as i32)
}

#[test]
fn the_port_plans_the_same_paths_as_the_client() {
    let fx: Value = serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/tests/fixtures/clientpath_plans.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    )
    .unwrap();
    let Some(g) = grid(fx["tile"].as_u64().unwrap()) else {
        eprintln!("skipped: the test area's cell grid is not in dumps/");
        return;
    };
    let plans = fx["plans"].as_array().unwrap();
    let (mut exact, mut deviating) = (0, 0);
    let mut bad = Vec::new();
    for p in plans {
        let params = Params {
            min_dist: p["min"].as_u64().unwrap() as u32,
            max_dist: p["max"].as_u64().unwrap() as u32,
            radius: p["radius"].as_i64().unwrap() as i8,
            ..Params::default()
        };
        let got = plan(
            &g,
            cell(&p["start"]),
            cell(&p["goal"]),
            p["level"].as_i64().unwrap() as i32,
            &params,
        );
        let want: Vec<_> = p["path"].as_array().unwrap().iter().map(cell).collect();
        if got.path == want {
            exact += 1;
        } else if p["note"] == "midline" {
            // Planned while moving along a line, which this fixture did not log (the moving test
            // needs the line point: `plans_while_moving_test_sight_from_the_exact_point`).
            deviating += 1;
            assert_eq!(got.path.first(), want.first(), "{p}");
        } else {
            bad.push(format!("{p} -> {:?}", got.path));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} plans differ:\n{}",
        bad.len(),
        plans.len(),
        bad.join("\n")
    );
    eprintln!("{exact} plans exact, {deviating} with the known deviation");
    assert!(exact >= plans.len() - 8);
}

/// Plans made while the entity moves along a line (fixture `clientpath_moving_plans.json`, a
/// private session with the line state logged): the line-of-sight tests that end at the start
/// run from the entity's exact point on its line (`moving`), which the plain test gets wrong in
/// a few plans.
#[test]
fn plans_while_moving_test_sight_from_the_exact_point() {
    let fx: Value = serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/tests/fixtures/clientpath_moving_plans.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    )
    .unwrap();
    let Some(g) = grid(fx["tile"].as_u64().unwrap()) else {
        eprintln!("skipped: the test area's cell grid is not in dumps/");
        return;
    };
    let plans = fx["plans"].as_array().unwrap();
    let (mut bad, mut plain_wrong) = (Vec::new(), 0);
    for p in plans {
        let moving = p["moving"]
            .as_array()
            .map(|v| (v[0].as_f64().unwrap() as f32, v[1].as_f64().unwrap() as f32));
        let params = Params {
            min_dist: p["min"].as_u64().unwrap() as u32,
            max_dist: p["max"].as_u64().unwrap() as u32,
            radius: p["radius"].as_i64().unwrap() as i8,
            moving,
            size_level: None,
        };
        let (start, goal, level) = (
            cell(&p["start"]),
            cell(&p["goal"]),
            p["level"].as_i64().unwrap() as i32,
        );
        let want: Vec<_> = p["path"].as_array().unwrap().iter().map(cell).collect();
        let got = plan(&g, start, goal, level, &params);
        if got.path != want {
            bad.push(format!("{p} -> {:?}", got.path));
        }
        let plain = Params {
            moving: None,
            ..params
        };
        if plan(&g, start, goal, level, &plain).path != want {
            plain_wrong += 1;
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} plans differ:\n{}",
        bad.len(),
        plans.len(),
        bad.join("\n")
    );
    eprintln!(
        "{} plans exact; {plain_wrong} would differ without the moving test",
        plans.len()
    );
    assert!(
        plain_wrong > 0,
        "the fixture no longer exercises the moving test"
    );
}
