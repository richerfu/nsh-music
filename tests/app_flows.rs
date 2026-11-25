//! End-to-end headless UI verification for `nsh-music` across Desktop
//! (`1280x800`), Pad Landscape (`1024x768`), Pad Portrait (`768x1024`), and
//! Phone (`390x844`) viewports.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nsh_core::geometry::{Points, Size};
use nsh_core::platform::{Idiom, Os};
use nsh_engine::{EngineConfig, Environment, Root};
use nsh_music_core::local::LocalMusicSource;
use nsh_music_core::session::PlaybackSession;
use nsh_music_core::settings::AppSettings;
use nsh_testing::Headless;

const GONE_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_TOUCH_TARGET_PT: f32 = 44.0;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Serializes headless UI tests and snapshots/restores all persisted user files
/// (`settings.json`, `session.json`, `library.json`, `plugins/`, `plugin-data/`)
/// so every test starts completely empty (zero mock data) and leaves the host
/// directories untouched.
static TEST_STATE_LOCK: Mutex<()> = Mutex::new(());

struct FileBackup {
    path: PathBuf,
    prev: Option<Vec<u8>>,
}

impl FileBackup {
    fn stash(path: PathBuf) -> Self {
        let prev = fs::read(&path).ok();
        let _ = fs::remove_file(&path);
        Self { path, prev }
    }

    fn restore(&self) {
        match &self.prev {
            Some(bytes) => {
                if let Some(parent) = self.path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(&self.path, bytes);
            }
            None => {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

struct DirBackup {
    path: PathBuf,
    backup: Option<PathBuf>,
}

impl DirBackup {
    fn stash(path: PathBuf, suffix: u64) -> Self {
        let backup = if path.exists() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("dir");
            let dest = path.with_file_name(format!(
                "{name}.nsh-test-backup-{}-{suffix}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dest);
            fs::rename(&path, &dest).expect("stash existing directory for test isolation");
            Some(dest)
        } else {
            None
        };
        Self { path, backup }
    }

    fn restore(&self) {
        let _ = fs::remove_dir_all(&self.path);
        if let Some(backup) = &self.backup {
            let _ = fs::rename(backup, &self.path);
        }
    }
}

struct IsolatedStateGuard {
    _lock: MutexGuard<'static, ()>,
    settings: FileBackup,
    session: FileBackup,
    library: FileBackup,
    plugins_dir: DirBackup,
    plugin_data_dir: DirBackup,
    temp_dirs: Vec<PathBuf>,
}

impl IsolatedStateGuard {
    fn acquire() -> Self {
        let lock = TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let suffix = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);

        let library_path = LocalMusicSource::default_library_path();
        let data_base = library_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::temp_dir().join("nsh-music"));

        let settings = FileBackup::stash(AppSettings::default_path());
        let session = FileBackup::stash(PlaybackSession::default_path());
        let library = FileBackup::stash(library_path);
        let plugins_dir = DirBackup::stash(data_base.join("plugins"), suffix);
        let plugin_data_dir = DirBackup::stash(data_base.join("plugin-data"), suffix);

        Self {
            _lock: lock,
            settings,
            session,
            library,
            plugins_dir,
            plugin_data_dir,
            temp_dirs: Vec::new(),
        }
    }

    fn make_temp_dir(&mut self, label: &str) -> PathBuf {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "nsh-music-app-flow-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp test dir");
        self.temp_dirs.push(dir.clone());
        dir
    }
}

impl Drop for IsolatedStateGuard {
    fn drop(&mut self) {
        for dir in &self.temp_dirs {
            let _ = fs::remove_dir_all(dir);
        }
        self.plugins_dir.restore();
        self.plugin_data_dir.restore();
        self.library.restore();
        self.session.restore();
        self.settings.restore();
    }
}

fn write_minimal_wav(path: &Path) {
    let sample_rate: u32 = 8_000;
    let channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    // 15 seconds so the real audio engine does not finish and auto-advance
    // before headless assertions inspect active track state and sidecar lyrics.
    let num_samples: u32 = sample_rate * 15;
    let data_size = num_samples * u32::from(channels) * u32::from(bits_per_sample / 8);
    let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample / 8);
    let block_align = channels * (bits_per_sample / 8);
    let riff_chunk_size = 36 + data_size;

    let mut buf = Vec::with_capacity((44 + data_size) as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&riff_chunk_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&bits_per_sample.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());
    buf.resize(buf.len() + data_size as usize, 0);

    fs::write(path, buf).expect("write minimal wav");
}

fn write_fixture_local_wavs(dir: &Path) {
    write_minimal_wav(&dir.join("Acoustic Sunrise.wav"));
    write_minimal_wav(&dir.join("Evening Breeze.wav"));
    fs::write(
        dir.join("Acoustic Sunrise.lrc"),
        "[00:00.10]Sunlight over warm strings\n[00:02.00]Calm morning resonance\n",
    )
    .expect("write sidecar lrc");
}

fn write_fixture_js_plugin(dir: &Path) {
    fs::write(
        dir.join("plugin.json"),
        r#"{
            "id": "fixture-wave",
            "name": "Fixture Wave Music",
            "version": "1.2.0",
            "author": "Verifier Suite",
            "description": "Local JS fixture plugin for headless integration tests",
            "main": "index.js",
            "navigation": [
                { "title": "Favorites", "page": "favlist", "icon": "heart" }
            ],
            "permissions": {
                "network": ["api.fixture.invalid"]
            }
        }"#,
    )
    .expect("write fixture plugin.json");

    fs::write(
        dir.join("index.js"),
        r#"
const SAMPLE_TRACK = {
    id: "fw-1",
    name: "Signal Drift",
    artists: [{ id: "ar-1", name: "Wave Runner" }],
    album: { id: "al-1", name: "Test Signals" },
    duration: 180000
};

export async function getHome() {
    return {
        title: "Fixture Home",
        sections: [
            {
                title: "Fixture Picks",
                tracks: [SAMPLE_TRACK]
            }
        ]
    };
}

export async function browse(arg) {
    const page = typeof arg === "string" ? arg : (arg && arg.page) || "";
    if (page === "favlist") {
        return {
            title: "Favorites",
            sections: [
                {
                    title: "Favorite Waves",
                    tracks: [SAMPLE_TRACK]
                }
            ]
        };
    }
    return {
        title: "Fixture Browse",
        sections: [
            {
                title: "Browse All",
                tracks: [SAMPLE_TRACK]
            }
        ]
    };
}

export async function getBrowse(page) {
    return browse(page);
}

export async function suggest(arg) {
    const q = typeof arg === "string" ? arg : (arg && arg.query) || "";
    if (!q.trim()) {
        return [];
    }
    return ["Signal Drift", "Signal Wave", "Signal Acoustic"];
}

export async function searchSuggestions(query) {
    return suggest(query);
}

export async function search(query) {
    return {
        tracks: [SAMPLE_TRACK],
        more: false
    };
}

export async function getComments() {
    return {
        comments: [
            {
                id: "c-1",
                author: "Listener",
                text: "Crisp acoustic mix!",
                likes: 42,
                time: 1730000000
            }
        ],
        total: 1,
        more: false
    };
}
"#,
    )
    .expect("write fixture index.js");
}

