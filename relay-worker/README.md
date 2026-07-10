# chud-party-relay

Cloudflare Worker (Rust, `workers-rs`) hosting the Chud Skins party-mode relay rooms.
One Durable Object per room key; members exchange skin selections through a shared
WebSocket room. See `docs/SKINS_PORT.md` §party for the wire contract.

## Deploy

```
cd relay-worker
npx wrangler deploy
```

Requires: Rust `wasm32-unknown-unknown` target, `worker-build` (the build command
installs it if missing), and a logged-in wrangler (`npx wrangler login`).

The deployed URL (e.g. `https://chud-party-relay.<account>.workers.dev`) goes into the
app config as `party_relay_url` (or env `CHUD_RELAY_URL`).

## Local dev

```
npx wrangler dev
```
