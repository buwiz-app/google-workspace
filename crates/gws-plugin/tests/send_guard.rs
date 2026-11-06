//! The guard for a test run on a real account. Alone in this binary, as what
//! it sets is the whole process's. No `stub` feature here: a real account means
//! a build without it, so this has to pass in both.

use gws::Core;
use gws::apis::Apis;
use gws_fake_google::{FAKE_EMAIL, FakeGoogle, Seed};
use serde_json::{Value, json};

fn signed_in(google: &FakeGoogle) -> Core {
    let dir = std::env::temp_dir().join(format!("gws-guard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let core = Core::new(&Apis::at(&google.base()), Ok(gws_store::Paths::in_dir(dir)));
    core.accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/gmail.modify"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    core
}

fn data(answer: Value) -> Value {
    assert_ne!(answer["isError"], true, "{answer}");
    answer["structuredContent"].clone()
}

fn send(core: &Core, draft: &Value) -> Value {
    let armed = data(core.run(
        "gws_mail_send_arm",
        &json!({"draft": draft["draft"], "revision": draft["revision"]}),
    ));
    core.run(
        "gws_mail_send",
        &json!({"draft": draft["draft"], "revision": draft["revision"], "armed": armed["armed"]}),
    )
}

#[test]
fn a_test_run_on_a_real_account_cannot_write_to_anyone_else() {
    // SAFETY: the one test in this binary, before any thread of its own
    unsafe { std::env::set_var("GWS_ONLY_TO", "support@buwiz.com") };
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google);
    for draft in [
        json!({"to": ["x@example.com"], "body": "no"}),
        json!({"to": ["support@buwiz.com"], "cc": ["x@example.com"], "body": "no"}),
        json!({"to": ["support@buwiz.com"], "bcc": ["x@example.com"], "body": "no"}),
    ] {
        let saved = data(core.run("gws_mail_draft_save", &draft));
        let refused = send(&core, &saved);
        assert!(
            refused["structuredContent"]["refused"].is_string(),
            "{draft}: {refused}"
        );
        assert!(refused.to_string().contains("writes only to"), "{refused}");
    }
    assert_eq!(google.sent(), []);
    let fine = data(core.run(
        "gws_mail_draft_save",
        &json!({"to": ["SUPPORT@buwiz.com"], "body": "buwiz-test"}),
    ));
    data(send(&core, &fine));
    assert_eq!(google.sent().len(), 1);
}
