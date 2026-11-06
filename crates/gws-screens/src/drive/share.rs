//! Share: who will have a file once the press goes, every one of them with their
//! role, before it can be pressed. The list is the plugin's whole read of
//! Google's, with the people being added marked and those being taken off set
//! apart, so what's on screen is what the press leaves. The press names what it
//! does. It stays outside the scroll, so a long list can't take it away.
//!
//! The press carries the mark of the list it was read from, and the plugin
//! refuses a list that moved since: someone not shown here is someone the person
//! never agreed to. Link sharing isn't here. A link already on a file is shown,
//! as it's access like any other, and can't be changed from this screen.

use std::collections::HashSet;

use buwiz_app::Bridge;
use nsh_core::element::input;
use nsh_core::prelude::*;
use serde_json::{Value, json};

use super::edits::{failed_line, press, press_as, say};
use super::file::{Access, Row, access_in, role_line};
use super::write::{Acting, landed};
use super::{Drive, ask, fetch, refused, value};
use crate::shell::{BG, CHOSEN, DIM, INK, LINE, Latest, MUTE, PANEL};

/// What a person can be given, and the press for it.
const ROLES: [&str; 3] = ["reader", "commenter", "writer"];

#[derive(Clone, PartialEq)]
enum Read {
    Reading,
    Ready,
    Failed(String),
}

