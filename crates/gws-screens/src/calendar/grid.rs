//! The side column and the views: days on a time grid, the month, the list.

use buwiz_app::Bridge;
use chrono::{Datelike, Local, NaiveDate};
use nsh_core::prelude::*;
use serde_json::json;

use super::event::{Calendar, Event};
use super::time::{self, Span, When};
use super::{
    Cal, fetch, load_calendars, open_event, outcome, refresh, run, show_view, tell, today, value,
};
use crate::compose::press_send_button;
use crate::shell::{CHOSEN, DIM, INK, LINE, MUTE, RAIL};

/// One hour on the grid, in points.
const HOUR: f32 = 48.0;
const GUTTER: &str = "w-[52px]";
/// The shortest an event shows, so it can be read and pressed.
const SHORTEST: u32 = 30;
/// The hour the grid opens at.
const FIRST_HOUR: f32 = 7.0;
/// The most events a day's all-day row shows before "+n more".
const ALL_DAY_ROWS: usize = 3;
const ROW_HEIGHT: f32 = 52.0;
const CHIP_HEIGHT: f32 = 17.0;

/// Each calendar's colour, by its place in the list: our own, a light and a
/// dark of each.
const TINTS: [&str; 6] = [
    "bg-[#dce6d4] dark:bg-[#2e3c28]",
    "bg-[#f0dfcc] dark:bg-[#47341f]",
    "bg-[#e3d9ec] dark:bg-[#3a2c47]",
    "bg-[#cfe5e2] dark:bg-[#1f3f3b]",
    "bg-[#efe6bd] dark:bg-[#433b17]",
    "bg-[#dadfe6] dark:bg-[#30363f]",
];

fn tint(calendars: &[Calendar], id: &str) -> &'static str {
    let at = calendars.iter().position(|c| c.id == id).unwrap_or(0);
    TINTS[at % TINTS.len()]
}

fn describe(e: &Event) -> Option<nsh_core::AccessibilityValue> {
    value(format!(
        "{}, {}",
        e.name(),
        time::when_line(&e.when, &Local)
    ))
}

/// An event in a day: fixed label, its name in the value and the text.
fn chip(b: &Bridge, m: Cal, e: &Event, tint: &str) -> View {
    let (b, key) = (b.clone(), e.key());
    view()
        .app_class(Some(
            format!(
                "px-1 rounded-sm overflow-hidden h-[{CHIP_HEIGHT}px] {tint} {}",
                if e.faint() { "opacity-60" } else { "" }
            )
            .into(),
        ))
        .label("Open this event")
        .accessibility_value(describe(e))
        .on_press(move || open_event(&b, m, key.clone()))
        .child(
            text(e.name())
                .number_of_lines(1)
                .app_class(Some(format!("text-[11px] {INK}").into())),
        )
}

/// "+n more", which opens the day.
fn more(b: &Bridge, m: Cal, day: NaiveDate, n: usize) -> View {
    let b = b.clone();
    view()
        .app_class(Some("px-1".into()))
        .label("Show the rest of this day")
        .accessibility_value(value(day.to_string()))
        .on_press(move || show_view(&b, m, Span::Day, day))
        .child(text(format!("+{n} more")).app_class(Some(format!("text-[11px] {MUTE}").into())))
}

fn day_rows(rows: &[Event], day: NaiveDate) -> impl Iterator<Item = &Event> {
    rows.iter().filter(move |e| e.when.touches(day, &Local))
}

fn shown_days(span: Span, anchor: NaiveDate) -> Vec<NaiveDate> {
    let (first, end) = time::range(span, anchor);
    first.iter_days().take_while(|d| *d < end).collect()
}

