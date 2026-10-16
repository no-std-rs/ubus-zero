import { test } from "node:test";
import assert from "node:assert/strict";
import { crc32, encodeConfig, manifestWithSettings, validateConfig, CONFIG_OFFSET } from "../config.js";

export const settings = () => ({ wifi_ssid: "test network", wifi_password: "test-password", ubus_url: "http://192.168.8.1:8080/ubus", ubus_username: "dashboard", ubus_password: "test-router-password", wan_interface: "wan", wifi_device: "" });

test("settings sector has the firmware magic, size, checksum, and UTF-8 JSON", () => {
  const config = settings(); config.wifi_ssid = "Zażółć";
  const sector = encodeConfig(config);
  const view = new DataView(sector.buffer);
  const size = view.getUint32(8, true);
  assert.equal(sector.length, 4096);
  assert.equal(new TextDecoder().decode(sector.subarray(0, 8)), "UBUSCFG1");
  assert.equal(view.getUint32(12, true), crc32(sector.subarray(16, 16 + size)));
  assert.deepEqual(JSON.parse(new TextDecoder().decode(sector.subarray(16, 16 + size))), config);
  assert.ok(sector.subarray(16 + size).every(b => b === 255));
  assert.equal(crc32(new TextEncoder().encode("123456789")), 0xcbf43926);
});

test("rejects invalid credentials, routes, Unicode byte limits, and unknown fields", () => {
  for (const patch of [{ wifi_ssid: "界".repeat(11) }, { wifi_password: "short" }, { ubus_url: "https://router/ubus" },
    { ubus_url: "http://user:secret@router/ubus" }, { ubus_url: "http://router/ubus\r\n" },
    { ubus_password: "" }, { wan_interface: "wan/../../etc" }, { other: "unknown" }]) {
    assert.throws(() => validateConfig({ ...settings(), ...patch }));
  }
  assert.doesNotThrow(() => validateConfig({ ...settings(), wifi_password: "" }));
});

test("install manifest writes settings only to the dedicated partition", () => {
  const release = { chip: "ESP32-C3", flash_size: 4194304, config_offset: CONFIG_OFFSET, revision: "a".repeat(40), sha256: "b".repeat(64), size: 1000000 };
  const manifest = manifestWithSettings(release, "blob:firmware", "blob:settings");
  assert.deepEqual(manifest.builds[0].parts, [{ path: "blob:firmware", offset: 0 }, { path: "blob:settings", offset: 0x3f0000 }]);
  assert.equal(manifest.new_install_improv_wait_time, 0);
  assert.throws(() => manifestWithSettings({ ...release, config_offset: 0x10000 }, "f", "s"));
  assert.throws(() => manifestWithSettings({ ...release, chip: "ESP32" }, "f", "s"));
});
