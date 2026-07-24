#!/usr/bin/env bash
set -euo pipefail

demo_dir=/tmp/yolop-hero-mission-control
session_dir=/tmp/yolop-hero-sessions

rm -rf "$demo_dir" "$session_dir"
mkdir -p "$demo_dir/src" "$session_dir"

cat >"$demo_dir/Cargo.toml" <<'EOF'
[package]
name = "mission-control"
version = "0.1.0"
edition = "2024"

[dependencies]
clap = { version = "=4.5.20", features = ["derive"] }
serde = { version = "=1.0.210", features = ["derive"] }
serde_json = "=1.0.128"
EOF

cat >"$demo_dir/src/main.rs" <<'EOF'
use clap::Parser;

#[derive(Parser)]
#[command(name = "mission-control", about = "Print a spacecraft launch status")]
struct Cli {
    /// Mission callsign
    #[arg(default_value = "Odyssey")]
    mission: String,
}

fn status(mission: &str) -> String {
    format!("{mission}: all systems nominal")
}

fn main() {
    let cli = Cli::parse();
    println!("{}", status(&cli.mission));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_text_status() {
        assert_eq!(status("Odyssey"), "Odyssey: all systems nominal");
    }
}
EOF

cat >"$demo_dir/README.md" <<'EOF'
# Mission Control

A tiny launch-status CLI.

```console
$ cargo run -- Odyssey
Odyssey: all systems nominal
```
EOF

(
    cd "$demo_dir"
    cargo generate-lockfile -q
)
