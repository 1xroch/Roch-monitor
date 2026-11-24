# Vendored crates

`roch-core`, `roch-nvidia`, `roch-amd`, `roch-intel` and `roch-amdgpu` are copied unchanged from
Roch GPU, https://github.com/1xroch/Roch-GPU, at commit `724d14216888051a4987946aedb052e027180725`
("Bump Roch GPU header and package version to 1.1.5"), the commit Roch GPU 1.1.5 was released from.
No edits are carried on top of them: the earlier `roch-amd` PCIe changes and the older
`GpuDevice` fields `roch-amdgpu` was adapted to are all in Roch GPU at that commit.

They are copied rather than referenced so this repository builds on its own. Roch Monitor calls only
their read paths: `GpuBackend::initialize`, `devices`, `capabilities`, `read_telemetry` and
`measure_extra_clocks`. None of the setters is reachable from `roch-sensors`.

To update them, copy the corresponding directories from a newer Roch GPU checkout (`rsync -a
--delete`, then touch the files so cargo rebuilds them, since rsync keeps their older times), then
run `cargo test`, `cargo check --target x86_64-pc-windows-msvc` and `RochMonitor --dump` on the
operating system with the card.
