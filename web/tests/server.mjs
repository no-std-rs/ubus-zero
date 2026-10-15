import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { resolve, extname } from "node:path";
import { fileURLToPath } from "node:url";
const root = resolve(process.env.FLASHER_SITE || fileURLToPath(new URL("../../dist/flasher/", import.meta.url)));
const types = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".json": "application/json", ".bin": "application/octet-stream" };
createServer(async (request, response) => {
  const name = new URL(request.url, "http://localhost").pathname;
  const file = resolve(root, `.${name === "/" ? "/index.html" : name}`);
  if (!file.startsWith(root + "/")) { response.writeHead(403).end(); return; }
  try { response.writeHead(200, { "Content-Type": types[extname(file)] || "text/plain", "Access-Control-Allow-Origin": "*", "Cache-Control": "no-store" }); response.end(await readFile(file)); }
  catch { response.writeHead(404).end(); }
}).listen(4173, "127.0.0.1");
