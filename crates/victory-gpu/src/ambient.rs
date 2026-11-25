//! A least amount of indirect light while ray-traced probes do not run.
//!
//! The title's indirect diffuse light comes from probes filled by tracing
//! rays, which do not run here yet: its program that resolves them
//! (`CS_ScreenProbes_TemporalResolve_*`) leaves next to nothing, and whatever
//! the sun does not reach directly is black. Right after that program, its
//! output is replaced by a constant. The constant is the same everywhere:
//! the light in these images does not go with the title's exposure. The
//! title's own lighting then applies it with the surfaces' colors.
//!
//! `VICTORY_AMBIENT=<level>` sets it (0.6 by default, 0 for none); `off`
//! keeps the title's own output.
//! shortcut: the same light everywhere, from everywhere; gone once rays run.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

use crate::host::{HostList, host};
use crate::object::This;

/// The programs it follows, by the start of their name.
pub(crate) const PROGRAM: &[u8] = b"CS_ScreenProbes_TemporalResolve";
/// Where they write the indirect diffuse light (R9G9B9E5 as 32-bit
/// integers): root parameter and place in its table.
pub(crate) const LIGHT: (u8, u32) = (1, 176);

/// 0.35 with neutral sky-fill tint balances interior readability without
/// blowing out sunlit hair or washing the cell orange. `VICTORY_AMBIENT=off`
/// leaves what the title's resolve wrote; 0 is no indirect diffuse light.
const DEFAULT: f32 = 0.35;
const DEFAULT_TINT: [f32; 3] = [0.94, 0.97, 1.0];

fn tint() -> [f32; 3] {
    static TINT: OnceLock<[f32; 3]> = OnceLock::new();
    *TINT.get_or_init(|| {
        let Ok(set) = std::env::var("VICTORY_AMBIENT_TINT") else {
            return DEFAULT_TINT;
        };
        let parts: Vec<f32> = set
            .split(',')
            .filter_map(|p| p.trim().parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .collect();
        if parts.len() == 3 {
            [parts[0], parts[1], parts[2]]
        } else {
            DEFAULT_TINT
        }
    })
}

/// The floor; `None` if off.
pub(crate) fn level() -> Option<f32> {
    static LEVEL: OnceLock<f32> = OnceLock::new();
    let level = *LEVEL.get_or_init(|| {
        let set = std::env::var("VICTORY_AMBIENT").unwrap_or_default();
        if set.trim() == "off" {
            return -1.0;
        }
        set.trim()
            .parse()
            .ok()
            .filter(|l: &f32| l.is_finite() && *l >= 0.0)
            .unwrap_or(DEFAULT)
    });
    (level >= 0.0).then_some(level)
}

const SOURCE: &str = r#"
// The root signature is made in code ([`root_signature`]): Wine's compiler
// ignores a `[RootSignature]`. The image's size comes with the constants: it
// has no `GetDimensions` on a writable texture.
cbuffer Floor : register(b0) { float level; float3 tint; uint width; uint height; };
RWTexture2D<uint> light : register(u0);
float3 unpack(uint v) {
    return float3(v & 511, (v >> 9) & 511, (v >> 18) & 511) * exp2(float(v >> 27) - 24.0);
}
uint pack(float3 c) {
    c = clamp(c, 0.0, 65408.0);
    float most = max(c.r, max(c.g, c.b));
    int e = clamp(int(floor(log2(max(most, 1e-9)))) + 16, 0, 31);
    float scale = exp2(float(24 - e));
    if (most * scale >= 511.5) { e = min(e + 1, 31); scale = exp2(float(24 - e)); }
    uint3 m = min(uint3(c * scale + 0.5), 511u);
    return m.r | (m.g << 9) | (m.b << 18) | (uint(e) << 27);
}
[numthreads(8, 8, 1)]
void main(uint3 at : SV_DispatchThreadID) {
    if (at.x >= width || at.y >= height) return;
    // Without rays the resolve's inputs are unwritten and hold whatever
    // their memory had; replace with a constant indirect diffuse floor.
    light[at.xy] = pack(tint * level);
}
"#;

/// Views, used in turn: more than the passes of every list in flight.
const SLOTS: u32 = 1024;

struct Pass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    views: ID3D12DescriptorHeap,
    step: u32,
    next: AtomicU32,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pass {}
// SAFETY: as above.
unsafe impl Sync for Pass {}

fn make(device: &ID3D12Device) -> windows::core::Result<Pass> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("ambient"),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    let text = |b: &ID3DBlob| {
        // SAFETY: the blob's own bytes.
        unsafe { std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()) }
            .to_vec()
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            tracing::warn!("ambient light: {}", String::from_utf8_lossy(&text(&errors)));
        }
        return Err(e);
    }
    let code = text(&code.ok_or_else(windows::core::Error::empty)?);
    let root = root_signature(device)?;
    // SAFETY: plain creations.
    unsafe {
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.as_ptr().cast(),
                BytecodeLength: code.len(),
            },
            ..Default::default()
        })?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        let views = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: kind,
            NumDescriptors: SLOTS,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })?;
        Ok(Pass {
            root,
            pso,
            views,
            step: device.GetDescriptorHandleIncrementSize(kind),
            next: AtomicU32::new(0),
        })
    }
}

