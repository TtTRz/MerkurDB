# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in MerkurDB, please **do not** open a public issue.

Instead, report it privately via GitHub's Security Advisory system:

1. Go to the [Security](https://github.com/TtTRz/MerkurDB/security) tab
2. Click "Report a vulnerability"
3. Describe the issue in detail

We aim to respond within 48 hours and publish fixes promptly.

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.4.x   | Yes       |

## Security Considerations for Deployers

- MerkurDB stores embeddings and memory content on disk. Secure the data directory appropriately.
- All endpoints except `/v1/health` and `/v1/metrics` require `Authorization: Bearer <token>` (`auth.tokens` in config). The server refuses to start with an empty token list unless `auth.disabled = true` **and** `server.dev_mode = true` — never enable `dev_mode` in production.
- The default configuration binds to `127.0.0.1` (localhost only). If exposing externally, keep the built-in bearer auth on and put TLS in front of it.
- Logical namespaces (`X-Merkur-Namespace`) are an isolation convenience, **not** a security boundary — any authenticated caller may claim any bucket.
- API keys for hosted providers (OpenAI embedder, LLM consolidator backends) are loaded from config files or environment variables. Do not commit config files containing secrets; prefer the `MERKUR_*` env vars.
- LanceDB feature requires `protoc` (protobuf compiler). This is a build-time dependency only and not present in the runtime image.
