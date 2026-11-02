//! Doodads (`docs/re/terrain/doodads.md` in the hub): the objects a room (`.arm`) brings to the map, and
//! the doodad hash the client reports.
//!
//! `TerrainGen_Doodads` (`0x141ff56d0`) walks every region's room placements (the list the
//! de-repetition pass also uses, region +0x58, 0x38 bytes each) and calls `TerrainGen_TileDoodads`
//! (`0x141ff3490`) for each. A placement's doodad entries are the room's `doodads`; the cell
//! of each comes from `0x141fe7a50`: the entry's cell inside the room, turned by the placement's
//! orientation (table `0x142f02118`) within the room's `w × h` tiles, plus the placement's
//! corner. [`resolve`] then follows each entry's `AOSet` picks (`0x141fe76a0`, drawing from the
//! layout's random stream, in placement order). Entries whose object type has `+0x2d0 == 0` add
//! `y + murmur(path) + (x << 16)` to the doodad hash; static types stamp their hull into the
//! collision grid ([`crate::collision`]).

use crate::{
    generate::{Files, Room},
    hash,
    rng::TerrainRng,
};

/// Cells per tile side.
pub const TILE: i32 = 23;

/// One placed doodad, in cells of the map it was placed in.
#[derive(Debug, Clone, PartialEq)]
pub struct Doodad {
    pub x: i32,
    pub y: i32,
    /// Radians, in `0..=2π` (the export's angle).
    pub angle: f32,
    /// The `.ao` file (`Metadata/...`), as the room names it.
    pub path: String,
    /// The object type (`Metadata/MiscellaneousObjects/Doodad`, ...).
    pub kind: String,
    /// Whether the doodad adds to the doodad hash ([`type_counts`]); [`resolve`] sets it,
    /// placement leaves it `false`.
    pub counted: bool,
    /// The object type is static (`Positioned.static = true`): only those stamp their hull into
    /// the collision grid; set by [`resolve`].
    pub solid: bool,
    /// The type's side flag (`+0x2d0`, from `BaseEvents`): 0 both sides, [`SIDE_SERVER`] or
    /// [`SIDE_CLIENT`]; the generator's filter decides by it whether the object exists at all
    /// ([`Doodad::exists`]); set by [`resolve`].
    pub side: u8,
    /// The type has a `league_category` (`+0x2d4` other than 0x39): it exists only in areas
    /// whose league category equals it, which no area without a league mechanic rolled has.
    pub league: bool,
    /// Tile rectangles (x0, y0, x1, y1) of later covering placements (`0x141ff3490`'s list of
    /// placements with flag `+0x1c & 2`): a `Doodad`-family object inside one is not created.
    pub covers: Vec<(i32, i32, i32, i32)>,
    /// The generator's checks after the hash drop the object ([`dropped_by_type`], `covers`).
    pub dropped: bool,
    /// Object group of a no-overlap object (`0x141ff4a50`): 0 for room doodads, the tile's
    /// `.tdt` object group for tile objects.
    pub group: u8,
    /// Whether the object is placed (stamps its hull); [`no_overlap`] clears it for no-overlap
    /// objects that lose to a neighbour.
    pub placed: bool,
    /// The object file the doodad resolved to after the `AOSet` picks (its `Hull` is rasterised
    /// into the collision grid); empty before [`resolve`].
    pub leaf: String,
    /// The placement is mirrored (orientations 4..7) and the arm entry's flip flag.
    pub mirrored: bool,
    pub entry_flip: bool,
    /// The arm entry's scale.
    pub entry_scale: f32,
    /// Mirrored hull (placement XOR pick XOR entry flag; `0x141fe7a50`); set by [`resolve`].
    pub flip: bool,
    /// Entry scale times the `AOSet` scale pick; set by [`resolve`].
    pub scale: f32,
}

/// `0x141fe7a50`: the cell of an entry `(ex, ey)` of a room of `w × h` tiles turned by
/// orientation `o`, relative to the placement's corner. The table at `0x142f02118` (three bytes
/// per orientation) says which of `(w·23 - ex - 1, ex, h·23 - ey - 1, ey)` give x and y.
pub fn turn_cell(ex: i32, ey: i32, w: i32, h: i32, o: u8) -> (i32, i32) {
    const T: [[usize; 3]; 9] = [
        [0, 1, 1],
        [1, 0, 1],
        [0, 0, 0],
        [1, 1, 0],
        [0, 0, 1],
        [1, 0, 0],
        [0, 1, 0],
        [1, 1, 1],
        [0, 1, 1],
    ];
    let a = [w * TILE - ex - 1, ex, h * TILE - ey - 1, ey];
    let t = T[(o as usize).min(8)];
    let x = a[t[1] + t[0] * 2];
    let y = a[t[2] + if t[0] == 0 { 2 } else { 0 }];
    (x, y)
}

/// The entry's angle after the placement's turn (`0x141ff3490`/`0x141fe7a50`): mirrored
/// orientations (4..7) negate it, each quarter turn adds π/2, then it is wrapped into `0..=2π`.
pub fn turn_angle(radians: f32, o: u8) -> f32 {
    use std::f32::consts::{FRAC_PI_2, TAU};
    let mut a = if (4..8).contains(&o) { TAU - radians } else { radians };
    a += (o & 3) as f32 * FRAC_PI_2;
    while a < 0.0 {
        a += TAU;
    }
    while a > TAU {
        a -= TAU;
    }
    a
}

/// The `outside_tiles` zones of `room` placed with its turned corner at `origin` (tiles) and
/// orientation `o`, as cell rectangles (x0, y0, x1, y1, end exclusive). `0x141ffbab0` (after the
/// layout) turns a zone's first cell and its last cell (`x_max - 1`, `y_max - 1`) like a doodad
/// cell and hands the rectangle to `0x141fdd690`, which flags the corners ([`mark_outside`]).
/// The zone's object type (empty in every `outside_tiles` zone of this build) passes the side
/// filter.
pub fn room_outside(room: &Room, origin: (i32, i32), o: u8) -> Vec<(i32, i32, i32, i32)> {
    let (w, h) = (room.width() as i32, room.height() as i32);
    let Some(zones) = &room.arm.zones else { return Vec::new() };
    zones
        .iter()
        .filter(|z| z.name == "outside_tiles")
        .map(|z| {
            let a = turn_cell(z.x_min, z.y_min, w, h, o);
            let b = turn_cell(z.x_max - 1, z.y_max - 1, w, h, o);
            let (ox, oy) = (origin.0 * TILE, origin.1 * TILE);
            (a.0.min(b.0) + ox, a.1.min(b.1) + oy, a.0.max(b.0) + 1 + ox, a.1.max(b.1) + 1 + oy)
        })
        .collect()
}

