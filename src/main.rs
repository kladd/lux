//! Lux, a terminal multiplexer whose one binary runs as both client and
//! server.

mod client;
mod protocol;
mod server;

use protocol::Request;

const COMMANDS: &[(&str, Option<&str>)] = &[
    ("attach-session", Some("attach")),
    ("kill-server", None),
    ("kill-session", None),
    ("list-sessions", Some("ls")),
    ("new-session", Some("new")),
    ("upgrade", None),
];

/// Match a verb by exact name or alias first, then by unique prefix.
fn resolve(verb: &str) -> Option<&'static str> {
    if let Some((name, _)) = COMMANDS
        .iter()
        .find(|(name, alias)| *name == verb || *alias == Some(verb))
    {
        return Some(name);
    }
    let mut matches = COMMANDS.iter().filter(|(name, _)| name.starts_with(verb));
    match (matches.next(), matches.next()) {
        (Some((name, _)), None) => Some(name),
        _ => None,
    }
}

fn usage() -> i32 {
    eprintln!(
        "usage: lux [[new|new-session] [-s <name>] | [a|attach|attach-session] [-t <name>] | ls|list-sessions | kill-session -t <name> | kill-server | upgrade]"
    );
    2
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let strs: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = match strs.as_slice() {
        [] => client::attach(Request::New),
        ["-s", name] | ["-t", name] => client::attach(Request::Session((*name).into())),
        ["__server"] => server::run(None),
        ["__server", "--resume", state, reply] => match (state.parse(), reply.parse()) {
            (Ok(state), Ok(reply)) => server::run(Some((state, reply))),
            _ => usage(),
        },
        ["__upgrade-check", format] => server::upgrade::check_format(format),
        ["proxy"] => client::proxy(),
        [verb, rest @ ..] => match (resolve(verb), rest) {
            (Some("new-session"), []) => client::attach(Request::New),
            (Some("new-session"), ["-s", name]) => client::attach(Request::Session((*name).into())),
            (Some("attach-session"), []) => client::attach(Request::Recent),
            (Some("attach-session"), ["-t", name]) => {
                client::attach(Request::Session((*name).into()))
            }
            (Some("list-sessions"), []) => client::ls(),
            (Some("kill-session"), ["-t", name]) => client::kill_session(name),
            (Some("kill-server"), []) => client::kill_server(),
            (Some("upgrade"), []) => client::upgrade(),
            _ => usage(),
        },
    };
    std::process::exit(code);
}
