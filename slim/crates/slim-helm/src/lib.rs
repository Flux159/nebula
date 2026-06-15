//! slim-helm: chart load + values merge + Go-template(+sprig) render, applied
//! through the slim-kube facade. No Tiller, no cluster Secrets — release state
//! is a local file. Used by the standalone `helm-slim` binary.

pub mod sprig;

use serde_json::{json, Value};
use slim_client::http::Client;
use slim_kube::Facade;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct HelmError(pub String);
impl std::fmt::Display for HelmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for HelmError {}
fn he<T>(s: impl Into<String>) -> Result<T, HelmError> {
    Err(HelmError(s.into()))
}
pub type HelmResult<T> = Result<T, HelmError>;

pub struct Chart {
    pub name: String,
    pub version: String,
    pub app_version: String,
    pub default_values: Value,
    /// (relative path, content) of manifest templates.
    pub templates: Vec<(String, String)>,
    /// concatenated helper/define library (_helpers.tpl etc.).
    pub library: String,
}

impl Chart {
    /// Load from a directory or a .tgz archive.
    pub fn load(path: &Path) -> HelmResult<Chart> {
        if path.is_dir() {
            Self::load_dir(path)
        } else if path
            .extension()
            .map(|e| e == "tgz" || e == "gz")
            .unwrap_or(false)
        {
            Self::load_tgz(path)
        } else {
            he(format!("not a chart: {}", path.display()))
        }
    }

    fn load_dir(dir: &Path) -> HelmResult<Chart> {
        let chart_yaml = std::fs::read_to_string(dir.join("Chart.yaml"))
            .map_err(|e| HelmError(format!("Chart.yaml: {e}")))?;
        let meta: Value = yaml_to_json(&chart_yaml)?;
        let name = meta
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("chart")
            .to_string();
        let version = meta
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0")
            .to_string();
        let app_version = meta
            .get("appVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let default_values = match std::fs::read_to_string(dir.join("values.yaml")) {
            Ok(s) => yaml_to_json(&s)?,
            Err(_) => json!({}),
        };

        let mut templates = Vec::new();
        let mut library = String::new();
        let tdir = dir.join("templates");
        collect_templates(&tdir, &tdir, &mut templates, &mut library)?;
        Ok(Chart {
            name,
            version,
            app_version,
            default_values,
            templates,
            library,
        })
    }

    fn load_tgz(path: &Path) -> HelmResult<Chart> {
        let f = std::fs::File::open(path).map_err(|e| HelmError(e.to_string()))?;
        let gz = flate2::read::GzDecoder::new(f);
        let mut ar = tar::Archive::new(gz);
        let tmp = std::env::temp_dir().join(format!("slimhelm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        ar.unpack(&tmp).map_err(|e| HelmError(e.to_string()))?;
        // chart is the single top dir
        let entry = std::fs::read_dir(&tmp)
            .map_err(|e| HelmError(e.to_string()))?
            .filter_map(|e| e.ok())
            .find(|e| e.path().is_dir())
            .ok_or_else(|| HelmError("empty chart archive".into()))?;
        let c = Self::load_dir(&entry.path());
        let _ = std::fs::remove_dir_all(&tmp);
        c
    }
}

fn collect_templates(
    base: &Path,
    dir: &Path,
    templates: &mut Vec<(String, String)>,
    library: &mut String,
) -> HelmResult<()> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            collect_templates(base, &p, templates, library)?;
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&p) else {
            continue;
        };
        if name.ends_with(".tpl") || name.starts_with('_') {
            library.push_str(&content);
            library.push('\n');
        } else if name == "NOTES.txt" {
            // skip
        } else if name.ends_with(".yaml") || name.ends_with(".yml") {
            let rel = p
                .strip_prefix(base)
                .unwrap_or(&p)
                .to_string_lossy()
                .into_owned();
            templates.push((rel, content));
        }
    }
    Ok(())
}

pub struct RenderOptions {
    pub release: String,
    pub namespace: String,
    pub is_upgrade: bool,
}

/// Render every template, returning concatenated multi-doc YAML.
pub fn render(chart: &Chart, values: &Value, opts: &RenderOptions) -> HelmResult<String> {
    let ctx = json!({
        "Values": values,
        "Chart": {
            "Name": chart.name,
            "Version": chart.version,
            "AppVersion": chart.app_version,
        },
        "Release": {
            "Name": opts.release,
            "Namespace": opts.namespace,
            "Service": "Helm",
            "IsInstall": !opts.is_upgrade,
            "IsUpgrade": opts.is_upgrade,
            "Revision": 1,
        },
        "Capabilities": {
            "KubeVersion": {"Version": "v1.29.0", "Major": "1", "Minor": "29"},
            "APIVersions": [],
            "HelmVersion": {"Version": "slim-0.1.0"},
        },
        "Files": {},
        "Template": {"Name": chart.name, "BasePath": format!("{}/templates", chart.name)},
    });

    let mut out = String::new();
    for (rel, content) in &chart.templates {
        let full = format!("{}\n{}", chart.library, content);
        let mut tmpl =
            slim_tmpl::Template::parse(&full).map_err(|e| HelmError(format!("{rel}: {e}")))?;
        sprig::register(&mut tmpl);
        let rendered = tmpl
            .render(&ctx)
            .map_err(|e| HelmError(format!("{rel}: {e}")))?;
        if rendered.trim().is_empty() {
            continue;
        }
        out.push_str(&format!(
            "---\n# Source: {}/templates/{}\n",
            chart.name, rel
        ));
        out.push_str(rendered.trim_end());
        out.push('\n');
    }
    Ok(out)
}

