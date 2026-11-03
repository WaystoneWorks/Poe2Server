//! Offline check of the movement port: replays what a run's server sent (movement trace) through
//! the client model and compares its grid per frame with the client's (`frame #` and `LINE` lines
//! of the DLL's log, `POE_HOOK_MOVEMENT_LOG=1`).
//!
//! `cargo run -p poe-game --example line_compare -- TRACE LOG PORT [--show N] [--from F] [--to F]`

use poe_game::clientmotion::{ClientMotion, Curve, LoggedLine, MotionCurves};
use poe_game::grid::Grid;
use poe_game::movement::{Open, Walkable};
use std::collections::BTreeMap;

const DT: f32 = 0.033;
const IS_SPRINTING: i64 = 22342;
const SPRINT_SPEED: i64 = 174;
const MUST_SPRINT: i64 = 22426;
/// `offset_scale` of the class's `.ao`.
const OFFSET_SCALE: f32 = 0.9;

#[derive(Default, Clone)]
struct ClientFrame {
    grid: Option<(i32, i32)>,
    line: Option<LoggedLine>,
}

impl ClientFrame {
    fn cell(&self) -> Option<(i32, i32)> {
        match self.line {
            Some(l) if l.swap => Some((l.cur.1, l.cur.0)),
            Some(l) => Some(l.cur),
            None => self.grid,
        }
    }
    fn end(&self) -> Option<(i32, i32)> {
        self.line.map(|l| if l.swap { (l.e.1, l.e.0) } else { l.e })
    }
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace().find_map(|w| w.strip_prefix(key))
}

fn pair_i(line: &str, key: &str) -> (i32, i32) {
    let v: Vec<i32> = field(line, key).unwrap().split(',').map(|x| x.parse().unwrap()).collect();
    (v[0], v[1])
}

fn pair_f(line: &str, key: &str) -> (f32, f32) {
    let v: Vec<f32> = field(line, key).unwrap().split(',').map(|x| x.parse().unwrap()).collect();
    (v[0], v[1])
}

fn num(line: &str, key: &str) -> f32 {
    field(line, key).unwrap().parse().unwrap()
}

fn load_log(path: &str) -> BTreeMap<u64, ClientFrame> {
    let text = std::fs::read_to_string(path).expect("log");
    let lines: Vec<&str> = text.lines().collect();
    let first = lines.iter().rposition(|l| l.contains("frame #1 dt")).unwrap_or(0);
    let mut out: BTreeMap<u64, ClientFrame> = BTreeMap::new();
    for l in &lines[first..] {
        if let Some(i) = l.find("frame #") {
            let rest = &l[i + 7..];
            let n: u64 = rest.split_whitespace().next().unwrap().parse().unwrap();
            if let Some(g) = rest.find("grid (") {
                let s = rest[g + 6..].trim_end_matches(')');
                let v: Vec<i32> = s.split(", ").filter_map(|x| x.parse().ok()).collect();
                out.entry(n).or_default().grid = Some((v[0], v[1]));
            }
        } else if l.contains(" LINE frame=") {
            let n: u64 = field(l, "frame=").unwrap().parse().unwrap();
            out.entry(n).or_default().line = Some(LoggedLine {
                s: pair_i(l, "s="),
                p: pair_f(l, "p="),
                q: pair_i(l, "q="),
                e: pair_i(l, "e="),
                cur: pair_i(l, "cur="),
                flag: field(l, "flag=") == Some("1"),
                swap: field(l, "swap=") == Some("1"),
                sign: num(l, "sign=") as i32,
                slope: num(l, "slope="),
                minacc: num(l, "minacc="),
                len: num(l, "len="),
                t: num(l, "T="),
                el: num(l, "el="),
                thr: num(l, "thr="),
                acc: num(l, "acc="),
                pend: num(l, "pend="),
            });
        }
    }
    out
}

/// `curve name` from an `.amd` text file (UTF-16).
fn amd_curve(path: &str, name: &str) -> Curve {
    let raw = std::fs::read(path).expect("amd");
    let words: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let text = String::from_utf16_lossy(&words).replace('\r', "");
    Curve::from_amd(&text, name).expect("curve")
}

#[derive(Clone, Debug)]
enum Out {
    Dir(i32, i32, i32, i32),
    Start(i32, i32, i32, i32, u32),
    Stats(Vec<(i64, i64)>),
    Target(i32, i32),
    SprintReleased,
    /// The player's key (input, not sent to the client).
    Key(bool),
}

