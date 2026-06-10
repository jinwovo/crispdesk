// Post-build marker: write {"type":"module"} into dist/renderer/.
//
// tsconfig.renderer.json emits NATIVE ES modules (the browser loads
// dist/renderer/renderer.js as <script type="module">). The root package.json
// intentionally stays CommonJS so Electron can load the CommonJS-emitted
// dist/main.js + dist/preload.js. Without a marker, Node treats dist/renderer/*.js
// as CommonJS and prints MODULE_TYPELESS_PACKAGE_JSON warnings (and reparses) when
// the unit tests import dist/renderer/wire.js. Dropping a scoped package.json that
// flags ONLY the renderer output as ESM resolves that cleanly without affecting
// the CommonJS main/preload. This file is a build artifact, regenerated each build.

import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const rendererDir = join(here, "..", "dist", "renderer");

mkdirSync(rendererDir, { recursive: true });
writeFileSync(join(rendererDir, "package.json"), JSON.stringify({ type: "module" }) + "\n");
