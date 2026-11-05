//! Server commands: one registry for chat commands (`!name args`, prefix from `[commands]`), the
//! client DLL overlay (the debug link sends [`wire`] lines) and the web API, so a command cannot
//! exist in one and not in another. Execution is in `exec.rs` (`InstanceServer::run_command`);
//! name lists for the [`Kind`]s behind [`Names`] are in `names.rs`.
//!
//! Syntax: verb first, with subcommands: `path... positional... --flag value --switch`, quotes
//! for spaces. One parser for chat, overlay and web. [`locate`] walks the tree, [`parse_args`]
//! checks the arguments, [`resolve`] turns names into ids.
//!
//! The built-in commands are a compile-time list; mods add theirs at startup
//! ([`install_mod_commands`]: native commands a mod registered and the `[[commands]]` of data
//! mods), after which the registry is fixed for the life of the process.

use std::{sync::OnceLock, time::Duration};

use poe_hook_shared::cmdspec::{CmdArg, CmdFlag, CmdSpec};
use poe_mod_host::{CommandAction, CommandInfo};

/// An argument's type: how the overlay asks for it, how it is checked, where completion comes
/// from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    Float,
    /// One word (or quoted words).
    Text,
    /// The rest of the line, as typed (messages).
    Rest,
    /// `on`/`off` (also true/false, 1/0, yes/no).
    Bool,
    /// `30s`, `5m`, `2h`, `1d`; a bare number counts minutes.
    Duration,
    Choice(&'static [&'static str]),
    /// `WorldAreas` (id or display name).
    Area,
    /// Terrain seed (`0` or `random`: the server picks one).
    Seed,
    /// 1-based topology of the area (`0` or `natural`: the client's choice).
    Topology,
    /// Region orientation 0..8 (`natural`: the client's choice).
    Orientation,
    /// An instance id (the overlay lists them).
    Instance,
    /// A character name.
    Player,
    Class,
    Ascendancy,
    ItemBase,
    Unique,
    Mod,
    Stat,
    Currency,
    Gem,
    Monster,
    League,
    Waypoint,
    Rarity,
    DamageType,
}

/// Kinds with a name list that a value must be in (`Player` and `Instance` also complete, but
/// `execute` decides what an unknown one means).
const LISTS: &[(&str, Kind)] = &[
    ("area", Kind::Area),
    ("player", Kind::Player),
    ("instance", Kind::Instance),
    ("class", Kind::Class),
    ("ascendancy", Kind::Ascendancy),
    ("item_base", Kind::ItemBase),
    ("unique", Kind::Unique),
    ("mod", Kind::Mod),
    ("stat", Kind::Stat),
    ("currency", Kind::Currency),
    ("gem", Kind::Gem),
    ("monster", Kind::Monster),
    ("league", Kind::League),
    ("waypoint", Kind::Waypoint),
    ("rarity", Kind::Rarity),
    ("damage_type", Kind::DamageType),
];

impl Kind {
    fn wire(self) -> String {
        if let Some((name, _)) = LISTS.iter().find(|(_, k)| *k == self) {
            return (*name).into();
        }
        match self {
            Kind::Int => "int".into(),
            Kind::Float => "float".into(),
            Kind::Text => "text".into(),
            Kind::Rest => "rest".into(),
            Kind::Bool => "bool".into(),
            Kind::Duration => "duration".into(),
            Kind::Choice(c) => format!("choice={}", c.join("|")),
            Kind::Seed => "seed".into(),
            Kind::Topology => "topology".into(),
            Kind::Orientation => "orientation".into(),
            _ => "text".into(),
        }
    }

    /// The kind a `complete <kind> <prefix>` request names.
    pub fn listed(wire: &str) -> Option<Self> {
        LISTS.iter().find(|(n, _)| *n == wire).map(|(_, k)| *k)
    }

    /// A value must be one of the kind's names (and is replaced by its id).
    fn strict(self) -> bool {
        LISTS.iter().any(|(_, k)| *k == self) && !matches!(self, Kind::Player | Kind::Instance)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Arg {
    pub name: &'static str,
    pub kind: Kind,
    pub optional: bool,
}

const fn arg(name: &'static str, kind: Kind) -> Arg {
    Arg { name, kind, optional: false }
}

const fn opt(name: &'static str, kind: Kind) -> Arg {
    Arg { name, kind, optional: true }
}

/// `--name value`, or a switch `--name` when `kind` is `None`.
#[derive(Debug, Clone, Copy)]
pub struct Flag {
    pub name: &'static str,
    pub kind: Option<Kind>,
    pub help: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Help,
    Character,
    Stats,
    Items,
    World,
    Monsters,
    Life,
    Players,
    Server,
    Accounts,
    Research,
    Mods,
}

impl Category {
    pub const ALL: [Category; 12] = [
        Category::Help,
        Category::Character,
        Category::Stats,
        Category::Items,
        Category::World,
        Category::Monsters,
        Category::Life,
        Category::Players,
        Category::Server,
        Category::Accounts,
        Category::Research,
        Category::Mods,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Category::Help => "Help",
            Category::Character => "Character",
            Category::Stats => "Stats",
            Category::Items => "Items",
            Category::World => "World",
            Category::Monsters => "Monsters",
            Category::Life => "Life",
            Category::Players => "Players",
            Category::Server => "Server",
            Category::Accounts => "Accounts",
            Category::Research => "Research",
            Category::Mods => "Mods",
        }
    }
}

/// Who may run a command (checked against the account role when it runs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Player,
    Admin,
}

/// A game feature a command needs. Its commands stay out of the registry until the feature is
/// listed in [`BUILT`], so `help` never lists something that does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Items,
    Stats,
    Currency,
    Passives,
    Life,
    Flasks,
    Skills,
    Monsters,
    Loot,
    Npcs,
    Waypoints,
}

/// The features that exist. Each feature step adds its own.
pub const BUILT: &[Feature] = &[];

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// `["item", "give"]`.
    pub path: &'static [&'static str],
    /// Whole-path aliases, one word each: `lvl` = `level set`.
    pub aliases: &'static [&'static str],
    pub category: Category,
    pub level: Level,
    /// Positional arguments.
    pub args: &'static [Arg],
    pub flags: &'static [Flag],
    /// One line.
    pub summary: &'static str,
    /// A paragraph, shown by `help <command>`.
    pub help: &'static str,
    pub examples: &'static [&'static str],
    /// The Debug tab with its form (`poe_hook_shared::cmdspec::DEBUG_PAGES`).
    pub overlay: Option<&'static str>,
    /// Hidden until the feature exists.
    pub requires: Option<Feature>,
}

impl Spec {
    /// `instance join`.
    pub fn name(&self) -> String {
        self.path.join(" ")
    }

    pub fn is_admin(&self) -> bool {
        self.level == Level::Admin
    }

    /// Its feature exists (or it needs none).
    pub fn available(&self) -> bool {
        self.requires.is_none_or(|f| BUILT.contains(&f))
    }

    /// May an account with this role see and run it?
    pub fn visible_to(&self, admin: bool) -> bool {
        self.available() && (admin || !self.is_admin())
    }
}

/// The defaults of [`COMMANDS`] entries: an admin command.
const BASE: Spec = Spec {
    path: &[],
    aliases: &[],
    category: Category::Server,
    level: Level::Admin,
    args: &[],
    flags: &[],
    summary: "",
    help: "",
    examples: &[],
    overlay: None,
    requires: None,
};

