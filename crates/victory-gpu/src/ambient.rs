//! A stand-in for the indirect light the title traces rays for.
//!
//! With the ray-tracing structure builder off (`crate::pipeline::builder_runs`)
//! the title's rays find nothing. Its program that resolves the diffuse light
//! of its screen probes (`CS_ScreenProbes_TemporalResolve_*`) then leaves the
//! sky's light outdoors, nothing indoors (walls are black where no lamp or
//! sun reaches) and, in blocks of the picture's tiles, what the probes'
//! memory held. Right after that program the light is raised to an estimate
//! and bounded by a multiple of it.
//!
//! That a stand-in can be put there, and in which of the program's images,
//! is beloooo's find (VIctory-Belobug-AMD, 06031237, 4f45ab88, cd9f78ce,
//! GPL-3.0-or-later), which puts a constant there, shaded in creases from the
//! scene's depth. The estimate here is worked out from the frame instead, so
//! it follows the hour, the room and its lamps by itself: the light the
//! visible surfaces send out (the title keeps it for its own screen traces:
//! `CS_CacheScreenSpaceRadiance`, the frame before) is averaged in 64x36
//! cells, and every point gets the mean of the cells weighted by the share of
//! its surroundings they cover, as a directional value (first-order harmonics
//! at 160x90 points, taken between them by depth).
//!
//! What it assumes: what is not on screen sends out what is (with a slow
//! mean of the last seconds, so that turning into a dark corner is not black
//! at once). No occlusion but the creases'.
//!
//! `VICTORY_AMBIENT`:
//! - unset or `0`/`off`: the title's own output.
//! - a number: that share of the estimate (1: all of it).
//! - `constant:<level>`: the constant, as beloooo's fork has it.
//!
//! `VICTORY_AMBIENT_SHADE=<strength>,<radius in metres>` sets the creases.
//!
//! The air of a room is lit by the same probes: without rays they hold the
//! sky's light indoors too, a brown veil at sunset. `VICTORY_AMBIENT_AIR`
//! bounds what the air scatters by a multiple of the light around.
//!
//! Reflections are traced too, at half the picture's size: rays the
//! title's screen-space march resolves (`CS_ResolveScreenSpace`) take the
//! light seen on screen; the others go to the structures, find nothing, and
//! `CS_RayMiss` gives each the sky's light from the sky's cube maps (in a
//! cutscene frame, capture haze2: 10 to 30 against 0.01 to 0.1 for the room
//! the eyes and floors are in, and blue). `VICTORY_AMBIENT_SPECULAR` puts in,
//! right after that program, the same estimate taken along the reflected
//! direction ([`Role::Miss`]); the screen-space resolve that comes next
//! still writes what it found over it. (readmet3xt-lab/foxhound 2db59d4,
//! f89b79d, d226814, bda6e59, 9b91d27, 9f6fb77, beside this tree's diffuse
//! stand-in: its own switch, its own gate, its own pass.)

use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::s;

use crate::host::{HostList, host};

/// Programs of the title the stand-in follows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Role {
    /// Resolves the diffuse light.
    Resolve,
    /// Keeps the light visible surfaces send out.
    Radiance,
    /// Adds the lamps' light to what the air scatters (the last program
    /// that writes that volume before it is summed up along the view).
    Air,
    /// Gives the reflection rays that found nothing the sky's light
    /// (`CS_RayMiss`, not `CS_RayMiss_Volumetrics` nor the screen probes').
    /// Named only with `VICTORY_AMBIENT_SPECULAR` set.
    Miss,
}

/// The program's part in this, by its name in the container.
pub(crate) fn role(container: &[u8]) -> Option<Role> {
    let has = |name: &[u8]| container.windows(name.len()).any(|w| w == name);
    if has(b"CS_ScreenProbes_TemporalResolve") {
        Some(Role::Resolve)
    } else if has(b"CS_CacheScreenSpaceRadiance") {
        Some(Role::Radiance)
    } else if has(b"CS_AccumulateLocalLightInscatteringVolume") {
        Some(Role::Air)
    } else if specular().is_some() && is_miss(container) {
        Some(Role::Miss)
    } else {
        None
    }
}

/// `CS_RayMiss_` and the eight hex digits of a variant's hash.
fn is_miss(container: &[u8]) -> bool {
    const NAME: &[u8] = b"CS_RayMiss_";
    container.windows(NAME.len() + 9).any(|w| {
        w.starts_with(NAME) && w[NAME.len()..NAME.len() + 8].iter().all(u8::is_ascii_hexdigit) && w[NAME.len() + 8] == b'_'
    })
}

/// Where the resolve writes the diffuse light (R9G9B9E5 as 32-bit integers)
/// and reads the scene's depth, and where the other writes the light of
/// visible surfaces: root parameter and place in its table, in dwords.
pub(crate) const LIGHT: (u8, u32) = (1, 176);
pub(crate) const DEPTH: (u8, u32) = (0, 464);
pub(crate) const RADIANCE: (u8, u32) = (1, 16);
/// Where the light the air scatters is.
pub(crate) const AIR: (u8, u32) = (1, 32);
/// Where the miss program writes the reflected light (R11G11B10, half the
/// picture's size) and where it reads the depth it is given (a 32-bit depth
/// of the picture's size).
pub(crate) const MISS_LIGHT: (u8, u32) = (1, 104);
pub(crate) const MISS_DEPTH: (u8, u32) = (0, 1000);
/// The miss program's first root constant: which surfaces its rays are of.
pub(crate) const MISS_OPAQUE: u32 = 0;
pub(crate) const MISS_ALPHA: u32 = 1;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Set {
    Off,
    Estimate(f32),
    Constant(f32),
}

fn parse(set: &str) -> Set {
    let set = set.trim();
    let number = |t: &str| t.trim().parse::<f32>().ok().filter(|v| v.is_finite() && *v > 0.0);
    match set.strip_prefix("constant:") {
        Some(level) => number(level).map_or(Set::Off, Set::Constant),
        None => number(set).map_or(Set::Off, Set::Estimate),
    }
}

fn set() -> Set {
    static SET: OnceLock<Set> = OnceLock::new();
    *SET.get_or_init(|| match parse(&std::env::var("VICTORY_AMBIENT").unwrap_or_default()) {
        // With the band asked for and no share, the band's level.
        Set::Off => rt_band().map_or(Set::Off, |b| Set::Estimate(b.level)),
        set => set,
    })
}

