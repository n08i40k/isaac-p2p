//! Own UDP transport for the game's ISteamNetworking P2P traffic.
//!
//! Topology is a star: the host listens on a UDP port, every client connects to it, and the host forwards packets
//! between clients. Players are addressed by Steam ID, as the game does, so the ISteamNetworking hooks only swap
//! the wire underneath.
//!
//! Wire format, little endian, every datagram starts with MAGIC and a type byte. The handshake is in the clear:
//!   HELLO    u32 version, u64 steam_id, u32 session, [32] client key, u16 n, [n] Steam ticket, [16] mac
//!                                                                                  client -> host, repeated
//!   WELCOME  u32 version, u64 host_steam_id, u32 session, [32] host key, [16] mac    host -> client
//!
//! A host runs in one of two modes. With a password, the MACs are keyed with it (see `crypto`) and only players
//! who know it complete the handshake. Without one (the default), the client puts a Steam session ticket into
//! its HELLO; the host has Steam verify it (BeginAuthSession, answered by a ValidateAuthTicketResponse_t
//! callback) and answers only once Steam confirmed the Steam ID and that player is a member of its lobby. The
//! MACs then use a fixed key and only guard against corruption; the ticket is what proves who is connecting.
//! It does not stop someone who can read the traffic on the way from replaying a ticket before its owner uses
//! it; a password does.
//! Everything after it is sealed: SEALED u32 session, u64 counter, encrypt(u8 type, body), [16] tag, where type
//! and body are one of:
//!   ROSTER   u64 lobby_id, u8 count, count * u64 ids   host -> client, on change and every second
//!   PING     u32 sent_ms / PONG u32 echoed_ms          keep-alive and RTT, both directions
//!   BYE                                                orderly disconnect
//!   DATA     u64 from, u64 to, u8 channel, payload     unreliable message
//!   REL      u32 seq, u8 flags, fragment               reliable stream; first fragment of a message starts with
//!                                                      u64 from, u64 to, u8 channel, u32 total size
//!   ACK      u32 next_expected, u64 bitmap             cumulative ack + the 64 sequence numbers after it
//!
//! Reliable messages share one ordered stream per link, split into fragments of at most FRAGMENT_SIZE bytes.
//! A fragment is sent again when its retransmission timeout runs out (smoothed RTT + 4 * RTT variance, doubled
//! on every timeout of that fragment), or right away once FAST_RETRANSMIT_NACKS acks have reported later
//! fragments but not this one. The newest fragment is probed once more after about one RTT, since no later
//! fragment can reveal its loss. RTT samples come from PING/PONG and from acks of fragments sent only once.
//!
//! At most `cwnd` fragments are in flight per link; later ones wait in the send window. The congestion window
//! grows by one fragment per acked fragment up to `ssthresh` and by one per window after it, halves on a fast
//! retransmit (once per window of data) and drops to MIN_CWND on a timeout.
//!
//! A client keeps a session number per connection attempt. When the host stops answering, the client starts a
//! new session and says hello again for up to RECONNECT_TIMEOUT_MS; the host replaces a link whose HELLO carries
//! a different session, so both ends restart their reliable streams together. A HELLO only replaces a link that
//! has been quiet for REPLACE_QUIET_MS, so a replayed old HELLO cannot cut off a live player.
//!
//! Players find each other through the Steam lobby: each side asks STUN servers for its public address, the host
//! publishes its addresses as lobby data (LOBBY_KEY = "version;host steam id;ip:port,..."), and every client
//! publishes its own as lobby member data ("version;ip:port,..."). The client says hello to every host address
//! while the host sends PUNCH datagrams to every address of a client it has no link with, which opens both NATs
//! for the other side's packets. The first valid WELCOME decides the address. The password never goes into the
//! lobby: anyone who can see a lobby can read its data. The offer says which mode the host runs in.
//!
//! A sealed datagram is matched to its link by session number. One that opens and is not a replay moves the
//! link to the address it came from, so a player whose IP or port changes keeps the session.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs, UdpSocket};
use std::os::windows::io::AsRawSocket;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::steam::{P2PSessionState, SEND_RELIABLE, SEND_RELIABLE_WITH_BUFFERING};
use crate::{crypto, log, settings, stun, win};

const MAGIC: u8 = 0x49; // 'I'
const PROTOCOL_VERSION: u32 = 4;

const T_HELLO: u8 = 1;
const T_WELCOME: u8 = 2;
const T_ROSTER: u8 = 3;
const T_PING: u8 = 4;
const T_PONG: u8 = 5;
const T_BYE: u8 = 6;
const T_DATA: u8 = 7;
const T_REL: u8 = 8;
const T_ACK: u8 = 9;
const T_SEALED: u8 = 10;
const T_PUNCH: u8 = 11; // no body; only opens the sender's NAT towards the receiver

const MAX_LINKS: usize = 8;
const MAX_DATAGRAM: usize = 1400;
const FRAGMENT_SIZE: usize = 1200;
const DATA_HEADER: usize = 8 + 8 + 1;
/// Bytes a sealed datagram adds to its type byte and body.
const SEAL_OVERHEAD: usize = crypto::SEALED_HEADER + crypto::TAG_SIZE;
/// WELCOME, and the fixed part of HELLO before its ticket.
const HANDSHAKE_SIZE: usize = 2 + 4 + 8 + 4 + 32 + crypto::MAC_SIZE;
const HANDSHAKE_SIGNED: usize = HANDSHAKE_SIZE - crypto::MAC_SIZE;
const MAX_TICKET: usize = 1024;
const REL_HEADER: usize = 4 + 1;
const MESSAGE_HEADER: usize = 8 + 8 + 1 + 4;
const WINDOW: u32 = 1024;
pub const MAX_MESSAGE: u32 = 1 << 20;
const MAX_QUEUED_BYTES: u32 = 16 << 20;
const HELLO_INTERVAL_MS: u64 = 250;
const CONNECT_TIMEOUT_MS: u64 = 10000;
const PING_INTERVAL_MS: u64 = 1000;
const LINK_TIMEOUT_MS: u64 = 10000;
const ROSTER_INTERVAL_MS: u64 = 1000;
const MIN_RTO_MS: u64 = 20;
const MAX_RTO_MS: u64 = 1000;
const MAX_BACKOFF: u8 = 4; // timeout doubles up to 2^MAX_BACKOFF times
const FAST_RETRANSMIT_NACKS: u8 = 2;
const ACK_BITS: u32 = 64;
const MIN_CWND: f32 = 8.0;
const INITIAL_CWND: f32 = 64.0;
const RECONNECT_TIMEOUT_MS: u64 = 60000;
const REPLACE_QUIET_MS: u64 = 3000;
const VERIFY_TIMEOUT_MS: u64 = 15000;
const REJECT_LOG_INTERVAL_MS: u64 = 5000;
const LOBBY_TICK_MS: u64 = 500;
const STUN_INTERVAL_MS: u64 = 500;
const STUN_TRIES: u8 = 6;
const MAX_CANDIDATES: usize = 5;

const FLAG_FIRST: u8 = 1;
const FLAG_LAST: u8 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    None,
    Host,
    Client,
}

fn now_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

fn get32(p: &[u8]) -> u32 {
    u32::from_le_bytes(p[..4].try_into().unwrap())
}

fn get64(p: &[u8]) -> u64 {
    u64::from_le_bytes(p[..8].try_into().unwrap())
}

// ---------------------------------------------------------------------------------------------------------------
// Socket.

type Socket = Option<Arc<UdpSocket>>;

static RNG: AtomicU32 = AtomicU32::new(0x2545_F491);

fn random() -> u32 {
    let mut x = RNG.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    RNG.store(x, Ordering::Relaxed);
    x
}

fn should_drop() -> bool {
    let percent = settings::get().drop_percent;
    percent > 0 && random() % 100 < percent
}

fn raw_send(sock: &Socket, to: SocketAddrV4, data: &[u8]) {
    if should_drop() {
        return;
    }
    if let Some(sock) = sock {
        let _ = sock.send_to(data, to);
    }
}

/// Seals `kind, body` for a link and sends it; nothing goes out before the handshake set the keys.
fn seal_send(tx: &mut Option<crypto::Sealer>, session: u32, sock: &Socket, to: SocketAddrV4, kind: u8, body: &[u8]) {
    let Some(tx) = tx.as_mut() else { return };
    let mut plain = Vec::with_capacity(1 + body.len());
    plain.push(kind);
    plain.extend_from_slice(body);
    raw_send(sock, to, &tx.seal(MAGIC, T_SEALED, session, &plain));
}

// ---------------------------------------------------------------------------------------------------------------
// Links and the reliable stream.

struct SendSlot {
    used: bool,
    acked: bool,
    retransmitted: bool,
    backoff: u8,
    nacks: u8,
    size: u16,
    sent_at: u64,
    datagram: [u8; REL_HEADER + FRAGMENT_SIZE],
}

