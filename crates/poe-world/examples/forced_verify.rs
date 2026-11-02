//! Replays a batch of client-generated layouts with the port and compares them: the check behind
//! the 50-seed matrix (docs/plans/terrain-all-areas.md, WP7.16).
//!
//! `cargo run --release -p poe-world --example forced_verify -- <batch-results.tsv>
//! [--out dumps/<version>/terrain/vectors/forced-results.tsv] [--threads N] [--grid-fails FILE]`
//!
//! Input (`dumps/<version>/terrain/batch/<run>/batch-results.tsv`, written by the client batch,
//! `#` lines and blanks skipped): `area  seed  topology  orientation  status  tile_hash
//! doodad_hash  width  height  ms  detail`, topology 1-based as `map_force_topology`,
//! orientation 0-7. Rows whose `status` is not `ok` (the client itself failed or timed out) carry
//! no reference and are skipped with a count. `width`/`height` of 0 or `-` mean unknown.
//!
//! Every remaining row is generated with the same forced choice (`generate_with`, topology and
//! orientation set) on threads; a layout is generated once however many rows name it. Output
//! rows, in input order: `area  seed  topology  orientation  match|fail  detail`, the format
//! `cargo xtask terrain-matrix` reads as `forced-results.tsv`. The detail of a `fail` is the
//! first difference: `map WxH vs client WxH`, the generator's error (`error: ...`, `panic`,
//! `timeout after Ns`), or `tile hash ours X client Y`. A tile-hash match also compares the doodad
//! hash: the detail says `doodads match` or `doodads differ: ours X client Y`; the row stays
//! `match` either way (the tile result decides), and stderr ends with the counts.
//!
//! - `--out FILE`: merge into FILE instead of printing: rows of FILE with the same area, seed,
//!   topology and orientation are replaced, other rows are kept (so batches accumulate).
//! - `--threads N` (default: all cores).
//! - `POE_FORCED_BUDGET=<seconds>` (default 120): per-job time budget; a job over it is
//!   abandoned (its thread keeps spinning) and reported as a fail. After eight hung jobs
//!   (`POE_FORCED_MAX_HUNG`) the run stops; each hung job is logged to stderr.
//!
//! A per-area summary (`matched/checked`) goes to stderr.
//!
//! Collision grids: a row with a 12th column `grid_hash` (FNV-1a 64 of the client's `grids.bin`
//! bytes, written by the batch since the grid-hash column was added) also builds the port's grid
//! (client side) when the tile hash matches, and the detail gains `grids match` or `grids differ:
//! ours X client Y`. `--grid-fails FILE` writes the differing layouts as `area seed topology
//! orientation` lines (an `export.txt` for a re-run with full exports).

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use poe_data::GameData;
use poe_world::{area::Forced, generate::CachedFiles, rng::TerrainRng};

/// One client row.
struct Job {
    area: String,
    seed: u32,
    topology: u32,
    orientation: u8,
    tile_hash: String,
    /// The client's doodad hash.
    doodad_hash: String,
    size: Option<(u32, u32)>,
    /// The client's grid hash (column 12), when the batch wrote one.
    grid_hash: Option<String>,
}

/// FNV-1a 64, as the client batch hashes its `grids.bin` bytes.
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

/// What the port made of one (area, seed, topology, orientation).
#[derive(Clone)]
enum Ours {
    Done { hash: String, doodad_hash: String, size: (u32, u32), grid_hash: Option<String> },
    Error(String),
    Panic,
    Timeout(u64),
}

fn parse_batch(text: &str) -> (Vec<Job>, usize) {
    let (mut jobs, mut skipped) = (Vec::new(), 0);
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 9 {
            continue;
        }
        let (Ok(seed), Ok(topology), Ok(orientation)) = (f[1].parse(), f[2].parse(), f[3].parse()) else { continue };
        if f[4] != "ok" {
            skipped += 1;
            continue;
        }
        let size = match (f[7].parse::<u32>(), f[8].parse::<u32>()) {
            (Ok(w), Ok(h)) if w > 0 && h > 0 => Some((w, h)),
            _ => None,
        };
        jobs.push(Job { area: f[0].into(), seed, topology, orientation, tile_hash: f[5].into(), doodad_hash: f[6].into(), size, grid_hash: f.get(11).map(|h| h.trim()).filter(|h| !h.is_empty() && h.bytes().any(|b| b != b'0')).map(String::from) });
    }
    (jobs, skipped)
}

