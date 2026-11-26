//! The first tests of `host` (moved out of `host.rs` whole, one indent less
//! and otherwise unchanged, so that the file stays under the asset policy's
//! 512 KiB per file: docs/asset-policy.md). A child module of `host`: `super`
//! is `host`.

#[test]
fn sparse_copies_preserve_islands_and_observe_remaps() {
    use windows_sys::Win32::System::Memory::*;
    const N: usize = 5 << 16;
    // SAFETY: we reserve a private region, commit selected pages inside it,
    // and only directly access the pages while they are writable.
    unsafe {
        let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
        assert!(!base.is_null());
        let mut expected = vec![0; N];
        for (offset, value) in [(0, 9), (2 << 16, 37), (N - 4096, 85)] {
            let at = base.add(offset);
            assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
            at.write_bytes(value, 4096);
            expected[offset..offset + 4096].fill(value);
        }
        for remap in [false, true] {
            if remap {
                let mut old = 0;
                assert_ne!(VirtualProtect(base.cast(), 4096, PAGE_NOACCESS, &mut old), 0);
                expected[..4096].fill(0);
                let at = base.add(1 << 16);
                assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
                at.write_bytes(0xc7, 4096);
                expected[1 << 16..(1 << 16) + 4096].fill(0xc7);
            }
            let mut out = vec![0xee; N];
            super::copy_mapped(out.as_mut_ptr().add(27), base.add(27), N - 54, false);
            assert_eq!(out[27..N - 27], expected[27..N - 27]);
            assert!(out[..27].iter().chain(&out[N - 27..]).all(|&v| v == 0xee));
        }
        assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
    }
}

use super::*;

/// The title memory a texture-to-buffer read-back lands in: its rows at
/// the title's pitch, nothing for an empty one or one past the address
/// space (after fxceless PR #565's test).
#[test]
fn a_read_back_lands_in_its_rows_and_no_further() {
    assert_eq!(read_back_span(0x1000, [16, 3, 64]), Some((0x1000, 0x1090)));
    assert_eq!(read_back_span(0x1000, [16, 1, 64]), Some((0x1000, 0x1010)));
    assert_eq!(read_back_span(0x1000, [0, 3, 64]), None);
    assert_eq!(read_back_span(0x1000, [16, 0, 64]), None);
    assert_eq!(read_back_span(usize::MAX - 8, [16, 1, 16]), None);
}

