#!/bin/bash
# clawpit 一键启动：hub + TUI 都跑进 tmux，已存在则跳过。
# 用法：bash ~/p/clawpit/start.sh   （WSL 重启后跑一次即可）
set -euo pipefail
BIN="$(dirname "$(readlink -f "$0")")/target/debug"

# tmux server 可能整个没了（WSL 重启），先确保存在
tmux start-server 2>/dev/null || true

if ! tmux has-session -t clawpit-hub 2>/dev/null; then
  tmux new-session -d -s clawpit-hub "$BIN/clawpit"
  echo "✓ hub 已启动（tmux 会话 clawpit-hub，http://localhost:7664）"
else
  echo "· hub 已在运行"
fi

if ! tmux has-session -t clawpit-tui 2>/dev/null; then
  tmux new-session -d -s clawpit-tui -x 180 -y 45 "$BIN/clawpit-tui"
  echo "✓ TUI 已启动（tmux attach -t clawpit-tui）"
else
  echo "· TUI 已在运行"
fi

sleep 1.5
curl -s -m 3 http://localhost:7664/health && echo " ← daemon 健康"
