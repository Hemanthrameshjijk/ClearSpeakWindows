//! Prints which programs ClearSpeak sees using a microphone, once a second (`cargo run --example mic_users_check --
//! 10`). This is what call detection ("a call app has the mic") is based on.

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    for i in 0..secs {
        println!("{:>3}s  {:?}", i + 1, clearspeak_app::call_log::mic_users());
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
