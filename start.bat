@echo off
chcp 65001 >nul
cd /d "%~dp0"

echo 启动豆包本地生图服务 http://127.0.0.1:8788
echo 先勾选免责声明、输入启动密码，面板才会弹出（关掉面板是收进托盘，服务继续跑）
start "" "%~dp0豆包生图服务.exe"
