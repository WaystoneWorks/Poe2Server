//! Character stat calculator: a Rust port of the calculation rules of Path of Building 2
//! (PathOfBuilding-PoE2, MIT licence, Copyright (c) Path of Building Community; the reference
//! commit is pinned in `third_party/fetch-tools.sh` of the hub; licence in
//! `LICENSE-PathOfBuilding.md`).
//!
//! Pure functions, no I/O: game data comes in through [`Input`]. Checked against PoB2 itself by
//! `cargo xtask stats-check`. Which stats exist for the calculator is [`map::STAT_MAP`]; every
//! other stat is "not implemented".
//! Docs: hub `docs/architecture/stats.md`.

pub mod calc;
pub mod defence;
pub mod items;
pub mod map;
pub mod moddb;
pub mod network;
pub mod offence;

pub use calc::{CharacterBase, Constants, Enemy, Input, Output, calculate};
pub use items::{ItemBase, ItemInput, Slot, WeaponBase, WeaponType};
pub use moddb::{Cfg, Kind, Mod, ModDb, Tag, flag, keyword};
pub use offence::SkillProfile;

/// PoB2's `round`: half up, also for negative numbers (`floor(x + 0.5)`).
pub fn round(v: f64) -> f64 {
    (v + 0.5).floor()
}

/// PoB2's `round(v, dec)`.
pub fn round_to(v: f64, dec: i32) -> f64 {
    let p = 10f64.powi(dec);
    (v * p + 0.5).floor() / p
}