fn write_fixture_phone_auth_plugin(dir: &Path) {
    fs::write(
        dir.join("plugin.json"),
        r#"{
            "id": "fixture-phone-auth",
            "name": "Fixture Phone Auth",
            "version": "1.0.0",
            "author": "Verifier Suite",
            "description": "Phone single-header and WebSignIn fixture plugin",
            "main": "index.js",
            "navigation": [
                { "title": "Favorites", "page": "favlist", "icon": "heart" }
            ],
            "permissions": {
                "network": ["music.youtube.com"]
            }
        }"#,
    )
    .expect("write fixture phone auth plugin.json");

    fs::write(
        dir.join("index.js"),
        r#"
const TRACK = {
    id: "pa-1",
    name: "Harbor Echo",
    artists: [{ id: "ar-pa", name: "Port Synth" }],
    album: { id: "al-pa", name: "Night Dock" },
    duration: 185000
};

let signedIn = false;

export async function home() {
    return {
        title: "Fixture Home",
        sections: [
            {
                title: "Featured Dock",
                tracks: [TRACK]
            }
        ]
    };
}

export async function browse(arg) {
    const page = typeof arg === "string" ? arg : (arg && arg.page) || "";
    if (page === "favlist") {
        return {
            title: "Favorites",
            sections: [
                {
                    title: "Favorite Waves",
                    tracks: [TRACK]
                }
            ]
        };
    }
    return {
        title: "Fixture Browse",
        sections: [
            {
                title: "Browse All",
                tracks: [TRACK]
            }
        ]
    };
}

export async function stations() {
    return [{ id: "st-1", name: "Synth FM" }];
}

export async function shows() {
    return [{ id: "sh-1", name: "Sound Lab", publisher: "Studio" }];
}

export async function signIn() {
    const res = await host.webSignIn({
        url: "https://music.youtube.com/",
        title: "Fixture Web Sign In",
        cookiesOf: "https://music.youtube.com",
        until: ["SAPISID", "__Secure-3PSID"],
        allowManualCookie: true
    });
    if (!res || res.cancelled || !res.cookie) {
        throw new Error("sign-in cancelled");
    }
    const hasSapisid = (res.cookies && res.cookies.SAPISID) || res.cookie.includes("SAPISID=");
    const hasPsid = (res.cookies && res.cookies["__Secure-3PSID"]) || res.cookie.includes("__Secure-3PSID=");
    if (!hasSapisid || !hasPsid) {
        throw new Error("missing required cookies");
    }
    signedIn = true;
    return { id: "u-99", name: "Verified Listener" };
}

export async function account() {
    if (!signedIn) {
        return null;
    }
    // Simulate transient network failure so store.refresh_sources() must preserve cached account.
    throw new Error("transient account probe error");
}
"#,
    )
    .expect("write fixture phone auth index.js");
}

fn start_viewport(width: f32, height: f32, os: Os, idiom: Idiom) -> Headless {
    let viewport = Size::new(Points(width), Points(height));
    let root = Root::of::<nsh_app_music::App>()
        .tokens(nsh_app_music::app_tokens())
        .app_id("com.richerfu.nshmusic.test");
    let env = Environment {
        window: viewport,
        os,
        idiom,
        ..Environment::default()
    };

    let mut config = EngineConfig::new(root);
    config.viewport = viewport;
    config.environment = env;

    let mut app = Headless::start(config).expect("headless nsh-music engine starts");
    app.wait_for("id:nsh-music-root")
        .expect("root container mounts");
    app
}

fn check_min_bounds(app: &Headless, test_id: &str, min_w: f32, min_h: f32) -> Result<(), String> {
    let nodes = app.screen().layout_nodes();
    let Some(node) = nodes
        .iter()
        .find(|n| !n.hidden && n.test_id.as_deref() == Some(test_id))
    else {
        return Err(format!("missing visible LayoutNode `{test_id}`"));
    };
    let w = node.bounds.size.width.0;
    let h = node.bounds.size.height.0;
    if w < min_w - 0.5 || h < min_h - 0.5 {
        return Err(format!(
            "`{test_id}` is {w:.1}x{h:.1}pt (expected >= {min_w:.0}x{min_h:.0}pt)"
        ));
    }
    Ok(())
}

fn node_origin_y(app: &Headless, test_id: &str) -> Result<f32, String> {
    let nodes = app.screen().layout_nodes();
    let Some(node) = nodes
        .iter()
        .find(|n| !n.hidden && n.test_id.as_deref() == Some(test_id))
    else {
        return Err(format!("missing visible LayoutNode `{test_id}`"));
    };
    Ok(node.bounds.origin.y.0)
}

fn check_min_touch_targets(app: &Headless, test_ids: &[&str]) -> Vec<String> {
    test_ids
        .iter()
        .filter_map(|&id| check_min_bounds(app, id, MIN_TOUCH_TARGET_PT, MIN_TOUCH_TARGET_PT).err())
        .collect()
}

