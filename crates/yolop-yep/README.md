# yolop-yep

The **yolop extension protocol (YEP)**: shared wire types plus a small server
SDK for authoring [yolop](https://github.com/everruns/yolop) extension
capability servers in Rust.

A yolop extension's executable logic is a subprocess yolop spawns and talks to
over newline-delimited JSON on stdio. This crate is the single source of truth
for that wire format (also used by the yolop host), and gives authors a
handler-based server instead of a hand-rolled JSON-RPC loop.

```rust
use yolop_yep::{Server, ToolResponse};
use serde_json::json;

fn main() -> std::io::Result<()> {
    Server::new("my-ext")
        .instructions("What the model should know about this extension.")
        .tool("greet", |args| {
            let who = args.get("who").and_then(|v| v.as_str()).unwrap_or("world");
            ToolResponse::ok(json!({ "greeting": format!("hello, {who}") }))
        })
        .serve() // reads stdin until EOF
}
```

Package the built binary with a `plugin.json` whose
`yolop.capabilityServer.command` points at it, name the crate
`yolop-extension-<name>`, and users install it with `/extensions install`.

See [`specs/extensions.md`](https://github.com/everruns/yolop/blob/main/specs/extensions.md)
for the full protocol and packaging model.

## Status

Pre-1.0, tracking the yolop extension protocol version (currently `1.0`). The
protocol follows `MAJOR.MINOR` with major-match compatibility and additive
minors; unknown fields are ignored and missing fields defaulted.
