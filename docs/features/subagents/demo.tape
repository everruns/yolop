Require cargo
Require node

Output docs/features/subagents/demo.gif
Output docs/features/subagents/demo.mp4

Set Shell zsh
Set Width 1500
Set Height 900
Set FontSize 15
Set FontFamily "Menlo, DejaVu Sans Mono, monospace"
Set LetterSpacing 0
Set LineHeight 1.1
Set TypingSpeed 18ms
Set CursorBlink false
Set Theme "Dracula"
Set WindowBar Colorful
Set BorderRadius 12
Set Padding 12
Set Margin 24
Set MarginFill "#5b4b8a"
Set Framerate 30
Set PlaybackSpeed 2.2

Env NO_COLOR ""
Env COLORTERM "truecolor"
Env RUST_LOG "error"

Hide
Type "cargo build -q && docs/features/subagents/demo-setup.sh && alias yolop='target/debug/yolop -C /tmp/yolop-subagents-mission-control --theme dracula --provider codex -m gpt-5.6-sol --reasoning-effort medium --session-dir /tmp/yolop-subagents-sessions' && clear"
Enter
Wait+Line@5m /[>$%#]/
Show
Sleep 1s

Type "(cd /tmp/yolop-subagents-mission-control && node dashboard.mjs)"
Enter
Wait+Screen /HOLD — 0\/16 systems green/
Sleep 2s

Type "yolop"
Enter
Sleep 3s

Type "Restore Mission Control. Follow AGENTS.md exactly: create the 4x4 background swarm (exactly 20 sub-agents total). Critical: every spawn_agent call at both levels must contain only name, instructions, target.type=subagent, and mode=background; omit every optional field. Do not run the full suite or dashboard; the recorder owns final verification. No Git or network. Once all four leads have returned, finish with exactly SWARM DISPATCHED — WATCH THE BOARD."
Enter
Wait+Screen@3m /AGENTS [1-9]/
Wait+Screen@5m /AGENTS 20/
Sleep 12s

Wait+Screen@8m /SWARM DISPATCHED — WATCH THE BOARD/
Sleep 30s
Ctrl+D
Sleep 3s
Type "until (cd /tmp/yolop-subagents-mission-control && node dashboard.mjs >/dev/null 2>&1); do sleep 1; done; clear; (cd /tmp/yolop-subagents-mission-control && node dashboard.mjs)"
Enter
Wait+Screen@2m /LAUNCH READY — 16\/16 systems green/
Sleep 5s
