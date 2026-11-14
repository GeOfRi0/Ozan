# Ozan Soundboard — Ultra-Lightweight Windows Soundboard & Direct Microphone APO Injector

**Ozan Soundboard** (`ozan.exe` + `ozan-apo.dll`) is a high-performance Windows desktop soundboard built with **Rust** and **Tauri v2** that injects real-time audio directly into your active physical microphone without requiring third-party virtual audio drivers (such as VB-Cable).

When minimized to the System Tray, the application is engineered to consume **< 25 MB RAM (~3 MB Private)** and **< 1% CPU**.

---

## Core Technical Features

### 1. Windows User-Mode APO (`ozan-apo.dll`) & APO Chaining
- **Driverless Direct Microphone Injection**: Written in pure Rust, `ozan-apo.dll` (`CLSID: {8A4E7B21-6C3D-4F92-B8E1-5D2A9F0C4E73}`) operates inside the Windows Audio Engine (`audiodg.exe`) as a User-Mode Audio Processing Object implementing `IAudioProcessingObject`, `IAudioProcessingObjectRT`, `IAudioProcessingObjectConfiguration`, `IAudioSystemEffects`, and `IAudioSystemEffects2`.
- **Non-Destructive APO Chaining**: Existing manufacturer audio effects under the active microphone's Windows Registry (`FxProperties`) key (noise suppression, beamforming, AGC, EQ, etc.) are **preserved**. Original CLSID entries are backed up, `ozan-apo.dll` is prepended to the effect chain, and processed audio buffers are forwarded downstream to the original APO chain.

### 2. Lock-Free Shared Memory (IPC) & Real-Time Audio Processing
- **Named Shared Memory & Lock-Free SPSC Ring Buffer**: Audio frames are transferred between `ozan.exe` and `ozan-apo.dll` inside `audiodg.exe` via Named Shared Memory (`Global\OzanAudioSharedMem_v1` with `Local\OzanAudioSharedMem_v1` fallback) backed by a 64-byte cache-line aligned (`#[repr(C, align(64))]`) lock-free Single-Producer Single-Consumer (SPSC) ring buffer.
- **Zero-Allocation / Lock-Free Real-Time Thread (`APOProcess`)**: The real-time `APOProcess` callback performs zero heap allocations (`Box`, `Vec`, `String`), zero mutex locks, and zero disk or registry I/O.
- **Real-Time Resampling & Channel Matching via `rubato`**: Dynamically adapts to varying hardware sample rates (`44.1 kHz`, `48 kHz`, `96 kHz`, **Bluetooth `16 kHz` Mono**, etc.) using `rubato`'s allocation-free `process_into_buffer` interface alongside automatic Stereo/Mono channel routing.
- **Hardware Hotplug Detection (`IMMNotificationClient`)**: USB and Bluetooth microphone connections, disconnections, and default communication device changes are detected instantaneously via the Windows Core Audio `IMMNotificationClient` interface.

### 3. Disk-Streaming Decoder & Multi-Format Support
- **`.opus`, `.mp3`, `.wav`, `.ogg`, and `.flac` Support**: Audio files added via drag-and-drop are copied to `%APPDATA%\Ozan\sounds\` and optionally peak-normalized to `-1 dBFS`.
- **Zero Bulk RAM Preloading**: Audio assets are never loaded entirely into memory; a dedicated `ozan-decoder` worker thread streams and decodes small chunks directly from disk during playback, allowing arbitrary track durations.
- **5-Channel Polyphony & FIFO Hard-Cut**: Up to 5 audio tracks can play concurrently without ducking. Triggering a 6th track immediately terminates the oldest active stream (`FIFO hard-cut`). Each sound card supports per-item **Restart** or **Overlap** trigger modes.

### 4. Self-Healing & Fallback Mode
- **SHA-256 & `FxProperties` Integrity Verification**: On every startup, the application verifies the SHA-256 checksum of `ozan-apo.dll`, its COM registration, and the Windows Registry `FxProperties` APO chain.
- **One-Click Automated Repair**: If a Windows feature update or external utility resets audio endpoint registry keys, the interface provides one-click restoration.
- **Crash-Free Fallback Mode**: If endpoint security software (EDR/Antivirus) or system policy blocks DLL injection into `audiodg.exe`, the application never hangs or crashes; it automatically transitions into **Fallback (Direct Output) Mode** and notifies the user in the UI.

### 5. Global Hotkey Capture, Audio Trimming, Push-to-Talk & Backup
- **Unrestricted Mouse / MMO & Keyboard Hotkeys**: Captures standard keyboard keys, `XBUTTON1`/`XBUTTON2`, and extended multi-button MMO mouse inputs via `WH_KEYBOARD_LL`, `WH_MOUSE_LL`, and Windows Raw Input / HID (`WM_INPUT`). Includes per-hotkey **Passthrough** toggles and a global **`End` (`VK_END`) Panic** key.
- **Non-Destructive & Permanent Audio Trimming**: Interactive waveform range selection supporting both **Play Once (Preview Trim)** and **Save Trimmed Copy**.
- **Push-to-Talk Automation & Mic Auto-Mute**: Automatically holds a configured Push-to-Talk key via `SendInput` during soundboard playback, with an optional toggle to temporarily mute physical microphone voice input while clips play.
- **Profile Backup & Restore (`.zip`)**: Exports and imports all audio files, folder mappings, and hotkey configurations as a single portable `.zip` archive.

---

## Building from Source

### Prerequisites
- **Operating System**: Windows 10 / 11 (x64)
- **Rust**: `1.77+` (`x86_64-pc-windows-msvc`)
- **C++ Build Tools**: MSVC Build Tools + Windows 10/11 SDK
- **Node.js**: `20+`

### Release Build Steps

1. Install frontend dependencies and build the UI bundle:
   ```powershell
   npm install
   npm run build
   ```

2. Build the Rust workspace (`ozan.exe` and `ozan-apo.dll`) in release mode:
   ```powershell
   cd src-tauri
   cargo build --release --workspace
   Copy-Item -Force "target/release/ozan_apo.dll" "target/release/ozan-apo.dll"
   ```

3. Package the application using the Tauri CLI:
   ```powershell
   npx tauri build
   ```

4. Build Artifacts:
   - Main Executable: `src-tauri/target/release/ozan.exe`
   - User-Mode APO Library: `src-tauri/target/release/ozan-apo.dll`

---

## Code Signing Policy

Free code signing is provided by [SignPath.io](https://about.signpath.io/), using a certificate by the [SignPath Foundation](https://signpath.org/).

Releases tagged with `v*` are built reproducibly via GitHub Actions ([.github/workflows/release.yml](.github/workflows/release.yml)) according to [signpath.yml](signpath.yml), generating verifiable SHA-256 checksums for `ozan.exe` and `ozan-apo.dll`.

## License

This project is licensed under the [MIT License](LICENSE).