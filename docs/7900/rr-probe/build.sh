#!/usr/bin/env bash
set -euo pipefail
if [[ $# -ne 3 ]]; then
    echo "Usage: $0 NVIDIA_DLSS_SDK XWIN_SDK OUTPUT_DIRECTORY" >&2
    exit 2
fi
sdk=$(realpath "$1")
xwin=$(realpath "$2")
source=$(realpath "$(dirname "$0")/rr-probe.cpp.txt")
msvc="$xwin/VC/Tools/MSVC/14.44.17.14"
headers="$xwin/Windows Kits/10/Include/10.0.26100"
libs="$xwin/Windows Kits/10/Lib/10.0.26100"
lib="$sdk/lib/Windows_x86_64/x64/nvsdk_ngx_d.lib"

mkdir -p "$3"
cd "$3"
export LIB="$msvc/lib/x64;$libs/ucrt/x64;$libs/um/x64"
clang-cl --target=x86_64-pc-windows-msvc /nologo /c /TP /O2 /MD /EHsc /W4 /std:c++20 \
    /imsvc"$msvc/include" /imsvc"$headers/ucrt" /imsvc"$headers/shared" /imsvc"$headers/um" /imsvc"$headers/winrt" \
    /imsvc"$sdk/include" /Forr-probe.obj "$source"
lld-link /nologo /out:rr-probe.exe /subsystem:console rr-probe.obj "$lib" d3d12.lib dxgi.lib advapi32.lib user32.lib
