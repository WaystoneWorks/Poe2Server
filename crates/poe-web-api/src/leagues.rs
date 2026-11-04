//! League rules shared by the web API, the `poe-server league` CLI, the `!league` chat command
//! and the auto-end task: banners of a new family's variants, ending a family (with an audit
//! line per character) and ending families at their end date.

use std::{collections::HashMap, sync::Arc, time::Duration};

use poe_data::{GameData, views::LeagueFlagRow};
use poe_store::{LeagueMove, LeagueRefusal, LeagueStore, Variant};
use tracing::{info, warn};

/// Unix seconds now.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Banners for each member of a family whose base uses `base_flag` (a `LeagueFlag.Id`): the
/// set's Hardcore / SSF / HC SSF rows ([`LeagueFlagRow::variant_of`]); a member without one is
/// left out and gets the base's banner.
pub fn family_flags(data: &GameData, base_flag: &str) -> anyhow::Result<HashMap<Variant, String>> {
    let rows = LeagueFlagRow::all(data)?;
    if !rows.iter().any(|r| r.id == base_flag) {
        anyhow::bail!("no LeagueFlag row {base_flag:?}");
    }
    Ok(Variant::ALL
        .into_iter()
        .filter_map(|v| {
            LeagueFlagRow::variant_of(&rows, base_flag, v.hardcore(), v.ssf()).map(|r| (v, r.id.clone()))
        })
        .collect())
}

/// Ends `family` into `target` and writes one audit log line per moved character (the moves
/// are also stored in `league_moves`). `by` names who ended it (`auto`, an admin).
pub fn end_family(
    store: &dyn LeagueStore,
    family: &str,
    target: &str,
    by: &str,
) -> anyhow::Result<Result<Vec<LeagueMove>, LeagueRefusal>> {
    let outcome = store.end_family(family, target, unix_now())?;
    match &outcome {
        Ok(moves) => {
            for m in moves {
                info!(target: "audit", character = %m.name, id = m.character_id, from = %m.from, to = %m.to, "league ended: character moved");
            }
            info!(target: "audit", %family, %target, moved = moves.len(), %by, "league ended");
        }
        Err(e) => warn!(%family, %target, %by, "league end refused: {e}"),
    }
    Ok(outcome)
}

/// Ends the families whose end date has passed, each into its `migrate_to` family or, without
/// one, the default family. A family with neither stays as it is (listed as over by the client)
/// and is logged once per server run.
pub fn end_due(store: &dyn LeagueStore, warned: &mut Vec<String>) -> anyhow::Result<()> {
    for league in store.due_to_end(unix_now())? {
        let target = league.migrate_to.clone().or_else(|| {
            store
                .leagues()
                .ok()?
                .into_iter()
                .find(|l| l.is_default && l.is_base() && l.family != league.family)
                .map(|l| l.id)
        });
        match target {
            Some(t) => {
                if end_family(store, &league.family, &t, "auto")?.is_err() && !warned.contains(&league.id) {
                    warned.push(league.id.clone());
                }
            }
            None if !warned.contains(&league.id) => {
                warn!(league = %league.id, "league end date passed but it has no target league and there is no default league: end it in the web admin");
                warned.push(league.id.clone());
            }
            None => {}
        }
    }
    Ok(())
}

/// Checks for leagues to end every `every` (the server spawns it).
pub async fn run_auto_end(store: Arc<dyn LeagueStore>, every: Duration) {
    let mut warned = Vec::new();
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        if let Err(e) = end_due(&*store, &mut warned) {
            warn!("league auto-end: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poe_store::{LeagueState, NewLeagueFamily, SqliteStore};

    #[test]
    fn due_families_end_into_their_target_or_the_default() {
        let store = SqliteStore::in_memory().unwrap();
        let fam = |name: &str, permanent, default, ends: Option<i64>, to: Option<&str>| NewLeagueFamily {
            name: name.into(),
            hardcore: true,
            permanent,
            is_default: default,
            ends_at: ends,
            migrate_to: to.map(Into::into),
            ..Default::default()
        };
        store.create_family(&fam("Standard", true, true, None, None), 1).unwrap().unwrap();
        store.create_family(&fam("Keep", true, false, None, None), 1).unwrap().unwrap();
        store.create_family(&fam("Dawn", false, false, Some(10), Some("Keep")), 1).unwrap().unwrap();
        store.create_family(&fam("Dusk", false, false, Some(10), None), 1).unwrap().unwrap();
        store.create_family(&fam("Later", false, false, Some(i64::MAX - 1), None), 1).unwrap().unwrap();
        let mut warned = Vec::new();
        end_due(&store, &mut warned).unwrap();
        let state = |id: &str| store.league(id).unwrap().unwrap();
        assert_eq!((state("Dawn").state, state("Dawn").migrate_to.as_deref()), (LeagueState::Ended, Some("Keep")));
        assert_eq!((state("HC Dusk").state, state("HC Dusk").migrate_to.as_deref()), (LeagueState::Ended, Some("Standard")));
        assert_eq!(state("Later").state, LeagueState::Active);
        assert!(warned.is_empty());
    }
}
