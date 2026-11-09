//! A mail's pictures against the stub Google: the ones inside the mail show at
//! once, the ones on the web wait for a press, and a trusted sender is kept
//! for the account and goes with it.

use gws::Core;
use gws::apis::Apis;
use gws_fake_google::{FakeGoogle, Mail, Seed};
use serde_json::{Value, json};

const OWNER: &str = "owner@example.com";

fn paths(name: &str) -> gws_store::Paths {
    let dir = std::env::temp_dir().join(format!("gws-pictures-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    gws_store::Paths::in_dir(dir)
}

fn signed_in(google: &FakeGoogle, paths: &gws_store::Paths) -> Core {
    let core = Core::new(&Apis::at(&google.base()), Ok(paths.clone()));
    core.unlock("a-seed");
    let set = core.account("buwiz.account.set", &json!({"account": "google", "label": "default", "provider": "google", "subject": OWNER, "scopes": [], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    assert_ne!(set["isError"], true, "{set}");
    core
}

/// A port nothing holds, so the rich mail can name the stub before it starts.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The stub, with the rich mail pointing its web pictures at it.
fn rich() -> (FakeGoogle, String) {
    let port = free_port();
    let seed = Seed::default().with_rich_mail(&format!("http://127.0.0.1:{port}"));
    let news = seed
        .mails
        .iter()
        .find(|m| m.subject.starts_with("Five roofs"))
        .unwrap()
        .thread
        .clone();
    (FakeGoogle::start_on(port, seed), news)
}

fn data(r: Value) -> Value {
    assert_ne!(r["isError"], true, "{r}");
    r["structuredContent"].clone()
}

fn pictures_fetched(google: &FakeGoogle) -> usize {
    google
        .log()
        .iter()
        .filter(|s| s.target.starts_with("/pictures/"))
        .count()
}

#[test]
fn a_picture_inside_the_mail_shows_at_once_and_isnt_counted_as_hidden() {
    let (google, news) = rich();
    let core = signed_in(&google, &paths("cid"));
    let view = data(core.run("gws_mail_thread", &json!({"thread": news})));
    let page = view["page"].as_str().unwrap();
    assert_eq!(page.matches("data:image/png;base64,").count(), 1, "{page}");
    assert!(!page.contains("cid:"), "{page}");
    assert_eq!(view["images"], 2, "the two on the web are still out");
    assert_eq!(view["pictures"]["from"], "news@digest.example");
    assert_eq!(view["pictures"]["trusted"], false);
    assert_eq!(
        pictures_fetched(&google),
        0,
        "opening fetches nothing from the web"
    );
    // once more, from the store, the same
    let again = data(core.run("gws_mail_thread", &json!({"thread": news})));
    assert_eq!(again["page"], view["page"]);
}

fn with_inline(id: &str, inline: Vec<(String, String, String, Vec<u8>)>) -> Mail {
    let html = inline
        .iter()
        .map(|(cid, ..)| format!("<img src=\"cid:{cid}\">"))
        .collect::<String>();
    Mail {
        id: id.into(),
        thread: id.into(),
        labels: vec!["INBOX".into()],
        from: "Ana Reyes <Ana@Example.com>".into(),
        to: OWNER.into(),
        subject: "Pictures".into(),
        at: 1_800_000_000_000,
        text: "pictures".into(),
        html: Some(format!("<p>pictures</p>{html}")),
        message_id: format!("<{id}@example.com>"),
        attachments: Vec::new(),
        inline,
    }
}

#[test]
fn a_picture_inside_the_mail_over_2_mb_or_not_a_picture_stays_out() {
    let google = FakeGoogle::start(Seed::default());
    let png = |n: usize| {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(n, 0);
        bytes
    };
    let part = |cid: &str, mime: &str, bytes: Vec<u8>| {
        (cid.to_owned(), format!("{cid}.png"), mime.to_owned(), bytes)
    };
    google.deliver(with_inline(
        "badpics1",
        vec![
            part("big", "image/png", png(2 * 1024 * 1024 + 1)),
            part(
                "html",
                "image/png",
                b"<html><script>alert(1)</script>".to_vec(),
            ),
            part("svg", "image/svg+xml", b"<svg onload=alert(1)/>".to_vec()),
            part("fine", "application/octet-stream", png(100)),
        ],
    ));
    let core = signed_in(&google, &paths("badcid"));
    let view = data(core.run("gws_mail_thread", &json!({"thread": "badpics1"})));
    let page = view["page"].as_str().unwrap();
    assert_eq!(view["images"], 3, "{page}");
    assert_eq!(page.matches("data:image/png;base64,").count(), 1, "{page}");
    assert!(!page.contains("script") && !page.contains("svg"), "{page}");
    assert_eq!(view["pictures"]["from"], "ana@example.com", "lower case");
    let fetched: Vec<_> = google
        .log()
        .into_iter()
        .filter(|s| s.target.contains("/attachments/"))
        .collect();
    assert_eq!(fetched.len(), 3, "the one over 2 MB isn't fetched at all");
}

#[test]
fn a_trusted_sender_survives_a_restart_and_goes_with_a_sign_out() {
    let (google, news) = rich();
    let paths = paths("trusted");
    let core = signed_in(&google, &paths);
    data(core.run(
        "gws_mail_picture_sender_add",
        &json!({"address": "News@Digest.example"}),
    ));
    drop(core);
    let core = signed_in(&google, &paths);
    let view = data(core.run("gws_mail_thread", &json!({"thread": news})));
    assert_eq!(view["pictures"]["trusted"], true);
    data(core.run(
        "gws_mail_picture_sender_remove",
        &json!({"address": "news@digest.example"}),
    ));
    let view = data(core.run("gws_mail_thread", &json!({"thread": news})));
    assert_eq!(view["pictures"]["trusted"], false);
    data(core.run(
        "gws_mail_picture_sender_add",
        &json!({"address": "news@digest.example"}),
    ));
    let cleared = core.account(
        "buwiz.account.clear",
        &json!({"account": "google", "label": "default"}),
    );
    assert_ne!(cleared["isError"], true, "{cleared}");
    drop(core);
    let core = signed_in(&google, &paths);
    let view = data(core.run("gws_mail_thread", &json!({"thread": news})));
    assert_eq!(
        view["pictures"]["trusted"], false,
        "it went with the sign-out"
    );
}

#[test]
fn only_an_address_is_trusted() {
    let google = FakeGoogle::start(Seed::default());
    let core = signed_in(&google, &paths("address"));
    for bad in ["", "news", "a b@x.example", "<a@x.example>"] {
        let r = core.run("gws_mail_picture_sender_add", &json!({"address": bad}));
        assert_eq!(r["isError"], true, "{bad:?}");
    }
}

/// The model reads mail and never fetches a picture: reading it touches no
/// picture on the web, and the pictures tools are the screen's alone.
#[test]
fn the_model_reading_a_mail_fetches_no_picture() {
    let (google, news) = rich();
    let core = signed_in(&google, &paths("model"));
    core.run_for_chat(
        "gws_search_mail",
        &json!({"query": "from:news@digest.example"}),
    );
    // the newsletter is one message, with its thread's id
    let read = data(core.run_for_model("gws_read_mail", &json!({"id": news})));
    assert!(read.to_string().contains("Five roofs"), "{read}");
    assert_eq!(pictures_fetched(&google), 0);
    // and were the loop's check ever bypassed, the model's own path refuses
    for (tool, args) in [
        ("gws_mail_pictures", json!({"thread": news})),
        (
            "gws_mail_picture_sender_add",
            json!({"address": "news@digest.example"}),
        ),
    ] {
        let refused = core.run_for_model(tool, &args);
        assert_eq!(
            refused["structuredContent"]["code"], "permission_denied",
            "{tool}"
        );
        let refused = core.run_for_chat(tool, &args);
        assert_eq!(
            refused["structuredContent"]["code"], "permission_denied",
            "{tool}"
        );
    }
    assert_eq!(pictures_fetched(&google), 0);
    for tool in [
        "gws_mail_pictures",
        "gws_mail_picture_sender_add",
        "gws_mail_picture_sender_remove",
    ] {
        let def = gws::tools::defs()
            .into_iter()
            .find(|d| d.name == tool)
            .unwrap();
        assert_eq!(def.audience, gws::tools::APP_ONLY, "{tool}");
        assert!(!def.read_only, "{tool}");
    }
}

/// A release build fetches nothing from this Mac or over plain http, so the
/// stub's pictures stay out however they're asked for.
#[cfg(not(feature = "stub"))]
#[test]
fn outside_the_stub_its_pictures_are_refused() {
    let (google, news) = rich();
    let core = signed_in(&google, &paths("release"));
    let view = data(core.run("gws_mail_pictures", &json!({"thread": news})));
    assert_eq!(view["images"], 2);
    assert_eq!(view["pictures"]["asked"], true);
    assert_eq!(pictures_fetched(&google), 0);
}

#[cfg(feature = "stub")]
#[test]
fn show_pictures_brings_the_web_pictures_in_and_keeps_none() {
    let (google, news) = rich();
    let core = signed_in(&google, &paths("show"));
    data(core.run("gws_mail_thread", &json!({"thread": news})));
    let answer = core.run("gws_mail_pictures", &json!({"thread": news}));
    assert!(
        answer.to_string().len() <= 1024 * 1024,
        "a result stops at 1 MiB"
    );
    let view = data(answer);
    let page = view["page"].as_str().unwrap();
    assert_eq!(view["images"], 0, "{page}");
    assert_eq!(page.matches("data:image/png;base64,").count(), 3);
    assert!(!page.contains("/pictures/"), "{page}");
    assert_eq!(pictures_fetched(&google), 2);
    // nothing fetched was kept: the next open is the one before the press
    let stored = data(core.run("gws_mail_thread", &json!({"thread": news})));
    assert_eq!(stored["images"], 2);
    assert_eq!(pictures_fetched(&google), 2);
}

#[cfg(feature = "stub")]
#[test]
fn a_trusted_ask_fetches_only_the_trusted_senders_pictures() {
    let (google, news) = rich();
    let core = signed_in(&google, &paths("only"));
    let view = data(core.run(
        "gws_mail_pictures",
        &json!({"thread": news, "trusted": true}),
    ));
    assert_eq!(view["images"], 2, "nobody is trusted yet");
    assert_eq!(pictures_fetched(&google), 0);
    data(core.run(
        "gws_mail_picture_sender_add",
        &json!({"address": "news@digest.example"}),
    ));
    let view = data(core.run(
        "gws_mail_pictures",
        &json!({"thread": news, "trusted": true}),
    ));
    assert_eq!(view["images"], 0);
    assert_eq!(view["pictures"]["trusted"], true);
}

/// A From anyone can write is no sender to trust: a mail Gmail doesn't vouch
/// for shows its bar as a stranger's, and a trusted ask fetches nothing of it.
#[cfg(feature = "stub")]
#[test]
fn a_forged_sender_is_never_trusted() {
    let (google, news) = rich();
    google.forge_sender(&news);
    let core = signed_in(&google, &paths("forged"));
    data(core.run(
        "gws_mail_picture_sender_add",
        &json!({"address": "news@digest.example"}),
    ));
    let view = data(core.run("gws_mail_thread", &json!({"thread": news})));
    assert_eq!(view["pictures"]["from"], "news@digest.example");
    assert_eq!(view["pictures"]["trusted"], false, "{}", view["pictures"]);
    let asked = data(core.run(
        "gws_mail_pictures",
        &json!({"thread": news, "trusted": true}),
    ));
    assert_eq!(asked["images"], 2);
    assert_eq!(pictures_fetched(&google), 0);
}

/// One thread, two senders, each with a web picture of their own.
#[cfg(feature = "stub")]
fn two_senders(google: &FakeGoogle) {
    let one = |id: &str, from: &str, picture: &str, at: i64| Mail {
        id: id.into(),
        thread: "twosenders01".into(),
        labels: vec!["INBOX".into()],
        from: from.into(),
        to: OWNER.into(),
        subject: "Both of us".into(),
        at,
        text: "pictures".into(),
        html: Some(format!(
            "<p>pictures</p><img src=\"{}/pictures/{picture}\">",
            google.base()
        )),
        message_id: format!("<{id}@example.com>"),
        attachments: Vec::new(),
        inline: Vec::new(),
    };
    google.deliver(one(
        "twosenders01",
        "Ana <ana@example.com>",
        "hero.png",
        1_800_000_000_000,
    ));
    google.deliver(one(
        "twosenders02",
        "Ben <ben@example.com>",
        "roof2.png",
        1_800_000_100_000,
    ));
}

#[cfg(feature = "stub")]
#[test]
fn always_from_a_sender_fetches_only_that_senders_pictures() {
    let (google, _) = rich();
    two_senders(&google);
    let core = signed_in(&google, &paths("from"));
    let view = data(core.run(
        "gws_mail_pictures",
        &json!({"thread": "twosenders01", "from": "Ana@Example.com"}),
    ));
    assert_eq!(view["images"], 1, "Ben's stays out");
    assert_eq!(pictures_fetched(&google), 1);
    assert!(
        google
            .log()
            .iter()
            .any(|s| s.target == "/pictures/hero.png")
    );
    // and Show pictures takes both
    let all = data(core.run("gws_mail_pictures", &json!({"thread": "twosenders01"})));
    assert_eq!(all["images"], 0);
}

/// The sender offered is the newest who isn't the person, never the person.
#[test]
fn the_persons_own_reply_isnt_the_sender_offered() {
    let google = FakeGoogle::start(Seed::default());
    let mail = |id: &str, from: &str, labels: Vec<String>, at: i64| Mail {
        id: id.into(),
        thread: if id.starts_with("ownonly") {
            id
        } else {
            "ownreply0001"
        }
        .into(),
        labels,
        from: from.into(),
        to: OWNER.into(),
        subject: "Re: hi".into(),
        at,
        text: "hi".into(),
        html: None,
        message_id: format!("<{id}@example.com>"),
        attachments: Vec::new(),
        inline: Vec::new(),
    };
    google.deliver(mail(
        "ownreply0001",
        "Ana <ana@example.com>",
        vec!["INBOX".into()],
        1_800_000_000_000,
    ));
    google.deliver(mail(
        "ownreply0002",
        &format!("Me <{OWNER}>"),
        vec!["SENT".into()],
        1_800_000_100_000,
    ));
    let core = signed_in(&google, &paths("own"));
    let view = data(core.run("gws_mail_thread", &json!({"thread": "ownreply0001"})));
    assert_eq!(view["pictures"]["from"], "ana@example.com");
    google.deliver(mail(
        "ownonly00001",
        &format!("Me <{OWNER}>"),
        vec!["SENT".into()],
        1_800_000_200_000,
    ));
    let mine = data(core.run("gws_mail_thread", &json!({"thread": "ownonly00001"})));
    assert_eq!(mine["pictures"]["from"], Value::Null);
}
