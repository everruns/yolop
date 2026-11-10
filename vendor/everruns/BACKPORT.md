# Session SQLite backport

This is the published `everruns` 0.51.0 facade, with the session-directory
SQLite change from [everruns/everruns#5323](https://github.com/everruns/everruns/pull/5323).
The implementation module is byte-identical to upstream commit `e0c03d2f9`. The other Everruns family
crates stay on aligned published 0.51.0 versions. The module SHA-256 is
`c25dddc71f4f1c2917cdde47cf0eabe0dff2efdd078bb5f98d3f6d11aa0494bd`.

Changed upstream files:

- `src/hosting/sqlite/mod.rs`
- `src/hosting/sqlite/ports.rs`
- `src/hosting/sqlite/services.rs`
- `src/hosting/sqlite/session_directory.rs` (identical upstream and here)

The facade is vendored because child execution, fenced persistence and legacy
cutover use private framework seams. Reimplementing them in Yolop would create
a second execution authority.

TODO: once an aligned Everruns release includes `SqliteHosting::open_session`,
`open_session_from` and `schedule_store`, bump every Everruns family pin, remove
the Cargo patch and this vendor directory, and rerun migration, child restart,
lock isolation and the full workspace checks. Remove the backport before
publishing Yolop to crates.io; source and binary builds use it in the meantime.
