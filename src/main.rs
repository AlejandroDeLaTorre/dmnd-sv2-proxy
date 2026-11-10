//! dmnd-sv2-proxy: lets a stock SRI SV2 client (e.g. the sv2-ui JD client) mine on the DMND pool.
//!
//! It terminates Noise from the downstream client with its own authority key, opens Noise to DMND
//! (verifying DMND's authority key) and relays SV2 frames, changing only what DMND requires:
//!  1. device_id in SetupConnection = `<16 random alphanumerics>::POOLED::<DMND token>`
//!     (DMND's login; SRI sends it empty).
//!  2. nominal_hash_rate in OpenMiningChannel x HASHRATE_SCALE, max_target relaxed, so DMND's
//!     channel target is one the SRI JDC accepts.
//!  3. The Job Declaration session is confirmed locally and only opened at DMND once a mining
//!     channel exists (DMND ignores token requests on an earlier JD session).
//!  4. A zero-value `OP_RETURN 0a "DIFF32" <f32 difficulty>` output is appended to the coinbase
//!     outputs of AllocateMiningJobToken.Success (DMND's "difficulty commit").
//!  5. UpdateChannel is dropped (fixed DMND difficulty), and one "Missing difficulty commit"
//!     SetCustomMiningJob.Error is hidden from the JDC.
//!  6. user_identity / user_identifier are rewritten to dmnd-client's `ABC<32 hex>` shape.
//!
//! Env: TOKEN (DMND token, required), LISTEN (default 172.18.0.1:20000),
//! UPSTREAM (default mining.dmnd.work:20000), KEY_FILE (default ~/.config/dmnd-sv2-proxy.key),
//! HASHRATE_SCALE (default 2), RUST_LOG (default info).

use std::{env, fs, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};

use rand::{distributions::Alphanumeric, Rng};
use stratum_apps::{
    key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
    network_helpers::{accept_noise_connection, connect_with_noise},
    stratum_core::framing_sv2::framing::SerializedFrame,
};
use secp256k1::SecretKey;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
};
use tracing::{debug, error, info, warn};

/// DMND mainnet pool authority key (dmnd-client `MAIN_AUTH_PUB_KEY`).
const DMND_AUTHORITY: &str = "9c44K6QVizyPWb9xfeqhckFRosxWwB3EfytGa4CfTdD526qb2QV";
const CERT_VALIDITY_SECS: u64 = 3600;
const HEADER_SIZE: usize = 6;
const MSG_SETUP_CONNECTION: u8 = 0x00;
const MSG_SETUP_CONNECTION_SUCCESS: u8 = 0x01;
const MSG_SETUP_CONNECTION_ERROR: u8 = 0x02;
const MSG_OPEN_STANDARD_CHANNEL: u8 = 0x10;
const PROTOCOL_JOB_DECLARATION: u8 = 1;
const MSG_OPEN_EXTENDED_CHANNEL: u8 = 0x13;
const MSG_OPEN_STANDARD_CHANNEL_SUCCESS: u8 = 0x11;
const MSG_OPEN_EXTENDED_CHANNEL_SUCCESS: u8 = 0x14;
const MSG_ALLOCATE_MINING_JOB_TOKEN: u8 = 0x50;
const MSG_ALLOCATE_MINING_JOB_TOKEN_SUCCESS: u8 = 0x51;
const MSG_SET_TARGET: u8 = 0x21;
const MSG_UPDATE_CHANNEL: u8 = 0x16;
const MSG_SET_CUSTOM_MINING_JOB_ERROR: u8 = 0x24;

type TargetStore = Arc<std::sync::Mutex<Option<[u8; 32]>>>;

fn load_or_create_key() -> (Secp256k1PublicKey, Secp256k1SecretKey) {
    let path = env::var("KEY_FILE").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(env::var("HOME").expect("HOME")).join(".config/dmnd-sv2-proxy.key")
    });
    let secret: Secp256k1SecretKey = match fs::read_to_string(&path) {
        Ok(s) => s.trim().parse().expect("invalid key file"),
        Err(_) => {
            let sk = Secp256k1SecretKey(SecretKey::new(&mut rand::thread_rng()));
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .expect("cannot create key file");
            writeln!(f, "{sk}").unwrap();
            info!("Created new proxy authority key at {}", path.display());
            sk
        }
    };
    (Secp256k1PublicKey::from(secret.clone()), secret)
}