#[test]
fn desktop_zero_mock_empty_boot_settings_and_locale() {
    let _guard = IsolatedStateGuard::acquire();
    let mut app = start_viewport(1280.0, 800.0, Os::MacOs, Idiom::Desktop);

    // 1. Desktop shell mounts cleanly with zero fake/mock tracks and actionable CTAs.
    app.wait_for("id:desktop-shell").unwrap();
    app.wait_for("id:desktop-sidebar").unwrap();
    app.wait_for("id:player-bar").unwrap();
    app.wait_for("还没有音乐").unwrap();
    app.wait_for("添加音乐文件夹").unwrap();
    app.wait_for("安装插件").unwrap();
    app.wait_for("id:player-bar >> text:未在播放").unwrap();

    // Standalone `nav-plugins` has been unified into the Desktop Settings window.
    assert!(
        !app.screen().matches("id:nav-plugins").unwrap(),
        "standalone `nav-plugins` must be absent from desktop sidebar"
    );

    // Confirm zero leftover mock/demo strings are displayed on boot.
    for forbidden in [
        "Harbor Lights",
        "Local & Demo",
        "午夜霓虹漫游",
        "霓虹港湾",
        "内置试听",
        "早上好",
        "MyGo Music",
    ] {
        assert!(
            !app.screen().shows(forbidden),
            "unexpected mock/demo text `{forbidden}` visible on clean boot"
        );
    }

    // 2. Clicking `nav-settings` opens the native Desktop Settings child window.
    assert!(
        app.window_state("settings").unwrap().is_none(),
        "settings window should start closed"
    );
    app.press("id:nav-settings").unwrap();
    app.wait_for("id:settings-sidebar").unwrap();
    app.wait_for("id:settings-search-input").unwrap();
    app.wait_for("id:settings-detail-pane").unwrap();
    assert!(
        app.window_state("settings").unwrap().is_some(),
        "clicking nav-settings must open the native `settings` child window"
    );

    // Verify all 6 unified Settings sections exist inside the Settings sidebar.
    for section_id in [
        "id:settings-nav-general",
        "id:settings-nav-plugins",
        "id:settings-nav-playback",
        "id:settings-nav-cache",
        "id:settings-nav-experiments",
        "id:settings-nav-about",
    ] {
        app.wait_for(section_id).unwrap();
    }

    // Theme toggles in General: Light -> Dark -> System.
    app.press("role:button[浅色]").unwrap();
    assert!(app.screen().selected("role:button[浅色]").unwrap());
    app.press("role:button[深色]").unwrap();
    assert!(app.screen().selected("role:button[深色]").unwrap());
    app.press("role:button[跟随系统]").unwrap();
    assert!(app.screen().selected("role:button[跟随系统]").unwrap());

    // Live locale switch: zh-CN -> en -> verify About brand is strictly `NSH Music`.
    app.press("role:button[English]").unwrap();
    app.wait_for("id:settings-nav-general >> text:General")
        .unwrap();
    app.wait_for("id:settings-nav-plugins >> text:Plugins")
        .unwrap();
    app.wait_for("id:nav-home >> text:Discover").unwrap();

    // Filter Settings via live search input inside the Settings child window.
    app.type_text("id:settings-search-input", "Fog").unwrap();
    app.wait_for("Ambient Fog Backdrop").unwrap();
    app.clear("id:settings-search-input").unwrap();

    // Verify About section strictly displays `NSH Music`.
    app.press("id:settings-nav-about").unwrap();
    app.wait_for("id:settings-detail-pane >> text:NSH Music")
        .unwrap();
    assert!(
        !app.screen().shows("MyGo Music"),
        "About pane must never show `MyGo Music`"
    );

    // Switch back to General & zh-CN.
    app.press("id:settings-nav-general").unwrap();
    app.press("role:button[简体中文]").unwrap();
    app.wait_for("id:nav-home >> text:发现").unwrap();

    // Close the Settings child window and confirm `mod+,` re-opens it cleanly.
    app.press_window_close("settings").unwrap();
    app.wait_for_gone("id:settings-sidebar", GONE_TIMEOUT)
        .unwrap();
    assert!(app.window_state("settings").unwrap().is_none());

    app.press_key("mod+,").unwrap();
    app.wait_for("id:settings-sidebar").unwrap();
    assert!(
        app.window_state("settings").unwrap().is_some(),
        "`mod+,` shortcut must open the Desktop Settings child window"
    );
    app.press_window_close("settings").unwrap();
    app.wait_for_gone("id:settings-sidebar", GONE_TIMEOUT)
        .unwrap();
}

#[test]
fn desktop_local_wav_scan_playback_and_capability_gated_now_playing() {
    let mut guard = IsolatedStateGuard::acquire();
    let music_dir = guard.make_temp_dir("wav-scan");
    write_fixture_local_wavs(&music_dir);

    let mut app = start_viewport(1280.0, 800.0, Os::MacOs, Idiom::Desktop);
    app.wait_for("id:desktop-shell").unwrap();
    app.wait_for("还没有音乐").unwrap();

    // 1. Pick local music folder via Home CTA -> lofty scans real WAVs + sidecar LRC.
    app.on_pick_paths(vec![music_dir]).unwrap();
    app.press("role:button[添加音乐文件夹]").unwrap();

    app.wait_for("id:desktop-main-viewport >> text:Acoustic Sunrise")
        .unwrap();
    app.wait_for("id:desktop-main-viewport >> text:Evening Breeze")
        .unwrap();

    // 2. Play real scanned WAV track -> player-bar updates.
    app.press("id:desktop-main-viewport >> text:Acoustic Sunrise >> nth=0")
        .unwrap();
    app.wait_for("id:player-bar >> text:Acoustic Sunrise")
        .unwrap();

    // Next track advances to second local WAV.
    app.press("id:desktop-next-btn").unwrap();
    app.wait_for("id:player-bar >> text:Evening Breeze")
        .unwrap();

    // Prev track returns to first local WAV (which has sidecar LRC lyrics).
    app.press("id:desktop-prev-btn").unwrap();
    app.wait_for("id:player-bar >> text:Acoustic Sunrise")
        .unwrap();

    // 3. Open Now Playing overlay -> verify capability-gated tabs on Local source.
    app.press("id:desktop-now-playing-trigger").unwrap();
    app.wait_for("id:now-playing-host").unwrap();
    app.wait_for("id:now-playing-overlay").unwrap();
    app.wait_for("id:now-playing-overlay >> text:Acoustic Sunrise")
        .unwrap();

    // Local source does NOT support `comments` or podcast episodes: those tabs must be hidden!
    assert!(
        !app.screen()
            .matches("id:now-playing-overlay >> label:评论")
            .unwrap(),
        "Comments tab must be hidden on Local source"
    );
    assert!(
        !app.screen()
            .matches("id:now-playing-overlay >> label:节目详情")
            .unwrap(),
        "Podcast tab must be hidden for non-episode local track"
    );

    // Lyrics & Queue tabs remain available and switch cleanly.
    assert!(
        app.screen()
            .selected("id:now-playing-overlay >> label:歌词")
            .unwrap()
    );
    app.wait_for("id:now-playing-overlay >> text:Sunlight over warm strings")
        .unwrap();

    app.press("id:now-playing-overlay >> label:队列").unwrap();
    app.wait_for("id:now-playing-overlay >> text:/即将播放/")
        .unwrap();

    app.press("id:now-playing-overlay >> label:歌词").unwrap();
    assert!(
        app.screen()
            .selected("id:now-playing-overlay >> label:歌词")
            .unwrap()
    );

    // Switch Now Playing modes cleanly: Cover -> Vinyl -> Cassette -> Cover.
    assert!(
        app.screen()
            .selected("id:now-playing-overlay >> label:封面")
            .unwrap()
    );
    app.press("id:now-playing-overlay >> label:黑胶唱机")
        .unwrap();
    assert!(
        app.screen()
            .selected("id:now-playing-overlay >> label:黑胶唱机")
            .unwrap()
    );
    app.press("id:now-playing-overlay >> label:磁带机").unwrap();
    assert!(
        app.screen()
            .selected("id:now-playing-overlay >> label:磁带机")
            .unwrap()
    );
    app.press("id:now-playing-overlay >> label:封面").unwrap();
    assert!(
        app.screen()
            .selected("id:now-playing-overlay >> label:封面")
            .unwrap()
    );

    // Dismiss Now Playing via Escape and confirm unmount.
    app.press_key("escape").unwrap();
    app.wait_for_gone("id:now-playing-overlay", GONE_TIMEOUT)
        .unwrap();
}

