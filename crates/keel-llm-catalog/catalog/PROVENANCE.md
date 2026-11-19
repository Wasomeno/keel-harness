# models.dev snapshot provenance

`models.dev.toml` is a vendored snapshot of model facts from
[models.dev](https://models.dev).

- Source: <https://models.dev/api.json>
- Fetched: 2026-11-19
- Generator: `cargo run -p keel-catalog-gen` (or `--from FILE` for a local dump)
- License: MIT, Copyright (c) 2025 models.dev
  (<https://github.com/sst/models.dev/blob/dev/LICENSE>)

Only facts are copied: provider and model ids, names, context and output
limits, tool/image/reasoning capabilities, and the `deprecated` status for
models the API marks as such. The snapshot is filtered to the providers and
model ids the checked-in `overlay.toml` routes; it is not the whole models.dev
catalog.

Endpoints, auth, per-model protocol, path overrides, `compat` overrides and the
`cache` capability are not in models.dev. They live in `overlay.toml` and win
over the snapshot.

The snapshot is committed and embedded with `include_str!`; a build never
fetches models.dev. Regeneration is an explicit developer action. The generator
writes through a temporary file and renames it into place, so a failed fetch or
parse leaves the previous (last-known-good) file untouched.

The API no longer lists these older models: `anthropic/claude-opus-4-1`,
`opencode/gpt-4.1`, `opencode/gemini-2.5-pro`, both `commandcode` models, and
both `grok` models. Their facts are pinned in `overlay.toml`. It also does not
carry the `antigravity` or `custom` providers at all; their four models pin
every fact in the overlay. Three facts differ from models.dev at generation
time and are pinned to preserve existing keel behavior: `openai/gpt-5.5`
context (400000 vs 1050000), `anthropic/claude-haiku-4-5` output (32000 vs
64000), and `opencode/claude-sonnet-4-5` context (200000 vs 1000000).