/// `0x141fdd690`: lattice flag 8 on every corner from `ceil(x0 / 23)` to `ceil(x1 / 23)` and
/// `ceil(y0 / 23)` to `ceil(y1 / 23)` (inclusive, clamped to the map) of each zone.
pub fn mark_outside(map: &mut crate::generate::TileMap) {
    let ceil = |v: i32| v.div_euclid(TILE) + (v.rem_euclid(TILE) != 0) as i32;
    for (x0, y0, x1, y1) in std::mem::take(&mut map.outside) {
        let (ex, ey) = (ceil(x1).min(map.width as i32), ceil(y1).min(map.height as i32));
        for y in ceil(y0).max(0)..=ey {
            for x in ceil(x0).max(0)..=ex {
                if let Some(i) = map.point_index(x, y) {
                    map.lattice[i].flags |= 8;
                }
            }
        }
    }
}

/// The doodads of `room` placed with its turned corner at `origin` (tiles) and orientation `o`.
pub fn room_doodads(room: &Room, origin: (i32, i32), o: u8) -> Vec<Doodad> {
    let (w, h) = (room.width() as i32, room.height() as i32);
    room.arm
        .doodads
        .iter()
        .map(|d| {
            let (x, y) = turn_cell(d.x as i32, d.y as i32, w, h, o);
            Doodad {
                x: x + origin.0 * TILE,
                y: y + origin.1 * TILE,
                angle: turn_angle(d.radians1, o),
                path: d.ao_file.clone(),
                kind: d.stub.clone(),
                counted: false,
                leaf: String::new(),
                solid: false,
                side: 0,
                league: false,
                covers: Vec::new(),
                dropped: false,
                group: 0,
                placed: true,
                mirrored: (4..8).contains(&o),
                entry_flip: d.bool1,
                entry_scale: d.scale,
                flip: false,
                scale: 1.0,
            }
        })
        .collect()
}

/// Whether objects of type `kind` count for the hash (`+0x2d0 == 0` and `+0x2d4` accepted on
/// the type's data, `0x141ff3490`): the type exists (a template `.ot`, or an `.ao` extending
/// one) and its merged `BaseEvents` sets neither `server_only` nor `client_only` (the side flag
/// that `+0x2d0` holds) and no `league_category` (`+0x2d4`: 0x39 without one; with one it must
/// equal the area's league category, which is 0 for areas without league mechanics rolled).
pub fn type_solid(gd: &poe_data::GameData, kind: &str) -> bool {
    poe_data::template::ObjectTemplate::load(gd, kind)
        .map(|t| t.component("Positioned").iter().any(|(k, v)| k == "static" && v == "true"))
        .unwrap_or(false)
}

/// Whether objects of type `kind` count for the hash (see above).
pub fn type_counts(gd: &poe_data::GameData, kind: &str) -> bool {
    let Ok(t) = poe_data::template::ObjectTemplate::load(gd, kind) else { return false };
    // The type has a template of its own: a path that only has an `.ao` (extending another
    // type, such as `Metadata/Chests/SirenEggs/SirenEgg_02` in G1_3) is not an object type.
    if !t.files.last().is_some_and(|f| f.ends_with(".ot")) {
        return false;
    }
    !t.component("BaseEvents").iter().any(|(k, v)| match k.as_str() {
        "server_only" | "client_only" => v == "true",
        "league_category" => !v.is_empty(),
        _ => false,
    })
}

/// What the doodad pass reads from an object file (`.ao`): its `AOSet` component, merged over the
/// `extends` chain (`FUN_141d5af20` looks the component up, `0x141fe76a0` uses it).
#[derive(Debug, Clone, Default)]
pub struct AoInfo {
    /// Some file of the chain declares `AOSet` (for example through `Metadata/AOSetParent`).
    pub has_set: bool,
    /// `ao = "<weight> <path>"` entries; the path `null` is `None`.
    pub choices: Vec<(Option<String>, u32)>,
    /// Number of `fixed_ao` entries.
    pub fixed: usize,
    pub rotation: (f32, f32),
    pub random_rotation: bool,
    pub scale: (f32, f32),
    pub translate_radius: f32,
    /// `random_orientation` (all eight orientations) / `random_flip` (the mirrored pair): the
    /// allowed-orientation mask at `+0x5c` (`0x11` flip, `0xff` orientation; 0 none).
    pub orientation_mask: u8,
    /// `Hull` block lines (`key`, unquoted value) over the `extends` chain, base first.
    pub hull: Vec<(String, String)>,
}

/// Orientation names in bit order (`.tmd` entry lists, `random_orientation` lists).
const ORIENTATION_NAMES: [&str; 8] = ["I", "R90", "R180", "R270", "FI", "FR90", "FR180", "FR270"];

/// The orientation mask of a space-separated list of orientation names.
fn orientation_names(v: &str) -> u8 {
    v.split_whitespace().filter_map(|t| ORIENTATION_NAMES.iter().position(|n| *n == t)).fold(0, |m, i| m | 1 << i)
}

fn quoted(v: &str) -> &str {
    v.trim().trim_matches('"')
}

