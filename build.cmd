@echo off
chcp 65001 >nul
rem Сборка efc под Windows. Нужен Rust (https://rustup.rs).
cargo build --release || exit /b 1
if not exist dist mkdir dist
copy /y target\release\efc.exe dist\efc.exe >nul || exit /b 1
echo Готово: dist\efc.exe
