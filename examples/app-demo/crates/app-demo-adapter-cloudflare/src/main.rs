#[expect(
    clippy::print_stderr,
    reason = "host stub; the real binary only runs on wasm32-unknown-unknown"
)]
fn main() {
    eprintln!(
        "Run `wrangler dev` or target wasm32-unknown-unknown to execute app-demo-adapter-cloudflare."
    );
}
