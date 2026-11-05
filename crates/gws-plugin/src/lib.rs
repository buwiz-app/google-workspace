//! The Google Workspace plugin, `app.buwiz.google-workspace`: a stdio MCP
//! server Buwiz Bot's broker spawns. It holds no Google credentials of its
//! own: the Bot signs the person in (the `google` account its plugin.json
//! declares, protocol 1.0 §20) and pushes the access token.
//!
//! `buwiz_plugin::Server` does the protocol: provision, session
//! certificates, tickets and the caller checks. [`serve`] puts a concurrent
//! loop around it, because a slow Google call must not hold up a token push,
//! and the SDK's `Tool` has no `_meta` for the audience.

pub mod accounts;
pub mod apis;
pub mod gmail;
pub mod js;
pub mod manifest;
pub mod serve;
pub mod tools;

use std::sync::{Arc, Mutex, MutexGuard};

use buwiz_plugin::{Args, Caller, Plugin, Tool};
use gws_store::{Paths, Store};
use serde_json::Value;

use accounts::Accounts;
use apis::Apis;
use gmail::Gmail;

/// What every tool's thread shares.
pub struct Core {
    pub accounts: Accounts,
    pub gmail: Gmail,
    pub paths: Paths,
    /// The store's key, from the provisioned plugin key: memory only. Its
    /// being here is also how the loop knows provision happened: the SDK
    /// keeps its own key private.
    store_key: Mutex<Option<String>>,
    /// Opened at first use: a store that won't open must not stop mail search.
    store: Mutex<Option<Store>>,
}

impl Core {
    pub fn new(apis: &Apis, paths: Paths) -> Self {
        Self { accounts: Accounts::default(), gmail: Gmail::new(&apis.gmail), paths, store_key: Mutex::new(None), store: Mutex::new(None) }
    }

    /// `buwiz.provision`'s seed, accepted: the store can open from here on.
    pub fn unlock(&self, seed: &str) {
        *self.store_key.lock().unwrap_or_else(|e| e.into_inner()) = Some(gws_store::key_from_seed(seed));
    }

    /// Whether the Bot has provisioned this process.
    pub fn provisioned(&self) -> bool {
        self.store_key.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    /// Runs one of the plugin's tools. `args` were checked as an object.
    pub fn run(&self, tool: &str, args: &Value) -> Value {
        tools::run(&self.gmail, &self.accounts, tool, args)
    }

    /// The store, opened if it isn't yet. What it holds can be fetched again,
    /// so a store made under another plugin key is started afresh.
    pub fn store(&self) -> Result<MutexGuard<'_, Option<Store>>, gws_store::Error> {
        let mut held = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if held.is_none() {
            let key = self.store_key.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or(gws_store::Error { code: "locked", message: "The store opens after the Bot provisions the plugin.".to_owned() })?;
            *held = Some(match Store::open(&self.paths, &key) {
                Err(e) if e.code == "wrong_key" => {
                    gws_store::wipe(&self.paths)?;
                    Store::open(&self.paths, &key)?
                }
                other => other?,
            });
        }
        Ok(held)
    }

    /// `buwiz.uninstall`: every token and every file goes.
    pub fn uninstall(&self) {
        self.accounts.forget();
        *self.store.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Err(e) = gws_store::wipe(&self.paths) {
            eprintln!("buwiz-gws: uninstall: {}", e.code);
        }
    }
}

/// The plugin as the SDK sees it.
pub struct Gws {
    pub core: Arc<Core>,
}

impl Gws {
    pub fn from_env() -> Result<Self, String> {
        let paths = Paths::from_env().map_err(|e| e.message)?;
        Ok(Self { core: Arc::new(Core::new(&Apis::from_env(), paths)) })
    }
}

impl Plugin for Gws {
    const ID: &'static str = manifest::ID;
    const NAME: &'static str = "buwiz-gws";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
    const RELEASE_KEY_ID: &'static str = manifest::RELEASE_KEY_ID;
    const INSTRUCTIONS: &'static str = "Gmail is read-only here: gws_search_mail finds messages, gws_read_mail reads one. Neither sends, labels or deletes mail.";

    fn manifest(key_id: &str) -> Value {
        manifest::unsigned(key_id)
    }

    fn tools(&self) -> Vec<Tool> {
        tools::list()
    }

    fn call(&mut self, tool: &str, args: &Args, _caller: &Caller) -> Value {
        self.core.run(tool, &Value::Object(args.clone()))
    }

    fn ticket_title(&mut self, _ticket: &str) -> Option<String> {
        None
    }

    fn uninstall(&mut self) {
        self.core.uninstall();
    }
}
