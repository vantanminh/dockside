//! Content-Addressed Storage (CAS): local disk L1 cache + optional Cloudflare R2 L2 backend.
//!
//! Blobs are hashed with BLAKE3 on raw bytes, compressed with zstd, and verified on every read.
//! R2 uses a minimal hand-rolled AWS SigV4 client.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::macros::format_description;

use crate::error::{CoreError, CoreResult};
use crate::hash::{compress_zstd, decompress_zstd, hash_bytes, is_valid_hash};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct R2Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

#[derive(Clone)]
struct R2Client {
    http: reqwest::Client,
    cfg: R2Config,
    base: Url,
}

impl R2Client {
    fn new(cfg: R2Config) -> CoreResult<Self> {
        let base = Url::parse(cfg.endpoint.trim_end_matches('/'))
            .map_err(|e| CoreError::Config(format!("invalid R2 endpoint: {e}")))?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| CoreError::RemoteStorage(e.to_string()))?;
        Ok(Self { http, cfg, base })
    }

    fn object_key(&self, hash: &str) -> String {
        let shard = &hash[..2];
        let prefix = self.cfg.prefix.trim_matches('/');
        if prefix.is_empty() {
            format!("{shard}/{hash}.zst")
        } else {
            format!("{prefix}/{shard}/{hash}.zst")
        }
    }

    fn object_url(&self, key: &str) -> CoreResult<(Url, String, String)> {
        let mut url = self.base.clone();
        let encoded_key = uri_encode(key, true);
        let path = format!("/{}/{}", uri_encode(&self.cfg.bucket, false), encoded_key);
        url.set_path(&path);
        let host = url
            .host_str()
            .ok_or_else(|| CoreError::Config("R2 endpoint missing host".to_string()))?
            .to_string();
        let host_header = match url.port() {
            Some(p) => format!("{host}:{p}"),
            None => host,
        };
        Ok((url, host_header, path))
    }

    async fn put_compressed(&self, hash: &str, compressed: &[u8]) -> CoreResult<()> {
        let key = self.object_key(hash);
        let (url, host, path) = self.object_url(&key)?;
        let payload_sha = sha256_hex(compressed);
        let signed = sign_sigv4(
            &self.cfg,
            "PUT",
            &host,
            &path,
            "",
            &payload_sha,
            OffsetDateTime::now_utc(),
        );
        let resp = self
            .http
            .request(Method::PUT, url)
            .header("host", &host)
            .header("x-amz-content-sha256", &signed.payload_hash)
            .header("x-amz-date", &signed.stamp)
            .header("authorization", &signed.authorization)
            .header("content-type", "application/zstd")
            .body(Bytes::copy_from_slice(compressed))
            .send()
            .await
            .map_err(|e| CoreError::RemoteStorage(format!("R2 PUT failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(CoreError::RemoteStorage(format!(
                "R2 PUT returned HTTP {}",
                resp.status().as_u16()
            )));
        }
        Ok(())
    }

    async fn get_compressed(&self, hash: &str) -> CoreResult<Option<Vec<u8>>> {
        let key = self.object_key(hash);
        let (url, host, path) = self.object_url(&key)?;
        let empty_sha = sha256_hex(b"");
        let signed = sign_sigv4(
            &self.cfg,
            "GET",
            &host,
            &path,
            "",
            &empty_sha,
            OffsetDateTime::now_utc(),
        );
        let resp = self
            .http
            .request(Method::GET, url)
            .header("host", &host)
            .header("x-amz-content-sha256", &signed.payload_hash)
            .header("x-amz-date", &signed.stamp)
            .header("authorization", &signed.authorization)
            .send()
            .await
            .map_err(|e| CoreError::RemoteStorage(format!("R2 GET failed: {e}")))?;

        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(CoreError::RemoteStorage(format!(
                "R2 GET returned HTTP {}",
                resp.status().as_u16()
            )));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| CoreError::RemoteStorage(format!("R2 read body failed: {e}")))?;
        Ok(Some(body.to_vec()))
    }

    async fn head_exists(&self, hash: &str) -> CoreResult<bool> {
        let key = self.object_key(hash);
        let (url, host, path) = self.object_url(&key)?;
        let empty_sha = sha256_hex(b"");
        let signed = sign_sigv4(
            &self.cfg,
            "HEAD",
            &host,
            &path,
            "",
            &empty_sha,
            OffsetDateTime::now_utc(),
        );
        let resp = self
            .http
            .request(Method::HEAD, url)
            .header("host", &host)
            .header("x-amz-content-sha256", &signed.payload_hash)
            .header("x-amz-date", &signed.stamp)
            .header("authorization", &signed.authorization)
            .send()
            .await
            .map_err(|e| CoreError::RemoteStorage(format!("R2 HEAD failed: {e}")))?;

        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if resp.status().is_success() {
            return Ok(true);
        }
        Err(CoreError::RemoteStorage(format!(
            "R2 HEAD returned HTTP {}",
            resp.status().as_u16()
        )))
    }
}

#[derive(Clone)]
pub struct CasStore {
    local_dir: PathBuf,
    r2: Option<R2Client>,
}

impl CasStore {
    pub fn open(local_dir: impl AsRef<Path>, r2_cfg: Option<R2Config>) -> CoreResult<Self> {
        let dir = local_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let r2 = match r2_cfg {
            Some(cfg) => Some(R2Client::new(cfg)?),
            None => None,
        };
        Ok(Self { local_dir: dir, r2 })
    }

    pub fn is_r2_enabled(&self) -> bool {
        self.r2.is_some()
    }

    pub fn local_dir(&self) -> &Path {
        &self.local_dir
    }

