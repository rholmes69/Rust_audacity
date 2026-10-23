@echo off
rem Puts a "Rust Audacity" shortcut (with the gold logo icon) on your desktop.
rem Works from a downloaded release (rust_audacity.exe next to this file) or from the
rem source folder (builds the app first if it hasn't been built yet).
cd /d "%~dp0"
set "EXE=%~dp0rust_audacity.exe"
if exist "%EXE%" goto make
set "EXE=%~dp0target\release\rust_audacity.exe"
if not exist "%EXE%" (
  echo Building Rust Audacity for the first time, this takes a few minutes...
  cargo build --release
  if errorlevel 1 (
    echo.
    echo Build failed. Install Rust from https://rustup.rs and try again.
    pause
    exit /b 1
  )
)
:make
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$d=[Environment]::GetFolderPath('Desktop');" ^
  "$s=(New-Object -ComObject WScript.Shell).CreateShortcut((Join-Path $d 'Rust Audacity.lnk'));" ^
  "$s.TargetPath='%EXE%';" ^
  "$s.WorkingDirectory='%~dp0';" ^
  "$s.IconLocation='%EXE%,0';" ^
  "$s.Description='Rust Audacity - audio editor';" ^
  "$s.Save(); Write-Host ('Shortcut created on ' + $d)"
pause
