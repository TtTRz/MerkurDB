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
| 0.1.x   | Yes       |

## Security Considerations for Deployers

- MerkurDB stores embeddings and memory content on disk. Secure the data directory appropriately.
- The default configuration binds to `127.0.0.1` (localhost only). If exposing externally, use a reverse proxy with authentication.
- API keys for OpenAI/Ollama are loaded from config files or environment variables. Do not commit config files containing secrets.
- LanceDB feature requires `protoc` (protobuf compiler). This is a build-time dependency only and not present in the runtime image.
