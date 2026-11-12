//! What a deferred `.dSYM` job needs from a linked Mach-O executable: its
//! `LC_UUID` and the object files its `N_OSO` debug map names.
//!
//! `dsymutil` reads DWARF from those objects, not from the executable. Cargo
//! rewrites them in place on the next build of the same unit, so a job that
//! runs after the wrapper returned must read link-time copies. This reader
//! lists them without spawning `dsymutil --symtab` and without reading the
//! executable's code: only the load commands, the symbol table and the few
//! strings `N_OSO` entries point at.
//!
//! Only thin little-endian 64-bit images are read (what rustc links for
//! `aarch64-apple-darwin` and `x86_64-apple-darwin`). Anything else is
//! `Ok(None)` and the caller keeps the synchronous path.

use anyhow::{Context as _, Result, bail, ensure};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MH_MAGIC_64_LE: [u8; 4] = [0xcf, 0xfa, 0xed, 0xfe];
const MACH_HEADER_64_LEN: u64 = 32;
const LC_SYMTAB: u32 = 0x2;
const LC_UUID: u32 = 0x1b;
const N_OSO: u8 = 0x66;
const NLIST_64_LEN: usize = 16;
/// Symbols read per chunk, so a large executable's table is never held whole.
const SYMBOLS_PER_CHUNK: usize = 1 << 14;
/// Longest `N_OSO` string read: a path plus an archive member name.
const MAX_OSO_LEN: u64 = 8192;

/// The parts of an executable's debug map a deferred `.dSYM` job needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DebugMap {
    pub(crate) uuid: [u8; 16],
    /// Each distinct `N_OSO` string, in symbol-table order. An archive
    /// member is spelled `archive(member)`; see [`oso_file`].
    pub(crate) objects: Vec<String>,
}

/// Read `path`'s debug map. `Ok(None)` when it is not a thin little-endian
/// 64-bit Mach-O image with an `LC_UUID`.
pub(crate) fn read_debug_map_at(path: &Path) -> Result<Option<DebugMap>> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    read_debug_map(&mut file)
        .with_context(|| format!("reading the debug map of {}", path.display()))
}

/// [`read_debug_map_at`] over any seekable reader.
pub(crate) fn read_debug_map(file: &mut (impl Read + Seek)) -> Result<Option<DebugMap>> {
    let mut header = [0_u8; MACH_HEADER_64_LEN as usize];
    if file.read_exact(&mut header).is_err() || header[..4] != MH_MAGIC_64_LE {
        return Ok(None);
    }
    let ncmds = le32(&header, 16);
    let sizeofcmds = le32(&header, 20) as usize;
    let mut commands = vec![0_u8; sizeofcmds];
    file.read_exact(&mut commands)
        .context("load commands run past the end of the file")?;

    let mut uuid = None;
    let mut symtab = None;
    let mut offset = 0_usize;
    for _ in 0..ncmds {
        ensure!(
            offset + 8 <= commands.len(),
            "load command table is truncated"
        );
        let cmd = le32(&commands, offset);
        let cmdsize = le32(&commands, offset + 4) as usize;
        ensure!(
            cmdsize >= 8 && offset + cmdsize <= commands.len(),
            "load command {cmd:#x} has an invalid size {cmdsize}"
        );
        let command = &commands[offset..offset + cmdsize];
        match cmd {
            LC_UUID => {
                ensure!(cmdsize == 24, "LC_UUID has size {cmdsize}");
                uuid = Some(<[u8; 16]>::try_from(&command[8..24])?);
            }
            LC_SYMTAB => {
                ensure!(cmdsize == 24, "LC_SYMTAB has size {cmdsize}");
                symtab = Some(Symtab {
                    symoff: u64::from(le32(command, 8)),
                    nsyms: le32(command, 12) as usize,
                    stroff: u64::from(le32(command, 16)),
                    strsize: u64::from(le32(command, 20)),
                });
            }
            _ => {}
        }
        offset += cmdsize;
    }
    let Some(uuid) = uuid else {
        return Ok(None);
    };
    let objects = match symtab {
        Some(symtab) => read_oso_strings(file, symtab)?,
        None => Vec::new(),
    };
    Ok(Some(DebugMap { uuid, objects }))
}

#[derive(Clone, Copy)]
struct Symtab {
    symoff: u64,
    nsyms: usize,
    stroff: u64,
    strsize: u64,
}

