fn main() {
    let sk = crypto_box::SecretKey::generate(&mut crypto_box::aead::OsRng);
    let pk_b64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(sk.public_key().to_bytes())
    };
    let sealed = webo::github::seal_secret(&pk_b64, "hello-secret").expect("seal");
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(sealed).expect("b64");
    let opened = sk.unseal(&bytes).expect("unseal");
    println!("round-trip: {}", String::from_utf8_lossy(&opened));
}
