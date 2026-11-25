#!/usr/bin/env node
/**
 * KuroCat relay server — zero dependencies, Node 18+.
 *
 *   node relay/kurocat-relay.mjs --port 27422
 *
 * Both friends connect OUT to this process (so no port forwarding / public IP is
 * needed on either home network). Each room holds exactly two cats; messages are
 * forwarded verbatim to the other member of the room.
 *
 * Protocol (newline-delimited JSON over TCP):
 *   client -> relay : {"t":"join","room":"KURO","name":"Q","v":1}
 *                     {"t":"msg","d":{...}} | {"t":"ping"} | {"t":"leave"}
 *   relay -> client : {"t":"joined","room":"KURO","count":1,"you":"Q"}
 *                     {"t":"peer","event":"join"|"leave","name":"F","count":2}
 *                     {"t":"msg","d":{...},"from":"F"} | {"t":"pong"} | {"t":"error","msg":"..."}
 *
 * The desktop app speaks exactly the same protocol for its built-in
 * "host a LAN room" mode, so one implementation covers both.
 */
import net from 'node:net';

const args = process.argv.slice(2);
const readArg = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback;
};

const PORT = Number(readArg('port', process.env.PORT || 27422));
const HOST = readArg('host', process.env.HOST || '0.0.0.0');
const MAX_PEERS = Math.max(2, Number(readArg('max-peers', 2)) || 2);
const MAX_FRAME = 256 * 1024;
const IDLE_TIMEOUT_MS = 90_000;

/** @type {Map<string, Set<Client>>} */
const rooms = new Map();
let connectionSeq = 0;

const log = (...parts) => {
  const stamp = new Date().toISOString().slice(11, 19);
  console.log(`[${stamp}]`, ...parts);
};

const sanitizeRoom = (raw) =>
  String(raw || '')
    .replace(/[^a-zA-Z0-9]/g, '')
    .slice(0, 12)
    .toUpperCase();

const sanitizeName = (raw) => {
  const name = String(raw || '')
    .replace(/[\u0000-\u001f\u007f]/g, '')
    .trim()
    .slice(0, 16);
  return name || 'KuroCat';
};

const send = (client, frame) => {
  if (client.dead) return;
  try {
    client.socket.write(`${JSON.stringify(frame)}\n`);
  } catch {
    dropClient(client);
  }
};

const broadcastRoom = (room, frame, skipId = null) => {
  const members = rooms.get(room);
  if (!members) return;
  for (const client of members) {
    if (client.id !== skipId) send(client, frame);
  }
};

const roomCount = (room) => rooms.get(room)?.size ?? 0;

function dropClient(client) {
  if (client.dead) return;
  client.dead = true;
  const members = rooms.get(client.room);
  if (members) {
    members.delete(client);
    if (members.size === 0) rooms.delete(client.room);
  }
  broadcastRoom(
    client.room,
    { t: 'peer', event: 'leave', name: client.name, count: roomCount(client.room) },
  );
  log(`- ${client.name} left ${client.room} (${roomCount(client.room)} left) conn#${client.id}`);
  try {
    client.socket.destroy();
  } catch {
    /* already gone */
  }
}

const server = net.createServer((socket) => {
  const client = {
    id: ++connectionSeq,
    socket,
    buffer: '',
    room: null,
    name: 'KuroCat',
    dead: false,
    lastSeen: Date.now(),
    joined: false,
  };

  socket.setNoDelay(true);
  socket.setEncoding('utf8');

  socket.on('data', (chunk) => {
    client.lastSeen = Date.now();
    client.buffer += chunk;
    if (client.buffer.length > MAX_FRAME * 4) {
      send(client, { t: 'error', msg: 'frame too large' });
      dropClient(client);
      return;
    }
    let index;
    while ((index = client.buffer.indexOf('\n')) >= 0) {
      const line = client.buffer.slice(0, index).trim();
      client.buffer = client.buffer.slice(index + 1);
      if (!line) continue;
      if (line.length > MAX_FRAME) {
        send(client, { t: 'error', msg: 'frame too large' });
        dropClient(client);
        return;
      }
      let frame;
      try {
        frame = JSON.parse(line);
      } catch {
        continue;
      }
      handleFrame(client, frame);
      if (client.dead) return;
    }
  });

  socket.on('error', () => dropClient(client));
  socket.on('close', () => dropClient(client));
});

function handleFrame(client, frame) {
  const type = typeof frame?.t === 'string' ? frame.t : '';

  if (!client.joined) {
    if (type !== 'join') {
      send(client, { t: 'error', msg: 'send join first' });
      return;
    }
    const room = sanitizeRoom(frame.room);
    if (!room) {
      send(client, { t: 'error', msg: 'invalid room code' });
      return;
    }
    if (roomCount(room) >= MAX_PEERS) {
      send(client, { t: 'error', msg: 'room is full (max two)' });
      return;
    }
    client.room = room;
    client.name = sanitizeName(frame.name);
    client.joined = true;
    if (!rooms.has(room)) rooms.set(room, new Set());
    rooms.get(room).add(client);

    const count = roomCount(room);
    send(client, { t: 'joined', room, count, you: client.name });
    broadcastRoom(
      room,
      { t: 'peer', event: 'join', name: client.name, count },
      client.id,
    );
    log(`+ ${client.name} joined ${room} (${count}/${MAX_PEERS}) conn#${client.id}`);
    return;
  }

  switch (type) {
    case 'ping':
      send(client, { t: 'pong' });
      break;
    case 'msg': {
      if (!frame.d || typeof frame.d !== 'object') return;
      broadcastRoom(client.room, { t: 'msg', d: frame.d, from: client.name }, client.id);
      break;
    }
    case 'leave':
      dropClient(client);
      break;
    default:
      break;
  }
}

// Drop sockets that stopped talking (laptop lid closed, Wi-Fi dropped, ...).
const sweeper = setInterval(() => {
  const now = Date.now();
  for (const members of rooms.values()) {
    for (const client of members) {
      if (now - client.lastSeen > IDLE_TIMEOUT_MS) {
        log(`! idle timeout ${client.name} conn#${client.id}`);
        dropClient(client);
      }
    }
  }
}, 15_000);
sweeper.unref();

server.listen(PORT, HOST, () => {
  log(`KuroCat relay listening on ${HOST}:${PORT} (max ${MAX_PEERS} cats per room)`);
  log('Point both KuroCat clients at this host:port and use the same room code.');
});

const shutdown = (signal) => {
  log(`${signal} received, closing relay…`);
  for (const members of rooms.values()) {
    for (const client of [...members]) dropClient(client);
  }
  server.close(() => process.exit(0));
  setTimeout(() => process.exit(0), 1500).unref();
};

process.on('SIGINT', () => shutdown('SIGINT'));
process.on('SIGTERM', () => shutdown('SIGTERM'));

server.on('error', (err) => {
  console.error(`[relay] fatal: ${err.message}`);
  process.exit(1);
});