/// With ray tracing (`VICTORY_AMBIENT_RT=<least>,<most>[,<level>]`; unset:
/// off, the title's own light whenever the builder runs): the diffuse light
/// the title's resolve leaves is kept between `least` and `most` times the
/// estimate (level: the share of the estimate when `VICTORY_AMBIENT` is not
/// set, 0.8). The title's traced light flickers and goes black indoors while
/// the structures its rays walk are partly wrong (reports/RT-FLICKER.md): its
/// detail stays inside the band, its swings and black rooms do not.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Band {
    least: f32,
    most: f32,
    level: f32,
}

pub(crate) fn rt_band() -> Option<Band> {
    static SET: OnceLock<Option<Band>> = OnceLock::new();
    *SET.get_or_init(|| {
        let band = parse_band(&std::env::var("VICTORY_AMBIENT_RT").ok()?)?;
        tracing::info!(least = band.least, most = band.most, level = band.level, "the stand-in keeps the traced indirect light in a band (VICTORY_AMBIENT_RT)");
        Some(band)
    })
}

fn parse_band(set: &str) -> Option<Band> {
    let mut parts = set.split(',').map(|p| p.trim().parse::<f32>().ok().filter(|v| v.is_finite() && *v >= 0.0));
    let least = parts.next().flatten()?;
    let most = parts.next().flatten().unwrap_or(4.0).max(least).max(1e-3);
    let level = parts.next().flatten().filter(|v| *v > 0.0).unwrap_or(0.8);
    Some(Band { least, most, level })
}

/// Whether the stand-in is on: set, and the title's rays find nothing (or
/// the band is asked for with them).
pub(crate) fn on() -> bool {
    set() != Set::Off && (rt_band().is_some() || !crate::pipeline::builder_runs())
}

/// `VICTORY_AMBIENT_SPECULAR=<share>[,<sky>]`: reflections whose rays found
/// nothing take `share` of the estimate along the reflected direction
/// instead of the sky's light; `sky` (0 to 1, 0 if not given) of them stays
/// the title's sky, made no brighter than the estimate. Unset or 0: the
/// title's sky.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Specular {
    share: f32,
    sky: f32,
}

fn parse_specular(set: &str) -> Option<Specular> {
    let mut parts = set.split(',').map(|t| t.trim().parse::<f32>().ok().filter(|v| v.is_finite()));
    let share = parts.next().flatten().filter(|v| *v > 0.0)?;
    let sky = parts.next().flatten().map_or(0.0, |v| v.clamp(0.0, 1.0));
    Some(Specular { share, sky })
}

fn specular() -> Option<Specular> {
    static SET: OnceLock<Option<Specular>> = OnceLock::new();
    *SET.get_or_init(|| parse_specular(&std::env::var("VICTORY_AMBIENT_SPECULAR").unwrap_or_default()))
}

/// Whether the stand-in for reflections is on: set, and the title's rays
/// find nothing. Whatever `VICTORY_AMBIENT` says ([`on`] is the diffuse
/// stand-in's).
pub(crate) fn specular_on() -> bool {
    specular().is_some() && !crate::pipeline::builder_runs()
}

/// Whether the stand-in follows a dispatch of the program of `role`: the
/// diffuse stand-in's programs when [`on`]; the miss program when
/// [`specular_on`]; and the keeping of the light ([`Role::Radiance`]) for
/// either, as the estimate of both starts from it.
pub(crate) fn follows(role: Role) -> bool {
    match role {
        Role::Resolve | Role::Air => on(),
        Role::Radiance => on() || specular_on(),
        Role::Miss => specular_on(),
    }
}

/// The alpha layer's reflections too (`VICTORY_AMBIENT_SPECULAR_ALPHA=1`):
/// its pixels are of transparent surfaces (glass, foam, wakes on water) and
/// the depth read is the opaque one behind them; outdoors on water that is
/// the bottom, the reflected direction is wrong and the estimate dark (play
/// session 2026-11-27 of foxhound 9f6fb77: dark patches on the water around
/// a boat). Unset: opaque surfaces only.
pub(crate) fn specular_alpha() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_AMBIENT_SPECULAR_ALPHA").is_ok_and(|v| v.trim() == "1"))
}

/// The root constants of the passes: 17, and with the stand-in for
/// reflections two more (the size of its image, [`Role::Miss`]'s own).
fn constants() -> u32 {
    if specular().is_some() { 19 } else { 17 }
}

/// The title's leftover light is kept between these multiples of the
/// estimate: with rays, the band's; without, at least the estimate and at
/// most four times it.
fn bounds() -> (f32, f32) {
    match rt_band() {
        Some(b) if crate::pipeline::builder_runs() => (b.least, b.most),
        _ => (1.0, 4.0),
    }
}

/// The most the air scatters, as a multiple of the light around
/// (`VICTORY_AMBIENT_AIR=<multiple>`; unset or 0: what the title has).
fn air() -> f32 {
    static SET: OnceLock<f32> = OnceLock::new();
    *SET.get_or_init(|| {
        let set = std::env::var("VICTORY_AMBIENT_AIR").unwrap_or_default();
        set.trim().parse().ok().filter(|v: &f32| v.is_finite() && *v > 0.0).unwrap_or(0.0)
    })
}

/// How the slow mean of the light around follows the frames
/// (`VICTORY_AMBIENT_SETTLE=<time constant in ms>[,<jump factor>[,<fresh
/// seconds>]]`; unset: 250,3; 0: 0.04 of the way a frame, as before, which is 25
/// frames: 0.8 s at the console's 30 frames a second, 2-4 s at 7-13). Set,
/// it is the same share of the way a second whatever the frame rate, and a
/// change by more than the jump factor (default 3; 0: none) is taken at
/// once. Fresh seconds (default 0: none): for that long after such a jump
/// the light the title's own resolve left is bounded by the estimate, not
/// four times it, rising back to four times by the end.
#[derive(Clone, Copy)]
struct Settle {
    ms: f32,
    jump: f32,
    fresh: f32,
}

