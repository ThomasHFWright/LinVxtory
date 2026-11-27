# Indexed texture queue experiment

The producer can skip host-written texture objects that the upload
consumer already discards. Placed textures and title-filled objects
retain the same path. The setting is experimental. Common build and
launch steps keep the existing queue policy.

Build the current SR tree `1ec8cdd4791d95b60ce2457d71d7b96c70b3b700`
or current RR tree `917283c4a1d53d71966163b856ed4b335137a6c1` in its
own external directory. Set `kit` to the absolute LinVxtory checkout
before entering the renderer directory, then apply and verify the overlay.

```sh
git apply --index "$kit/docs/7900/nvidia-texture-queue.patch.txt"
git write-tree
```

The SR experiment produces `31df5ecd44d00195c0ebb65f84ed46d149aae11c`.
The RR experiment produces `512ce829a2df04c19fd685deab0febe7850572f1`.
Verify the matching tree before rebuilding with the existing locked
diag and DLSS settings. Use separate runtime directories for each
comparison and the same executable.

Set `VICTORY_QUEUE_HOST_TEXTURES=1` for the existing policy, or `0` for
the experiment. Test a streaming transition and a settled cell replay.
Track the texture upload pass time, oldest pending title write and visible
texture readiness. Filtering can move the due count below 1,024, where the
pass switches back to a smaller catch-up budget. Queue length alone does
not establish a speedup. A scoped test for producer slot replacement is
included. Verify that it successfully creates a host heap when running it.
[Upstream change](https://github.com/trustno1-coder/VIctory-NVIDIA/commit/5dda41ddff71e3f6052b843c115d2e09e89df41d)
