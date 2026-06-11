//! kubectl-slim — a standalone kubectl-compatible client that talks the real
//! Kubernetes REST API to the slim apiserver-lite (served by slimd over a unix
//! socket). CRUD/CRDs/scale/logs/exec all go through the apiserver, which
//! reconciles workloads into engine containers.
//!
//! Nebula's `nebula kubectl` wrapper execs this when the engine is slim.
//! HOST binary. Supported verbs: apply/delete/get/logs/scale/exec/describe.

use slim_client::http::Client;
use slim_kube::Facade;
use std::io::Read;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run(&argv));
}

fn run(argv: &[String]) -> i32 {
    // global flags: -n/--namespace, -f/--filename (collected per-verb too)
    let mut namespace = String::from("default");
    let mut rest = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-n" | "--namespace" => {
                namespace = argv.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            s if s.starts_with("--namespace=") => {
                namespace = s["--namespace=".len()..].to_string();
                i += 1;
            }
            "--help" | "-h" if rest.is_empty() => {
                usage();
                return 0;
            }
            "version" if rest.is_empty() => {
                println!("kubectl-slim: Client slim-0.1.0 (nebula-slim k8s facade)");
                return 0;
            }
            _ => {
                rest.push(argv[i].clone());
                i += 1;
            }
        }
    }
    if rest.is_empty() {
        usage();
        return 0;
    }
    let client = Client::discover_kube();
    let facade = Facade::new(&client, &namespace);
    let verb = rest[0].as_str();
    let args = &rest[1..];
    let mut out = |s: &str| print!("{s}");

    let result = match verb {
        "apply" => verb_apply(&facade, args, &mut out),
        "delete" => verb_delete(&facade, args, &mut out),
        "get" => verb_get(&facade, args),
        "scale" => verb_scale(&facade, args, &mut out),
        "logs" => verb_logs(&facade, args),
        "exec" => verb_exec(&facade, args),
        "describe" => verb_describe(&facade, args),
        other => {
            eprintln!("error: unknown command \"{other}\" for kubectl-slim");
            return 1;
        }
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn read_filename(args: &[String]) -> Result<Option<String>, String> {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-f" | "--filename" => return Ok(Some(args.get(i + 1).cloned().ok_or("-f needs a path")?)),
            s if s.starts_with("--filename=") => return Ok(Some(s["--filename=".len()..].to_string())),
            s if s.starts_with("-f=") => return Ok(Some(s["-f=".len()..].to_string())),
            _ => i += 1,
        }
    }
    Ok(None)
}

fn slurp(path: &str) -> Result<String, String> {
    if path == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
        Ok(s)
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
    }
}

fn verb_apply(facade: &Facade, args: &[String], out: &mut dyn FnMut(&str)) -> Result<i32, String> {
    let path = read_filename(args)?.ok_or("apply requires -f")?;
    let strict = args.iter().any(|a| a == "--strict");
    let yaml = slurp(&path)?;
    let skipped = facade.apply_yaml(&yaml, out).map_err(|e| e.to_string())?;
    if !skipped.is_empty() && strict {
        return Err(format!(
            "--strict: {} object(s) of unsupported kinds were skipped: {}",
            skipped.len(),
            skipped.join(", ")
        ));
    }
    Ok(0)
}

fn verb_delete(facade: &Facade, args: &[String], out: &mut dyn FnMut(&str)) -> Result<i32, String> {
    if let Some(path) = read_filename(args)? {
        let yaml = slurp(&path)?;
        facade.delete_yaml(&yaml, out).map_err(|e| e.to_string())?;
        return Ok(0);
    }
    // delete <kind> <name...>
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if positional.len() < 2 {
        return Err("delete requires -f FILE or KIND NAME".into());
    }
    let kind = positional[0];
    for name in &positional[1..] {
        facade.delete(kind, name, out).map_err(|e| e.to_string())?;
    }
    Ok(0)
}

fn verb_get(facade: &Facade, args: &[String]) -> Result<i32, String> {
    let output = flag_value(args, &["-o", "--output"]);
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let kind = positional.first().map(|s| s.as_str()).unwrap_or("all");
    let name = positional.get(1).map(|s| s.as_str());
    let objs = facade.get(kind, name).map_err(|e| e.to_string())?;
    if objs.is_empty() {
        if let Some(n) = name {
            return Err(format!("{} \"{n}\" not found", kind));
        }
        println!("No resources found in {} namespace.", facade.namespace);
        return Ok(0);
    }
    match output.as_deref() {
        Some("json") => {
            println!("{}", serde_json::to_string_pretty(&objs.iter().map(|o| {
                serde_json::json!({"kind": o.kind, "name": o.name, "ready": o.ready, "status": o.status})
            }).collect::<Vec<_>>()).unwrap_or_default());
        }
        Some("name") => {
            for o in &objs {
                println!("{}/{}", o.kind.to_lowercase(), o.name);
            }
        }
        Some("wide") | None => {
            // Table.
            println!("{:<28} {:<8} {:<10} {:<10}", "NAME", "READY", "STATUS", "RESTARTS");
            for o in &objs {
                println!("{:<28} {:<8} {:<10} {:<10}", o.name, o.ready, o.status, o.restarts);
            }
        }
        Some(other) => return Err(format!("unsupported output format: {other}")),
    }
    Ok(0)
}

