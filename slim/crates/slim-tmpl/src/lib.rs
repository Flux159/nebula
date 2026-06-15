//! slim-tmpl: a Go text/template subset evaluator.
//!
//! Used for `docker inspect -f` / `--format` and (later) Helm chart rendering.
//! Supports: text + `{{ actions }}`, `{{- -}}` trim, comments, field access
//! `.A.B`, `$var`, `$` root, pipelines `a | f arg`, parenthesized calls,
//! literals (string/int/float/bool/nil), `if/else if/else/end`,
//! `range [$i, $v :=] .. else .. end`, `with .. else .. end`,
//! `define`/`template`/`block`, variable assignment, break/continue, and the
//! Go built-ins (and/or/not/eq/ne/lt/le/gt/ge/len/index/slice/printf/print/
//! json/...). Extra funcs registrable for sprig (Helm).
//!
//! Divergence from Go (documented): printf `%v` of a map/array prints compact
//! JSON; numeric coercion compares ints and floats numerically.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

pub type Func = fn(&[Value]) -> Result<Value, TmplError>;

#[derive(Debug)]
pub struct TmplError(pub String);
impl fmt::Display for TmplError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "template: {}", self.0)
    }
}
impl std::error::Error for TmplError {}
fn err<T>(s: impl Into<String>) -> Result<T, TmplError> {
    Err(TmplError(s.into()))
}

pub fn render(src: &str, ctx: &Value) -> Result<String, TmplError> {
    Template::parse(src)?.render(ctx)
}

// ---------- lexer ----------

#[derive(Debug, Clone)]
enum Tok {
    Text(String),
    Action(String), // the inside of {{ }}, trimmed
}

fn lex(src: &str) -> Result<Vec<Tok>, TmplError> {
    let b = src.as_bytes();
    let mut toks = Vec::new();
    let mut text = String::new();
    let mut i = 0;
    while i < b.len() {
        if i + 1 < b.len() && b[i] == b'{' && b[i + 1] == b'{' {
            i += 2;
            let mut trim_left = false;
            if i < b.len() && b[i] == b'-' {
                trim_left = true;
                i += 1;
            }
            let start = i;
            let mut end = None;
            while i + 1 < b.len() {
                if b[i] == b'}' && b[i + 1] == b'}' {
                    end = Some(i);
                    break;
                }
                i += 1;
            }
            let close = end.ok_or_else(|| TmplError("unclosed action".into()))?;
            let mut inner = src[start..close].to_string();
            let mut trim_right = false;
            if inner.trim_end().ends_with('-') {
                let t = inner.trim_end();
                inner = t[..t.len() - 1].to_string();
                trim_right = true;
            }
            i = close + 2;
            if trim_left {
                while text.ends_with([' ', '\t', '\n', '\r']) {
                    text.pop();
                }
            }
            if !text.is_empty() {
                toks.push(Tok::Text(std::mem::take(&mut text)));
            }
            let inner_trimmed = inner.trim().to_string();
            if !inner_trimmed.starts_with("/*") {
                toks.push(Tok::Action(inner_trimmed));
            }
            if trim_right {
                while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
                    i += 1;
                }
            }
        } else {
            text.push(b[i] as char);
            i += 1;
        }
    }
    if !text.is_empty() {
        toks.push(Tok::Text(text));
    }
    Ok(toks)
}

// ---------- node tree ----------

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Output(Expr),
    Assign {
        name: String,
        expr: Expr,
        declare: bool,
    },
    If {
        branches: Vec<(Expr, Vec<Node>)>,
        else_body: Option<Vec<Node>>,
    },
    Range {
        key: Option<String>,
        val: Option<String>,
        expr: Expr,
        body: Vec<Node>,
        else_body: Option<Vec<Node>>,
    },
    With {
        expr: Expr,
        body: Vec<Node>,
        else_body: Option<Vec<Node>>,
    },
    Template {
        name: String,
        arg: Option<Expr>,
    },
    Break,
    Continue,
}

#[derive(Debug, Clone)]
enum Expr {
    Dot,
    Root,
    Field(Vec<String>),
    Var(String, Vec<String>),
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Nil,
    Call(String, Vec<Expr>),
    Pipe(Box<Expr>, Box<Expr>),
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
    defines: &'a mut BTreeMap<String, Vec<Node>>,
}

