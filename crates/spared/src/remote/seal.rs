//! Client-side authenticated encryption of a backup archive: the STREAM construction that age and
//! Tink use, over ChaCha20-Poly1305 from `ring`.
//!
//! A sealed archive is a header (`SPRDSEAL`, a version byte, a random 32-byte salt) followed by
//! segments of 64 KiB of plaintext, each with its 16-byte tag. The key of one archive is
//! HKDF-SHA256 of the operator's key and the salt. A segment's nonce is an 11-byte big-endian
//! counter and one byte that is 1 only on the last segment, so a stream cut at a segment boundary
//! does not open. Every segment but the last is full; the last may be empty only when it is the
//! only one. The associated data names the app and the remote id, so an archive renamed to another
//! app or id does not open either.

use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use sha2::{Digest, Sha256};
use std::io::{Error, ErrorKind, Read, Result, Write};

pub const SEGMENT: usize = 64 * 1024;
const TAG: usize = 16;
const MAGIC: &[u8; 8] = b"SPRDSEAL";
const VERSION: u8 = 1;
pub const SALT_LEN: usize = 32;
pub const HEADER_LEN: usize = MAGIC.len() + 1 + SALT_LEN;
/// The `encryption.alg` of a sealed archive in its manifest.
pub const ALG: &str = "chacha20-poly1305-stream-64k";
const HKDF_INFO: &[u8] = b"spared remote backup v1 archive key";

/// The operator's 32-byte backup key. Its `Debug` prints `***`, and its bytes are cleared when it
/// is dropped.
pub struct BackupKey([u8; 32]);

impl std::fmt::Debug for BackupKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BackupKey(***)")
    }
}

impl Drop for BackupKey {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

impl BackupKey {
    /// A key file holds 64 hex digits; surrounding white space is ignored.
    pub fn parse(text: &str) -> std::result::Result<BackupKey, String> {
        let t = text.trim();
        match unhex(t) {
            Some(b) if b.len() == 32 => {
                let mut k = [0u8; 32];
                k.copy_from_slice(&b);
                Ok(BackupKey(k))
            }
            _ => Err(format!(
                "a backup key is 64 hex digits (32 bytes), and this one is {} characters",
                t.len()
            )),
        }
    }

    pub fn generate() -> Result<BackupKey> {
        let mut k = [0u8; 32];
        random(&mut k)?;
        Ok(BackupKey(k))
    }

    /// The key as a key file holds it. Only `spared backup-key new` prints it.
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    /// The first 8 bytes of SHA-256(key), in hex: names the key in a manifest without revealing
    /// it, so a restore with the wrong key fails before anything is downloaded.
    pub fn key_id(&self) -> String {
        hex(&Sha256::digest(self.0)[..8])
    }

