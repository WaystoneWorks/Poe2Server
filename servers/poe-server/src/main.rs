mod lifecycle;
mod mods_cli;

use std::sync::Arc;

use login_server::LoginServer;
use poe_cluster::{MemoryInstances, MemorySessions, MemoryTickets};
use poe_config::Config;
use poe_data::GameData;
use poe_proto::Protocol;
use poe_store::SqliteStore;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    lifecycle::wait_for_parent().await;
    let config = Arc::new(Config::load_default()?);
    // `poe-server mod <list|install|enable|disable|verify>` manages the mods folder offline.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("mod") {
        return mods_cli::run(&config.mods.dir, &args[1..]);
    }

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.logging.filter));
    // The web API keeps recent events for its live log viewer (`[web] log_buffer`).
    let logs = poe_web_api::LogBuffer::new(config.web.log_buffer);
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(logs.layer())
        .init();
    tracing::info!(version = %config.game.version, "starting poe-server");
    if let Some(v) = &config.version_info {
        tracing::info!(
            build_revision = %v.build_revision,
            status = ?v.status,
            opcodes = %config.game.protocol_dir.display(),
            client_hashes = v.client.hashes.len(),
            hashes_confirmed = v.client.hashes_confirmed,
            allow_unknown_client = config.security.allow_unknown_client,
            "client build (modules/protocol/versions/{}.toml)",
            v.version
        );
        if v.status != poe_config::versions::Status::Supported {
            tracing::warn!("client {} is `{:?}`, not a supported build", v.version, v.status);
        }
    }
    for w in config.network.warnings().into_iter().chain(config.security_warnings()) {
        tracing::warn!("{w}");
    }

    let protocol = Arc::new(Protocol::load(&config.game.protocol_dir)?);
    let store = Arc::new(SqliteStore::open(&config.database.path)?);
    let accounts: Arc<dyn poe_accounts::AccountStore> =
        Arc::new(poe_accounts::SqliteAccounts::new(store.clone(), config.cluster.id.clone()));
    // `poe-server account <create|role|disable|enable|passwd|list> [name] [value]` manages
    // accounts offline (the first admin), then exits. `create`/`passwd` take the password with
    // `--password-stdin` (one line on standard input; it is not visible in the process list).
    // The password as a command-line argument still works but is discouraged.
    if args.first().map(String::as_str) == Some("account") {
        let cli = poe_accounts::parse_account_cli(&args[1..], &mut std::io::stdin().lock()).map_err(anyhow::Error::msg)?;
        return match poe_accounts::run_account_command(&*accounts, None, &cli.sub, cli.name.as_deref(), cli.value.as_deref()) {
            Ok(done) => {
                println!("{}", done.text);
                Ok(())
            }
            Err(e) => anyhow::bail!(e),
        };
    }
    // `poe-server character <create|list> ...` manages characters offline (test setup), then exits.
    if args.first().map(String::as_str) == Some("character") {
        return character_command(&config, &store, &*accounts, &args[1..]);
    }
    // `poe-server league <list|create|end|delete> ...` manages leagues offline, then exits.
    if args.first().map(String::as_str) == Some("league") {
        return league_command(&config, &store, &args[1..]);
    }
    let tickets = Arc::new(MemoryTickets::new(std::time::Duration::from_secs(60)));
    let sessions = Arc::new(MemorySessions::default());
    let instances = Arc::new(MemoryInstances::default());
    // Server mods: planned and loaded before anything serves. Their commands join the registry
    // and their data overlays go in front of the game files.
    let mods = Arc::new(poe_mod_host::ModHost::load(&poe_mod_host::HostConfig {
        mods: config.mods.clone(),
        game_version: config.game.version.clone(),
        server_version: env!("CARGO_PKG_VERSION").to_owned(),
        server_dir: std::env::current_dir().unwrap_or_default(),
        reserved_commands: instance_server::commands::COMMANDS.iter().map(|c| c.name.to_owned()).collect(),
    }));
    instance_server::commands::install_mod_commands(mods.commands());
    let mut game_data = GameData::open(&config.game.game_dir, &config.game.schema)?;
    for dir in mods.overlay_dirs() {
        tracing::info!(dir = %dir.display(), "mods: data overlay");
        game_data.files_mut().push_override(dir.clone());
    }
    let data = Arc::new(game_data);
    tracing::info!(game_dir = %config.game.game_dir.display(), "game data opened");
    // One limiter for the login server and the web API.
    let limiter = Arc::new(poe_accounts::LoginLimiter::from_config(&config.security));
    let login = Arc::new(LoginServer {
        config: config.clone(),
        protocol: protocol.clone(),
        store: store.clone(),
        leagues: store.clone(),
        accounts: accounts.clone(),
        limiter: limiter.clone(),
        data: data.clone(),
        tickets: tickets.clone(),
        sessions: sessions.clone(),
        instances: instances.clone(),
    });

    let instance = Arc::new(instance_server::InstanceServer {
        config: config.clone(),
        protocol: protocol.clone(),
        tickets: tickets.clone(),
        data: data.clone(),
        stats: Arc::new(poe_game::stats::StatRegistry::load(&data)?),
        store: store.clone(),
        leagues: store.clone(),
        accounts: accounts.clone(),
        seeds: Arc::new(instance_server::seeds::KnownSeeds::load(
            &config.dev.known_seeds,
        )),
        reports: Default::default(),
        terrain: Arc::new(instance_server::terrain_store::TerrainStore::new(&config.spawn)),
        sessions,
        instances: instances.clone(),
        live: Default::default(),
        actors: Default::default(),
        mods: mods.clone(),
        layouts: Default::default(),
    });

    let (web_lifecycle, mut shutdown_requests) = poe_web_api::Lifecycle::new();
    // Dashboard charts: a sampler fills the ring buffer (players, instances, frame time of the
    // instance actors, memory and CPU of this process, network traffic of the game connections).
    let metrics = Arc::new(poe_cluster::Metrics::new(
        config.metrics.interval_secs,
        config.metrics.retention_hours * 3600,
    ));
    if config.metrics.enabled {
        poe_cluster::metrics::spawn_sampler(Arc::new(poe_cluster::Sampler::new(
            metrics.clone(),
            instance.actors.ticks.clone(),
            instance.clone(),
            instances.clone(),
            Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as i64)
            }),
        )
        .with_net_counter(Arc::new(|| (poe_net::stats::bytes_in(), poe_net::stats::bytes_out())))));
    }
    // Scheduled tasks (scheduled restarts), in the server's local time zone.
    let scheduler = Arc::new(poe_web_api::scheduler::Scheduler::new(
        store.clone(),
        instance.clone(),
        web_lifecycle.clone(),
        accounts.clone(),
        Arc::new(poe_web_api::SystemClock),
        jiff::tz::TimeZone::system(),
        &config.scheduler,
    ));
    tokio::spawn(scheduler.clone().run());
    // Leagues end at their end date (characters move to the target family).
    tokio::spawn(poe_web_api::leagues::run_auto_end(store.clone(), std::time::Duration::from_secs(30)));
    if poe_store::LeagueStore::leagues(&*store)?.iter().all(|l| l.state != poe_store::LeagueState::Active) {
        tracing::warn!("this server has no league: players can log in but cannot create characters (web admin, Leagues page)");
    }
    let web_state = poe_web_api::WebState {
        config: config.clone(),
        accounts: accounts.clone(),
        limiter,
        store: store.clone(),
        leagues: store.clone(),
        data: Some(data.clone()),
        instances: instances.clone(),
        admin: instance.clone(),
        logs: logs.clone(),
        web_tokens: Default::default(),
        ws_tickets: Default::default(),
        lifecycle: web_lifecycle,
        metrics,
        scheduler,
        motd: Arc::new(poe_web_api::Motd::new(&config.server.motd)),
        mods: mods.clone(),
        config_root: std::path::PathBuf::from("."),
        started: std::time::Instant::now(),
    };
    let web_enabled = config.web.enabled;
    let web = async move {
        if web_enabled {
            poe_web_api::serve(web_state).await
        } else {
            tracing::info!("web api disabled ([web] enabled = false)");
            std::future::pending().await
        }
    };
    mods.dispatch(&poe_mod_host::EventData::server(poe_mod_host::EVENT_SERVER_START));
    // Runs until a failure or until a stop or restart is requested: by an admin (web API,
    // after its countdown) or by SIGINT/SIGTERM (at once).
    let kind = {
        let run = async { tokio::try_join!(login.clone().run(), instance.clone().run(), web) };
        tokio::select! {
            r = run => {
                r?;
                return Ok(());
            }
            k = shutdown_requests.recv() => k.unwrap_or(poe_web_api::ShutdownKind::Stop),
            () = lifecycle::signal() => poe_web_api::ShutdownKind::Stop,
        }
    };
    tracing::info!(action = kind.as_str(), "shutting down: disconnecting players");
    // Mods hear about the stop first (while players are still there), then shut down after.
    let submitted = mods.dispatch(&poe_mod_host::EventData::server(poe_mod_host::EVENT_SERVER_STOP));
    for s in submitted {
        if let poe_mod_host::ModCommand::Broadcast { text } = s.command {
            instance.live.broadcast(instance_server::live::ChatOut { sender: instance_server::SERVER_NAME.into(), text }, |_| true);
        }
    }
    drain(&instance).await;
    mods.shutdown();
    let action = lifecycle::decide(kind, config.server.restart_mode, lifecycle::HAS_EXEC);
    tracing::info!(?action, "shutdown complete");
    lifecycle::perform(action)
}