/// A day's timed events on the grid, side by side where they overlap.
fn column(b: &Bridge, m: Cal, day: NaiveDate, rows: &[Event], calendars: &[Calendar]) -> View {
    let events: Vec<(&Event, (u32, u32))> = day_rows(rows, day)
        .filter(|e| !e.when.in_all_day_row(&Local))
        .filter_map(|e| Some((e, time::slice(&e.when, day, &Local)?)))
        .map(|(e, (from, to))| (e, (from, to.max(from + SHORTEST).min(1440))))
        .collect();
    let spans: Vec<(u32, u32)> = events.iter().map(|(_, span)| *span).collect();
    let lanes = time::lanes(&spans);
    let height = HOUR * 24.0;
    let mut col = view().app_class(Some(
        format!("relative flex-1 h-[{height}px] border-l {LINE}").into(),
    ));
    for hour in 1..24 {
        let top = hour as f32 * HOUR;
        col = col.child(view().app_class(Some(
            format!("absolute left-0 right-0 top-[{top}px] h-[1px] border-t {LINE}").into(),
        )));
    }
    for ((e, (from, to)), (lane, of)) in events.iter().zip(lanes) {
        let (b, key) = (b.clone(), e.key());
        let top = *from as f32 / 60.0 * HOUR;
        let tall = (*to - *from) as f32 / 60.0 * HOUR;
        let wide = 100.0 / of as f32;
        let left = lane as f32 * wide;
        let colour = tint(calendars, &e.calendar);
        let mut cell = view()
            .app_class(Some(
                format!(
                    "absolute top-[{top}px] h-[{tall}px] left-[{left}%] w-[{wide}%] px-1 rounded-sm overflow-hidden {colour} {}",
                    if e.faint() { "opacity-60" } else { "" }
                )
                .into(),
            ))
            .label("Open this event")
            .accessibility_value(describe(e))
            .on_press(move || open_event(&b, m, key.clone()))
            .child(
                text(e.name())
                    .number_of_lines(1)
                    .app_class(Some(format!("text-[11px] {INK}").into())),
            );
        if tall >= HOUR {
            cell = cell.child(
                text(time::start_line(&e.when, &Local))
                    .number_of_lines(1)
                    .app_class(Some(format!("text-[10px] {MUTE}").into())),
            );
        }
        col = col.child(cell);
    }
    if day == today() {
        col = col.child(dynamic(move || {
            m.now.get();
            let top = time::slice(&now_when(), day, &Local)
                .map_or(0.0, |(from, _)| from as f32 / 60.0 * HOUR);
            view().test_id("now-line").app_class(Some(
                format!(
                    "absolute left-0 right-0 top-[{top}px] h-[2px] bg-[#c2410c] dark:bg-[#fb923c]"
                )
                .into(),
            ))
        }));
    }
    col
}

fn now_when() -> When {
    let now = Local::now().fixed_offset();
    When::Timed {
        start: now,
        end: now,
    }
}

fn day_heads(days: &[NaiveDate]) -> View {
    days.iter().fold(
        view()
            .app_class(Some(format!("flex-row border-b {LINE}").into()))
            .child(view().app_class(Some(GUTTER.into()))),
        |row, day| {
            let weight = if *day == today() { "font-semibold" } else { "" };
            row.child(
                view()
                    .app_class(Some("flex-1 px-1 py-1 overflow-hidden".into()))
                    .child(
                        text(day.format("%a %-d").to_string())
                            .number_of_lines(1)
                            .app_class(Some(format!("text-[12px] {weight} {INK}").into())),
                    ),
            )
        },
    )
}

fn all_day_row(
    b: &Bridge,
    m: Cal,
    days: &[NaiveDate],
    rows: &[Event],
    calendars: &[Calendar],
) -> View {
    let start = view()
        .app_class(Some(format!("flex-row border-b {LINE}").into()))
        .child(
            view()
                .app_class(Some(format!("{GUTTER} px-1 py-1").into()))
                .child(text("All day").app_class(Some(format!("text-[10px] {DIM}").into()))),
        );
    days.iter().fold(start, |row, day| {
        let events: Vec<&Event> = day_rows(rows, *day)
            .filter(|e| e.when.in_all_day_row(&Local))
            .collect();
        let extra = events.len().saturating_sub(ALL_DAY_ROWS);
        let cell = events.iter().take(ALL_DAY_ROWS).fold(
            view().app_class(Some(
                format!("flex-1 gap-[1px] p-[1px] min-h-[20px] overflow-hidden border-l {LINE}")
                    .into(),
            )),
            |cell, e| cell.child(chip(b, m, e, tint(calendars, &e.calendar))),
        );
        row.child(if extra > 0 {
            cell.child(more(b, m, *day, extra))
        } else {
            cell
        })
    })
}

