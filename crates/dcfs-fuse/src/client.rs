//! Server client abstraction for DCFS API.

use async_trait::async_trait;
use dcfs_protocol::{
    CreateNodeRequest, FsInfoResponse, ListChildrenResponse, NodeResponse, PatchNodeRequest,
    RenameNodeRequest,
};
use std::time::Duration;
use uuid::Uuid;

/// Abstract interface to the DCFS server.
///
/// This trait allows for different implementations (HTTP, fake for testing, etc.)
#[async_trait]
pub trait ServerClient: Send + Sync + 'static {
    /// How the server chunks files.
    async fn fs_info(&self) -> Result<FsInfoResponse, ClientError>;

    /// Get node metadata by ID.
    async fn get_node(&self, node_id: Uuid) -> Result<NodeResponse, ClientError>;

    /// Get node metadata by path.
    async fn get_node_by_path(&self, path: &str) -> Result<NodeResponse, ClientError>;

    /// Resolve one name inside a directory, without listing it.
    async fn find_child(&self, parent_id: Uuid, name: &[u8]) -> Result<NodeResponse, ClientError>;

    /// List children of a directory.
    async fn list_children(
        &self,
        parent_id: Uuid,
        limit: Option<u32>,
        offset: Option<u32>,
    ) -> Result<ListChildrenResponse, ClientError>;

    /// Create a new node (file or directory).
    async fn create_node(&self, req: CreateNodeRequest) -> Result<NodeResponse, ClientError>;

    /// Update node attributes.
    async fn patch_node(
        &self,
        node_id: Uuid,
        req: PatchNodeRequest,
    ) -> Result<NodeResponse, ClientError>;

    /// Rename a node.
    async fn rename_node(
        &self,
        node_id: Uuid,
        req: RenameNodeRequest,
    ) -> Result<NodeResponse, ClientError>;

    /// Delete a node.
    async fn delete_node(&self, node_id: Uuid) -> Result<(), ClientError>;

    /// Read file data.
    async fn read_file(
        &self,
        node_id: Uuid,
        offset: u64,
        size: u64,
    ) -> Result<Vec<u8>, ClientError>;

    /// Read, and say whether the bytes may be cached.
    ///
    /// They may not while a write to the file is open: the same version id
    /// serves different bytes as the write goes on, and if it is abandoned the
    /// file goes back to what was committed. Only a committed version is
    /// immutable enough to keep.
    async fn read_file_cacheable(
        &self,
        node_id: Uuid,
        offset: u64,
        size: u64,
    ) -> Result<(Vec<u8>, bool), ClientError> {
        self.read_file(node_id, offset, size)
            .await
            .map(|bytes| (bytes, true))
    }

    /// Write file data.
    async fn write_file(&self, node_id: Uuid, offset: u64, data: &[u8])
        -> Result<u64, ClientError>;

    /// Sync file data to storage.
    async fn sync_file(&self, node_id: Uuid) -> Result<(), ClientError>;
}

/// Client errors.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("not found")]
    NotFound,

    #[error("already exists")]
    AlreadyExists,

    #[error("invalid name")]
    InvalidName,

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("directory not empty")]
    DirectoryNotEmpty,

    #[error("unauthorized")]
    Unauthorized,

    #[error("backend unavailable")]
    BackendUnavailable,

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// How long a request keeps trying before the caller sees an error.
///
/// A mount is a filesystem: an application writing to it has no way to retry
/// for itself, so a moment of lost connectivity must look like a slow write,
/// not a failed one. Past this the kernel gets an error, because blocking a
/// process forever is worse than telling it the truth.
fn default_retry() -> dcfs_core::RetryConfig {
    dcfs_core::RetryConfig::new()
        .with_max_attempts(6)
        .with_initial_backoff(Duration::from_millis(250))
        .with_max_backoff(Duration::from_secs(8))
        .with_multiplier(2.0)
        // Jitter, because every request in flight when a server comes back
        // would otherwise retry in the same instant.
        .with_jitter(true)
}

/// HTTP client implementation.
pub struct HttpClient {
    client: reqwest::Client,
    base_url: String,
    retry: dcfs_core::RetryConfig,
}

