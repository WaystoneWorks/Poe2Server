//! Glue-level tests: encoded client packets go in, encoded server packets come out.
//!
//! The simulation itself is tested in `poe-game` (`crates/poe-game/tests/replay.rs`). These tests
//! cover what this crate adds: the packet to input mapping (opcodes by name through the
//! protocol table), the event to packet mapping (field values), and the frame offsets of the
//! replies as the session sends them (a batch per tick, closed by the `SimulationTick` marker).
//! The scenarios are the same derived fixtures (`crates/poe-game/tests/fixtures/*.json`, from
//! the official captures).

use std::{path::Path, sync::Arc};

use poe_data::views::GrantedSkill;
use poe_game::movement::MoveStat;
use poe_net::Reader;
use poe_proto::{
    Direction, Message, OpcodeTable, Protocol,
    instance::{
        AimTarget, ComponentDelta, MoveDirection, MoveDirectionUpdate, OrientationLerp,
        SetAimTarget, SkillReleased, SprintInput, SprintState, StanceRequest, StartAction,
        StatsUpdate, UpdateSkillTarget, UseSkill,
    },
    skill::{
        BuffAdded, BuffRemoved, SkillInputEnd, SkillInputEndAck, SkillReleasedAck,
        UpdateSkillTargetAck, UseSkillAck,
    },
};
use serde_json::Value;

use crate::{
    actor_tests::{Fake, Fixture, Sent},
    movement::{ActionEvent, Codec, StatKeys, client_input},
    player::{OwnedSkill, sim_config},
};

pub(crate) const MOVE_SKILL: u32 = 0x2000_2909;
pub(crate) const ROLL_SKILL: u32 = 0x8000_0040;
pub(crate) const ROLL_ACTION_TYPE: u16 = 60748;

/// The opcode tables the official captures of these tests were recorded with (0.5.5e and 0.5.5e
/// Hotfix 1, kept under `dumps/0.5.5f/captures`): Poe2Protocol keeps them for replay in
/// `capture-opcodes/`.
pub(crate) fn capture_opcodes_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/protocol/capture-opcodes/0.5.5e.hotfix1")
}

/// The protocol tables of the captures' build, with `ActionEvent` added when `instance.toml` does not list it
/// (so the tests also see the `0x1ca` messages).
pub(crate) fn protocol() -> Arc<Protocol> {
    let dir = capture_opcodes_dir();
    let mut protocol = Protocol::load(&dir).expect("protocol tables");
    if protocol
        .instance
        .id(Direction::Server, ActionEvent::NAME)
        .is_err()
    {
        let text = std::fs::read_to_string(dir.join("instance.toml")).unwrap();
        let text = text.replacen("[server]\n", "[server]\nActionEvent = 0x01ca\n", 1);
        protocol.instance = OpcodeTable::parse("instance", &text).unwrap();
    }
    Arc::new(protocol)
}

const STAT_IDS: [MoveStat; 11] = [
    MoveStat::IsDodgeRolling,
    MoveStat::StunThresholdWhilePerformingAction,
    MoveStat::VirtualHeavyStunThreshold,
    MoveStat::VirtualLightStunThreshold,
    MoveStat::SprintBlendTimeOverrideMs,
    MoveStat::IsSprinting,
    MoveStat::MustSprint,
    MoveStat::AllHitsStunYou,
    MoveStat::VirtualSkillIgnoreNamelockingDynamically,
    MoveStat::BaseMoveSpeedFromSprint,
    MoveStat::MovementVelocityPermyriad,
];

/// Stand-in for the `Stats.dat` keys: position in [`STAT_IDS`] plus 100.
pub(crate) fn stat_keys() -> StatKeys {
    Arc::new(|id: &str| {
        STAT_IDS
            .iter()
            .position(|s| s.id() == id)
            .map(|i| i as u32 + 100)
    })
}

fn stat_id(key: u32) -> &'static str {
    STAT_IDS[(key - 100) as usize].id()
}

fn granted(action_type: Option<u16>, constants: Vec<(String, i64)>) -> GrantedSkill {
    GrantedSkill {
        effect: 25,
        effect_level_row: 899,
        stat_set_rows: vec![905],
        action_type,
        constant_stats: constants,
        implicit_stats: Vec::new(),
    }
}

