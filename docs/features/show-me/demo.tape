Require cargo

Output docs/features/show-me/demo.gif
Output docs/features/show-me/demo.mp4

Set Shell bash
Set Width 1600
Set Height 1000
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
Set Framerate 24
Set PlaybackSpeed 3.4

Env NO_COLOR ""
Env COLORTERM "truecolor"
Env RUST_LOG "error"

Hide
Type "cargo build -q && docs/features/show-me/demo-setup.sh && alias yolop='target/debug/yolop -C /tmp/yolop-show-me-orbit --theme dracula --provider anthropic -m claude-opus-4-8 --session-dir /tmp/yolop-show-me-sessions' && clear"
Enter
Wait+Line@5m /[>$%#]/
Show
Sleep 1s

Type "yolop"
Enter
Sleep 3s

Type "Find a skill called show-me in the skills registry and install the humanlayer one into this workspace. Keep it brief and finish with SKILL+INSTALLED (replace + with a space)."
Enter
Wait+Screen@4m /SKILL INSTALLED/
Sleep 3s

Type "Use the show-me skill: how does a repository deletion request flow through this service? Answer with one mermaid sequence diagram over Client, HTTP, Shard, Workflow, Tokens and Git. Keep it narrow so it fits an 80-column terminal: bare one-word participant names with no aliases, message labels of at most three words, no arguments, no Note lines. Read the code with the file tools only, no shell. No prose before it. Close with one short sentence that begins Tombstone+first (replace + with a space)."
Enter
Wait+Screen@5m /Tombstone first/
Sleep 8s
