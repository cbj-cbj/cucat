//! Multiplayer transport for KuroCat.
//!
//! Two friends run KuroCat; each one's cat is mirrored onto the other's desktop.
//! Only normalized input *geometry* travels over the wire (virtual-key codes and
//! normalized cursor coordinates), never typed characters or text.
//!
//! Wire format: newline-delimited JSON over plain TCP.
//!
//! Client -> relay:
//!   {"t":"join","room":"ABCD","name":"Kuro","v":1}
//!   {"t":"msg","d":{...}}        forwarded verbatim to the other room member
//!   {"t":"ping"} / {"t":"leave"}
//!
//! Relay -> client:
//!   {"t":"joined","room":"ABCD","count":1,"you":"Kuro"}
//!   {"t":"peer","event":"join"|"leave","name":"...","count":2}
//!   {"t":"msg","d":{...},"from":"..."}
//!   {"t":"pong"} / {"t":"error","msg":"..."}
//!
//! The same relay code powers the public relay server (see `relay/kurocat-relay.mjs`
//! for the Node equivalent used on a VPS) and the built-in "host a LAN room" mode,
//! where the app binds a port and then dials itself over loopback.

use crate::input_hook::InputEvent;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// Event emitted on the peer window for every remote input event.
pub const EVENT_INPUT: &str = "kuro-peer-input";
/// Event carrying the friend's visual settings (skin / layout / scale / mode).
pub const EVENT_STATE: &str = "kuro-peer-state";
/// Connection status: idle / connecting / connected / error.
pub const EVENT_STATUS: &str = "kuro-peer-status";
/// Room membership changes (someone joined or left).
pub const EVENT_PRESENCE: &str = "kuro-peer-presence";

/// Default port used by the built-in relay when hosting a LAN room.
pub const DEFAULT_PORT: u16 = 27422;
/// A room holds exactly two cats: you and your friend.
const MAX_ROOM_PEERS: usize = 2;
/// Unique per-connection id. Display names are not unique (both sides often keep
/// the default), so room fan-out must key on the connection, not the name.
static RELAY_CONN_SEQ: AtomicU64 = AtomicU64::new(1);
/// Per-connection outgoing queue. Overflow drops the oldest frame instead of
/// blocking the global input hook thread.
const WRITER_QUEUE: usize = 512;
/// Hard cap for a single inbound frame (guards against a misbehaving peer).
const MAX_FRAME_BYTES: usize = 256 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(20);
const READ_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone, Serialize)]
struct NetStatus {
    state: String,
    role: String,
    detail: String,
    peers: usize,
    room: String,
    server: String,
}

struct Session {
    role: String,
    room: String,
    server: String,
    running: Arc<AtomicBool>,
    tx: SyncSender<String>,
    stream: TcpStream,
    app: AppHandle,
}

struct RelayControl {
    port: u16,
    running: Arc<AtomicBool>,
}

struct RelayClient {
    id: u64,
    tx: SyncSender<String>,
}

type RoomMap = Arc<Mutex<HashMap<String, Vec<RelayClient>>>>;

fn session_slot() -> &'static Mutex<Option<Session>> {
    static SLOT: OnceLock<Mutex<Option<Session>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn relay_slot() -> &'static Mutex<Option<RelayControl>> {
    static SLOT: OnceLock<Mutex<Option<RelayControl>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Room codes are short, shoutable, and case-insensitive: "ab12" == "AB12".
pub fn sanitize_room(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect::<String>()
        .to_ascii_uppercase()
}

pub fn sanitize_name(raw: &str) -> String {
    let name: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(16)
        .collect();
    if name.is_empty() {
        "KuroCat".to_string()
    } else {
        name
    }
}

/// Accepts "host:port", "host" (default port) and strips an optional scheme.
fn parse_server(raw: &str) -> Result<SocketAddr, String> {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix("tcp://")
        .or_else(|| trimmed.strip_prefix("kurocat://"))
        .unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Err("请先填写服务器地址（例如 relay.example.com:27422）".into());
    }
    let with_port = if trimmed.rsplit(':').next().map(|p| p.parse::<u16>().is_ok()) == Some(true)
        && !trimmed.ends_with(']')
    {
        trimmed.to_string()
    } else {
        format!("{trimmed}:{DEFAULT_PORT}")
    };
    let mut addrs = with_port
        .to_socket_addrs()
        .map_err(|e| format!("无法解析服务器地址 {with_port}：{e}"))?;
    addrs
        .next()
        .ok_or_else(|| format!("服务器地址 {with_port} 没有可用的 IP"))
}