/// Splits an SV2 STR0_255 at `off`; returns (start, end) of the whole field incl. length byte.
fn str0_255(buf: &[u8], off: usize) -> Option<(usize, usize)> {
    let len = *buf.get(off)? as usize;
    (off + 1 + len <= buf.len()).then_some((off, off + 1 + len))
}

/// Rebuilds a SetupConnection frame with `device_id` replaced. Returns None if it doesn't parse.
fn rewrite_setup_connection(frame: &[u8], device_id: &str) -> Option<Vec<u8>> {
    let payload = frame.get(HEADER_SIZE..)?;
    // protocol u8, min u16, max u16, flags u32
    let mut off = 1 + 2 + 2 + 4;
    let (_, e) = str0_255(payload, off)?; // endpoint_host
    off = e + 2; // endpoint_port u16
    for _ in 0..3 {
        let (_, e) = str0_255(payload, off)?; // vendor, hardware_version, firmware
        off = e;
    }
    let (dev_start, dev_end) = str0_255(payload, off)?;
    if dev_end != payload.len() || device_id.len() > 255 {
        return None;
    }
    let protocol = payload[0];
    let mut new_payload = payload[..dev_start].to_vec();
    new_payload.push(device_id.len() as u8);
    new_payload.extend_from_slice(device_id.as_bytes());
    let len = new_payload.len() as u32;
    let mut out = frame[..3].to_vec(); // ext_type u16 + msg_type u8
    out.extend_from_slice(&len.to_le_bytes()[..3]);
    out.extend_from_slice(&new_payload);
    info!("Rewrote SetupConnection (protocol {protocol}) device_id");
    Some(out)
}

/// DMND derives a channel's target from `nominal_hash_rate` at fewer shares/minute than SRI's
/// default 6, so the target it returns is ~1.67x easier than the max_target the SRI JDC computed,
/// and the JDC aborts with InitialTargetAboveMaxTarget (it checks against its own stored value).
/// Announcing a higher nominal_hash_rate (HASHRATE_SCALE, default 2) makes DMND pick a harder
/// target, which the JDC accepts; shares are still credited at DMND's own target.
/// Also raises max_target to the maximum so DMND never refuses on that side.
/// Layout: request_id u32, user_identity STR0_255, nominal_hash_rate f32, max_target U256.
fn adjust_open_channel(frame: &mut [u8], scale: f32) {
    let Some(len) = frame.get(HEADER_SIZE + 4).map(|l| *l as usize) else { return };
    let hr = HEADER_SIZE + 4 + 1 + len;
    let Some(hr_bytes) = frame.get(hr..hr + 4) else { return };
    let nominal = f32::from_le_bytes(hr_bytes.try_into().unwrap());
    let scaled = nominal * scale;
    frame[hr..hr + 4].copy_from_slice(&scaled.to_le_bytes());
    if let Some(t) = frame.get_mut(hr + 4..hr + 36) {
        t.fill(0xff);
    }
    info!(
        "OpenMiningChannel (msg 0x{:02x}): nominal_hash_rate {nominal:.3e} -> {scaled:.3e}, max_target relaxed",
        frame[2]
    );
}

/// DMND rejects custom jobs ("Missing difficulty commit") unless the coinbase carries a zero-value
/// `OP_RETURN 0a "DIFF32" <f32 LE channel difficulty>` output (dmnd-client
/// `mining_upstream/upstream.rs` `apply_difficulty_commitment`). SRI's JDC builds its coinbase from
/// the outputs in AllocateMiningJobToken.Success, so append the commitment there.
/// Payload: request_id u32, mining_job_token B0_255, coinbase_outputs B0_64K (Vec<TxOut>).
fn add_difficulty_commitment(frame: Vec<u8>, target_le: [u8; 32]) -> Vec<u8> {
    use stratum_apps::stratum_core::bitcoin::{
        consensus::{deserialize, serialize},
        Amount, ScriptBuf, Target, TxOut,
    };
    let p = HEADER_SIZE;
    let Some(&tok_len) = frame.get(p + 4) else { return frame };
    let outs_len_at = p + 4 + 1 + tok_len as usize;
    let Some(len_bytes) = frame.get(outs_len_at..outs_len_at + 2) else { return frame };
    let outs_len = u16::from_le_bytes([len_bytes[0], len_bytes[1]]) as usize;
    let outs_at = outs_len_at + 2;
    let Some(outs_bytes) = frame.get(outs_at..outs_at + outs_len) else { return frame };
    let Ok(mut outputs) = deserialize::<Vec<TxOut>>(outs_bytes) else {
        warn!("could not decode coinbase_outputs in AllocateMiningJobToken.Success");
        return frame;
    };
    let difficulty = Target::from_le_bytes(target_le).difficulty_float() as f32;
    let mut script = b"\x6a\x0aDIFF32".to_vec();
    script.extend_from_slice(&difficulty.to_le_bytes());
    outputs.retain(|o| {
        let s = o.script_pubkey.as_bytes();
        !(s.first() == Some(&0x6a) && s.get(2..8) == Some(b"DIFF32".as_slice()))
    });
    outputs.push(TxOut {
        value: Amount::ZERO,
        script_pubkey: ScriptBuf::from_bytes(script),
    });
    let new_outs = serialize(&outputs);
    let mut out = frame[..outs_len_at].to_vec();
    out.extend_from_slice(&(new_outs.len() as u16).to_le_bytes());
    out.extend_from_slice(&new_outs);
    out.extend_from_slice(&frame[outs_at + outs_len..]);
    let payload_len = (out.len() - HEADER_SIZE) as u32;
    out[3..6].copy_from_slice(&payload_len.to_le_bytes()[..3]);
    info!("Added DIFF32 difficulty commitment ({difficulty}) to token coinbase outputs");
    out
}

