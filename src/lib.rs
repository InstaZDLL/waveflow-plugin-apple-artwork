//! WaveFlow Apple Motion Artwork plugin — guest side.
//!
//! Implements `waveflow:metadata/v1`'s `album-info` to resolve Apple Music's
//! animated album covers (motion artwork). The algorithm mirrors the public
//! Apple Music web player (clean-room, no vendored code):
//!
//! 1. **iTunes Search** (`itunes.apple.com/search`) → the album's Apple
//!    Music URL, from which we read the storefront + numeric id.
//! 2. **Anonymous token** — GET the album page, find its JS bundles, scrape
//!    the bearer JWT the web player embeds. Cached; re-scraped on a 401/403.
//! 3. **AMP catalogue API** (`amp-api.music.apple.com/.../albums/{id}
//!    ?extend=editorialVideo`) with the token → `editorialVideo`.
//! 4. **Resolve the m3u8 → mp4** — the `motionDetailSquare.video` is an HLS
//!    master playlist; we pick the highest-resolution progressive `.mp4`
//!    variant so the app's native `<video>` can play it (no HLS.js).
//!
//! Every outbound request goes through `waveflow:host/http` (allowlisted to
//! the Apple hosts). Results are cached in the per-plugin scratch store — a
//! positive hit, a negative sentinel, and the token — so a given album hits
//! Apple at most once. That caching IS the rate-limit discipline: the host
//! also serialises calls to this plugin, so there is no request storm to
//! throttle.

#[allow(warnings)]
mod bindings;

use bindings::exports::waveflow::metadata::enricher::{
    AlbumDetails, ArtistDetails, Guest, LyricsLine,
};
use bindings::waveflow::host::config;
use bindings::waveflow::host::http::{self, Request};
use bindings::waveflow::host::log::{self, Level};
use bindings::waveflow::host::storage;

use serde::Deserialize;

const USER_AGENT: &str = concat!("WaveFlow/Apple-Artwork/", env!("CARGO_PKG_VERSION"));

/// Cap on redirect hops we follow manually (the host disables redirects).
const MAX_REDIRECTS: usize = 4;

/// Scratch-store key for the cached anonymous web-player token.
const TOKEN_KEY: &str = "apple:token";

struct AppleArtwork;

impl Guest for AppleArtwork {
    /// Not implemented — this plugin only supplies album motion artwork.
    fn artist_info(_name: String) -> Result<ArtistDetails, String> {
        Ok(ArtistDetails {
            bio: None,
            image_url: None,
            similar: Vec::new(),
        })
    }

    /// Not implemented — see `artist_info`.
    fn lyrics(_artist: String, _title: String) -> Result<Vec<LyricsLine>, String> {
        Ok(Vec::new())
    }

    /// Resolve motion artwork for `(artist, title)`. Never returns `Err`:
    /// a miss (no match / no motion) or a transient failure both surface as
    /// empty `AlbumDetails` so the host's fallback chain treats us as "no
    /// contribution" rather than a hard error. Confirmed misses are cached;
    /// transient failures are NOT (so a network blip doesn't stick).
    fn album_info(artist: String, title: String) -> Result<AlbumDetails, String> {
        let key = cache_key(&artist, &title);

        if let Some(cached) = read_cache(&key) {
            return Ok(cached.into_details());
        }

        match resolve_album(&artist, &title) {
            Ok(Some(motion)) => {
                write_cache(&key, &Cached::Motion(motion.clone()));
                Ok(motion.into_details())
            }
            Ok(None) => {
                write_cache(&key, &Cached::None);
                Ok(empty_album())
            }
            Err(e) => {
                log::emit(Level::Debug, &format!("apple-artwork: {e}"));
                Ok(empty_album())
            }
        }
    }
}

bindings::export!(AppleArtwork with_types_in bindings);

// ----- resolution pipeline -------------------------------------------------

#[derive(Clone)]
struct Motion {
    square: String,
    tall: Option<String>,
}

impl Motion {
    fn into_details(self) -> AlbumDetails {
        AlbumDetails {
            description: None,
            cover_url: None,
            track_count: None,
            motion_cover_url: Some(self.square),
            motion_cover_tall_url: self.tall,
        }
    }
}

