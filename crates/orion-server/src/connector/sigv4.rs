//! AWS Signature Version 4 — the page of HMAC math behind the `storage`
//! connector (#265).
//!
//! Hand-rolled deliberately: the alternative is the `aws-sigv4` crate and its
//! aws-smithy dependency family, for one well-specified chain the in-tree
//! `hmac`/`sha2` already express. Correctness is pinned by AWS's published
//! vectors — the S3 presigned-GET example from the SigV4 documentation and
//! the `get-vanilla` case of the official test suite — in the tests below.
//!
//! Two entry points, matching the two things the connector does:
//! [`presign_url`] (query-string auth — `storage_presign`, pure computation)
//! and [`sign_headers`] (`Authorization`-header auth — `storage_head`'s one
//! HEAD request, and the connectivity probe).

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// Everything the signing chain needs to know about one request's identity.
/// Secrets arrive already resolved (the connector registry resolves `env://`
/// references at load).
pub struct SigningContext<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    /// STS temporary-credential token, carried as `X-Amz-Security-Token`.
    pub session_token: Option<&'a str>,
    pub region: &'a str,
    /// `"s3"` for object storage; a parameter so the official generic test
    /// suite (service `"service"`) can pin the chain.
    pub service: &'a str,
    /// The `host` header value the client will use — addressing style
    /// (virtual-hosted vs path-style) is decided by the caller and is already
    /// baked into this and into `path`.
    pub host: &'a str,
    /// The canonical URI: absolute path, each segment URI-encoded per the
    /// SigV4 rules ([`encode_path`]).
    pub path: &'a str,
    /// UTC signing instant, `yyyymmddThhmmssZ` (ISO 8601 basic).
    pub amz_date: &'a str,
}

impl SigningContext<'_> {
    fn date(&self) -> &str {
        &self.amz_date[..8]
    }

    fn credential_scope(&self) -> String {
        format!(
            "{}/{}/{}/aws4_request",
            self.date(),
            self.region,
            self.service
        )
    }

    fn signing_key(&self) -> Vec<u8> {
        let k_date = hmac_sha256(format!("AWS4{}", self.secret_key).as_bytes(), self.date());
        let k_region = hmac_sha256(&k_date, self.region);
        let k_service = hmac_sha256(&k_region, self.service);
        hmac_sha256(&k_service, "aws4_request")
    }
}

fn hmac_sha256(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// SHA-256 of the empty string — the payload hash of a bodyless signed
/// request (HEAD, GET without presigning).
pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// The payload-hash sentinel presigned URLs use: the client's bytes are not
/// known at signing time.
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// Percent-encode one string per the SigV4 rules: unreserved characters
/// (`A–Z a–z 0–9 - _ . ~`) stay, everything else is `%XX` uppercase.
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b'/' if keep_slash => out.push('/'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Encode an object key as a canonical URI path: each segment encoded, `/`
/// separators preserved.
pub fn encode_path(key: &str) -> String {
    format!("/{}", uri_encode(key.trim_start_matches('/'), true))
}

/// The canonical query string: pairs sorted by encoded name (then value),
/// both halves URI-encoded.
fn canonical_query(params: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn string_to_sign(ctx: &SigningContext<'_>, canonical_request: &str) -> String {
    format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        ctx.amz_date,
        ctx.credential_scope(),
        sha256_hex(canonical_request.as_bytes())
    )
}

/// Presign one request: the full URL a client can use until `expires_secs`
/// after `amz_date`. Query-string auth per the SigV4 spec — only `host` is
/// signed (plus any `extra_headers`, e.g. a PUT `content-type` constraint the
/// client must then send verbatim), and the payload is `UNSIGNED-PAYLOAD`.
///
/// `extra_query` carries the response-override parameters
/// (`response-content-disposition`, …); they are part of the signature, so a
/// client cannot strip or alter them.
pub fn presign_url(
    ctx: &SigningContext<'_>,
    scheme: &str,
    method: &str,
    expires_secs: u64,
    extra_query: &[(String, String)],
    extra_headers: &[(String, String)],
) -> String {
    // Headers: host always, plus any extras, sorted by name.
    let mut headers: Vec<(String, String)> = vec![("host".to_string(), ctx.host.to_string())];
    headers.extend(
        extra_headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string())),
    );
    headers.sort();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers = headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();

    let mut query: Vec<(String, String)> = vec![
        (
            "X-Amz-Algorithm".to_string(),
            "AWS4-HMAC-SHA256".to_string(),
        ),
        (
            "X-Amz-Credential".to_string(),
            format!("{}/{}", ctx.access_key, ctx.credential_scope()),
        ),
        ("X-Amz-Date".to_string(), ctx.amz_date.to_string()),
        ("X-Amz-Expires".to_string(), expires_secs.to_string()),
        ("X-Amz-SignedHeaders".to_string(), signed_headers.clone()),
    ];
    if let Some(token) = ctx.session_token {
        query.push(("X-Amz-Security-Token".to_string(), token.to_string()));
    }
    query.extend(extra_query.iter().cloned());

    let canonical_request = format!(
        "{method}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{UNSIGNED_PAYLOAD}",
        ctx.path,
        canonical_query(&query),
    );
    let signature = hex::encode(hmac_sha256(
        &ctx.signing_key(),
        &string_to_sign(ctx, &canonical_request),
    ));
    query.push(("X-Amz-Signature".to_string(), signature));

    // The URL's own query keeps the canonical ordering — harmless, and it
    // makes output deterministic for tests and diffs.
    format!(
        "{scheme}://{}{}?{}",
        ctx.host,
        ctx.path,
        canonical_query(&query)
    )
}

