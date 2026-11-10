# dmnd-sv2-proxy

A small local proxy that lets the Stratum V2 Reference Implementation (SRI) mine on the
[DMND](https://dmnd.work) pool with Job Declaration, using the
[SV2 UI](https://stratumprotocol.org/#start-mining-steps) (`stratumv2/sv2-ui`) and its JD client
`jd_client_sv2` v0.9.2.

```
Bitaxe ──SV2──▶ SV2 UI (SRI JD client) ──▶ dmnd-sv2-proxy ──SV2──▶ DMND pool
SV1 miner ──SV1──▶ SV2 UI translator ──┘
```

Stock SRI cannot connect to DMND: DMND drops the connection or rejects its jobs. The proxy sits
between the SV2 UI's JD client and DMND, terminates Noise on both sides, and changes only what
DMND requires. Everything else is relayed unchanged.

| # | DMND expects | Stock SRI does | The proxy |
| --- | --- | --- | --- |
| 1 | `SetupConnection.device_id` = `<16 alphanumerics>::POOLED::<DMND token>` | Sends an empty `device_id` | Writes your token into it |
| 2 | Channel target derived from `nominal_hash_rate`, ~1.67x easier than SRI's `max_target` | Rejects it (`InitialTargetAboveMaxTarget`) | Announces `HASHRATE_SCALE` x the hashrate, relaxes `max_target` |
| 3 | Job Declaration session opened after a mining channel exists | Opens it at startup | Confirms JD setup locally, opens the real session once a channel is open |
| 4 | Coinbase output `OP_RETURN 0a "DIFF32" <f32 LE difficulty>` in every custom job | Does not add it ("Missing difficulty commit") | Appends it to the coinbase outputs in `AllocateMiningJobToken.Success` |
| 5 | Fixed channel difficulty | Vardiff sends `UpdateChannel`; the next job's DIFF32 goes stale and the JDC falls back to solo | Drops `UpdateChannel`; hides one "Missing difficulty commit" error |
| 6 | User identity like dmnd-client's `ABC<32 hex>` | Sends the UI's username | Rewrites it (may not be required) |

DMND's expectations come from the [dmnd-client](https://github.com/dmnd-pool/dmnd-client)
source; rows 1 to 5 were each confirmed against live DMND responses on 2026-11-10.

**Status:** a working workaround, not supported by DMND or the SRI project. Tested on one Linux
machine with one Bitaxe Gamma (AxeOS v2.15.4, SV2 and SV1) for minutes, with every job and share
accepted. Long-run stability, finding a block and multiple miners are untested.

## Build

Needs Rust (tested with 1.88).

```bash
git clone <this repo> ~/dmnd-sv2-proxy
cd ~/dmnd-sv2-proxy
cargo build --release
```

## Run

1. Create the Docker network the SV2 UI uses and note its gateway (the UI reuses an existing one):

   ```bash
   docker network create sv2-network
   docker network inspect sv2-network -f '{{(index .IPAM.Config 0).Gateway}}'
   ```

2. Put your DMND token (from [join.dmnd.work](https://join.dmnd.work)) in a private file:

   ```bash
   printf 'TOKEN=%s\n' 'YOUR_DMND_TOKEN' > ~/.config/dmnd.env && chmod 600 ~/.config/dmnd.env
   ```

3. Install the service: copy `dmnd-sv2-proxy.service.example` to
   `~/.config/systemd/user/dmnd-sv2-proxy.service`, replace `GATEWAY` with the address from step 1,
   then:

   ```bash
   systemctl --user daemon-reload
   systemctl --user enable --now dmnd-sv2-proxy
   journalctl --user -u dmnd-sv2-proxy -n 5
   ```

   The log prints `Proxy authority public key: 9…`. The key is created once in
   `~/.config/dmnd-sv2-proxy.key`.

4. In the SV2 UI, set the **Custom Pool** to: Address = the gateway, Pool Port = `20000`,
   JD Port = `20000`, Authority Public Key = the proxy's key.

5. Point miners at the UI: SV2 firmware (Bitaxe) to `<machine-ip>:34265` with the key shown in the
   UI's "SV2 Firmware" card; SV1 miners to `stratum+tcp://<machine-ip>:34255`.

| Setting | Default | Meaning |
| --- | --- | --- |
| `TOKEN` | required | Your DMND token |
| `LISTEN` | `172.18.0.1:20000` | Where the SV2 UI's JD client connects (the `sv2-network` gateway) |
| `UPSTREAM` | `mining.dmnd.work:20000` | DMND pool address |
| `KEY_FILE` | `~/.config/dmnd-sv2-proxy.key` | The proxy's own authority key |
| `HASHRATE_SCALE` | `2` | Multiplier on the announced hashrate |
| `RUST_LOG` | `info` | `dmnd_sv2_proxy=debug` logs every message type |

A healthy start logs `DMND accepted SetupConnection` (protocol 0), `JD setup confirmed locally`,
`OpenMiningChannel … nominal_hash_rate … -> …`, `relaying protocol 1`, then
`Added DIFF32 difficulty commitment (…)` about every 6 seconds.

## Security notes

- Your token is only read from the environment and only sent to DMND, inside the encrypted SV2
  connection. Never commit `dmnd.env` or the `.key` file.
- The proxy listens only on the Docker network gateway, not on your LAN.
- The SV2 UI's JD client uses the same built-in SV2 key on every install; keep ports 34265 and 34255
  on your LAN.