struct RecvSlot {
    have: bool,
    flags: u8,
    size: u16,
    data: [u8; FRAGMENT_SIZE],
}

/// A complete reliable message taken off a link's stream.
struct Delivery {
    from: u64,
    to: u64,
    channel: u8,
    data: Vec<u8>,
}

struct Link {
    welcomed: bool,
    confirmed: bool, // host side: the client answered after WELCOME, so replies reach it
    addr: SocketAddrV4,
    steam_id: u64,
    session: u32,
    tx: Option<crypto::Sealer>,
    rx: Option<crypto::Opener>,
    secret: Option<crypto::EphemeralSecret>, // client: own handshake key until WELCOME
    handshake: Vec<u8>, // own HELLO (client) or WELCOME (host), sent again when the peer repeats itself
    peer_hello: Vec<u8>, // host: the client's HELLO while Steam verifies it
    verifying: bool,     // host: Steam has the client's ticket and has not confirmed it yet
    verified: bool,      // host: Steam confirmed the ticket; waiting for lobby membership
    auth_begun: bool,    // host: EndAuthSession is due when the link closes
    ticket: Option<u32>, // client: Steam ticket handle to cancel when the link closes
    opened_at: u64,
    last_recv: u64,
    last_ping: u64,
    srtt_ms: f32,
    rttvar_ms: f32,
    has_rtt: bool,
    retransmits: u64,
    fragments_sent: u64,

    send_base: u32, // oldest unacked sequence number
    send_sent: u32, // fragments before this one have been transmitted
    send_next: u32,
    cwnd: f32,
    ssthresh: f32,
    recovery: u32, // no further window cut for losses before this sequence number
    send: Vec<SendSlot>, // WINDOW slots, indexed by seq % WINDOW

    recv_next: u32,
    recv: Vec<RecvSlot>,
    ack_due: bool,

    message: Option<Vec<u8>>, // reassembly of the current reliable message
    message_size: u32,
    message_from: u64,
    message_to: u64,
    message_channel: u8,
}

impl Link {
    fn new(addr: SocketAddrV4) -> Box<Link> {
        let mut send = Vec::with_capacity(WINDOW as usize);
        send.resize_with(WINDOW as usize, || SendSlot {
            used: false,
            acked: false,
            retransmitted: false,
            backoff: 0,
            nacks: 0,
            size: 0,
            sent_at: 0,
            datagram: [0; REL_HEADER + FRAGMENT_SIZE],
        });
        let mut recv = Vec::with_capacity(WINDOW as usize);
        recv.resize_with(WINDOW as usize, || RecvSlot { have: false, flags: 0, size: 0, data: [0; FRAGMENT_SIZE] });
        Box::new(Link {
            welcomed: false,
            confirmed: false,
            addr,
            steam_id: 0,
            session: 0,
            tx: None,
            rx: None,
            secret: None,
            handshake: Vec::new(),
            peer_hello: Vec::new(),
            verifying: false,
            verified: false,
            auth_begun: false,
            ticket: None,
            opened_at: now_ms(),
            last_recv: now_ms(),
            last_ping: 0,
            srtt_ms: 100.0,
            rttvar_ms: 50.0,
            has_rtt: false,
            retransmits: 0,
            fragments_sent: 0,
            send_base: 0,
            send_sent: 0,
            send_next: 0,
            cwnd: INITIAL_CWND,
            ssthresh: WINDOW as f32,
            recovery: 0,
            send,
            recv_next: 0,
            recv,
            ack_due: false,
            message: None,
            message_size: 0,
            message_from: 0,
            message_to: 0,
            message_channel: 0,
        })
    }

    fn rto_ms(&self) -> u64 {
        ((self.srtt_ms + 4.0 * self.rttvar_ms) as u64).clamp(MIN_RTO_MS, MAX_RTO_MS)
    }

    fn rtt_sample(&mut self, sample_ms: f32) {
        if !self.has_rtt {
            self.srtt_ms = sample_ms;
            self.rttvar_ms = sample_ms / 2.0;
            self.has_rtt = true;
        } else {
            self.rttvar_ms = 0.75 * self.rttvar_ms + 0.25 * (self.srtt_ms - sample_ms).abs();
            self.srtt_ms = 0.875 * self.srtt_ms + 0.125 * sample_ms;
        }
    }

    fn transmit(&mut self, sock: &Socket, kind: u8, body: &[u8]) {
        seal_send(&mut self.tx, self.session, sock, self.addr, kind, body);
    }

    fn unacked(&self) -> u32 {
        self.send_next.wrapping_sub(self.send_base)
    }

    fn in_flight(&self) -> u32 {
        self.send_sent.wrapping_sub(self.send_base)
    }

    /// Transmits queued fragments while the congestion window has room.
    fn flush(&mut self, sock: &Socket, now: u64) {
        while self.send_sent != self.send_next && self.in_flight() < self.cwnd as u32 {
            let slot = &mut self.send[(self.send_sent % WINDOW) as usize];
            slot.sent_at = now;
            self.fragments_sent += 1;
            seal_send(&mut self.tx, self.session, sock, self.addr, T_REL, &slot.datagram[..slot.size as usize]);
            self.send_sent = self.send_sent.wrapping_add(1);
        }
    }

    fn on_fragment_acked(&mut self) {
        self.cwnd += if self.cwnd < self.ssthresh { 1.0 } else { 1.0 / self.cwnd };
        self.cwnd = self.cwnd.min(WINDOW as f32);
    }

    fn on_loss(&mut self, seq: u32, timeout: bool) {
        if timeout {
            self.ssthresh = (self.cwnd / 2.0).max(MIN_CWND);
            self.cwnd = MIN_CWND;
            self.recovery = self.send_sent;
        } else if seq.wrapping_sub(self.recovery) < WINDOW * 2 {
            // Loss at or after the recovery point: one cut per window of data.
            self.cwnd = (self.cwnd / 2.0).max(MIN_CWND);
            self.ssthresh = self.cwnd;
            self.recovery = self.send_sent;
        }
    }

    fn reliable_send(&mut self, sock: &Socket, from: u64, to: u64, channel: u8, data: &[u8]) -> bool {
        let total = MESSAGE_HEADER + data.len();
        let fragments = total.div_ceil(FRAGMENT_SIZE) as u32;
        if self.unacked() + fragments > WINDOW {
            return false;
        }
        let mut header = [0u8; MESSAGE_HEADER];
        header[..8].copy_from_slice(&from.to_le_bytes());
        header[8..16].copy_from_slice(&to.to_le_bytes());
        header[16] = channel;
        header[17..21].copy_from_slice(&(data.len() as u32).to_le_bytes());

        let now = now_ms();
        let mut offset = 0; // position in header + data
        for i in 0..fragments {
            let seq = self.send_next;
            self.send_next = self.send_next.wrapping_add(1);
            let slot = &mut self.send[(seq % WINDOW) as usize];
            let chunk = (total - offset).min(FRAGMENT_SIZE);
            let out = &mut slot.datagram;
            out[..4].copy_from_slice(&seq.to_le_bytes());
            out[4] = (if i == 0 { FLAG_FIRST } else { 0 }) | (if i + 1 == fragments { FLAG_LAST } else { 0 });
            for k in 0..chunk {
                out[REL_HEADER + k] =
                    if offset < MESSAGE_HEADER { header[offset] } else { data[offset - MESSAGE_HEADER] };
                offset += 1;
            }
            slot.size = (REL_HEADER + chunk) as u16;
            slot.used = true;
            slot.acked = false;
            slot.retransmitted = false;
            slot.backoff = 0;
            slot.nacks = 0;
        }
        self.flush(sock, now);
        true
    }

    fn reliable_fragment(&mut self, mut fragment: &[u8], flags: u8, out: &mut Vec<Delivery>) {
        if flags & FLAG_FIRST != 0 {
            self.message = None;
            if fragment.len() < MESSAGE_HEADER {
                return;
            }
            let size = get32(&fragment[17..]);
            if size > MAX_MESSAGE {
                return;
            }
            self.message_from = get64(fragment);
            self.message_to = get64(&fragment[8..]);
            self.message_channel = fragment[16];
            self.message_size = size;
            self.message = Some(Vec::with_capacity(size as usize));
            fragment = &fragment[MESSAGE_HEADER..];
        }
        let Some(message) = self.message.as_mut() else { return };
        let room = self.message_size as usize - message.len();
        message.extend_from_slice(&fragment[..fragment.len().min(room)]);
        if flags & FLAG_LAST != 0 {
            let data = self.message.take().unwrap();
            out.push(Delivery { from: self.message_from, to: self.message_to, channel: self.message_channel, data });
        }
    }