/// Replaces the STR0_255 that starts `str_off` bytes into the payload with `value` and fixes the
/// frame's payload length. Returns the original frame unchanged if it doesn't parse.
fn replace_str_field(frame: Vec<u8>, str_off: usize, value: &str) -> Vec<u8> {
    let start = HEADER_SIZE + str_off;
    let Some(&len) = frame.get(start) else { return frame };
    let end = start + 1 + len as usize;
    if end > frame.len() || value.len() > 255 {
        return frame;
    }
    let mut out = frame[..start].to_vec();
    out.push(value.len() as u8);
    out.extend_from_slice(value.as_bytes());
    out.extend_from_slice(&frame[end..]);
    let payload_len = (out.len() - HEADER_SIZE) as u32;
    out[3..6].copy_from_slice(&payload_len.to_le_bytes()[..3]);
    out
}

fn describe_upstream_reply(bytes: &[u8]) {
    match bytes.get(2) {
        Some(&MSG_SETUP_CONNECTION_SUCCESS) => info!("DMND accepted SetupConnection"),
        Some(&MSG_SETUP_CONNECTION_ERROR) => {
            let code = bytes
                .get(HEADER_SIZE + 4..)
                .and_then(|p| str0_255(p, 0).map(|(s, e)| String::from_utf8_lossy(&p[s + 1..e]).into_owned()))
                .unwrap_or_default();
            warn!("DMND rejected SetupConnection: {code}");
        }
        Some(t) => info!("First upstream message type 0x{t:02x}"),
        None => {}
    }
}