fn settle() -> Option<Settle> {
    static SET: OnceLock<Option<Settle>> = OnceLock::new();
    *SET.get_or_init(|| {
        // Default on (250 ms, jump factor 3: reports/LIGHT-POPIN.md); `0`
        // gives the old 0.04 of the way a frame.
        let set = std::env::var("VICTORY_AMBIENT_SETTLE").unwrap_or_else(|_| "250,3".to_owned());
        let mut parts = set.split(',').map(|p| p.trim().parse::<f32>().ok());
        let ms = parts.next().flatten().filter(|v| v.is_finite() && *v > 0.0)?;
        let jump = parts.next().flatten().filter(|v| v.is_finite() && *v >= 0.0).unwrap_or(3.0);
        let fresh = parts.next().flatten().filter(|v| v.is_finite() && *v >= 0.0).unwrap_or(0.0);
        tracing::info!(
            ms,
            jump,
            fresh,
            "the stand-in's mean of the light around settles by the clock (VICTORY_AMBIENT_SETTLE)"
        );
        Some(Settle { ms, jump, fresh })
    })
}

/// The share of the way the slow mean goes at this frame's keeping
/// ([`Role::Radiance`]; the other passes do not read it).
fn follow(role: Role) -> f32 {
    match settle() {
        Some(settle) if role == Role::Radiance => 1.0 - (-frame_ms(role) / settle.ms).exp(),
        _ => 0.04,
    }
}

/// Milliseconds since the last keeping ([`Role::Radiance`], once a frame);
/// the first, and after a pause of over a second, as a frame of 30. Other
/// roles: the last answer.
fn frame_ms(role: Role) -> f32 {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
    static LAST: AtomicU64 = AtomicU64::new(0);
    static DT: AtomicU32 = AtomicU32::new(0x4204_0000); // 33.0
    if role != Role::Radiance {
        return f32::from_bits(DT.load(Relaxed));
    }
    // (Asked twice in a keeping: the words and the share.)
    static ASKED: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    if ASKED.swap(now, Relaxed) == now {
        return f32::from_bits(DT.load(Relaxed));
    }
    let last = LAST.swap(now, Relaxed);
    let dt = match now.saturating_sub(last) {
        0 => 1.0,
        dt if last == 0 || dt > 1000 => 33.0,
        dt => dt as f32,
    };
    DT.store(dt.to_bits(), Relaxed);
    dt
}

/// Strength and radius of the creases' shade.
fn shade() -> [f32; 2] {
    static SET: OnceLock<[f32; 2]> = OnceLock::new();
    *SET.get_or_init(|| {
        let mut shade = [1.0, 0.6];
        let set = std::env::var("VICTORY_AMBIENT_SHADE").unwrap_or_default();
        for (to, v) in shade.iter_mut().zip(set.split(',')) {
            if let Ok(v) = v.trim().parse() {
                *to = v;
            }
        }
        shade
    })
}

const SOURCE: &str = r#"
#ifdef SPECULAR
#define RS "RootConstants(num32BitConstants=19, b0), DescriptorTable(SRV(t0)), DescriptorTable(UAV(u0)), UAV(u1), UAV(u2), UAV(u3)"
#else
#define RS "RootConstants(num32BitConstants=17, b0), DescriptorTable(SRV(t0)), DescriptorTable(UAV(u0)), UAV(u1), UAV(u2), UAV(u3)"
#endif
#define CX 64
#define CY 36
#define CELLS (CX * CY)
#define PX 160
#define PY 90
cbuffer Set : register(b0) {
    float level; float constant; float strength; float radius;
    float near; float tan_y; uint width; uint height;
    float follow; float most; float air; uint probe;
    float jump; float fresh; float dt;
    // The image's slices (`width` and `height` are its size): what
    // `GetDimensions` of the writable image gives, in the form for Wine's
    // compiler (WINE_HLSL), which has no such method on it. That form
    // also tests floats for NaN and infinity on their bits (no isnan or
    // isinf there).
    uint slices;
    // The least the title's leftover light is kept at, times the estimate
    // (`VICTORY_AMBIENT_RT`; 1 without it).
    float least;
#ifdef SPECULAR
    // The size of the reflections' image (`Role::Miss`: `width` and
    // `height` are its depth's, twice that), for Wine's compiler.
    uint image_width; uint image_height;
#endif
};
Texture2D<float> depth : register(t0);
// Cells: the light they send out (then, at CELLS, the slow mean), from
// CELLS + 1 on where they are.
RWStructuredBuffer<float4> cells : register(u1);
// Three a point: red, green and blue, each a mean and a direction.
RWStructuredBuffer<float4> points : register(u2);
// For the light probe (`crate::lightprobe`): sampled pixels, and the sums of
// what the title's resolve left, what was found and what was written. In
// words: Wine's compiler has Interlocked* only as functions on a UAV's
// element, not as methods of a byte address buffer.
RWStructuredBuffer<uint> stats : register(u3);
static const float3 LUMA = float3(0.2126, 0.7152, 0.0722);

// The point seen at a texel, in metres from the eye (x right, y up, z away).
float3 seen(int2 p, float2 size) {
    float d = depth.Load(int3(clamp(p, int2(0, 0), int2(size) - 1), 0));
    float z = min(near / max(d, 1e-7), 1000.0);
    float2 ndc = (float2(p) + 0.5) / size * 2.0 - 1.0;
    return float3(ndc.x * tan_y * size.x / size.y * z, -ndc.y * tan_y * z, z);
}

#ifdef KEEP
RWTexture2D<float3> radiance : register(u0);
[RootSignature(RS)]
[numthreads(8, 4, 1)]
void keep(uint3 at : SV_DispatchThreadID) {
    uint w, h;
#ifdef WINE_HLSL
    w = width;
    h = height;
#else
    radiance.GetDimensions(w, h);
#endif
    float3 sum = 0;
    for (uint y = 0; y < 5; y++) for (uint x = 0; x < 5; x++) {
        uint2 p = uint2((float2(at.xy) + (float2(x, y) + 0.5) / 5.0) * float2(w, h) / float2(CX, CY));
        float3 c = max(radiance[p], 0.0);
#ifdef WINE_HLSL
        // (Wine's compiler has no isnan or isinf: the same tests on the
        // bits, all exponent bits set and the mantissa not zero or zero.)
        uint3 bits = asuint(c) & 0x7fffffffu;
        c = any(bits > 0x7f800000u) || any(bits == 0x7f800000u) ? 0.0 : c;
#else
        c = any(isnan(c)) || any(isinf(c)) ? 0.0 : c;
#endif
        // Lamps and glints count, but not for everything.
        sum += c / max(1.0, dot(c, LUMA) / 20.0);
    }
    cells[at.y * CX + at.x] = float4(sum / 25.0, 1.0);
}
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void mean() {
    float3 sum = 0;
    for (uint i = 0; i < CELLS; i++) sum += cells[i].rgb;
    float4 was = cells[CELLS];
    float3 now = sum / CELLS;
    // (`jump` > 0: a change of more than that factor either way is
    // followed at once, as a new room is.)
    float a = max(dot(now, LUMA), 1e-5), b = max(dot(was.rgb, LUMA), 1e-5);
    bool jumped = jump > 0.0 && max(a / b, b / a) > jump;
    // Its fourth part: seconds since the light around last jumped (from
    // 0.001; 0: nothing kept yet).
    float age = was.a > 0.0 && !jumped ? was.a + dt : 0.001;
    cells[CELLS] = float4(was.a > 0.0 ? lerp(was.rgb, now, jumped ? 1.0 : follow) : now, age);
}
#endif

