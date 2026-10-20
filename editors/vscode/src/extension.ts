// The OrbitScript extension: starts orbit-lsp for `.orb` files (and Lua's,
// when asked), and registers the `orbit` debug type, whose adapter is
// `orbit --debug-server` to launch and `orbit --debug-attach <pid>` to attach.

import * as vscode from "vscode";
import { LanguageClient, LanguageClientOptions, ServerOptions, TransportKind } from "vscode-languageclient/node";

import { adapterArgs, describe, listAttachable, serverOptions } from "./orbit";

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const output = vscode.window.createOutputChannel("OrbitScript", { log: true });
  context.subscriptions.push(output);

  context.subscriptions.push(
    vscode.commands.registerCommand("orbit.restartServer", async () => {
      await stopServer();
      await startServer(output);
    }),
    vscode.workspace.onDidChangeConfiguration(async (e) => {
      if (e.affectsConfiguration("orbit.server")) {
        await stopServer();
        await startServer(output);
      }
    }),
    vscode.commands.registerCommand("orbit.pickProcess", pickProcess),
    vscode.debug.registerDebugConfigurationProvider("orbit", new Configurations()),
    vscode.debug.registerDebugAdapterDescriptorFactory("orbit", new Adapters()),
  );
  await startServer(output);
}

export async function deactivate(): Promise<void> {
  await stopServer();
}

function settings(): vscode.WorkspaceConfiguration {
  return vscode.workspace.getConfiguration("orbit");
}

async function startServer(output: vscode.LogOutputChannel): Promise<void> {
  const config = settings();
  const command = config.get<string>("server.path", "orbit-lsp");
  const server: ServerOptions = { command, args: ["--stdio"], transport: TransportKind.stdio };
  const documentSelector = [{ scheme: "file", language: "orbitscript" }];
  if (config.get<boolean>("server.lua", false)) {
    documentSelector.push({ scheme: "file", language: "lua" });
  }
  const options: LanguageClientOptions = {
    documentSelector,
    outputChannel: output,
    initializationOptions: serverOptions(config.get<string[]>("server.searchPath", [])),
  };
  client = new LanguageClient("orbit", "OrbitScript", server, options);
  try {
    await client.start();
  } catch (e) {
    client = undefined;
    const choice = await vscode.window.showErrorMessage(
      `OrbitScript: couldn't start ${command}: ${(e as Error).message}`,
      "Open Settings",
    );
    if (choice) {
      await vscode.commands.executeCommand("workbench.action.openSettings", "orbit.server.path");
    }
  }
}

async function stopServer(): Promise<void> {
  const c = client;
  client = undefined;
  if (c) {
    await c.stop();
  }
}

/** Fill in a launch of the current file when there is no launch.json. */
class Configurations implements vscode.DebugConfigurationProvider {
  resolveDebugConfiguration(
    _folder: vscode.WorkspaceFolder | undefined,
    config: vscode.DebugConfiguration,
  ): vscode.DebugConfiguration | undefined {
    if (!config.type && !config.request && !config.name) {
      const editor = vscode.window.activeTextEditor;
      const language = editor?.document.languageId;
      if (language !== "orbitscript" && language !== "lua") {
        void vscode.window.showInformationMessage("Open an OrbitScript or Lua file to debug it.");
        return undefined;
      }
      return { type: "orbit", request: "launch", name: "Run the current file", program: "${file}" };
    }
    return config;
  }
}

/** Run the adapter for the session: `orbit --debug-server` or `--debug-attach <pid>`. */
class Adapters implements vscode.DebugAdapterDescriptorFactory {
  createDebugAdapterDescriptor(session: vscode.DebugSession): vscode.DebugAdapterDescriptor {
    const orbit = settings().get<string>("debug.orbitPath", "orbit");
    const c = session.configuration;
    return new vscode.DebugAdapterExecutable(orbit, adapterArgs(c.request, c.pid));
  }
}

/** `${command:pickProcess}`: choose among the programs `orbit --debug-attach --list` names. */
async function pickProcess(): Promise<string | undefined> {
  const orbit = settings().get<string>("debug.orbitPath", "orbit");
  let found;
  try {
    found = await listAttachable(orbit);
  } catch (e) {
    void vscode.window.showErrorMessage(`OrbitScript: ${(e as Error).message}`);
    return undefined;
  }
  if (found.length === 0) {
    void vscode.window.showInformationMessage(
      "No orbit program to attach to. Start one with `orbit --debug-listen` (any platform) or plain `orbit` (Linux).",
    );
    return undefined;
  }
  const picked = await vscode.window.showQuickPick(
    found.map((a) => ({
      label: describe(a),
      description: `pid ${a.pid}`,
      detail: a.listening ? undefined : "starts listening when attached",
      pid: a.pid,
    })),
    { placeHolder: "The orbit program to attach to" },
  );
  return picked ? String(picked.pid) : undefined;
}
