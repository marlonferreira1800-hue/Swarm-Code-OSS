# Swarm IDE — Enxame ao Vivo (VS Code / Code-OSS Extension)

Orquestrador visual multiagente em **Tela Única** conectado ao motor Rust `SwarmEngine` (`swarm-server-rust`).

## Recursos

- **Tela Única — Enxame ao Vivo**: cards em tempo real de todos os agentes, logs ao vivo, aprovações inline, grafo de comunicação (`@xyflow/react`) e mural de `@menções`.
- **Sidebar Swarm Enxame**: status do motor Rust, agentes ativos/aguardando e envio rápido de tarefas.
- **Decorações ao vivo no editor**: arquivos sendo editados por agentes exibem destaque verde e marcador `✏️ @agentId` diretamente nos editores abertos.
- **Zero dependências de runtime**: empacotada com `esbuild` (`dist/extension.js`) e assets estáticos de webview (`media/webview/webview.{js,css}`).

## Comandos

- `Swarm IDE: Abrir Tela Única do Enxame` (`swarmIde.openLiveScreen`)
- `Swarm IDE: Enviar Tarefa ao Enxame` (`swarmIde.sendPrompt`)
- `Swarm IDE: Iniciar Motor Rust` (`swarmIde.startServer`)
- `Swarm IDE: Parar Motor Rust` (`swarmIde.stopServer`)

## Build

```bash
npm run build:webview -w client
npm run build --prefix vscode-extension
```