pub(crate) fn roll_skill(distance: Option<i64>) -> OwnedSkill {
    OwnedSkill {
        id: ROLL_SKILL,
        granted: granted(
            Some(ROLL_ACTION_TYPE),
            distance
                .map(|d| vec![("dodge_roll_base_travel_distance".to_string(), d)])
                .unwrap_or_default(),
        ),
        id_name: "DodgeRollPlayer".into(),
        stats: Vec::new(),
    }
}

/// The simulation configuration of a test character (37 cells/s, sprint +50 %), with the
/// game's curves when the hub's game files are there.
pub(crate) fn movement_config(skills: &[OwnedSkill]) -> poe_game::movement::MovementConfig {
    let mut cfg = sim_config(
        &poe_config::MovementConfig::default(),
        skills,
        Some(MOVE_SKILL),
        Some(ROLL_ACTION_TYPE),
        37.0,
        // The classes' `sprint_movement_speed_+%` (Character.ot).
        Some(50.0),
    );
    if let Some((curves, offset_scale)) = poe_game::motion_data::hub_motion() {
        cfg.curves = Some(curves);
        cfg.offset_scale = offset_scale;
    }
    cfg
}

/// A client talking to an instance actor through the test harness: encodes packets, decodes
/// what the actor sends per tick.
struct Session {
    fx: Fixture,
    me: Fake,
}

impl Session {
    fn new(cell: (i32, i32)) -> Self {
        // The fixtures are official captures of a solo zone: the player is entity 1.
        // The real port, as `actor_init` chooses it (the actor tests' default is `Scripted`).
        let mut fx = Fixture::customised(1, |init| crate::actor::ActorInit {
            model: Arc::new(poe_game::locomotion::ClientModel),
            ..init
        });
        poe_game::motion_data::hub_motion_or_panic();
        let me = fx.join("Alice", cell, movement_config(&[roll_skill(None)]));
        Self { fx, me }
    }

    /// Sends a client packet through the same path as the server: opcode to name through the
    /// table, then [`client_input`].
    fn send<M: Message>(&mut self, message: &M) {
        self.fx.send(&self.me, message);
    }

    /// One server frame: the messages of the batch (without the tick marker), decoded back from
    /// their bytes.
    fn tick(&mut self) -> Vec<Sent> {
        self.fx.tick();
        self.me.take(&self.fx.protocol)
    }

    fn cell(&self) -> (i32, i32) {
        self.fx.cell(&self.me)
    }

    fn names(&mut self) -> Vec<&'static str> {
        self.tick().iter().map(|s| s.name).collect()
    }

    fn walk(&mut self, x: i32, y: i32) {
        self.send(&MoveDirection {
            unk_0: 1,
            x,
            y,
            unk_tail: [0; 3],
        });
    }

    fn aim(&mut self, x: i32, y: i32) {
        self.send(&AimTarget {
            local_index: 1,
            mode: 2,
            cell: Some((x, y)),
            entity: None,
        });
    }

    fn use_skill(&mut self, skill: u32, flags: u16, x: i32, y: i32) {
        self.send(&UseSkill {
            player: 1,
            skill,
            flags,
            unk_13: 0xff,
            target_flags: 0,
            target_entity: None,
            x,
            y,
        });
    }
}

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/../../crates/poe-game/tests/fixtures/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture")).expect("json")
}

fn num(v: &Value, k: &str) -> i32 {
    v[k].as_i64().unwrap_or_else(|| panic!("{k} in {v}")) as i32
}

/// The server message name an expected event is sent as.
fn expected_name(ev: &str) -> &'static str {
    match ev {
        "move_ack" => MoveDirectionUpdate::NAME,
        "action_start" => StartAction::NAME,
        "action_event" => ActionEvent::NAME,
        "stats" => StatsUpdate::NAME,
        "skill_ack" => UseSkillAck::NAME,
        "target_ack" => UpdateSkillTargetAck::NAME,
        "input_end_ack" => SkillInputEndAck::NAME,
        "sprint" => SprintState::NAME,
        "buff_added" => BuffAdded::NAME,
        "buff_removed" => BuffRemoved::NAME,
        "aim_echo" => SetAimTarget::NAME,
        "stance" => ComponentDelta::NAME,
        "orientation_lerp" => OrientationLerp::NAME,
        "released_ack" => SkillReleasedAck::NAME,
        other => panic!("fixture event {other}"),
    }
}