/// Sign a bodyless request with header auth. Returns the headers to attach:
/// `host` is signed but returned implicitly (reqwest sets it from the URL);
/// the rest — `x-amz-date`, `x-amz-content-sha256` (S3 requires it),
/// `x-amz-security-token` when present, and `authorization` — come back for
/// the caller to set.
pub fn sign_headers(ctx: &SigningContext<'_>, method: &str) -> Vec<(String, String)> {
    let mut signed: Vec<(String, String)> = vec![
        ("host".to_string(), ctx.host.to_string()),
        ("x-amz-date".to_string(), ctx.amz_date.to_string()),
    ];
    // S3 requires the payload hash header; the generic test-suite service
    // does not send it. Keying on the service keeps both paths honest.
    if ctx.service == "s3" {
        signed.push((
            "x-amz-content-sha256".to_string(),
            EMPTY_PAYLOAD_SHA256.to_string(),
        ));
        if let Some(token) = ctx.session_token {
            signed.push(("x-amz-security-token".to_string(), token.to_string()));
        }
    }
    signed.sort();
    let signed_names = signed
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers = signed
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<String>();
    let payload_hash = EMPTY_PAYLOAD_SHA256;

    let canonical_request = format!(
        "{method}\n{}\n\n{canonical_headers}\n{signed_names}\n{payload_hash}",
        ctx.path
    );
    let signature = hex::encode(hmac_sha256(
        &ctx.signing_key(),
        &string_to_sign(ctx, &canonical_request),
    ));

    let mut out: Vec<(String, String)> = signed.into_iter().filter(|(k, _)| k != "host").collect();
    out.push((
        "authorization".to_string(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            ctx.access_key,
            ctx.credential_scope(),
            signed_names,
            signature
        ),
    ));
    out
}

/// The current instant in the `yyyymmddThhmmssZ` form SigV4 wants.
pub fn amz_date_now() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's published presigned-GET example (SigV4 documentation): every
    /// value fixed, one known-correct signature. If the canonicalization,
    /// scope, or key chain drifts, this fails with AWS's own numbers.
    #[test]
    fn aws_documented_presign_vector() {
        let ctx = SigningContext {
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            session_token: None,
            region: "us-east-1",
            service: "s3",
            host: "examplebucket.s3.amazonaws.com",
            path: "/test.txt",
            amz_date: "20130524T000000Z",
        };
        let url = presign_url(&ctx, "https", "GET", 86400, &[], &[]);
        assert!(
            url.contains(
                "X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
            ),
            "{url}"
        );
        assert!(
            url.starts_with("https://examplebucket.s3.amazonaws.com/test.txt?"),
            "{url}"
        );
        assert!(
            url.contains(
                "X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request"
            ),
            "{url}"
        );
    }

    /// `get-vanilla` from the official SigV4 test suite — the header-auth
    /// chain against its known signature.
    #[test]
    fn suite_get_vanilla_header_vector() {
        let ctx = SigningContext {
            access_key: "AKIDEXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            session_token: None,
            region: "us-east-1",
            service: "service",
            host: "example.amazonaws.com",
            path: "/",
            amz_date: "20150830T123600Z",
        };
        let headers = sign_headers(&ctx, "GET");
        let auth = headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.as_str())
            .expect("test");
        assert!(
            auth.ends_with(
                "Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
            ),
            "{auth}"
        );
        assert!(auth.contains("SignedHeaders=host;x-amz-date"), "{auth}");
    }

    #[test]
    fn key_segments_encode_but_slashes_survive() {
        assert_eq!(
            encode_path("video/topic 1/output.m3u8"),
            "/video/topic%201/output.m3u8"
        );
        assert_eq!(encode_path("/already/rooted"), "/already/rooted");
        assert_eq!(uri_encode("a+b=c", false), "a%2Bb%3Dc");
    }

    #[test]
    fn session_token_is_signed_into_the_query() {
        let ctx = SigningContext {
            access_key: "AKIA",
            secret_key: "secret",
            session_token: Some("TOKEN123"),
            region: "eu-west-1",
            service: "s3",
            host: "b.example.com",
            path: "/k",
            amz_date: "20260819T000000Z",
        };
        let url = presign_url(&ctx, "https", "GET", 60, &[], &[]);
        assert!(url.contains("X-Amz-Security-Token=TOKEN123"), "{url}");
    }

    #[test]
    fn extra_query_and_signed_content_type_change_the_signature() {
        let ctx = SigningContext {
            access_key: "AKIA",
            secret_key: "secret",
            session_token: None,
            region: "eu-west-1",
            service: "s3",
            host: "b.example.com",
            path: "/k",
            amz_date: "20260819T000000Z",
        };
        let plain = presign_url(&ctx, "https", "GET", 60, &[], &[]);
        let with_override = presign_url(
            &ctx,
            "https",
            "GET",
            60,
            &[(
                "response-content-disposition".to_string(),
                "attachment; filename=\"a.mp4\"".to_string(),
            )],
            &[],
        );
        assert_ne!(plain, with_override);
        assert!(
            with_override.contains("response-content-disposition=attachment%3B%20filename%3D"),
            "{with_override}"
        );

        let put = presign_url(
            &ctx,
            "https",
            "PUT",
            60,
            &[],
            &[("Content-Type".to_string(), "video/mp4".to_string())],
        );
        assert!(
            put.contains("X-Amz-SignedHeaders=content-type%3Bhost"),
            "{put}"
        );
    }
}