/// How many catalogue candidates we're willing to probe for one album.
///
/// Each attempt costs a bearer-token fetch plus an amp-api call, and Apple
/// rate-limits aggressively, so we stop well before exhausting the search
/// results. Verified matches are probed first, so the cap almost never
/// bites on a well-tagged library.
const MAX_CANDIDATES: usize = 3;

/// `Ok(Some)` = motion found, `Ok(None)` = confirmed no motion (cache it),
/// `Err` = transient failure (don't cache).
fn resolve_album(artist: &str, title: &str) -> Result<Option<Motion>, String> {
    let candidates = itunes_lookup(artist, title)?;
    if candidates.is_empty() {
        // No Apple catalogue match — treat as a confirmed miss so we don't
        // re-search every track change for an album Apple doesn't carry.
        return Ok(None);
    }

    // Probe candidates in order (verified name matches first). Apple often
    // returns a single / EP / clean edition ahead of the album that actually
    // carries the editorial video, so stopping at the first hit — as this
    // used to — lost covers that were one result away.
    let mut editorial = None;
    for candidate in candidates.iter().take(MAX_CANDIDATES) {
        // A transient failure (rate limit, network) propagates immediately
        // rather than burning through the remaining candidates: retrying
        // them now would just deepen the rate limit, and `Err` tells the
        // host not to cache the miss.
        match fetch_editorial_video(
            &candidate.storefront,
            &candidate.album_id,
            &candidate.album_url,
        )? {
            Some(found) => {
                editorial = Some(found);
                break;
            }
            None => continue,
        }
    }
    let Some(editorial) = editorial else {
        return Ok(None);
    };

    // User option (`manifest.toml` → `[[options]]`, set in-app): when on, pick
    // the highest-resolution rendition of ANY codec (Apple's 4K covers are
    // H.265/HEVC-only). Default off → H.264 1080, which every WebView plays.
    let prefer_hevc = config::get_option("prefer_hevc")
        .map(|v| v == "true")
        .unwrap_or(false);

    let square = resolve_m3u8_to_mp4(&editorial.square_m3u8, prefer_hevc)?;
    let tall = editorial
        .tall_m3u8
        .and_then(|u| resolve_m3u8_to_mp4(&u, prefer_hevc).ok());

    Ok(Some(Motion { square, tall }))
}

// ----- step 1: iTunes search ----------------------------------------------

#[derive(Deserialize)]
struct ItunesResp {
    #[serde(default)]
    results: Vec<ItunesResult>,
}

#[derive(Deserialize)]
struct ItunesResult {
    #[serde(rename = "collectionViewUrl")]
    collection_view_url: Option<String>,
    #[serde(rename = "collectionName")]
    collection_name: Option<String>,
}

/// One catalogue album we can ask amp-api about.
struct Candidate {
    storefront: String,
    album_id: String,
    album_url: String,
}

/// Search iTunes for the album and return every usable candidate, the ones
/// whose title actually matches the request first.
///
/// `explicit=Yes` matters: without it Apple can hand back the *clean*
/// edition, which is a different catalogue id and frequently has no
/// editorial video even when the explicit edition does.
fn itunes_lookup(artist: &str, title: &str) -> Result<Vec<Candidate>, String> {
    let term = url_encode(&format!("{artist} {title}"));
    let url = format!(
        "https://itunes.apple.com/search?term={term}&entity=album&limit=5&explicit=Yes"
    );
    let (status, body) = get_text(&url, &[("Accept", "application/json")])?;
    if status == 429 || status == 403 {
        return Err(format!("itunes rate limited: {status}"));
    }
    if !(200..300).contains(&status) {
        return Err(format!("itunes status {status}"));
    }
    let parsed: ItunesResp =
        serde_json::from_str(&body).map_err(|e| format!("itunes json: {e}"))?;

    // Three tiers, probed in this order. Apple's own ranking is not
    // reliable here: searching for an album routinely returns the
    // same-named *single* first, and that single usually has no editorial
    // video even when the album does.
    //
    //   exact    "Short n' Sweet"            == requested
    //   partial  "Short n' Sweet (Deluxe)"   contains requested — real
    //            editions, but also "Better - Single" for "Better", which
    //            is exactly why it ranks below exact
    //   other    no title agreement at all — last resort, since a wrong
    //            album beats no cover only marginally
    let mut exact = Vec::new();
    let mut partial = Vec::new();
    let mut other = Vec::new();
    for r in parsed.results {
        let Some(url) = r.collection_view_url else {
            continue;
        };
        let Some((storefront, album_id)) = parse_album_url(&url) else {
            continue;
        };
        let candidate = Candidate {
            storefront,
            album_id,
            album_url: url,
        };
        match r.collection_name.as_deref().map(|n| rank_album_name(n, title)) {
            Some(NameMatch::Exact) => exact.push(candidate),
            Some(NameMatch::Partial) => partial.push(candidate),
            _ => other.push(candidate),
        }
    }
    exact.extend(partial);
    exact.extend(other);
    Ok(exact)
}

