# Local proxy TLS fixtures

These public test assets are only for the local native-host proxy fixture. `server-key.der` is an intentionally published test key, not a credential. Never use it outside tests.

- `ca.der` is the fixture CA certificate.
- `server.der` is the server certificate for `native-proxy.test`.
- `server-key.der` is its PKCS#8 private key.

The server certificate is valid from October 6, 2026 to October 3, 2036. The test client trusts only this fixture CA and verifies the certificate and DNS name. A request for a different server name must fail verification. Renew the fixture assets before expiry; do not disable certificate or name checks to extend their lifetime.

The fixture terminates TLS and relays HTTP to the managed native host. It does not add incoming TLS support to the Axum adapter.
