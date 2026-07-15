# Party Mode — data sharing disclosure

Party mode lets Chud users in the same League lobby see each other's skin
picks in-game. It is **off by default** and never connects anywhere until you
accept this disclosure in the app (Skins → Party Mode). Revoking consent or
turning Party off disconnects immediately and clears the session.

## What is transmitted (and what is not)

When Party mode is ON and you are in a lobby with other Chud users, your
client connects to the Chud party relay and shares, with the members of your
room only:

| Field | What it is |
|---|---|
| Display name | Your Riot game name (the name teammates already see in the lobby). |
| Ephemeral session key | A random public key generated fresh each time Party is enabled — used so peers can verify your selections weren't tampered with. It is not derived from your account. |
| Skin selection | Champion id, skin id, optional chroma id. |
| Custom-mod fingerprint | If you selected a custom mod: a 16-hex-character content hash. The mod file itself is **never** uploaded — peers can only match a mod they already have locally. |
| Announcer pack id | If you selected a Library announcer: its public Library catalog id and display name. |

**Not transmitted:** your summoner ID, PUUID, account id, rank, region,
match history, IP-visible-to-peers (peers only ever talk to the relay, never
to each other directly), files, or anything outside the table above.

The relay assigns every connection a random member id; clients cannot claim
an identity, and selections are signed by your ephemeral session key and
bound to the room's epoch so they can't be forged or replayed.

## Who processes it

The relay is a Cloudflare Worker (Durable Object) operated by the Chud
developer, running on Cloudflare's network (Cloudflare, Inc. acts as the
hosting processor). Traffic is TLS-encrypted in transit with a
Cloudflare-issued certificate.

## Retention and deletion

The relay keeps **no database**. Room state lives only in the memory
attached to the live WebSocket connections; when you disconnect (leave,
disable Party, close Chud), your entry disappears from the room immediately,
and a room with no connections ceases to exist. Nothing is logged
server-side beyond Cloudflare's standard operational metrics. There is
nothing to delete after disconnect — disconnecting *is* deletion.

## Inbound protections on your machine

- Peer selections are only injected when the claimed champion is actually in
  your live champ-select roster (verified against your own League client).
- Peer-advertised announcer packs are **not** downloaded unless you also
  enable "auto-download peer announcers", and even then the pack id must
  exist in the Chud Library catalog — the peer's free-text name is never
  trusted for filenames or downloads.
- All relay messages are size-, schema-, rate-, and member-limited
  server-side.

## Consent versioning

This disclosure is versioned. If it materially changes, Party mode disables
itself until you review and accept the new version.
