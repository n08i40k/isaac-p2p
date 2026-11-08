//! Transport cryptography: password key, handshake MACs, session keys, and sealed datagrams.
//!
//! The password becomes a pre-shared key (BLAKE2b). HELLO and WELCOME each carry an ephemeral X25519 public key
//! and a BLAKE2b MAC under that key, so only players who know the password complete a handshake. Both sides then
//! derive one ChaCha20-Poly1305 key per direction from the X25519 shared secret, the pre-shared key, both public
//! keys and the session number. Every later datagram is sealed with a per-direction counter as nonce; the
//! receiver drops counters it has already seen or that fall more than REPLAY_WINDOW behind the newest.

use blake2::digest::consts::{U16, U32, U64};
use blake2::digest::{Digest, Mac};
use blake2::{Blake2b, Blake2bMac};
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use rand_core::{OsRng, RngCore};

pub use x25519_dalek::{EphemeralSecret, PublicKey};

pub const MAC_SIZE: usize = 16;
pub const TAG_SIZE: usize = 16;
/// Cleartext part of a sealed datagram: magic, type, u32 session, u64 counter.
pub const SEALED_HEADER: usize = 2 + 4 + 8;
const REPLAY_WINDOW: u64 = 64;

pub type Psk = [u8; 32];

pub fn psk(password: &str) -> Psk {
    let mut hash = Blake2b::<U32>::new();
    hash.update(b"isaac-p2p password v1\0");
    hash.update(password.as_bytes());
    hash.finalize().into()
}

fn keyed<M: Mac + blake2::digest::KeyInit>(psk: &Psk, parts: &[&[u8]]) -> M {
    let mut mac = <M as blake2::digest::KeyInit>::new_from_slice(psk).expect("32-byte key");
    for part in parts {
        mac.update(part);
    }
    mac
}

pub fn mac(psk: &Psk, parts: &[&[u8]]) -> [u8; MAC_SIZE] {
    keyed::<Blake2bMac<U16>>(psk, parts).finalize().into_bytes().into()
}

/// Constant-time check of a handshake MAC.
pub fn verify(psk: &Psk, parts: &[&[u8]], tag: &[u8]) -> bool {
    keyed::<Blake2bMac<U16>>(psk, parts).verify_slice(tag).is_ok()
}

pub fn random_u32() -> u32 {
    OsRng.next_u32()
}

pub fn new_secret() -> (EphemeralSecret, [u8; 32]) {
    let secret = EphemeralSecret::random_from_rng(OsRng);
    let public = PublicKey::from(&secret).to_bytes();
    (secret, public)
}

/// Keys for (client -> host, host -> client), or `None` when the peer's key gives no shared secret.
pub fn session_keys(
    psk: &Psk,
    secret: EphemeralSecret,
    peer_public: [u8; 32],
    client_public: &[u8; 32],
    host_public: &[u8; 32],
    session: u32,
) -> Option<([u8; 32], [u8; 32])> {
    let shared = secret.diffie_hellman(&PublicKey::from(peer_public));
    if !shared.was_contributory() {
        return None;
    }
    let out = keyed::<Blake2bMac<U64>>(
        psk,
        &[b"isaac-p2p keys v1", shared.as_bytes(), client_public, host_public, &session.to_le_bytes()],
    )
    .finalize()
    .into_bytes();
    Some((out[..32].try_into().unwrap(), out[32..].try_into().unwrap()))
}

fn nonce(counter: u64) -> Nonce {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    nonce.into()
}

/// Sending half of a link: seals datagrams under one key with increasing counters.
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Sealer {
    pub fn new(key: &[u8; 32]) -> Sealer {
        Sealer { cipher: ChaCha20Poly1305::new(Key::from_slice(key)), counter: 0 }
    }

    /// `magic, kind, session, counter, encrypt(plain), tag`; the cleartext header is authenticated too.
    pub fn seal(&mut self, magic: u8, kind: u8, session: u32, plain: &[u8]) -> Vec<u8> {
        self.counter += 1;
        let mut out = Vec::with_capacity(SEALED_HEADER + plain.len() + TAG_SIZE);
        out.extend_from_slice(&[magic, kind]);
        out.extend_from_slice(&session.to_le_bytes());
        out.extend_from_slice(&self.counter.to_le_bytes());
        out.extend_from_slice(plain);
        let (header, body) = out.split_at_mut(SEALED_HEADER);
        let tag = self.cipher.encrypt_in_place_detached(&nonce(self.counter), header, body).expect("in-memory seal");
        out.extend_from_slice(&tag);
        out
    }
}

