//! `GET /api/v1/art/{path}`: a game image as PNG, decoded from the server's own game files
//! (nothing of the game is committed or shipped). Two kinds of path, both under `Art/`:
//!
//! - a direct `.dds` file (`Art/2DArt/Logos/POELeagueLogoRunesofAldur.dds`);
//! - a UI image id without extension (`Art/2DArt/UIImages/Common/LeagueStandard`): a rectangle of
//!   a texture atlas, listed in `Art/UIImages*.txt` as
//!   `"<id>" "<texture>.dds" x1 y1 x2 y2` (UTF-16, one image per line).
//!
//! The league art is all BC7; `image_dds` (pure Rust, `bcdec_rs`) also decodes BC1 to BC5 and
//! uncompressed formats. Results are cached in memory.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

use axum::{
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use image_dds::{SurfaceRgba8, ddsfile::Dds};
use poe_data::GameData;

use crate::{ApiError, WebState, public::blocking};

/// Longest accepted path.
const MAX_PATH: usize = 200;
/// Cached PNGs kept at most (the cache is cleared when it is full).
const CACHE_MAX: usize = 512;
/// Seconds a browser may keep an image.
const MAX_AGE: u32 = 86_400;
/// Atlas list files (`Art/UIImages1.txt`, `Art/UIImages2.txt`, ...) are read until one is missing.
const ATLAS_LISTS: u32 = 16;

/// Where a UI image lies: texture path and pixel rectangle (`x1 y1 x2 y2`, `x2`/`y2` exclusive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtlasEntry {
    pub texture: String,
    pub rect: [u32; 4],
}

/// The URL under which the web API serves an image (`None` for an empty path).
pub fn art_url(path: &str) -> Option<String> {
    (!path.is_empty()).then(|| format!("/api/v1/art/{path}"))
}

/// A path the endpoint accepts: relative, under `Art/`, plain characters.
pub fn valid_path(path: &str) -> bool {
    path.len() <= MAX_PATH
        && path.len() > 4
        && path[..4].eq_ignore_ascii_case("art/")
        && !path.contains("//")
        && !path.split('/').any(|p| p.is_empty() || p == "." || p == "..")
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.' | ' ' | '+'))
}

/// Parses the text of an atlas list file into `lowercased id -> entry`.
pub fn parse_atlas(text: &str, into: &mut HashMap<String, AtlasEntry>) {
    for line in text.lines() {
        let mut parts = line.split('"');
        let (Some(_), Some(id), Some(_), Some(texture), Some(rest)) =
            (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let nums: Vec<u32> = rest.split_whitespace().filter_map(|n| n.parse().ok()).collect();
        if let [x1, y1, x2, y2] = nums[..] {
            into.insert(
                id.to_ascii_lowercase(),
                AtlasEntry { texture: texture.to_owned(), rect: [x1, y1, x2, y2] },
            );
        }
    }
}

/// UTF-16LE (with or without byte order mark) or plain UTF-8 text of a game file.
fn decode_text(bytes: &[u8]) -> String {
    let utf16 = bytes.starts_with(&[0xff, 0xfe]) || bytes.get(1) == Some(&0);
    if utf16 {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .skip(usize::from(bytes.starts_with(&[0xff, 0xfe])))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn atlas(data: &GameData) -> &'static HashMap<String, AtlasEntry> {
    static ATLAS: OnceLock<HashMap<String, AtlasEntry>> = OnceLock::new();
    ATLAS.get_or_init(|| {
        let mut map = HashMap::new();
        for n in 1..=ATLAS_LISTS {
            match data.files().read(&format!("Art/UIImages{n}.txt")) {
                Ok(bytes) => parse_atlas(&decode_text(&bytes), &mut map),
                Err(_) => break,
            }
        }
        map
    })
}

fn cache() -> &'static Mutex<HashMap<String, Arc<Vec<u8>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Vec<u8>>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Decodes a DDS and crops `rect` (the whole image without one) to PNG bytes.
pub fn dds_to_png(dds_bytes: &[u8], rect: Option<[u32; 4]>) -> anyhow::Result<Vec<u8>> {
    let dds = Dds::read(&mut std::io::Cursor::new(dds_bytes))?;
    let surface = SurfaceRgba8::decode_layers_mipmaps_dds(&dds, 0..1, 0..1)?;
    let (w, h) = (surface.width, surface.height);
    let pixels = surface
        .get(0, 0, 0)
        .ok_or_else(|| anyhow::anyhow!("the texture has no pixels"))?;
    let [x1, y1, x2, y2] = rect.unwrap_or([0, 0, w, h]);
    let (x2, y2) = (x2.min(w), y2.min(h));
    anyhow::ensure!(x1 < x2 && y1 < y2, "empty rectangle {x1} {y1} {x2} {y2} in a {w}x{h} texture");
    let (cw, ch) = (x2 - x1, y2 - y1);
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for y in y1..y2 {
        let start = ((y * w + x1) * 4) as usize;
        out.extend_from_slice(&pixels[start..start + (cw * 4) as usize]);
    }
    let mut png_bytes = Vec::new();
    let mut enc = png::Encoder::new(&mut png_bytes, cw, ch);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&out)?;
    Ok(png_bytes)
}

/// The PNG for `path`, `None` when the game has no such image.
fn render(data: &GameData, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let (texture, rect) = if path.to_ascii_lowercase().ends_with(".dds") {
        (path.to_owned(), None)
    } else {
        match atlas(data).get(&path.to_ascii_lowercase()) {
            Some(e) => (e.texture.clone(), Some(e.rect)),
            None => return Ok(None),
        }
    };
    let Ok(bytes) = data.files().read(&texture) else {
        return Ok(None);
    };
    dds_to_png(&bytes, rect).map(Some)
}

/// A game image (a `.dds` file or a UI image id under `Art/`) as a PNG. Cached for a day.
#[utoipa::path(get, operation_id = "get_art", path = "/api/v1/art/{path}", tag = "public",
    params(("path" = String, Path, description = "game path under Art/: a .dds file, or a UI image id without extension")),
    responses((status = 200, content_type = "image/png", body = Vec<u8>), (status = 400, body = crate::ErrorBody),
        (status = 404, body = crate::ErrorBody), (status = 503, body = crate::ErrorBody)))]
