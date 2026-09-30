//! WaveFlow Apple Motion Artwork plugin — guest side.
//!
//! Implements `waveflow:metadata/v1`'s `album-info` to resolve Apple Music's
//! animated album covers (motion artwork). The algorithm mirrors the public
//! Apple Music web player (clean-room, no vendored code):
//!
//! 1. **iTunes Search** (`itunes.apple.com/search`) → the album's Apple
//!    Music URL, from which we read the storefront + numeric id; when the
//!    search misses the album, the artist's discography
//!    (`itunes.apple.com/lookup`) is searched instead.
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
//! positive hit, a negative sentinel, and the token — so a given album is
//! resolved at most once: a handful of requests the first time, none after.
//! A transient failure is not cached and is retried on the next play. That
//! caching IS the rate-limit discipline: the host
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

/// A plain desktop browser's: these are the web player's own endpoints,
/// and a request that named this plugin would single it out.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

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
///
/// A title that joins two releases (`Jar of Flies / Sap`, the CD that
/// pairs both EPs) is looked up whole first. Only when that finds no
/// motion cover is each part tried in turn, and then only the release
/// itself or an edition of it counts: a title that merely starts the
/// same is too loose a match for half of someone's tag.
fn resolve_album(artist: &str, title: &str) -> Result<Option<Motion>, String> {
    if let Some(motion) = resolve_title(artist, title, NameMatch::Partial)? {
        return Ok(Some(motion));
    }
    for part in combined_parts(title) {
        if let Some(motion) = resolve_title(artist, part, NameMatch::Edition)? {
            return Ok(Some(motion));
        }
    }
    Ok(None)
}

/// The releases a title joins with ` / `, or nothing for a single title.
/// The spaces are required: a bare slash sits inside ordinary titles
/// (`Love/Hate`, `AC/DC`).
fn combined_parts(title: &str) -> Vec<&str> {
    let parts: Vec<&str> = title
        .split(" / ")
        .map(str::trim)
        .filter(|part| !normalize_album_name(part).is_empty())
        .collect();
    if parts.len() < 2 {
        return Vec::new();
    }
    parts
}

/// One lookup of [`resolve_album`], keeping catalogue titles that match at
/// `loosest` or better.
fn resolve_title(artist: &str, title: &str, loosest: NameMatch) -> Result<Option<Motion>, String> {
    let mut candidates = itunes_lookup(artist, title)?;
    candidates.retain(|candidate| candidate.rank <= loosest);
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
    #[serde(rename = "wrapperType")]
    wrapper_type: Option<String>,
    #[serde(rename = "collectionViewUrl")]
    collection_view_url: Option<String>,
    #[serde(rename = "collectionName")]
    collection_name: Option<String>,
    #[serde(rename = "artistName")]
    artist_name: Option<String>,
    #[serde(rename = "artistId")]
    artist_id: Option<u64>,
}

/// One catalogue album we can ask amp-api about.
struct Candidate {
    storefront: String,
    album_id: String,
    album_url: String,
    /// How its title agreed with the one asked for.
    rank: NameMatch,
}

/// Find the catalogue editions of the requested album, best match first.
///
/// Only albums **by the requested artist whose title agrees** are returned.
/// Anything else used to be probed as a last resort, and that is how a
/// library album with no motion cover of its own showed another one:
/// *Facelift* took the cover of *Jar of Flies*, the next result for the
/// same artist. An album Apple has no motion cover for now stays static.
///
/// The keyword search comes first, since it costs one request. It is not
/// enough on its own: for a term like "Pearl Jam Ten" or "Nirvana
/// Nevermind" it ranks live albums, singles and other artists' songs above
/// the album itself, often pushing it out of the results altogether. When
/// it finds neither the album nor an edition of it, the artist's whole
/// discography is listed (`lookup?id=…&entity=album`) and searched instead;
/// what the search did find is only used if that finds nothing.
fn itunes_lookup(artist: &str, title: &str) -> Result<Vec<Candidate>, String> {
    let term = url_encode(&format!("{artist} {title}"));
    // `explicit=Yes` matters: without it Apple can hand back the *clean*
    // edition, which is a different catalogue id and frequently has no
    // editorial video even when the explicit edition does.
    let search = itunes_get(&format!(
        "https://itunes.apple.com/search?term={term}&entity=album&limit=10&explicit=Yes"
    ))?;
    let from_search = rank_candidates(&search, artist, title);
    // Only the album itself, or an edition of it, settles the search. A
    // title that merely starts the same (`Ten Redux`) may be all the search
    // shows while the discography holds `Ten`: it is kept for last.
    if from_search
        .first()
        .is_some_and(|c| c.rank < NameMatch::Partial)
    {
        return Ok(from_search);
    }

    // The search usually names the artist even when it misses the album,
    // which saves the artist lookup.
    let artist_id = match search
        .iter()
        .filter(|r| {
            r.artist_name
                .as_deref()
                .is_some_and(|a| is_artist(a, artist))
        })
        .find_map(|r| r.artist_id)
    {
        Some(id) => id,
        None => match find_artist_id(artist)? {
            Some(id) => id,
            None => return Ok(from_search),
        },
    };
    let discography = itunes_get(&format!(
        "https://itunes.apple.com/lookup?id={artist_id}&entity=album&limit=200"
    ))?;
    let from_discography = rank_candidates(&discography, artist, title);
    Ok(if from_discography.is_empty() {
        from_search
    } else {
        from_discography
    })
}