#ifdef PLACE
[RootSignature(RS)]
[numthreads(8, 4, 1)]
void place(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    float3 p = seen(int2((float2(at.xy) + 0.5) * size / float2(CX, CY)), size);
    cells[CELLS + 1 + at.y * CX + at.x] = float4(p, p.z * p.z);
}
#endif

#ifdef GATHER
[RootSignature(RS)]
[numthreads(8, 2, 1)]
void gather(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    float3 p = seen(int2((float2(at.xy) + 0.5) * size / float2(PX, PY)), size);
    float r2 = 0.25 + 0.0025 * p.z * p.z;
    float whole = 0;
    float3 toward = 0, light = 0, red = 0, green = 0, blue = 0;
    for (uint i = 0; i < CELLS; i++) {
        float4 c = cells[CELLS + 1 + i];
        float3 L = cells[i].rgb;
        float3 d = c.xyz - p;
        float dd = dot(d, d);
        float w = c.w / (dd + r2);
        float3 dir = d / sqrt(dd + 0.25 * r2);
        whole += w;
        toward += w * dir;
        light += w * L;
        red += w * L.r * dir;
        green += w * L.g * dir;
        blue += w * L.b * dir;
    }
    whole = max(whole, 1e-6);
    light /= whole;
    toward /= whole;
    uint at0 = (at.y * PX + at.x) * 3;
    points[at0] = float4(red / whole - light.r * toward, light.r);
    points[at0 + 1] = float4(green / whole - light.g * toward, light.g);
    points[at0 + 2] = float4(blue / whole - light.b * toward, light.b);
}
#endif

#ifdef AIR
RWTexture3D<float3> scattered : register(u0);
// No more than a multiple of the light around: of the room as the last
// seconds showed it, and of the surface seen in this direction (towards a
// window the air outside may be as bright as the sky is).
[RootSignature(RS)]
[numthreads(8, 8, 2)]
void clear_air(uint3 at : SV_DispatchThreadID) {
    uint w, h, d;
#ifdef WINE_HLSL
    w = width;
    h = height;
    d = slices;
#else
    scattered.GetDimensions(w, h, d);
#endif
    if (at.x >= w || at.y >= h || at.z >= d) return;
    uint2 q = min(uint2((float2(at.xy) + 0.5) / float2(w, h) * float2(PX, PY)), uint2(PX - 1, PY - 1));
    uint at0 = (q.y * PX + q.x) * 3;
    float3 seen_there = float3(points[at0].w, points[at0 + 1].w, points[at0 + 2].w);
    float far = float(at.z) / float(d);
    float cap = air * lerp(dot(cells[CELLS].rgb, LUMA), dot(seen_there, LUMA), far);
    float3 c = scattered[at];
    float has = dot(c, LUMA);
    if (has > cap) scattered[at] = c * (cap / has);
}
#endif

#ifdef REFLECT
RWTexture2D<float3> reflected : register(u0);
float3 finite_or_zero(float3 c) {
#ifdef WINE_HLSL
    uint3 bits = asuint(c) & 0x7fffffffu;
    return any(bits > 0x7f800000u) || any(bits == 0x7f800000u) ? 0.0 : c;
#else
    return any(isnan(c)) || any(isinf(c)) ? 0.0 : c;
#endif
}
// Each texel of the reflections' image (half the picture's size): the light
// the points around send along the reflected direction, as `raise` takes it
// along the normal; `level` is the share, `constant` the share of the
// title's sky kept, made no brighter than that.
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void reflect_light(uint3 at : SV_DispatchThreadID) {
    uint w, h;
#ifdef WINE_HLSL
    w = image_width;
    h = image_height;
#else
    reflected.GetDimensions(w, h);
#endif
    if (at.x >= w || at.y >= h) return;
    float2 size = float2(width, height);
    int2 p = int2((float2(at.xy) + 0.5) * size / float2(w, h));
    float3 c = seen(p, size);
    // Nothing is reflected by the sky.
    if (c.z >= 999.0) return;
    float3 l = seen(p + int2(-1, 0), size), r = seen(p + int2(1, 0), size);
    float3 u = seen(p + int2(0, -1), size), d = seen(p + int2(0, 1), size);
    float3 dx = abs(l.z - c.z) < abs(r.z - c.z) ? c - l : r - c;
    float3 dy = abs(u.z - c.z) < abs(d.z - c.z) ? c - u : d - c;
    float3 n = normalize(cross(dy, dx) + float3(0, 0, -1e-9));
    if (dot(n, c) > 0.0) n = -n;
    float3 v = normalize(c);
    float3 ray = v - 2.0 * dot(v, n) * n;
    float2 g = (float2(p) + 0.5) / size * float2(PX, PY) - 0.5;
    int2 g0 = int2(floor(g));
    float2 f = g - float2(g0);
    float4 sr = 0, sg = 0, sb = 0;
    float sum = 0;
    for (int k = 0; k < 4; k++) {
        int2 q = clamp(g0 + int2(k & 1, k >> 1), int2(0, 0), int2(PX, PY) - 1);
        float z = seen(int2((float2(q) + 0.5) * size / float2(PX, PY)), size).z;
        float2 b = float2(k & 1 ? f.x : 1 - f.x, k >> 1 ? f.y : 1 - f.y);
        float wgt = (b.x * b.y + 1e-3) / (0.02 + abs(z - c.z) / c.z);
        uint at0 = (q.y * PX + q.x) * 3;
        sr += wgt * points[at0];
        sg += wgt * points[at0 + 1];
        sb += wgt * points[at0 + 2];
        sum += wgt;
    }
    float3 all = float3(sr.w, sg.w, sb.w) / sum;
    // Radiance from the first moments: three times the direction's share
    // for an even spread of directions, a little less for a smooth image.
    float3 found = all + 2.0 * float3(dot(sr.xyz, ray), dot(sg.xyz, ray), dot(sb.xyz, ray)) / sum;
    found = clamp(found, 0.1 * all, 3.0 * all);
    found = lerp(found, cells[CELLS].rgb, 0.2) * level;
    found = max(finite_or_zero(found), 0.0);
    float3 was = reflected[at.xy];
    was = max(finite_or_zero(was), 0.0);
    float has = dot(was, LUMA);
    was *= min(1.0, dot(found, LUMA) / max(has, 1e-6));
    reflected[at.xy] = lerp(found, was, constant);
}
#endif