pub async fn get(State(s): State<WebState>, Path(path): Path<String>) -> Result<Response, ApiError> {
    if !valid_path(&path) {
        return Err(ApiError::bad_request("not an image path under Art/"));
    }
    let Some(data) = s.data.clone() else {
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "this server has no game data loaded"));
    };
    let key = path.to_ascii_lowercase();
    let hit = cache().lock().unwrap().get(&key).cloned();
    let png = match hit {
        Some(p) => p,
        None => {
            let p = path.clone();
            let rendered = blocking(move || render(&data, &p)).await?.map_err(ApiError::internal)?;
            let png = Arc::new(rendered.ok_or_else(|| ApiError::not_found("no such image"))?);
            let mut cache = cache().lock().unwrap();
            if cache.len() >= CACHE_MAX {
                cache.clear();
            }
            cache.insert(key, png.clone());
            png
        }
    };
    let mut res = (*png).clone().into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_str(&format!("public, max-age={MAX_AGE}")).unwrap());
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_checked() {
        assert!(valid_path("Art/2DArt/UIImages/Common/LeagueStandard"));
        assert!(valid_path("art/2DArt/Logos/POELeagueLogoRunesofAldur.dds"));
        for bad in ["", "Art", "Art/", "Data/x.dds", "Art/../x", "Art//x", "/Art/x", "Art/x\\y", "Art/./x", "Art/x?y"] {
            assert!(!valid_path(bad), "{bad:?}");
        }
        assert!(!valid_path(&format!("Art/{}", "a".repeat(300))));
    }

    #[test]
    fn atlas_lines_parse() {
        let mut map = HashMap::new();
        parse_atlas(
            "\"Art/2DArt/UIImages/Common/LeagueStandard\" \"Art/Textures/Interface/2D/x.dds\" 8 0 140 119\nbroken line\n\"a\" \"b.dds\" 1 2 three 4\n",
            &mut map,
        );
        assert_eq!(map.len(), 1);
        let e = &map["art/2dart/uiimages/common/leaguestandard"];
        assert_eq!((e.texture.as_str(), e.rect), ("Art/Textures/Interface/2D/x.dds", [8, 0, 140, 119]));
    }

    #[test]
    fn text_decodes_utf16_and_utf8() {
        let utf16: Vec<u8> = "\"a\" x".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_text(&utf16), "\"a\" x");
        let mut bom = vec![0xff, 0xfe];
        bom.extend(&utf16);
        assert_eq!(decode_text(&bom), "\"a\" x");
        assert_eq!(decode_text(b"\"a\" x"), "\"a\" x");
    }

    #[test]
    fn urls() {
        assert_eq!(art_url(""), None);
        assert_eq!(art_url("Art/x").as_deref(), Some("/api/v1/art/Art/x"));
    }
}
