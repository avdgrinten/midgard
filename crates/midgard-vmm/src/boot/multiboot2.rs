//! Boot of a multiboot2 kernel (e.g., eir, the boot stub of Managarm): we load the
//! segments of its ELF image, write the boot information and enter the kernel in 32-bit
//! protected mode (the specification's "I386 machine state").
//!
//! Of the header's tags, we only check that the kernel does not require to be loaded in
//! another way than as an ELF image. Requests that we cannot meet (e.g., for a
//! framebuffer, which eir does not mark as optional) are ignored; the kernel sees that
//! the boot information has no such tag.

use anyhow::{Context, Result, bail};
use midgard_hv::x86::{CR0_ET, CR0_PE};
use midgard_hv::{GuestMemory, Regs};
use zerocopy::little_endian::{U16, U32};
use zerocopy::{FromBytes, Immutable, KnownLayout};

use super::{initial_registers, memory_map, segment};
use crate::platform::HIGH_MEMORY;

/// The boot information is below 1 MiB (kernels are loaded above), and ends below
/// [`INFO_END`]: Managarm starts its other CPUs through a trampoline at that address.
const INFO_ADDRESS: u64 = 0x8000;
const INFO_END: u64 = 0x1_0000;

const PAGE_SIZE: u64 = 0x1000;

pub(super) const ELF_MAGIC: &[u8; 4] = b"\x7FELF";
const ELF_CLASS_32: u8 = 1;
const ELF_DATA_LITTLE_ENDIAN: u8 = 1;
const ELF_MACHINE_386: u16 = 3;
const ELF_SEGMENT_LOAD: u32 = 1;

/// The header is in the first [`HEADER_SEARCH`] bytes of the image, 8-byte aligned.
const HEADER_MAGIC: u32 = 0xE852_50D6;
const HEADER_SEARCH: usize = 32768;
const HEADER_ARCHITECTURE_I386: u32 = 0;
const HEADER_TAG_END: u16 = 0;
const HEADER_TAG_ADDRESS: u16 = 2;
const HEADER_TAG_ENTRY_ADDRESS: u16 = 3;
/// A flag of a header tag: the boot loader may ignore the tag.
const HEADER_TAG_OPTIONAL: u16 = 1 << 0;

/// What the kernel finds in EAX.
const BOOTLOADER_MAGIC: u32 = 0x36D7_6289;

const TAG_END: u32 = 0;
const TAG_CMDLINE: u32 = 1;
const TAG_MODULE: u32 = 3;
const TAG_MEMORY_MAP: u32 = 6;
const TAG_ACPI_NEW: u32 = 15;

// Selectors of the code and data segments; there is no GDT behind them (the kernel must
// load its own before it loads a segment register).
const BOOT_CS: u16 = 0x08;
const BOOT_DS: u16 = 0x10;

/// Elf32_Ehdr.
#[derive(FromBytes, Immutable, KnownLayout)]
#[repr(C)]
struct ElfHeader {
    magic: [u8; 4],
    class: u8,
    data: u8,
    _ident: [u8; 10],
    type_: U16,
    machine: U16,
    version: U32,
    entry: U32,
    phoff: U32,
    shoff: U32,
    flags: U32,
    ehsize: U16,
    phentsize: U16,
    phnum: U16,
    shentsize: U16,
    shnum: U16,
    shstrndx: U16,
}

/// Elf32_Phdr.
#[derive(FromBytes, Immutable, KnownLayout)]
#[repr(C)]
struct ProgramHeader {
    type_: U32,
    offset: U32,
    vaddr: U32,
    paddr: U32,
    filesz: U32,
    memsz: U32,
    flags: U32,
    align: U32,
}

/// The fixed part of the multiboot2 header (its tags follow).
#[derive(FromBytes, Immutable, KnownLayout)]
#[repr(C)]
struct Header {
    magic: U32,
    architecture: U32,
    length: U32,
    checksum: U32,
}

/// The fixed part of a tag of the multiboot2 header.
#[derive(FromBytes, Immutable, KnownLayout)]
#[repr(C)]
struct HeaderTag {
    type_: U16,
    flags: U16,
    size: U32,
}

