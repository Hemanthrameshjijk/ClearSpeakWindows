//! Prints what the "VICIdial agent screen" method reads, once a second (`cargo run --example vicidial_screen_check --
//! 40` for 40 s). Open the VICIdial agent page in the browser first, on the visible tab. The number is printed: run it
//! only on test calls.

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let w = clearspeak_app::vicidial_screen::ScreenWatch::default();
    w.set_on(true);
    for i in 0..secs {
        std::thread::sleep(std::time::Duration::from_secs(1));
        println!("{:>3}s  {:?}", i + 1, w.state());
    }
}
