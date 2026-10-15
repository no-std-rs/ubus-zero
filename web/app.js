import { encodeConfig, manifestWithSettings } from "./config.js";

const $ = id => document.getElementById(id);
const form = $("settings");
const status = $("status");
const install = $("installer");
const objectUrls = [];
let release;
let firmwareUrl;

function message(text, error = false) {
  status.textContent = text;
  status.classList.toggle("error", error);
  status.setAttribute("role", error ? "alert" : "status");
}

function forgetSettings() {
  install.manifest = "";
  install.removeAttribute("manifest");
  $("install-area").hidden = true;
  for (const url of objectUrls.splice(0)) URL.revokeObjectURL(url);
}

form.addEventListener("input", () => {
  forgetSettings();
  if (release) message("Your settings stay here. Prepare when you're ready.");
});
form.addEventListener("submit", event => {
  event.preventDefault();
  forgetSettings();
  try {
    if (!release || !firmwareUrl) throw new Error("Wait for the latest firmware to finish loading.");
    const config = Object.fromEntries(new FormData(form));
    const sector = encodeConfig(config);
    const settingsUrl = URL.createObjectURL(new Blob([sector], { type: "application/octet-stream" }));
    objectUrls.push(settingsUrl);
    const manifest = manifestWithSettings(release, firmwareUrl, settingsUrl);
    const manifestUrl = URL.createObjectURL(new Blob([JSON.stringify(manifest)], { type: "application/json" }));
    objectUrls.push(manifestUrl);
    install.manifest = manifestUrl;
    $("install-area").hidden = false;
    message("Ready. Connect your display by USB, then choose Install on display.");
    $("install-area").scrollIntoView({ behavior: "smooth", block: "nearest" });
  } catch (error) { message(error.message, true); }
});

window.addEventListener("pagehide", () => {
  forgetSettings();
  if (firmwareUrl) URL.revokeObjectURL(firmwareUrl);
  firmwareUrl = undefined;
  release = undefined;
  $("prepare").disabled = true;
  form.reset();
});
window.addEventListener("pageshow", event => {
  if (event.persisted) loadRelease().catch(error => message(error.message, true));
});

async function loadRelease() {
  const response = await fetch("./latest.json", { cache: "no-store", credentials: "omit" });
  if (!response.ok) throw new Error("The latest main build is not available yet. Try again shortly.");
  const candidate = await response.json();
  manifestWithSettings(candidate, "blob:firmware", "blob:settings");
  const binaryResponse = await fetch("./router-display.bin", { cache: "no-store", credentials: "omit" });
  if (!binaryResponse.ok) throw new Error("The firmware download failed. Please reload to retry.");
  const buffer = await binaryResponse.arrayBuffer();
  if (buffer.byteLength !== candidate.size) throw new Error("The download size changed. Reload for the newest build.");
  const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", buffer))].map(x => x.toString(16).padStart(2, "0")).join("");
  if (digest !== candidate.sha256) throw new Error("The download checksum did not match. Reload to retry.");
  firmwareUrl = URL.createObjectURL(new Blob([buffer], { type: "application/octet-stream" }));
  release = candidate;
  $("revision").textContent = `main · ${release.revision.slice(0, 12)}`;
  $("revision").href = `https://github.com/no-std-rs/ubus-zero/commit/${release.revision}`;
  $("digest").textContent = release.sha256;
  $("download").hidden = false;
  $("prepare").disabled = false;
  if (!navigator.serial || !window.isSecureContext) {
    $("prepare").disabled = true;
    message("USB flashing needs a desktop browser with Web Serial, such as Chrome or Edge. You can still download the binary.");
  } else { message("Latest firmware verified. Add your Wi-Fi and router settings below."); }
}

loadRelease().catch(error => message(error.message, true));