#[test]
fn desktop_modal_layout_regression_and_staged_plugin_install() {
    let mut guard = IsolatedStateGuard::acquire();
    let plugin_dir = guard.make_temp_dir("fixture-plugin");
    write_fixture_js_plugin(&plugin_dir);

    let mut app = start_viewport(1280.0, 800.0, Os::MacOs, Idiom::Desktop);
    app.wait_for("id:desktop-shell").unwrap();
    assert!(!app.screen().matches("id:nav-plugins").unwrap());

    // 1. Open Desktop Settings window -> Plugins section and verify import toolbar buttons.
    app.press("id:nav-settings").unwrap();
    app.wait_for("id:settings-nav-plugins").unwrap();
    app.press("id:settings-nav-plugins").unwrap();
    app.wait_for("id:plugins-btn-github").unwrap();
    app.wait_for("id:plugins-btn-folder").unwrap();
    app.wait_for("id:plugins-btn-zip").unwrap();
    app.wait_for("id:plugins-btn-open-dir").unwrap();

    // 2. Open ActiveModal::InstallPlugin inside Settings window and assert non-0px bounds.
    app.press("id:plugins-btn-github").unwrap();
    app.wait_for("id:install-plugin-input").unwrap();
    app.wait_for("id:install-plugin-submit").unwrap();
    app.wait_for("id:install-plugin-pick-folder").unwrap();
    app.wait_for("id:install-plugin-pick-zip").unwrap();

    for id in [
        "install-plugin-input",
        "install-plugin-submit",
        "install-plugin-pick-folder",
        "install-plugin-pick-zip",
    ] {
        check_min_bounds(&app, id, 80.0, 28.0).unwrap_or_else(|e| {
            panic!("InstallPlugin modal layout regression: {e}");
        });
    }

    // 3. Stage the temporary local JS plugin directory -> ConfirmPluginInstall modal opens.
    app.type_text("id:install-plugin-input", &plugin_dir.to_string_lossy())
        .unwrap();
    app.press("id:install-plugin-submit").unwrap();
    app.wait_for("id:confirm-plugin-install-btn").unwrap();
    app.wait_for("Fixture Wave Music").unwrap();
    app.wait_for("api.fixture.invalid").unwrap();

    // Assert ConfirmPluginInstall primary CTA has healthy non-0px bounds.
    check_min_bounds(&app, "confirm-plugin-install-btn", 80.0, 28.0).unwrap_or_else(|e| {
        panic!("ConfirmPluginInstall modal layout regression: {e}");
    });

    // 4. Confirm installation -> plugin appears in Settings Plugins list, then close Settings.
    app.press("id:confirm-plugin-install-btn").unwrap();
    app.wait_for_gone("id:confirm-plugin-install-btn", GONE_TIMEOUT)
        .unwrap();
    app.wait_for("id:settings-detail-pane >> text:Fixture Wave Music")
        .unwrap();
    app.press_window_close("settings").unwrap();
    app.wait_for_gone("id:settings-sidebar", GONE_TIMEOUT)
        .unwrap();

    // Switch active source to the newly installed plugin and verify its home feed & custom nav load.
    app.press("id:desktop-source-switcher").unwrap();
    app.wait_for("切换音乐源").unwrap();
    app.wait_for("Fixture Wave Music").unwrap();
    app.press("role:button[/Fixture Wave Music/]").unwrap();
    app.wait_for("Signal Drift").unwrap();
    app.wait_for("id:desktop-sidebar >> role:button[Favorites]")
        .unwrap();

    // 5. Regression (#116): Back/Forward across Browse + custom plugin nav highlights <= 1 item.
    let assert_single_selection = |app: &Headless,
                                   expected_browse: bool,
                                   expected_fav: bool,
                                   expected_home: bool,
                                   step: &str| {
        let browse_sel = app.screen().selected("id:nav-browse").unwrap();
        let fav_sel = app
            .screen()
            .selected("id:desktop-sidebar >> role:button[Favorites]")
            .unwrap();
        let home_sel = app.screen().selected("id:nav-home").unwrap();
        let total = usize::from(browse_sel) + usize::from(fav_sel) + usize::from(home_sel);
        assert!(
            total <= 1,
            "[{step}] multiple nav items highlighted simultaneously (browse={browse_sel}, fav={fav_sel}, home={home_sel})"
        );
        assert_eq!(
            browse_sel, expected_browse,
            "[{step}] nav-browse active mismatch"
        );
        assert_eq!(fav_sel, expected_fav, "[{step}] Favorites active mismatch");
        assert_eq!(home_sel, expected_home, "[{step}] nav-home active mismatch");
    };

    app.press("id:nav-browse").unwrap();
    assert_single_selection(&app, true, false, false, "click Browse");

    app.press("id:desktop-sidebar >> role:button[Favorites]")
        .unwrap();
    assert_single_selection(&app, false, true, false, "click custom Favorites");

    app.press("id:nav-home").unwrap();
    assert_single_selection(&app, false, false, true, "click Home");

    // Back -> Favorites (nav-browse MUST NOT be selected!).
    app.press("id:desktop-back-btn").unwrap();
    assert_single_selection(&app, false, true, false, "Back -> Favorites");

    // Back -> Browse (nav-browse IS selected, Favorites is NOT!).
    app.press("id:desktop-back-btn").unwrap();
    assert_single_selection(&app, true, false, false, "Back -> Browse");

    // Forward -> Favorites (still strictly single selection).
    app.press("id:desktop-forward-btn").unwrap();
    assert_single_selection(&app, false, true, false, "Forward -> Favorites");

    // 6. Regression (#116 + #115): Floating search suggestions cause 0px shift on sidebar nav,
    // and Search page renders results cleanly without an inline suggestion pill wall.
    let nav_home_y_before = node_origin_y(&app, "nav-home").unwrap();
    let nav_browse_y_before = node_origin_y(&app, "nav-browse").unwrap();

    app.type_text("id:desktop-search-input", "Signal").unwrap();
    app.wait_for("id:desktop-search-suggestions").unwrap();
    app.wait_for("id:desktop-search-suggestions >> text:Signal Drift")
        .unwrap();

    let nav_home_y_after = node_origin_y(&app, "nav-home").unwrap();
    let nav_browse_y_after = node_origin_y(&app, "nav-browse").unwrap();
    assert!(
        (nav_home_y_after - nav_home_y_before).abs() < 0.5,
        "sidebar `nav-home` shifted vertically when suggestions opened ({nav_home_y_before} -> {nav_home_y_after})"
    );
    assert!(
        (nav_browse_y_after - nav_browse_y_before).abs() < 0.5,
        "sidebar `nav-browse` shifted vertically when suggestions opened ({nav_browse_y_before} -> {nav_browse_y_after})"
    );

    // Selecting a suggestion closes the floating popover and opens clean Desktop Search results.
    app.press("id:desktop-search-suggestions >> text:Signal Drift")
        .unwrap();
    app.wait_for_gone("id:desktop-search-suggestions", GONE_TIMEOUT)
        .unwrap();
    app.wait_for("id:search-page-input").unwrap();
    app.wait_for("id:desktop-main-viewport >> text:Signal Drift")
        .unwrap();
    assert!(
        !app.screen()
            .matches("id:desktop-main-viewport >> text:搜索建议")
            .unwrap(),
        "Desktop Search page should not render an inline suggestion pill wall above results"
    );
}

