//! Preflight for the barrier descriptions passed to the D3D12 runtime.
//! Resource state histories and split pairs span calls and submission scopes.

use std::{fmt::Debug, panic::Location};
use windows::{Win32::Graphics::Direct3D12::*, core::Interface};

type Check = Result<(), &'static str>;

const COPY_ACCESS: i32 = D3D12_BARRIER_ACCESS_COPY_SOURCE.0 | D3D12_BARRIER_ACCESS_COPY_DEST.0;
const AS_READ: i32 = D3D12_BARRIER_ACCESS_RAYTRACING_ACCELERATION_STRUCTURE_READ.0;
const AS_WRITE: i32 = D3D12_BARRIER_ACCESS_RAYTRACING_ACCELERATION_STRUCTURE_WRITE.0;
const BUILD_AS: i32 = D3D12_BARRIER_SYNC_BUILD_RAYTRACING_ACCELERATION_STRUCTURE.0;
const COPY_AS: i32 = D3D12_BARRIER_SYNC_COPY_RAYTRACING_ACCELERATION_STRUCTURE.0;
const POSTBUILD: i32 = D3D12_BARRIER_SYNC_EMIT_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO.0;
const ALL: i32 = D3D12_BARRIER_SYNC_ALL.0;
const SPLIT: i32 = D3D12_BARRIER_SYNC_SPLIT.0;
const NO_ACCESS: i32 = D3D12_BARRIER_ACCESS_NO_ACCESS.0;
const SRV: i32 = D3D12_BARRIER_ACCESS_SHADER_RESOURCE.0;
const UAV: i32 = D3D12_BARRIER_ACCESS_UNORDERED_ACCESS.0;
const DEPTH_READ: i32 = D3D12_BARRIER_ACCESS_DEPTH_STENCIL_READ.0;
const DEPTH_WRITE: i32 = D3D12_BARRIER_ACCESS_DEPTH_STENCIL_WRITE.0;
const SHADING: i32 = D3D12_BARRIER_SYNC_VERTEX_SHADING.0 | D3D12_BARRIER_SYNC_PIXEL_SHADING.0
    | D3D12_BARRIER_SYNC_COMPUTE_SHADING.0 | D3D12_BARRIER_SYNC_ALL_SHADING.0
    | D3D12_BARRIER_SYNC_NON_PIXEL_SHADING.0 | D3D12_BARRIER_SYNC_DRAW.0;
const COMPUTE_SYNCS: i32 = ALL | SPLIT | D3D12_BARRIER_SYNC_COMPUTE_SHADING.0
    | D3D12_BARRIER_SYNC_RAYTRACING.0 | D3D12_BARRIER_SYNC_COPY.0
    | D3D12_BARRIER_SYNC_EXECUTE_INDIRECT.0 | D3D12_BARRIER_SYNC_ALL_SHADING.0
    | D3D12_BARRIER_SYNC_NON_PIXEL_SHADING.0 | BUILD_AS | COPY_AS | POSTBUILD
    | D3D12_BARRIER_SYNC_CLEAR_UNORDERED_ACCESS_VIEW.0;
const DIRECT_SYNCS: i32 = COMPUTE_SYNCS | SHADING | D3D12_BARRIER_SYNC_INDEX_INPUT.0
    | D3D12_BARRIER_SYNC_DEPTH_STENCIL.0 | D3D12_BARRIER_SYNC_RENDER_TARGET.0
    | D3D12_BARRIER_SYNC_RESOLVE.0;
const COMPUTE_ACCESSES: i32 = COPY_ACCESS | SRV | UAV | AS_READ | AS_WRITE
    | D3D12_BARRIER_ACCESS_CONSTANT_BUFFER.0 | D3D12_BARRIER_ACCESS_INDIRECT_ARGUMENT.0;