impl HttpClient {
    /// Create a client with no credentials. Only reaches a server started
    /// without `API_TOKEN`, which the released binary refuses to do.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_token(base_url, None)
    }

    /// Create a client that presents `token` as a bearer credential.
    ///
    /// The token is baked into a default header so no call site can forget it.
    /// It is never logged: reqwest marks `Authorization` sensitive.
    pub fn with_token(base_url: impl Into<String>, token: Option<&str>) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(token) = token {
            let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .expect("API token must be a valid header value");
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        Self {
            client: reqwest::Client::builder()
                .default_headers(headers)
                .build()
                .expect("failed to build the HTTP client"),
            base_url: base_url.into(),
            retry: default_retry(),
        }
    }

    /// Replace the retry policy. Tests use this to keep the backoff short.
    pub fn with_retry(mut self, retry: dcfs_core::RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1{}", self.base_url.trim_end_matches('/'), path)
    }
}

/// Send a request, retrying the failures that a moment of lost connectivity
/// produces: no response at all, a 429, or a 5xx.
///
/// Every call site is safe to replay. Reads are trivially so; a write carries
/// its own offset, so writing the same bytes twice lands them in the same
/// place; a create carries an idempotency key that the server uses as the new
/// node's id, so a replay collides with itself rather than making a second
/// node; a delete or a rename replayed after it succeeded is a no-op the
/// callers below translate.
async fn send_retrying(
    retry: &dcfs_core::RetryConfig,
    url: &str,
    build: impl Fn() -> reqwest::RequestBuilder,
) -> Result<(reqwest::Response, bool), ClientError> {
    let mut retried = false;
    for attempt in 0..retry.max_attempts {
        let outcome = build().send().await;
        let wait = match &outcome {
            Ok(resp) if resp.status().is_server_error() || resp.status().as_u16() == 429 => {
                Some(format!("HTTP {}", resp.status()))
            }
            Ok(_) => None,
            Err(e) => Some(e.to_string()),
        };

        let Some(reason) = wait else {
            return Ok((outcome.expect("checked above"), retried));
        };
        if attempt + 1 == retry.max_attempts {
            break;
        }

        let backoff = retry.backoff_duration(attempt);
        tracing::warn!(
            attempt = attempt + 1,
            backoff_ms = backoff.as_millis(),
            "{url} did not answer ({reason}); retrying"
        );
        tokio::time::sleep(backoff).await;
        retried = true;
    }

    // Out of attempts: one last try, whose result the caller sees either way.
    match build().send().await {
        Ok(resp) => Ok((resp, retried)),
        Err(e) => Err(ClientError::Http(e.to_string())),
    }
}

/// Turn an unexpected status into an error, logging it first.
///
/// The kernel only ever sees an errno, so without this a server-side rejection
/// reaches the user as a bare "Input/output error" with nothing to go on.
fn unexpected(method: &str, url: &str, status: u16) -> ClientError {
    tracing::warn!("{method} {url} -> HTTP {status}");
    ClientError::Http(format!("HTTP {status}"))
}

#[async_trait]
impl ServerClient for HttpClient {
    async fn fs_info(&self) -> Result<FsInfoResponse, ClientError> {
        let url = self.url("/fs");
        let (resp, _) = send_retrying(&self.retry, &url, || self.client.get(&url)).await?;
        match resp.status().as_u16() {
            200 => resp
                .json()
                .await
                .map_err(|e| ClientError::Http(e.to_string())),
            401 | 403 => Err(ClientError::Unauthorized),
            status => Err(unexpected("request", &url, status)),
        }
    }

    async fn get_node(&self, node_id: Uuid) -> Result<NodeResponse, ClientError> {
        let url = self.url(&format!("/nodes/{}", node_id));
        let (resp, _) = send_retrying(&self.retry, &url, || self.client.get(&url)).await?;

        match resp.status().as_u16() {
            200 => resp
                .json()
                .await
                .map_err(|e| ClientError::Http(e.to_string())),
            401 | 403 => Err(ClientError::Unauthorized),
            404 => Err(ClientError::NotFound),
            status => Err(unexpected("request", &url, status)),
        }
    }

