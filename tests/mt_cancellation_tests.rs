//! `set_cancellation` reaches the multi-threaded LZMA2 decoder: once the flag is set, the
//! next read of an entry fails with an error that carries `lzma_rust2::DecoderCancelled`.

#![cfg(all(feature = "compress", feature = "util"))]

use std::{
    io::{self, Cursor, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use lzma_rust2::DecoderCancelled;
use sevenz_rust2::{encoder_options::Lzma2Options, *};

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

/// An archive of one entry in 1 MiB work units (the dictionary of level 1).
fn archive(content: &[u8]) -> Vec<u8> {
    let mut archive = Vec::new();
    let mut writer = ArchiveWriter::new(Cursor::new(&mut archive)).unwrap();
    writer.set_content_methods(vec![Lzma2Options::from_level_mt(1, 4, MIB as u64).into()]);
    writer
        .push_archive_entry(ArchiveEntry::new_file("data.bin"), Some(content))
        .unwrap();
    writer.finish().unwrap();
    archive
}

fn is_cancelled(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<DecoderCancelled>())
}

/// Reads the start of the entry, sets the flag, and returns what the next read reports.
fn read_until_cancelled(
    flag: &AtomicBool,
    reader: &mut dyn Read,
    expected: &[u8],
) -> io::Result<usize> {
    let mut buf = vec![0u8; 64 * 1024];
    reader.read_exact(&mut buf).unwrap();
    assert!(buf == expected[..buf.len()], "output differs");
    flag.store(true, Ordering::Relaxed);
    reader.read(&mut buf)
}

#[test]
fn cancellation_reaches_the_multi_threaded_lzma2_decoder() {
    let content = content(12 * MIB);
    let archive = archive(&content);

    let flag = Arc::new(AtomicBool::new(false));
    let mut reader =
        ArchiveReader::new(Cursor::new(archive.as_slice()), Password::empty()).unwrap();
    reader.set_thread_count(4);
    reader.set_cancellation(Arc::clone(&flag));
    let mut results = Vec::new();
    let _ = reader.for_each_entries(|_, entry| {
        results.push(read_until_cancelled(&flag, entry, &content));
        Ok(false)
    });
    assert_eq!(results.len(), 1);
    let error = results.pop().unwrap().unwrap_err();
    assert!(is_cancelled(&error), "through ArchiveReader: {error}");

    let flag = Arc::new(AtomicBool::new(false));
    let parsed = reader.archive().clone();
    let mut source = Cursor::new(archive.as_slice());
    let password = Password::empty();
    let mut decoder = BlockDecoder::new(4, 0, &parsed, &password, &mut source);
    decoder.set_cancellation(Arc::clone(&flag));
    let mut results = Vec::new();
    let _ = decoder.for_each_entries(&mut |_, entry| {
        results.push(read_until_cancelled(&flag, entry, &content));
        Ok(false)
    });
    assert_eq!(results.len(), 1);
    let error = results.pop().unwrap().unwrap_err();
    assert!(is_cancelled(&error), "through BlockDecoder: {error}");
}
