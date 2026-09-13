Require cargo

Output docs/demo.gif

Set Shell zsh
Set Width 1200
Set Height 720
Set FontSize 17
Set FontFamily "Menlo, DejaVu Sans Mono, monospace"
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
Type "cargo build -q && docs/demo-setup.sh && alias yolop='target/debug/yolop -C /tmp/yolop-hero-pizza-rocket --theme dracula --provider openai --reasoning-effort low --session-dir /tmp/yolop-hero-sessions' && clear"
Enter
Wait+Line@5m /[>$%#]/
Show
Sleep 1s

Type "yolop"
Enter
Sleep 3s

Type "Throw a pizza-rocket launch party: set clap=4.6.4, serde=1.0.229, serde_json=1.0.151; add --json for cheesy delivery status plus --party that prints an ASCII rocket, a 3-2-1 countdown, and extra cheese; unit-test text and JSON; run fmt and tests; then run cargo run -- --party to show the party. Skip Git. Keep it concise. Finish with Pizza+away! (replace + with space)."
Enter
Wait+Screen@3m /Pizza away!/
Sleep 4s