impl Parser<'_> {
    fn parse_body(&mut self, stop: &[&str]) -> Result<(Vec<Node>, String), TmplError> {
        let mut out = Vec::new();
        while self.pos < self.toks.len() {
            match self.toks[self.pos].clone() {
                Tok::Text(t) => {
                    out.push(Node::Text(t));
                    self.pos += 1;
                }
                Tok::Action(a) => {
                    let kw = a.split_whitespace().next().unwrap_or("");
                    if stop.contains(&kw) {
                        self.pos += 1;
                        return Ok((out, a));
                    }
                    self.pos += 1;
                    if let Some(node) = self.parse_action(&a)? {
                        out.push(node);
                    }
                }
            }
        }
        if !stop.is_empty() {
            return err(format!("unexpected EOF, expected one of {stop:?}"));
        }
        Ok((out, String::new()))
    }

    fn parse_action(&mut self, a: &str) -> Result<Option<Node>, TmplError> {
        let kw = a.split_whitespace().next().unwrap_or("");
        match kw {
            "if" => {
                let cond = parse_pipeline(a[2..].trim())?;
                let mut branches = Vec::new();
                let (body, stopped) = self.parse_body(&["else", "end"])?;
                branches.push((cond, body));
                let mut else_body = None;
                let mut stopped = stopped;
                loop {
                    let skw = stopped.split_whitespace().next().unwrap_or("");
                    if skw == "end" {
                        break;
                    }
                    let rest = stopped["else".len()..].trim().to_string();
                    if let Some(elif) = rest.strip_prefix("if") {
                        let cond = parse_pipeline(elif.trim())?;
                        let (body, s2) = self.parse_body(&["else", "end"])?;
                        branches.push((cond, body));
                        stopped = s2;
                    } else {
                        let (body, _s) = self.parse_body(&["end"])?;
                        else_body = Some(body);
                        break;
                    }
                }
                Ok(Some(Node::If {
                    branches,
                    else_body,
                }))
            }
            "range" => {
                let (key, val, expr) = parse_range_head(a[5..].trim())?;
                let (body, stopped) = self.parse_body(&["else", "end"])?;
                let else_body = if stopped.starts_with("else") {
                    let (b, _) = self.parse_body(&["end"])?;
                    Some(b)
                } else {
                    None
                };
                Ok(Some(Node::Range {
                    key,
                    val,
                    expr,
                    body,
                    else_body,
                }))
            }
            "with" => {
                let expr = parse_pipeline(a[4..].trim())?;
                let (body, stopped) = self.parse_body(&["else", "end"])?;
                let else_body = if stopped.starts_with("else") {
                    let (b, _) = self.parse_body(&["end"])?;
                    Some(b)
                } else {
                    None
                };
                Ok(Some(Node::With {
                    expr,
                    body,
                    else_body,
                }))
            }
            "define" => {
                let name = unquote(a[6..].trim());
                let (body, _) = self.parse_body(&["end"])?;
                self.defines.insert(name, body);
                Ok(None)
            }
            "block" => {
                let rest = a[5..].trim();
                let (name_part, arg_part) = split_first_token(rest);
                let name = unquote(name_part);
                let arg = if arg_part.trim().is_empty() {
                    None
                } else {
                    Some(parse_pipeline(arg_part.trim())?)
                };
                let (body, _) = self.parse_body(&["end"])?;
                self.defines.insert(name.clone(), body);
                Ok(Some(Node::Template { name, arg }))
            }
            "template" => {
                let rest = a[8..].trim();
                let (name_part, arg_part) = split_first_token(rest);
                let name = unquote(name_part);
                let arg = if arg_part.trim().is_empty() {
                    None
                } else {
                    Some(parse_pipeline(arg_part.trim())?)
                };
                Ok(Some(Node::Template { name, arg }))
            }
            "break" => Ok(Some(Node::Break)),
            "continue" => Ok(Some(Node::Continue)),
            "end" | "else" => err(format!("unexpected {kw}")),
            _ => {
                if a.starts_with('$') {
                    if let Some(idx) = find_assign(a) {
                        let name = a[1..idx.0].trim().to_string();
                        let expr = parse_pipeline(a[idx.1..].trim())?;
                        return Ok(Some(Node::Assign {
                            name,
                            expr,
                            declare: idx.2,
                        }));
                    }
                }
                Ok(Some(Node::Output(parse_pipeline(a)?)))
            }
        }
    }
}

fn find_assign(a: &str) -> Option<(usize, usize, bool)> {
    if let Some(p) = a.find(":=") {
        return Some((p, p + 2, true));
    }
    let b = a.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'=' {
            let prev = if i > 0 { b[i - 1] } else { b' ' };
            let next = if i + 1 < b.len() { b[i + 1] } else { b' ' };
            if next != b'=' && !matches!(prev, b'=' | b'!' | b'<' | b'>') {
                return Some((i, i + 1, false));
            }
        }
        i += 1;
    }
    None
}

