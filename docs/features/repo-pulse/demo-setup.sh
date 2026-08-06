#!/usr/bin/env bash
set -euo pipefail

demo_dir=/tmp/yolop-repo-pulse-demo
session_dir=/tmp/yolop-repo-pulse-sessions
bin_dir=/tmp/yolop-repo-pulse-bin

rm -rf "$demo_dir" "$session_dir" "$bin_dir"
mkdir -p "$demo_dir/src" "$session_dir" "$bin_dir"

git -C "$demo_dir" init -q -b feature/repo-pulse
git -C "$demo_dir" config user.name "Demo User"
git -C "$demo_dir" config user.email "demo@example.com"

cat >"$demo_dir/README.md" <<'EOF'
# Flight Deck

A small terminal operations workspace.
EOF

cat >"$demo_dir/Cargo.toml" <<'EOF'
[package]
name = "flight-deck"
version = "0.1.0"
edition = "2024"
EOF

git -C "$demo_dir" add README.md Cargo.toml
git -C "$demo_dir" commit -qm "feat: initialize terminal workspace"
git -C "$demo_dir" remote add origin https://github.com/everruns/yolop.git

printf '\nRepository pulse demo.\n' >>"$demo_dir/README.md"
printf 'pub fn status() -> &\x27static str { "ready" }\n' >"$demo_dir/src/lib.rs"

cat >"$bin_dir/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

if [[ " $* " == *" status --porcelain=v1 "* ]]; then
  sleep 6
fi
exec /usr/bin/git "$@"
EOF
chmod +x "$bin_dir/git"
