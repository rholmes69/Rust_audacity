@echo off
rem Puts a "Rust Audacity" shortcut (with the gold logo icon) on your desktop.
rem Builds the app first if it hasn't been built yet.
cd /d "%~dp0"
if not exist "target\release\rust_audacity.exe" (
  echo Building Rust Audacity for the first time, this takes a few minutes...
  cargo build --release
  if errorlevel 1 (
    echo.
    echo Build failed. Install Rust from https://rustup.rs and try again.
    pause
    exit /b 1
  )
)
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$d=[Environment]::GetFolderPath('Desktop');" ^
  "$s=(New-Object -ComObject WScript.Shell).CreateShortcut((Join-Path $d 'Rust Audacity.lnk'));" ^
  "$s.TargetPath='%~dp0target\release\rust_audacity.exe';" ^
  "$s.WorkingDirectory='%~dp0';" ^
  "$s.IconLocation='%~dp0assets\rust_audacity.ico,0';" ^
  "$s.Description='Rust Audacity - audio editor';" ^
  "$s.Save(); Write-Host ('Shortcut created on ' + $d)"
pause