/// How well a catalogue title agrees with the one we asked for.
#[derive(PartialEq)]
enum NameMatch {
    Exact,
    Partial,
    None,
}

/// Compare a catalogue album title against the requested one.
///
/// Both sides are lowercased with punctuation flattened to spaces first, so
/// apostrophes, dashes and stray double spaces never decide the outcome.
fn rank_album_name(found: &str, requested: &str) -> NameMatch {
    let found = normalize_album_name(found);
    let requested = normalize_album_name(requested);
    if found.is_empty() || requested.is_empty() {
        return NameMatch::None;
    }
    if found == requested {
        return NameMatch::Exact;
    }
    if found.contains(&requested) {
        return NameMatch::Partial;
    }
    NameMatch::None
}

/// Lowercase and flatten punctuation, so only the words decide a match.
///
/// Apostrophes are **dropped** rather than turned into a separator: they sit
/// inside words, so replacing them with a space splits "Don't" into "don t"
/// and a library tagged `Dont Call Me Up` would then never match Apple's
/// `Don't Call Me Up`. Every other non-alphanumeric run collapses to a
/// single space, which keeps real word boundaries (dashes, parentheses).
fn normalize_album_name(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;
    for ch in input.chars() {
        if matches!(ch, '\'' | '\u{2019}' | '\u{02BC}' | '`') {
            // Intra-word punctuation: skip without breaking the word.
            continue;
        }
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

/// `https://music.apple.com/us/album/better-single/1834571502`
/// → `("us", "1834571502")`.
fn parse_album_url(url: &str) -> Option<(String, String)> {
    let after = url.split("music.apple.com/").nth(1)?;
    let segments: Vec<&str> = after.split('/').collect();
    let store = segments.first()?.trim();
    if store.len() != 2 || !store.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    // Album id is the last path segment, numeric (drop any query string).
    let last = segments.last()?.split('?').next()?.trim();
    if last.is_empty() || !last.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((store.to_string(), last.to_string()))
}

// ----- step 2 + 3: token + AMP editorialVideo -----------------------------

struct Editorial {
    square_m3u8: String,
    tall_m3u8: Option<String>,
}

/// Fetch `editorialVideo` for an album, minting/refreshing the anonymous
/// token as needed. Retries once on 401/403 with a freshly scraped token
/// (the cached one expires every few months).
fn fetch_editorial_video(
    storefront: &str,
    album_id: &str,
    album_url: &str,
) -> Result<Option<Editorial>, String> {
    let api = format!(
        "https://amp-api.music.apple.com/v1/catalog/{storefront}/albums/{album_id}\
         ?extend=editorialVideo&platform=web&l=en-US"
    );

    let mut token = get_token(album_url, false)?;
    for attempt in 0..2 {
        let (status, body) = get_text(
            &api,
            &[
                ("Authorization", &format!("Bearer {token}")),
                ("Origin", "https://music.apple.com"),
            ],
        )?;
        if (status == 401 || status == 403) && attempt == 0 {
            token = get_token(album_url, true)?;
            continue;
        }
        if status == 429 {
            return Err("amp-api rate limited".into());
        }
        if !(200..300).contains(&status) {
            return Err(format!("amp-api status {status}"));
        }
        return parse_editorial_video(&body);
    }
    Err("amp-api auth failed".into())
}

fn parse_editorial_video(body: &str) -> Result<Option<Editorial>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("amp json: {e}"))?;
    let ev = &v["data"][0]["attributes"]["editorialVideo"];
    if ev.is_null() {
        return Ok(None);
    }
    let square = ev["motionDetailSquare"]["video"].as_str();
    let tall = ev["motionDetailTall"]["video"]
        .as_str()
        .map(str::to_string);
    match square {
        Some(sq) => Ok(Some(Editorial {
            square_m3u8: sq.to_string(),
            tall_m3u8: tall,
        })),
        None => Ok(None),
    }
}