// For each access bit, one of the corresponding sync scopes must be present.
const ACCESS_SYNC: &[(i32, i32)] = &[
    (D3D12_BARRIER_ACCESS_VERTEX_BUFFER.0, D3D12_BARRIER_SYNC_VERTEX_SHADING.0
        | D3D12_BARRIER_SYNC_NON_PIXEL_SHADING.0 | D3D12_BARRIER_SYNC_DRAW.0
        | D3D12_BARRIER_SYNC_ALL_SHADING.0),
    (D3D12_BARRIER_ACCESS_CONSTANT_BUFFER.0, SHADING | D3D12_BARRIER_SYNC_RAYTRACING.0),
    (D3D12_BARRIER_ACCESS_INDEX_BUFFER.0, D3D12_BARRIER_SYNC_DRAW.0 | D3D12_BARRIER_SYNC_INDEX_INPUT.0),
    (D3D12_BARRIER_ACCESS_RENDER_TARGET.0, D3D12_BARRIER_SYNC_DRAW.0 | D3D12_BARRIER_SYNC_RENDER_TARGET.0),
    (UAV, SHADING | POSTBUILD | D3D12_BARRIER_SYNC_RAYTRACING.0 | D3D12_BARRIER_SYNC_CLEAR_UNORDERED_ACCESS_VIEW.0),
    (DEPTH_READ | DEPTH_WRITE, D3D12_BARRIER_SYNC_DRAW.0 | D3D12_BARRIER_SYNC_DEPTH_STENCIL.0),
    (SRV, SHADING | BUILD_AS | D3D12_BARRIER_SYNC_RAYTRACING.0),
    (D3D12_BARRIER_ACCESS_STREAM_OUTPUT.0, D3D12_BARRIER_SYNC_VERTEX_SHADING.0
        | D3D12_BARRIER_SYNC_NON_PIXEL_SHADING.0 | D3D12_BARRIER_SYNC_DRAW.0
        | D3D12_BARRIER_SYNC_ALL_SHADING.0),
    (D3D12_BARRIER_ACCESS_INDIRECT_ARGUMENT.0, D3D12_BARRIER_SYNC_EXECUTE_INDIRECT.0),
    (COPY_ACCESS, D3D12_BARRIER_SYNC_COPY.0),
    (D3D12_BARRIER_ACCESS_RESOLVE_SOURCE.0 | D3D12_BARRIER_ACCESS_RESOLVE_DEST.0, D3D12_BARRIER_SYNC_RESOLVE.0),
    (AS_READ, D3D12_BARRIER_SYNC_COMPUTE_SHADING.0 | D3D12_BARRIER_SYNC_RAYTRACING.0
        | D3D12_BARRIER_SYNC_ALL_SHADING.0 | D3D12_BARRIER_SYNC_NON_PIXEL_SHADING.0
        | BUILD_AS | COPY_AS | POSTBUILD),
    (AS_WRITE, D3D12_BARRIER_SYNC_COMPUTE_SHADING.0 | D3D12_BARRIER_SYNC_RAYTRACING.0
        | D3D12_BARRIER_SYNC_ALL_SHADING.0 | D3D12_BARRIER_SYNC_NON_PIXEL_SHADING.0
        | BUILD_AS | COPY_AS),
    (D3D12_BARRIER_ACCESS_SHADING_RATE_SOURCE.0, D3D12_BARRIER_SYNC_PIXEL_SHADING.0
        | D3D12_BARRIER_SYNC_DRAW.0 | D3D12_BARRIER_SYNC_ALL_SHADING.0),
];

fn sync_access(queue: D3D12_COMMAND_LIST_TYPE, sync: i32, access: i32) -> Check {
    let (syncs, accesses) = match queue {
        D3D12_COMMAND_LIST_TYPE_DIRECT => (DIRECT_SYNCS, ACCESS_SYNC.iter().fold(0, |mask, &(bits, _)| mask | bits)),
        D3D12_COMMAND_LIST_TYPE_COMPUTE => (COMPUTE_SYNCS, COMPUTE_ACCESSES),
        D3D12_COMMAND_LIST_TYPE_COPY => (ALL | SPLIT | D3D12_BARRIER_SYNC_COPY.0, COPY_ACCESS),
        _ => return Err("unsupported command list type"),
    };
    if sync & !syncs != 0 || sync & SPLIT != 0 && sync != SPLIT {
        return Err("invalid sync mask for command list type");
    }
    if access & !accesses != 0 && access != NO_ACCESS {
        return Err("invalid access mask for command list type");
    }
    if sync == 0 && access != NO_ACCESS {
        return Err("SYNC_NONE requires ACCESS_NO_ACCESS");
    }
    // NO_ACCESS alone can also accompany a wait, for example at alias activation.
    if access == NO_ACCESS || sync == SPLIT {
        return Ok(());
    }
    if ACCESS_SYNC.iter().any(|&(bits, compatible)| access & bits != 0 && sync & (ALL | compatible) == 0) {
        return Err("access mask needs a compatible sync scope");
    }
    Ok(())
}

