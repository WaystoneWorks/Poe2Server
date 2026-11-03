//! The stub movement model: the entity stands where it was spawned or teleported and every
//! movement input is dropped. The client runs in lockstep and starts a movement only when the
//! server answers it, so the player does not move; login, area transfers and teleports work.
//! It holds the seam until the exact port of the client's movement replaces it.

use crate::movement::{Cell, Keyed, MoveEvent, MoveInput, Mover, MovementConfig, MovementModel, StepCtx};

/// Makes [`StandStill`] movers.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandStillModel;

impl MovementModel for StandStillModel {
    fn name(&self) -> &'static str {
        "standstill"
    }

    fn spawn(&self, cell: Cell, _config: &MovementConfig) -> Box<dyn Mover> {
        Box::new(StandStill::new(cell))
    }
}

/// An entity that stands on its cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandStill {
    cell: Cell,
    /// A teleport not reported yet.
    placed: bool,
    /// Inputs dropped so far (debug link).
    dropped: u64,
}

impl StandStill {
    pub fn new(cell: Cell) -> Self {
        Self {
            cell,
            placed: false,
            dropped: 0,
        }
    }
}

impl Mover for StandStill {
    fn input(&mut self, _input: MoveInput) {
        self.dropped += 1;
    }

    fn teleport(&mut self, cell: Cell) {
        self.cell = cell;
        self.placed = true;
    }

    fn tick(&mut self, _ctx: &mut StepCtx) -> Vec<Keyed> {
        if std::mem::take(&mut self.placed) {
            vec![MoveEvent::Teleport { cell: self.cell }.into()]
        } else {
            Vec::new()
        }
    }

    fn cell(&self) -> Cell {
        self.cell
    }

    fn debug_fields(&self) -> Vec<(&'static str, String)> {
        vec![
            ("state", "StandStill".into()),
            ("dropped_inputs", self.dropped.to_string()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::movement::{Open, SkillIds};

    #[test]
    fn stands_and_reports_a_teleport_once() {
        let config = MovementConfig::new(
            SkillIds {
                move_skill: 1,
                roll_skill: 2,
                roll_skill_other_set: 3,
            },
            [0; 3],
        );
        let (mut action, mut buff) = (32768, 0);
        let mut ctx = StepCtx {
            frame: 1,
            dt: 0.033,
            terrain: &Open,
            config: &config,
            next_action: &mut action,
            next_buff: &mut buff,
        };
        let mut m = StandStillModel.spawn((10, 10), &config);
        m.input(MoveInput::Direction { x: 180, y: 0 });
        assert!(m.tick(&mut ctx).is_empty());
        assert_eq!(m.cell(), (10, 10));
        m.teleport((40, 41));
        assert_eq!(m.tick(&mut ctx), [Keyed::from(MoveEvent::Teleport { cell: (40, 41) })]);
        assert!(m.tick(&mut ctx).is_empty());
        assert_eq!(m.cell(), (40, 41));
    }
}