/// Get the anonymous web-player bearer token. Cached in the scratch store;
/// `force` re-scrapes (used after a 401/403).
fn get_token(album_url: &str, force: bool) -> Result<String, String> {
    if !force {
        if let Some(t) = read_state_str(TOKEN_KEY) {
            if !t.is_empty() {
                return Ok(t);
            }
        }
    }

    let (status, html) = get_text(album_url, &[])?;
    if !(200..300).contains(&status) {
        return Err(format!("album page status {status}"));
    }

    for path in find_js_bundles(&html) {
        let js_url = format!("https://music.apple.com{path}");
        let Ok((s, js)) = get_text(&js_url, &[]) else {
            continue;
        };
        if !(200..300).contains(&s) {
            continue;
        }
        if let Some(token) = find_jwt(&js) {
            write_state_str(TOKEN_KEY, &token);
            return Ok(token);
        }
    }
    Err("no anonymous token found in Apple Music bundles".into())
}

/// Scan the album-page HTML for `/assets/*.js` bundle paths likely to carry
/// the token (index / web-client / apple-music), de-duplicated in order.
fn find_js_bundles(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find("/assets/") {
        rest = &rest[pos..];
        // The path runs until the closing quote (or whitespace).
        let end = rest
            .find(|c: char| c == '"' || c == '\'' || c.is_whitespace())
            .unwrap_or(rest.len());
        let path = &rest[..end];
        if path.ends_with(".js")
            && (path.contains("index") || path.contains("web-client") || path.contains("apple-music"))
            && !out.contains(&path.to_string())
        {
            out.push(path.to_string());
        }
        rest = &rest[end.min(rest.len())..];
        // Advance at least one char to avoid re-matching the same position.
        if let Some(next) = rest.strip_prefix("/assets/") {
            let _ = next;
        } else if !rest.is_empty() {
            rest = &rest[1.min(rest.len())..];
        }
    }
    out
}

/// Find the first JWT-shaped token (`eyJ…` header, three base64url segments)
/// in a JS bundle.
fn find_jwt(js: &str) -> Option<String> {
    let bytes = js.as_bytes();
    let mut i = 0;
    while let Some(rel) = js[i..].find("eyJ") {
        let start = i + rel;
        let mut end = start;
        let mut dots = 0;
        while end < bytes.len() {
            let c = bytes[end];
            if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' {
                end += 1;
            } else if c == b'.' {
                dots += 1;
                end += 1;
            } else {
                break;
            }
        }
        let token = &js[start..end];
        // A JWT is header.payload.signature — three segments, plausibly long.
        if dots == 2 && token.len() > 80 {
            return Some(token.to_string());
        }
        i = end.max(start + 1);
    }
    None
}

// ----- step 4: m3u8 → mp4 --------------------------------------------------

