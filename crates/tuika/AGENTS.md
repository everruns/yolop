# tuika — coding-agent guidance

`tuika` is a standalone, published terminal-UI toolkit (layout, overlays, focus,
components over ratatui). It knows nothing about yolop. See `README.md` for the
model and `crates/tuika/README.md`'s test section for how rendering is tested
hermetically.

## Docs layout

- `docs/components.md` — the public component gallery (name, description, demo
  per component). Keep it **presentational only**: no build or regeneration
  instructions belong here (they live in this file).
- `docs/demos/*.gif` — the committed demo recordings referenced by
  `components.md` and, via `raw.githubusercontent.com` URLs, inline on each
  component's `struct` doc so they render on docs.rs.
- `docs/hero.gif` — the README hero: a recording of a composite gallery screen.
  See [Hero screenshot](#hero-screenshot).

## Component demos

One example is the single source of truth for the gallery:
[`examples/demo.rs`](examples/demo.rs). Its `DEMOS` registry declares every
scene (name, blurb, recording size, builder); the CLI, the tape generator, and
the integrity check all read it.

```bash
cargo run -p tuika --example demo -- list            # list scenes
cargo run -p tuika --example demo -- spinner          # interactive (q/esc quits)
cargo run -p tuika --example demo -- spinner --dump    # print one frame as text
cargo run -p tuika --example demo -- check             # verify the docs assets
```

### Regenerating the GIFs

[`scripts/gen-tuika-demos.sh`](scripts/gen-tuika-demos.sh) rebuilds every
recording. It asks the example to emit one VHS tape per scene
into a temp dir — **tapes are generated, not committed** — records each, and
runs `check`. Requires [VHS](https://github.com/charmbracelet/vhs) with `ttyd`
and `ffmpeg` on `PATH`.

```bash
scripts/gen-tuika-demos.sh              # all scenes
scripts/gen-tuika-demos.sh spinner tabs # just these
```

Recordings are captured at 2× pixel density and displayed at half width
(`width="880"` / rustdoc's `max-width`), so they stay crisp on HiDPI screens.

### Adding a component demo

1. Add a `scene_*` builder and a `DEMOS` entry in `examples/demo.rs` (set
   `rows` to the content height and `animated` for motion scenes).
2. Confirm it renders: `cargo run -p tuika --example demo -- <name> --dump`.
3. Record it: `scripts/gen-tuika-demos.sh <name>`.
4. Reference `demos/<name>.gif` in `docs/components.md` and inline on the
   component's `struct` doc (via the `raw.githubusercontent.com/.../main/...`
   URL, so docs.rs resolves it).

## Hero screenshot

The README hero (`docs/hero.gif`) is a VHS recording of a composite "app" scene
that exercises most of the toolkit at once. The scene lives in
[`examples/screenshot.rs`](examples/screenshot.rs) as one `scene()` builder, and
[`scripts/gen-tuika-hero.sh`](scripts/gen-tuika-hero.sh) records it: it builds
the example, runs its `run` subcommand full-screen under VHS (window bar for
chrome, theme background matched to tuika's), and writes the GIF. Requires the
same toolchain as the demos — VHS with `ttyd` and `ffmpeg`.

```bash
scripts/gen-tuika-hero.sh
```

Regenerate it whenever the components' look changes so the hero stays truthful.

The example doubles as an **offline, recorder-free** path: the same `scene()`
also serializes to a crisp animated SVG (block glyphs painted as rects; static
cells shared across frames, only the moving region duplicated). Handy when VHS
isn't available, or for a vector asset.

```bash
cargo run -p tuika --example screenshot            # write an SVG (default path)
cargo run -p tuika --example screenshot -- out.svg # custom path
cargo run -p tuika --example screenshot -- --dump  # print one frame as text
cargo run -p tuika --example screenshot -- run     # animate it (what VHS records)
```

### The check invariant

`demo -- check` asserts every scene has a non-empty recording, no orphan GIF
lingers, and every `demos/<name>.gif` referenced by a component doc or
`components.md` maps to a real scene. It runs in the `tuika-msrv` CI job and at
the end of the generator, so gallery drift fails CI instead of shipping a broken
image to docs.rs.
