// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A bucket reached over HTTP: the S3 protocol, signed here, sent by whoever
//! has a way to send.
//!
//! A native process has a full S3 client. A Worker has `fetch` and nothing
//! else, and a bucket in another account cannot be bound to it — it can only
//! be asked, as anyone asks, with a signed request. So the protocol is here
//! (Signature Version 4, `ListObjectsV2`, ranged `GET`, `HEAD`) and the
//! transport is a trait of one method: what differs between hosts is how a
//! request leaves, never what it says.

use std::ops::Range;

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::{Entry, Listing, Objects, RepoError};

/// A request as the protocol built it, for a transport to send as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpReply {
    pub status: u16,
    /// The `Content-Length` header, which is all a `HEAD` is asked for.
    pub content_length: Option<u64>,
    pub body: Vec<u8>,
}

/// How a request leaves this host.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait Http: Send + Sync {
    /// The time, in seconds since the Unix epoch: a signature carries it.
    fn now(&self) -> u64;
    async fn send(&self, request: &Signed) -> Result<HttpReply, String>;
}

/// The bucket's address and the key that signs for it.
#[derive(Clone)]
pub struct S3Config {
    /// `https://host`, without a trailing slash; the bucket goes in the path.
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: String,
}

/// [`Objects`] over the S3 protocol.
pub struct S3Objects<H: Http> {
    config: S3Config,
    http: H,
}

/// SHA-256 of nothing: the payload of every request here.
const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// HMAC-SHA256 (RFC 2104).
fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let inner: Vec<u8> = block
        .iter()
        .map(|b| b ^ 0x36)
        .chain(message.iter().copied())
        .collect();
    let outer: Vec<u8> = block
        .iter()
        .map(|b| b ^ 0x5c)
        .chain(sha256(&inner))
        .collect();
    sha256(&outer)
}

/// RFC 3986 encoding as the signature wants it: everything but the
/// unreserved characters, and `/` kept only in a path.
fn encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `(YYYYMMDD, YYYYMMDDTHHMMSSZ)` for a Unix time.
fn stamps(unix: u64) -> (String, String) {
    let (days, rest) = (unix / 86_400, unix % 86_400);
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let date = format!("{year:04}{month:02}{day:02}");
    let time = format!(
        "{date}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    );
    (date, time)
}

/// What a request is, before it is signed.
pub struct Unsigned<'a> {
    pub method: &'static str,
    /// The host, as the `Host` header will carry it.
    pub host: &'a str,
    /// The path, not yet encoded, starting with `/`.
    pub path: &'a str,
    /// Query parameters, not yet encoded.
    pub query: &'a [(&'a str, String)],
    /// More headers to send and sign, lower-case names.
    pub headers: &'a [(&'a str, String)],
}

/// The `Authorization` header and the headers it covers (Signature
/// Version 4), for an empty payload.
pub fn sign(
    request: &Unsigned<'_>,
    access_key_id: &str,
    secret_access_key: &str,
    region: &str,
    unix: u64,
) -> (String, Vec<(String, String)>, String) {
    let (date, time) = stamps(unix);
    let mut headers: Vec<(String, String)> = vec![
        ("host".into(), request.host.into()),
        ("x-amz-content-sha256".into(), EMPTY.into()),
        ("x-amz-date".into(), time.clone()),
    ];
    headers.extend(
        request
            .headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone())),
    );
    headers.sort();
    let mut query: Vec<(String, String)> = request
        .query
        .iter()
        .map(|(k, v)| (encode(k, false), encode(v, false)))
        .collect();
    query.sort();
    let query = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let signed = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "{}\n{}\n{query}\n{}\n{signed}\n{EMPTY}",
        request.method,
        encode(request.path, true),
        headers
            .iter()
            .map(|(k, v)| format!("{k}:{}\n", v.trim()))
            .collect::<String>(),
    );
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{time}\n{scope}\n{}",
        hex(&sha256(canonical.as_bytes()))
    );
    let mut key = hmac(
        format!("AWS4{secret_access_key}").as_bytes(),
        date.as_bytes(),
    );
    for part in [region, "s3", "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    let authorization =
        format!("AWS4-HMAC-SHA256 Credential={access_key_id}/{scope},SignedHeaders={signed},Signature={signature}");
    (authorization, headers, query)
}

