use serde::{Deserialize, Serialize};

/// Supported UI locales.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Locale {
    #[default]
    #[serde(rename = "zh-CN", alias = "zh_CN", alias = "zh", alias = "zhCn")]
    ZhCn,
    #[serde(rename = "en", alias = "en-US", alias = "en_US", alias = "En")]
    En,
}

impl Locale {
    /// Returns the BCP-47 language code for this locale.
    pub fn code(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::En => "en",
        }
    }

    /// Returns the native display label for this locale.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::ZhCn => "简体中文",
            Self::En => "English",
        }
    }

    /// Parses a locale code case-insensitively with sensible fallback.
    pub fn from_code(code: &str) -> Self {
        let lower = code.trim().to_ascii_lowercase();
        if lower.starts_with("en") {
            Self::En
        } else {
            Self::ZhCn
        }
    }

    /// All supported locales in UI display order.
    pub const ALL: [Locale; 2] = [Locale::ZhCn, Locale::En];
}

macro_rules! define_translations {
    ($( $key:literal => ($zh:literal, $en:literal) ),* $(,)?) => {
        /// Every defined translation key in the catalog.
        pub const ALL_KEYS: &[&str] = &[
            $( $key, )*
        ];

        fn lookup(locale: Locale, key: &str) -> Option<&'static str> {
            match (locale, key) {
                $(
                    (Locale::ZhCn, $key) => Some($zh),
                    (Locale::En, $key) => Some($en),
                )*
                _ => None,
            }
        }
    };
}