/// The distinct strings of every `N_OSO` symbol, in table order.
fn read_oso_strings(file: &mut (impl Read + Seek), symtab: Symtab) -> Result<Vec<String>> {
    let mut string_offsets = Vec::new();
    let mut chunk = vec![0_u8; SYMBOLS_PER_CHUNK * NLIST_64_LEN];
    let mut read = 0;
    file.seek(SeekFrom::Start(symtab.symoff))?;
    while read < symtab.nsyms {
        let count = (symtab.nsyms - read).min(SYMBOLS_PER_CHUNK);
        let bytes = &mut chunk[..count * NLIST_64_LEN];
        file.read_exact(bytes)
            .context("symbol table runs past the end of the file")?;
        for symbol in bytes.as_chunks::<NLIST_64_LEN>().0 {
            if symbol[4] == N_OSO {
                string_offsets.push(u64::from(le32(symbol, 0)));
            }
        }
        read += count;
    }

    let mut objects: Vec<String> = Vec::new();
    for string_offset in string_offsets {
        let object = read_string(file, symtab, string_offset)?;
        if !objects.contains(&object) {
            objects.push(object);
        }
    }
    Ok(objects)
}

/// The NUL-terminated string at `offset` in the string table.
fn read_string(file: &mut (impl Read + Seek), symtab: Symtab, offset: u64) -> Result<String> {
    ensure!(
        offset < symtab.strsize,
        "N_OSO string offset {offset} is outside the string table"
    );
    let len = (symtab.strsize - offset).min(MAX_OSO_LEN);
    file.seek(SeekFrom::Start(symtab.stroff + offset))?;
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(len)
        .read_to_end(&mut bytes)
        .context("reading an N_OSO string")?;
    let Some(end) = bytes.iter().position(|&byte| byte == 0) else {
        bail!("N_OSO string at {offset} is not terminated");
    };
    bytes.truncate(end);
    String::from_utf8(bytes).context("N_OSO string is not UTF-8")
}

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
}

/// The file an `N_OSO` string names: the archive of an `archive(member)`
/// entry, the string itself otherwise. Cargo rewrites an rlib in place as
/// readily as an object, so a deferred job captures the whole archive.
pub(crate) fn oso_file(entry: &str) -> &str {
    match entry
        .strip_suffix(')')
        .and_then(|rest| rest.rfind('(').map(|open| &rest[..open]))
    {
        Some(archive) if !archive.is_empty() => archive,
        _ => entry,
    }
}

