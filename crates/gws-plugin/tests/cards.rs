//! A chat's cards across a restart of the plugin. A card holds only a ticket,
//! so the ticket has to mean the same thing to the next process: an update, a
//! crash or the Bot starting again must not leave a card with nothing to show.
//! A restart here is a second `Core` over the same folder and the same key,
//! with nothing of the first one's memory.

use gws::Core;
use gws::apis::Apis;
use gws_fake_google::{FAKE_EMAIL, FakeCalendar, FakeGoogle, Seed};
use serde_json::{Value, json};

const LOST: &str = "This card lost its link when the plugin restarted.";

fn folder(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("gws-cards-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A process: signed in, with the store's key, over `dir`.
fn started(base: &str, dir: &std::path::Path) -> Core {
    let core = Core::new(&Apis::at(base), Ok(gws_store::Paths::in_dir(dir)));
    core.unlock("seed");
    push(&core);
    core
}

fn push(core: &Core) {
    let set = core.account("buwiz.account.set", &json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/gmail.modify", "https://www.googleapis.com/auth/drive", "https://www.googleapis.com/auth/calendar.events"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    assert_ne!(set["isError"], true, "{set}");
}

fn data(answer: Value) -> Value {
    assert_ne!(answer["isError"], true, "{answer}");
    assert_eq!(answer["structuredContent"].get("refused"), None, "{answer}");
    answer["structuredContent"].clone()
}

fn prop(result: &Value, key: &str) -> String {
    result["_meta"]["buwiz/open"]["props"][key]
        .as_str()
        .unwrap_or_else(|| panic!("no card: {result}"))
        .to_owned()
}

fn tomorrow(time: &str) -> String {
    let day = chrono::Utc::now().date_naive() + chrono::Duration::days(1);
    format!("{day}T{time}+08:00")
}

fn event() -> Value {
    json!({"title": "Lunch", "start": tomorrow("12:00:00"), "end": tomorrow("13:00:00"), "guests": ["ana@example.com"], "where": "Cafe"})
}

#[test]
fn a_mail_card_shows_its_mail_after_the_plugin_restarts() {
    let google = FakeGoogle::start(Seed::demo(12));
    let dir = folder("mail");
    let first = started(&google.base(), &dir);
    let ticket = prop(
        &first.run_for_chat("gws_search_mail", &json!({"query": "from:ana"})),
        "search",
    );
    let before = data(first.run("gws_mail_card", &json!({"ticket": ticket})));
    assert!(!before["rows"].as_array().unwrap().is_empty());
    let row = before["rows"][0]["ticket"].as_str().unwrap().to_owned();
    drop(first);

    let second = started(&google.base(), &dir);
    let after = data(second.run("gws_mail_card", &json!({"ticket": ticket})));
    let rows = after["rows"].as_array().unwrap();
    assert!(!rows.is_empty(), "{after}");
    assert!(
        rows.iter()
            .all(|r| r["sender"].as_str().unwrap().contains("Ana")),
        "{after}"
    );
    // a card that was never reloaded still opens its rows, and maximizes
    let opened = data(second.run("gws_mail_thread", &json!({"thread": row})));
    assert!(opened["page"].as_str().unwrap().contains("fake"));
    let full = data(second.run("gws_mail_list", &json!({"search": ticket})));
    assert!(!full["threads"].as_array().unwrap().is_empty());
    use buwiz_plugin::Plugin as _;
    let mut plugin = gws::Gws {
        core: std::sync::Arc::new(second),
    };
    assert_eq!(plugin.ticket_title(&ticket).as_deref(), Some("Mail"));
}

#[test]
fn a_draft_card_shows_its_draft_after_the_plugin_restarts() {
    let google = FakeGoogle::start(Seed::demo(2));
    let dir = folder("draft");
    let first = started(&google.base(), &dir);
    let ticket = prop(
        &first.run_for_chat(
            "gws_mail_draft_save",
            &json!({"to": ["ana@example.com"], "subject": "Hi", "body": "Noon?"}),
        ),
        "draft",
    );
    drop(first);
    let second = started(&google.base(), &dir);
    let read = data(second.run("gws_mail_draft_read", &json!({"ticket": ticket})));
    assert_eq!(read["body"], "Noon?");
}

#[test]
fn a_files_card_shows_its_files_after_the_plugin_restarts() {
    let google = FakeGoogle::start(Seed::demo(4).with_drive());
    let dir = folder("drive");
    let first = started(&google.base(), &dir);
    let ticket = prop(
        &first.run_for_chat("gws_search_drive", &json!({"query": "meeting"})),
        "search",
    );
    drop(first);
    let second = started(&google.base(), &dir);
    let card = data(second.run("gws_drive_card", &json!({"ticket": ticket})));
    assert!(!card["rows"].as_array().unwrap().is_empty(), "{card}");
    let full = data(second.run("gws_drive_list", &json!({"search": ticket})));
    assert!(!full["files"].as_array().unwrap().is_empty());
    use buwiz_plugin::Plugin as _;
    let mut plugin = gws::Gws {
        core: std::sync::Arc::new(second),
    };
    assert_eq!(plugin.ticket_title(&ticket).as_deref(), Some("Drive"));
}

#[test]
fn an_event_card_shows_its_event_after_the_plugin_restarts() {
    let google = FakeGoogle::start(Seed {
        calendars: vec![FakeCalendar::new(
            FAKE_EMAIL,
            "Owner",
            true,
            "America/New_York",
        )],
        ..Seed::default()
    });
    let dir = folder("event");
    let first = started(&google.base(), &dir);
    let ticket = prop(&first.run_for_chat("gws_event_draft", &event()), "draft");
    drop(first);
    let second = started(&google.base(), &dir);
    let draft = data(second.run("gws_event_draft_read", &json!({"ticket": ticket})));
    assert_eq!(draft["title"], "Lunch");
    assert_eq!(draft["guests"], json!(["ana@example.com"]));
}

#[test]
fn a_cards_ticket_never_ages_out_and_a_rows_does() {
    let google = FakeGoogle::start(Seed::demo(4).with_drive());
    let dir = folder("ages");
    let first = started(&google.base(), &dir);
    let card = prop(
        &first.run_for_chat("gws_search_drive", &json!({"query": "meeting"})),
        "search",
    );
    // far more rows than both generations hold
    let old = gws::mail::issue_row(&first, &gws::mail::Subject::Thread("t-0".to_owned()));
    let mut last = String::new();
    for n in 1..2000 {
        last = gws::mail::issue_row(&first, &gws::mail::Subject::Thread(format!("t-{n}")));
    }
    assert_eq!(gws::mail::subject_of(&first, &old), None, "the oldest went");
    assert!(gws::mail::subject_of(&first, &last).is_some());
    assert!(gws::mail::subject_of(&first, &card).is_some());
    drop(first);
    let second = started(&google.base(), &dir);
    assert_eq!(gws::mail::subject_of(&second, &old), None);
    assert!(gws::mail::subject_of(&second, &last).is_some());
    assert!(
        gws::mail::subject_of(&second, &card).is_some(),
        "the card's own is kept"
    );
}

#[test]
fn a_ticket_lost_before_this_is_bound_again_by_the_search_that_made_the_card() {
    let google = FakeGoogle::start(Seed::demo(12).with_drive());
    let dir = folder("lost");
    let core = started(&google.base(), &dir);
    let lost = "tkt_0123456789ABCDEFGHJKMNPQRS";

    let gone = core.run("gws_mail_card", &json!({"ticket": lost}));
    assert_eq!(gone["isError"], true);
    assert_eq!(gone["structuredContent"]["code"], "ticket_lost");
    assert_eq!(gone["structuredContent"]["message"], LOST);
    let card = data(core.run(
        "gws_mail_card",
        &json!({"ticket": lost, "query": "from:ana"}),
    ));
    assert!(!card["rows"].as_array().unwrap().is_empty(), "{card}");
    // bound for good: the next read needs no query, nor does the next process
    data(core.run("gws_mail_card", &json!({"ticket": lost})));
    drop(core);
    let core = started(&google.base(), &dir);
    data(core.run("gws_mail_card", &json!({"ticket": lost})));

    let files = "tkt_ZZZZZZZZZZZZZZZZZZZZZZZZZZ";
    let gone = core.run("gws_drive_card", &json!({"ticket": files}));
    assert_eq!(gone["structuredContent"]["refused"], "ticket_lost");
    assert_eq!(gone["structuredContent"]["message"], LOST);
    let card = data(core.run(
        "gws_drive_card",
        &json!({"ticket": files, "query": "meeting"}),
    ));
    assert!(!card["rows"].as_array().unwrap().is_empty(), "{card}");
    data(core.run("gws_drive_card", &json!({"ticket": files})));

    let draft = "tkt_YYYYYYYYYYYYYYYYYYYYYYYYYY";
    let gone = core.run("gws_event_draft_read", &json!({"ticket": draft}));
    assert_eq!(gone["structuredContent"]["refused"], "ticket_lost");
    // the chat's fields bind it only when they say the account it was drafted
    // on: without one it could be saved to whoever is signed in now
    let unowned = core.run(
        "gws_event_draft_read",
        &json!({"ticket": draft, "fields": event()}),
    );
    assert_eq!(
        unowned["structuredContent"]["message"],
        "This card is out of date."
    );
    let mut fields = event();
    fields["as_account"] = json!(FAKE_EMAIL);
    let read = data(core.run(
        "gws_event_draft_read",
        &json!({"ticket": draft, "fields": fields}),
    ));
    assert_eq!(read["title"], "Lunch");
    assert_eq!(
        data(core.run("gws_event_draft_read", &json!({"ticket": draft})))["where"],
        "Cafe"
    );
    // fields that make no event bind nothing
    let bad = core.run(
        "gws_event_draft_read",
        &json!({"ticket": "tkt_XXXXXXXXXXXXXXXXXXXXXXXXXX", "fields": {"title": "x", "start": "soon", "end": "later", "as_account": FAKE_EMAIL}}),
    );
    assert_eq!(bad["structuredContent"]["refused"], "ticket_lost");

    let mail = core.run("gws_mail_draft_read", &json!({"ticket": lost}));
    assert_eq!(mail["isError"], true, "a search's ticket is no draft's");
    let mail = core.run(
        "gws_mail_draft_read",
        &json!({"ticket": "tkt_WWWWWWWWWWWWWWWWWWWWWWWWWW"}),
    );
    assert_eq!(mail["structuredContent"]["code"], "ticket_lost");
    assert_eq!(mail["structuredContent"]["message"], LOST);
}

#[test]
fn only_a_ticket_is_ever_bound_and_no_card_says_out_of_date() {
    let google = FakeGoogle::start(Seed::demo(4).with_drive());
    let core = started(&google.base(), &folder("shape"));
    for ticket in ["", "nonsense", "tkt_short", "in:inbox"] {
        let card = core.run(
            "gws_mail_card",
            &json!({"ticket": ticket, "query": "from:ana"}),
        );
        assert_eq!(card["structuredContent"]["code"], "ticket_lost", "{ticket}");
    }
    let mut said = String::new();
    for (tool, key) in [
        ("gws_mail_card", "ticket"),
        ("gws_mail_draft_read", "ticket"),
        ("gws_drive_card", "ticket"),
        ("gws_drive_list", "search"),
        ("gws_event_draft_read", "ticket"),
    ] {
        said.push_str(
            &core
                .run(tool, &json!({key: "tkt_00000000000000000000000000"}))
                .to_string(),
        );
    }
    assert!(!said.contains("out of date"), "{said}");
}

#[test]
fn signing_out_takes_the_accounts_tickets_with_its_mail() {
    let google = FakeGoogle::start(Seed {
        calendars: vec![FakeCalendar::new(
            FAKE_EMAIL,
            "Owner",
            true,
            "America/New_York",
        )],
        ..Seed::demo(6)
    });
    let dir = folder("signout");
    let core = started(&google.base(), &dir);
    let search = prop(
        &core.run_for_chat("gws_search_mail", &json!({"query": "from:ana"})),
        "search",
    );
    let draft = prop(&core.run_for_chat("gws_event_draft", &event()), "draft");
    let cleared = core.account(
        "buwiz.account.clear",
        &json!({"account": "google", "label": "default"}),
    );
    assert_ne!(cleared["isError"], true, "{cleared}");
    assert_eq!(gws::mail::subject_of(&core, &search), None);
    assert_eq!(gws::mail::subject_of(&core, &draft), None);
    drop(core);
    // signed in again, in a new process: nothing of the old sign-in's is behind them
    let core = started(&google.base(), &dir);
    assert_eq!(gws::mail::subject_of(&core, &draft), None);
    let gone = core.run("gws_event_draft_read", &json!({"ticket": draft}));
    assert_eq!(gone["structuredContent"]["refused"], "ticket_lost");
}

#[test]
fn with_no_store_tickets_last_as_long_as_the_process() {
    let google = FakeGoogle::start(Seed::demo(6));
    // never unlocked: the store can't open
    let core = Core::new(
        &Apis::at(&google.base()),
        Ok(gws_store::Paths::in_dir(folder("nostore"))),
    );
    core.accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/calendar.events"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    let ticket = prop(&core.run_for_chat("gws_event_draft", &event()), "draft");
    let draft = data(core.run("gws_event_draft_read", &json!({"ticket": ticket})));
    assert_eq!(draft["title"], "Lunch");
    for n in 0..2000 {
        gws::mail::issue_row(&core, &gws::mail::Subject::Thread(format!("t-{n}")));
    }
    assert!(
        gws::mail::subject_of(&core, &ticket).is_some(),
        "a card's ticket isn't pushed out by rows"
    );
}

// --- What a card shows when Google can't be asked ------------------------------

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A process over `dir` that reaches no Google at all.
fn cut_off(dir: &std::path::Path) -> Core {
    started("http://127.0.0.1:1", dir)
}

/// A process over `dir` whose token Google no longer takes.
fn refused_by_google(base: &str, dir: &std::path::Path) -> Core {
    let core = Core::new(&Apis::at(base), Ok(gws_store::Paths::in_dir(dir)));
    core.unlock("seed");
    let set = core.account("buwiz.account.set", &json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/gmail.modify", "https://www.googleapis.com/auth/drive"], "access_token": "not-a-token-google-knows", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    assert_ne!(set["isError"], true, "{set}");
    core
}

fn subjects(card: &Value, key: &str) -> Vec<String> {
    card["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[key].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn offline_a_mail_card_keeps_the_mail_it_last_showed_and_says_from_when() {
    let google = FakeGoogle::start(Seed::demo(12));
    let dir = folder("mail-offline");
    let first = started(&google.base(), &dir);
    // a search only Google can run: nothing kept has these words in it
    let ticket = prop(
        &first.run_for_chat("gws_search_mail", &json!({"query": "from:ana"})),
        "search",
    );
    let before = now_ms();
    let live = data(first.run("gws_mail_card", &json!({"ticket": ticket})));
    let after = now_ms();
    assert_eq!(live["saved"], false);
    assert_eq!(live.get("why"), None, "{live}");
    assert_eq!(live.get("saved_at"), None, "{live}");
    drop(first);

    let dead = cut_off(&dir);
    let kept = data(dead.run("gws_mail_card", &json!({"ticket": ticket})));
    assert_eq!(kept["saved"], true);
    assert_eq!(kept["why"], "offline");
    let at = kept["saved_at"].as_i64().unwrap();
    assert!((before..=after).contains(&at), "{before} {at} {after}");
    assert_eq!(subjects(&kept, "subject"), subjects(&live, "subject"));
    assert!(!subjects(&kept, "subject").is_empty());
    assert!(
        matches!(
            gws::mail::subject_of(&dead, kept["rows"][0]["ticket"].as_str().unwrap()),
            Some(gws::mail::Subject::Thread(_))
        ),
        "a kept row has its ticket"
    );
}

#[test]
fn a_mail_card_google_refuses_keeps_its_mail_and_says_signed_out() {
    let google = FakeGoogle::start(Seed::demo(12));
    let dir = folder("mail-refused");
    let first = started(&google.base(), &dir);
    let ticket = prop(
        &first.run_for_chat("gws_search_mail", &json!({"query": "from:ana"})),
        "search",
    );
    let live = data(first.run("gws_mail_card", &json!({"ticket": ticket})));
    drop(first);

    let refused = refused_by_google(&google.base(), &dir);
    let kept = data(refused.run("gws_mail_card", &json!({"ticket": ticket})));
    assert_eq!(kept["saved"], true);
    assert_eq!(kept["why"], "signed_out");
    assert!(kept["saved_at"].is_i64());
    assert_eq!(subjects(&kept, "subject"), subjects(&live, "subject"));

    // with nothing kept there is nothing to show: the sign-in line, as before
    let bare = refused_by_google(&google.base(), &folder("mail-refused-bare"));
    let asked = bare.run(
        "gws_mail_card",
        &json!({"ticket": "tkt_0123456789ABCDEFGHJKMNPQRS", "query": "from:ana"}),
    );
    assert_eq!(
        asked["structuredContent"]["code"], "account_required",
        "{asked}"
    );
}

#[test]
fn offline_or_refused_a_files_card_keeps_the_files_it_last_showed() {
    let google = FakeGoogle::start(Seed::demo(4).with_drive());
    let dir = folder("drive-offline");
    let first = started(&google.base(), &dir);
    let ticket = prop(
        &first.run_for_chat("gws_search_drive", &json!({"query": "meeting"})),
        "search",
    );
    let before = now_ms();
    let live = data(first.run("gws_drive_card", &json!({"ticket": ticket})));
    let after = now_ms();
    assert_eq!(live["saved"], false);
    assert_eq!(live.get("why"), None, "{live}");
    drop(first);

    let dead = cut_off(&dir);
    let kept = data(dead.run("gws_drive_card", &json!({"ticket": ticket})));
    assert_eq!(
        (&kept["saved"], &kept["why"]),
        (&json!(true), &json!("offline"))
    );
    let at = kept["saved_at"].as_i64().unwrap();
    assert!((before..=after).contains(&at), "{before} {at} {after}");
    assert_eq!(subjects(&kept, "name"), subjects(&live, "name"));
    assert!(!subjects(&kept, "name").is_empty());
    drop(dead);

    let refused = refused_by_google(&google.base(), &dir);
    let kept = data(refused.run("gws_drive_card", &json!({"ticket": ticket})));
    assert_eq!(
        (&kept["saved"], &kept["why"]),
        (&json!(true), &json!("signed_out"))
    );
    assert_eq!(subjects(&kept, "name"), subjects(&live, "name"));

    let bare = refused_by_google(&google.base(), &folder("drive-refused-bare"));
    let asked = bare.run(
        "gws_drive_card",
        &json!({"ticket": "tkt_0123456789ABCDEFGHJKMNPQRS", "query": "meeting"}),
    );
    assert_eq!(
        asked["structuredContent"]["code"], "account_required",
        "{asked}"
    );
}

// --- A card as plain text, from the store alone -------------------------------

#[test]
fn a_cards_text_comes_from_whats_kept_and_asks_google_nothing() {
    let google = FakeGoogle::start(Seed {
        calendars: vec![FakeCalendar::new(
            FAKE_EMAIL,
            "Owner",
            true,
            "America/New_York",
        )],
        ..Seed::demo(12).with_drive()
    });
    let dir = folder("text");
    let first = started(&google.base(), &dir);
    let mail = prop(
        &first.run_for_chat("gws_search_mail", &json!({"query": "from:ana"})),
        "search",
    );
    let shown = data(first.run("gws_mail_card", &json!({"ticket": mail})));
    let files = prop(
        &first.run_for_chat("gws_search_drive", &json!({"query": "meeting"})),
        "search",
    );
    let listed = data(first.run("gws_drive_card", &json!({"ticket": files})));
    let event = prop(&first.run_for_chat("gws_event_draft", &event()), "draft");
    let unread = prop(
        &first.run_for_chat("gws_search_mail", &json!({"query": "from:ben"})),
        "search",
    );
    drop(first);

    // no Google to ask: whatever comes is the store's
    let core = cut_off(&dir);
    let text = data(core.run("gws_card_text", &json!({"ticket": mail})));
    assert_eq!(text["title"], "Mail");
    let lines: Vec<&str> = text["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert_eq!(lines.len(), shown["rows"].as_array().unwrap().len());
    for (line, row) in lines.iter().zip(shown["rows"].as_array().unwrap()) {
        assert_eq!(
            *line,
            format!(
                "{} — {}",
                row["sender"].as_str().unwrap(),
                row["subject"].as_str().unwrap()
            )
        );
    }
    assert!(text["at"].is_i64());

    let text = data(core.run("gws_card_text", &json!({"ticket": files})));
    assert_eq!(text["title"], "Drive");
    let names = subjects(&listed, "name");
    let lines = text["lines"].as_array().unwrap();
    assert_eq!(lines.len(), names.len());
    assert!(lines[0].as_str().unwrap().starts_with(&names[0]), "{text}");

    let text = data(core.run("gws_card_text", &json!({"ticket": event})));
    assert_eq!(text["title"], "Calendar");
    let said = text["lines"].to_string();
    assert!(
        said.contains("Lunch") && said.contains("Cafe") && said.contains("ana@example.com"),
        "{said}"
    );

    // a card that never read live has nothing kept, and neither has a stranger's ticket
    for ticket in [
        unread.as_str(),
        "tkt_0123456789ABCDEFGHJKMNPQRS",
        "nonsense",
    ] {
        let none = core.run("gws_card_text", &json!({"ticket": ticket}));
        assert_eq!(none["isError"], true, "{none}");
        assert_eq!(none["structuredContent"]["code"], "not_found", "{none}");
    }
}

#[test]
fn only_a_screen_may_read_a_cards_text() {
    let tools = gws::tools::defs();
    let tool = tools
        .iter()
        .find(|t| t.name == "gws_card_text")
        .expect("the tool is listed");
    assert_eq!(tool.audience, gws::tools::APP_ONLY);
    assert!(tool.read_only);
    assert!(!tool.open_world, "it never asks Google");
}