    async fn get_node_by_path(&self, path: &str) -> Result<NodeResponse, ClientError> {
        // ponytail: only used for "/" at mount time, so the path is not
        // percent-encoded; encode it here before resolving arbitrary paths.
        let url = self.url(&format!("/nodes/resolve?path={}", path));
        let (resp, _) = send_retrying(&self.retry, &url, || self.client.get(&url)).await?;

        match resp.status().as_u16() {
            200 => resp
                .json()
                .await
                .map_err(|e| ClientError::Http(e.to_string())),
            401 | 403 => Err(ClientError::Unauthorized),
            404 => Err(ClientError::NotFound),
            status => Err(unexpected("request", &url, status)),
        }
    }

    async fn find_child(&self, parent_id: Uuid, name: &[u8]) -> Result<NodeResponse, ClientError> {
        let encoded = dcfs_protocol::NameBytes(name.to_vec()).to_base64();
        let url = self.url(&format!("/nodes/{parent_id}/children/{encoded}"));
        let (resp, _) = send_retrying(&self.retry, &url, || self.client.get(&url)).await?;
        match resp.status().as_u16() {
            200 => resp
                .json()
                .await
                .map_err(|e| ClientError::Http(e.to_string())),
            401 | 403 => Err(ClientError::Unauthorized),
            404 => Err(ClientError::NotFound),
            status => Err(unexpected("request", &url, status)),
        }
    }

    async fn list_children(
        &self,
        parent_id: Uuid,
        limit: Option<u32>,
        offset: Option<u32>,
    ) -> Result<ListChildrenResponse, ClientError> {
        let mut url = self.url(&format!("/nodes/{}/children", parent_id));
        let mut params = Vec::new();
        if let Some(l) = limit {
            params.push(format!("limit={}", l));
        }
        if let Some(o) = offset {
            params.push(format!("offset={}", o));
        }
        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }

        let (resp, _) = send_retrying(&self.retry, &url, || self.client.get(&url)).await?;