/// `uuid` as 32 lowercase hex digits.
pub(crate) fn uuid_hex(uuid: &[u8; 16]) -> String {
    uuid.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    /// A minimal thin 64-bit little-endian image: `LC_UUID` (when given),
    /// `LC_SYMTAB`, then the symbols `(type, string)` and their strings.
    pub(crate) fn synthetic_macho(uuid: Option<[u8; 16]>, symbols: &[(u8, &str)]) -> Vec<u8> {
        let uuid_len = if uuid.is_some() { 24 } else { 0 };
        let commands_len = uuid_len + 24;
        let symoff = 32 + commands_len;
        let stroff = symoff + symbols.len() * NLIST_64_LEN;
        let mut strings = vec![0_u8];
        let mut nlist = Vec::new();
        for (kind, name) in symbols {
            let index = strings.len() as u32;
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
            nlist.extend_from_slice(&index.to_le_bytes());
            nlist.push(*kind);
            nlist.extend_from_slice(&[0; 11]);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&MH_MAGIC_64_LE);
        out.extend_from_slice(&0x0100_000c_u32.to_le_bytes());
        out.extend_from_slice(&0_u32.to_le_bytes());
        out.extend_from_slice(&2_u32.to_le_bytes()); // MH_EXECUTE
        out.extend_from_slice(&(1 + u32::from(uuid.is_some())).to_le_bytes());
        out.extend_from_slice(&(commands_len as u32).to_le_bytes());
        out.extend_from_slice(&[0; 8]);
        if let Some(uuid) = uuid {
            out.extend_from_slice(&LC_UUID.to_le_bytes());
            out.extend_from_slice(&24_u32.to_le_bytes());
            out.extend_from_slice(&uuid);
        }
        for value in [
            LC_SYMTAB,
            24,
            symoff as u32,
            symbols.len() as u32,
            stroff as u32,
            strings.len() as u32,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&nlist);
        out.extend_from_slice(&strings);
        out
    }

    const UUID: [u8; 16] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32,
        0x10,
    ];

    #[test]
    fn reads_the_uuid_and_each_distinct_oso_string_in_order() {
        let image = synthetic_macho(
            Some(UUID),
            &[
                (0x64, "/src/main.rs"), // N_SO
                (N_OSO, "deps/b.rcgu.o"),
                (0x0f, "_main"),
                (N_OSO, "deps/liba.rlib(a.o)"),
                (N_OSO, "deps/b.rcgu.o"),
                (N_OSO, "/toolchain/libstd.rlib(std.o)"),
            ],
        );
        let map = read_debug_map(&mut Cursor::new(image)).unwrap().unwrap();
        assert_eq!(map.uuid, UUID);
        assert_eq!(
            map.objects,
            [
                "deps/b.rcgu.o",
                "deps/liba.rlib(a.o)",
                "/toolchain/libstd.rlib(std.o)"
            ]
        );
    }

    #[test]
    fn reads_symbol_tables_larger_than_one_chunk() {
        let filler = vec![(0x0f_u8, "_f"); SYMBOLS_PER_CHUNK + 3];
        let mut symbols = filler.clone();
        symbols.push((N_OSO, "deps/late.o"));
        symbols.extend(filler);
        let image = synthetic_macho(Some(UUID), &symbols);
        let map = read_debug_map(&mut Cursor::new(image)).unwrap().unwrap();
        assert_eq!(map.objects, ["deps/late.o"]);
    }

    #[test]
    fn other_images_are_not_read() {
        assert_eq!(read_debug_map(&mut Cursor::new(Vec::new())).unwrap(), None);
        let mut big_endian = synthetic_macho(Some(UUID), &[]);
        big_endian[..4].copy_from_slice(&[0xfe, 0xed, 0xfa, 0xcf]);
        assert_eq!(read_debug_map(&mut Cursor::new(big_endian)).unwrap(), None);
        let mut fat = synthetic_macho(Some(UUID), &[]);
        fat[..4].copy_from_slice(&[0xca, 0xfe, 0xba, 0xbe]);
        assert_eq!(read_debug_map(&mut Cursor::new(fat)).unwrap(), None);
        let no_uuid = synthetic_macho(None, &[(N_OSO, "deps/a.o")]);
        assert_eq!(read_debug_map(&mut Cursor::new(no_uuid)).unwrap(), None);
    }

    #[test]
    fn an_image_without_a_symbol_table_has_no_objects() {
        let mut image = synthetic_macho(Some(UUID), &[]);
        // Turn LC_SYMTAB into an unknown command of the same size.
        image[56..60].copy_from_slice(&0x7777_u32.to_le_bytes());
        let map = read_debug_map(&mut Cursor::new(image)).unwrap().unwrap();
        assert!(map.objects.is_empty());
    }

    #[test]
    fn malformed_images_are_errors() {
        let image = synthetic_macho(Some(UUID), &[(N_OSO, "deps/a.o")]);

        let mut truncated = image.clone();
        truncated.truncate(40);
        assert!(read_debug_map(&mut Cursor::new(truncated)).is_err());

        let mut short_command = image.clone();
        short_command[36..40].copy_from_slice(&4_u32.to_le_bytes());
        assert!(read_debug_map(&mut Cursor::new(short_command)).is_err());

        let mut long_command = image.clone();
        long_command[36..40].copy_from_slice(&64_u32.to_le_bytes());
        assert!(read_debug_map(&mut Cursor::new(long_command)).is_err());

        let mut many_commands = image.clone();
        many_commands[16..20].copy_from_slice(&3_u32.to_le_bytes());
        assert!(read_debug_map(&mut Cursor::new(many_commands)).is_err());

        let mut odd_uuid = image.clone();
        odd_uuid[36..40].copy_from_slice(&32_u32.to_le_bytes());
        odd_uuid[20..24].copy_from_slice(&56_u32.to_le_bytes());
        assert!(read_debug_map(&mut Cursor::new(odd_uuid)).is_err());

        let mut odd_symtab = image.clone();
        odd_symtab[60..64].copy_from_slice(&16_u32.to_le_bytes());
        assert!(read_debug_map(&mut Cursor::new(odd_symtab)).is_err());

        let mut missing_symbols = image.clone();
        missing_symbols[68..72].copy_from_slice(&1000_u32.to_le_bytes());
        assert!(read_debug_map(&mut Cursor::new(missing_symbols)).is_err());

        let mut outside_strings = image.clone();
        let strsize = le32(&outside_strings, 76);
        outside_strings[80..84].copy_from_slice(&strsize.to_le_bytes());
        assert!(read_debug_map(&mut Cursor::new(outside_strings)).is_err());

        let mut unterminated = image.clone();
        unterminated.pop();
        assert!(read_debug_map(&mut Cursor::new(unterminated)).is_err());

        let mut not_utf8 = image;
        let last = not_utf8.len() - 2;
        not_utf8[last] = 0xff;
        assert!(read_debug_map(&mut Cursor::new(not_utf8)).is_err());
    }

    #[test]
    fn an_oso_string_names_an_archive_or_an_object() {
        assert_eq!(
            oso_file("deps/liba-1.rlib(a-1.a.cgu.0.rcgu.o)"),
            "deps/liba-1.rlib"
        );
        assert_eq!(oso_file("/t/lib(x).rlib(m.o)"), "/t/lib(x).rlib");
        assert_eq!(oso_file("deps/a.rcgu.o"), "deps/a.rcgu.o");
        assert_eq!(oso_file("deps/a(.o"), "deps/a(.o");
        assert_eq!(oso_file("(member)"), "(member)");
        assert_eq!(oso_file("x)"), "x)");
    }

    #[test]
    fn uuids_print_as_lowercase_hex() {
        assert_eq!(uuid_hex(&UUID), "0123456789abcdeffedcba9876543210");
    }
}