fn parse_range_head(s: &str) -> Result<(Option<String>, Option<String>, Expr), TmplError> {
    if let Some(idx) = s.find(":=") {
        let vars = s[..idx].trim();
        let expr = parse_pipeline(s[idx + 2..].trim())?;
        let names: Vec<&str> = vars
            .split(',')
            .map(|v| v.trim().trim_start_matches('$'))
            .collect();
        return Ok(match names.as_slice() {
            [v] => (None, Some(v.to_string()), expr),
            [k, v] => (Some(k.to_string()), Some(v.to_string()), expr),
            _ => (None, None, expr),
        });
    }
    Ok((None, None, parse_pipeline(s)?))
}

// ---------- pipeline / expression parser ----------

fn parse_pipeline(s: &str) -> Result<Expr, TmplError> {
    let parts = split_top(s, '|');
    let mut iter = parts.into_iter();
    let first = iter
        .next()
        .ok_or_else(|| TmplError("empty pipeline".into()))?;
    let mut expr = parse_command(first.trim())?;
    for stage in iter {
        let staged = parse_command(stage.trim())?;
        expr = match staged {
            Expr::Call(name, mut args) => {
                args.push(expr);
                Expr::Call(name, args)
            }
            Expr::Field(f) if f.len() == 1 => Expr::Call(f[0].clone(), vec![expr]),
            other => Expr::Pipe(Box::new(expr), Box::new(other)),
        };
    }
    Ok(expr)
}

fn parse_command(s: &str) -> Result<Expr, TmplError> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(Expr::Dot);
    }
    if s.starts_with('(') && balanced(s) {
        return parse_pipeline(&s[1..s.len() - 1]);
    }
    let tokens = split_args(s);
    if tokens.len() == 1 {
        return parse_operand(&tokens[0]);
    }
    let name = &tokens[0];
    if is_ident(name) {
        let args = tokens[1..]
            .iter()
            .map(|t| parse_operand(t))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Expr::Call(name.clone(), args));
    }
    parse_operand(&tokens[0])
}

fn parse_operand(s: &str) -> Result<Expr, TmplError> {
    let s = s.trim();
    if s.starts_with('(') && balanced(s) {
        return parse_pipeline(&s[1..s.len() - 1]);
    }
    match s {
        "." => return Ok(Expr::Dot),
        "$" => return Ok(Expr::Root),
        "nil" => return Ok(Expr::Nil),
        "true" => return Ok(Expr::Bool(true)),
        "false" => return Ok(Expr::Bool(false)),
        _ => {}
    }
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('`') && s.ends_with('`') && s.len() >= 2)
    {
        return Ok(Expr::Str(unescape(&s[1..s.len() - 1], s.starts_with('"'))));
    }
    if let Ok(i) = s.parse::<i64>() {
        return Ok(Expr::Int(i));
    }
    if let Ok(f) = s.parse::<f64>() {
        return Ok(Expr::Float(f));
    }
    if let Some(rest) = s.strip_prefix('$') {
        if rest.starts_with('.') {
            let path: Vec<String> = rest
                .split('.')
                .filter(|p| !p.is_empty())
                .map(String::from)
                .collect();
            return Ok(Expr::Var(String::new(), path));
        }
        let parts: Vec<String> = rest
            .split('.')
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect();
        let (name, path) = if parts.is_empty() {
            (String::new(), vec![])
        } else {
            (parts[0].clone(), parts[1..].to_vec())
        };
        return Ok(Expr::Var(name, path));
    }
    if let Some(rest) = s.strip_prefix('.') {
        if rest.is_empty() {
            return Ok(Expr::Dot);
        }
        let path: Vec<String> = rest
            .split('.')
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect();
        return Ok(Expr::Field(path));
    }
    if is_ident(s) {
        return Ok(Expr::Call(s.to_string(), vec![]));
    }
    err(format!("cannot parse operand: {s}"))
}

// ---------- the template ----------

pub struct Template {
    nodes: Vec<Node>,
    defines: BTreeMap<String, Vec<Node>>,
    funcs: BTreeMap<String, Func>,
}

impl Template {
    pub fn parse(src: &str) -> Result<Template, TmplError> {
        let toks = lex(src)?;
        let mut defines = BTreeMap::new();
        let nodes = {
            let mut p = Parser {
                toks: &toks,
                pos: 0,
                defines: &mut defines,
            };
            let (nodes, _) = p.parse_body(&[])?;
            nodes
        };
        Ok(Template {
            nodes,
            defines,
            funcs: BTreeMap::new(),
        })
    }

