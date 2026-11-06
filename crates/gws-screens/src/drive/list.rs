//! The rows: a list of columns or a grid of cards, both virtual, so a folder of
//! thousands holds only what's near the screen. Cells are recycled, so every
//! read goes through the cell's position.

use buwiz_app::Bridge;
use nsh_core::AccessibilityValue;
use nsh_core::prelude::*;

use super::file::Row;
use super::{Drive, Layout, details_at, next_page, open_at};
use crate::shell::{CHOSEN, DIM, INK, LINE, MUTE};

const ROW_HEIGHT: f32 = 48.0;
const CARD_HEIGHT: f32 = 136.0;
/// The narrowest a card gets before a column goes.
const CARD_WIDTH: f32 = 172.0;
/// How wide the list must be for the columns to fit beside a readable name.
const COLUMNS_FROM: f32 = 540.0;
// room for a long value at 12 pt: "Sep 30, 2024", "1023.9 MB"
const OWNER: &str = "w-[140px]";
const CHANGED: &str = "w-[92px]";
const SIZE: &str = "w-[76px]";

/// How many cards fit across.
pub(super) fn cards_across(width: f32) -> usize {
    ((width / CARD_WIDTH) as usize).max(1)
}

/// What an empty list says, with the next press when this folder takes files.
fn empty_line(view: &str, folder: bool, searching: bool, adds: bool) -> String {
    let line = if searching {
        return "No files match that. Try other words.".to_owned();
    } else if folder {
        "This folder is empty."
    } else {
        match view {
            "shared" => return "Nothing is shared with you yet.".to_owned(),
            "recent" => return "Nothing opened or changed lately.".to_owned(),
            "starred" => return "Nothing starred yet.".to_owned(),
            "trash" => return "Trash is empty.".to_owned(),
            _ => "My Drive is empty.",
        }
    };
    if adds {
        format!("{line} Press New folder or Upload to add to it.")
    } else {
        line.to_owned()
    }
}

fn empty(m: Drive) -> impl IntoElement {
    dynamic(move || {
        let line = if m.loading.get() {
            "Loading your files…".to_owned()
        } else if m.problem.with(Option::is_some) {
            String::new()
        } else {
            empty_line(
                m.view.get(),
                m.folder.with(Option::is_some),
                m.search.with(Option::is_some),
                m.here_adds.get(),
            )
        };
        view()
            .app_class(Some("p-6 items-center".into()))
            .child(text(line).app_class(Some(format!("text-[13px] {MUTE}").into())))
    })
}

fn edited(m: Drive, at: Position) -> bool {
    m.edits.with(|e| {
        m.rows.with(|r| {
            r.get(at.index())
                .is_some_and(|r| e.iter().any(|e| e.id == r.id))
        })
    })
}

fn spoken(m: Drive, at: Position) -> Option<AccessibilityValue> {
    let mark = if edited(m, at) { ", edited here" } else { "" };
    m.rows.with(|r| {
        r.get(at.index()).map(|r| AccessibilityValue {
            text: Some(format!("{}{mark}", r.spoken()).into()),
            ..AccessibilityValue::default()
        })
    })
}

/// "edited" beside a file with changes here, and nothing beside the rest.
fn edited_mark(m: Drive, at: Position) -> Text {
    text(move || if edited(m, at) { "edited" } else { "" })
        .number_of_lines(1)
        .app_class(Some(format!("text-[11px] {MUTE}").into()))
}

/// The way to a file's details, now a press on it opens it. Beside the row's
/// own press and not inside it, so each is plainly one thing to a screen reader.
fn details_press(b: Bridge, m: Drive, at: Position, place: &'static str) -> View {
    view()
        .app_class(Some(place.into()))
        .label("Show details")
        .accessibility_value(move || spoken(m, at))
        .on_press(move || details_at(&b, m, at.index()))
        .child(
            text("Details")
                .number_of_lines(1)
                .app_class(Some(format!("text-[11px] {MUTE}").into())),
        )
}

fn picked(m: Drive, at: Position) -> bool {
    m.picked.with(|p| {
        m.rows
            .with(|r| r.get(at.index()).is_some_and(|r| p.contains(&r.id)))
    })
}

fn state(m: Drive, at: Position) -> AccessibilityState {
    AccessibilityState {
        selected: picked(m, at),
        ..AccessibilityState::default()
    }
}