fn emit(app: &AppHandle, event: &str, payload: Value) {
    let _ = app.emit(event, payload);
}

fn emit_status(app: &AppHandle, status: NetStatus) {
    let _ = app.emit(EVENT_STATUS, status);
}

fn write_frame(stream: &mut TcpStream, frame: &str) -> std::io::Result<()> {
    stream.write_all(frame.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// Reads one newline-terminated frame with a hard size cap.
fn read_frame<R: BufRead>(reader: &mut R, buf: &mut String) -> std::io::Result<bool> {
    buf.clear();
    let mut bytes = Vec::new();
    let n = reader.read_until(b'\n', &mut bytes)?;
    if n == 0 {
        return Ok(false);
    }
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    *buf = String::from_utf8_lossy(&bytes).trim().to_string();
    Ok(!buf.is_empty())
}

fn encode(frame: &Value) -> String {
    frame.to_string()
}

// ---------------------------------------------------------------------------
// public API used by the app
// ---------------------------------------------------------------------------

/// Fan a local input event out to the friend. Runs on the input hook thread, so
/// it must never block: a full queue simply drops the frame.
pub fn broadcast_input(ev: &InputEvent) {
    let Ok(guard) = session_slot().lock() else {
        return;
    };
    let Some(session) = guard.as_ref() else {
        return;
    };
    if !session.running.load(Ordering::Relaxed) {
        return;
    }
    let Ok(ev_json) = serde_json::to_value(ev) else {
        return;
    };
    let frame = encode(&json!({ "t": "msg", "d": { "k": "input", "ev": ev_json } }));
    let _ = session.tx.try_send(frame);
}

/// Publish our current look & layout so the friend's window matches the original.
pub fn broadcast_state(state: &Value) {
    let Ok(guard) = session_slot().lock() else {
        return;
    };
    let Some(session) = guard.as_ref() else {
        return;
    };
    if !session.running.load(Ordering::Relaxed) {
        return;
    }
    let frame = encode(&json!({ "t": "msg", "d": { "k": "state", "s": state } }));
    let _ = session.tx.try_send(frame);
}

pub fn is_connected() -> bool {
    match session_slot().lock() {
        Ok(guard) => guard
            .as_ref()
            .map(|s| s.running.load(Ordering::Relaxed))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Best-effort LAN address of this machine (no packets are sent: connecting a UDP
/// socket only makes the OS pick the outbound interface).
pub fn local_ip_string() -> Option<String> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("8.8.8.8", 80)).ok()?;
    socket
        .local_addr()
        .ok()
        .map(|addr| addr.ip().to_string())
        .filter(|ip| ip != "0.0.0.0")
}

/// Host a room on this machine: bind the port, then connect to ourselves over
/// loopback so both modes share exactly one code path.
pub fn start_host(app: AppHandle, port: u16, room: &str, name: &str) -> Result<u16, String> {
    let room = sanitize_room(room);
    if room.is_empty() {
        return Err("房间码不能为空".into());
    }
    let name = sanitize_name(name);
    let port = relay_start(port)?;
    if let Err(err) = connect(app.clone(), &format!("127.0.0.1:{port}"), &room, &name) {
        relay_stop();
        return Err(err);
    }
    if let Ok(mut guard) = session_slot().lock() {
        if let Some(session) = guard.as_mut() {
            session.role = "host".to_string();
        }
    }
    let lan_addr = format!(
        "{}:{port}",
        local_ip_string().unwrap_or_else(|| "本机IP".to_string())
    );
    let _ = app.emit(
        EVENT_STATUS,
        NetStatus {
            state: "connected".into(),
            role: "host".into(),
            detail: format!("局域网房间已开启：{lan_addr}（朋友填这个地址）"),
            peers: 0,
            room: room.clone(),
            server: lan_addr.clone(),
        },
    );
    Ok(port)
}

/// Join a room through a relay (or a friend's LAN host address).
pub fn connect(app: AppHandle, server: &str, room: &str, name: &str) -> Result<(), String> {
    let room = sanitize_room(room);
    if room.is_empty() {
        return Err("房间码不能为空".into());
    }
    let name = sanitize_name(name);
    let addr = parse_server(server)?;
    stop_session(false);

    emit_status(
        &app,
        NetStatus {
            state: "connecting".into(),
            role: "guest".into(),
            detail: format!("正在连接 {addr} …"),
            peers: 0,
            room: room.clone(),
            server: addr.to_string(),
        },
    );

    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| format!("连接 {addr} 失败：{e}"))?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));

    let running = Arc::new(AtomicBool::new(true));
    let peers = Arc::new(Mutex::new(0usize));

    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let handshake = encode(&json!({ "t": "join", "room": room, "name": name, "v": 1 }));
    write_frame(&mut writer, &handshake).map_err(|e| format!("发送房间信息失败：{e}"))?;

    let (tx, rx) = sync_channel::<String>(WRITER_QUEUE);
    {
        let mut guard = session_slot().lock().map_err(|_| "网络状态锁失败".to_string())?;
        if guard.is_some() {
            // A concurrent connect() won the race; the caller can retry.
            return Err("正在联机中，请先断开".into());
        }
        guard.replace(Session {
            role: "guest".into(),
            room: room.clone(),
            server: addr.to_string(),
            running: running.clone(),
            tx: tx.clone(),
            stream: stream.try_clone().map_err(|e| e.to_string())?,
            app: app.clone(),
        });
    }

    spawn_writer(writer, rx, running.clone());
    spawn_heartbeat(tx, running.clone());
    spawn_reader(app.clone(), stream, room, addr.to_string(), running, peers);
    Ok(())
}