    pub fn add_func(&mut self, name: &str, f: Func) {
        self.funcs.insert(name.to_string(), f);
    }

    pub fn add_associated(&mut self, name: &str, src: &str) -> Result<(), TmplError> {
        let toks = lex(src)?;
        let mut defines = std::mem::take(&mut self.defines);
        {
            let mut p = Parser {
                toks: &toks,
                pos: 0,
                defines: &mut defines,
            };
            let (nodes, _) = p.parse_body(&[])?;
            defines.insert(name.to_string(), nodes);
        }
        self.defines = defines;
        Ok(())
    }

    pub fn render(&self, ctx: &Value) -> Result<String, TmplError> {
        let mut out = String::new();
        let mut ev = Eval {
            root: ctx,
            defines: &self.defines,
            funcs: &self.funcs,
            vars: Vec::new(),
        };
        ev.exec(&self.nodes, ctx, &mut out)?;
        Ok(out)
    }
}

struct Eval<'a> {
    root: &'a Value,
    defines: &'a BTreeMap<String, Vec<Node>>,
    funcs: &'a BTreeMap<String, Func>,
    vars: Vec<(String, Value)>,
}

enum Flow {
    Normal,
    Break,
    Continue,
}

impl Eval<'_> {
    fn lookup_var(&self, name: &str) -> Value {
        self.vars
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Null)
    }

    fn exec(&mut self, nodes: &[Node], dot: &Value, out: &mut String) -> Result<Flow, TmplError> {
        for node in nodes {
            match node {
                Node::Text(t) => out.push_str(t),
                Node::Output(e) => {
                    let v = self.eval(e, dot)?;
                    out.push_str(&stringify(&v));
                }
                Node::Assign {
                    name,
                    expr,
                    declare,
                } => {
                    let v = self.eval(expr, dot)?;
                    if *declare {
                        self.vars.push((name.clone(), v));
                    } else if let Some(slot) = self.vars.iter_mut().rev().find(|(n, _)| n == name) {
                        slot.1 = v;
                    } else {
                        self.vars.push((name.clone(), v));
                    }
                }
                Node::If {
                    branches,
                    else_body,
                } => {
                    let mut done = false;
                    for (cond, body) in branches {
                        if truthy(&self.eval(cond, dot)?) {
                            let depth = self.vars.len();
                            let f = self.exec(body, dot, out)?;
                            self.vars.truncate(depth);
                            if !matches!(f, Flow::Normal) {
                                return Ok(f);
                            }
                            done = true;
                            break;
                        }
                    }
                    if !done {
                        if let Some(eb) = else_body {
                            let depth = self.vars.len();
                            let f = self.exec(eb, dot, out)?;
                            self.vars.truncate(depth);
                            if !matches!(f, Flow::Normal) {
                                return Ok(f);
                            }
                        }
                    }
                }
                Node::With {
                    expr,
                    body,
                    else_body,
                } => {
                    let v = self.eval(expr, dot)?;
                    if truthy(&v) {
                        let depth = self.vars.len();
                        let f = self.exec(body, &v, out)?;
                        self.vars.truncate(depth);
                        if !matches!(f, Flow::Normal) {
                            return Ok(f);
                        }
                    } else if let Some(eb) = else_body {
                        let f = self.exec(eb, dot, out)?;
                        if !matches!(f, Flow::Normal) {
                            return Ok(f);
                        }
                    }
                }
                Node::Range {
                    key,
                    val,
                    expr,
                    body,
                    else_body,
                } => {
                    let v = self.eval(expr, dot)?;
                    let items: Vec<(Value, Value)> = match &v {
                        Value::Array(a) => a
                            .iter()
                            .enumerate()
                            .map(|(i, x)| (Value::from(i as i64), x.clone()))
                            .collect(),
                        Value::Object(m) => m
                            .iter()
                            .map(|(k, x)| (Value::from(k.clone()), x.clone()))
                            .collect(),
                        _ => vec![],
                    };
                    if items.is_empty() {
                        if let Some(eb) = else_body {
                            let f = self.exec(eb, dot, out)?;
                            if !matches!(f, Flow::Normal) {
                                return Ok(f);
                            }
                        }
                        continue;
                    }
                    for (k, item) in items {
                        let depth = self.vars.len();
                        if let Some(kn) = key {
                            self.vars.push((kn.clone(), k.clone()));
                        }
                        if let Some(vn) = val {
                            self.vars.push((vn.clone(), item.clone()));
                        }
                        let f = self.exec(body, &item, out)?;
                        self.vars.truncate(depth);
                        match f {
                            Flow::Break => break,
                            Flow::Continue | Flow::Normal => {}
                        }
                    }
                }
                Node::Template { name, arg } => {
                    let body = self
                        .defines
                        .get(name)
                        .ok_or_else(|| TmplError(format!("no template {name:?}")))?;
                    let new_dot = match arg {
                        Some(e) => self.eval(e, dot)?,
                        None => Value::Null,
                    };
                    let depth = self.vars.len();
                    self.exec(body, &new_dot, out)?;
                    self.vars.truncate(depth);
                }
                Node::Break => return Ok(Flow::Break),
                Node::Continue => return Ok(Flow::Continue),
            }
        }
        Ok(Flow::Normal)
    }

    fn eval(&self, e: &Expr, dot: &Value) -> Result<Value, TmplError> {
        match e {
            Expr::Dot => Ok(dot.clone()),
            Expr::Root => Ok(self.root.clone()),
            Expr::Nil => Ok(Value::Null),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Int(i) => Ok(Value::from(*i)),
            Expr::Float(f) => Ok(Value::from(*f)),
            Expr::Str(s) => Ok(Value::String(s.clone())),
            Expr::Field(path) => Ok(get_path(dot, path)),
            Expr::Var(name, path) => {
                let base = if name.is_empty() {
                    self.root.clone()
                } else {
                    self.lookup_var(name)
                };
                Ok(get_path(&base, path))
            }
            Expr::Pipe(l, r) => {
                let lv = self.eval(l, dot)?;
                match &**r {
                    Expr::Call(name, args) => {
                        let mut argv: Vec<Value> = args
                            .iter()
                            .map(|a| self.eval(a, dot))
                            .collect::<Result<_, _>>()?;
                        argv.push(lv);
                        self.call(name, &argv)
                    }
                    _ => Ok(lv),
                }
            }
            Expr::Call(name, args) if name == "include" || name == "tpl" => {
                // Engine-level funcs: they render templates, so they need
                // access to defines/funcs (Helm's `include`/`tpl`).
                let argv: Vec<Value> = args
                    .iter()
                    .map(|a| self.eval(a, dot))
                    .collect::<Result<_, _>>()?;
                if name == "include" {
                    let tname = argv.first().map(stringify).unwrap_or_default();
                    let sub_dot = argv.get(1).cloned().unwrap_or(Value::Null);
                    let body = self
                        .defines
                        .get(&tname)
                        .ok_or_else(|| TmplError(format!("include: no template {tname:?}")))?;
                    let mut out = String::new();
                    let mut ev = Eval {
                        root: self.root,
                        defines: self.defines,
                        funcs: self.funcs,
                        vars: Vec::new(),
                    };
                    ev.exec(body, &sub_dot, &mut out)?;
                    Ok(Value::String(out))
                } else {
                    let src = argv.first().map(stringify).unwrap_or_default();
                    let sub_dot = argv.get(1).cloned().unwrap_or_else(|| dot.clone());
                    let t = Template::parse(&src)?;
                    let mut ev = Eval {
                        root: &sub_dot,
                        defines: self.defines,
                        funcs: self.funcs,
                        vars: Vec::new(),
                    };
                    let mut out = String::new();
                    ev.exec(&t.nodes, &sub_dot, &mut out)?;
                    Ok(Value::String(out))
                }
            }
            Expr::Call(name, args) => {
                if self.is_func(name) {
                    let argv: Vec<Value> = args
                        .iter()
                        .map(|a| self.eval(a, dot))
                        .collect::<Result<_, _>>()?;
                    self.call(name, &argv)
                } else if args.is_empty() {
                    Ok(get_path(dot, std::slice::from_ref(name)))
                } else {
                    err(format!("function {name:?} not defined"))
                }
            }
        }
    }

    fn is_func(&self, name: &str) -> bool {
        self.funcs.contains_key(name) || BUILTINS.contains(&name)
    }

    fn call(&self, name: &str, args: &[Value]) -> Result<Value, TmplError> {
        if let Some(f) = self.funcs.get(name) {
            return f(args);
        }
        builtin(name, args)
    }
}