fn shade(m: Drive, at: Position) -> &'static str {
    if picked(m, at) { CHOSEN } else { "" }
}

fn column(m: Drive, width: &'static str, end: bool, body: impl Fn() -> String + 'static) -> View {
    view()
        .app_class(move || {
            let shown = if wide(m) { width } else { "hidden" };
            // not stretched: a text takes what it needs, which tests can read
            let side = if end { "items-end" } else { "items-start" };
            Some(format!("{shown} {side}").into())
        })
        .child(
            text(body)
                .number_of_lines(1)
                .app_class(Some(format!("text-[12px] {MUTE}").into())),
        )
}

fn wide(m: Drive) -> bool {
    m.width.get() >= COLUMNS_FROM
}

/// What goes under a name. Without the columns it carries what they would.
fn under_name(r: &Row, wide: bool) -> String {
    if wide {
        r.tag().to_owned()
    } else {
        format!(
            "{}, {}, {}, {}",
            r.tag(),
            r.owner_line(),
            r.changed_line(),
            r.size_line()
        )
    }
}

fn row_view(b: Bridge, m: Drive, at: Position) -> impl IntoElement {
    let field = move |f: fn(&Row) -> String| {
        move || {
            m.rows
                .with(|r| r.get(at.index()).map(f).unwrap_or_default())
        }
    };
    let details = b.clone();
    view()
        .app_class(move || {
            Some(
                format!(
                    "h-[{ROW_HEIGHT}px] flex-row items-center border-b {LINE} {}",
                    shade(m, at)
                )
                .into(),
            )
        })
        .child(
            view()
                .app_class(Some(
                    "flex-1 min-w-0 flex-row items-center gap-3 pl-3".into(),
                ))
                .label("Open this item")
                .accessibility_value(move || spoken(m, at))
                .accessibility_state(move || state(m, at))
                .on_press(move || open_at(&b, m, at.index()))
                .child(
                    view()
                        .app_class(Some("flex-1 min-w-0".into()))
                        .child(
                            view()
                                .app_class(Some("flex-row items-center gap-1".into()))
                                .child(
                                    text(field(|r| r.name.clone()))
                                        .number_of_lines(1)
                                        .app_class(Some(
                                            format!("flex-1 min-w-0 text-[13px] {INK}").into(),
                                        )),
                                )
                                .child(edited_mark(m, at)),
                        )
                        .child(
                            text(move || {
                                let wide = wide(m);
                                m.rows.with(|r| {
                                    r.get(at.index())
                                        .map(|r| under_name(r, wide))
                                        .unwrap_or_default()
                                })
                            })
                            .number_of_lines(1)
                            .app_class(Some(format!("text-[11px] {DIM}").into())),
                        ),
                )
                .child(column(m, OWNER, false, field(Row::owner_line)))
                .child(column(m, CHANGED, false, field(Row::changed_line)))
                .child(column(m, SIZE, true, field(Row::size_line))),
        )
        .child(details_press(details, m, at, "px-3 py-3"))
}

fn heading(m: Drive) -> impl IntoElement {
    let name = |line: &'static str| {
        text(line)
            .number_of_lines(1)
            .app_class(Some(format!("text-[11px] {MUTE}").into()))
    };
    let over = move |line: &'static str, width: &'static str, end: bool| {
        view()
            .app_class(move || {
                Some(format!("{width} {}", if end { "items-end" } else { "" }).into())
            })
            .child(name(line))
    };
    view()
        .app_class(move || {
            let shown = if wide(m) { "flex-row" } else { "hidden" };
            Some(format!("{shown} items-center gap-3 px-3 py-1 border-b {LINE}").into())
        })
        .child(
            view()
                .app_class(Some("flex-1 min-w-0".into()))
                .child(name("Name")),
        )
        .child(over("Owner", OWNER, false))
        .child(over("Changed", CHANGED, false))
        .child(over("Size", SIZE, true))
}

