//! slim-client: the docker-CLI-compatible command surface, used by the
//! standalone `docker-slim` binary. Speaks the Engine API to slimd over the
//! same unix socket the real docker CLI uses.

pub mod http;
pub mod tty;

mod args;
pub mod cmds;
mod format;

use http::Client;

/// Entry point used by the `docker-slim` binary. `argv` excludes the program
/// name. Returns the process exit code.
pub fn run(argv: &[String]) -> i32 {
    // Global flags (-H/--host, -v/--version) before the subcommand.
    let mut i = 0;
    let mut host: Option<String> = None;
    while i < argv.len() {
        match argv[i].as_str() {
            "-H" | "--host" => {
                host = argv.get(i + 1).cloned();
                i += 2;
            }
            h if h.starts_with("--host=") => {
                host = Some(h["--host=".len()..].to_string());
                i += 1;
            }
            "-v" | "--version" => {
                println!("Docker version slim-0.1.0 (nebula-slim), build slim");
                return 0;
            }
            "--help" | "-h" if i == 0 => {
                print_usage();
                return 0;
            }
            _ => break,
        }
    }
    let rest = &argv[i..];
    if rest.is_empty() {
        print_usage();
        return 0;
    }
    if let Some(h) = host {
        std::env::set_var("DOCKER_HOST", h);
    }
    let client = Client::discover();
    let cmd = rest[0].as_str();
    let cargs = &rest[1..];

    let result = match cmd {
        "version" => cmds::version(&client),
        "info" => cmds::info(&client),
        "pull" => cmds::pull(&client, cargs),
        "push" => cmds::push(&client, cargs),
        "images" => cmds::images(&client, cargs),
        "run" => cmds::run(&client, cargs),
        "create" => cmds::create(&client, cargs),
        "start" => cmds::start(&client, cargs),
        "stop" => cmds::stop(&client, cargs),
        "restart" => cmds::restart(&client, cargs),
        "kill" => cmds::kill(&client, cargs),
        "rm" => cmds::rm(&client, cargs),
        "ps" => cmds::ps(&client, cargs),
        "logs" => cmds::logs(&client, cargs),
        "exec" => cmds::exec(&client, cargs),
        "inspect" => cmds::inspect(&client, cargs),
        "cp" => cmds::cp(&client, cargs),
        "build" => cmds::build(&client, cargs),
        "tag" => cmds::tag(&client, cargs),
        "load" => cmds::load(&client, cargs),
        "save" => cmds::save(&client, cargs),
        "rmi" => cmds::rmi(&client, cargs),
        "wait" => cmds::wait(&client, cargs),
        "port" => cmds::port(&client, cargs),
        "stats" => cmds::stats(&client, cargs),
        "events" => cmds::events(&client, cargs),
        "login" => cmds::login(&client, cargs),
        "logout" => cmds::logout(cargs),
        "image" => cmds::image_sub(&client, cargs),
        "container" => cmds::container_sub(&client, cargs),
        "volume" => cmds::volume(&client, cargs),
        "network" => cmds::network(&client, cargs),
        "system" => cmds::system(&client, cargs),
        other => {
            eprintln!("docker-slim: '{other}' is not a slim command.\nRun 'docker-slim --help'.");
            Err(cmds::CmdError::Handled(125))
        }
    };
    match result {
        Ok(()) => 0,
        Err(cmds::CmdError::Handled(code)) => code,
        Err(cmds::CmdError::Msg(m)) => {
            eprintln!("Error: {m}");
            1
        }
    }
}

fn print_usage() {
    print!(
        "Usage: docker-slim COMMAND\n\n\
        A slim, docker-compatible CLI for the nebula slim engine.\n\n\
        Common Commands:\n\
        \x20 run       Create and run a new container from an image\n\
        \x20 exec      Execute a command in a running container\n\
        \x20 ps        List containers\n\
        \x20 build     Build an image from a Dockerfile\n\
        \x20 pull      Download an image from a registry\n\
        \x20 images    List images\n\
        \x20 load      Load an image from a docker-save archive\n\
        \x20 logs      Fetch the logs of a container\n\
        \x20 inspect   Return low-level information on objects\n\n\
        Management Commands:\n\
        \x20 container, image, volume, network, system\n\n\
        Run 'docker-slim COMMAND --help' for more information on a command.\n"
    );
}
