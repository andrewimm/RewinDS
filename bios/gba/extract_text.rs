//! Extract the `.text` section of a 32-bit little-endian ARM ELF object and
//! write it, padded to 16 KiB, as a flat GBA BIOS image.
//!
//! This exists so the BIOS can be built with only Apple clang (the same
//! assembler the `arm` crate's fixtures use) — no cross-linker or objcopy. It
//! relies on `bios.s` being written so its assembled `.text` is position-
//! independent (see the header comment there); the presence of any `.text`
//! relocation section means that assumption was broken, and we refuse to emit a
//! silently-wrong image.
//!
//! Usage: extract_text <input.o> <output.bin>

use std::process::exit;

const BIOS_SIZE: usize = 0x4000; // 16 KiB

fn u16le(b: &[u8], off: usize) -> usize {
    u16::from_le_bytes([b[off], b[off + 1]]) as usize
}

fn u32le(b: &[u8], off: usize) -> usize {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]) as usize
}

fn fail(msg: &str) -> ! {
    eprintln!("extract_text: {msg}");
    exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        fail("usage: extract_text <input.o> <output.bin>");
    }
    let elf = std::fs::read(&args[1]).unwrap_or_else(|e| fail(&format!("read {}: {e}", args[1])));

    // ELF identification: magic, 32-bit class, little-endian, EM_ARM.
    if elf.len() < 0x34 || &elf[0..4] != b"\x7fELF" {
        fail("not an ELF file");
    }
    if elf[4] != 1 {
        fail("not a 32-bit ELF (ELFCLASS32 expected)");
    }
    if elf[5] != 1 {
        fail("not little-endian (ELFDATA2LSB expected)");
    }
    if u16le(&elf, 0x12) != 0x28 {
        fail("not an ARM object (EM_ARM expected)");
    }

    // Section header table.
    let e_shoff = u32le(&elf, 0x20);
    let e_shentsize = u16le(&elf, 0x2E);
    let e_shnum = u16le(&elf, 0x30);
    let e_shstrndx = u16le(&elf, 0x32);
    if e_shoff == 0 || e_shnum == 0 {
        fail("no section header table");
    }

    let section = |i: usize| -> &[u8] {
        let off = e_shoff + i * e_shentsize;
        &elf[off..off + e_shentsize]
    };
    let name_at = |strtab: &[u8], off: usize| -> String {
        let end = strtab[off..].iter().position(|&c| c == 0).unwrap_or(0);
        String::from_utf8_lossy(&strtab[off..off + end]).into_owned()
    };

    // Section-name string table (pointed to by e_shstrndx).
    let shstr = section(e_shstrndx);
    let shstr = &elf[u32le(shstr, 0x10)..u32le(shstr, 0x10) + u32le(shstr, 0x14)];

    let mut text: Option<Vec<u8>> = None;
    for i in 0..e_shnum {
        let sh = section(i);
        let name = name_at(shstr, u32le(sh, 0x00));
        // A relocation section for .text means unresolved absolute references
        // survived assembly — the flat extract would be wrong.
        if name == ".rel.text" || name == ".rela.text" {
            fail(".text has relocations; bios.s must be position-independent");
        }
        if name == ".text" {
            let off = u32le(sh, 0x10);
            let size = u32le(sh, 0x14);
            text = Some(elf[off..off + size].to_vec());
        }
    }

    let text = text.unwrap_or_else(|| fail("no .text section found"));
    if text.len() > BIOS_SIZE {
        fail(&format!(".text is {} bytes, exceeds 16 KiB", text.len()));
    }

    let mut image = text;
    image.resize(BIOS_SIZE, 0);
    std::fs::write(&args[2], &image).unwrap_or_else(|e| fail(&format!("write {}: {e}", args[2])));
}
