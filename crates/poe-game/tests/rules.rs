//! Rules no captured fixture covers, on the [`World`] with the port (`ClientModel`): walls,
//! click paths around them, teleports, weapon swaps, entity targets. Restored from the unit tests
//! of the movement before the restart where the rule still holds.
#![cfg(game_files)] // Needs the game files (build.rs).

use std::sync::Arc;

use poe_game::{
    clientpath::Obstacle,
    grid::{GridSource, VecGrid},
    locomotion::ClientModel,
    movement::{ActionKind, MoveEvent, MoveInput, MovementConfig, ObjectReach, Open, SkillIds, UseSkill},
    world::{EntityId, World, WorldConfig},
};

const MOVE: u32 = 0x2000_2909;
const ROLL: u32 = 0x8000_0040;
const ATTACK: u32 = 0x8040_0040;
const WAKE: u32 = 0x2000_b7d9;

fn world(grid: Arc<dyn GridSource>, cell: (i32, i32)) -> (World, EntityId) {
    world_with(grid, cell, |_| {})
}

fn world_with(grid: Arc<dyn GridSource>, cell: (i32, i32), edit: impl FnOnce(&mut MovementConfig)) -> (World, EntityId) {
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
    edit(&mut cfg);
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

/// The Move action starts only 9 cells or more from its target (`+0x1f0`, `0x141990470`): the
/// client stands still for a closer click, and so does the server.
#[test]
fn a_click_closer_than_three_cells_does_not_walk() {
    // vf `+0x1f0` `0x141990470` compares the squared length (`0x14016cb30`) with 9.
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


const INTERACT: u32 = 0x2000_c266;

fn interact_world(cell: (i32, i32)) -> (World, EntityId) {
    world_with(Arc::new(Open), cell, |c| {
        c.interact_skill = INTERACT;
        c.sprint_clearance_level = 4;
    })
}

/// `Waypoint.ot` (`object_size` 5, override 10) and `CheckpointBase.ot` (2, override 10).
const WAYPOINT: ObjectReach = ObjectReach { reach: 18, targeting: 5 };
const CHECKPOINT: ObjectReach = ObjectReach { reach: 15, targeting: 2 };

fn with_object(w: &mut World, object: ObjectReach) {
    w.set_objects([(500, object)].into_iter().collect());
}

fn click_object(w: &mut World, p: EntityId, at: (i32, i32)) {
    let mut s = skill(INTERACT, at.0, at.1);
    s.entity = Some(500);
    s.target_flags = 8;
    w.input(p, MoveInput::UseSkill(s));
}

/// The object's ring: `reach + player size - 1 + targeting`; 26 for a waypoint (official: clicks
/// from up to 25.5 cells act at once), 20 for a checkpoint.
#[test]
fn a_click_acts_at_once_inside_the_objects_ring() {
    for (object, ring) in [(WAYPOINT, 26), (CHECKPOINT, 20)] {
        let (mut w, p) = interact_world((100, 100));
        with_object(&mut w, object);
        tick(&mut w);
        click_object(&mut w, p, (100 + ring, 100));
        let ev = tick(&mut w);
        assert!(ev.iter().any(|e| matches!(e, MoveEvent::Interact { entity: 500, .. })), "{ev:?}");
        assert!(!ev.iter().any(|e| matches!(e, MoveEvent::ActionStart { kind: ActionKind::Move, .. })), "{ev:?}");
        assert_eq!(cell(&w, p), (100, 100));
    }
}

/// Official: a click from 41.4 cells acted at 26.0 of the waypoint; `ActionEvent` on arrival, and
/// the player stops there.
#[test]
fn a_far_click_walks_to_the_objects_ring_and_stops() {
    for (object, ring) in [(WAYPOINT, 26), (CHECKPOINT, 20)] {
        let (mut w, p) = interact_world((100, 100));
        with_object(&mut w, object);
        tick(&mut w);
        click_object(&mut w, p, (160, 100));
        let first = tick(&mut w);
        assert!(first.iter().any(|e| matches!(e, MoveEvent::ActionStart { kind: ActionKind::Move, .. })), "{first:?}");
        assert!(!first.iter().any(|e| matches!(e, MoveEvent::Interact { .. })), "{first:?}");
        let done = (0..200).find(|_| tick(&mut w).iter().any(|e| matches!(e, MoveEvent::Interact { entity: 500, .. })));
        assert!(done.is_some(), "never interacted, at {:?}", cell(&w, p));
        for _ in 0..30 {
            tick(&mut w);
        }
        let (x, y) = cell(&w, p);
        assert_eq!((x, y), (160 - ring, 100), "ring {ring}");
    }
}

/// A click in reach during a walk stops the line: `Pathfinding_FollowPath` arrives at once.
#[test]
fn a_click_in_reach_stops_a_walk() {
    let (mut w, p) = interact_world((100, 100));
    with_object(&mut w, WAYPOINT);
    tick(&mut w);
    w.input(p, MoveInput::UseSkill(skill(MOVE, 40, 0)));
    for _ in 0..4 {
        tick(&mut w);
    }
    let at = cell(&w, p);
    click_object(&mut w, p, (at.0, at.1 + 10));
    assert!(tick(&mut w).iter().any(|e| matches!(e, MoveEvent::Interact { entity: 500, .. })));
    for _ in 0..30 {
        tick(&mut w);
    }
    let end = cell(&w, p);
    assert!(end.0 - at.0 <= 3 && end.1 == at.1, "walked on from {at:?} to {end:?}");
}

/// A new click forgets the walk to the object: nothing acts when it passes the ring.
#[test]
fn a_new_click_cancels_the_walk_to_an_object() {
    let (mut w, p) = interact_world((100, 100));
    with_object(&mut w, CHECKPOINT);
    tick(&mut w);
    click_object(&mut w, p, (160, 100));
    tick(&mut w);
    tick(&mut w);
    w.input(p, MoveInput::UseSkill(skill(MOVE, 58, 0)));
    for _ in 0..60 {
        assert!(!tick(&mut w).iter().any(|e| matches!(e, MoveEvent::Interact { .. })));
    }
}

/// The near click's action slows no walk after it: `Interaction` is no attack.
#[test]
fn a_click_in_reach_does_not_slow_the_next_walk() {
    let walked = |click: bool| {
        let (mut w, p) = interact_world((100, 100));
        with_object(&mut w, WAYPOINT);
        tick(&mut w);
        if click {
            click_object(&mut w, p, (110, 100));
            tick(&mut w);
        }
        w.input(p, MoveInput::UseSkill(skill(MOVE, 50, 0)));
        for _ in 0..20 {
            tick(&mut w);
        }
        cell(&w, p).0
    };
    assert_eq!(walked(true), walked(false));
}

/// A world with a sprint that just ended by key release: the client plays `SprintEnd` and coasts.
fn coasting_after_a_sprint(object: ObjectReach) -> (World, EntityId) {
    let rows: Vec<String> = (0..400).map(|_| ".".repeat(300)).collect();
    let rows: Vec<&str> = rows.iter().map(|r| r.as_str()).collect();
    let (mut w, p) = world_with(Arc::new(VecGrid::from_rows(&rows)), (150, 60), |c| {
        c.interact_skill = INTERACT;
        c.sprint_clearance_level = 4;
    });
    with_object(&mut w, object);
    w.input(p, MoveInput::Direction { x: 0, y: 180 });
    tick(&mut w);
    w.input(p, MoveInput::SprintInput { held: true });
    w.input(p, MoveInput::UseSkill(skill(ROLL, 0, 20)));
    for _ in 0..60 {
        tick(&mut w);
    }
    w.input(p, MoveInput::Direction { x: 0, y: 0 });
    w.input(p, MoveInput::SprintInput { held: false });
    w.input(p, MoveInput::SkillInputEnd);
    for _ in 0..3 {
        tick(&mut w);
    }
    (w, p)
}

/// Game test `b4-sprint-click`: a click in reach during the coast ends the `SprintEnd` clip, and
/// the client stands where it is (position constant from the frame of the `StartAction`).
#[test]
fn a_click_in_reach_ends_the_coast() {
    let (mut w, p) = coasting_after_a_sprint(CHECKPOINT);
    let at = cell(&w, p);
    click_object(&mut w, p, (at.0, at.1 + 10));
    let ev = tick(&mut w);
    assert!(ev.iter().any(|e| matches!(e, MoveEvent::Interact { entity: 500, .. })), "{ev:?}");
    let stop = cell(&w, p);
    for _ in 0..60 {
        tick(&mut w);
    }
    assert_eq!(cell(&w, p), stop);
}

/// The user's snaps of 2026-11-06: `SprintEnd` does not drop an `Interaction` (vf70 = 0, a plain
/// Move's is 1), so a far click during the coast walks to the ring and acts on arrival.
#[test]
fn a_far_click_during_the_coast_walks_to_the_ring_and_acts() {
    let (mut w, p) = coasting_after_a_sprint(CHECKPOINT);
    let at = cell(&w, p);
    let target = (at.0, at.1 + 60);
    click_object(&mut w, p, target);
    let first = tick(&mut w);
    assert!(first.iter().any(|e| matches!(e, MoveEvent::ActionStart { kind: ActionKind::Move, .. })), "{first:?}");
    let done = (0..200).find(|_| tick(&mut w).iter().any(|e| matches!(e, MoveEvent::Interact { entity: 500, .. })));
    assert!(done.is_some(), "never interacted, at {:?}", cell(&w, p));
    for _ in 0..30 {
        tick(&mut w);
    }
    let end = cell(&w, p);
    assert_eq!(end, (at.0, target.1 - 20), "the first ring cell");
}

/// A second click during that walk: it goes on and acts once, from the same ring.
#[test]
fn a_second_click_during_the_coast_walk_still_acts_on_the_ring() {
    let (mut w, p) = coasting_after_a_sprint(CHECKPOINT);
    let at = cell(&w, p);
    let target = (at.0, at.1 + 60);
    click_object(&mut w, p, target);
    for _ in 0..5 {
        tick(&mut w);
    }
    click_object(&mut w, p, target);
    let acted = (0..200).filter(|_| tick(&mut w).iter().any(|e| matches!(e, MoveEvent::Interact { entity: 500, .. }))).count();
    assert_eq!(acted, 1);
    assert_eq!(cell(&w, p), (at.0, target.1 - 20));
}

/// A key walk towards a checkpoint, clicked in reach on the way (`G1_2` seed 1, the user's test of
/// 2026-11-07 and game test `b4-interact-wasd` "checkpoint, key held"). Returns the frame of the
/// `StartAction`'s tick and the cell there.
fn click_in_reach_with_a_key_held(w: &mut World, p: EntityId) -> (i32, i32) {
    with_object(w, CHECKPOINT);
    w.input(p, MoveInput::Direction { x: 0, y: 180 });
    for _ in 0..10 {
        tick(w);
    }
    let at = cell(w, p);
    click_object(w, p, (at.0, at.1 + 12));
    let ev = tick(w);
    assert!(ev.iter().any(|e| matches!(e, MoveEvent::Interact { entity: 500, .. })), "{ev:?}");
    cell(w, p)
}

/// The `Interaction` holds slot 0 for its 0.1 s (`0x14198e070`); the held key, pointing the way it
/// pointed at the start, does not cancel it (`Actor_StartWasdWalk` `0x141e5da30`). Its end
/// (`0x141e53140`) walks on at once. Hook, scale 0.1: slot 0 from 12188 to 13279 ms, then a Move.
#[test]
fn a_key_held_through_a_click_in_reach_waits_for_the_interaction() {
    let (mut w, p) = interact_world((100, 100));
    let stop = click_in_reach_with_a_key_held(&mut w, p);
    // The start frame's own update counts: 0.1 - 3 x 1/30 <= 0 in the third tick from it.
    for _ in 0..2 {
        tick(&mut w);
        assert_eq!(cell(&w, p), stop);
    }
    for _ in 0..6 {
        tick(&mut w);
    }
    assert!(cell(&w, p).1 > stop.1, "walks on: {:?} from {stop:?}", cell(&w, p));
}

/// Axes against the ones held at the start cancel the `Interaction` and walk at once.
#[test]
fn a_reversed_key_cancels_the_interaction() {
    let (mut w, p) = interact_world((100, 100));
    let stop = click_in_reach_with_a_key_held(&mut w, p);
    w.input(p, MoveInput::Direction { x: 0, y: -180 });
    for _ in 0..3 {
        tick(&mut w);
    }
    assert!(cell(&w, p).1 < stop.1, "walks back: {:?} from {stop:?}", cell(&w, p));
}

/// A blocking object (`CheckpointBase.ot`: `blocking`, size 2) is a ring of walls in the client's
/// path search (`0x141ee0360`, radius size level 4 + size 2): a key walk at it bends round it, as
/// the client's did at the `G1_2` checkpoint (about 6 cells from its centre).
#[test]
fn a_key_walk_bends_round_a_blocking_object() {
    let walk = |obstacles: Vec<Obstacle>| {
        let (mut w, p) = interact_world((100, 100));
        w.set_obstacles(obstacles);
        tick(&mut w);
        w.input(p, MoveInput::Direction { x: 0, y: 180 });
        let mut cells = Vec::new();
        for _ in 0..40 {
            tick(&mut w);
            cells.push(cell(&w, p));
        }
        cells
    };
    let straight = walk(Vec::new());
    assert!(straight.iter().all(|c| c.0 == 100), "{straight:?}");
    let round = walk(vec![Obstacle { cell: (100, 112), size: 2 }]);
    let d2 = |c: &(i32, i32)| (c.0 - 100).pow(2) + (c.1 - 112).pow(2);
    assert!(round.iter().all(|c| d2(c) > 16), "enters the ring: {round:?}");
    assert!(round.iter().any(|c| c.0 != 100), "{round:?}");
    assert!(round.last().unwrap().1 > 120, "walks past it: {round:?}");
}

/// Without `interact_skill` set the click is an ordinary skill: no interaction.
#[test]
fn other_skills_never_interact() {
    let (mut w, p) = world(Arc::new(Open), (100, 100));
    tick(&mut w);
    click_object(&mut w, p, (110, 115));
    assert!(!tick(&mut w).iter().any(|e| matches!(e, MoveEvent::Interact { .. })));
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

/// `0x1419894c0` (user test 4 f2319): a roll queued into a running roll does not face its target
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

/// `0x14198597b` (user test 4 f4016): the roll's target window opens with the axes of before a `0x64`
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

/// `0x1dc` SetAimTarget (`0x141e5e830`): the echo of an aim turns the player towards the cell, a WASD
/// walk keeps that facing (no `Positioned_SetFacing` from its waypoints, hook `FACE` rows,
/// crazy-keys 2026-11-04), and a roll's start does not turn (`0x1419894c0` is 2*pi for a roll), so a
/// roll along the keys away from the aim is backward.
#[test]
fn the_aim_echo_sets_the_facing_and_a_key_walk_keeps_it() {
    let rows: Vec<String> = (0..400).map(|_| ".".repeat(300)).collect();
    let rows: Vec<&str> = rows.iter().map(|r| r.as_str()).collect();
    let (mut w, p) = world(Arc::new(VecGrid::from_rows(&rows)), (150, 200));
    let facing = |w: &World| w.entity(p).unwrap().movement.facing();
    // Facing of a direction: `atan2(dy, dx) + pi/2`, in [0, 2pi).
    let ang = |dx: f32, dy: f32| (dy.atan2(dx) + std::f32::consts::FRAC_PI_2).rem_euclid(std::f32::consts::TAU);
    w.input(p, MoveInput::Aim { x: 150, y: 250 });
    tick(&mut w);
    tick(&mut w);
    assert!((facing(&w) - ang(0.0, 1.0)).abs() < 1e-4, "facing {} after the aim", facing(&w));
    // Keys up the screen: the walk does not face its waypoints.
    w.input(p, MoveInput::Direction { x: 0, y: -180 });
    for _ in 0..10 {
        tick(&mut w);
    }
    assert!(cell(&w, p).1 < 195, "no walk: {:?}", cell(&w, p));
    assert!((facing(&w) - ang(0.0, 1.0)).abs() < 1e-4, "facing {} during the key walk", facing(&w));
    // A new aim during the walk turns the player again.
    let c = cell(&w, p);
    w.input(p, MoveInput::Aim { x: c.0 + 40, y: c.1 });
    tick(&mut w);
    tick(&mut w);
    assert!((facing(&w) - ang(1.0, 0.0)).abs() < 0.1, "facing {} after the second aim", facing(&w));
}

fn washed_up_world(cell: (i32, i32)) -> (World, EntityId) {
    world_with(Arc::new(Open), cell, |c| {
        c.wake_skill = WAKE;
        c.washed_up = true;
        c.wake_frames = 20;
    })
}

/// The client's first skill use targets (0, 0) (2026-10-14, 4 of 4): it only wakes the character,
/// with the wake-up on its cell, the action event and the ack, in that order of answers.
#[test]
fn the_first_skill_use_of_a_washed_up_character_wakes_it_where_it_lies() {
    let (mut w, p) = washed_up_world((284, 1141));
    w.input(p, MoveInput::UseSkill(skill(MOVE, -284, -1141)));
    let events = tick(&mut w);
    let start = events.iter().find_map(|e| match e {
        MoveEvent::ActionStart { skill, cell, target, flags, unk, entity, .. } => Some((*skill, *cell, *target, *flags, *unk, *entity)),
        _ => None,
    });
    assert_eq!(start, Some((WAKE, (284, 1141), (284, 1141), 0, 0xff, None)));
    assert!(events.iter().any(|e| matches!(e, MoveEvent::ActionEvent)));
    assert!(events.iter().any(|e| matches!(e, MoveEvent::SkillAck { skill: MOVE, .. })));
    for _ in 0..40 {
        tick(&mut w);
    }
    assert_eq!(cell(&w, p), (284, 1141));
}

/// Only the first one wakes: an ordinary character walks at once.
#[test]
fn a_character_that_is_not_washed_up_walks() {
    let (mut w, p) = world_with(Arc::new(Open), (284, 1141), |c| c.wake_skill = WAKE);
    w.input(p, MoveInput::Direction { x: 127, y: 127 });
    for _ in 0..10 {
        tick(&mut w);
    }
    assert_ne!(cell(&w, p), (284, 1141));
}

/// The bug of B1: a key pressed during the wake-up must not walk the server's character while the
/// client plays `WashedUp`, or the client's next move snaps to where the server got to.
#[test]
fn nothing_moves_the_character_during_the_wake_up() {
    let (mut w, p) = washed_up_world((284, 1141));
    w.input(p, MoveInput::UseSkill(skill(MOVE, -284, -1141)));
    tick(&mut w);
    w.input(p, MoveInput::Direction { x: 127, y: 127 });
    tick(&mut w);
    w.input(p, MoveInput::UseSkill(skill(MOVE, 50, 50)));
    let answers = tick(&mut w);
    assert!(answers.iter().any(|e| matches!(e, MoveEvent::SkillAck { .. })));
    assert!(!answers.iter().any(|e| matches!(e, MoveEvent::ActionStart { .. })));
    for _ in 0..10 {
        tick(&mut w);
        assert_eq!(cell(&w, p), (284, 1141));
    }
}

/// A key still down when the wake-up ends walks in that frame (movement log: `Move` starts in the
/// frame `WashedUp` ends); a key released before does not.
#[test]
fn a_key_held_through_the_wake_up_walks_when_it_ends() {
    let (mut w, p) = washed_up_world((284, 1141));
    w.input(p, MoveInput::UseSkill(skill(MOVE, -284, -1141)));
    tick(&mut w);
    w.input(p, MoveInput::Direction { x: 127, y: 127 });
    for _ in 0..30 {
        tick(&mut w);
    }
    assert_ne!(cell(&w, p), (284, 1141));

    let (mut w, p) = washed_up_world((284, 1141));
    w.input(p, MoveInput::UseSkill(skill(MOVE, -284, -1141)));
    tick(&mut w);
    w.input(p, MoveInput::Direction { x: 127, y: 127 });
    tick(&mut w);
    w.input(p, MoveInput::Direction { x: 0, y: 0 });
    for _ in 0..30 {
        tick(&mut w);
    }
    assert_eq!(cell(&w, p), (284, 1141));
}
