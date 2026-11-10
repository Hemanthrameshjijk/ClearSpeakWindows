//! The public half of the owner's license-signing key. It can only check keys, never make them.
//! The production key made for the license server (its secret is an encrypted file outside every repository).

pub const PUBLIC_KEY: [u8; 32] = [
    0xd6, 0xb9, 0x5c, 0xda, 0xe2, 0xd7, 0x43, 0x35, 0x9d, 0xd4, 0x48, 0x85, 0x94, 0x28, 0x6c, 0x12, 0x0a, 0x8e, 0x5e, 0x57, 0x60,
    0x85, 0x2a, 0x99, 0x64, 0x0e, 0x27, 0xd0, 0x98, 0x8f, 0xc7, 0x51,
];

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// End-to-end on this PC: `CLEARNAI_TEST_TOKEN=<token made by licensegen for this PC's Device Code>
    /// cargo test -p clearnai-app --lib license_key -- --ignored`. Needs PowerShell (reads the real hardware).
    #[test]
    #[ignore]
    fn a_token_made_for_this_pc_verifies_with_the_built_in_public_key() {
        let token = std::env::var("CLEARNAI_TEST_TOKEN").expect("set CLEARNAI_TEST_TOKEN");
        crate::device_id::start_reading();
        for _ in 0..100 {
            if crate::device_id::components().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let p = license::token::verify(&token, &PUBLIC_KEY, &crate::device_id::texts(), now).expect("must verify on this PC");
        println!("verified: {} features {:?}", p.name, p.feat);
    }
}
