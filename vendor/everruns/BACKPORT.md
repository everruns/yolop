# Session SQLite backport

This is the published `everruns` 0.51.0 facade with the session-directory
SQLite change from [everruns/everruns#5323](https://github.com/everruns/everruns/pull/5323)
and the layout-transfer and read-only inspection changes from
[everruns/everruns#5345](https://github.com/everruns/everruns/pull/5345) at commit
`3272a32077a489d80ed30890dad9d3c687b787a2`. Other Everruns family crates stay
on aligned published 0.51.0 versions.

Changed upstream files:

- `src/hosting/sqlite/mod.rs`
- `src/hosting/sqlite/ports.rs`
- `src/hosting/sqlite/services.rs`
- `src/hosting/sqlite/export.rs`
- `src/hosting/sqlite/session_directory.rs`
- `src/hosting/sqlite/session_directory/source.rs`

The last three modules are byte-identical to that upstream commit. SHA-256:

- `export.rs`: `c130831a21024305734129e17b50ccda3db0b9e751a3650cd761ad27d254acf5`
- `session_directory.rs`: `ff07abfe22924f429e625664bc84dbf9c6f8c535a6d3d8e03e651d393bdc38fc`
- `session_directory/source.rs`: `d93eaed08680b42663f64801989f4df3cf2d893287916dcfdbda7f694db5d2a2`

The facade is vendored because child execution, fenced persistence and legacy
cutover use private framework seams. Reimplementing them in Yolop would create
a second execution authority.

TODO: once an aligned Everruns release includes `SqliteHosting::open_session`,
`open_session_from_sources`, `schedule_store`, `visit_session_events` and the
visible `background/` runtime-artifact policy, bump every Everruns family pin,
remove the Cargo patch, vendor directory and the filesystem policy alias in
`src/runtime/mod.rs`. Retain aliases for saved task paths and rerun layout
migration, history search, child restart, background completion, lock isolation
and the full workspace checks. Remove the backport before publishing Yolop to
crates.io; source and binary builds use it in the meantime.