fn get_path(v: &Value, path: &[String]) -> Value {
    let mut cur = v.clone();
    for key in path {
        cur = match &cur {
            Value::Object(m) => m.get(key).cloned().unwrap_or(Value::Null),
            _ => return Value::Null,
        };
    }
    cur
}

// ---------- builtins ----------

const BUILTINS: &[&str] = &[
    "and", "or", "not", "eq", "ne", "lt", "le", "gt", "ge", "len", "index", "slice", "print",
    "printf", "println", "json", "urlquery", "html", "js",
];

fn builtin(name: &str, args: &[Value]) -> Result<Value, TmplError> {
    match name {
        "and" => Ok(args
            .iter()
            .find(|v| !truthy(v))
            .cloned()
            .unwrap_or_else(|| args.last().cloned().unwrap_or(Value::Bool(false)))),
        "or" => Ok(args
            .iter()
            .find(|v| truthy(v))
            .cloned()
            .unwrap_or_else(|| args.last().cloned().unwrap_or(Value::Bool(false)))),
        "not" => Ok(Value::Bool(!truthy(args.first().unwrap_or(&Value::Null)))),
        "eq" => {
            let f = args.first().unwrap_or(&Value::Null);
            Ok(Value::Bool(
                args.len() == 1 || args[1..].iter().any(|a| values_eq(f, a)),
            ))
        }
        "ne" => Ok(Value::Bool(!values_eq(
            args.first().unwrap_or(&Value::Null),
            args.get(1).unwrap_or(&Value::Null),
        ))),
        "lt" => Ok(Value::Bool(cmp(args)? == std::cmp::Ordering::Less)),
        "le" => Ok(Value::Bool(cmp(args)? != std::cmp::Ordering::Greater)),
        "gt" => Ok(Value::Bool(cmp(args)? == std::cmp::Ordering::Greater)),
        "ge" => Ok(Value::Bool(cmp(args)? != std::cmp::Ordering::Less)),
        "len" => Ok(Value::from(
            length(args.first().unwrap_or(&Value::Null)) as i64
        )),
        "index" => index(args),
        "slice" => slice(args),
        "print" => Ok(Value::String(
            args.iter().map(stringify).collect::<Vec<_>>().join(""),
        )),
        "println" => Ok(Value::String(format!(
            "{}\n",
            args.iter().map(stringify).collect::<Vec<_>>().join(" ")
        ))),
        "printf" => printf(args),
        "json" => Ok(Value::String(
            serde_json::to_string(args.first().unwrap_or(&Value::Null)).unwrap_or_default(),
        )),
        "urlquery" | "html" | "js" => Ok(Value::String(stringify(
            args.first().unwrap_or(&Value::Null),
        ))),
        _ => err(format!("function {name:?} not defined")),
    }
}

