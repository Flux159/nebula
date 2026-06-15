//! Dockerfile parser: line continuations, comments, parser directives,
//! per-instruction arg parsing (shell vs exec form), multi-stage names.

#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    From {
        image: String,
        stage: Option<String>,
        platform: Option<String>,
    },
    Run(ShellOrExec),
    Cmd(ShellOrExec),
    Entrypoint(ShellOrExec),
    Copy {
        from: Option<String>,
        chown: Option<String>,
        sources: Vec<String>,
        dest: String,
    },
    Add {
        chown: Option<String>,
        sources: Vec<String>,
        dest: String,
    },
    Env(Vec<(String, String)>),
    Arg {
        name: String,
        default: Option<String>,
    },
    Label(Vec<(String, String)>),
    Expose(Vec<String>),
    Workdir(String),
    User(String),
    Volume(Vec<String>),
    StopSignal(String),
    Shell(Vec<String>),
    Healthcheck(String), // stored raw; enforcement optional
    /// Recognized-but-skipped (onbuild, maintainer): kept for warnings.
    Unsupported {
        verb: String,
        rest: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ShellOrExec {
    /// JSON array form: exec'd directly.
    Exec(Vec<String>),
    /// String form: run via the configured SHELL (default ["/bin/sh","-c"]).
    Shell(String),
}

#[derive(Debug)]
pub struct ParseError(pub String);
impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Dockerfile parse error: {}", self.0)
    }
}
impl std::error::Error for ParseError {}

pub struct Dockerfile {
    pub instructions: Vec<Instruction>,
    pub escape: char,
}

/// Logical lines: strip comments, honor the escape char for continuations.
fn logical_lines(src: &str) -> (char, Vec<String>) {
    let mut escape = '\\';
    let mut lines = Vec::new();
    let mut iter = src.lines().peekable();

    // Parser directives (only before the first instruction/comment-blank).
    while let Some(line) = iter.peek() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('#') {
            let rest = rest.trim();
            if let Some((k, v)) = rest.split_once('=') {
                if k.trim().eq_ignore_ascii_case("escape") {
                    if let Some(c) = v.trim().chars().next() {
                        escape = c;
                    }
                    iter.next();
                    continue;
                }
                if k.trim().eq_ignore_ascii_case("syntax") {
                    iter.next();
                    continue; // ignored: we are not buildkit
                }
            }
            // a normal comment ends the directive scan
            break;
        } else {
            break;
        }
    }

    let mut current = String::new();
    for raw in iter {
        let line = raw;
        // Full-line comments (after directives) are dropped, but only when
        // not in a continuation.
        if current.is_empty() && line.trim_start().starts_with('#') {
            continue;
        }
        let trimmed_end = line.trim_end();
        let continues = trimmed_end.ends_with(escape);
        if continues {
            current.push_str(trimmed_end.trim_end_matches(escape));
            current.push(' ');
        } else {
            current.push_str(line);
            let done = std::mem::take(&mut current);
            if !done.trim().is_empty() {
                lines.push(done.trim().to_string());
            }
        }
    }
    if !current.trim().is_empty() {
        lines.push(current.trim().to_string());
    }
    (escape, lines)
}

pub fn parse(src: &str) -> Result<Dockerfile, ParseError> {
    let (escape, lines) = logical_lines(src);
    let mut instructions = Vec::new();
    for line in &lines {
        let (verb, rest) = match line.split_once(char::is_whitespace) {
            Some((v, r)) => (v.to_string(), r.trim().to_string()),
            None => (line.clone(), String::new()),
        };
        let v = verb.to_ascii_uppercase();
        let inst = match v.as_str() {
            "FROM" => parse_from(&rest)?,
            "RUN" => Instruction::Run(parse_shell_or_exec(&rest)),
            "CMD" => Instruction::Cmd(parse_shell_or_exec(&rest)),
            "ENTRYPOINT" => Instruction::Entrypoint(parse_shell_or_exec(&rest)),
            "COPY" => parse_copy(&rest, false)?,
            "ADD" => parse_copy(&rest, true)?,
            "ENV" => Instruction::Env(parse_kv(&rest)?),
            "ARG" => parse_arg(&rest),
            "LABEL" => Instruction::Label(parse_kv(&rest)?),
            "EXPOSE" => Instruction::Expose(rest.split_whitespace().map(String::from).collect()),
            "WORKDIR" => Instruction::Workdir(rest),
            "USER" => Instruction::User(rest),
            "VOLUME" => Instruction::Volume(parse_string_list(&rest)),
            "STOPSIGNAL" => Instruction::StopSignal(rest),
            "SHELL" => Instruction::Shell(match parse_shell_or_exec(&rest) {
                ShellOrExec::Exec(v) => v,
                ShellOrExec::Shell(s) => vec![s],
            }),
            "HEALTHCHECK" => Instruction::Healthcheck(rest),
            other => Instruction::Unsupported {
                verb: other.to_string(),
                rest,
            },
        };
        instructions.push(inst);
    }
    if !instructions
        .iter()
        .any(|i| matches!(i, Instruction::From { .. }))
    {
        return Err(ParseError("no FROM instruction".into()));
    }
    Ok(Dockerfile {
        instructions,
        escape,
    })
}

