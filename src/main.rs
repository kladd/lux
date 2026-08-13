//! Lux — terminal multiplexer. Phase 8: sessions & server/client split.
//!
//! This binary is both the client and the server, kept in separate modules
//! and communicating only through the attach protocol in `protocol`: a
//! bare `lux` spawns the server on demand and
//! attaches; `__server` is the hidden server entry point.

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
];

/// Resolve a CLI verb to a full command name: exact name or alias match
/// first, then an unambiguous prefix of a full name.
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
        "usage: lux [[new|new-session] [-s <name>] | [a|attach|attach-session] [-t <name>] | ls|list-sessions | kill-session -t <name> | kill-server]"
    );
    2
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let strs: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = match strs.as_slice() {
        // A bare invocation creates an auto-named session and attaches.
        [] => client::attach(Request::New),
        // Bare `-s`/`-t` are kept as separate spellings for muscle memory
        // but behave identically: attach-or-create by name.
        ["-s", name] | ["-t", name] => client::attach(Request::Session((*name).into())),
        ["__server"] => server::run(),
        [verb, rest @ ..] => match (resolve(verb), rest) {
            (Some("new-session"), []) => client::attach(Request::New),
            (Some("new-session"), ["-s", name]) => client::attach(Request::Session((*name).into())),
            // A bare attach goes back to the most recently attached
            // session, or starts fresh if nothing has been attached to.
            (Some("attach-session"), []) => client::attach(Request::Recent),
            (Some("attach-session"), ["-t", name]) => {
                client::attach(Request::Session((*name).into()))
            }
            (Some("list-sessions"), []) => client::ls(),
            (Some("kill-session"), ["-t", name]) => client::kill_session(name),
            (Some("kill-server"), []) => client::kill_server(),
            _ => usage(),
        },
    };
    std::process::exit(code);
}
