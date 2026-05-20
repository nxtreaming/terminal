# Profiles And Cookies

Profile sync from local Chrome into Browser Use cloud is deferred for this terminal release. Do not call `sync_local_profile`; it is intentionally not part of the current tool surface.

What is available now:

- `browser local profiles --json`: list local profiles with built-in Rust filesystem discovery. No external CLI is required.
- `browser local profiles inspect <profile-id-or-name> --domains-only`: copy the selected profile to a temporary profile, inspect cookies through CDP, and show domain-level cookie summaries only. Never dump raw cookies by default.
- `browser remote profiles --json`: list Browser Use cloud profiles with ID/name/domain summary/last-used metadata.
- `browser remote start --profile-id <uuid>`: start and connect a cloud browser using an existing cloud profile.
- `browser remote start --profile-name <name>`: resolve one exact cloud profile name and start/connect it.
- `browser remote stop`: stop the Rust-owned cloud browser so billing ends and cloud profile cookie changes can persist.

Chat-driven flow:

1. Show cloud profiles with `browser remote profiles --json`.
2. If the user wants a real local logged-in browser, use `browser connect local`; do not sync or copy profiles.
3. If the user wants a cloud profile, ask which cloud profile to use, then run `browser remote start --profile-id <uuid>` or `browser remote start --profile-name <name>`.
4. If no cloud profile exists, start clean with `browser remote start`.

Important limits:

- Local real profiles are used by attaching to an already-open browser with `browser connect local`.
- Do not launch the user's real default Chrome profile with remote-debugging flags.
- Do not copy a real Chrome profile and assume cookies will work; Chrome profile locks and cookie encryption make that unreliable.
- Local-to-cloud sync is follow-up work for ENG-4739.
