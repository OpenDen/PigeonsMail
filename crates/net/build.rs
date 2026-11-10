use std::path::Path;

fn main() {
    let secret_file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("oauth-secrets")
        .join("google-client-secret.txt");
    println!("cargo:rerun-if-changed={}", secret_file.display());
    println!("cargo:rerun-if-env-changed=MAILTHIN_GOOGLE_CLIENT_SECRET");

    let secret = std::fs::read_to_string(&secret_file)
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    println!("cargo:rustc-env=MAILTHIN_BUILTIN_GOOGLE_CLIENT_SECRET={secret}");
}
