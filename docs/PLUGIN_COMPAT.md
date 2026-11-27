# `nsh-music` Plugin Compatibility & Mobile Migration Guide / 插件兼容性与移动端迁移指南

`nsh-music` provides a high-compatibility JavaScript / TypeScript plugin runtime powered by **QuickJS (`quickjs-jit`) + `oxc` TypeScript stripping**, supporting existing **`mygo-music`** plugins across **macOS, Windows, Linux, iOS, Android, and HarmonyOS (Tier-1)**.

`nsh-music` 提供基于 **QuickJS + `oxc` TypeScript 转译** 的跨平台插件运行时，开箱兼容 **`mygo-music`** 生态插件，并针对 **iOS / Android / 鸿蒙 HarmonyOS** 沙箱环境内置了虚拟 FFmpeg 代理与登录迁移方案。

---

## 1. Compatibility Tiers / 兼容性分级

Static analysis (`compat.rs`) inspects `plugin.json` and the plugin's JS/TS source at install/load time and classifies every source into one of three tiers:

| Tier (`CompatTier`) | Core Level (`PluginCompatLevel`) | Meaning / 含义 | Platforms / 平台支持 |
|---|---|---|---|
| **`Full`** | `FullNative` | Pure JS/TS + `host.fetch` / `host.crypto` / `host.storage` / QR auth | Desktop + iOS + Android + HarmonyOS |
| **`Full` (Virtual Shims)** | `VirtualFfmpeg` | Calls `host.spawn("ffmpeg" \| "yt-dlp", ...)` — transparently intercepted in-process by `VirtualFfmpegShim` / `VirtualYtdlpShim` | Desktop + iOS + Android + HarmonyOS (zero external binary needed!) |
| **`Partial`** | `FullNative` / `VirtualFfmpeg` | Uses `host.webSignIn(...)` where mobile WebViews may restrict `HttpOnly` cookies — works via embedded WebView + native cookie store / manual fallback | Desktop (full) / Mobile (WebView + native cookie store / manual fallback) |
| **`MigrationRequired`** | `DesktopProcessLimited` | Invokes arbitrary unvirtualized external CLI binaries (e.g. `python3`, custom scripts) via `host.spawn` | Desktop only; returns structured `MobileSandboxError` on mobile/OHOS |

---

## 2. `mygo-music` 38-Export Compatibility Matrix / 38 个导出函数兼容矩阵

Both **named ESM exports** (`export async function search(...)`) and **default object exports** (`export default { async search(...) {} }`) are supported. Both camelCase long names and `mygo-music` concise aliases are recognized automatically.

| # | Canonical Export | Supported Alias(es) | `MusicSource` Method | Mobile Support |
|---|---|---|---|---|
| 1 | `getTrack` | `track` | `PluginSource::get_track` | Full |
| 2 | `getStreamUrl` | `stream` | `MusicSource::stream` | Full (+ `VirtualFfmpegShim`) |
| 3 | `getLyrics` | `lyrics` | `MusicSource::lyrics` | Full |
| 4 | `search` | — | `MusicSource::search` | Full |
| 5 | `searchSuggestions` | `suggest` | `MusicSource::suggest` | Full |
| 6 | `getHome` | `home` | `MusicSource::home` | Full |
| 7 | `getBrowse` | `browse` | `MusicSource::browse` | Full |
| 8 | `getPlaylist` | `playlist` | `MusicSource::playlist` | Full |
| 9 | `getAlbum` | `album` | `MusicSource::album` | Full |
| 10 | `getArtist` | `artist` | `MusicSource::artist` | Full |
| 11 | `getLikedTracks` | `liked` | `MusicSource::liked` | Full |
| 12 | `isLiked` | — | `MusicSource::is_liked` | Full |
| 13 | `toggleLike` | `like` | `MusicSource::like` | Full |
| 14 | `likeEpisode` | — | `MusicSource::like_episode` | Full |
| 15 | `getRecent` | `recent` | `MusicSource::recent` | Full |
| 16 | `onPlayed` | `played` | `MusicSource::played` | Full |
| 17 | `getUserPlaylists` | `playlists` | `MusicSource::playlists` | Full |
| 18 | `createPlaylist` | — | `MusicSource::create_playlist` | Full |
| 19 | `renamePlaylist` | — | `MusicSource::rename_playlist` | Full |
| 20 | `deletePlaylist` | — | `MusicSource::delete_playlist` | Full |
| 21 | `addToPlaylist` | — | `MusicSource::add_to_playlist` | Full |
| 22 | `removeFromPlaylist` | — | `MusicSource::remove_from_playlist` | Full |
| 23 | `movePlaylistTrack` | — | `MusicSource::move_playlist_track` | Full |
| 24 | `savePlaylist` | — | `MusicSource::save_playlist` | Full |
| 25 | `followArtist` | — | `MusicSource::follow_artist` | Full |
| 26 | `saveAlbum` | — | `MusicSource::save_album` | Full |
| 27 | `getSavedAlbums` | `savedAlbums` | `MusicSource::saved_albums` | Full |
| 28 | `getRadioStations` | `stations` | `MusicSource::stations` | Full |
| 29 | `getRadioTracks` | `stationNext` | `MusicSource::station_next` | Full |
| 30 | `getPodcastShows` | `shows` | `MusicSource::shows` | Full |
| 31 | `getPodcastEpisodes` | `show` | `MusicSource::show` | Full |
| 32 | `subscribeShow` | — | `MusicSource::subscribe_show` | Full |
| 33 | `getComments` | `comments` | `MusicSource::comments` | Full |
| 34 | `likeComment` | — | `MusicSource::like_comment` | Full |
| 35 | `postComment` | — | `MusicSource::post_comment` | Full |
| 36 | `getAuthStatus` | `account` | `MusicSource::account` | Full |
| 37 | `signIn` | — | `MusicSource::sign_in` | Full / Partial (`webSignIn`) |
| 38 | `signOut` | — | `MusicSource::sign_out` | Full |

