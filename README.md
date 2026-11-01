# Google Workspace for Buwiz Bot

`buwiz-gws` is Buwiz Bot's first-party Google Workspace plugin,
`app.buwiz.google-workspace` (mini app protocol 2.0.0). It is a stdio MCP
server with two read-only tools, `gws_search_mail` and `gws_read_mail`.
They are the mail source v1 routines read through.

It holds no Google credentials of its own. It proxies to the user's own
Google Workspace MCP server (workspace-mcp),
which it starts on the first mail call. Two values from the install
record's environment configure it:
- `GWS_UPSTREAM`: the upstream's command, as compact JSON;
- `GWS_USER_EMAIL`: the account.

A missing or failing upstream gives a plain `isError` result, never a
crash.

## Commands

```sh
buwiz-gws mcp        # serve MCP on stdio (what the Bot's broker spawns); also the default
buwiz-gws manifest   # print the unsigned manifest
```

The plugin reads its signed manifest from `GWS_MANIFEST`. Without it, it
reads `buwiz-gws.manifest.json` next to the binary.

## Build and test

```sh
cargo test --locked
cargo build --release --locked
```

The Bot builds it from a pinned commit of this repo
(`plugins/google-workspace/rev` in buwiz-bot/buwiz-bot).

## Signing the manifest

The Bot's broker refuses an unsigned manifest. The publisher key
`gc-2026-01` lives in the owner's login Keychain. The Bot's
`scripts/gws-install-wizard` signs `buwiz-gws manifest` with
`rox-broker sign-manifest`. A release takes the signed manifest from the
`GWS_MANIFEST_JSON` secret.

## Parity with the Node plugin

`buwiz-gws` answers byte for byte as the Bot's Node plugin
(`server/src/v1/plugins/google-workspace/server.mjs`), which it replaces.
To check it, run the Bot's `server/test/v1-gws.test.mjs` with `GWS_CMD`
set to this binary. GWS-06 compares the two.

`src/v1` (JCS and keys) and `src/mcp.rs` (the stdio MCP client) are copies
of the Bot's `rox-v1` and `rox-mcp` crates. Keep them in step with the
Bot's, so the signed bytes match.