/// Fetch an HLS master playlist and return a directly-playable progressive
/// `.mp4` URL for the app's native `<video>`.
///
/// Apple's motion master playlist lists ONLY segmented HLS variants
/// (`…_WxH.m3u8`) — there is no progressive `.mp4` entry to pick. But each
/// variant has a sibling progressive mp4 at the same URL with the trailing
/// `.m3u8` swapped for `-.mp4` (verified against live assets). WebView2 has
/// no HLS.js (can't play `.m3u8`) AND no HEVC license (can't play `hvc1` /
/// H.265), so we pick the highest-resolution **H.264 (`avc1`)** variant and
/// derive its mp4. Order of preference: highest-res avc1 mp4 → a literal
/// `.mp4` variant if a playlist ever lists one directly → highest-res mp4 of
/// any codec as a last resort (some Windows installs do carry an HEVC codec).
fn resolve_m3u8_to_mp4(m3u8_url: &str, prefer_hevc: bool) -> Result<String, String> {
    let (status, text) = get_text(m3u8_url, &[])?;
    if !(200..300).contains(&status) {
        return Err(format!("m3u8 status {status}"));
    }

    let lines: Vec<&str> = text.lines().collect();
    let mut best_avc1: Option<(u64, String)> = None; // derived mp4, H.264 only
    let mut best_literal: Option<(u64, String)> = None; // a `.mp4` in the playlist
    let mut best_any: Option<(u64, String)> = None; // derived mp4, any codec

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        // `#EXT-X-STREAM-INF` (playable variants) but NOT
        // `#EXT-X-I-FRAME-STREAM-INF` (trick-play, inline URI) — the latter
        // starts with `#EXT-X-I`, so the prefix check already excludes it.
        if line.starts_with("#EXT-X-STREAM-INF") {
            let pixels = parse_resolution(line).unwrap_or(0);
            let is_avc1 = codecs_contains(line, "avc1");
            // The URI is the next non-empty, non-comment line.
            let mut j = i + 1;
            while j < lines.len() {
                let l = lines[j].trim();
                if l.is_empty() || l.starts_with('#') {
                    j += 1;
                } else {
                    break;
                }
            }
            if j < lines.len() {
                let uri = resolve_url(m3u8_url, lines[j].trim());
                let path = uri.split('?').next().unwrap_or(&uri);
                if path.ends_with(".mp4") {
                    if better(&best_literal, pixels) {
                        best_literal = Some((pixels, uri));
                    }
                } else if let Some(mp4) = derive_progressive_mp4(&uri) {
                    if is_avc1 && better(&best_avc1, pixels) {
                        best_avc1 = Some((pixels, mp4.clone()));
                    }
                    if better(&best_any, pixels) {
                        best_any = Some((pixels, mp4));
                    }
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }

    // Default: prefer H.264 (avc1) for universal playback. When the user opts
    // into HEVC, take the highest-resolution rendition of any codec first
    // (Apple's 4K/2160 covers are H.265-only).
    let pick = if prefer_hevc {
        best_any.or(best_avc1).or(best_literal)
    } else {
        best_avc1.or(best_literal).or(best_any)
    };
    pick.map(|(_, uri)| uri)
        .ok_or_else(|| "no playable variant in m3u8".into())
}

/// Derive the progressive mp4 sibling of an HLS variant playlist URL:
/// `…_1080x1080.m3u8` → `…_1080x1080-.mp4`. Returns `None` for a URL that
/// isn't a `.m3u8` (nothing to swap).
fn derive_progressive_mp4(variant_url: &str) -> Option<String> {
    let path = variant_url.split('?').next().unwrap_or(variant_url);
    let stem = path.strip_suffix(".m3u8")?;
    Some(format!("{stem}-.mp4"))
}

/// True when an `#EXT-X-STREAM-INF` line's `CODECS="…"` attribute contains
/// `needle` (e.g. `"avc1"`). Missing/malformed attribute → false.
fn codecs_contains(stream_inf: &str, needle: &str) -> bool {
    stream_inf
        .split("CODECS=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .is_some_and(|codecs| codecs.contains(needle))
}

fn better(current: &Option<(u64, String)>, pixels: u64) -> bool {
    current.as_ref().is_none_or(|(p, _)| pixels > *p)
}

/// Parse `RESOLUTION=2160x2160` from an `#EXT-X-STREAM-INF` line into a
/// pixel count for picking the largest variant.
fn parse_resolution(line: &str) -> Option<u64> {
    let after = line.split("RESOLUTION=").nth(1)?;
    let dims = after.split(|c: char| c == ',' || c.is_whitespace()).next()?;
    let (w, h) = dims.split_once('x')?;
    let w: u64 = w.trim().parse().ok()?;
    let h: u64 = h.trim().parse().ok()?;
    Some(w.saturating_mul(h))
}

// ----- HTTP helpers --------------------------------------------------------

/// GET `url` (following manual redirects), returning `(status, body_text)`.
fn get_text(url: &str, extra_headers: &[(&str, &str)]) -> Result<(u16, String), String> {
    let mut current = url.to_string();
    for _ in 0..MAX_REDIRECTS {
        let mut headers: Vec<(String, String)> = vec![
            ("User-Agent".into(), USER_AGENT.into()),
            ("Accept-Language".into(), "en-US,en;q=0.9".into()),
        ];
        for (k, v) in extra_headers {
            headers.push(((*k).to_string(), (*v).to_string()));
        }
        let resp = http::send(&Request {
            method: "GET".into(),
            url: current.clone(),
            headers,
            body: None,
        })
        .map_err(|e| format!("http: {e}"))?;

        if (300..400).contains(&resp.status) {
            if let Some(loc) = header_get(&resp.headers, "location") {
                current = resolve_url(&current, &loc);
                continue;
            }
        }
        let text = String::from_utf8_lossy(&resp.body).into_owned();
        return Ok((resp.status, text));
    }
    Err("too many redirects".into())
}

fn header_get(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// Resolve `href` (absolute, root-relative, or path-relative) against `base`.
fn resolve_url(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let host_end = base[scheme_end..]
        .find('/')
        .map(|i| scheme_end + i)
        .unwrap_or(base.len());
    if href.starts_with('/') {
        return format!("{}{}", &base[..host_end], href);
    }
    // Path-relative: drop the last segment of the base path.
    let path_start = host_end;
    let last_slash = base[path_start..]
        .rfind('/')
        .map(|i| path_start + i + 1)
        .unwrap_or(base.len());
    format!("{}{}", &base[..last_slash], href)
}

/// Minimal percent-encoder for query values (alphanum + `-_.~` pass through).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

// ----- caching (scratch store) --------------------------------------------

enum Cached {
    Motion(Motion),
    None,
}

impl Cached {
    fn into_details(self) -> AlbumDetails {
        match self {
            Cached::Motion(m) => m.into_details(),
            Cached::None => empty_album(),
        }
    }
}

/// Normalised `motion:<artist>|<title>` cache key — lowercase, punctuation
/// collapsed to single spaces, so trivial tag differences hit the same row.
fn cache_key(artist: &str, title: &str) -> String {
    format!("motion:{}|{}", normalise(artist), normalise(title))
}

fn normalise(s: &str) -> String {
    let mapped: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn read_cache(key: &str) -> Option<Cached> {
    let raw = read_state_str(key)?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v.get("n").is_some() {
        return Some(Cached::None);
    }
    let square = v.get("s")?.as_str()?.to_string();
    let tall = v.get("t").and_then(|x| x.as_str()).map(str::to_string);
    Some(Cached::Motion(Motion { square, tall }))
}

fn write_cache(key: &str, cached: &Cached) {
    let raw = match cached {
        Cached::None => "{\"n\":1}".to_string(),
        Cached::Motion(m) => {
            let mut obj = serde_json::Map::new();
            obj.insert("s".into(), serde_json::Value::String(m.square.clone()));
            if let Some(t) = &m.tall {
                obj.insert("t".into(), serde_json::Value::String(t.clone()));
            }
            serde_json::Value::Object(obj).to_string()
        }
    };
    write_state_str(key, &raw);
}

fn read_state_str(key: &str) -> Option<String> {
    match storage::read_state(key) {
        Ok(Some(bytes)) => String::from_utf8(bytes).ok(),
        _ => None,
    }
}

fn write_state_str(key: &str, value: &str) {
    let _ = storage::write_state(key, value.as_bytes());
}

fn empty_album() -> AlbumDetails {
    AlbumDetails {
        description: None,
        cover_url: None,
        track_count: None,
        motion_cover_url: None,
        motion_cover_tall_url: None,
    }
}
