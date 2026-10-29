# Managed capability signing and publication

RepoTunnel's managed capabilities (`ast-grep`, `rea`, and `rea-node`) use a trust channel that is deliberately separate from the normal RepoTunnel application updater.

## Current safe state

The client has a dedicated public TUF bootstrap slot at:

`src-tauri/resources/managed-capabilities/tuf/root.json`

Until the production signing ceremony is complete, that file contains exactly `UNPROVISIONED`. In this state signed remote capability updates fail closed. The pinned fresh-install bootstrap for supported built-in capabilities remains independent of this remote update channel.

Do not replace the sentinel with an unsigned file, a root downloaded opportunistically at runtime, or metadata authenticated with RepoTunnel's application-updater key.

## One-time production trust ceremony

This is intentionally an operator action outside normal source builds. Private signing material must never be created in, copied into, or committed to the RepoTunnel source tree.

1. Create/configure the dedicated TUF-on-CI capability repository and its publication origin.
2. Use dedicated capability signing identities. Keep root signing offline, hardware-backed, or KMS-backed and threshold-capable. Keep targets signing separate from RepoTunnel's application-release key. Use narrowly scoped online signing for snapshot/timestamp.
3. Initialize the TUF-on-CI repository and complete its initial signing event. The result must be a publicly distributable signed `root.json`; private keys stay outside this repository.
4. Confirm that the public TUF repository URL exactly matches `CAPABILITY_TUF_REPOSITORY` in `src-tauri/src/managed_capabilities/tuf.rs`. If the production origin differs, update the pinned origin as a reviewed source change before provisioning the root.
5. Save the candidate signed public root outside the repository and run the exact client-side proof:

```bash
cd src-tauri
REPOTUNNEL_CAPABILITY_TUF_ROOT_CANDIDATE=/absolute/path/to/root.json \
  cargo test --locked \
  managed_capabilities::tuf::tests::candidate_bootstrap_root_is_self_signed_and_role_complete \
  -- --ignored --exact --nocapture
```

This verifies required root/targets/snapshot/timestamp role structure and uses the same TUF client to verify that the candidate root satisfies its own root-signature threshold.

6. Only after that proof passes, replace `src-tauri/resources/managed-capabilities/tuf/root.json` with the signed public root metadata.
7. Run the focused TUF tests and the full RepoTunnel regression suite before any release.

## Candidate production flow

`.github/workflows/managed-capabilities.yml` is the producer/validation side. It discovers candidates, validates all supported native target combinations, builds exact archives, performs adapter smoke tests, derives release identities from the exact bytes, builds a monotonic capability index, and emits a signing handoff.

The handoff is not trusted publication. Before future artifact upload, verify the exact bundle locally or in the signing workflow:

```bash
node scripts/managed-capabilities/verify-publication.mjs \
  --index managed-publication/capability-index.json \
  --bundle-dir managed-publication
```

The verifier rejects missing, tampered, wrong-size, wrong-hash, duplicate, unsafe, or unreferenced ZIP archives. The workflow runs this check before it creates the signing handoff.

After the handoff is accepted by the dedicated TUF signing process, only the exact verified archives referenced by the signed index may be published to the fixed RepoTunnel managed-capabilities release origin. Never rebuild an archive after its release identity has been signed.

## Root rotation

Future root-key changes use TUF root rotation. Do not replace the compiled bootstrap root merely to distribute ordinary capability updates. A RepoTunnel application release should change the bootstrap root only when intentionally changing the initial trust anchor for fresh installations or recovering from a separately reviewed trust event.

## Failure rule

If signing, metadata validation, artifact verification, compatibility smoke tests, or publication verification fails, stop publication. Keep the currently active known-good capability. Do not fall back to unsigned metadata, arbitrary URLs, system Node/npm, or an upstream `latest` executable.