/// Parses one `.ao` file's text: its `extends` and `AOSet` block.
fn parse_ao(text: &str) -> (Option<String>, Option<AoInfo>, Vec<(String, String)>) {
    let mut extends = None;
    let mut hull: Vec<(String, String)> = Vec::new();
    let mut set: Option<AoInfo> = None;
    let (mut depth, mut name) = (0usize, String::new());
    for line in text.lines() {
        let l = line.trim();
        if depth == 0 {
            if let Some(r) = l.strip_prefix("extends") {
                extends = Some(quoted(r).to_string());
                continue;
            }
            let head = l.split(['{', '}']).next().unwrap_or("").trim();
            if !head.is_empty() && !l.starts_with("//") {
                name = head.to_string();
            }
            if name == "AOSet" && set.is_none() {
                set = Some(AoInfo { has_set: true, scale: (1.0, 1.0), ..Default::default() });
            }
        }
        if depth == 1 && name == "Hull" && let Some((k, v)) = l.split_once('=') {
            hull.push((k.trim().to_string(), quoted(v).to_string()));
        }
        let inside = depth == 1 && name == "AOSet";
        if inside && let Some((k, v)) = l.split_once('=') {
            let (k, v) = (k.trim(), quoted(v));
            let s = set.as_mut().unwrap();
            let f = |v: &str| v.parse::<f32>().unwrap_or(0.0);
            match k {
                "ao" => {
                    let (w, p) = v.split_once(' ').unwrap_or((v, "null"));
                    let p = p.trim();
                    s.choices.push((if p == "null" { None } else { Some(p.to_string()) }, w.trim().parse().unwrap_or(0)));
                }
                "fixed_ao" => s.fixed += 1,
                "random_rotation" => s.random_rotation = v == "true",
                "random_rotation_min" => s.rotation.0 = f(v),
                "random_rotation_max" => s.rotation.1 = f(v),
                "random_scale_min" => s.scale.0 = f(v),
                "random_scale_max" => s.scale.1 = f(v),
                "random_translate_radius" => s.translate_radius = f(v),
                "random_orientation" if v == "true" => s.orientation_mask |= 0xff,
                // A list of orientation names (`"I R180 FI FR180"`) allows only those.
                "random_orientation" => s.orientation_mask |= orientation_names(v),
                "random_flip" if v == "true" => s.orientation_mask |= 0x11,
                _ => {}
            }
        }
        for c in l.chars() {
            match c {
                '{' => depth += 1,
                '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    (extends, set, hull)
}

/// Object files by path, parsed once.
pub struct AoCache<'a> {
    files: &'a dyn Files,
    cache: std::collections::HashMap<String, std::sync::Arc<AoInfo>>,
}

impl<'a> AoCache<'a> {
    pub fn new(files: &'a dyn Files) -> Self {
        Self { files, cache: Default::default() }
    }

    /// The merged `AOSet` information of `path` (a missing file is a plain object).
    pub fn get(&mut self, path: &str) -> std::sync::Arc<AoInfo> {
        let key = crate::generate::normalize_path(path);
        if let Some(i) = self.cache.get(&key) {
            return i.clone();
        }
        let mut info = AoInfo { scale: (1.0, 1.0), ..Default::default() };
        let mut chain = Vec::new();
        let mut next = Some(key.clone());
        while let Some(p) = next.take() {
            if chain.len() > 16 {
                break;
            }
            let Ok(bytes) = self.files.read(&format!("{p}")) else { break };
            let (ext, set, hull) = parse_ao(&crate::text_utf16(&bytes));
            chain.push((set, hull));
            next = ext.filter(|e| !e.is_empty() && e != "nothing").map(|e| crate::generate::normalize_path(&format!("{e}.ao")));
        }
        for (set, hull) in chain.into_iter().rev() {
            info.hull.extend(hull);
            let Some(set) = set else { continue };
            info.has_set = true;
            info.choices.extend(set.choices);
            info.fixed += set.fixed;
            info.random_rotation |= set.random_rotation;
            info.orientation_mask |= set.orientation_mask;
            if set.rotation != (0.0, 0.0) {
                info.rotation = set.rotation;
            }
            if set.scale != (1.0, 1.0) {
                info.scale = set.scale;
            }
            if set.translate_radius != 0.0 {
                info.translate_radius = set.translate_radius;
            }
        }
        let info = std::sync::Arc::new(info);
        self.cache.insert(key, info.clone());
        info
    }
}

/// `0x1421c3f40`: the orientation `u` selects among those the mask allows (bit `i` for
/// orientation `i`); a mask of 0 or 0xff takes `u & 7`.
fn orientation_pick(mask: u8, u: u32) -> u8 {
    if (1..0xff).contains(&mask) {
        let list: Vec<u8> = (0..8).filter(|i| mask >> i & 1 != 0).collect();
        list[u as usize % list.len()]
    } else {
        (u & 7) as u8
    }
}

/// `0x141fe76a0`: follows the `AOSet` choices of a doodad's object file (one `below(total)` draw
/// per level, `0x141fe7737`) until a file without choices; `None` when a `null` choice (or an
/// empty set) ends the chain: the client then skips the entry. A file without an `AOSet` or with
/// `fixed_ao` entries ends the chain, and the last set seen then draws for its rotation
/// (`0x200`), orientation (`0x348`), scale (`0x200`, `0x141fe78dc`) and translation (two `0x200`).
/// Returns the angle and cell offset that adds to the entry.
struct Pick {
    angle: f32,
    off: (i32, i32),
    scale: f32,
    mirror: bool,
    leaf: String,
}

fn pick(cache: &mut AoCache, path: &str, rng: &mut TerrainRng) -> Option<Pick> {
    let mut path = path.to_string();
    let mut last: Option<std::sync::Arc<AoInfo>> = None;
    loop {
        let info = cache.get(&path);
        if std::env::var("POE_DBG_DOODADS").is_ok() {
            eprintln!("DOODAD pick {path}: set {} choices {} fixed {} rot {:?}/{} scale {:?} radius {} orient {:#x}", info.has_set, info.choices.len(), info.fixed, info.rotation, info.random_rotation, info.scale, info.translate_radius, info.orientation_mask);
        }
        if info.has_set {
            last = Some(info.clone());
        }
        if !info.has_set || info.fixed != 0 {
            let mut angle = 0.0;
            let mut off = (0, 0);
            let (mut scale, mut mirror) = (1.0f32, false);
            if let Some(s) = &last {
                // `random_rotation = true` without a range is -π..π.
                let (lo, hi) = if s.random_rotation && s.rotation == (0.0, 0.0) { (-std::f32::consts::PI, std::f32::consts::PI) } else { s.rotation };
                if lo < 0.0 || hi > 0.0 {
                    let r = rng.below(0x200) as f32;
                    angle = (hi - lo) * r / 512.0 + lo;
                }
                if s.orientation_mask != 0 {
                    let u = rng.below(0x348);
                    let o = orientation_pick(s.orientation_mask, u);
                    if std::env::var("POE_DBG_DOODAD_ORDER").is_ok() {
                        eprintln!("    orientation u {u} mask {:#x} -> {o}", s.orientation_mask);
                    }
                    angle = turn_angle(angle, o);
                    mirror = (4..8).contains(&o);
                }
                if s.scale.0 < 1.0 || s.scale.1 > 1.0 {
                    let u = rng.below(0x200) as f32;
                    scale = (s.scale.1 - s.scale.0) * u * 0.001953125 + s.scale.0;
                }
                if s.translate_radius > 0.0 {
                    let d = (s.translate_radius * s.translate_radius * rng.below(0x200) as f32 / 512.0).sqrt();
                    let t = rng.below(0x200) as f32 / 512.0 * std::f32::consts::TAU;
                    off = ((d * t.sin() + 0.5).floor() as i32, (-d * t.cos() + 0.5).floor() as i32);
                }
            }
            return Some(Pick { angle, off, scale, mirror, leaf: path });
        }
        let total: u32 = info.choices.iter().map(|c| c.1).sum();
        let mut r = rng.below(total) as i64;
        let mut chosen = None;
        for (p, w) in &info.choices {
            if r < *w as i64 {
                chosen = Some(p.clone());
                break;
            }
            r -= *w as i64;
        }
        match chosen.flatten() {
            Some(p) => path = p,
            None => return None,
        }
    }
}

/// Side flag of `server_only` types (`+0x2d0 == 1`; `0x141ff48e0` compares with 1).
pub const SIDE_SERVER: u8 = 1;
/// Side flag of `client_only` types.
pub const SIDE_CLIENT: u8 = 2;

/// Which process runs the generator. The generator's object filter (`0x141ff48e0`, the base
/// generator's vtable `+0xa0`) keeps an object of a side-flagged type only on its own side: the
/// server (the globals `0x1445c995c` / `0x14458c74c` set) keeps `server_only` types and drops
/// `client_only` ones, a client process does the opposite. The official server's grid is the
/// [`Side::Server`] one; the DLL's batch exports run the same generator in the client process
/// ([`Side::Client`]), which is what they are compared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Client,
    Server,
}

impl Doodad {
    /// Whether the generator's filter (`0x141ff48e0`) keeps the object on `side`: its type has no
    /// league category (or the area's, never the case here) and is not of the other side.
    pub fn exists(&self, side: Side) -> bool {
        !self.league
            && !self.dropped
            && match self.side {
                0 => true,
                s => (s == SIDE_SERVER) == (side == Side::Server),
            }
    }
}

/// `(counts for the hash, static, side, league category)` of an object type, read once per
/// process: all come from the type's `.ot`/`.ao` files, and reading them through `GameData`
/// takes its bundle lock, which serialised the threads of the batch tools. One process uses one
/// game version.
fn type_flags(gd: &poe_data::GameData, kind: &str) -> (bool, bool, u8, bool) {
    use std::sync::{Mutex, OnceLock};
    static FLAGS: OnceLock<Mutex<std::collections::HashMap<String, (bool, bool, u8, bool)>>> = OnceLock::new();
    let map = FLAGS.get_or_init(Default::default);
    if let Some(f) = map.lock().unwrap_or_else(|e| e.into_inner()).get(kind) {
        return *f;
    }
    let counts = type_counts(gd, kind);
    // An `.ao`-only path is no object type, so it neither counts nor stamps a hull; `type_solid`
    // reads the template only.
    let template = poe_data::template::ObjectTemplate::load(gd, kind).ok();
    let own = template.as_ref().is_some_and(|t| t.files.last().is_some_and(|f| f.ends_with(".ot")));
    let events: Vec<(String, String)> = template.as_ref().map(|t| t.component("BaseEvents").iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()).unwrap_or_default();
    let set = |key: &str| events.iter().any(|(k, v)| k == key && v == "true");
    let side = if set("server_only") { SIDE_SERVER } else if set("client_only") { SIDE_CLIENT } else { 0 };
    let league = events.iter().any(|(k, v)| k == "league_category" && !v.is_empty());
    let f = (counts, own && type_solid(gd, kind), side, league);
    map.lock().unwrap_or_else(|e| e.into_inner()).insert(kind.to_string(), f);
    f
}

/// The doodad pass over the placed doodads in placement order: each entry resolves its object
/// file (drawing from `rng` for `AOSet` choices), entries that resolve to nothing are dropped, and
/// [`Doodad::counted`] is set from the object type.
pub fn resolve(gd: &poe_data::GameData, files: &dyn Files, rng: &mut TerrainRng, doodads: Vec<Doodad>) -> Vec<Doodad> {
    let mut cache = AoCache::new(files);
    let mut out = Vec::with_capacity(doodads.len());
    let dbg = std::env::var("POE_DBG_DOODAD_ORDER").is_ok();
    for (i, mut d) in doodads.into_iter().enumerate() {
        let before = rng.draws;
        if dbg {
            eprintln!("    base angle {:.4} at ({}, {})", d.angle, d.x, d.y);
        }
        let picked = pick(&mut cache, &d.path, rng);
        if dbg && rng.draws != before {
            eprintln!("DOODAD #{i} ({}, {}) {} draws {} -> {}", d.x, d.y, d.path.rsplit('/').next().unwrap_or(""), rng.draws - before, if picked.is_some() { "kept" } else { "null" });
        }
        let Some(p) = picked else { continue };
        d.angle += p.angle;
        d.x += p.off.0;
        d.y += p.off.1;
        d.leaf = p.leaf;
        d.flip = d.mirrored ^ p.mirror ^ d.entry_flip;
        d.scale = d.entry_scale * p.scale;
        (d.counted, d.solid, d.side, d.league) = type_flags(gd, &d.kind);
        let no_covers = std::env::var_os("POE_NO_COVERS").is_some();
        d.dropped = (std::env::var_os("POE_NO_ANGLE_DROP").is_none() && dropped_by_type(&d.kind, d.angle))
            || (!no_covers && is_doodad_family(&d.kind) && d.covers.iter().any(|c| (c.0..c.2).contains(&d.x.div_euclid(TILE)) && (c.1..c.3).contains(&d.y.div_euclid(TILE))));
        out.push(d);
    }
    out
}

/// One object entry of a tile's `.tmd` file (`version 8` lines: cell `x y`, a flag, the world
/// position `x y z` when the flag is set (else only `z`), the angle, a quaternion, the flip flag, the scale, then the quoted `.ao` path, a
/// name, an `.fmt`, the object type, a number and the tile orientations the entry exists in).
#[derive(Debug, Clone, PartialEq)]
pub struct TmdEntry {
    pub x: i32,
    pub y: i32,
    /// World position within the tile (250 units per tile side).
    pub wx: f32,
    pub wy: f32,
    pub radians: f32,
    pub flip: bool,
    pub scale: f32,
    pub ao: String,
    pub kind: String,
    /// Tile orientations the entry exists in (entry `+0x1e`, bit per orientation; 0 = all):
    /// `I R90 R180 R270 FI FR90 FR180 FR270` are orientations 0..7.
    pub orientations: u8,
}

/// The tile's art object list (tile data `+0x178`, loaded by `0x141d082a0` only when tile data
/// `+0x16c` (the low byte of [`crate::tdt::Tdt::body_flags`]) lacks bit 8; it keeps the last
/// existing `.tmd` of the `.tgt`'s `;` segments). P3_1's `ambush_CenterHoleLarge_01.tdt` (flags
/// `0x9a`) has none: its `.tmd` beside the `.tgt` repeats the `.tdt`'s own objects. `0x141d07630`
/// places it: without body flag 0x200 the whole `.tgt` path with the extension swapped for `.tmd`
/// (`0x141d07eb0`), with it the per-sub-tile objects ([`subtile_objects`]).
fn art_entries(files: &dyn Files, tdt: &crate::tdt::Tdt) -> Vec<TmdEntry> {
    if tdt.body_flags & 8 != 0 {
        return Vec::new();
    }
    let Some(tgt) = tdt.tgt.as_deref() else { return Vec::new() };
    if tdt.body_flags & 0x200 != 0 {
        return subtile_objects(files, tdt, tgt);
    }
    tgt_tmd(tgt).map(|p| tmd_entries(files, &p)).unwrap_or_default()
}

/// A `.tgt` path with the extension swapped for `.tmd` (`0x141d083f0`).
fn tgt_tmd(tgt: &str) -> Option<String> {
    let tgt = tgt.trim();
    (tgt.len() > 4).then(|| format!("{}.tmd", &tgt[..tgt.len() - 4]))
}

/// `0x143512a00[o + mask * 8]` is the OR over the mask's bits `j` of bit `ORIENT_COMPOSE[j][o]`:
/// an entry orientation `j` seen through a sub-tile group turned by `o`.
const ORIENT_COMPOSE: [[u8; 8]; 8] = [
    [0, 3, 2, 1, 4, 5, 6, 7],
    [1, 0, 3, 2, 5, 6, 7, 4],
    [2, 1, 0, 3, 6, 7, 4, 5],
    [3, 2, 1, 0, 7, 4, 5, 6],
    [4, 5, 6, 7, 0, 3, 2, 1],
    [5, 6, 7, 4, 1, 0, 3, 2],
    [6, 7, 4, 5, 2, 1, 0, 3],
    [7, 4, 5, 6, 3, 2, 1, 0],
];

fn compose_mask(mask: u8, o: u8) -> u8 {
    (0..8).filter(|j| mask & (1 << j) != 0).fold(0, |m, j| m | 1 << ORIENT_COMPOSE[j][(o & 7) as usize])
}

/// The per-sub-tile objects of a tile with body flag 0x200 (tile data `+0x16d & 2`,
/// `0x141d07630`): row by row over the sub-tiles (row-major, the tile's width), each group origin
/// (`b1` without 2 joined left, 4 joined above, 8 skipped) places the `.tmd` of `.tgt` segment
/// `parts[0]` (`0x141d07c80`; skipped when missing), turned by the group's orientation `parts[1]`
/// within its span (`0x141d074a0`: 1 + the run of right neighbours with `b1 & 2`, 1 + the run
/// of lower neighbours with `b1 & 4`; swapped for odd orientations) and shifted by the origin
/// sub-tile times 250. The results are tile-level entries: cell = floor(world * 0.092), angle
/// as [`turn_angle`], flip toggled for orientations 4..7, the orientation mask composed through
/// `0x143512a00`. Not ported: the `alttiles` variant (`0x141d11c50`).
fn subtile_objects(files: &dyn Files, tdt: &crate::tdt::Tdt, tgt: &str) -> Vec<TmdEntry> {
    let segments: Vec<&str> = tgt.split(';').collect();
    let (w, h) = (tdt.size.0 as usize, tdt.size.1 as usize);
    let b1 = |x: usize, y: usize| tdt.subtiles.get(y * w + x).map_or(0, |s| s.b1);
    let mut out = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let Some(st) = tdt.subtiles.get(y * w + x) else { continue };
            if st.b1 & 0x0e != 0 {
                continue;
            }
            let Some(parts) = st.parts else { continue };
            let Some(path) = segments.get(parts[0] as usize).and_then(|t| tgt_tmd(t)) else { continue };
            let entries = tmd_entries(files, &path);
            if entries.is_empty() {
                continue;
            }
            let o = parts[1] & 7;
            let sw = 1 + (x + 1..w).take_while(|&i| b1(i, y) & 2 != 0).count() as i32;
            let sh = 1 + (y + 1..h).take_while(|&j| b1(x, j) & 4 != 0).count() as i32;
            let (sw, sh) = if o & 1 == 1 { (sh, sw) } else { (sw, sh) };
            for e in entries {
                let (tx, ty) = turn_world_f(e.wx, e.wy, sw, sh, o);
                let (wx, wy) = (x as f32 * 250.0 + tx, y as f32 * 250.0 + ty);
                out.push(TmdEntry {
                    x: (wx * 0.092).floor() as i32,
                    y: (wy * 0.092).floor() as i32,
                    wx,
                    wy,
                    radians: turn_angle(e.radians, o),
                    flip: e.flip ^ (o > 3),
                    orientations: compose_mask(e.orientations, o),
                    ..e
                });
            }
        }
    }
    out
}

