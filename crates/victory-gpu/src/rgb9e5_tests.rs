//! Synthetic packer checks, independent of title format promotion and FidelityFX.
#![cfg(all(test, windows))]

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::host::{host, transition};
use crate::rgb9e5::bridge;

/// This needs the compiler DLLs, but does not create a device or use a GPU.
/// Exercise the same compiler entry point the production packer uses.
#[test]
fn rgb9e5_packer_compilation_preserves_its_root_signature() {
    let bytes = super::compile_packer().expect("RGB9E5 packer must compile");
    let container = victory_shader::Container::parse(&bytes).expect("valid shader container");
    let root = container
        .part(victory_shader::FourCC::RTS0)
        .expect("RGB9E5 packer must retain the embedded root signature under Wine");
    assert!(!root.data.is_empty(), "empty packer root signature");
    assert!(
        container.digest.iter().any(|&b| b != 0),
        "unsigned packer container"
    );
    eprintln!(
        "RGB9E5 packer: {} bytes, {} root-signature bytes, nonzero digest",
        bytes.len(),
        root.data.len()
    );
}

/// A clamped half in units of 2^-24: every finite half is an integer here.
fn half_units(h: u16) -> u64 {
    const MAX: u64 = 511 << 31; // 65408 * 2^24
    let exponent = (h >> 10) & 31;
    let fraction = u64::from(h & 1023);
    if h & 0x8000 != 0 || exponent == 31 && fraction != 0 {
        0
    } else if exponent == 31 {
        MAX
    } else if exponent == 0 {
        fraction
    } else {
        ((1024 + fraction) << (exponent - 1)).min(MAX)
    }
}

/// Choose the finest shared scale whose rounded mantissas fit. No float,
/// log2, exp2 or shader implementation is used by this reference.
fn reference(rgb: [u16; 3]) -> u32 {
    let units = rgb.map(half_units);
    for exponent in 0..=31u32 {
        let half_step = if exponent == 0 {
            0
        } else {
            1u64 << (exponent - 1)
        };
        let q = units.map(|n| (n + half_step) >> exponent);
        if q.iter().all(|&n| n <= 511) {
            return q[0] as u32 | (q[1] as u32) << 9 | (q[2] as u32) << 18 | exponent << 27;
        }
    }
    unreachable!("clamped components always fit exponent 31")
}

#[test]
fn rgb9e5_integer_reference_covers_clamps_ties_and_exponent_carry() {
    assert_eq!(reference([0, 0, 0]), 0);
    assert_eq!(reference([1, 0, 0]), 1); // smallest positive half
    assert_eq!(reference([0x3c00; 3]), 0x8402_0100); // (1, 1, 1)
    assert_eq!(reference([0x7bfc; 3]), u32::MAX); // largest RGB9E5 value
    assert_eq!(reference([0x7bff; 3]), u32::MAX); // largest finite half clamps
    assert_eq!(reference([0x7c00, 0x7e00, 0xbc00]), 0xf800_01ff); // +inf, NaN, -1
    assert_eq!(reference([0xfc00, 0x8000, 0x7c01]), 0); // -inf, -0, signaling NaN
    assert_eq!(
        reference([0x3c00, 0x1800, 0x1e00]),
        16 << 27 | 256 | 1 << 9 | 2 << 18
    );
    assert_eq!(reference([0x3ffd, 0, 0]), 16 << 27 | 511);
    assert_eq!(reference([0x3ffe, 0, 0]), 17 << 27 | 256); // rounded 512 carries
}

const WIDTH: u32 = 257;
const HEIGHT: u32 = 256;
const FROM_PITCH: u32 = (WIDTH * 8).next_multiple_of(256);
const TO_PITCH: u32 = (WIDTH * 4).next_multiple_of(256);
const GUARD: usize = 512; // also a valid placed-footprint offset
const CANARY: u8 = 0xa5;

fn pattern(pixel: u32, pass: u32) -> [u16; 4] {
    let h = pixel as u16;
    if pass == 0 {
        // Red alone exposes small values that a larger shared channel would hide.
        [h, 0, 0, !h]
    } else {
        // Each channel permutes all 65536 bit patterns; alpha must be ignored.
        [
            h ^ 0x8000,
            h.rotate_left(5),
            h.wrapping_mul(40503).wrapping_add(17),
            h,
        ]
    }
}

fn footprint(
    resource: &ID3D12Resource,
    format: DXGI_FORMAT,
    pitch: u32,
    offset: u64,
) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: borrowed COM pointer; the caller retains the resource through execution.
        pResource: unsafe { std::mem::transmute_copy(resource) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: offset,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: format,
                    Width: WIDTH,
                    Height: HEIGHT,
                    Depth: 1,
                    RowPitch: pitch,
                },
            },
        },
    }
}

fn subresource(resource: &ID3D12Resource) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: borrowed COM pointer, retained by the caller.
        pResource: unsafe { std::mem::transmute_copy(resource) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: 0,
        },
    }
}

