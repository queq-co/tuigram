# Research: distributable & secure app registration

> **Phase 1 placeholder.** This is the central distribution/security question.

## The problem

Telegram requires an `api_id` + `api_hash` (obtained at
https://my.telegram.org) to use the API. Telegram's terms say these must not be
published — yet an **open-source, distributable** client has to ship *something*
so users can log in. This tension shapes the entire login design (Phase 2).

## Options to evaluate

- **User-supplied credentials**: each user registers their own `api_id`/`api_hash`
  at first run. Maximally ToS-safe, worst UX.
- **Bundled (obfuscated)**: ship the project's credentials in the binary. Best
  UX, but they are extractable and risk rate-limits/bans if abused.
- **Hybrid**: bundle a default, allow override via config/env for power users.
- Study how third-party clients handle this in practice:
  - Nekogram: https://github.com/Nekogram/Nekogram
  - Other TDLib-based clients and the official apps' approach.

## Security requirements (whichever path)

- Never commit `api_id`/`api_hash` or session data to git.
- Encrypt/obfuscate at rest where it adds real protection (note: obfuscation in a
  client binary is not true secrecy — document the threat model honestly).
- Protect the **user's** TDLib session/database (the sensitive asset) with
  appropriate file permissions and optional local encryption key.

## Links

- API registration: https://core.telegram.org/api/obtaining_api_id
- TDLib docs: https://core.telegram.org/tdlib
