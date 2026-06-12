# Components

Reference implementations of features most Nebula apps eventually want —
written to be **read, copied and adapted** (by you or your coding agent),
not installed as opaque dependencies.

## Contract (v0)

A component is a directory containing:

```
<name>/
  COMPONENT.md   what it does, why, and step-by-step wiring instructions
                 (written for a coding agent: exact files to touch)
  src/           implementation (Rust and/or JS — see COMPONENT.md)
  ui/            optional UI pieces
```

Drop the directory in `components/`, open its `COMPONENT.md`, follow the
steps. Components may assume the standard scaffold layout (AGENTS.md).

## Catalog

- **model-config** (first official component): cloud API keys + local
  llama.cpp/LM Studio endpoints — settings store (SQLite, write-only
  secrets), hyper routes, settings UI, and a deterministic mock model
  server for key-free e2e testing. Extracted from the galaxy app; hyper is
  the component standard for Rust HTTP.

More planned: secrets vault, agent terminal (xterm.js over vessel exec),
OAuth broker, scheduled/headless runner. See tasks/writedocumentation.md
in the nebula repo for the roadmap.