fn card_view(b: Bridge, m: Drive, at: Position) -> impl IntoElement {
    let field = move |f: fn(&Row) -> String| {
        move || {
            m.rows
                .with(|r| r.get(at.index()).map(f).unwrap_or_default())
        }
    };
    let details = b.clone();
    view()
        .app_class(Some(format!("h-[{CARD_HEIGHT}px] p-1").into()))
        .child(
            view()
                .app_class(move || {
                    Some(
                        format!(
                            "flex-1 gap-1 p-2 rounded-md border overflow-hidden {LINE} {}",
                            shade(m, at)
                        )
                        .into(),
                    )
                })
                .label("Open this item")
                .accessibility_value(move || spoken(m, at))
                .accessibility_state(move || state(m, at))
                .on_press(move || open_at(&b, m, at.index()))
                .child(
                    view()
                        .app_class(Some("flex-row items-center gap-1".into()))
                        .child(
                            text(field(|r| r.tag().to_owned()))
                                .number_of_lines(1)
                                .app_class(Some(
                                    format!("text-[10px] font-semibold {MUTE}").into(),
                                )),
                        )
                        .child(edited_mark(m, at)),
                )
                .child(
                    text(field(|r| r.name.clone()))
                        .number_of_lines(2)
                        .app_class(Some(format!("text-[13px] {INK}").into())),
                )
                .child(
                    text(field(Row::owner_line))
                        .number_of_lines(1)
                        .app_class(Some(format!("text-[11px] {MUTE}").into())),
                )
                .child(
                    text(field(Row::changed_line))
                        .number_of_lines(1)
                        .app_class(Some(format!("text-[11px] {DIM}").into())),
                ),
        )
        .child(details_press(details, m, at, "px-2 py-[2px] items-end"))
}

fn ids(m: Drive) -> Vec<String> {
    m.rows
        .with(|r| r.iter().map(|r| r.id.clone()).collect::<Vec<_>>())
}

fn watched(viewable: &ViewableItems, m: Drive) {
    if let Some(range) = viewable.viewable {
        m.seen.set(range);
    }
}

fn rows_list(b: Bridge, m: Drive) -> impl IntoElement {
    let more = b.clone();
    virtual_list(
        move || m.rows.with(Vec::len),
        move |at| row_view(b.clone(), m, at),
    )
    .element_ref(&m.list)
    // rows come and go anywhere when the list changes, not just at its end
    .keys(move || ids(m))
    .estimated_item_size(px(ROW_HEIGHT))
    // back where the person was: a switch between List and Grid builds this anew
    .initial_scroll_index(m.seen.get_untracked().0)
    .on_viewable_items_change(move |v: &ViewableItems| watched(v, m))
    .empty(empty(m))
    .on_load_more(move || next_page(&more, m))
    .app_class(Some("flex-1".into()))
}

fn cards_list(b: Bridge, m: Drive) -> impl IntoElement {
    let more = b.clone();
    virtual_list(
        move || m.rows.with(Vec::len),
        move |at| card_view(b.clone(), m, at),
    )
    .element_ref(&m.list)
    .keys(move || ids(m))
    .columns(move || cards_across(m.width.get()) as u32)
    .estimated_item_size(px(CARD_HEIGHT))
    .initial_scroll_index(m.seen.get_untracked().0)
    .on_viewable_items_change(move |v: &ViewableItems| watched(v, m))
    .empty(empty(m))
    .on_load_more(move || next_page(&more, m))
    .app_class(Some("flex-1 px-1".into()))
}

/// The rows as the person last chose, built anew only when that changes.
pub(super) fn body(b: Bridge, m: Drive) -> impl IntoElement {
    let layout = computed(move || m.layout.get());
    dynamic(move || {
        let holder = view().app_class(Some("flex-1".into()));
        // a key each, or the one list is kept with its offset in points
        match layout.get() {
            Layout::List => holder
                .key("list")
                .child(heading(m))
                .child(rows_list(b.clone(), m)),
            Layout::Grid => holder.key("grid").child(cards_list(b.clone(), m)),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_list_says_which_list_and_the_press_only_where_it_is_offered() {
        assert_eq!(
            empty_line("my", false, false, true),
            "My Drive is empty. Press New folder or Upload to add to it."
        );
        assert_eq!(empty_line("my", false, false, false), "My Drive is empty.");
        assert_eq!(
            empty_line("my", true, false, true),
            "This folder is empty. Press New folder or Upload to add to it."
        );
        assert_eq!(
            empty_line("shared", true, false, false),
            "This folder is empty."
        );
        assert_eq!(empty_line("trash", false, false, false), "Trash is empty.");
        assert_eq!(
            empty_line("recent", false, false, true),
            "Nothing opened or changed lately."
        );
        assert_eq!(
            empty_line("starred", false, true, true),
            "No files match that. Try other words."
        );
    }
}