fn verb_scale(facade: &Facade, args: &[String], out: &mut dyn FnMut(&str)) -> Result<i32, String> {
    let replicas: i64 = flag_value(args, &["--replicas"])
        .ok_or("scale requires --replicas")?
        .parse()
        .map_err(|_| "invalid --replicas")?;
    // accept "deployment/web" or "deployment web" (kind defaults to deployment)
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let target = positional.last().ok_or("scale requires a target")?;
    let (kind, name) = match target.split_once('/') {
        Some((k, n)) => (k, n),
        None => ("deployments", target.as_str()),
    };
    facade.scale(kind, name, replicas, out).map_err(|e| e.to_string())?;
    Ok(0)
}

fn verb_logs(facade: &Facade, args: &[String]) -> Result<i32, String> {
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let pod = positional.first().ok_or("logs requires a pod name")?;
    let follow = args.iter().any(|a| a == "-f" || a == "--follow");
    let mut out = std::io::stdout();
    facade.logs(pod, follow, &mut out).map_err(|e| e.to_string())?;
    Ok(0)
}

fn verb_exec(facade: &Facade, args: &[String]) -> Result<i32, String> {
    // kubectl exec [-it] POD [-c container] -- cmd...
    let sep = args.iter().position(|a| a == "--");
    let (head, cmd): (&[String], Vec<String>) = match sep {
        Some(i) => (&args[..i], args[i + 1..].to_vec()),
        None => (args, vec![]),
    };
    let positional: Vec<&String> = head.iter().filter(|a| !a.starts_with('-')).collect();
    let pod = positional.first().ok_or("exec requires a pod name")?;
    if cmd.is_empty() {
        return Err("exec requires a command after --".into());
    }
    let interactive = head.iter().any(|a| a == "-i" || a == "-it" || a == "-ti" || a == "--stdin");
    let tty = head.iter().any(|a| a == "-t" || a == "-it" || a == "-ti" || a == "--tty");
    facade.exec(pod, &cmd, interactive, tty).map_err(|e| e.to_string())
}

fn verb_describe(facade: &Facade, args: &[String]) -> Result<i32, String> {
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let kind = positional.first().map(|s| s.as_str()).unwrap_or("all");
    let name = positional.get(1).map(|s| s.as_str());
    let objs = facade.get(kind, name).map_err(|e| e.to_string())?;
    for o in &objs {
        println!("Name:     {}", o.name);
        println!("Kind:     {}", o.kind);
        println!("Status:   {}", o.status);
        println!("Ready:    {}", o.ready);
        println!("Pods:     {}", o.members.join(", "));
        println!();
    }
    Ok(0)
}

fn flag_value(args: &[String], names: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        for n in names {
            if args[i] == *n {
                return args.get(i + 1).cloned();
            }
            let pfx = format!("{n}=");
            if let Some(v) = args[i].strip_prefix(&pfx) {
                return Some(v.to_string());
            }
        }
        i += 1;
    }
    None
}

fn usage() {
    print!(
        "kubectl-slim — Kubernetes facade over the nebula slim engine\n\n\
        Usage: kubectl-slim [-n NS] VERB ...\n\n\
        Verbs:\n\
        \x20 apply -f FILE [--strict]  Create/update Deployments, Pods, Jobs, Services, ConfigMaps, Secrets\n\
        \x20                          (--strict: non-zero exit if any unsupported kind is skipped)\n\
        \x20 delete -f FILE        Delete the objects in FILE\n\
        \x20 delete KIND NAME      Delete by kind/name\n\
        \x20 get KIND [NAME]       List objects (pods, deployments, services, jobs)\n\
        \x20 scale --replicas=N deployment/NAME\n\
        \x20 logs [-f] POD         Stream a pod's logs\n\
        \x20 exec [-it] POD -- CMD Run a command in a pod\n\
        \x20 describe KIND [NAME]  Lite describe\n\n\
        Talks the real k8s REST API to the slim apiserver-lite: CRDs, custom\n\
        resources, operators (watch), and any registered kind work. RBAC is not\n\
        enforced (the VM is the boundary).\n"
    );
}
