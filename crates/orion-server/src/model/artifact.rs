//! Where a model's bytes are and how a node gets them: the storage-side
//! reference, the signed fetch through a storage connector, and the
//! digest-keyed disk cache.
//!
//! A model row never carries its bytes. It names a storage connector, an
//! object key and the digest the bytes must hash to, and the node that
//! admits the row fetches the object itself — a SigV4-signed GET through the
//! connector, exactly the request `storage_head` makes with a different
//! verb, under the same private-address posture and the same operation
//! gate. The digest is the contract: whatever the bucket serves is hashed
//! before it is kept, and a mismatch keeps nothing. The cache is one file per
//! digest under `models.cache_dir`, written atomically and swept
//! least-recently-used to `models.max_cache_bytes`.
//!
//! Every byte that crosses the network goes through
//! [`crate::http_body::read_bounded`], so `models.max_artifact_bytes` bounds
//! the memory a fetch can be made to hold, not merely the result it returns.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::connector::{StorageConnectorConfig, sigv4};
use crate::http_body::{ReadError, read_bounded};

/// Where a model version's bytes live, as a row stores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// The storage connector the object is fetched through.
    pub connector: String,
    /// The object key within the connector's bucket.
    pub key: String,
    /// `sha256:<64 hex>` — what the bytes must hash to.
    pub digest: String,
    /// The size the author declared, if any. Informational: the digest is
    /// the contract, and the size a node acts on is the one the bucket
    /// reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// What a HEAD said about an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadInfo {
    pub size: Option<u64>,
    pub etag: Option<String>,
}

/// Why an artifact could not be produced, by the stage it failed in — the
/// stage is what an admission records.
#[derive(Debug)]
pub enum FetchError {
    /// The connector's operation gates refuse reads.
    Gate(String),
    /// The HEAD failed: transport, timeout, a status other than 200.
    Head(String),
    /// The GET failed: transport, timeout, a status other than 200.
    Fetch(String),
    /// The object is over `max_artifact_bytes`, by its declared length or
    /// by the bytes that arrived.
    Size { limit: usize, declared: Option<u64> },
    /// The claimed digest is not a `sha256:<64 hex>` string.
    BadDigest { claimed: String },
    /// The bytes hash to something other than the claim. Nothing was kept.
    DigestMismatch { claimed: String, computed: String },
    /// The cache directory could not be read or written.
    Cache(String),
}

impl FetchError {
    /// The admission stage this failure belongs to.
    pub fn stage(&self) -> &'static str {
        match self {
            Self::Gate(_) => "gate",
            Self::Head(_) => "head",
            Self::Fetch(_) => "fetch",
            Self::Size { .. } => "size",
            Self::BadDigest { .. } | Self::DigestMismatch { .. } => "digest",
            Self::Cache(_) => "cache",
        }
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gate(m) | Self::Head(m) | Self::Fetch(m) | Self::Cache(m) => f.write_str(m),
            Self::Size {
                limit,
                declared: Some(declared),
            } => write!(
                f,
                "the object is {declared} bytes, over models.max_artifact_bytes ({limit})"
            ),
            Self::Size {
                limit,
                declared: None,
            } => write!(
                f,
                "the object body exceeds models.max_artifact_bytes ({limit})"
            ),
            Self::BadDigest { claimed } => write!(
                f,
                "digest '{claimed}' is not an artifact digest: expected 'sha256:' followed by 64 \
                 lowercase hex characters"
            ),
            Self::DigestMismatch { claimed, computed } => write!(
                f,
                "the object hashes to {computed}, not the {claimed} the row names; nothing was \
                 kept"
            ),
        }
    }
}

impl std::error::Error for FetchError {}

/// A temporary file left behind by an interrupted write is junk once it is
/// this old; younger ones may still be being written by another task.
const STALE_TEMP_AGE: Duration = Duration::from_secs(60 * 60);

/// The digest-keyed disk cache and the fetch that fills it.
pub struct ArtifactStore {
    cache_dir: PathBuf,
    max_cache_bytes: u64,
    /// Digests whose cached file this process has hashed and found correct,
    /// so a cache hit costs one hash per process rather than one per
    /// generation.
    verified: Mutex<HashSet<String>>,
}

