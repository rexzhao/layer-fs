//! Packing entries into WinFsp's directory buffer: whole records only, 8-aligned, `Size` counted.
//!
//! A partial record is not a shorter entry — the OS reads the next record's name out of the middle of it.

use fs_winfsp::abi::{dir_info, FileInfo};
use fs_winfsp::dir_buffer::DirBuffer;

fn info(index: u64) -> FileInfo {
    FileInfo {
        file_attributes: 0x80,
        reparse_tag: 0,
        allocation_size: 0,
        file_size: index * 10,
        creation_time: 1,
        last_access_time: 2,
        last_write_time: 3,
        change_time: 4,
        index_number: index,
        hard_links: 0,
        ea_size: 0,
    }
}

fn units(name: &str) -> Vec<u16> {
    name.encode_utf16().collect()
}

/// Two records fit, the third does not: the buffer holds exactly the complete ones.
#[test]
fn only_whole_records_are_written() {
    let name = units("a.txt");
    let record = dir_info::aligned(dir_info::record_size(name.len()));
    let mut storage = vec![0u8; record * 2 + record / 2]; // room for two whole records and a bit
    let mut buffer = DirBuffer::new(&mut storage);

    assert!(buffer.push(&name, &info(1)), "cell first: fits");
    assert!(buffer.push(&name, &info(2)), "cell second: fits");
    assert!(
        !buffer.push(&name, &info(3)),
        "cell third: refused — a record that does not fit whole is not written at all"
    );
    assert_eq!(
        buffer.written(),
        record * 2,
        "cell written: exactly two whole records, never a partial third"
    );

    // The second record starts where the first one ends, 8-aligned, and its own header is intact.
    let second = &storage[record..record * 2];
    assert_eq!(
        u16::from_ne_bytes([second[0], second[1]]) as usize,
        dir_info::record_size(name.len()),
        "cell size field: the second record counts itself"
    );
    assert_eq!(
        u64::from_ne_bytes(
            second[dir_info::FILE_INFO_AT + 56..dir_info::FILE_INFO_AT + 64]
                .try_into()
                .expect("8 bytes"),
        ),
        2,
        "cell identity: index_number is at +56 **inside** the embedded FileInfo, i.e. absolute 8+56 = 64"
    );
    assert_eq!(
        &second[dir_info::NAME_AT..dir_info::NAME_AT + name.len() * 2],
        name.iter()
            .flat_map(|unit| unit.to_ne_bytes())
            .collect::<Vec<u8>>()
            .as_slice(),
        "cell name: the name sits right after the fixed header"
    );

    // Padding after an odd-length name is zeroed, not left as whatever the buffer held.
    let odd = units("odd");
    let odd_record = dir_info::aligned(dir_info::record_size(odd.len()));
    let mut storage = vec![0xAAu8; odd_record];
    let mut buffer = DirBuffer::new(&mut storage);
    assert!(buffer.push(&odd, &info(9)), "cell odd: fits");
    let tail = dir_info::record_size(odd.len());
    assert!(
        storage[tail..].iter().all(|byte| *byte == 0),
        "cell padding: zeroed — WinFsp may copy the padded span, and stale bytes there look like data"
    );
}
