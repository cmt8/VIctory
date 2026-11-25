//! Whether the host GPU at hand takes title memory in place: the probe
//! (`VICTORY_INPLACE_PROBE`) on the real device rather than the software
//! adapter (which Wine does not have, so the probe's own test passes there
//! without trying anything), and the way a stretch of the page pool's
//! section would be opened: the stretch mapped once more at an address of
//! ours and opened as a part of an allocation (`From::Part`,
//! `OpenExistingHeapFromAddress1`), the GPU reading and writing the bytes
//! the title's own view shows.
//!
//! Only with `VICTORY_MEMBENCH=1` and a host GPU. Under Wine:
//! `VICTORY_MEMBENCH=1 cargo xtask wine-test --vkd3d-proton <dir> -- -p
//! victory-gpu --release --lib host_in_place_probe -- --nocapture`.

use super::*;

#[test]
fn host_in_place_probe() {
    if std::env::var_os("VICTORY_MEMBENCH").is_none_or(|v| v != "1") {
        return;
    }
    let Some(host) = crate::host::host() else {
        return; // no host GPU
    };
    let device = &host.device;
    for line in probe(device, &|| None, 0).lines {
        println!("membench: in place on the host GPU: {line}");
    }
    let len = 8 * K64;
    // SAFETY: a section and two views of the test's own; every address
    // used lies inside them.
    unsafe {
        let section = mem::CreateFileMappingW(
            -1isize as _,
            std::ptr::null(),
            mem::PAGE_READWRITE | mem::SEC_COMMIT,
            0,
            len as u32,
            std::ptr::null(),
        );
        assert!(!section.is_null());
        let title = mem::MapViewOfFile(section, mem::FILE_MAP_WRITE, 0, 0, len)
            .Value
            .cast::<u8>();
        let ours = mem::MapViewOfFile(section, mem::FILE_MAP_WRITE, 0, 0, len)
            .Value
            .cast::<u8>();
        assert!(!title.is_null() && !ours.is_null());
        let opened = open(device, From::Part(ours as usize, len));
        println!(
            "membench: in place on the host GPU: a section's stretch mapped again at an address of ours, opened as a part: {}",
            hr(&opened)
        );
        let Ok(heap) = opened else { return };
        let placed = place(device, &heap, 0, len as u64);
        println!(
            "membench: in place on the host GPU:   a buffer placed over it: {}",
            hr(&placed)
        );
        let Ok(buffer) = placed else { return };
        let mut rig = Rig::new(device).expect("a queue of the probe's own");
        fill(title, len, 7);
        let reads = rig
            .fetch(&buffer, 0, len)
            .is_ok_and(|got| same(title, &got));
        let writes =
            rig.store(&buffer, 16, &[0xa5; 64]).is_ok() && same(title.add(16), &[0xa5; 64]);
        println!(
            "membench: in place on the host GPU:   the GPU reads the bytes the title's view shows: {reads}; its write shows there: {writes}; device removed: {:?}",
            crate::descriptor::host::device_removed()
        );
    }
}