/// One page of a `ListObjectsV2` answer.
#[derive(Debug, Default, PartialEq, Eq)]
struct Page {
    files: Vec<Entry>,
    dirs: Vec<String>,
    next: Option<String>,
}

fn page(xml: &str) -> Result<Page, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let text = |node: roxmltree::Node<'_, '_>, name: &str| {
        node.children()
            .find(|c| c.has_tag_name(name))
            .and_then(|c| c.text())
            .map(str::to_string)
    };
    let mut out = Page::default();
    for node in doc.root_element().children() {
        if node.has_tag_name("Contents") {
            let key = text(node, "Key").ok_or("a listed object has no key")?;
            let size = text(node, "Size")
                .and_then(|s| s.parse().ok())
                .ok_or("a listed object has no size")?;
            out.files.push(Entry { key, size });
        } else if node.has_tag_name("CommonPrefixes") {
            if let Some(prefix) = text(node, "Prefix") {
                out.dirs.push(prefix.trim_end_matches('/').to_string());
            }
        } else if node.has_tag_name("NextContinuationToken") {
            out.next = node.text().map(str::to_string);
        }
    }
    Ok(out)
}

impl<H: Http> S3Objects<H> {
    pub fn new(config: S3Config, http: H) -> Self {
        Self { config, http }
    }

    fn host(&self) -> &str {
        self.config
            .endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
    }