/// A view one thread has made and not yet recorded keeps its slot while
/// other threads make far more views than the old shared ring had
/// (after upstream PR #2844's `pending_rtv_survives_other_recorders`).
#[test]
fn a_view_not_yet_recorded_keeps_its_slot_while_other_threads_make_views() {
    let Some(host) = host() else {
        return; // no host GPU here
    };
    let null_rtv = D3D12_RENDER_TARGET_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    let pending = host.rtv(None, Some(&null_rtv));
    let others: Vec<usize> = std::thread::scope(|scope| {
        (0..4)
            .map(|_| scope.spawn(|| (0..2 * RTV_RING).map(|_| host.rtv(None, Some(&null_rtv)).ptr).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|t| t.join().unwrap())
            .collect()
    });
    assert!(!others.contains(&pending.ptr), "another thread's view took a slot not yet recorded");
    // The thread's own ring comes round only after its own views.
    let mine: Vec<usize> = (0..THREAD_RING).map(|_| host.rtv(None, Some(&null_rtv)).ptr).collect();
    assert_eq!(mine.last(), Some(&pending.ptr));
    assert!(!mine[..mine.len() - 1].contains(&pending.ptr));
}

/// The card compare as the runtime makes it (`make_card_work`: its
/// pipeline and root signature), on the host GPU: the steps and
/// answers of `indirect`'s WARP test of it, which does not run where
/// there is no WARP adapter (Wine).
#[test]
fn the_card_compare_stores_the_hash_it_made_when_told_on_the_host() {
    the_card_compare_stores_the_hash_it_made_when_told_on_the_host_as(false);
}

/// The same with a group of 64 threads a piece (`VICTORY_CARD_HASH_WIDE=1`).
#[test]
fn the_wide_card_compare_stores_the_hash_it_made_when_told_on_the_host() {
    the_card_compare_stores_the_hash_it_made_when_told_on_the_host_as(true);
}

fn the_card_compare_stores_the_hash_it_made_when_told_on_the_host_as(wide: bool) {
    let Some(host) = host() else {
        return; // no host GPU here
    };
    let work = host.make_card_work_as(wide).expect("the card compare must build on the host");
    const PIECE: usize = 64 << 10;
    const PIECES: u32 = 3;
    let size = PIECE * PIECES as usize;
    let (now, now_ptr) = host.shared_buffer(size as u64).unwrap();
    let (hashes, _) = host.shared_buffer(16 * u64::from(PIECES)).unwrap();
    let (differs, differs_ptr) = host.shared_buffer(64).unwrap();
    let content: Vec<u8> = (0..size).map(|i| (i * 31 + i / 251) as u8).collect();
    // SAFETY: the mapping holds `size` bytes.
    unsafe { std::ptr::copy_nonoverlapping(content.as_ptr(), now_ptr, size) };
    // One pass over the three pieces; what it left in `differs` (7s
    // where it stored nothing).
    let run = |mode: u32| -> [u32; 3] {
        let list = host.new_list().unwrap();
        // SAFETY: an open list, live buffers, the compare's own root
        // signature; the mappings hold what is read and written.
        unsafe {
            for piece in 0..PIECES as usize {
                differs_ptr.cast::<u32>().add(piece).write_volatile(7);
            }
            let l = &list.list;
            l.SetComputeRootSignature(&work.root);
            l.SetPipelineState(&work.pso);
            let constants = [0u32, 0, size as u32, PIECES, 0, mode];
            l.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
            l.SetComputeRootShaderResourceView(1, now.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(2, hashes.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(3, differs.GetGPUVirtualAddress());
            // Not read in these modes; bound as every use binds it.
            l.SetComputeRootShaderResourceView(4, now.GetGPUVirtualAddress());
            let (x, y) = card_groups(PIECES, work.wide);
            l.Dispatch(x, y, 1);
        }
        list.close();
        host.execute(&[&list]);
        // SAFETY: as above; the list has run.
        [0, 1, 2].map(|piece| unsafe { differs_ptr.cast::<u32>().add(piece).read_volatile() })
    };
    let at = PIECE + 1234;
    // SAFETY (each): inside the mapping.
    let set = |value: u8| unsafe { now_ptr.add(at).write_volatile(value) };
    // 0: the hashes are stored, nothing is flagged.
    assert_eq!(run(0), [7, 7, 7]);
    assert_eq!(run(1), [0, 0, 0]);
    // A change in the second piece: flagged, and with 1 flagged again
    // (the hash stays the old one).
    set(content[at] ^ 0x40);
    assert_eq!(run(1), [0, 1, 0]);
    assert_eq!(run(1), [0, 1, 0]);
    // Changed back: with 1 the piece looks untouched.
    set(content[at]);
    assert_eq!(run(1), [0, 0, 0]);
    // With 2 the change is flagged once, and the hash is the new one.
    set(content[at] ^ 0x40);
    assert_eq!(run(2), [0, 1, 0]);
    assert_eq!(run(2), [0, 0, 0]);
    // Changed back: flagged, where 1 saw nothing.
    set(content[at]);
    assert_eq!(run(2), [0, 1, 0]);
    assert_eq!(run(2), [0, 0, 0]);
    // A piece's last and first bytes (the last and first threads of a
    // wide pass's group) count as any other.
    for at in [2 * PIECE - 1, 2 * PIECE] {
        // SAFETY: inside the mapping.
        unsafe { now_ptr.add(at).write_volatile(content[at] ^ 0x81) };
        assert_eq!(run(1), if at < 2 * PIECE { [0, 1, 0] } else { [0, 0, 1] });
        // SAFETY: as above.
        unsafe { now_ptr.add(at).write_volatile(content[at]) };
        assert_eq!(run(1), [0, 0, 0]);
    }
}

/// The hash pass over a list of pieces (`piece_lists`, mode 3), on the
/// host GPU: it stores the hashes of the pieces the list names and of
/// no other, as mode 0 does for a run of them.
#[test]
fn the_card_hash_pass_over_a_list_stores_the_listed_pieces_only() {
    the_card_hash_pass_over_a_list_stores_the_listed_pieces_only_as(false);
}

/// The same with a group of 64 threads a piece (`VICTORY_CARD_HASH_WIDE=1`).
#[test]
fn the_wide_card_hash_pass_over_a_list_stores_the_listed_pieces_only() {
    the_card_hash_pass_over_a_list_stores_the_listed_pieces_only_as(true);
}

fn the_card_hash_pass_over_a_list_stores_the_listed_pieces_only_as(wide: bool) {
    let Some(host) = host() else {
        return; // no host GPU here
    };
    let work = host.make_card_work_as(wide).expect("the card compare must build on the host");
    const PIECE: usize = 64 << 10;
    const PIECES: u32 = 4;
    let size = PIECE * PIECES as usize;
    let (now, now_ptr) = host.shared_buffer(size as u64).unwrap();
    let (hashes, _) = host.shared_buffer(16 * u64::from(PIECES)).unwrap();
    let (differs, differs_ptr) = host.shared_buffer(64).unwrap();
    let (list_buffer, list_ptr) = host.shared_buffer(64).unwrap();
    let content: Vec<u8> = (0..size).map(|i| (i * 13 + i / 509) as u8).collect();
    // SAFETY: the mappings hold what is read and written.
    unsafe { std::ptr::copy_nonoverlapping(content.as_ptr(), now_ptr, size) };
    let run = |mode: u32, listed: &[u32]| -> [u32; 4] {
        let list = host.new_list().unwrap();
        // SAFETY: an open list, live buffers, the compare's own root
        // signature; the mappings hold what is read and written.
        unsafe {
            for piece in 0..PIECES as usize {
                differs_ptr.cast::<u32>().add(piece).write_volatile(7);
            }
            // Behind a word the pass must not read.
            list_ptr.cast::<u32>().write_volatile(PIECES + 9);
            for (i, &piece) in listed.iter().enumerate() {
                list_ptr.cast::<u32>().add(1 + i).write_volatile(piece);
            }
            let l = &list.list;
            l.SetComputeRootSignature(&work.root);
            l.SetPipelineState(&work.pso);
            let constants = if mode == 3 {
                [0u32, 0, size as u32, listed.len() as u32, 4, 3]
            } else {
                [0u32, 0, size as u32, PIECES, 0, mode]
            };
            l.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
            l.SetComputeRootShaderResourceView(1, now.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(2, hashes.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(3, differs.GetGPUVirtualAddress());
            l.SetComputeRootShaderResourceView(4, list_buffer.GetGPUVirtualAddress());
            // Groups for all the pieces, whatever the list holds: those
            // past it must store nothing.
            let (x, y) = card_groups(PIECES, work.wide);
            l.Dispatch(x, y, 1);
        }
        list.close();
        host.execute(&[&list]);
        // SAFETY: as above; the list has run.
        [0, 1, 2, 3].map(|piece| unsafe { differs_ptr.cast::<u32>().add(piece).read_volatile() })
    };
    // SAFETY (each): inside the mapping.
    let set = |at: usize, value: u8| unsafe { now_ptr.add(at).write_volatile(value) };
    assert_eq!(run(0, &[]), [7, 7, 7, 7]);
    assert_eq!(run(1, &[]), [0, 0, 0, 0]);
    // Pieces 1, 2 and 3 change; the list names 3 and 1: stored, flagged
    // nowhere (the list's pass flags nothing); 2 is still flagged.
    for piece in 1..4 {
        set(piece * PIECE + 99, content[piece * PIECE + 99] ^ 0x21);
    }
    assert_eq!(run(3, &[3, 1]), [7, 7, 7, 7]);
    assert_eq!(run(1, &[]), [0, 0, 1, 0]);
    // An empty list stores nothing.
    set(99, content[99] ^ 0x21);
    assert_eq!(run(3, &[]), [7, 7, 7, 7]);
    assert_eq!(run(1, &[]), [1, 0, 1, 0]);
}

/// The relay's fetch on a copy queue of its own (`fetch_aside`), on the
/// host GPU: bytes the host's kind of queue wrote into a buffer in the
/// card's memory come down whole once the copy queue has waited for
/// that queue's fence, which the CPU saw reached already.
#[test]
fn a_copy_queue_fetches_what_another_queue_wrote_once_its_fence_is_reached() {
    let Some(host) = host() else {
        return; // no host GPU here
    };
    const N: usize = 3 << 20;
    let (src, src_ptr) = host.shared_buffer(N as u64).unwrap();
    let (down, down_ptr) = host.shared_buffer(N as u64).unwrap();
    let card = host.uav_buffer(N as u64).unwrap();
    let content: Vec<u8> = (0..N).map(|i| (i * 7 + i / 4093) as u8).collect();
    // SAFETY: plain objects and lists of our own; the mappings hold N
    // bytes; each list has run before its memory is looked at.
    unsafe {
        std::ptr::copy_nonoverlapping(content.as_ptr(), src_ptr, N);
        down_ptr.write_bytes(0xee, N);
        let direct: ID3D12CommandQueue =
            host.device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).unwrap();
        let copy: ID3D12CommandQueue =
            host.device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_COPY, ..Default::default() }).unwrap();
        let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        let done: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        let wait = |f: &ID3D12Fence, value: u64| {
            f.SetEventOnCompletion(value, event).unwrap();
            WaitForSingleObject(event, INFINITE);
        };
        let (allocator, list) = host.make_prelude().unwrap();
        allocator.Reset().unwrap();
        list.Reset(&allocator, None).unwrap();
        list.CopyBufferRegion(&card, 0, &src, 0, N as u64);
        list.Close().unwrap();
        direct.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        direct.Signal(&fence, 1).unwrap();
        wait(&fence, 1);
        let copy_allocator: ID3D12CommandAllocator = host.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY).unwrap();
        let copy_list: ID3D12GraphicsCommandList =
            host.device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &copy_allocator, None).unwrap();
        // In pieces, as the fetch takes them: cut where a copy that
        // does not start on a piece boundary cuts them (odd offsets).
        let mut cuts: Vec<usize> = std::iter::once(0).chain((5..N).step_by(64 << 10)).chain([N]).collect();
        cuts.dedup();
        for w in cuts.windows(2).rev() {
            copy_list.CopyBufferRegion(&down, w[0] as u64, &card, w[0] as u64, (w[1] - w[0]) as u64);
        }
        copy_list.Close().unwrap();
        copy.Wait(&fence, 1).unwrap();
        copy.ExecuteCommandLists(&[Some(copy_list.cast().unwrap())]);
        copy.Signal(&done, 1).unwrap();
        wait(&done, 1);
        let _ = windows::Win32::Foundation::CloseHandle(event);
        assert!(std::slice::from_raw_parts(down_ptr, N) == content.as_slice(), "the fetch on the copy queue came down whole");
    }
}

#[test]
fn warp_adapter_accepts_only_software() {
    // This probes DXGI without creating a D3D12 device or running a title.
    // SAFETY: plain DXGI adapter queries.
    let raw = unsafe {
        let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
        else {
            return;
        };
        factory.EnumWarpAdapter::<IDXGIAdapter>()
    };
    let Ok(raw) = raw else { return };
    // SAFETY: EnumWarpAdapter returns a DXGI adapter, which implements
    // IDXGIAdapter1; `raw` owns that interface.
    let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
    eprintln!(
        "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
        desc.VendorId, desc.Flags
    );
    assert_eq!(
        warp_adapter().is_ok(),
        desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
    );
}
