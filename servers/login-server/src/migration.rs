//! The client's League Migrations window on character select: what it lists and the moves it
//! asks for. Protocol: docs/protocol/login.md, "League migrations".

use poe_data::views::league_flag_index;
use poe_net::Connection;
use poe_proto::{
    backend_error::{self as be, BackendError},
    login::{LeagueEntry, LeaguesWithProgress, MigrationResult},
};
use poe_store::{Account, Character, League, LeagueMove, LeagueRefusal, LeagueState, LeagueStore, Store};
use tracing::{info, warn};

use crate::{LoginServer, session};

/// What a Migrate click names: one character, or a league for all the account's characters.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Target<'a> {
    Character(&'a str),
    League(&'a str),
}

/// The window's (from, to) pairs: each league with a character of the account that has a
/// player move into a league that has not ended, once, in the order of the characters.
fn pairs<'a>(leagues: &'a [League], characters: &[Character]) -> Vec<(&'a League, &'a League)> {
    let mut out: Vec<(&League, &League)> = Vec::new();
    for c in characters {
        if out.iter().any(|(from, _)| from.id == c.league) {
            continue;
        }
        let Some(from) = leagues.iter().find(|l| l.id == c.league) else {
            continue;
        };
        let Some((_, to)) = from.player_leave() else {
            continue;
        };
        if let Some(to) = leagues.iter().find(|l| l.id == to && l.state != LeagueState::Ended) {
            out.push((from, to));
        }
    }
    out
}

/// The `0x001a` entries: `[from, to, from, to, ...]`.
fn entries(leagues: &[League], characters: &[Character], flag_row: impl Fn(&str) -> Option<u32>) -> Vec<LeagueEntry> {
    pairs(leagues, characters)
        .into_iter()
        .flat_map(|(from, to)| [from, to])
        .map(|l| session::league_entry(l, leagues, &flag_row))
        .collect()
}

/// Sends the window's list. An empty one makes the client say there is nothing to migrate.
pub(crate) async fn send_list(server: &LoginServer, conn: &mut Connection, account_id: u32) -> anyhow::Result<()> {
    let leagues = server.leagues.leagues()?;
    let characters = server.store.characters(account_id)?;
    let flag_row = |id: &str| league_flag_index(&server.data, id).ok().flatten();
    let list = LeaguesWithProgress { result: 0, leagues: entries(&leagues, &characters, flag_row) };
    info!(pairs = list.leagues.len() / 2, "leagues with progress sent");
    conn.send(&server.protocol.login.encode(&list)?).await?;
    Ok(())
}

/// Runs a Migrate click and answers it: the result, then on success the new character list and
/// the new window list (the client asks for neither; result 0 alone changes nothing on screen).
pub(crate) async fn run(server: &LoginServer, conn: &mut Connection, account: &Account, target: Target<'_>) -> anyhow::Result<()> {
    let online = |name: &str| server.admin.online().iter().any(|o| o.character.eq_ignore_ascii_case(name));
    let outcome = migrate(&*server.store, &*server.leagues, &online, account, target, session::unix_now());
    let result = MigrationResult { result: outcome.as_ref().err().map_or(0, |e| e.code), account_id: account.id };
    conn.send(&server.protocol.login.encode(&result)?).await?;
    match outcome {
        Ok(moves) => {
            for m in &moves {
                info!(name = %m.name, from = %m.from, to = %m.to, "league migration");
            }
            session::send_character_list(server, conn, account.id).await?;
            send_list(server, conn, account.id).await?;
        }
        Err(e) => info!(?target, code = e.code, "league migration refused"),
    }
    Ok(())
}

/// The move itself, or the `BackendErrors` code the client shows under "Migration Failed".
fn migrate(
    store: &dyn Store,
    leagues: &dyn LeagueStore,
    online: &dyn Fn(&str) -> bool,
    account: &Account,
    target: Target<'_>,
    now: i64,
) -> Result<Vec<LeagueMove>, BackendError> {
    let db = |e: anyhow::Error| {
        warn!("league migration: {e}");
        be::DATABASE_ERROR
    };
    let characters = store.characters(account.id).map_err(|e| db(e.into()))?;
    let refusal = |r: LeagueRefusal| {
        info!("league migration: {r}");
        match r {
            LeagueRefusal::NotFound(_) => be::LEAGUE_DOES_NOT_EXIST,
            _ => be::LEAGUE_DISALLOWS_MANUAL_MIGRATION,
        }
    };
    match target {
        Target::Character(name) => {
            let Some(c) = characters.iter().find(|c| c.name == name) else {
                return Err(session::not_owned_code(store, name));
            };
            if online(&c.name) {
                return Err(be::ACCOUNT_IS_ONLINE);
            }
            let league = leagues.league(&c.league).map_err(|e| db(e.into()))?;
            let Some((kind, _)) = league.and_then(|l| l.player_leave()) else {
                return Err(be::LEAGUE_DISALLOWS_MANUAL_MIGRATION);
            };
            let m = leagues.migrate_character(c.id, kind, &account.name, now).map_err(|e| db(e.into()))?;
            m.map(|m| vec![m]).map_err(refusal)
        }
        Target::League(id) => {
            if characters.iter().any(|c| c.league == id && online(&c.name)) {
                return Err(be::ACCOUNT_IS_ONLINE);
            }
            leagues.migrate_account_league(account.id, id, &account.name, now).map_err(|e| db(e.into()))?.map_err(refusal)
        }
    }
}

