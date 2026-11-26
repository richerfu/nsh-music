use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{
    ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HOST, HeaderMap, HeaderName,
    HeaderValue, LOCATION, RANGE,
};
use reqwest::{Client, Method, Response};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use url::Url;

use crate::{AudioError, AudioResult};

/// Default maximum disk cache size: 512 MiB.
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 512 * 1024 * 1024;
/// Default streaming chunk size: 64 KiB.
pub const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;
/// Maximum HTTP redirect hops followed manually.
pub const MAX_REDIRECTS: usize = 10;

/// Returns a dedicated multi-threaded Tokio runtime handle for background audio I/O
/// and virtual relay servers. Using a dedicated runtime avoids deadlocks when sync
/// audio APIs are called from a single-threaded Tokio runtime (e.g. `#[tokio::test]`)
/// or from UI threads without an active Tokio context.
pub(crate) fn io_runtime_handle() -> tokio::runtime::Handle {
    static AUDIO_IO_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    AUDIO_IO_RT
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("nsh-audio-io")
                .build()
                .expect("failed to initialize nsh-music-audio I/O runtime")
        })
        .handle()
        .clone()
}

/// Configuration for [`AudioCache`].
#[derive(Debug, Clone)]
pub struct StreamCacheConfig {
    pub cache_dir: PathBuf,
    pub max_bytes: u64,
    pub chunk_size: usize,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

impl Default for StreamCacheConfig {
    fn default() -> Self {
        let base = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("nsh-music")
            .join("audio-cache");
        Self {
            cache_dir: base,
            max_bytes: DEFAULT_CACHE_MAX_BYTES,
            chunk_size: DEFAULT_CHUNK_SIZE,
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(120),
        }
    }
}

/// Result of an HTTP request sent with manual redirect traversal preserving headers.
pub struct RedirectedResponse {
    pub final_url: Url,
    pub redirect_count: usize,
    pub status: reqwest::StatusCode,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub total_size: Option<u64>,
    pub accepts_ranges: bool,
    pub response: Response,
}

/// Builds a `reqwest::Client` configured with `Policy::none()` so redirects are handled
/// manually while preserving custom plugin `requestHeaders` across cross-host CDNs.
pub fn build_no_redirect_client(connect_timeout: Duration) -> AudioResult<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(connect_timeout)
        .pool_idle_timeout(Some(Duration::from_secs(30)))
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .tcp_nodelay(true)
        .build()
        .map_err(|e| AudioError::Http(format!("failed to build HTTP client: {e}")))
}

/// Builds a `reqwest::Client` for local loopback streams (`127.0.0.1`, `localhost`, `::1`)
/// with `Policy::none()` and `.no_proxy()` so local relays are never intercepted by OS
/// system proxy settings.
pub fn build_loopback_no_redirect_client(connect_timeout: Duration) -> AudioResult<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(connect_timeout)
        .pool_idle_timeout(Some(Duration::from_secs(30)))
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .tcp_nodelay(true)
        .build()
        .map_err(|e| AudioError::Http(format!("failed to build loopback HTTP client: {e}")))
}

/// Sends an HTTP request and manually follows HTTP 3xx redirects up to `max_redirects`
/// while preserving all custom `headers` (e.g. `Referer`, `Cookie`, `User-Agent`, `Origin`,
/// `Authorization`) across cross-host CDN redirects.
///
/// The `Host` header (if supplied) is stripped on redirect hops so the HTTP client sets
/// the correct authority for each redirect target.
pub async fn send_with_preserved_headers(
    client: &Client,
    method: Method,
    url: &str,
    headers: &[(String, String)],
    range: Option<&str>,
    max_redirects: usize,
) -> AudioResult<RedirectedResponse> {
    let mut current_url = Url::parse(url)
        .map_err(|e| AudioError::InvalidInput(format!("invalid URL '{url}': {e}")))?;
    let mut current_method = method;
    let mut redirect_count = 0usize;

    loop {
        let mut header_map = HeaderMap::new();
        for (name, value) in headers {
            let trimmed_name = name.trim();
            if trimmed_name.is_empty() {
                continue;
            }
            if redirect_count > 0 && trimmed_name.eq_ignore_ascii_case(HOST.as_str()) {
                continue;
            }
            if let (Ok(hn), Ok(hv)) = (
                HeaderName::from_bytes(trimmed_name.as_bytes()),
                HeaderValue::from_str(value.trim()),
            ) {
                header_map.insert(hn, hv);
            }
        }
        if let Some(range_val) = range
            && let Ok(hv) = HeaderValue::from_str(range_val)
        {
            header_map.insert(RANGE, hv);
        }

        let idempotent = matches!(current_method, Method::GET | Method::HEAD);
        let response = match client
            .request(current_method.clone(), current_url.clone())
            .headers(header_map.clone())
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err)
                if idempotent
                    && !err.is_timeout()
                    && (err.is_connect() || err.is_request() || err.is_body()) =>
            {
                tokio::time::sleep(Duration::from_millis(150)).await;
                client
                    .request(current_method.clone(), current_url.clone())
                    .headers(header_map)
                    .send()
                    .await
                    .map_err(|e| {
                        AudioError::Http(format!("request to {current_url} failed: {e}"))
                    })?
            }
            Err(err) => {
                return Err(AudioError::Http(format!(
                    "request to {current_url} failed: {err}"
                )));
            }
        };

        let status = response.status();
        if status.is_redirection() {
            let Some(loc_hdr) = response.headers().get(LOCATION) else {
                return Err(AudioError::Http(format!(
                    "redirect status {status} from {current_url} without Location header"
                )));
            };
            let loc_str = loc_hdr
                .to_str()
                .map_err(|e| AudioError::Http(format!("invalid Location header: {e}")))?;
            if redirect_count >= max_redirects {
                return Err(AudioError::Http(format!(
                    "exceeded maximum redirects ({max_redirects}) for {url}"
                )));
            }
            current_url = current_url.join(loc_str).map_err(|e| {
                AudioError::Http(format!(
                    "failed to resolve redirect '{loc_str}' against {current_url}: {e}"
                ))
            })?;
            if status == reqwest::StatusCode::SEE_OTHER {
                current_method = Method::GET;
            }
            redirect_count += 1;
            continue;
        }

        let resp_headers = response.headers();
        let content_type = resp_headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());
        let content_length = resp_headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| response.content_length());
        let content_range_total = resp_headers
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range_total);
        let total_size =
            content_range_total.or(if status == reqwest::StatusCode::PARTIAL_CONTENT {
                None
            } else {
                content_length
            });
        let accepts_ranges = status == reqwest::StatusCode::PARTIAL_CONTENT
            || resp_headers
                .get(ACCEPT_RANGES)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.eq_ignore_ascii_case("bytes"))
                .unwrap_or(false);

        return Ok(RedirectedResponse {
            final_url: current_url,
            redirect_count,
            status,
            content_type,
            content_length,
            total_size,
            accepts_ranges,
            response,
        });
    }
}