fn length(v: &Value) -> usize {
    match v {
        Value::String(s) => s.chars().count(),
        Value::Array(a) => a.len(),
        Value::Object(m) => m.len(),
        _ => 0,
    }
}

fn index(args: &[Value]) -> Result<Value, TmplError> {
    let mut cur = args.first().cloned().unwrap_or(Value::Null);
    for k in &args[1..] {
        cur = match &cur {
            Value::Array(a) => {
                let i = k.as_i64().unwrap_or(-1);
                if i < 0 || i as usize >= a.len() {
                    return err("index out of range");
                }
                a[i as usize].clone()
            }
            Value::Object(m) => m.get(&stringify(k)).cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        };
    }
    Ok(cur)
}

fn slice(args: &[Value]) -> Result<Value, TmplError> {
    let v = args.first().unwrap_or(&Value::Null);
    let start = args.get(1).and_then(|x| x.as_i64()).unwrap_or(0).max(0) as usize;
    match v {
        Value::Array(a) => {
            let s = start.min(a.len());
            let end = args
                .get(2)
                .and_then(|x| x.as_i64())
                .map(|e| e as usize)
                .unwrap_or(a.len())
                .clamp(s, a.len());
            Ok(Value::Array(a[s..end].to_vec()))
        }
        Value::String(st) => {
            let chars: Vec<char> = st.chars().collect();
            let s = start.min(chars.len());
            let end = args
                .get(2)
                .and_then(|x| x.as_i64())
                .map(|e| e as usize)
                .unwrap_or(chars.len())
                .clamp(s, chars.len());
            Ok(Value::String(chars[s..end].iter().collect()))
        }
        _ => Ok(Value::Null),
    }
}

fn cmp(args: &[Value]) -> Result<std::cmp::Ordering, TmplError> {
    let a = args.first().unwrap_or(&Value::Null);
    let b = args.get(1).unwrap_or(&Value::Null);
    if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
        return Ok(x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal));
    }
    Ok(stringify(a).cmp(&stringify(b)))
}

fn values_eq(a: &Value, b: &Value) -> bool {
    if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
        return x == y;
    }
    a == b
}