fn scopes(
    queue: D3D12_COMMAND_LIST_TYPE,
    kind: D3D12_BARRIER_TYPE,
    before: (D3D12_BARRIER_SYNC, D3D12_BARRIER_ACCESS),
    after: (D3D12_BARRIER_SYNC, D3D12_BARRIER_ACCESS),
) -> Check {
    let (sync_before, access_before) = before;
    let (sync_after, access_after) = after;
    sync_access(queue, sync_before.0, access_before.0)?;
    sync_access(queue, sync_after.0, access_after.0)?;
    let split_before = sync_before.0 == SPLIT;
    let split_after = sync_after.0 == SPLIT;
    if (split_before && split_after) || (kind == D3D12_BARRIER_TYPE_GLOBAL && (split_before || split_after)) {
        return Err("invalid split barrier");
    }
    Ok(())
}

fn layout_access(
    queue: D3D12_COMMAND_LIST_TYPE,
    layout: D3D12_BARRIER_LAYOUT,
    access: D3D12_BARRIER_ACCESS,
    desc: &D3D12_RESOURCE_DESC,
    layoutless: bool,
) -> Check {
    let flags = desc.Flags;
    let simultaneous = flags.contains(D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS);
    let allowed = match layout {
        D3D12_BARRIER_LAYOUT_UNDEFINED if layoutless => !0,
        D3D12_BARRIER_LAYOUT_UNDEFINED => NO_ACCESS,
        D3D12_BARRIER_LAYOUT_COMMON if simultaneous => COPY_ACCESS | SRV | UAV
            | D3D12_BARRIER_ACCESS_RENDER_TARGET.0 | D3D12_BARRIER_ACCESS_RESOLVE_SOURCE.0
            | D3D12_BARRIER_ACCESS_RESOLVE_DEST.0,
        D3D12_BARRIER_LAYOUT_COMMON => COPY_ACCESS | SRV,
        D3D12_BARRIER_LAYOUT_DIRECT_QUEUE_COMMON | D3D12_BARRIER_LAYOUT_COMPUTE_QUEUE_COMMON => COPY_ACCESS | SRV | UAV,
        D3D12_BARRIER_LAYOUT_DIRECT_QUEUE_GENERIC_READ => SRV | D3D12_BARRIER_ACCESS_COPY_SOURCE.0
            | DEPTH_READ | D3D12_BARRIER_ACCESS_SHADING_RATE_SOURCE.0 | D3D12_BARRIER_ACCESS_RESOLVE_SOURCE.0,
        D3D12_BARRIER_LAYOUT_GENERIC_READ | D3D12_BARRIER_LAYOUT_COMPUTE_QUEUE_GENERIC_READ => SRV | D3D12_BARRIER_ACCESS_COPY_SOURCE.0,
        D3D12_BARRIER_LAYOUT_RENDER_TARGET => D3D12_BARRIER_ACCESS_RENDER_TARGET.0,
        D3D12_BARRIER_LAYOUT_UNORDERED_ACCESS | D3D12_BARRIER_LAYOUT_DIRECT_QUEUE_UNORDERED_ACCESS
        | D3D12_BARRIER_LAYOUT_COMPUTE_QUEUE_UNORDERED_ACCESS => UAV,
        D3D12_BARRIER_LAYOUT_DEPTH_STENCIL_WRITE => DEPTH_READ | DEPTH_WRITE,
        D3D12_BARRIER_LAYOUT_DEPTH_STENCIL_READ => DEPTH_READ,
        D3D12_BARRIER_LAYOUT_SHADER_RESOURCE | D3D12_BARRIER_LAYOUT_DIRECT_QUEUE_SHADER_RESOURCE
        | D3D12_BARRIER_LAYOUT_COMPUTE_QUEUE_SHADER_RESOURCE => SRV,
        D3D12_BARRIER_LAYOUT_COPY_SOURCE | D3D12_BARRIER_LAYOUT_DIRECT_QUEUE_COPY_SOURCE
        | D3D12_BARRIER_LAYOUT_COMPUTE_QUEUE_COPY_SOURCE => D3D12_BARRIER_ACCESS_COPY_SOURCE.0,
        D3D12_BARRIER_LAYOUT_COPY_DEST | D3D12_BARRIER_LAYOUT_DIRECT_QUEUE_COPY_DEST
        | D3D12_BARRIER_LAYOUT_COMPUTE_QUEUE_COPY_DEST => D3D12_BARRIER_ACCESS_COPY_DEST.0,
        D3D12_BARRIER_LAYOUT_RESOLVE_SOURCE => D3D12_BARRIER_ACCESS_RESOLVE_SOURCE.0,
        D3D12_BARRIER_LAYOUT_RESOLVE_DEST => D3D12_BARRIER_ACCESS_RESOLVE_DEST.0,
        D3D12_BARRIER_LAYOUT_SHADING_RATE_SOURCE => D3D12_BARRIER_ACCESS_SHADING_RATE_SOURCE.0,
        _ => return Err("invalid layout for graphics command list"),
    };
    let direct_only = (D3D12_BARRIER_LAYOUT_DIRECT_QUEUE_COMMON.0..=D3D12_BARRIER_LAYOUT_DIRECT_QUEUE_COPY_DEST.0)
        .contains(&layout.0) || matches!(layout, D3D12_BARRIER_LAYOUT_RENDER_TARGET | D3D12_BARRIER_LAYOUT_DEPTH_STENCIL_WRITE
            | D3D12_BARRIER_LAYOUT_DEPTH_STENCIL_READ | D3D12_BARRIER_LAYOUT_RESOLVE_SOURCE
            | D3D12_BARRIER_LAYOUT_RESOLVE_DEST | D3D12_BARRIER_LAYOUT_SHADING_RATE_SOURCE);
    let compute_only = (D3D12_BARRIER_LAYOUT_COMPUTE_QUEUE_COMMON.0..=D3D12_BARRIER_LAYOUT_COMPUTE_QUEUE_COPY_DEST.0)
        .contains(&layout.0);
    if direct_only && queue != D3D12_COMMAND_LIST_TYPE_DIRECT
        || compute_only && queue != D3D12_COMMAND_LIST_TYPE_COMPUTE
        || queue == D3D12_COMMAND_LIST_TYPE_COPY && layout != D3D12_BARRIER_LAYOUT_COMMON
    {
        return Err("layout is incompatible with command list type");
    }
    if access.0 != NO_ACCESS
        && (access.0 & !allowed != 0 || (layout == D3D12_BARRIER_LAYOUT_UNDEFINED && !layoutless))
    {
        return Err("texture layout is incompatible with access mask");
    }
    resource_access(flags, access.0)
}