#[test]
fn pad_landscape_and_portrait_adaptive_flows() {
    let mut guard = IsolatedStateGuard::acquire();
    let music_dir = guard.make_temp_dir("pad-wav");
    write_fixture_local_wavs(&music_dir);

    let mut app = start_viewport(1024.0, 768.0, Os::Ios, Idiom::Pad);

    // 1. Pad Landscape (1024x768): left icon rail + floating player dock + zero-mock boot.
    app.wait_for("id:shell-pad-landscape").unwrap();
    app.wait_for("id:pad-icon-rail").unwrap();
    app.wait_for("id:pad-player-dock").unwrap();
    app.wait_for("还没有音乐").unwrap();

    // Standalone `pad-nav-plugins` is removed (unified inside Settings split view).
    assert!(
        !app.screen().matches("id:pad-nav-plugins").unwrap(),
        "standalone `pad-nav-plugins` must be absent on Pad"
    );

    // Capability-gated nav items: Local supports Browse (albums/local), but NOT Radio/Podcasts.
    app.wait_for("id:pad-nav-browse").unwrap();
    for hidden_id in ["id:pad-nav-radio", "id:pad-nav-podcasts"] {
        assert!(
            !app.screen().matches(hidden_id).unwrap(),
            "{hidden_id} should be hidden when active source is Local"
        );
    }

    // Touch target sanity (>= 44pt) across all visible Pad Landscape shell controls.
    let mut touch_violations = check_min_touch_targets(
        &app,
        &[
            "pad-rail-drawer-btn",
            "pad-nav-home",
            "pad-nav-browse",
            "pad-nav-library",
            "pad-nav-search",
            "pad-nav-settings",
            "pad-source-switcher",
            "pad-search-btn",
            "pad-settings-btn",
            "pad-now-playing-trigger",
            "pad-prev-btn",
            "pad-next-btn",
            "pad-queue-btn",
        ],
    );

    // Scan local WAVs via Home CTA, play a track, and inspect Now Playing on Pad Landscape.
    app.on_pick_paths(vec![music_dir]).unwrap();
    app.press("role:button[添加音乐文件夹]").unwrap();
    app.wait_for("id:pad-main-viewport >> text:Acoustic Sunrise")
        .unwrap();
    app.press("id:pad-main-viewport >> text:Acoustic Sunrise >> nth=0")
        .unwrap();
    app.wait_for("id:pad-player-dock >> text:Acoustic Sunrise")
        .unwrap();

    // Navigate Pad destinations & verify iPadOS 2-column split Settings with unified Plugins.
    app.press("id:pad-nav-library").unwrap();
    app.wait_for("新建歌单").unwrap();
    app.press("id:pad-nav-settings").unwrap();
    app.wait_for("id:pad-settings-split").unwrap();
    for section_id in [
        "settings-nav-general",
        "settings-nav-plugins",
        "settings-nav-playback",
        "settings-nav-cache",
        "settings-nav-experiments",
        "settings-nav-about",
    ] {
        app.wait_for(&format!("id:{section_id}")).unwrap();
    }
    touch_violations.extend(check_min_touch_targets(
        &app,
        &[
            "settings-nav-general",
            "settings-nav-plugins",
            "settings-nav-playback",
            "settings-nav-cache",
            "settings-nav-experiments",
            "settings-nav-about",
        ],
    ));

    app.press("id:settings-nav-plugins").unwrap();
    app.wait_for("id:plugins-btn-github").unwrap();

    app.press("id:pad-back-btn").unwrap();
    app.wait_for("新建歌单").unwrap();
    app.press("id:pad-forward-btn").unwrap();
    app.wait_for("id:pad-settings-split").unwrap();

    // Open & close Now Playing overlay on Pad Landscape.
    app.press("id:pad-now-playing-trigger").unwrap();
    app.wait_for("id:now-playing-overlay").unwrap();
    app.press("id:now-playing-overlay >> label:黑胶唱机")
        .unwrap();
    assert!(
        app.screen()
            .selected("id:now-playing-overlay >> label:黑胶唱机")
            .unwrap()
    );
    app.press("id:now-playing-overlay >> label:收起播放页")
        .unwrap();
    app.wait_for_gone("id:now-playing-overlay", GONE_TIMEOUT)
        .unwrap();

    // 2. Rotate to Pad Portrait (768x1024): switches to portrait shell + drawer button.
    app.set_viewport(Size::new(Points(768.0), Points(1024.0)))
        .unwrap();
    app.wait_for("id:shell-pad-portrait").unwrap();
    app.wait_for("id:pad-portrait-drawer-btn").unwrap();
    touch_violations.extend(check_min_touch_targets(
        &app,
        &[
            "pad-portrait-drawer-btn",
            "pad-source-switcher",
            "pad-search-btn",
            "pad-settings-btn",
            "pad-now-playing-trigger",
        ],
    ));

    assert!(
        touch_violations.is_empty(),
        "Pad touch target violations:\n{}",
        touch_violations.join("\n")
    );
}

