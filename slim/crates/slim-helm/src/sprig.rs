//! The sprig function subset Helm charts actually use, registered into a
//! slim-tmpl Template. Each is a pure `fn(&[Value]) -> Result<Value, _>`.
//! `include`/`tpl` are handled inside slim-tmpl's evaluator (they need engine
//! access); everything here is value-in/value-out.

use slim_tmpl::{Template, TmplError};
use serde_json::Value;

fn e<T>(s: impl Into<String>) -> Result<T, TmplError> {
    Err(TmplError(s.into()))
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

pub fn register(t: &mut Template) {
    // string
    t.add_func("default", |a| {
        // sprig: default DEFAULT GIVEN -> GIVEN if truthy else DEFAULT
        let def = a.first().cloned().unwrap_or(Value::Null);
        let given = a.get(1).cloned().unwrap_or(Value::Null);
        Ok(if truthy(&given) { given } else { def })
    });
    t.add_func("quote", |a| Ok(Value::String(format!("\"{}\"", s(a.first().unwrap_or(&Value::Null))))));
    t.add_func("squote", |a| Ok(Value::String(format!("'{}'", s(a.first().unwrap_or(&Value::Null))))));
    t.add_func("upper", |a| Ok(Value::String(s(a.first().unwrap_or(&Value::Null)).to_uppercase())));
    t.add_func("lower", |a| Ok(Value::String(s(a.first().unwrap_or(&Value::Null)).to_lowercase())));
    t.add_func("title", |a| Ok(Value::String(title_case(&s(a.first().unwrap_or(&Value::Null))))));
    t.add_func("trim", |a| Ok(Value::String(s(a.first().unwrap_or(&Value::Null)).trim().to_string())));
    // last-arg-is-subject convention (sprig is data-last)
    t.add_func("trimSuffix", |a| {
        let suf = s(a.first().unwrap_or(&Value::Null));
        let v = s(a.get(1).unwrap_or(&Value::Null));
        Ok(Value::String(v.strip_suffix(&suf).unwrap_or(&v).to_string()))
    });
    t.add_func("trimPrefix", |a| {
        let pre = s(a.first().unwrap_or(&Value::Null));
        let v = s(a.get(1).unwrap_or(&Value::Null));
        Ok(Value::String(v.strip_prefix(&pre).unwrap_or(&v).to_string()))
    });
    t.add_func("replace", |a| {
        let from = s(a.first().unwrap_or(&Value::Null));
        let to = s(a.get(1).unwrap_or(&Value::Null));
        let v = s(a.get(2).unwrap_or(&Value::Null));
        Ok(Value::String(v.replace(&from, &to)))
    });
    t.add_func("repeat", |a| {
        let n = a.first().and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
        Ok(Value::String(s(a.get(1).unwrap_or(&Value::Null)).repeat(n)))
    });
    t.add_func("trunc", |a| {
        let n = a.first().and_then(|v| v.as_i64()).unwrap_or(0);
        let v = s(a.get(1).unwrap_or(&Value::Null));
        let out: String = if n >= 0 {
            v.chars().take(n as usize).collect()
        } else {
            let k = (-n) as usize;
            v.chars().skip(v.chars().count().saturating_sub(k)).collect()
        };
        Ok(Value::String(out))
    });
    t.add_func("contains", |a| {
        Ok(Value::Bool(s(a.get(1).unwrap_or(&Value::Null)).contains(&s(a.first().unwrap_or(&Value::Null)))))
    });
    t.add_func("hasPrefix", |a| {
        Ok(Value::Bool(s(a.get(1).unwrap_or(&Value::Null)).starts_with(&s(a.first().unwrap_or(&Value::Null)))))
    });
    t.add_func("hasSuffix", |a| {
        Ok(Value::Bool(s(a.get(1).unwrap_or(&Value::Null)).ends_with(&s(a.first().unwrap_or(&Value::Null)))))
    });
    t.add_func("indent", |a| {
        let n = a.first().and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
        Ok(Value::String(indent(&s(a.get(1).unwrap_or(&Value::Null)), n, false)))
    });
    t.add_func("nindent", |a| {
        let n = a.first().and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
        Ok(Value::String(indent(&s(a.get(1).unwrap_or(&Value::Null)), n, true)))
    });
    t.add_func("toYaml", |a| Ok(Value::String(to_yaml(a.first().unwrap_or(&Value::Null)))));
    t.add_func("toJson", |a| Ok(Value::String(serde_json::to_string(a.first().unwrap_or(&Value::Null)).unwrap_or_default())));
    t.add_func("b64enc", |a| Ok(Value::String(b64(s(a.first().unwrap_or(&Value::Null)).as_bytes()))));
    t.add_func("b64dec", |a| Ok(Value::String(String::from_utf8_lossy(&b64d(&s(a.first().unwrap_or(&Value::Null)))).into_owned())));
    t.add_func("required", |a| {
        let msg = s(a.first().unwrap_or(&Value::Null));
        let v = a.get(1).cloned().unwrap_or(Value::Null);
        if truthy(&v) {
            Ok(v)
        } else {
            e(msg)
        }
    });
    t.add_func("empty", |a| Ok(Value::Bool(!truthy(a.first().unwrap_or(&Value::Null)))));
    t.add_func("coalesce", |a| Ok(a.iter().find(|v| truthy(v)).cloned().unwrap_or(Value::Null)));
    t.add_func("ternary", |a| {
        // ternary TRUEVAL FALSEVAL COND
        let cond = truthy(a.get(2).unwrap_or(&Value::Null));
        Ok(if cond { a.first().cloned().unwrap_or(Value::Null) } else { a.get(1).cloned().unwrap_or(Value::Null) })
    });
    // numeric
    t.add_func("add", |a| Ok(Value::from(a.iter().filter_map(|v| v.as_i64()).sum::<i64>())));
    t.add_func("sub", |a| {
        let x = a.first().and_then(|v| v.as_i64()).unwrap_or(0);
        let y = a.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
        Ok(Value::from(x - y))
    });
    t.add_func("mul", |a| Ok(Value::from(a.iter().filter_map(|v| v.as_i64()).product::<i64>())));
    t.add_func("div", |a| {
        let x = a.first().and_then(|v| v.as_i64()).unwrap_or(0);
        let y = a.get(1).and_then(|v| v.as_i64()).unwrap_or(1);
        Ok(Value::from(if y != 0 { x / y } else { 0 }))
    });
    t.add_func("int", |a| Ok(Value::from(a.first().and_then(coerce_i64).unwrap_or(0))));
    // collections
    t.add_func("list", |a| Ok(Value::Array(a.to_vec())));
    t.add_func("first", |a| Ok(a.first().and_then(|v| v.as_array()).and_then(|x| x.first()).cloned().unwrap_or(Value::Null)));
    t.add_func("kindOf", |a| Ok(Value::String(kind_of(a.first().unwrap_or(&Value::Null)).into())));
}

fn coerce_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "invalid",
        Value::Bool(_) => "bool",
        Value::Number(_) => "float64",
        Value::String(_) => "string",
        Value::Array(_) => "slice",
        Value::Object(_) => "map",
    }
}

fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn indent(s: &str, n: usize, leading_newline: bool) -> String {
    let pad = " ".repeat(n);
    let body = s.lines().map(|l| format!("{pad}{l}")).collect::<Vec<_>>().join("\n");
    if leading_newline {
        format!("\n{body}")
    } else {
        body
    }
}

/// Serialize a JSON value to YAML (helm's toYaml). Uses serde_yaml then trims
/// the trailing newline (helm's toYaml has none).
pub fn to_yaml(v: &Value) -> String {
    serde_yaml::to_string(v).unwrap_or_default().trim_end().to_string()
}

pub fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn b64d(s: &str) -> Vec<u8> {
    let inv = |c: u8| -> i8 {
        match c {
            b'A'..=b'Z' => (c - b'A') as i8,
            b'a'..=b'z' => (c - b'a' + 26) as i8,
            b'0'..=b'9' => (c - b'0' + 52) as i8,
            b'+' => 62,
            b'/' => 63,
            _ => -1,
        }
    };
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
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
