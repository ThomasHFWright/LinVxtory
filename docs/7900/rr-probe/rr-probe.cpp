#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <d3d12.h>
#include <dxgi1_4.h>
#include <wrl/client.h>
#include <DirectXPackedVector.h>
#include <nvsdk_ngx_helpers_dlssd_d3d.h>

#include <algorithm>
#include <array>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>

using Microsoft::WRL::ComPtr;
using DirectX::PackedVector::XMConvertFloatToHalf;
using DirectX::PackedVector::XMConvertHalfToFloat;

constexpr unsigned inputWidth = 1280, inputHeight = 720;
constexpr unsigned outputWidth = 3840, outputHeight = 2160;
constexpr auto inputState = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
constexpr auto outputState = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
constexpr auto rgbaFormat = DXGI_FORMAT_R16G16B16A16_FLOAT;
constexpr uint16_t sentinel = 0xc800; // -8 as a half float.

[[noreturn]] void fail(const char* message) {
    std::fprintf(stderr, "FAIL %s\n", message);
    std::exit(1);
}

void check(HRESULT result, const char* stage) {
    if (FAILED(result)) {
        std::fprintf(stderr, "%s hresult=0x%08lx\n", stage, static_cast<unsigned long>(result));
        fail(stage);
    }
}

void ngxCheck(NVSDK_NGX_Result result, const char* stage) {
    std::printf("%s result=0x%08x\n", stage, static_cast<unsigned>(result));
    if (result != NVSDK_NGX_Result_Success)
        fail(stage);
}

ComPtr<ID3D12Resource> resource(ID3D12Device* device, D3D12_HEAP_TYPE heapType,
                               const D3D12_RESOURCE_DESC& desc, D3D12_RESOURCE_STATES state,
                               const wchar_t* name) {
    D3D12_HEAP_PROPERTIES heap{};
    heap.Type = heapType;
    heap.CreationNodeMask = heap.VisibleNodeMask = 1;
    ComPtr<ID3D12Resource> result;
    check(device->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state,
                                           nullptr, IID_PPV_ARGS(&result)), "CreateCommittedResource");
    check(result->SetName(name), "SetName");
    return result;
}

ComPtr<ID3D12Resource> buffer(ID3D12Device* device, UINT64 size, D3D12_HEAP_TYPE heap,
                             D3D12_RESOURCE_STATES state, const wchar_t* name) {
    D3D12_RESOURCE_DESC desc{};
    desc.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    desc.Width = size;
    desc.Height = desc.DepthOrArraySize = desc.MipLevels = desc.SampleDesc.Count = 1;
    desc.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    return resource(device, heap, desc, state, name);
}

enum Guide { Color, Depth, Motion, Diffuse, Specular, NormalsRoughness, SpecularMotion, Output };
using Pixel = std::array<float, 4>;

Pixel texel(Guide guide, unsigned x, unsigned y, unsigned frame) {
    const bool foreground = x > 270 && x < 1040 && y > 150 && y < 570;
    const bool tile = ((x / 43) ^ (y / 31)) & 1;
    switch (guide) {
    case Color: {
        const uint32_t bits = (x * 73856093u) ^ (y * 19349663u) ^ ((frame + 1) * 83492791u);
        const float noise = static_cast<float>((bits ^ (bits >> 13)) & 127) / 1024.0f;
        const float light = (tile ? 0.38f : 0.15f) + noise + frame * 0.05f;
        return foreground ? Pixel{light, 0.3f * light, 0.12f, 1.0f}
                          : Pixel{0.06f, 0.6f * light, light, 1.0f};
    }
    case Depth:
        return {foreground ? 2.0f : 5.0f};
    case Motion:
    case SpecularMotion:
        return {};
    case Diffuse:
        return foreground ? Pixel{0.7f, 0.21f, 0.12f, 1.0f} : Pixel{0.1f, 0.36f, 0.6f, 1.0f};
    case Specular:
        return {0.04f, 0.04f, 0.04f, 1.0f};
    case NormalsRoughness:
        return foreground ? Pixel{0.0f, 0.0f, -1.0f, 0.3f} : Pixel{0.0f, 0.6f, -0.8f, 0.65f};
    case Output:
        return {-8.0f, -8.0f, -8.0f, -8.0f};
    }
    fail("unknown guide");
}

void transition(ID3D12GraphicsCommandList* list, ID3D12Resource* texture,
                D3D12_RESOURCE_STATES before, D3D12_RESOURCE_STATES after) {
    D3D12_RESOURCE_BARRIER barrier{};
    barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    barrier.Transition = {texture, D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, before, after};
    list->ResourceBarrier(1, &barrier);
}

