//! Shared test helpers: the captures' protocol tables, stand-in stat keys, a test character's
//! movement configuration and [`Scripted`], a test movement model that answers inputs with fixed
//! events (the actor tests check relaying and encoding, not movement).

use std::{path::Path, sync::Arc};

use poe_data::views::GrantedSkill;
use poe_game::movement::{
    ActionKind, Cell, Keyed, MoveEvent, MoveInput, MoveStat, Mover, MovementConfig, MovementModel,
    StepCtx, ACTION_INPUT_FLAG,
};
use poe_proto::{Direction, Message, OpcodeTable, Protocol};

use crate::{
    movement::{ActionEvent, StatKeys},
    player::{OwnedSkill, sim_config},
};

pub(crate) const MOVE_SKILL: u32 = 0x2000_2909;
pub(crate) const ROLL_SKILL: u32 = 0x8000_0040;
pub(crate) const ROLL_ACTION_TYPE: u16 = 60748;

/// The build whose opcodes the tests use (`modules/protocol/versions/<version>.toml`).
pub(crate) const CAPTURE_VERSION: &str = "0.5.5e.hotfix1";

/// The opcode folder of [`CAPTURE_VERSION`].
pub(crate) fn capture_opcodes_dir() -> std::path::PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    poe_config::VersionFile::load(&root, CAPTURE_VERSION)
        .expect("version file of the captures' build")
        .opcodes_dir(&root)
}

/// The protocol tables, with `ActionEvent` added when `instance.toml` does not list it (so the
/// tests also see the `0x1ca` messages).
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

/// The movement configuration of a test character (37 cells/s), without curves.
pub(crate) fn movement_config(skills: &[OwnedSkill]) -> MovementConfig {
    sim_config(
        &poe_config::MovementConfig::default(),
        skills,
        Some(MOVE_SKILL),
        Some(ROLL_ACTION_TYPE),
        37.0,
        None,
    )
}

/// A test model: a direction is acked with the entity's cell, a roll starts an action with the
/// roll's stats (one owner-only), `AimMode(1)` enters combat stance; a teleport is reported.
/// Nothing moves.
pub(crate) struct ScriptedModel;

impl MovementModel for ScriptedModel {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn spawn(&self, cell: Cell, config: &MovementConfig) -> Box<dyn Mover> {
        Box::new(Scripted {
            cell,
            roll_skill: config.skills.roll_skill,
            inputs: Vec::new(),
            placed: false,
            combat: false,
            wasd: (0, 0),
        })
    }
}

#[derive(Debug)]
pub(crate) struct Scripted {
    cell: Cell,
    roll_skill: u32,
    inputs: Vec<MoveInput>,
    placed: bool,
    combat: bool,
    wasd: Cell,
}

impl Mover for Scripted {
    fn input(&mut self, input: MoveInput) {
        self.inputs.push(input);
    }

    fn teleport(&mut self, cell: Cell) {
        self.cell = cell;
        self.placed = true;
    }

    fn tick(&mut self, ctx: &mut StepCtx) -> Vec<Keyed> {
        let mut out = Vec::new();
        if std::mem::take(&mut self.placed) {
            out.push(MoveEvent::Teleport { cell: self.cell });
        }
        for input in std::mem::take(&mut self.inputs) {
            match input {
                MoveInput::Direction { x, y } => {
                    self.wasd = (x, y);
                    out.push(MoveEvent::MoveAck {
                        dir: (x, y),
                        cell: self.cell,
                    });
                }
                MoveInput::UseSkill(u) if u.skill == self.roll_skill => {
                    let action = *ctx.next_action;
                    *ctx.next_action = action.wrapping_add(1);
                    let target = (self.cell.0 + u.x, self.cell.1 + u.y);
                    out.extend([
                        MoveEvent::ActionStart {
                            kind: ActionKind::Roll,
                            skill: u.skill,
                            flags: u.flags | ACTION_INPUT_FLAG,
                            action,
                            cell: self.cell,
                            target,
                            entity: None,
                            unk: u.unk,
                        },
                        MoveEvent::ActionEvent,
                        MoveEvent::Stats(vec![(MoveStat::IsDodgeRolling, 1)]),
                        MoveEvent::SkillAck {
                            skill: u.skill,
                            flags: u.flags,
                            unk: u.unk,
                            target_flags: u.target_flags,
                            entity: None,
                            x: target.0,
                            y: target.1,
                        },
                        MoveEvent::Stats(vec![
                            (MoveStat::StunThresholdWhilePerformingAction, 100),
                            (MoveStat::VirtualHeavyStunThreshold, 100),
                            (MoveStat::VirtualLightStunThreshold, 100),
                        ]),
                    ]);
                }
                MoveInput::SprintInput { held } => out.push(MoveEvent::SprintState { held }),
                MoveInput::AimMode { mode: 1 } => self.combat = true,
                _ => {}
            }
        }
        out.into_iter().map(Keyed::from).collect()
    }

    fn cell(&self) -> Cell {
        self.cell
    }

    fn in_combat_stance(&self) -> bool {
        self.combat
    }

    fn debug_fields(&self) -> Vec<(&'static str, String)> {
        vec![
            ("state", "Scripted".into()),
            ("wasd", format!("{} {}", self.wasd.0, self.wasd.1)),
        ]
    }
}
