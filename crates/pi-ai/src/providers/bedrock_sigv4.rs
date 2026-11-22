//! AWS Signature V4 helpers for the Bedrock Converse Stream adapter.
//!
//! Ground truth: TS `packages/ai/src/api/bedrock-converse-stream.ts:177-259`
//! (credential / region / profile resolution and client config) and lines
//! `1251-1299` (`getConfiguredBedrockRegion`, endpoint-region handling), plus
//! the AWS Signature V4 specification applied by
//! `@aws-sdk/client-bedrock-runtime` for the `ConverseStream` operation
//! (`POST /model/{modelId}/converse-stream`, service name `bedrock`).
//!
//! This module is self-contained and uses only `sha2` (already in the workspace
//! lockfile): `HMAC-SHA256` is built on top of `SHA-256` so no new transitive
//! dependencies (and no Cargo.lock churn) are needed for signing.

// 1:1 port of the upstream TS adapter: the TS-shaped control flow, literal
// tables and helper structs trigger pedantic lints; keep the mirror exact.
#![allow(
    clippy::assert_is_empty,
    clippy::assigning_clones,
    clippy::collapsible_if,
    clippy::default_trait_access,
    clippy::doc_markdown,
    clippy::field_reassign_with_default,
    clippy::float_cmp,
    clippy::format_push_string,
    clippy::if_not_else,
    clippy::if_same_then_else,
    clippy::manual_assert_eq,
    clippy::manual_is_multiple_of,
    clippy::manual_strip,
    clippy::map_unwrap_or,
    clippy::needless_pass_by_value,
    clippy::redundant_closure_for_method_calls,
    clippy::result_large_err,
    clippy::single_match_else,
    clippy::struct_excessive_bools,
    clippy::too_many_arguments,
    clippy::unnecessary_get_then_check,
    clippy::unnecessary_map_or,
    clippy::unnecessary_wraps,
    clippy::useless_concat,
    dead_code
)]

use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// AWS credentials used to sign a Bedrock request (TS lines `1260-1272`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsCredentials {
    /// `AWS_ACCESS_KEY_ID`.
    pub access_key_id: String,
    /// `AWS_SECRET_ACCESS_KEY`.
    pub secret_access_key: String,
    /// `AWS_SESSION_TOKEN`, when using temporary credentials.
    pub session_token: Option<String>,
}

/// The authentication headers produced for one signed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedRequest {
    /// Value of the `Authorization` header.
    pub authorization: String,
    /// Value of the `x-amz-date` header (`%Y%m%dT%H%M%SZ`).
    pub x_amz_date: String,
    /// Hex-encoded `SHA-256` of the request payload.
    pub x_amz_content_sha256: String,
    /// Value of `x-amz-security-token`, when temporary credentials are used.
    pub x_amz_security_token: Option<String>,
}

/// Lowercase hex encoding of a byte slice (AWS signatures are lowercase hex).
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `HMAC-SHA256(key, data)` built from `SHA-256` (RFC 2104).
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= block[i];
        opad[i] ^= block[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    let digest = outer.finalize();
    let mut result = [0u8; 32];
    result.copy_from_slice(&digest);
    result
}

/// Hex-encoded `SHA-256` of `data`.
fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Current UTC time as the `(x-amz-date, date-stamp)` pair SigV4 needs.
pub fn amz_timestamp() -> (String, String) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    let stamp = format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z");
    let date = stamp[..8].to_string();
    (stamp, date)
}

/// Days since the Unix epoch to a proleptic Gregorian `(year, month, day)`.
/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

/// Percent-encode one URI path segment with RFC 3986 unreserved bytes only.
/// Model IDs may contain `:` and `/` (inference-profile ARNs); both become
/// `%3A` / `%2F`, matching the AWS SDK's path serialization.
pub fn uri_encode_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Canonical request path for a Converse Stream call (`/model/…/converse-stream`).
pub fn model_path(model_id: &str) -> String {
    format!("/model/{}/converse-stream", uri_encode_segment(model_id))
}

/// Extract the region embedded in a Bedrock inference-profile / model ARN.
/// Mirrors the TS regex `^arn:aws(?:-[a-z0-9-]+)?:bedrock:([a-z0-9-]+):`
/// (TS line `212`).
pub fn region_from_arn(model_id: &str) -> Option<String> {
    let rest = model_id.strip_prefix("arn:aws")?;
    let colon = rest.find(':')?;
    let partition = &rest[..colon];
    if !partition.is_empty() {
        let valid = partition
            .strip_prefix('-')?
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid {
            return None;
        }
    }
    let region = rest[colon + 1..]
        .strip_prefix("bedrock:")?
        .split(':')
        .next()?;
    if region.is_empty()
        || !region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    Some(region.to_string())
}