    fn archive_key(&self, salt: &[u8; SALT_LEN]) -> LessSafeKey {
        struct Len;
        impl ring::hkdf::KeyType for Len {
            fn len(&self) -> usize {
                32
            }
        }
        let mut k = [0u8; 32];
        ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, salt)
            .extract(&self.0)
            .expand(&[HKDF_INFO], Len)
            .and_then(|okm| okm.fill(&mut k))
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        let key = UnboundKey::new(&CHACHA20_POLY1305, &k).expect("a 32-byte ChaCha20 key");
        for b in k.iter_mut() {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        LessSafeKey::new(key)
    }
}

pub fn random(buf: &mut [u8]) -> Result<()> {
    use ring::rand::SecureRandom;
    ring::rand::SystemRandom::new()
        .fill(buf)
        .map_err(|_| Error::other("the system random source failed"))
}

pub fn new_salt() -> Result<[u8; SALT_LEN]> {
    let mut s = [0u8; SALT_LEN];
    random(&mut s)?;
    Ok(s)
}

pub fn salt_hex(salt: &[u8; SALT_LEN]) -> String {
    hex(salt)
}

/// The associated data of every segment: what the archive is, for which app and remote id.
pub fn aad(app: &str, remote_id: &str) -> Vec<u8> {
    format!("spared-remote-backup-v1|{app}|{remote_id}").into_bytes()
}

fn nonce(counter: u64, last: bool) -> Nonce {
    let mut n = [0u8; 12];
    n[3..11].copy_from_slice(&counter.to_be_bytes());
    n[11] = last as u8;
    Nonce::assume_unique_for_key(n)
}

fn corrupt(what: String) -> Error {
    Error::new(ErrorKind::InvalidData, what)
}

/// Writes a sealed archive to `out`. Call [`Sealer::finish`]: dropping it loses the last segment.
pub struct Sealer<W: Write> {
    key: LessSafeKey,
    aad: Vec<u8>,
    buf: Vec<u8>,
    counter: u64,
    out: W,
}

impl<W: Write> Sealer<W> {
    pub fn new(key: &BackupKey, salt: &[u8; SALT_LEN], aad: Vec<u8>, mut out: W) -> Result<Self> {
        out.write_all(MAGIC)?;
        out.write_all(&[VERSION])?;
        out.write_all(salt)?;
        Ok(Sealer {
            key: key.archive_key(salt),
            aad,
            buf: Vec::with_capacity(SEGMENT + TAG + 1),
            counter: 0,
            out,
        })
    }

    fn seal(&mut self, n: usize, last: bool) -> Result<()> {
        let rest = self.buf.split_off(n);
        let mut seg = std::mem::replace(&mut self.buf, rest);
        self.key
            .seal_in_place_append_tag(
                nonce(self.counter, last),
                Aad::from(&self.aad[..]),
                &mut seg,
            )
            .map_err(|_| Error::other("sealing a segment failed"))?;
        self.counter += 1;
        self.out.write_all(&seg)
    }

