//! Rules no captured fixture covers, on the [`World`] with the port (`ClientModel`): walls,
//! click paths around them, teleports, weapon swaps, entity targets. Restored from the unit tests
//! of the movement before the restart where the rule still holds.

use std::sync::Arc;

use poe_game::{
    grid::{GridSource, VecGrid},
    locomotion::ClientModel,
    movement::{ActionKind, MoveEvent, MoveInput, MovementConfig, Open, SkillIds, UseSkill},
    world::{EntityId, World, WorldConfig},
};

const MOVE: u32 = 0x2000_2909;
const ROLL: u32 = 0x8000_0040;
const ATTACK: u32 = 0x8040_0040;

fn world(grid: Arc<dyn GridSource>, cell: (i32, i32)) -> (World, EntityId) {
    let mut cfg = MovementConfig::new(
        SkillIds {
            move_skill: MOVE,
            roll_skill: ROLL,
            roll_skill_other_set: u32::MAX,
        },
        [100, 101, 102],
    );
    let (curves, offset_scale) = poe_game::motion_data::hub_motion_or_panic();
    cfg.curves = Some(curves);
    cfg.offset_scale = offset_scale;
    let mut w = World::new(WorldConfig::new(cfg), Arc::new(ClientModel), grid);
    let p = w.spawn_player(cell);
    w.entity_mut(p).unwrap().movement.set_stance(false);
    (w, p)
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

fn tick(w: &mut World) -> Vec<MoveEvent> {
    w.tick().into_iter().filter_map(|e| e.movement().cloned()).collect()
}

fn cell(w: &World, p: EntityId) -> (i32, i32) {
    w.entity(p).unwrap().movement.cell()
}

#[test]
fn walking_stops_at_a_wall() {
    let grid = VecGrid::from_rows(&["..........", "..........", "##########"]);
    let (mut w, p) = world(Arc::new(grid), (2, 1));
    w.input(p, MoveInput::Direction { x: 0, y: 180 });
    for _ in 0..20 {
        tick(&mut w);
        assert_eq!(cell(&w, p).1, 1, "into the wall");
    }
}

#[test]
fn click_to_move_walks_around_a_wall_and_stops() {
    let grid = VecGrid::from_rows(&[
        "........#...........",
        "........#...........",
        "........#...........",
        "........#...........",
        "....................",
    ]);
    let (mut w, p) = world(Arc::new(grid), (1, 0));
    w.input(p, MoveInput::UseSkill(skill(MOVE, 14, 0)));
    let ev = tick(&mut w);
    assert!(
        ev.iter().any(|e| matches!(
            e,
            MoveEvent::ActionStart {
                kind: ActionKind::Move,
                cell: (1, 0),
                target: (15, 0),
                ..
            }
        )),
        "{ev:?}"
    );
    assert!(ev.iter().any(|e| matches!(e, MoveEvent::SkillAck { x: 15, y: 0, .. })), "{ev:?}");
    let mut went_down = false;
    for _ in 0..120 {
        tick(&mut w);
        went_down |= cell(&w, p).1 >= 3;
        let (x, y) = cell(&w, p);
        assert!(x != 8 || y == 4, "through the wall at {:?}", (x, y));
        if cell(&w, p) == (15, 0) {
            break;
        }
    }
    assert!(went_down);
    assert_eq!(cell(&w, p), (15, 0));
    for _ in 0..10 {
        tick(&mut w);
    }
    assert_eq!(cell(&w, p), (15, 0), "it stops");
}

/// The Move action starts only 9 cells or more from its target (`+0x1f0`, `0x1419850f0`): the
/// client stands still for a closer click, and so does the server.
#[test]
fn a_click_closer_than_nine_cells_does_not_walk() {
    for (d, walks) in [(8, false), (9, true)] {
        let (mut w, p) = world(Arc::new(Open), (50, 50));
        w.input(p, MoveInput::UseSkill(skill(MOVE, d, 0)));
        for _ in 0..30 {
            tick(&mut w);
        }
        assert_eq!(cell(&w, p) == (50 + d, 50), walks, "{d}: {:?}", cell(&w, p));
    }
}

#[test]
fn teleport_places_and_reports() {
    let (mut w, p) = world(Arc::new(Open), (10, 10));
    w.input(p, MoveInput::Direction { x: 180, y: 0 });
    tick(&mut w);
    w.teleport(p, (200, 300));
    let ev = tick(&mut w);
    assert!(ev.contains(&MoveEvent::Teleport { cell: (200, 300) }), "{ev:?}");
    assert_eq!(cell(&w, p).1, 300);
}

#[test]
fn a_weapon_swap_is_answered_in_the_same_batch() {
    // 2026-10-20_18-41-14: 0xfe {1, 1} and {1, 0} (standing) -> 0x1d5 + 0xff in r+0.
    let (mut w, p) = world(Arc::new(Open), (50, 50));
    tick(&mut w);
    w.input(p, MoveInput::WeaponSwap { set: 1 });
    assert_eq!(tick(&mut w), vec![MoveEvent::WeaponSet { set: 1, requested: true }]);
    w.input(p, MoveInput::WeaponSwap { set: 0 });
    assert_eq!(tick(&mut w), vec![MoveEvent::WeaponSet { set: 0, requested: true }]);
    assert_eq!(tick(&mut w), vec![]);
}

#[test]
fn entity_target_skills_use_absolute_cells() {
    let (mut w, p) = world(Arc::new(Open), (10, 10));
    tick(&mut w);
    let mut s = skill(ATTACK, 40, 41);
    s.entity = Some(77);
    s.target_flags = 8;
    w.input(p, MoveInput::UseSkill(s));
    let ev = tick(&mut w);
    assert!(
        ev.iter().any(|e| matches!(
            e,
            MoveEvent::ActionStart {
                kind: ActionKind::Other,
                target: (40, 41),
                entity: Some(77),
                ..
            }
        )),
        "{ev:?}"
    );
    assert!(
        ev.iter().any(|e| matches!(
            e,
            MoveEvent::SkillAck {
                x: 40,
                y: 41,
                entity: Some(77),
                target_flags: 8,
                ..
            }
        )),
        "{ev:?}"
    );
}