/// The entries of a `.tmd` file (UTF-16 text or binary); empty when it is missing or not a `.tmd`.
pub fn tmd_entries(files: &dyn Files, path: &str) -> Vec<TmdEntry> {
    let Ok(bytes) = files.read(&path.to_ascii_lowercase()) else { return Vec::new() };
    if let Some(v) = bytes.first().copied().filter(|&v| v == 5 || v == 6) {
        return tmd_binary(&bytes, v).unwrap_or_default();
    }
    let text = crate::text::decode_utf16(&bytes);
    if !text.trim_start().starts_with("version") {
        return Vec::new();
    }
    text.lines().skip(1).filter_map(parse_tmd_line).collect()
}

/// A binary `.tmd` (`0x14291aab0`, first byte the version): u32 entry count, the entries, then a
/// UTF-16 string table (u32 length in chars, NUL-terminated strings addressed by char index).
/// Version 5 entries are 0x24 bytes: world `x y`, height, x-rotation, angle, two asset-name
/// strings, type string, `.ao` string (scale 1, no flip). Version 6 entries are 0x2d bytes: world
/// `x y`, height, two rotations, angle `+0x14`, scale `+0x18`, asset names `+0x1c/+0x20`, type
/// `+0x24`, `.ao` `+0x28`, flip byte `+0x2c`. Binary entries carry no orientation list (all
/// orientations).
fn tmd_binary(b: &[u8], v: u8) -> Option<Vec<TmdEntry>> {
    let u32_at = |o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]));
    let f32_at = |o: usize| u32_at(o).map(f32::from_bits);
    let n = u32_at(1)? as usize;
    let size = if v == 5 { 0x24 } else { 0x2d };
    let table_at = 5 + n * size;
    let chars = u32_at(table_at)? as usize;
    let table: Vec<u16> = b.get(table_at + 4..table_at + 4 + chars * 2)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let string = |i: u32| -> String {
        let i = i as usize;
        if i >= table.len() {
            return String::new();
        }
        let end = table[i..].iter().position(|&c| c == 0).map_or(table.len(), |p| i + p);
        String::from_utf16_lossy(&table[i..end])
    };
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let e = 5 + k * size;
        let (wx, wy) = (f32_at(e)?, f32_at(e + 4)?);
        let (radians, scale, kind, ao, flip) = if v == 5 {
            (f32_at(e + 0x10)?, 1.0, u32_at(e + 0x1c)?, u32_at(e + 0x20)?, false)
        } else {
            (f32_at(e + 0x14)?, f32_at(e + 0x18)?, u32_at(e + 0x24)?, u32_at(e + 0x28)?, *b.get(e + 0x2c)? != 0)
        };
        out.push(TmdEntry {
            x: (wx * 0.092).floor() as i32,
            y: (wy * 0.092).floor() as i32,
            wx,
            wy,
            radians,
            flip,
            scale,
            ao: if ao == u32::MAX { String::new() } else { string(ao) },
            kind: string(kind),
            orientations: 0,
        });
    }
    Some(out)
}

