//! Google Workspace's native screens, compiled into Buwiz Bot. The manifest
//! names `gws.workspace`; the plugin serves the tools it calls.

mod workspace;

use buwiz_app::{ABI, ScreenDef};

pub fn screens() -> &'static [ScreenDef] {
    &[ScreenDef { id: "gws.workspace", abi: ABI, build: workspace::build }]
}
