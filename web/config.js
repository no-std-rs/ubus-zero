// Matches examples/esp32c3-router-display/src/config.rs and partitions.csv.
export const CONFIG_OFFSET = 0x3f0000;
export const SECTOR_SIZE = 4096;
const encoder = new TextEncoder();
const fields = ["wifi_ssid", "wifi_password", "ubus_url", "ubus_username", "ubus_password", "wan_interface", "wifi_device"];
const length = value => encoder.encode(value).length;

export function validateConfig(config) {
  if (Object.keys(config).length !== fields.length || fields.some(k => typeof config[k] !== "string" || config[k].includes("\0"))) throw new Error("Complete the Wi-Fi and router settings.");
  if (length(config.wifi_ssid) < 1 || length(config.wifi_ssid) > 32) throw new Error("The Wi-Fi name must be 1–32 UTF-8 bytes.");
  const password = config.wifi_password;
  if (password !== "" && !(length(password) >= 8 && length(password) <= 63) && !/^[\da-fA-F]{64}$/.test(password)) throw new Error("Use an 8–63 byte Wi-Fi password, or leave it empty for an open network.");
  let url;
  try { url = new URL(config.ubus_url); } catch { throw new Error("Enter a valid router URL."); }
  if (!config.ubus_url.startsWith("http://") || url.protocol !== "http:" || !url.hostname || url.username || url.password || url.hash || /[?#@]/.test(config.ubus_url.slice(7).split("/")[0]) || /\s|[\x00-\x1f\x7f]/.test(config.ubus_url) || length(config.ubus_url) > 256) throw new Error("Use the router's http:// LAN ubus URL, without a username or password in the URL.");
  if (length(config.ubus_username) < 1 || length(config.ubus_username) > 64 || length(config.ubus_password) < 1 || length(config.ubus_password) > 128) throw new Error("Enter a dashboard username (up to 64 bytes) and password (up to 128 bytes).");
  if (!/^[\w.-]{1,64}$/.test(config.wan_interface) || !/^[\w.-]{0,64}$/.test(config.wifi_device)) throw new Error("Use the router's logical WAN and optional Wi-Fi interface names.");
  return config;
}

export function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
  }
  return (crc ^ 0xffffffff) >>> 0;
}

export function encodeConfig(config) {
  validateConfig(config);
  const payload = encoder.encode(JSON.stringify(config));
  if (payload.length > 2048) throw new Error("The settings exceed the device's configuration limit.");
  const sector = new Uint8Array(SECTOR_SIZE).fill(0xff);
  sector.set(encoder.encode("UBUSCFG1"));
  const view = new DataView(sector.buffer);
  view.setUint32(8, payload.length, true);
  view.setUint32(12, crc32(payload), true);
  sector.set(payload, 16);
  return sector;
}

export function manifestWithSettings(release, firmwareUrl, settingsUrl) {
  if (release.chip !== "ESP32-C3" || release.config_offset !== CONFIG_OFFSET || release.flash_size !== 4194304
      || !/^[0-9a-f]{40}$/.test(release.revision) || !/^[0-9a-f]{64}$/.test(release.sha256)
      || !Number.isSafeInteger(release.size) || release.size < 65536 || release.size >= CONFIG_OFFSET) throw new Error("The firmware metadata does not match this board.");
  return {
    name: "ubus-zero router display", version: `main-${release.revision.slice(0, 12)}`,
    new_install_prompt_erase: true, new_install_improv_wait_time: 0,
    builds: [{ chipFamily: "ESP32-C3", parts: [{ path: firmwareUrl, offset: 0 }, { path: settingsUrl, offset: CONFIG_OFFSET }] }],
  };
}
