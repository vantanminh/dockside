# Security policy

## Reporting a vulnerability

Please **do not open a public issue** for a security problem. Use GitHub's private vulnerability
reporting (the "Report a vulnerability" button on the repository's Security tab). Include what you
found, how to reproduce it, and the version or commit.

This is an early (0.x) project run by a small group, so there is no formal response-time
guarantee.

## Scope notes

- The server authenticates with a single shared bearer token and has no per-user permissions. Treat
  that token like a root credential, run the server behind TLS, and do not expose it to untrusted
  networks.
- The verify gate and release steps run commands taken from the repository's own configuration
  and code, on the server. Anyone who can land a change can therefore run code on the server.
  Run it in a container with limited privileges and credentials.