fn parse_tmd_line(line: &str) -> Option<TmdEntry> {
    let head = &line[..line.find('"')?];
    let nums: Vec<f32> = head.split_whitespace().map(|t| t.parse().ok()).collect::<Option<_>>()?;
    let quoted: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
    let n = nums.len();
    if n < 7 || quoted.len() < 4 {
        return None;
    }
    let custom = std::env::var("POE_DBG_TMD_NAMES").ok();
    let names: Vec<&str> = custom.as_deref().map(|v| v.split(',').collect()).unwrap_or_else(|| ORIENTATION_NAMES.to_vec());
    let tail = line.rsplit('"').next().unwrap_or("");
    let orientations = tail.split_whitespace().filter_map(|t| names.iter().position(|n| *n == t)).fold(0u8, |m, i| m | 1 << i);
    Some(TmdEntry {
        orientations,
        x: nums[0] as i32,
        y: nums[1] as i32,
        // The third number says whether a world `x y z` follows (else only a height): entries
        // without one sit at their cell's centre.
        wx: if nums[2] != 0.0 { nums[3] } else { (nums[0] + 0.5) * 250.0 / 23.0 },
        wy: if nums[2] != 0.0 { nums[4] } else { (nums[1] + 0.5) * 250.0 / 23.0 },
        radians: nums[n - 7],
        flip: nums[n - 2] != 0.0,
        scale: nums[n - 1],
        ao: quoted[0].to_string(),
        kind: quoted[3].to_string(),
    })
}

