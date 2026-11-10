//! What a card's ticket stands for, kept so the next process still knows. A
//! chat's card holds only its ticket, and the plugin restarts for an update, a
//! crash or the Bot starting again: a ticket that died with the process left
//! the card with nothing to show.
//!
//! A card's own ticket is kept among its account's newest [`CARDS_MAX`]; an
//! older one keeps only its ticket and account, standing for nothing
//! ([`GONE`]), so its card says it is out of date rather than read anything
//! again. A row's ticket, which its card issues again whenever it reads, is
//! kept only among the newest [`ROWS_MAX`]. Each belongs to the account signed
//! in when it was issued, stands for something only under it, and goes with
//! that account's mail.

use rusqlite::{OptionalExtension as _, params};

use crate::{Result, Store};

/// How many rows' tickets are kept.
pub const ROWS_MAX: i64 = 1024;

/// How many cards' tickets each account keeps what they stand for.
pub const CARDS_MAX: i64 = 2000;

/// How many cards past [`CARDS_MAX`] are still known as gone, oldest dropped.
pub const GONE_MAX: i64 = 20_000;

/// The kind of a card's ticket that aged out: it stands for nothing.
pub const GONE: &str = "gone";

/// What the store knows of a ticket: what it stands for, whether it's a
/// card's own, and whose it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub ticket: Ticket,
    pub card: bool,
    pub account: String,
}

/// A ticket and what it stands for: a kind the plugin names, and its subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub ticket: String,
    pub kind: String,
    pub subject: String,
}

