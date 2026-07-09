//! Fuzzes the blob store's two untrusted-input surfaces:
//! `hh_core::blob::fuzzing::fuzz_get_arbitrary_hash` (a hash string of any
//! shape must never panic `blob_path`'s byte slice or escape the blobs dir)
//! and `fuzz_decompress` (arbitrary on-disk bytes fed through zstd decode +
//! BLAKE3 verification must never panic, only error on corruption).

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
enum Input<'a> {
    Hash(&'a str),
    Bytes(&'a [u8]),
}

fuzz_target!(|input: Input| {
    match input {
        Input::Hash(h) => hh_core::blob::fuzzing::fuzz_get_arbitrary_hash(h),
        Input::Bytes(b) => hh_core::blob::fuzzing::fuzz_decompress(b),
    }
});
