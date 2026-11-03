//! The character's movement data from the game files: the roll and sprint-end curves and the
//! class's `offset_scale`. Required at player setup (hub `docs/decisions.md`: no stand-in); the
//! movement port reads them through [`crate::movement::MovementConfig`].

use std::sync::Arc;

/// The class whose `offset_scale` [`hub_motion`] reads (the test account's class).
const HUB_CLASS: &str = "Metadata/Characters/Dex/DexFour";
/// Where the character's dodge roll and sprint end curves are (`Character.act`). Characters are
/// unarmed for now; the weapon variants follow the `dodge_*_01` pattern.
const ROLL_AMD: &str = "art/models/charactersfour/animations/basedodgeroll/onerig.amd";
const SPRINT_AMD: &str = "art/models/charactersfour/animations/basesprint/onerig.amd";
/// The dodge roll's animation events (a JSON timeline, `Character.act`).
const ROLL_ATL: &str = "metadata/characters/timelines/base_dodge_roll/base_dodge_roll.atl";
/// The timeline animation whose gameplay events every unarmed roll clip inherits.
const ROLL_ATL_ANIMATION: &str = "GP_dodge_roll_base_01";
/// The sprint's timeline and the animation of its end's move event.
const SPRINT_ATL: &str = "metadata/characters/timelines/base_sprint/base_sprint.atl";
const SPRINT_END_ATL_ANIMATION: &str = "GP_sprinting_end_base_01";
/// The characters' animation list (`SprintEnd`; no class file overrides it).
const CHARACTER_ACT: &str = "metadata/characters/character.act";

/// A movement curve of an animation (`.amd`: duration and one position per frame).
#[derive(Debug, Clone, PartialEq)]
pub struct Curve {
    /// Seconds.
    pub length: f32,
    pub positions: Arc<[f32]>,
}

impl Curve {
    pub fn new(length: f32, positions: Vec<f32>) -> Self {
        Self {
            length,
            positions: positions.into(),
        }
    }

    /// The curve `name` of an `.amd` file's text (per animation: `"name"`, `once` or `loop`,
    /// duration in ms, a line, then `count v0 v1 ...`).
    pub fn from_amd(text: &str, name: &str) -> Option<Self> {
        let quoted = format!("\"{name}\"");
        let mut lines = text.lines().skip_while(|l| l.trim() != quoted);
        let ms: f32 = lines.nth(2)?.trim().parse().ok()?;
        let positions = lines
            .nth(1)?
            .split_whitespace()
            .skip(1)
            .map(|v| v.parse().ok())
            .collect::<Option<Vec<f32>>>()?;
        (positions.len() >= 2).then(|| Self::new(ms / 1000.0, positions))
    }

    /// The time at which the curve reaches `v`, linear between positions (`0x141ecd760`), `length`
    /// past its end.
    pub fn time_of(&self, v: f32) -> f32 {
        let n = self.positions.len();
        let i = self.positions.partition_point(|p| *p <= v);
        if i >= n {
            return self.length;
        }
        if i == 0 {
            return 0.0;
        }
        let (a, b) = (self.positions[i - 1], self.positions[i]);
        let f = if a == b { 1.0 } else { (v - a) / (b - a) };
        ((i - 1) as f32 + f) / (n - 1) as f32 * self.length
    }

    /// The curve's position at `t` seconds; looping past the end adds the whole curve's
    /// travel per loop (`FUN_141ecccc0`).
    pub fn value(&self, t: f32) -> f32 {
        let n = self.positions.len();
        if n < 2 || t < 0.0 {
            return 0.0;
        }
        let loops = (t / self.length).floor();
        let frame = (t % self.length) / self.length * (n - 1) as f32;
        let i = (frame.floor() as usize).min(n - 1);
        let next = (i + 1).min(n - 1);
        let first = self.positions[0];
        let last = self.positions[n - 1];
        (self.positions[next] - self.positions[i]) * (frame - i as f32)
            + self.positions[i]
            + (last - first) * loops
    }
}

