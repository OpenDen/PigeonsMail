# Dot-source before building on Windows:  . .\scripts\dev-env.ps1
# Puts Rust and Strawberry Perl on PATH. Perl is needed to build the vendored OpenSSL used by
# SQLCipher (Git's bundled Perl is not sufficient).
$env:Path = "C:\Strawberry\perl\bin;$env:USERPROFILE\.cargo\bin;$env:Path"
