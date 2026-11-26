# FidelityFX RGB9E5 output

Adapted from [marquezinii's VIctory PR1868](https://github.com/Marvisak/VIctory/pull/1868), head `7db1c078711da6a5085b018129cbdb7aa9259203` (MIT). An actual RGB9E5 host output without typed UAV stores is written through FP16, packed into RGB9E5, then copied into the output. Failed texture creation can retry without unsupported render-target/UAV uses; those copy-only textures cannot be bound or cleared as render targets.

This fork retains its existing RGB9E5-to-FP16 render-target stand-in and shader decoding. If the logical output already has an FP16 host texture, the original FidelityFX description and direct path are preserved. Other output formats never enter the RGB9E5 packing path. The integration regression checks both normal FP16 and logical RGB9E5 outputs using their actual host formats; a separate packer regression bypasses the console stand-in so it exercises real RGB9E5 packing. The official PC SDK remains external and user-supplied.

The packing shader uses this fork's existing system-compiler/DXC fallback. Wine can compile HLSL while dropping its embedded root signature; the compiler-only regression checks the actual packer container before any GPU work. The exhaustive GPU packing test remains required.
