use crate::error::{CoreError, CoreResult};

/// Compute lowercase hex BLAKE3 digest of raw bytes.
pub fn hash_bytes(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// Validate a 64-character lowercase hexadecimal BLAKE3 digest.
pub fn is_valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Compress raw bytes using zstd (level 3 = fast, great ratio for source code).
pub fn compress_zstd(raw: &[u8]) -> CoreResult<Vec<u8>> {
    zstd::bulk::compress(raw, 3).map_err(|e| CoreError::Compression(e.to_string()))
}

/// Decompress zstd payload with streaming decoder (no fixed upper bound assumption on uncompressed size).
pub fn decompress_zstd(compressed: &[u8]) -> CoreResult<Vec<u8>> {
    zstd::stream::decode_all(compressed).map_err(|e| CoreError::Compression(e.to_string()))
}

/// Normalize and strictly validate a repository-relative POSIX path.
///
/// Rejects absolute paths, Windows drive letters, NUL bytes, `..` traversal,
/// empty segments, and `.git` metadata directories.
pub fn normalize_path(input: &str) -> CoreResult<String> {
    if input.is_empty() {
        return Err(CoreError::InvalidPath {
            path: input.to_string(),
            reason: "path cannot be empty".to_string(),
        });
    }
    if input.bytes().any(|b| b == 0) {
        return Err(CoreError::InvalidPath {
            path: input.to_string(),
            reason: "path contains NUL byte".to_string(),
        });
    }

    let replaced = input.replace('\\', "/");
    if replaced.starts_with('/') {
        return Err(CoreError::InvalidPath {
            path: input.to_string(),
            reason: "absolute paths are forbidden".to_string(),
        });
    }
    if replaced.as_bytes().get(1) == Some(&b':') {
        return Err(CoreError::InvalidPath {
            path: input.to_string(),
            reason: "drive-qualified paths are forbidden".to_string(),
        });
    }

    let trimmed = replaced.trim_end_matches('/');
    let mut parts = Vec::new();
    for segment in trimmed.split('/') {
        if segment.is_empty() || segment == "." {
            if parts.is_empty() && segment == "." {
                continue;
            }
            return Err(CoreError::InvalidPath {
                path: input.to_string(),
                reason: "empty or dot path segment".to_string(),
            });
        }
        if segment == ".." {
            return Err(CoreError::InvalidPath {
                path: input.to_string(),
                reason: "parent traversal '..' is forbidden".to_string(),
            });
        }
        if segment == ".git" {
            return Err(CoreError::InvalidPath {
                path: input.to_string(),
                reason: "'.git' paths are forbidden in dockside".to_string(),
            });
        }
        parts.push(segment);
    }

    if parts.is_empty() {
        return Err(CoreError::InvalidPath {
            path: input.to_string(),
            reason: "path resolved to empty".to_string(),
        });
    }

    Ok(parts.join("/"))
}

/// Normalize a scope prefix (allows empty string or `"."` to mean full repository).
pub fn normalize_scope_prefix(input: &str) -> CoreResult<String> {
    let trimmed = input.trim().replace('\\', "/");
    let trimmed = trimmed.trim_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return Ok(String::new());
    }
    normalize_path(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_windows_paths_and_rejects_traversal() -> CoreResult<()> {
        assert_eq!(
            normalize_path(r"services\api\src\main.rs")?,
            "services/api/src/main.rs"
        );
        assert_eq!(
            normalize_path("./apps/web/app/root.tsx")?,
            "apps/web/app/root.tsx"
        );
        assert!(normalize_path("../secret").is_err());
        assert!(normalize_path("/etc/passwd").is_err());
        assert!(normalize_path("C:/Users/foo").is_err());
        assert!(normalize_path("services/api/.git/HEAD").is_err());
        Ok(())
    }

    #[test]
    fn blake3_and_zstd_roundtrip() -> CoreResult<()> {
        let data = b"hello dockside!";
        let h = hash_bytes(data);
        assert!(is_valid_hash(&h));
        let compressed = compress_zstd(data)?;
        let out = decompress_zstd(&compressed)?;
        assert_eq!(out, data);
        Ok(())
    }
}
