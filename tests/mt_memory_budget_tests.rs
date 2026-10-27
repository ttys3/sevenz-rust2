//! `set_mt_memory_budget` reaches the multi-threaded LZMA2 decoder: peak heap use,
//! measured by a counting global allocator, follows the budget. The one test in this
//! file owns the allocator's counters.

#![cfg(all(feature = "compress", feature = "util"))]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    io::{Cursor, Read},
    sync::atomic::{AtomicUsize, Ordering},
};

use sevenz_rust2::{encoder_options::Lzma2Options, *};

struct Allocator;

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(size: usize) {
    let current = CURRENT.fetch_add(size, Ordering::SeqCst) + size;
    PEAK.fetch_max(current, Ordering::SeqCst);
}

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grow(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grow(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, layout, size) };
        if !ptr.is_null() {
            if size > layout.size() {
                grow(size - layout.size());
            } else {
                CURRENT.fetch_sub(layout.size() - size, Ordering::SeqCst);
            }
        }
        ptr
    }
}

/// The most the heap held, over what it held before, while `f` ran.
fn heap_peak(f: impl FnOnce()) -> usize {
    let before = CURRENT.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    f();
    PEAK.load(Ordering::SeqCst) - before
}

const MIB: usize = 1024 * 1024;

/// Executable bytes and pseudo-random bytes in turn, 64 KiB each.
fn content(len: usize) -> Vec<u8> {
    let exe = std::fs::read("tests/resources/decompress_x86.exe").unwrap();
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    (0..len)
        .map(|i| {
            if (i / (64 * 1024)) % 2 == 0 {
                exe[i % exe.len()]
            } else {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            }
        })
        .collect()
}

/// Streams every entry through a fixed buffer, checking it against `expected`.
fn check_entries(
    each: impl FnOnce(&mut dyn FnMut(&ArchiveEntry, &mut dyn Read) -> Result<bool, Error>),
    expected: &[u8],
) {
    let mut buf = [0u8; 16 * 1024];
    let mut at = 0;
    each(&mut |entry, reader| {
        if entry.is_directory {
            return Ok(true);
        }
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                return Ok(true);
            }
            assert!(buf[..n] == expected[at..at + n], "output differs at {at}");
            at += n;
        }
    });
    assert_eq!(at, expected.len());
}

#[test]
fn the_budget_reaches_the_multi_threaded_lzma2_decoder() {
    // 12 work units of 1 MiB (the dictionary of level 1).
    let content = content(12 * MIB);
    let mut archive = Vec::new();
    {
        let mut writer = ArchiveWriter::new(Cursor::new(&mut archive)).unwrap();
        writer.set_content_methods(vec![Lzma2Options::from_level_mt(1, 4, MIB as u64).into()]);
        writer
            .push_archive_entry(ArchiveEntry::new_file("data.bin"), Some(content.as_slice()))
            .unwrap();
        writer.finish().unwrap();
    }

    let archive_reader = |budget: Option<u64>| {
        let mut reader =
            ArchiveReader::new(Cursor::new(archive.as_slice()), Password::empty()).unwrap();
        reader.set_thread_count(4);
        if let Some(budget) = budget {
            reader.set_mt_memory_budget(budget);
        }
        reader
    };

    let mut peaks = Vec::new();
    for budget in [None, Some(4 * MIB as u64), Some(0)] {
        let mut reader = archive_reader(budget);
        let through_reader = heap_peak(|| {
            check_entries(|each| reader.for_each_entries(each).unwrap(), &content);
        });

        let reader = archive_reader(None);
        let parsed = reader.archive().clone();
        let mut source = Cursor::new(archive.as_slice());
        let password = Password::empty();
        let through_block_decoder = heap_peak(|| {
            let mut decoder = BlockDecoder::new(4, 0, &parsed, &password, &mut source);
            if let Some(budget) = budget {
                decoder.set_mt_memory_budget(budget);
            }
            check_entries(
                |each| {
                    decoder
                        .for_each_entries(&mut |entry, reader| each(entry, reader))
                        .unwrap();
                },
                &content,
            );
        });
        println!(
            "budget {budget:?}: {through_reader} through ArchiveReader, {through_block_decoder} through BlockDecoder"
        );
        peaks.push((budget, through_reader, through_block_decoder));
    }

    let (_, unbounded, _) = peaks[0];
    for (budget, through_reader, through_block_decoder) in peaks {
        let Some(budget) = budget else { continue };
        // On top of the budget: the decoding stack, and one dictionary for the decoder
        // that takes over when a unit does not fit (budget 0).
        let bound = (budget as usize).max(MIB + 256 * 1024) + 512 * 1024;
        assert!(
            through_reader <= bound,
            "budget {budget}: {through_reader} > {bound}"
        );
        assert!(
            through_block_decoder <= bound,
            "budget {budget}: {through_block_decoder} > {bound}"
        );
        assert!(
            through_reader < unbounded,
            "budget {budget}: {through_reader} >= {unbounded}"
        );
    }
}