    fn on_rel(&mut self, body: &[u8], out: &mut Vec<Delivery>) {
        if body.len() < 5 {
            return;
        }
        let seq = get32(body);
        let flags = body[4];
        let fragment = &body[5..];
        self.ack_due = true;
        if seq.wrapping_sub(self.recv_next) >= WINDOW || fragment.len() > FRAGMENT_SIZE {
            return; // duplicate or outside the window
        }
        let slot = &mut self.recv[(seq % WINDOW) as usize];
        if slot.have {
            return;
        }
        slot.have = true;
        slot.flags = flags;
        slot.size = fragment.len() as u16;
        slot.data[..fragment.len()].copy_from_slice(fragment);
        loop {
            let index = (self.recv_next % WINDOW) as usize;
            if !self.recv[index].have {
                break;
            }
            self.recv[index].have = false;
            self.recv_next = self.recv_next.wrapping_add(1);
            let (flags, size) = (self.recv[index].flags, self.recv[index].size as usize);
            let data = self.recv[index].data;
            self.reliable_fragment(&data[..size], flags, out);
        }
    }

    fn send_ack(&mut self, sock: &Socket) {
        let mut bitmap = 0u64;
        for i in 0..ACK_BITS {
            if self.recv[(self.recv_next.wrapping_add(1 + i) % WINDOW) as usize].have {
                bitmap |= 1 << i;
            }
        }
        let mut body = [0u8; 12];
        body[..4].copy_from_slice(&self.recv_next.to_le_bytes());
        body[4..].copy_from_slice(&bitmap.to_le_bytes());
        self.transmit(sock, T_ACK, &body);
        self.ack_due = false;
    }

    fn on_ack(&mut self, sock: &Socket, body: &[u8]) {
        if body.len() < 12 {
            return;
        }
        let next_expected = get32(body);
        let bitmap = get64(&body[4..]);
        if next_expected.wrapping_sub(self.send_base) > self.in_flight() {
            return; // stale
        }
        let now = now_ms();
        while self.send_base != next_expected {
            let slot = &mut self.send[(self.send_base % WINDOW) as usize];
            let newly = slot.used && !slot.acked;
            let sample = (newly && !slot.retransmitted).then(|| (now - slot.sent_at) as f32);
            slot.used = false;
            self.send_base = self.send_base.wrapping_add(1);
            if let Some(sample) = sample {
                self.rtt_sample(sample);
            }
            if newly {
                self.on_fragment_acked();
            }
        }
        let mut highest = None; // offset past next_expected of the newest fragment the peer has
        for i in 0..ACK_BITS {
            let seq = next_expected.wrapping_add(1 + i);
            if bitmap & (1 << i) != 0 && seq.wrapping_sub(self.send_base) < self.in_flight() {
                let slot = &mut self.send[(seq % WINDOW) as usize];
                if !slot.acked {
                    let sample = (!slot.retransmitted).then(|| (now - slot.sent_at) as f32);
                    slot.acked = true;
                    if let Some(sample) = sample {
                        self.rtt_sample(sample);
                    }
                    self.on_fragment_acked();
                }
                highest = Some(i + 1);
            }
        }
        // Fragments before the newest received one are missing: a gap reported often enough is a loss.
        let Some(highest) = highest else {
            self.flush(sock, now);
            return;
        };
        for i in 0..highest {
            let seq = next_expected.wrapping_add(i);
            let slot = &mut self.send[(seq % WINDOW) as usize];
            if !slot.used || slot.acked {
                continue;
            }
            slot.nacks += 1;
            if slot.nacks >= FAST_RETRANSMIT_NACKS {
                slot.nacks = 0;
                slot.retransmitted = true;
                slot.sent_at = now;
                self.retransmits += 1;
                seal_send(&mut self.tx, self.session, sock, self.addr, T_REL, &slot.datagram[..slot.size as usize]);
                self.on_loss(seq, false);
            }
        }
        self.flush(sock, now);
    }

    fn retransmit(&mut self, sock: &Socket, now: u64) {
        let rto = self.rto_ms();
        // Tail loss probe: nothing after the newest fragment can reveal its loss, so it is sent once more after
        // about one RTT instead of waiting for the full timeout.
        if self.in_flight() > 0 && self.send_sent == self.send_next {
            let probe_ms = (self.srtt_ms + 2.0 * self.rttvar_ms) as u64 + 5;
            let seq = self.send_sent.wrapping_sub(1);
            let slot = &mut self.send[(seq % WINDOW) as usize];
            if slot.used && !slot.acked && !slot.retransmitted && now - slot.sent_at >= probe_ms.min(rto) {
                slot.sent_at = now;
                slot.retransmitted = true;
                self.retransmits += 1;
                seal_send(&mut self.tx, self.session, sock, self.addr, T_REL, &slot.datagram[..slot.size as usize]);
            }
        }
        let mut seq = self.send_base;
        let mut timed_out = None;
        while seq != self.send_sent {
            let slot = &mut self.send[(seq % WINDOW) as usize];
            if slot.used && !slot.acked && now - slot.sent_at >= (rto << slot.backoff).min(MAX_RTO_MS * 4) {
                slot.sent_at = now;
                slot.retransmitted = true;
                slot.nacks = 0;
                slot.backoff = (slot.backoff + 1).min(MAX_BACKOFF);
                self.retransmits += 1;
                seal_send(&mut self.tx, self.session, sock, self.addr, T_REL, &slot.datagram[..slot.size as usize]);
                timed_out.get_or_insert(seq);
            }
            seq = seq.wrapping_add(1);
        }
        if let Some(seq) = timed_out {
            self.on_loss(seq, true);
        }
        self.flush(sock, now);
    }

