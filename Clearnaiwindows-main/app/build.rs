// Embeds the app icon into clearnairt.exe so Explorer, the taskbar and the installer shortcuts show the ClearNAI logo.
// Windows targets only; a failure to embed is a warning, never a build error.
fn main() {
    println!("cargo:rerun-if-changed=assets/clearnai.ico");
    // Which source the build came from, for the log's start-up header ("unknown" outside a git checkout).
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=CLEARNAI_GIT_COMMIT={commit}");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/clearnai.ico");
        res.set("ProductName", "ClearNAI");
        res.set("FileDescription", "ClearNAI");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the app icon: {e}");
        }
    }
}