#ifdef RAISE
RWTexture2D<uint> light : register(u0);
float3 unpack(uint v) {
    return float3(v & 511, (v >> 9) & 511, (v >> 18) & 511) * exp2(float(v >> 27) - 24.0);
}
uint pack(float3 c) {
    c = clamp(c, 0.0, 65408.0);
    float top = max(c.r, max(c.g, c.b));
    int e = clamp(int(floor(log2(max(top, 1e-9)))) + 16, 0, 31);
    float scale = exp2(float(24 - e));
    if (top * scale >= 511.5) { e = min(e + 1, 31); scale = exp2(float(24 - e)); }
    uint3 m = min(uint3(c * scale + 0.5), 511u);
    return m.r | (m.g << 9) | (m.b << 18) | (uint(e) << 27);
}
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void raise(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    if (at.x >= width || at.y >= height) return;
    int2 p = int2(at.xy);
    float3 c = seen(p, size);
    float3 l = seen(p + int2(-1, 0), size), r = seen(p + int2(1, 0), size);
    float3 u = seen(p + int2(0, -1), size), d = seen(p + int2(0, 1), size);
    float3 dx = abs(l.z - c.z) < abs(r.z - c.z) ? c - l : r - c;
    float3 dy = abs(u.z - c.z) < abs(d.z - c.z) ? c - u : d - c;
    float3 n = normalize(cross(dy, dx) + float3(0, 0, -1e-9));
    if (dot(n, c) > 0.0) n = -n;
    // How open it is (as in beloooo's cd9f78ce).
    float open = 1.0;
    float px = min(radius * size.y / (2.0 * tan_y * c.z), 128.0);
    float noise = frac(52.9829189 * frac(dot(float2(p), float2(0.06711056, 0.00583715))));
    if (strength > 0.0 && px >= 1.5 && c.z < 400.0) {
        float sum = 0.0;
        const int N = 12;
        for (int i = 0; i < N; i++) {
            float a = i * 2.399963 + noise * 6.2831853;
            float2 o = float2(cos(a), sin(a)) * (sqrt((i + 0.5) / N) * px);
            float3 v = seen(p + int2(round(o)), size) - c;
            float vv = dot(v, v);
            float over = dot(v, n) - 0.004 * c.z - 0.005;
            sum += max(over, 0.0) / sqrt(vv + 1e-6) * saturate(1.0 - vv / (radius * radius));
        }
        open = saturate(1.0 - strength * 2.0 * sum / N);
    }
    float3 found;
    if (constant > 0.0) {
        found = float3(1.0, 0.98, 0.94) * constant * open;
        light[p] = pack(found);
        return;
    }
    // Between the four points around, those at its own depth first; which
    // four is dithered a little.
    float2 g = (float2(p) + 0.5) / size * float2(PX, PY) - 0.5 + (noise - 0.5) * 0.5;
    int2 g0 = int2(floor(g));
    float2 f = g - float2(g0);
    float4 sr = 0, sg = 0, sb = 0;
    float sum = 0;
    for (int k = 0; k < 4; k++) {
        int2 q = clamp(g0 + int2(k & 1, k >> 1), int2(0, 0), int2(PX, PY) - 1);
        float z = seen(int2((float2(q) + 0.5) * size / float2(PX, PY)), size).z;
        float2 b = float2(k & 1 ? f.x : 1 - f.x, k >> 1 ? f.y : 1 - f.y);
        float w = (b.x * b.y + 1e-3) / (0.02 + abs(z - c.z) / c.z);
        uint at0 = (q.y * PX + q.x) * 3;
        sr += w * points[at0];
        sg += w * points[at0 + 1];
        sb += w * points[at0 + 2];
        sum += w;
    }
    float3 all = float3(sr.w, sg.w, sb.w) / sum;
    found = all + 1.2 * float3(dot(sr.xyz, n), dot(sg.xyz, n), dot(sb.xyz, n)) / sum;
    found = clamp(found, 0.2 * all, 2.5 * all);
    found = lerp(found, cells[CELLS].rgb, 0.2) * level * open;
    float3 was = unpack(light[p]);
    // Filter magenta garbage probes left by uninitialized/corrupted memory on AMD.
    if (was.g < 0.5 * min(was.r, was.b) && max(was.r, was.b) > 0.02) was = 0.0;
    // And the bluer kind of it (blue over 2.5x green and over red): left as
    // a faint violet by the line above. The sky's light keeps green above
    // 0.4x blue, sunsets keep blue low, so neither is touched. (Ahyildiz,
    // VIctory-NVIDIA 826ba15: a purple-blue glow on a lit sheet of paper in
    // the prison cell.)
    if (was.b > 2.5 * was.g && was.b > was.r && was.b > 0.02) was = 0.0;
    float left = dot(was, LUMA);
    // What the program left counts as far as it can be true: the sky's light
    // outdoors, not the blocks of other memory.
    // For `fresh` seconds after the light around jumped (a new room) the
    // title's leftover counts for no more than the estimate, then up to
    // `most` times it again: its history holds the place before.
    float bound = fresh > 0.0 ? lerp(1.0, most, saturate(cells[CELLS].a / fresh)) : most;
    float top = max(dot(found, LUMA) * bound, 1e-6);
    float has = dot(was, LUMA);
    was *= min(1.0, top / max(has, 1e-6));
    float3 written = max(was, found * least);
    light[p] = pack(written);
    if (probe != 0 && (at.x & 15) == 0 && (at.y & 15) == 0) {
        uint ignored;
        InterlockedAdd(stats[0], 1, ignored);
        InterlockedAdd(stats[1], uint(min(left, 1000.0) * 256.0), ignored);
        InterlockedAdd(stats[2], uint(min(dot(found, LUMA), 1000.0) * 256.0), ignored);
        InterlockedAdd(stats[3], uint(min(dot(written, LUMA), 1000.0) * 256.0), ignored);
    }
}
#endif
"#;