fn resource_access(flags: D3D12_RESOURCE_FLAGS, access: i32) -> Check {
    if access & UAV != 0 && !flags.contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS)
        || access & D3D12_BARRIER_ACCESS_RENDER_TARGET.0 != 0 && !flags.contains(D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET)
        || access & (DEPTH_READ | DEPTH_WRITE) != 0 && !flags.contains(D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL)
        || access & SRV != 0 && flags.contains(D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE)
    {
        return Err("access is incompatible with resource creation flags");
    }
    Ok(())
}

fn buffer_desc(desc: &D3D12_RESOURCE_DESC, offset: u64, size: u64) -> Check {
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_BUFFER {
        return Err("buffer barrier requires a buffer resource");
    }
    if offset != 0 || size != u64::MAX && size != desc.Width {
        return Err("buffer barrier must cover the whole buffer");
    }
    Ok(())
}

fn texture_desc(
    queue: D3D12_COMMAND_LIST_TYPE,
    barrier: &D3D12_TEXTURE_BARRIER,
    desc: &D3D12_RESOURCE_DESC,
) -> Check {
    if !matches!(desc.Dimension, D3D12_RESOURCE_DIMENSION_TEXTURE1D
        | D3D12_RESOURCE_DIMENSION_TEXTURE2D | D3D12_RESOURCE_DIMENSION_TEXTURE3D)
    {
        return Err("texture barrier requires a texture resource");
    }
    if barrier.Flags != D3D12_TEXTURE_BARRIER_FLAG_NONE
        && !(barrier.Flags == D3D12_TEXTURE_BARRIER_FLAG_DISCARD && barrier.LayoutBefore == D3D12_BARRIER_LAYOUT_UNDEFINED)
    {
        return Err("invalid texture barrier flags or DISCARD layout");
    }
    let layoutless = barrier.LayoutBefore == D3D12_BARRIER_LAYOUT_UNDEFINED
        && barrier.LayoutAfter == D3D12_BARRIER_LAYOUT_UNDEFINED;
    if queue == D3D12_COMMAND_LIST_TYPE_COPY && barrier.LayoutBefore != barrier.LayoutAfter {
        return Err("copy lists do not support texture layout transitions");
    }
    layout_access(queue, barrier.LayoutBefore, barrier.AccessBefore, desc, layoutless)?;
    layout_access(queue, barrier.LayoutAfter, barrier.AccessAfter, desc, layoutless)
}

