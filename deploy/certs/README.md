# Extra CA certificates

Put PEM certificates with the `.crt` extension here, for example the corporate root and issuing CAs.

* The Router image trusts them for outbound HTTPS (Microsoft JWKS and the Entra token endpoint).
* The nginx container trusts them, in addition to the system CAs, when it verifies backend certificates on the
  internal sync listener (port 8080, production).

Never put private keys here.
