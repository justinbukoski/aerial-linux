# Apple CDN trust anchor

`apple-root-ca.cer` is the public Apple Root CA downloaded over verified HTTPS from [Apple](https://www.apple.com/appleca/AppleIncRootCertificate.cer), linked by [Apple's certificate authority page](https://www.apple.com/certificateauthority/).

SHA-256: `b0b1730ecbc7ff4505142c49f1295e6eda6bcaed7e2c68c5be91b5a11001f024`.

The aerial CDN uses this root, which is absent from the tested Fedora/webpki root stores. The certificate is added only to the client restricted to `https://sylvan.apple.com:443`. Hostname and validity verification remain enabled. System trust is unchanged. This is a public certificate, not a private key.
