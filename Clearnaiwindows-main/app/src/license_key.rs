//! The public half of the owner's license-signing key. It can only check keys, never make them.
//! Made with `licensegen newkey`; the matching secret key is kept by the owner, outside this repository.

pub const PUBLIC_KEY: [u8; 32] = [
    0xa5, 0x2a, 0x25, 0x20, 0xc6, 0x0d, 0xb1, 0x8b, 0x2f, 0x12, 0xf9, 0xb6, 0xfd, 0x20, 0x74, 0x07, 0x45, 0x50, 0x25, 0xee, 0xc6,
    0x91, 0xb0, 0x66, 0x3c, 0x86, 0x9d, 0x27, 0xd6, 0xa3, 0x54, 0xe8,
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