struct Texture {
    ComPtr<ID3D12Resource> image, upload;
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT layout{};
    UINT64 size = 0;

    void create(ID3D12Device* device, unsigned width, unsigned height, DXGI_FORMAT format,
                D3D12_RESOURCE_FLAGS flags, const wchar_t* name) {
        D3D12_RESOURCE_DESC desc{};
        desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
        desc.Width = width;
        desc.Height = height;
        desc.DepthOrArraySize = desc.MipLevels = desc.SampleDesc.Count = 1;
        desc.Format = format;
        desc.Flags = flags;
        image = resource(device, D3D12_HEAP_TYPE_DEFAULT, desc, D3D12_RESOURCE_STATE_COPY_DEST, name);
        device->GetCopyableFootprints(&desc, 0, 1, 0, &layout, nullptr, nullptr, &size);
        upload = buffer(device, size, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ,
                        L"RR upload");
    }

    D3D12_TEXTURE_COPY_LOCATION imageLocation() const {
        D3D12_TEXTURE_COPY_LOCATION location{};
        location.pResource = image.Get();
        location.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
        return location;
    }

    D3D12_TEXTURE_COPY_LOCATION bufferLocation(ID3D12Resource* allocation) const {
        D3D12_TEXTURE_COPY_LOCATION location{};
        location.pResource = allocation;
        location.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
        location.PlacedFootprint = layout;
        return location;
    }

    void fill(ID3D12GraphicsCommandList* list, Guide guide, unsigned frame,
              D3D12_RESOURCE_STATES state) const {
        void* mapped = nullptr;
        const D3D12_RANGE noRead{0, 0};
        check(upload->Map(0, &noRead, &mapped), "Map upload");
        const auto& footprint = layout.Footprint;
        for (unsigned y = 0; y < footprint.Height; ++y) {
            auto* row = static_cast<unsigned char*>(mapped) + layout.Offset + y * footprint.RowPitch;
            for (unsigned x = 0; x < footprint.Width; ++x) {
                const Pixel value = texel(guide, x, y, frame);
                if (footprint.Format == rgbaFormat) {
                    uint16_t encoded[4];
                    for (unsigned c = 0; c < 4; ++c)
                        encoded[c] = XMConvertFloatToHalf(value[c]);
                    std::memcpy(row + x * sizeof(encoded), encoded, sizeof(encoded));
                } else {
                    const size_t bytes = footprint.Format == DXGI_FORMAT_R32_FLOAT ? 4 : 8;
                    std::memcpy(row + x * bytes, value.data(), bytes);
                }
            }
        }
        upload->Unmap(0, nullptr);
        const auto dst = imageLocation();
        const auto src = bufferLocation(upload.Get());
        if (frame)
            transition(list, image.Get(), state, D3D12_RESOURCE_STATE_COPY_DEST);
        list->CopyTextureRegion(&dst, 0, 0, 0, &src, nullptr);
        transition(list, image.Get(), D3D12_RESOURCE_STATE_COPY_DEST, state);
    }
};

float bilinear(unsigned x, unsigned y, unsigned frame) {
    const float sx = std::clamp((x + 0.5f) / 3.0f - 0.5f, 0.0f, float(inputWidth - 1));
    const float sy = std::clamp((y + 0.5f) / 3.0f - 0.5f, 0.0f, float(inputHeight - 1));
    const unsigned x0 = static_cast<unsigned>(sx), y0 = static_cast<unsigned>(sy);
    const unsigned x1 = std::min(x0 + 1, inputWidth - 1), y1 = std::min(y0 + 1, inputHeight - 1);
    const float top = std::lerp(texel(Color, x0, y0, frame)[0], texel(Color, x1, y0, frame)[0], sx - x0);
    const float bottom = std::lerp(texel(Color, x0, y1, frame)[0], texel(Color, x1, y1, frame)[0], sx - x0);
    return std::lerp(top, bottom, sy - y0);
}

