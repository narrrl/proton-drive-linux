# SDK 0.11.0 review

Reviewed the sibling `../proton-sdk-rs` checkout at `84039a1` against 0.10.1
(`48dd410`), including its changelog and source diff.

## Features

- Folder creation, streaming uploads, draft replacement, and revision uploads
  preserve the supplied timestamp's UTC offset and fractional seconds.
- `ModificationTime` is exported as an alias for
  `chrono::DateTime<chrono::FixedOffset>`.
- Photo modification time can be set independently of capture time. Downloaded
  claimed modification timestamps remain verbatim strings.
- Neither SDK crate defines Cargo feature flags. No feature opt-in is needed.
  The Drive crate now depends directly on `chrono` 0.4; the SDK already used it.
  The minimum Rust version remains 1.96, matching this workspace.

## Breaking changes and adaptation

The modification-time argument changes from `Option<i64>` to
`Option<ModificationTime>` on `create_folder`, `upload_file_from`,
`upload_file_replacing_draft_from`, `upload_new_revision`, and
`upload_new_revision_from`. `None` remains valid.

The real `DriveApi` adapter converts the client's stored epoch seconds to UTC
through `pdfs_core::timestamp::modification_time`. Internal storage, the trait,
and the simulated Drive continue using epoch seconds. Out-of-range timestamps
are omitted rather than panicking. Existing upload paths still supply whole
seconds; this bump alone does not add subsecond storage or offset tracking.

`PhotoUploadMetadata` gains `last_modification_time`, and the SDK no longer
copies `capture_time` into it. Our literals already use `Default`, so this would
compile while silently changing uploaded metadata. Control uploads, Takeout
imports (including the EXIF fallback), and redating now set both explicitly to
preserve the previous behavior when a capture time is known. Without a known
capture time, modification time stays omitted as before.

No other public API changes were found in the reviewed diff. No database or
recovery migration is required by this SDK bump.

## Dependencies

The workspace requires the published 0.11.0 versions of both crates from
crates.io. No local patches or sibling checkout are needed to build it.

## Verification

The published crates' source files match the reviewed sibling checkout at
`84039a1`. On the final branch based on `main`, these checks passed:

```sh
cargo test --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

The test suite reported 1,135 passed and 67 ignored; ignored tests were not run.
Tests ran outside the sandbox because control-socket tests require socket
permissions. No live account or mounted-filesystem acceptance testing was
performed.