#[test]
fn phone_flows_back_press_sheet_dismiss_and_touch_targets() {
    let mut guard = IsolatedStateGuard::acquire();
    let music_dir = guard.make_temp_dir("phone-wav");
    write_fixture_local_wavs(&music_dir);

    let mut app = start_viewport(390.0, 844.0, Os::Ios, Idiom::Phone);

    // 1. Phone shell boots cleanly in zero-mock empty state without duplicate top gear, header, or back arrow.
    app.wait_for("id:shell-phone").unwrap();
    app.wait_for("id:phone-header").unwrap();
    app.wait_for("id:phone-refresh-btn").unwrap();
    app.wait_for("id:phone-tab-bar").unwrap();
    app.wait_for("还没有音乐").unwrap();
    assert!(
        !app.screen().matches("id:phone-settings-btn").unwrap(),
        "duplicate top settings gear `phone-settings-btn` must be absent on Phone"
    );
    assert!(
        !app.screen().matches("id:phone-back-btn").unwrap(),
        "`phone-back-btn` must stay hidden on Phone root tabs"
    );
    assert_eq!(
        app.screen()
            .count("id:phone-main-viewport >> text:发现")
            .unwrap(),
        0,
        "Phone Home body must not render a duplicate `发现` heading"
    );
    assert_eq!(
        app.screen().count("text:本地音乐").unwrap(),
        1,
        "Phone Home must render strictly one `本地音乐` source switcher"
    );

    // >= 44pt touch target sanity across always-visible Phone shell controls.
    let mut touch_violations = check_min_touch_targets(
        &app,
        &[
            "phone-source-switcher",
            "phone-refresh-btn",
            "phone-search-btn",
            "tab-home",
            "tab-browse",
            "tab-library",
            "tab-settings",
        ],
    );

    // 2. Root tab switching uses `switch_tab`: NEVER shows `phone-back-btn` and keeps `phone-tab-bar`.
    for tab in [
        "id:tab-browse",
        "id:tab-library",
        "id:tab-settings",
        "id:tab-home",
    ] {
        app.press(tab).unwrap();
        app.wait_for("id:phone-tab-bar").unwrap();
        assert!(
            !app.screen().matches("id:phone-back-btn").unwrap(),
            "root tab {tab} must not display `phone-back-btn`"
        );
    }

    // 3. Phone Settings 2-level native hierarchy: Level-1 grouped list -> Level-2 detail page.
    app.press("id:tab-settings").unwrap();
    app.wait_for("id:phone-settings-list").unwrap();
    let phone_settings_rows = [
        "phone-settings-plugins",
        "phone-settings-general",
        "phone-settings-playback",
        "phone-settings-cache",
        "phone-settings-experiments",
        "phone-settings-about",
    ];
    for row_id in phone_settings_rows {
        app.wait_for(&format!("id:{row_id}")).unwrap();
    }
    touch_violations.extend(check_min_touch_targets(&app, &phone_settings_rows));

    // Drill down into Plugins (`Route::SettingsDetail(SettingsSection::Plugins)`).
    app.press("id:phone-settings-plugins").unwrap();
    app.wait_for("id:phone-settings-detail").unwrap();
    app.wait_for("id:phone-back-btn").unwrap();
    app.wait_for("id:phone-plugins-install-btn").unwrap();
    assert!(
        !app.screen().matches("id:phone-tab-bar").unwrap(),
        "`phone-tab-bar` must be hidden on Level-2 pushed routes"
    );
    assert!(
        !app.screen().matches("id:plugins-btn-open-dir").unwrap(),
        "desktop-only `Open Plugins Folder` button must be hidden on Phone"
    );
    touch_violations.extend(check_min_touch_targets(
        &app,
        &["phone-back-btn", "phone-plugins-install-btn"],
    ));

    // Tap `<` Back -> returns cleanly to Level-1 `phone-settings-list` with `phone-tab-bar` restored.
    app.press("id:phone-back-btn").unwrap();
    app.wait_for("id:phone-settings-list").unwrap();
    app.wait_for("id:phone-tab-bar").unwrap();
    assert!(
        !app.screen().matches("id:phone-back-btn").unwrap(),
        "`phone-back-btn` must hide again on Level-1 `phone-settings-list`"
    );

    // Return to Home tab for Source Switcher, WAV scan, and Now Playing verification.
    app.press("id:tab-home").unwrap();
    app.wait_for("id:phone-source-switcher").unwrap();

    // 4. Open Source Switcher bottom sheet and dismiss cleanly via Phone back-press.
    app.press("id:phone-source-switcher").unwrap();
    app.wait_for("切换音乐源").unwrap();
    app.back().unwrap();
    app.wait_for_gone("切换音乐源", GONE_TIMEOUT).unwrap();

    // 5. Scan local WAV folder, play a track -> floating mini-player dock appears.
    app.on_pick_paths(vec![music_dir]).unwrap();
    app.press("role:button[添加音乐文件夹]").unwrap();
    app.wait_for("id:phone-main-viewport >> text:Acoustic Sunrise")
        .unwrap();
    app.press("id:phone-main-viewport >> text:Acoustic Sunrise >> nth=0")
        .unwrap();
    app.wait_for("id:phone-mini-player-dock").unwrap();
    app.wait_for("id:phone-mini-player >> text:Acoustic Sunrise")
        .unwrap();

    // Check >= 44pt touch targets on mini-player controls too.
    touch_violations.extend(check_min_touch_targets(
        &app,
        &["phone-mini-player", "phone-next-btn"],
    ));

    // 6. Open Now Playing sheet from mini-player, verify capability gating & modes, dismiss via back().
    app.press("id:phone-mini-player").unwrap();
    app.wait_for("id:now-playing-overlay").unwrap();
    assert!(
        !app.screen()
            .matches("id:now-playing-overlay >> label:评论")
            .unwrap(),
        "Comments must be hidden on Local source in Phone Now Playing"
    );

    app.press("id:now-playing-overlay >> label:黑胶唱机")
        .unwrap();
    assert!(
        app.screen()
            .selected("id:now-playing-overlay >> label:黑胶唱机")
            .unwrap()
    );
    app.press("id:now-playing-overlay >> label:磁带机").unwrap();
    assert!(
        app.screen()
            .selected("id:now-playing-overlay >> label:磁带机")
            .unwrap()
    );
    app.press("id:now-playing-overlay >> label:歌词").unwrap();
    app.wait_for("id:now-playing-overlay >> text:Sunlight over warm strings")
        .unwrap();

    app.back().unwrap();
    app.wait_for_gone("id:now-playing-overlay", GONE_TIMEOUT)
        .unwrap();

    // 7. System `back()` pops pushed Level-2 screen (Search -> Home).
    app.press("id:phone-search-btn").unwrap();
    app.wait_for("id:phone-back-btn").unwrap();
    assert!(!app.screen().matches("id:phone-tab-bar").unwrap());
    app.back().unwrap();
    app.wait_for("id:phone-tab-bar").unwrap();
    assert!(!app.screen().matches("id:phone-back-btn").unwrap());

    assert!(
        touch_violations.is_empty(),
        "Phone touch target violations:\n{}",
        touch_violations.join("\n")
    );
}

