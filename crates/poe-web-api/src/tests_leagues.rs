//! The league endpoints (`league_api`) and `GET /art/...`.

use std::{path::PathBuf, sync::Arc};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use poe_accounts::Role;
use poe_store::NewCharacter;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::tests::{Fixture, fixture, repo_root};

async fn admin(f: &Fixture) -> String {
    f.account("root", "correct horse", Role::Admin);
    f.login("root", "correct horse").await
}

fn ids(v: &Value) -> Vec<&str> {
    v.as_array().unwrap().iter().map(|l| l["id"].as_str().unwrap()).collect()
}

fn character(f: &Fixture, name: &str, league: &str) {
    let account = f.state.accounts.find("root").unwrap().unwrap();
    f.state
        .store
        .create_character(&NewCharacter {
            account_id: account.id,
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

async fn create(f: &Fixture, t: &str, body: Value) -> (StatusCode, Value) {
    f.call("POST", "/api/v1/admin/leagues", Some(t), Some(body)).await
}

#[tokio::test]
async fn league_endpoints_need_an_admin_except_the_public_list() {
    let f = fixture();
    f.account("alice", "correct horse", Role::Player);
    let alice = f.login("alice", "correct horse").await;
    for (method, uri) in [
        ("GET", "/api/v1/admin/leagues"),
        ("POST", "/api/v1/admin/leagues"),
        ("POST", "/api/v1/admin/leagues/Standard"),
        ("POST", "/api/v1/admin/leagues/Standard/end"),
        ("POST", "/api/v1/admin/leagues/Standard/delete"),
        ("POST", "/api/v1/admin/characters/1/league"),
        ("GET", "/api/v1/admin/data/league-flags"),
        ("GET", "/api/v1/admin/data/league-mechanics"),
        ("GET", "/api/v1/admin/data/areas"),
    ] {
        assert_eq!(f.call(method, uri, None, None).await.0, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(f.call(method, uri, Some(&alice), Some(json!({}))).await.0, StatusCode::FORBIDDEN, "{uri}");
    }
    assert_eq!(f.call("GET", "/api/v1/leagues", None, None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn create_list_update_and_hide() {
    let f = fixture();
    let t = admin(&f).await;
    let (status, rows) = create(&f, &t, json!({"name": "Dawn", "hardcore": true, "ssf": true, "default": true, "ends_at": 4_000_000_000i64})).await;
    assert_eq!(status, StatusCode::CREATED, "{rows}");
    assert_eq!(ids(&rows), ["Dawn", "HC Dawn", "SSF Dawn", "HC SSF Dawn"]);
    assert_eq!(rows[0]["state"], "active");
    assert!(rows[0]["is_default"].as_bool().unwrap());
    assert_eq!(f.calls(), ["root: record league create [Dawn, HC Dawn, SSF Dawn, HC SSF Dawn]"]);

    // Public list: active leagues only; no game data means no banner.
    let (status, list) = f.call("GET", "/api/v1/leagues", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&list), ["Dawn", "HC Dawn", "SSF Dawn", "HC SSF Dawn"]);
    assert!(list[0]["banner_url"].is_null() && list[0]["flag"].is_null());
    assert_eq!(list[0]["ends_at"], 4_000_000_000i64);

    // Hide one variant and rename it; the public list drops it, the admin list keeps it.
    let (status, hc) = f
        .call("POST", "/api/v1/admin/leagues/HC%20Dawn", Some(&t), Some(json!({"state": "hidden", "display_name": "Hard Dawn"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{hc}");
    assert_eq!((hc["state"].as_str(), hc["display_name"].as_str()), (Some("hidden"), Some("Hard Dawn")));
    let (_, list) = f.call("GET", "/api/v1/leagues", None, None).await;
    assert_eq!(ids(&list), ["Dawn", "SSF Dawn", "HC SSF Dawn"]);
    let (_, all) = f.call("GET", "/api/v1/admin/leagues", Some(&t), None).await;
    assert_eq!(all.as_array().unwrap().len(), 4);
    assert_eq!(all[1]["characters"], 0);

    // Nullable fields: absent keeps, null clears (family-wide).
    let (status, d) = f.call("POST", "/api/v1/admin/leagues/Dawn", Some(&t), Some(json!({"permanent": false}))).await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["ends_at"], 4_000_000_000i64);
    let (_, d) = f.call("POST", "/api/v1/admin/leagues/Dawn", Some(&t), Some(json!({"ends_at": null}))).await;
    assert!(d["ends_at"].is_null());
    let (_, d) = f.call("POST", "/api/v1/admin/leagues/Dawn", Some(&t), Some(json!({"starts_at": 5}))).await;
    assert_eq!((d["starts_at"].as_i64(), d["ends_at"].is_null()), (Some(5), true));
    assert!(f.calls().contains(&"root: record league update [HC Dawn]".to_owned()), "{:?}", f.calls());
}

#[tokio::test]
async fn refusals_map_to_status_codes() {
    let f = fixture();
    let t = admin(&f).await;
    assert_eq!(create(&f, &t, json!({"name": "Dawn"})).await.0, StatusCode::CREATED);
    assert_eq!(create(&f, &t, json!({"name": "Dawn"})).await.0, StatusCode::CONFLICT);
    let (status, body) = create(&f, &t, json!({"name": "  "})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("invalid league name"));
    assert_eq!(create(&f, &t, json!({"name": "Dusk", "starts_at": 10, "ends_at": 5})).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(create(&f, &t, json!({"name": "Dusk", "migrate_to": "Nowhere"})).await.0, StatusCode::BAD_REQUEST);
    // Game data needed for flags and mechanics.
    assert_eq!(create(&f, &t, json!({"name": "Dusk", "flag": "StandardFlag"})).await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(create(&f, &t, json!({"name": "Dusk", "mechanics": ["Breach"]})).await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(f.call("GET", "/api/v1/admin/data/league-flags", Some(&t), None).await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(f.call("GET", "/api/v1/admin/data/league-mechanics", Some(&t), None).await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(f.call("GET", "/api/v1/admin/data/areas", Some(&t), None).await.0, StatusCode::SERVICE_UNAVAILABLE);

    let post = |uri: &'static str, body: Value| {
        let (f, t) = (&f, &t);
        async move { f.call("POST", uri, Some(t), Some(body)).await }
    };
    assert_eq!(post("/api/v1/admin/leagues/Nope", json!({"state": "hidden"})).await.0, StatusCode::NOT_FOUND);
    assert_eq!(post("/api/v1/admin/leagues/Dawn", json!({"state": "ended"})).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(post("/api/v1/admin/leagues/Dawn", json!({"state": 5})).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(post("/api/v1/admin/leagues/Nope/end", json!({"target": "Dawn"})).await.0, StatusCode::NOT_FOUND);
    assert_eq!(post("/api/v1/admin/leagues/Dawn/end", json!({"target": "Dawn"})).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(post("/api/v1/admin/leagues/Dawn/end", json!({"target": "Nowhere"})).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(post("/api/v1/admin/leagues/Nope/delete", json!({"confirm": "Nope"})).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ending_moves_characters_and_a_delete_needs_the_exact_id() {
    let f = fixture();
    let t = admin(&f).await;
    assert_eq!(create(&f, &t, json!({"name": "Standard", "hardcore": true, "permanent": true, "default": true})).await.0, StatusCode::CREATED);
    assert_eq!(create(&f, &t, json!({"name": "Dawn", "hardcore": true})).await.0, StatusCode::CREATED);
    character(&f, "Bob", "Dawn");
    character(&f, "Hardy", "HC Dawn");

    let (_, all) = f.call("GET", "/api/v1/admin/leagues", Some(&t), None).await;
    let count = |id: &str| all.as_array().unwrap().iter().find(|l| l["id"] == id).unwrap()["characters"].as_u64().unwrap();
    assert_eq!((count("Dawn"), count("HC Dawn")), (1, 1));

    // Not empty: refused with 409, nothing deleted.
    let (status, _) = f.call("POST", "/api/v1/admin/leagues/Dawn/delete", Some(&t), Some(json!({"confirm": "Dawn"}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Wrong confirmation: 400 before anything else.
    let (status, _) = f.call("POST", "/api/v1/admin/leagues/Dawn/delete", Some(&t), Some(json!({"confirm": "dawn"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Any member ends the family.
    let (status, end) = f.call("POST", "/api/v1/admin/leagues/HC%20Dawn/end", Some(&t), Some(json!({"target": "Standard"}))).await;
    assert_eq!(status, StatusCode::OK, "{end}");
    assert_eq!((end["family"].as_str(), end["count"].as_u64()), (Some("Dawn"), Some(2)));
    let moved: Vec<(&str, &str, &str)> = end["moved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["name"].as_str().unwrap(), m["from"].as_str().unwrap(), m["to"].as_str().unwrap()))
        .collect();
    assert!(moved.contains(&("Bob", "Dawn", "Standard")) && moved.contains(&("Hardy", "HC Dawn", "Hardcore")), "{moved:?}");
    let (_, list) = f.call("GET", "/api/v1/leagues", None, None).await;
    assert!(!ids(&list).contains(&"Dawn"));
    let (status, _) = f.call("POST", "/api/v1/admin/leagues/Dawn/end", Some(&t), Some(json!({"target": "Standard"}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(f.calls().contains(&"root: record league end [Dawn -> Standard: 2 moved]".to_owned()), "{:?}", f.calls());

    // Empty now: deleting the base removes the family.
    let (status, del) = f.call("POST", "/api/v1/admin/leagues/Dawn/delete", Some(&t), Some(json!({"confirm": "Dawn"}))).await;
    assert_eq!(status, StatusCode::OK, "{del}");
    assert_eq!(del["deleted"], json!(["Dawn", "HC Dawn"]));
    let (_, all) = f.call("GET", "/api/v1/admin/leagues", Some(&t), None).await;
    assert!(!ids(&all).contains(&"Dawn"));
}

#[tokio::test]
async fn admins_move_offline_characters_to_any_league() {
    let f = fixture();
    let t = admin(&f).await;
    create(&f, &t, json!({"name": "Standard", "permanent": true})).await;
    create(&f, &t, json!({"name": "Dawn", "hardcore": true, "ssf": true, "hc_death": "stay"})).await;
    // Bob is online in the fixture; Sam is not.
    character(&f, "Bob", "SSF Dawn");
    character(&f, "Sam", "SSF Dawn");
    let uri = |id: u32| format!("/api/v1/admin/characters/{id}/league");
    let (st, body) = f.call("POST", &uri(1), Some(&t), Some(json!({"league": "Standard"}))).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    let (st, body) = f.call("POST", &uri(2), Some(&t), Some(json!({"league": "Standard"}))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!((body["name"].as_str(), body["from"].as_str(), body["to"].as_str()), (Some("Sam"), Some("SSF Dawn"), Some("Standard")));
    assert_eq!(f.call("POST", &uri(2), Some(&t), Some(json!({"league": "Standard"}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.call("POST", &uri(2), Some(&t), Some(json!({"league": "Nope"}))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(f.call("POST", &uri(99), Some(&t), Some(json!({"league": "Standard"}))).await.0, StatusCode::NOT_FOUND);
    let (_, all) = f.call("GET", "/api/v1/admin/leagues", Some(&t), None).await;
    let dawn = all.as_array().unwrap().iter().find(|l| l["id"] == "HC Dawn").unwrap();
    assert_eq!(dawn["hc_death"], "stay");
    assert_eq!(create(&f, &t, json!({"name": "Dusk", "hc_death": "bogus"})).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn art_needs_a_valid_path_and_game_data() {
    let f = fixture();
    let (status, _) = f.call("GET", "/api/v1/art/Data/x.dds", None, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = f.call("GET", "/api/v1/art/Art/2DArt/Logos/x.dds", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// With the real game files (skipped without `GameFiles/` and the schema): flags, mechanics,
/// art as PNG, league creation with a flag.
#[tokio::test]
async fn real_game_data_gives_flags_mechanics_and_art() {
    let root: PathBuf = repo_root();
    let game = root.join("GameFiles");
    let schema = root.join("third_party/dat-schema/schema-poe2.min.json");
    if !game.join("Bundles2/_.index.bin").exists() || !schema.exists() {
        eprintln!("skipping: GameFiles or schema not present");
        return;
    }
    let mut f = fixture();
    f.state.data = Some(Arc::new(poe_data::GameData::open(&game, &schema).unwrap()));
    f.app = crate::router(f.state.clone());
    let t = admin(&f).await;

    let (status, flags) = f.call("GET", "/api/v1/admin/data/league-flags", Some(&t), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ids(&flags).contains(&"StandardFlag"));
    let (status, mech) = f.call("GET", "/api/v1/admin/data/league-mechanics", Some(&t), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(mech.as_array().unwrap().iter().all(|m| m["implemented"] == false && !m["name"].as_str().unwrap().is_empty()));
    let a_mechanic = mech[0]["id"].as_str().unwrap().to_owned();
    let (status, areas) = f.call("GET", "/api/v1/admin/data/areas", Some(&t), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ids(&areas).contains(&"G1_1"), "areas list has the Riverbank");

    assert_eq!(create(&f, &t, json!({"name": "Dusk", "flag": "NoSuchFlag"})).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(create(&f, &t, json!({"name": "Dusk", "mechanics": ["NoSuchMechanic"]})).await.0, StatusCode::BAD_REQUEST);
    let (status, rows) = create(&f, &t, json!({"name": "Dawn", "hardcore": true, "flag": "StandardFlag", "mechanics": [a_mechanic]})).await;
    assert_eq!(status, StatusCode::CREATED, "{rows}");
    assert_eq!(rows[1]["flag"], "HardcoreFlag");
    let (_, list) = f.call("GET", "/api/v1/leagues", None, None).await;
    let banner = list[0]["banner_url"].as_str().expect("the flag has a banner").to_owned();
    assert!(banner.starts_with("/api/v1/art/Art/"));

    let png = |uri: String| {
        let app = f.app.clone();
        async move {
            let res = app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
            let (status, headers) = (res.status(), res.headers().clone());
            (status, headers, res.into_body().collect().await.unwrap().to_bytes())
        }
    };
    // A UI image (a rectangle of an atlas) and a direct .dds file.
    for uri in [banner, "/api/v1/art/Art/2DArt/Logos/POELeagueLogoRunesofAldur.dds".to_owned()] {
        let (status, headers, bytes) = png(uri.clone()).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(headers[header::CONTENT_TYPE], "image/png");
        assert_eq!(headers[header::CACHE_CONTROL], "public, max-age=86400");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{uri}");
    }
    let (status, ..) = png("/api/v1/art/Art/2DArt/UIImages/Common/NoSuchImage".into()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, ..) = png("/api/v1/art/Art/2DArt/Logos/NoSuchFile.dds".into()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
