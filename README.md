# Google Workspace for Buwiz Bot

`buwiz-gws` is Buwiz Bot's first-party Google Workspace plugin,
`app.buwiz.google-workspace`: a Buwiz mini app protocol 1.0 package. Its
screens are native (`crates/gws-screens`, compiled into the Bot), its tools run
in a stdio MCP server the Bot spawns (`crates/gws-plugin`), and it keeps what it
needs offline in an encrypted store (`crates/gws-store`).

It holds no Google credentials of its own. Its `plugin.json` declares one
account, `google`; Buwiz Bot signs you in to it with your own Google client,
pushes the access token with `buwiz.account.set` (after provision, after
sign-in and after each refresh) and takes it back with `buwiz.account.clear`.
The plugin keeps the token in memory only. Without a token (none pushed,
expired, or Google answering 401) a tool answers `isError` with
`{code: "account_required", account: "google"}`, and the Bot asks you in the
chat to sign in. Other failures are a plain `isError` result, never a crash.

Every tool says who may call it (`_meta["buwiz/audience"]`). The AI can read
and fill drafts. It can't send, delete, share or invite: those run only from a
person's own press.

| Crate | What |
|---|---|
| `crates/gws-plugin` | Package `buwiz-gws`: the binary, the tools, the manifest. `serve.rs` is a concurrent loop around `buwiz_plugin::Server`, which keeps doing provision, certificates, tickets and the caller checks |
| `crates/gws-screens` | The native screens. The Bot pins this crate |
| `crates/gws-store` | SQLCipher in `GWS_DATA_DIR`. Its key comes from the plugin key the Bot provisions, so none is on disk |
| `crates/gws-fake-google` | A stub Google on 127.0.0.1 with fake data, for tests and test Bots. Never in a release |

## Install and sign in

1. Ask Buwiz Bot's agent to install `https://github.com/buwiz-app/google-workspace`.
   A card in the chat lists the package (its Google account among it);
   review and install it.
2. Add your Google OAuth client once in Buwiz Bot's Setup (the shared
   Google client that Google connectors use) if you have not.
3. Open the plugin's page in Buwiz Bot, and under Accounts, sign in to
   Google. If a routine or the agent reads mail before you sign in, a
   "Sign in to Google" card shows up in the chat.

A Buwiz Bot on protocol 1.0 is needed. Earlier Bots ran pre-release
numbers of the protocol: on a 2.1 Bot install 1.2.0
(`https://github.com/buwiz-app/google-workspace/releases/tag/v1.2.0`), on a
2.0 Bot 1.1.0
(`https://github.com/buwiz-app/google-workspace/releases/tag/v1.1.0`).

## Commands

```sh
buwiz-gws mcp                    # serve MCP on stdio (what the Bot's broker spawns); also the default
buwiz-gws manifest [--key-id id] # print the unsigned manifest
```

The plugin serves the signed manifest named by `BUWIZ_PLUGIN_MANIFEST`
(plugin.json sets it to `${PLUGIN_ROOT}/manifest.signed.json`).

## Build and test

```sh
cargo test --locked
tools/bot/conformance.sh        # the Bot's conformance runner, under a throwaway key
tools/bot/live.sh up            # a real test Bot against the stub Google, on workspace 3
tools/bot/live.sh down
```

Tests use fake data and the stub. The `stub` feature is what lets a build talk
to the stub (`GWS_STUB_BASE`, loopback only). A release is built without it, and
then the variable does nothing.

## The package

`package/plugin.json` is the package's source: `mcpServers.gws` runs
`${PLUGIN_ROOT}/bin/buwiz-gws mcp`, and its `buwiz` block names the mini
app (id, short, version, protocol 1.0.0), its `google` account and the
publisher key `gc-2026-01`. The release script fills in `buwiz.files` (the
signed served manifest) and `buwiz.binaries` (one per platform), each with
its sha256, and signs the block. Keep its versions equal to
`crates/gws-plugin/Cargo.toml` (`cargo test` checks).

## Releasing

A GitHub release is what Buwiz Bot installs from. Its assets are
`plugin.json`, `manifest.signed.json` and one `buwiz-gws-<os>-<arch>`
binary per platform. `tools/release/publish.sh` makes one on the owner's
Mac, from a clean tree:

```sh
BUWIZ_BROKER_BIN=<buwiz-bot>/broker/target/release/buwiz-broker tools/release/publish.sh            # dist/release/ only
BUWIZ_BROKER_BIN=<buwiz-bot>/broker/target/release/buwiz-broker tools/release/publish.sh --publish  # and the GitHub release
```

It builds this machine's binary, signs the served manifest with
`buwiz-broker sign-manifest`, writes `plugin.json` and signs its `buwiz`
block (`sign-manifest --block buwiz`), both with `gc-2026-01` from the
login Keychain: two Keychain prompts. `--trust <trust.json>` also checks
the result as the Bot does (`buwiz-broker manifest verify` and
`manifest package`). For a local check without the real key, sign with a
throwaway seed file and a trust store that names its public key:

```sh
BUWIZ_BROKER_BIN=… tools/release/publish.sh --key-file <seed file> --trust <trust.json>
```

Add `--prerelease` to publish a release that Buwiz Bot does not install
from the repo URL yet (a coordinated switch); mark it latest on GitHub
later.

Run it with `--publish` on another platform to add that platform's binary
to the same release: it takes the release's `plugin.json`, adds the
binary, signs it again and uploads both.

`cargo test` with `BUWIZ_BROKER_BIN` set also signs the package with a
throwaway key and checks it with `buwiz-broker manifest package`.

## Licence

Apache-2.0. See `LICENSE` and `NOTICE`.
