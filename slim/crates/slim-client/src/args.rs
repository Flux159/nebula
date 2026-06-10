//! Tiny getopt-style parser for docker-CLI args. Handles combined short flags
//! (`-it`), `--flag=value`, `--flag value`, repeated flags, and the trailing
//! positional args (image + command for `run`).
//!
//! Keys are normalized to their dash-stripped form and recorded under BOTH the
//! token's and its alias's stripped names, so a caller can query a flag by
//! either its short or long spelling (`p.flag("-q")` or `p.flag("quiet")`).

use std::collections::BTreeMap;

pub struct Parsed {
    pub values: BTreeMap<String, Vec<String>>,
    pub flags: BTreeMap<String, bool>,
    pub positional: Vec<String>,
}

fn norm(k: &str) -> String {
    k.trim_start_matches('-').to_string()
}

impl Parsed {
    pub fn flag(&self, name: &str) -> bool {
        self.flags.get(&norm(name)).copied().unwrap_or(false)
    }
    pub fn first(&self, name: &str) -> Option<&str> {
        self.values.get(&norm(name)).and_then(|v| v.first()).map(|s| s.as_str())
    }
    pub fn all(&self, name: &str) -> Vec<String> {
        self.values.get(&norm(name)).cloned().unwrap_or_default()
    }
    fn set_flag(&mut self, token: &str, canon: &str) {
        self.flags.insert(norm(token), true);
        self.flags.insert(norm(canon), true);
    }
    fn add_value(&mut self, token: &str, canon: &str, val: String) {
        self.values.entry(norm(token)).or_default().push(val.clone());
        if norm(token) != norm(canon) {
            self.values.entry(norm(canon)).or_default().push(val);
        }
    }
}

pub fn parse(
    argv: &[String],
    bools: &[&str],
    valued: &[&str],
    aliases: &[(&str, &str)],
    stop_at_first_positional: bool,
) -> Result<Parsed, String> {
    let alias = |k: &str| -> String {
        aliases.iter().find(|(a, _)| *a == k).map(|(_, c)| c.to_string()).unwrap_or_else(|| k.to_string())
    };
    let in_list = |k: &str, list: &[&str]| -> bool {
        let ck = alias(k);
        list.iter().any(|e| alias(e) == ck || *e == k || norm(e) == norm(&ck))
    };
    let is_bool = |k: &str| in_list(k, bools);
    let is_valued = |k: &str| in_list(k, valued);

    let mut out = Parsed { values: BTreeMap::new(), flags: BTreeMap::new(), positional: Vec::new() };
    let mut i = 0;
    let mut only_positional = false;
    while i < argv.len() {
        let a = &argv[i];
        if only_positional || a == "-" || !a.starts_with('-') {
            out.positional.push(a.clone());
            if stop_at_first_positional {
                out.positional.extend(argv[i + 1..].iter().cloned());
                break;
            }
            i += 1;
            continue;
        }
        if a == "--" {
            only_positional = true;
            i += 1;
            continue;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (format!("--{n}"), Some(v.to_string())),
                None => (a.clone(), None),
            };
            let canon = alias(&name);
            if is_bool(&name) {
                out.set_flag(&name, &canon);
                i += 1;
            } else if is_valued(&name) {
                let val = match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        argv.get(i).cloned().ok_or_else(|| format!("flag {name} needs a value"))?
                    }
                };
                out.add_value(&name, &canon, val);
                i += 1;
            } else {
                return Err(format!("unknown flag: {name}"));
            }
        } else {
            let chars: Vec<char> = a[1..].chars().collect();
            let mut j = 0;
            while j < chars.len() {
                let short = format!("-{}", chars[j]);
                let canon = alias(&short);
                if is_bool(&short) {
                    out.set_flag(&short, &canon);
                    j += 1;
                } else if is_valued(&short) {
                    let rest: String = chars[j + 1..].iter().collect();
                    let val = if !rest.is_empty() {
                        rest
                    } else {
                        i += 1;
                        argv.get(i).cloned().ok_or_else(|| format!("flag {short} needs a value"))?
                    };
                    out.add_value(&short, &canon, val);
                    break;
                } else {
                    return Err(format!("unknown flag: {short}"));
                }
            }
            i += 1;
        }
    }
    Ok(out)
}
