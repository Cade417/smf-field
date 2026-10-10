# Running and deploying ShowMeFire Field

ShowMeFire Field ships in two forms built from the same code:

- the **desktop app** (`smf-field.exe`), which has every feature;
- the **web app**, the same program compiled to WebAssembly. It runs in a browser and is hosted as
  static files on Cloudflare Pages.

Both talk to the Show Me Fire API (`https://api.showmefire.org`) for fire data and team maps.

---

## 1. Desktop app

### One-time setup

1. Install Rust with [rustup](https://rustup.rs). The repo pins the stable toolchain and installs
   rustfmt and clippy on its own.
2. On Windows, install the **Visual Studio Build Tools** with the "Desktop development with C++"
   workload. Rust needs its linker. No other C libraries are needed: SQLite and TLS are compiled in.

### Run it

From the repo root (`M:\_Development\ShowMeFire\smf-field`):

```powershell
cargo run --release -p smf-app
```

The first build takes a few minutes, and later builds are incremental. The program is written to
`target\release\smf-field.exe`. You can run that file directly, pin it to the taskbar, or copy it
anywhere. It has no other files it needs.

For a faster build with slower rendering, use the debug build:

```powershell
cargo run -p smf-app
```

### Useful settings

| Environment variable | Effect |
|---|---|
| `SMF_API_BASE=http://localhost:8000` | Use a local API instead of production for fire data and team maps. |
| `W3W_API_KEY=...` | what3words key, for when you don't want to enter it in the app (the what3words menu). |
| `SMF_UPDATE_URL=...` | Turn on update checks. They are off by default. |

Settings, saved sign-ins and keys are stored in `%APPDATA%\ShowMeFire\Field\config`. The tile cache
lives in `%LOCALAPPDATA%\ShowMeFire\Field\cache`. Delete the cache folder to free disk space. Do not
delete offline areas you downloaded on purpose.

### Installers (not finished)

`crates/smf-app/Cargo.toml` contains settings for cargo-packager, which can build NSIS, dmg and
AppImage installers, and `.github/workflows/release.yml` builds them. Neither has been tested on a
clean machine, and the installers are not code-signed, so Windows SmartScreen will warn users. Until
that's done, hand out the release `smf-field.exe` directly.

### Checks before a commit

```powershell
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

---

## 2. Web app

### One-time setup

```powershell
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
```

Version 0.21.14 of [Trunk](https://trunkrs.dev) has been tested. Trunk downloads the matching
`wasm-bindgen` and `wasm-opt` itself.

### Run it locally

From `crates\smf-app`:

```powershell
trunk serve --release
```

Then open <http://127.0.0.1:8080>. Trunk rebuilds and reloads the page when you save a file. Using
`--release` matters: a debug WebAssembly build works, but it is slow to download and to render.

### Build the files to deploy

From `crates\smf-app`:

```powershell
$env:CARGO_PROFILE_RELEASE_OPT_LEVEL = 's'; trunk build --release
```

Optimizing for size (`s`) makes a much smaller download. Without it the WebAssembly file is far
larger, and maps render just as fast either way.

The output goes to the repo's `dist\` folder:

| File | What it is |
|---|---|
| `index.html` | The page. |
| `smf-field-<hash>.js` and `smf-field-<hash>_bg.wasm` | The app, about 8 MB. The hash in the name changes on every build, so browsers never run a stale copy. |
| `_headers` | Cloudflare Pages response headers (security headers, and permission to use location). The source is `crates\smf-app\web\_headers`. |

To try the built files without Trunk:

```powershell
python -m http.server 8090 --directory dist
```

Then open <http://127.0.0.1:8090>.

### What a browser needs

- A current Chrome, Edge, Firefox or Safari. The app uses WebGPU when the browser has it and falls
  back to WebGL2 when it doesn't.
- The page must be served over HTTPS, or from `localhost`, for "GPS > This device" to work.

### What the web app does not have yet

These features are desktop-only today:

- offline area downloads
- Google Earth (KMZ SuperOverlay) export
- shapefile import and export
- local MBTiles, PMTiles and GeoTIFF layers
- serial, USB and gpsd GPS
- APRS and inReach live tracking

Browsers cannot open serial ports or raw network sockets, which rules out the GPS and APRS sources.
Printing to PDF and GeoTIFF works and downloads the file. Fire data, drawing, import and export of
GPX, KML and GeoJSON, 3D terrain, what3words, trail snapping, and team maps also work in the browser.

The web app always reads fire data from production (`api.showmefire.org`). For team maps, you can
point at another server in the Team dialog's server field.

---

## 3. Deploying the web app to Cloudflare Pages

The web app is plain static files, so it goes on **Cloudflare Pages** as a "direct upload" project.
Cloudflare's build machines don't have to compile Rust: you build locally (or in CI) and upload
`dist\`.

### 3.1 Create the Pages project (once)

You need Node.js for `npx`. Run these from anywhere:

```powershell
npx wrangler login
npx wrangler pages project create smf-field --production-branch master
```

`wrangler login` opens a browser. Sign in with the Cloudflare account that owns `showmefire.org`.

### 3.2 Build and upload (every release)

From `crates\smf-app`:

```powershell
$env:CARGO_PROFILE_RELEASE_OPT_LEVEL = 's'; trunk build --release
npx wrangler pages deploy ..\..\dist --project-name smf-field --branch master
```

`--branch master` makes this the production deployment. If you use any other branch name, the
upload becomes a preview with its own `https://<hash>.smf-field.pages.dev` address, which is useful
for testing before release. Pages allows files up to 25 MiB, and the ~8 MB `.wasm` file fits easily.

### 3.3 Give it a real address (once)

In the Cloudflare dashboard:

1. Open **Workers & Pages > smf-field > Custom domains > Set up a custom domain**.
2. Enter `field.showmefire.org`, or whichever name you choose.

Because `showmefire.org` is already on Cloudflare, the DNS record and certificate are created for you.

### 3.4 Prepare the API (once, and needed before the web app fully works)

The browser app calls `api.showmefire.org` from another origin, so the API must allow it. The
required API changes are merged to the API repo's local `main` but **not pushed or deployed yet**:

- `routers/field.py`: team maps and accounts.
- `core/public_cors.py`: `Access-Control-Allow-Origin: *` on the public `/tiles/` responses.

Steps:

1. **Deploy the API's `main`** the way you normally deploy the API.
2. **Set the allowed web origins** in the API's environment, then restart it:
   ```
   CORS_EXTRA_ORIGINS=https://field.showmefire.org,https://smf-field.pages.dev
   ```
   This allows team maps from those addresses. Preview addresses (`<hash>.smf-field.pages.dev`) are
   not covered, so test team features on the production address.
3. **Optional:** set `FIELD_TEAM_CREATE_KEY` to an operator key, so you can create teams without a
   paid account. Without it, only accounts on a paid plan can create teams. Plans are granted with
   the admin endpoints described in the API repo's `docs/field_collab.md`.
4. **Purge Cloudflare's cache for tiles once.** In the dashboard, go to **showmefire.org > Caching >
   Configuration > Custom purge**, choose "prefix", and enter `api.showmefire.org/tiles/`.
   Cloudflare may still hold tile responses that were cached before the CORS header existed, and
   browsers will reject those. If you skip this step, the fire danger layer stays blank in the web
   app until those cached copies expire.
5. **Don't cache or shorten the team endpoints.** Team sync holds requests to
   `/api/field/v1/.../events` open for about 25 seconds. That is well under Cloudflare's 100-second
   limit, but make sure no Cache Rule or Worker touches `/api/field/`.

### 3.5 Check the deployment

1. Open `https://field.showmefire.org`. The map should load, and with **Show Me Fire > Fire danger**
   switched on, the colored overlay should appear over Missouri.
2. Open the browser console (F12). There should be no CORS errors.
3. Check the API headers from a terminal:
   ```powershell
   curl.exe -sI -H "Origin: https://field.showmefire.org" "https://api.showmefire.org/api/conditions/layers"
   ```
   The response should include `access-control-allow-origin: https://field.showmefire.org`.
4. Sign in to a team from two browsers, draw something in one, and confirm it appears in the other
   within a couple of seconds.

### 3.6 Rolling back

In the dashboard, open **Workers & Pages > smf-field > Deployments**. Pick an earlier deployment and
choose **Rollback to this deployment**. Rollback takes effect immediately and needs no rebuild.

### 3.7 Optional: deploy from CI

The repo has no git remote yet. Once it's on GitHub, a workflow can do the same two commands:

1. Install Rust, the `wasm32-unknown-unknown` target and Trunk.
2. Run `trunk build --release`.
3. Run `npx wrangler pages deploy dist --project-name smf-field --branch master`.

The workflow needs a `CLOUDFLARE_API_TOKEN` secret (a token with the "Cloudflare Pages: Edit"
permission) and a `CLOUDFLARE_ACCOUNT_ID` secret.

---

## 4. Licence reminder

ShowMeFire Field is free to use and modify, but it may not be redistributed or sold (see `LICENSE`
and `NOTICE.md`). Hosting it on Show Me Fire's own domain is fine. Publishing the built files or the
installers anywhere else counts as redistribution.