/// Bytes of the cells' buffer and of the points'.
const CELLS: u64 = 64 * 36;
const CELL_BYTES: u64 = (2 * CELLS + 1) * 16;
const POINT_BYTES: u64 = 160 * 90 * 48;
/// Where the slow mean is among the cells (float4s).
pub(crate) const CELLS_AT: u32 = CELLS as u32;

struct Pass {
    root: ID3D12RootSignature,
    keep: ID3D12PipelineState,
    mean: ID3D12PipelineState,
    place: ID3D12PipelineState,
    gather: ID3D12PipelineState,
    raise: ID3D12PipelineState,
    air: ID3D12PipelineState,
    /// The reflections' pass, made only with `VICTORY_AMBIENT_SPECULAR` set.
    reflect: Option<ID3D12PipelineState>,
    cells: ID3D12Resource,
    points: ID3D12Resource,
    stats: ID3D12Resource,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pass {}
// SAFETY: as above.
unsafe impl Sync for Pass {}

/// The passes' root signature (the `[RootSignature]` of [`SOURCE`], made in
/// code: Wine's compiler puts none in the blob): the constants (17 at b0),
/// the depth's and the image's tables (t0, u0), the cells, points and
/// stats as root UAVs (u1-u3).
fn root(device: &ID3D12Device, code: &[u8]) -> windows::core::Result<ID3D12RootSignature> {
    use crate::host::{root_constants, root_range, root_table, root_view};
    let srv = [root_range(D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 0, 1)];
    let uav = [root_range(D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 0, 1)];
    let params = [
        root_constants(0, constants()),
        root_table(&srv),
        root_table(&uav),
        root_view(D3D12_ROOT_PARAMETER_TYPE_UAV, 1),
        root_view(D3D12_ROOT_PARAMETER_TYPE_UAV, 2),
        root_view(D3D12_ROOT_PARAMETER_TYPE_UAV, 3),
    ];
    crate::host::root_of(device, code, &params, "indirect light stand-in")
}

fn make(device: &ID3D12Device, buffer: impl Fn(u64) -> Option<ID3D12Resource>) -> windows::core::Result<Pass> {
    let mut root: Option<ID3D12RootSignature> = None;
    let mut pipeline = |part: &str, entry| -> windows::core::Result<ID3D12PipelineState> {
        // A writable image's size from `GetDimensions` where the compiler
        // has it (Microsoft's), else from the constants (Wine's).
        // (With the stand-in for reflections the constants are two more, in
        // every pass; without it the source is as it was.)
        let more = if specular().is_some() { "#define SPECULAR\n" } else { "" };
        let forms = [
            format!("{more}#define {part}\n{SOURCE}"),
            format!("{more}#define {part}\n#define WINE_HLSL\n{SOURCE}"),
        ];
        let (code, form) = crate::host::compile_first(&forms, entry, s!("cs_5_1"))?;
        if form > 0 {
            tracing::info!(part, "the indirect light stand-in takes its image's size from its constants (Wine's compiler)");
        }
        // SAFETY: plain creations.
        unsafe {
            if root.is_none() {
                root = Some(self::root(device, &code)?);
            }
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
                ..Default::default()
            };
            device.CreateComputePipelineState(&desc)
        }
    };
    let keep = pipeline("KEEP", s!("keep"))?;
    let mean = pipeline("KEEP", s!("mean"))?;
    let place = pipeline("PLACE", s!("place"))?;
    let gather = pipeline("GATHER", s!("gather"))?;
    let raise = pipeline("RAISE", s!("raise"))?;
    let air = pipeline("AIR", s!("clear_air"))?;
    let reflect = match specular() {
        Some(_) => Some(pipeline("REFLECT", s!("reflect_light"))?),
        None => None,
    };
    let made = |bytes| buffer(bytes).ok_or_else(windows::core::Error::empty);
    Ok(Pass {
        root: root.ok_or_else(windows::core::Error::empty)?,
        keep,
        mean,
        place,
        gather,
        raise,
        air,
        reflect,
        cells: made(CELL_BYTES)?,
        points: made(POINT_BYTES)?,
        stats: made(256)?,
    })
}

fn pass() -> Option<&'static Pass> {
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let host = host()?;
        make(&host.device, |bytes| host.uav_buffer(bytes))
            .inspect_err(|e| tracing::warn!(error = %e, "no stand-in for the indirect light"))
            .ok()
    })
    .as_ref()
}

/// The probe's sums and the cells (with the slow mean), once the passes are
/// made and the stand-in is on.
pub(crate) fn probe_buffers() -> Option<(ID3D12Resource, ID3D12Resource)> {
    if !on() {
        return None;
    }
    pass().map(|p| (p.stats.clone(), p.cells.clone()))
}

fn uav_barrier(list: &ID3D12GraphicsCommandList) {
    let barrier = D3D12_RESOURCE_BARRIER { Type: D3D12_RESOURCE_BARRIER_TYPE_UAV, ..Default::default() };
    // SAFETY: an open list; the barrier names no resource.
    unsafe { list.ResourceBarrier(&[barrier]) };
}

/// Whether the light of a frame has been kept: the estimate has something
/// to begin with.
static KEPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What the passes read and write, as views in the heap the list has set.
pub(crate) struct Views {
    /// The image the title's program just wrote.
    pub(crate) image: D3D12_GPU_DESCRIPTOR_HANDLE,
    /// The scene's depth, for [`Role::Resolve`].
    pub(crate) depth: D3D12_GPU_DESCRIPTOR_HANDLE,
    /// The size of the image, and how many slices it has.
    pub(crate) size: [u32; 2],
    pub(crate) slices: u32,
    /// The size of the depth: the image's but for [`Role::Miss`], whose
    /// depth is the picture's (twice its image each way). Only that role
    /// reads it.
    pub(crate) depth_size: [u32; 2],
}

