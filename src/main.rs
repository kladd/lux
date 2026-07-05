//! Lux — terminal multiplexer. Phase 8: sessions & server/client split.
//!
//! This binary is both the client and the server (REQ-SESSION-001 keeps
//! them in separate modules, communicating only through the attach
//! protocol in `protocol`): a bare `lux` spawns the server on demand and
//! attaches; `__server` is the hidden server entry point.

mod client;
mod protocol;
mod server;

use protocol::Request;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let strs: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = match strs.as_slice() {
        // Create-and-attach a fresh session (REQ-SESSION-006).
        [] => client::attach(Request::New(None)),
        // REQ-SESSION-019: create named, failing on collision. The
        // `new-session` verb form matches tmux (REQ-SESSION-033).
        ["-s", name] | ["new-session", "-s", name] => {
            client::attach(Request::New(Some((*name).into())))
        }
        // REQ-SESSION-026: attach to an existing session. The `attach`
        // verb form matches tmux (REQ-SESSION-033).
        ["-t", name] | ["attach", "-t", name] => {
            client::attach(Request::Attach((*name).into()))
        }
        // REQ-SESSION-020/021.
        ["ls"] => client::ls(),
        ["kill-server"] => client::kill_server(),
        ["__server"] => server::run(),
        _ => {
            eprintln!(
                "usage: lux [[new-session] -s <name> | [attach] -t <name> | ls | kill-server]"
            );
            2
        }
    };
    std::process::exit(code);
}
