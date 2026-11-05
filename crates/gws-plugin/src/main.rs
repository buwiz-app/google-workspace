//! `buwiz-gws [mcp]`: serve MCP on stdio (what Buwiz Bot's broker spawns).
//! Every other subcommand is the SDK's: `manifest`, `package`, `--version`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args == ["mcp"] {
        return match gws::serve::serve(gws::Gws::from_env()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("buwiz-gws: stdio: {e}");
                ExitCode::FAILURE
            }
        };
    }
    buwiz_plugin::cli::main::<gws::Gws>(|| Ok(gws::Gws::from_env()))
}
