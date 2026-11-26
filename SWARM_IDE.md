# Swarm IDE (Code-OSS)

Swarm IDE is a Code-OSS fork (`marlonferreira1800-hue/Swarm-Code-OSS`) bundled with the built-in **Swarm IDE** multi-agent orchestration extension (`extensions/swarm-ide/`), the native Rust backend **SwarmEngine** (`swarm-server-rust/`), and **OpenVSX** marketplace support.

## Architecture

1. **Code-OSS Workbench (`product.json`)**
   - Branded as **Swarm IDE** (`nameShort: "Swarm IDE"`, `nameLong: "Swarm IDE (Code-OSS)"`, `applicationName: "swarm-ide"`, `dataFolderName: ".swarm-ide"`, `urlProtocol: "swarm-ide"`).
   - Configured with OpenVSX (`extensionsGallery`) so third-party MIT/open-source extensions and `.vsix` packages install out of the box without Microsoft proprietary marketplace locks.

2. **Built-in Extension (`extensions/swarm-ide/`)**
   - Packaged directly inside Code-OSS as a zero-runtime-dependency built-in extension (`dist/extension.js` bundled via `esbuild`, zero `dependencies` / `optionalDependencies` in `package.json`).
   - Provides:
     - **Activity Bar & Sidebar (`swarm-ide.sidebar`)**: live Rust engine health status, active/waiting agent list, quick prompt submission, and start/stop controls.
     - **Live Swarm Screen (`swarmIde.openLiveScreen`)**: prebuilt React/Vite Webview (`media/webview/webview.js` and `media/webview/webview.css`) showing real-time agent cards, logs, inline approval controls, communication graph, and `@mention` message board.
     - **Live Editor Decorations**: highlights workspace files currently being edited by agents with green line decorations and `✏️ @<agentId>` markers.
     - **Workspace File Opening**: clicking a file inside the Live Swarm Screen opens it directly in a Code-OSS editor tab via `vscode.workspace.openTextDocument`.

3. **Embedded Rust Backend (`swarm-server-rust/`)**
   - High-performance Axum + Tokio backend (`swarm-server-rust`) exposing:
     - `GET /api/health` — engine status and active `projectRoot`
     - `GET /api/files` — safe workspace file listing
     - `GET /api/file?path=...` — safe read access confined to `PROJECT_ROOT`
     - `POST /api/task` — task submission to `@coordinator`
     - `GET /ws` — real-time WebSocket stream (`agents`, `board:history`, `board:post`, `edit:indicator`, `edit:clear`, `approval:request`, `agent:log`, `status`)
   - On extension startup (when `swarmIde.autoStartServer` is enabled), `extensions/swarm-ide/dist/extension.js` probes `http://127.0.0.1:4000/api/health` (or configured `swarmIde.port`) and, if not already running, spawns `extensions/swarm-ide/bin/swarm-server-rust.exe` (falling back to `swarmIde.serverBinaryPath`, sibling `server-rust/target/{release,debug}`, or `cargo run` when developing from source) with `PORT` and `PROJECT_ROOT` set to the active workspace folder.

## OpenVSX & `.vsix` Marketplace Support

`product.json` sets `extensionsGallery` to OpenVSX:

```json
"extensionsGallery": {
  "serviceUrl": "https://open-vsx.org/vscode/gallery",
  "itemUrl": "https://open-vsx.org/vscode/item",
  "resourceUrlTemplate": "https://openvsxorg.blob.core.windows.net/resources/{publisher}/{name}/{version}/{path}",
  "controlUrl": "",
  "recommendationsUrl": ""
}
```

You can install extensions directly from the Extensions sidebar (powered by OpenVSX) or install local `.vsix` files via **Extensions: Install from VSIX...**.

## Build & Run

### 1. Test and Build the Rust Backend

```powershell
cargo test --manifest-path swarm-server-rust/Cargo.toml
cargo build --release --manifest-path swarm-server-rust/Cargo.toml
New-Item -ItemType Directory -Force -Path extensions/swarm-ide/bin | Out-Null
Copy-Item swarm-server-rust/target/release/swarm-server-rust.exe extensions/swarm-ide/bin/swarm-server-rust.exe -Force
```

### 2. Full Windows Code-OSS Build (CI / Release)

Automated via [`.github/workflows/build-swarm-code-oss.yml`](.github/workflows/build-swarm-code-oss.yml) on `windows-latest`:

1. Runs `cargo test` and `cargo build --release` for `swarm-server-rust`, placing `swarm-server-rust.exe` at `extensions/swarm-ide/bin/swarm-server-rust.exe`.
2. Installs Code-OSS dependencies with `pnpm install --frozen-lockfile` (`VSCODE_SKIP_NODE_VERSION_CHECK=1`, `ELECTRON_SKIP_BINARY_DOWNLOAD=0`) and prepares Electron typings (`node build/npm/electronTypes.ts`).
3. Builds Code-OSS and packages local/built-in extensions (including `extensions/swarm-ide`) via `pnpm run gulp core-ci`, followed by `pnpm run gulp compile-copilot-extension-build`.
4. Packages the Windows x64 application via `pnpm run gulp vscode-win32-x64-min-ci` (with Windows 10 SDK `x64` tools on `PATH`) into `..\VSCode-win32-x64`, zipped as `Swarm-IDE-CodeOSS-win32-x64.zip`.

### 3. Sparse Local Development Notes

This repository can be cloned lightly with partial clone + sparse-checkout:

```bash
git clone --depth 1 --filter=blob:none --sparse https://github.com/marlonferreira1800-hue/Swarm-Code-OSS.git
cd Swarm-Code-OSS
git sparse-checkout add extensions/swarm-ide swarm-server-rust .github/workflows
```