// ---------- values ----------

/// chart defaults ← values files ← --set overrides.
pub fn build_values(chart: &Chart, files: &[String], sets: &[String]) -> HelmResult<Value> {
    let mut v = chart.default_values.clone();
    for f in files {
        let content = std::fs::read_to_string(f).map_err(|e| HelmError(format!("{f}: {e}")))?;
        let fv = yaml_to_json(&content)?;
        deep_merge(&mut v, &fv);
    }
    for s in sets {
        apply_set(&mut v, s)?;
    }
    Ok(v)
}

fn deep_merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, ov) in o {
                deep_merge(b.entry(k.clone()).or_insert(Value::Null), ov);
            }
        }
        (b, o) => *b = o.clone(),
    }
}

fn apply_set(root: &mut Value, spec: &str) -> HelmResult<()> {
    let (path, raw) = spec
        .split_once('=')
        .ok_or_else(|| HelmError(format!("invalid --set {spec}")))?;
    let val = parse_scalar(raw);
    let keys: Vec<&str> = path.split('.').collect();
    if !root.is_object() {
        *root = json!({});
    }
    let mut cur = root;
    for (i, k) in keys.iter().enumerate() {
        if i + 1 == keys.len() {
            cur[*k] = val.clone();
        } else {
            if !cur.get(*k).map(|v| v.is_object()).unwrap_or(false) {
                cur[*k] = json!({});
            }
            cur = cur.get_mut(*k).unwrap();
        }
    }
    Ok(())
}

fn parse_scalar(s: &str) -> Value {
    match s {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        _ => {
            if let Ok(i) = s.parse::<i64>() {
                Value::from(i)
            } else if let Ok(f) = s.parse::<f64>() {
                Value::from(f)
            } else {
                Value::String(s.to_string())
            }
        }
    }
}

// ---------- install / uninstall / list ----------

pub struct Helm<'a> {
    pub client: &'a Client,
    pub namespace: String,
}

impl<'a> Helm<'a> {
    pub fn new(client: &'a Client, namespace: &str) -> Self {
        Helm {
            client,
            namespace: if namespace.is_empty() {
                "default".into()
            } else {
                namespace.into()
            },
        }
    }

    pub fn install(
        &self,
        release: &str,
        chart: &Chart,
        values: &Value,
        out: &mut dyn FnMut(&str),
    ) -> HelmResult<Vec<String>> {
        let opts = RenderOptions {
            release: release.to_string(),
            namespace: self.namespace.clone(),
            is_upgrade: false,
        };
        let manifests = render(chart, values, &opts)?;
        let facade = Facade::new(self.client, &self.namespace);
        let skipped = facade
            .apply_yaml(&manifests, out)
            .map_err(|e| HelmError(e.to_string()))?;
        save_release(
            release,
            &self.namespace,
            &chart.name,
            &chart.version,
            &manifests,
        )?;
        out(&format!(
            "NAME: {release}\nLAST DEPLOYED: {}\nNAMESPACE: {}\nSTATUS: deployed\nREVISION: 1\n",
            slim_kube_now(),
            self.namespace
        ));
        // Returns the kinds the facade couldn't map (CRDs, RBAC, etc.) so the
        // CLI can warn or fail under --strict.
        Ok(skipped)
    }

    pub fn uninstall(&self, release: &str, out: &mut dyn FnMut(&str)) -> HelmResult<()> {
        let rec = load_release(release)?;
        let facade = Facade::new(self.client, &rec.namespace);
        facade
            .delete_yaml(&rec.manifests, out)
            .map_err(|e| HelmError(e.to_string()))?;
        delete_release(release);
        out(&format!("release \"{release}\" uninstalled\n"));
        Ok(())
    }

    pub fn list(&self) -> HelmResult<Vec<ReleaseRecord>> {
        Ok(list_releases())
    }
}

// ---------- release store (local) ----------

#[derive(Debug, Clone)]
pub struct ReleaseRecord {
    pub name: String,
    pub namespace: String,
    pub chart: String,
    pub version: String,
    pub manifests: String,
}