Additional supported lifecycle & remote-control exports:
- **QR Sign-In**: `getQrSignIn`, `pollQrSignIn`
- **Dynamic Settings**: `getConfigSchema`, `onConfigChange`
- **Remote Playback (`playback: "remote"`)**: `remotePlay`, `remotePause`, `remoteResume`, `remoteSeek`, `remoteVolume`, `remoteState`, `remoteDevices`, `remoteUse`

---

## 3. `host.*` Global API Reference / `host.*` 全局接口

Every plugin executes in its own isolated QuickJS worker thread with `globalThis.host` injected:

| API | Methods / Fields | Notes |
|---|---|---|
| `host.fetch(url, opts?)` | Returns `{ ok, status, statusText, url, redirected, bodyUsed, headers, body, text(), json(), bytes(), arrayBuffer(), clone() }` | Backed by Rust `reqwest` (`rustls-tls`, zero OpenSSL); auto-applies `requestHeaders` (`Api` kind); supports `redirect: "follow" \| "manual" \| "error"`, `AbortSignal`, `URLSearchParams` bodies, and Web Fetch `ReadableStream` `response.body` (`locked`, `getReader()` -> `read()`/`cancel()`/`releaseLock()`/`closed`, `body.cancel()`, `[Symbol.asyncIterator]()`) |
| `host.storage` | `get(k)`, `set(k, v)`, `remove(k)`, `keys()`, `clear()` | Persisted atomically per plugin (`<data_dir>/<plugin_id>/storage.json`) |
| `host.config` (`host.settings`) | `get(k, def?)`, `set(k, v)`, `all()` | Persisted per plugin; drives `{{config.xxx}}` header templates |
| `host.crypto` | `md5`, `sha256`, `hmacSha256`, `aesEcbEncrypt`, `aesEcbDecrypt`, `aesCbcEncrypt`, `aesCbcDecrypt`, `base64Encode`, `base64Decode`, `base64UrlEncode`, `base64UrlDecode`, `hexEncode`, `hexDecode` | 100% pure Rust (AES-128/192/256 PKCS#7, FIPS-180-4 SHA-256, RFC 2104 HMAC) |
| `host.log` | `debug`, `info`, `warn`, `error` | Routed to Rust `log` target `plugin::<id>` (also aliased to `console.*`) |
| `host.i18n` | `t(key, params?)`, `locale()` | Loads `locales/<lang>.json` (`zh-CN`, `en`, automatic fallback) |
| `host.capabilities` | `{ platform, isMobile, embeddedFfmpeg: true, embeddedYtdlp: true, virtualProcesses: ["ffmpeg", "yt-dlp"], nativeCodecs, openUrl: true, canSpawnExternal, webSignInMode }` | Runtime feature detection for adaptive plugins |
| `host.spawn(cmd, args?, opts?)` | Handle: `stdout()`, `stderr()`, `errorOutput()`, `readLine()`, `write()`, `closeStdin()`, `wait()`, `kill()`, `isRunning()` | When `Mobile Virtualization Compatibility` is enabled (default), Desktop prefers user-installed `ffmpeg` / `yt-dlp` on `PATH` (when permitted) and seamlessly falls back to in-process `VirtualFfmpegShim` / `VirtualYtdlpShim` (`YouTube.js` + `yt-dlp/ejs`), while Mobile runs in-process virtualization directly inside the sandbox; when disabled, calls go straight to system `PATH` binaries without virtualization |
| `host.webSignIn(opts)` | `{ url, title?, incognito?, userAgent?, evalJs?, cookieNames?, allowManualCookie? }` | Invokes native embedded `web_view()` via `SignInPromptCallback` / `WebSignInHandler` |
| `host.prompt({ qr, text })` | Emits QR code / instruction prompt to UI | Used by NetEase / Bilibili / QQ QR-code login flows |
| `host.sleep(ms)` | Returns `Promise<void>` | Non-blocking Tokio timer |

### Web Standard Globals / Web 标准全局 API

In addition to `globalThis.host`, the plugin QuickJS VM provides standard Web APIs so modern bundled plugins run without Node.js shims:

- **`fetch(input, init?)`**: Alias of `host.fetch` returning a Web Fetch response with `ReadableStream`-compatible `response.body` (`getReader()`, `cancel()`, `locked`, `[Symbol.asyncIterator]()`), `text()`, `json()`, `bytes()` (`Uint8Array`), `arrayBuffer()`, `clone()`, `bodyUsed`, `redirected`, `headers.getSetCookie()`, `AbortSignal` cancellation, and `redirect: "follow" | "manual" | "error"`.
- **`TextEncoder` / `TextDecoder`**: Stateful UTF-8 `TextDecoder(label?, { fatal?, ignoreBOM? })` supporting `{ stream: true }` multi-byte UTF-8 buffering across chunk boundaries (`ArrayBuffer` and any `ArrayBufferView`).
- **`crypto`**: `crypto.getRandomValues(typedArray)` (OS CSPRNG-backed integer `TypedArray` fill, 65,536-byte quota) and `crypto.randomUUID()` (RFC 4122 v4).
- **`URL` / `URLSearchParams` / `AbortController` / `AbortSignal` / `structuredClone` / `atob` / `btoa` / `setTimeout` / `setInterval`**: Built-in cancellable timers and Web URL/Abort primitives.

---

## 4. `plugin.json`, `requestHeaders`, and `locales/`

### `requestHeaders` Template Expansion / 请求头匹配与模板展开

```json
{
  "requestHeaders": [
    {
      "match": "*.hdslb.com",
      "kinds": ["image"],
      "headers": {
        "Referer": "https://www.bilibili.com/"
      }
    },
    {
      "match": "*.bilivideo.com",
      "kinds": ["audio"],
      "headers": {
        "Referer": "https://www.bilibili.com/",
        "Cookie": "{{cookie}}",
        "Authorization": "Bearer {{config.apiToken}}"
      }
    }
  ]
}
```

- Supported globs: exact hosts (`api.bilibili.com`), wildcard subdomains (`*.hdslb.com`), wildcard `*`, and URL path globs (`https://cdn.*.net/audio/*`).
- Supported kinds: `"image"`, `"audio"`, `"api"` (omitting `kinds` matches all).
- Supported templates: `{{config.<key>}}`, `{{cookie}}`, `{{host}}`, `{{origin}}`, `{{url}}`. Empty template expansions are automatically omitted.

### `locales/<lang>.json`

Place translation files in `locales/zh-CN.json` and `locales/en.json`. Lookup order:
1. Exact normalized locale (`zh-cn`)
2. Base language (`zh`)
3. `en` / `en-us` fallback
4. First available bundle

---

## 5. How `VirtualFfmpegShim` Enables Unmodified `youtube-music` & `bilibili` Plugins on Mobile

### 虚拟 FFmpeg 代理如何让 `youtube-music` / `bilibili` 插件零修改运行在移动端

On desktop, plugins like `mygo-music`'s `youtube-music` and `bilibili` invoke an external `ffmpeg` binary via `host.spawn("ffmpeg", ...)` for two reasons:
1. **Capability Probing**:
   - `ffmpeg -version`
   - `ffmpeg -hide_banner -h protocol=http` (checking for `-request_size`)
2. **Local Loopback HTTP Stream Relay**:
   - `ffmpeg -headers "Referer: ...\r\nCookie: ..." -user_agent "..." -reconnect 1 -i <upstream> -vn -f mp4 -listen 1 http://127.0.0.1:<port>/`
   - The plugin polls `proc.errorOutput().includes("Input #0")` and then returns `http://127.0.0.1:<port>/` as the stream URL.

On iOS, Android, and HarmonyOS, spawning external executables is prohibited by the OS sandbox. `nsh-music` solves this **in-process**:

1. When **Mobile Virtualization Compatibility (`移动端虚拟化兼容`)** is enabled (default), `host.spawn(cmd, args, opts)` checks `is_ffmpeg_command(cmd)`: Desktop prefers a user-installed `ffmpeg` on `PATH` (when `Allow Programs` is granted) and seamlessly falls back to `VirtualFfmpegShim` when absent, while Mobile runs `VirtualFfmpegShim` directly in-process. When disabled by the user, `host.spawn` invokes system `PATH` directly without virtualization.
2. Probe invocations (`-version`, `-h protocol=http`) return synthetic FFmpeg banners and protocol capabilities immediately without launching a subprocess.
3. Relay invocations (`-listen 1 http://127.0.0.1:<port>/`) spin up an in-process **Tokio loopback HTTP server** inside `nsh_music_audio::virtual_ffmpeg::VirtualFfmpegShim`, immediately emit `Input #0, ...` on virtual stderr (`proc.errorOutput()`), and stream the upstream audio using Rust `reqwest` with the exact parsed `-headers` / `-user_agent` preserved across cross-host 3xx CDN redirects.
4. `proc.kill()` cleanly shuts down the loopback server when tracks switch or the plugin unloads.

**Result**: Unmodified `youtube-music` and `bilibili` plugins work seamlessly on Phone, Pad, and Desktop with **zero external FFmpeg installation**.

---

## 5.1 How `VirtualYtdlpShim` Enables `host.spawn("yt-dlp", ...)` on Mobile & Desktop Without Python

### 虚拟 yt-dlp 代理如何在移动端与无 Python 环境下零修改运行 `yt-dlp` 调用

Plugins such as `youtube-music` fall back to `host.spawn("yt-dlp", ...)` (passing session cookies over stdin via `--config-locations -` and requesting `--print "%(.{url,http_headers})j"`, `-g`, or `-j`). On iOS, Android, HarmonyOS, and desktops without `yt-dlp` installed, `nsh-music` intercepts `yt-dlp` via `YtdlpVirtualShim` (`crates/nsh-music-plugin/src/virtual_ytdlp.rs`, advertised via `host.capabilities.embeddedYtdlp === true`):

1. **Mobile Virtualization Compatibility & User Self-Install Routing / 自动择优与用户自装策略** (`Settings -> Plugins -> Mobile Virtualization Compatibility` / `设置 → 插件 → 移动端虚拟化兼容`):
   - **Enabled (default / 默认开启)**:
     - **Desktop (macOS / Windows / Linux)**: Automatically prefers a user-installed `yt-dlp` (or `ffmpeg`) on system `PATH` (when `Allow Programs` is granted), and seamlessly falls back to in-process virtualization (`YouTube.js + yt-dlp/ejs` / `VirtualFfmpegShim`) when absent. NSH Music never auto-downloads external binaries.
     - **Mobile (iOS / Android / HarmonyOS)**: Runs in-process virtualization directly inside the app sandbox with zero Python or external executable dependency.
   - **Disabled / 用户关闭开关**:
     - Bypasses in-process virtualization and invokes `yt-dlp` / `ffmpeg` directly from system `PATH` (surfacing a runtime error banner on the plugin card if unavailable or blocked by the sandbox).
2. **CLI & Stdin Config Compatibility**:
   - `--version` immediately emits a compatible `yt-dlp` version banner (identifying the embedded `YouTube.js` + `yt-dlp/ejs` engine) and exits `0`.
   - `--config-locations -` waits for `proc.closeStdin()` and parses quoted flags (`--add-headers "Cookie:..."`, `--add-header`, `--user-agent`, `--cookies`) merged with CLI flags (`-f`/`--format`, `--extractor-args`, `--print`, `-g`/`--get-url`, `-j`/`--dump-json`).
3. **Embedded Upstream `YouTube.js` + `yt-dlp/ejs` Engine (in `quickjs-jit`)**:
   - Instead of a handwritten InnerTube implementation, `VirtualYtdlpShim` embeds upstream **[LuanRT/YouTube.js (`youtubei.js`)](https://github.com/LuanRT/YouTube.js)** (MIT) together with **[yt-dlp/ejs](https://github.com/yt-dlp/ejs)** (Unlicense; `meriyah` / `astring` MIT) inside `quickjs-jit`, bridged to `HostContext`'s Rust `reqwest` (`rustls-tls`) HTTP stack.
   - Supports authenticated cookie sessions with automatic anonymous client fallback across upstream `YouTube.js` clients, invokes `yt-dlp/ejs` to solve signature (`sig`) and throttle (`n`) challenges offline (with disk-cached preprocessed player scripts), prefers direct audio streams (`AudioEngine`'s progressive cache streams them natively without a redundant loopback relay) and falls back to HLS manifests relayed by `VirtualFfmpegShim` when needed.
   - Emits exact `%(.{url,http_headers})j`, `-g`, or `-j` output lines on stdout and standard `ERROR: [youtube] <id>: <reason>` diagnostics on stderr when unavailable or sign-in is required.

---

## 6. Mobile Migration Guide / 移动端迁移指南

### Case A: Plugin calls an unvirtualized external CLI (`python3`, `node`, custom binaries, etc.) — `MigrationRequired`

On iOS, Android, and HarmonyOS, calling `host.spawn("python3", ...)` (any binary outside `host.capabilities.virtualProcesses`, i.e., not `ffmpeg` or `yt-dlp`) throws a structured `MobileSandboxError`:
```json
{
  "name": "MobileSandboxError",
  "code": "MOBILE_SANDBOX_ERROR",
  "command": "python3",
  "platform": "harmonyos",
  "migrationPath": "Spawning external binary `python3` is prohibited by the harmonyos mobile sandbox (not permitted on mobile). Check `host.capabilities.canSpawnExternal` first; replace external HTTP/signing CLIs with `host.fetch` + `host.crypto`, return direct stream URLs with `headers`, or rely on `host.spawn(\"ffmpeg\" | \"yt-dlp\", ...)` which are virtualized in-process. See docs/PLUGIN_COMPAT.md for migration steps."
}
```

**Step-by-step migration / 迁移步骤**:
1. **Check capabilities at runtime / 运行时检查能力**:
   ```ts
   if (host.capabilities.canSpawnExternal) {
     // Desktop fallback using external CLI
   } else {
     // Pure JS/TS path using host.fetch + host.crypto
   }
   ```
2. **Replace CLI HTTP/signing logic with `host.fetch` + `host.crypto`**:
   - Use `host.crypto.md5`, `sha256`, `hmacSha256`, `aesEcbEncrypt`, and `base64Encode` instead of shelling out to Python/OpenSSL.
3. **Return `StreamInfo` with `headers` directly**:
   - Instead of proxying through a CLI downloader, return `{ url, headers: { Referer, Cookie, "User-Agent": ... } }` from `getStreamUrl`. `nsh-music-audio`'s progressive HTTP cache preserves custom headers across 3xx redirects natively.
4. **If you need remuxing/loopback relay**:
   - Use `host.spawn("ffmpeg", ...)` — it is universally supported via `VirtualFfmpegShim` (`host.capabilities.embeddedFfmpeg === true`).

### Case B: Plugin uses `host.webSignIn` with `HttpOnly` Cookies — `Partial`

Mobile WebViews (`WKWebView` on iOS, `WebView` on Android, ArkWeb on HarmonyOS) may restrict JavaScript (`document.cookie`) from reading `HttpOnly` session cookies.

**Step-by-step migration / 迁移步骤**:
1. **Prefer QR Code Login (`getQrSignIn` / `pollQrSignIn` / `host.prompt`)**:
   - QR sign-in requires zero WebView cookie extraction and achieves **`Full`** compatibility on every platform.
2. **Enable Manual Cookie Fallback on `host.webSignIn`**:
   ```ts
   const res = await host.webSignIn({
     url: "https://example.com/login",
     title: host.i18n.t("auth.title"),
     incognito: true,
     cookieNames: ["SESSDATA", "MUSIC_U"],
     allowManualCookie: true,
   });
   const cookie = res.cookie || host.config.get("cookie");
   ```
3. **Declare a `cookie` setting in `plugin.json`**:
   - Adding a `type: "password"` `cookie` entry in `configSchema` lets mobile users paste or update credentials directly inside Settings -> Plugin Config as a reliable fallback.