/// Extract the region from a standard Bedrock runtime endpoint host
/// (TS `getStandardBedrockEndpointRegion`, lines `1274-1286`).
pub fn endpoint_region(base_url: &str) -> Option<String> {
    let authority = base_url.split_once("://").map_or(base_url, |(_, r)| r);
    let host = authority
        .split('/')
        .next()
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let core = host
        .strip_prefix("bedrock-runtime-fips.")
        .or_else(|| host.strip_prefix("bedrock-runtime."))?;
    let region = core.split('.').next()?;
    let suffix = &core[region.len()..];
    if suffix != ".amazonaws.com" && suffix != ".amazonaws.com.cn" {
        return None;
    }
    if region.is_empty() {
        return None;
    }
    Some(region.to_string())
}

/// Build the SigV4 `Authorization` header and companion headers for one
/// Bedrock request. The signed headers are exactly `content-type`, `host`,
/// `x-amz-date` (plus `x-amz-security-token` for temporary credentials),
/// matching the AWS SDK for non-`S3` services.
pub fn sign_request(
    credentials: &AwsCredentials,
    region: &str,
    service: &str,
    host: &str,
    canonical_uri: &str,
    payload: &[u8],
    amz_date: &str,
    date_stamp: &str,
) -> SignedRequest {
    let payload_hash = sha256_hex(payload);
    let (token_header, signed_headers) = match &credentials.session_token {
        Some(token) => (
            format!("x-amz-security-token:{token}\n"),
            "content-type;host;x-amz-date;x-amz-security-token",
        ),
        None => (String::new(), "content-type;host;x-amz-date"),
    };
    let canonical_headers = format!(
        "content-type:application/json\nhost:{host}\nx-amz-date:{amz_date}\n{token_header}"
    );
    let canonical_request =
        format!("POST\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let secret = format!("AWS4{}", credentials.secret_access_key);
    let k_date = hmac_sha256(secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    SignedRequest {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id
        ),
        x_amz_date: amz_date.to_string(),
        x_amz_content_sha256: payload_hash,
        x_amz_security_token: credentials.session_token.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_vector() {
        let key = [0x0bu8; 20];
        let digest = hmac_sha256(&key, b"Hi There");
        assert_eq!(
            hex(&digest),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn encodes_model_ids_and_builds_path() {
        assert_eq!(uri_encode_segment("a:b/c"), "a%3Ab%2Fc");
        assert_eq!(
            model_path("anthropic.claude-3-5-sonnet-v1:0"),
            "/model/anthropic.claude-3-5-sonnet-v1%3A0/converse-stream"
        );
    }

    #[test]
    fn extracts_arn_and_endpoint_regions() {
        assert_eq!(
            region_from_arn("arn:aws:bedrock:eu-west-1:123:inference-profile/x"),
            Some("eu-west-1".to_string())
        );
        assert_eq!(
            region_from_arn("arn:aws-us-gov:bedrock:us-gov-west-1:123:model/x"),
            Some("us-gov-west-1".to_string())
        );
        assert_eq!(region_from_arn("anthropic.claude-3"), None);
        assert_eq!(
            endpoint_region("https://bedrock-runtime.ap-southeast-2.amazonaws.com"),
            Some("ap-southeast-2".to_string())
        );
        assert_eq!(
            endpoint_region("https://bedrock-runtime-fips.us-east-1.amazonaws.com.cn"),
            Some("us-east-1".to_string())
        );
        assert_eq!(endpoint_region("https://proxy.example.com"), None);
    }

    #[test]
    fn signature_header_shape() {
        let creds = AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "secret".into(),
            session_token: None,
        };
        let signed = sign_request(
            &creds,
            "us-east-1",
            "bedrock",
            "bedrock-runtime.us-east-1.amazonaws.com",
            "/model/m/converse-stream",
            b"{}",
            "20240101T000000Z",
            "20240101",
        );
        assert!(
            signed.authorization.starts_with(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20240101/us-east-1/bedrock/aws4_request, "
            ),
            "{}",
            signed.authorization
        );
        assert!(
            signed
                .authorization
                .contains("SignedHeaders=content-type;host;x-amz-date, Signature=")
        );
        assert_eq!(signed.x_amz_date, "20240101T000000Z");
        assert_eq!(signed.x_amz_security_token, None);
        assert_eq!(signed.x_amz_content_sha256.len(), 64);

        let with_token = AwsCredentials {
            session_token: Some("token".into()),
            ..creds
        };
        let signed = sign_request(
            &with_token,
            "us-east-1",
            "bedrock",
            "host",
            "/m",
            b"",
            "20240101T000000Z",
            "20240101",
        );
        assert!(signed.authorization.contains("x-amz-security-token"));
        assert_eq!(signed.x_amz_security_token.as_deref(), Some("token"));
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
    }
}