const _: () = {
    assert!(size_of::<ElfHeader>() == 52);
    assert!(size_of::<ProgramHeader>() == 32);
};

/// Finds the multiboot2 header and checks that we can boot the kernel as it asks.
fn check_header(kernel: &[u8]) -> Result<()> {
    let search = &kernel[..kernel.len().min(HEADER_SEARCH)];
    let found = (0..search.len()).step_by(8).find_map(|offset| {
        let (header, _) = Header::ref_from_prefix(&search[offset..]).ok()?;
        let sum = (header.magic.get())
            .wrapping_add(header.architecture.get())
            .wrapping_add(header.length.get())
            .wrapping_add(header.checksum.get());
        (header.magic.get() == HEADER_MAGIC && sum == 0).then_some((offset, header))
    });
    let Some((offset, header)) = found else {
        bail!("kernel is an ELF image without a multiboot2 header");
    };
    if header.architecture.get() != HEADER_ARCHITECTURE_I386 {
        bail!("multiboot2 kernel is not for i386");
    }
    let tags = (header.length.get() as usize)
        .checked_sub(size_of::<Header>())
        .and_then(|length| {
            let start = offset + size_of::<Header>();
            kernel.get(start..start + length)
        })
        .context("multiboot2 header is truncated")?;

    let mut offset = 0;
    while let Some((tag, _)) =
        (tags.get(offset..)).and_then(|tag| HeaderTag::ref_from_prefix(tag).ok())
    {
        match tag.type_.get() {
            HEADER_TAG_END => break,
            // Both replace what the ELF headers say.
            HEADER_TAG_ADDRESS | HEADER_TAG_ENTRY_ADDRESS
                if tag.flags.get() & HEADER_TAG_OPTIONAL == 0 =>
            {
                bail!("multiboot2 kernels that are not loaded as ELF images are not supported")
            }
            _ => {}
        }
        if (tag.size.get() as usize) < size_of::<HeaderTag>() {
            bail!("multiboot2 header has an invalid tag");
        }
        offset += (tag.size.get() as usize).next_multiple_of(8);
    }
    Ok(())
}

/// Loads the segments of the ELF image at their physical addresses. Returns the entry
/// point (its physical address) and the end of the kernel.
fn load_segments(memory: &GuestMemory, kernel: &[u8]) -> Result<(u64, u64)> {
    let (header, _) = ElfHeader::ref_from_prefix(kernel)
        .ok()
        .filter(|(header, _)| &header.magic == ELF_MAGIC)
        .context("kernel is not an ELF image")?;
    if header.class != ELF_CLASS_32
        || header.data != ELF_DATA_LITTLE_ENDIAN
        || header.machine.get() != ELF_MACHINE_386
    {
        bail!("multiboot2 kernel is not a 32-bit x86 ELF image");
    }

    let entry = u64::from(header.entry.get());
    let mut physical_entry = None;
    // The memory of the segments that we loaded (their first addresses and the ones after
    // their last).
    let mut loaded: Vec<(u64, u64)> = Vec::new();
    for index in 0..header.phnum.get() as usize {
        let offset = header.phoff.get() as usize + index * header.phentsize.get() as usize;
        let (segment, _) = (kernel.get(offset..))
            .and_then(|bytes| ProgramHeader::ref_from_prefix(bytes).ok())
            .context("kernel has a truncated program header")?;
        let size = u64::from(segment.memsz.get());
        if segment.type_.get() != ELF_SEGMENT_LOAD || size == 0 {
            continue;
        }
        let address = u64::from(segment.paddr.get());
        let offset = segment.offset.get() as usize;
        let data = (kernel.get(offset..))
            .and_then(|bytes| bytes.get(..segment.filesz.get() as usize))
            .context("kernel has a truncated segment")?;
        if data.len() as u64 > size {
            bail!("kernel has a segment with more data than memory");
        }
        // The boot information and the legacy areas are below.
        if address < HIGH_MEMORY {
            bail!("kernel segment at {address:#x} is below 1 MiB");
        }
        let end = address + size;
        if end > memory.size() {
            bail!("not enough guest memory for the kernel");
        }
        if loaded
            .iter()
            .any(|&(other, other_end)| address < other_end && other < end)
        {
            bail!("kernel has overlapping segments");
        }
        // Guest memory is zeroed, which is what the rest of the segment (the BSS) needs;
        // no other segment writes there.
        memory
            .write(address, data)
            .context("failed to load the kernel")?;
        loaded.push((address, end));

        // The entry point is a virtual address, but the kernel is entered where it was
        // loaded: like GRUB, we translate through the segment that contains it.
        let virtual_address = u64::from(segment.vaddr.get());
        if (virtual_address..virtual_address + size).contains(&entry) {
            physical_entry = Some(entry - virtual_address + address);
        }
    }
    let Some(end) = loaded.iter().map(|&(_, end)| end).max() else {
        bail!("kernel has no segments to load");
    };
    let entry = physical_entry.context("kernel's entry point is not in a segment")?;
    Ok((entry, end))
}

