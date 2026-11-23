@echo off
REM Requires Rust: https://rustup.rs  (one-time install)
cargo build --release
echo.
echo Done! Your app: target\release\video-segmenter.exe
pause