    fn local_path(&self, hash: &str) -> CoreResult<PathBuf> {
        if !is_valid_hash(hash) {
            return Err(CoreError::InvalidHash(hash.to_string()));
        }
        Ok(self.local_dir.join(&hash[..2]).join(format!("{hash}.zst")))
    }

    pub fn exists_local(&self, hash: &str) -> bool {
        self.local_path(hash).map(|p| p.is_file()).unwrap_or(false)
    }

    pub async fn exists(&self, hash: &str) -> CoreResult<bool> {
        if self.exists_local(hash) {
            return Ok(true);
        }
        if let Some(r2) = &self.r2 {
            return r2.head_exists(hash).await;
        }
        Ok(false)
    }

    /// Store raw bytes in L1 disk cache (and write-through to R2 if configured).
    /// Returns the BLAKE3 lowercase hex digest.
    pub async fn put_bytes(&self, raw: &[u8]) -> CoreResult<String> {
        let hash = hash_bytes(raw);
        let dst = self.local_path(&hash)?;
        let already_local = dst.is_file();

        let compressed = if !already_local || self.r2.is_some() {
            Some(compress_zstd(raw)?)
        } else {
            None
        };

        if !already_local && let Some(ref payload) = compressed {
            write_atomic(&dst, payload)?;
        }

        if let (Some(r2), Some(payload)) = (&self.r2, compressed.as_ref())
            && !r2.head_exists(&hash).await?
        {
            r2.put_compressed(&hash, payload).await?;
        }

        Ok(hash)
    }

    /// Synchronous local-only put (useful when syncing local cache or unit tests without R2).
    pub fn put_bytes_local(&self, raw: &[u8]) -> CoreResult<String> {
        let hash = hash_bytes(raw);
        let dst = self.local_path(&hash)?;
        if !dst.is_file() {
            let compressed = compress_zstd(raw)?;
            write_atomic(&dst, &compressed)?;
        }
        Ok(hash)
    }

    /// Retrieve raw bytes by BLAKE3 hash (reads L1, falls back to R2 and populates L1).
    pub async fn get_bytes(&self, hash: &str) -> CoreResult<Vec<u8>> {
        let path = self.local_path(hash)?;
        let compressed = if path.is_file() {
            std::fs::read(&path)?
        } else if let Some(r2) = &self.r2 {
            match r2.get_compressed(hash).await? {
                Some(bytes) => {
                    write_atomic(&path, &bytes)?;
                    bytes
                }
                None => return Err(CoreError::NotFound(hash.to_string())),
            }
        } else {
            return Err(CoreError::NotFound(hash.to_string()));
        };

        let raw = decompress_zstd(&compressed)?;
        let actual = hash_bytes(&raw);
        if actual != hash {
            return Err(CoreError::HashMismatch {
                expected: hash.to_string(),
                actual,
            });
        }
        Ok(raw)
    }

    pub async fn put_json<T: Serialize>(&self, value: &T) -> CoreResult<String> {
        let bytes = serde_json::to_vec(value).map_err(|e| CoreError::Serialize(e.to_string()))?;
        self.put_bytes(&bytes).await
    }

    pub async fn get_json<T: DeserializeOwned>(&self, hash: &str) -> CoreResult<T> {
        let bytes = self.get_bytes(hash).await?;
        serde_json::from_slice(&bytes).map_err(|e| CoreError::Serialize(e.to_string()))
    }
}

fn write_atomic(dst: &Path, data: &[u8]) -> CoreResult<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dst.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    // On Windows rename fails if dst exists (race between concurrent writers of identical CAS blob).
    match std::fs::rename(&tmp, dst) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            if dst.is_file() {
                Ok(())
            } else {
                Err(CoreError::Io(e))
            }
        }
    }
}

fn hmac_sha256(key: &[u8], data: &str) -> Vec<u8> {
    HmacSha256::new_from_slice(key).map_or_else(
        |_| Vec::new(),
        |mut m| {
            m.update(data.as_bytes());
            m.finalize().into_bytes().to_vec()
        },
    )
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

struct SignedHeaders {
    authorization: String,
    payload_hash: String,
    stamp: String,
}

fn sign_sigv4(
    cfg: &R2Config,
    method: &str,
    host: &str,
    path: &str,
    query: &str,
    payload_hash: &str,
    now: OffsetDateTime,
) -> SignedHeaders {
    let utc = now.to_offset(time::UtcOffset::UTC);
    let date = utc
        .format(format_description!("[year][month][day]"))
        .unwrap_or_default();
    let stamp = utc
        .format(format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .unwrap_or_default();
    let signed_names = "host;x-amz-content-sha256;x-amz-date";
    let canonical = format!(
        "{method}\n{path}\n{query}\nhost:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{stamp}\n\n{signed_names}\n{payload_hash}"
    );
    let scope = format!("{date}/{}/s3/aws4_request", cfg.region);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let k_date = hmac_sha256(format!("AWS4{}", cfg.secret_access_key).as_bytes(), &date);
    let k_region = hmac_sha256(&k_date, &cfg.region);
    let k_service = hmac_sha256(&k_region, "s3");
    let k_signing = hmac_sha256(&k_service, "aws4_request");
    let signature = hex::encode(hmac_sha256(&k_signing, &to_sign));
    SignedHeaders {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_names}, Signature={signature}",
            cfg.access_key_id
        ),
        payload_hash: payload_hash.to_string(),
        stamp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_cas_put_get_and_verify() -> CoreResult<()> {
        let dir = tempfile::tempdir()?;
        let cas = CasStore::open(dir.path(), None)?;
        let content = b"fn main() { println!(\"dockside\"); }";
        let h = cas.put_bytes(content).await?;
        assert!(cas.exists(&h).await?);
        let back = cas.get_bytes(&h).await?;
        assert_eq!(back, content);
        Ok(())
    }
}
