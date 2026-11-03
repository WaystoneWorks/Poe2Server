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
}

impl MotionCurves {
    /// From the game's two `.amd` files (UTF-16 text).
    pub fn from_amd(roll_text: &str, sprint_text: &str) -> Option<Self> {
        Some(Self {
            roll: Curve::from_amd(roll_text, "dodge_unarmed_01")?,
            roll_back: Curve::from_amd(roll_text, "dodge_unarmed_bwd_01")?,
            roll_sprint: Curve::from_amd(roll_text, "dodge_unarmed_01_sprinting")?,
            roll_back_sprint: Curve::from_amd(roll_text, "dodge_unarmed_bwd_01_sprinting")?,
            sprint_end: Curve::from_amd(sprint_text, "sprint_end_unarmed_01")?,
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
        Ok(Self::from_amd(&text(ROLL_AMD)?, &text(SPRINT_AMD)?))
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
    }
}