uint64_t readOutput(const Texture& output, ID3D12Resource* readback, unsigned frame) {
    const D3D12_RANGE read{0, static_cast<SIZE_T>(output.size)};
    void* mapped = nullptr;
    check(readback->Map(0, &read, &mapped), "Map output");
    uint64_t hash = 14695981039346656037ull;
    unsigned unwritten = 0, nonfinite = 0, nonzero = 0, samples = 0, changed = 0;
    float minRed = 1000.0f, maxRed = -1000.0f;
    for (unsigned y = 0; y < outputHeight; ++y) {
        const auto* row = reinterpret_cast<const uint16_t*>(
            static_cast<const unsigned char*>(mapped) + output.layout.Offset +
            y * output.layout.Footprint.RowPitch);
        for (unsigned x = 0; x < outputWidth; ++x) {
            const auto* rgb = row + x * 4;
            const float red = XMConvertHalfToFloat(rgb[0]);
            minRed = std::min(minRed, red);
            maxRed = std::max(maxRed, red);
            for (unsigned c = 0; c < 3; ++c) {
                hash = (hash ^ rgb[c]) * 1099511628211ull;
                unwritten += rgb[c] == sentinel;
                nonfinite += !std::isfinite(XMConvertHalfToFloat(rgb[c]));
            }
            nonzero += std::abs(red) > 0.001f;
            if (x % 17 == 0 && y % 17 == 0) {
                ++samples;
                changed += std::abs(red - bilinear(x, y, frame)) > 0.003f;
            }
        }
    }
    const D3D12_RANGE noWrite{0, 0};
    readback->Unmap(0, &noWrite);
    std::printf("readback frame=%u hash_rgb=0x%016llx sentinel_components=%u nonfinite=%u "
                "nonzero_red=%u min_red=%.6f max_red=%.6f different_from_bilinear=%u/%u\n",
                frame, static_cast<unsigned long long>(hash), unwritten, nonfinite, nonzero,
                minRed, maxRed, changed, samples);
    if (unwritten || nonfinite || nonzero < outputWidth * outputHeight / 10 ||
        maxRed - minRed < 0.01f || changed < samples / 100)
        fail("RR output verification");
    return hash;
}

void NVSDK_CONV ngxLog(const char* message, NVSDK_NGX_Logging_Level level, NVSDK_NGX_Feature feature) {
    std::printf("NGX feature=%d level=%d %s\n", int(feature), int(level), message);
}