/// Compares the fields that do not depend on the walked distance (positions are covered by the
/// simulation's own replay tests).
fn check_fields(exp: &Value, sent: &Sent) -> Result<(), String> {
    let same = |what: &str, a: String, b: String| {
        (a == b)
            .then_some(())
            .ok_or_else(|| format!("{what}: official {a}, ours {b}"))
    };
    match exp["ev"].as_str().unwrap() {
        "move_ack" => {
            let m = sent.decode::<MoveDirectionUpdate>();
            same(
                "dir",
                format!(
                    "{:?}",
                    (
                        exp["dir"][0].as_i64().unwrap(),
                        exp["dir"][1].as_i64().unwrap()
                    )
                ),
                format!("{:?}", (m.dir_x as i64, m.dir_y as i64)),
            )?;
            same("header", "1".into(), m.header.entity_id.to_string())
        }
        "action_start" => {
            let m = sent.decode::<StartAction>();
            same(
                "skill/flags",
                format!("{:#x}/{:#x}", num(exp, "skill") as u32, num(exp, "flags")),
                format!("{:#x}/{:#x}", m.skill, m.flags),
            )
        }
        "stats" => {
            let m = sent.decode::<StatsUpdate>();
            let ours: Vec<(String, i32)> = m
                .stats
                .iter()
                .map(|&(k, v)| (stat_id(k).to_string(), v))
                .collect();
            let theirs: Vec<(String, i32)> = exp["stats"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    (
                        p[0].as_str().unwrap().to_string(),
                        p[1].as_i64().unwrap() as i32,
                    )
                })
                .collect();
            same("stats", format!("{theirs:?}"), format!("{ours:?}"))?;
            same("set", "0".into(), m.set.to_string())
        }
        "skill_ack" => {
            let m = sent.decode::<UseSkillAck>().params;
            same(
                "skill/flags/unk/target flags",
                format!(
                    "{:#x}/{:#x}/{:#x}/{:#x}",
                    num(exp, "skill") as u32,
                    num(exp, "flags"),
                    num(exp, "unk"),
                    num(exp, "target_flags") | 4
                ),
                format!(
                    "{:#x}/{:#x}/{:#x}/{:#x}",
                    m.skill, m.flags, m.unk_13, m.target_flags
                ),
            )
        }
        "target_ack" => {
            let m = sent.decode::<UpdateSkillTargetAck>();
            same(
                "skill",
                num(exp, "skill").to_string(),
                (m.skill as i32).to_string(),
            )
        }
        "sprint" => {
            let m = sent.decode::<SprintState>();
            same("held", exp["held"].to_string(), m.held.to_string())
        }
        "buff_added" => {
            let m = sent.decode::<BuffAdded>();
            let fade = exp.get("fade").map(|f| f.as_f64().unwrap() as f32);
            same(
                "index/row/fade",
                format!("{}/{}/{:?}", num(exp, "index"), num(exp, "row"), fade),
                format!("{}/{}/{:?}", m.index, m.buff, m.flag_02.map(f32::from_bits)),
            )
        }
        "buff_removed" => {
            let m = sent.decode::<BuffRemoved>();
            same("index", num(exp, "index").to_string(), m.index.to_string())
        }
        "aim_echo" => {
            let m = sent.decode::<SetAimTarget>();
            same(
                "cell",
                format!("{},{}", num(exp, "x"), num(exp, "y")),
                format!("{},{}", m.x, m.y),
            )
        }
        "stance" => {
            let m = sent.decode::<ComponentDelta>();
            let mut want = vec![0u8, 0x04, exp["combat"].as_bool().unwrap() as u8];
            if let Some(r) = exp["request"].as_u64() {
                want[1] |= 0x10;
                want.push(r as u8);
            }
            same(
                "delta",
                format!("{} {want:02x?}", m.component == 6),
                format!("true {:02x?}", m.delta),
            )
        }
        "orientation_lerp" => {
            let m = sent.decode::<OrientationLerp>();
            same(
                "value",
                format!("{:.3} 0", exp["value"].as_f64().unwrap()),
                format!("{:.3} {}", m.value, m.flag),
            )
        }
        _ => Ok(()),
    }
}