/// `0x14159a260`: a tile-local world position (250 units per tile) turned by orientation `o`
/// within `w × h` tiles (`w·250 − x` where cells use `w·23 − x − 1`), as the object's cell. The
/// client places tile objects at this position, so an entry on the tile's edge lands one cell
/// from where the cell formula ([`turn_cell`]) puts it.
fn turn_world(wx: f32, wy: f32, w: i32, h: i32, o: u8) -> (i32, i32) {
    let (x, y) = turn_world_f(wx, wy, w, h, o);
    ((x * 0.092).floor() as i32, (y * 0.092).floor() as i32)
}

/// [`turn_world`] before the cell step: the turned world position (table `0x142f02118`).
fn turn_world_f(wx: f32, wy: f32, w: i32, h: i32, o: u8) -> (f32, f32) {
    const T: [[usize; 3]; 9] = [[0, 1, 1], [1, 0, 1], [0, 0, 0], [1, 1, 0], [0, 0, 1], [1, 0, 0], [0, 1, 0], [1, 1, 1], [0, 1, 1]];
    let a = [w as f32 * 250.0 - wx, wx, h as f32 * 250.0 - wy, wy];
    let t = T[(o as usize).min(8)];
    (a[t[1] + t[0] * 2], a[t[2] + if t[0] == 0 { 2 } else { 0 }])
}

