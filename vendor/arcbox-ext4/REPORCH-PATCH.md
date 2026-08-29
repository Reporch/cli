# Reporch reproducibility patch

This directory vendors `arcbox-ext4` 0.1.2 under its original MIT OR
Apache-2.0 terms. Reporch changes only `timestamp_now`: when
`SOURCE_DATE_EPOCH` is present, inode creation/change timestamps use that
validated epoch instead of the host wall clock.

The upstream crate otherwise makes two identical OCI-to-ext4 builds differ by
time-dependent inode bytes and their checksums. The toolchain builder requires
`SOURCE_DATE_EPOCH`, so official images fail closed if reproducible time is not
configured. Remove this patch only after an upstream release exposes an
equivalent deterministic timestamp contract and a double-build fixture passes.