/// Parses total byte length from an HTTP `Content-Range` header like `bytes 0-1023/4096`.
pub fn parse_content_range_total(header_value: &str) -> Option<u64> {
    let (_, total_part) = header_value.rsplit_once('/')?;
    let trimmed = total_part.trim();
    if trimmed == "*" {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

/// Returns `true` when `url` points to a local loopback host (`127.0.0.1`, `localhost`,
/// `::1`, etc.) such as an ephemeral [`VirtualFfmpegShim`](crate::VirtualFfmpegShim) relay.
/// Ephemeral loopback URLs reuse ports across tracks and must never read from or persist
/// into the disk LRU cache.
pub(crate) fn is_loopback_url(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Computes a deterministic hex SHA-256 cache key for a URL or identifier.
pub fn cache_key_for_url(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    hex::encode(hasher.finalize())
}

#[derive(Debug, Clone)]
struct CacheEntry {
    key: String,
    path: PathBuf,
    size: u64,
    last_accessed: u64,
}

struct AudioCacheInner {
    cache_dir: PathBuf,
    max_bytes: AtomicU64,
    chunk_size: usize,
    client: Client,
    loopback_client: Client,
    access_seq: AtomicU64,
    entries: Mutex<HashMap<String, CacheEntry>>,
}

/// Thread-safe, clonable progressive HTTP audio cache with LRU byte eviction.
#[derive(Clone)]
pub struct AudioCache {
    inner: Arc<AudioCacheInner>,
}

impl std::fmt::Debug for AudioCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioCache")
            .field("cache_dir", &self.inner.cache_dir)
            .field("max_bytes", &self.inner.max_bytes.load(Ordering::Relaxed))
            .field("entry_count", &self.entry_count())
            .field("total_bytes", &self.total_bytes())
            .finish()
    }
}

impl AudioCache {
    /// Creates an [`AudioCache`] with the provided configuration, scanning existing
    /// cached `.audio` files on disk and enforcing the LRU byte cap.
    pub fn new(config: StreamCacheConfig) -> AudioResult<Self> {
        fs::create_dir_all(&config.cache_dir)?;
        let client = build_no_redirect_client(config.connect_timeout)?;
        let loopback_client = build_loopback_no_redirect_client(config.connect_timeout)?;
        let cache = Self {
            inner: Arc::new(AudioCacheInner {
                cache_dir: config.cache_dir,
                max_bytes: AtomicU64::new(config.max_bytes),
                chunk_size: config.chunk_size.max(4096),
                client,
                loopback_client,
                access_seq: AtomicU64::new(1),
                entries: Mutex::new(HashMap::new()),
            }),
        };
        cache.scan_existing()?;
        cache.evict_if_needed()?;
        Ok(cache)
    }

    fn client_for_url(&self, url: &Url) -> &Client {
        if is_loopback_url(url) {
            &self.inner.loopback_client
        } else {
            &self.inner.client
        }
    }

    /// Convenience constructor with an explicit cache directory and byte cap.
    pub fn with_dir(cache_dir: PathBuf, max_bytes: u64) -> AudioResult<Self> {
        Self::new(StreamCacheConfig {
            cache_dir,
            max_bytes,
            ..StreamCacheConfig::default()
        })
    }

    /// Creates a cache using default platform cache directories.
    pub fn default_cache() -> AudioResult<Self> {
        Self::new(StreamCacheConfig::default())
    }

    pub fn cache_dir(&self) -> &Path {
        &self.inner.cache_dir
    }

    pub fn max_bytes(&self) -> u64 {
        self.inner.max_bytes.load(Ordering::Relaxed)
    }

    pub fn set_max_bytes(&self, max_bytes: u64) -> AudioResult<usize> {
        self.inner.max_bytes.store(max_bytes, Ordering::Relaxed);
        self.evict_if_needed()
    }

    pub fn entry_count(&self) -> usize {
        self.inner.entries.lock().map(|e| e.len()).unwrap_or(0)
    }

    pub fn total_bytes(&self) -> u64 {
        self.inner
            .entries
            .lock()
            .map(|e| e.values().map(|v| v.size).sum())
            .unwrap_or(0)
    }

    pub fn contains(&self, url: &str) -> bool {
        let key = cache_key_for_url(url);
        self.inner
            .entries
            .lock()
            .map(|e| {
                e.get(&key)
                    .map(|entry| entry.path.exists())
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// Returns the cached file path if the URL is completely cached, updating its LRU timestamp.
    pub fn get_cached_path(&self, url: &str) -> Option<PathBuf> {
        let key = cache_key_for_url(url);
        let mut guard = self.inner.entries.lock().ok()?;
        let entry = guard.get_mut(&key)?;
        if !entry.path.exists() {
            guard.remove(&key);
            return None;
        }
        entry.last_accessed = self.next_access_stamp();
        Some(entry.path.clone())
    }

    /// Directly inserts raw bytes into the disk cache under `url` (useful for tests or offline saves),
    /// then enforces LRU byte eviction.
    pub fn put_bytes(&self, url: &str, bytes: &[u8]) -> AudioResult<PathBuf> {
        let key = cache_key_for_url(url);
        let final_path = self.final_path_for_key(&key);
        let tmp_path = self.part_path_for_key(&key);
        {
            let mut file = File::create(&tmp_path)?;
            file.write_all(bytes)?;
            file.flush()?;
        }
        fs::rename(&tmp_path, &final_path)?;
        self.register_complete_entry(key, final_path.clone(), bytes.len() as u64)?;
        Ok(final_path)
    }

    /// Removes all cached entries from disk.
    pub fn clear(&self) -> AudioResult<()> {
        let mut guard = self
            .inner
            .entries
            .lock()
            .map_err(|_| AudioError::Other("cache mutex poisoned".into()))?;
        for entry in guard.values() {
            let _ = fs::remove_file(&entry.path);
        }
        guard.clear();
        Ok(())
    }

    /// Evicts least-recently-used cached entries until total size is within `max_bytes`.
    /// Returns the number of evicted files.
    pub fn evict_if_needed(&self) -> AudioResult<usize> {
        let max_bytes = self.max_bytes();
        let mut guard = self
            .inner
            .entries
            .lock()
            .map_err(|_| AudioError::Other("cache mutex poisoned".into()))?;
        let mut total: u64 = guard.values().map(|e| e.size).sum();
        if total <= max_bytes {
            return Ok(0);
        }

        let mut sorted: Vec<CacheEntry> = guard.values().cloned().collect();
        sorted.sort_by_key(|e| e.last_accessed);

        let mut evicted = 0usize;
        for candidate in sorted {
            if total <= max_bytes {
                break;
            }
            if guard.remove(&candidate.key).is_some() {
                let _ = fs::remove_file(&candidate.path);
                total = total.saturating_sub(candidate.size);
                evicted += 1;
            }
        }
        Ok(evicted)
    }

    /// Performs an async HTTP GET with manual redirect traversal preserving all custom headers.
    pub async fn fetch_with_redirects(
        &self,
        url: &str,
        headers: &[(String, String)],
        range: Option<(u64, Option<u64>)>,
    ) -> AudioResult<RedirectedResponse> {
        let range_hdr = range.map(|(start, end)| match end {
            Some(e) => format!("bytes={start}-{e}"),
            None => format!("bytes={start}-"),
        });
        let client = Url::parse(url)
            .ok()
            .as_ref()
            .map(|parsed| self.client_for_url(parsed))
            .unwrap_or(&self.inner.client);
        send_with_preserved_headers(
            client,
            Method::GET,
            url,
            headers,
            range_hdr.as_deref(),
            MAX_REDIRECTS,
        )
        .await
    }

    /// Opens a progressive, seekable audio stream for a local path, `file://` URI,
    /// cached asset, or remote `http(s)://` URL.
    pub fn open_progressive(
        &self,
        url: &str,
        headers: Vec<(String, String)>,
    ) -> AudioResult<ProgressiveStream> {
        if let Some(local_path) = resolve_local_path(url) {
            let file = File::open(&local_path)?;
            let len = file.metadata()?.len();
            return Ok(ProgressiveStream::from_local_file(file, len, None));
        }

        let parsed = Url::parse(url)
            .map_err(|e| AudioError::InvalidInput(format!("unsupported media URI '{url}': {e}")))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(AudioError::InvalidInput(format!(
                "unsupported URI scheme '{}' in '{url}'",
                parsed.scheme()
            )));
        }

        let is_loopback = is_loopback_url(&parsed);
        if !is_loopback
            && let Some(cached_path) = self.get_cached_path(url)
            && let Ok(file) = File::open(&cached_path)
        {
            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
            return Ok(ProgressiveStream::from_local_file(file, len, None));
        }

        let key = cache_key_for_url(url);
        let final_path = self.final_path_for_key(&key);
        let part_path = self.part_path_for_key(&key);

        let rt = io_runtime_handle();
        let client = self.client_for_url(&parsed).clone();
        let req_url = url.to_owned();
        let req_headers = headers.clone();

        // Issue initial request via dedicated I/O runtime without nesting `block_on`.
        let (init_tx, init_rx) = std::sync::mpsc::channel();
        rt.spawn(async move {
            let res = send_with_preserved_headers(
                &client,
                Method::GET,
                &req_url,
                &req_headers,
                None,
                MAX_REDIRECTS,
            )
            .await;
            let _ = init_tx.send(res);
        });

        let redirected = init_rx
            .recv()
            .map_err(|_| AudioError::Http("HTTP stream opener terminated unexpectedly".into()))??;

        if !redirected.status.is_success() {
            return Err(AudioError::Http(format!(
                "HTTP {} when opening stream {}",
                redirected.status, redirected.final_url
            )));
        }

        let part_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&part_path)?;
        let read_file = File::open(&part_path)?;

        let shared = Arc::new(ProgressiveShared {
            state: Mutex::new(ProgressiveProgress {
                downloaded: 0,
                total_size: redirected.total_size,
                finished: false,
                error: None,
            }),
            downloaded_atomic: AtomicU64::new(0),
            ready: Condvar::new(),
            cancelled: AtomicBool::new(false),
            cancel_notify: Notify::new(),
        });

        let bg_shared = Arc::clone(&shared);
        let bg_cache = self.clone();
        let bg_key = key.clone();
        let bg_part_path = part_path.clone();
        let bg_final_path = final_path.clone();
        let mut response = redirected.response;

        rt.spawn(async move {
            let mut writer = part_file;
            let mut total_written = 0u64;
            loop {
                if bg_shared.cancelled.load(Ordering::Relaxed) {
                    drop(writer);
                    let _ = fs::remove_file(&bg_part_path);
                    return;
                }
                let next_chunk = tokio::select! {
                    _ = bg_shared.cancel_notify.notified() => {
                        drop(writer);
                        let _ = fs::remove_file(&bg_part_path);
                        return;
                    }
                    chunk = response.chunk() => chunk,
                };
                match next_chunk {
                    Ok(Some(chunk)) => {
                        if chunk.is_empty() {
                            continue;
                        }
                        if let Err(err) = writer.write_all(&chunk) {
                            bg_shared.set_error(format!("cache write error: {err}"));
                            drop(writer);
                            let _ = fs::remove_file(&bg_part_path);
                            return;
                        }
                        total_written += chunk.len() as u64;
                        bg_shared.advance_downloaded(total_written);
                    }
                    Ok(None) => {
                        let _ = writer.flush();
                        drop(writer);
                        if !is_loopback
                            && bg_cache.max_bytes() > 0
                            && fs::rename(&bg_part_path, &bg_final_path).is_ok()
                        {
                            let _ = bg_cache.register_complete_entry(
                                bg_key,
                                bg_final_path,
                                total_written,
                            );
                        } else if !is_loopback || bg_shared.cancelled.load(Ordering::Relaxed) {
                            let _ = fs::remove_file(&bg_part_path);
                        }
                        bg_shared.mark_finished(total_written);
                        return;
                    }
                    Err(err) => {
                        bg_shared.set_error(format!("HTTP stream error: {err}"));
                        drop(writer);
                        let _ = fs::remove_file(&bg_part_path);
                        return;
                    }
                }
            }
        });

        Ok(ProgressiveStream::from_remote(RemoteStreamInner {
            cache: self.clone(),
            final_url: redirected.final_url.to_string(),
            headers,
            read_file: Some(read_file),
            cleanup_part_path: if is_loopback { Some(part_path) } else { None },
            pos: 0,
            total_size: redirected.total_size,
            accepts_ranges: redirected.accepts_ranges,
            content_type: redirected.content_type,
            shared,
            range_cache: None,
        }))
    }

    /// Spawns a background prefetch downloading up to `max_prefetch_bytes` (or full file if
    /// `None`) into the LRU disk cache so the next track starts seamlessly.
    pub fn prefetch(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        max_prefetch_bytes: Option<usize>,
    ) -> tokio::task::JoinHandle<AudioResult<PathBuf>> {
        let cache = self.clone();
        io_runtime_handle().spawn(async move {
            if cache.max_bytes() == 0 {
                return Err(AudioError::Other("audio disk cache is disabled".into()));
            }
            if Url::parse(&url).ok().as_ref().is_some_and(is_loopback_url) {
                return Err(AudioError::Other(
                    "skipping disk cache prefetch for ephemeral loopback stream".into(),
                ));
            }
            if let Some(existing) = cache.get_cached_path(&url) {
                return Ok(existing);
            }
            let range = max_prefetch_bytes.map(|n| (0u64, Some(n.saturating_sub(1) as u64)));
            let mut resp = cache.fetch_with_redirects(&url, &headers, range).await?;
            if !resp.status.is_success() {
                return Err(AudioError::Http(format!(
                    "prefetch HTTP {} for {}",
                    resp.status, resp.final_url
                )));
            }
            let key = cache_key_for_url(&url);
            let final_path = cache.final_path_for_key(&key);
            let part_path = cache.part_path_for_key(&key);
            let mut file = File::create(&part_path)?;
            let mut written = 0u64;
            while let Some(chunk) = resp
                .response
                .chunk()
                .await
                .map_err(|e| AudioError::Http(format!("prefetch chunk error: {e}")))?
            {
                file.write_all(&chunk)?;
                written += chunk.len() as u64;
            }
            file.flush()?;
            drop(file);
            // Only promote to complete cache entry if full track was downloaded;
            // remove partial temp files so they never leak in the cache directory.
            let is_complete = max_prefetch_bytes.is_none()
                || resp
                    .total_size
                    .map(|total| written >= total)
                    .unwrap_or(false);
            if is_complete {
                fs::rename(&part_path, &final_path)?;
                cache.register_complete_entry(key, final_path.clone(), written)?;
                Ok(final_path)
            } else {
                let _ = fs::remove_file(&part_path);
                Ok(final_path)
            }
        })
    }

    fn final_path_for_key(&self, key: &str) -> PathBuf {
        self.inner.cache_dir.join(format!("{key}.audio"))
    }

    fn part_path_for_key(&self, key: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.inner.cache_dir.join(format!("{key}.{stamp}.part"))
    }

    fn next_access_stamp(&self) -> u64 {
        self.inner.access_seq.fetch_add(1, Ordering::Relaxed)
    }

    fn register_complete_entry(&self, key: String, path: PathBuf, size: u64) -> AudioResult<()> {
        let stamp = self.next_access_stamp();
        {
            let mut guard = self
                .inner
                .entries
                .lock()
                .map_err(|_| AudioError::Other("cache mutex poisoned".into()))?;
            guard.insert(
                key.clone(),
                CacheEntry {
                    key,
                    path,
                    size,
                    last_accessed: stamp,
                },
            );
        }
        self.evict_if_needed()?;
        Ok(())
    }

    fn scan_existing(&self) -> AudioResult<()> {
        let Ok(read_dir) = fs::read_dir(&self.inner.cache_dir) else {
            return Ok(());
        };
        let mut scanned = Vec::new();
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("part") {
                let _ = fs::remove_file(&path);
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("audio") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            scanned.push((stem.to_owned(), path, meta.len(), modified));
        }
        scanned.sort_by_key(|(_, _, _, mtime)| *mtime);

        let mut guard = self
            .inner
            .entries
            .lock()
            .map_err(|_| AudioError::Other("cache mutex poisoned".into()))?;
        for (key, path, size, _) in scanned {
            let stamp = self.next_access_stamp();
            guard.insert(
                key.clone(),
                CacheEntry {
                    key,
                    path,
                    size,
                    last_accessed: stamp,
                },
            );
        }
        Ok(())
    }
}

fn resolve_local_path(uri: &str) -> Option<PathBuf> {
    if let Some(rest) = uri.strip_prefix("file://") {
        if let Ok(parsed) = Url::parse(uri)
            && let Ok(path) = parsed.to_file_path()
        {
            return Some(path);
        }
        return Some(PathBuf::from(rest));
    }
    let path = Path::new(uri);
    if path.exists() {
        return Some(path.to_path_buf());
    }
    None
}

struct ProgressiveProgress {
    downloaded: u64,
    total_size: Option<u64>,
    finished: bool,
    error: Option<String>,
}

struct ProgressiveShared {
    state: Mutex<ProgressiveProgress>,
    downloaded_atomic: AtomicU64,
    ready: Condvar,
    cancelled: AtomicBool,
    cancel_notify: Notify,
}

impl ProgressiveShared {
    fn advance_downloaded(&self, bytes: u64) {
        self.downloaded_atomic.store(bytes, Ordering::Release);
        if let Ok(mut guard) = self.state.lock() {
            guard.downloaded = bytes;
            self.ready.notify_all();
        }
    }

    fn mark_finished(&self, final_size: u64) {
        self.downloaded_atomic.store(final_size, Ordering::Release);
        if let Ok(mut guard) = self.state.lock() {
            guard.downloaded = final_size;
            guard.total_size = Some(final_size);
            guard.finished = true;
            self.ready.notify_all();
        }
    }

    fn set_error(&self, err: String) {
        if let Ok(mut guard) = self.state.lock() {
            guard.error = Some(err);
            guard.finished = true;
            self.ready.notify_all();
        }
    }
}

struct RangeWindow {
    start: u64,
    data: Vec<u8>,
}

struct RemoteStreamInner {
    cache: AudioCache,
    final_url: String,
    headers: Vec<(String, String)>,
    read_file: Option<File>,
    cleanup_part_path: Option<PathBuf>,
    pos: u64,
    total_size: Option<u64>,
    accepts_ranges: bool,
    content_type: Option<String>,
    shared: Arc<ProgressiveShared>,
    range_cache: Option<RangeWindow>,
}

impl Drop for RemoteStreamInner {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::Relaxed);
        self.shared.cancel_notify.notify_waiters();
        // Close read handle before removing ephemeral loopback `.part` files (Windows-safe).
        let _ = self.read_file.take();
        if let Some(ref part_path) = self.cleanup_part_path {
            let _ = fs::remove_file(part_path);
        }
    }
}