#[test]
fn rgb9e5_bridge_packs_all_half_patterns_and_reuses_its_states() {
    let host = host().expect("GPU required for RGB9E5 packer regression");
    let shared = bridge([WIDTH, HEIGHT]).expect("RGB9E5 bridge and packer required");
    let mut output: Option<ID3D12Resource> = None;
    // SAFETY: valid raw host texture, deliberately bypassing title target_format(67).
    unsafe {
        host.device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: u64::from(WIDTH),
                    Height: HEIGHT,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: DXGI_FORMAT_R9G9B9E5_SHAREDEXP,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut output,
            )
            .expect("raw RGB9E5 COPY_DEST texture");
    }
    let output = output.unwrap();
    // SAFETY: live raw host texture.
    assert_eq!(
        unsafe { output.GetDesc() }.Format,
        DXGI_FORMAT_R9G9B9E5_SHAREDEXP
    );
    let input_bytes = (FROM_PITCH * HEIGHT) as usize;
    let packed_bytes = (TO_PITCH * HEIGHT) as usize;
    let readback_bytes = GUARD + packed_bytes + GUARD;
    for pass in 0..2 {
        assert!(std::ptr::eq(shared, bridge([WIDTH, HEIGHT]).unwrap()));
        let list = host.new_list().expect("host command list");
        let (upload, data) = list.upload(host, input_bytes as u64).expect("FP16 upload");
        // SAFETY: the list owns this mapped buffer, not yet submitted to the GPU.
        let bytes = unsafe { std::slice::from_raw_parts_mut(data, input_bytes) };
        bytes.fill(CANARY);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let at = (y * FROM_PITCH + x * 8) as usize;
                for (c, h) in pattern(y * WIDTH + x, pass).into_iter().enumerate() {
                    bytes[at + c * 2..at + c * 2 + 2].copy_from_slice(&h.to_le_bytes());
                }
            }
        }
        let (sentinel, data) = list
            .upload(host, readback_bytes as u64)
            .expect("guard upload");
        // SAFETY: initialize all bytes of a mapped upload buffer before submission.
        unsafe { data.write_bytes(CANARY, readback_bytes) };
        let (readback, data) = list
            .readback(host, readback_bytes as u64)
            .expect("RGB9E5 readback");
        // SAFETY: live resources, aligned footprints and explicit state transitions.
        unsafe {
            list.list
                .CopyBufferRegion(&readback, 0, &sentinel, 0, readback_bytes as u64);
            list.list.ResourceBarrier(&[transition(
                shared.texture(),
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )]);
            list.list.CopyTextureRegion(
                &subresource(shared.texture()),
                0,
                0,
                0,
                &footprint(&upload, DXGI_FORMAT_R16G16B16A16_FLOAT, FROM_PITCH, 0),
                None,
            );
            list.list.ResourceBarrier(&[transition(
                shared.texture(),
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
        }
        shared.write(&list.list, &output);
        // SAFETY: bridge.write leaves output in COPY_DEST and restores its own resources.
        unsafe {
            list.list.ResourceBarrier(&[transition(
                &output,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            list.list.CopyTextureRegion(
                &footprint(
                    &readback,
                    DXGI_FORMAT_R9G9B9E5_SHAREDEXP,
                    TO_PITCH,
                    GUARD as u64,
                ),
                0,
                0,
                0,
                &subresource(&output),
                None,
            );
            list.list.ResourceBarrier(&[transition(
                &output,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )]);
        }
        list.close();
        host.execute(&[&list]);
        // SAFETY: device query after the host's synchronous submission.
        unsafe { host.device.GetDeviceRemovedReason() }.expect("device survived RGB9E5 packing");
        // SAFETY: execution finished; the list still owns the mapped readback.
        let bytes = unsafe { std::slice::from_raw_parts(data, readback_bytes) };
        assert!(
            bytes[..GUARD].iter().all(|&b| b == CANARY),
            "prefix guard, pass {pass}"
        );
        assert!(
            bytes[GUARD + packed_bytes..].iter().all(|&b| b == CANARY),
            "suffix guard, pass {pass}"
        );
        for y in 0..HEIGHT {
            let row = GUARD + (y * TO_PITCH) as usize;
            for x in 0..WIDTH {
                let at = row + (x * 4) as usize;
                let got = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
                let [r, g, b, _] = pattern(y * WIDTH + x, pass);
                assert_eq!(
                    got,
                    reference([r, g, b]),
                    "pass {pass} ({x},{y}) half {r:04x}/{g:04x}/{b:04x}"
                );
            }
            assert!(
                bytes[row + (WIDTH * 4) as usize..row + TO_PITCH as usize]
                    .iter()
                    .all(|&b| b == CANARY),
                "row padding, pass {pass} y={y}"
            );
        }
    }
}