    fn send_to(&mut self, sock: &Socket, from: u64, to: u64, channel: u8, data: &[u8], reliable: bool) -> bool {
        if !reliable && data.len() <= MAX_DATAGRAM - SEAL_OVERHEAD - 1 - DATA_HEADER {
            let mut body = Vec::with_capacity(DATA_HEADER + data.len());
            body.extend_from_slice(&from.to_le_bytes());
            body.extend_from_slice(&to.to_le_bytes());
            body.push(channel);
            body.extend_from_slice(data);
            self.transmit(sock, T_DATA, &body);
            return true;
        }
        self.reliable_send(sock, from, to, channel, data)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Transport state.

pub struct Message {
    pub from: u64,
    pub channel: i32,
    pub data: Vec<u8>,
}

struct State {
    role: Role,
    sock: Socket,
    links: [Option<Box<Link>>; MAX_LINKS],
    roster: Vec<u64>,
    lobby: u64,
    last_roster_sent: u64,
    roster_dirty: bool,

    // Client state.
    host_addr: SocketAddrV4,
    connect_started: u64,
    reconnect_deadline: u64, // nonzero while a lost host connection is being restored
    last_hello: u64,
    joined_lobby: u64,
    pending_join_lobby: u64,

    inbox: VecDeque<Message>,
    inbox_bytes: u32,

    password: String, // empty: players are verified through Steam
    psk: crypto::Psk,
    last_reject_log: Option<u64>,
    lobby_members: Vec<u64>, // host: members of its lobby, refreshed by lobby_tick
    auto_hosting: bool,      // hosting because this player owns the lobby (host=auto)
    host_override: Option<bool>, // F11: hosting forced on or off, instead of following lobby ownership

    // Rendezvous through the Steam lobby.
    stun: Vec<StunProbe>,
    stun_started: bool,
    lan: Vec<SocketAddrV4>,
    host_candidates: Vec<SocketAddrV4>,   // client: every address the host offered
    punch_targets: Vec<(u64, Vec<SocketAddrV4>)>, // host: lobby members and their addresses
    last_punch: u64,
    last_lobby_tick: u64,
    published: String,       // what this player last put into the lobby
    join_wanted: Option<String>, // a "join" command (with its password, empty for none) waiting for an offer
    join_tried_lobby: u64,
    join_hint_lobby: u64,
}

struct StunProbe {
    server: SocketAddrV4,
    transaction: [u8; 12],
    tries: u8,
    last: u64,
    result: Option<SocketAddrV4>,
}

static STATE: Mutex<State> = Mutex::new(State {
    role: Role::None,
    sock: None,
    links: [const { None }; MAX_LINKS],
    roster: Vec::new(),
    lobby: 0,
    last_roster_sent: 0,
    roster_dirty: false,
    host_addr: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
    connect_started: 0,
    reconnect_deadline: 0,
    last_hello: 0,
    joined_lobby: 0,
    pending_join_lobby: 0,
    inbox: VecDeque::new(),
    inbox_bytes: 0,
    password: String::new(),
    psk: [0; 32],
    last_reject_log: None,
    lobby_members: Vec::new(),
    auto_hosting: false,
    host_override: None,
    stun: Vec::new(),
    stun_started: false,
    lan: Vec::new(),
    host_candidates: Vec::new(),
    punch_targets: Vec::new(),
    last_punch: 0,
    last_lobby_tick: 0,
    published: String::new(),
    join_wanted: None,
    join_tried_lobby: 0,
    join_hint_lobby: 0,
});
/// STUN servers, resolved once in the background at start-up.
static STUN_SERVERS: std::sync::OnceLock<Vec<SocketAddrV4>> = std::sync::OnceLock::new();

/// Steam session tickets, provided by `hooks` once Steam is up.
pub trait SteamAuth: Send + Sync {
    /// A ticket for this player and its handle.
    fn ticket(&self) -> Option<(u32, Vec<u8>)>;
    /// Hands a player's ticket to Steam; the answer arrives through `auth_result`.
    fn begin(&self, ticket: &[u8], steam_id: u64) -> bool;
    fn end(&self, steam_id: u64);
    fn cancel(&self, handle: u32);
}

static AUTH: std::sync::OnceLock<Box<dyn SteamAuth>> = std::sync::OnceLock::new();

pub fn set_auth(auth: Box<dyn SteamAuth>) {
    let _ = AUTH.set(auth);
}
static STARTED: AtomicBool = AtomicBool::new(false);
static MY_ID: AtomicU64 = AtomicU64::new(0);

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn my_id() -> u64 {
    MY_ID.load(Ordering::Relaxed)
}

fn started() -> bool {
    STARTED.load(Ordering::Acquire)
}

impl State {
    fn link(&mut self, index: usize) -> &mut Link {
        self.links[index].as_mut().unwrap()
    }

    fn link_by_addr(&self, addr: SocketAddrV4) -> Option<usize> {
        self.links.iter().position(|l| l.as_ref().is_some_and(|l| l.addr == addr))
    }

    fn link_by_id(&self, id: u64) -> Option<usize> {
        self.links.iter().position(|l| l.as_ref().is_some_and(|l| l.welcomed && l.steam_id == id))
    }

    fn link_open(&mut self, addr: SocketAddrV4) -> Option<usize> {
        let index = self.links.iter().position(Option::is_none)?;
        self.links[index] = Some(Link::new(addr));
        Some(index)
    }

    fn link_close(&mut self, index: usize, reason: &str) {
        let Some(link) = self.links[index].take() else { return };
        if link.welcomed {
            log!("transport: {} ({}) disconnected: {}", link.steam_id, link.addr, reason);
        }
        if let Some(auth) = AUTH.get() {
            if link.auth_begun {
                auth.end(link.steam_id);
            }
            if let Some(handle) = link.ticket {
                auth.cancel(handle);
            }
        }
        self.roster_dirty = true;
    }

    fn inbox_push(&mut self, from: u64, channel: u8, data: Vec<u8>) {
        let size = data.len() as u32;
        if self.inbox_bytes + size > MAX_QUEUED_BYTES {
            log!("transport: inbox full, dropping {} bytes from {}", size, from);
            return;
        }
        self.inbox.push_back(Message { from, channel: channel as i32, data });
        self.inbox_bytes += size;
    }

    fn rebuild_roster(&mut self) {
        self.roster.clear();
        self.roster.push(my_id());
        for link in self.links.iter().flatten() {
            if link.welcomed {
                self.roster.push(link.steam_id);
            }
        }
    }

    fn deliver_or_forward(&mut self, source: usize, mut from: u64, to: u64, channel: u8, data: Vec<u8>, reliable: bool) {
        if self.role == Role::Host {
            from = self.link(source).steam_id; // clients cannot speak for someone else
        }
        if to == my_id() {
            self.inbox_push(from, channel, data);
            return;
        }
        if self.role != Role::Host {
            return;
        }
        let Some(target) = self.link_by_id(to).filter(|&t| t != source) else { return };
        let sock = &self.sock;
        let link = self.links[target].as_mut().unwrap();
        if !link.send_to(sock, from, to, channel, &data, reliable) {
            log!("transport: send window to {} full, dropping a forwarded message", to);
        }
    }

    fn send_roster(&mut self) {
        self.rebuild_roster();
        let mut body = Vec::with_capacity(9 + 8 * self.roster.len());
        body.extend_from_slice(&self.lobby.to_le_bytes());
        body.push(self.roster.len() as u8);
        for id in &self.roster {
            body.extend_from_slice(&id.to_le_bytes());
        }
        let State { links, sock, .. } = self;
        for link in links.iter_mut().flatten() {
            if link.welcomed {
                link.transmit(sock, T_ROSTER, &body);
            }
        }
        self.last_roster_sent = now_ms();
        self.roster_dirty = false;
    }

    /// Logs a rejected handshake at most every REJECT_LOG_INTERVAL_MS.
    fn reject(&mut self, args: std::fmt::Arguments) {
        let now = now_ms();
        if self.last_reject_log.is_none_or(|last| now - last >= REJECT_LOG_INTERVAL_MS) {
            self.last_reject_log = Some(now);
            log!("transport: {}", args);
        }
    }

    fn on_datagram(&mut self, from_addr: SocketAddrV4, data: &[u8]) {
        if stun::is_response(data) {
            self.on_stun(from_addr, data);
            return;
        }
        if data.len() < 2 || data[0] != MAGIC {
            return;
        }
        match data[1] {
            T_HELLO if self.role == Role::Host => self.on_hello(from_addr, data),
            T_WELCOME if self.role == Role::Client => self.on_welcome(from_addr, data),
            T_SEALED => self.on_sealed(from_addr, data),
            _ => {}
        }
    }

    fn on_hello(&mut self, from_addr: SocketAddrV4, hello: &[u8]) {
        if hello.len() < HANDSHAKE_SIGNED + 2 + crypto::MAC_SIZE {
            return;
        }
        let version = get32(&hello[2..]);
        if version != PROTOCOL_VERSION {
            self.reject(format_args!(
                "{} speaks protocol {}, this is {}; both players need the same isaac-p2p",
                from_addr, version, PROTOCOL_VERSION
            ));
            return;
        }
        let ticket_len = u16::from_le_bytes([hello[HANDSHAKE_SIGNED], hello[HANDSHAKE_SIGNED + 1]]) as usize;
        let signed_len = HANDSHAKE_SIGNED + 2 + ticket_len;
        if ticket_len > MAX_TICKET || hello.len() < signed_len + crypto::MAC_SIZE {
            return;
        }
        let hello = &hello[..signed_len + crypto::MAC_SIZE];
        if !crypto::verify(&self.psk, &[&hello[..signed_len]], &hello[signed_len..]) {
            self.reject(format_args!(
                "hello from {} with a different password{}",
                from_addr,
                if self.password.is_empty() { " (this host verifies players through Steam: no password)" } else { "" }
            ));
            return;
        }
        let id = get64(&hello[6..]);
        let session = get32(&hello[14..]);
        let ticket = &hello[HANDSHAKE_SIGNED + 2..signed_len];
        let now = now_ms();

        // Also a link still being verified: a client says hello on every path to the host at once.
        let existing = self
            .link_by_addr(from_addr)
            .or_else(|| self.links.iter().position(|l| l.as_ref().is_some_and(|l| l.steam_id == id)));
        if let Some(i) = existing {
            let link = self.link(i);
            if link.session == session {
                // A repeated HELLO, possibly on a slower path: the link stays on the path the first one took.
                if link.welcomed {
                    let (addr, welcome) = (link.addr, link.handshake.clone());
                    raw_send(&self.sock, addr, &welcome);
                }
                return; // still being verified otherwise
            }
            if link.confirmed && now - link.last_recv < REPLACE_QUIET_MS {
                self.reject(format_args!("ignored a new hello for {} while its session is active", id));
                return;
            }
            self.link_close(i, "reconnected");
        }

        let steam_mode = self.password.is_empty();
        if steam_mode && ticket.is_empty() {
            self.reject(format_args!("{} sent no Steam ticket; it needs this host's password", from_addr));
            return;
        }
        let Some(index) = self.link_open(from_addr) else {
            log!("transport: no free slot for {}", from_addr);
            return;
        };
        let link = self.link(index);
        link.steam_id = id;
        link.session = session;
        link.peer_hello = hello.to_vec();
        if !steam_mode {
            self.finish_handshake(index);
            return;
        }
        let begun = AUTH.get().is_some_and(|auth| auth.begin(ticket, id));
        if !begun {
            self.links[index] = None;
            self.reject(format_args!("Steam refused the ticket of {} from {}", id, from_addr));
            return;
        }
        let link = self.link(index);
        link.verifying = true;
        link.auth_begun = true;
        log!("transport: {} says hello from {}, verifying with Steam", id, from_addr);
    }

    /// Answers a client's HELLO: keys, WELCOME, and the client joins the roster.
    fn finish_handshake(&mut self, index: usize) {
        let psk = self.psk;
        let link = self.link(index);
        let hello = std::mem::take(&mut link.peer_hello);
        let signed = &hello[..hello.len() - crypto::MAC_SIZE];
        let client_public: [u8; 32] = hello[18..50].try_into().unwrap();
        let (secret, host_public) = crypto::new_secret();
        let Some((c2h, h2c)) = crypto::session_keys(&psk, secret, client_public, &client_public, &host_public, link.session)
        else {
            self.link_close(index, "bad key");
            return;
        };
        let mut welcome = Vec::with_capacity(HANDSHAKE_SIZE);
        welcome.extend_from_slice(&[MAGIC, T_WELCOME]);
        welcome.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        welcome.extend_from_slice(&my_id().to_le_bytes());
        welcome.extend_from_slice(&link.session.to_le_bytes());
        welcome.extend_from_slice(&host_public);
        let tag = crypto::mac(&psk, &[signed, &welcome]);
        welcome.extend_from_slice(&tag);
        link.verifying = false;
        link.welcomed = true;
        link.last_recv = now_ms();
        link.tx = Some(crypto::Sealer::new(&h2c));
        link.rx = Some(crypto::Opener::new(&c2h));
        link.handshake = welcome;
        let (id, addr, welcome) = (link.steam_id, link.addr, link.handshake.clone());
        raw_send(&self.sock, addr, &welcome);
        if self.password.is_empty() {
            log!("transport: {} verified by Steam, answering at {}", id, addr);
        } else {
            log!("transport: {} says hello from {}, answering", id, addr);
        }
        self.roster_dirty = true;
    }

    /// A verified player is let in once it shows up among the lobby's members.
    fn admit_verified(&mut self) {
        for i in 0..MAX_LINKS {
            let ready = self.links[i]
                .as_ref()
                .is_some_and(|l| l.verifying && l.verified && self.lobby_members.contains(&l.steam_id));
            if ready {
                self.finish_handshake(i);
            }
        }
    }

    fn on_welcome(&mut self, from_addr: SocketAddrV4, welcome: &[u8]) {
        let reconnecting = self.reconnect_deadline != 0;
        let psk = self.psk;
        let offered = self.host_candidates.contains(&from_addr);
        let Some(link) = self.links[0].as_mut() else { return };
        if link.welcomed || !(from_addr == link.addr || offered) || welcome.len() < HANDSHAKE_SIZE {
            return;
        }
        if get32(&welcome[2..]) != PROTOCOL_VERSION || get32(&welcome[14..]) != link.session {
            return;
        }
        let hello = &link.handshake[..link.handshake.len() - crypto::MAC_SIZE];
        let signed = &welcome[..HANDSHAKE_SIGNED];
        if !crypto::verify(&psk, &[hello, signed], &welcome[HANDSHAKE_SIGNED..HANDSHAKE_SIZE]) {
            return;
        }
        let host_public: [u8; 32] = welcome[18..50].try_into().unwrap();
        let client_public: [u8; 32] = hello[18..50].try_into().unwrap();
        let Some(secret) = link.secret.take() else { return };
        let Some((c2h, h2c)) =
            crypto::session_keys(&psk, secret, host_public, &client_public, &host_public, link.session)
        else {
            return;
        };
        link.tx = Some(crypto::Sealer::new(&c2h));
        link.rx = Some(crypto::Opener::new(&h2c));
        link.welcomed = true;
        link.addr = from_addr;
        link.steam_id = get64(&welcome[6..]);
        link.last_recv = now_ms();
        if reconnecting {
            log!("transport: reconnected to host {} at {}", link.steam_id, link.addr);
        } else {
            log!("transport: connected to host {} at {}", link.steam_id, link.addr);
        }
        self.reconnect_deadline = 0;
        self.host_addr = from_addr;
        // The host's roster follows; until then, the host itself is reachable over the transport already.
        let host_id = get64(&welcome[6..]);
        if !self.roster.contains(&host_id) {
            self.roster.push(host_id);
        }
    }

    fn on_sealed(&mut self, from_addr: SocketAddrV4, datagram: &[u8]) {
        if datagram.len() < crypto::SEALED_HEADER {
            return;
        }
        let session = get32(&datagram[2..]);
        let Some(i) =
            self.links.iter().position(|l| l.as_ref().is_some_and(|l| l.session == session && l.rx.is_some()))
        else {
            return;
        };
        let role = self.role;
        let link = self.link(i);
        let Some(plain) = link.rx.as_mut().unwrap().open(datagram) else { return };
        if plain.is_empty() {
            return;
        }
        link.last_recv = now_ms();
        if link.addr != from_addr {
            log!("transport: {} moved from {} to {}", link.steam_id, link.addr, from_addr);
            link.addr = from_addr;
            if role == Role::Client {
                self.host_addr = from_addr;
            }
        }
        let link = self.link(i);
        if role == Role::Host && !link.confirmed {
            link.confirmed = true;
            log!("transport: {} connected from {}", link.steam_id, link.addr);
        }
        self.on_packet(i, plain[0], &plain[1..]);
    }

    /// An authentic packet from link `i`.
    fn on_packet(&mut self, i: usize, kind: u8, body: &[u8]) {
        let role = self.role;
        match kind {
            T_ROSTER => {
                if role == Role::Client && body.len() >= 9 {
                    let host_lobby = get64(body);
                    let count = body[8] as usize;
                    if count > MAX_LINKS + 1 || body.len() < 9 + 8 * count {
                        return;
                    }
                    self.roster = (0..count).map(|k| get64(&body[9 + 8 * k..])).collect();
                    if host_lobby != 0 && host_lobby != self.joined_lobby {
                        self.joined_lobby = host_lobby;
                        self.pending_join_lobby = host_lobby;
                        log!("transport: host is in lobby {}, joining", host_lobby);
                    }
                }
            }
            T_PING => {
                if body.len() >= 4 {
                    let sock = self.sock.clone();
                    self.link(i).transmit(&sock, T_PONG, &body[..4]);
                }
            }
            T_PONG => {
                if body.len() >= 4 {
                    let sample = (now_ms() as u32).wrapping_sub(get32(body)) as i32;
                    if (0..10000).contains(&sample) {
                        self.link(i).rtt_sample(sample as f32);
                    }
                }
            }
            T_BYE => {
                self.link_close(i, "left");
                if role == Role::Client {
                    self.role = Role::None;
                    self.roster.clear();
                }
            }
            T_DATA => {
                if body.len() >= DATA_HEADER {
                    self.deliver_or_forward(i, get64(body), get64(&body[8..]), body[16], body[17..].to_vec(), false);
                }
            }
            T_REL => {
                let mut deliveries = Vec::new();
                self.link(i).on_rel(body, &mut deliveries);
                for d in deliveries {
                    self.deliver_or_forward(i, d.from, d.to, d.channel, d.data, true);
                }
            }
            T_ACK => {
                let sock = self.sock.clone();
                self.link(i).on_ack(&sock, body);
            }
            _ => {}
        }
    }

    fn tick(&mut self, now: u64) {
        if self.role == Role::Client {
            let Some(host) = self.links[0].as_ref() else { return };
            if !host.welcomed {
                if self.reconnect_deadline != 0 && now > self.reconnect_deadline {
                    log!("transport: host {} did not come back; leaving the session", self.host_addr);
                    self.link_close(0, "timeout");
                    self.role = Role::None;
                    self.roster.clear();
                    self.reconnect_deadline = 0;
                    return;
                }
                if self.reconnect_deadline == 0 && now - self.connect_started > CONNECT_TIMEOUT_MS {
                    log!(
                        "transport: no answer from {} (host not running, port not forwarded, a different \
                         password, or the host's answers leave through a VPN)",
                        self.host_addr
                    );
                    self.link_close(0, "timeout");
                    self.role = Role::None;
                    return;
                }
                if now - self.last_hello >= HELLO_INTERVAL_MS {
                    raw_send(&self.sock, host.addr, &host.handshake);
                    for &addr in self.host_candidates.iter().filter(|&&a| a != host.addr) {
                        raw_send(&self.sock, addr, &host.handshake);
                    }
                    self.last_hello = now;
                }
                self.stun_tick(now);
                return;
            }
        }
        for i in 0..MAX_LINKS {
            let Some(link) = self.links[i].as_mut() else { continue };
            if link.verifying && now - link.opened_at > VERIFY_TIMEOUT_MS {
                let id = link.steam_id;
                let reason = if link.verified { "is not in this lobby" } else { "was not verified by Steam in time" };
                self.link_close(i, "not admitted");
                log!("transport: turned {} away: it {}", id, reason);
                continue;
            }
            if !link.welcomed {
                continue;
            }
            if now - link.last_recv > LINK_TIMEOUT_MS {
                if self.role == Role::Host && !link.confirmed {
                    log!(
                        "transport: {} only sent hello: the answers did not reach it. A VPN or a second route on \
                         this PC can send them out from another IP; route UDP port {} past it",
                        link.steam_id,
                        self.sock.as_ref().and_then(|s| s.local_addr().ok()).map_or(0, |a| a.port())
                    );
                }
                self.link_close(i, "timeout");
                if self.role == Role::Client {
                    log!("transport: lost the host, reconnecting for up to {} s", RECONNECT_TIMEOUT_MS / 1000);
                    self.open_host_link(now);
                    self.reconnect_deadline = now + RECONNECT_TIMEOUT_MS;
                    return;
                }
                continue;
            }
            if now - link.last_ping >= PING_INTERVAL_MS {
                link.transmit(&self.sock, T_PING, &(now as u32).to_le_bytes());
                link.last_ping = now;
            }
            link.retransmit(&self.sock, now);
            if link.ack_due {
                link.send_ack(&self.sock);
            }
        }
        if self.role == Role::Host && (self.roster_dirty || now - self.last_roster_sent >= ROSTER_INTERVAL_MS) {
            self.send_roster();
        }
        if self.role == Role::Host && now - self.last_punch >= HELLO_INTERVAL_MS {
            self.last_punch = now;
            for (member, addrs) in &self.punch_targets {
                if self.link_by_id(*member).is_none() {
                    for &addr in addrs {
                        raw_send(&self.sock, addr, &[MAGIC, T_PUNCH]);
                    }
                }
            }
        }
        self.stun_tick(now);
    }

    fn stun_tick(&mut self, now: u64) {
        if !self.stun_started {
            // Servers resolve in the background; probes start once they are known.
            let Some(servers) = STUN_SERVERS.get() else { return };
            self.stun_started = true;
            self.stun = servers
                .iter()
                .take(2)
                .map(|&server| {
                    let mut transaction = [0u8; 12];
                    for chunk in transaction.chunks_mut(4) {
                        chunk.copy_from_slice(&crypto::random_u32().to_le_bytes());
                    }
                    StunProbe { server, transaction, tries: 0, last: 0, result: None }
                })
                .collect();
        }
        let State { stun: probes, sock, .. } = self;
        for probe in probes.iter_mut() {
            if probe.result.is_none() && probe.tries < STUN_TRIES && now - probe.last >= STUN_INTERVAL_MS {
                probe.tries += 1;
                probe.last = now;
                if let Some(sock) = sock {
                    let _ = sock.send_to(&stun::request(&probe.transaction), probe.server);
                }
            }
        }
    }

    fn on_stun(&mut self, from_addr: SocketAddrV4, data: &[u8]) {
        let Some((transaction, public)) = stun::parse_response(data) else { return };
        let Some(probe) = self.stun.iter_mut().find(|p| p.transaction == transaction && p.server == from_addr) else {
            return;
        };
        if probe.result.is_some() {
            return;
        }
        probe.result = Some(public);
        log!("transport: public address {} (STUN {})", public, from_addr);
        let results: Vec<SocketAddrV4> = self.stun.iter().filter_map(|p| p.result).collect();
        if results.len() == 2 && results[0] != results[1] {
            log!(
                "transport: the NAT gives each destination its own port ({} vs {}); players outside this network \
                 can only reach this PC through a forwarded port or a VPN",
                results[0],
                results[1]
            );
        }
    }

    /// Addresses other players can try for this socket: `[transport] addresses` (forwarded ports) first, then
    /// public ones from STUN, then LAN ones, home networks (192.168/16, 10/8) before 172.16/12, where VPN and
    /// container bridges usually live.
    fn candidates(&self) -> Vec<SocketAddrV4> {
        let mut out: Vec<SocketAddrV4> = Vec::new();
        let fixed = parse_candidates(&settings::get().addresses);
        let mut lan = self.lan.clone();
        lan.sort_by_key(|a| match a.ip().octets() {
            [192, 168, ..] => 0,
            [10, ..] => 1,
            _ => 2,
        });
        let found = self.stun.iter().filter_map(|p| p.result).chain(lan);
        for addr in fixed.into_iter().chain(found) {
            if !out.contains(&addr) && out.len() < MAX_CANDIDATES {
                out.push(addr);
            }
        }
        out
    }

    /// Connects as a client to a host that offered its addresses in the lobby.
    fn connect_to(&mut self, host_id: u64, candidates: Vec<SocketAddrV4>, password: String) {
        self.close_all(true);
        if !self.open_client_socket() {
            return;
        }
        self.role = Role::Client;
        self.set_password(password);
        self.host_addr = candidates[0];
        log!("transport: connecting to host {} through the lobby: {}", host_id, format_candidates(&candidates));
        self.host_candidates = candidates;
        if !self.open_host_link(now_ms()) {
            self.close_all(false);
            return;
        }
        self.joined_lobby = self.lobby; // already in the host's lobby
    }

    /// Opens the client's link to the host (slot 0) with a fresh session.
    fn open_host_link(&mut self, now: u64) -> bool {
        let addr = self.host_addr;
        let Some(index) = self.link_open(addr) else { return false };
        let session = crypto::random_u32() | 1;
        let (secret, client_public) = crypto::new_secret();
        let ticket = if self.password.is_empty() {
            match AUTH.get().and_then(|auth| auth.ticket()) {
                Some(ticket) => Some(ticket),
                None => {
                    log!("transport: Steam gave no session ticket; connect with the host's password instead");
                    self.links[index] = None;
                    return false;
                }
            }
        } else {
            None
        };
        let mut hello = Vec::with_capacity(HANDSHAKE_SIZE + 2 + MAX_TICKET);
        hello.extend_from_slice(&[MAGIC, T_HELLO]);
        hello.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        hello.extend_from_slice(&my_id().to_le_bytes());
        hello.extend_from_slice(&session.to_le_bytes());
        hello.extend_from_slice(&client_public);
        let bytes = ticket.as_ref().map_or(&[][..], |(_, bytes)| &bytes[..bytes.len().min(MAX_TICKET)]);
        hello.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        hello.extend_from_slice(bytes);
        let tag = crypto::mac(&self.psk, &[&hello]);
        hello.extend_from_slice(&tag);
        let link = self.link(index);
        link.ticket = ticket.map(|(handle, _)| handle);
        link.session = session;
        link.secret = Some(secret);
        link.handshake = hello;
        self.connect_started = now;
        self.last_hello = 0;
        true
    }

    fn close_all(&mut self, say_bye: bool) {
        for i in 0..MAX_LINKS {
            if let Some(link) = self.links[i].as_mut() {
                if say_bye {
                    link.transmit(&self.sock, T_BYE, &[]);
                }
                self.link_close(i, "disconnect");
            }
        }
        self.sock = None;
        self.role = Role::None;
        self.roster.clear();
        self.joined_lobby = 0;
        self.pending_join_lobby = 0;
        self.reconnect_deadline = 0;
        self.inbox.clear();
        self.inbox_bytes = 0;
        self.stun.clear();
        self.lan.clear();
        self.host_candidates.clear();
        self.punch_targets.clear();
        self.published.clear();
    }

    /// A client uses `[transport] port` too when it is free, so one rule for that port (a router forward, a VPN
    /// exclusion) covers both roles; otherwise any port.
    fn open_client_socket(&mut self) -> bool {
        let port = settings::get().port;
        match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
            Ok(probe) => {
                drop(probe);
                self.open_socket(port) || self.open_socket(0)
            }
            Err(_) => self.open_socket(0),
        }
    }

    fn open_socket(&mut self, port: u16) -> bool {
        let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
            Ok(sock) => sock,
            Err(e) => {
                log!("transport: cannot bind UDP port {}: {}", port, e);
                return false;
            }
        };
        let _ = sock.set_nonblocking(true);
        let raw = sock.as_raw_socket() as usize;
        let size: i32 = 1 << 20;
        let off: u32 = 0;
        let mut returned = 0u32;
        unsafe {
            let size_ptr = (&size as *const i32).cast();
            win::setsockopt(raw, win::SOL_SOCKET, win::SO_RCVBUF, size_ptr, 4);
            win::setsockopt(raw, win::SOL_SOCKET, win::SO_SNDBUF, size_ptr, 4);
            // ICMP "port unreachable" from a peer that went away must not fail every following recvfrom.
            win::WSAIoctl(
                raw,
                win::SIO_UDP_CONNRESET,
                (&off as *const u32).cast(),
                4,
                core::ptr::null_mut(),
                0,
                &mut returned,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            );
        }
        let port = sock.local_addr().map_or(port, |a| a.port());
        self.lan = local_ips().into_iter().map(|ip| SocketAddrV4::new(ip, port)).collect();
        self.stun_started = false;
        self.sock = Some(Arc::new(sock));
        true
    }
}