/// The boot information: a list of tags behind its total size (and a reserved field).
struct BootInfo(Vec<u8>);

impl BootInfo {
    fn new() -> Self {
        Self(vec![0; 8])
    }

    fn tag(&mut self, type_: u32, payload: &[u8]) {
        self.0.extend(type_.to_le_bytes());
        self.0.extend((8 + payload.len() as u32).to_le_bytes());
        self.0.extend(payload);
        // Tags are 8-byte aligned (their sizes do not include the padding).
        self.0.resize(self.0.len().next_multiple_of(8), 0);
    }

    fn finish(mut self) -> Vec<u8> {
        self.tag(TAG_END, &[]);
        let size = self.0.len() as u32;
        self.0[..4].copy_from_slice(&size.to_le_bytes());
        self.0
    }
}

/// The boot information for the kernel; `module` is where the initrd is (its first
/// address and the one after its last).
fn boot_info(
    memory: &GuestMemory,
    cmdline: &str,
    module: Option<(u32, u32)>,
    rsdp: &[u8],
) -> Vec<u8> {
    let mut info = BootInfo::new();

    let mut string = cmdline.as_bytes().to_vec();
    string.push(0);
    info.tag(TAG_CMDLINE, &string);

    if let Some((start, end)) = module {
        let mut payload = Vec::new();
        payload.extend(start.to_le_bytes());
        payload.extend(end.to_le_bytes());
        // The module's command line.
        payload.extend(b"initrd\0");
        info.tag(TAG_MODULE, &payload);
    }

    let mut payload = Vec::new();
    // The size and the version of the entries.
    payload.extend(24u32.to_le_bytes());
    payload.extend(0u32.to_le_bytes());
    for (address, size, type_) in memory_map(memory) {
        payload.extend(address.to_le_bytes());
        payload.extend(size.to_le_bytes());
        payload.extend(type_.to_le_bytes());
        payload.extend(0u32.to_le_bytes());
    }
    info.tag(TAG_MEMORY_MAP, &payload);

    // A copy of the RSDP.
    info.tag(TAG_ACPI_NEW, rsdp);
    info.finish()
}

/// Loads the kernel (and the initrd as its only module) and writes the boot information;
/// returns the registers with which the boot CPU enters the kernel. `rsdp` is the ACPI
/// RSDP, which the boot information has a copy of.
pub(crate) fn load(
    memory: &GuestMemory,
    kernel: &[u8],
    initrd: Option<&[u8]>,
    cmdline: &str,
    rsdp: &[u8],
) -> Result<Regs> {
    check_header(kernel)?;
    let (entry, kernel_end) = load_segments(memory, kernel)?;

    let module = match initrd {
        Some(initrd) => {
            // Modules are page-aligned; like other boot loaders, we put them behind the
            // kernel.
            let start = kernel_end.next_multiple_of(PAGE_SIZE);
            let end = start + initrd.len() as u64;
            if end > memory.size() {
                bail!("not enough guest memory for the initrd");
            }
            memory
                .write(start, initrd)
                .context("failed to load the initrd")?;
            Some((start as u32, end as u32))
        }
        None => None,
    };

    let info = boot_info(memory, cmdline, module, rsdp);
    if INFO_ADDRESS + info.len() as u64 > INFO_END {
        bail!(
            "kernel command line is too long (the boot information has {} bytes too many)",
            INFO_ADDRESS + info.len() as u64 - INFO_END
        );
    }
    memory
        .write(INFO_ADDRESS, &info)
        .context("failed to write the boot information")?;

    Ok(entry_registers(entry))
}