/// The tile-object pass (`0x141ff4030`, the generator's vtable `+0x28`, after the doodad pass):
/// row by row over the tile records that are a tile's corner (record `+0x34/+0x35` zero), the
/// objects of the tile's art list (tile data `+0x178`: the `.tmd` beside the tile's `.tgt`,
/// `0x141d083f0`) and then of its own list (`+0x180`: the `.tdt`'s `tmd`) are placed like room
/// doodads (`0x141ff43d0`): the `AOSet` pick draws from `rng`, continuing the doodad pass's
/// stream, the entry cell turns within the tile's size by the record's orientation
/// (`0x141fe7a50`). The objects do not enter the doodad hash. Not ported: the environment's
/// `alttiles/<env>/<env>_<name>.tmd` variant of the art list (`0x141d11c50`) and the per-sub-tile
/// objects of tiles with tile data `+0x16d & 2` (`0x141d07630`).
pub fn tile_objects(gd: &poe_data::GameData, files: &dyn Files, rng: &mut TerrainRng, map: &crate::generate::TileMap) -> Vec<Doodad> {
    let mut cache = AoCache::new(files);
    let mut out = Vec::new();
    let dbg_draws = std::env::var("POE_DBG_TMD_DRAWS").is_ok();
    if dbg_draws && rng.log.is_none() {
        rng.log = Some(Vec::new());
    }
    for ty in 0..map.height {
        for tx in 0..map.width {
            let Some(c) = map.get(tx, ty).filter(|c| c.offset == (0, 0)) else { continue };
            let Ok(tdt) = crate::generate::load_tdt(files, &c.path.to_ascii_lowercase()) else { continue };
            let (w, h) = (tdt.size.0 as i32, tdt.size.1 as i32);
            let o = c.orientation;
            let own = tdt.tmd.clone().unwrap_or_default();
            for (list, entries) in [("art".to_string(), art_entries(files, &tdt)), (own.clone(), tmd_entries(files, &own))] {
                for e in entries {
                    if e.ao.is_empty() || e.kind.is_empty() {
                        continue;
                    }
                    let before = rng.draws;
                    let logged = rng.log.as_ref().map_or(0, Vec::len);
                    let picked = pick(&mut cache, &e.ao, rng);
                    if dbg_draws && rng.draws != before {
                        let ns: Vec<(u32, u32)> = rng.log.as_ref().map(|l| l[logged..].to_vec()).unwrap_or_default();
                        eprintln!("TMDDRAW tile ({tx},{ty}) {} n {:?} {}", c.path.rsplit('/').next().unwrap_or(""), ns, e.ao.rsplit('/').next().unwrap_or(""));
                    }
                    let Some(p) = picked else { continue };
                    // `0x141ff43d0` checks the orientation mask after the pick (the draws happen).
                    if e.orientations != 0 && e.orientations & (1 << (o & 7)) == 0 {
                        continue;
                    }
                    let (x, y) = match std::env::var("POE_DBG_TMD_PLACE").as_deref() {
                        Ok("world") => turn_world(e.wx, e.wy, w, h, o),
                        Ok("hybrid") => {
                            let (cx, cy) = turn_cell(e.x, e.y, w, h, o);
                            let (wx, wy) = turn_world(e.wx, e.wy, w, h, o);
                            let edge = |v: f32, n: i32| v == 0.0 || v == n as f32 * 250.0;
                            // Which output axis comes from the entry's x: odd orientations swap.
                            let (from_x_edge, from_y_edge) = (edge(e.wx, w), edge(e.wy, h));
                            let swap = o & 1 == 1 && o < 8;
                            let (ex_edge, ey_edge) = if swap { (from_y_edge, from_x_edge) } else { (from_x_edge, from_y_edge) };
                            (if ex_edge { wx } else { cx }, if ey_edge { wy } else { cy })
                        }
                        _ => turn_cell(e.x, e.y, w, h, o),
                    };
                    if std::env::var("POE_DBG_TMD_LIST").is_ok() {
                        eprintln!("TMDOBJ tile ({tx},{ty}) {} list {} kind {}", c.path, list.rsplit('/').next().unwrap_or(""), e.kind);
                    }
                    if std::env::var("POE_DBG_TMD_POS").is_ok() && (x, y) != turn_world(e.wx, e.wy, w, h, o) {
                        eprintln!("TMDPOS tile ({tx},{ty}) o{o} entry ({},{}) world ({},{}) cell {:?} world-cell {:?} solid-kind {} {}", e.x, e.y, e.wx, e.wy, (x, y), turn_world(e.wx, e.wy, w, h, o), e.kind.rsplit('/').next().unwrap_or(""), c.path.rsplit('/').next().unwrap_or(""));
                    }
                    let mut d = Doodad {
                        x: x + tx as i32 * TILE + p.off.0,
                        y: y + ty as i32 * TILE + p.off.1,
                        angle: turn_angle(e.radians, o) + p.angle,
                        path: e.ao,
                        kind: e.kind,
                        counted: false,
                        solid: false,
                        side: 0,
                        league: false,
                        covers: Vec::new(),
                        dropped: false,
                        group: tdt.object_group,
                        placed: true,
                        leaf: p.leaf,
                        mirrored: (4..8).contains(&o),
                        entry_flip: e.flip,
                        entry_scale: e.scale,
                        flip: false,
                        scale: e.scale * p.scale,
                    };
                    d.flip = d.mirrored ^ p.mirror ^ d.entry_flip;
                    let (_, solid, side, league) = type_flags(gd, &d.kind);
                    (d.solid, d.side, d.league) = (solid, side, league);
                    d.dropped = std::env::var_os("POE_NO_ANGLE_DROP").is_none() && dropped_by_type(&d.kind, d.angle);
                    out.push(d);
                }
            }
        }
    }
    out
}

/// `0x141fe8420`: the object types (MiscObjects `ObjectHASH32` keys) a later covering placement
/// removes from an earlier room.
const DOODAD_FAMILY: [&str; 15] = [
    "Metadata/MiscellaneousObjects/Doodad",
    "Metadata/MiscellaneousObjects/DoodadInvisible",
    "Metadata/MiscellaneousObjects/DoodadNoBlocking",
    "Metadata/MiscellaneousObjects/DoodadNoOverlap",
    "Metadata/MiscellaneousObjects/DoodadNoOverlap3",
    "Metadata/MiscellaneousObjects/TileUpperDoodad",
    "Metadata/MiscellaneousObjects/TileUpperDoodadNoOverlap",
    "Metadata/MiscellaneousObjects/TileUpperDoodad_ExtendDistance",
    "Metadata/Terrain/Gallows/Act2/2_town/Objects/DoodadScrollNoJostle",
    "Metadata/Terrain/Gallows/Act2/2_town/Objects/DoodadScrollNoJostle_extend_tallwall",
    "Metadata/Terrain/Gallows/Act2/2_town/Objects/DoodadNoScrollNoJostle",
    "Metadata/Terrain/Gallows/Act2/2_town/Objects/DoodadNoScrollNoJostle_extend_tallwall",
    "Metadata/MiscellaneousObjects/DoodadNoWobble",
    "Metadata/MiscellaneousObjects/DoodadLayerDoodad",
    "Metadata/MiscellaneousObjects/AntiTallWallDoodad",
];

fn is_doodad_family(kind: &str) -> bool {
    DOODAD_FAMILY.iter().any(|k| k.eq_ignore_ascii_case(kind))
}

/// The angle checks of `0x141ff3490` and `0x141ff43d0`: with `a = angle - 0.7603982` (+2pi when
/// negative), `AntiTallWallDoodad` (`0x141fe8860`) is dropped for `a >= 3.1915927` and the
/// upper-tile types (`0x141fe86f0`) for `a < 3.1915927`.
fn dropped_by_type(kind: &str, angle: f32) -> bool {
    const UPPER: [&str; 5] = [
        "Metadata/MiscellaneousObjects/TileUpperDoodad",
        "Metadata/MiscellaneousObjects/TileUpperDoodadNoOverlap",
        "Metadata/MiscellaneousObjects/TileUpperDoodad_ExtendDistance",
        "Metadata/Terrain/Gallows/Act2/2_town/Objects/DoodadScrollNoJostle_extend_tallwall",
        "Metadata/Terrain/Gallows/Act2/2_town/Objects/DoodadNoScrollNoJostle_extend_tallwall",
    ];
    let mut a = angle - 0.7603982;
    if a < 0.0 {
        a += std::f32::consts::TAU;
    }
    (kind.eq_ignore_ascii_case("Metadata/MiscellaneousObjects/AntiTallWallDoodad") && a >= 3.1915927) || (UPPER.iter().any(|k| k.eq_ignore_ascii_case(kind)) && a < 3.1915927)
}