    /// Seal what is left as the last segment and return the writer.
    pub fn finish(mut self) -> Result<W> {
        let n = self.buf.len();
        self.seal(n, true)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

impl<W: Write> Write for Sealer<W> {
    fn write(&mut self, data: &[u8]) -> Result<usize> {
        let take = data.len().min(SEGMENT + 1 - self.buf.len().min(SEGMENT));
        self.buf.extend_from_slice(&data[..take]);
        // A full segment is sealed only once a byte after it exists: until then it may be the last.
        if self.buf.len() > SEGMENT {
            self.seal(SEGMENT, false)?;
        }
        Ok(take)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Read until `buf` is full or the input ends; the number of bytes read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// The salt in a sealed archive's header, after checking the magic and the version.
pub fn read_header(r: &mut impl Read) -> Result<[u8; SALT_LEN]> {
    let mut h = [0u8; HEADER_LEN];
    if read_full(r, &mut h)? != HEADER_LEN || &h[..MAGIC.len()] != MAGIC {
        return Err(corrupt("the archive is not a sealed spared archive".into()));
    }
    if h[MAGIC.len()] != VERSION {
        return Err(corrupt(format!(
            "the archive is sealed with format version {}, which this spared does not read",
            h[MAGIC.len()]
        )));
    }
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&h[MAGIC.len() + 1..]);
    Ok(salt)
}

/// Reads the plaintext of a sealed archive, opening one segment at a time. Bytes are returned only
/// from segments whose tag checked. A segment that does not open, a missing last segment or bytes
/// after it are `InvalidData` errors.
pub struct Opener<R: Read> {
    key: LessSafeKey,
    aad: Vec<u8>,
    inner: R,
    counter: u64,
    /// The next ciphertext chunk, read ahead by one byte so the last segment is known.
    chunk: Vec<u8>,
    plain: Vec<u8>,
    pos: usize,
    done: bool,
}

impl<R: Read> Opener<R> {
    /// Read the header from `inner` and prepare to open what follows.
    pub fn new(key: &BackupKey, aad: Vec<u8>, mut inner: R) -> Result<(Self, [u8; SALT_LEN])> {
        let salt = read_header(&mut inner)?;
        Ok((
            Opener {
                key: key.archive_key(&salt),
                aad,
                inner,
                counter: 0,
                chunk: Vec::with_capacity(SEGMENT + TAG + 1),
                plain: vec![],
                pos: 0,
                done: false,
            },
            salt,
        ))
    }

    fn next_segment(&mut self) -> Result<()> {
        // `chunk` holds at most one byte carried over from the previous read.
        let have = self.chunk.len();
        self.chunk.resize(SEGMENT + TAG + 1, 0);
        let got = have + read_full(&mut self.inner, &mut self.chunk[have..])?;
        self.chunk.truncate(got);
        let last = got <= SEGMENT + TAG;
        let carry = if last { None } else { self.chunk.pop() };
        if self.chunk.len() < TAG || (last && self.chunk.len() == TAG && self.counter > 0) {
            return Err(corrupt(format!(
                "the archive ends inside segment {} (truncated)",
                self.counter
            )));
        }
        let n = self
            .key
            .open_in_place(
                nonce(self.counter, last),
                Aad::from(&self.aad[..]),
                &mut self.chunk,
            )
            .map_err(|_| {
                corrupt(format!(
                    "segment {} does not open: the archive was changed, cut short, sealed with another key, or belongs to another app or backup id",
                    self.counter
                ))
            })?
            .len();
        self.plain.clear();
        self.plain.extend_from_slice(&self.chunk[..n]);
        self.pos = 0;
        self.chunk.clear();
        self.chunk.extend(carry);
        self.counter += 1;
        self.done = last;
        Ok(())
    }
}

impl<R: Read> Read for Opener<R> {
    fn read(&mut self, out: &mut [u8]) -> Result<usize> {
        while self.pos == self.plain.len() {
            if self.done {
                return Ok(0);
            }
            self.next_segment()?;
        }
        let n = out.len().min(self.plain.len() - self.pos);
        out[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Open every segment of a sealed archive and discard the plaintext: the whole archive is
/// authentic, and complete, or this fails. Returns the plaintext length and the header's salt.
pub fn verify(key: &BackupKey, aad: Vec<u8>, r: impl Read) -> Result<(u64, [u8; SALT_LEN])> {
    let (mut o, salt) = Opener::new(key, aad, r)?;
    let n = std::io::copy(&mut o, &mut std::io::sink())?;
    Ok((n, salt))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> BackupKey {
        BackupKey([b; 32])
    }

    fn seal(k: &BackupKey, aad: &[u8], data: &[u8]) -> Vec<u8> {
        let mut s = Sealer::new(k, &[7; SALT_LEN], aad.to_vec(), vec![]).unwrap();
        // Odd-sized writes, so segment boundaries fall inside a write.
        for part in data.chunks(1000 + 7) {
            s.write_all(part).unwrap();
        }
        s.finish().unwrap()
    }

    fn open(k: &BackupKey, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
        let (mut o, _) = Opener::new(k, aad.to_vec(), sealed)?;
        let mut v = vec![];
        o.read_to_end(&mut v)?;
        Ok(v)
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn round_trips_at_every_segment_boundary() {
        let k = key(1);
        for n in [
            0,
            1,
            SEGMENT - 1,
            SEGMENT,
            SEGMENT + 1,
            2 * SEGMENT,
            3 * SEGMENT + 5,
        ] {
            let d = data(n);
            let s = seal(&k, b"a", &d);
            let segments = n.div_ceil(SEGMENT).max(1);
            assert_eq!(s.len(), HEADER_LEN + n + segments * TAG, "n={n}");
            assert_eq!(open(&k, b"a", &s).unwrap(), d, "n={n}");
            assert_eq!(verify(&k, b"a".to_vec(), &s[..]).unwrap().0, n as u64);
        }
    }

    #[test]
    fn an_empty_archive_is_one_empty_last_segment() {
        let s = seal(&key(1), b"a", b"");
        assert_eq!(s.len(), HEADER_LEN + TAG);
        assert_eq!(open(&key(1), b"a", &s).unwrap(), b"");
        assert!(
            open(&key(1), b"a", &s[..HEADER_LEN]).is_err(),
            "no segment at all"
        );
    }

    #[test]
    fn exactly_one_full_segment_is_sealed_as_the_last() {
        let d = data(SEGMENT);
        let s = seal(&key(1), b"a", &d);
        assert_eq!(
            s.len(),
            HEADER_LEN + SEGMENT + TAG,
            "no empty trailing segment"
        );
        assert_eq!(open(&key(1), b"a", &s).unwrap(), d);
    }

    #[test]
    fn a_stream_cut_at_a_segment_boundary_does_not_open() {
        let k = key(1);
        let s = seal(&k, b"a", &data(2 * SEGMENT + 10));
        let first = HEADER_LEN + SEGMENT + TAG;
        let e = open(&k, b"a", &s[..first]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidData);
        assert!(e.to_string().contains("segment 0 does not open"), "{e}");
        let two = first + SEGMENT + TAG;
        assert!(open(&k, b"a", &s[..two]).is_err());
        assert!(open(&k, b"a", &s[..s.len() - 1]).is_err());
        let mut more = s.clone();
        more.extend_from_slice(&[0u8; TAG]);
        assert!(
            open(&k, b"a", &more).is_err(),
            "an empty segment after the last"
        );
    }

    #[test]
    fn an_empty_last_segment_after_full_ones_is_refused() {
        // Built by hand: one full segment marked not-last, then an empty last one. The sealer
        // never writes this, and the opener refuses it.
        let k = key(1);
        let salt = [7u8; SALT_LEN];
        let ak = k.archive_key(&salt);
        let mut out = vec![];
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&salt);
        let mut seg = data(SEGMENT);
        ak.seal_in_place_append_tag(nonce(0, false), Aad::from(&b"a"[..]), &mut seg)
            .unwrap();
        out.extend_from_slice(&seg);
        let mut last = vec![];
        ak.seal_in_place_append_tag(nonce(1, true), Aad::from(&b"a"[..]), &mut last)
            .unwrap();
        out.extend_from_slice(&last);
        assert!(open(&k, b"a", &out).is_err());
    }

    #[test]
    fn a_flipped_byte_another_key_or_another_name_do_not_open() {
        let k = key(1);
        let s = seal(&k, b"app|id", &data(SEGMENT + 100));
        for i in [HEADER_LEN, HEADER_LEN + SEGMENT + TAG + 3, s.len() - 1] {
            let mut t = s.clone();
            t[i] ^= 1;
            assert!(open(&k, b"app|id", &t).is_err(), "flip at {i}");
        }
        let mut salt = s.clone();
        salt[MAGIC.len() + 1] ^= 1;
        assert!(open(&k, b"app|id", &salt).is_err(), "changed salt");
        assert!(open(&key(2), b"app|id", &s).is_err());
        assert!(open(&k, b"other|id", &s).is_err());
        let mut magic = s.clone();
        magic[0] = b'X';
        assert!(open(&k, b"app|id", &magic).is_err());
    }

    #[test]
    fn keys_parse_from_hex_and_never_print() {
        let k = BackupKey::generate().unwrap();
        let text = format!("  {}\n", k.to_hex());
        let back = BackupKey::parse(&text).unwrap();
        assert_eq!(back.key_id(), k.key_id());
        assert_eq!(k.key_id().len(), 16);
        assert_eq!(format!("{back:?}"), "BackupKey(***)");
        assert!(BackupKey::parse("abc").is_err());
        assert!(BackupKey::parse(&"g".repeat(64)).is_err());
        assert!(BackupKey::parse(&"ab".repeat(33)).is_err());
        assert_ne!(
            BackupKey::generate().unwrap().to_hex(),
            k.to_hex(),
            "two keys differ"
        );
    }
}