fn printf(args: &[Value]) -> Result<Value, TmplError> {
    let fmt = args.first().map(stringify).unwrap_or_default();
    let mut out = String::new();
    let mut ai = 1;
    let b = fmt.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'%' {
            out.push(b[i] as char);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        if i < b.len() && b[i] == b'%' {
            out.push('%');
            i += 1;
            continue;
        }
        while i < b.len() && matches!(b[i], b'-' | b'+' | b' ' | b'0' | b'#') {
            i += 1;
        }
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let mut prec: Option<usize> = None;
        if i < b.len() && b[i] == b'.' {
            i += 1;
            let ps = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            prec = fmt[ps..i].parse().ok();
        }
        if i >= b.len() {
            break;
        }
        let verb = b[i] as char;
        i += 1;
        let spec = &fmt[start..i];
        let arg = args.get(ai).cloned().unwrap_or(Value::Null);
        ai += 1;
        out.push_str(&format_verb(verb, spec, &arg, prec));
    }
    Ok(Value::String(out))
}

fn format_verb(verb: char, spec: &str, arg: &Value, prec: Option<usize>) -> String {
    let inner = &spec[1..spec.len() - 1];
    let flags_width = inner.split_once('.').map(|(a, _)| a).unwrap_or(inner);
    let zero = flags_width.starts_with('0');
    let left = flags_width.starts_with('-');
    let width: usize = flags_width
        .trim_start_matches(['-', '+', ' ', '0', '#'])
        .parse()
        .unwrap_or(0);

    let body = match verb {
        'd' => arg
            .as_i64()
            .map(|n| n.to_string())
            .unwrap_or_else(|| stringify(arg)),
        'f' | 'F' => {
            let p = prec.unwrap_or(6);
            arg.as_f64()
                .map(|n| format!("{n:.p$}"))
                .unwrap_or_else(|| stringify(arg))
        }
        'e' => arg.as_f64().map(|n| format!("{n:e}")).unwrap_or_default(),
        'g' => arg.as_f64().map(|n| format!("{n}")).unwrap_or_default(),
        's' | 'v' => {
            let s = stringify(arg);
            match prec {
                Some(p) => s.chars().take(p).collect(),
                None => s,
            }
        }
        'q' => format!("{:?}", stringify(arg)),
        't' => truthy(arg).to_string(),
        'x' => arg
            .as_i64()
            .map(|n| format!("{n:x}"))
            .unwrap_or_else(|| hex_str(&stringify(arg))),
        'X' => arg.as_i64().map(|n| format!("{n:X}")).unwrap_or_default(),
        'o' => arg.as_i64().map(|n| format!("{n:o}")).unwrap_or_default(),
        'b' => arg.as_i64().map(|n| format!("{n:b}")).unwrap_or_default(),
        'c' => arg
            .as_i64()
            .and_then(|n| char::from_u32(n as u32))
            .map(|c| c.to_string())
            .unwrap_or_default(),
        _ => stringify(arg),
    };
    pad_to(&body, width, left, zero && !left)
}