/// What's in the panel now.
#[derive(Clone, Copy)]
pub(super) struct Sharing {
    read: Signal<Read>,
    people: Signal<Vec<Access>>,
    /// The mark of the list on screen.
    seen: Signal<String>,
    /// New people: an address and a role.
    adding: Signal<Vec<(String, &'static str)>>,
    /// Who the press takes off, by their permission.
    leaving: Signal<HashSet<String>>,
    typed: Signal<String>,
    role: Signal<&'static str>,
    /// What's wrong with the address typed.
    problem: Signal<Option<&'static str>>,
    turn: Latest,
}

impl Sharing {
    pub(super) fn new() -> Self {
        Self {
            read: signal(Read::Reading),
            people: signal(Vec::new()),
            seen: signal(String::new()),
            adding: signal(Vec::new()),
            leaving: signal(HashSet::new()),
            typed: signal(String::new()),
            role: signal("reader"),
            problem: signal(None),
            turn: Latest::new(),
        }
    }
}

async fn read_people(b: Bridge, m: Drive, id: String) {
    let s = m.sharing;
    let turn = s.turn.ask();
    s.read.set(Read::Reading);
    let answer = ask(&b, "gws_drive_access", json!({"id": id})).await;
    if !s.turn.counts(turn) {
        return;
    }
    match answer {
        Ok(v) => {
            s.people.set(access_in(&v));
            s.seen.set(
                v.get("seen")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            );
            // who's leaving can only be who's here
            let left = s.people.with_untracked(|p| {
                s.leaving.with_untracked(|l| {
                    l.iter()
                        .filter(|id| p.iter().any(|p| p.id == **id))
                        .cloned()
                        .collect()
                })
            });
            s.leaving.set(left);
            s.read.set(Read::Ready);
        }
        Err(why) => s.read.set(Read::Failed(why)),
    }
}

/// Opens the panel on `row`, with nobody added and nobody leaving.
pub(super) fn open(b: &Bridge, m: Drive, row: Row) {
    let s = m.sharing;
    m.said.set(None);
    s.people.set(Vec::new());
    s.adding.set(Vec::new());
    s.leaving.set(HashSet::new());
    s.typed.set(String::new());
    s.role.set("reader");
    s.problem.set(None);
    s.read.set(Read::Reading);
    fetch(m, read_people(b.clone(), m, row.id.clone()));
    m.acting.set(Some(Acting::Share(row)));
}

/// What the press does, in as few words as say it.
fn press_words(adding: usize, leaving: usize) -> String {
    let people = |n: usize| {
        if n == 1 {
            "1 person".to_owned()
        } else {
            format!("{n} people")
        }
    };
    match (adding, leaving) {
        (0, n) => format!("Remove {}", people(n)),
        (n, 0) => format!("Share with {}", people(n)),
        (n, gone) => format!("Share with {} and remove {gone}", people(n)),
    }
}

/// An address well enough formed to be worth showing. The plugin checks again.
fn looks_like_address(address: &str) -> bool {
    address.split_once('@').is_some_and(|(who, host)| {
        !who.is_empty()
            && host.contains('.')
            && !host.ends_with('.')
            && !address.contains(char::is_whitespace)
    })
}

fn add(m: Drive) {
    let s = m.sharing;
    let typed = s.typed.get_untracked();
    let address = typed.trim().to_owned();
    if !looks_like_address(&address) {
        s.problem
            .set(Some("That doesn't look like an email address."));
        return;
    }
    let held = s
        .people
        .with_untracked(|p| p.iter().any(|p| p.email.eq_ignore_ascii_case(&address)))
        || s.adding
            .with_untracked(|a| a.iter().any(|(a, _)| a.eq_ignore_ascii_case(&address)));
    if held {
        s.problem.set(Some("That person has this file already."));
        return;
    }
    let role = s.role.get_untracked();
    s.adding.update(|a| a.push((address, role)));
    s.typed.set(String::new());
    s.problem.set(None);
}

/// The press. Everything it sends is what's on screen at this moment.
fn share(b: &Bridge, m: Drive, id: &str) {
    let s = m.sharing;
    if m.writing.get_untracked() || s.read.get_untracked() != Read::Ready {
        return;
    }
    // an address left in the field isn't on the list, so it can't go by
    // surprise, and it mustn't be dropped without a word
    if !s.typed.get_untracked().trim().is_empty() {
        s.problem.set(Some("Add that address, or clear it, first."));
        return;
    }
    let adding = s.adding.get_untracked();
    let remove: Vec<String> = s.leaving.get_untracked().into_iter().collect();
    let add: Vec<Value> = adding
        .iter()
        .map(|(email, role)| json!({"email": email, "role": role}))
        .collect();
    let args = json!({"id": id, "add": add, "remove": remove, "seen": s.seen.get_untracked()});
    let call = b.call("gws_drive_share", args);
    let (b, id) = (b.clone(), id.to_owned());
    m.writing.set(true);
    m.said.set(None);
    fetch(m, async move {
        let answer = call.await;
        m.writing.set(false);
        match answer {
            Ok(v) => shared(&b, m, &id, &v),
            Err(e) => say(m, &failed_line(&e)),
        }
    });
}

/// What became of the press. A list that moved, or a press that went only some
/// of the way, comes back to be looked at again.
fn shared(b: &Bridge, m: Drive, id: &str, v: &Value) {
    let s = m.sharing;
    if let Some(why) = refused(v) {
        say(m, &why);
        if v.get("refused").and_then(Value::as_str) == Some("changed") {
            fetch(m, read_people(b.clone(), m, id.to_owned()));
        }
        return;
    }
    if v.get("partial") == Some(&Value::Bool(true)) {
        say(m, "Only part of that went through. This is who has it now.");
        s.adding.set(Vec::new());
        s.leaving.set(HashSet::new());
        fetch(m, read_people(b.clone(), m, id.to_owned()));
        landed(b, m, v);
        return;
    }
    let count = |key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0) as usize;
    m.acting.set(None);
    say(m, &done_words(count("added"), count("removed")));
    landed(b, m, v);
}

fn done_words(added: usize, removed: usize) -> String {
    let people = |n: usize| {
        if n == 1 {
            "1 person".to_owned()
        } else {
            format!("{n} people")
        }
    };
    match (added, removed) {
        (0, n) => format!("Removed {}.", people(n)),
        (n, 0) => format!("Shared with {}.", people(n)),
        (n, gone) => format!("Shared with {} and removed {gone}.", people(n)),
    }
}

/// One person, as the screen reader hears them too. The label is fixed, and the
/// press to take them off sits beside it, not inside it.
fn person(who: String, role: String, mark: &'static str, off: Option<View>) -> View {
    let spoken = if mark.is_empty() {
        format!("{who}, {role}")
    } else {
        format!("{who}, {role}, {mark}")
    };
    let line = view()
        .app_class(Some(
            "flex-row flex-wrap items-center gap-x-2 flex-1 min-w-[60px]".into(),
        ))
        .label("Someone with access")
        .accessibility_value(value(spoken))
        .child(text(who).app_class(Some(format!("min-w-0 max-w-full text-[12px] {INK}").into())))
        .child(text(role).app_class(Some(format!("text-[11px] {DIM}").into())));
    let line = if mark.is_empty() {
        line
    } else {
        line.child(text(mark).app_class(Some(format!("text-[11px] font-semibold {INK}").into())))
    };
    let row = view()
        .app_class(Some(
            format!("flex-row flex-wrap items-center gap-1 px-3 py-1 border-b {LINE}").into(),
        ))
        .child(line);
    match off {
        Some(press) => row.child(press),
        None => row,
    }
}