async fn handle(
    downstream: TcpStream,
    pk: Secp256k1PublicKey,
    sk: Secp256k1SecretKey,
    upstream_addr: String,
    token: String,
    channel_open: Arc<watch::Sender<bool>>,
    user_id: String,
    target_store: TargetStore,
) {
    let peer = downstream.peer_addr().ok();
    let down = match accept_noise_connection(downstream, pk, sk, CERT_VALIDITY_SECS).await {
        Ok(s) => s,
        Err(e) => return warn!("{peer:?}: downstream handshake failed: {e:?}"),
    };
    let (mut down_r, mut down_w) = down.into_split();

    // First downstream frame must be SetupConnection; add the DMND token to device_id.
    let setup = match down_r.read_frame().await {
        Ok(f) => f.as_bytes().to_vec(),
        Err(e) => return warn!("{peer:?}: no SetupConnection: {e:?}"),
    };
    if setup.get(2) != Some(&MSG_SETUP_CONNECTION) || setup[..2] != [0, 0] {
        return warn!("{peer:?}: first message is not SetupConnection");
    }
    let protocol = setup.get(HEADER_SIZE).copied().unwrap_or(0xff);
    let rand16: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(16)
        .map(char::from)
        .collect();
    let Some(setup) = rewrite_setup_connection(&setup, &format!("{rand16}::POOLED::{token}")) else {
        return warn!("{peer:?}: could not parse SetupConnection");
    };

    // dmnd-client only connects its Job Declaration session after its mining channel is open, and
    // DMND ignores token requests on a JD session opened earlier. SRI's JDC opens both at startup
    // and won't start its downstream server until JD setup succeeds, so confirm JD setup locally
    // and defer the real upstream JD connection until a mining channel is open.
    let mut deferred_jd = false;
    if protocol == PROTOCOL_JOB_DECLARATION && !*channel_open.borrow() {
        let ok = vec![0, 0, MSG_SETUP_CONNECTION_SUCCESS, 6, 0, 0, 2, 0, 1, 0, 0, 0];
        if down_w.write_frame(SerializedFrame::from_bytes(ok).unwrap()).await.is_err() {
            return;
        }
        info!("{peer:?}: JD setup confirmed locally; waiting for a mining channel before connecting to DMND");
        let mut rx = channel_open.subscribe();
        if rx.wait_for(|open| *open).await.is_err() {
            return;
        }
        deferred_jd = true;
    }

    let up_tcp = match TcpStream::connect(&upstream_addr).await {
        Ok(s) => s,
        Err(e) => return error!("{peer:?}: cannot reach {upstream_addr}: {e}"),
    };
    let authority: Secp256k1PublicKey = DMND_AUTHORITY.parse().unwrap();
    let up = match connect_with_noise(up_tcp, Some(authority)).await {
        Ok(s) => s,
        Err(e) => return error!("{peer:?}: upstream handshake failed: {e:?}"),
    };
    let (mut up_r, mut up_w) = up.into_split();
    if up_w.write_frame(SerializedFrame::from_bytes(setup).unwrap()).await.is_err() {
        return error!("{peer:?}: failed to send SetupConnection upstream");
    }
    info!("{peer:?}: relaying protocol {protocol} to {upstream_addr} (device_id rewritten)");
    if deferred_jd {
        // The downstream already got a SetupConnection.Success from us; consume DMND's.
        match up_r.read_frame().await {
            Ok(f) => {
                let b = f.as_bytes().to_vec();
                describe_upstream_reply(&b);
                if b.get(2) != Some(&MSG_SETUP_CONNECTION_SUCCESS) {
                    return;
                }
            }
            Err(e) => return error!("{peer:?}: DMND closed deferred JD setup: {e:?}"),
        }
    }

    let hashrate_scale: f32 = env::var("HASHRATE_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2.0);
    let mut open_rx = channel_open.subscribe();
    let open_tx = channel_open.clone();
    let opened_here = Arc::new(AtomicBool::new(false));
    let opened_flag = opened_here.clone();
    let to_up = async move {
        loop {
            let frame = down_r.read_frame().await?;
            let bytes = frame.as_bytes().to_vec();
            // DMND ignores AllocateMiningJobToken sent before a mining channel is open (dmnd-client
            // always has its channel open first); hold token requests until one is.
            if bytes.get(2) == Some(&MSG_ALLOCATE_MINING_JOB_TOKEN) && bytes[..2] == [0, 0] && !*open_rx.borrow() {
                info!("{peer:?}: holding AllocateMiningJobToken until a mining channel is open");
                let _ = open_rx.wait_for(|open| *open).await;
                info!("{peer:?}: mining channel open, releasing AllocateMiningJobToken");
            }
            // SRI's JDC vardiff sends UpdateChannel; DMND answers with SetTarget, which invalidates
            // the DIFF32 commitment of jobs built from already-granted tokens, and the next custom
            // job is rejected. dmnd-client never updates its upstream channel, so drop these.
            if bytes[..2] == [0x00, 0x80] || bytes[..2] == [0, 0] {
                if bytes[2] == MSG_UPDATE_CHANNEL {
                    debug!("{peer:?}: dropped UpdateChannel (keeps DMND channel difficulty fixed)");
                    continue;
                }
            }
            let out = if matches!(bytes.get(2), Some(&MSG_OPEN_STANDARD_CHANNEL) | Some(&MSG_OPEN_EXTENDED_CHANNEL)) {
                // user_identity follows request_id (u32); use dmnd-client's identity format.
                let mut b = replace_str_field(bytes, 4, &user_id);
                adjust_open_channel(&mut b, hashrate_scale);
                b
            } else if bytes.get(2) == Some(&MSG_ALLOCATE_MINING_JOB_TOKEN) && bytes[..2] == [0, 0] {
                // user_identifier is the first field.
                info!("{peer:?}: AllocateMiningJobToken user_identifier -> {user_id}");
                replace_str_field(bytes, 0, &user_id)
            } else {
                bytes
            };
            debug!("{peer:?} -> pool  ext=0x{:04x} msg=0x{:02x} len={}", u16::from_le_bytes([out[0], out[1]]), out[2], out.len() - HEADER_SIZE);
            let f = SerializedFrame::from_bytes(out).map_err(|_| stratum_apps::network_helpers::Error::SocketClosed)?;
            up_w.write_frame(f).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), stratum_apps::network_helpers::Error>(())
    };
    let to_down = async move {
        let mut first = true;
        loop {
            let frame = up_r.read_frame().await?;
            let bytes = frame.as_bytes().to_vec();
            if first {
                describe_upstream_reply(&bytes);
                first = false;
            }
            let ext = u16::from_le_bytes([bytes[0], bytes[1]]) & 0x7fff;
            let mut bytes = bytes;
            if ext == 0 && matches!(bytes[2], MSG_OPEN_STANDARD_CHANNEL_SUCCESS | MSG_OPEN_EXTENDED_CHANNEL_SUCCESS) {
                // request_id u32, channel_id u32, target U256
                if let Some(t) = bytes.get(HEADER_SIZE + 8..HEADER_SIZE + 40) {
                    *target_store.lock().unwrap() = Some(t.try_into().unwrap());
                }
                opened_flag.store(true, Ordering::Relaxed);
                open_tx.send_replace(true);
            } else if ext == 0 && bytes[2] == MSG_SET_TARGET {
                // channel_id u32, maximum_target U256
                if let Some(t) = bytes.get(HEADER_SIZE + 4..HEADER_SIZE + 36) {
                    *target_store.lock().unwrap() = Some(t.try_into().unwrap());
                    info!("{peer:?}: DMND changed channel target");
                }
            } else if ext == 0 && bytes[2] == MSG_SET_CUSTOM_MINING_JOB_ERROR {
                // channel_id u32, request_id u32, error_code STR0_255
                let code = bytes
                    .get(HEADER_SIZE + 8..)
                    .and_then(|p| str0_255(p, 0).map(|(s, e)| String::from_utf8_lossy(&p[s + 1..e]).into_owned()))
                    .unwrap_or_default();
                if code.contains("difficulty commit") {
                    // A job built from a token granted before a difficulty change. SRI's JDC would
                    // fall back to solo for good; the next job (next template) has the right
                    // commitment, so hide this one rejection from it.
                    warn!("{peer:?}: DMND rejected a job with a stale difficulty commit; not forwarding the error");
                    continue;
                }
                warn!("{peer:?}: DMND rejected custom job: {code}");
            } else if ext == 0 && bytes[2] == MSG_ALLOCATE_MINING_JOB_TOKEN_SUCCESS {
                let target = *target_store.lock().unwrap();
                match target {
                    Some(t) => bytes = add_difficulty_commitment(bytes, t),
                    None => warn!("{peer:?}: token granted before any channel target is known"),
                }
            }
            debug!("{peer:?} <- pool  ext=0x{:04x} msg=0x{:02x} len={}", u16::from_le_bytes([bytes[0], bytes[1]]), bytes[2], bytes.len() - HEADER_SIZE);
            let f = SerializedFrame::from_bytes(bytes).map_err(|_| stratum_apps::network_helpers::Error::SocketClosed)?;
            down_w.write_frame(f).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), stratum_apps::network_helpers::Error>(())
    };
    tokio::select! {
        r = to_up => info!("{peer:?}: downstream side ended: {r:?}"),
        r = to_down => info!("{peer:?}: upstream side ended: {r:?}"),
    }
    if opened_here.load(Ordering::Relaxed) {
        channel_open.send_replace(false);
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .init();
    let token = env::var("TOKEN").expect("TOKEN (DMND token) must be set");
    let listen = env::var("LISTEN").unwrap_or_else(|_| "172.18.0.1:20000".into());
    let upstream = env::var("UPSTREAM").unwrap_or_else(|_| "mining.dmnd.work:20000".into());
    let (pk, sk) = load_or_create_key();
    info!("Proxy authority public key: {pk}");
    let listener = TcpListener::bind(&listen).await.expect("bind failed");
    info!("Listening on {listen}, upstream {upstream}");
    let channel_open = Arc::new(watch::Sender::new(false));
    // Same shape as dmnd-client's process-wide USER_IDENTIFIER: "ABC" + 16 random bytes as hex.
    let user_id = format!(
        "ABC{}",
        rand::thread_rng()
            .gen::<[u8; 16]>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    info!("Using user identity {user_id}");
    let target_store: TargetStore = Arc::new(std::sync::Mutex::new(None));
    loop {
        let (sock, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("accept failed: {e}");
                continue;
            }
        };
        tokio::spawn(handle(
            sock,
            pk.clone(),
            sk.clone(),
            upstream.clone(),
            token.clone(),
            channel_open.clone(),
            user_id.clone(),
            target_store.clone(),
        ));
    }
}
