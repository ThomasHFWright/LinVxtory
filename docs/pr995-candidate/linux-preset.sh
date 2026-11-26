# Source in a fresh launch environment after setting private paths.
# Does not start Wine or the title. Keep input read-only and state outside Git.
: "${VICTORY_DXC:?Set the Windows path to DXC 1.9.2609 dxcompiler.dll}"
: "${VICTORY_DXIL:?Set the Windows path to DXC 1.9.2609 dxil.dll}"
: "${VICTORY_DATA_DIR:?Set a private candidate data directory}"
: "${VICTORY_CACHE_DIR:?Set a private candidate cache directory}"
: "${WINEPREFIX:?Set a private candidate Wine prefix}"
: "${VKD3D_SHADER_CACHE_PATH:?Set a private candidate driver-cache directory}"
export VICTORY_DXC VICTORY_DXIL VICTORY_DATA_DIR VICTORY_CACHE_DIR
export WINEPREFIX VKD3D_SHADER_CACHE_PATH
export WINEDLLOVERRIDES='mscoree,mshtml=;d3dcompiler_47=b;d3d12,d3d12core,dxgi=n'
export VKD3D_DEBUG=info
export VKD3D_DISABLE_EXTENSIONS=VK_NV_raw_access_chains
export VICTORY_ASYNC_SUBMIT=1 VICTORY_QUEUE2=1 VICTORY_JOIN_PARTS=1
export VICTORY_INDIRECT_BATCH=3 VICTORY_INDIRECT_WINDOW=0
export VICTORY_COPY_PLAIN=1 VICTORY_TEXTURE_NO_WAIT=1 VICTORY_PIPELINE_WAIT_MS=0
export VICTORY_WINDOW_MAX=2400x1350
# The measured trial disabled the title watchdog and had an external time limit.
export VICTORY_NO_WATCHDOG=1
# Preserve the candidate's default retry selection used in the measured trial.
unset VICTORY_WAVE_LOCAL VICTORY_WAVE_WIDE VICTORY_WAVE_PAIRS
