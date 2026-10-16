# Wallet test fixtures (TEST ONLY)

Throwaway keys and certificates for the wallet unit tests in `src/wallet.rs`.
They are not secrets and are not trusted by Apple or Google:

- `test-wwdr.pem`: a self-signed CA standing in for Apple's WWDR intermediate.
- `test-cert.pem`: a "Pass Type ID" certificate issued by that CA
  (UID `pass.ph.pisowifi.test`, team `TEAMID1234`).
- `test-key.pem`: the RSA key for `test-cert.pem`, also used as the
  Google service-account key in `test-service-account.json`.

Never use these in a real hub config.