fn report(
    list: usize, site: &str, caller: &Location<'_>, kind: i32, group: usize,
    count: usize, index: usize, error: &'static str, entry: impl Debug,
) {
    tracing::error!(list = format_args!("{list:#x}"), site, caller = %caller, kind, group,
        count, index, error, entry = ?entry, "rf-x7: invalid barrier group skipped");
}

/// The payload must point to `count` live entries when non-null.
unsafe fn check_entries<T: Debug>(
    entries: *const T, count: usize, mut check: impl FnMut(&T) -> Check,
    mut invalid: impl FnMut(usize, &'static str, Option<&T>),
) -> bool {
    if count == 0 {
        return true;
    }
    if entries.is_null() {
        invalid(0, "null barrier payload", None);
        return false;
    }
    // SAFETY: the caller owns the typed payload and keeps it live for recording.
    let entries = unsafe { std::slice::from_raw_parts(entries, count) };
    for (index, entry) in entries.iter().enumerate() {
        if let Err(error) = check(entry) {
            invalid(index, error, Some(entry));
            return false;
        }
    }
    true
}

#[track_caller]
pub(crate) unsafe fn filter_groups(
    list: &ID3D12GraphicsCommandList7,
    site: &str,
    groups: &[D3D12_BARRIER_GROUP],
) -> Vec<D3D12_BARRIER_GROUP> {
    // SAFETY: the list and the tagged payloads are live.
    let queue = unsafe { list.GetType() };
    unsafe { groups_for_queue(queue, list.as_raw() as usize, site, groups) }
}

#[track_caller]
unsafe fn groups_for_queue(
    queue: D3D12_COMMAND_LIST_TYPE, list: usize, site: &str, groups: &[D3D12_BARRIER_GROUP],
) -> Vec<D3D12_BARRIER_GROUP> {
    let caller = Location::caller();
    groups.iter().enumerate().filter_map(|(index, group)| {
        let kind = group.Type;
        let count = group.NumBarriers as usize;
        let log = |entry_idx, error, entry: &dyn Debug| {
            report(list, site, caller, kind.0, index, count, entry_idx, error, entry);
        };
        let valid = unsafe {
            match kind {
                D3D12_BARRIER_TYPE_GLOBAL => check_entries(group.Anonymous.pGlobalBarriers, count, |b| {
                    scopes(queue, kind, (b.SyncBefore, b.AccessBefore), (b.SyncAfter, b.AccessAfter))
                }, |i, error, entry| log(i, error, &entry)),
                D3D12_BARRIER_TYPE_BUFFER => check_entries(group.Anonymous.pBufferBarriers, count, |b| {
                    scopes(queue, kind, (b.SyncBefore, b.AccessBefore), (b.SyncAfter, b.AccessAfter))?;
                    let desc = b.pResource.as_ref().ok_or("null buffer resource")?.GetDesc();
                    buffer_desc(&desc, b.Offset, b.Size)?;
                    let access = (b.AccessBefore.0 | b.AccessAfter.0) & !NO_ACCESS;
                    if access & (DEPTH_READ | DEPTH_WRITE | D3D12_BARRIER_ACCESS_RENDER_TARGET.0
                        | D3D12_BARRIER_ACCESS_SHADING_RATE_SOURCE.0) != 0
                    {
                        return Err("texture-only access in buffer barrier");
                    }
                    resource_access(desc.Flags, access)
                }, |i, error, entry| log(i, error, &entry)),
                D3D12_BARRIER_TYPE_TEXTURE => check_entries(group.Anonymous.pTextureBarriers, count, |b| {
                    scopes(queue, kind, (b.SyncBefore, b.AccessBefore), (b.SyncAfter, b.AccessAfter))?;
                    let desc = b.pResource.as_ref().ok_or("null texture resource")?.GetDesc();
                    texture_desc(queue, b, &desc)
                }, |i, error, entry| log(i, error, &entry)),
                _ => {
                    log(0, "unknown barrier group type", &kind);
                    false
                }
            }
        };
        (valid && count != 0).then_some(*group)
    }).collect()
}

fn legacy_transition(desc: &D3D12_RESOURCE_DESC, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> Check {
    if before == after {
        return Err("legacy transition does not change resource state");
    }
    const WRITE_STATES: i32 = D3D12_RESOURCE_STATE_RENDER_TARGET.0 | D3D12_RESOURCE_STATE_UNORDERED_ACCESS.0
        | D3D12_RESOURCE_STATE_DEPTH_WRITE.0 | D3D12_RESOURCE_STATE_STREAM_OUT.0
        | D3D12_RESOURCE_STATE_COPY_DEST.0 | D3D12_RESOURCE_STATE_RESOLVE_DEST.0;
    const GRAPHICS_STATES: i32 = WRITE_STATES | D3D12_RESOURCE_STATE_GENERIC_READ.0
        | D3D12_RESOURCE_STATE_DEPTH_READ.0 | D3D12_RESOURCE_STATE_RESOLVE_SOURCE.0
        | D3D12_RESOURCE_STATE_SHADING_RATE_SOURCE.0;
    for state in [before, after] {
        if state.0 & !GRAPHICS_STATES != 0 {
            return Err("legacy state is invalid for a graphics transition");
        }
        let write = state.0 & WRITE_STATES;
        if write != 0 && (write != state.0 || write.count_ones() > 1) {
            return Err("legacy state mixes a write state with another state");
        }
        if crate::host::legal_for(desc.Flags.0 as u32, state.0 as u32) != state.0 as u32 {
            return Err("legacy state is incompatible with creation flags");
        }
        if state.0 & D3D12_RESOURCE_STATE_RESOLVE_SOURCE.0 != 0 && desc.SampleDesc.Count <= 1
            || state.0 & D3D12_RESOURCE_STATE_RESOLVE_DEST.0 != 0 && desc.SampleDesc.Count != 1
        {
            return Err("legacy resolve state is incompatible with sample count");
        }
    }
    Ok(())
}

#[track_caller]
pub(crate) unsafe fn legacy_array(list: &ID3D12GraphicsCommandList, site: &str, barriers: &[D3D12_RESOURCE_BARRIER]) -> bool {
    let caller = Location::caller();
    for (index, barrier) in barriers.iter().enumerate() {
        // SAFETY: use the union member selected by Type, with live resource pointers.
        let (result, entry): (Check, &dyn Debug) = unsafe {
            match barrier.Type {
                D3D12_RESOURCE_BARRIER_TYPE_TRANSITION => {
                    let transition = &*barrier.Anonymous.Transition;
                    let result = match transition.pResource.as_ref() {
                        Some(resource) => legacy_transition(&resource.GetDesc(), transition.StateBefore, transition.StateAfter),
                        None => Err("null transition resource"),
                    };
                    (result, transition)
                }
                D3D12_RESOURCE_BARRIER_TYPE_UAV => {
                    let uav = &*barrier.Anonymous.UAV;
                    let result = match uav.pResource.as_ref() {
                        Some(resource) => resource_access(resource.GetDesc().Flags, UAV),
                        None => Ok(()),
                    };
                    (result, uav)
                }
                D3D12_RESOURCE_BARRIER_TYPE_ALIASING => (Ok(()), &*barrier.Anonymous.Aliasing),
                _ => (Err("unknown legacy barrier type"), &barrier.Type),
            }
        };
        let result = result.and_then(|()| {
            if barrier.Flags == D3D12_RESOURCE_BARRIER_FLAG_NONE
                || (barrier.Type == D3D12_RESOURCE_BARRIER_TYPE_TRANSITION
                    && matches!(barrier.Flags, D3D12_RESOURCE_BARRIER_FLAG_BEGIN_ONLY | D3D12_RESOURCE_BARRIER_FLAG_END_ONLY))
            {
                Ok(())
            } else {
                Err("invalid legacy barrier flags")
            }
        });
        if let Err(error) = result {
            report(list.as_raw() as usize, site, caller, barrier.Type.0, 0, barriers.len(), index, error, (barrier.Flags, entry));
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIRECT: D3D12_COMMAND_LIST_TYPE = D3D12_COMMAND_LIST_TYPE_DIRECT;

    #[test]
    fn accepts_copy_and_clamp_masks() {
        assert!(sync_access(DIRECT, ALL, D3D12_BARRIER_ACCESS_COMMON.0).is_ok());
        assert!(sync_access(DIRECT, D3D12_BARRIER_SYNC_COPY.0, COPY_ACCESS).is_ok());
        let producer = D3D12_BARRIER_SYNC_ALL_SHADING | D3D12_BARRIER_SYNC_CLEAR_UNORDERED_ACCESS_VIEW;
        assert!(sync_access(DIRECT, producer.0, UAV).is_ok());
        assert!(sync_access(DIRECT, D3D12_BARRIER_SYNC_COMPUTE_SHADING.0, SRV | UAV).is_ok());
        assert!(sync_access(DIRECT, D3D12_BARRIER_SYNC_COPY.0, COPY_ACCESS | UAV).is_err());
        assert!(sync_access(D3D12_COMMAND_LIST_TYPE_COPY, ALL, UAV).is_err());
    }

    #[test]
    fn validates_none_no_access_and_split_independently() {
        assert!(sync_access(DIRECT, 0, NO_ACCESS).is_ok());
        assert!(sync_access(DIRECT, ALL, NO_ACCESS).is_ok());
        assert!(sync_access(DIRECT, ALL, NO_ACCESS | UAV).is_err());
        assert!(sync_access(DIRECT, 0, D3D12_BARRIER_ACCESS_COMMON.0).is_err());
        assert!(sync_access(DIRECT, SPLIT | ALL, UAV).is_err());
        let split = (D3D12_BARRIER_SYNC_SPLIT, D3D12_BARRIER_ACCESS_UNORDERED_ACCESS);
        let full = (D3D12_BARRIER_SYNC_ALL, D3D12_BARRIER_ACCESS_UNORDERED_ACCESS);
        assert!(scopes(DIRECT, D3D12_BARRIER_TYPE_GLOBAL, full, split).is_err());
        assert!(scopes(DIRECT, D3D12_BARRIER_TYPE_BUFFER, full, split).is_ok());
        assert!(scopes(DIRECT, D3D12_BARRIER_TYPE_BUFFER, split, split).is_err());
    }

    #[test]
    fn rejects_wrong_resource_kind_and_partial_buffer_ranges() {
        let mut desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 256,
            ..Default::default()
        };
        assert!(buffer_desc(&desc, 0, 256).is_ok());
        assert!(buffer_desc(&desc, 0, u64::MAX).is_ok());
        assert!(buffer_desc(&desc, 0, 128).is_err());
        assert!(buffer_desc(&desc, 1, u64::MAX).is_err());
        assert!(texture_desc(DIRECT, &D3D12_TEXTURE_BARRIER::default(), &desc).is_err());
        desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
        assert!(buffer_desc(&desc, 0, u64::MAX).is_err());
    }

    #[test]
    fn checks_layout_and_resource_creation_flags() {
        let mut desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let uav = D3D12_BARRIER_ACCESS_UNORDERED_ACCESS;
        assert!(layout_access(DIRECT, D3D12_BARRIER_LAYOUT_UNORDERED_ACCESS, uav, &desc, false).is_ok());
        assert!(layout_access(DIRECT, D3D12_BARRIER_LAYOUT_COPY_SOURCE, uav, &desc, false).is_err());
        assert!(layout_access(D3D12_COMMAND_LIST_TYPE_COMPUTE, D3D12_BARRIER_LAYOUT_DIRECT_QUEUE_UNORDERED_ACCESS, uav, &desc, false).is_err());
        desc.Flags = D3D12_RESOURCE_FLAG_NONE;
        assert!(layout_access(DIRECT, D3D12_BARRIER_LAYOUT_UNORDERED_ACCESS, uav, &desc, false).is_err());
    }

    #[test]
    fn drops_an_entire_invalid_group_and_keeps_valid_groups_in_order() {
        let all = D3D12_GLOBAL_BARRIER {
            SyncBefore: D3D12_BARRIER_SYNC_ALL,
            SyncAfter: D3D12_BARRIER_SYNC_ALL,
            AccessBefore: D3D12_BARRIER_ACCESS_COMMON,
            AccessAfter: D3D12_BARRIER_ACCESS_COMMON,
        };
        let first = [all];
        let mixed = [all, D3D12_GLOBAL_BARRIER {
            SyncAfter: D3D12_BARRIER_SYNC_COPY,
            AccessAfter: D3D12_BARRIER_ACCESS_UNORDERED_ACCESS,
            ..all
        }];
        let last = [all, all];
        let group = |entries: &[D3D12_GLOBAL_BARRIER]| D3D12_BARRIER_GROUP {
            Type: D3D12_BARRIER_TYPE_GLOBAL,
            NumBarriers: entries.len() as u32,
            Anonymous: D3D12_BARRIER_GROUP_0 { pGlobalBarriers: entries.as_ptr() },
        };
        let null = D3D12_BARRIER_GROUP {
            Type: D3D12_BARRIER_TYPE_BUFFER,
            NumBarriers: 1,
            Anonymous: D3D12_BARRIER_GROUP_0 { pBufferBarriers: std::ptr::null() },
        };
        // SAFETY: all non-null pointers refer to live payloads of the tagged type.
        let valid = unsafe { groups_for_queue(DIRECT, 0, "test", &[group(&first), group(&mixed), null, group(&last)]) };
        assert_eq!(valid.iter().map(|g| g.NumBarriers).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn rejects_invalid_legacy_states() {
        let mut desc = D3D12_RESOURCE_DESC::default();
        let common = D3D12_RESOURCE_STATE_COMMON;
        let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        assert!(legacy_transition(&desc, common, common).is_err());
        assert!(legacy_transition(&desc, common, uav).is_err());
        desc.Flags = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
        assert!(legacy_transition(&desc, common, uav).is_ok());
        assert!(legacy_transition(&desc, common, uav | D3D12_RESOURCE_STATE_COPY_SOURCE).is_err());
        assert!(legacy_transition(&desc, common, D3D12_RESOURCE_STATE_RESERVED_INTERNAL_4000).is_err());
    }
}