impl RemoteStreamInner {
    fn fetch_range_window(&mut self, offset: u64, min_len: usize) -> io::Result<()> {
        let window_size = (min_len.max(self.cache.inner.chunk_size * 4)) as u64;
        let end = match self.total_size {
            Some(total) if offset >= total => {
                self.range_cache = Some(RangeWindow {
                    start: offset,
                    data: Vec::new(),
                });
                return Ok(());
            }
            Some(total) => (offset + window_size - 1).min(total - 1),
            None => offset + window_size - 1,
        };

        let client = Url::parse(&self.final_url)
            .ok()
            .as_ref()
            .map(|parsed| self.cache.client_for_url(parsed))
            .unwrap_or(&self.cache.inner.client)
            .clone();
        let url = self.final_url.clone();
        let headers = self.headers.clone();
        let range_hdr = format!("bytes={offset}-{end}");
        let max_full_fallback_bytes = offset.saturating_add(window_size) as usize;
        let (tx, rx) = std::sync::mpsc::channel();

        io_runtime_handle().spawn(async move {
            let res = async {
                let mut resp = send_with_preserved_headers(
                    &client,
                    Method::GET,
                    &url,
                    &headers,
                    Some(&range_hdr),
                    MAX_REDIRECTS,
                )
                .await?;
                if resp.status != reqwest::StatusCode::PARTIAL_CONTENT && !resp.status.is_success()
                {
                    return Err(AudioError::Http(format!(
                        "range seek HTTP {} for {}",
                        resp.status, resp.final_url
                    )));
                }
                let status = resp.status;
                let mut data = Vec::new();
                while let Some(chunk) = resp
                    .response
                    .chunk()
                    .await
                    .map_err(|e| AudioError::Http(format!("range chunk error: {e}")))?
                {
                    data.extend_from_slice(&chunk);
                    // If server ignored Range (200 OK), stop once we cover the requested window.
                    if status != reqwest::StatusCode::PARTIAL_CONTENT
                        && data.len() >= max_full_fallback_bytes
                    {
                        break;
                    }
                }
                Ok((status, data, resp.total_size))
            }
            .await;
            let _ = tx.send(res);
        });

        let (status, data, updated_total) = rx
            .recv()
            .map_err(|_| io::Error::other("range fetch task failed"))?
            .map_err(|e| io::Error::other(e.to_string()))?;

        if let Some(total) = updated_total {
            self.total_size = Some(total);
        }

        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            self.range_cache = Some(RangeWindow {
                start: offset,
                data,
            });
            Ok(())
        } else {
            // Server ignored the HTTP Range header and sent bytes starting at offset 0.
            // Never mislabel offset-0 bytes as `start: offset`.
            self.accepts_ranges = false;
            if offset < data.len() as u64 {
                self.range_cache = Some(RangeWindow { start: 0, data });
                Ok(())
            } else {
                self.range_cache = None;
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "upstream ignored HTTP Range request and did not cover requested offset",
                ))
            }
        }
    }

    fn read_from_file_at(
        &mut self,
        offset: u64,
        buf: &mut [u8],
        avail: usize,
    ) -> io::Result<usize> {
        let file = self
            .read_file
            .as_mut()
            .ok_or_else(|| io::Error::other("progressive read file closed"))?;
        file.seek(SeekFrom::Start(offset))?;
        let to_read = buf.len().min(avail);
        let n = file.read(&mut buf[..to_read])?;
        self.pos += n as u64;
        Ok(n)
    }

    fn read_bytes(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if let Some(total) = self.total_size
            && self.pos >= total
        {
            return Ok(0);
        }

        // Lock-free fast path: read directly from progressive file when already buffered.
        let fast_downloaded = self.shared.downloaded_atomic.load(Ordering::Acquire);
        if self.pos < fast_downloaded {
            let avail = (fast_downloaded - self.pos) as usize;
            return self.read_from_file_at(self.pos, buf, avail);
        }

        // Slow path: inspect progress state and range window.
        let (downloaded, finished, error) = {
            let guard = self
                .shared
                .state
                .lock()
                .map_err(|_| io::Error::other("stream mutex poisoned"))?;
            (guard.downloaded, guard.finished, guard.error.clone())
        };

        if self.pos < downloaded {
            let avail = (downloaded - self.pos) as usize;
            return self.read_from_file_at(self.pos, buf, avail);
        }

        // Check range window cache when seeking ahead of the progressive writer.
        if let Some(window) = &self.range_cache
            && self.pos >= window.start
            && self.pos < window.start + window.data.len() as u64
        {
            let rel = (self.pos - window.start) as usize;
            let avail = window.data.len() - rel;
            let n = buf.len().min(avail);
            buf[..n].copy_from_slice(&window.data[rel..rel + n]);
            self.pos += n as u64;
            return Ok(n);
        }

        // If position is far ahead of the progressive download and server accepts ranges, fetch via HTTP Range!
        // Keep threshold at 512 KiB so DASH fMP4 fragment skips (~160 KiB per `mdat`) reuse the
        // already-open progressive download instead of firing dozens of serial Range connections.
        const RANGE_JUMP_THRESHOLD: u64 = 512 * 1024;
        if self.accepts_ranges && self.pos > downloaded.saturating_add(RANGE_JUMP_THRESHOLD) {
            match self.fetch_range_window(self.pos, buf.len()) {
                Ok(()) => {
                    if let Some(window) = &self.range_cache {
                        if window.data.is_empty() {
                            return Ok(0);
                        }
                        if self.pos >= window.start
                            && self.pos < window.start + window.data.len() as u64
                        {
                            let rel = (self.pos - window.start) as usize;
                            let avail = window.data.len() - rel;
                            let n = buf.len().min(avail);
                            buf[..n].copy_from_slice(&window.data[rel..rel + n]);
                            self.pos += n as u64;
                            return Ok(n);
                        }
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Unsupported => {
                    // Server ignored Range (`accepts_ranges` is now false); fall through
                    // to the sequential progressive stream rather than returning bad bytes.
                }
                Err(err) => return Err(err),
            }
        }

        if finished {
            if let Some(err) = error {
                return Err(io::Error::other(err));
            }
            return Ok(0);
        }

        // Wait on Condvar for progressive download to advance past `self.pos`.
        let mut guard = self
            .shared
            .state
            .lock()
            .map_err(|_| io::Error::other("stream mutex poisoned"))?;
        while self.pos >= guard.downloaded && !guard.finished {
            let (next_guard, wait_res) = self
                .shared
                .ready
                .wait_timeout(guard, Duration::from_secs(30))
                .map_err(|_| io::Error::other("stream condvar poisoned"))?;
            guard = next_guard;
            if wait_res.timed_out() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for audio stream bytes",
                ));
            }
        }

        if let Some(total) = guard.total_size {
            self.total_size = Some(total);
        }

        if self.pos < guard.downloaded {
            let avail = (guard.downloaded - self.pos) as usize;
            drop(guard);
            return self.read_from_file_at(self.pos, buf, avail);
        }

        if let Some(err) = guard.error.clone() {
            return Err(io::Error::other(err));
        }

        Ok(0)
    }

    fn seek_pos(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target: i64 = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::Current(delta) => (self.pos as i64).saturating_add(delta),
            SeekFrom::End(delta) => {
                let total = self.total_size.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "cannot seek from end on stream with unknown length",
                    )
                })?;
                (total as i64).saturating_add(delta)
            }
        };

        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot seek to negative position",
            ));
        }
        let target_u64 = target as u64;
        if let Some(total) = self.total_size {
            self.pos = target_u64.min(total);
        } else {
            self.pos = target_u64;
        }
        Ok(self.pos)
    }
}

