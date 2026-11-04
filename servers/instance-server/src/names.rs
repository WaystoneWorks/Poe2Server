//! The name lists behind command arguments (`commands::Names`): from game data (read once, on
//! first use), the league table and the live sessions.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use poe_data::views::NameList;
use tracing::warn;

use crate::{
    InstanceServer,
    commands::{Kind, Name, Names},
};

/// Matches one `complete` request answers with.
pub const COMPLETIONS: usize = 30;

/// Game-data lists already read.
#[derive(Default)]
pub struct NameCache(Mutex<HashMap<NameList, Arc<Vec<Name>>>>);

fn game_list(kind: Kind) -> Option<NameList> {
    Some(match kind {
        Kind::Area => NameList::Area,
        Kind::Waypoint => NameList::Waypoint,
        Kind::Class => NameList::Class,
        Kind::Ascendancy => NameList::Ascendancy,
        Kind::ItemBase => NameList::ItemBase,
        Kind::Unique => NameList::Unique,
        Kind::Mod => NameList::Mod,
        Kind::Stat => NameList::Stat,
        Kind::Currency => NameList::Currency,
        Kind::Gem => NameList::Gem,
        Kind::Monster => NameList::Monster,
        Kind::Rarity => NameList::Rarity,
        Kind::DamageType => NameList::DamageType,
        _ => return None,
    })
}

impl Names for InstanceServer {
    fn names(&self, kind: Kind) -> Option<Arc<Vec<Name>>> {
        let plain = |ids: Vec<String>| Some(Arc::new(ids.into_iter().map(|id| Name { id, display: String::new() }).collect::<Vec<_>>()));
        match kind {
            Kind::Player => return plain(self.live.online().into_iter().map(|o| o.character).collect()),
            Kind::Instance => return plain(self.instances.list().iter().map(|i| i.id.to_string()).collect()),
            Kind::League => {
                return match self.leagues.leagues() {
                    Ok(rows) => plain(rows.into_iter().map(|l| l.id).collect()),
                    Err(e) => {
                        warn!("league names: {e}");
                        None
                    }
                };
            }
            _ => {}
        }
        let list = game_list(kind)?;
        let mut cache = self.names.0.lock().unwrap();
        if let Some(found) = cache.get(&list) {
            return Some(found.clone());
        }
        let rows = match list.rows(&self.data) {
            Ok(rows) => rows,
            Err(e) => {
                warn!(?list, "name list: {e:#}");
                return None;
            }
        };
        let names: Arc<Vec<Name>> = Arc::new(rows.into_iter().map(|r| Name { id: r.id, display: r.display }).collect());
        cache.insert(list, names.clone());
        Some(names)
    }
}
