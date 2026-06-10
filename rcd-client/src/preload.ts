// rcd-client — preload script.
//
// Intentionally near-empty for M1. The renderer implements the entire answerer
// using only standard browser APIs that are available even with
// contextIsolation:true + sandbox:true:
//   - WebSocket            (signaling transport)
//   - RTCPeerConnection    (WebRTC)
//   - DOM / requestAnimationFrame
//
// So there is no need to expose any Node/Electron capability across the bridge
// yet. We keep the contextBridge import + a placeholder API as the seam where
// future native features will be surfaced safely.
//
// TODO(M2+): if the client ever needs OS-level features (e.g. global hotkeys,
// persisting config to disk, clipboard sync), expose narrow, validated methods
// here via contextBridge.exposeInMainWorld("rcd", { ... }) — never the raw
// ipcRenderer or Node modules.

import { contextBridge, ipcRenderer } from "electron";

contextBridge.exposeInMainWorld("rcd", {
  // Placeholder so `window.rcd` exists; carries the milestone for diagnostics.
  version: "m1",
  // Clipboard bridge: the renderer can't reliably use navigator.clipboard from a
  // file:// page, so route through the main process's Electron `clipboard` module.
  clipboard: {
    readText: (): Promise<string> => ipcRenderer.invoke("clipboard:read"),
    writeText: (text: string): Promise<void> => ipcRenderer.invoke("clipboard:write", text),
  },
});
