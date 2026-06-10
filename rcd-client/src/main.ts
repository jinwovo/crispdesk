// rcd-client — Electron MAIN process.
//
// Responsibilities (M1): create the window with safe defaults and load the
// root index.html. All WebRTC/signaling logic lives in the renderer, which uses
// only browser APIs (WebSocket, RTCPeerConnection, DOM), so the main process
// stays intentionally thin.

import { app, BrowserWindow, Menu, ipcMain, clipboard } from "electron";
import * as path from "node:path";

function createWindow(): void {
  const win = new BrowserWindow({
    width: 1280,
    height: 800,
    backgroundColor: "#000000",
    title: "rcd — client",
    webPreferences: {
      // Security defaults: renderer is isolated and cannot touch Node directly.
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      // Preload is compiled to dist/preload.js (sibling of this file's output).
      preload: path.join(__dirname, "preload.js"),
    },
  });

  // index.html lives at the project root; compiled main.js lives in dist/,
  // so go up one level to reach it.
  void win.loadFile(path.join(__dirname, "..", "index.html"));

  // TODO(M1-dev): uncomment to debug the renderer / inspect WebRTC internals.
  // win.webContents.openDevTools({ mode: "detach" });
}

// Standard Electron lifecycle.
app.whenReady().then(() => {
  // Kill the default application menu so its accelerators (Ctrl+W close,
  // Ctrl+R reload, F11, ...) don't swallow keystrokes we forward to the host.
  Menu.setApplicationMenu(null);

  // Clipboard bridge for the renderer (see preload.ts).
  ipcMain.handle("clipboard:read", () => clipboard.readText());
  ipcMain.handle("clipboard:write", (_ev, text: unknown) => {
    clipboard.writeText(typeof text === "string" ? text : "");
  });

  createWindow();

  app.on("activate", () => {
    // macOS: re-create a window when the dock icon is clicked and none are open.
    if (BrowserWindow.getAllWindows().length === 0) {
      createWindow();
    }
  });
});

app.on("window-all-closed", () => {
  // Quit on all platforms except macOS, where apps typically stay alive.
  if (process.platform !== "darwin") {
    app.quit();
  }
});
