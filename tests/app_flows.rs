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
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("dir");
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
            "permissions": {
                "network": ["api.fixture.invalid"]
            }
        }"#,
    )
    .expect("write fixture plugin.json");

    fs::write(
        dir.join("index.js"),
        r#"
export async function getHome() {
    return {
        title: "Fixture Home",
        sections: [
            {
                title: "Fixture Picks",
                tracks: [
                    {
                        id: "fw-1",
                        name: "Signal Drift",
                        artists: [{ id: "ar-1", name: "Wave Runner" }],
                        album: { id: "al-1", name: "Test Signals" },
                        duration: 180000
                    }
                ]
            }
        ]
    };
}

export async function search(query) {
    return {
        tracks: [
            {
                id: "fw-1",
                name: "Signal Drift",
                artists: [{ id: "ar-1", name: "Wave Runner" }],
                album: { id: "al-1", name: "Test Signals" },
                duration: 180000
            }
        ],
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

fn start_viewport(width: f32, height: f32, os: Os, idiom: Idiom) -> Headless {
    let viewport = Size::new(Points(width), Points(height));
    let root = Root::of::<nsh_app_music::App>()
        .tokens(nsh_app_music::app_tokens())
        .app_id("com.southorange.nshmusic.test");
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

    // Confirm zero leftover mock/demo strings are displayed on boot.
    for forbidden in [
        "Harbor Lights",
        "Local & Demo",
        "午夜霓虹漫游",
        "霓虹港湾",
        "内置试听",
        "早上好",
    ] {
        assert!(
            !app.screen().shows(forbidden),
            "unexpected mock/demo text `{forbidden}` visible on clean boot"
        );
    }

    // 2. Navigate to Settings: test search filter, appearance theme toggle,
    // experiments, cache section, and live zh-CN <-> en locale switch.
    app.press("id:nav-settings").unwrap();
    app.wait_for("id:settings-search-input").unwrap();
    app.wait_for("外观与界面").unwrap();
    app.wait_for("实验性功能").unwrap();
    app.wait_for("缓存").unwrap();

    // Theme toggles: Light -> Dark -> System.
    app.press("role:button[浅色]").unwrap();
    assert!(app.screen().selected("role:button[浅色]").unwrap());
    app.press("role:button[深色]").unwrap();
    assert!(app.screen().selected("role:button[深色]").unwrap());
    app.press("role:button[跟随系统]").unwrap();
    assert!(app.screen().selected("role:button[跟随系统]").unwrap());

    // Live locale switch: zh-CN -> en -> zh-CN.
    app.press("role:button[English]").unwrap();
    app.wait_for("Appearance").unwrap();
    app.wait_for("Experiments").unwrap();
    app.wait_for("Ambient Fog Backdrop").unwrap();
    app.wait_for("id:nav-home >> text:Discover").unwrap();

    // Filter Settings via live search input.
    app.type_text("id:settings-search-input", "Fog").unwrap();
    app.wait_for("Ambient Fog Backdrop").unwrap();
    app.clear("id:settings-search-input").unwrap();
    app.wait_for("Appearance").unwrap();

    // Switch back to zh-CN.
    app.press("role:button[简体中文]").unwrap();
    app.wait_for("外观与界面").unwrap();
    app.wait_for("id:nav-home >> text:发现").unwrap();
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
    app.wait_for("id:now-playing-overlay >> text:播放队列")
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

    // 1. Navigate to Plugins screen and verify import toolbar buttons.
    app.press("id:nav-plugins").unwrap();
    app.wait_for("id:plugins-btn-github").unwrap();
    app.wait_for("id:plugins-btn-folder").unwrap();
    app.wait_for("id:plugins-btn-zip").unwrap();
    app.wait_for("id:plugins-btn-open-dir").unwrap();

    // 2. Open ActiveModal::InstallPlugin and assert non-0px modal child layout bounds.
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

    // 4. Confirm installation -> plugin appears in Plugins list and Source Switcher!
    app.press("id:confirm-plugin-install-btn").unwrap();
    app.wait_for_gone("id:confirm-plugin-install-btn", GONE_TIMEOUT)
        .unwrap();
    app.wait_for("Fixture Wave Music").unwrap();

    app.press("id:desktop-source-switcher").unwrap();
    app.wait_for("切换音乐源").unwrap();
    app.wait_for("Fixture Wave Music").unwrap();

    // Select installed plugin as active source and verify its home feed loads.
    app.press("role:button[/Fixture Wave Music/]").unwrap();
    app.press("id:nav-home").unwrap();
    app.wait_for("Signal Drift").unwrap();
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
            "pad-nav-plugins",
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

    // Navigate Pad destinations & verify Back/Forward history controls.
    app.press("id:pad-nav-library").unwrap();
    app.wait_for("新建歌单").unwrap();
    app.press("id:pad-nav-settings").unwrap();
    app.wait_for("外观与界面").unwrap();
    app.press("id:pad-back-btn").unwrap();
    app.wait_for("新建歌单").unwrap();
    app.press("id:pad-forward-btn").unwrap();
    app.wait_for("外观与界面").unwrap();

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

    // 1. Phone shell boots cleanly in zero-mock empty state.
    app.wait_for("id:shell-phone").unwrap();
    app.wait_for("id:phone-header").unwrap();
    app.wait_for("id:phone-tab-bar").unwrap();
    app.wait_for("还没有音乐").unwrap();

    // >= 44pt touch target sanity across always-visible Phone shell controls.
    let mut touch_violations = check_min_touch_targets(
        &app,
        &[
            "phone-source-switcher",
            "phone-search-btn",
            "phone-settings-btn",
            "tab-home",
            "tab-browse",
            "tab-library",
            "tab-settings",
        ],
    );

    // 2. Open Source Switcher bottom sheet and dismiss cleanly via Phone back-press.
    app.press("id:phone-source-switcher").unwrap();
    app.wait_for("切换音乐源").unwrap();
    app.back().unwrap();
    app.wait_for_gone("切换音乐源", GONE_TIMEOUT).unwrap();

    // 3. Scan local WAV folder, play a track -> floating mini-player dock appears.
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

    // 4. Open Now Playing sheet from mini-player, verify capability gating & modes, dismiss via back().
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

    // 5. Navigate tabs and confirm system back-press pops navigation history.
    app.press("id:tab-library").unwrap();
    app.wait_for("新建歌单").unwrap();
    app.press("id:tab-settings").unwrap();
    app.wait_for("外观与界面").unwrap();
    app.back().unwrap();
    app.wait_for("新建歌单").unwrap();

    assert!(
        touch_violations.is_empty(),
        "Phone touch target violations:\n{}",
        touch_violations.join("\n")
    );
}