int main() {
    std::setvbuf(stdout, nullptr, _IONBF, 0);
    static_assert(NVSDK_NGX_Feature_RayReconstruction == 13);
    std::printf("feature=RayReconstruction id=%u sdk_api=0x%x input=%ux%u output=%ux%u "
                "mode=UltraPerformance\n", unsigned(NVSDK_NGX_Feature_RayReconstruction),
                unsigned(NVSDK_NGX_Version_API), inputWidth, inputHeight, outputWidth, outputHeight);

    ComPtr<IDXGIFactory4> factory;
    check(CreateDXGIFactory1(IID_PPV_ARGS(&factory)), "CreateDXGIFactory1");
    ComPtr<ID3D12Device> device;
    for (UINT index = 0; ; ++index) {
        ComPtr<IDXGIAdapter1> adapter;
        const HRESULT result = factory->EnumAdapters1(index, &adapter);
        if (result == DXGI_ERROR_NOT_FOUND)
            break;
        check(result, "EnumAdapters1");
        DXGI_ADAPTER_DESC1 desc{};
        check(adapter->GetDesc1(&desc), "GetDesc1");
        if (desc.VendorId != 0x10de || (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE))
            continue;
        std::printf("adapter=%ls vendor=0x%04x device=0x%04x luid=%08lx:%08lx\n", desc.Description,
                    desc.VendorId, desc.DeviceId, static_cast<unsigned long>(desc.AdapterLuid.HighPart),
                    desc.AdapterLuid.LowPart);
        check(D3D12CreateDevice(adapter.Get(), D3D_FEATURE_LEVEL_12_0, IID_PPV_ARGS(&device)),
              "D3D12CreateDevice");
        break;
    }
    if (!device)
        fail("no NVIDIA hardware adapter");

    ComPtr<ID3D12CommandQueue> queue;
    D3D12_COMMAND_QUEUE_DESC queueDesc{};
    queueDesc.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    check(device->CreateCommandQueue(&queueDesc, IID_PPV_ARGS(&queue)), "CreateCommandQueue");
    ComPtr<ID3D12CommandAllocator> allocator;
    check(device->CreateCommandAllocator(queueDesc.Type, IID_PPV_ARGS(&allocator)), "CreateCommandAllocator");
    ComPtr<ID3D12GraphicsCommandList> list;
    check(device->CreateCommandList(0, queueDesc.Type, allocator.Get(), nullptr, IID_PPV_ARGS(&list)),
          "CreateCommandList");
    ComPtr<ID3D12Fence> fence;
    check(device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&fence)), "CreateFence");
    const HANDLE event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!event)
        fail("CreateEventW");
    auto submit = [&](const char* stage, UINT64 value) {
        check(list->Close(), "Close command list");
        ID3D12CommandList* lists[] = {list.Get()};
        queue->ExecuteCommandLists(1, lists);
        check(queue->Signal(fence.Get(), value), "Signal fence");
        check(fence->SetEventOnCompletion(value, event), "SetEventOnCompletion");
        const DWORD wait = WaitForSingleObject(event, 45000);
        const HRESULT removed = device->GetDeviceRemovedReason();
        const UINT64 completed = fence->GetCompletedValue();
        std::printf("fence stage=%s requested=%llu completed=%llu wait=%lu device=0x%08lx\n",
                    stage, value, completed, wait, static_cast<unsigned long>(removed));
        check(removed, "GetDeviceRemovedReason");
        if (wait != WAIT_OBJECT_0 || completed != value)
            fail(stage);
        check(allocator->Reset(), "Reset allocator");
        check(list->Reset(allocator.Get(), nullptr), "Reset command list");
    };

    wchar_t exe[32768];
    const DWORD exeLength = GetModuleFileNameW(nullptr, exe, ARRAYSIZE(exe));
    if (!exeLength || exeLength == ARRAYSIZE(exe))
        fail("GetModuleFileNameW");
    const auto featureDir = std::filesystem::path(exe).parent_path();
    const auto dataDir = std::filesystem::current_path();
    const wchar_t* paths[] = {featureDir.c_str()};
    NVSDK_NGX_FeatureCommonInfo common{};
    common.PathListInfo = {paths, 1};
    common.LoggingInfo = {ngxLog, NVSDK_NGX_LOGGING_LEVEL_VERBOSE, false};
    ngxCheck(NVSDK_NGX_D3D12_Init_with_ProjectID("3125490a-c41b-4fae-a950-54c531cfb866",
        NVSDK_NGX_ENGINE_TYPE_CUSTOM, "1.0", dataDir.c_str(), device.Get(), &common), "ngx_init");

    NVSDK_NGX_Parameter* params = nullptr;
    ngxCheck(NVSDK_NGX_D3D12_GetCapabilityParameters(&params), "ngx_get_capabilities");
    if (!params)
        fail("null NGX capabilities");
    auto capability = [&](const char* name) {
        int value = 0;
        const auto result = NVSDK_NGX_Parameter_GetI(params, name, &value);
        std::printf("capability %s value=0x%x result=0x%08x\n", name, unsigned(value), unsigned(result));
        return result == NVSDK_NGX_Result_Success ? value : 0;
    };
    capability(NVSDK_NGX_Parameter_SuperSamplingDenoising_FeatureInitResult);
    capability(NVSDK_NGX_Parameter_SuperSamplingDenoising_NeedsUpdatedDriver);
    capability(NVSDK_NGX_Parameter_SuperSamplingDenoising_MinDriverVersionMajor);
    capability(NVSDK_NGX_Parameter_SuperSamplingDenoising_MinDriverVersionMinor);
    if (capability(NVSDK_NGX_Parameter_SuperSamplingDenoising_Available) != 1)
        fail("RR unavailable");

    constexpr auto mode = NVSDK_NGX_PerfQuality_Value_UltraPerformance;
    unsigned width = 0, height = 0, maxX = 0, maxY = 0, minX = 0, minY = 0;
    float sharpness = 0.0f;
    ngxCheck(NGX_DLSSD_GET_OPTIMAL_SETTINGS(params, outputWidth, outputHeight, mode,
        &width, &height, &maxX, &maxY, &minX, &minY, &sharpness), "ngx_rr_optimal");
    std::printf("optimal=%ux%u min=%ux%u max=%ux%u\n", width, height, minX, minY, maxX, maxY);
    if (width != inputWidth || height != inputHeight)
        fail("NGX RR optimal size differs from the 720p to 4K fixture");

    NVSDK_NGX_DLSSD_Create_Params create{};
    create.InDenoiseMode = NVSDK_NGX_DLSS_Denoise_Mode_DLUnified;
    create.InRoughnessMode = NVSDK_NGX_DLSS_Roughness_Mode_Packed;
    create.InUseHWDepth = NVSDK_NGX_DLSS_Depth_Type_Linear;
    create.InWidth = inputWidth;
    create.InHeight = inputHeight;
    create.InTargetWidth = outputWidth;
    create.InTargetHeight = outputHeight;
    create.InPerfQualityValue = mode;
    create.InFeatureCreateFlags = NVSDK_NGX_DLSS_Feature_Flags_IsHDR |
        NVSDK_NGX_DLSS_Feature_Flags_MVLowRes | NVSDK_NGX_DLSS_Feature_Flags_AutoExposure;
    NVSDK_NGX_Handle* feature = nullptr;
    ngxCheck(NGX_D3D12_CREATE_DLSSD_EXT(list.Get(), 1, 1, &feature, params, &create), "ngx_rr_create");
    if (!feature)
        fail("null RR feature handle");
    submit("rr_create", 1);

    Texture inputs[Output], output;
    auto createInput = [&](Guide guide, DXGI_FORMAT format, const wchar_t* name) {
        inputs[guide].create(device.Get(), inputWidth, inputHeight, format, D3D12_RESOURCE_FLAG_NONE, name);
    };
    createInput(Color, rgbaFormat, L"RR noisy HDR color");
    createInput(Depth, DXGI_FORMAT_R32_FLOAT, L"RR view depth");
    createInput(Motion, DXGI_FORMAT_R32G32_FLOAT, L"RR motion");
    createInput(Diffuse, rgbaFormat, L"RR diffuse albedo");
    createInput(Specular, rgbaFormat, L"RR specular albedo");
    createInput(NormalsRoughness, rgbaFormat, L"RR normals and roughness");
    createInput(SpecularMotion, DXGI_FORMAT_R32G32_FLOAT, L"RR specular motion");
    output.create(device.Get(), outputWidth, outputHeight, rgbaFormat,
                  D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, L"RR output");
    const auto readback = buffer(device.Get(), output.size, D3D12_HEAP_TYPE_READBACK,
                                 D3D12_RESOURCE_STATE_COPY_DEST, L"RR readback");

    NVSDK_NGX_D3D12_DLSSD_Eval_Params eval{};
    eval.pInColor = inputs[Color].image.Get();
    eval.pInDepth = inputs[Depth].image.Get();
    eval.pInMotionVectors = inputs[Motion].image.Get();
    eval.pInDiffuseAlbedo = inputs[Diffuse].image.Get();
    eval.pInSpecularAlbedo = inputs[Specular].image.Get();
    eval.pInNormals = inputs[NormalsRoughness].image.Get();
    eval.pInMotionVectorsReflections = inputs[SpecularMotion].image.Get();
    eval.pInOutput = output.image.Get();
    eval.InRenderSubrectDimensions = {inputWidth, inputHeight};
    eval.InMVScaleX = eval.InMVScaleY = eval.InPreExposure = eval.InExposureScale = 1.0f;
    eval.InFrameTimeDeltaInMsec = 16.67f;

    uint64_t hashes[2];
    for (unsigned frame = 0; frame < 2; ++frame) {
        for (unsigned guide = 0; guide < Output; ++guide)
            inputs[guide].fill(list.Get(), static_cast<Guide>(guide), frame, inputState);
        output.fill(list.Get(), Output, frame, outputState);
        eval.InReset = (frame == 0);
        // The synthetic scene is stationary. Jitter and noise advance between samples.
        eval.InJitterOffsetX = frame ? 0.25f : -0.25f;
        eval.InJitterOffsetY = frame ? 1.0f / 6.0f : -1.0f / 6.0f;
        std::printf("evaluate frame=%u reset=%d jitter=%.4f,%.4f\n", frame, eval.InReset,
                    eval.InJitterOffsetX, eval.InJitterOffsetY);
        ngxCheck(NGX_D3D12_EVALUATE_DLSSD_EXT(list.Get(), feature, params, &eval), "ngx_rr_evaluate");
        transition(list.Get(), output.image.Get(), outputState, D3D12_RESOURCE_STATE_COPY_SOURCE);
        const auto src = output.imageLocation();
        const auto dst = output.bufferLocation(readback.Get());
        list->CopyTextureRegion(&dst, 0, 0, 0, &src, nullptr);
        transition(list.Get(), output.image.Get(), D3D12_RESOURCE_STATE_COPY_SOURCE, outputState);
        submit(frame ? "rr_history" : "rr_reset", frame + 2);
        hashes[frame] = readOutput(output, readback.Get(), frame);
    }
    if (hashes[0] == hashes[1])
        fail("reset and history outputs are identical");

    ngxCheck(NVSDK_NGX_D3D12_ReleaseFeature(feature), "ngx_rr_release");
    ngxCheck(NVSDK_NGX_D3D12_DestroyParameters(params), "ngx_destroy_parameters");
    ngxCheck(NVSDK_NGX_D3D12_Shutdown1(device.Get()), "ngx_shutdown");
    CloseHandle(event);
    std::puts("PASS D3D12 Ray Reconstruction created, evaluated, fenced, and verified on both frames");
}