/// The pass's root signature: the level, tint and the image's size (six
/// constants at b0), and a table of one UAV at u0, as the string
/// `RootConstants(num32BitConstants=6, b0), DescriptorTable(UAV(u0))` would
/// make it. Serialized here because Wine's compiler ignores a
/// `[RootSignature]` in the source, and the blob it gives holds none.
fn root_signature(device: &ID3D12Device) -> windows::core::Result<ID3D12RootSignature> {
    let range = [D3D12_DESCRIPTOR_RANGE1 {
        RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
        NumDescriptors: 1,
        BaseShaderRegister: 0,
        RegisterSpace: 0,
        Flags: D3D12_DESCRIPTOR_RANGE_FLAG_NONE,
        OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
    }];
    let table = D3D12_ROOT_PARAMETER1 {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
        Anonymous: D3D12_ROOT_PARAMETER1_0 {
            DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 { NumDescriptorRanges: 1, pDescriptorRanges: range.as_ptr() },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    };
    crate::host::root_signature(device, &[crate::host::root_constants(0, 6), table], "ambient light")
}

fn uav_barrier(list: &ID3D12GraphicsCommandList) {
    let barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        ..Default::default()
    };
    // SAFETY: an open list; the barrier names no resource.
    unsafe { list.ResourceBarrier(&[barrier]) };
}

/// Replaces what `light` holds with the ambient floor, in `list`, after the
/// program that wrote it as an image. Returns whether it recorded anything
/// (the list's descriptor heaps, root signature, and pipeline are then ours).
pub(crate) fn raise(list: &HostList, light: This) -> bool {
    let (Some(level), Some(host)) = (level(), host()) else {
        return false;
    };
    let Some(light) = crate::resource::host_resource(light) else {
        return false;
    };
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    let Some(pass) = PASS
        .get_or_init(|| {
            make(&host.device)
                .inspect_err(|e| tracing::warn!(error = %e, "no ambient light"))
                .ok()
        })
        .as_ref()
    else {
        return false;
    };
    // SAFETY: a live resource.
    let d = unsafe { light.GetDesc() };
    static ONCE: std::sync::Once = std::sync::Once::new();
    if d.Format != DXGI_FORMAT_R32_UINT {
        ONCE.call_once(|| tracing::warn!(format = d.Format.0, "no ambient light: another format"));
        return false;
    }
    ONCE.call_once(|| {
        tracing::info!(level, width = d.Width, height = d.Height, "ambient light");
    });
    list.keep(light);
    let t = tint();
    record(&host.device, pass, &list.list, light, (d.Width as u32, d.Height), [level, t[0], t[1], t[2]]);
    true
}

