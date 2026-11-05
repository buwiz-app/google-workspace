//! `buwiz-gws [mcp]`: serve MCP on stdio (what Buwiz Bot's broker spawns).
//! Every other subcommand is the SDK's: `manifest`, `package`, `--version`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args == ["mcp"] {
        return match gws::Gws::from_env().and_then(|plugin| gws::serve::serve(plugin).map_err(|e| format!("stdio: {e}"))) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("buwiz-gws: {e}");
                ExitCode::FAILURE
            }
        };
    }
    buwiz_plugin::cli::main::<gws::Gws>(gws::Gws::from_env)
}