fn release_dir() -> PathBuf {
    let base = std::env::var("NEBULA_HOME")
        .map(|h| format!("{h}/slim/helm"))
        .unwrap_or_else(|_| {
            format!(
                "{}/.nebula/slim/helm",
                std::env::var("HOME").unwrap_or_default()
            )
        });
    PathBuf::from(base)
}

fn save_release(
    name: &str,
    ns: &str,
    chart: &str,
    version: &str,
    manifests: &str,
) -> HelmResult<()> {
    let dir = release_dir();
    std::fs::create_dir_all(&dir).map_err(|e| HelmError(e.to_string()))?;
    let header = format!("#name:{name}\n#namespace:{ns}\n#chart:{chart}\n#version:{version}\n");
    std::fs::write(
        dir.join(format!("{name}.release")),
        format!("{header}{manifests}"),
    )
    .map_err(|e| HelmError(e.to_string()))?;
    Ok(())
}

fn load_release(name: &str) -> HelmResult<ReleaseRecord> {
    let p = release_dir().join(format!("{name}.release"));
    let content = std::fs::read_to_string(&p)
        .map_err(|_| HelmError(format!("release: not found: {name}")))?;
    Ok(parse_release(name, &content))
}

fn parse_release(name: &str, content: &str) -> ReleaseRecord {
    let mut ns = "default".to_string();
    let mut chart = String::new();
    let mut version = String::new();
    let mut body = String::new();
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("#namespace:") {
            ns = v.to_string();
        } else if let Some(v) = line.strip_prefix("#chart:") {
            chart = v.to_string();
        } else if let Some(v) = line.strip_prefix("#version:") {
            version = v.to_string();
        } else if line.starts_with("#name:") {
            // skip
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    ReleaseRecord {
        name: name.to_string(),
        namespace: ns,
        chart,
        version,
        manifests: body,
    }
}

fn delete_release(name: &str) {
    let _ = std::fs::remove_file(release_dir().join(format!("{name}.release")));
}

fn list_releases() -> Vec<ReleaseRecord> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(release_dir()) {
        for e in rd.flatten() {
            if e.path()
                .extension()
                .map(|x| x == "release")
                .unwrap_or(false)
            {
                if let Ok(content) = std::fs::read_to_string(e.path()) {
                    let name = e
                        .path()
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    out.push(parse_release(&name, &content));
                }
            }
        }
    }
    out
}

// ---------- helpers ----------

fn yaml_to_json(s: &str) -> HelmResult<Value> {
    let v: serde_yaml::Value =
        serde_yaml::from_str(s).map_err(|e| HelmError(format!("YAML: {e}")))?;
    serde_json::to_value(&v).map_err(|e| HelmError(e.to_string()))
}

fn slim_kube_now() -> String {
    // Coarse timestamp without a time dep.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch {secs}")
}

/// Read a values file from stdin or path (helper for the binary).
pub fn read_values_file(path: &str) -> HelmResult<String> {
    if path == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| HelmError(e.to_string()))?;
        Ok(s)
    } else {
        std::fs::read_to_string(path).map_err(|e| HelmError(format!("{path}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_and_set() {
        let mut base = json!({"image": {"repo": "nginx", "tag": "1.0"}, "replicas": 1});
        deep_merge(&mut base, &json!({"image": {"tag": "2.0"}}));
        assert_eq!(base["image"]["tag"], json!("2.0"));
        assert_eq!(base["image"]["repo"], json!("nginx"));
        apply_set(&mut base, "replicas=3").unwrap();
        assert_eq!(base["replicas"], json!(3));
        apply_set(&mut base, "image.pullPolicy=Always").unwrap();
        assert_eq!(base["image"]["pullPolicy"], json!("Always"));
    }

    #[test]
    fn render_a_chart() {
        let dir = std::env::temp_dir().join(format!("slimhelm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("templates")).unwrap();
        std::fs::write(
            dir.join("Chart.yaml"),
            "name: web\nversion: 1.0.0\nappVersion: \"2\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("values.yaml"),
            "replicas: 2\nimage: nginx:alpine\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("templates/_helpers.tpl"),
            "{{- define \"web.fullname\" -}}\n{{ .Release.Name }}-web\n{{- end -}}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("templates/deployment.yaml"),
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {{ include \"web.fullname\" . }}\nspec:\n  replicas: {{ .Values.replicas }}\n  template:\n    spec:\n      containers:\n        - name: web\n          image: {{ .Values.image | quote }}\n",
        )
        .unwrap();
        let chart = Chart::load(&dir).unwrap();
        let values = build_values(&chart, &[], &["replicas=5".into()]).unwrap();
        let opts = RenderOptions {
            release: "rel".into(),
            namespace: "default".into(),
            is_upgrade: false,
        };
        let out = render(&chart, &values, &opts).unwrap();
        assert!(out.contains("name: rel-web"), "got:\n{out}");
        assert!(out.contains("replicas: 5"), "got:\n{out}");
        assert!(out.contains("image: \"nginx:alpine\""), "got:\n{out}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
