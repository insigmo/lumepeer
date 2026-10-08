# Lumepeer

Lumepeer is a simple remote desktop app for connecting to another computer and using it as if you were there.

It lets you:

* View and control a remote screen in real time
* Share files and clipboard content
* Use audio and chat during a session
* Work with multiple monitors and fullscreen mode
* Adjust quality automatically to keep the connection responsive
* Connect to trusted devices without someone being at the remote computer

The person on the remote computer always controls what the other side can access, and permissions can be changed or revoked during a session.

Lumepeer works on **Windows, Linux, and macOS**.

## Install

Download the latest version from the [Releases](https://github.com/insigmo/lumepeer/releases/latest) page.

### Linux

```sh
curl -fsSL https://raw.githubusercontent.com/insigmo/lumepeer/refs/heads/master/install.sh | bash
```

### Windows

```powershell
irm https://raw.githubusercontent.com/insigmo/lumepeer/refs/heads/master/install.ps1 | iex
```

## Build from source

```sh
cd apps/desktop
npm install
npm run build
cd ../..

cargo build --workspace
cargo test --workspace
```

On Windows, a release build also needs `ninja` on PATH (`winget install
Ninja-build.Ninja`); without it libopus would be compiled unoptimised
([ADR 0149](docs/adr/0149-libopus-is-built-with-ninja-on-windows.md)).

## License

See the repository for license information.