#[cfg(test)]
mod tests {
    use poe_proto::Protocol;
    use poe_store::{NewCharacter, NewLeagueFamily, SqliteStore};

    use super::*;

    /// Account "a" with a character in `SSF Dawn`, `HC SSF Dawn` and `Dawn`; account "b" with one.
    fn world() -> (SqliteStore, Account, Account) {
        let store = SqliteStore::in_memory().unwrap();
        let family = NewLeagueFamily { name: "Dawn".into(), hardcore: true, ssf: true, ..Default::default() };
        store.create_family(&family, 1).unwrap().unwrap();
        let (a, b) = (store.create_account("a").unwrap(), store.create_account("b").unwrap());
        for (account, name, league) in
            [(a.id, "OneSsf", "SSF Dawn"), (a.id, "TwoSsf", "SSF Dawn"), (a.id, "BothFlags", "HC SSF Dawn"), (a.id, "Plain", "Dawn"), (b.id, "Theirs", "SSF Dawn")]
        {
            store
                .create_character(&NewCharacter {
                    account_id: account,
                    name: name.into(),
                    league: league.into(),
                    class_id: 2,
                    start_state: "DexFourDefault".into(),
                    level: 1,
                    area: "G1_1".into(),
                })
                .unwrap()
                .unwrap();
        }
        (store, a, b)
    }

    #[test]
    fn the_window_lists_each_movable_league_of_the_account_once_in_pairs() {
        let (store, a, b) = world();
        let leagues = store.leagues().unwrap();
        let ids = |account: &Account| -> Vec<String> {
            entries(&leagues, &store.characters(account.id).unwrap(), |_| None).into_iter().map(|e| e.id).collect()
        };
        assert_eq!(ids(&a), ["SSF Dawn", "Dawn", "HC SSF Dawn", "HC Dawn"]);
        assert_eq!(ids(&b), ["SSF Dawn", "Dawn"]);
    }

    #[test]
    fn a_target_that_ended_is_not_listed() {
        let (store, a, _) = world();
        let mut leagues = store.leagues().unwrap();
        leagues.iter_mut().find(|l| l.id == "Dawn").unwrap().state = LeagueState::Ended;
        let listed = entries(&leagues, &store.characters(a.id).unwrap(), |_| None);
        assert_eq!(listed.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["HC SSF Dawn", "HC Dawn"]);
    }

    #[test]
    fn migrate_moves_one_or_all_and_refuses_with_the_client_codes() {
        let (store, a, _) = world();
        let nobody = |_: &str| false;
        let go = |online: &dyn Fn(&str) -> bool, target| migrate(&store, &store, online, &a, target, 5);
        let code = |r: Result<Vec<LeagueMove>, BackendError>| r.unwrap_err().code;

        assert_eq!(code(go(&nobody, Target::Character("Theirs"))), be::NOT_CHARACTER_OWNER.code);
        assert_eq!(code(go(&nobody, Target::Character("Nobody"))), be::CHARACTER_DOES_NOT_EXIST.code);
        assert_eq!(code(go(&nobody, Target::Character("Plain"))), be::LEAGUE_DISALLOWS_MANUAL_MIGRATION.code);
        assert_eq!(code(go(&nobody, Target::League("Nope"))), be::LEAGUE_DOES_NOT_EXIST.code);
        // Online: one character, or any character of a Migrate All; nothing moves.
        let two_online = |n: &str| n == "TwoSsf";
        assert_eq!(code(go(&two_online, Target::Character("TwoSsf"))), be::ACCOUNT_IS_ONLINE.code);
        assert_eq!(code(go(&two_online, Target::League("SSF Dawn"))), be::ACCOUNT_IS_ONLINE.code);

        // Both flags: SSF goes first, then hardcore.
        let moved = go(&nobody, Target::Character("BothFlags")).unwrap();
        assert_eq!((moved[0].from.as_str(), moved[0].to.as_str()), ("HC SSF Dawn", "HC Dawn"));
        let moved = go(&nobody, Target::Character("BothFlags")).unwrap();
        assert_eq!((moved[0].from.as_str(), moved[0].to.as_str()), ("HC Dawn", "Dawn"));
        // All of the account's, not the other account's.
        let moved = go(&nobody, Target::League("SSF Dawn")).unwrap();
        assert_eq!(moved.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["OneSsf", "TwoSsf"]);
        assert_eq!(code(go(&nobody, Target::League("SSF Dawn"))), be::LEAGUE_DISALLOWS_MANUAL_MIGRATION.code);
    }

    /// The bytes the client's readers expect (opcodes from `login.toml`).
    #[test]
    fn the_messages_on_the_wire() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let p = Protocol::load(&poe_config::VersionFile::latest(&root).unwrap().opcodes_dir(&root)).unwrap();
        let wire = p.login.encode(&MigrationResult { result: be::ACCOUNT_IS_ONLINE.code, account_id: 0x0102 }).unwrap();
        assert_eq!(wire, [0, 0x44, 0x01, 0x80, 0, 0, 1, 2]);
        let wire = p.login.encode(&LeaguesWithProgress::default()).unwrap();
        assert_eq!(wire, [0, 0x1a, 0, 0, 0, 0, 0, 0]);
    }
}