/// The curves of the character's dodge roll and sprint end (`basedodgeroll`, `basesprint`).
#[derive(Debug, Clone, PartialEq)]
pub struct MotionCurves {
    pub roll: Curve,
    pub roll_back: Curve,
    pub roll_sprint: Curve,
    pub roll_back_sprint: Curve,
    pub sprint_end: Curve,
    pub roll_events: RollEvents,
    /// The sprint end's move event end, seconds of the clip (`base_sprint.atl`
    /// `GP_sprinting_end_base_01` event `move` `end_time`).
    pub sprint_end_move_time: f32,
    /// The `SprintEnd` animation's base speed (`Character.act` `sprinting_end_*_01 : speed`), the
    /// clip's rate denominator in `0x141e56ae0`.
    pub sprint_end_base_speed: f32,
}

/// The gameplay events of the dodge roll's timeline that move the character.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RollEvents {
    /// `sprint_start` (seconds): the event `0xb91f` of `DodgeRoll_OnAnimationEvent`.
    pub sprint_start: f32,
    /// `StartTargetUpdateEventType`'s first value (the event's `+0x64`) in radians per second: how
    /// fast a `0x1af` turns the running roll.
    pub target_turn_rate: f32,
    /// The same event's window (seconds): `act + 0x240` is set inside it.
    pub target_update: (f32, f32),
}

impl RollEvents {
    /// From the timeline's JSON text.
    pub fn from_atl(text: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let events = v["animations"]
            .as_array()?
            .iter()
            .find(|a| a["name"] == ROLL_ATL_ANIMATION)?["events"]
            .as_array()?;
        let sprint = events.iter().find(|e| e["name"] == "sprint_start")?;
        let update = events.iter().find(|e| e["type"] == "StartTargetUpdateEventType")?;
        let rate: f32 = update["data"].as_str()?.split_whitespace().next()?.parse().ok()?;
        Some(Self {
            sprint_start: sprint["time"].as_f64()? as f32,
            target_turn_rate: rate * 0.017_453_292,
            target_update: (update["time"].as_f64()? as f32, update["end_time"].as_f64()? as f32),
        })
    }
}

/// The `move` event's `end_time` of the sprint end in the sprint's timeline (JSON text).
pub fn sprint_end_move_time(sprint_atl: &str) -> Option<f32> {
    let v: serde_json::Value = serde_json::from_str(sprint_atl.trim_start_matches('\u{feff}')).ok()?;
    let events = v["animations"]
        .as_array()?
        .iter()
        .find(|a| a["name"] == SPRINT_END_ATL_ANIMATION)?["events"]
        .as_array()?;
    Some(events.iter().find(|e| e["name"] == "move")?["end_time"].as_f64()? as f32)
}

/// `SprintEnd = " sprinting_end_*_01 : speed = 600 " timeline ...` in an `.act`.
pub fn sprint_end_base_speed(act: &str) -> Option<f32> {
    let (_, rest) = act.split_once("SprintEnd =")?;
    let entry = rest.split("timeline").next()?;
    let (_, speed) = entry.split_once("speed")?;
    speed.trim_start().trim_start_matches('=').split_whitespace().next()?.trim_matches('"').parse().ok()
}

impl MotionCurves {
    /// From the game's two `.amd` files, the roll's and sprint's timelines and `Character.act`
    /// (UTF-16 text).
    pub fn from_files(roll_text: &str, sprint_text: &str, roll_atl: &str, sprint_atl: &str, act: &str) -> Option<Self> {
        Some(Self {
            roll: Curve::from_amd(roll_text, "dodge_unarmed_01")?,
            roll_back: Curve::from_amd(roll_text, "dodge_unarmed_bwd_01")?,
            roll_sprint: Curve::from_amd(roll_text, "dodge_unarmed_01_sprinting")?,
            roll_back_sprint: Curve::from_amd(roll_text, "dodge_unarmed_bwd_01_sprinting")?,
            sprint_end: Curve::from_amd(sprint_text, "sprint_end_unarmed_01")?,
            roll_events: RollEvents::from_atl(roll_atl.trim_start_matches('\u{feff}'))?,
            sprint_end_move_time: sprint_end_move_time(sprint_atl)?,
            sprint_end_base_speed: sprint_end_base_speed(act)?,
        })
    }