const LAYOUT: [Arg; 4] = [
    arg("area", Kind::Area),
    opt("seed", Kind::Seed),
    opt("topology", Kind::Topology),
    opt("orientation", Kind::Orientation),
];

const HELP_PAGE: Option<&str> = Some("Debug>Help");
const WORLD_PAGE: Option<&str> = Some("Debug>World");
const UNLOCK_ARGS: [Arg; 3] = [arg("character", Kind::Player), opt("what", Kind::Choice(&["acts", "waypoints", "all", "act", "waypoint"])), opt("value", Kind::Text)];
const PLAYERS_PAGE: Option<&str> = Some("Debug>Players");
const CHARACTER_PAGE: Option<&str> = Some("Debug>Character");

pub const COMMANDS: &[Spec] = &[
    Spec {
        path: &["help"],
        category: Category::Help,
        level: Level::Player,
        args: &[opt("search", Kind::Rest)],
        summary: "Lists, searches and explains the commands.",
        help: "Without a word: the commands you may use, by category. With a command's name or alias: its usage, help, examples, aliases, level and overlay page. With any other word: the commands whose name, alias or summary contains it. A trailing number is a page.",
        examples: &["help", "help instance", "help instance join", "help 2"],
        overlay: HELP_PAGE,
        ..BASE
    },
    Spec {
        path: &["where"],
        category: Category::Help,
        level: Level::Player,
        summary: "Your instance, area, league, seed, cell and the players with you.",
        help: "Where you are: the instance id and how many players are in it, the area, your league, the layout seed and your grid cell.",
        examples: &["where"],
        overlay: HELP_PAGE,
        ..BASE
    },
    Spec {
        path: &["uptime"],
        category: Category::Help,
        level: Level::Player,
        summary: "Server uptime, version and supported client.",
        help: "How long this server has been running, its version and the client build it serves.",
        examples: &["uptime"],
        overlay: HELP_PAGE,
        ..BASE
    },
    Spec {
        path: &["seed"],
        category: Category::Help,
        level: Level::Player,
        summary: "This instance's terrain seed and layout.",
        help: "The layout seed of this instance, with its forced topology and orientation (natural: the client's own choice).",
        examples: &["seed"],
        overlay: HELP_PAGE,
        ..BASE
    },
    Spec {
        path: &["stuck"],
        category: Category::World,
        level: Level::Player,
        summary: "Moves you to the nearest walkable cell, or the area's start.",
        help: "For when you are stuck in a wall: moves you to the nearest walkable cell with room around it. With none in reach you go back to where you entered the instance.",
        examples: &["stuck"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["who"],
        category: Category::Players,
        summary: "Online players with their league, level and instance.",
        help: "Every player online: character, account, level, league, area and instance.",
        examples: &["who"],
        overlay: PLAYERS_PAGE,
        ..BASE
    },
    Spec {
        path: &["player", "kick"],
        aliases: &["kick"],
        category: Category::Players,
        args: &[arg("player", Kind::Player), opt("reason", Kind::Rest)],
        summary: "Disconnects a player.",
        help: "Ends the player's game session. The reason, if any, is sent to them as a chat line first and goes to the audit log.",
        examples: &["player kick Ann", "kick Ann spamming"],
        overlay: PLAYERS_PAGE,
        ..BASE
    },
    Spec {
        path: &["player", "mute"],
        aliases: &["mute"],
        category: Category::Players,
        args: &[arg("player", Kind::Player), opt("duration", Kind::Duration)],
        summary: "Stops a player's chat, for a while or for good.",
        help: "Muted players get \"You are muted.\" instead of their chat line. Without a duration the mute lasts until unmute (or a server restart).",
        examples: &["player mute Ann 30m", "mute Ann 2h", "mute Ann"],
        overlay: PLAYERS_PAGE,
        ..BASE
    },
    Spec {
        path: &["player", "unmute"],
        aliases: &["unmute"],
        category: Category::Players,
        args: &[arg("player", Kind::Player)],
        summary: "Lets a muted player chat again.",
        help: "Removes a mute before it runs out.",
        examples: &["player unmute Ann", "unmute Ann"],
        overlay: PLAYERS_PAGE,
        ..BASE
    },
    Spec {
        path: &["summon"],
        category: Category::Players,
        args: &[arg("player", Kind::Player)],
        summary: "Brings a player to you.",
        help: "Puts the player on your cell. A player in another instance is moved into yours, which ends their session there and sends them through the normal hand-off.",
        examples: &["summon Ann"],
        overlay: PLAYERS_PAGE,
        ..BASE
    },
    Spec {
        path: &["announce"],
        category: Category::Server,
        args: &[arg("message", Kind::Rest)],
        summary: "A server message to every player.",
        help: "Sends the message as a chat line from <Server> to everyone online.",
        examples: &["announce Restart in five minutes"],
        overlay: PLAYERS_PAGE,
        ..BASE
    },
    Spec {
        path: &["area", "list"],
        category: Category::World,
        args: &[opt("search", Kind::Rest)],
        summary: "Areas from WorldAreas with act and level.",
        help: "Lists every area, or those whose id or name contains the search word. A trailing number is a page.",
        examples: &["area list", "area list clearfell", "area list town 2"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["instance", "list"],
        aliases: &["instances"],
        category: Category::World,
        summary: "Lists the instances (id, area, seed, layout, players).",
        help: "Every instance on this server with its layout choice and the players inside.",
        examples: &["instance list", "instances"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["instance", "new"],
        aliases: &["instance_new"],
        category: Category::World,
        args: &LAYOUT,
        summary: "Creates an instance without entering it.",
        help: "Seed: a number or random. Topology (1-based in the area's list) and orientation (0-7): a number or natural, the client's own choice.",
        examples: &["instance new G1_1", "instance new G1_1 1001 2 3"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["unlock"],
        category: Category::World,
        args: &UNLOCK_ARGS,
        summary: "Unlocks act maps and waypoints for an online character.",
        help: "Without a choice, or with acts, waypoints or all: every waypoint of the acts and interludes. act <n>: one act of WorldAreas.Act (the interludes are act 6). waypoint <area>: one area. The world map follows at once.",
        examples: &["unlock Ann", "unlock Ann all", "unlock Ann act 2", "unlock Ann waypoint G1_2"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["lock"],
        category: Category::World,
        args: &UNLOCK_ARGS,
        summary: "Locks act maps and waypoints of an online character again.",
        help: "The same choices as unlock.",
        examples: &["lock Ann", "lock Ann act 2", "lock Ann waypoint G1_2"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["waypoint", "unlock"],
        category: Category::World,
        args: &[opt("what", Kind::Choice(&["acts", "waypoints", "all", "act", "waypoint"])), opt("value", Kind::Text)],
        summary: "Unlocks waypoints and act maps for you.",
        help: "The same as unlock with your character's name: all (default), act <n>, waypoint <area>.",
        examples: &["waypoint unlock all", "waypoint unlock act 2", "waypoint unlock waypoint G1_4"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["checkpoint", "unlock"],
        category: Category::World,
        args: &[opt("scope", Kind::Choice(&["all"]))],
        summary: "Unlocks every checkpoint of the instance you are in.",
        help: "The area map then offers travel to each of them.",
        examples: &["checkpoint unlock all"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["travel", "status"],
        category: Category::World,
        summary: "Your unlocked waypoints, this layout's checkpoints and your respawn point.",
        help: "Also lists how many travel objects this instance has.",
        examples: &["travel status"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["instance", "transfer"],
        aliases: &["transfer", "go"],
        category: Category::World,
        args: &LAYOUT,
        summary: "Creates an instance and moves you into it.",
        help: "Like instance new, then you go there through the normal hand-off. Seed, topology and orientation as in instance new.",
        examples: &["instance transfer G1_town 1", "go G1_1", "transfer \"The Riverbank\""],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["instance", "join"],
        aliases: &["join"],
        category: Category::World,
        args: &[arg("id", Kind::Instance)],
        summary: "Moves you into an existing instance.",
        help: "Instance ids are in instance list.",
        examples: &["instance join 3", "join 3"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["instance", "close"],
        aliases: &["close"],
        category: Category::World,
        args: &[arg("id", Kind::Instance)],
        summary: "Closes an instance (its players are disconnected).",
        help: "Instance ids are in instance list.",
        examples: &["instance close 3", "close 3"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["instance", "reset"],
        category: Category::World,
        summary: "A fresh instance of this area, with you inside.",
        help: "Creates a new instance of the area you are in (towns and hideouts keep their fixed seed, others get a random one) and moves you there.",
        examples: &["instance reset"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["tp"],
        aliases: &["teleport"],
        category: Category::World,
        args: &[arg("x|player", Kind::Player), opt("y", Kind::Int)],
        summary: "Moves you to a grid cell or to a player.",
        help: "Two numbers are a grid cell in this instance. A name is an online player: you land on their cell, moving to their instance if it is another one.",
        examples: &["tp 378 345", "tp Ann", "teleport 378 345"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["time", "scale"],
        category: Category::World,
        args: &[opt("factor", Kind::Float)],
        summary: "Slows this whole instance down (client and server together).",
        help: "Factor 0.03 to 1 in steps of 1/33; 1 is normal speed; no factor shows the current one. Sends the client SimulationFrameValue, and the server simulates with the same frame length, so both stay in step. It applies to everyone in the instance. Faster than 1 is refused: the client plays faster only with frames queued, which would add latency.",
        examples: &["time scale 0.5", "time scale 1", "time scale"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["league", "list"],
        category: Category::World,
        summary: "Lists the leagues with state and character count.",
        help: "Every league in the database. The web admin creates and edits them.",
        examples: &["league list"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["league", "end"],
        category: Category::World,
        args: &[arg("league", Kind::League), arg("target", Kind::Text)],
        summary: "Ends a league family and moves its characters into the target.",
        help: "Ends the league's whole family (softcore, hardcore and so on). Its characters move to the matching variant of the target family.",
        examples: &["league end Dawn Standard"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["speed"],
        category: Category::Character,
        args: &[opt("increased", Kind::Int), opt("action", Kind::Int)],
        summary: "Your movement speed: increased % and action speed %.",
        help: "Sets the stats the movement port reads, until gear and buffs exist; the next frame moves at the new speed and sends the stats. Goes when stat add lands.",
        examples: &["speed 50", "speed 0 -30"],
        overlay: CHARACTER_PAGE,
        ..BASE
    },
    Spec {
        path: &["account"],
        category: Category::Accounts,
        args: &[
            arg("action", Kind::Choice(&["list", "create", "role", "disable", "enable", "passwd"])),
            opt("name", Kind::Text),
            opt("value", Kind::Rest),
        ],
        summary: "Lists and manages accounts.",
        help: "create <name> <password>, role <name> player|admin, disable|enable <name>, passwd <name> <password>, list. Passwords never reach the log or the audit file.",
        examples: &["account list", "account role Ann admin"],
        overlay: PLAYERS_PAGE,
        ..BASE
    },
    Spec {
        path: &["terrain", "compare"],
        aliases: &["terrain"],
        category: Category::Research,
        summary: "Generates this instance's terrain and compares it with the client's.",
        help: "Runs the server's generator on this instance's area and seed. The result is in the server log.",
        examples: &["terrain compare", "terrain"],
        overlay: WORLD_PAGE,
        ..BASE
    },
    Spec {
        path: &["research", "area_info"],
        aliases: &["area_info"],
        category: Category::Research,
        args: &[arg("area", Kind::Area), arg("seed", Kind::Seed)],
        summary: "Sends a second AreaInfo on this connection (the client may crash).",
        help: "Research only: what the client does with an AreaInfo while it is in the world.",
        examples: &["research area_info G1_1 1"],
        overlay: WORLD_PAGE,
        ..BASE
    },
];

/// A command that a mod added: its spec (leaked, mods load once per process) and what it does.
#[derive(Debug)]
pub struct Extra {
    pub spec: &'static Spec,
    pub mod_id: String,
    pub action: CommandAction,
}

/// The one argument of every mod command: the rest of the line.
static MOD_ARGS: [Arg; 1] = [opt("args", Kind::Rest)];

/// Words mods may not name a command: the first word of every built-in path and every alias.
pub fn reserved_names() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in COMMANDS {
        for w in c.aliases.iter().chain(c.path.first()) {
            if !out.iter().any(|o| o == w) {
                out.push((*w).to_owned());
            }
        }
    }
    out
}

/// Builds the registry entries for the commands the mod host collected. Names that the built-in
/// commands or an earlier entry already use are dropped (the host refuses them first; this keeps
/// the registry consistent regardless).
pub fn make_extras(infos: &[CommandInfo]) -> Vec<Extra> {
    let leak = |s: &str| -> &'static str { Box::leak(s.to_owned().into_boxed_str()) };
    let reserved = reserved_names();
    let mut out: Vec<Extra> = Vec::new();
    for c in infos {
        let taken = reserved.iter().any(|r| r.eq_ignore_ascii_case(&c.name))
            || out.iter().any(|e| e.spec.path[0].eq_ignore_ascii_case(&c.name));
        if taken {
            continue;
        }
        let name: &'static str = leak(&c.name);
        let help: &'static str = leak(&c.help);
        out.push(Extra {
            spec: Box::leak(Box::new(Spec {
                path: Box::leak(Box::new([name])),
                category: Category::Mods,
                level: if c.admin { Level::Admin } else { Level::Player },
                args: &MOD_ARGS,
                summary: help,
                help,
                overlay: HELP_PAGE,
                ..BASE
            })),
            mod_id: c.mod_id.clone(),
            action: c.action.clone(),
        });
    }
    out
}

/// The text of a data-tier reply or alias with `{player}` (the caller's character) and
/// `{args}` (the rest of the line) filled in.
pub fn fill(text: &str, player: &str, args: &str) -> String {
    text.replace("{player}", player).replace("{args}", args)
}

static EXTRAS: OnceLock<Vec<Extra>> = OnceLock::new();

/// Adds the mods' commands to the registry. Once per process, before the server runs (mods are
/// never reloaded); a second call is ignored.
pub fn install_mod_commands(infos: &[CommandInfo]) {
    let _ = EXTRAS.set(make_extras(infos));
}

/// The commands mods added.
pub fn extras() -> &'static [Extra] {
    EXTRAS.get().map_or(&[], Vec::as_slice)
}

/// The mod command `spec` belongs to, if it is one.
pub fn extra(spec: &Spec) -> Option<&'static Extra> {
    extras().iter().find(|e| std::ptr::eq(e.spec, spec))
}

/// Every available command: the built-in ones, then the mods'.
pub fn all() -> impl Iterator<Item = &'static Spec> {
    all_in(extras())
}

fn all_in(extras: &'static [Extra]) -> impl Iterator<Item = &'static Spec> {
    COMMANDS.iter().filter(|c| c.available()).chain(extras.iter().map(|e| e.spec))
}

/// The available command with this exact path (`"instance join"`).
pub fn find(name: &str) -> Option<&'static Spec> {
    all().find(|c| c.name().eq_ignore_ascii_case(name))
}

/// `name <arg> [arg] [--flag <kind>]` for messages.
pub fn usage(spec: &Spec) -> String {
    to_shared(spec).usage()
}

fn to_shared(spec: &Spec) -> CmdSpec {
    let list = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    CmdSpec {
        path: list(spec.path),
        aliases: list(spec.aliases),
        category: spec.category.name().to_owned(),
        admin: spec.is_admin(),
        args: spec
            .args
            .iter()
            .map(|a| CmdArg { name: a.name.to_owned(), kind: a.kind.wire(), optional: a.optional })
            .collect(),
        flags: spec
            .flags
            .iter()
            .map(|f| CmdFlag { name: f.name.to_owned(), kind: f.kind.map(Kind::wire), help: f.help.to_owned() })
            .collect(),
        summary: spec.summary.to_owned(),
        help: spec.help.to_owned(),
        examples: list(spec.examples),
        overlay: spec.overlay.map(str::to_owned),
    }
}

/// Longest `cmdspec` line: a debug frame carries about 3000 bytes.
const MAX_WIRE: usize = 2900;

/// The overlay's line for a command: `cmdspec<TAB><one JSON object>`
/// (`poe_hook_shared::cmdspec::CmdSpec`). A very long mod help is cut to fit a datagram.
pub fn wire(spec: &Spec) -> String {
    let mut shared = to_shared(spec);
    let mut json = serde_json::to_string(&shared).expect("a command spec serializes");
    if json.len() > MAX_WIRE {
        shared.help = cut(&shared.help, 400);
        shared.summary = cut(&shared.summary, 200);
        json = serde_json::to_string(&shared).expect("a command spec serializes");
    }
    format!("cmdspec\t{json}")
}

fn cut(text: &str, max: usize) -> String {
    text.char_indices().nth(max).map_or_else(|| text.to_owned(), |(i, _)| format!("{}...", &text[..i]))
}

/// A parsed command line: the command, its arguments (missing optional ones are `None`) and its
/// flags (in `spec.flags` order; a given switch is `Some("")`).
#[derive(Debug)]
pub struct Parsed {
    pub spec: &'static Spec,
    pub args: Vec<Option<String>>,
    pub flags: Vec<Option<String>>,
}

impl Parsed {
    pub fn get(&self, name: &str) -> Option<&str> {
        let i = self.spec.args.iter().position(|a| a.name == name)?;
        self.args.get(i)?.as_deref()
    }

    pub fn int(&self, name: &str) -> Option<i64> {
        self.get(name)?.parse().ok()
    }

    pub fn float(&self, name: &str) -> Option<f64> {
        self.get(name)?.parse().ok()
    }

    pub fn duration(&self, name: &str) -> Option<Duration> {
        parse_duration(self.get(name)?)
    }

    /// The value of a `--flag`.
    pub fn flag(&self, name: &str) -> Option<&str> {
        let i = self.spec.flags.iter().position(|f| f.name == name)?;
        self.flags.get(i)?.as_deref()
    }

    /// A `--switch` was given.
    pub fn switch(&self, name: &str) -> bool {
        self.flag(name).is_some()
    }

    /// Seed: `None` for absent, `0` or `random`.
    pub fn seed(&self) -> Option<u32> {
        self.get("seed").and_then(|v| v.parse().ok()).filter(|&s| s != 0)
    }

    /// Topology: `None` for absent, `0` or `natural`.
    pub fn topology(&self) -> Option<u32> {
        self.get("topology").and_then(|v| v.parse().ok()).filter(|&t| t != 0)
    }

    /// Orientation: `None` for absent or `natural`.
    pub fn orientation(&self) -> Option<u8> {
        self.get("orientation").and_then(|v| v.parse().ok()).filter(|&o| o < 8)
    }
}

/// `30s`, `5m`, `2h`, `1d`; a bare number counts minutes. Zero is no duration.
pub fn parse_duration(word: &str) -> Option<Duration> {
    let word = word.trim().to_ascii_lowercase();
    let (digits, unit) = match word.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => word.split_at(i),
        None => (word.as_str(), "m"),
    };
    let n: u64 = digits.parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" => n.checked_mul(60)?,
        "h" => n.checked_mul(3600)?,
        "d" => n.checked_mul(86400)?,
        _ => return None,
    };
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// `30m`, `2h` for messages.
pub fn show_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        _ if s >= 86400 && s.is_multiple_of(86400) => format!("{}d", s / 86400),
        _ if s >= 3600 && s.is_multiple_of(3600) => format!("{}h", s / 3600),
        _ if s >= 60 && s.is_multiple_of(60) => format!("{}m", s / 60),
        _ => format!("{s}s"),
    }
}

/// The next word of `line` and what follows it: quoted words lose their quotes (`\"` is a quote
/// inside them). The flag says the word was quoted, so it cannot be a `--flag`.
fn next_word(line: &str) -> Result<Option<(String, &str, bool)>, String> {
    let line = line.trim_start();
    if line.is_empty() {
        return Ok(None);
    }
    let Some(quoted) = line.strip_prefix('"') else {
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        return Ok(Some((word.to_owned(), rest, false)));
    };
    let mut word = String::new();
    let mut chars = quoted.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Ok(Some((word, &quoted[i + 1..], true))),
            '\\' if quoted[i + 1..].starts_with('"') => {
                chars.next();
                word.push('"');
            }
            c => word.push(c),
        }
    }
    Err("a quote is not closed".to_owned())
}

/// Why [`locate`] found no command.
#[derive(Debug, PartialEq, Eq)]
pub enum Miss {
    Empty,
    /// The first word names nothing.
    Unknown(String),
    /// The first word is a group (`instance`) and what follows is no subcommand.
    Subcommand { group: String, word: Option<String> },
}

impl Miss {
    /// The message for an account: only names it may see.
    pub fn text(&self, admin: bool) -> String {
        self.text_in(extras(), admin)
    }

    fn text_in(&self, extras: &'static [Extra], admin: bool) -> String {
        let visible = || all_in(extras).filter(move |c| c.visible_to(admin));
        match self {
            Miss::Empty => "no command given (try help)".to_owned(),
            Miss::Unknown(word) => {
                let mut names: Vec<String> = Vec::new();
                for c in visible() {
                    names.push(c.path[0].to_owned());
                    names.extend(c.aliases.iter().map(|a| (*a).to_owned()));
                }
                names.dedup();
                match closest(&names, word) {
                    near if near.is_empty() => format!("unknown command {word:?} (try help)"),
                    near => format!("unknown command {word:?}; did you mean {}? (try help)", near.join(", ")),
                }
            }
            Miss::Subcommand { group, word } => {
                let subs: Vec<String> = visible().filter(|c| c.path[0].eq_ignore_ascii_case(group) && c.path.len() > 1).map(|c| c.path[1].to_owned()).collect();
                if subs.is_empty() {
                    return Miss::Unknown(group.clone()).text_in(extras, admin);
                }
                match word {
                    Some(w) => format!("{group} has no subcommand {w:?}; it has: {}", subs.join(", ")),
                    None => format!("{group} needs a subcommand: {}", subs.join(", ")),
                }
            }
        }
    }
}

/// Finds the command a line names: its longest matching path, else a one-word alias. Returns
/// the spec and the unparsed rest of the line.
pub fn locate(line: &str) -> Result<(&'static Spec, &str), Miss> {
    locate_in(extras(), line)
}

fn locate_in<'a>(extras: &'static [Extra], line: &'a str) -> Result<(&'static Spec, &'a str), Miss> {
    let line = line.trim();
    let Ok(Some((first, after_first, _))) = next_word(line) else {
        return Err(Miss::Empty);
    };
    let second = next_word(after_first).ok().flatten();
    let mut best: Option<(&'static Spec, &'a str)> = None;
    let mut group = false;
    for spec in all_in(extras) {
        if !spec.path[0].eq_ignore_ascii_case(&first) {
            continue;
        }
        group = true;
        match spec.path {
            [_] => best = best.or(Some((spec, after_first))),
            [_, sub] => {
                if let Some((word, rest, _)) = &second
                    && sub.eq_ignore_ascii_case(word)
                {
                    return Ok((spec, *rest));
                }
            }
            _ => {}
        }
    }
    if let Some(found) = best {
        return Ok(found);
    }
    if let Some(spec) = all_in(extras).find(|c| c.aliases.iter().any(|a| a.eq_ignore_ascii_case(&first))) {
        return Ok((spec, after_first));
    }
    if group {
        return Err(Miss::Subcommand { group: first, word: second.map(|s| s.0) });
    }
    Err(Miss::Unknown(first))
}

/// Checks the arguments of `spec` in the rest of a line.
pub fn parse_args(spec: &'static Spec, mut rest: &str) -> Result<Parsed, String> {
    let fail = |e: String| format!("{e}; usage: {}", usage(spec));
    let mut args: Vec<Option<String>> = vec![None; spec.args.len()];
    let mut flags: Vec<Option<String>> = vec![None; spec.flags.len()];
    let mut pos = 0;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        // Everything is one argument from a Rest argument on, flags included.
        if (!rest.starts_with("--") || spec.flags.is_empty())
            && let Some(a) = spec.args.get(pos)
            && a.kind == Kind::Rest
        {
            args[pos] = Some(rest.to_owned());
            pos += 1;
            break;
        }
        let Some((word, after, quoted)) = next_word(rest).map_err(fail)? else {
            break;
        };
        rest = after;
        if !quoted && let Some(flag) = word.strip_prefix("--") {
            let (name, inline) = match flag.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (flag, None),
            };
            let Some(i) = spec.flags.iter().position(|f| f.name.eq_ignore_ascii_case(name)) else {
                return Err(fail(format!("no flag --{name}")));
            };
            flags[i] = Some(match spec.flags[i].kind {
                None if inline.is_some() => return Err(fail(format!("--{name} takes no value"))),
                None => String::new(),
                Some(kind) => {
                    let value = match inline {
                        Some(v) => v,
                        None => {
                            let Some((v, after, _)) = next_word(rest).map_err(fail)? else {
                                return Err(fail(format!("--{name} needs a value")));
                            };
                            rest = after;
                            v
                        }
                    };
                    check(kind, &value).ok_or_else(|| fail(format!("bad --{name}: {value:?}")))?
                }
            });
            continue;
        }
        let Some(a) = spec.args.get(pos) else {
            return Err(fail("too many arguments".to_owned()));
        };
        args[pos] = Some(check(a.kind, &word).ok_or_else(|| fail(format!("bad <{}>: {word:?}", a.name)))?);
        pos += 1;
    }
    if let Some(missing) = spec.args.iter().zip(&args).find(|(a, v)| !a.optional && v.is_none()) {
        return Err(fail(format!("missing <{}>", missing.0.name)));
    }
    Ok(Parsed { spec, args, flags })
}

/// A value's check: the value to use (`Choice` gives the listed spelling, `Bool` `on` or `off`)
/// or `None` when it is not one of the kind's.
fn check(kind: Kind, w: &str) -> Option<String> {
    let lower = w.to_ascii_lowercase();
    let ok = match kind {
        Kind::Int | Kind::Instance => w.parse::<i64>().is_ok(),
        Kind::Float => w.parse::<f64>().is_ok_and(f64::is_finite),
        Kind::Duration => parse_duration(w).is_some(),
        Kind::Bool => {
            return match lower.as_str() {
                "on" | "true" | "1" | "yes" => Some("on".to_owned()),
                "off" | "false" | "0" | "no" => Some("off".to_owned()),
                _ => None,
            };
        }
        Kind::Choice(c) => return c.iter().find(|o| o.eq_ignore_ascii_case(w)).map(|o| (*o).to_owned()),
        Kind::Seed => lower == "random" || w.parse::<u32>().is_ok(),
        Kind::Topology => lower == "natural" || w.parse::<u32>().is_ok(),
        Kind::Orientation => lower == "natural" || w.parse::<u8>().is_ok_and(|o| o < 8),
        _ => true,
    };
    ok.then(|| w.to_owned())
}

/// An entry of a name list: what a value becomes, and what a player calls it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name {
    pub id: String,
    /// Empty when the game data has none.
    pub display: String,
}

/// The name lists of the [`Kind`]s (`names.rs` from game data, the database and live sessions).
pub trait Names {
    /// `None`: the list could not be read, so values are taken as they are.
    fn names(&self, kind: Kind) -> Option<std::sync::Arc<Vec<Name>>>;
}

/// Replaces the values of name-list kinds by their ids: case-insensitive, by id or display name.
/// A miss suggests the three closest names.
pub fn resolve(p: &mut Parsed, names: &dyn Names) -> Result<(), String> {
    let spec = p.spec;
    let args = spec.args.iter().map(|a| (a.kind, format!("<{}>", a.name))).zip(p.args.iter_mut());
    let flags = spec.flags.iter().filter_map(|f| Some((f.kind?, format!("--{}", f.name)))).zip(p.flags.iter_mut());
    for ((kind, label), value) in args.chain(flags) {
        let Some(word) = value else { continue };
        if !kind.strict() {
            continue;
        }
        let Some(list) = names.names(kind) else { continue };
        *word = match_name(&list, word).map_err(|e| format!("bad {label}: {e}; usage: {}", usage(spec)))?;
    }
    Ok(())
}

/// The id `word` names in `list`.
pub fn match_name(list: &[Name], word: &str) -> Result<String, String> {
    if let Some(n) = list.iter().find(|n| n.id.eq_ignore_ascii_case(word)) {
        return Ok(n.id.clone());
    }
    let same: Vec<&Name> = list.iter().filter(|n| n.display.eq_ignore_ascii_case(word)).collect();
    match same.as_slice() {
        [one] => return Ok(one.id.clone()),
        [] => {}
        several => {
            let ids: Vec<&str> = several.iter().take(3).map(|n| n.id.as_str()).collect();
            return Err(format!("{word:?} names {} rows, use an id: {}", several.len(), ids.join(", ")));
        }
    }
    let candidates: Vec<String> = list.iter().flat_map(|n| [n.id.clone(), n.display.clone()]).filter(|n| !n.is_empty()).collect();
    match closest(&candidates, word) {
        near if near.is_empty() => Err(format!("{word:?} is not known")),
        near => Err(format!("{word:?} is not known; did you mean {}?", near.join(", "))),
    }
}

/// Up to three names nearest to `word`: those containing it first, then by edit distance, and
/// only ones reasonably close.
fn closest(names: &[String], word: &str) -> Vec<String> {
    let word = word.to_lowercase();
    let mut scored: Vec<(usize, &String)> = names
        .iter()
        .filter_map(|n| {
            let lower = n.to_lowercase();
            if lower.contains(&word) {
                return Some((0, n));
            }
            let d = edit_distance(&lower, &word);
            (d <= (word.len() / 2).max(2)).then_some((d, n))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.len().cmp(&b.1.len())).then(a.1.cmp(b.1)));
    scored.dedup_by(|a, b| a.1 == b.1);
    scored.into_iter().take(3).map(|(_, n)| n.clone()).collect()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            let next = (row[j + 1] + 1).min(row[j] + 1).min(prev + cost);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// The entries matching what was typed: id or display name starting with it first, then
/// containing it.
pub fn complete<'a>(list: &'a [Name], typed: &str, limit: usize) -> Vec<&'a Name> {
    let typed = typed.to_lowercase();
    let rank = |n: &Name| {
        let (id, display) = (n.id.to_lowercase(), n.display.to_lowercase());
        if id.starts_with(&typed) || display.starts_with(&typed) {
            Some(0)
        } else if id.contains(&typed) || display.contains(&typed) {
            Some(1)
        } else {
            None
        }
    };
    let mut hits: Vec<(u8, &Name)> = list.iter().filter_map(|n| rank(n).map(|r| (r, n))).collect();
    hits.sort_by_key(|h| h.0);
    hits.into_iter().take(limit).map(|h| h.1).collect()
}

/// Lines per page of long output.
const PAGE_LINES: usize = 12;

/// A search string with its optional trailing page number: `item 2` -> (`item`, 2).
pub fn split_page(query: Option<&str>) -> (Option<&str>, usize) {
    let q = query.map(str::trim).filter(|q| !q.is_empty());
    let Some(q) = q else { return (None, 1) };
    let (head, last) = q.rsplit_once(char::is_whitespace).map_or(("", q), |(h, l)| (h.trim_end(), l));
    match last.parse::<usize>() {
        Ok(n) => (Some(head).filter(|h| !h.is_empty()), n.max(1)),
        Err(_) => (Some(q), 1),
    }
}

/// One page of `lines`, one chat line each, ending with how to reach the next page.
pub fn paged(lines: &[String], page: usize, next: &str) -> String {
    let pages = lines.len().div_ceil(PAGE_LINES).max(1);
    let page = page.clamp(1, pages);
    let mut out: Vec<String> = lines.iter().skip((page - 1) * PAGE_LINES).take(PAGE_LINES).cloned().collect();
    if pages > 1 {
        out.push(if page < pages { format!("page {page}/{pages}: {next} {}", page + 1) } else { format!("page {page}/{pages}") });
    }
    out.join("\n")
}

/// The lines `help [search or path]` shows an account.
pub fn help_lines(query: Option<&str>, admin: bool, prefix: &str) -> Vec<String> {
    help_lines_in(extras(), query, admin, prefix)
}

fn help_lines_in(extras: &'static [Extra], query: Option<&str>, admin: bool, prefix: &str) -> Vec<String> {
    let visible: Vec<&'static Spec> = all_in(extras).filter(|c| c.visible_to(admin)).collect();
    let Some(query) = query else {
        let mut out = vec![format!("Commands ({prefix}help <word or command>):")];
        for cat in Category::ALL {
            let names: Vec<String> = visible.iter().filter(|c| c.category == cat).map(|c| c.name()).collect();
            if !names.is_empty() {
                out.push(format!("{}: {}", cat.name(), names.join(", ")));
            }
        }
        return out;
    };
    if let Ok((spec, rest)) = locate_in(extras, query)
        && rest.trim().is_empty()
        && spec.visible_to(admin)
    {
        return help_detail(spec, prefix);
    }
    let q = query.to_lowercase();
    let hits: Vec<&&Spec> = visible
        .iter()
        .filter(|c| c.name().to_lowercase().contains(&q) || c.aliases.iter().any(|a| a.to_lowercase().contains(&q)) || c.summary.to_lowercase().contains(&q))
        .collect();
    if hits.is_empty() {
        return vec![format!("no command matches {query:?} ({prefix}help lists them)")];
    }
    hits.iter().map(|c| format!("{prefix}{}: {}", usage(c), c.summary)).collect()
}

fn help_detail(spec: &Spec, prefix: &str) -> Vec<String> {
    let mut out = vec![format!("{prefix}{}", usage(spec)), spec.summary.to_owned()];
    if spec.help != spec.summary && !spec.help.is_empty() {
        out.push(spec.help.to_owned());
    }
    out.extend(spec.flags.iter().map(|f| format!("--{}: {}", f.name, f.help)));
    if !spec.examples.is_empty() {
        out.push(format!("Examples: {}", spec.examples.iter().map(|e| format!("{prefix}{e}")).collect::<Vec<_>>().join("   ")));
    }
    if !spec.aliases.is_empty() {
        out.push(format!("Aliases: {}", spec.aliases.iter().map(|a| format!("{prefix}{a}")).collect::<Vec<_>>().join(" ")));
    }
    let level = if spec.is_admin() { "admin" } else { "player" };
    out.push(match spec.overlay {
        Some(page) => format!("Level: {level}   Overlay: {page}"),
        None => format!("Level: {level}"),
    });
    out
}

/// The command table of docs/guides/server-admin.md (`cargo xtask commands-doc`).
pub fn doc_table() -> String {
    let mut out = String::from("| Command | Aliases | Level | Overlay | What it does |\n|---|---|---|---|---|\n");
    let code = |s: &str| format!("`{}`", s.replace('|', "\\|"));
    for cat in Category::ALL {
        for c in COMMANDS.iter().filter(|c| c.category == cat && c.available()) {
            out += &format!(
                "| {} | {} | {} | {} | {} |\n",
                code(&usage(c)),
                c.aliases.iter().map(|a| code(a)).collect::<Vec<_>>().join(", "),
                if c.is_admin() { "admin" } else { "player" },
                c.overlay.unwrap_or_default().trim_start_matches("Debug>"),
                c.summary.replace('|', "\\|"),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Locates and parses like the server does, without an account check.
    fn parse(line: &str) -> Result<Parsed, String> {
        parse_in(extras(), line)
    }

    fn parse_in(extras: &'static [Extra], line: &str) -> Result<Parsed, String> {
        let (spec, rest) = locate_in(extras, line).map_err(|m| m.text_in(extras, true))?;
        parse_args(spec, rest)
    }

    fn name(p: &Parsed) -> String {
        p.spec.name()
    }

    #[test]
    fn parses_and_checks() {
        let p = parse("instance transfer G1_8 1001 natural 3").unwrap();
        assert_eq!(p.get("area"), Some("G1_8"));
        assert_eq!((p.seed(), p.topology(), p.orientation()), (Some(1001), None, Some(3)));
        let p = parse("instance transfer G1_8").unwrap();
        assert_eq!((p.seed(), p.topology(), p.orientation()), (None, None, None));
        assert!(parse("speed x").is_err());
        assert!(parse("tp 1 2 3").is_err());
        assert!(parse("tp 1 x").is_err());
        assert_eq!(parse("announce hello there all").unwrap().get("message"), Some("hello there all"));
        assert!(parse("nonsense").is_err());
        assert!(parse("where extra").is_err());
    }

    #[test]
    fn every_old_name_and_alias_reaches_its_command() {
        for (line, path) in [
            ("kick Ann", "player kick"),
            ("mute Ann 5", "player mute"),
            ("unmute Ann", "player unmute"),
            ("instances", "instance list"),
            ("instance_new G1_1", "instance new"),
            ("transfer G1_1", "instance transfer"),
            ("go G1_1", "instance transfer"),
            ("join 3", "instance join"),
            ("close 3", "instance close"),
            ("teleport 1 2", "tp"),
            ("terrain", "terrain compare"),
            ("area_info G1_1 1", "research area_info"),
            ("league list", "league list"),
            ("HELP", "help"),
            ("Instance JOIN 3", "instance join"),
        ] {
            assert_eq!(name(&parse(line).unwrap()), path, "{line}");
        }
        for spec in COMMANDS {
            for example in spec.examples {
                assert!(parse(example).is_ok(), "example of {}: {example}: {:?}", spec.name(), parse(example).err());
            }
        }
    }

    #[test]
    fn group_words_explain_their_subcommands() {
        let e = parse("instance").unwrap_err();
        assert!(e.contains("needs a subcommand") && e.contains("join"), "{e}");
        let e = parse("instance frob").unwrap_err();
        assert!(e.contains("no subcommand") && e.contains("list"), "{e}");
    }

    #[test]
    fn unknown_commands_suggest_only_visible_names() {
        let miss = locate("kik").unwrap_err();
        assert!(miss.text(true).contains("kick"), "{}", miss.text(true));
        assert!(!miss.text(false).contains("kick"), "{}", miss.text(false));
        let miss = locate("instance").unwrap_err();
        assert!(miss.text(true).contains("subcommand"));
        assert!(!miss.text(false).contains("join"), "{}", miss.text(false));
    }

    static GIVE_FLAGS: [Flag; 3] = [
        Flag { name: "ilvl", kind: Some(Kind::Int), help: "item level" },
        Flag { name: "rarity", kind: Some(Kind::Rarity), help: "rarity" },
        Flag { name: "ground", kind: None, help: "drops it" },
    ];
    static GIVE: Spec = Spec {
        path: &["give"],
        args: &[arg("base", Kind::ItemBase), opt("note", Kind::Rest)],
        flags: &GIVE_FLAGS,
        ..BASE
    };

    #[test]
    fn flags_quotes_and_rest() {
        let p = parse_args(&GIVE, "\"Iron Ring\" --ilvl 82 --ground").unwrap();
        assert_eq!(p.get("base"), Some("Iron Ring"));
        assert_eq!(p.flag("ilvl"), Some("82"));
        assert!(p.switch("ground"));
        assert_eq!(p.flag("rarity"), None);
        let p = parse_args(&GIVE, "--ilvl=5 Ring --RARITY rare").unwrap();
        assert_eq!((p.get("base"), p.flag("ilvl"), p.flag("rarity")), (Some("Ring"), Some("5"), Some("rare")));
        // A Rest argument takes the rest as typed, flags included; a quoted word is never a flag.
        let p = parse_args(&GIVE, "Ring some --note \"here\"").unwrap();
        assert_eq!(p.get("note"), Some("some --note \"here\""));
        assert_eq!(parse_args(&GIVE, "\"--ground\"").unwrap().get("base"), Some("--ground"));
        assert_eq!(parse_args(&GIVE, r#""a \"b\" c""#).unwrap().get("base"), Some(r#"a "b" c"#));
    }

    #[test]
    fn errors_name_the_bad_argument_and_show_the_usage() {
        for (line, wants) in [
            ("--frob", "no flag --frob"),
            ("Ring --ilvl", "--ilvl needs a value"),
            ("Ring --ilvl x", "bad --ilvl"),
            ("Ring --ground=1", "takes no value"),
            ("\"Ring", "not closed"),
            ("--ilvl 1", "missing <base>"),
        ] {
            let e = parse_args(&GIVE, line).unwrap_err();
            assert!(e.contains(wants) && e.contains("usage: give <base>"), "{line}: {e}");
        }
        let e = parse("instance join x").unwrap_err();
        assert!(e.contains("bad <id>") && e.contains("usage: instance join <id>"), "{e}");
    }

    struct Stub;
    impl Names for Stub {
        fn names(&self, kind: Kind) -> Option<Arc<Vec<Name>>> {
            let n = |id: &str, display: &str| Name { id: id.into(), display: display.into() };
            match kind {
                Kind::ItemBase => Some(Arc::new(vec![
                    n("Metadata/Rings/Ring1", "Iron Ring"),
                    n("Metadata/Rings/Ring2", "Ruby Ring"),
                    n("Metadata/Rings/Twin1", "Twin"),
                    n("Metadata/Rings/Twin2", "Twin"),
                ])),
                _ => None,
            }
        }
    }

    #[test]
    fn names_match_by_id_or_display_name_and_suggest_on_a_miss() {
        let resolved = |line: &str| {
            let mut p = parse_args(&GIVE, line).unwrap();
            resolve(&mut p, &Stub).map(|()| p.get("base").unwrap().to_owned())
        };
        assert_eq!(resolved("\"iron ring\"").unwrap(), "Metadata/Rings/Ring1");
        assert_eq!(resolved("metadata/rings/ring2").unwrap(), "Metadata/Rings/Ring2");
        let e = resolved("twin").unwrap_err();
        assert!(e.contains("2 rows") && e.contains("Twin1"), "{e}");
        let e = resolved("\"Irn Ring\"").unwrap_err();
        assert!(e.contains("bad <base>") && e.contains("did you mean") && e.contains("Iron Ring"), "{e}");
        assert!(resolved("zzzzzzzzzzzzzzzzzzzz").unwrap_err().contains("not known"));
    }

    #[test]
    fn completion_ranks_prefixes_first() {
        let list = vec![
            Name { id: "b_ring".into(), display: "Iron Ring".into() },
            Name { id: "ring_a".into(), display: String::new() },
            Name { id: "other".into(), display: "Sword".into() },
        ];
        let ids = |typed: &str| complete(&list, typed, 5).iter().map(|n| n.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids("RING"), ["ring_a", "b_ring"]);
        assert_eq!(ids("sw"), ["other"]);
        assert_eq!(complete(&list, "", 2).len(), 2);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("5"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("2H"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("1d"), Some(Duration::from_secs(86400)));
        for bad in ["", "m", "5x", "0", "-5"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
        assert_eq!(show_duration(Duration::from_secs(1800)), "30m");
        assert_eq!(show_duration(Duration::from_secs(90)), "90s");
    }

    #[test]
    fn help_shows_players_only_what_they_may_use() {
        let list = help_lines(None, false, "!").join("\n");
        assert!(list.contains("where") && list.contains("uptime"), "{list}");
        for hidden in ["kick", "account", "tp", "instance"] {
            assert!(!list.contains(hidden), "{hidden} in {list}");
        }
        let admin = help_lines(None, true, "!").join("\n");
        assert!(admin.contains("player kick") && admin.contains("World: "), "{admin}");
        // A hidden command's name or alias is not explained or found.
        assert!(help_lines(Some("instance join"), false, "!")[0].contains("no command matches"));
        assert!(help_lines(Some("kick"), false, "!")[0].contains("no command matches"));
    }

    #[test]
    fn help_explains_a_command_and_searches() {
        let d = help_lines(Some("instance join"), true, "!").join("\n");
        for wanted in ["!instance join <id>", "Examples: !instance join 3", "Aliases: !join", "Level: admin", "Overlay: Debug>World"] {
            assert!(d.contains(wanted), "{wanted} in {d}");
        }
        // An alias reaches the same page.
        assert_eq!(help_lines(Some("join"), true, "!"), help_lines(Some("instance join"), true, "!"));
        let s = help_lines(Some("instance"), true, "!").join("\n");
        assert!(s.contains("!instance join") && s.contains("!instance reset"), "{s}");
        // Search reaches summaries too.
        assert!(help_lines(Some("disconnects"), true, "!").join("\n").contains("player kick"));
        assert!(help_lines(Some("frobnicate"), true, "!")[0].contains("no command matches"));
    }

    #[test]
    fn long_output_is_paged() {
        let lines: Vec<String> = (0..30).map(|i| format!("l{i}")).collect();
        let one = paged(&lines, 1, "!help x");
        assert!(one.starts_with("l0\n") && one.ends_with("page 1/3: !help x 2"), "{one}");
        let last = paged(&lines, 9, "!help x");
        assert!(last.starts_with("l24\n") && last.ends_with("page 3/3"), "{last}");
        assert_eq!(paged(&lines[..2], 1, "!x"), "l0\nl1");
        assert_eq!(split_page(Some("item 2")), (Some("item"), 2));
        assert_eq!(split_page(Some("2")), (None, 2));
        assert_eq!(split_page(Some("instance join")), (Some("instance join"), 1));
        assert_eq!(split_page(Some("  ")), (None, 1));
        assert_eq!(split_page(None), (None, 1));
    }

    #[test]
    fn mod_commands_extend_the_registry_without_replacing_built_ins() {
        let infos = vec![
            CommandInfo { name: "motd".into(), help: "Shows it.".into(), admin: false, mod_id: "m".into(), action: CommandAction::Reply("hi".into()) },
            // A built-in name, an alias and a repeat are dropped.
            CommandInfo { name: "instance".into(), help: "mine".into(), admin: false, mod_id: "m".into(), action: CommandAction::Native },
            CommandInfo { name: "kick".into(), help: "mine".into(), admin: false, mod_id: "m".into(), action: CommandAction::Native },
            CommandInfo { name: "motd".into(), help: "again".into(), admin: true, mod_id: "n".into(), action: CommandAction::Native },
            CommandInfo { name: "secret".into(), help: "x".into(), admin: true, mod_id: "m".into(), action: CommandAction::Native },
        ];
        let extras: &'static [Extra] = Box::leak(make_extras(&infos).into_boxed_slice());
        assert_eq!(extras.iter().map(|e| e.spec.name()).collect::<Vec<_>>(), ["motd", "secret"]);
        let p = parse_in(extras, "motd some words here").unwrap();
        assert_eq!(name(&p), "motd");
        assert_eq!(p.get("args"), Some("some words here"));
        assert_eq!(parse_in(extras, "motd").unwrap().get("args"), None);
        assert!(parse_in(extras, "secret").unwrap().spec.is_admin());
        assert_eq!(parse_in(extras, "secret").unwrap().spec.category, Category::Mods);
        // Mod commands keep their declared level in help.
        assert!(help_lines_in(extras, None, false, "!").join("\n").contains("motd"));
        assert!(!help_lines_in(extras, None, false, "!").join("\n").contains("secret"));
        // Built-ins still resolve to the built-in spec.
        assert_eq!(name(&parse_in(extras, "kick Bob").unwrap()), "player kick");
        assert!(parse_in(extras, "nonsense").is_err());
        assert!(wire(extras[0].spec).starts_with("cmdspec\t{"));
    }

    #[test]
    fn replies_and_aliases_fill_in_the_caller_and_the_arguments() {
        assert_eq!(fill("Hi {player}, you said {args}.", "Ann", "boo"), "Hi Ann, you said boo.");
        assert_eq!(fill("announce [{player}] {args}", "Ann", "hello all"), "announce [Ann] hello all");
        assert_eq!(fill("no placeholders", "Ann", "x"), "no placeholders");
        // An alias line parses like a typed one.
        let line = fill("where {args}", "Ann", "");
        assert!(parse(line.trim()).is_ok());
    }

    #[test]
    fn every_built_in_is_complete() {
        for c in COMMANDS {
            let n = c.name();
            assert!(!c.path.is_empty() && c.path.len() <= 2, "{n}: path");
            assert!(!c.summary.is_empty(), "{n}: summary");
            assert!(!c.help.is_empty(), "{n}: help");
            assert!(!c.examples.is_empty(), "{n}: examples");
            let page = c.overlay.unwrap_or_else(|| panic!("{n}: overlay"));
            assert!(poe_hook_shared::cmdspec::DEBUG_PAGES.contains(&page), "{n}: {page} is no overlay page");
        }
    }

    #[test]
    fn names_are_unambiguous() {
        let mut seen: Vec<String> = Vec::new();
        for c in COMMANDS {
            let name = c.name();
            assert!(!seen.contains(&name), "{name} twice");
            seen.push(name);
        }
        for c in COMMANDS {
            for a in c.aliases {
                assert!(!seen.iter().any(|s| s == a), "alias {a} of {} repeats a name", c.name());
                seen.push((*a).to_owned());
                // A one-word path outranks an alias, so none may have the alias's word.
                assert!(!COMMANDS.iter().any(|o| o.path == [*a]), "alias {a}");
            }
        }
    }

    #[test]
    fn every_command_has_a_wire_line_that_fits_a_datagram() {
        for c in COMMANDS {
            let line = wire(c);
            assert!(line.len() < 3000, "{}: {} bytes", c.name(), line.len());
            let (head, json) = line.split_once('\t').unwrap();
            assert_eq!(head, "cmdspec");
            let back: CmdSpec = serde_json::from_str(json).unwrap();
            assert_eq!(back.name(), c.name());
            assert_eq!(back.admin, c.is_admin());
        }
    }

    #[test]
    fn features_not_built_hide_their_commands() {
        static HIDDEN: Spec = Spec { path: &["level", "set"], requires: Some(Feature::Items), ..BASE };
        assert!(!HIDDEN.available());
        assert!(!HIDDEN.visible_to(true));
        assert!(BASE.available());
    }

    #[test]
    fn the_doc_table_lists_every_command() {
        let table = doc_table();
        for c in COMMANDS {
            assert!(table.contains(&c.name()), "{}", c.name());
        }
        assert!(table.contains("`tp <x\\|player> [y]`"), "{table}");
    }
}
