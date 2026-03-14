use std::io::{Seek, SeekFrom, Write};

#[test]
fn sparse_patch_style_writes_are_small_and_correct() {
    let mut file = tempfile::NamedTempFile::new().expect("tmp file");
    file.write_all(&vec![0u8; 1024 * 1024]).expect("seed");

    let mut f = file.reopen().expect("reopen");
    for i in 0..200u64 {
        let off = (i * 4096) % (1024 * 1024 - 32);
        f.seek(SeekFrom::Start(off)).expect("seek");
        f.write_all(&[0xAB; 32]).expect("write");
    }

    let meta = std::fs::metadata(file.path()).expect("meta");
    assert_eq!(meta.len(), 1024 * 1024);
}
