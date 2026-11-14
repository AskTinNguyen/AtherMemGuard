@echo off
rem Starts the MemCap watchers at logon. Put a shortcut to this file in shell:startup.
rem Edit MEMCAP to the permanent folder where MemCap.ps1 lives.
set MEMCAP=C:\Tools\MemCap\MemCap.ps1
set PS=powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "%MEMCAP%"

rem Give apps a minute to start so the first attach catches them; watchers re-attach every 15 s anyway.
timeout /t 60 /nobreak >nul

start "" %PS% -Group Chrome -Attach chrome -MemoryPriority Low -TrimAboveMB 4096 -Watch
start "" %PS% -Group Chat -Attach Slack,ChatGPT,'Wispr Flow' -MemoryPriority VeryLow -TrimAboveMB 1536 -Watch
start "" %PS% -Group Claude -Attach claude -MemoryPriority Low -Watch
