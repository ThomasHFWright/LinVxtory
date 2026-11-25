//! Local synthetic probe; contains no title code or data.
use super::host::{Dispatch, Host, Stride, View};
use windows::Win32::Graphics::Direct3D12::ID3D12RootSignature;

#[test]
#[ignore = "local GPU experiment; requires VICTORY_PC_DXC"]
fn diagnose_buffer_view_contract() {
    let dxc = std::env::var_os("VICTORY_PC_DXC").expect("VICTORY_PC_DXC");
    let host = Host::new(false).expect("SM 6.6 GPU");
    let dir = std::env::temp_dir().join(format!("victory-binding-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let data: Vec<u32> = (0..65536).map(|i| 100 + i).collect();
    for stride in [12u32, 16, 24, 176, 336] {
        let input = dir.join("probe.hlsl");
        let output = dir.join("probe.dxil");
        let source = format!(r#"
struct Record {{ uint words[{}]; }};
[RootSignature("RootFlags(CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED)")]
[numthreads(1,1,1)]
void main() {{
    ByteAddressBuffer raw = ResourceDescriptorHeap[0];
    StructuredBuffer<Record> structured = ResourceDescriptorHeap[0];
    RWByteAddressBuffer output = ResourceDescriptorHeap[1];
    output.Store(0, raw.Load({stride}));
    output.Store(4, structured[1].words[0]);
}}
"#, stride / 4);
        std::fs::write(&input, source).unwrap();
        let compiled = std::process::Command::new(&dxc)
            .args(["-T", "cs_6_6", "-E", "main", "-Fo"])
            .arg(&output).arg(&input).output().unwrap();
        assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));
        let dxil = std::fs::read(&output).unwrap();
        // SAFETY: synthetic DXIL contains its matching root signature.
        let root: ID3D12RootSignature = unsafe { host.device.CreateRootSignature(0, &dxil) }.unwrap();
        let pso = host.compute_pipeline_with(&root, &dxil).unwrap();
        let expected = 100 + stride / 4;
        for (label, view) in [
            ("raw", View::Buffer { data: &data, stride: Stride::Raw }),
            ("structured", View::Buffer { data: &data, stride: Stride::Structured(stride) }),
            ("offset16", View::Buffer { data: &data, stride: Stride::RawOffset(16) }),
            ("null", View::NullRawSrv),
        ] {
            let expected = match label { "null" => 0, "offset16" => expected + 4, _ => expected };
            let views = [view, View::RawUav(2)];
            let result = host.dispatch(&root, &pso, &Dispatch {
                roots: vec![], views: &views, samplers: &[], groups: [1, 1, 1],
            }).unwrap();
            let got = &result[0];
            eprintln!("BINDING_PROBE stride={stride} view={label} expected={expected} raw_read={} structured_read={}", got[0], got[1]);
            let matching = if label == "raw" { got[0] } else { got[1] };
            assert_eq!(matching, expected, "matching handle/view must read the intended bytes");
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}