define_translations! {
    // App & common actions
    "app.name" => ("NSH Music", "NSH Music"),
    "common.ok" => ("确定", "OK"),
    "common.cancel" => ("取消", "Cancel"),
    "common.save" => ("保存", "Save"),
    "common.delete" => ("删除", "Delete"),
    "common.rename" => ("重命名", "Rename"),
    "common.create" => ("创建", "Create"),
    "common.edit" => ("编辑", "Edit"),
    "common.close" => ("关闭", "Close"),
    "common.confirm" => ("确认", "Confirm"),
    "common.retry" => ("重试", "Retry"),
    "common.refresh" => ("刷新", "Refresh"),
    "common.loading" => ("加载中…", "Loading..."),
    "common.load_more" => ("加载更多", "Load More"),
    "common.loading_more" => ("正在加载更多…", "Loading more..."),
    "common.empty" => ("暂无内容", "Nothing here yet"),
    "common.more" => ("更多", "More"),
    "common.see_all" => ("查看全部", "See All"),
    "common.back" => ("返回", "Back"),
    "common.forward" => ("前进", "Forward"),
    "common.done" => ("完成", "Done"),
    "common.enabled" => ("已启用", "Enabled"),
    "common.disabled" => ("已停用", "Disabled"),
    "common.copy" => ("复制", "Copy"),
    "common.copied" => ("已复制", "Copied"),
    "common.open" => ("打开", "Open"),
    "common.add" => ("添加", "Add"),
    "common.remove" => ("移除", "Remove"),
    "common.unknown" => ("未知", "Unknown"),
    "common.error" => ("操作失败", "Action Failed"),
    "common.success" => ("已完成", "Done"),
    "common.save_changes" => ("保存更改", "Save changes"),
    "common.discard_changes" => ("放弃更改", "Discard changes"),

    // Account
    "account.my_page" => ("我的主页", "My Page"),

    // Sources & home empty states
    "source.local" => ("本地音乐", "Local Files"),
    "home.empty_title" => ("还没有音乐", "No Music Yet"),
    "home.empty_subtitle" => ("添加本地音乐文件夹，或安装音乐插件开始收听。", "Add a local music folder or install a plugin to start listening."),
    "home.add_folder" => ("添加音乐文件夹", "Add Music Folder"),
    "home.install_plugin" => ("安装插件", "Install Plugin"),

    // Navigation
    "nav.home" => ("发现", "Discover"),
    "nav.discover" => ("发现", "Discover"),
    "nav.browse" => ("浏览", "Browse"),
    "nav.search" => ("搜索", "Search"),
    "nav.library" => ("资料库", "Library"),
    "nav.radio" => ("电台", "Radio"),
    "nav.stations" => ("电台", "Radio"),
    "nav.podcasts" => ("播客", "Podcasts"),
    "nav.radio_podcasts" => ("电台播客", "Radio & Podcasts"),
    "nav.playlists" => ("歌单", "Playlists"),
    "nav.my_playlists" => ("我的歌单", "My Playlists"),
    "nav.saved_playlists" => ("收藏的歌单", "Saved Playlists"),
    "nav.liked" => ("我喜欢的音乐", "Liked Songs"),
    "nav.albums" => ("专辑", "Albums"),
    "nav.artists" => ("艺人", "Artists"),
    "nav.recent" => ("最近播放", "Recently Played"),
    "nav.local" => ("本地音乐", "Local Files"),
    "nav.settings" => ("设置", "Settings"),
    "nav.plugins" => ("插件", "Plugins"),
    "nav.sources" => ("音乐源", "Sources"),
    "nav.my_page" => ("我的主页", "My Page"),
    "nav.sign_in" => ("登录", "Sign In"),
    "nav.sign_out" => ("退出登录", "Sign Out"),
    "nav.collapse_sidebar" => ("收起侧边栏", "Collapse Sidebar"),
    "nav.expand_sidebar" => ("展开侧边栏", "Expand Sidebar"),

    // Playback controls & queue
    "player.play" => ("播放", "Play"),
    "player.pause" => ("暂停", "Pause"),
    "player.next" => ("下一曲", "Next"),
    "player.prev" => ("上一曲", "Previous"),
    "player.stop" => ("停止", "Stop"),
    "player.shuffle" => ("随机播放", "Shuffle"),
    "player.shuffle_on" => ("随机播放已开启", "Shuffle On"),
    "player.shuffle_off" => ("随机播放已关闭", "Shuffle Off"),
    "player.repeat" => ("循环模式", "Repeat"),
    "player.repeat_off" => ("顺序播放", "Repeat Off"),
    "player.repeat_all" => ("列表循环", "Repeat All"),
    "player.repeat_one" => ("单曲循环", "Repeat One"),
    "player.volume" => ("音量", "Volume"),
    "player.mute" => ("静音", "Mute"),
    "player.unmute" => ("取消静音", "Unmute"),
    "player.like" => ("喜欢", "Like"),
    "player.unlike" => ("取消喜欢", "Unlike"),
    "player.liked" => ("已喜欢", "Liked"),
    "player.queue" => ("播放队列", "Queue"),
    "player.queue_empty" => ("播放队列为空", "Queue is empty"),
    "player.queue_clear" => ("清空队列", "Clear Queue"),
    "player.add_to_queue" => ("添加到队列", "Add to Queue"),
    "player.play_next" => ("下一首播放", "Play Next"),
    "player.play_all" => ("播放全部", "Play All"),
    "player.shuffle_all" => ("随机播放全部", "Shuffle All"),
    "player.up_next" => ("即将播放", "Up Next"),
    "player.now_playing" => ("正在播放", "Now Playing"),
    "player.idle_title" => ("未在播放", "Not Playing"),
    "player.nothing_playing" => ("未在播放", "Not Playing"),
    "player.unavailable" => ("曲目不可用", "Track Unavailable"),
    "player.buffering" => ("缓冲中…", "Buffering..."),
    "player.quality" => ("音质", "Quality"),
    "player.quality.standard" => ("标准", "Standard"),
    "player.quality.high" => ("极高", "High"),
    "player.quality.lossless" => ("无损", "Lossless"),
    "player.lossless" => ("无损", "Lossless"),
    "player.devices" => ("播放设备", "Playback Devices"),
    "player.remote_device" => ("远程设备", "Remote Device"),
    "player.local_device" => ("本机播放", "This Device"),

    // Now Playing modes, lyric stages & side panels
    "now_playing.title" => ("正在播放", "Now Playing"),
    "now_playing.mode.cover" => ("封面", "Cover"),
    "now_playing.mode.vinyl" => ("黑胶唱机", "Vinyl"),
    "now_playing.mode.cassette" => ("磁带机", "Cassette"),
    "now_playing.stage" => ("歌词舞台", "Lyric Stage"),
    "now_playing.stage.classic" => ("经典", "Classic"),
    "now_playing.stage.flow" => ("流光", "Flow"),
    "now_playing.stage.steps" => ("阶梯", "Steps"),
    "now_playing.stage.tilt" => ("倾斜", "Tilt"),
    "now_playing.stage.wall" => ("弹幕墙", "Wall"),
    "now_playing.panel.lyrics" => ("歌词", "Lyrics"),
    "now_playing.panel.stage" => ("舞台", "Stage"),
    "now_playing.panel.queue" => ("队列", "Queue"),
    "now_playing.panel.wall" => ("封面墙", "Wall"),
    "now_playing.panel.comments" => ("评论", "Comments"),
    "now_playing.panel.podcast" => ("节目详情", "Episode"),
    "now_playing.top_comment" => ("热评", "Top Comment"),
    "now_playing.stage_style" => ("舞台样式", "Stage Style"),
    "now_playing.player_style" => ("播放样式", "Player Style"),
    "now_playing.sparks" => ("歌词星火", "Sparks"),
    "now_playing.lyrics.empty" => ("暂无歌词", "No lyrics available"),
    "now_playing.lyrics.enjoy" => ("暂无歌词，静心聆听。", "No lyrics. Enjoy the music."),
    "now_playing.lyrics.unsynced" => ("纯文本歌词", "Unsynced lyrics"),
    "now_playing.lyrics.translation" => ("翻译", "Translation"),
    "now_playing.lyrics.romaji" => ("音译", "Romaji"),
    "now_playing.comments.empty" => ("暂无评论", "No comments yet"),
    "now_playing.comments.all_loaded" => ("已加载全部评论", "That's all the comments"),
    "now_playing.comments.placeholder" => ("写下此刻的感受…", "Share your thoughts..."),
    "now_playing.comments.post" => ("发送", "Post"),
    "now_playing.comments.count" => ("{count} 条评论", "{count} comments"),
    "now_playing.expand" => ("展开播放页", "Expand Now Playing"),
    "now_playing.minimize" => ("收起播放页", "Minimize Now Playing"),
    "now_playing.view_comments" => ("查看评论", "View Comments"),
    "now_playing.go_to_artist" => ("查看艺人", "Go to Artist"),
    "now_playing.go_to_album" => ("查看专辑", "Go to Album"),

    // Browse, search, library, podcasts & stations
    "browse.featured" => ("精选推荐", "Featured"),
    "browse.recommended" => ("推荐歌单", "Recommended Playlists"),
    "browse.new_releases" => ("专辑", "Albums"),
    "browse.charts" => ("热歌榜单", "Charts"),
    "browse.genres" => ("流派与心境", "Genres & Moods"),
    "browse.empty" => ("暂无内容", "Nothing here yet"),
    "search.placeholder" => ("搜索歌曲、专辑、艺人或歌单…", "Search tracks, albums, artists, playlists..."),
    "search.recent" => ("最近搜索", "Recent Searches"),
    "search.clear_recent" => ("清除历史", "Clear History"),
    "search.suggestions" => ("搜索建议", "Suggestions"),
    "search.no_results" => ("未找到匹配结果", "No matching results"),
    "search.tab.tracks" => ("单曲", "Tracks"),
    "search.tab.albums" => ("专辑", "Albums"),
    "search.tab.artists" => ("艺人", "Artists"),
    "search.tab.playlists" => ("歌单", "Playlists"),
    "search.tab.shows" => ("播客", "Podcasts"),
    "track.unknown_artist" => ("未知艺术家", "Unknown Artist"),
    "track.count_fmt" => ("{count} 首歌曲", "{count} tracks"),
    "track.copy_title" => ("复制歌名", "Copy Title"),
    "track.copy_title_artists" => ("复制歌名和歌手", "Copy Title and Artists"),
    "track.copy_link" => ("复制链接", "Copy Link"),
    "track.download" => ("下载…", "Download..."),
    "track.downloading" => ("正在下载…", "Downloading..."),
    "track.downloaded" => ("已下载 {name}", "Downloaded {name}"),
    "track.download_failed" => ("下载失败，请稍后再试", "Couldn't download this song"),
    "track.go_to_podcast" => ("前往播客", "Go to Podcast"),
    "library.tracks_count" => ("{count} 首曲目", "{count} tracks"),
    "library.episodes_count" => ("{count} 期节目", "{count} episodes"),
    "library.albums_count" => ("{count} 张专辑", "{count} albums"),
    "library.create_playlist" => ("新建歌单", "New Playlist"),
    "library.rename_playlist" => ("重命名歌单", "Rename Playlist"),
    "library.delete_playlist" => ("删除歌单", "Delete Playlist"),
    "library.add_to_playlist" => ("添加到歌单", "Add to Playlist"),
    "library.remove_from_playlist" => ("从歌单移除", "Remove from Playlist"),
    "playlist.rename" => ("重命名歌单", "Rename Playlist"),
    "playlist.delete" => ("删除歌单", "Delete Playlist"),
    "playlist.delete_confirm_title" => ("删除歌单「{name}」？", "Delete playlist \"{name}\"?"),
    "playlist.delete_confirm_desc" => ("此操作无法撤销。", "This action cannot be undone."),
    "playlist.remove_track" => ("从歌单移除", "Remove from Playlist"),
    "playlist.move_up" => ("上移", "Move Up"),
    "playlist.move_down" => ("下移", "Move Down"),
    "playlist.add_to" => ("添加到歌单", "Add to Playlist"),
    "playlist.new_and_add" => ("新建歌单…", "New Playlist..."),
    "library.save_album" => ("收藏专辑", "Save Album"),
    "library.unsave_album" => ("已收藏专辑", "Saved Album"),
    "library.save_playlist" => ("收藏歌单", "Save Playlist"),
    "library.unsave_playlist" => ("已收藏歌单", "Saved Playlist"),
    "library.follow_artist" => ("关注艺人", "Follow"),
    "library.following_artist" => ("已关注", "Following"),
    "library.scan_folders" => ("扫描文件夹", "Scan Folders"),
    "library.add_folder" => ("添加文件夹…", "Add Folder..."),
    "library.rescan" => ("重新扫描", "Rescan"),
    "library.local_empty" => ("尚未扫描到本地音频文件", "No local audio files found"),
    "podcast.subscribe" => ("订阅播客", "Subscribe"),
    "podcast.subscribed" => ("已订阅", "Subscribed"),
    "podcast.episodes" => ("全部节目", "Episodes"),
    "podcast.description" => ("节目简介", "About"),
    "podcast.published" => ("发布时间", "Published"),
    "station.tune_in" => ("收听电台", "Tune In"),
    "station.live_mix" => ("电台", "Radio"),
    "station.refresh_mix" => ("换一批", "Next Mix"),
    "track.column.title" => ("标题", "Title"),
    "track.column.artist" => ("艺人", "Artist"),
    "track.column.album" => ("专辑", "Album"),
    "track.column.duration" => ("时长", "Duration"),

    // Settings
    "settings.title" => ("设置", "Settings"),
    "settings.section.general" => ("通用", "General"),
    "settings.section.appearance" => ("外观与界面", "Appearance"),
    "settings.section.audio" => ("播放", "Playback"),
    "settings.section.playback" => ("播放", "Playback"),
    "settings.section.library" => ("本地音乐", "Local Files"),
    "settings.section.storage" => ("缓存", "Cache"),
    "settings.section.cache" => ("缓存", "Cache"),
    "settings.section.experiments" => ("实验性功能", "Experiments"),
    "settings.section.plugins" => ("插件", "Plugins"),
    "settings.section.about" => ("关于", "About"),
    "settings.plugins_count" => ("已安装 {count} 个插件", "{count} installed"),
    "settings.mobile_hint" => ("在设备上管理通用设置、插件与缓存", "Manage general preferences, plugins, and cache"),
    "settings.theme" => ("外观", "Theme"),
    "settings.theme.system" => ("跟随系统", "System"),
    "settings.theme.light" => ("浅色", "Light"),
    "settings.theme.dark" => ("深色", "Dark"),
    "settings.locale" => ("语言", "Language"),
    "settings.locale.zh_cn" => ("简体中文", "简体中文"),
    "settings.locale.en" => ("English", "English"),
    "settings.quality" => ("音质偏好", "Audio Quality"),
    "settings.replay_gain" => ("响度均衡 (ReplayGain)", "ReplayGain"),
    "settings.replay_gain.desc" => ("平滑不同曲目间的音量差异", "Normalize loudness across tracks"),
    "settings.crossfade" => ("交叉淡入淡出", "Crossfade"),
    "settings.crossfade.off" => ("关闭", "Off"),
    "settings.cache_limit" => ("缓存大小上限", "Cache Limit"),
    "settings.cache.used" => ("当前已用 {size}", "{size} used"),
    "settings.clear_cache" => ("清除缓存", "Clear Cache"),
    "settings.cache.cleared" => ("已清除音频缓存", "Audio cache cleared"),
    "settings.default_player_style" => ("播放页样式", "Player View Style"),
    "settings.default_stage_style" => ("歌词舞台样式", "Lyric Stage Style"),
    "settings.exp.hot_comments" => ("间奏热评浮现", "Quiet-Part Top Comments"),
    "settings.exp.hot_comments.desc" => ("间奏或前奏时在歌词区域显示高赞评论", "Show top comments during long instrumental passages"),
    "settings.stage_sparks" => ("歌词星火动效", "Stage Sparks"),
    "settings.stage_sparks_desc" => ("演唱时在舞台歌词上触发微光星火", "Throw sparks off each word on the stage as it is sung"),
    "settings.search_placeholder" => ("搜索设置…", "Search settings..."),
    "settings.exp.fog_backdrop" => ("流体雾感背景", "Ambient Fog Backdrop"),
    "settings.exp.fog_backdrop.desc" => ("播放页使用实时着色器流体光影", "Use GPU ambient fog shader behind Now Playing"),
    "settings.exp.smoke_progress" => ("烟雾光晕进度条", "Smoke Progress Bar"),
    "settings.exp.smoke_progress.desc" => ("进度条播放头附带柔和流光效果", "Soft luminous tail on the playback scrubber"),
    "settings.exp.grow_from_cover" => ("封面展开动效", "Grow from Cover"),
    "settings.exp.grow_from_cover.desc" => ("打开播放页时从封面位置弹性展开", "Spring-expand Now Playing from the cover artwork"),
    "settings.local_folders" => ("本地音乐文件夹", "Local Music Folders"),
    "settings.local_folders.empty" => ("尚未添加文件夹", "No folders added"),

    // Plugin manager
    "plugins.title" => ("插件", "Plugins"),
    "plugins.install" => ("安装插件", "Install Plugin"),
    "plugins.from_github" => ("从 GitHub 安装", "Install from GitHub"),
    "plugins.from_folder" => ("导入本地文件夹", "Import Folder"),
    "plugins.from_zip" => ("导入 ZIP", "Import ZIP"),
    "plugins.open_folder" => ("打开插件目录", "Open Plugins Folder"),
    "plugins.show_folder" => ("显示目录", "Show Folder"),
    "plugins.check_update" => ("检查更新", "Check for Updates"),
    "plugins.up_to_date" => ("已是最新版本", "Up to date"),
    "plugins.update_available" => ("发现新版本 v{version}", "Update available: v{version}"),
    "plugins.allow_programs" => ("允许运行外部程序", "Allow Programs"),
    "plugins.stop_allowing" => ("停止允许", "Stop Allowing"),
    "plugins.programs_warning" => ("该插件请求运行外部程序：{commands}", "This plugin requests permission to run external programs: {commands}"),
    "plugins.uninstall" => ("移除", "Remove"),
    "plugins.reload" => ("重载", "Reload"),
    "plugins.configure" => ("设置", "Settings"),
    "plugins.enable" => ("启用", "Enable"),
    "plugins.disable" => ("停用", "Disable"),
    "plugins.empty" => ("尚未安装插件", "No plugins installed"),
    "plugins.empty_title" => ("尚未安装插件", "No Plugins Installed"),
    "plugins.empty_subtitle" => ("可通过 GitHub 仓库、本地目录、ZIP 压缩包导入插件，或直接将插件拖放到窗口。", "Install from GitHub, a local folder, a ZIP archive, or drag and drop onto the window."),
    "plugins.source_local" => ("本地目录", "Local Folder"),
    "plugins.source_zip" => ("ZIP 压缩包", "ZIP Archive"),
    "plugins.source_github" => ("GitHub", "GitHub"),
    "plugins.install_placeholder" => ("输入 owner/repo、owner/repo@分支 或 GitHub 链接", "Enter owner/repo, owner/repo@ref, or GitHub URL"),
    "plugins.version" => ("版本", "Version"),
    "plugins.author" => ("作者", "Author"),
    "plugins.sign_in" => ("登录", "Sign In"),
    "plugins.sign_out" => ("退出登录", "Sign Out"),
    "plugins.signed_in_as" => ("已登录：{name}", "Signed in as {name}"),
    "plugins.not_signed_in" => ("未登录", "Not signed in"),
    "plugins.permissions" => ("权限声明", "Permissions"),
    "plugins.network_hosts" => ("网络访问", "Network"),
    "plugins.process_commands" => ("外部程序", "External Programs"),
    "plugins.confirm_install.title" => ("确认安装插件", "Confirm Plugin Installation"),
    "plugins.confirm_install.permissions" => ("请求的权限", "Requested Permissions"),
    "plugins.confirm_install.network" => ("网络访问", "Network"),
    "plugins.confirm_install.process" => ("外部程序", "External Programs"),
    "plugins.confirm_install.none" => ("无需特殊权限", "No special permissions required"),
    "plugins.confirm_remove.title" => ("移除插件「{name}」？", "Remove plugin \"{name}\"?"),
    "plugins.confirm_remove.desc" => ("将删除该插件目录，已收藏的其它音乐源不受影响。", "This deletes the plugin directory from disk."),
    "plugins.installed_fmt" => ("已安装「{name}」", "Installed \"{name}\""),
    "plugins.removed_fmt" => ("已移除插件「{name}」", "Removed \"{name}\""),
    "plugins.reloaded_fmt" => ("已重载「{name}」", "Reloaded \"{name}\""),
    "plugins.scan_done_fmt" => ("扫描完成：共 {count} 首本地歌曲", "Scan complete: {count} local tracks"),
    "plugins.drop_hint" => ("松开以导入插件或音乐文件夹", "Drop to import plugin or music folder"),
    "plugins.compat_mobile_virtualized" => ("移动端虚拟化兼容", "Mobile Virtualized"),
    "plugins.mobile_virtual_compat" => ("移动端虚拟化兼容", "Mobile Virtualization Compatibility"),
    "plugins.mobile_virtual_compat.desc" => ("开启后内置虚拟化兼容执行 ffmpeg / yt-dlp 等命令；关闭后直接通过系统 PATH 调用外部命令，失败时将在对应插件卡片显示错误。", "Intercept ffmpeg / yt-dlp in-process on mobile and zero-dependency setups. When off, invoke external commands directly via system PATH and surface errors on the plugin card."),
    "plugins.runtime_error_title" => ("命令调用失败", "Command Invocation Failed"),
    "plugins.web_sign_in_hint" => ("请在下方网页中完成账号登录，登录成功后将自动获取会话。", "Sign in below to authorize this plugin. Your session will be captured automatically."),
    "plugins.web_sign_in_manual_cookie" => ("无法自动获取 Cookie？可在此手动粘贴：", "Can't capture cookies automatically? Paste Cookie header manually:"),
    "plugins.web_sign_in_submit_cookie" => ("提交 Cookie", "Submit Cookie"),

    // Desktop application menu & tray
    "menu.app" => ("NSH Music", "NSH Music"),
    "menu.about" => ("关于 NSH Music", "About NSH Music"),
    "menu.settings" => ("设置…", "Settings..."),
    "menu.quit" => ("退出 NSH Music", "Quit NSH Music"),
    "menu.file" => ("文件", "File"),
    "menu.import_folder" => ("导入本地音乐文件夹…", "Import Music Folder..."),
    "menu.install_plugin" => ("安装插件…", "Install Plugin..."),
    "menu.playback" => ("播放", "Playback"),
    "menu.window" => ("窗口", "Window"),
    "tray.show_window" => ("显示主窗口", "Show Window"),

    // Compatibility badges
    "compat.full_native" => ("跨平台原生", "Native"),
    "compat.virtual_ffmpeg" => ("内置音视频处理", "Bundled FFmpeg"),
    "compat.desktop_limited" => ("需要桌面外部程序", "External Program"),
    "compat.partial" => ("部分功能受限", "Partial"),
    "compat.migration_required" => ("需要调整", "Needs Permission"),

    // Dialogs & modals
    "dialog.install_plugin.title" => ("安装插件", "Install Plugin"),
    "dialog.install_plugin.desc" => ("输入 GitHub 仓库地址，或选择本地插件目录 / ZIP 压缩包。", "Enter a GitHub repository, or choose a local folder / ZIP archive."),
    "dialog.plugin_config.title" => ("插件设置", "Plugin Settings"),
    "dialog.create_playlist.title" => ("新建歌单", "Create Playlist"),
    "dialog.create_playlist.placeholder" => ("输入歌单名称", "Enter playlist name"),
    "dialog.rename_playlist.title" => ("重命名歌单", "Rename Playlist"),
    "dialog.delete_playlist.title" => ("删除歌单", "Delete Playlist"),
    "dialog.delete_playlist.confirm" => ("确定要删除歌单「{name}」吗？此操作无法撤销。", "Delete playlist \"{name}\"? This cannot be undone."),
    "dialog.qr_sign_in.title" => ("扫码登录", "Scan QR Code to Sign In"),
    "dialog.qr_sign_in.hint" => ("使用对应客户端扫描二维码", "Scan this QR code to sign in"),
    "dialog.web_sign_in.title" => ("网页登录", "Web Sign In"),
    "dialog.web_sign_in.cookie_fallback" => ("手动粘贴 Cookie", "Paste Cookie Manually"),
    "dialog.source_switcher.title" => ("切换音乐源", "Switch Music Source"),
    "dialog.sign_in_required.title" => ("需要登录", "Sign In Required"),
    "dialog.setup_required.title" => ("需要配置", "Setup Required"),
}