impl ArtifactStore {
    pub fn new(cache_dir: impl Into<PathBuf>, max_cache_bytes: u64) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            max_cache_bytes,
            verified: Mutex::new(HashSet::new()),
        }
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Where the bytes under `digest` live once cached: `cache_dir/<hex>`.
    pub fn path_for(&self, digest: &str) -> PathBuf {
        self.cache_dir
            .join(digest.strip_prefix("sha256:").unwrap_or(digest))
    }

    /// A signed HEAD of `key` through `storage`, the request `storage_head`
    /// makes. Gated on `operations.presign_get`, as the GET that follows is:
    /// a signed read is the same permission whichever verb asks.
    pub async fn head(
        &self,
        storage: &StorageConnectorConfig,
        client: &reqwest::Client,
        key: &str,
    ) -> Result<HeadInfo, FetchError> {
        require_read_gate(storage)?;
        let request = signed(
            storage,
            client,
            key,
            "HEAD",
            Duration::from_millis(storage.timeout_ms),
        )
        .await
        .map_err(FetchError::Head)?;
        let response = request.send().await.map_err(|e| {
            FetchError::Head(if e.is_timeout() {
                "HEAD timed out".to_string()
            } else {
                // `without_url`: the message names the connector's object,
                // never its endpoint.
                format!("HEAD failed: {}", e.without_url())
            })
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(FetchError::Head(format!("HEAD answered HTTP {status}")));
        }
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        Ok(HeadInfo {
            size: header("content-length").and_then(|v| v.parse::<u64>().ok()),
            etag: header("etag").map(|v| v.trim_matches('"').to_string()),
        })
    }

    /// The bytes under `artifact.digest`, on disk and verified.
    ///
    /// A cache hit is re-hashed once per process; a file that does not hash
    /// to its name is removed and fetched again. A miss is a signed GET
    /// streamed under `max_artifact_bytes`, hashed, compared with the claim
    /// — a mismatch writes nothing — then written beside its final name and
    /// renamed into place, after which the directory is swept to
    /// `max_cache_bytes`.
    pub async fn fetch(
        &self,
        storage: &StorageConnectorConfig,
        client: &reqwest::Client,
        artifact: &ArtifactRef,
        max_artifact_bytes: usize,
        fetch_timeout: Duration,
    ) -> Result<PathBuf, FetchError> {
        if !crate::crypto::is_sha256_digest(&artifact.digest) {
            return Err(FetchError::BadDigest {
                claimed: artifact.digest.clone(),
            });
        }
        let path = self.path_for(&artifact.digest);
        if let Some(hit) = self.cache_hit(&artifact.digest, &path).await? {
            return Ok(hit);
        }

        require_read_gate(storage)?;
        let request = signed(storage, client, &artifact.key, "GET", fetch_timeout)
            .await
            .map_err(FetchError::Fetch)?;
        let response = request.send().await.map_err(|e| {
            FetchError::Fetch(if e.is_timeout() {
                "GET timed out".to_string()
            } else {
                format!("GET failed: {}", e.without_url())
            })
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(FetchError::Fetch(format!("GET answered HTTP {status}")));
        }
        let bytes = read_bounded(response, max_artifact_bytes)
            .await
            .map_err(|e| match e {
                ReadError::TooLarge { limit, declared } => FetchError::Size { limit, declared },
                ReadError::Transport(e) => FetchError::Fetch(if e.is_timeout() {
                    "GET timed out".to_string()
                } else {
                    format!("GET failed: {}", e.without_url())
                }),
            })?;

        let claimed = artifact.digest.clone();
        let cache_dir = self.cache_dir.clone();
        let final_path = path.clone();
        // Hashing and writing hundreds of megabytes is blocking work; the
        // task that admits a model must not hold an executor thread for it.
        let written = tokio::task::spawn_blocking(move || {
            let computed = crate::crypto::sha256_digest(&bytes);
            if computed != claimed {
                return Err(FetchError::DigestMismatch { claimed, computed });
            }
            write_atomically(&cache_dir, &final_path, &bytes)
        })
        .await
        .map_err(|e| FetchError::Cache(format!("the cache write task failed: {e}")))?;
        written?;

        self.verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(artifact.digest.clone());
        let remaining = self.sweep_keeping(Some(&path))?;
        crate::metrics::set_model_cache_bytes(remaining);
        Ok(path)
    }

    /// `path` if it holds the bytes under `digest`. A file this process has
    /// not hashed yet is hashed now; one that does not match is removed so
    /// the caller fetches it again.
    async fn cache_hit(&self, digest: &str, path: &Path) -> Result<Option<PathBuf>, FetchError> {
        if !path.is_file() {
            return Ok(None);
        }
        let already = self
            .verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(digest);
        if !already {
            let hash_path = path.to_path_buf();
            let computed = tokio::task::spawn_blocking(move || {
                std::fs::read(&hash_path).map(|bytes| crate::crypto::sha256_digest(&bytes))
            })
            .await
            .map_err(|e| FetchError::Cache(format!("the cache hash task failed: {e}")))?
            .map_err(|e| FetchError::Cache(format!("cannot read {}: {e}", path.display())))?;
            if computed != digest {
                tracing::warn!(
                    path = %path.display(),
                    expected = digest,
                    found = computed,
                    "cached artifact does not hash to its name; removing it and fetching again"
                );
                let _ = std::fs::remove_file(path);
                return Ok(None);
            }
            self.verified
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(digest.to_string());
        }
        touch(path);
        Ok(Some(path.to_path_buf()))
    }

    /// Bytes the cache directory holds right now, temporary files included.
    /// `0` for a directory that does not exist yet.
    pub fn cached_bytes(&self) -> u64 {
        list_files(&self.cache_dir).iter().map(|f| f.len).sum()
    }

    /// Remove least-recently-used files, by modification time, until the
    /// directory is within `max_cache_bytes`. Returns the bytes remaining.
    pub fn sweep(&self) -> Result<u64, FetchError> {
        self.sweep_keeping(None)
    }

    /// [`Self::sweep`], never removing `keep` — the file a fetch just wrote,
    /// which the caller is about to use whatever the ceiling says.
    fn sweep_keeping(&self, keep: Option<&Path>) -> Result<u64, FetchError> {
        let mut files = list_files(&self.cache_dir);
        let now = SystemTime::now();
        // A temporary file still being written by another task is not a
        // candidate; one old enough to be a crash's leftover is.
        files.retain(|f| {
            !f.temporary
                || now
                    .duration_since(f.modified)
                    .is_ok_and(|age| age > STALE_TEMP_AGE)
        });
        let mut total: u64 = files.iter().map(|f| f.len).sum();
        files.sort_by_key(|f| f.modified);
        for file in files {
            if total <= self.max_cache_bytes {
                break;
            }
            if keep.is_some_and(|k| k == file.path) {
                continue;
            }
            std::fs::remove_file(&file.path).map_err(|e| {
                FetchError::Cache(format!("cannot remove {}: {e}", file.path.display()))
            })?;
            if let Some(name) = file.path.file_name().and_then(|n| n.to_str()) {
                self.verified
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&format!("sha256:{name}"));
            }
            total -= file.len;
        }
        Ok(total)
    }
}