    /// Reads the curves from the game files.
    pub fn load(files: &poe_data::GameFiles) -> poe_data::Result<Option<Self>> {
        let text = |path: &str| {
            files.read(path).map(|bytes| {
                let words: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16_lossy(&words).replace('\r', "")
            })
        };
        Ok(Self::from_files(
            &text(ROLL_AMD)?,
            &text(SPRINT_AMD)?,
            &text(ROLL_ATL)?,
            &text(SPRINT_ATL)?,
            &text(CHARACTER_ACT)?,
        ))
    }
}

/// `offset_scale` from the `.ao` of `metadata_id` (`Melee { offset_scale = 0.9 }`), the class's
/// own file or the ones it extends.
pub fn load_offset_scale(files: &poe_data::GameFiles, metadata_id: &str) -> Option<f32> {
    let mut path = metadata_id.to_owned();
    for _ in 0..16 {
        let bytes = files.read(&format!("{path}.ao").to_lowercase()).ok()?;
        let words: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let text = String::from_utf16_lossy(&words);
        let value = |key: &str| {
            text.lines().find_map(|l| {
                // Keys inside a block (`Melee { ... }`) are indented with tabs.
                let (k, v) = l.trim().split_once(|c: char| c == '=' || c.is_whitespace())?;
                (k.trim() == key).then(|| v.trim().trim_matches(['=', ' ', '"']).to_owned())
            })
        };
        if let Some(v) = value("offset_scale") {
            return v.parse().ok();
        }
        path = value("extends")?;
        if path == "nothing" {
            return None;
        }
    }
    None
}

/// The game's movement data from the hub's `GameFiles/` (`POE_GAME_DIR` to point elsewhere), read
/// once: the curves and the `offset_scale` of the test account's class. For tests and tools; a
/// server reads its own game data. `None` without the game files.
pub fn hub_motion() -> Option<(Arc<MotionCurves>, f32)> {
    static MOTION: std::sync::OnceLock<Option<(Arc<MotionCurves>, f32)>> = std::sync::OnceLock::new();
    MOTION
        .get_or_init(|| {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..");
            let game = std::env::var_os("POE_GAME_DIR")
                .map_or_else(|| root.join("GameFiles"), std::path::PathBuf::from);
            let files = poe_data::GameFiles::open(&game).ok()?;
            let curves = MotionCurves::load(&files).ok()??;
            let scale = load_offset_scale(&files, HUB_CLASS)?;
            Some((Arc::new(curves), scale))
        })
        .clone()
}

/// [`hub_motion`] for tests that need it.
pub fn hub_motion_or_panic() -> (Arc<MotionCurves>, f32) {
    hub_motion().expect(
        "movement tests need the game's curves: the hub's GameFiles/ (or POE_GAME_DIR) with \
         basedodgeroll and basesprint onerig.amd",
    )
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_curve_is_read_from_amd_text_and_loops() {
        let text = "\"other\"\nonce\n100\n-\n2 0 1\n\"dodge\"\nonce\n500\n-\n3 0 2 4\n";
        let c = Curve::from_amd(text, "dodge").unwrap();
        assert_eq!((c.length, &c.positions[..]), (0.5, &[0.0, 2.0, 4.0][..]));
        assert_eq!(c.value(0.125), 1.0);
        assert_eq!(c.value(0.5 + 0.25), 4.0 + 2.0);
        assert!(Curve::from_amd(text, "missing").is_none());
    }

    #[test]
    fn the_hub_game_files_have_the_curves_and_the_offset_scale() {
        let (curves, scale) = hub_motion_or_panic();
        assert!(curves.roll.positions.len() >= 2 && curves.sprint_end.length > 0.0);
        assert!(scale > 0.0);
        assert_eq!(curves.roll_events.sprint_start, 0.5);
        assert!((curves.roll_events.target_turn_rate - 3000f32.to_radians()).abs() < 0.001);
    }
}
