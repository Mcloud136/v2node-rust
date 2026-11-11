use base64::Engine;
use x25519_dalek::{PublicKey, StaticSecret};

fn main() {
    let secret = StaticSecret::random();
    let public = PublicKey::from(&secret);
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    println!("PRIVATE={}", encoder.encode(secret.to_bytes()));
    println!("PUBLIC={}", encoder.encode(public.to_bytes()));
}
