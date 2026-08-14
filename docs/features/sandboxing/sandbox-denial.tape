Output "docs/features/sandboxing/sandbox-denial.gif"

Set Shell zsh
Set Width 1200
Set Height 440
Set FontSize 17
Set FontFamily "Menlo, DejaVu Sans Mono, monospace"
Set LetterSpacing 0
Set TypingSpeed 35ms
Set CursorBlink false
Set WindowBar Colorful
Set BorderRadius 10
Set Margin 24
Set MarginFill "#111318"
Env NO_COLOR ""
Env COLORTERM "truecolor"

Type "target/debug/yolop --provider llmsim --session-dir /tmp/yolop-vhs-sessions"
Enter
Sleep 3s
Type "!touch ../yolop-sandbox-escape-proof.txt"
Enter
Wait+Screen /native sandbox likely blocked this operation/
Sleep 2s
Ctrl+C
