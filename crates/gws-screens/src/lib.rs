//! Google Workspace's native screens, compiled into Buwiz Bot: Mail, Calendar
//! and Drive, each a full screen of its own. The manifest names them; the
//! plugin serves the tools they call.

pub mod calendar;
mod compose;
mod drive;
mod mail;
pub mod reader;
mod shell;

use buwiz_app::{ABI, Bridge, ScreenDef};
use nsh_core::element::Root;

pub fn screens() -> &'static [ScreenDef] {
    &[
        ScreenDef {
            id: "gws.mail",
            abi: ABI,
            build: mail,
        },
        ScreenDef {
            id: "gws.calendar",
            abi: ABI,
            build: calendar,
        },
        ScreenDef {
            id: "gws.drive",
            abi: ABI,
            build: drive,
        },
    ]
}

fn mail(bridge: Bridge) -> Root {
    mail::build(bridge)
}

fn calendar(bridge: Bridge) -> Root {
    calendar::build(bridge)
}

fn drive(bridge: Bridge) -> Root {
    drive::build(bridge)
}