/// Disconnects every player and waits (a few seconds at most) until their sessions have ended:
/// a session saves its character when it ends. The database commits every write at once, so
/// nothing else is pending afterwards.
async fn drain(instance: &instance_server::InstanceServer) {
    for p in instance.live.online() {
        instance.live.kick(&p.character);
    }
    let end = tokio::time::Instant::now() + lifecycle::DRAIN_TIMEOUT;
    while !instance.live.online().is_empty() && tokio::time::Instant::now() < end {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let left = instance.live.online().len();
    if left > 0 {
        tracing::warn!(left, "sessions still open after the drain timeout");
    }
}

const CHARACTER_USAGE: &str = "usage: poe-server character create <account> <name> <start_state> [area] [league]\n       poe-server character list [account]\n  start_state: a CharacterStartStates id (e.g. DexFourDefault); area: a WorldAreas id (default: the start state's hub area, else [player] start_area); league default: the default league (`poe-server league list`). A character created with an explicit town/hideout area counts as having played the intro.";

/// Offline character management for test setups: `character create` makes a character the way
/// the character creation screen does (same slot limit, name rules and start-state lookup).
fn character_command(
    config: &Config,
    store: &Arc<SqliteStore>,
    accounts: &dyn poe_accounts::AccountStore,
    args: &[String],
) -> anyhow::Result<()> {
    use poe_store::{NewCharacter, Store};
    match args.first().map(String::as_str) {
        Some("list") => {
            let all = store.all_characters()?;
            for c in all {
                let owner = accounts.find_by_id(c.account_id)?.map(|a| a.username).unwrap_or_default();
                if args.get(1).is_some_and(|a| *a != owner) {
                    continue;
                }
                println!("{owner}\t{}\t{}\t{}\t{}\t{}\tlevel {}", c.id, c.name, c.start_state, c.league, c.area, c.level);
            }
            Ok(())
        }
        Some("create") if args.len() >= 4 => {
            let (account, name, start_state) = (&args[1], &args[2], &args[3]);
            let Some(owner) = accounts.find(account)? else {
                anyhow::bail!("no account {account:?}");
            };
            let data = GameData::open(&config.game.game_dir, &config.game.schema)?;
            let Some(start) = poe_data::views::StartState::find(&data, start_state)? else {
                anyhow::bail!("unknown start state {start_state:?}");
            };
            let explicit_area = args.get(4).cloned();
            let area = explicit_area
                .clone()
                .or_else(|| start.hub_area.clone())
                .or_else(|| config.player.start_area.clone())
                .unwrap_or_default();
            let Some(world_area) = poe_data::views::WorldArea::find(&data, &area)? else {
                anyhow::bail!("unknown area {area:?}");
            };
            let leagues = poe_store::LeagueStore::leagues(&**store)?;
            let league = match args.get(5) {
                Some(l) => l.clone(),
                None => leagues
                    .iter()
                    .find(|l| l.is_default && l.is_base())
                    .or_else(|| leagues.iter().find(|l| l.state == poe_store::LeagueState::Active))
                    .map(|l| l.id.clone())
                    .ok_or_else(|| anyhow::anyhow!("no league: create one first (poe-server league create)"))?,
            };
            match leagues.iter().find(|l| l.id == league).map(|l| l.open_at(poe_web_api::leagues::unix_now())) {
                None => anyhow::bail!("unknown league {league:?}"),
                Some(Err(why)) => anyhow::bail!("league {league:?} takes no new characters ({why:?})"),
                Some(Ok(())) => {}
            }
            if store.characters(owner.id)?.len() >= config.accounts.max_characters as usize {
                anyhow::bail!("character slots full");
            }
            let created = store.create_character(&NewCharacter {
                account_id: owner.id,
                name: name.clone(),
                league,
                class_id: start.class_id,
                start_state: start.id.clone(),
                level: start.level,
                area,
            })?;
            let Some(c) = created else {
                anyhow::bail!("character name {name:?} is taken");
            };
            if explicit_area.is_some() && (world_area.is_town || world_area.is_hideout) {
                store.set_intro_done(c.id)?;
            }
            println!("created {} (id {}, {}, {}, area {}) for {account}", c.name, c.id, start.class_name, c.league, c.area);
            Ok(())
        }
        _ => anyhow::bail!(CHARACTER_USAGE),
    }
}

const LEAGUE_USAGE: &str = "usage: poe-server league list
       poe-server league create <name> [--hardcore] [--ssf] [--permanent] [--default] [--flag <LeagueFlag.Id>] [--starts <unix s>] [--ends <unix s>] [--migrate-to <league>]
       poe-server league end <league> <target league>
       poe-server league delete <league>
  create makes the base league and, with --hardcore / --ssf, the official variants (HC <name>, SSF <name>, HC SSF <name>); --flag picks the banner, the variants get the set's matching rows. end moves every variant's characters to the target's matching variant. delete removes a variant, or a whole family given its base; only without characters.";

/// Offline league management (`poe-server league ...`); the web admin's Leagues page does the
/// same online.
fn league_command(config: &Config, store: &Arc<SqliteStore>, args: &[String]) -> anyhow::Result<()> {
    use poe_store::{LeagueStore, NewLeagueFamily};
    let refused = |e: poe_store::LeagueRefusal| anyhow::anyhow!("{e}");
    match args.first().map(String::as_str) {
        Some("list") => {
            let counts = store.character_counts()?;
            for l in store.leagues()? {
                let mut tags = Vec::new();
                if l.is_default && l.is_base() {
                    tags.push("default");
                }
                if l.permanent && l.is_base() {
                    tags.push("permanent");
                }
                if l.hardcore {
                    tags.push("HC");
                }
                if l.ssf {
                    tags.push("SSF");
                }
                println!(
                    "{}\t{}\tfamily {}\t{}\t{}\tflag {}\t{} characters",
                    l.id,
                    l.display_name,
                    l.family,
                    l.state.as_str(),
                    tags.join(" "),
                    l.flag.as_deref().unwrap_or("-"),
                    counts.get(&l.id).copied().unwrap_or(0)
                );
            }
            Ok(())
        }
        Some("create") if args.len() >= 2 => {
            let mut new = NewLeagueFamily { name: args[1].clone(), ..Default::default() };
            let mut it = args[2..].iter();
            let num = |v: Option<&String>, what: &str| -> anyhow::Result<i64> {
                v.ok_or_else(|| anyhow::anyhow!("{what} needs a value"))?.parse().map_err(|_| anyhow::anyhow!("{what}: unix seconds"))
            };
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--hardcore" => new.hardcore = true,
                    "--ssf" => new.ssf = true,
                    "--permanent" => new.permanent = true,
                    "--default" => new.is_default = true,
                    "--flag" => {
                        let flag = it.next().ok_or_else(|| anyhow::anyhow!("--flag needs a LeagueFlag id"))?;
                        let data = GameData::open(&config.game.game_dir, &config.game.schema)?;
                        new.flags = poe_web_api::leagues::family_flags(&data, flag)?;
                    }
                    "--starts" => new.starts_at = Some(num(it.next(), "--starts")?),
                    "--ends" => new.ends_at = Some(num(it.next(), "--ends")?),
                    "--migrate-to" => new.migrate_to = it.next().cloned(),
                    other => anyhow::bail!("unknown option {other:?}\n{LEAGUE_USAGE}"),
                }
            }
            let rows = store.create_family(&new, poe_web_api::leagues::unix_now())?.map_err(refused)?;
            let ids: Vec<_> = rows.iter().map(|l| l.id.as_str()).collect();
            println!("created {}", ids.join(", "));
            Ok(())
        }
        Some("end") if args.len() >= 3 => {
            let family = store.league(&args[1])?.map(|l| l.family).unwrap_or_else(|| args[1].clone());
            let moves = poe_web_api::leagues::end_family(&**store, &family, &args[2], "cli")?.map_err(refused)?;
            println!("ended {family}: {} characters moved", moves.len());
            for m in moves {
                println!("{}\t{} -> {}", m.name, m.from, m.to);
            }
            Ok(())
        }
        Some("delete") if args.len() >= 2 => {
            let ids = store.delete_league(&args[1])?.map_err(refused)?;
            println!("deleted {}", ids.join(", "));
            Ok(())
        }
        _ => anyhow::bail!(LEAGUE_USAGE),
    }
}
