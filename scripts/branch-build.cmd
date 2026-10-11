@echo off
setlocal
rem Scope the execution-policy bypass to this child process only.
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0branch-build.ps1" %*
exit /b %errorlevel%
