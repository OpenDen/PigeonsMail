cargo clean
. .\scripts\dev-env.ps1
cargo build --release -p pigeonmail-app
.\target\release\pigeonmail-app.exe