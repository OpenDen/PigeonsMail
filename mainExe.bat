cargo clean
. .\scripts\dev-env.ps1
cargo build --release -p mailthin-app
.\target\release\mailthin-app.exe