enum StreamBackend {
    Local { file: File, len: u64 },
    Remote(RemoteStreamInner),
}

/// Unified sync `Read + Seek + Send + Sync` media stream backed by either a local/cached
/// file or a progressive HTTP/HTTPS stream with Range seek support.
pub struct ProgressiveStream {
    backend: Mutex<StreamBackend>,
    byte_len: Option<u64>,
    seekable: bool,
    content_type: Option<String>,
}

impl ProgressiveStream {
    fn from_local_file(file: File, len: u64, content_type: Option<String>) -> Self {
        Self {
            backend: Mutex::new(StreamBackend::Local { file, len }),
            byte_len: Some(len),
            seekable: true,
            content_type,
        }
    }

    fn from_remote(inner: RemoteStreamInner) -> Self {
        let byte_len = inner.total_size;
        // Symphonia demuxers (e.g. IsoMp4Reader) require `byte_len().is_some()` whenever
        // `is_seekable() == true`; report seekable ONLY when total byte length is known.
        let seekable = byte_len.is_some();
        let content_type = inner.content_type.clone();
        Self {
            backend: Mutex::new(StreamBackend::Remote(inner)),
            byte_len,
            seekable,
            content_type,
        }
    }

    pub fn byte_len(&self) -> Option<u64> {
        if let Ok(guard) = self.backend.lock() {
            match &*guard {
                StreamBackend::Local { len, .. } => return Some(*len),
                StreamBackend::Remote(r) => {
                    if let Some(t) = r.total_size {
                        return Some(t);
                    }
                    if let Ok(st) = r.shared.state.lock()
                        && st.total_size.is_some()
                    {
                        return st.total_size;
                    }
                }
            }
        }
        self.byte_len
    }