/// For the line every ten seconds: what the stand-in for reflections did.
struct Said {
    /// Dispatches of the miss program it followed, frames they were in,
    /// and those it left as they were: of water or the alpha layer, before
    /// a frame's light was kept, and those whose rays' image or picture's
    /// depth was not found (or not of the format and size it takes).
    ran: AtomicU64,
    frames: AtomicU64,
    water: AtomicU64,
    not_ready: AtomicU64,
    not_found: AtomicU64,
    last_frame: AtomicU64,
    /// Bit 0 opaque surfaces, 1 alpha, 2 water, 3 others.
    layers: AtomicU64,
}

static SAID_MISS: Said = Said {
    ran: AtomicU64::new(0),
    frames: AtomicU64::new(0),
    water: AtomicU64::new(0),
    not_ready: AtomicU64::new(0),
    not_found: AtomicU64::new(0),
    last_frame: AtomicU64::new(u64::MAX),
    layers: AtomicU64::new(0),
};

/// What came of a dispatch of the miss program, counted for the line.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum MissNote {
    Ran(u32),
    Water,
    /// Before a frame's light was kept: the estimate has nothing yet.
    NotReady,
    /// The rays' image or the picture's depth not found, or not of the
    /// format and size the pass takes.
    NotFound,
}

/// Counts what came of a dispatch of the miss program; every ten seconds an
/// info line says it, so a run shows that the stand-in ran and where.
pub(crate) fn note_miss(note: MissNote) {
    use AtomicOrdering::Relaxed;
    if specular().is_none() {
        return;
    }
    let s = &SAID_MISS;
    match note {
        MissNote::Ran(layer) => {
            s.ran.fetch_add(1, Relaxed);
            s.layers.fetch_or(1 << layer.min(3), Relaxed);
            let frame = crate::frame::current();
            if s.last_frame.swap(frame, Relaxed) != frame {
                s.frames.fetch_add(1, Relaxed);
            }
        }
        MissNote::Water => {
            s.water.fetch_add(1, Relaxed);
        }
        MissNote::NotReady => {
            s.not_ready.fetch_add(1, Relaxed);
        }
        MissNote::NotFound => {
            s.not_found.fetch_add(1, Relaxed);
        }
    }
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    let Some(since) = *last else {
        *last = Some(now);
        return;
    };
    if now.duration_since(since).as_secs_f32() < 10.0 {
        return;
    }
    *last = Some(now);
    let layers = s.layers.swap(0, Relaxed);
    let named: Vec<&str> = [(1, "opaque"), (2, "alpha"), (4, "water"), (8, "other")]
        .iter()
        .filter(|(bit, _)| layers & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    let Specular { share, sky } = specular().unwrap_or(Specular { share: 0.0, sky: 0.0 });
    tracing::info!(
        "a stand-in for reflections (VICTORY_AMBIENT_SPECULAR={share},{sky}) in the last {:.0} s: \
         {} frames, {} dispatches of CS_RayMiss followed (surfaces: {}), each by an estimate along \
         the reflected direction put in right after it (before CS_ResolveScreenSpace); left as the title had them: \
         {} of water or the alpha layer, {} before a frame's light was kept, {} whose rays' image or picture's \
         depth was not found",
        now.duration_since(since).as_secs_f32(),
        s.frames.swap(0, Relaxed),
        s.ran.swap(0, Relaxed),
        if named.is_empty() { "none".to_owned() } else { named.join(", ") },
        s.water.swap(0, Relaxed),
        s.not_ready.swap(0, Relaxed),
        s.not_found.swap(0, Relaxed),
    );
}

/// After the title's program of that role, in `list` with the program's view
/// heap set. Replaces the list's compute root signature and pipeline; false
/// if it recorded nothing.
pub(crate) fn after(list: &HostList, role: Role, views: &Views) -> bool {
    use std::sync::atomic::Ordering;
    let Some(pass) = pass() else {
        return false;
    };
    let (level, constant) = match (role, set()) {
        // The stand-in for reflections: its share of the estimate, and the
        // share of the title's sky kept (`VICTORY_AMBIENT_SPECULAR`).
        (Role::Miss, _) => match (specular(), &pass.reflect) {
            (Some(s), Some(_)) => (s.share, s.sky),
            _ => return false,
        },
        // The keeping runs for the stand-in for reflections too: its
        // estimate starts from the light kept, as the diffuse one's does.
        (Role::Radiance, Set::Off) if specular().is_some() => (0.0, 0.0),
        (_, Set::Off) => return false,
        (_, Set::Estimate(level)) => (level, 0.0f32),
        (_, Set::Constant(level)) => (0.0, level),
    };
    if role == Role::Miss && !KEPT.load(Ordering::Relaxed) {
        note_miss(MissNote::NotReady);
        return false;
    }
    if role != Role::Radiance && constant == 0.0 && !KEPT.load(Ordering::Relaxed) {
        return false;
    }
    if role == Role::Air && (constant > 0.0 || air() == 0.0) {
        return false;
    }
    static SAID: Mutex<u32> = Mutex::new(0);
    {
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if *said < 4 {
            *said += 1;
            tracing::info!(?role, size = ?views.size, level, constant, "a stand-in for the indirect light");
        }
    }
    list.keep(&pass.cells);
    list.keep(&pass.points);
    list.keep(&pass.stats);
    let [strength, radius] = shade();
    let floats = [level, constant, strength, radius, 0.1, 0.45];
    let mut words = [0u32; 19];
    for (to, v) in words.iter_mut().zip(floats) {
        *to = v.to_bits();
    }
    words[6] = views.size[0];
    words[7] = views.size[1];
    words[8] = follow(role).to_bits();
    let (least, most) = bounds();
    words[9] = most.to_bits();
    words[16] = least.to_bits();
    words[10] = air().to_bits();
    words[11] = u32::from(crate::lightprobe::on());
    words[12] = settle().map_or(0.0, |s| s.jump).to_bits();
    words[13] = settle().map_or(0.0, |s| s.fresh).to_bits();
    words[14] = (frame_ms(role) / 1000.0).to_bits();
    words[15] = views.slices;
    // The stand-in for reflections: `width` and `height` are the size of
    // its depth (the picture's, which `seen` reads), and its image's (half
    // that) is in words of its own. The words the other roles read are
    // theirs as before.
    if role == Role::Miss {
        words[6] = views.depth_size[0];
        words[7] = views.depth_size[1];
        words[17] = views.size[0];
        words[18] = views.size[1];
    }
    let count = constants();
    let l = &list.list;
    // SAFETY: an open list with the heap of the views set; the title's
    // program has just used both views, so their textures are in the states
    // these views need; buffers the list keeps.
    unsafe {
        uav_barrier(l);
        l.SetComputeRootSignature(&pass.root);
        l.SetComputeRoot32BitConstants(0, count, words.as_ptr().cast(), 0);
        l.SetComputeRootDescriptorTable(1, views.depth);
        l.SetComputeRootDescriptorTable(2, views.image);
        #[cfg(feature = "diagnostics")]
        if crate::crumbs::on() {
            for buffer in [&pass.cells, &pass.points, &pass.stats] {
                crate::crumbs::view(l, crate::crumbs::views::OTHER, true, buffer);
            }
        }
        l.SetComputeRootUnorderedAccessView(3, pass.cells.GetGPUVirtualAddress());
        l.SetComputeRootUnorderedAccessView(4, pass.points.GetGPUVirtualAddress());
        l.SetComputeRootUnorderedAccessView(5, pass.stats.GetGPUVirtualAddress());
        let run = |pso: &ID3D12PipelineState, x, y, z| {
            l.SetPipelineState(pso);
            l.Dispatch(x, y, z);
            uav_barrier(l);
        };
        match role {
            Role::Radiance => {
                run(&pass.keep, 8, 9, 1);
                run(&pass.mean, 1, 1, 1);
                KEPT.store(true, Ordering::Relaxed);
            }
            Role::Air => run(&pass.air, views.size[0].div_ceil(8), views.size[1].div_ceil(8), views.slices.div_ceil(2)),
            Role::Resolve => {
                if constant == 0.0 {
                    run(&pass.place, 8, 9, 1);
                    run(&pass.gather, 20, 45, 1);
                }
                run(&pass.raise, views.size[0].div_ceil(8), views.size[1].div_ceil(8), 1);
            }
            Role::Miss => {
                // The points afresh, from this frame's depth: the diffuse
                // stand-in makes them later in the frame.
                run(&pass.place, 8, 9, 1);
                run(&pass.gather, 20, 45, 1);
                if let Some(reflect) = &pass.reflect {
                    run(reflect, views.size[0].div_ceil(8), views.size[1].div_ceil(8), 1);
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        assert_eq!(parse(""), Set::Off);
        assert_eq!(parse("off"), Set::Off);
        assert_eq!(parse("0"), Set::Off);
        assert_eq!(parse("0.8"), Set::Estimate(0.8));
        assert_eq!(parse("constant:0.3"), Set::Constant(0.3));
        assert_eq!(parse("constant:nan"), Set::Off);
    }

    #[test]
    fn the_band_with_rays() {
        assert_eq!(parse_band("0.5,1.5"), Some(Band { least: 0.5, most: 1.5, level: 0.8 }));
        assert_eq!(parse_band("1,1,0.6"), Some(Band { least: 1.0, most: 1.0, level: 0.6 }));
        // The top is never under the floor; a missing top is four times.
        assert_eq!(parse_band("2,1"), Some(Band { least: 2.0, most: 2.0, level: 0.8 }));
        assert_eq!(parse_band("0.7"), Some(Band { least: 0.7, most: 4.0, level: 0.8 }));
        assert_eq!(parse_band(""), None);
        assert_eq!(parse_band("x,1"), None);
    }

    #[test]
    fn specular_settings() {
        assert_eq!(parse_specular(""), None);
        assert_eq!(parse_specular("0"), None);
        assert_eq!(parse_specular("off"), None);
        assert_eq!(parse_specular("1"), Some(Specular { share: 1.0, sky: 0.0 }));
        assert_eq!(parse_specular("0.6, 0.25"), Some(Specular { share: 0.6, sky: 0.25 }));
        assert_eq!(parse_specular("0.6,7"), Some(Specular { share: 0.6, sky: 1.0 }));
        assert_eq!(parse_specular("0.6,x"), Some(Specular { share: 0.6, sky: 0.0 }));
    }

    /// The miss program by its name (foxhound 2db59d4's cases; there they
    /// are of `role`, which here names the miss program only with
    /// `VICTORY_AMBIENT_SPECULAR` set).
    #[test]
    fn the_miss_program_by_name() {
        assert!(is_miss(b"..CS_RayMiss_de6b91a6_Wrapped.."));
        assert!(!is_miss(b"..CS_RayMiss_Volumetrics_3c5d8c00_Wrapped.."));
        assert!(!is_miss(b"..CS_ScreenProbe_RayMiss_80a88544_Wrapped.."));
        if specular().is_none() {
            assert_eq!(role(b"..CS_RayMiss_de6b91a6_Wrapped.."), None, "not named with the switch unset");
        }
    }

    #[test]
    fn roles_by_name() {
        assert_eq!(role(b"..CS_ScreenProbes_TemporalResolve_a911b49c_Wrapped.."), Some(Role::Resolve));
        assert_eq!(role(b"..CS_CacheScreenSpaceRadiance_eed2fc35.."), Some(Role::Radiance));
        assert_eq!(role(b"CS_AccumulateLocalLightInscatteringVolume_7e2f5eef"), Some(Role::Air));
        assert_eq!(role(b"CS_TemporalResolve"), None);
    }

    /// The passes compile with the system's compiler and the host takes them.
    #[test]
    fn the_passes_build() {
        let Some(host) = host() else {
            return;
        };
        make(&host.device, |bytes| host.uav_buffer(bytes)).expect("the passes");
    }

    /// With the stand-in for reflections (two constants more in every pass,
    /// and a pass of its own) the passes compile with the system's compiler
    /// too, in one of their forms as [`make`] takes them.
    #[test]
    fn the_passes_build_with_reflections() {
        for (part, entry) in [
            ("KEEP", s!("keep")),
            ("KEEP", s!("mean")),
            ("PLACE", s!("place")),
            ("GATHER", s!("gather")),
            ("RAISE", s!("raise")),
            ("AIR", s!("clear_air")),
            ("REFLECT", s!("reflect_light")),
        ] {
            let forms = [
                format!("#define SPECULAR\n#define {part}\n{SOURCE}"),
                format!("#define SPECULAR\n#define {part}\n#define WINE_HLSL\n{SOURCE}"),
            ];
            if let Err(e) = crate::host::compile_first(&forms, entry, s!("cs_5_1")) {
                panic!("{part}: {e}");
            }
        }
    }
}