fn hex_str(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn pad_to(s: &str, width: usize, left: bool, zero: bool) -> String {
    if s.len() >= width {
        return s.to_string();
    }
    let pad = width - s.len();
    if left {
        format!("{s}{}", " ".repeat(pad))
    } else if zero {
        format!("{}{s}", "0".repeat(pad))
    } else {
        format!("{}{s}", " ".repeat(pad))
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

/// Render a value the way Go's template `{{.}}` does. Null prints "<no value>".
fn stringify(v: &Value) -> String {
    match v {
        Value::Null => "<no value>".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(v).unwrap_or_default(),
    }
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn balanced(s: &str) -> bool {
    if !s.starts_with('(') || !s.ends_with(')') {
        return false;
    }
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i != s.len() - 1 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

fn split_first_token(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            return (&s[..end + 2], &rest[end + 1..]);
        }
    }
    match s.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b),
        None => (s, ""),
    }
}

fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '`' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '(' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' => {
                    depth -= 1;
                    cur.push(c);
                }
                c if c.is_whitespace() && depth == 0 => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn split_top(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '`' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '(' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' => {
                    depth -= 1;
                    cur.push(c);
                }
                c if c == sep && depth == 0 => out.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            },
        }
    }
    out.push(cur);
    out
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('`') && s.ends_with('`')))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn unescape(s: &str, process: bool) -> String {
    if !process {
        return s.to_string();
    }
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn r(src: &str, ctx: Value) -> String {
        render(src, &ctx).unwrap()
    }

    #[test]
    fn fields_and_text() {
        let c = json!({"Name": "web", "State": {"Running": true, "Status": "running"}});
        assert_eq!(r("{{.Name}}", c.clone()), "web");
        assert_eq!(r("name={{.Name}}!", c.clone()), "name=web!");
        assert_eq!(r("{{.State.Running}}", c.clone()), "true");
        assert_eq!(r("{{.State.Status}}", c.clone()), "running");
        assert_eq!(r("{{.Missing}}", c), "<no value>");
    }

    #[test]
    fn conditionals() {
        let c = json!({"State": {"Status": "running"}});
        assert_eq!(
            r(
                "{{if eq .State.Status \"running\"}}up{{else}}down{{end}}",
                c.clone()
            ),
            "up"
        );
        let c2 = json!({"State": {"Status": "exited"}});
        assert_eq!(
            r(
                "{{if eq .State.Status \"running\"}}up{{else}}down{{end}}",
                c2
            ),
            "down"
        );
        let c3 = json!({"n": 5});
        assert_eq!(r("{{if gt .n 3}}big{{end}}", c3.clone()), "big");
        assert_eq!(
            r(
                "{{if lt .n 3}}small{{else if eq .n 5}}five{{else}}?{{end}}",
                c3
            ),
            "five"
        );
    }

    #[test]
    fn ranges() {
        let c = json!({"items": ["a", "b", "c"]});
        assert_eq!(r("{{range .items}}{{.}},{{end}}", c.clone()), "a,b,c,");
        assert_eq!(
            r("{{range $i, $v := .items}}{{$i}}={{$v}} {{end}}", c),
            "0=a 1=b 2=c "
        );
        let empty = json!({"items": []});
        assert_eq!(r("{{range .items}}x{{else}}none{{end}}", empty), "none");
    }

    #[test]
    fn ranges_over_networks() {
        let c = json!({"NetworkSettings": {"Networks": {"bridge": {"IPAddress": "10.88.0.2"}}}});
        assert_eq!(
            r(
                "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
                c
            ),
            "10.88.0.2"
        );
    }

    #[test]
    fn pipelines_and_json() {
        let c = json!({"Config": {"Image": "alpine"}});
        assert_eq!(r("{{json .Config}}", c.clone()), "{\"Image\":\"alpine\"}");
        assert_eq!(r("{{.Config.Image | printf \"img=%s\"}}", c), "img=alpine");
    }

    #[test]
    fn printf_formats() {
        let c = json!({});
        assert_eq!(r("{{printf \"%.2f\" 3.14159}}", c.clone()), "3.14");
        assert_eq!(r("{{printf \"%q\" \"hi\"}}", c.clone()), "\"hi\"");
        assert_eq!(r("{{printf \"%d-%d\" 1 2}}", c.clone()), "1-2");
        assert_eq!(r("{{printf \"%05d\" 42}}", c.clone()), "00042");
        assert_eq!(r("{{printf \"%x\" 255}}", c), "ff");
    }

    #[test]
    fn trim_markers() {
        let c = json!({"x": "v"});
        assert_eq!(r("a  {{- .x }}  b", c.clone()), "av  b");
        assert_eq!(r("a  {{ .x -}}  b", c), "a  vb");
    }

    #[test]
    fn vars_and_with() {
        let c = json!({"a": {"b": "deep"}});
        assert_eq!(r("{{with .a}}{{.b}}{{end}}", c.clone()), "deep");
        assert_eq!(r("{{$x := .a.b}}{{$x}}", c.clone()), "deep");
        assert_eq!(r("{{with .missing}}x{{else}}y{{end}}", c), "y");
    }

    #[test]
    fn define_and_template() {
        let c = json!({"Name": "n"});
        assert_eq!(
            r("{{define \"t\"}}[{{.Name}}]{{end}}{{template \"t\" .}}", c),
            "[n]"
        );
    }

    #[test]
    fn and_or_not() {
        let c = json!({"a": true, "b": false});
        assert_eq!(r("{{if and .a (not .b)}}yes{{end}}", c.clone()), "yes");
        assert_eq!(r("{{if or .b .a}}y{{end}}", c), "y");
    }

    #[test]
    fn len_and_index() {
        let c = json!({"items": ["x", "y", "z"]});
        assert_eq!(r("{{len .items}}", c.clone()), "3");
        assert_eq!(r("{{index .items 1}}", c), "y");
    }

    #[test]
    fn root_access() {
        let c = json!({"Name": "top", "inner": {"x": 1}});
        assert_eq!(r("{{with .inner}}{{$.Name}}{{end}}", c), "top");
    }

    #[test]
    fn add_func_for_sprig() {
        let mut t = Template::parse("{{upper .name}}").unwrap();
        t.add_func("upper", |args| {
            Ok(Value::String(
                args.first()
                    .map(super::stringify)
                    .unwrap_or_default()
                    .to_uppercase(),
            ))
        });
        assert_eq!(t.render(&json!({"name": "abc"})).unwrap(), "ABC");
    }
}
