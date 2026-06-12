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

- **model-config** (first official component, in progress): connect a local
  llama.cpp server or a cloud API key — a Rust API base layer + a settings
  UI. The canonical example of the shape; coming from the galaxy app.

More planned: secrets vault, agent terminal (xterm.js over vessel exec),
OAuth broker, scheduled/headless runner. See tasks/writedocumentation.md
in the nebula repo for the roadmap.