/// Receiving half of a link: opens sealed datagrams and rejects replays.
pub struct Opener {
    cipher: ChaCha20Poly1305,
    highest: u64,
    seen: u64, // bit i: counter highest - i was accepted
}

impl Opener {
    pub fn new(key: &[u8; 32]) -> Opener {
        Opener { cipher: ChaCha20Poly1305::new(Key::from_slice(key)), highest: 0, seen: 0 }
    }

    fn fresh(&self, counter: u64) -> bool {
        if counter > self.highest {
            return true;
        }
        let behind = self.highest - counter;
        behind < REPLAY_WINDOW && self.seen & (1 << behind) == 0 && counter != 0
    }

    fn accept(&mut self, counter: u64) {
        if counter > self.highest {
            let shift = counter - self.highest;
            self.seen = if shift >= REPLAY_WINDOW { 0 } else { self.seen << shift };
            self.seen |= 1;
            self.highest = counter;
        } else {
            self.seen |= 1 << (self.highest - counter);
        }
    }

    /// The plaintext of an authentic datagram not seen before.
    pub fn open(&mut self, datagram: &[u8]) -> Option<Vec<u8>> {
        if datagram.len() < SEALED_HEADER + TAG_SIZE {
            return None;
        }
        let counter = u64::from_le_bytes(datagram[6..14].try_into().unwrap());
        if !self.fresh(counter) {
            return None;
        }
        let (header, rest) = datagram.split_at(SEALED_HEADER);
        let (body, tag) = rest.split_at(rest.len() - TAG_SIZE);
        let mut plain = body.to_vec();
        self.cipher.decrypt_in_place_detached(&nonce(counter), header, &mut plain, Tag::from_slice(tag)).ok()?;
        self.accept(counter);
        Some(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_and_replay() {
        let key = [7u8; 32];
        let mut tx = Sealer::new(&key);
        let mut rx = Opener::new(&key);
        let a = tx.seal(0x49, 10, 5, b"one");
        let b = tx.seal(0x49, 10, 5, b"two");
        let c = tx.seal(0x49, 10, 5, b"three");
        assert_eq!(rx.open(&c).as_deref(), Some(&b"three"[..]));
        assert_eq!(rx.open(&a).as_deref(), Some(&b"one"[..])); // reordered, still in the window
        assert_eq!(rx.open(&a), None); // replay
        assert_eq!(rx.open(&c), None); // replay
        assert_eq!(rx.open(&b).as_deref(), Some(&b"two"[..]));
    }

    #[test]
    fn tampering_and_wrong_key_fail() {
        let mut tx = Sealer::new(&[1u8; 32]);
        let mut datagram = tx.seal(0x49, 10, 5, b"payload");
        assert_eq!(Opener::new(&[2u8; 32]).open(&datagram), None);
        let last = datagram.len() - 1;
        datagram[last] ^= 1;
        assert_eq!(Opener::new(&[1u8; 32]).open(&datagram), None);
        let mut datagram = tx.seal(0x49, 10, 5, b"payload");
        datagram[3] ^= 1; // session in the authenticated header
        assert_eq!(Opener::new(&[1u8; 32]).open(&datagram), None);
    }

    #[test]
    fn old_counters_outside_the_window_are_dropped() {
        let key = [3u8; 32];
        let mut tx = Sealer::new(&key);
        let mut rx = Opener::new(&key);
        let first = tx.seal(0x49, 10, 1, b"x");
        for _ in 0..REPLAY_WINDOW {
            let d = tx.seal(0x49, 10, 1, b"y");
            assert!(rx.open(&d).is_some());
        }
        assert_eq!(rx.open(&first), None);
    }

    #[test]
    fn handshake_keys_match() {
        let psk = psk("secret");
        let (client_secret, client_public) = new_secret();
        let (host_secret, host_public) = new_secret();
        let a = session_keys(&psk, client_secret, host_public, &client_public, &host_public, 9).unwrap();
        let b = session_keys(&psk, host_secret, client_public, &client_public, &host_public, 9).unwrap();
        assert_eq!(a, b);
        let tag = mac(&psk, &[b"hello"]);
        assert!(verify(&psk, &[b"hello"], &tag));
        assert!(!verify(&super::psk("other"), &[b"hello"], &tag));
    }
}
