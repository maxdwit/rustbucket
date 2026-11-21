# Trusting the certificate

The standalone container makes its own self-signed certificate. Nextcloud clients (and browsers) only talk to a
server whose certificate they trust, so each device has to be told to trust this one, **once**.
`make cert` copies it to `rustbucket-cert.pem` and prints its SHA-256 fingerprint (the container also logs the
fingerprint at every start: compare it before you trust anything).

The certificate must contain the **exact name or address the device uses** (`make run TLS_NAMES=...`, see
[install.md](install.md)); otherwise the device rejects it even after you trusted it.

| Device | How |
|---|---|
| Linux | copy the file to `/usr/local/share/ca-certificates/rustbucket.crt`, run `sudo update-ca-certificates` (Arch: `sudo trust anchor --store rustbucket-cert.pem`). Firefox has its own store (Settings > Privacy > Certificates > Import). Chromium uses the system store. |
| Windows | double-click the file > Install Certificate > Local Machine > "Trusted Root Certification Authorities". Edge/Chrome use the same store; Firefox has its own. |
| Android | Settings > Security > Encryption & credentials > Install a certificate > **CA certificate**, pick the file. If an app ignores user-installed certificates on your Android version, use a certificate from a public CA instead (put it into the `/certs` volume). |
| iOS / iPadOS | open the file (Safari/Files), Settings > "Profile downloaded" > Install, then **Settings > General > About > Certificate Trust Settings** > switch it to full trust. |

Tips
- The desktop client also offers to accept an untrusted certificate for one account at the first connection.
- A certificate from a public CA (e.g. via your own reverse proxy, see [install.md](install.md)) needs no device setup at all.
- The Nextcloud clients cache the certificate decision per account; after fixing trust, remove and re-add a failing account.
- Test from the device with the browser first: if the web login page opens without a warning, the clients will connect too.