#[test]
fn phone_single_header_and_web_sign_in_refresh() {
    let mut guard = IsolatedStateGuard::acquire();
    let plugin_dir = guard.make_temp_dir("phone-auth-plugin");
    write_fixture_phone_auth_plugin(&plugin_dir);

    let mut app = start_viewport(390.0, 844.0, Os::Ios, Idiom::Phone);
    app.wait_for("id:shell-phone").unwrap();
    app.wait_for("id:phone-header").unwrap();
    app.wait_for("id:phone-source-switcher").unwrap();
    app.wait_for("id:phone-refresh-btn").unwrap();
    app.wait_for("id:phone-search-btn").unwrap();

    // 1. Phone Home single header & single source switcher (zero duplicates).
    assert_eq!(
        app.screen().count("id:phone-header >> text:发现").unwrap(),
        1
    );
    assert_eq!(
        app.screen()
            .count("id:phone-main-viewport >> text:发现")
            .unwrap(),
        0,
        "Phone Home must not duplicate `发现` heading inside the scrollable body"
    );
    assert_eq!(
        app.screen().count("text:本地音乐").unwrap(),
        1,
        "Phone Home must render strictly one `本地音乐` source switcher"
    );

    let mut touch_violations = check_min_touch_targets(
        &app,
        &[
            "phone-source-switcher",
            "phone-refresh-btn",
            "phone-search-btn",
        ],
    );

    // 2. Phone Browse root tab: `phone-refresh-btn` >= 44pt and zero duplicate `浏览` body heading.
    app.press("id:tab-browse").unwrap();
    app.wait_for("id:phone-refresh-btn").unwrap();
    assert_eq!(
        app.screen().count("id:phone-header >> text:浏览").unwrap(),
        1
    );
    assert_eq!(
        app.screen()
            .count("id:phone-main-viewport >> text:浏览")
            .unwrap(),
        0,
        "Phone Browse must not duplicate `浏览` heading inside the page body"
    );
    touch_violations.extend(check_min_touch_targets(&app, &["phone-refresh-btn"]));

    // 3. Install fixture plugin with custom navigation, stations, shows, and WebSignIn auth.
    app.press("id:tab-settings").unwrap();
    app.wait_for("id:phone-settings-plugins").unwrap();
    app.press("id:phone-settings-plugins").unwrap();
    app.wait_for("id:phone-plugins-install-btn").unwrap();
    app.press("id:phone-plugins-install-btn").unwrap();
    app.wait_for("id:install-plugin-pick-folder").unwrap();

    app.on_pick_paths(vec![plugin_dir]).unwrap();
    app.press("id:install-plugin-pick-folder").unwrap();
    app.wait_for("确认安装插件").unwrap();
    app.press("role:button[确认]").unwrap();
    app.wait_for_gone("确认安装插件", GONE_TIMEOUT).unwrap();
    app.wait_for("Fixture Phone Auth").unwrap();

    // 4. End-to-end WebSignIn auto-close + immediate account update + refresh_sources error preservation.
    // Allow the Phone bottom sheet close animation to finish before tapping the plugin card underneath.
    std::thread::sleep(Duration::from_millis(350));
    app.press("role:button[登录]").unwrap();
    app.wait_for("id:web-sign-in-cookie-input").unwrap();
    app.wait_for("id:web-sign-in-submit-btn").unwrap();
    app.wait_for("SAPISID").unwrap();
    app.wait_for("__Secure-3PSID").unwrap();

    app.type_text(
        "id:web-sign-in-cookie-input",
        "SAPISID=aaa; __Secure-3PSID=bbb",
    )
    .unwrap();
    app.press("id:web-sign-in-submit-btn").unwrap();
    app.wait_for_gone("id:web-sign-in-submit-btn", GONE_TIMEOUT)
        .unwrap();
    app.wait_for("已登录：Verified Listener").unwrap();

    // Re-enter Plugins (triggers `store.refresh_sources()` whose plugin `account()` throws):
    // cached signed-in account must remain intact and never regress to unsigned state.
    app.press("id:phone-back-btn").unwrap();
    app.wait_for("id:phone-settings-plugins").unwrap();
    app.press("id:phone-settings-plugins").unwrap();
    app.wait_for("id:phone-settings-detail").unwrap();
    std::thread::sleep(Duration::from_millis(250));
    app.wait_for("已登录：Verified Listener").unwrap();

    // 5. Switch Phone active source to `Fixture Phone Auth` and verify single nav strip on Browse.
    app.press("id:phone-back-btn").unwrap();
    app.wait_for("id:tab-home").unwrap();
    app.press("id:tab-home").unwrap();
    app.wait_for("id:phone-source-switcher").unwrap();
    app.press("id:phone-source-switcher").unwrap();
    app.wait_for("切换音乐源").unwrap();
    app.press("Fixture Phone Auth").unwrap();
    app.wait_for_gone("切换音乐源", GONE_TIMEOUT).unwrap();
    app.wait_for("id:phone-main-viewport >> text:Harbor Echo")
        .unwrap();

    // On Phone Home: strictly 1 source switcher, zero duplicate heading, zero shell nav strip.
    assert_eq!(
        app.screen().count("text:Fixture Phone Auth").unwrap(),
        1,
        "Phone Home must show strictly one `Fixture Phone Auth` source switcher"
    );
    assert_eq!(
        app.screen()
            .count("id:phone-main-viewport >> text:发现")
            .unwrap(),
        0
    );
    assert_eq!(
        app.screen().count("text:Favorites").unwrap(),
        0,
        "Removed `phone_source_nav_strip` must not render `Favorites` on Phone Home"
    );

    // On Phone Browse: strictly a single horizontal pill bar (Radio, Podcasts, Favorites) with zero duplicates.
    app.press("id:tab-browse").unwrap();
    app.wait_for("id:phone-main-viewport >> text:Favorites")
        .unwrap();
    assert_eq!(
        app.screen()
            .count("id:phone-main-viewport >> text:浏览")
            .unwrap(),
        0
    );
    assert_eq!(
        app.screen().count("text:Favorites").unwrap(),
        1,
        "Phone Browse must show strictly one `Favorites` pill (no duplicate shell strip + page pills)"
    );
    assert_eq!(
        app.screen()
            .count("id:phone-main-viewport >> text:电台")
            .unwrap(),
        1,
        "Phone Browse should prepend capability-gated `电台` pill once"
    );
    assert_eq!(
        app.screen()
            .count("id:phone-main-viewport >> text:播客")
            .unwrap(),
        1,
        "Phone Browse should prepend capability-gated `播客` pill once"
    );
    touch_violations.extend(check_min_touch_targets(&app, &["phone-refresh-btn"]));

    // Level-2 Favorites / Radio / Podcasts routes show single header + refresh button >= 44pt.
    app.press("id:phone-main-viewport >> text:Favorites")
        .unwrap();
    app.wait_for("id:phone-back-btn").unwrap();
    app.wait_for("id:phone-refresh-btn").unwrap();
    app.wait_for("id:phone-main-viewport >> text:Favorite Waves")
        .unwrap();
    touch_violations.extend(check_min_touch_targets(
        &app,
        &["phone-back-btn", "phone-refresh-btn"],
    ));
    app.press("id:phone-back-btn").unwrap();
    app.wait_for("id:phone-main-viewport >> text:电台").unwrap();

    app.press("id:phone-main-viewport >> text:电台").unwrap();
    app.wait_for("id:phone-header >> text:电台").unwrap();
    app.wait_for("id:phone-main-viewport >> text:Synth FM")
        .unwrap();
    assert_eq!(
        app.screen()
            .count("id:phone-main-viewport >> role:header[电台]")
            .unwrap(),
        0,
        "Phone Radio page must not duplicate `电台` H1 heading in body"
    );
    assert_eq!(
        app.screen()
            .count("id:phone-main-viewport >> role:button[刷新]")
            .unwrap(),
        0,
        "Phone Radio page must not render a duplicate body Refresh button"
    );
    touch_violations.extend(check_min_touch_targets(&app, &["phone-refresh-btn"]));
    app.press("id:phone-back-btn").unwrap();
    app.wait_for("id:phone-main-viewport >> text:播客").unwrap();

    app.press("id:phone-main-viewport >> text:播客").unwrap();
    app.wait_for("id:phone-header >> text:播客").unwrap();
    app.wait_for("id:phone-main-viewport >> text:Sound Lab")
        .unwrap();
    assert_eq!(
        app.screen()
            .count("id:phone-main-viewport >> role:header[播客]")
            .unwrap(),
        0,
        "Phone Podcasts page must not duplicate `播客` H1 heading in body"
    );
    assert_eq!(
        app.screen()
            .count("id:phone-main-viewport >> role:button[刷新]")
            .unwrap(),
        0,
        "Phone Podcasts page must not render a duplicate body Refresh button"
    );
    touch_violations.extend(check_min_touch_targets(&app, &["phone-refresh-btn"]));

    assert!(
        touch_violations.is_empty(),
        "Phone single-header touch target violations:\n{}",
        touch_violations.join("\n")
    );
}

