# WaveFlow — Apple Motion Artwork plugin

A [WaveFlow](https://github.com/InstaZDLL/WaveFlow) plugin that fetches **Apple Music's animated album covers** (motion artwork) and hands them to the app to render behind the now-playing view.

It's a sandboxed WebAssembly component implementing the `waveflow:metadata/v1` world — one portable `plugin.wasm`, no network access except the Apple hosts its manifest allowlists.

## What it does

`album-info(artist, title)` resolves an animated cover by mirroring the public Apple Music web player (clean-room — no vendored code):

1. **iTunes Search** (`itunes.apple.com/search`) → the album's Apple Music URL (storefront + numeric id).
2. **Anonymous token** — scrapes the bearer JWT the web player embeds in its JS bundles. Cached in the plugin's scratch store; re-scraped on a 401/403.
3. **AMP catalogue API** (`amp-api.music.apple.com/.../albums/{id}?extend=editorialVideo`) → `editorialVideo.motionDetailSquare/Tall.video` (HLS `.m3u8`).
4. **m3u8 → mp4** — picks the highest-resolution progressive `.mp4` variant so WaveFlow's native `<video>` can play it (the desktop webview has no HLS.js).

All HTTP goes through the host's permissioned `waveflow:host/http`. Results — a positive hit, a negative sentinel, and the token — are cached in the per-plugin scratch store, so **a given album hits Apple at most once**. That caching is the rate-limit discipline; the host also serialises calls to the plugin.

## Build

Needs [`cargo-component`](https://github.com/bytecodealliance/cargo-component) + the `wasm32-wasip1` target:

```bash
cargo component build --release
# -> target/wasm32-wasip1/release/waveflow_plugin_apple_artwork.wasm  (~170 KB)
```

## Install

Users don't build it — they install it from WaveFlow's in-app plugin store (Settings → Plugins → Store) once it's listed in [InstaZDLL/waveflow-plugins](https://github.com/InstaZDLL/waveflow-plugins). Requires **WaveFlow 1.6.2+** (the `waveflow:metadata@1.1.0` motion-artwork fields).

## Permissions

Declared in [`manifest.toml`](manifest.toml) and enforced by the host sandbox — the plugin can reach only:

- `itunes.apple.com` · `music.apple.com` · `amp-api.music.apple.com` · `mvod.itunes.apple.com`

It has no filesystem access and persists only to its own 10 MB scratch store (the resolution cache + token).

## Caveats

- **Best-effort, unofficial.** It reads Apple's anonymous web-player token and a private catalogue endpoint. Apple can change either at any time; the token scrape or the m3u8 shape may need a patch when they do — that's the nature of this kind of plugin, and exactly why it lives out-of-core as an opt-in plugin.
- **Coverage is partial** — Apple only produces motion artwork for a subset of albums. No match / no motion is cached as a miss.

## License

[MIT](LICENSE).
