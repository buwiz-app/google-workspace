//! A file's details, beside the list or over it. The press to close stays
//! outside the scroll, so a long list of people can't take it away.

use nsh_core::prelude::*;

use super::file::{Facts, Preview};
use super::{Drive, FactsState, close_details};
use crate::compose::press_text;
use crate::shell::{DIM, INK, LINE, MUTE, PANEL};

/// One thing about the file: its name for it above, so a narrow pane has no
/// fixed column to run out of.
fn line(title: &'static str, body: String) -> View {
    view()
        .app_class(Some("px-3 py-1".into()))
        .child(text(title).app_class(Some(format!("text-[11px] {MUTE}").into())))
        .child(text(body).app_class(Some(format!("text-[13px] {INK}").into())))
}

fn people(facts: &Facts) -> View {
    if facts.people.is_empty() {
        return view();
    }
    facts.people.iter().fold(
        view()
            .app_class(Some("px-3 py-1 gap-1".into()))
            .child(text("Who has access").app_class(Some(format!("text-[11px] {MUTE}").into()))),
        |list, p| {
            list.child(
                view()
                    .app_class(Some("flex-row flex-wrap gap-x-2".into()))
                    .child(
                        text(p.who.clone()).app_class(Some(
                            format!("min-w-0 max-w-full text-[12px] {INK}").into(),
                        )),
                    )
                    .child(
                        text(p.role.clone()).app_class(Some(format!("text-[11px] {DIM}").into())),
                    ),
            )
        },
    )
}

fn facts_view(m: Drive) -> impl IntoElement {
    dynamic(move || match m.facts.get() {
        FactsState::Reading => view()
            .app_class(Some("px-3 py-1".into()))
            .child(text("Reading…").app_class(Some(format!("text-[12px] {MUTE}").into()))),
        FactsState::Missing(why) => view()
            .app_class(Some("px-3 py-1".into()))
            .child(text(why).app_class(Some(format!("text-[12px] {MUTE}").into()))),
        FactsState::Known(facts) => {
            let list = view();
            let list = if facts.path.is_empty() {
                list
            } else {
                list.child(line("Path", facts.path.clone()))
            };
            let list = list.child(people(&facts));
            if facts.description.is_empty() {
                list
            } else {
                list.child(line("Description", facts.description.clone()))
            }
        }
    })
}

fn preview_view(m: Drive) -> impl IntoElement {
    let framed = |inner: View| {
        view()
            .app_class(Some(format!("mx-3 my-2 pt-2 border-t {LINE}").into()))
            .child(inner)
    };
    dynamic(move || match m.preview.get() {
        Preview::Nothing => view(),
        Preview::Reading => framed(
            view().child(text("Reading…").app_class(Some(format!("text-[12px] {MUTE}").into()))),
        ),
        Preview::Reason(why) => {
            framed(view().child(text(why).app_class(Some(format!("text-[12px] {MUTE}").into()))))
        }
        // a stranger's words: plain text, selectable, and never a page
        Preview::Text(body) => framed(
            view()
                .selectable(true)
                .child(text(body).app_class(Some(format!("text-[12px] {INK}").into()))),
        ),
        Preview::Image(url) => framed(
            view().child(
                image(url)
                    .label("Preview of this file")
                    .app_class(Some("w-full h-[220px] object-contain".into())),
            ),
        ),
    })
}

pub(super) fn pane(m: Drive) -> impl IntoElement {
    dynamic(move || {
        let Some(row) = m.open.get() else {
            return view();
        };
        view()
            .app_class(Some(format!("flex-1 {PANEL}").into()))
            .child(
                view()
                    .app_class(Some(
                        format!("flex-row flex-wrap items-center gap-1 px-1 py-1 border-b {LINE}")
                            .into(),
                    ))
                    .child(text(row.name.clone()).number_of_lines(2).app_class(Some(
                        format!("flex-1 min-w-[80px] px-2 text-[14px] font-semibold {INK}").into(),
                    )))
                    .child(press_text("Close these details", "Close", move || {
                        close_details(m);
                    })),
            )
            .child(
                scroll().app_class(Some("flex-1".into())).child(
                    view()
                        .app_class(Some("py-1".into()))
                        .child(line("Kind", row.tag().to_owned()))
                        .child(line("Owner", row.owner_line()))
                        .child(line("Changed", row.changed_line()))
                        .child(line("Size", row.size_line()))
                        .child(facts_view(m))
                        .child(preview_view(m)),
                ),
            )
    })
}