/// Feeds a fixture's inputs to a session as encoded client packets (an input of frame `f` is
/// sent before tick `f + 1`) and compares every batch with the official one: message names in
/// order, field values, and therefore the frame offsets.
fn replay(name: &str) {
    let fx = fixture(name);
    let start = (
        fx["start_cell"][0].as_i64().unwrap() as i32,
        fx["start_cell"][1].as_i64().unwrap() as i32,
    );
    let mut s = Session::new(start);
    // Fixtures that start after the entry stance say which one it was: play the entry first.
    if let Some(combat) = fx["combat_stance"].as_bool() {
        s.send(&AimTarget {
            local_index: 1,
            mode: 1,
            cell: None,
            entity: None,
        });
        if !combat {
            s.send(&StanceRequest {
                player: 1,
                value: 1,
            });
            s.send(&AimTarget {
                local_index: 1,
                mode: 0,
                cell: None,
                entity: None,
            });
        }
        for _ in 0..20 {
            s.tick();
        }
    }
    let inputs = fx["inputs"].as_array().unwrap();
    let expect = fx["expect"].as_array().unwrap();
    let last = expect
        .iter()
        .map(|e| e["f"].as_u64().unwrap())
        .max()
        .unwrap();
    let mut problems = Vec::new();
    for frame in 1..=last {
        for i in inputs
            .iter()
            .filter(|v| v["f"].as_u64().unwrap() + 1 == frame)
        {
            match i["op"].as_str().unwrap() {
                "wasd" => s.walk(num(i, "x"), num(i, "y")),
                "aim" => s.aim(num(i, "x"), num(i, "y")),
                "use_skill" => s.send(&UseSkill {
                    player: 1,
                    skill: num(i, "skill") as u32,
                    flags: num(i, "flags") as u16,
                    unk_13: num(i, "unk") as u8,
                    target_flags: num(i, "target_flags") as u8,
                    target_entity: i.get("entity").map(|e| e.as_u64().unwrap() as u32),
                    x: num(i, "x"),
                    y: num(i, "y"),
                }),
                "update_target" => s.send(&UpdateSkillTarget {
                    player: 1,
                    x: num(i, "x"),
                    y: num(i, "y"),
                    skill: num(i, "skill") as u32,
                }),
                "input_end" => s.send(&SkillInputEnd { player: 1 }),
                "sprint" => s.send(&SprintInput {
                    player: 1,
                    held: i["held"].as_bool().unwrap(),
                }),
                "released" => s.send(&SkillReleased {
                    player: 1,
                    skill: num(i, "skill") as u32,
                    unk: num(i, "flag") as u8,
                }),
                "aim_mode" => s.send(&AimTarget {
                    local_index: 1,
                    mode: num(i, "mode") as u8,
                    cell: None,
                    entity: None,
                }),
                "stance_request" => s.send(&StanceRequest {
                    player: 1,
                    value: num(i, "value") as u8,
                }),
                op => panic!("fixture input {op}"),
            }
        }
        let batch = s.tick();
        let exp: Vec<&Value> = expect
            .iter()
            .filter(|e| e["f"].as_u64().unwrap() == frame)
            .collect();
        let ours: Vec<&str> = batch.iter().map(|b| b.name).collect();
        let theirs: Vec<&str> = exp
            .iter()
            .map(|e| expected_name(e["ev"].as_str().unwrap()))
            .collect();
        if ours != theirs {
            problems.push(format!("frame {frame}: official {theirs:?}, ours {ours:?}"));
            continue;
        }
        for (e, b) in exp.iter().zip(&batch) {
            if let Err(why) = check_fields(e, b) {
                problems.push(format!("frame {frame} {}: {why}", b.name));
            }
        }
    }
    assert!(problems.is_empty(), "{name}:\n{}", problems.join("\n"));
}

/// Stance changes through the codec: `0x2a6` before the Player delta with the stance byte
/// and the requested value (`0x1000`).
#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn stance_fixtures() {
    for name in [
        "stance_to_mouse_mode",
        "stance_to_combat_mode",
        "stance_first_skill_in_mouse_mode",
        "stance_first_skill_merged",
        "stance_entry_with_request",
        "stance_entry",
    ] {
        replay(name);
    }
}

