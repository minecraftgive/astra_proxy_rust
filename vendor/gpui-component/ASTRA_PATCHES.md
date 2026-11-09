# Astra patches to gpui-component 0.9.0

Source: crates.io `gpui-component` 0.9.0, upstream
https://github.com/longbridge/gpui-kit, revision
`7591a35eced761e3613c01e71ff04c10129dae1a`, `crates/component`.
The original Apache-2.0 license is retained in `LICENSE-APACHE`; the source
revision is retained in `.cargo_vcs_info.json`. The normalized package manifest
is used locally, with corresponding feature changes in `Cargo.toml.orig`.

## Optional extra components

Add the default-enabled `extra-components` feature around chart, dock, diff,
calendar/date/time, questionnaire, carousel and table modules. Their startup
registrations and date/time re-exports use the same feature. `chrono` and
`similar` are optional dependencies of this group; their only direct consumers
in this crate are the date/time and diff modules respectively.

Default builds keep all upstream modules and initialization. Astra requests
`default-features = false`: it uses its own chart/list views and none of these
extra component APIs. Input, editor, selection, menu, notification, text,
theme, inspector, platform backends and image capabilities remain unchanged.
In particular, Markdown, LSP and theme-watching dependencies are not removed:
the retained foundation uses them.

## Validation

Run the Astra GUI checks and tests with its default dependency selection:

```sh
cargo check --locked -p astra-gui --all-targets
cargo test --locked -p astra-gui
```

Also compile the default/full upstream configuration by enabling
`gpui-component/extra-components` for the GUI build:

```sh
cargo check --locked -p astra-gui --features gpui-component/extra-components
```

Verified on macOS arm64 on 2026-11-09: the Astra workspace all-target check
passed with extra components disabled; the command above also passed with
extra components enabled. Private table/diff-only helpers are gated with
their callers so the reduced build introduces no unused-code warnings.

Compare both builds before updating this patch to a new upstream release.
Desktop smoke tests remain necessary on macOS, Windows and Linux; successful
Rust compilation is not a substitute for window, editor and input behavior
checks.