/// The first difference between the client's row and ours, or `None` when they agree.
fn compare(job: &Job, ours: &Ours) -> Option<String> {
    match ours {
        Ours::Error(e) => Some(format!("error: {e}")),
        Ours::Panic => Some("panic".into()),
        Ours::Timeout(s) => Some(format!("timeout after {s}s")),
        Ours::Done { hash, size, .. } => {
            if let Some((w, h)) = job.size
                && (w, h) != *size
            {
                return Some(format!("map {}x{} vs client {w}x{h}", size.0, size.1));
            }
            (*hash != job.tile_hash).then(|| format!("tile hash ours {hash} client {}", job.tile_hash))
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut positional = Vec::new();
    let mut out: Option<String> = None;
    let mut grid_fails_out: Option<String> = None;
    let mut threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => out = Some(args.next().ok_or("--out needs a file")?),
            "--grid-fails" => grid_fails_out = Some(args.next().ok_or("--grid-fails needs a file")?),
            "--threads" => threads = args.next().ok_or("--threads needs a number")?.parse()?,
            _ => positional.push(a),
        }
    }
    let path = positional.first().ok_or("usage: forced_verify <batch-results.tsv> [--out forced-results.tsv] [--threads N]")?;
    let budget: u64 = std::env::var("POE_FORCED_BUDGET").ok().map(|s| s.parse()).transpose()?.unwrap_or(120);
    let max_hung: usize = std::env::var("POE_FORCED_MAX_HUNG").ok().map(|s| s.parse()).transpose()?.unwrap_or(8);
    let (jobs, skipped) = parse_batch(&std::fs::read_to_string(path)?);
    let gd: &'static GameData = Box::leak(Box::new(GameData::open(
        Path::new("GameFiles"),
        Path::new("third_party/dat-schema/schema-poe2.min.json"),
    )?));
    let files: &'static CachedFiles<'static> = Box::leak(Box::new(CachedFiles::new(gd.files())));
    std::panic::set_hook(Box::new(|_| {}));

    // One generate per distinct layout.
    let mut keys: Vec<(String, u32, u32, u8)> = Vec::new();
    let mut index: HashMap<(String, u32, u32, u8), usize> = HashMap::new();
    for j in &jobs {
        let k = (j.area.clone(), j.seed, j.topology, j.orientation);
        index.entry(k.clone()).or_insert_with(|| {
            keys.push(k);
            keys.len() - 1
        });
    }
    // The client's tile hash per layout that carries a grid hash: the grid is built only then.
    let want_grid: HashMap<(String, u32, u32, u8), String> =
        jobs.iter().filter(|j| j.grid_hash.is_some()).map(|j| ((j.area.clone(), j.seed, j.topology, j.orientation), j.tile_hash.clone())).collect();
    let want_grid = &want_grid;
    eprintln!("{} rows ({skipped} skipped: client status not ok), {} distinct layouts, {threads} threads", jobs.len(), keys.len());
    let next = AtomicUsize::new(0);
    let hung = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Ours>>> = Mutex::new(vec![None; keys.len()]);
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((area, seed, topology, orientation)) = keys.get(i).cloned() else { break };
                    let key = (area.clone(), seed, topology, orientation);
                    let (tx, rx) = mpsc::channel();
                    let area_c = area.clone();
                    let grid_for = want_grid.get(&key).cloned();
                    std::thread::spawn(move || {
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let mut rng = TerrainRng::new(seed);
                            let forced = Forced { topology: Some(topology), orientation: Some(orientation) };
                            poe_world::area::generate_with_files(gd, files, &area, &mut rng, Some(forced)).map(|g| {
                                let hash = g.map.tile_hash().to_string();
                                let grid_hash = grid_for.filter(|t| *t == hash).and_then(|_| {
                                    let grid = poe_world::collision::build(&g.map, files, g.doodads.iter().chain(&g.tile_objects), poe_world::doodads::Side::Client).ok()?;
                                    // `POE_DBG_GRID_DUMP=DIR`: the port's grid (`area_seed_topology_orientation.bin`)
                                    // and its solid placed objects (`.objects.tsv`: x y angle flip leaf) per layout.
                                    if let Some(d) = std::env::var_os("POE_DBG_GRID_DUMP") {
                                        let stem = std::path::Path::new(&d).join(format!("{area_c}_{seed}_{topology}_{orientation}"));
                                        let _ = std::fs::write(stem.with_extension("bin"), grid.pwg1());
                                        let objs: String = g.doodads.iter().chain(&g.tile_objects).filter(|o| o.placed && o.solid).map(|o| format!("{}\t{}\t{:.4}\t{}\t{}\n", o.x, o.y, o.angle, o.flip, o.leaf)).collect();
                                        let _ = std::fs::write(stem.with_extension("objects.tsv"), objs);
                                    }
                                    Some(format!("{:016x}", fnv1a64(&grid.pwg1())))
                                });
                                Ours::Done { hash, doodad_hash: g.doodad_hash().to_string(), size: (g.map.width, g.map.height), grid_hash }
                            })
                        }));
                        let _ = tx.send(match r {
                            Ok(Ok(o)) => o,
                            Ok(Err(e)) => Ours::Error(e.to_string().replace(['\n', '\t'], " ")),
                            Err(_) => Ours::Panic,
                        });
                    });
                    let ours = rx.recv_timeout(Duration::from_secs(budget)).unwrap_or_else(|_| {
                        hung.fetch_add(1, Ordering::Relaxed);
                        eprintln!("timeout after {budget}s: {}\t{}\t{}\t{}", key.0, key.1, key.2, key.3);
                        Ours::Timeout(budget)
                    });
                    results.lock().unwrap()[i] = Some(ours);
                    if (i + 1) % 5000 == 0 {
                        eprintln!("{} of {} layouts done", i + 1, keys.len());
                    }
                    if hung.load(Ordering::Relaxed) >= max_hung {
                        eprintln!("{max_hung} jobs hung; stopping");
                        std::process::exit(2);
                    }
                }
            });
        }
    });
    let results = results.into_inner().unwrap();

    let mut rows: Vec<((String, u32, u32, u8), String)> = Vec::new();
    let mut per_area: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
    let (mut doodads_match, mut doodads_differ) = (0u32, 0u32);
    let (mut grids_match, mut grids_differ) = (0u32, 0u32);
    let mut grid_fails: Vec<String> = Vec::new();
    for j in &jobs {
        let ours = results[index[&(j.area.clone(), j.seed, j.topology, j.orientation)]].clone().expect("every layout was generated");
        let e = per_area.entry(j.area.as_str()).or_default();
        e.1 += 1;
        let line = match compare(j, &ours) {
            None => {
                e.0 += 1;
                // Tile hash and size match: the doodad hash is reported but does not decide
                // match/fail (the matrix reads the tile result).
                match &ours {
                    Ours::Done { doodad_hash, .. } if *doodad_hash == j.doodad_hash => {
                        doodads_match += 1;
                        "match\tdoodads match".to_string()
                    }
                    Ours::Done { doodad_hash, .. } => {
                        doodads_differ += 1;
                        format!("match\tdoodads differ: ours {doodad_hash} client {}", j.doodad_hash)
                    }
                    _ => "match".to_string(),
                }
            }
            Some(d) => format!("fail\t{d}"),
        };
        let line = match (&ours, &j.grid_hash) {
            (Ours::Done { grid_hash: Some(g), .. }, Some(c)) if g == c => {
                grids_match += 1;
                format!("{line}; grids match")
            }
            (Ours::Done { grid_hash: Some(g), .. }, Some(c)) => {
                grids_differ += 1;
                grid_fails.push(format!("{} {} {} {}", j.area, j.seed, j.topology, j.orientation));
                format!("{line}; grids differ: ours {g} client {c}")
            }
            _ => line,
        };
        rows.push(((j.area.clone(), j.seed, j.topology, j.orientation), line));
    }
    eprintln!("tile-matching rows: doodad hash matches {doodads_match}, differs {doodads_differ}");
    eprintln!("grid hashes: match {grids_match}, differ {grids_differ}");
    if let Some(file) = &grid_fails_out {
        grid_fails.sort();
        grid_fails.dedup();
        std::fs::write(file, grid_fails.iter().map(|l| format!("{l}\n")).collect::<String>())?;
        eprintln!("wrote {} differing layouts to {file}", grid_fails.len());
    }
    for (a, (m, n)) in &per_area {
        eprintln!("{a}\t{m}/{n}");
    }
    let render = |k: &(String, u32, u32, u8), l: &str| format!("{}\t{}\t{}\t{}\t{l}", k.0, k.1, k.2, k.3);
    match out {
        None => rows.iter().for_each(|(k, l)| println!("{}", render(k, l))),
        Some(file) => {
            // Merge: keep the old rows (and comments) whose layout this batch did not redo.
            let new: HashMap<&(String, u32, u32, u8), ()> = rows.iter().map(|(k, _)| (k, ())).collect();
            let mut text = String::new();
            if let Ok(old) = std::fs::read_to_string(&file) {
                for l in old.lines() {
                    let f: Vec<&str> = l.split('\t').collect();
                    let redone = f.len() >= 5
                        && matches!((f[1].parse::<u32>(), f[2].parse::<u32>(), f[3].parse::<u8>()), (Ok(s), Ok(t), Ok(o)) if new.contains_key(&(f[0].to_string(), s, t, o)));
                    if !redone && !l.trim().is_empty() {
                        text.push_str(l);
                        text.push('\n');
                    }
                }
            }
            if text.is_empty() {
                text.push_str("# area\tseed\ttopology (1-based)\torientation\tmatch|fail\tdetail  (forced_verify and forced-layout client traces; see docs/plans/terrain-all-areas.md)\n");
            }
            let mut seen = std::collections::HashSet::new();
            for (k, l) in &rows {
                if seen.insert(k) {
                    text.push_str(&render(k, l));
                    text.push('\n');
                }
            }
            std::fs::write(&file, text)?;
            eprintln!("merged {} rows into {file}", seen.len());
        }
    }
    Ok(())
}
