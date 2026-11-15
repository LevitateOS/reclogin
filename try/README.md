# The display try (2026-11-15)

A throwaway program that showed, on both OSes, what `docs/reclogin-technical.md`
(section 10, in the LevitateOS repository) rests on. It is not part of reclogin
and is not a workspace member. Keep it as a record; do not grow it.

- `spike daemon SECONDS` (as root): takes a free console and the display,
  opens the input devices, and starts itself as `spike greeter` under uid
  65534, which sets the mode, draws a test login picture on the processor and
  reads keys and the pointer through the handles it was given.
- `spike session USER wayland|x11 SECONDS COMMAND` (as root, outside any login
  session): asks the login service for a new session on a free console, hands
  that console to the user, runs COMMAND there as the user, shows it, ends it
  and shows the first console again.

Build one static program that runs on both OSes:

```sh
cargo build --release --target x86_64-unknown-linux-musl
```

It embeds two files that are not kept here: `frame.png` (the website's
`public/art/levitate-frame.webp`, converted) and `font.ttf` (any TrueType
font). Put them next to `Cargo.toml` first.