/// The iTunes id of the artist named `artist`, if the catalogue has one.
fn find_artist_id(artist: &str) -> Result<Option<u64>, String> {
    let term = url_encode(artist);
    let found = itunes_get(&format!(
        "https://itunes.apple.com/search?term={term}&entity=musicArtist&limit=5"
    ))?;
    Ok(found
        .iter()
        .filter(|r| {
            r.artist_name
                .as_deref()
                .is_some_and(|a| is_artist(a, artist))
        })
        .find_map(|r| r.artist_id))
}

fn itunes_get(url: &str) -> Result<Vec<ItunesResult>, String> {
    let (status, body) = get_text(url, &[("Accept", "application/json")])?;
    if status == 429 || status == 403 {
        return Err(format!("itunes rate limited: {status}"));
    }
    if !(200..300).contains(&status) {
        return Err(format!("itunes status {status}"));
    }
    let parsed: ItunesResp =
        serde_json::from_str(&body).map_err(|e| format!("itunes json: {e}"))?;
    Ok(parsed.results)
}

/// The albums in `results` by `artist` whose title agrees with `title`,
/// exact matches first, then the same album under an edition suffix, then
/// titles that merely contain the one asked for.
fn rank_candidates(results: &[ItunesResult], artist: &str, title: &str) -> Vec<Candidate> {
    let mut ranked: Vec<(NameMatch, Candidate)> = Vec::new();
    for r in results {
        // A `lookup` answer starts with the artist itself.
        if r.wrapper_type.as_deref().is_some_and(|w| w != "collection") {
            continue;
        }
        if !r
            .artist_name
            .as_deref()
            .is_some_and(|a| same_artist(a, artist))
        {
            continue;
        }
        let rank = match r
            .collection_name
            .as_deref()
            .map(|n| rank_album_name(n, title))
        {
            Some(NameMatch::None) | None => continue,
            Some(rank) => rank,
        };
        let Some(url) = r.collection_view_url.as_deref() else {
            continue;
        };
        let Some((storefront, album_id)) = parse_album_url(url) else {
            continue;
        };
        ranked.push((
            rank,
            Candidate {
                storefront,
                album_id,
                album_url: url.to_string(),
                rank,
            },
        ));
    }
    // The album itself and its editions go together: they share a cover.
    // A title that merely starts the same (`Ten Redux` for `Ten`) is
    // another record, only tried when neither exists — otherwise an album
    // with no motion cover would take that record's.
    let best = if ranked.iter().any(|(rank, _)| *rank < NameMatch::Partial) {
        NameMatch::Edition
    } else {
        NameMatch::Partial
    };
    ranked.retain(|(rank, _)| *rank <= best);
    // Stable, so Apple's own order decides within a tier.
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, c)| c).collect()
}

/// How well a catalogue title agrees with the one we asked for, best first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum NameMatch {
    /// `Nevermind` for `Nevermind`.
    Exact,
    /// The same album under another edition: `Ten` for `Ten (Remastered)`,
    /// `Jar of Flies - EP` for `Jar of Flies`.
    Edition,
    /// A title that starts with the one asked for, or the single of a title
    /// asked for as an album (`Better - Single` for `Better`): sometimes the
    /// right release, but a single rarely carries the album's motion cover,
    /// which is why it ranks last.
    Partial,
    None,
}