/// Everyone the press leaves with the file, new ones marked, then who it takes
/// off. Nobody who'd lose the file is on the first list, and nobody is left out.
fn people(m: Drive) -> impl IntoElement {
    dynamic(move || {
        let s = m.sharing;
        let leaving = s.leaving.get();
        let (gone, staying): (Vec<Access>, Vec<Access>) = s
            .people
            .get()
            .into_iter()
            .partition(|p| leaving.contains(&p.id));
        let list = view().child(view().app_class(Some("px-3 pt-2 pb-1".into())).child(
            text("Who will have access").app_class(Some(format!("text-[11px] {MUTE}").into())),
        ));
        let list = staying.into_iter().fold(list, |list, p| {
            let off = p.removable.then(|| {
                let id = p.id.clone();
                press_as(
                    "Take this person off",
                    "Remove".to_owned(),
                    &p.who,
                    move || {
                        s.leaving.update(|l| {
                            l.insert(id.clone());
                        });
                    },
                )
            });
            list.child(person(p.who, p.role, "", off))
        });
        let list = s.adding.get().into_iter().fold(list, |list, (who, role)| {
            let address = who.clone();
            let off = press_as(
                "Don't add this person",
                "Remove".to_owned(),
                &who,
                move || {
                    s.adding.update(|a| a.retain(|(a, _)| *a != address));
                },
            );
            list.child(person(who, role_line(role), "New", Some(off)))
        });
        if gone.is_empty() {
            return list;
        }
        let list =
            list.child(view().app_class(Some("px-3 pt-2 pb-1".into())).child(
                text("Will lose access").app_class(Some(format!("text-[11px] {MUTE}").into())),
            ));
        gone.into_iter().fold(list, |list, p| {
            let id = p.id.clone();
            let keep = press_as("Keep this person", "Keep".to_owned(), &p.who, move || {
                s.leaving.update(|l| {
                    l.remove(&id);
                });
            });
            list.child(person(p.who, p.role, "Leaving", Some(keep)))
        })
    })
}

/// Fewer words than its label: a role the AI chose would go at the person's Add.
fn role_press(m: Drive, role: &'static str, label: &'static str, shows: &'static str) -> View {
    let s = m.sharing;
    view()
        .app_class(move || {
            let on = s.role.get() == role;
            Some(
                format!(
                    "max-w-full px-2 py-1 rounded-md border {LINE} {}",
                    if on { CHOSEN } else { "" }
                )
                .into(),
            )
        })
        .label(label)
        .on_press(move || s.role.set(role))
        .child(text(shows).app_class(Some(format!("text-[12px] {INK}").into())))
}

/// The address and the role of someone new. A folder's people view or edit: what
/// Google offers there is from memory, so this leaves commenting to files.
fn adder(m: Drive, folder: bool) -> View {
    let s = m.sharing;
    let roles = ROLES.iter().filter(|r| !folder || **r != "commenter").fold(
        view().app_class(Some("flex-row flex-wrap items-center gap-1".into())),
        |row, role| {
            let (label, shows) = match *role {
                "reader" => ("Can view", "View"),
                "commenter" => ("Can comment", "Comment"),
                _ => ("Can edit", "Edit"),
            };
            row.child(role_press(m, role, label, shows))
        },
    );
    view()
        .app_class(Some("gap-1 px-3 py-2".into()))
        .child(
            input()
                // the @ keeps the AI out: what it staged here would go at the person's press
                .label("Email address, like name@company.com")
                .value(move || s.typed.get())
                .placeholder("Add an email address".to_owned())
                .auto_focus(true)
                .app_class(Some(
                    format!("min-w-0 max-w-full rounded-lg border px-2.5 py-1 text-[13px] {BG} {LINE} {INK}").into(),
                ))
                .on_change(move |value: &str| {
                    s.typed.set(value.to_owned());
                    s.problem.set(None);
                })
                .on_submit(move |_: &str| add(m)),
        )
        .child(roles)
        .child(press_as("Add this person", "Add".to_owned(), "", move || add(m)))
        .child(dynamic(move || match s.problem.get() {
            Some(why) => view().child(text(why).app_class(Some(format!("text-[11px] {INK}").into()))),
            None => view(),
        }))
}