#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn wasd_start_turn_stop_with_aim_echoes() {
    replay("wasd_start_turn_stop");
}

#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn tap_roll_while_walking() {
    replay("tap_roll_while_walking");
}

#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn tap_rolls_chained_second_one_waits() {
    replay("tap_rolls_chained");
}

#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn sprint_hold_with_release() {
    replay("sprint_hold_release");
}

/// The sprint of the 2026-10-20 capture ended by the key release with the cursor direction in
/// flight: the packets of the end and the stop (the coasting itself is checked against the
/// client's own positions in `poe-game`'s replay test).
#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn sprint_wasd_released_then_space_up() {
    replay("sprint_wasd_released_then_space_up");
}

#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn sprint_ended_by_an_attack() {
    replay("sprint_ended_by_attack");
}

#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn wasd_is_answered_in_the_next_tick_with_the_servers_cell() {
    let mut s = Session::new((100, 100));
    s.walk(180, 0);
    let batch = s.tick();
    assert_eq!(batch.len(), 1);
    let m = batch[0].decode::<MoveDirectionUpdate>();
    assert_eq!((m.dir_x, m.dir_y, m.grid_x, m.grid_y), (180, 0, 100, 100));
    assert_eq!(m.header.entity_id, 1);
    // Nothing is sent for a walk in progress.
    assert!(s.tick().is_empty());
    for _ in 0..8 {
        s.tick();
    }
    s.walk(0, 0);
    let m = s.tick()[0].decode::<MoveDirectionUpdate>();
    assert_eq!((m.dir_x, m.dir_y, m.grid_y), (0, 0, 100));
    assert!(m.grid_x > 105, "the server walked on: {}", m.grid_x);
    assert_eq!(s.cell().0, m.grid_x);
}

#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn a_roll_batch_has_the_official_order_and_the_confirmed_target() {
    let mut s = Session::new((371, 1052));
    s.send(&SprintInput {
        player: 1,
        held: true,
    });
    s.use_skill(ROLL_SKILL, 0x400, 15, -35);
    let batch = s.tick();
    let names: Vec<_> = batch.iter().map(|b| b.name).collect();
    assert_eq!(
        names,
        [
            "SprintState",
            "StartAction",
            "ActionEvent",
            "StatsUpdate",
            "UseSkillAck",
            "StatsUpdate"
        ]
    );
    let start = batch[1].decode::<StartAction>();
    assert_eq!(
        (start.skill, start.flags, start.grid_x, start.grid_y),
        (ROLL_SKILL, 0x410, 371, 1052)
    );
    assert_eq!((start.target_x, start.target_y), (386, 1017));
    assert!(start.action_id >= 32768, "server-wide action counter");
    let ack = batch[4].decode::<UseSkillAck>().params;
    assert_eq!((ack.x, ack.y), (386, 1017));
    assert_eq!(ack.target_flags & 0x04, 0x04);
}

#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn skill_release_and_input_end_are_acked_in_the_next_tick() {
    let mut s = Session::new((10, 10));
    s.send(&SkillReleased {
        player: 1,
        skill: MOVE_SKILL,
        unk: 0,
    });
    s.send(&SkillInputEnd { player: 1 });
    let batch = s.tick();
    let released = batch[0].decode::<SkillReleasedAck>();
    assert_eq!((released.skill, released.flag), (MOVE_SKILL, 0));
    assert!(batch[1].is::<SkillInputEndAck>());
    assert!(s.tick().is_empty());
}

#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn combat_stance_goes_out_after_the_first_aim_mode() {
    let mut s = Session::new((10, 10));
    s.send(&AimTarget {
        local_index: 1,
        mode: 1,
        cell: None,
        entity: None,
    });
    let mut stance = None;
    for frame in 1..=20 {
        for b in s.tick() {
            if b.is::<ComponentDelta>() {
                stance = Some((frame, b.decode::<ComponentDelta>()));
            }
        }
    }
    let (frame, delta) = stance.expect("a stance change");
    assert_eq!(frame, 8 + 1, "stance_delay_frames after the input's tick");
    assert_eq!(delta.component, 6);
    assert_eq!(delta.delta, [0x00, 0x04, 0x01]);
    assert_eq!(delta.header.unk_14, 0xffff_ffff);
}