/// This PC's IPv4 addresses for other players on the same network: private ones of every adapter first, then
/// the one the default route uses. No packet is sent.
fn local_ips() -> Vec<Ipv4Addr> {
    let mut ips: Vec<Ipv4Addr> = Vec::new();
    let mut name = [0u8; 256];
    if unsafe { win::gethostname(name.as_mut_ptr(), name.len() as i32) } == 0 {
        let len = name.iter().position(|&b| b == 0).unwrap_or(0);
        if let Ok(host) = std::str::from_utf8(&name[..len]) {
            if let Ok(addrs) = (host, 0).to_socket_addrs() {
                for addr in addrs {
                    if let SocketAddr::V4(a) = addr {
                        if a.ip().is_private() && !ips.contains(a.ip()) {
                            ips.push(*a.ip());
                        }
                    }
                }
            }
        }
    }
    let routed = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|probe| probe.connect((Ipv4Addr::new(8, 8, 8, 8), 53)).map(|_| probe))
        .and_then(|probe| probe.local_addr());
    if let Ok(SocketAddr::V4(a)) = routed {
        if !a.ip().is_unspecified() && !a.ip().is_loopback() && !ips.contains(a.ip()) {
            ips.push(*a.ip());
        }
    }
    ips
}

fn format_candidates(addrs: &[SocketAddrV4]) -> String {
    addrs.iter().map(SocketAddrV4::to_string).collect::<Vec<_>>().join(",")
}

