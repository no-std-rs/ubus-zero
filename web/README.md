# Router display USB installer

The static site at <https://no-std-rs.github.io/ubus-zero/> installs the
`no_std` ESP32-C3 dashboard. It uses a bundled, pinned ESP Web Tools 10.4.0 and
Web Serial. No credentials, analytics, application server, or third-party scripts
are needed at runtime.

The public factory image has a bootloader, partition table and application, in
DIO/80 MHz mode for the 4 MB GUITION ESP32-2424S012. It contains no Wi-Fi or router
credentials. Settings are encoded in a 4 KiB sector at `0x3f0000` and written over
USB as a second image. The firmware validates the header, bounded JSON payload,
CRC-32, and fields before starting the radio. Missing settings show the setup
screen. Settings remain unencrypted in the device's local flash.

`latest.json` identifies the source commit, size and SHA-256 of the merged binary.
The page downloads and verifies it before enabling preparation. Form values stay
in memory and Blob URLs, are cleared on navigation, and are never uploaded or
stored by the site. The USB flasher's native dialog checks the ESP32-C3 chip; the
physical board must still match the listed GUITION model and wiring.

`.github/workflows/firmware.yml` builds and tests on PRs, and publishes on `main`.
The exact same files go to Runnerless and Pages. Runnerless's default-branch-only
publisher is declared in `.ci-toolkit.yml`; it unpacks the named Actions artifact
and keeps one complete main set until a successful replacement. No crates.io
release or package-publishing credential is involved. Pages exposes a public
download mirror for browser fetches, including clients on other origins; deployment
verification must confirm `Access-Control-Allow-Origin: *` on the actual responses.
The Pages job performs an anonymous GET with an unrelated Origin header and
requires wildcard CORS, the expected main commit, byte length and SHA-256.

Runnerless URLs are commit-bound:

```text
https://ci-toolkit.pawelchcki.workers.dev/artifacts/no-std-rs/ubus-zero/<full-main-sha>/router-display.bin
```

`https://no-std-rs.github.io/ubus-zero/latest.json` is the current public main
pointer and includes its corresponding Runnerless URL. The installer reads the
identical Pages copy to avoid depending on redirect CORS policy at another host.

From the repository root, to build and preview:

```sh
sh tools/build-dashboard-artifact.sh
npm --prefix web ci
npm --prefix web run build
python3 tools/assemble-flasher.py dist/firmware dist/flasher
node web/tests/server.mjs
```

Visit <http://127.0.0.1:4173/>; localhost is a secure context for Web
Serial. The artifact metadata marks local uncommitted builds as dirty; CI refuses
to publish one.

Checks:

```sh
npm --prefix web test
cd web
npx playwright install chromium
npm run test:browser
```

The browser tests verify the actual settings Blob and manifest, ensure passwords
never enter network requests, reject a corrupt download, and check mobile layout.
They cannot exercise a physical USB display without attached hardware.

References: [ESP Web Tools](https://esphome.github.io/esp-web-tools/),
[Web Serial](https://developer.chrome.com/docs/capabilities/serial).
