//! **AES-128-CBC with PKCS#7 padding, decryption side** — what an HLS `EXT-X-KEY:METHOD=AES-128`
//! segment is (RFC 8216 §5.2): the whole segment (or its byte range) encrypted under one 16-byte
//! key, CBC-chained from the tag's `IV` or, without one, from the segment's media sequence number
//! as a big-endian 128-bit integer, and padded with PKCS#7.
//!
//! No crate in `Cargo.lock` offers raw AES (`ring` exposes AEADs only, and only to tests), so this
//! is the textbook byte-oriented cipher of FIPS-197, which is small and needs no unsafe code. It is
//! not constant-time; it decrypts a public media stream whose key the playlist hands to anybody who
//! asks, so a timing channel has nothing to leak. The S-box is computed (the multiplicative inverse
//! in GF(2⁸) and the affine map, FIPS-197 §5.1.1) rather than transcribed, so there is no table to
//! mistype; the FIPS-197 and SP 800-38A vectors in the tests pin the whole cipher.
//!
//! The encrypting direction exists for the tests (the vectors are stated both ways, and the
//! loopback HLS test needs encrypted segments to serve).

/// Multiply in GF(2⁸) modulo x⁸ + x⁴ + x³ + x + 1.
const fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

/// The forward S-box: `affine(inverse(x))`, with 0 mapping through inverse 0.
const fn make_sbox() -> [u8; 256] {
    // Exponent and logarithm tables to the generator 3, so an inverse is one lookup:
    // inv(x) = 3^(255 - log3 x). 0 has no inverse and maps to 0.
    let mut exp = [0u8; 255];
    let mut log = [0u8; 256];
    let mut p = 1u8;
    let mut i = 0usize;
    while i < 255 {
        exp[i] = p;
        log[p as usize] = i as u8;
        p = gmul(p, 3);
        i += 1;
    }
    let mut sbox = [0u8; 256];
    let mut x = 0usize;
    while x < 256 {
        let inv = if x == 0 { 0 } else { exp[(255 - log[x] as usize) % 255] };
        let s = inv ^ inv.rotate_left(1) ^ inv.rotate_left(2) ^ inv.rotate_left(3) ^ inv.rotate_left(4) ^ 0x63;
        sbox[x] = s;
        x += 1;
    }
    sbox
}

const fn invert(sbox: &[u8; 256]) -> [u8; 256] {
    let mut inv = [0u8; 256];
    let mut x = 0usize;
    while x < 256 {
        inv[sbox[x] as usize] = x as u8;
        x += 1;
    }
    inv
}

const SBOX: [u8; 256] = make_sbox();
const INV_SBOX: [u8; 256] = invert(&SBOX);
const RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

/// One AES block.
pub type Block = [u8; 16];

/// An expanded AES-128 key: the eleven round keys of FIPS-197 §5.2.
#[derive(Clone)]
pub struct Aes128 {
    round: [Block; 11],
}

impl std::fmt::Debug for Aes128 {
    /// Never the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Aes128(<key>)")
    }
}

impl Aes128 {
    pub fn new(key: &Block) -> Aes128 {
        let mut w = [[0u8; 4]; 44];
        for (i, word) in w.iter_mut().take(4).enumerate() {
            word.copy_from_slice(&key[4 * i..4 * i + 4]);
        }
        for i in 4..44 {
            let mut t = w[i - 1];
            if i % 4 == 0 {
                t = [SBOX[t[1] as usize] ^ RCON[i / 4 - 1], SBOX[t[2] as usize], SBOX[t[3] as usize], SBOX[t[0] as usize]];
            }
            for j in 0..4 {
                w[i][j] = w[i - 4][j] ^ t[j];
            }
        }
        let mut round = [[0u8; 16]; 11];
        for (r, rk) in round.iter_mut().enumerate() {
            for c in 0..4 {
                rk[4 * c..4 * c + 4].copy_from_slice(&w[4 * r + c]);
            }
        }
        Aes128 { round }
    }

