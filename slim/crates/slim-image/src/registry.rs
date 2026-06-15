//! Registry v2 client: manifests + blobs with anonymous/basic/bearer-token
//! auth (Docker Hub, ghcr, generic v2).
//!
//! Redirects are handled manually because blob GETs 307 to CDNs that reject
//! forwarded Authorization headers.

use crate::refs::Reference;
use slim_api::image::*;
use std::io::Read;
use std::time::Duration;

pub struct RegistryClient {
    agent: ureq::Agent,
    /// bearer token cache per (registry, repo).
    token: std::sync::Mutex<Option<String>>,
    pub auth: Option<BasicAuth>,
    insecure_http: bool,
}

#[derive(Debug, Clone)]
pub struct BasicAuth {
    pub username: String,
    pub password: String,
}

#[derive(Debug)]
pub struct RegistryError(pub String);

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for RegistryError {}

type Result<T> = std::result::Result<T, RegistryError>;

fn err(msg: impl Into<String>) -> RegistryError {
    RegistryError(msg.into())
}

impl RegistryClient {
    pub fn new(auth: Option<BasicAuth>) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(20))
                .redirects(0)
                .user_agent("nebula-slim/0.1")
                .build(),
            token: std::sync::Mutex::new(None),
            auth,
            insecure_http: false,
        }
    }

    /// localhost registries are commonly plain HTTP.
    pub fn for_reference(reference: &Reference, auth: Option<BasicAuth>) -> Self {
        let mut c = Self::new(auth);
        c.insecure_http =
            reference.registry.starts_with("localhost") || reference.registry.starts_with("127.");
        c
    }

    fn base(&self, r: &Reference) -> String {
        let scheme = if self.insecure_http { "http" } else { "https" };
        format!("{scheme}://{}/v2", r.api_host())
    }

    /// GET with auth + manual redirects. Returns (status, body reader).
    fn get(
        &self,
        r: &Reference,
        url: &str,
        accept: &str,
    ) -> Result<(u16, Box<dyn Read + Send>, Option<String>)> {
        let mut url = url.to_string();
        let mut send_auth = true;
        for _hop in 0..6 {
            let mut req = self.agent.get(&url);
            if !accept.is_empty() {
                req = req.set("Accept", accept);
            }
            if send_auth {
                if let Some(tok) = self.token.lock().unwrap().clone() {
                    req = req.set("Authorization", &format!("Bearer {tok}"));
                } else if let Some(a) = &self.auth {
                    req = req.set("Authorization", &basic_header(a));
                }
            }
            let resp = match req.call() {
                Ok(r) => r,
                Err(ureq::Error::Status(code, resp)) => {
                    if code == 401 {
                        // Acquire a bearer token per WWW-Authenticate and retry.
                        let challenge = resp.header("www-authenticate").unwrap_or("").to_string();
                        self.fetch_token(&challenge, r)?;
                        continue;
                    }
                    return Err(err(format!(
                        "registry returned {code} for {url}: {}",
                        resp.into_string()
                            .unwrap_or_default()
                            .chars()
                            .take(300)
                            .collect::<String>()
                    )));
                }
                Err(e) => return Err(err(format!("request to {url} failed: {e}"))),
            };
            let status = resp.status();
            if (301..=308).contains(&status) {
                let loc = resp
                    .header("location")
                    .ok_or_else(|| err("redirect without Location"))?;
                url = if loc.starts_with("http") {
                    loc.to_string()
                } else {
                    // relative redirect
                    let base = url.split('/').take(3).collect::<Vec<_>>().join("/");
                    format!("{base}{loc}")
                };
                send_auth = false; // never forward credentials cross-host
                continue;
            }
            let ctype = resp.header("content-type").map(|s| s.to_string());
            return Ok((status, resp.into_reader(), ctype));
        }
        Err(err("too many redirects"))
    }

    fn fetch_token(&self, challenge: &str, r: &Reference) -> Result<()> {
        // WWW-Authenticate: Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="..."
        let params = parse_challenge(challenge);
        let realm = params
            .iter()
            .find(|(k, _)| k == "realm")
            .map(|(_, v)| v.clone())
            .ok_or_else(|| err(format!("unsupported auth challenge: {challenge}")))?;
        let mut url = format!("{realm}?");
        if let Some((_, service)) = params.iter().find(|(k, _)| k == "service") {
            url.push_str(&format!("service={}&", urlenc(service)));
        }
        let scope = params
            .iter()
            .find(|(k, _)| k == "scope")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| format!("repository:{}:pull", r.repo));
        url.push_str(&format!("scope={}", urlenc(&scope)));

        let mut req = self.agent.get(&url);
        if let Some(a) = &self.auth {
            req = req.set("Authorization", &basic_header(a));
        }
        let resp = req
            .call()
            .map_err(|e| err(format!("token endpoint failed: {e}")))?;
        let body: serde_json::Value = resp
            .into_json()
            .map_err(|e| err(format!("bad token response: {e}")))?;
        let tok = body["token"]
            .as_str()
            .or_else(|| body["access_token"].as_str())
            .ok_or_else(|| err("token response missing token"))?;
        *self.token.lock().unwrap() = Some(tok.to_string());
        Ok(())
    }

    /// Resolve a reference to the arch-specific manifest.
    /// Returns (manifest, manifest digest, raw manifest bytes).
    pub fn manifest(&self, r: &Reference, arch: &str) -> Result<(Manifest, String, Vec<u8>)> {
        let tag_or_digest = if !r.digest.is_empty() {
            r.digest.clone()
        } else {
            r.tag.clone()
        };
        let url = format!("{}/{}/manifests/{}", self.base(r), r.repo, tag_or_digest);
        let accept =
            format!("{MT_MANIFEST_LIST_V2}, {MT_MANIFEST_V2}, {MT_OCI_INDEX}, {MT_OCI_MANIFEST}");
        let (status, mut body, ctype) = self.get(r, &url, &accept)?;
        if status == 404 {
            return Err(err(format!(
                "manifest for {} not found: manifest unknown",
                r.familiar()
            )));
        }
        if status != 200 {
            return Err(err(format!("manifest fetch returned {status}")));
        }
        let mut raw = Vec::new();
        body.read_to_end(&mut raw)
            .map_err(|e| err(format!("manifest read: {e}")))?;
        let digest = format!("sha256:{}", hex::encode(crate::sha256(&raw)));
        let ctype = ctype.unwrap_or_default();

        if ctype.contains("list") || ctype.contains("index") || is_index(&raw) {
            let index: ManifestIndex =
                serde_json::from_slice(&raw).map_err(|e| err(format!("bad index: {e}")))?;
            let pick = pick_platform(&index, arch).ok_or_else(|| {
                err(format!(
                    "no matching manifest for linux/{arch} in {} (available: {})",
                    r.familiar(),
                    index
                        .manifests
                        .iter()
                        .filter_map(|m| m.platform.as_ref())
                        .map(|p| format!("{}/{}", p.os, p.architecture))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
            let mut pinned = r.clone();
            pinned.digest = pick.digest.clone();
            pinned.tag = String::new();
            return self.manifest(&pinned, arch);
        }
        let m: Manifest =
            serde_json::from_slice(&raw).map_err(|e| err(format!("bad manifest: {e}")))?;
        Ok((m, digest, raw))
    }

    /// Stream a blob to `sink`, verifying its digest on the fly.
    pub fn fetch_blob(
        &self,
        r: &Reference,
        digest: &str,
        mut sink: impl std::io::Write,
        mut progress: impl FnMut(u64),
    ) -> Result<u64> {
        let url = format!("{}/{}/blobs/{}", self.base(r), r.repo, digest);
        let (status, mut body, _) = self.get(r, &url, "")?;
        if status != 200 {
            return Err(err(format!("blob {digest} fetch returned {status}")));
        }
        let mut hasher = crate::Sha256Stream::new();
        let mut buf = [0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let n = body
                .read(&mut buf)
                .map_err(|e| err(format!("blob read: {e}")))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            sink.write_all(&buf[..n])
                .map_err(|e| err(format!("blob write: {e}")))?;
            total += n as u64;
            progress(total);
        }
        let got = format!("sha256:{}", hasher.finish_hex());
        if got != digest {
            return Err(err(format!(
                "blob digest mismatch: want {digest} got {got}"
            )));
        }
        Ok(total)
    }
}

fn is_index(raw: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(raw)
        .map(|v| v.get("manifests").is_some())
        .unwrap_or(false)
}

fn pick_platform(index: &ManifestIndex, arch: &str) -> Option<Descriptor> {
    let matches = |p: &Platform, a: &str| p.os == "linux" && (p.architecture == a);
    // Prefer exact arch; fall back to amd64 (Rosetta exists in the full
    // engine but NOT under slimd — keep the fallback so `pull` works, the
    // run will fail with a clear exec format error).
    index
        .manifests
        .iter()
        .find(|m| {
            m.platform
                .as_ref()
                .map(|p| matches(p, arch))
                .unwrap_or(false)
        })
        .or_else(|| {
            index.manifests.iter().find(|m| {
                m.platform
                    .as_ref()
                    .map(|p| matches(p, "amd64"))
                    .unwrap_or(false)
            })
        })
        .cloned()
}

fn parse_challenge(s: &str) -> Vec<(String, String)> {
    let s = s
        .trim_start_matches("Bearer ")
        .trim_start_matches("bearer ");
    let mut out = Vec::new();
    for part in s.split(',') {
        if let Some((k, v)) = part.split_once('=') {
            out.push((k.trim().to_string(), v.trim().trim_matches('"').to_string()));
        }
    }
    out
}

fn basic_header(a: &BasicAuth) -> String {
    format!(
        "Basic {}",
        b64(format!("{}:{}", a.username, a.password).as_bytes())
    )
}

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Tiny base64 (standard, padded) — not worth a dependency.
pub fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn b64_decode(s: &str) -> Vec<u8> {
    const fn inv(c: u8) -> i8 {
        match c {
            b'A'..=b'Z' => (c - b'A') as i8,
            b'a'..=b'z' => (c - b'a' + 26) as i8,
            b'0'..=b'9' => (c - b'0' + 52) as i8,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => -1,
        }
    }
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for &c in s.as_bytes() {
        let v = inv(c);
        if v < 0 {
            continue;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_roundtrip() {
        assert_eq!(b64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(b64_decode("dXNlcjpwYXNz"), b"user:pass");
        assert_eq!(b64(b"a"), "YQ==");
        assert_eq!(b64_decode("YQ=="), b"a");
    }

    #[test]
    fn challenge_parse() {
        let c = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io""#;
        let p = parse_challenge(c);
        assert_eq!(p[0].1, "https://auth.docker.io/token");
        assert_eq!(p[1].1, "registry.docker.io");
    }
}
