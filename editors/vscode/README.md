# OrbitScript for VS Code

Highlighting, the `orbit-lsp` language server and the `orbit` debugger for OrbitScript (`.orb`, `.orbi`) and Lua 5.5.

This folder is self-contained: its own `package.json`, no build coupling to Cargo (ADR-12). It isn't on the marketplace yet.

## What it does

- **Highlighting** for OrbitScript from a TextMate grammar, refined by the server's semantic tokens. Lua keeps VS Code's own grammar.
- **The language server** (`orbit-lsp`, see [docs/design/language-server.md](../../docs/design/language-server.md)) for `.orb` files. It gives errors as you type, outline, go to definition, references, rename, hover, completion, signature help, inlay hints, workspace symbols and formatting (`orbit-fmt`'s style, see [docs/design/formatter.md](../../docs/design/formatter.md)). OrbitScript files are formatted on save; set `editor.formatOnSave` to `false` under `[orbitscript]` to turn that off. With `orbit.server.lua` on, Lua files get syntax errors from it too, but they aren't formatted.
- **Debugging** with the `orbit` debug type:
  - `launch` runs a script through `orbit --debug-server`.
  - `attach` connects to a running program through `orbit --debug-attach <pid>`.
  - The process picker lists what `orbit --debug-attach --list` finds. On Linux and macOS that is every `orbit` program; on Windows only those started with `orbit --debug-listen` (others can still be attached by pid).

## Settings

| Setting | Default | |
|---|---|---|
| `orbit.server.path` | `orbit-lsp` | The language server: a path, or a name on `PATH` |
| `orbit.server.searchPath` | `[]` | Module templates (`lib/?.orb`) tried before `package.path`'s |
| `orbit.server.lua` | `false` | Also serve Lua files |
| `orbit.debug.orbitPath` | `orbit` | The `orbit` the debugger runs |
| `orbit.trace.server` | `off` | Log LSP messages in the OrbitScript output |

## Debug configurations

```json
{
  "version": "0.2.0",
  "configurations": [
    { "type": "orbit", "request": "launch", "name": "Run main", "program": "${workspaceFolder}/main.orb", "args": [] },
    { "type": "orbit", "request": "attach", "name": "Attach", "pid": "${command:pickProcess}" }
  ]
}
```

`launch` also takes `cwd`, `env` (a value of `null` removes a variable), `stopOnEntry` and `console` (`integratedTerminal`, the default, `externalTerminal` or `internalConsole`). With no `launch.json`, F5 runs the current file.

## Building

```sh
cd editors/vscode
npm ci
npm run compile     # out/extension.js
npm test            # unit tests and grammar tests; set ORBIT=/path/to/orbit to also test the attach list
npm run package     # orbitscript-<version>.vsix
code --install-extension orbitscript-*.vsix
```

Build `orbit-lsp` and `orbit` from the repository root, either with `./build.sh` (both land in `target/package/out/bin`) or with `cargo build --release -p orbit-lsp -p orbit-cli`, and put them on `PATH`, or point the settings at them.