    fn add_round_key(s: &mut Block, k: &Block) {
        for (b, k) in s.iter_mut().zip(k) {
            *b ^= k;
        }
    }

    /// The state is column-major (`s[r + 4c]`), as the input bytes fill it (FIPS-197 §3.4).
    fn shift_rows(s: &mut Block) {
        let o = *s;
        for r in 1..4 {
            for c in 0..4 {
                s[r + 4 * c] = o[r + 4 * ((c + r) % 4)];
            }
        }
    }

    fn inv_shift_rows(s: &mut Block) {
        let o = *s;
        for r in 1..4 {
            for c in 0..4 {
                s[r + 4 * ((c + r) % 4)] = o[r + 4 * c];
            }
        }
    }

    fn mix_columns(s: &mut Block) {
        for c in 0..4 {
            let a = [s[4 * c], s[4 * c + 1], s[4 * c + 2], s[4 * c + 3]];
            s[4 * c] = gmul(a[0], 2) ^ gmul(a[1], 3) ^ a[2] ^ a[3];
            s[4 * c + 1] = a[0] ^ gmul(a[1], 2) ^ gmul(a[2], 3) ^ a[3];
            s[4 * c + 2] = a[0] ^ a[1] ^ gmul(a[2], 2) ^ gmul(a[3], 3);
            s[4 * c + 3] = gmul(a[0], 3) ^ a[1] ^ a[2] ^ gmul(a[3], 2);
        }
    }

    fn inv_mix_columns(s: &mut Block) {
        for c in 0..4 {
            let a = [s[4 * c], s[4 * c + 1], s[4 * c + 2], s[4 * c + 3]];
            s[4 * c] = gmul(a[0], 14) ^ gmul(a[1], 11) ^ gmul(a[2], 13) ^ gmul(a[3], 9);
            s[4 * c + 1] = gmul(a[0], 9) ^ gmul(a[1], 14) ^ gmul(a[2], 11) ^ gmul(a[3], 13);
            s[4 * c + 2] = gmul(a[0], 13) ^ gmul(a[1], 9) ^ gmul(a[2], 14) ^ gmul(a[3], 11);
            s[4 * c + 3] = gmul(a[0], 11) ^ gmul(a[1], 13) ^ gmul(a[2], 9) ^ gmul(a[3], 14);
        }
    }

    /// The cipher (FIPS-197 §5.1).
    pub fn encrypt_block(&self, b: &mut Block) {
        Self::add_round_key(b, &self.round[0]);
        for r in 1..10 {
            for x in b.iter_mut() {
                *x = SBOX[*x as usize];
            }
            Self::shift_rows(b);
            Self::mix_columns(b);
            Self::add_round_key(b, &self.round[r]);
        }
        for x in b.iter_mut() {
            *x = SBOX[*x as usize];
        }
        Self::shift_rows(b);
        Self::add_round_key(b, &self.round[10]);
    }

    /// The inverse cipher (FIPS-197 §5.3).
    pub fn decrypt_block(&self, b: &mut Block) {
        Self::add_round_key(b, &self.round[10]);
        for r in (1..10).rev() {
            Self::inv_shift_rows(b);
            for x in b.iter_mut() {
                *x = INV_SBOX[*x as usize];
            }
            Self::add_round_key(b, &self.round[r]);
            Self::inv_mix_columns(b);
        }
        Self::inv_shift_rows(b);
        for x in b.iter_mut() {
            *x = INV_SBOX[*x as usize];
        }
        Self::add_round_key(b, &self.round[0]);
    }
}

/// Why a CBC stream did not decrypt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CbcError {
    /// The ciphertext is not a whole number of blocks.
    Truncated,
    /// The last block's PKCS#7 padding is not valid: the wrong key or IV, or a corrupted segment.
    BadPadding,
}

/// **Streaming CBC decryption.** Ciphertext arrives in whatever pieces the transport delivers;
/// plaintext leaves in whole blocks, except that the LAST decrypted block is held back until
/// [`finish`](Self::finish), which is the only point at which it is known to carry the padding.
pub struct CbcDecryptor {
    aes: Aes128,
    prev: Block,
    partial: Block,
    partial_len: usize,
    held: Option<Block>,
}