impl Store {
    /// The newest ticket kept for this subject of `account`'s.
    pub fn ticket_for(&self, account: &str, kind: &str, subject: &str) -> Result<Option<String>> {
        if kind == GONE {
            return Ok(None);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT ticket FROM tickets WHERE account = ? AND kind = ? AND subject = ? ORDER BY seq DESC LIMIT 1",
                params![account, kind, subject],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Keeps each ticket for `account`, as the newest. One already a card's
    /// stays one, and a ticket kept before keeps what it stood for and whose
    /// it is. Rows past [`ROWS_MAX`] go, oldest first; `account`'s cards past
    /// [`CARDS_MAX`] stand for nothing from then on ([`GONE`]). → the tickets
    /// that went so now, for whoever holds them elsewhere to drop.
    pub fn keep_tickets(
        &self,
        account: &str,
        tickets: &[Ticket],
        card: bool,
    ) -> Result<Vec<String>> {
        let tx = self.conn.unchecked_transaction()?;
        let mut seq: i64 = tx.query_row("SELECT coalesce(max(seq), 0) FROM tickets", [], |r| {
            r.get(0)
        })?;
        for t in tickets {
            seq += 1;
            tx.execute(
                "INSERT INTO tickets(ticket, account, kind, subject, card, seq) VALUES(?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(ticket) DO UPDATE SET seq = excluded.seq, card = max(card, excluded.card) \
                 WHERE tickets.account = excluded.account AND tickets.kind != 'gone'",
                params![t.ticket, account, t.kind, t.subject, card, seq],
            )?;
        }
        tx.execute(
            "DELETE FROM tickets WHERE card = 0 AND seq <= (SELECT seq FROM tickets WHERE card = 0 ORDER BY seq DESC LIMIT 1 OFFSET ?)",
            [ROWS_MAX],
        )?;
        // an old card's event, search or draft is no longer kept: its ticket is
        // known as gone, so its card says it's out of date
        let gone: Vec<String> = tx
            .prepare(
                "UPDATE tickets SET kind = 'gone', subject = '' WHERE account = ?1 AND card = 1 AND kind != 'gone' AND seq <= \
                 (SELECT seq FROM tickets WHERE account = ?1 AND card = 1 AND kind != 'gone' ORDER BY seq DESC LIMIT 1 OFFSET ?2) \
                 RETURNING ticket",
            )?
            .query_map(params![account, CARDS_MAX], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        tx.execute(
            "DELETE FROM tickets WHERE kind = 'gone' AND seq <= (SELECT seq FROM tickets WHERE kind = 'gone' ORDER BY seq DESC LIMIT 1 OFFSET ?)",
            [GONE_MAX],
        )?;
        tx.commit()?;
        Ok(gone)
    }

    /// What the store knows of `ticket`, whoever's it is.
    pub fn ticket(&self, ticket: &str) -> Result<Option<Kept>> {
        Ok(self
            .conn
            .query_row(
                "SELECT ticket, kind, subject, card, account FROM tickets WHERE ticket = ?",
                [ticket],
                |r| {
                    Ok(Kept {
                        ticket: Ticket {
                            ticket: r.get(0)?,
                            kind: r.get(1)?,
                            subject: r.get(2)?,
                        },
                        card: r.get(3)?,
                        account: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Paths;

    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn store(name: &str) -> (Paths, Store) {
        let dir = std::env::temp_dir().join(format!("gws-tickets-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::in_dir(dir);
        let store = Store::open(&paths, KEY).unwrap();
        (paths, store)
    }

    fn ticket(n: usize, kind: &str) -> Ticket {
        Ticket {
            ticket: format!("tkt_{n}"),
            kind: kind.to_owned(),
            subject: format!("subject {n}"),
        }
    }

    #[test]
    fn a_ticket_is_there_when_the_store_opens_again() {
        let (paths, s) = store("again");
        s.keep_tickets("me", &[ticket(1, "search")], true).unwrap();
        drop(s);
        let s = Store::open(&paths, KEY).unwrap();
        assert_eq!(
            s.ticket("tkt_1").unwrap(),
            Some(Kept {
                ticket: ticket(1, "search"),
                card: true,
                account: "me".to_owned()
            })
        );
        assert_eq!(
            s.ticket_for("me", "search", "subject 1")
                .unwrap()
                .as_deref(),
            Some("tkt_1")
        );
        assert_eq!(s.ticket_for("you", "search", "subject 1").unwrap(), None);
        assert_eq!(s.ticket("tkt_2").unwrap(), None);
    }

    #[test]
    fn rows_past_the_limit_go_oldest_first_and_cards_never_do() {
        let (_, s) = store("bounded");
        s.keep_tickets("me", &[ticket(0, "search")], true).unwrap();
        let rows: Vec<Ticket> = (1..=ROWS_MAX as usize + 10)
            .map(|n| ticket(n, "thread"))
            .collect();
        for some in rows.chunks(100) {
            s.keep_tickets("me", some, false).unwrap();
        }
        assert!(s.ticket("tkt_0").unwrap().is_some(), "the card's stays");
        assert_eq!(s.ticket("tkt_10").unwrap(), None, "the oldest rows went");
        assert!(s.ticket("tkt_11").unwrap().is_some());
        let kept: i64 = s
            .conn()
            .query_row("SELECT count(*) FROM tickets WHERE card = 0", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(kept, ROWS_MAX);
        // a row read again is the newest again
        s.keep_tickets("me", &[ticket(11, "thread")], false)
            .unwrap();
        s.keep_tickets("me", &[ticket(5000, "thread")], false)
            .unwrap();
        assert!(s.ticket("tkt_11").unwrap().is_some());
        assert_eq!(s.ticket("tkt_12").unwrap(), None);
    }

    #[test]
    fn a_rows_ticket_a_card_takes_is_kept_and_keeps_its_subject() {
        let (_, s) = store("pinned");
        s.keep_tickets("me", &[ticket(1, "drive_search")], false)
            .unwrap();
        s.keep_tickets("me", &[ticket(1, "drive_search")], true)
            .unwrap();
        s.keep_tickets("me", &[ticket(1, "drive_search")], false)
            .unwrap();
        assert_eq!(
            s.ticket("tkt_1").unwrap().map(|k| (k.ticket, k.card)),
            Some((ticket(1, "drive_search"), true))
        );
        // two cards can stand for one search
        let mut twin = ticket(2, "drive_search");
        twin.subject = "subject 1".to_owned();
        s.keep_tickets("me", &[twin], true).unwrap();
        assert!(s.ticket("tkt_1").unwrap().is_some() && s.ticket("tkt_2").unwrap().is_some());
    }

    #[test]
    fn a_ticket_stays_the_account_it_was_issued_to() {
        let (_, s) = store("whose");
        s.keep_tickets("me", &[ticket(1, "search")], true).unwrap();
        // the same ticket kept for another account changes nothing
        s.keep_tickets("you", &[ticket(1, "thread")], true).unwrap();
        let kept = s.ticket("tkt_1").unwrap().unwrap();
        assert_eq!(
            (kept.account.as_str(), kept.ticket.kind.as_str()),
            ("me", "search")
        );
        assert_eq!(s.ticket_for("you", "thread", "subject 1").unwrap(), None);
    }

    #[test]
    fn an_accounts_oldest_cards_go_out_of_date_and_others_keep_theirs() {
        let (_, s) = store("cards");
        s.keep_tickets("you", &[ticket(0, "event_draft")], true)
            .unwrap();
        let cards: Vec<Ticket> = (1..=CARDS_MAX as usize + 5)
            .map(|n| ticket(n, "event_draft"))
            .collect();
        for some in cards.chunks(500) {
            s.keep_tickets("me", some, true).unwrap();
        }
        let old = s.ticket("tkt_5").unwrap().unwrap();
        assert_eq!(
            (
                old.ticket.kind.as_str(),
                old.ticket.subject.as_str(),
                old.account.as_str()
            ),
            (GONE, "", "me"),
            "known, and nothing in it"
        );
        assert_eq!(
            s.ticket_for("me", "event_draft", "subject 5").unwrap(),
            None
        );
        assert_eq!(
            s.ticket("tkt_6").unwrap().unwrap().ticket.kind,
            "event_draft"
        );
        assert_eq!(
            s.ticket("tkt_0").unwrap().unwrap().ticket.kind,
            "event_draft",
            "another account's are its own"
        );
        // a gone card isn't brought back by being kept again
        s.keep_tickets("me", &[ticket(5, "event_draft")], true)
            .unwrap();
        assert_eq!(s.ticket("tkt_5").unwrap().unwrap().ticket.kind, GONE);
        let live: i64 = s
            .conn()
            .query_row(
                "SELECT count(*) FROM tickets WHERE account = 'me' AND card = 1 AND kind != 'gone'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(live, CARDS_MAX);
        // a keep says which cards went with it
        let gone = s
            .keep_tickets("me", &[ticket(9000, "event_draft")], true)
            .unwrap();
        assert_eq!(gone, ["tkt_6"]);
    }

    #[test]
    fn an_accounts_tickets_go_with_it_and_nobody_elses() {
        let (_, s) = store("forget");
        s.keep_tickets("me", &[ticket(1, "event_draft")], true)
            .unwrap();
        s.keep_tickets("you", &[ticket(2, "event_draft")], true)
            .unwrap();
        assert!(s.accounts().unwrap().contains(&"me".to_owned()));
        s.forget_account("me").unwrap();
        assert_eq!(s.ticket("tkt_1").unwrap(), None);
        assert!(s.ticket("tkt_2").unwrap().is_some());
        assert_eq!(s.accounts().unwrap(), ["you"]);
    }
}
