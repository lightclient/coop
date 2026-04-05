# BUG-008: `coop signal link` fails to provision the secondary device with HTTP 409

**Status:** Fixed
**Found:** 2026-04-05
**Scenario:** Signal e2e setup for subagent verification (`cargo run --features signal --bin coop -- signal link` + `signal-cli addDevice --uri ...`)

## Symptom

Linking coop as a secondary Signal device did not complete successfully.

Observed behavior before the fix:
- `coop signal link` printed a QR code and provisioning URL as expected
- the local primary-device command `signal-cli -a <bob-number> addDevice --uri '<provisioning-url>'` returned without a visible error
- but the coop-side link flow failed and left `db/signal.db` unusable for startup

A subsequent startup with that freshly written DB reported that the client was still not registered.

## Trace / Command Evidence

Coop-side link output before the fix:

```text
Provisioning URL: sgnl://linkdevice?... 
WARN websocket closed code=1000 reason="Closed"
ERROR failed to decode HTTP 409 status error=error decoding response body
Error: failed to complete signal linking

Caused by:
    0: failed to provision device: Service error: Unexpected response: HTTP 409
    1: Service error: Unexpected response: HTTP 409
    2: Unexpected response: HTTP 409
```

Then startup with the resulting DB:

```text
ERROR failed to initialize signal channel
  error="failed to load registered signal account: this client is not yet registered, please register or link as a secondary device"
```

After the fix, the same workflow completed with:

```text
INFO successfully registered device ...
INFO signal linking completed ...
linked signal device using ./db/signal.db
```

## Root Cause

`vendor/libsignal-service-rs` was sending an outdated linked-device registration payload.

The Rust `LinkRequest` used a legacy `LinkAccountAttributes` shape with capability fields like `deleteSync` / `ssre2`, while current Signal server behavior expects an `AccountAttributes`-style payload for `/v1/devices/link` with fields matching the modern Java implementation used by `signal-cli` — including fields such as `voice`, `video`, `unidentifiedAccessKey`, and capability names like `storage`, `versionedExpirationTimer`, `attachmentBackfill`, and `spqr`.

The provisioning code also generated linked-device registration IDs from the tiny `1..256` range instead of the normal registration-id range.

## Fix

Fixed by patching the vendored `libsignal-service-rs` used by coop:

- `vendor/libsignal-service-rs/src/push_service/linking.rs`
  - updated linked-device account-attribute serialization to match the current Signal payload shape
  - renamed capability fields to the current server-accepted names
- `vendor/libsignal-service-rs/src/provisioning/mod.rs`
  - sent the corrected account attributes during `link_device`
  - switched linked-device registration IDs to the normal `generate_registration_id` range
- `Cargo.toml`
  - patched `libsignal-service` to the vendored local copy so the fix lives in the repo

## Test Coverage

Verification performed with live Signal setup:

- `cargo build --features signal`
- `cargo run --features signal --bin coop -- signal link --device-name coop-agent`
- `signal-cli -a <primary-number> addDevice --uri '<provisioning-url>'`
- confirmed successful output:
  - `successfully registered device ...`
  - `signal linking completed ...`
  - `linked signal device using ./db/signal.db`