    pub fn is_seekable(&self) -> bool {
        self.seekable
    }

    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// Returns currently buffered/downloaded bytes on disk.
    pub fn buffered_bytes(&self) -> u64 {
        let Ok(guard) = self.backend.lock() else {
            return 0;
        };
        match &*guard {
            StreamBackend::Local { len, .. } => *len,
            StreamBackend::Remote(r) => r.shared.state.lock().map(|s| s.downloaded).unwrap_or(0),
        }
    }
}

impl Read for ProgressiveStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut guard = self
            .backend
            .lock()
            .map_err(|_| io::Error::other("stream mutex poisoned"))?;
        match &mut *guard {
            StreamBackend::Local { file, .. } => file.read(buf),
            StreamBackend::Remote(remote) => remote.read_bytes(buf),
        }
    }
}

impl Seek for ProgressiveStream {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let mut guard = self
            .backend
            .lock()
            .map_err(|_| io::Error::other("stream mutex poisoned"))?;
        match &mut *guard {
            StreamBackend::Local { file, .. } => file.seek(pos),
            StreamBackend::Remote(remote) => remote.seek_pos(pos),
        }
    }
}

#[cfg(feature = "audio-symphonia")]
impl symphonia::core::io::MediaSource for ProgressiveStream {
    fn is_seekable(&self) -> bool {
        ProgressiveStream::is_seekable(self)
    }

