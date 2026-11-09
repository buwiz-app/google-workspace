//! The senders whose web pictures a person always wants shown, for each
//! account. Only a person's press adds or removes one, and a sign-out takes
//! them with the rest of the account.

use rusqlite::{OptionalExtension as _, params};

use crate::{Result, Store};

impl Store {
    /// `address` (lower case) shows its pictures from now on.
    pub fn trust_sender(&self, account: &str, address: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO picture_senders(account, address, added) VALUES(?, ?, ?) ON CONFLICT(account, address) DO NOTHING",
            params![account, address, now],
        )?;
        Ok(())
    }

    /// `address` goes back to asking first.
    pub fn distrust_sender(&self, account: &str, address: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM picture_senders WHERE account = ? AND address = ?",
            params![account, address],
        )?;
        Ok(())
    }

    pub fn sender_trusted(&self, account: &str, address: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM picture_senders WHERE account = ? AND address = ?",
                params![account, address],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}

#[cfg(test)]
mod tests {
    use crate::{Paths, Store, key_from_seed};

    #[test]
    fn a_trusted_sender_survives_a_reopen_and_goes_with_its_account() {
        let dir = std::env::temp_dir().join(format!("gws-store-senders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (paths, key) = (Paths::in_dir(&dir), key_from_seed("senders"));
        {
            let s = Store::open(&paths, &key).unwrap();
            s.trust_sender("me@example.com", "news@digest.example", 1)
                .unwrap();
            s.trust_sender("me@example.com", "news@digest.example", 2)
                .unwrap();
            s.trust_sender("other@example.com", "ana@example.com", 1)
                .unwrap();
        }
        let s = Store::open(&paths, &key).unwrap();
        assert!(
            s.sender_trusted("me@example.com", "news@digest.example")
                .unwrap()
        );
        assert!(
            !s.sender_trusted("other@example.com", "news@digest.example")
                .unwrap(),
            "each account has its own list"
        );
        assert!(s.accounts().unwrap().contains(&"me@example.com".to_owned()));
        s.forget_account("me@example.com").unwrap();
        assert!(
            !s.sender_trusted("me@example.com", "news@digest.example")
                .unwrap()
        );
        assert!(
            s.sender_trusted("other@example.com", "ana@example.com")
                .unwrap()
        );
        s.distrust_sender("other@example.com", "ana@example.com")
            .unwrap();
        assert!(
            !s.sender_trusted("other@example.com", "ana@example.com")
                .unwrap()
        );
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