/// The no-overlap object types (`0x141fe8040`: three type hashes). The generator defers their
/// objects (`0x141ff4a50`) and places them in `doodads_post` ([`no_overlap`]). The one with hash
/// `0x91b983f1` (`DoodadNoOverlap3`) goes to a separate list, the others into groups.
const NO_OVERLAP: [&str; 3] = [
    "Metadata/MiscellaneousObjects/DoodadNoOverlap",
    "Metadata/MiscellaneousObjects/DoodadNoOverlap3",
    "Metadata/MiscellaneousObjects/TileUpperDoodadNoOverlap",
];
/// The no-overlap type of hash `0x91b983f1` (MiscObjects `ObjectHASH32`; its own list in
/// `0x141ff4a50`).
const NO_OVERLAP_LISTED: &str = "Metadata/MiscellaneousObjects/DoodadNoOverlap3";

fn is_no_overlap(kind: &str) -> bool {
    NO_OVERLAP.iter().any(|k| k.eq_ignore_ascii_case(kind))
}

/// `doodads_post` (`0x141ff4cd0`): the deferred no-overlap objects of one pass are placed. It runs
/// twice, each time with an empty placed set: for the room doodads at the end of
/// `TerrainGen_Doodads` (`0x141ff6e88`, before the tile-object pass draws), and for the tile
/// objects after that pass (`0x141fff1bc`). Groups go in descending key order (the map's vector
/// from its end; room doodads are one group): `below(n)`, a Fisher-Yates shuffle (`uniform(n - k)`
/// for k = 0.., swapping element k with k + draw), then each object is placed unless a placed one
/// lies within its 3 x 3 cells (`0x1420006e0`; the placed cells are a set, `0x141fffb30`). Objects
/// of the listed type are placed afterwards, in order, unless a placed object or another listed
/// object lies within their 3 x 3 cells. Only objects the generator's filter keeps take part (the
/// no-overlap types have no side flag, so on either side).
pub fn no_overlap(rng: &mut TerrainRng, list: &mut [Doodad], tile_objects: bool) {
    let mut groups: std::collections::BTreeMap<u8, Vec<usize>> = Default::default();
    let mut listed: Vec<usize> = Vec::new();
    for (i, d) in list.iter_mut().enumerate() {
        if d.leaf.is_empty() || !is_no_overlap(&d.kind) || d.league || d.dropped || d.side != 0 {
            continue;
        }
        d.placed = false;
        if d.kind.eq_ignore_ascii_case(NO_OVERLAP_LISTED) {
            listed.push(i);
        } else {
            groups.entry(if tile_objects { d.group } else { 0 }).or_default().push(i);
        }
    }
    let mut placed: std::collections::HashSet<(i32, i32)> = Default::default();
    let near = |set: &std::collections::HashSet<(i32, i32)>, (x, y): (i32, i32)| (-1..=1).any(|dy| (-1..=1).any(|dx| set.contains(&(x + dx, y + dy))));
    let dbg = std::env::var_os("POE_DBG_NO_OVERLAP").is_some();
    for (_, mut members) in groups.into_iter().rev() {
        let n = members.len();
        let first = rng.below(n as u32);
        if dbg {
            eprintln!("NO_OVERLAP group n {n} below {first}");
        }
        for k in 0..n {
            let j = rng.uniform((n - k) as u32) as usize;
            if dbg {
                eprintln!("NO_OVERLAP uniform {} -> {j}", n - k);
            }
            members.swap(k, k + j);
        }
        if dbg {
            for &m in &members {
                let d = &list[m];
                eprintln!("NO_OVERLAP order tiles {tile_objects} ({}, {}) {}", d.x, d.y, d.leaf.rsplit('/').next().unwrap_or(""));
            }
        }
        for m in members {
            let c = (list[m].x, list[m].y);
            if near(&placed, c) {
                continue;
            }
            placed.insert(c);
            list[m].placed = true;
        }
    }
    let cells: Vec<(i32, i32)> = listed.iter().map(|&m| (list[m].x, list[m].y)).collect();
    for (k, &m) in listed.iter().enumerate() {
        let (x, y) = cells[k];
        if near(&placed, (x, y)) {
            continue;
        }
        let close = cells.iter().filter(|&&(cx, cy)| (cx - x).abs() <= 1 && (cy - y).abs() <= 1).count();
        if close >= 2 {
            continue;
        }
        list[m].placed = true;
    }
}

/// The doodad hash of the doodads that count (`+0x2d0 == 0` on their type), in any order.
pub fn doodad_hash<'a>(doodads: impl IntoIterator<Item = &'a Doodad>) -> u32 {
    hash::doodad_hash(doodads.into_iter().map(|d| (d.x, d.y, d.path.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_orientation_lists() {
        // `street_short_st_set.ao` (G3_8): a list allows only the named orientations.
        let set = |v: &str| parse_ao(&format!("version 3\nAOSet\n{{\n\trandom_orientation = \"{v}\"\n\tao = \"100 a.ao\"\n}}\n")).1.unwrap().orientation_mask;
        assert_eq!(set("I R180 FI FR180"), 0x55);
        assert_eq!(set("true"), 0xff);
        assert_eq!(orientation_pick(0x55, 3), 6);
    }

    #[test]
    fn turns_are_bijections() {
        // A 2×1 tile room: the 8 orientations send (3, 5) to distinct, in-range cells.
        let mut seen = std::collections::HashSet::new();
        for o in 0..8 {
            let (x, y) = turn_cell(3, 5, 2, 1, o);
            let (rw, rh) = if o & 1 == 1 && !(4..8).contains(&o) || matches!(o, 3 | 5 | 7) { (23, 46) } else { (46, 23) };
            assert!(x >= 0 && x < rw && y >= 0 && y < rh, "o{o}: {x},{y}");
            seen.insert((x, y));
        }
        assert!(seen.len() >= 6);
        assert_eq!(turn_cell(3, 5, 2, 1, 0), (3, 5));
    }
}
