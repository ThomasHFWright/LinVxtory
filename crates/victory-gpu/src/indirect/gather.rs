//! An ordered prefix of read-only indirect draws, clamped by one prelude pass.
//! Stop at the first fallback or possible argument write: moving a later clamp
//! ahead of either would change the argument bytes or the remaining budget.

use super::{Clamp, clamp, host, min_stride, transition, uav_barrier};
use crate::host::HostList;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

const MOST: u32 = 4096;

#[derive(Default)]
pub(crate) struct Gather {
    #[cfg(test)]
    enabled_for_test: bool,
    closed: bool,
    batch: Option<Batch>,
}

struct Batch {
    clamp: &'static Clamp,
    src: ID3D12Resource,
    out: ID3D12Resource,
    kind: u32,
    stride: u32,
    records: u32,
    table: usize,
    table_gpu: u64,
    budget: u64,
}

// SAFETY: the offset table is the owner's upload allocation, written only
// under its gather lock and held until the fenced list is reset.
unsafe impl Send for Gather {}

impl Gather {
    pub(crate) fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            let on = std::env::var_os("VICTORY_INDIRECT_GATHER").is_some_and(|v| v == "1");
            if on { tracing::info!("indirect prefix gathering enabled (VICTORY_INDIRECT_GATHER)"); }
            on
        })
    }

    #[cfg(test)]
    pub(crate) fn enable_for_test(&mut self) {
        self.enabled_for_test = true;
    }

    pub(crate) fn push(
        &mut self,
        owner: &HostList,
        src: &ID3D12Resource,
        offset: u64,
        spec: (u32, u32, u32),
    ) -> Option<(ID3D12Resource, u64)> {
        let enabled = Self::enabled();
        #[cfg(test)]
        let enabled = enabled || self.enabled_for_test;
        if !enabled {
            return None;
        }
        self.push_enabled(owner, src, offset, spec)
    }

    fn push_enabled(
        &mut self,
        owner: &HostList,
        src: &ID3D12Resource,
        offset: u64,
        (kind, stride, records): (u32, u32, u32),
    ) -> Option<(ID3D12Resource, u64)> {
        let result = (|| {
            if self.closed
                || records != 1
                || !matches!(kind, 0 | 1 | 10)
                || stride < min_stride(kind)?
                || stride > 32
                || !stride.is_multiple_of(4)
                || !offset.is_multiple_of(4)
            {
                return None;
            }
            let offset = u32::try_from(offset).ok()?;
            let end = offset.checked_add(stride)?;
            // SAFETY: the caller holds the source resource.
            if u64::from(end) > unsafe { src.GetDesc() }.Width {
                return None;
            }
            if let Some(batch) = &self.batch {
                if batch.src.as_raw() != src.as_raw()
                    || batch.kind != kind
                    || batch.stride != stride
                    || batch.records == MOST
                {
                    return None;
                }
            } else {
                // Allocate and compile now: a slot returned to a draw must be
                // guaranteed to have a producer when its prelude is recorded.
                let clamp = clamp()?;
                let host = host()?;
                let mut out = None;
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: u64::from(stride) * u64::from(MOST),
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                    ..Default::default()
                };
                // SAFETY: a plain buffer description and our out pointer.
                unsafe {
                    host.device.CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut out,
                    )
                }
                .ok()?;
                let out = out?;
                let budget = owner.budget()?;
                let (table_gpu, table) = owner.scratch(u64::from(MOST) * 4)?;
                self.batch = Some(Batch {
                    clamp,
                    src: src.clone(),
                    out,
                    kind,
                    stride,
                    records: 0,
                    table: table as usize,
                    table_gpu,
                    budget,
                });
            }
            let batch = self.batch.as_mut()?;
            let slot = batch.records;
            // Repeated source offsets remain separate executions and charges.
            // SAFETY: at most MOST slots in the owner's live upload allocation.
            unsafe { (batch.table as *mut u32).add(slot as usize).write(offset) };
            batch.records += 1;
            Some((batch.out.clone(), u64::from(slot) * u64::from(stride)))
        })();
        if result.is_none() {
            self.close();
        }
        result
    }

    pub(crate) fn close(&mut self) {
        self.closed = true;
    }

    pub(crate) fn has_work(&self) -> bool {
        self.batch.as_ref().is_some_and(|b| b.records != 0)
    }

    /// Called at submission after the source refreshes and budget reset.
    pub(crate) fn record(&self, prelude: &ID3D12GraphicsCommandList) {
        let Some(b) = &self.batch else { return };
        static BATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let batches = BATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if batches.is_power_of_two() {
            tracing::info!(batches, records = b.records, "indirect records clamped in one prelude pass");
        }
        let constants = [b.kind, b.stride, b.records, 2u32];
        // SAFETY: all allocations and the clamp were retained when accepting
        // the first record; owner.reset happens only after the list has run.
        unsafe {
            prelude.ResourceBarrier(&[
                uav_barrier(),
                transition(
                    &b.out,
                    D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                ),
            ]);
            prelude.SetComputeRootSignature(&b.clamp.root);
            prelude.SetPipelineState(&b.clamp.pso);
            prelude.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
            prelude.SetComputeRootShaderResourceView(1, b.src.GetGPUVirtualAddress());
            prelude.SetComputeRootShaderResourceView(4, b.table_gpu);
            prelude.SetComputeRootUnorderedAccessView(2, b.budget);
            prelude.SetComputeRootUnorderedAccessView(3, b.out.GetGPUVirtualAddress());
            prelude.Dispatch(1, 1, 1);
            prelude.ResourceBarrier(&[
                uav_barrier(),
                transition(
                    &b.out,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                ),
            ]);
        }
    }

    /// End of the main list: make its gather scratch reusable on resubmission.
    pub(crate) fn finish(&self, commands: &ID3D12GraphicsCommandList) {
        if let Some(b) = &self.batch {
            // SAFETY: the prelude produced this buffer before any main-list use.
            unsafe {
                commands.ResourceBarrier(&[transition(
                    &b.out,
                    D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                    D3D12_RESOURCE_STATE_COMMON,
                )])
            };
        }
    }
}

#[cfg(test)]
#[path = "gather_tests.rs"]
mod tests;
