# Metal Performance HUD for Windows (iOS HUD Helper)

A small Windows app, written in Rust, that turns on Apple's **Metal Performance HUD**
(the FPS / GPU overlay) on an iPhone or iPad, without a Mac.

It does three things, one button each:

1. **Turn on Developer Mode.** Makes the Developer Mode switch appear in
   Settings → Privacy & Security.
2. **Unlock the Developer menu.** Installs Apple's "Developer Disk Image" on the device,
   the same thing Xcode does on a Mac. After that, **Settings → Developer → Graphics HUD**
   shows up and you can turn on "Show Graphics HUD".
3. **Launch a game with the HUD.** Starts a game with `MTL_HUD_ENABLED=1`, which asks
   Apple's Metal graphics framework to draw the HUD. Some games only show the HUD this way.

## Before you start

- **Install iTunes or the "Apple Devices" app** from Apple. They include the USB drivers
  and the "Apple Mobile Device Service" this app talks to.
- Use a USB cable, unlock the device, and tap **Trust** if it asks.
- You need an internet connection the first time you unlock the Developer menu.
  The app downloads about 20 MB, and on iOS 17+ Apple's servers sign the image for your device.

## Using it

1. Run `ios-hud-helper.exe`. Windows SmartScreen may warn you because the app isn't signed.
   Click **More info → Run anyway**.
2. Your device shows up at the top. Follow steps 1 → 2 → 3.
3. The **Developer menu disappears when the device restarts.** Click step 2 again to bring it back.

## Limitations

- Apple says the HUD is a developer tool and there is "no supported way" to force it in
  App Store apps. Many games still show it, but some don't.
- **iOS 17.0–17.3**: launching games (step 3) needs a kind of network tunnel this app
  doesn't build. Update to iOS 17.4 or newer. Steps 1 and 2 still work.
- Developer Mode slightly lowers your device's security. You can turn it off again in
  Settings → Privacy & Security → Developer Mode.

## Building it yourself

1. Install Rust from <https://rustup.rs>.
2. In this folder, run:

   ```bash
   cargo build --release
   ```

3. The app is at `target\release\ios-hud-helper.exe`.

To build the Windows app from Linux:

```bash
rustup target add x86_64-pc-windows-gnu
```

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

(That needs the `mingw-w64` package installed.)

Run the tests. The second command also downloads the real image files, about 25 MB:

```bash
cargo test
```

```bash
cargo test -- --ignored
```

## How the code is organized

| File | What it does |
| --- | --- |
| `src/main.rs` | The window: buttons, device picker, game list, activity log. Uses [egui](https://github.com/emilk/egui). |
| `src/device.rs` | Everything that talks to the iPhone/iPad, using the [idevice](https://github.com/jkcoxson/idevice) library. |
| `src/ddi.rs` | Downloads and caches the Developer Disk Image files. |
| `src/worker.rs` | Runs slow work in the background so the window never freezes. |

Downloaded images are cached in `%LOCALAPPDATA%\iOS HUD Helper`.

## Credits

- [idevice](https://github.com/jkcoxson/idevice) by Jackson Coxson (MIT) does the device communication.
- Developer Disk Images come from the [DeveloperDiskImage](https://github.com/doronz88/DeveloperDiskImage)
  mirror used by [pymobiledevice3](https://github.com/doronz88/pymobiledevice3).