impl CbcDecryptor {
    pub fn new(key: &Block, iv: &Block) -> CbcDecryptor {
        CbcDecryptor { aes: Aes128::new(key), prev: *iv, partial: [0; 16], partial_len: 0, held: None }
    }

    /// Decrypt what `input` completes, appending the plaintext that is certainly not padding.
    pub fn update(&mut self, mut input: &[u8], out: &mut Vec<u8>) {
        while !input.is_empty() {
            let take = (16 - self.partial_len).min(input.len());
            self.partial[self.partial_len..self.partial_len + take].copy_from_slice(&input[..take]);
            self.partial_len += take;
            input = &input[take..];
            if self.partial_len < 16 {
                return;
            }
            self.partial_len = 0;
            let cipher = self.partial;
            let mut plain = cipher;
            self.aes.decrypt_block(&mut plain);
            for (p, v) in plain.iter_mut().zip(&self.prev) {
                *p ^= v;
            }
            self.prev = cipher;
            if let Some(h) = self.held.replace(plain) {
                out.extend_from_slice(&h);
            }
        }
    }

    /// The end of the ciphertext: strip and check the padding of the held block.
    pub fn finish(self, out: &mut Vec<u8>) -> Result<(), CbcError> {
        if self.partial_len != 0 {
            return Err(CbcError::Truncated);
        }
        let Some(last) = self.held else {
            // An empty ciphertext has no padding block at all, which PKCS#7 never produces.
            return Err(CbcError::Truncated);
        };
        let pad = last[15] as usize;
        if pad == 0 || pad > 16 || !last[16 - pad..].iter().all(|&b| b as usize == pad) {
            return Err(CbcError::BadPadding);
        }
        out.extend_from_slice(&last[..16 - pad]);
        Ok(())
    }
}

/// CBC-encrypt `plain` with PKCS#7 padding: the inverse of [`CbcDecryptor`], for the tests and the
/// test servers that need an encrypted segment.
#[cfg(test)]
pub fn cbc_encrypt_pkcs7(key: &Block, iv: &Block, plain: &[u8]) -> Vec<u8> {
    let aes = Aes128::new(key);
    let pad = 16 - plain.len() % 16;
    let mut data = plain.to_vec();
    data.extend(std::iter::repeat_n(pad as u8, pad));
    let mut prev = *iv;
    for chunk in data.chunks_mut(16) {
        let mut b: Block = chunk.try_into().expect("whole blocks");
        for (x, p) in b.iter_mut().zip(&prev) {
            *x ^= p;
        }
        aes.encrypt_block(&mut b);
        chunk.copy_from_slice(&b);
        prev = b;
    }
    data
}

