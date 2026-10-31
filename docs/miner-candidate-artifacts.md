# Miner candidate artifacts

The manual `miner-release.yml` path builds an exact `candidate_source` commit
without creating a GitHub release or tag. CPU and GPU crate versions must
match. Missing, abbreviated, branch-name, mismatched-commit and manual-tag
inputs fail before compilation. The release path separately requires a
`miner-v<version>` tag matching both crates.

The workflow definition is selected by `--ref`; the source built is selected
by `candidate_source`. Both identities are recorded. For the merged mining
candidate, source is `5f3054367646d4abc667d501e01669ff24979e14`, CPU/GPU 0.2.13.

```sh
gh workflow run miner-release.yml --repo DineroLabs/dinero-sv2 \
  --ref codex/miner-candidate-artifacts \
  -f candidate_source=5f3054367646d4abc667d501e01669ff24979e14
```

Each matrix artifact contains its binary, a JSON record with source/workflow
commits, version, target, run/attempt and binary hash/size, plus checksums for
both files. Hashing occurs after the Windows signing step. Existing signing
requirements remain. Mac signing/notarization remains separate.

Artifact compilation is not GPU execution or network qualification. The JSON
record explicitly starts with `runtime_qualified: false`; retain separate
runtime evidence when exercising downloaded artifacts. Do not turn build
success into an activation or hardware-support claim.

The candidate identity guard executes in PR CI using disposable Git histories,
including wrong-source/version/event cases. The nine target builds execute only
for manual dispatch or release tags. Actions artifacts follow the repository's
normal access policy; they are not published release assets.
