# Third-party notices

RepoTunnel uses open-source dependencies listed in `src-tauri/Cargo.toml` and `package.json`.

## ngrok Rust SDK

RepoTunnel's managed ngrok public MCP provider uses the `ngrok` Rust SDK published by ngrok. The upstream project is licensed under the Apache License 2.0 or MIT License, at the user's option. RepoTunnel does not bundle a developer ngrok account, authtoken, or domain; each user connects their own ngrok account.

## scrcpy server

RepoTunnel's Phone live-view path bundles the official `scrcpy-server` component from Genymobile scrcpy 4.1. It is used only as the Android-side video server; users do not need to install the scrcpy desktop application separately.

Upstream project: https://github.com/Genymobile/scrcpy

Copyright (C) 2018 Genymobile

Copyright (C) 2018-2026 Romain Vimont

scrcpy is licensed under the Apache License, Version 2.0.
