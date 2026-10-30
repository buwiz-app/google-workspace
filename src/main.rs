//! `rox-gws [mcp]`: the first-party Google Workspace plugin, id
//! dev.goldcoders.google-workspace, short `gws`, as a stdio MCP server
//! (what Rox's broker spawns). `rox-gws manifest`: print the unsigned
//! manifest, for the signing step (`rox-broker sign-manifest`).
//!
//! v1 routines read mail only through `reads`, from read-only tools of an
//! approved plugin (spec 5.9, Q1); this plugin is that source. It holds no
//! Google credentials of its own: it proxies to the user's own Google
//! Workspace MCP server (workspace-mcp), started from `GWS_UPSTREAM` with
//! the account from `GWS_USER_EMAIL` (both from the install record's env,
//! see `config`). The upstream answers in text ("Message ID: …" lines,
//! "--- BODY ---" sections); gws_search_mail and gws_read_mail turn that
//! into structured items. A missing or failing upstream is a plain isError
//! result, never a crash.
//!
//! It answers byte for byte as `server/src/v1/plugins/google-workspace/
//! server.mjs`, which it replaces; `server/test/v1-gws.test.mjs` runs
//! against either (`GWS_CMD`).

use std::io::Write as _;
use std::process::ExitCode;

mod config;
mod gmail;
mod js;
mod reserved;
mod rpc;
mod tools;
mod upstream;

const USAGE: &str = "usage: rox-gws [mcp]\n       rox-gws manifest";
const MANIFEST: &str = include_str!("../manifest.json");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["mcp"] => {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("rox-gws: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let served = rt.block_on(rpc::serve());
            // drops the tasks (the upstream with them: kill on drop) without
            // waiting on the thread still blocked reading an open stdin
            rt.shutdown_timeout(std::time::Duration::from_millis(500));
            match served {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("rox-gws: stdio: {e}");
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
