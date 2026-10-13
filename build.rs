//! Stamps the build with its commit and time, so `lux upgrade` can tell
//! one build from the next.

use std::process::Command;

fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim();
    (out.status.success() && !text.is_empty()).then(|| text.to_string())
}

fn main() {
    let commit = output("git", &["describe", "--always", "--dirty"])
        .unwrap_or_else(|| "unknown commit".into());
    let built = output("date", &["-u", "+%Y-%m-%d %H:%M:%S UTC"])
        .unwrap_or_else(|| "at an unknown time".into());
    println!("cargo:rustc-env=LUX_BUILD={commit}, built {built}");
}