#[test]
fn phone_browse_tab_cache_and_idempotent_switch() {
    let mut guard = IsolatedStateGuard::acquire();
    let plugin_dir = guard.make_temp_dir("phone-browse-cache-plugin");
    write_fixture_phone_auth_plugin(&plugin_dir);

    let mut app = start_viewport(390.0, 844.0, Os::Ios, Idiom::Phone);
    app.wait_for("id:shell-phone").unwrap();
    app.wait_for("id:phone-tab-bar").unwrap();

    // 1. Empty local library: Home -> Browse -> repeated Browse -> Library -> Browse hits cache
    //    so `phone-refresh-btn` remains enabled (`browse_loading == false`) without skeleton flicker.
    app.press("id:tab-browse").unwrap();
    app.wait_for("id:phone-refresh-btn").unwrap();
    assert!(
        app.screen().enabled("id:phone-refresh-btn").unwrap(),
        "Home -> Browse must hit hydrated cache with browse_loading == false"
    );

    // Repeated switch_tab onto active Browse must be a no-op with browse_loading == false.
    app.press("id:tab-browse").unwrap();
    app.press("id:tab-browse").unwrap();
    assert!(
        app.screen().enabled("id:phone-refresh-btn").unwrap(),
        "repeated tap on active Browse tab must keep browse_loading == false"
    );

    // Home -> Browse -> Library -> Browse cache hit.
    app.press("id:tab-library").unwrap();
    app.wait_for("新建歌单").unwrap();
    app.press("id:tab-browse").unwrap();
    app.wait_for("id:phone-refresh-btn").unwrap();
    assert!(
        app.screen().enabled("id:phone-refresh-btn").unwrap(),
        "Browse -> Library -> Browse must hit cache with browse_loading == false"
    );

    // 2. Populated plugin source: install fixture plugin, switch source, and verify cached
    //    Home -> Browse -> Library -> Browse and repeated sub-pill navigation stay instant.
    app.press("id:tab-settings").unwrap();
    app.wait_for("id:phone-settings-plugins").unwrap();
    app.press("id:phone-settings-plugins").unwrap();
    app.wait_for("id:phone-plugins-install-btn").unwrap();
    app.press("id:phone-plugins-install-btn").unwrap();
    app.wait_for("id:install-plugin-pick-folder").unwrap();

    app.on_pick_paths(vec![plugin_dir]).unwrap();
    app.press("id:install-plugin-pick-folder").unwrap();
    app.wait_for("确认安装插件").unwrap();
    app.press("role:button[确认]").unwrap();
    app.wait_for_gone("确认安装插件", GONE_TIMEOUT).unwrap();
    app.wait_for("Fixture Phone Auth").unwrap();

    app.press("id:phone-back-btn").unwrap();
    app.wait_for("id:tab-home").unwrap();
    app.press("id:tab-home").unwrap();
    app.wait_for("id:phone-source-switcher").unwrap();
    app.press("id:phone-source-switcher").unwrap();
    app.wait_for("切换音乐源").unwrap();
    app.press("Fixture Phone Auth").unwrap();
    app.wait_for_gone("切换音乐源", GONE_TIMEOUT).unwrap();
    app.wait_for("id:phone-main-viewport >> text:Harbor Echo")
        .unwrap();

    // Home -> Browse uses cached root browse data immediately (`Harbor Echo` visible, not loading).
    app.press("id:tab-browse").unwrap();
    app.wait_for("id:phone-main-viewport >> text:Harbor Echo")
        .unwrap();
    assert!(
        app.screen().enabled("id:phone-refresh-btn").unwrap(),
        "plugin Home -> Browse must hit browse_cache with browse_loading == false"
    );

    // Repeated tap on Browse tab keeps content intact and browse_loading == false.
    app.press("id:tab-browse").unwrap();
    assert!(
        app.screen().enabled("id:phone-refresh-btn").unwrap(),
        "repeated Browse tap on plugin source must keep browse_loading == false"
    );
    assert!(
        app.screen()
            .matches("id:phone-main-viewport >> text:Harbor Echo")
            .unwrap()
    );

    // Browse -> Library -> Browse hits cache immediately.
    app.press("id:tab-library").unwrap();
    app.wait_for("id:phone-header >> text:资料库").unwrap();
    app.press("id:tab-browse").unwrap();
    app.wait_for("id:phone-main-viewport >> text:Harbor Echo")
        .unwrap();
    assert!(
        app.screen().enabled("id:phone-refresh-btn").unwrap(),
        "plugin Browse -> Library -> Browse must hit browse_cache with browse_loading == false"
    );
}