/// Day and week. The scroll stays put when rows change: a reload mustn't jump.
pub(super) fn days(b: Bridge, m: Cal) -> impl IntoElement {
    let grid = b.clone();
    let hours = (0..24).fold(
        view().app_class(Some(format!("{GUTTER} h-[{}px]", HOUR * 24.0).into())),
        |gutter, hour| {
            gutter.child(
                view()
                    .app_class(Some(format!("h-[{HOUR}px] px-1").into()))
                    .child(
                        text(format!("{hour:02}:00"))
                            .app_class(Some(format!("text-[10px] {DIM}").into())),
                    ),
            )
        },
    );
    view()
        .app_class(Some("flex-1".into()))
        .child(dynamic(move || {
            let (rows, calendars) = (m.events.get(), m.calendars.get());
            let days = shown_days(m.span.get(), m.anchor.get());
            view()
                .child(day_heads(&days))
                .child(all_day_row(&b, m, &days, &rows, &calendars))
        }))
        .child(
            scroll()
                .content_offset(point(px(0.0), px(FIRST_HOUR * HOUR)))
                .app_class(Some("flex-1".into()))
                .child(
                    view()
                        .app_class(Some("flex-row".into()))
                        .child(hours)
                        .child(dynamic(move || {
                            let (rows, calendars) = (m.events.get(), m.calendars.get());
                            let days = shown_days(m.span.get(), m.anchor.get());
                            days.iter().fold(
                                view().app_class(Some("flex-1 flex-row".into())),
                                |row, day| row.child(column(&grid, m, *day, &rows, &calendars)),
                            )
                        })),
                ),
        )
}

/// How many events a month's cell has room for, of `height` points: the rest
/// go behind "+n more", which has to fit too.
fn chips_in(height: f32) -> usize {
    let room = height - 18.0 - CHIP_HEIGHT;
    ((room / (CHIP_HEIGHT + 1.0)).floor() as usize).clamp(1, 6)
}

pub(super) fn month(b: Bridge, m: Cal) -> impl IntoElement {
    dynamic(move || {
        let (rows, calendars) = (m.events.get(), m.calendars.get());
        let anchor = m.anchor.get();
        let days = time::month_days(anchor);
        let weeks = days.len() / 7;
        // the header, the line of weekdays, and what may sit under the view
        let cell_height = (window().height.0 - 110.0) / weeks.max(1) as f32;
        let room = chips_in(cell_height);
        let names =
            ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
                .iter()
                .fold(
                    view().app_class(Some(format!("flex-row border-b {LINE}").into())),
                    |row, name| {
                        row.child(view().app_class(Some("flex-1 px-1 py-[2px]".into())).child(
                            text(*name).app_class(Some(format!("text-[11px] {MUTE}").into())),
                        ))
                    },
                );
        days.chunks(7).fold(
            view().app_class(Some("flex-1".into())).child(names),
            |grid, week| {
                let line = week.iter().fold(
                    view().app_class(Some(format!("flex-1 flex-row border-b {LINE}").into())),
                    |line, day| {
                        let events: Vec<&Event> = day_rows(&rows, *day).collect();
                        let (shown, extra) = if events.len() > room {
                            (room, events.len() - room)
                        } else {
                            (events.len(), 0)
                        };
                        let dim = if day.month() == anchor.month() {
                            INK
                        } else {
                            DIM
                        };
                        let (press, on) = (b.clone(), *day);
                        let head = view()
                            .label("Show this day")
                            .accessibility_value(value(day.to_string()))
                            .on_press(move || show_view(&press, m, Span::Day, on))
                            .child(
                                text(day.day().to_string())
                                    .app_class(Some(format!("px-1 text-[11px] {dim}").into())),
                            );
                        let cell = events.iter().take(shown).fold(
                            view()
                                .app_class(Some(
                                    format!(
                                        "flex-1 border-l overflow-hidden gap-[1px] p-[1px] {LINE}"
                                    )
                                    .into(),
                                ))
                                .child(head),
                            |cell, e| cell.child(chip(&b, m, e, tint(&calendars, &e.calendar))),
                        );
                        line.child(if extra > 0 {
                            cell.child(more(&b, m, *day, extra))
                        } else {
                            cell
                        })
                    },
                );
                grid.child(line)
            },
        )
    })
}

