@echo off
rem Chat with a local Ollama model that can edit audio in your Music folder.
rem Change the model or folder below if you like.
"%~dp0..\target\release\rust_audacity_chat.exe" --model qwen3:8b --root "%USERPROFILE%\Music" %*
pause