pub fn stop() {
    stop_session(true);
    relay_stop();
}

/// Closes the current room connection (if any) without touching the local relay.
fn stop_session(announce: bool) {
    let session = {
        match session_slot().lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        }
    };
    if let Some(session) = session {
        session.running.store(false, Ordering::Relaxed);
        let _ = session.stream.shutdown(Shutdown::Both);
        if announce {
            emit_status(
                &session.app,
                NetStatus {
                    state: "idle".into(),
                    role: session.role.clone(),
                    detail: "已断开联机".into(),
                    peers: 0,
                    room: session.room.clone(),
                    server: session.server.clone(),
                },
            );
        }
        emit(&session.app, EVENT_PRESENCE, json!({ "event": "reset", "name": null, "count": 0 }));
    }
}

/// Keeps NAT/firewall state alive on a quiet connection (no keys pressed).
fn spawn_heartbeat(tx: SyncSender<String>, running: Arc<AtomicBool>) {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            thread::sleep(HEARTBEAT_EVERY);
            if !running.load(Ordering::Relaxed) {
                break;
            }
            if tx.try_send(encode(&json!({ "t": "ping" }))).is_err() {
                break;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// client side
// ---------------------------------------------------------------------------

fn spawn_writer(mut stream: TcpStream, rx: Receiver<String>, running: Arc<AtomicBool>) {
    thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(frame) => {
                    if write_frame(&mut stream, &frame).is_err() {
                        running.store(false, Ordering::Relaxed);
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = write_frame(&mut stream, &encode(&json!({ "t": "leave" })));
                    break;
                }
            }
        }
        let _ = stream.shutdown(Shutdown::Both);
    });
}

fn spawn_reader(
    app: AppHandle,
    stream: TcpStream,
    room: String,
    server: String,
    running: Arc<AtomicBool>,
    peers: Arc<Mutex<usize>>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let mut failure: Option<String> = None;

        loop {
            if !running.load(Ordering::Relaxed) {
                break;
            }
            match read_frame(&mut reader, &mut line) {
                Ok(false) => {
                    failure = Some("服务器已关闭连接".into());
                    break;
                }
                Ok(true) => {}
                Err(e) => {
                    failure = Some(format!("网络读取失败：{e}"));
                    break;
                }
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            handle_server_frame(&app, &value, &room, &server, &peers);
        }

        let was_running = running.swap(false, Ordering::Relaxed);
        if was_running {
            if let Some(detail) = failure {
                emit_status(
                    &app,
                    NetStatus {
                        state: "error".into(),
                        role: "guest".into(),
                        detail,
                        peers: 0,
                        room: room.clone(),
                        server: server.clone(),
                    },
                );
            } else {
                emit_status(
                    &app,
                    NetStatus {
                        state: "idle".into(),
                        role: "guest".into(),
                        detail: "联机已断开".into(),
                        peers: 0,
                        room: room.clone(),
                        server: server.clone(),
                    },
                );
            }
            emit(&app, EVENT_PRESENCE, json!({ "event": "reset", "name": null, "count": 0 }));
            // The session object stays until the user reconnects; mark it dead so
            // broadcast_input stops queueing frames.
        }
    });
}