fn footer(b: Bridge, m: Drive, row: Row) -> View {
    let s = m.sharing;
    let (id, name) = (row.id.clone(), row.name.clone());
    view()
        .app_class(Some(
            format!("flex-row flex-wrap items-center gap-1 px-2 py-1 border-t {LINE}").into(),
        ))
        .child(dynamic(move || match s.read.get() {
            Read::Reading => view().child(
                text("Reading who has this…").app_class(Some(format!("text-[11px] {MUTE}").into())),
            ),
            // nobody presses Share on a list they weren't shown
            Read::Failed(why) => {
                let (b, id) = (b.clone(), id.clone());
                view()
                    .app_class(Some("flex-row flex-wrap items-center gap-1".into()))
                    .child(
                        text(why).app_class(Some(format!("max-w-full text-[11px] {INK}").into())),
                    )
                    .child(press("Read who has this again", "", move || {
                        fetch(m, read_people(b.clone(), m, id.clone()));
                    }))
            }
            Read::Ready => {
                let (adding, leaving) = (s.adding.with(Vec::len), s.leaving.with(HashSet::len));
                if adding + leaving == 0 {
                    return view().child(
                        text("Add someone to share this with.")
                            .app_class(Some(format!("text-[11px] {MUTE}").into())),
                    );
                }
                let (b, id) = (b.clone(), id.clone());
                // as wide as the bar at most, or the longest words run off it
                view().app_class(Some("max-w-full".into())).child(press_as(
                    "Share with these people",
                    press_words(adding, leaving),
                    &name,
                    move || share(&b, m, &id),
                ))
            }
        }))
        .child(press_as(
            "Close sharing",
            "Cancel".to_owned(),
            "",
            move || {
                m.acting.set(None);
                m.said.set(None);
            },
        ))
}

pub(super) fn pane(b: Bridge, m: Drive, row: Row) -> View {
    let folder = row.is_folder();
    view()
        .app_class(Some(format!("flex-1 min-h-0 {PANEL}").into()))
        .child(
            view()
                .app_class(Some(format!("px-3 py-1 border-b {LINE}").into()))
                .label("The file being shared")
                .accessibility_value(value(row.name.clone()))
                .child(
                    text("Share")
                        .app_class(Some(format!("text-[13px] font-semibold {INK}").into())),
                )
                .child(
                    text(row.name.clone())
                        .number_of_lines(1)
                        .app_class(Some(format!("text-[12px] {MUTE}").into())),
                ),
        )
        .child(
            scroll().app_class(Some("flex-1 min-h-0".into())).child(
                view().child(adder(m, folder)).child(people(m)).child(
                    view().app_class(Some("px-3 py-2".into())).child(
                        text("Sharing by link isn't here. Google Drive's own page has it.")
                            .app_class(Some(format!("text-[11px] {DIM}").into())),
                    ),
                ),
            ),
        )
        .child(footer(b, m, row))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_press_says_what_it_does() {
        assert_eq!(press_words(1, 0), "Share with 1 person");
        assert_eq!(press_words(2, 0), "Share with 2 people");
        assert_eq!(press_words(0, 1), "Remove 1 person");
        assert_eq!(press_words(0, 3), "Remove 3 people");
        assert_eq!(press_words(2, 1), "Share with 2 people and remove 1");
    }

    #[test]
    fn afterwards_it_says_what_it_did() {
        assert_eq!(done_words(1, 0), "Shared with 1 person.");
        assert_eq!(done_words(0, 2), "Removed 2 people.");
        assert_eq!(done_words(2, 1), "Shared with 2 people and removed 1.");
    }

    #[test]
    fn an_address_is_checked_before_it_joins_the_list() {
        assert!(looks_like_address("ana@example.com"));
        for bad in [
            "",
            "ana",
            "@example.com",
            "ana@example",
            "ana@example.",
            "a b@example.com",
        ] {
            assert!(!looks_like_address(bad), "{bad}");
        }
    }
}
