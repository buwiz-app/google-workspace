//! `buwiz-gws [mcp]`: the first-party Google Workspace plugin, id
//! app.buwiz.google-workspace, short `gws`, as a stdio MCP server
//! (what Buwiz Bot's broker spawns). `buwiz-gws manifest`: print the
//! unsigned manifest, for the signing step (`rox-broker sign-manifest`).
//!
//! v1 routines read mail only through `reads`, from read-only tools of an
//! approved plugin (spec 5.9, Q1); this plugin is that source. It holds no
//! Google credentials of its own: Buwiz Bot signs the user in to Google
//! (the `google` account its plugin.json declares, mini app protocol 1.0
//! §4) and pushes the access token with `buwiz.account.set`; the mail tools
//! call Gmail's REST API with it. Without a token they answer
//! `account_required`, and the Bot asks the user to sign in.

use std::io::Write as _;
use std::process::ExitCode;

mod accounts;
mod config;
#[cfg(test)]
mod fake_gmail;
mod gmail;
mod js;
mod reserved;
mod rpc;
mod tools;
mod v1;

const USAGE: &str = "usage: buwiz-gws [mcp]\n       buwiz-gws manifest";
const MANIFEST: &str = include_str!("../manifest.json");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["mcp"] => {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("buwiz-gws: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let served = rt.block_on(rpc::serve());
            // drops the tasks without waiting on the thread still blocked
            // reading an open stdin
            rt.shutdown_timeout(std::time::Duration::from_millis(500));
            match served {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("buwiz-gws: stdio: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        ["manifest"] => {
            let mut out = std::io::stdout().lock();
            match out.write_all(MANIFEST.as_bytes()).and_then(|()| out.flush()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::FAILURE,
            }
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
