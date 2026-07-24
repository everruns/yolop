Require cargo

Output docs/demo.gif

Set Shell zsh
Set Width 1200
Set Height 720
Set FontSize 17
Set FontFamily "Menlo"
Set LetterSpacing 0
Set LineHeight 1.15
Set TypingSpeed 24ms
Set CursorBlink false
Set Theme "Dracula"
Set WindowBar Colorful
Set BorderRadius 12
Set Padding 14
Set Margin 28
Set MarginFill "#5b4b8a"
Set Framerate 30
Set PlaybackSpeed 2.4

Env NO_COLOR ""
Env COLORTERM "truecolor"
Env RUST_LOG "error"

Hide
Type "cargo build -q && docs/demo-setup.sh && alias yolop='target/debug/yolop -C /tmp/yolop-hero-mission-control --fullscreen --theme dracula --provider codex -m gpt-5.6-sol --reasoning-effort low --session-dir /tmp/yolop-hero-sessions' && clear"
Enter
Wait+Line@5m /[>$%#]/
Show
Sleep 1s

Type "yolop"
Enter
Sleep 3s

Type "Modernize mission-control: set clap=4.6.4, serde=1.0.229, and serde_json=1.0.151; add --json output; unit-test both text and JSON rendering using only existing dependencies; run fmt and tests. Skip Git. Keep it concise. Finish with Ready+for+launch (replace + with spaces)."
Enter
Wait+Screen@3m /Ready for launch/
Sleep 4s