/// One cell of the list. Cells are recycled, so every read goes through `at`: a
/// position read once would keep showing, and opening, its first row.
fn row_view(b: Bridge, m: Cal, at: Position) -> impl IntoElement {
    let field = move |f: fn(&Event) -> String| {
        move || {
            m.events
                .with(|r| r.get(at.index()).map(f).unwrap_or_default())
        }
    };
    view()
        .app_class(Some(
            format!("flex-row gap-2 h-[{ROW_HEIGHT}px] px-3 py-1.5 border-b {LINE}").into(),
        ))
        .label("Open this event")
        .accessibility_value(move || m.events.with(|r| r.get(at.index()).and_then(describe)))
        .on_press(move || {
            if let Some(key) = m
                .events
                .with_untracked(|r| r.get(at.index()).map(Event::key))
            {
                open_event(&b, m, key);
            }
        })
        .child(view().app_class(move || {
            let colour = m.calendars.with(|c| {
                m.events
                    .with(|r| r.get(at.index()).map_or(TINTS[0], |e| tint(c, &e.calendar)))
            });
            Some(format!("w-[6px] rounded-sm {colour}").into())
        }))
        .child(
            view()
                .app_class(Some("flex-1 overflow-hidden".into()))
                .child(
                    text(field(Event::name))
                        .number_of_lines(1)
                        .app_class(Some(format!("text-[13px] {INK}").into())),
                )
                .child(
                    text(field(|e| time::when_line(&e.when, &Local)))
                        .number_of_lines(1)
                        .app_class(Some(format!("text-[11px] {MUTE}").into())),
                ),
        )
}

pub(super) fn agenda(b: Bridge, m: Cal) -> impl IntoElement {
    let empty = dynamic(move || {
        let line = match (m.loading.get(), m.searching.with(Option::is_some)) {
            (true, _) => "Loading your events…",
            (false, true) => "No events match that.",
            (false, false) if m.problem.with(Option::is_some) => "",
            (false, false) => "Nothing in these days.",
        };
        view()
            .app_class(Some("p-6 items-center".into()))
            .child(text(line).app_class(Some(format!("text-[13px] {MUTE}").into())))
    });
    virtual_list(
        move || m.events.with(Vec::len),
        move |at| row_view(b.clone(), m, at),
    )
    // rows come and go anywhere when the view changes, not just at its end
    .keys(move || {
        m.events.with(|r| {
            r.iter()
                .map(|e| format!("{}/{}", e.calendar, e.id))
                .collect::<Vec<_>>()
        })
    })
    .estimated_item_size(px(ROW_HEIGHT))
    .empty(empty)
    .app_class(Some("flex-1".into()))
}