fn parse_from(rest: &str) -> Result<Instruction, ParseError> {
    let mut platform = None;
    let mut toks: Vec<&str> = rest.split_whitespace().collect();
    toks.retain(|t| {
        if let Some(p) = t.strip_prefix("--platform=") {
            platform = Some(p.to_string());
            false
        } else {
            true
        }
    });
    if toks.is_empty() {
        return Err(ParseError("FROM requires an image".into()));
    }
    let image = toks[0].to_string();
    let stage = if toks.len() >= 3 && toks[1].eq_ignore_ascii_case("as") {
        Some(toks[2].to_string())
    } else {
        None
    };
    Ok(Instruction::From {
        image,
        stage,
        platform,
    })
}

fn parse_copy(rest: &str, is_add: bool) -> Result<Instruction, ParseError> {
    let mut from = None;
    let mut chown = None;
    let mut toks: Vec<String> = Vec::new();
    // Flags then the (JSON or space) list. Support --from=, --chown=, --chmod=.
    for tok in split_respecting_quotes(rest) {
        if let Some(v) = tok.strip_prefix("--from=") {
            from = Some(v.to_string());
        } else if let Some(v) = tok.strip_prefix("--chown=") {
            chown = Some(v.to_string());
        } else if tok.starts_with("--chmod=") || tok.starts_with("--link") {
            // accepted, ignored
        } else {
            toks.push(tok);
        }
    }
    // JSON array form?
    let paths: Vec<String> = if rest.contains('[') {
        let arr = &rest[rest.find('[').unwrap()..];
        parse_string_list(arr)
    } else {
        toks
    };
    if paths.len() < 2 {
        return Err(ParseError(format!(
            "{} requires at least one source and a destination",
            if is_add { "ADD" } else { "COPY" }
        )));
    }
    let dest = paths.last().unwrap().clone();
    let sources = paths[..paths.len() - 1].to_vec();
    Ok(if is_add {
        Instruction::Add {
            chown,
            sources,
            dest,
        }
    } else {
        Instruction::Copy {
            from,
            chown,
            sources,
            dest,
        }
    })
}

fn parse_arg(rest: &str) -> Instruction {
    match rest.split_once('=') {
        Some((n, v)) => Instruction::Arg {
            name: n.trim().to_string(),
            default: Some(unquote(v.trim())),
        },
        None => Instruction::Arg {
            name: rest.trim().to_string(),
            default: None,
        },
    }
}

/// ENV/LABEL: either `K V` (single, rest-of-line value) or `K=V K2=V2 ...`.
fn parse_kv(rest: &str) -> Result<Vec<(String, String)>, ParseError> {
    if !rest.contains('=') {
        // `ENV KEY value with spaces`
        let (k, v) = rest.split_once(char::is_whitespace).ok_or_else(|| {
            ParseError(format!("expected `KEY value` or `KEY=value`, got: {rest}"))
        })?;
        return Ok(vec![(k.to_string(), v.trim().to_string())]);
    }
    let mut out = Vec::new();
    for tok in split_respecting_quotes(rest) {
        if let Some((k, v)) = tok.split_once('=') {
            out.push((k.to_string(), unquote(v)));
        }
    }
    if out.is_empty() {
        return Err(ParseError(format!("could not parse key/value: {rest}")));
    }
    Ok(out)
}

fn parse_shell_or_exec(rest: &str) -> ShellOrExec {
    let t = rest.trim();
    if t.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Vec<String>>(t) {
            return ShellOrExec::Exec(v);
        }
    }
    ShellOrExec::Shell(t.to_string())
}

fn parse_string_list(rest: &str) -> Vec<String> {
    let t = rest.trim();
    if t.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Vec<String>>(t) {
            return v;
        }
    }
    t.split_whitespace().map(String::from).collect()
}

fn split_respecting_quotes(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let df = parse(
            "FROM alpine:3.19 AS base\n\
             RUN echo hello && \\\n    echo world\n\
             ENV A=1 B=\"two words\"\n\
             COPY --from=base /a /b ./dest/\n\
             CMD [\"sh\", \"-c\", \"echo hi\"]\n",
        )
        .unwrap();
        assert!(matches!(&df.instructions[0],
            Instruction::From { image, stage: Some(s), .. } if image == "alpine:3.19" && s == "base"));
        match &df.instructions[1] {
            Instruction::Run(ShellOrExec::Shell(s)) => {
                assert!(s.contains("echo hello") && s.contains("echo world"))
            }
            other => panic!("{other:?}"),
        }
        match &df.instructions[2] {
            Instruction::Env(kv) => {
                assert_eq!(kv[0], ("A".into(), "1".into()));
                assert_eq!(kv[1], ("B".into(), "two words".into()));
            }
            other => panic!("{other:?}"),
        }
        match &df.instructions[3] {
            Instruction::Copy {
                from: Some(f),
                sources,
                dest,
                ..
            } => {
                assert_eq!(f, "base");
                assert_eq!(sources, &["/a", "/b"]);
                assert_eq!(dest, "./dest/");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            df.instructions[4],
            Instruction::Cmd(ShellOrExec::Exec(vec![
                "sh".into(),
                "-c".into(),
                "echo hi".into()
            ]))
        );
    }

    #[test]
    fn comments_and_directives() {
        let df = parse("# escape=`\n# a comment\nFROM x\nRUN echo a `\n  echo b\n").unwrap();
        assert_eq!(df.escape, '`');
        match &df.instructions[1] {
            Instruction::Run(ShellOrExec::Shell(s)) => {
                assert!(s.contains("echo a") && s.contains("echo b"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn no_from_errors() {
        assert!(parse("RUN echo hi\n").is_err());
    }
}