/// The IV of a segment whose key tag names none: its media sequence number, big-endian, in the
/// low 64 bits of the 128 (RFC 8216 §5.2).
pub fn sequence_iv(sequence: u64) -> Block {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&sequence.to_be_bytes());
    iv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn block(s: &str) -> Block {
        hex(s).try_into().unwrap()
    }

    #[test]
    fn the_computed_sbox_is_the_standard_one() {
        // FIPS-197 Figure 7, spot checks across the table.
        assert_eq!(SBOX[0x00], 0x63);
        assert_eq!(SBOX[0x01], 0x7c);
        assert_eq!(SBOX[0x53], 0xed);
        assert_eq!(SBOX[0xff], 0x16);
        assert_eq!(INV_SBOX[0x63], 0x00);
        assert_eq!(INV_SBOX[0x16], 0xff);
    }

    #[test]
    fn fips_197_appendix_b_and_c1_vectors() {
        // Appendix B: the worked example.
        let aes = Aes128::new(&block("2b7e151628aed2a6abf7158809cf4f3c"));
        let mut b = block("3243f6a8885a308d313198a2e0370734");
        aes.encrypt_block(&mut b);
        assert_eq!(b, block("3925841d02dc09fbdc118597196a0b32"));
        aes.decrypt_block(&mut b);
        assert_eq!(b, block("3243f6a8885a308d313198a2e0370734"));
        // Appendix C.1: AES-128.
        let aes = Aes128::new(&block("000102030405060708090a0b0c0d0e0f"));
        let mut b = block("00112233445566778899aabbccddeeff");
        aes.encrypt_block(&mut b);
        assert_eq!(b, block("69c4e0d86a7b0430d8cdb78070b4c55a"));
        aes.decrypt_block(&mut b);
        assert_eq!(b, block("00112233445566778899aabbccddeeff"));
    }

    const SP_KEY: &str = "2b7e151628aed2a6abf7158809cf4f3c";
    const SP_IV: &str = "000102030405060708090a0b0c0d0e0f";
    const SP_PLAIN: &str = "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e5130c81c46a35ce411e5fbc1191a0a52eff69f2445df4f9b17ad2b417be66c3710";
    const SP_CIPHER: &str = "7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b273bed6b8e3c1743b7116e69e222295163ff1caa1681fac09120eca307586e1a7";

    #[test]
    fn sp_800_38a_f21_cbc_encrypt_vectors() {
        let encrypted = cbc_encrypt_pkcs7(&block(SP_KEY), &block(SP_IV), &hex(SP_PLAIN));
        // The four blocks of F.2.1, then the one block of padding PKCS#7 adds to a whole input.
        assert_eq!(&encrypted[..64], &hex(SP_CIPHER)[..]);
        assert_eq!(encrypted.len(), 80);
    }

    #[test]
    fn sp_800_38a_f22_cbc_decrypt_vectors_in_any_chunking() {
        // F.2.2 is raw CBC (no padding): decrypt the four blocks plus the PKCS#7 block F.2.1's
        // plaintext gets, fed in every chunk size from 1 to the whole, and the plaintext is F.2.2's.
        let cipher = cbc_encrypt_pkcs7(&block(SP_KEY), &block(SP_IV), &hex(SP_PLAIN));
        assert_eq!(&cipher[..64], &hex(SP_CIPHER)[..]);
        for chunk in 1..=cipher.len() {
            let mut d = CbcDecryptor::new(&block(SP_KEY), &block(SP_IV));
            let mut out = Vec::new();
            for piece in cipher.chunks(chunk) {
                d.update(piece, &mut out);
            }
            d.finish(&mut out).unwrap();
            assert_eq!(out, hex(SP_PLAIN), "chunk size {chunk}");
        }
    }

    #[test]
    fn padding_and_length_are_checked() {
        let key = block(SP_KEY);
        let iv = block(SP_IV);
        for len in [0usize, 1, 15, 16, 17, 188 * 7] {
            let plain: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
            let c = cbc_encrypt_pkcs7(&key, &iv, &plain);
            let mut d = CbcDecryptor::new(&key, &iv);
            let mut out = Vec::new();
            d.update(&c, &mut out);
            d.finish(&mut out).unwrap();
            assert_eq!(out, plain, "len {len}");
        }
        // A wrong key leaves garbage padding (with overwhelming probability, and certainly for
        // this fixed input).
        let c = cbc_encrypt_pkcs7(&key, &iv, b"segment bytes");
        let mut d = CbcDecryptor::new(&block("000102030405060708090a0b0c0d0e0f"), &iv);
        let mut out = Vec::new();
        d.update(&c, &mut out);
        assert_eq!(d.finish(&mut out), Err(CbcError::BadPadding));
        let mut d = CbcDecryptor::new(&key, &iv);
        d.update(&c[..c.len() - 1], &mut Vec::new());
        assert_eq!(d.finish(&mut Vec::new()), Err(CbcError::Truncated));
    }

    #[test]
    fn a_sequence_iv_is_the_big_endian_number() {
        assert_eq!(sequence_iv(0), [0; 16]);
        assert_eq!(sequence_iv(0x0102), block("00000000000000000000000000000102"));
    }
}
