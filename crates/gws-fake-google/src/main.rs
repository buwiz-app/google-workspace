//! `gws-fake-google [--port <n>] [--mails <n>] [--files <n>] [--hostile] [--rich]`: the stub
//! Google on 127.0.0.1 for a test Bot. `--files` adds a folder of that many. `--rich` adds the
//! reader's hard cases, and the invite's event in the primary calendar, unanswered. Prints
//! `base=http://127.0.0.1:<port>`, then serves until killed.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<usize>().ok())
    };
    let port = value("--port").unwrap_or(0) as u16;
    let mails = value("--mails").unwrap_or(120);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let mut seed = gws_fake_google::Seed::demo(mails)
        .with_calendar_around(now)
        .with_drive();
    if let Some(files) = value("--files") {
        seed = seed.with_big_folder("drv-big", "Big folder", files);
    }
    if args.iter().any(|a| a == "--rich") {
        seed = seed.with_rich_mail(&format!("http://127.0.0.1:{port}"));
    }
    if args.iter().any(|a| a == "--hostile") {
        seed = seed.with_hostile(&format!("http://127.0.0.1:{port}"));
    }
    seed.only_grants = args
        .iter()
        .position(|a| a == "--grant")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let google = gws_fake_google::FakeGoogle::start_on(port, seed);
    println!("base={}", google.base());
    loop {
        std::thread::park();
    }
}
