@echo off
chcp 65001 >nul
powershell -NoProfile -ExecutionPolicy Bypass -File C:\dev\bench\run-bench.ps1 20 30
