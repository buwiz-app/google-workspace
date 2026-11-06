//! `gws-fake-google [--port <n>] [--mails <n>] [--hostile]`: the stub Google on 127.0.0.1
//! for a test Bot. Prints `base=http://127.0.0.1:<port>`, then serves until killed.

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
    let mut seed = gws_fake_google::Seed::demo(mails);
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