fn parse_candidates(text: &str) -> Vec<SocketAddrV4> {
    text.split(',').filter_map(|a| a.trim().parse().ok()).take(MAX_CANDIDATES).collect()
}

/// "version;host steam id;addresses;steam|password" from the lobby, when it is this protocol's. The flag is
/// true when the host verifies players through Steam.
fn parse_offer(text: &str) -> Option<(u64, Vec<SocketAddrV4>, bool)> {
    let mut parts = text.split(';');
    if parts.next()?.parse::<u32>().ok()? != PROTOCOL_VERSION {
        return None;
    }
    let host = parts.next()?.parse().ok()?;
    let addrs = parse_candidates(parts.next()?);
    let steam = parts.next()? == "steam";
    (!addrs.is_empty()).then_some((host, addrs, steam))
}

/// Steam's answer to a ticket handed over with `SteamAuth::begin` (ValidateAuthTicketResponse_t).
pub fn auth_result(steam_id: u64, response: u32) {
    if !started() {
        return;
    }
    let mut st = state();
    let Some(i) = st.links.iter().position(|l| l.as_ref().is_some_and(|l| l.verifying && l.steam_id == steam_id))
    else {
        return;
    };
    if response != 0 {
        st.link_close(i, "not verified");
        log!("transport: Steam did not verify {} (response {}); turned away", steam_id, response);
        return;
    }
    st.link(i).verified = true;
    if !st.lobby_members.contains(&steam_id) {
        log!("transport: {} verified by Steam, waiting for it to appear in the lobby", steam_id);
    }
    st.admit_verified();
}