    async fn request(
        &self,
        method: &'static str,
        key: &str,
        query: &[(&str, String)],
        range: Option<Range<u64>>,
    ) -> Result<HttpReply, RepoError> {
        let path = format!("/{}/{key}", self.config.bucket);
        let path = path.trim_end_matches('/');
        let ranged: Vec<(&str, String)> = range
            .iter()
            .map(|r| ("range", format!("bytes={}-{}", r.start, r.end - 1)))
            .collect();
        let unsigned = Unsigned {
            method,
            host: self.host(),
            path,
            query,
            headers: &ranged,
        };
        let (authorization, mut headers, query) = sign(
            &unsigned,
            &self.config.access_key_id,
            &self.config.secret_access_key,
            &self.config.region,
            self.http.now(),
        );
        // `Host` is the transport's to set, from the URL.
        headers.retain(|(k, _)| k != "host");
        headers.push(("authorization".into(), authorization));
        let url = format!(
            "{}{}{}{query}",
            self.config.endpoint.trim_end_matches('/'),
            encode(path, true),
            if query.is_empty() { "" } else { "?" },
        );
        let reply = self
            .http
            .send(&Signed {
                method,
                url,
                headers,
            })
            .await
            .map_err(|e| RepoError::Store(format!("{key}: {e}")))?;
        match reply.status {
            200 | 206 => Ok(reply),
            404 => Err(RepoError::NotFound(key.to_string())),
            status => Err(RepoError::Store(format!(
                "{key}: HTTP {status} — {}",
                String::from_utf8_lossy(&reply.body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            ))),
        }
    }

    /// Every page of a listing.
    async fn listing(&self, prefix: &str, delimiter: bool) -> Result<Page, RepoError> {
        let prefix = prefix.trim_matches('/');
        // A directory is named with its slash, or `a/b` would also list `a/bc`.
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        let mut all = Page::default();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2".to_string()), ("prefix", prefix.clone())];
            if delimiter {
                query.push(("delimiter", "/".into()));
            }
            if let Some(t) = &token {
                query.push(("continuation-token", t.clone()));
            }
            let reply = self.request("GET", "", &query, None).await?;
            let got = page(&String::from_utf8_lossy(&reply.body))
                .map_err(|e| RepoError::Store(format!("listing {prefix}: {e}")))?;
            all.files.extend(got.files);
            all.dirs.extend(got.dirs);
            match got.next {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        all.files.sort_by(|a, b| a.key.cmp(&b.key));
        all.dirs.sort();
        Ok(all)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<H: Http> Objects for S3Objects<H> {
    fn label(&self) -> String {
        format!("bucket {}", self.config.bucket)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, RepoError> {
        Ok(self.listing(prefix, false).await?.files)
    }

    async fn browse(&self, prefix: &str) -> Result<Listing, RepoError> {
        let page = self.listing(prefix, true).await?;
        Ok(Listing {
            dirs: page.dirs,
            files: page.files,
        })
    }

    async fn size(&self, key: &str) -> Result<u64, RepoError> {
        self.request("HEAD", key, &[], None)
            .await?
            .content_length
            .ok_or_else(|| RepoError::Store(format!("{key}: no Content-Length")))
    }

    async fn read(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
        if range.is_empty() {
            return Ok(Vec::new());
        }
        let wanted = range.end - range.start;
        let body = self.request("GET", key, &[], Some(range)).await?.body;
        if body.len() as u64 != wanted {
            return Err(RepoError::Store(format!(
                "{key}: asked for {wanted} bytes, got {}",
                body.len()
            )));
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The examples of the S3 API reference, "Signature Calculations for the
    // Authorization Header": the key, the date and the signatures are theirs.
    const KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const WHEN: u64 = 1_369_353_600; // 2013-05-24T00:00:00Z

    #[test]
    fn a_time_is_stamped_as_the_signature_wants() {
        assert_eq!(
            stamps(WHEN),
            ("20130524".to_string(), "20130524T000000Z".to_string())
        );
        assert_eq!(stamps(0).1, "19700101T000000Z");
        assert_eq!(stamps(1791203696).1, "20261005T123456Z");
    }

    #[test]
    fn a_ranged_get_signs_as_the_reference_does() {
        let range = [("range", "bytes=0-9".to_string())];
        let request = Unsigned {
            method: "GET",
            host: "examplebucket.s3.amazonaws.com",
            path: "/test.txt",
            query: &[],
            headers: &range,
        };
        let (authorization, ..) = sign(&request, KEY, SECRET, "us-east-1", WHEN);
        assert!(
            authorization.ends_with(
                "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            ),
            "{authorization}"
        );
        assert!(authorization.contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date"));
    }

    #[test]
    fn a_listing_signs_as_the_reference_does() {
        let query = [("max-keys", "2".to_string()), ("prefix", "J".to_string())];
        let request = Unsigned {
            method: "GET",
            host: "examplebucket.s3.amazonaws.com",
            path: "/",
            query: &query,
            headers: &[],
        };
        let (authorization, _, query) = sign(&request, KEY, SECRET, "us-east-1", WHEN);
        assert_eq!(query, "max-keys=2&prefix=J");
        assert!(
            authorization.ends_with(
                "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
            ),
            "{authorization}"
        );
    }

    #[test]
    fn a_key_is_encoded_once_and_slashes_kept() {
        assert_eq!(
            encode("packs/a b/1-8.tuilepack", true),
            "packs/a%20b/1-8.tuilepack"
        );
        assert_eq!(encode("packs/a", false), "packs%2Fa");
        assert_eq!(encode("é~_-.", true), "%C3%A9~_-.");
    }

    #[test]
    fn a_listing_page_gives_objects_directories_and_what_is_next() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
            <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
              <Name>b</Name><IsTruncated>true</IsTruncated>
              <Contents><Key>packs/a/1-8.tuilepack</Key><Size>1234</Size></Contents>
              <Contents><Key>packs/a/1-8.tuilepack.scene</Key><Size>17</Size></Contents>
              <CommonPrefixes><Prefix>packs/a/sub/</Prefix></CommonPrefixes>
              <NextContinuationToken>abc=</NextContinuationToken>
            </ListBucketResult>"#;
        let p = page(xml).expect("page");
        assert_eq!(p.files.len(), 2);
        assert_eq!(
            p.files[0],
            Entry {
                key: "packs/a/1-8.tuilepack".into(),
                size: 1234
            }
        );
        assert_eq!(p.dirs, ["packs/a/sub"]);
        assert_eq!(p.next.as_deref(), Some("abc="));
        assert!(page("<not xml").is_err());
    }
}