/// Compare a catalogue album title against the requested one.
///
/// Both sides are lowercased with punctuation flattened to spaces first, so
/// apostrophes, dashes and stray double spaces never decide the outcome.
fn rank_album_name(found: &str, requested: &str) -> NameMatch {
    let found_words = normalize_album_name(found);
    let requested_words = normalize_album_name(requested);
    if found_words.is_empty() || requested_words.is_empty() {
        return NameMatch::None;
    }
    if found_words == requested_words {
        return NameMatch::Exact;
    }
    let found_base = normalize_album_name(&strip_edition(found));
    let requested_base = normalize_album_name(&strip_edition(requested));
    if !found_base.is_empty() && found_base == requested_base {
        let single = |words: &str| words.ends_with(" single");
        return if single(&found_words) && !single(&requested_words) {
            NameMatch::Partial
        } else {
            NameMatch::Edition
        };
    }
    // Leading words only: containment let `Live on Ten Legs` answer for
    // `Ten`, the very kind of other album this ranking must refuse.
    if found_words.starts_with(&format!("{requested_words} ")) {
        return NameMatch::Partial;
    }
    NameMatch::None
}

/// A title without what marks an edition rather than an album: bracketed
/// or parenthesised groups (`(Remastered)`, `[2011 Remaster]`,
/// `(30th Anniversary Super Deluxe)`) and a trailing ` - EP` / ` - Single`.
fn strip_edition(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut depth = 0usize;
    for ch in title.chars() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    let trimmed = out.trim();
    for suffix in [" - ep", " - single"] {
        // Any case. The suffix is ASCII, so the cut is checked to land on
        // a character boundary before a non-ASCII title is sliced.
        let cut = trimmed.len().saturating_sub(suffix.len());
        if trimmed.len() > suffix.len()
            && trimmed.is_char_boundary(cut)
            && trimmed[cut..].eq_ignore_ascii_case(suffix)
        {
            return trimmed[..cut].trim().to_string();
        }
    }
    trimmed.to_string()
}

/// Whether a catalogue artist is the one asked for: some name is on both
/// sides. Either credit can list several artists (`Pearl Jam; Eddie
/// Vedder`), split on `;` only — `&` and `,` also sit inside single names
/// (`Simon & Garfunkel`, `Tyler, The Creator`). A leading "The", case and
/// punctuation never count.
fn same_artist(found: &str, requested: &str) -> bool {
    let names = |credit: &str| -> Vec<String> {
        credit
            .split(';')
            .map(|name| without_the(&normalize_album_name(name)))
            .filter(|name| !name.is_empty())
            .collect()
    };
    let found = names(found);
    names(requested).iter().any(|name| found.contains(name))
}

/// Stricter than [`same_artist`], for picking the artist whose discography
/// is listed: the catalogue credit must be that artist alone, so a row
/// that merely co-credits them cannot hand over another artist's id.
fn is_artist(found: &str, requested: &str) -> bool {
    let found = without_the(&normalize_album_name(found));
    !found.is_empty()
        && requested
            .split(';')
            .any(|name| without_the(&normalize_album_name(name)) == found)
}

fn without_the(name: &str) -> String {
    name.strip_prefix("the ").unwrap_or(name).to_string()
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
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("amp json: {e}"))?;
    let ev = &v["data"][0]["attributes"]["editorialVideo"];
    if ev.is_null() {
        return Ok(None);
    }
    let square = ev["motionDetailSquare"]["video"].as_str();
    let tall = ev["motionDetailTall"]["video"].as_str().map(str::to_string);
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
        let Ok((s, js)) = get_bytes(&js_url, &[]) else {
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
            && (path.contains("index")
                || path.contains("web-client")
                || path.contains("apple-music"))
            && !out.contains(&path.to_string())
        {
            out.push(path.to_string());
        }
        rest = &rest[end.min(rest.len())..];
        // Advance at least one char to avoid re-matching the same position.
        // A whole char: the terminator can be a non-ASCII space (U+00A0 and
        // friends are `char::is_whitespace`), and slicing at byte 1 of one
        // would panic — which in wasm is a trap, killing the call.
        if !rest.starts_with("/assets/") {
            match rest.chars().next() {
                Some(c) => rest = &rest[c.len_utf8()..],
                None => break,
            }
        }
    }
    out
}