    fn byte_len(&self) -> Option<u64> {
        ProgressiveStream::byte_len(self)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn test_cache_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "nsh-cache-unit-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_unseekable_stream_when_content_length_missing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let payload: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
        let server_payload = payload.clone();

        let srv = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            // Advertise Accept-Ranges: bytes WITHOUT Content-Length (the exact trigger
            // that previously crashed IsoMp4Reader with SeekError(Unseekable)).
            let hdr = "HTTP/1.1 200 OK\r\nContent-Type: audio/mp4\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n";
            socket.write_all(hdr.as_bytes()).await.expect("write hdr");
            socket.write_all(&server_payload).await.expect("write body");
            let _ = socket.shutdown().await;
        });

        let dir = test_cache_dir("unseekable");
        let cache = AudioCache::with_dir(dir.clone(), 1024 * 1024).expect("cache");
        let url = format!("http://{addr}/live-stream");

        let mut stream = cache.open_progressive(&url, Vec::new()).expect("open");
        assert_eq!(stream.byte_len(), None);
        assert!(
            !stream.is_seekable(),
            "stream without Content-Length must report is_seekable() == false"
        );

        let mut received = Vec::new();
        stream.read_to_end(&mut received).expect("read to end");
        assert_eq!(received, payload);

        srv.await.expect("srv");
        drop(stream);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_range_fallback_200_ok_does_not_corrupt_offset() {
        let pattern: Vec<u8> = (0..700_000usize)
            .map(|i| ((i / 251) ^ (i % 251)) as u8)
            .collect();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let srv_pattern = Arc::new(pattern.clone());
        let req_count = Arc::new(AtomicUsize::new(0));
        let srv_count = Arc::clone(&req_count);

        let srv = tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let pat = Arc::clone(&srv_pattern);
                let count = Arc::clone(&srv_count);
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                    let seq = count.fetch_add(1, Ordering::SeqCst);
                    if req.contains("range:") {
                        // Ignore Range and return 200 OK starting at byte 0!
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            pat.len()
                        );
                        let _ = socket.write_all(hdr.as_bytes()).await;
                        let _ = socket.write_all(&pat).await;
                    } else {
                        // Initial GET: send headers + only first 4 KiB then pause so seek triggers Range.
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                            pat.len()
                        );
                        let _ = socket.write_all(hdr.as_bytes()).await;
                        let _ = socket.write_all(&pat[..4096]).await;
                        if seq == 0 {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                        let _ = socket.write_all(&pat[4096..]).await;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });

        let dir = test_cache_dir("range-200");
        let cache = AudioCache::with_dir(dir.clone(), 1024 * 1024).expect("cache");
        let url = format!("http://{addr}/track.mp3");
        let mut stream = cache.open_progressive(&url, Vec::new()).expect("open");

        // Jump past RANGE_JUMP_THRESHOLD (512 KiB) to offset 600_000.
        stream.seek(SeekFrom::Start(600_000)).expect("seek");
        let mut slice = [0u8; 64];
        stream.read_exact(&mut slice).expect("read exact");
        assert_eq!(
            &slice[..],
            &pattern[600_000..600_064],
            "200 OK Range fallback must return bytes at offset 600_000, NOT corrupted offset 0 bytes"
        );

        // Also verify short 200 OK body (shorter than offset) returns Unsupported & disables accepts_ranges.
        let short_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind short");
        let short_addr = short_listener.local_addr().expect("short addr");
        let short_srv = tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut socket, _)) = short_listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                    if req.contains("range:") {
                        let body = [0xABu8; 1024];
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(hdr.as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                    } else {
                        let hdr = "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 300000\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n";
                        let _ = socket.write_all(hdr.as_bytes()).await;
                        let _ = socket.write_all(&[0u8; 512]).await;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        let short_stream = cache
            .open_progressive(&format!("http://{short_addr}/short.mp3"), Vec::new())
            .expect("open short");
        {
            let mut guard = short_stream.backend.lock().expect("lock");
            if let StreamBackend::Remote(ref mut inner) = *guard {
                let err = inner
                    .fetch_range_window(200_000, 64)
                    .expect_err("short 200 OK must return Unsupported");
                assert_eq!(err.kind(), io::ErrorKind::Unsupported);
                assert!(!inner.accepts_ranges);
            } else {
                panic!("expected remote stream");
            }
        }

        drop(stream);
        drop(short_stream);
        let _ = srv.await;
        let _ = short_srv.await;
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_loopback_url_bypasses_disk_cache() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let url = format!("http://{addr}/");

        let dir = test_cache_dir("loopback-bypass");
        let cache = AudioCache::with_dir(dir.clone(), 1024 * 1024).expect("cache");
        // Seed poison bytes under this exact loopback URL.
        cache
            .put_bytes(&url, b"STALE-POISON-DATA")
            .expect("seed poison");
        assert!(cache.contains(&url));

        let fresh_payload = b"FRESH-RELAY-AUDIO-BYTES".to_vec();
        let srv_payload = fresh_payload.clone();
        let srv = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let hdr = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/webm\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                srv_payload.len()
            );
            socket.write_all(hdr.as_bytes()).await.expect("hdr");
            socket.write_all(&srv_payload).await.expect("body");
            let _ = socket.shutdown().await;
        });

        let mut stream = cache.open_progressive(&url, Vec::new()).expect("open");
        let mut received = Vec::new();
        stream.read_to_end(&mut received).expect("read");
        assert_eq!(
            received, fresh_payload,
            "loopback URL must bypass disk cache and never serve stale poison bytes"
        );
        drop(stream);
        srv.await.expect("srv");

        // Fresh loopback URL must NOT persist into the LRU disk cache upon completion.
        let listener2 = TcpListener::bind("127.0.0.1:0").await.expect("bind2");
        let addr2 = listener2.local_addr().expect("addr2");
        let url2 = format!("http://{addr2}/ephemeral");
        let srv2 = tokio::spawn(async move {
            let (mut socket, _) = listener2.accept().await.expect("accept2");
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let body = b"EPHEMERAL-BYTES";
            let hdr = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(hdr.as_bytes()).await;
            let _ = socket.write_all(body).await;
            let _ = socket.shutdown().await;
        });
        let mut stream2 = cache.open_progressive(&url2, Vec::new()).expect("open2");
        let mut got2 = Vec::new();
        stream2.read_to_end(&mut got2).expect("read2");
        assert_eq!(got2, b"EPHEMERAL-BYTES");
        drop(stream2);
        srv2.await.expect("srv2");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !cache.contains(&url2),
            "ephemeral loopback URL must never be persisted into the disk cache"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_loopback_client_selected_and_reads_cleanly() {
        let dir = test_cache_dir("loopback-client");
        let cache = AudioCache::with_dir(dir.clone(), 1024 * 1024).expect("cache");

        // Verify client_for_url routes loopback hosts to the `.no_proxy()` loopback client
        // and external hosts to the standard client.
        for loopback in [
            "http://127.0.0.1:24567/",
            "http://localhost:8080/stream",
            "http://[::1]:9000/",
        ] {
            let parsed = Url::parse(loopback).expect("parse loopback");
            assert!(
                std::ptr::eq(cache.client_for_url(&parsed), &cache.inner.loopback_client),
                "expected loopback_client for {loopback}"
            );
        }
        let remote = Url::parse("https://rr1---sn.googlevideo.com/videoplayback").expect("parse");
        assert!(
            std::ptr::eq(cache.client_for_url(&remote), &cache.inner.client),
            "expected standard client for remote URL"
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let payload = b"LOOPBACK-NO-PROXY-AUDIO-STREAM".to_vec();
        let srv_payload = payload.clone();
        let srv = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let hdr = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                srv_payload.len()
            );
            socket.write_all(hdr.as_bytes()).await.expect("hdr");
            socket.write_all(&srv_payload).await.expect("body");
            let _ = socket.shutdown().await;
        });

        let url = format!("http://{addr}/");
        let mut stream = cache.open_progressive(&url, Vec::new()).expect("open");
        let mut out = Vec::new();
        stream.read_to_end(&mut out).expect("read");
        assert_eq!(out, payload);
        drop(stream);
        srv.await.expect("srv");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_send_with_preserved_headers_retries_once_on_transient_get_drop() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let srv_hits = Arc::clone(&hits);

        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let attempt = srv_hits.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    if attempt == 0 {
                        // Drop socket immediately without sending a response to simulate
                        // a stale pooled connection / transient transport reset.
                        drop(socket);
                        return;
                    }
                    let body = b"RECOVERED-AFTER-RETRY";
                    let hdr = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(hdr.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let client = build_loopback_no_redirect_client(Duration::from_secs(5)).expect("client");
        let url = format!("http://{addr}/retry-me");
        let resp =
            send_with_preserved_headers(&client, Method::GET, &url, &[], None, MAX_REDIRECTS)
                .await
                .expect("GET should recover on single transient retry");
        assert_eq!(resp.status, reqwest::StatusCode::OK);
        let body = resp.response.bytes().await.expect("bytes");
        assert_eq!(body.as_ref(), b"RECOVERED-AFTER-RETRY");
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        srv.abort();
    }
}
