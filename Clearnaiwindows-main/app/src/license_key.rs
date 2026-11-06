//! The public half of the owner's license-signing key. It can only check keys, never make them.
//! Made with `licensegen newkey`; the matching secret key is kept by the owner, outside this repository.

pub const PUBLIC_KEY: [u8; 32] = [
    0xa5, 0x2a, 0x25, 0x20, 0xc6, 0x0d, 0xb1, 0x8b, 0x2f, 0x12, 0xf9, 0xb6, 0xfd, 0x20, 0x74, 0x07, 0x45, 0x50, 0x25, 0xee, 0xc6,
    0x91, 0xb0, 0x66, 0x3c, 0x86, 0x9d, 0x27, 0xd6, 0xa3, 0x54, 0xe8,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Optional end-to-end check: `CLEARNAI_TEST_LICENSE=<key made by licensegen> cargo test -p clearnai-app --lib license_key`.
    #[test]
    fn a_key_made_with_the_owners_secret_verifies_with_the_built_in_public_key() {
        let Ok(key) = std::env::var("CLEARNAI_TEST_LICENSE") else { return };
        let l = license::verify(&key, &PUBLIC_KEY, 0).expect("the key must verify with the built-in public key");
        assert!(!l.name.is_empty());
    }
}