/// Find the first JWT-shaped token (`eyJ…` header, three base64url segments)
/// in a JS bundle.
///
/// Byte-level, and the `eyJ` search goes through `memchr`, which reads a
/// machine word at a time: `str::find` scans byte by byte, and over the
/// several megabytes Apple serves that alone spends more wasm instructions
/// than a plugin call is allowed (WaveFlow#733). A JWT is ASCII, so the
/// match is lifted back to a `String` at the end and only then.
fn find_jwt(bytes: &[u8]) -> Option<String> {
    let mut i = 0;
    while let Some(rel) = memchr::memmem::find(&bytes[i..], b"eyJ") {
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
        let token = &bytes[start..end];
        // A JWT is header.payload.signature — three segments, plausibly long.
        if dots == 2 && token.len() > 80 {
            // Every byte accepted above is ASCII, so this cannot fail.
            return String::from_utf8(token.to_vec()).ok();
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
    let dims = after
        .split(|c: char| c == ',' || c.is_whitespace())
        .next()?;
    let (w, h) = dims.split_once('x')?;
    let w: u64 = w.trim().parse().ok()?;
    let h: u64 = h.trim().parse().ok()?;
    Some(w.saturating_mul(h))
}

// ----- HTTP helpers --------------------------------------------------------

/// GET `url` (following manual redirects), returning `(status, body_text)`.
fn get_text(url: &str, extra_headers: &[(&str, &str)]) -> Result<(u16, String), String> {
    let (status, body) = get_bytes(url, extra_headers)?;
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

/// The same, as the bytes the host returned.
///
/// The JavaScript bundles are megabytes, and turning one into a `String`
/// walks it to validate UTF-8 and then copies the whole thing — before the
/// search has even started. The token is ASCII, so the scan reads the bytes
/// (WaveFlow#733).
fn get_bytes(url: &str, extra_headers: &[(&str, &str)]) -> Result<(u16, Vec<u8>), String> {
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
        return Ok((resp.status, resp.body));
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

/// Normalised `motion2:<artist>|<title>` cache key — lowercase, punctuation
/// collapsed to single spaces, so trivial tag differences hit the same row.
///
/// The `2` retires every answer cached before 0.3.4: those came from a
/// matcher that could settle on another album by the same artist, or miss
/// an album the search ranked out, and a cached answer is never re-checked.
fn cache_key(artist: &str, title: &str) -> String {
    format!("motion2:{}|{}", normalise(artist), normalise(title))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The token is found in bytes, without the file ever becoming a
    /// `String` — that conversion and its copy are most of what made the
    /// old scan cost 88 % of a call's fuel on Apple's 3.3 MB bundle.
    #[test]
    fn a_jwt_is_found_in_raw_bytes() {
        let token = format!(
            "eyJ{}.{}.{}",
            "a".repeat(40),
            "b".repeat(40),
            "c".repeat(40)
        );
        let mut js = b"var x=1;// ".to_vec();
        js.extend_from_slice(token.as_bytes());
        js.extend_from_slice(b";more");
        assert_eq!(find_jwt(&js).as_deref(), Some(token.as_str()));
    }

    /// Too short, and only one dot: neither is a JWT.
    #[test]
    fn a_lookalike_is_not_a_token() {
        assert_eq!(find_jwt(b"eyJshort.abc"), None);
        let long = format!("eyJ{}", "a".repeat(200));
        assert_eq!(find_jwt(long.as_bytes()), None);
    }

    fn album(artist: &str, title: &str, id: &str) -> ItunesResult {
        ItunesResult {
            wrapper_type: Some("collection".into()),
            collection_view_url: Some(format!("https://music.apple.com/us/album/x/{id}")),
            collection_name: Some(title.into()),
            artist_name: Some(artist.into()),
            artist_id: Some(1),
        }
    }

    fn ids(found: Vec<Candidate>) -> Vec<String> {
        found.into_iter().map(|c| c.album_id).collect()
    }

    /// `Facelift` has no motion cover; the next result for the same artist,
    /// `Jar of Flies`, has one and used to be shown instead.
    #[test]
    fn another_album_by_the_artist_is_never_a_candidate() {
        let results = [
            album("Alice In Chains", "Jar of Flies - EP", "1"),
            album("Alice In Chains", "Facelift", "2"),
        ];
        assert_eq!(
            ids(rank_candidates(&results, "Alice In Chains", "Facelift")),
            ["2"]
        );
        assert!(rank_candidates(&results[..1], "Alice In Chains", "Facelift").is_empty());
    }

    /// "Nirvana Nevermind" returns other artists' songs called Nevermind.
    #[test]
    fn another_artist_is_never_a_candidate() {
        let results = [
            album("Dennis Lloyd", "Nevermind - Single", "1"),
            album("Nirvana", "Nevermind (Deluxe)", "2"),
            album("Nirvana", "Nevermind", "3"),
        ];
        assert_eq!(
            ids(rank_candidates(&results, "Nirvana", "Nevermind")),
            ["3", "2"]
        );
    }

    /// The CD pairing both EPs is tagged as one title; each part is
    /// looked up on its own once the whole title finds nothing.
    #[test]
    fn a_combined_title_is_split_on_a_spaced_slash_only() {
        assert_eq!(
            combined_parts("Jar of Flies / Sap"),
            ["Jar of Flies", "Sap"]
        );
        assert_eq!(combined_parts("A / B / C"), ["A", "B", "C"]);
        assert!(combined_parts("Jar of Flies").is_empty());
        assert!(combined_parts("Love/Hate").is_empty());
        // A slash with nothing usable on one side is not two releases.
        assert!(combined_parts("Sap / ").is_empty());
        assert!(combined_parts(" / !!").is_empty());
    }

    /// A part of a combined title only takes the release itself or an
    /// edition of it: `Sap` must not answer with `Sap Sessions`.
    #[test]
    fn a_part_is_not_matched_by_a_title_that_only_starts_the_same() {
        assert_eq!(rank_album_name("Sap - EP", "Sap"), NameMatch::Edition);
        assert_eq!(rank_album_name("Sap Sessions", "Sap"), NameMatch::Partial);
        assert!(NameMatch::Partial > NameMatch::Edition);
    }

    /// `Ten Redux` is another record: never tried while `Ten` exists.
    #[test]
    fn a_partial_title_only_stands_in_for_a_missing_album() {
        let both = [
            album("Pearl Jam", "Ten Redux", "1"),
            album("Pearl Jam", "Ten", "2"),
        ];
        assert_eq!(ids(rank_candidates(&both, "Pearl Jam", "Ten")), ["2"]);
        assert_eq!(ids(rank_candidates(&both[..1], "Pearl Jam", "Ten")), ["1"]);
    }

    #[test]
    fn only_the_artist_alone_can_supply_the_discography() {
        assert!(is_artist("Pearl Jam", "Pearl Jam; Eddie Vedder"));
        assert!(!is_artist("Eddie Vedder; Pearl Jam", "Pearl Jam"));
        assert!(!is_artist("", ""));
    }

    #[test]
    fn editions_rank_after_the_exact_title_and_singles_last() {
        assert_eq!(
            rank_album_name("Dirt (Remastered)", "Dirt (Remastered)"),
            NameMatch::Exact
        );
        assert_eq!(
            rank_album_name("Ten", "Ten (Remastered)"),
            NameMatch::Edition
        );
        assert_eq!(
            rank_album_name("Jar of Flies - EP", "Jar of Flies"),
            NameMatch::Edition
        );
        assert_eq!(strip_edition("Jar of Flies - ep"), "Jar of Flies");
        assert_eq!(strip_edition("Better - SINGLE"), "Better");
        assert_eq!(strip_edition("Été - Single"), "Été");
        assert_eq!(
            rank_album_name("Better - Single", "Better"),
            NameMatch::Partial
        );
        assert_eq!(
            rank_album_name("The Colour And The Shape", "The Colour and the Shape"),
            NameMatch::Exact
        );
        assert_eq!(rank_album_name("Ten Redux", "Ten"), NameMatch::Partial);
        assert_eq!(rank_album_name("Live on Ten Legs", "Ten"), NameMatch::None);
        assert_eq!(
            rank_album_name("Jar of Flies - EP", "Facelift"),
            NameMatch::None
        );
    }

    #[test]
    fn artists_compare_without_case_the_or_co_credits() {
        assert!(same_artist("Alice In Chains", "Alice in Chains"));
        assert!(same_artist("Pearl Jam", "Pearl Jam; Eddie Vedder"));
        assert!(same_artist("The Beatles", "Beatles"));
        assert!(same_artist("Eddie Vedder; Pearl Jam", "Pearl Jam"));
        assert!(!same_artist("Dennis Lloyd", "Nirvana"));
        assert!(!same_artist("", ""));
    }

    /// A `lookup` answer starts with the artist, which is not an album.
    #[test]
    fn the_artist_row_of_a_lookup_is_skipped() {
        let mut artist_row = album("Pearl Jam", "Pearl Jam", "9");
        artist_row.wrapper_type = Some("artist".into());
        let results = [artist_row, album("Pearl Jam", "Pearl Jam", "4")];
        assert_eq!(
            ids(rank_candidates(&results, "Pearl Jam", "Pearl Jam")),
            ["4"]
        );
    }

    /// A non-ASCII space after a path used to be sliced at byte 1, in the
    /// middle of its two bytes — a panic, and in wasm that is a trap that
    /// kills the whole call.
    #[test]
    fn a_non_ascii_space_after_a_path_does_not_panic() {
        let html = "<script src=\"/assets/index~ab.js\"></script>\u{a0}/assets/x\u{2028}end";
        let found = find_js_bundles(html);
        assert_eq!(found, vec!["/assets/index~ab.js".to_string()]);
    }
}
