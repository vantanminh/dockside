use std::collections::BTreeMap;

use dockside_core::model::{
    BlobsExistReq, BlobsExistResp, ChangesetRecord, DeploymentRecord, HealthResp,
    ImportRevisionReq, LeaseRecord, OpenChangesetReq, OpenChangesetResp, ReleasePlan, ReleaseReq,
    RevisionRecord, RollbackReq, SequenceNextResp, SnapshotRecord, SnapshotReq, SubmitChangesetReq,
    TreeFlatResp,
};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("server returned HTTP {status}: {body}")]
    Server { status: u16, body: String },

    #[error("core error: {0}")]
    Core(#[from] dockside_core::CoreError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Message(String),
}

pub type CliResult<T> = Result<T, CliError>;

#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

impl ApiClient {
    pub fn new(base_url: String, token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        }
    }

    fn req(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut r = self
            .http
            .request(method, format!("{}{}", self.base_url, path));
        if let Some(ref t) = self.token {
            r = r.bearer_auth(t);
        }
        r
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> CliResult<T> {
        let resp = self.req(Method::GET, path).send().await?;
        parse_json(resp).await
    }

    async fn post_json<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        path: &str,
        body: &Req,
    ) -> CliResult<Resp> {
        let resp = self.req(Method::POST, path).json(body).send().await?;
        parse_json(resp).await
    }

    pub async fn health(&self) -> CliResult<HealthResp> {
        self.get_json("/v1/health").await
    }

    pub async fn missing_blobs(&self, hashes: Vec<String>) -> CliResult<Vec<String>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let resp: BlobsExistResp = self
            .post_json("/v1/blobs/missing", &BlobsExistReq { hashes })
            .await?;
        Ok(resp.missing)
    }

    pub async fn upload_blob(&self, hash: &str, raw: Vec<u8>) -> CliResult<()> {
        let resp = self
            .req(Method::PUT, &format!("/v1/blobs/{hash}"))
            .body(raw)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(CliError::Server {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        Ok(())
    }

    pub async fn download_blob(&self, hash: &str) -> CliResult<Vec<u8>> {
        let resp = self
            .req(Method::GET, &format!("/v1/blobs/{hash}"))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(CliError::Server {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// Upload every blob the server does not have yet.
    ///
    /// The existence check is batched (a proxy such as Cloudflare cuts a request after ~100 s, and the
    /// server checks hashes one by one) and uploads run with bounded concurrency.
    pub async fn upload_missing_map(&self, blobs: &BTreeMap<String, Vec<u8>>) -> CliResult<usize> {
        const CHECK_BATCH: usize = 64;
        const UPLOAD_CONCURRENCY: usize = 8;

        let hashes: Vec<String> = blobs.keys().cloned().collect();
        let mut missing: Vec<String> = Vec::new();
        for chunk in hashes.chunks(CHECK_BATCH) {
            missing.extend(self.missing_blobs(chunk.to_vec()).await?);
        }
        let count = missing.len();

        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(UPLOAD_CONCURRENCY));
        let mut tasks = tokio::task::JoinSet::new();
        for h in missing {
            let Some(raw) = blobs.get(&h) else { continue };
            let raw = raw.clone();
            let client = self.clone();
            let gate = std::sync::Arc::clone(&gate);
            tasks.spawn(async move {
                let _permit = gate
                    .acquire_owned()
                    .await
                    .map_err(|e| CliError::Message(format!("upload gate closed: {e}")))?;
                client.upload_blob(&h, raw).await
            });
        }
        while let Some(joined) = tasks.join_next().await {
            joined.map_err(|e| CliError::Message(format!("upload task failed: {e}")))??;
        }
        Ok(count)
    }

    pub async fn import_revision(&self, req: &ImportRevisionReq) -> CliResult<RevisionRecord> {
        match self.post_json("/v1/import", req).await {
            // A proxy (e.g. Cloudflare 524) may cut the connection while the server keeps working:
            // the server finishes the import regardless, so look the new revision up.
            Err(CliError::Server {
                status: 502 | 504 | 522 | 524,
                ..
            }) => self.wait_for_revision(&req.summary).await,
            other => other,
        }
    }

    async fn wait_for_revision(&self, summary: &str) -> CliResult<RevisionRecord> {
        for _ in 0..90 {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let revs: Vec<RevisionRecord> = self.get_json("/v1/revisions?limit=20").await?;
            if let Some(found) = revs.into_iter().find(|r| r.summary == summary) {
                return Ok(found);
            }
        }
        Err(CliError::Message(
            "import was cut by a proxy and no matching revision appeared within 15 minutes"
                .to_string(),
        ))
    }

    pub async fn get_tree(&self, rev_id: i64, scopes: &[String]) -> CliResult<TreeFlatResp> {
        let path = if scopes.is_empty() {
            format!("/v1/revisions/{rev_id}/tree")
        } else {
            format!("/v1/revisions/{rev_id}/tree?scope={}", scopes.join(","))
        };
        self.get_json(&path).await
    }

    pub async fn list_revisions(&self, limit: i64) -> CliResult<Vec<RevisionRecord>> {
        self.get_json(&format!("/v1/revisions?limit={limit}")).await
    }

    pub async fn open_changeset(&self, req: &OpenChangesetReq) -> CliResult<OpenChangesetResp> {
        self.post_json("/v1/changesets", req).await
    }

    pub async fn get_changeset(&self, cs_id: &str) -> CliResult<ChangesetRecord> {
        self.get_json(&format!("/v1/changesets/{cs_id}")).await
    }

    pub async fn list_changesets(
        &self,
        status: Option<&str>,
        limit: i64,
    ) -> CliResult<Vec<ChangesetRecord>> {
        let path = match status {
            Some(st) => format!("/v1/changesets?status={st}&limit={limit}"),
            None => format!("/v1/changesets?limit={limit}"),
        };
        self.get_json(&path).await
    }

    pub async fn add_snapshot(&self, cs_id: &str, req: &SnapshotReq) -> CliResult<SnapshotRecord> {
        self.post_json(&format!("/v1/changesets/{cs_id}/snapshots"), req)
            .await
    }

    pub async fn list_snapshots(&self, cs_id: &str) -> CliResult<Vec<SnapshotRecord>> {
        self.get_json(&format!("/v1/changesets/{cs_id}/snapshots"))
            .await
    }

    pub async fn submit_changeset(
        &self,
        cs_id: &str,
        req: &SubmitChangesetReq,
        wait: bool,
    ) -> CliResult<ChangesetRecord> {
        self.post_json(&format!("/v1/changesets/{cs_id}/submit?wait={wait}"), req)
            .await
    }

    pub async fn abandon_changeset(&self, cs_id: &str) -> CliResult<ChangesetRecord> {
        self.post_json(&format!("/v1/changesets/{cs_id}/abandon"), &())
            .await
    }

    pub async fn list_leases(&self) -> CliResult<Vec<LeaseRecord>> {
        self.get_json("/v1/leases").await
    }

    pub async fn next_sequence(&self, kind: &str) -> CliResult<SequenceNextResp> {
        self.post_json(&format!("/v1/sequences/{kind}/next"), &())
            .await
    }

    pub async fn list_deployments(&self, limit: i64) -> CliResult<Vec<DeploymentRecord>> {
        self.get_json(&format!("/v1/deployments?limit={limit}"))
            .await
    }

    pub async fn trigger_release(&self, req: &ReleaseReq) -> CliResult<ReleasePlan> {
        self.post_json("/v1/releases", req).await
    }

    pub async fn trigger_rollback(&self, req: &RollbackReq) -> CliResult<ReleasePlan> {
        self.post_json("/v1/rollbacks", req).await
    }
}

async fn parse_json<T: DeserializeOwned>(resp: reqwest::Response) -> CliResult<T> {
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        let status_code = status.as_u16();
        let body = String::from_utf8_lossy(&bytes).into_owned();
        if status == StatusCode::UNAUTHORIZED {
            return Err(CliError::Message(
                "Unauthorized (401): set --token or DOCKSIDE_TOKEN".to_string(),
            ));
        }
        return Err(CliError::Server {
            status: status_code,
            body,
        });
    }
    Ok(serde_json::from_slice(&bytes)?)
}