fn load_trace(path: &str, port: &str) -> BTreeMap<u64, Vec<Out>> {
    let text = std::fs::read_to_string(path).expect("trace");
    let mut out: BTreeMap<u64, Vec<Out>> = BTreeMap::new();
    let mut tick = 0u64;
    for l in text.lines() {
        let p: Vec<&str> = l.split('\t').collect();
        if p.len() < 3 || p[1] != port {
            continue;
        }
        let i = |k: usize| p[k].parse::<i32>().unwrap();
        match p[2] {
            "tick" => tick = p[3].parse().unwrap(),
            "out-dir" => out.entry(tick).or_default().push(Out::Dir(i(3), i(4), i(5), i(6))),
            "out-start" => {
                let skill = u32::from_str_radix(p[7].trim_start_matches("0x"), 16).unwrap();
                out.entry(tick).or_default().push(Out::Start(i(3), i(4), i(5), i(6), skill))
            }
            "out-sprint" if p[3] == "false" => out.entry(tick).or_default().push(Out::SprintReleased),
            "in-SprintInput" => out.entry(tick).or_default().push(Out::Key(l.contains("held: true"))),
            "out-target" => out.entry(tick).or_default().push(Out::Target(i(3), i(4))),
            "out-stats" => {
                let s = p[3].replace(['[', ']', '(', ')', ' '], "");
                let v: Vec<i64> = s.split(',').filter_map(|x| x.parse().ok()).collect();
                out.entry(tick)
                    .or_default()
                    .push(Out::Stats(v.chunks(2).map(|c| (c[0], c[1])).collect()));
            }
            _ => {}
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (trace, log, port) = (&args[1], &args[2], &args[3]);
    let opt = |k: &str| args.iter().position(|a| a == k).map(|i| args[i + 1].parse::<u64>().unwrap());
    let show = opt("--show").unwrap_or(30);
    // `--free`: never reload the client's state (logs without `LINE`): only the replayed snaps
    // correct the model, so one error runs on until the next snap.
    let free = args.iter().any(|a| a == "--free");
    let (from, to) = (opt("--from").unwrap_or(0), opt("--to").unwrap_or(u64::MAX));
    let amd = std::env::var("ROLL_AMD").expect("ROLL_AMD = path of basedodgeroll/onerig.amd");
    let sprint_amd = std::env::var("SPRINT_AMD").expect("SPRINT_AMD = path of basesprint/onerig.amd");
    let curves = std::sync::Arc::new(MotionCurves {
        roll: amd_curve(&amd, "dodge_unarmed_01"),
        roll_back: amd_curve(&amd, "dodge_unarmed_bwd_01"),
        roll_sprint: amd_curve(&amd, "dodge_unarmed_01_sprinting"),
        roll_back_sprint: amd_curve(&amd, "dodge_unarmed_bwd_01_sprinting"),
        sprint_end: amd_curve(&sprint_amd, "sprint_end_unarmed_01"),
    });
    let frames = load_log(log);
    let msgs = load_trace(trace, port);
    // `--grids DIR`: the session's area export; the grid with the fewest walked cells in walls.
    let mut best: Option<(usize, String, Grid)> = None;
    if let Some(i) = args.iter().position(|a| a == "--grids") {
        let mut paths: Vec<_> = std::fs::read_dir(&args[i + 1]).unwrap().flatten().map(|e| e.path()).collect();
        paths.sort();
        for dir in paths {
            let Some(g) = std::fs::read(dir.join("grids.bin")).ok().and_then(|d| Grid::parse(&d)) else { continue };
            let walls = frames.values().filter_map(|f| f.cell()).filter(|c| g.nibble(c.0, c.1) == 0).count();
            println!("{} walled cells {walls}", dir.display());
            if best.as_ref().is_none_or(|b| walls <= b.0) {
                best = Some((walls, dir.display().to_string(), g));
            }
        }
    }
    let open = Open;
    let terrain: &dyn Walkable = match &best {
        Some((_, _, g)) => g,
        None => &open,
    };

    let Some((&start, first)) = frames.iter().find(|(_, f)| f.cell().is_some()) else {
        return;
    };
    let mut m = ClientMotion::at_cell(first.cell().unwrap(), 37.0);
    let last = *frames.keys().last().unwrap();
    let (mut ok, mut bad, mut snaps, mut end_ok, mut end_bad, mut shown) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut roll_end: Option<u64> = None;
    let (mut released, mut ignored) = (false, false);
    m.curves = Some(curves);
    m.offset_scale = OFFSET_SCALE;
    let mut prev = first.cell().unwrap();
    for n in start..=last {
        if roll_end == Some(n) {
            m.end_roll();
            roll_end = None;
        }
        m.positioned_tick(DT, terrain);
        let mut notes = String::new();
        // The trace lists a frame's messages in code order, the client's batch has the `0x64` first.
        let mut batch: Vec<Out> = msgs.get(&n).cloned().unwrap_or_default();
        batch.sort_by_key(|o| match o {
            Out::Dir(..) => 0,
            Out::SprintReleased => 1,
            Out::Key(_) => 3,
            _ => 2,
        });
        for o in &batch {
            if let Out::Key(held) = o {
                released |= !*held;
                continue;
            }
            match *o {
                Out::Dir(x, y, cx, cy) => {
                    if (cx, cy) != m.grid() && (x, y) != (0, 0) {
                        notes += &format!(" snap{:?}", m.grid());
                        m.snap((cx, cy));
                        snaps += 1;
                    }
                    m.set_axes((x, y), terrain);
                    notes += &format!(" dir{x},{y}");
                }
                Out::Start(cx, cy, tx, ty, skill) => {
                    if skill == 1 {
                        released = false;
                        m.start_roll((cx, cy), (tx, ty), 46, DT, terrain);
                        roll_end = Some(n + 30);
                        notes += " roll";
                    }
                }
                Out::Stats(ref v) => {
                    // A sprint sent into a released roll (server bug, the tap-then-hold rule of
                    // the spec): the client takes the stats (turns, speed) but runs no sprint
                    // part: no sprint curve, no long line, no `SprintEnd`.
                    if v.contains(&(IS_SPRINTING, 1)) && released {
                        ignored = true;
                    }
                    let was_ignored = ignored;
                    if v.contains(&(IS_SPRINTING, 0)) {
                        ignored = false;
                    }
                    for &(id, val) in v {
                        if was_ignored && id == IS_SPRINTING {
                            m.is_sprinting = val != 0;
                            continue;
                        }
                        if id == IS_SPRINTING {
                            m.set_sprinting(val != 0);
                            m.level = if val != 0 { 4 } else { 0 };
                            if val != 0 {
                                m.set_roll_sprinting();
                                m.extend_roll(poe_game::clientmotion::ROLL_SPRINT_LINE);
                            }
                            notes += &format!(" sprinting={val}");
                        }
                        if id == MUST_SPRINT {
                            m.must_sprint = val != 0;
                        }
                        if id == SPRINT_SPEED {
                            m.scale = 1.0 + val as f32 / 100.0;
                        }
                    }
                }
                Out::Target(x, y) => m.steer_roll((x, y)),
                Out::SprintReleased => m.sprint_key_released(),
                Out::Key(_) => {}
            }
        }
        m.actor_tick(DT, terrain);
        let empty = ClientFrame::default();
        let f = frames.get(&n).unwrap_or(&empty);
        let Some(cell) = f.cell().or(Some(prev)) else { continue };
        prev = cell;
        let same = m.grid() == cell;
        if same {
            ok += 1;
        } else {
            bad += 1;
        }
        let end_same = m.line_end() == f.end();
        if end_same {
            end_ok += 1;
        } else {
            end_bad += 1;
        }
        let window = n >= from && n <= to;
        if (!same || !end_same || from > 0) && window && shown < show {
            shown += 1;
            println!(
                "f{n} grid {:?}/{:?} end {:?}/{:?} pt {:.2},{:.2} m {:?} log {:?} {notes}",
                m.grid(),
                cell,
                m.line_end(),
                f.end(),
                m.point().0,
                m.point().1,
                m.line_state(),
                f.line.map(|l| (l.p, l.el, l.acc, l.len, l.t)),
            );
        }
        if (!same || !end_same) && !free {
            // Carry on from the client's state: each error counts once.
            match f.line {
                Some(l) => m.load_line(l),
                None => m.load_no_line(cell),
            }
        }
    }
    println!(
        "frames {} grid ok {ok} bad {bad}; line end ok {end_ok} bad {end_bad}; snaps replayed {snaps}",
        ok + bad
    );
}
