# Contributing

## Development

Build the debug or release binary from the repository root:

```bash
cargo build
cargo build --release
```

Run offline tests:

```bash
cargo test
```

Run the live integration test against OpenAI through Doppler. This needs
`OPENAI_API_KEY` in your Doppler config:

```bash
doppler run -- cargo test --test integration -- --ignored
```

Format and lint:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

Run an offline smoke test:

```bash
cargo run -- --provider llmsim -p "hi"
```

Run a live OpenAI smoke test:

```bash
doppler run -- cargo run -- --provider openai -p "list the files in this repo"
```

## Community

Please report vulnerabilities through [`SECURITY.md`](./SECURITY.md). Project
participation is covered by [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md).