/// The game's Steam lobby, as the rendezvous needs it. Called on the game thread.
pub trait LobbyApi {
    fn data(&self, lobby: u64) -> String;
    fn set_data(&self, lobby: u64, value: &str);
    fn members(&self, lobby: u64) -> Vec<u64>;
    fn member_data(&self, lobby: u64, member: u64) -> String;
    fn set_member_data(&self, lobby: u64, value: &str);
    fn owner(&self, lobby: u64) -> u64;
}

/// Publishes this player's addresses in the lobby and picks up the other side's; joins a host's offer when
/// asked to (`join`) or when `[transport] join=1` (and, for a host with a password, the password is set).
pub fn lobby_tick(api: &dyn LobbyApi) {
    if !started() {
        return;
    }
    let mut st = state();
    let now = now_ms();
    let lobby = st.lobby;
    if lobby == 0 || now - st.last_lobby_tick < LOBBY_TICK_MS {
        return;
    }
    st.last_lobby_tick = now;
    if st.host_override.is_none() && settings::get().host_mode == settings::HostMode::Auto {
        let owner = api.owner(lobby) == my_id();
        if owner && st.role == Role::None {
            log!("transport: you own lobby {}, hosting (F11 stops)", lobby);
            st.auto_hosting = st.start_hosting(settings::get().port, settings::get().password.clone());
        } else if !owner && st.role == Role::Host && st.auto_hosting {
            log!("transport: no longer the owner of lobby {}, stopped hosting", lobby);
            st.close_all(true);
        }
    }
    let mine = format_candidates(&st.candidates());
    match st.role {
        Role::Host => {
            if !mine.is_empty() {
                let mode = if st.password.is_empty() { "steam" } else { "password" };
                let offer = format!("{};{};{};{}", PROTOCOL_VERSION, my_id(), mine, mode);
                if offer != st.published {
                    api.set_data(lobby, &offer);
                    log!("transport: offering a direct connection in lobby {}: {}", lobby, mine);
                    st.published = offer;
                }
            }
            let version = PROTOCOL_VERSION.to_string();
            st.lobby_members = api.members(lobby);
            st.admit_verified();
            st.punch_targets = st
                .lobby_members
                .clone()
                .into_iter()
                .filter(|&member| member != my_id())
                .filter_map(|member| {
                    let data = api.member_data(lobby, member);
                    let (v, addrs) = data.split_once(';')?;
                    let addrs = parse_candidates(addrs);
                    (v == version && !addrs.is_empty()).then_some((member, addrs))
                })
                .collect();
        }
        Role::Client => {
            if !mine.is_empty() && mine != st.published {
                api.set_member_data(lobby, &format!("{};{}", PROTOCOL_VERSION, mine));
                st.published = mine;
            }
        }
        Role::None => {
            if st.join_tried_lobby == lobby {
                return;
            }
            let Some((host_id, addrs, steam)) = parse_offer(&api.data(lobby)) else { return };
            if host_id == my_id() {
                return;
            }
            let settings = settings::get();
            let wanted = st.join_wanted.take();
            if wanted.is_none() && !settings.auto_join {
                return;
            }
            // A Steam-verifying host needs no password; a host with one needs it.
            let password = if steam {
                Some(String::new())
            } else {
                wanted.filter(|p| !p.is_empty()).or_else(|| Some(settings.password.clone()).filter(|p| !p.is_empty()))
            };
            match password {
                Some(password) => {
                    st.join_tried_lobby = lobby;
                    st.connect_to(host_id, addrs, password);
                }
                None if st.join_hint_lobby != lobby => {
                    st.join_hint_lobby = lobby;
                    log!("transport: lobby {} offers a direct connection with a password; type: join <password>", lobby);
                }
                None => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Network thread.

fn wait_readable(sock: &UdpSocket, timeout: Duration) -> bool {
    let mut set = win::FdSet { count: 1, sockets: [0; 64] };
    set.sockets[0] = sock.as_raw_socket() as usize;
    let timeout = win::Timeval { sec: 0, usec: timeout.as_micros() as i32 };
    unsafe { win::select(0, &mut set, core::ptr::null_mut(), core::ptr::null_mut(), &timeout) > 0 }
}

fn network_thread() {
    let mut buffer = [0u8; 2048];
    loop {
        let Some(sock) = state().sock.clone() else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        let ready = wait_readable(&sock, Duration::from_millis(2));

        let mut st = state();
        if !st.sock.as_ref().is_some_and(|s| Arc::ptr_eq(s, &sock)) {
            continue; // closed or replaced while waiting
        }
        if ready {
            loop {
                match sock.recv_from(&mut buffer) {
                    Ok((size, SocketAddr::V4(from))) if size > 0 => st.on_datagram(from, &buffer[..size]),
                    Ok((_, SocketAddr::V6(_))) => continue,
                    _ => break,
                }
            }
        }
        if st.sock.is_some() {
            st.tick(now_ms());
            let State { links, sock, .. } = &mut *st;
            for link in links.iter_mut().flatten() {
                if link.ack_due {
                    link.send_ack(sock);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Commands.

fn require_started() -> bool {
    if !started() {
        log!("transport: Steam is not initialised yet, try again in a moment");
    }
    started()
}

impl State {
    fn set_password(&mut self, password: String) {
        self.psk = crypto::psk(&password);
        self.password = password;
    }
}

impl State {
    fn start_hosting(&mut self, port: u16, password: String) -> bool {
        self.close_all(true);
        if !self.open_socket(port) {
            return false;
        }
        self.role = Role::Host;
        self.set_password(password);
        if self.password.is_empty() {
            log!("transport: players are verified through Steam and must be in your lobby (no password)");
        } else {
            log!("transport: password {} (clients: connect <this ip> {})", self.password, self.password);
        }
        self.rebuild_roster();
        log!(
            "transport: hosting on UDP port {} as {}{}",
            port,
            my_id(),
            if self.lobby != 0 { "" } else { "; create a lobby in the game, clients join it after connecting" }
        );
        true
    }
}

pub fn host(port: u16, password: Option<&str>) {
    if !require_started() {
        return;
    }
    let port = if port == 0 { settings::get().port } else { port };
    let password = password.map(str::to_string).unwrap_or_else(|| settings::get().password.clone());
    let mut st = state();
    st.host_override = Some(true);
    st.auto_hosting = false;
    st.start_hosting(port, password);
}

/// F11: stops hosting, or starts it; either way lobby ownership no longer decides until the game restarts.
pub fn toggle_hosting() {
    if !require_started() {
        return;
    }
    let mut st = state();
    if st.role == Role::Host {
        st.host_override = Some(false);
        st.close_all(true);
        log!("transport: stopped hosting (F11 starts again)");
    } else {
        st.host_override = Some(true);
        st.auto_hosting = false;
        st.start_hosting(settings::get().port, settings::get().password.clone());
    }
}

fn resolve(address: &str) -> Option<SocketAddrV4> {
    let (host, port) = match address.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().ok().filter(|&p| p != 0)?),
        None => (address, settings::get().port),
    };
    (host, port).to_socket_addrs().ok()?.find_map(|a| match a {
        SocketAddr::V4(a) => Some(a),
        SocketAddr::V6(_) => None,
    })
}

pub fn connect(address: &str, password: Option<&str>) {
    if !require_started() {
        return;
    }
    // Without a password the host must verify players through Steam, and this player be in its lobby.
    let password = password.map(str::to_string).unwrap_or_else(|| settings::get().password.clone());
    let Some(target) = resolve(address) else {
        log!("transport: cannot resolve '{}'", address);
        return;
    };
    let mut st = state();
    st.close_all(true);
    if st.open_client_socket() {
        st.role = Role::Client;
        st.set_password(password);
        st.host_addr = target;
        let opened = st.open_host_link(now_ms());
        log!("transport: connecting to {}", target);
        if !opened {
            st.close_all(false);
        }
    }
}

pub fn disconnect() {
    let mut st = state();
    let active = st.role != Role::None;
    st.close_all(true);
    drop(st);
    log!("{}", if active { "transport: disconnected" } else { "transport: not connected" });
}

pub fn status() {
    let st = state();
    match st.role {
        Role::None => log!("transport: idle (my Steam ID {})", my_id()),
        role => log!(
            "transport: {}, lobby {}, my Steam ID {}",
            if role == Role::Host { "hosting" } else { "client" },
            if role == Role::Host { st.lobby } else { st.joined_lobby },
            my_id()
        ),
    }
    for link in st.links.iter().flatten() {
        log!(
            "  {} at {}: {}, rtt {:.1} ms (var {:.1}, rto {} ms), {} unacked, cwnd {:.0}, {} retransmits",
            link.steam_id,
            link.addr,
            if link.welcomed { "connected" } else { "connecting" },
            link.srtt_ms,
            link.rttvar_ms,
            link.rto_ms(),
            link.unacked(),
            link.cwnd,
            link.retransmits
        );
    }
}

pub fn start(my_steam_id: u64) {
    MY_ID.store(my_steam_id, Ordering::Relaxed);
    RNG.store((now_ms() as u32) | 1, Ordering::Relaxed);
    std::thread::spawn(network_thread);
    std::thread::spawn(|| {
        let configured = &settings::get().stun_servers;
        let list = if configured.trim().is_empty() { stun::DEFAULT_SERVERS } else { configured.as_str() };
        let servers = stun::resolve_servers(list);
        if servers.is_empty() {
            log!("transport: no STUN server could be resolved; the lobby only offers the LAN address");
        }
        let _ = STUN_SERVERS.set(servers);
    });
    STARTED.store(true, Ordering::Release);
    log!("transport: ready, my Steam ID {}", my_steam_id);
    let settings = settings::get();
    if settings.host_mode == settings::HostMode::Always {
        host(settings.port, None);
    } else if !settings.auto_connect.is_empty() {
        connect(&settings.auto_connect, None);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Status for the overlay.

pub struct HudLink {
    pub steam_id: u64,
    pub addr: SocketAddrV4,
    pub connected: bool,
    pub rtt_ms: f32,
    pub rttvar_ms: f32,
    pub resend_percent: f32,
    pub cwnd: f32,
}

pub struct HudStatus {
    pub role: &'static str,
    pub port: u16,
    pub offered: bool,
    pub reconnecting: bool,
    pub password: String,
    pub links: Vec<HudLink>,
}

pub fn hud_status() -> HudStatus {
    let st = state();
    HudStatus {
        role: match st.role {
            Role::None => "idle",
            Role::Host => "hosting",
            Role::Client => "client",
        },
        port: st.sock.as_ref().and_then(|s| s.local_addr().ok()).map_or(0, |a| a.port()),
        offered: !st.published.is_empty(),
        reconnecting: st.reconnect_deadline != 0,
        password: st.password.clone(),
        links: st
            .links
            .iter()
            .flatten()
            .map(|l| HudLink {
                steam_id: l.steam_id,
                addr: l.addr,
                connected: l.welcomed,
                rtt_ms: l.srtt_ms,
                rttvar_ms: l.rttvar_ms,
                resend_percent: if l.fragments_sent == 0 {
                    0.0
                } else {
                    100.0 * l.retransmits as f32 / l.fragments_sent as f32
                },
                cwnd: l.cwnd,
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Lobby hand-off.

/// The Steam lobby this player is in; a host hands it to its clients.
pub fn set_lobby(lobby_id: u64) {
    if !started() {
        return;
    }
    let mut st = state();
    if st.lobby != lobby_id {
        st.lobby = lobby_id;
        st.roster_dirty = true;
        if st.role == Role::Host {
            log!("transport: lobby {}", lobby_id);
        }
    }
}

/// `join [password]`: connects through the host's offer in the current lobby (the password only for a host
/// that has one), or, when already connected, asks the game to join the host's lobby again.
pub fn join(password: Option<&str>) {
    let mut st = state();
    if st.role == Role::Client && password.is_none() {
        drop(st);
        join_again();
        return;
    }
    if st.lobby == 0 {
        log!("transport: not in a Steam lobby; join the host's lobby in the game first");
        return;
    }
    let password = password.map(str::to_string).unwrap_or_else(|| settings::get().password.clone());
    st.join_wanted = Some(password);
    st.join_tried_lobby = 0;
    if st.role != Role::None {
        st.close_all(true);
    }
    log!("transport: looking for the host's offer in lobby {}", st.lobby);
}

pub fn join_again() {
    let mut st = state();
    if st.role == Role::Client && st.joined_lobby != 0 {
        st.pending_join_lobby = st.joined_lobby;
        log!("transport: joining lobby {} again", st.joined_lobby);
    } else {
        log!("transport: no host lobby to join; connect to a host first");
    }
}

pub fn join_pending() -> bool {
    started() && {
        let st = state();
        st.role == Role::Client && st.pending_join_lobby != 0
    }
}

/// The lobby a client should join now and the host's Steam ID, once per hand-off.
pub fn take_join() -> Option<(u64, u64)> {
    if !started() {
        return None;
    }
    let mut st = state();
    if st.role != Role::Client || st.pending_join_lobby == 0 {
        return None;
    }
    let lobby = std::mem::take(&mut st.pending_join_lobby);
    let host = st.links[0].as_ref().map_or(0, |l| l.steam_id);
    Some((lobby, host))
}

// ---------------------------------------------------------------------------------------------------------------
// Packet path, called from the ISteamNetworking hooks.

/// Whether a peer's packets go over the transport instead of Steam.
pub fn owns(steam_id: u64) -> bool {
    if !started() || steam_id == my_id() {
        return false;
    }
    let st = state();
    st.role != Role::None && st.roster.contains(&steam_id)
}

pub fn send(to: u64, data: &[u8], send_type: i32, channel: i32) -> bool {
    if data.len() > MAX_MESSAGE as usize {
        return false;
    }
    let reliable = send_type == SEND_RELIABLE || send_type == SEND_RELIABLE_WITH_BUFFERING;
    let mut st = state();
    let index = match st.role {
        Role::Host => st.link_by_id(to),
        Role::Client if st.links[0].as_ref().is_some_and(|l| l.welcomed) => Some(0),
        _ => None,
    };
    let Some(index) = index else { return false };
    let State { links, sock, .. } = &mut *st;
    links[index].as_mut().unwrap().send_to(sock, my_id(), to, channel as u8, data, reliable)
}

/// Size of the next message on a channel.
pub fn available(channel: i32) -> Option<u32> {
    if !started() {
        return None;
    }
    let st = state();
    st.inbox.iter().find(|m| m.channel == channel).map(|m| m.data.len() as u32)
}

pub fn read(channel: i32) -> Option<Message> {
    if !started() {
        return None;
    }
    let mut st = state();
    let index = st.inbox.iter().position(|m| m.channel == channel)?;
    let message = st.inbox.remove(index)?;
    st.inbox_bytes -= message.data.len() as u32;
    Some(message)
}

pub fn session_state(steam_id: u64) -> Option<P2PSessionState> {
    if !owns(steam_id) {
        return None;
    }
    let st = state();
    let index = if st.role == Role::Host { st.link_by_id(steam_id) } else { Some(0) };
    let mut state = P2PSessionState::default();
    if let Some(link) = index.and_then(|i| st.links[i].as_ref()) {
        state.connection_active = link.welcomed as u8;
        state.connecting = !link.welcomed as u8;
        state.packets_queued_for_send = link.unacked() as i32;
        state.remote_ip = u32::from(*link.addr.ip());
        state.remote_port = link.addr.port();
    }
    Some(state)
}
