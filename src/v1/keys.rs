//! The key pieces of the mini app protocol this plugin needs (spec 4):
//! base64url without padding, sha256 as lowercase hex, and the public half
//! of the key the Bot provisions. Copied from the Bot's `rox-v1` crate
//! (`broker/rox-v1/src/keys.rs`), with the parts the plugin never calls
//! left out.

use base64::Engine as _;
use sha2::Digest as _;

/// base64url without padding.
pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes base64url without padding (padding or another alphabet is
/// refused: one encoding, one form).
pub fn unb64(text: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|e| format!("bad base64url: {e}"))
}

/// Lowercase hex of SHA-256 over `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// A provisioned key pair. It has no `Debug` and no way to export the seed.
pub struct SessionKey(ed25519_dalek::SigningKey);

impl SessionKey {
    /// The key pair of a seed (a plugin's key, spec 4.3): base64url of
    /// exactly 32 bytes, in its one canonical form.
    pub fn from_seed(seed: &str) -> Result<SessionKey, String> {
        let bytes = unb64(seed)?;
        let mut arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| "a key seed is 32 bytes".to_owned())?;
        if b64(&arr) != seed {
            arr.fill(0);
            return Err("a key seed is 32 bytes of base64url".to_owned());
        }
        let key = ed25519_dalek::SigningKey::from_bytes(&arr);
        arr.fill(0);
        Ok(SessionKey(key))
    }

    /// The public half, base64url.
    pub fn public(&self) -> String {
        b64(self.0.verifying_key().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64url_has_no_padding_and_refuses_other_forms() {
        assert_eq!(super::b64(&[0xfb, 0xff]), "-_8");
        assert_eq!(super::unb64("-_8").unwrap(), [0xfb, 0xff]);
        assert!(super::unb64("-_8=").is_err());
        assert!(super::unb64("+/8").is_err());
    }

    #[test]
    fn a_seed_is_32_bytes_in_its_canonical_form() {
        assert!(super::SessionKey::from_seed(&super::b64(&[7; 32])).is_ok());
        assert!(super::SessionKey::from_seed(&super::b64(&[7; 31])).is_err());
        // a non-canonical last character decodes to the same bytes: refused
        let mut odd = super::b64(&[7; 32]);
        odd.pop();
        odd.push('d');
        assert!(super::SessionKey::from_seed(&odd).is_err());
    }

    #[test]
    fn the_public_half_is_rfc_8032s() {
        // RFC 8032, 7.1, test 1
        let seed = super::b64(&[
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19,
            0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
        ]);
        let public = super::unb64(&super::SessionKey::from_seed(&seed).unwrap().public()).unwrap();
        let hex: String = public.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(super::sha256_hex(b"{}"), "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a");
    }
}
