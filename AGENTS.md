# AGENTS.md

Instructions for AI coding agents (Claude Code, Codex, Cursor, etc.) working in this repo.
Read `README.md` for the user-facing overview; this file covers how to work on the code safely.

## What this is

A single-binary Rust proxy that sits between the SRI SV2 UI's JD client (`jd_client_sv2` v0.9.2)
and the DMND pool. It terminates Noise on both sides and relays raw SV2 frames, rewriting only the
few messages DMND needs. Everything else must pass through byte-for-byte.

```
SV2 UI JD client ──Noise (proxy's key)──▶ dmnd-sv2-proxy ──Noise (DMND's key)──▶ DMND :20000
```

## Layout

- `src/main.rs` — the whole proxy (~450 lines). The header doc comment lists the six DMND fixes;
  keep it in sync with the code and with the table in `README.md`.
- `Cargo.toml` — `stratum-apps` is pinned to the sv2-apps git tag `v0.9.2` to match the SV2 UI's
  JD client. Do not bump it unless the SV2 UI's pinned JDC image changes too.
- `dmnd-sv2-proxy.service.example` — generic systemd user unit (`GATEWAY` placeholder).

## Commands

```bash
cargo build --release          # binary: target/release/dmnd-sv2-proxy
cargo fmt && cargo clippy      # before committing
TOKEN=... RUST_LOG=dmnd_sv2_proxy=debug ./target/release/dmnd-sv2-proxy   # logs every message type
```

There are no automated tests. The only real test is live mining: run the proxy, point the SV2 UI
at it and check the log (see "Verifying a change").

## How the code works

- `main` loads/creates the proxy's authority key (`KEY_FILE`), binds `LISTEN`, and spawns
  `handle` per downstream connection. The JD client opens two connections: mining (protocol 0)
  and Job Declaration (protocol 1).
- `handle` reads the first frame (must be `SetupConnection`), rewrites `device_id` with the token,
  connects upstream, then runs two loops: `to_up` (downstream → DMND) and `to_down` (DMND →
  downstream). Either side ending closes both.
- Process-wide shared state, passed into every `handle`:
  - `channel_open` (`watch` channel): true once DMND sends `OpenExtended/StandardMiningChannel.Success`.
    The JD connection and `AllocateMiningJobToken` wait on it.
  - `target_store`: the current channel target (from `Open*Channel.Success` or `SetTarget`),
    used to compute the DIFF32 commitment.
  - `user_id`: one `ABC<32 hex>` identity per process.
  This assumes one JD client (one mining channel) per proxy. Supporting several would need
  per-channel state.
- Frames are handled as raw bytes: 6-byte header = `extension_type` u16 LE (bit 15 = channel_msg),
  `msg_type` u8, `msg_length` u24 LE. Message type constants are at the top of `main.rs`.
  `replace_str_field` and `add_difficulty_commitment` must fix up `msg_length` when they change
  the payload size.

## DMND rules the code depends on (do not "simplify" these away)

Each was confirmed against live DMND responses; removing one breaks mining.

1. `SetupConnection.device_id` = `<16 alphanumerics>::POOLED::<token>`, or DMND closes the connection.
2. `nominal_hash_rate` × `HASHRATE_SCALE` (default 2) and relaxed `max_target` in `Open*Channel`,
   or the JDC fails with `InitialTargetAboveMaxTarget`.
3. JD `SetupConnection` is answered locally and the real JD connection is deferred until a mining
   channel is open; `AllocateMiningJobToken` is held until then. DMND silently ignores earlier ones.
4. `AllocateMiningJobToken.Success` gets an extra zero-value coinbase output
   `OP_RETURN 0a "DIFF32" <f32 LE difficulty>`, or custom jobs fail with "Missing difficulty commit".
5. `UpdateChannel` (0x16) from the JDC's vardiff is dropped, and one "difficulty commit"
   `SetCustomMiningJob.Error` is hidden, or the JDC falls back to solo mining permanently.
6. User identity is rewritten to `ABC<32 hex>` (dmnd-client's format; may not be required).

The reference for DMND's behaviour is the dmnd-client source
(https://github.com/dmnd-pool/dmnd-client, branch `master`). Check it before changing any of the above.

## Rules

- **Secrets:** the DMND token comes only from the `TOKEN` env var and must never be logged, printed,
  written to disk or committed. Never commit `dmnd.env`, `*.key`, `*.log`, `*.json` or the
  machine-specific `dmnd-sv2-proxy.service` (all in `.gitignore`). Check `git diff --cached` for
  tokens, local paths, IPs and wallet addresses before committing.
- **Pass-through by default:** only touch the specific message types listed above. Any new rewrite
  needs a comment explaining which DMND behaviour it works around.
- **Don't change defaults casually:** `LISTEN` defaults to the Docker `sv2-network` gateway so the
  proxy isn't exposed on the LAN; `DMND_AUTHORITY` is DMND's mainnet key from dmnd-client.
- **Keep docs in sync:** if behaviour, env vars or log lines change, update the header comment in
  `main.rs`, `README.md` and the service example.
- Don't restart or modify a user's running miners, pool settings or other mining services
  (e.g. dmnd-client) unless explicitly asked.

## Verifying a change

Build, restart the proxy (`systemctl --user restart dmnd-sv2-proxy` if installed as a service),
then watch `journalctl --user -u dmnd-sv2-proxy -f`. A healthy run logs (the mining and JD connections interleave):

1. `Proxy authority public key: …` and `Listening on …`
2. `JD setup confirmed locally; waiting for a mining channel …`
3. `relaying protocol 0 …` and `DMND accepted SetupConnection`
4. `OpenMiningChannel … nominal_hash_rate … -> …`
5. `relaying protocol 1 …`, then `Added DIFF32 difficulty commitment (…)` every few seconds

Then let it run at least 5 minutes: there should be no `DMND rejected custom job` warnings and the
SV2 UI should keep showing the DMND pool (not solo) with accepted shares.
