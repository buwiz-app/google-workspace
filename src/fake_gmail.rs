//! Tests only: a fake Gmail API on 127.0.0.1, one thread per connection,
//! answering each request from a route function and recording what it saw.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// A request the fake saw.
#[derive(Debug, Clone)]
pub struct Seen {
    /// the path and query, as sent (`/messages?q=…`)
    pub target: String,
    pub authorization: String,
}

pub struct Fake {
    /// the API base to give `Gmail::new`
    pub base: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

type Route = dyn Fn(&str) -> (u16, String) + Send + Sync;

/// A fake answering `route(target)` with `(status, JSON body)`.
pub fn serve(route: impl Fn(&str) -> (u16, String) + Send + Sync + 'static) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/gmail/v1/users/me", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let route: Arc<Route> = Arc::new(route);
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (route, log) = (route.clone(), log.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let target = line.split(' ').nth(1).unwrap_or_default().trim_start_matches("/gmail/v1/users/me").to_owned();
                let mut authorization = String::new();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':') {
                        if k.eq_ignore_ascii_case("authorization") {
                            authorization = v.trim().to_owned();
                        }
                    }
                }
                log.lock().unwrap().push(Seen { target: target.clone(), authorization });
                let (status, body) = route(&target);
                let reply = format!("HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(reply.as_bytes());
            });
        }
    });
    Fake { base, seen }
}