/// Zero-allocation localization lookup table for `nsh-music`.
pub struct I18n;

impl I18n {
    /// Looks up a localized string for `locale` and `key`.
    /// Falls back to returning `key` verbatim if the key is unknown.
    pub fn t(locale: Locale, key: &str) -> &str {
        lookup(locale, key).unwrap_or(key)
    }

    /// Formats a localized template string replacing `{name}` placeholders.
    pub fn t_fmt(locale: Locale, key: &str, args: &[(&str, &str)]) -> String {
        let mut out = Self::t(locale, key).to_owned();
        for (name, value) in args {
            let placeholder = format!("{{{name}}}");
            out = out.replace(&placeholder, value);
        }
        out
    }

    /// Returns all registered i18n keys.
    pub fn all_keys() -> &'static [&'static str] {
        ALL_KEYS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_keys_have_non_empty_zh_and_en_translations() {
        assert!(!I18n::all_keys().is_empty());
        for &key in I18n::all_keys() {
            let zh = lookup(Locale::ZhCn, key).expect("missing zh-CN translation");
            let en = lookup(Locale::En, key).expect("missing en translation");
            assert!(!zh.trim().is_empty(), "empty zh-CN value for {key}");
            assert!(!en.trim().is_empty(), "empty en value for {key}");
        }
    }

    #[test]
    fn unknown_key_falls_back_to_key() {
        assert_eq!(I18n::t(Locale::ZhCn, "custom.missing"), "custom.missing");
        assert_eq!(I18n::t(Locale::En, "custom.missing"), "custom.missing");
    }

    #[test]
    fn formatting_replaces_placeholders() {
        assert_eq!(
            I18n::t_fmt(Locale::ZhCn, "library.tracks_count", &[("count", "12")]),
            "12 首曲目"
        );
        assert_eq!(
            I18n::t_fmt(Locale::En, "library.tracks_count", &[("count", "12")]),
            "12 tracks"
        );
    }

    #[test]
    fn locale_code_and_serde_roundtrip() {
        assert_eq!(Locale::from_code("en-US"), Locale::En);
        assert_eq!(Locale::from_code("zh-CN"), Locale::ZhCn);
        assert_eq!(Locale::ZhCn.code(), "zh-CN");
        assert_eq!(Locale::En.code(), "en");

        let json = serde_json::to_string(&Locale::ZhCn).unwrap();
        assert_eq!(json, "\"zh-CN\"");
        let back: Locale = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Locale::ZhCn);
    }
}