/// Records the pass over `light`, a `width` by `height` texture of 32-bit
/// integers in the unordered-access state, into `l`: every texel gets the
/// level times the tint (`constants`), packed.
fn record(device: &ID3D12Device, pass: &Pass, l: &ID3D12GraphicsCommandList, light: &ID3D12Resource, (width, height): (u32, u32), constants: [f32; 4]) {
    let slot = pass.next.fetch_add(1, Ordering::Relaxed) % SLOTS;
    uav_barrier(l);
    // SAFETY: an open list; a live texture the list keeps, in the
    // unordered-access state as the program before left it; a view in a slot
    // no list in flight reads.
    unsafe {
        let cpu = pass.views.GetCPUDescriptorHandleForHeapStart();
        let gpu = pass.views.GetGPUDescriptorHandleForHeapStart();
        let at = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: cpu.ptr + (slot * pass.step) as usize,
        };
        let view = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        device.CreateUnorderedAccessView(light, None::<&ID3D12Resource>, Some(&view), at);
        l.SetDescriptorHeaps(&[Some(pass.views.clone())]);
        l.SetComputeRootSignature(&pass.root);
        l.SetPipelineState(&pass.pso);
        let words = [constants[0].to_bits(), constants[1].to_bits(), constants[2].to_bits(), constants[3].to_bits(), width, height];
        l.SetComputeRoot32BitConstants(0, words.len() as u32, words.as_ptr().cast(), 0);
        l.SetComputeRootDescriptorTable(
            1,
            D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: gpu.ptr + u64::from(slot * pass.step),
            },
        );
        l.Dispatch(width.div_ceil(8), height.div_ceil(8), 1);
    }
    uav_barrier(l);
}

#[cfg(test)]
mod tests {
    /// The pass compiles with the system's compiler and the host takes it.
    #[test]
    fn ambient_pass_builds() {
        let Some(host) = crate::host::host() else {
            return;
        };
        super::make(&host.device).expect("the ambient pass");
    }

    /// The pass writes every texel of an image of 32-bit integers with the
    /// level times the tint as R9G9B9E5, on the host GPU (under Wine with
    /// Wine's compiler too: the root signature is made in code).
    #[test]
    fn ambient_pass_fills_the_light_image() {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::*;
        let Some(host) = crate::host::host() else {
            return;
        };
        let pass = super::make(&host.device).expect("the ambient pass");
        let (width, height) = (20u32, 12u32);
        let mut light: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a texture of ours.
        unsafe {
            host.device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: u64::from(width),
                    Height: height,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: DXGI_FORMAT_R32_UINT,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                None,
                &mut light,
            )
        }
        .expect("light image");
        let light = light.expect("light image");
        const PITCH: u32 = 256;
        let back = host.read_back_buffer(u64::from(PITCH * height)).expect("read-back buffer");
        let list = host.new_list().expect("command list");
        let (level, tint) = (0.35f32, [0.94f32, 0.97, 1.0]);
        super::record(&host.device, &pass, &list.list, &light, (width, height), [level, tint[0], tint[1], tint[2]]);
        // SAFETY: an open list; live resources in the states named.
        unsafe {
            list.list.ResourceBarrier(&[crate::host::transition(&light, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&back),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: DXGI_FORMAT_R32_UINT, Width: width, Height: height, Depth: 1, RowPitch: PITCH },
                    },
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&light),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        }
        list.close();
        host.execute(&[&list]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: a read-back buffer the list has written and the host has
        // waited for (`execute`).
        unsafe { back.Map(0, None, Some(&mut mapped)) }.expect("mapped");
        // SAFETY: the mapped rows.
        let texels: Vec<u32> = (0..height)
            .flat_map(|y| (0..width).map(move |x| (y, x)))
            .map(|(y, x)| unsafe { mapped.cast::<u32>().add((y * PITCH / 4 + x) as usize).read() })
            .collect();
        // SAFETY: mapped above.
        unsafe { back.Unmap(0, None) };
        let first = texels[0];
        assert!(texels.iter().all(|&t| t == first), "every texel the same");
        // R9G9B9E5: nine bits a channel, a shared exponent biased by 24.
        let scale = 2f32.powi((first >> 27) as i32 - 24);
        let got = [first & 511, (first >> 9) & 511, (first >> 18) & 511].map(|m| m as f32 * scale);
        for (c, (got, want)) in got.iter().zip(tint.map(|t| t * level)).enumerate() {
            assert!((got - want).abs() <= want / 256.0, "channel {c}: {got} for {want} ({first:#x})");
        }
        list.reset();
    }
}