#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn a_move_click_starts_the_move_action_and_acks_the_request() {
    // Verified on the 2026-10-20 official capture (docs/protocol/movement.md#click-to-move).
    let mut s = Session::new((100, 100));
    s.use_skill(MOVE_SKILL, 0x66, 10, -5);
    let batch = s.tick();
    let names: Vec<_> = batch.iter().map(|b| b.name).collect();
    assert!(
        names.contains(&"StartAction") && names.contains(&"UseSkillAck"),
        "{names:?}"
    );
    let start = batch
        .iter()
        .find(|b| b.is::<StartAction>())
        .unwrap()
        .decode::<StartAction>();
    // The StartAction carries the request's flags plus 0x10 (official: 0x410 for a 0x400 press).
    assert_eq!((start.skill, start.flags), (MOVE_SKILL, 0x66 | 0x10));
    assert_eq!((start.target_x, start.target_y), (110, 95));
    // The player walks there on the server.
    for _ in 0..200 {
        s.tick();
    }
    assert_eq!(s.cell(), (110, 95));
}

#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn pause_holds_inputs_until_resumed() {
    let mut s = Session::new((100, 100));
    s.fx.actor.set_paused(s.me.session, true);
    s.walk(180, 0);
    assert!(s.tick().is_empty());
    assert!(s.tick().is_empty());
    assert_eq!(s.cell(), (100, 100));
    s.fx.actor.set_paused(s.me.session, false);
    assert_eq!(s.names(), ["MoveDirectionUpdate"]);
}

#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn teleport_is_a_direction_and_a_stop_with_the_new_cell() {
    let mut s = Session::new((100, 100));
    s.fx.teleport(&s.me, (300, 400));
    let batch = s.tick();
    assert_eq!(batch.len(), 2);
    let a = batch[0].decode::<MoveDirectionUpdate>();
    let b = batch[1].decode::<MoveDirectionUpdate>();
    assert_eq!(
        ((a.dir_x, a.dir_y), (a.grid_x, a.grid_y)),
        ((1, 0), (300, 400))
    );
    assert_eq!(
        ((b.dir_x, b.dir_y), (b.grid_x, b.grid_y)),
        ((0, 0), (300, 400))
    );
    assert_eq!(s.cell(), (300, 400));
}

#[test]
fn non_movement_packets_are_not_consumed() {
    let protocol = protocol();
    let mut r = Reader::new(&[1, 2, 3]);
    assert!(client_input("ChatSend", &mut r).unwrap().is_none());
    assert_eq!(r.position(), 0);
    // Every movement packet the dispatcher handles has a client opcode of that name.
    for name in [
        MoveDirection::NAME,
        UseSkill::NAME,
        UpdateSkillTarget::NAME,
        SkillInputEnd::NAME,
        SkillReleased::NAME,
        SprintInput::NAME,
        AimTarget::NAME,
        StanceRequest::NAME,
    ] {
        assert!(
            protocol.instance.id(Direction::Client, name).is_ok(),
            "{name}"
        );
    }
}

#[test]
fn action_event_is_skipped_while_instance_toml_lacks_it() {
    use poe_game::movement::MoveEvent;
    let raw = Arc::new(Protocol::load(&capture_opcodes_dir()).unwrap());
    let listed = raw
        .instance
        .id(Direction::Server, ActionEvent::NAME)
        .is_ok();
    let codec = Codec::new(raw, stat_keys());
    let sent = codec.encode(1, &MoveEvent::ActionEvent).unwrap();
    assert_eq!(sent.len(), usize::from(listed));
}

