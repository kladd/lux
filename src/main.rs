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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let strs: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = match strs.as_slice() {
        // A bare invocation or a bare `new` verb creates an auto-named
        // session and attaches.
        [] | ["new"] | ["new-session"] => client::attach(Request::New),
        // Named attach-or-create. `-s` and `-t` are kept as separate
        // spellings for muscle memory but behave identically, with or
        // without their verb.
        ["-s", name] | ["new", "-s", name] | ["new-session", "-s", name] => {
            client::attach(Request::Session((*name).into()))
        }
        ["-t", name] | ["attach", "-t", name] | ["attach-session", "-t", name] => {
            client::attach(Request::Session((*name).into()))
        }
        // A bare `attach` verb goes back to the most recently attached
        // session, or starts fresh if nothing has been attached to.
        ["attach"] | ["attach-session"] => client::attach(Request::Recent),
        ["ls"] => client::ls(),
        ["kill-server"] => client::kill_server(),
        ["__server"] => server::run(),
        _ => {
            eprintln!(
                "usage: lux [[new|new-session] [-s <name>] | [attach|attach-session] [-t <name>] | ls | kill-server]"
            );
            2
        }
    };
    std::process::exit(code);
}
