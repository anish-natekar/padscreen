# Contributing

Issues and pull requests are welcome.

## Before you send a change

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
shellcheck scripts/*.sh packaging/padscreenctl
```

CI runs the same. Tests marked `#[ignore]` need a real GNOME Wayland (or X11) session and change your displays;
run them only in a session where your work is saved:

```bash
cargo test --test managed_wayland -- --ignored --nocapture
```

## Guidelines

- Keep behaviour changes covered by a test. Pure logic (layout planning, parsing, rate-control maths) has unit
  tests; anything that talks to Mutter or the network has an integration test.
- Do not commit personal data: certificates, keys, `.env`, logs, device addresses. `.gitignore` covers the usual
  names; check `git status` anyway.
- New options go in `ServeArgs` with an `env = "PADSCREEN_..."` name, and in `.env.example` and the README.
- Security-relevant changes (who may connect, what a client may send) get a note in `SECURITY.md`.
- Comments say why, not what. Match the surrounding style; `cargo fmt` decides formatting.

Architecture notes are in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

By contributing you agree that your contributions are licensed under the MIT License.
