#!/usr/bin/env bash
set -euo pipefail

demo_dir=/tmp/yolop-hero-pizza-rocket
session_dir=/tmp/yolop-hero-sessions

rm -rf "$demo_dir" "$session_dir"
mkdir -p "$demo_dir/src" "$session_dir"

cat >"$demo_dir/Cargo.toml" <<'EOF'
[package]
name = "pizza-rocket"
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
#[command(name = "pizza-rocket", about = "Print interplanetary pizza delivery status")]
struct Cli {
    /// Pizza order
    #[arg(default_value = "Margherita")]
    order: String,
}

fn status(order: &str) -> String {
    format!("{order}: oven hot, all toppings nominal")
}

fn main() {
    let cli = Cli::parse();
    println!("{}", status(&cli.order));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_text_status() {
        assert_eq!(status("Margherita"), "Margherita: oven hot, all toppings nominal");
    }
}
EOF

cat >"$demo_dir/README.md" <<'EOF'
# Pizza Rocket

A tiny interplanetary pizza-delivery status CLI.

```console
$ cargo run -- Margherita
Margherita: oven hot, all toppings nominal
```
EOF

(
    cd "$demo_dir"
    cargo generate-lockfile -q
)