fn handle_server_frame(
    app: &AppHandle,
    value: &Value,
    room: &str,
    server: &str,
    peers: &Arc<Mutex<usize>>,
) {
    match value.get("t").and_then(|t| t.as_str()).unwrap_or("") {
        "joined" => {
            let count = value.get("count").and_then(|c| c.as_u64()).unwrap_or(1) as usize;
            if let Ok(mut p) = peers.lock() {
                *p = count;
            }
            emit_status(
                app,
                NetStatus {
                    state: "connected".into(),
                    role: "guest".into(),
                    detail: if count > 1 {
                        "好友已在房间里 🐾".to_string()
                    } else {
                        "已进入房间，等待对方接入…".to_string()
                    },
                    peers: count,
                    room: room.to_string(),
                    server: server.to_string(),
                },
            );
            // The friend was already waiting in the room, so no "peer join" frame
            // will ever arrive for them: announce them ourselves.
            if count > 1 {
                emit(
                    app,
                    EVENT_PRESENCE,
                    json!({ "event": "join", "name": Value::Null, "count": count }),
                );
            }
        }
        "peer" => {
            let event = value.get("event").and_then(|e| e.as_str()).unwrap_or("");
            let count = value.get("count").and_then(|c| c.as_u64()).unwrap_or(0) as usize;
            if let Ok(mut p) = peers.lock() {
                *p = count;
            }
            let name = value.get("name").cloned().unwrap_or(Value::Null);
            emit(
                app,
                EVENT_PRESENCE,
                json!({ "event": event, "name": name, "count": count }),
            );
            let (state, detail) = if count > 1 {
                ("connected", "好友已接入 🐾".to_string())
            } else {
                ("waiting", "等待好友加入…".to_string())
            };
            emit_status(
                app,
                NetStatus {
                    state: state.into(),
                    role: "guest".into(),
                    detail,
                    peers: count,
                    room: room.to_string(),
                    server: server.to_string(),
                },
            );
        }
        "msg" => {
            let Some(data) = value.get("d") else {
                return;
            };
            match data.get("k").and_then(|k| k.as_str()).unwrap_or("") {
                "input" => {
                    if let Some(ev) = data.get("ev") {
                        emit(app, EVENT_INPUT, ev.clone());
                    }
                }
                "state" => {
                    if let Some(state) = data.get("s") {
                        emit(app, EVENT_STATE, state.clone());
                    }
                }
                _ => {}
            }
        }
        "error" => {
            let msg = value
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("服务器拒绝连接");
            emit_status(
                app,
                NetStatus {
                    state: "error".into(),
                    role: "guest".into(),
                    detail: msg.to_string(),
                    peers: 0,
                    room: room.to_string(),
                    server: server.to_string(),
                },
            );
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// relay (also used by the host / LAN mode)
// ---------------------------------------------------------------------------

/// Bind the relay port and accept rooms. Returns the actually bound port.
pub fn relay_start(port: u16) -> Result<u16, String> {
    relay_stop();
    let listener = TcpListener::bind(("0.0.0.0", port))
        .map_err(|e| format!("端口 {port} 无法监听（可能被占用或被防火墙拦截）：{e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?.port();
    let running = Arc::new(AtomicBool::new(true));
    let peers = Arc::new(Mutex::new(0usize));
    let rooms: RoomMap = Arc::new(Mutex::new(HashMap::new()));

    {
        let mut guard = relay_slot().lock().map_err(|_| "中继状态锁失败".to_string())?;
        guard.replace(RelayControl {
            port: bound,
            running: running.clone(),
        });
    }

    let rooms_for_thread = rooms.clone();
    thread::spawn(move || {
        for incoming in listener.incoming() {
            if !running.load(Ordering::Relaxed) {
                break;
            }
            match incoming {
                Ok(stream) => {
                    let rooms = rooms_for_thread.clone();
                    let running = running.clone();
                    let peers = peers.clone();
                    thread::spawn(move || {
                        let _ = stream.set_nodelay(true);
                        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
                        handle_relay_client(stream, rooms, running, peers);
                    });
                }
                Err(_) => {
                    if !running.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(120));
                }
            }
        }
    });

    Ok(bound)
}

pub fn relay_stop() {
    let control = match relay_slot().lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => None,
    };
    if let Some(control) = control {
        control.running.store(false, Ordering::Relaxed);
        // Wake the blocked accept() so the thread can notice `running == false`.
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], control.port)),
            Duration::from_millis(200),
        );
    }
}

