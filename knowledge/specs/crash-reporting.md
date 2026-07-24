---
type: Product Specification
title: Crash reporting
description: Defines local, privacy-preserving reports for unexpected Yolop panics.
---

# Crash reporting

Yolop writes a local crash report when a process thread panics. This is
especially important for the alternate-screen TUI: the original panic hook runs
before terminal guards restore the user's screen, so its output can otherwise
disappear when the terminal returns to normal mode.

Reports live under the platform user-data directory:

- Linux: `$XDG_DATA_HOME/yolop/crashes/`
- macOS: `~/Library/Application Support/yolop/crashes/`
- Windows: `%APPDATA%\yolop\crashes\`

Names combine a UTC timestamp with a random suffix. Yolop keeps the newest five
reports. On Unix the directory is owner-only (`0o700`) and each report is
`0o600`.

A report contains only the Yolop version, timestamp, panic thread, source
location, a bounded panic message, and a bounded backtrace. Yolop does not add
prompts, messages, tool arguments or results, environment variables,
credentials, or tracing logs. A panic message can still contain application
data, so reports remain private local files and should be reviewed before
sharing. Writing is best-effort and must never cause another panic.

When the large-stack application thread panics, terminal guards restore the
screen first. Yolop then prints the report path and resumes the original panic
payload; the outer thread must not replace it with a generic `Any` diagnostic.