        match resp.status().as_u16() {
            200 => resp
                .json()
                .await
                .map_err(|e| ClientError::Http(e.to_string())),
            401 | 403 => Err(ClientError::Unauthorized),
            404 => Err(ClientError::NotFound),
            status => Err(unexpected("request", &url, status)),
        }
    }

    async fn create_node(&self, req: CreateNodeRequest) -> Result<NodeResponse, ClientError> {
        let url = self.url("/nodes");
        let (resp, retried) =
            send_retrying(&self.retry, &url, || self.client.post(&url).json(&req)).await?;

        match resp.status().as_u16() {
            201 | 200 => resp
                .json()
                .await
                .map_err(|e| ClientError::Http(e.to_string())),
            401 | 403 => Err(ClientError::Unauthorized),
            409 if retried => {
                // The first attempt may have succeeded with its answer lost on
                // the way back. The server uses the idempotency key as the new
                // node's id, so the collision is with ourselves if that node is
                // now there — which makes the retry a success, not a conflict.
                match self.get_node(req.idempotency_key).await {
                    Ok(node) if node.name.as_bytes() == req.name.as_bytes() => Ok(node),
                    _ => Err(ClientError::AlreadyExists),
                }
            }
            409 => Err(ClientError::AlreadyExists),
            400 => Err(ClientError::InvalidName),
            status => Err(unexpected("request", &url, status)),
        }
    }

    async fn patch_node(
        &self,
        node_id: Uuid,
        req: PatchNodeRequest,
    ) -> Result<NodeResponse, ClientError> {
        let url = self.url(&format!("/nodes/{}", node_id));
        let (resp, _) =
            send_retrying(&self.retry, &url, || self.client.patch(&url).json(&req)).await?;

        match resp.status().as_u16() {
            200 => resp
                .json()
                .await
                .map_err(|e| ClientError::Http(e.to_string())),
            401 | 403 => Err(ClientError::Unauthorized),
            404 => Err(ClientError::NotFound),
            409 => Err(ClientError::Conflict("conflict".into())),
            status => Err(unexpected("request", &url, status)),
        }
    }

    async fn rename_node(
        &self,
        node_id: Uuid,
        req: RenameNodeRequest,
    ) -> Result<NodeResponse, ClientError> {
        let url = self.url(&format!("/nodes/{}/rename", node_id));
        // A replayed rename asks for the same move again, which the server
        // treats as a no-op, so this needs no special handling.
        let (resp, _) =
            send_retrying(&self.retry, &url, || self.client.post(&url).json(&req)).await?;

        match resp.status().as_u16() {
            200 => resp
                .json()
                .await
                .map_err(|e| ClientError::Http(e.to_string())),
            401 | 403 => Err(ClientError::Unauthorized),
            404 => Err(ClientError::NotFound),
            // A rename onto a non-empty directory needs ENOTEMPTY, not EEXIST,
            // so the two 409s have to be told apart by their error code.
            409 => {
                let body = resp.text().await.unwrap_or_default();
                if body.contains("directory_not_empty") {
                    Err(ClientError::DirectoryNotEmpty)
                } else {
                    Err(ClientError::AlreadyExists)
                }
            }
            status => Err(unexpected("request", &url, status)),
        }
    }

    async fn delete_node(&self, node_id: Uuid) -> Result<(), ClientError> {
        let url = self.url(&format!("/nodes/{}", node_id));
        let (resp, retried) = send_retrying(&self.retry, &url, || self.client.delete(&url)).await?;

        match resp.status().as_u16() {
            204 | 200 => Ok(()),
            401 | 403 => Err(ClientError::Unauthorized),
            // A delete replayed after it already succeeded finds nothing left,
            // which is the outcome the caller asked for.
            404 if retried => Ok(()),
            404 => Err(ClientError::NotFound),
            // rmdir on a non-empty directory; the kernel needs ENOTEMPTY.
            409 => Err(ClientError::DirectoryNotEmpty),
            status => Err(unexpected("request", &url, status)),
        }
    }

    async fn read_file(
        &self,
        node_id: Uuid,
        offset: u64,
        size: u64,
    ) -> Result<Vec<u8>, ClientError> {
        let url = self.url(&format!(
            "/nodes/{}/data?offset={}&size={}",
            node_id, offset, size
        ));
        let (resp, _) = send_retrying(&self.retry, &url, || self.client.get(&url)).await?;

        match resp.status().as_u16() {
            200 => resp
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| ClientError::Http(e.to_string())),
            401 | 403 => Err(ClientError::Unauthorized),
            404 => Err(ClientError::NotFound),
            status => Err(unexpected("request", &url, status)),
        }
    }

    async fn read_file_cacheable(
        &self,
        node_id: Uuid,
        offset: u64,
        size: u64,
    ) -> Result<(Vec<u8>, bool), ClientError> {
        let url = self.url(&format!(
            "/nodes/{}/data?offset={}&size={}",
            node_id, offset, size
        ));
        let (resp, _) = send_retrying(&self.retry, &url, || self.client.get(&url)).await?;

        match resp.status().as_u16() {
            200 => {
                // Absent header: an older server, which committed every write,
                // so what it serves is immutable.
                let cacheable = resp
                    .headers()
                    .get("x-dfs-committed")
                    .map(|v| v.as_bytes() != b"false")
                    .unwrap_or(true);
                resp.bytes()
                    .await
                    .map(|b| (b.to_vec(), cacheable))
                    .map_err(|e| ClientError::Http(e.to_string()))
            }
            401 | 403 => Err(ClientError::Unauthorized),
            404 => Err(ClientError::NotFound),
            status => Err(unexpected("request", &url, status)),
        }
    }

    async fn write_file(
        &self,
        node_id: Uuid,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, ClientError> {
        let url = self.url(&format!("/nodes/{}/data?offset={}", node_id, offset));
        let (resp, _) = send_retrying(&self.retry, &url, || {
            self.client.put(&url).body(data.to_vec())
        })
        .await?;

        match resp.status().as_u16() {
            200 | 204 => Ok(data.len() as u64),
            401 | 403 => Err(ClientError::Unauthorized),
            404 => Err(ClientError::NotFound),
            status => Err(unexpected("request", &url, status)),
        }
    }

    async fn sync_file(&self, node_id: Uuid) -> Result<(), ClientError> {
        let url = self.url(&format!("/nodes/{}/sync", node_id));
        let (resp, _) = send_retrying(&self.retry, &url, || self.client.post(&url)).await?;

        match resp.status().as_u16() {
            200 | 204 => Ok(()),
            401 | 403 => Err(ClientError::Unauthorized),
            404 => Err(ClientError::NotFound),
            status => Err(unexpected("request", &url, status)),
        }
    }
}
