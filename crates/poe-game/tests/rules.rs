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
fn a_click_closer_than_three_cells_does_not_walk() {
    // vf `+0x1f0` `0x1419850f0` compares the squared length (`0x140163990`) with 9.
    for (d, walks) in [(2, false), (3, true)] {
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


/// The sprint end's clip values come from the game data (`base_sprint.atl`, `Character.act`).
#[test]
fn sprint_end_values_are_read_from_the_game_data() {
    let (curves, _) = poe_game::motion_data::hub_motion_or_panic();
    assert_eq!((curves.sprint_end_move_time, curves.sprint_end_base_speed), (0.75, 600.0));
}

/// A sprint that meets a wall at an angle ends with the Bonk clip, whose MoveEvent coasts along
/// the wall to the clip's end (game test `sprint-turn` case 4, 2026-11-03: the client coasted 6
/// cells along the wall; the clipped coast of a head-on wall is shorter than 2 cells: no move).
#[test]
fn a_sprint_bonk_at_an_angle_coasts_along_the_wall() {
    // Open for x 5..295, y 5..140; the start is 40 cells from the wall at y 140.
    let rows: Vec<String> = (0..200)
        .map(|y| (0..300).map(|x| if (5..295).contains(&x) && (5..140).contains(&y) { '.' } else { '#' }).collect())
        .collect();
    let rows: Vec<&str> = rows.iter().map(|r| r.as_str()).collect();
    let (mut w, p) = world(Arc::new(VecGrid::from_rows(&rows)), (150, 100));
    w.input(p, MoveInput::Direction { x: 127, y: 127 });
    tick(&mut w);
    w.input(p, MoveInput::SprintInput { held: true });
    w.input(p, MoveInput::UseSkill(skill(ROLL, 0, 0)));
    let mut cells = Vec::new();
    for _ in 0..150 {
        tick(&mut w);
        cells.push(cell(&w, p));
    }
    // The sprint slides along the wall at y 139 to the corner, stops (Bonk) and coasts on to the
    // clip's end at the last walkable cell.
    let stop = cells.windows(2).position(|c| c[0] == c[1] && c[0].0 >= 280).expect("the sprint stops at the corner");
    let (at, last) = (cells[stop], *cells.last().unwrap());
    assert_eq!(last, (294, 139), "coasted to the clip's end");
    assert!(last.0 - at.0 >= 2, "a coast after the Bonk at {at:?}");
}

/// A roll that starts while the SprintEnd clip plays keeps the sprint's facing (no turn to the
/// target), so rolling back against the sprint is a backward roll (`_bwd_` curve): user test 2
/// f2802 and seg1 f3578 (first roll frame 3.706 cells; forward: 3.732).
#[test]
fn a_roll_out_of_the_sprint_end_against_the_sprint_is_backward() {
    let rows: Vec<String> = (0..400).map(|_| ".".repeat(300)).collect();
    let rows: Vec<&str> = rows.iter().map(|r| r.as_str()).collect();
    let (mut w, p) = world(Arc::new(VecGrid::from_rows(&rows)), (150, 60));
    w.input(p, MoveInput::Direction { x: 0, y: 180 });
    tick(&mut w);
    w.input(p, MoveInput::SprintInput { held: true });
    w.input(p, MoveInput::UseSkill(skill(ROLL, 0, 20)));
    for _ in 0..60 {
        tick(&mut w);
    }
    w.input(p, MoveInput::SprintInput { held: false });
    w.input(p, MoveInput::SkillInputEnd);
    tick(&mut w);
    tick(&mut w);
    // Keys and cursor both back up the screen, against the sprint.
    w.input(p, MoveInput::Direction { x: 0, y: -180 });
    tick(&mut w);
    w.input(p, MoveInput::SprintInput { held: true });
    w.input(p, MoveInput::UseSkill(skill(ROLL, 5, -16)));
    let pos = |w: &World| w.entity(p).unwrap().movement.position();
    let mut steps = Vec::new();
    let mut last = pos(&w);
    for _ in 0..3 {
        tick(&mut w);
        let now = pos(&w);
        steps.push(((now.0 - last.0).powi(2) + (now.1 - last.1).powi(2)).sqrt());
        last = now;
    }
    // The roll's first frame also carries the coast's last step; its next two are the curve's
    // (the client's, user test 2 f2803-2804: 3.5289, 3.3449; forward: 3.5283, 3.3310).
    assert!((steps[1] - 3.5289).abs() < 0.002 && (steps[2] - 3.3449).abs() < 0.002, "roll steps {steps:?}: not the backward curve");
}

/// Where a roll has got to after `frames`, with the key released and pressed again at the given
/// frames of the roll (`None`: not at all).
fn roll_with_key(release: Option<u32>, press: Option<u32>, frames: u32) -> (f32, f32) {
    let rows: Vec<String> = (0..400).map(|_| ".".repeat(300)).collect();
    let rows: Vec<&str> = rows.iter().map(|r| r.as_str()).collect();
    let (mut w, p) = world(Arc::new(VecGrid::from_rows(&rows)), (150, 60));
    w.input(p, MoveInput::SprintInput { held: true });
    w.input(p, MoveInput::UseSkill(skill(ROLL, 0, 20)));
    for f in 0..frames {
        if release == Some(f) {
            w.input(p, MoveInput::SprintInput { held: false });
            w.input(p, MoveInput::SkillInputEnd);
        }
        if press == Some(f) {
            w.input(p, MoveInput::SprintInput { held: true });
        }
        tick(&mut w);
    }
    w.entity(p).unwrap().movement.position()
}

/// Actor `+0x915` (user test 3 f1824-1837): the sprint switch at the roll's event needs the key held
/// since the roll's start. A release and a new press before the event do not count as held.
#[test]
fn a_key_pressed_again_during_a_roll_does_not_make_it_sprint() {
    const FRAMES: u32 = 26; // past the event (0.5 s), before the clip's end
    let held = roll_with_key(None, None, FRAMES);
    let released = roll_with_key(Some(3), None, FRAMES);
    let again = roll_with_key(Some(3), Some(6), FRAMES);
    assert_ne!(held, released, "holding the key must switch to the sprint curve");
    assert_eq!(again, released, "a press during the roll must not switch to the sprint curve");
}

/// `act+0x461` is not set for a roll out of the SprintEnd clip (user test 3 f995-1002): a backward roll
/// that goes on sprinting stays backward after the event, so reversing it turns the other way than
/// a roll that turned around at the event: facing north, moving south, the keys north: through the west.
#[test]
fn a_backward_roll_out_of_the_sprint_end_stays_backward_after_the_event() {
    let rows: Vec<String> = (0..400).map(|_| ".".repeat(300)).collect();
    let rows: Vec<&str> = rows.iter().map(|r| r.as_str()).collect();
    let (mut w, p) = world(Arc::new(VecGrid::from_rows(&rows)), (150, 300));
    w.input(p, MoveInput::Direction { x: 0, y: -180 });
    tick(&mut w);
    w.input(p, MoveInput::SprintInput { held: true });
    w.input(p, MoveInput::UseSkill(skill(ROLL, 0, -20)));
    for _ in 0..60 {
        tick(&mut w);
    }
    w.input(p, MoveInput::SprintInput { held: false });
    w.input(p, MoveInput::SkillInputEnd);
    tick(&mut w);
    tick(&mut w);
    w.input(p, MoveInput::Direction { x: 0, y: 180 });
    tick(&mut w);
    w.input(p, MoveInput::SprintInput { held: true });
    w.input(p, MoveInput::UseSkill(skill(ROLL, 0, 20)));
    for _ in 0..20 {
        tick(&mut w);
    }
    let pos = |w: &World| w.entity(p).unwrap().movement.position();
    let before = pos(&w);
    w.input(p, MoveInput::Direction { x: 0, y: -180 });
    tick(&mut w);
    tick(&mut w);
    let now = pos(&w);
    assert!(now.0 < before.0 - 0.1, "reversal from {before:?} to {now:?}: must turn through the west");
}

/// `0x14197e160` (user test 4 f2319): a roll queued into a running roll does not face its target
/// first, so keys along the running roll with the cursor behind give a forward roll, not a backward one.
#[test]
fn a_roll_queued_into_a_roll_keeps_the_facing() {
    let rows: Vec<String> = (0..400).map(|_| ".".repeat(300)).collect();
    let rows: Vec<&str> = rows.iter().map(|r| r.as_str()).collect();
    let (mut w, p) = world(Arc::new(VecGrid::from_rows(&rows)), (150, 350));
    w.input(p, MoveInput::Direction { x: 0, y: -180 });
    tick(&mut w);
    w.input(p, MoveInput::SprintInput { held: true });
    w.input(p, MoveInput::UseSkill(skill(ROLL, 0, -20)));
    for _ in 0..3 {
        tick(&mut w);
    }
    w.input(p, MoveInput::SprintInput { held: false });
    w.input(p, MoveInput::SkillInputEnd);
    tick(&mut w);
    // Pressed again during the roll, with the cursor behind the player: it waits for the roll's end.
    w.input(p, MoveInput::SprintInput { held: true });
    w.input(p, MoveInput::UseSkill(skill(ROLL, 0, 20)));
    let pos = |w: &World| w.entity(p).unwrap().movement.position();
    let mut steps = Vec::new();
    let mut last = pos(&w);
    for _ in 0..45 {
        tick(&mut w);
        let now = pos(&w);
        steps.push(((now.0 - last.0).powi(2) + (now.1 - last.1).powi(2)).sqrt());
        last = now;
    }
    // The client's first steps of a queued roll (user test 4 f2320, f2321; test 3 f1852, f1853 agree):
    // the forward curve, with the new clip at 0.023 s in the tick that plays the old clip out.
    let first = steps.iter().rposition(|s| *s > 3.6).expect("no second roll");
    assert!((steps[first] - 3.8068).abs() < 0.005, "steps {:?}: first step", &steps[first..first + 3]);
    assert!((steps[first + 1] - 3.3921).abs() < 0.005, "steps {:?}: not the forward curve", &steps[first..first + 3]);
}

/// `0x14197a61b` (user test 4 f4016): the roll's target window opens with the axes of before a `0x64`
/// later in the batch, so a roll out of a cursor walk is locked against the cursor until the sprint switch.
#[test]
fn a_roll_started_with_the_axes_still_set_ignores_the_cursor() {
    let rows: Vec<String> = (0..400).map(|_| ".".repeat(400)).collect();
    let rows: Vec<&str> = rows.iter().map(|r| r.as_str()).collect();
    let (mut w, p) = world(Arc::new(VecGrid::from_rows(&rows)), (100, 200));
    w.input(p, MoveInput::Direction { x: 180, y: 0 });
    for _ in 0..4 {
        tick(&mut w);
    }
    w.input(p, MoveInput::SprintInput { held: true });
    w.input(p, MoveInput::UseSkill(skill(ROLL, 20, 0)));
    w.input(p, MoveInput::Direction { x: 0, y: 0 });
    let pos = |w: &World| w.entity(p).unwrap().movement.position();
    tick(&mut w);
    let start = pos(&w);
    for _ in 0..10 {
        w.input(p, MoveInput::UpdateSkillTarget { x: -20, y: 0, skill: ROLL });
        tick(&mut w);
    }
    let now = pos(&w);
    assert!(now.0 > start.0 + 5.0, "from {start:?} to {now:?}: the cursor must not turn the roll");
}
