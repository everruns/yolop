Require cargo

Output docs/features/repo-pulse/demo.gif
Output docs/features/repo-pulse/demo.mp4

Set Shell zsh
Set Width 1500
Set Height 820
Set FontSize 16
Set FontFamily "Menlo, DejaVu Sans Mono, monospace"
Set LetterSpacing 0
Set LineHeight 1.12
Set TypingSpeed 24ms
Set CursorBlink false
Set Theme "Dracula"
Set WindowBar Colorful
Set BorderRadius 12
Set Padding 14
Set Margin 28
Set MarginFill "#5b4b8a"
Set Framerate 30
Set PlaybackSpeed 1.5

Env NO_COLOR ""
Env COLORTERM "truecolor"
Env RUST_LOG "error"

Hide
Type "cargo build -q && docs/features/repo-pulse/demo-setup.sh && alias yolop='PATH=/tmp/yolop-repo-pulse-bin:$PATH target/debug/yolop -C /tmp/yolop-repo-pulse-demo --theme dracula --provider llmsim --session-dir /tmp/yolop-repo-pulse-sessions' && clear"
Enter
Wait+Line@5m /[>$%#]/
Show
Sleep 1s

Type "yolop"
Enter
Wait+Screen@20s /Ready in .*yolop-repo-pulse-demo/
Sleep 300ms

Type "Review this repository and suggest one focused improvement."
Wait+Screen@10s /everruns\/yolop on feature\/repo-pulse/
Wait+Screen@10s /2 changed paths/
Sleep 2s

Enter
Wait+Screen@20s /real responses/
Sleep 4s