fn small_month(b: Bridge, m: Cal) -> impl IntoElement {
    dynamic(move || {
        let anchor = m.anchor.get();
        let days = time::month_days(anchor);
        let names = ["S", "M", "T", "W", "T", "F", "S"].iter().fold(
            view().app_class(Some("flex-row".into())),
            |row, name| {
                row.child(
                    view()
                        .app_class(Some("flex-1 items-center".into()))
                        .child(text(*name).app_class(Some(format!("text-[10px] {DIM}").into()))),
                )
            },
        );
        days.chunks(7).fold(
            view()
                .app_class(Some("px-3 py-2 gap-[2px]".into()))
                .child(
                    text(anchor.format("%b %Y").to_string())
                        .app_class(Some(format!("text-[12px] font-semibold {INK}").into())),
                )
                .child(names),
            |grid, week| {
                grid.child(week.iter().fold(
                    view().app_class(Some("flex-row".into())),
                    |row, day| {
                        let (b, on) = (b.clone(), *day);
                        let chosen = if *day == anchor { CHOSEN } else { "" };
                        let ink = if day.month() == anchor.month() {
                            INK
                        } else {
                            DIM
                        };
                        row.child(
                            view()
                                .app_class(Some(
                                    format!("flex-1 items-center rounded-sm {chosen}").into(),
                                ))
                                .label("Go to this day")
                                .accessibility_value(value(day.to_string()))
                                .on_press(move || show_view(&b, m, m.span.get_untracked(), on))
                                .child(
                                    text(day.day().to_string())
                                        .app_class(Some(format!("text-[11px] {ink}").into())),
                                ),
                        )
                    },
                ))
            },
        )
    })
}

/// Show or hide a calendar. The store keeps the choice, so the press only shows
/// once it's kept.
fn toggle(b: &Bridge, m: Cal, id: String, shown: bool) {
    let call = b.call("gws_cal_show", json!({"calendar": id, "shown": shown}));
    let b = b.clone();
    run(m, async move {
        match outcome(call.await) {
            Ok(_) => {
                fetch(m, load_calendars(b.clone(), m));
                refresh(b, m).await;
            }
            Err((_, why)) => tell(m, why),
        }
    });
}

fn calendar_rows(b: Bridge, m: Cal) -> impl IntoElement {
    dynamic(move || {
        let calendars = m.calendars.get();
        calendars.iter().fold(
            view().app_class(Some("px-3 py-2 gap-1".into())).child(
                text("Calendars")
                    .app_class(Some(format!("text-[12px] font-semibold {INK}").into())),
            ),
            |list, c| {
                let (b, id, shown) = (b.clone(), c.id.clone(), c.shown);
                let colour = tint(&calendars, &c.id);
                let swatch = if shown { colour } else { "" };
                list.child(
                    view()
                        .app_class(Some("flex-row items-center gap-2 py-[2px]".into()))
                        .label("Show or hide this calendar")
                        .accessibility_value(value(format!(
                            "{}, {}",
                            c.name,
                            if shown { "shown" } else { "hidden" }
                        )))
                        .on_press(move || toggle(&b, m, id.clone(), !shown))
                        .child(view().app_class(Some(
                            format!("w-[12px] h-[12px] rounded-sm border {LINE} {swatch}").into(),
                        )))
                        .child(
                            text(c.name.clone())
                                .number_of_lines(1)
                                .app_class(Some(format!("flex-1 text-[12px] {INK}").into())),
                        ),
                )
            },
        )
    })
}

/// New, the small month and the calendars.
pub(super) fn side(b: Bridge, m: Cal) -> impl IntoElement {
    let days = b.clone();
    let rows = b;
    scroll()
        .app_class(Some(
            format!("w-[256px] h-full pt-2 border-r {LINE} {RAIL}").into(),
        ))
        .child(
            view()
                .app_class(Some("flex-row px-3 py-1".into()))
                .child(press_send_button("New event", "New", move || {
                    super::editor::new_event(m)
                })),
        )
        .child(small_month(days, m))
        .child(calendar_rows(rows, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_cell_still_has_room_for_one_and_the_more_line() {
        assert_eq!(chips_in(40.0), 1);
        assert_eq!(chips_in(90.0), 3);
    }
}
