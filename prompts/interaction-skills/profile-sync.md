# Profiles And Cookies

Use explicit profile commands for login-sensitive browser work. Never dump raw cookie values by default.

What is available:

- `browser local profiles --json`: list local profiles with built-in Rust filesystem discovery. No external CLI is required.
- `browser local profiles inspect <profile-id-or-name> --domains-only`: copy the selected profile to a temporary profile, inspect cookies through CDP, and show domain-level cookie summaries only.
- `browser profile sync [--profile <profile-id-or-name>] [--all-cookies|--domain <domain>...] [--exclude-domain <domain>...] [--cloud-profile-id <uuid>|--cloud-profile-name <name>|--new-cloud-profile-name <name>]`: import local cookies into a Browser Use Cloud profile.
- `browser remote profiles --json`: list Browser Use cloud profiles with ID/name/domain summary/last-used metadata.
- `browser remote start --profile-id <uuid>`: start and connect a cloud browser using an existing cloud profile.
- `browser remote start --profile-name <name>`: resolve one exact cloud profile name and start/connect it.
- `browser remote stop`: stop the Rust-owned cloud browser so billing ends and cloud profile cookie changes can persist.

Chat-driven flow:

1. If the user wants to use an existing cloud login, show cloud profiles with `browser remote profiles --json`, then run `browser remote start --profile-id <uuid>` or `browser remote start --profile-name <name>`.
2. If the user wants a real local logged-in browser, use `browser connect local`; do not sync or copy profiles unless the user asks for cloud cookie sync.
3. If the user asks to sync local cookies to cloud, run `browser local profiles --json` when a local profile is not specified, ask which profile to sync, then run `browser profile sync --profile <profile-id-or-name> --all-cookies` or domain-filtered sync.
4. If no cloud profile is specified during sync, the tool creates one named after the local browser profile. Use the returned `cloud_profile.id` with `browser remote start --profile-id <uuid>`.

Important limits:

- Cookie sync requires a configured Browser Use Cloud key. If missing, open `/auth` for Browser Use Cloud key setup, then rerun the sync command.
- Local real profiles are used by attaching to an already-open browser with `browser connect local`.
- Do not launch the user's real default Chrome profile with remote-debugging flags.
- Cookie sync uses a temporary local profile copy and a temporary Browser Use Cloud browser. It does not attach to or relaunch the user's real browser.
- Raw cookie values are never returned by default. Profile inspection and sync output should expose only domain/count/expiry summaries.
