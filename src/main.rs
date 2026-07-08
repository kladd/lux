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
        // Create-and-attach a fresh session.
        [] => client::attach(Request::New(None)),
        // Create named, failing on collision. tmux's verb
        // name and alias are both accepted for compatibility with its CLI.
        ["-s", name] | ["new", "-s", name] | ["new-session", "-s", name] => {
            client::attach(Request::New(Some((*name).into())))
        }
        // Attach to an existing session. Both verb forms
        // accepted for the same reason.
        ["-t", name] | ["attach", "-t", name] | ["attach-session", "-t", name] => {
            client::attach(Request::Attach((*name).into()))
        }
        ["ls"] => client::ls(),
        ["kill-server"] => client::kill_server(),
        ["__server"] => server::run(),
        _ => {
            eprintln!(
                "usage: lux [[new|new-session] -s <name> | [attach|attach-session] -t <name> | ls | kill-server]"
            );
            2
        }
    };
    std::process::exit(code);
}
