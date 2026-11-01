# Google Workspace for Buwiz Bot

`buwiz-gws` is Buwiz Bot's first-party Google Workspace plugin,
`app.buwiz.google-workspace`: a Buwiz mini app protocol 1.0 package (protocol
1.0.0). It is a stdio MCP server with two read-only tools,
`gws_search_mail` and `gws_read_mail`. They are the mail source v1
routines read through.

It holds no Google credentials of its own. Its `plugin.json` declares one
account, `google` (scope `gmail.readonly`); Buwiz Bot signs you in to it
and pushes the access token with `buwiz.account.set` (after provision,
after sign-in and after each refresh) and takes it back with
`buwiz.account.clear`. The plugin keeps the token in memory only and calls
Gmail's REST API with it. Without a token (none pushed, expired, or Gmail
answering 401) a mail tool answers `isError` with
`{code: "account_required", account: "google"}`, and the Bot asks you in
the chat to sign in. Other failures are a plain `isError` result, never a
crash.

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
buwiz-gws mcp        # serve MCP on stdio (what the Bot's broker spawns); also the default
buwiz-gws manifest   # print the unsigned manifest
```

The plugin reads its signed manifest from `GWS_MANIFEST` (plugin.json sets
it to `${PLUGIN_ROOT}/manifest.signed.json`). Without it, it reads
`buwiz-gws.manifest.json` next to the binary.

## Build and test

```sh
cargo test --locked
cargo build --release --locked
```

The Bot builds it from a pinned commit of this repo
(`plugins/google-workspace/rev` in buwiz-bot/buwiz-bot).

## The package

`package/plugin.json` is the package's source: `mcpServers.gws` runs
`${PLUGIN_ROOT}/bin/buwiz-gws mcp`, and its `buwiz` block names the mini
app (id, short, version, protocol 1.0.0), its `google` account and the
publisher key `gc-2026-01`. The release script fills in `buwiz.files` (the
signed served manifest) and `buwiz.binaries` (one per platform), each with
its sha256, and signs the block. Keep its versions equal to `manifest.json`
and `Cargo.toml` (`cargo test` checks).

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

## Parity with the Node plugin ends

Up to 1.1.0, `buwiz-gws` answered byte for byte as the Bot's Node plugin
(`server/src/v1/plugins/google-workspace/server.mjs`) over the user's
workspace-mcp, and the Bot's GWS-06 test compared the two. 1.2.0 reads
Gmail's API with the Bot's Google account instead, so GWS-06 parity ends:
the Node plugin stays on workspace-mcp. The tools' input schemas and
result shapes are unchanged.

`src/v1` (JCS and keys) is a copy of the Bot's `buwiz-protocol` crate (formerly `rox-v1`). Keep it in
step with the Bot's, so the signed bytes match.