pub fn relay_running() -> bool {
    match relay_slot().lock() {
        Ok(guard) => guard
            .as_ref()
            .map(|c| c.running.load(Ordering::Relaxed))
            .unwrap_or(false),
        Err(_) => false,
    }
}

fn room_count(rooms: &RoomMap, room: &str) -> usize {
    rooms
        .lock()
        .map(|map| map.get(room).map(|peers| peers.len()).unwrap_or(0))
        .unwrap_or(0)
}

fn send_to(tx: &SyncSender<String>, frame: &str) -> bool {
    match tx.try_send(frame.to_string()) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => true,
        Err(TrySendError::Disconnected(_)) => false,
    }
}

fn broadcast_room(rooms: &RoomMap, room: &str, frame: &str, skip_id: Option<u64>) {
    let Ok(mut map) = rooms.lock() else {
        return;
    };
    let Some(clients) = map.get_mut(room) else {
        return;
    };
    clients.retain(|c| {
        if Some(c.id) == skip_id {
            // Keep the sender in the room, but don't echo its own frame back.
            return true;
        }
        send_to(&c.tx, frame)
    });
}

fn handle_relay_client(
    stream: TcpStream,
    rooms: RoomMap,
    running: Arc<AtomicBool>,
    peers: Arc<Mutex<usize>>,
) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    // First frame must be a join; refuse anything else so the relay can't be
    // used as an open TCP proxy.
    let mut joined: Option<(String, String)> = None;
    let mut attempts = 0;
    while joined.is_none() && attempts < 3 {
        attempts += 1;
        match read_frame(&mut reader, &mut line) {
            Ok(true) => {}
            _ => return,
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            return;
        };
        if value.get("t").and_then(|t| t.as_str()) != Some("join") {
            let _ = write_frame(
                &mut writer,
                &encode(&json!({ "t": "error", "msg": "请先发送 join 进入房间" })),
            );
            continue;
        }
        let room = sanitize_room(value.get("room").and_then(|r| r.as_str()).unwrap_or(""));
        if room.is_empty() {
            let _ = write_frame(
                &mut writer,
                &encode(&json!({ "t": "error", "msg": "房间码无效" })),
            );
            continue;
        }
        let name = sanitize_name(value.get("name").and_then(|n| n.as_str()).unwrap_or(""));
        joined = Some((room, name));
    }
    let Some((room, name)) = joined else {
        return;
    };

    // Room capacity check happens before registration.
    if room_count(&rooms, &room) >= MAX_ROOM_PEERS {
        let _ = write_frame(
            &mut writer,
            &encode(&json!({ "t": "error", "msg": "房间已满（最多两人）" })),
        );
        return;
    }

    let (tx, rx) = sync_channel::<String>(WRITER_QUEUE);
    let conn_id = RELAY_CONN_SEQ.fetch_add(1, Ordering::Relaxed);
    {
        let Ok(mut map) = rooms.lock() else {
            return;
        };
        map.entry(room.clone())
            .or_default()
            .push(RelayClient {
                id: conn_id,
                tx: tx.clone(),
            });
    }
    if let Ok(mut count) = peers.lock() {
        *count = room_count(&rooms, &room);
    }

    let writer_running = running.clone();
    thread::spawn(move || {
        while writer_running.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(frame) => {
                    if write_frame(&mut writer, &frame).is_err() {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = writer.shutdown(Shutdown::Both);
    });

    let count = room_count(&rooms, &room);
    let _ = send_to(
        &tx,
        &encode(&json!({ "t": "joined", "room": room, "count": count, "you": name })),
    );
    broadcast_room(
        &rooms,
        &room,
        &encode(&json!({ "t": "peer", "event": "join", "name": name, "count": count })),
        Some(conn_id),
    );

    loop {
        if !running.load(Ordering::Relaxed) {
            break;
        }
        let read = read_frame(&mut reader, &mut line);
        match read {
            Ok(false) | Err(_) => break,
            Ok(true) => {}
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match value.get("t").and_then(|t| t.as_str()).unwrap_or("") {
            "ping" => {
                let _ = send_to(&tx, &encode(&json!({ "t": "pong" })));
            }
            "msg" => {
                let Some(data) = value.get("d") else {
                    continue;
                };
                let frame = encode(&json!({ "t": "msg", "d": data, "from": name }));
                broadcast_room(&rooms, &room, &frame, Some(conn_id));
            }
            "leave" => break,
            _ => {}
        }
    }

    // Deregister and tell the remaining peer.
    {
        if let Ok(mut map) = rooms.lock() {
            if let Some(clients) = map.get_mut(&room) {
                clients.retain(|c| c.id != conn_id);
                if clients.is_empty() {
                    map.remove(&room);
                }
            }
        }
    }
    let count = room_count(&rooms, &room);
    if let Ok(mut count_slot) = peers.lock() {
        *count_slot = count;
    }
    broadcast_room(
        &rooms,
        &room,
        &encode(&json!({ "t": "peer", "event": "leave", "name": name, "count": count })),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::time::Instant;

    /// The relay lives in process-wide statics, so these tests must not overlap.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct FakeClient {
        stream: TcpStream,
        reader: BufReader<TcpStream>,
    }

    impl FakeClient {
        fn join(addr: SocketAddr, room: &str, name: &str) -> Self {
            let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("timeout");
            let reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut client = Self { stream, reader };
            client.send(json!({ "t": "join", "room": room, "name": name }));
            client
        }

        fn send(&mut self, frame: Value) {
            let line = format!("{frame}\n");
            self.stream.write_all(line.as_bytes()).expect("write");
            self.stream.flush().expect("flush");
        }

        fn recv(&mut self) -> Value {
            let mut line = String::new();
            self.reader.read_line(&mut line).expect("read");
            serde_json::from_str(line.trim()).expect("json")
        }

        /// Reads frames until one with `t == wanted` shows up (or panics on timeout).
        fn recv_type(&mut self, wanted: &str) -> Value {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                assert!(Instant::now() < deadline, "timed out waiting for {wanted}");
                let frame = self.recv();
                if frame.get("t").and_then(|t| t.as_str()) == Some(wanted) {
                    return frame;
                }
            }
        }
    }

    #[test]
    fn room_codes_and_names_are_sanitized() {
        assert_eq!(sanitize_room("  ab-12 cd "), "AB12CD");
        assert_eq!(sanitize_room(""), "");
        assert_eq!(sanitize_name("   "), "KuroCat");
        assert_eq!(sanitize_name("a\u{7}b"), "ab");
        assert_eq!(sanitize_name("0123456789abcdefghij"), "0123456789abcdef");
    }

    #[test]
    fn relay_pairs_two_cats_and_forwards_messages() {
        let _guard = TEST_LOCK.lock().unwrap();
        let port = relay_start(0).expect("relay should bind an ephemeral port");
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();

        let mut alice = FakeClient::join(addr, "test-room", "Alice");
        let joined = alice.recv_type("joined");
        assert_eq!(joined["room"], "TESTROOM");
        assert_eq!(joined["count"], 1);

        let mut bob = FakeClient::join(addr, "test-room", "Bob");
        let bob_joined = bob.recv_type("joined");
        assert_eq!(bob_joined["count"], 2);

        // Alice is told that Bob arrived.
        let presence = alice.recv_type("peer");
        assert_eq!(presence["event"], "join");
        assert_eq!(presence["name"], "Bob");
        assert_eq!(presence["count"], 2);

        // Input frames are relayed verbatim, tagged with the sender.
        bob.send(json!({
            "t": "msg",
            "d": { "k": "input", "ev": { "kind": "keyDown", "vk": 65, "scan": 0, "repeat": false } }
        }));
        let relayed = alice.recv_type("msg");
        assert_eq!(relayed["from"], "Bob");
        assert_eq!(relayed["d"]["k"], "input");
        assert_eq!(relayed["d"]["ev"]["vk"], 65);

        // Bob leaving notifies Alice and drops the room back to one member.
        bob.send(json!({ "t": "leave" }));
        let left = alice.recv_type("peer");
        assert_eq!(left["event"], "leave");
        assert_eq!(left["count"], 1);

        relay_stop();
    }

    #[test]
    fn relay_refuses_a_third_cat_in_one_room() {
        let _guard = TEST_LOCK.lock().unwrap();
        let port = relay_start(0).expect("relay should bind an ephemeral port");
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();

        let mut alice = FakeClient::join(addr, "full", "Alice");
        alice.recv_type("joined");
        let mut bob = FakeClient::join(addr, "full", "Bob");
        bob.recv_type("joined");

        let mut carol = FakeClient::join(addr, "full", "Carol");
        let rejected = carol.recv_type("error");
        assert!(rejected["msg"].as_str().unwrap_or("").contains("房间已满"));

        relay_stop();
    }
}
