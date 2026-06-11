//! Image reference parsing: [registry/]repo[:tag][@digest] with docker's
//! defaulting rules (docker.io, library/, :latest).

#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    pub registry: String, // host[:port]
    pub repo: String,     // e.g. library/alpine
    pub tag: String,      // empty if digest pinned
    pub digest: String,   // "sha256:..." or empty
}

impl Reference {
    pub fn parse(s: &str) -> Reference {
        let (rest, digest) = match s.split_once('@') {
            Some((r, d)) => (r, d.to_string()),
            None => (s, String::new()),
        };
        // The first path component is a registry iff it contains '.' or ':'
        // or is "localhost" (docker's rule).
        let (registry, path) = match rest.split_once('/') {
            Some((first, remainder))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_string(), remainder.to_string())
            }
            _ => ("docker.io".to_string(), rest.to_string()),
        };
        // Tag: after the last ':' if it's not part of a port (no '/' after).
        let (repo, mut tag) = match path.rsplit_once(':') {
            Some((r, t)) if !t.contains('/') => (r.to_string(), t.to_string()),
            _ => (path.clone(), String::new()),
        };
        if tag.is_empty() && digest.is_empty() {
            tag = "latest".to_string();
        }
        let repo = if registry == "docker.io" && !repo.contains('/') {
            format!("library/{repo}")
        } else {
            repo
        };
        Reference { registry, repo, tag, digest }
    }

    /// Registry API host (docker.io → registry-1.docker.io). `SLIM_REGISTRY_MIRROR`
    /// redirects docker.io pulls through a pull-through mirror (e.g. mirror.gcr.io
    /// or a local registry:2) — the reference identity/tag is unchanged, only the
    /// network host. Lets CI/offline setups avoid Docker Hub anonymous rate limits.
    pub fn api_host(&self) -> String {
        if self.registry == "docker.io" {
            if let Ok(m) = std::env::var("SLIM_REGISTRY_MIRROR") {
                let m = m.trim();
                if !m.is_empty() {
                    return m.to_string();
                }
            }
            return "registry-1.docker.io".to_string();
        }
        self.registry.clone()
    }

    /// Canonical display form: docker.io/library/x → x, the way docker
    /// prints repo tags.
    pub fn familiar(&self) -> String {
        let repo = if self.registry == "docker.io" {
            self.repo.strip_prefix("library/").unwrap_or(&self.repo).to_string()
        } else {
            format!("{}/{}", self.registry, self.repo)
        };
        if !self.tag.is_empty() {
            format!("{repo}:{}", self.tag)
        } else {
            format!("{repo}@{}", self.digest)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rules() {
        let r = Reference::parse("alpine");
        assert_eq!(
            (r.registry.as_str(), r.repo.as_str(), r.tag.as_str()),
            ("docker.io", "library/alpine", "latest")
        );
        let r = Reference::parse("alpine:3.19");
        assert_eq!(r.tag, "3.19");
        assert_eq!(r.familiar(), "alpine:3.19");
        let r = Reference::parse("ghcr.io/owner/app:v1");
        assert_eq!(
            (r.registry.as_str(), r.repo.as_str(), r.tag.as_str()),
            ("ghcr.io", "owner/app", "v1")
        );
        let r = Reference::parse("localhost:5000/x");
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repo, "x");
        let r = Reference::parse("alpine@sha256:abcd");
        assert_eq!(r.digest, "sha256:abcd");
        assert_eq!(r.tag, "");
        let r = Reference::parse("louislam/uptime-kuma:1");
        assert_eq!(r.repo, "louislam/uptime-kuma");
    }
}
