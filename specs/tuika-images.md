# tuika image rendering

## Why

tuika renders everything into a ratatui cell buffer, where a cell carries one
grapheme plus a style and nothing else. Pictures — an avatar in a chat
transcript, a chart, a rendered diagram, an image referenced from markdown —
have no home in that model. Terminals that speak a graphics protocol (Kitty
graphics, iTerm2 inline images, Sixel) can paint real pixels, but only via
escape sequences that live *outside* the cell buffer.

tuika already smuggles out-of-band escapes past ratatui three times — OSC 8
hyperlinks (`hyperlink.rs`), OSC 52 clipboard (`clipboard.rs`), and OSC 9;4
progress (`native.rs`). Image support follows the same shape, with one new
constraint those three did not have (see *Cursor* below).

## What

A host can place a decoded image into the layout, sized in whole terminal
cells, and have it painted by the terminal's graphics protocol over the cells
tuika reserved for it. When the terminal does not support the protocol, the
same component degrades to a text placeholder (its alt text) with no garbage on
screen.

Scope is deliberately phased:

- **Phase 1 (this spec's initial cut): standalone `Image` component, Kitty
  graphics protocol only.** A host builds an `Image` from raw RGBA it decoded
  itself, reserves cells for it in the layout, and emits the graphics escapes
  after each frame. Capability detection gates the protocol; unsupported
  terminals get the alt-text fallback.
- **Phase 2: more protocols** behind the same component — iTerm2 inline images
  and Sixel — selected by capability detection.
- **Phase 3: markdown `![alt](url)`** wired to the component, including the
  streaming-cache integration in `MarkdownState`.

## Design

### Decoding stays in the host

tuika depends only on ratatui, crossterm, textwrap, unicode-\*, and
pulldown-cmark, and adds nothing heavy. Image *decoding* (PNG/JPEG → RGBA) is a
heavy dependency, so it stays in the host exactly like syntax highlighting does:
the host hands tuika an `ImageData` of raw RGBA plus pixel dimensions, the way
it hands markdown a `Highlighter`. tuika owns *presentation* (protocol
encoding, cell reservation, fallback), never decoding.

### The cell-reservation + out-of-band-emit model

An image cannot be written through `Surface` — `Surface` only writes cells. So
the model splits placement from emission, using the seam `RectProbe` already
established for reading a view's painted rect back out:

1. The `Image` view reserves a `cols × rows` cell footprint via `measure`, so
   the flex solver lays out around it like any other component.
2. On `render`, the view records its **absolute** painted `Rect` plus a handle
   to its pixel data into a shared `ImageLayer` (an `Rc<RefCell<…>>` handle,
   cheap to clone, cleared each frame — the same ownership shape as
   `RectProbe`). It paints the reserved cells blank (or, when unsupported, the
   alt-text placeholder) so ratatui's diff has stable content there.
3. **After** `terminal.draw()` returns, the host calls `ImageLayer::emit`,
   which writes each image's graphics escape positioned at its cell origin.

Emission happens after the frame because the graphics escape paints pixels the
cell buffer knows nothing about; doing it inside the paint pass would fight
ratatui's diff.

### Cursor: the one new constraint

OSC 8 / 52 / 9;4 are **cursor-neutral** — they can be spliced into a drawn cell
run without moving the cursor, which is why `HyperlinkBackend` can inline them.
Graphics escapes are **not**: Kitty places the image at the cursor. So
`ImageLayer::emit` wraps its writes in a cursor save/restore (`ESC 7` … `ESC
8`) and moves the cursor to each image's cell origin with a CUP (`ESC [ row ;
col H`) before emitting. Net effect on ratatui's cursor model is nil, so the
diff stays consistent.

### Terminal-response suppression

Kitty acknowledges every graphics command on the tty by default. In a
full-screen TUI those replies would be read by the event loop as bogus input.
Every command sets `q=2` to suppress both success and error responses.

### Capability detection is required

The existing out-of-band features rely on "an unknown OSC is swallowed
harmlessly." That does not hold for graphics protocols — an unsupported
terminal may render the payload as visible garbage. So image support is the
first tuika feature to need real capability detection. Phase 1 uses a
conservative environment probe (`TERM`, `TERM_PROGRAM`, `KITTY_WINDOW_ID`, and
the Ghostty marker), plus an explicit host override, and defaults to *no*
graphics (text fallback) when unsure. A future phase may add a runtime query
(the Kitty graphics query escape) for certainty.

### Re-transmission (known Phase-1 limitation)

Phase 1 transmits pixel data on every emit (`a=T`, transmit-and-display). This
is simple and correct but re-uploads the image each frame. A later phase should
transmit once with an image id (`a=t`, `i=…`) and thereafter only place it
(`a=p`), tracking which ids the terminal already holds.

## Non-goals

- Image *decoding* in tuika (host's job).
- Animation / video.
- Sub-cell pixel-exact sizing driven by cell-pixel geometry queries — sizing is
  quantized to whole cells (`View::measure` is `u16`).