/// The initial register state of the boot CPU: 32-bit protected mode without paging,
/// flat segments, the magic value in EAX and EBX pointing to the boot information.
fn entry_registers(entry: u64) -> Regs {
    let mut regs = initial_registers();
    regs.basic.rip = entry;
    regs.basic.rax = u64::from(BOOTLOADER_MAGIC);
    regs.basic.rbx = INFO_ADDRESS;

    // Code: execute/read, accessed. Data: read/write, accessed.
    regs.cs = segment(BOOT_CS, 0xB, 1, 0, 1);
    let data = segment(BOOT_DS, 0x3, 1, 0, 1);
    regs.ds = data;
    regs.es = data;
    regs.fs = data;
    regs.gs = data;
    regs.ss = data;

    regs.cr0 = CR0_PE | CR0_ET;
    regs
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMORY_SIZE: usize = 4 << 20;
    const LOAD_ADDRESS: u32 = 0x10_0000;
    const ENTRY: u32 = LOAD_ADDRESS + 0x20;

    fn header(tags: &[u8]) -> Vec<u8> {
        let length = (size_of::<Header>() + tags.len()) as u32;
        let checksum = 0u32.wrapping_sub(HEADER_MAGIC).wrapping_sub(length);
        let mut header = Vec::new();
        for field in [HEADER_MAGIC, HEADER_ARCHITECTURE_I386, length, checksum] {
            header.extend(field.to_le_bytes());
        }
        header.extend(tags);
        header
    }

    fn header_tag(type_: u16, flags: u16, payload: &[u8]) -> Vec<u8> {
        let mut tag = Vec::new();
        tag.extend(type_.to_le_bytes());
        tag.extend(flags.to_le_bytes());
        tag.extend((8 + payload.len() as u32).to_le_bytes());
        tag.extend(payload);
        tag.resize(tag.len().next_multiple_of(8), 0);
        tag
    }

    /// A header with `tags` in front of the end tag.
    fn header_with(tags: &[u8]) -> Vec<u8> {
        header(&[tags, &header_tag(HEADER_TAG_END, 0, &[])].concat())
    }

    /// A segment of a test image: where it is linked and where it is loaded, its data
    /// and its size in memory.
    struct TestSegment<'a> {
        vaddr: u32,
        paddr: u32,
        data: &'a [u8],
        memsz: u32,
    }

    /// An ELF image; the data of its segments starts at 8-byte aligned offsets.
    fn elf(entry: u32, segments: &[TestSegment]) -> Vec<u8> {
        let mut image = Vec::new();
        image.extend(ELF_MAGIC);
        image.extend([ELF_CLASS_32, ELF_DATA_LITTLE_ENDIAN, 1]);
        image.resize(16, 0);
        // An executable.
        image.extend(2u16.to_le_bytes());
        image.extend(ELF_MACHINE_386.to_le_bytes());
        image.extend(1u32.to_le_bytes());
        image.extend(entry.to_le_bytes());
        image.extend((size_of::<ElfHeader>() as u32).to_le_bytes());
        image.extend([0; 8]);
        image.extend((size_of::<ElfHeader>() as u16).to_le_bytes());
        image.extend((size_of::<ProgramHeader>() as u16).to_le_bytes());
        image.extend((segments.len() as u16).to_le_bytes());
        image.extend([0; 6]);
        assert_eq!(image.len(), size_of::<ElfHeader>());

        let headers = size_of::<ElfHeader>() + segments.len() * size_of::<ProgramHeader>();
        let mut offset = headers.next_multiple_of(8);
        for segment in segments {
            let fields = [
                ELF_SEGMENT_LOAD,
                offset as u32,
                segment.vaddr,
                segment.paddr,
                segment.data.len() as u32,
                segment.memsz,
                7,
                0x1000,
            ];
            for field in fields {
                image.extend(field.to_le_bytes());
            }
            offset = (offset + segment.data.len()).next_multiple_of(8);
        }
        for segment in segments {
            image.resize(image.len().next_multiple_of(8), 0);
            image.extend(segment.data);
        }
        image
    }

    /// An ELF image with one segment (`data`, and as much BSS) at [`LOAD_ADDRESS`].
    fn image(data: &[u8]) -> Vec<u8> {
        let segment = TestSegment {
            vaddr: LOAD_ADDRESS,
            paddr: LOAD_ADDRESS,
            data,
            memsz: 2 * data.len() as u32,
        };
        elf(ENTRY, &[segment])
    }

    fn read(memory: &GuestMemory, address: u64, size: usize) -> Vec<u8> {
        let mut bytes = vec![0; size];
        memory.read(address, &mut bytes).unwrap();
        bytes
    }

    /// The boot information that [`load`] wrote.
    fn written_info(memory: &GuestMemory) -> Vec<u8> {
        let size = memory.read_volatile::<u32>(INFO_ADDRESS).unwrap();
        read(memory, INFO_ADDRESS, size as usize)
    }

    /// The tags of the boot information: their types and payloads.
    fn tags(info: &[u8]) -> Vec<(u32, &[u8])> {
        let word = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().unwrap());
        assert_eq!(word(0) as usize, info.len());
        let mut tags = Vec::new();
        let mut offset = 8;
        while offset < info.len() {
            let size = word(offset + 4) as usize;
            assert!(size >= 8);
            tags.push((word(offset), &info[offset + 8..offset + size]));
            offset += size.next_multiple_of(8);
        }
        tags
    }

    fn types(tags: &[(u32, &[u8])]) -> Vec<u32> {
        tags.iter().map(|(type_, _)| *type_).collect()
    }

    /// The error with which [`load`] rejects `image` (without an initrd).
    fn load_error(image: &[u8]) -> String {
        let memory = GuestMemory::detached(MEMORY_SIZE);
        let error = load(&memory, image, None, "", &[]).unwrap_err();
        error.to_string()
    }

    #[test]
    fn loads_kernel_and_initrd() {
        let memory = GuestMemory::detached(MEMORY_SIZE);
        // eir's header: a framebuffer request that is not optional.
        let mut data = header_with(&header_tag(5, 0, &[0; 12]));
        data.resize(0x1234, 0xAA);
        let initrd = [0x55; 0x1800];
        let rsdp = [0x77; 36];
        let regs = load(&memory, &image(&data), Some(&initrd), "serial", &rsdp).unwrap();

        assert_eq!(regs.basic.rip, u64::from(ENTRY));
        assert_eq!(regs.basic.rax, u64::from(BOOTLOADER_MAGIC));
        assert_eq!(regs.basic.rbx, INFO_ADDRESS);
        assert_eq!(regs.cr0, CR0_PE | CR0_ET);
        assert_eq!(regs.efer, 0);
        assert_eq!((regs.cs.db, regs.cs.l), (1, 0));

        let address = u64::from(LOAD_ADDRESS);
        assert_eq!(read(&memory, address, data.len()), data);
        // The BSS ends at 0x2468; the initrd is on the next page.
        let module = address + 0x3000;
        assert_eq!(read(&memory, module, initrd.len()), initrd);

        let info = written_info(&memory);
        let tags = tags(&info);
        assert_eq!(
            types(&tags),
            [
                TAG_CMDLINE,
                TAG_MODULE,
                TAG_MEMORY_MAP,
                TAG_ACPI_NEW,
                TAG_END
            ]
        );
        assert_eq!(tags[0].1, b"serial\0");
        let mut expected = Vec::new();
        expected.extend((module as u32).to_le_bytes());
        expected.extend((module as u32 + initrd.len() as u32).to_le_bytes());
        expected.extend(b"initrd\0");
        assert_eq!(tags[1].1, expected);
        assert_eq!(tags[3].1, rsdp);

        // The entries of the memory map have 24 bytes (and version 0).
        let (map_header, entries) = tags[2].1.split_at(8);
        assert_eq!(map_header, [24, 0, 0, 0, 0, 0, 0, 0]);
        let (entries, rest) = entries.as_chunks::<24>();
        assert!(rest.is_empty());
        let entries: Vec<(u64, u64, u64)> = entries
            .iter()
            .map(|entry| {
                let field = |index: usize| {
                    u64::from_le_bytes(entry[8 * index..8 * index + 8].try_into().unwrap())
                };
                // The type, and the reserved field behind it.
                (field(0), field(1), field(2))
            })
            .collect();
        assert_eq!(
            entries,
            [
                (0, 0xA_0000, 1),
                (0xA_0000, 0x6_0000, 2),
                (0x10_0000, MEMORY_SIZE as u64 - 0x10_0000, 1),
                (0xF000_0000, 0x10_0000, 2),
            ]
        );
    }

    #[test]
    fn boots_without_an_initrd() {
        let memory = GuestMemory::detached(MEMORY_SIZE);
        load(&memory, &image(&header_with(&[])), None, "", &[]).unwrap();
        let info = written_info(&memory);
        assert_eq!(
            types(&tags(&info)),
            [TAG_CMDLINE, TAG_MEMORY_MAP, TAG_ACPI_NEW, TAG_END]
        );
    }

    #[test]
    fn finds_the_header() {
        let memory = GuestMemory::detached(MEMORY_SIZE);
        let load = |data: &[u8]| load(&memory, &image(data), None, "", &[]);
        let missing = |data: &[u8]| {
            let error = load(data).unwrap_err().to_string();
            assert!(error.contains("without a multiboot2 header"), "{error}");
        };
        missing(&[0; 64]);

        // A header with a wrong checksum (and a tag that we would reject) is none.
        let mut invalid = header_with(&header_tag(HEADER_TAG_ADDRESS, 0, &[0; 16]));
        invalid[12] ^= 1;
        missing(&invalid);
        // The search goes on behind it.
        load(&[invalid, header_with(&[])].concat()).unwrap();

        // The header is 8-byte aligned, in the first 32768 bytes of the image.
        missing(&[&[0; 4][..], &header_with(&[])].concat());
        missing(&[&[0; HEADER_SEARCH][..], &header_with(&[])].concat());
        load(&[&[0; 4096][..], &header_with(&[])].concat()).unwrap();
    }

    #[test]
    fn honors_the_header_tags() {
        // A kernel that is loaded by the addresses in its header.
        let data = header_with(&header_tag(HEADER_TAG_ADDRESS, 0, &[0; 16]));
        let error = load_error(&image(&data));
        assert!(error.contains("not loaded as ELF images"), "{error}");
        let data = header_with(&header_tag(HEADER_TAG_ENTRY_ADDRESS, 0, &[0; 4]));
        let error = load_error(&image(&data));
        assert!(error.contains("not loaded as ELF images"), "{error}");

        // If the tags are optional, we load the ELF image.
        let memory = GuestMemory::detached(MEMORY_SIZE);
        let tags = [
            header_tag(HEADER_TAG_ADDRESS, HEADER_TAG_OPTIONAL, &[0; 16]),
            header_tag(HEADER_TAG_ENTRY_ADDRESS, HEADER_TAG_OPTIONAL, &[0; 4]),
        ];
        let regs = load(&memory, &image(&header_with(&tags.concat())), None, "", &[]).unwrap();
        assert_eq!(regs.basic.rip, u64::from(ENTRY));
    }

    #[test]
    fn translates_the_entry_point() {
        const LINK_ADDRESS: u32 = 0xC010_0000;
        let data = header_with(&[]);
        let segment = || TestSegment {
            vaddr: LINK_ADDRESS,
            paddr: LOAD_ADDRESS,
            data: &data,
            memsz: 0x1000,
        };
        let memory = GuestMemory::detached(MEMORY_SIZE);
        let image = elf(LINK_ADDRESS + 0x20, &[segment()]);
        let regs = load(&memory, &image, None, "", &[]).unwrap();
        assert_eq!(regs.basic.rip, u64::from(ENTRY));

        // The physical address of the entry point is not what the image has.
        let error = load_error(&elf(ENTRY, &[segment()]));
        assert!(error.contains("entry point is not in a segment"), "{error}");
        let error = load_error(&elf(LINK_ADDRESS + 0x1000, &[segment()]));
        assert!(error.contains("entry point is not in a segment"), "{error}");
    }

    #[test]
    fn rejects_kernels_it_cannot_load() {
        let data = header_with(&[]);
        let segment = |paddr: u32, memsz: u32| TestSegment {
            vaddr: paddr,
            paddr,
            data: &data,
            memsz,
        };

        // A 64-bit ELF image.
        let mut elf64 = image(&data);
        elf64[4] = 2;
        let error = load_error(&elf64);
        assert!(error.contains("not a 32-bit x86 ELF image"), "{error}");

        // The segment (and its BSS) does not fit into the guest's memory.
        let mut large = data.clone();
        large.resize(MEMORY_SIZE / 2, 0);
        let error = load_error(&image(&large));
        assert!(error.contains("not enough guest memory"), "{error}");

        let error = load_error(&elf(0x8020, &[segment(0x8000, 0x1000)]));
        assert!(error.contains("below 1 MiB"), "{error}");

        let short = data.len() as u32 - 1;
        let error = load_error(&elf(ENTRY, &[segment(LOAD_ADDRESS, short)]));
        assert!(error.contains("more data than memory"), "{error}");

        // The second segment starts in the BSS of the first one.
        let segments = [
            segment(LOAD_ADDRESS, 0x1000),
            segment(LOAD_ADDRESS + 0x800, 0x1000),
        ];
        let error = load_error(&elf(ENTRY, &segments));
        assert!(error.contains("overlapping segments"), "{error}");
        // Segments that touch do not overlap.
        let segments = [
            segment(LOAD_ADDRESS + 0x1000, 0x1000),
            segment(LOAD_ADDRESS, 0x1000),
        ];
        let memory = GuestMemory::detached(MEMORY_SIZE);
        load(&memory, &elf(ENTRY, &segments), None, "", &[]).unwrap();

        // The image ends in the segment's data (behind the multiboot2 header).
        let mut padded = data.clone();
        padded.resize(data.len() + 64, 0);
        let complete = image(&padded);
        let error = load_error(&complete[..complete.len() - 1]);
        assert!(error.contains("truncated segment"), "{error}");
        // The program headers are not in the image.
        let mut headerless = complete.clone();
        headerless[28..32].copy_from_slice(&(complete.len() as u32).to_le_bytes());
        let error = load_error(&headerless);
        assert!(error.contains("truncated program header"), "{error}");
    }

    #[test]
    fn rejects_what_does_not_fit() {
        let memory = GuestMemory::detached(MEMORY_SIZE);
        let image = image(&header_with(&[]));

        let initrd = vec![0; MEMORY_SIZE];
        let error = load(&memory, &image, Some(&initrd), "", &[]).unwrap_err();
        assert!(
            error.to_string().contains("memory for the initrd"),
            "{error}"
        );

        // The boot information ends where it has to with the longest command line.
        load(&memory, &image, None, "", &[]).unwrap();
        let fixed = written_info(&memory).len() as u64;
        let longest = ((INFO_END - INFO_ADDRESS - fixed) as usize) & !7;
        load(&memory, &image, None, &"x".repeat(longest), &[]).unwrap();
        let error = load(&memory, &image, None, &"x".repeat(longest + 8), &[]).unwrap_err();
        assert!(
            error.to_string().contains("command line is too long"),
            "{error}"
        );
    }
}
