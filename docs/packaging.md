# Packaging & code signing

midi-editor ships as a classic Win32 desktop app. This document covers the
installer choice, file-association design, and the signing policy.

## Installer strategy

`installer/midi-editor.iss` — **Inno Setup 6**, built by
`scripts/package.ps1` into `dist/midi-editor-<version>-setup.exe`.

Why Inno Setup over the alternatives:

| option | verdict |
|---|---|
| **MSIX** | Rejected for now. MSIX requires a trusted signature to install at all — there is no unsigned dev story — and needs identity packaging (`Package.appxmanifest`, sparse packaging or full identity for shell verbs). Worth revisiting if we later want Microsoft Store distribution; the per-user install + capability registration below already covers what MSIX would buy us. |
| **WiX MSI** | Viable, but heavier to author/review (XML + ICE rules) with no functional gain for a per-user app. MSI still wins for per-machine/enterprise rollout — keep it in mind for a future `msi` flavor. |
| **Inno Setup** | Chosen: small declarative script, per-user install with no UAC prompt, first-class file-association + task checkboxes, reliable uninstall tracking, unsigned builds work locally, and CI-friendly (`ISCC` CLI). |

The install is **per-user** (`PrivilegesRequired=lowest`):

- files land in `%LOCALAPPDATA%\Programs\midi-editor\` — `midi-editor.exe`,
  `vst3-host-helper.exe`, `vst3-host-probe.exe` (loaded only as sidecars via
  `output::sidecar_binary`, so the AGENTS.md invariant holds),
  `mcp-bridge.exe`, and the license files;
- a Start Menu group is created (desktop shortcut is an unchecked task);
- all registry writes are under `HKCU` — nothing touches `HKLM`, so
  multi-user machines and the machine-wide default association are safe.

## File associations

Two separate things, per Microsoft's guidance:

1. **Application Capabilities** (always registered):
   `HKCU\Software\REO2248\midi-editor\Capabilities` + a
   `RegisteredApplications` entry make the app show up in
   *Open with…* and *Settings > Apps > Default apps*. This never steals a
   default — it only makes midi-editor selectable.
2. **Default association** (explicit opt-in): the `assocmid` task checkbox —
   unchecked by default — maps `.mid`, `.midi`, `.smf` to the
   `midi-editor.mid` ProgId under `HKCU\Software\Classes`. The user makes an
   explicit choice during install; nothing else in the installer or app
   claims the association silently.

The `open` command is `"<app>\midi-editor.exe" "%1"` — `"%1"` quoting keeps
paths containing spaces or shell metacharacters a single literal argv entry,
and the app itself parses argv with `std::env::args_os` (never panics on
non-UTF-8 filenames) while ignoring leading `-` flags.

## Clean uninstall

- `CloseApplications=yes` + a filter covering `midi-editor.exe`,
  `vst3-host-helper.exe`, `vst3-host-probe.exe`, `mcp-bridge.exe` asks the
  Restart Manager to close live processes; a `[Code]` step additionally runs
  `taskkill` (polite close, then `/F`) before install and uninstall because
  a silent install proceeds even when an app ignores the RM request — this
  is what guarantees no locked exe or live helper is left behind.
- Registry keys Inno writes are removed via `uninsdeletevalue` /
  `uninsdeletekey` flags (Inno does not remove them on its own), and the
  `[Code]` step drops `FileExts\<ext>\UserChoice` entries that still name
  our ProgId — a UserChoice pointing at a different app is kept.
- Inno removes every file it installed plus the install dir, and we also
  delete `%APPDATA%\midi-editor` (global prefs: recent files, count-in,
  MIDI input choice — app-owned state).
- **Not** removed: per-song `<name>.mid.editor.json` sidecars — they live
  next to the user's MIDI files and are user data.
- Association removal: everything under HKCU is uninstalled; if the user had
  opted in, `.mid/.midi/.smf` revert to *no default* rather than to a stale
  ProgId (Windows can't restore a *previous* app's association — that state
  isn't recorded).

## Building

```powershell
powershell -File scripts\package.ps1
```

- builds `cargo build --release --locked --bin midi-editor --bin mcp-bridge`
- `cargo install vst3-host --version =0.9.0 --locked` produces the helper
  and probe binaries (they are `[[bin]]` targets of the vst3-host crate,
  version-pinned to match `Cargo.lock`)
- stages everything under `dist\stage`, then invokes `ISCC`
- `-SkipInstaller` stages only; `-SkipHelpers` skips the helper build

Every `cargo` call goes through `vcvars64.bat` (MSYS2's `link.exe` must not
shadow MSVC's — the same reason `vcargo.cmd` exists).

## Code signing & release keys

Signing is **opt-in** so dev/CI builds stay friction-free:

- `MIDI_EDITOR_SIGN_PFX` — path to a code-signing `.pfx`. When set,
  `package.ps1` signs each staged `.exe` and the produced setup with
  `signtool sign /fd sha256 /td sha256 /tr http://timestamp.digicert.com`.
- `MIDI_EDITOR_SIGN_PFX_PASSWORD`, `MIDI_EDITOR_SIGN_TIMESTAMP` — optional.

Key-handling policy:

1. **Never commit** a certificate, `.pfx`, or password to the repo.
2. Real releases use an EV/OV certificate stored in CI secrets (GitHub
   Actions: base64 `.pfx` + password secrets; decode to a temp file at sign
   time, delete after). Prefer a hardware-token/cloud HSM cert for OV+.
3. For local smoke-testing a signed installer, generate a **self-signed**
   cert (`New-SelfSignedCertificate -Type CodeSigningCert …`), install it
   into `Cert:\CurrentUser\TrustedPublisher` on the test machine only, and
   never use it for shipped artifacts — it provides no trust.
4. Timestamping is mandatory so signatures outlive certificate expiry.
5. Signing happens **before** `ISCC` compiles the staged binaries and again
   **on** the setup `.exe` itself — an unsigned payload inside a signed
   wrapper still trips SmartScreen.

Unsigned builds install and run fine; SmartScreen shows the standard
unknown-publisher prompt. A trusted signature is a distribution decision,
not a build requirement.
