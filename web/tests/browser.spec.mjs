import { test, expect } from "@playwright/test";
import { readFile } from "node:fs/promises";

test("public firmware verifies and settings stay in browser-created USB blobs", async ({ page }) => {
  const requests = [];
  page.on("request", request => requests.push({ url: request.url(), body: request.postData() }));
  await page.addInitScript(() => { Object.defineProperty(navigator, "serial", { value: { getPorts: async () => [] } }); });
  await page.goto("/");
  await expect(page.getByRole("button", { name: "Prepare installation" })).toBeEnabled();
  await page.getByLabel("Wi-Fi name").fill("browser-test-network");
  await page.getByLabel("Wi-Fi password", { exact: true }).fill("private-wifi-password");
  await page.getByLabel("Dashboard password").fill("private-router-password");
  await page.getByRole("button", { name: "Prepare installation" }).click();
  await expect(page.getByRole("button", { name: "Install on display" })).toBeVisible();
  const manifest = await page.evaluate(async () => {
    const manifest = await (await fetch(document.getElementById("installer").manifest)).json();
    const sector = new Uint8Array(await (await fetch(manifest.builds[0].parts[1].path)).arrayBuffer());
    const length = new DataView(sector.buffer).getUint32(8, true);
    return { manifest, settings: JSON.parse(new TextDecoder().decode(sector.subarray(16, 16 + length))) };
  });
  expect(manifest.manifest.builds[0].chipFamily).toBe("ESP32-C3");
  expect(manifest.manifest.builds[0].parts[1].offset).toBe(0x3f0000);
  expect(manifest.settings.wifi_password).toBe("private-wifi-password");
  expect(manifest.settings.ubus_password).toBe("private-router-password");
  for (const request of requests.filter(r => !r.url.startsWith("blob:"))) {
    expect(JSON.stringify(request)).not.toContain("private-wifi-password");
    expect(JSON.stringify(request)).not.toContain("private-router-password");
    expect(new URL(request.url).origin).toBe("http://127.0.0.1:4173");
  }
  await page.getByLabel("Wi-Fi name").fill("changed network");
  await expect(page.getByRole("button", { name: "Install on display" })).toBeHidden();
});

test("corrupt firmware cannot be prepared or flashed", async ({ page }) => {
  await page.route("**/router-display.bin", route => route.fulfill({ body: Buffer.alloc(32), contentType: "application/octet-stream" }));
  await page.goto("/");
  await expect(page.getByRole("alert")).toContainText("download size changed");
  await expect(page.getByRole("button", { name: "Prepare installation" })).toBeDisabled();
});

test("a same-size firmware change fails SHA-256 verification", async ({ page }) => {
  const binary = await readFile(new URL("../../dist/flasher/router-display.bin", import.meta.url));
  binary[binary.length - 1] ^= 1;
  await page.route("**/router-display.bin", route => route.fulfill({ body: binary, contentType: "application/octet-stream" }));
  await page.goto("/");
  await expect(page.getByRole("alert")).toContainText("checksum did not match");
  await expect(page.getByRole("button", { name: "Prepare installation" })).toBeDisabled();
});

test("mobile layout remains usable and unsupported browsers can download", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.addInitScript(() => { Object.defineProperty(navigator, "serial", { value: undefined }); });
  await page.goto("/");
  await expect(page.getByRole("link", { name: "Download binary" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Prepare installation" })).toBeDisabled();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
});
