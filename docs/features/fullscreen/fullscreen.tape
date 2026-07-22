# yolop's experimental full-screen (alternate-screen) TUI, driven by the
# offline `llmsim` provider so the recording is deterministic and needs no API
# key. Regenerate from the repo root with a debug build present:
#
#   cargo build
#   vhs docs/features/fullscreen/fullscreen.tape
Output "docs/features/fullscreen/fullscreen.gif"

Set Shell zsh
Set Width 1480
Set Height 520
Set FontSize 17
Set FontFamily "Menlo"
Set LetterSpacing 0
Set TypingSpeed 35ms
Set CursorBlink false
Set WindowBar Colorful
Set BorderRadius 10
Set Margin 24
Set MarginFill "#111318"
Env NO_COLOR ""
Env COLORTERM "truecolor"
# A credential in the environment suppresses first-run onboarding; `--provider
# llmsim` still forces the offline driver, so no real request is ever made.
Env OPENAI_API_KEY "offline-llmsim-demo"

# Put the freshly built binary on PATH and drop into a clean throwaway
# workspace so the visible command reads like a normal `yolop` invocation and
# the transcript is deterministic.
Hide
Type `export PATH="$PWD/target/debug:$PATH"; rm -rf /tmp/yolop-demo; cd "$(mktemp -d)" && git init -q && printf 'fn main() {\n    println!("hello, yolop");\n}\n' > main.rs && git add main.rs && git -c user.email=demo@yolop -c user.name=demo commit -qm "init" >/dev/null && clear`
Enter
Sleep 1s
Show

# Launch the full-screen renderer on the alternate screen.
Type "yolop --fullscreen --provider llmsim --session-dir /tmp/yolop-demo"
Enter
Sleep 3s

# Send a prompt; the offline provider answers with its fixed message.
Type "What can you help me with?"
Sleep 500ms
Enter
Sleep 3s

Type "Great — how do I turn on a real model?"
Sleep 500ms
Enter
Sleep 3s

# Exit: Ctrl-C twice (or Ctrl-D).
Ctrl+C
Sleep 400ms
Ctrl+C
Sleep 1s