/// The read gate: a model fetch is a signed GET, which is what
/// `presign_get` hands out.
fn require_read_gate(storage: &StorageConnectorConfig) -> Result<(), FetchError> {
    if storage.operations.presign_get {
        Ok(())
    } else {
        Err(FetchError::Gate(
            "the storage connector does not allow reads (operations.presign_get = false), and a \
             model fetch is a signed GET"
                .to_string(),
        ))
    }
}

/// A SigV4-signed request for `key` through `storage`, under the same
/// private-address posture as every other egress.
async fn signed(
    storage: &StorageConnectorConfig,
    client: &reqwest::Client,
    key: &str,
    method: &str,
    timeout: Duration,
) -> Result<reqwest::RequestBuilder, String> {
    let (scheme, host, path) = storage.address(Some(key))?;
    let url = format!("{scheme}://{host}{path}");
    if !storage.allow_private_urls
        && let Err(msg) = crate::validation::validate_url_not_private(&url).await
    {
        return Err(format!("SSRF protection: {msg}"));
    }
    let amz_date = sigv4::amz_date_now();
    let ctx = sigv4::SigningContext::for_storage(storage, &host, &path, &amz_date);
    let mut request = match method {
        "HEAD" => client.head(&url),
        _ => client.get(&url),
    }
    .timeout(timeout);
    for (name, value) in sigv4::sign_headers(&ctx, method) {
        request = request.header(name, value);
    }
    Ok(request)
}