/// WASD held since before the roll: the sprint has no `MoveDirectionUpdate` stream and its end
/// sends none either (the client's direction was never scaled; a cell one off would snap it out
/// of the `SprintEnd` coast, `0x142158b60`). Server and client coast, then the held key walks on.
#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn a_sprint_without_a_stream_coasts_without_a_direction_update() {
    let mut s = Session::new((100, 100));
    s.walk(180, 0);
    s.tick();
    s.send(&SprintInput {
        player: 1,
        held: true,
    });
    s.use_skill(ROLL_SKILL, 0x400, 10, 0);
    for _ in 0..40 {
        let batch = s.tick();
        assert!(
            !batch.iter().any(|b| b.is::<MoveDirectionUpdate>()),
            "no stream without a vector after the sprint started"
        );
    }
    s.send(&SkillInputEnd { player: 1 });
    s.send(&SprintInput {
        player: 1,
        held: false,
    });
    let batch = s.tick();
    assert!(batch.iter().any(|b| b.is::<SprintState>()));
    let x = s.cell().0;
    for _ in 0..20 {
        let batch = s.tick();
        assert!(!batch.iter().any(|b| b.is::<MoveDirectionUpdate>()));
    }
    // The `SprintEnd` root motion: about 19 cells, most of it in these 20 frames.
    let coasted = s.cell().0 - x;
    assert!((16..=20).contains(&coasted), "{x} -> {}", s.cell().0);
    // `SprintEnd` refuses walks for 0.75 / rate; with no `0x64` plan the rate uses the speed after
    // the stats (T 1.007 s, 30 frames): the coast stops, then the held key walks on.
    let mut stood = 0;
    let mut last = s.cell().0;
    for _ in 0..20 {
        s.tick();
        stood += i32::from(s.cell().0 == last);
        last = s.cell().0;
    }
    assert!(stood >= 6, "{stood}");
    assert!(s.cell().0 >= x + coasted + 8, "{x} -> {}", s.cell().0);
}

#[test]
fn sim_config_comes_from_game_data_and_config() {
    let skills = vec![roll_skill(Some(52))];
    let cfg = sim_config(
        &poe_config::MovementConfig::default(),
        &skills,
        Some(MOVE_SKILL),
        Some(ROLL_ACTION_TYPE),
        41.5,
        Some(30.0),
    );
    assert_eq!(
        (cfg.skills.move_skill, cfg.skills.roll_skill),
        (MOVE_SKILL, ROLL_SKILL)
    );
    assert_eq!(cfg.roll_travel_distance, 52.0);
    assert_eq!(cfg.walk_speed, 41.5);
    assert_eq!(cfg.sprint_buff_rows, [2758, 2737, 2853]);
    assert_eq!(cfg.sprint_bonus_percent, Some(30.0));
    // Without the roll skill's constant or without a roll skill, the official defaults/sentinel.
    let cfg = sim_config(
        &poe_config::MovementConfig::default(),
        &[roll_skill(None)],
        None,
        Some(ROLL_ACTION_TYPE),
        37.0,
        None,
    );
    assert_eq!(
        (cfg.roll_travel_distance, cfg.skills.move_skill),
        (46.0, u32::MAX)
    );
    let cfg = sim_config(
        &poe_config::MovementConfig::default(),
        &[],
        Some(MOVE_SKILL),
        None,
        37.0,
        None,
    );
    assert_eq!(cfg.skills.roll_skill, u32::MAX);
}

/// The dev command `speed`: the actor hands the new stats to the movement, which sends
/// `movement_velocity_+permyriad` once and walks faster.
#[cfg_attr(not(game_files), ignore = "needs the game files")]
#[test]
fn speed_command_changes_the_walk_and_sends_the_stat_once() {
    let mut s = Session::new((100, 100));
    s.walk(180, 0);
    for _ in 0..10 {
        s.tick();
    }
    let x0 = s.cell().0;
    for _ in 0..30 {
        s.tick();
    }
    let slow = s.cell().0 - x0;
    s.fx.actor.handle(crate::actor::Cmd::SetSpeedStats {
        session: s.me.session,
        increased_percent: Some(100),
        action_speed_percent: None,
    });
    let x1 = s.cell().0;
    let mut stats = Vec::new();
    for _ in 0..30 {
        for sent in s.tick() {
            if sent.name == StatsUpdate::NAME {
                let m = sent.decode::<StatsUpdate>();
                stats.extend(m.stats.iter().map(|&(k, v)| (stat_id(k).to_string(), v)));
            }
        }
    }
    let fast = s.cell().0 - x1;
    assert_eq!(stats, [("movement_velocity_+permyriad".to_string(), 10000)]);
    assert!((fast - 2 * slow).abs() <= 2, "{slow} -> {fast} cells in 30 frames");
}