/// Write `bytes` beside `path` and rename into place, so a reader never sees
/// a partial file under a digest's name.
fn write_atomically(cache_dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), FetchError> {
    std::fs::create_dir_all(cache_dir)
        .map_err(|e| FetchError::Cache(format!("cannot create {}: {e}", cache_dir.display())))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact");
    let temp = cache_dir.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4()));
    let result = std::fs::write(&temp, bytes)
        .and_then(|()| std::fs::rename(&temp, path))
        .map_err(|e| FetchError::Cache(format!("cannot write {}: {e}", path.display())));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Bump a cached file's modification time so the sweep sees it as recently
/// used. Best effort: a file whose time cannot be set is swept earlier, not
/// lost.
fn touch(path: &Path) {
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

struct CachedFile {
    path: PathBuf,
    len: u64,
    modified: SystemTime,
    temporary: bool,
}

/// Every regular file directly under `dir`. A directory that does not exist
/// is empty.
fn list_files(dir: &Path) -> Vec<CachedFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let path = entry.path();
            let temporary = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".tmp-"));
            Some(CachedFile {
                len: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                temporary,
                path,
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A bucket standing in for the connector's: serves `body` under every
    /// key, counts the GETs, and asserts every request arrived signed.
    pub(crate) struct Bucket {
        pub addr: std::net::SocketAddr,
        pub gets: Arc<AtomicUsize>,
    }

    pub(crate) async fn spawn_bucket(body: Vec<u8>, status: Option<u16>) -> Bucket {
        spawn_bucket_with_delay(body, status, Duration::ZERO).await
    }

    pub(crate) async fn spawn_bucket_with_delay(
        body: Vec<u8>,
        status: Option<u16>,
        get_delay: Duration,
    ) -> Bucket {
        use axum::http::{HeaderMap, StatusCode};
        let gets = Arc::new(AtomicUsize::new(0));
        let body = Arc::new(body);
        let assert_signed = |headers: &HeaderMap| {
            assert!(
                headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.starts_with("AWS4-HMAC-SHA256 Credential=")),
                "the request must arrive signed"
            );
            assert!(headers.contains_key("x-amz-date"), "x-amz-date missing");
        };
        let head_body = body.clone();
        let head = move |headers: HeaderMap| async move {
            assert_signed(&headers);
            let mut out = HeaderMap::new();
            if let Some(code) = status {
                return (StatusCode::from_u16(code).expect("status"), out);
            }
            out.insert(
                "content-length",
                head_body.len().to_string().parse().expect("len"),
            );
            out.insert("etag", "\"etag-1\"".parse().expect("etag"));
            (StatusCode::OK, out)
        };
        let get_gets = gets.clone();
        let get = move |headers: HeaderMap| async move {
            assert_signed(&headers);
            get_gets.fetch_add(1, Ordering::SeqCst);
            if !get_delay.is_zero() {
                tokio::time::sleep(get_delay).await;
            }
            if let Some(code) = status {
                return (StatusCode::from_u16(code).expect("status"), Vec::new());
            }
            (StatusCode::OK, body.to_vec())
        };
        let app = axum::Router::new().route("/{*path}", axum::routing::get(get).head(head));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        Bucket { addr, gets }
    }

    pub(crate) fn storage_config(addr: std::net::SocketAddr) -> StorageConnectorConfig {
        StorageConnectorConfig {
            provider: crate::connector::StorageProvider::S3,
            endpoint: format!("http://{addr}"),
            region: "us-east-1".to_string(),
            bucket: "models".to_string(),
            access_key: "AK".to_string(),
            secret_key: "sk".to_string(),
            session_token: None,
            // Virtual-hosted would prepend the bucket to 127.0.0.1 and
            // resolve nowhere; path-style is also what self-hosted stores
            // want.
            force_path_style: true,
            allow_private_urls: true, // tests use localhost
            timeout_ms: 5_000,
            operations: Default::default(),
        }
    }

    pub(crate) fn temp_cache_dir() -> PathBuf {
        std::env::temp_dir().join(format!("orion-model-cache-{}", uuid::Uuid::new_v4()))
    }

    pub(crate) fn artifact(body: &[u8]) -> ArtifactRef {
        ArtifactRef {
            connector: "bucket".to_string(),
            key: "models/c4-tiny.onnx".to_string(),
            digest: crate::crypto::sha256_digest(body),
            size: Some(body.len() as u64),
        }
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[tokio::test]
    async fn a_fetch_verifies_writes_and_then_hits_the_cache() {
        let body = b"not really onnx, but bytes".to_vec();
        let bucket = spawn_bucket(body.clone(), None).await;
        let storage = storage_config(bucket.addr);
        let client = reqwest::Client::new();
        let dir = temp_cache_dir();
        let store = ArtifactStore::new(&dir, 1 << 20);
        let artifact = artifact(&body);

        let path = store
            .fetch(
                &storage,
                &client,
                &artifact,
                1 << 20,
                Duration::from_secs(5),
            )
            .await
            .expect("fetches");
        assert_eq!(path, dir.join(&artifact.digest["sha256:".len()..]));
        assert_eq!(std::fs::read(&path).expect("written"), body);
        assert_eq!(bucket.gets.load(Ordering::SeqCst), 1);
        assert_eq!(names_in(&dir).len(), 1, "no temp file left behind");
        assert_eq!(store.cached_bytes(), body.len() as u64);

        // The same store: verified once, no hash, no GET.
        let again = store
            .fetch(
                &storage,
                &client,
                &artifact,
                1 << 20,
                Duration::from_secs(5),
            )
            .await
            .expect("hit");
        assert_eq!(again, path);
        assert_eq!(bucket.gets.load(Ordering::SeqCst), 1);

        // A fresh process over the same directory re-hashes and still does
        // not fetch.
        let fresh = ArtifactStore::new(&dir, 1 << 20);
        fresh
            .fetch(
                &storage,
                &client,
                &artifact,
                1 << 20,
                Duration::from_secs(5),
            )
            .await
            .expect("hit after re-hash");
        assert_eq!(bucket.gets.load(Ordering::SeqCst), 1);

        // A file that does not hash to its name is replaced.
        std::fs::write(&path, b"corrupted").expect("corrupt");
        let fresh = ArtifactStore::new(&dir, 1 << 20);
        fresh
            .fetch(
                &storage,
                &client,
                &artifact,
                1 << 20,
                Duration::from_secs(5),
            )
            .await
            .expect("refetched");
        assert_eq!(std::fs::read(&path).expect("rewritten"), body);
        assert_eq!(bucket.gets.load(Ordering::SeqCst), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_digest_mismatch_writes_nothing() {
        let body = b"served bytes".to_vec();
        let bucket = spawn_bucket(body.clone(), None).await;
        let storage = storage_config(bucket.addr);
        let client = reqwest::Client::new();
        let dir = temp_cache_dir();
        let store = ArtifactStore::new(&dir, 1 << 20);
        let mut artifact = artifact(b"the bytes the row expected");
        let err = store
            .fetch(
                &storage,
                &client,
                &artifact,
                1 << 20,
                Duration::from_secs(5),
            )
            .await
            .expect_err("mismatch");
        assert_eq!(err.stage(), "digest");
        assert!(
            matches!(&err, FetchError::DigestMismatch { claimed, computed }
                if *claimed == artifact.digest && *computed == crate::crypto::sha256_digest(&body)),
            "{err}"
        );
        assert!(err.to_string().contains("nothing was kept"), "{err}");
        assert!(names_in(&dir).is_empty(), "{:?}", names_in(&dir));
        assert_eq!(store.cached_bytes(), 0);

        artifact.digest = "sha256:short".to_string();
        let err = store
            .fetch(
                &storage,
                &client,
                &artifact,
                1 << 20,
                Duration::from_secs(5),
            )
            .await
            .expect_err("bad digest");
        assert_eq!(err.stage(), "digest");
        assert!(matches!(err, FetchError::BadDigest { .. }), "{err}");
        assert_eq!(
            bucket.gets.load(Ordering::SeqCst),
            1,
            "a bad claim never fetches"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_object_over_the_size_cap_is_refused_before_it_is_read() {
        let body = vec![7u8; 100];
        let bucket = spawn_bucket(body.clone(), None).await;
        let storage = storage_config(bucket.addr);
        let client = reqwest::Client::new();
        let dir = temp_cache_dir();
        let store = ArtifactStore::new(&dir, 1 << 20);
        let artifact = artifact(&body);
        let err = store
            .fetch(&storage, &client, &artifact, 50, Duration::from_secs(5))
            .await
            .expect_err("too large");
        assert_eq!(err.stage(), "size");
        assert!(
            matches!(
                err,
                FetchError::Size {
                    limit: 50,
                    declared: Some(100)
                }
            ),
            "{err}"
        );
        assert!(names_in(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn head_reports_size_and_etag_and_the_read_gate_applies_to_both_verbs() {
        let body = b"twelve bytes".to_vec();
        let bucket = spawn_bucket(body.clone(), None).await;
        let mut storage = storage_config(bucket.addr);
        let client = reqwest::Client::new();
        let dir = temp_cache_dir();
        let store = ArtifactStore::new(&dir, 1 << 20);
        let info = store
            .head(&storage, &client, "models/c4-tiny.onnx")
            .await
            .expect("head");
        assert_eq!(
            info,
            HeadInfo {
                size: Some(12),
                etag: Some("etag-1".to_string())
            }
        );

        storage.operations.presign_get = false;
        let err = store.head(&storage, &client, "k").await.expect_err("gated");
        assert_eq!(err.stage(), "gate");
        assert!(err.to_string().contains("presign_get"), "{err}");
        let err = store
            .fetch(
                &storage,
                &client,
                &artifact(&body),
                1 << 20,
                Duration::from_secs(5),
            )
            .await
            .expect_err("gated");
        assert_eq!(err.stage(), "gate");
        assert_eq!(bucket.gets.load(Ordering::SeqCst), 0);

        // A denied object is a head failure with the status.
        let denied = spawn_bucket(body.clone(), Some(403)).await;
        let err = store
            .head(&storage_config(denied.addr), &client, "k")
            .await
            .expect_err("403");
        assert_eq!(err.stage(), "head");
        assert!(err.to_string().contains("403"), "{err}");
        let err = store
            .fetch(
                &storage_config(denied.addr),
                &client,
                &artifact(&body),
                1 << 20,
                Duration::from_secs(5),
            )
            .await
            .expect_err("403");
        assert_eq!(err.stage(), "fetch");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sweep_removes_the_least_recently_used_first_and_spares_a_fresh_write() {
        let dir = temp_cache_dir();
        std::fs::create_dir_all(&dir).expect("dir");
        let now = SystemTime::now();
        for (name, age) in [("a", 3), ("b", 2), ("c", 1)] {
            let path = dir.join(name);
            std::fs::write(&path, [0u8; 10]).expect("write");
            std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open")
                .set_modified(now - Duration::from_secs(age * 3600))
                .expect("mtime");
        }
        // A temp file being written right now is not a candidate.
        std::fs::write(dir.join(".d.tmp-1"), [0u8; 10]).expect("temp");

        let store = ArtifactStore::new(&dir, 25);
        assert_eq!(store.cached_bytes(), 40);
        let remaining = store.sweep().expect("sweep");
        assert_eq!(remaining, 20);
        assert_eq!(names_in(&dir), [".d.tmp-1", "b", "c"]);

        // The file a fetch just wrote survives a ceiling it alone exceeds.
        let store = ArtifactStore::new(&dir, 5);
        let keep = dir.join("b");
        let remaining = store.sweep_keeping(Some(&keep)).expect("sweep");
        assert_eq!(remaining, 10);
        assert_eq!(names_in(&dir), [".d.tmp-1", "b"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_cache_directory_is_empty() {
        let store = ArtifactStore::new(temp_cache_dir(), 10);
        assert_eq!(store.cached_bytes(), 0);
        assert_eq!(store.sweep().expect("nothing to sweep"), 0);
        assert_eq!(
            store
                .path_for("sha256:abc")
                .file_name()
                .and_then(|n| n.to_str()),
            Some("abc")
        );
    }

    #[test]
    fn the_artifact_reference_round_trips_without_an_optional_size() {
        let text = r#"{"connector":"bucket","key":"k","digest":"sha256:00"}"#;
        let parsed: ArtifactRef = serde_json::from_str(text).expect("parses");
        assert_eq!(parsed.size, None);
        assert_eq!(serde_json::to_string(&parsed).expect("serialises"), text);
    }
}
