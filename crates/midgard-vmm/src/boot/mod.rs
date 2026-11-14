//! Loading the guest kernel and the state in which the boot CPU enters it, by the boot
//! protocol of the kernel image.

use anyhow::Result;
use midgard_hv::x86::RFLAGS_FIXED;
use midgard_hv::{DescriptorTable, GuestMemory, Regs, Segment};

use crate::pci;
use crate::platform::{HIGH_MEMORY, LEGACY_HOLE, PCI_ECAM_BASE};

pub(crate) mod initramfs;
mod linux;
mod multiboot2;

/// Loads the kernel (and initrd) and writes all structures that the guest needs at entry;
/// returns the registers with which the boot CPU enters the kernel. `rsdp` is the ACPI
/// RSDP (for protocols that hand the kernel a copy of it).
///
/// The image's format decides the protocol, and the protocol only how the kernel is
/// loaded and entered; it does not say which operating system the guest is.
pub(crate) fn load(
    memory: &GuestMemory,
    kernel: &[u8],
    initrd: Option<&[u8]>,
    cmdline: &str,
    rsdp: &[u8],
) -> Result<Regs> {
    // ELF images boot through multiboot2 (e.g., Managarm's eir). Everything else has to
    // be a bzImage (loading it fails otherwise).
    if kernel.starts_with(multiboot2::ELF_MAGIC) {
        multiboot2::load(memory, kernel, initrd, cmdline, rsdp)
    } else {
        linux::load(memory, kernel, initrd, cmdline)
    }
}

// The types of the memory map's entries (the E820 types, which the boot protocols share).
const MEMORY_RAM: u32 = 1;
const MEMORY_RESERVED: u32 = 2;

/// The guest's memory map: the address, the size and the type of each range.
fn memory_map(memory: &GuestMemory) -> [(u64, u64, u32); 4] {
    [
        (0, LEGACY_HOLE, MEMORY_RAM),
        (LEGACY_HOLE, HIGH_MEMORY - LEGACY_HOLE, MEMORY_RESERVED),
        (HIGH_MEMORY, memory.size() - HIGH_MEMORY, MEMORY_RAM),
        (PCI_ECAM_BASE, pci::ECAM_SIZE, MEMORY_RESERVED),
    ]
}

/// A flat segment (all of the 4 GiB).
fn segment(selector: u16, type_: u8, s: u8, l: u8, db: u8) -> Segment {
    Segment {
        base: 0,
        limit: 0xFFFF_FFFF,
        selector,
        type_,
        present: 1,
        dpl: 0,
        db,
        s,
        l,
        g: 1,
        avl: 0,
        unusable: 0,
    }
}

/// The registers of the boot CPU that do not depend on the boot protocol; the protocol
/// sets the instruction pointer, the code and data segments and the control registers.
fn initial_registers() -> Regs {
    let mut regs = Regs::default();
    // Interrupts are disabled.
    regs.basic.rflags = RFLAGS_FIXED;
    // VM entry requires a busy TSS; the kernel loads its own TR before it needs one.
    regs.tr = Segment {
        base: 0,
        limit: 0x67,
        selector: 0,
        type_: 0xB,
        present: 1,
        dpl: 0,
        db: 0,
        s: 0,
        l: 0,
        g: 0,
        avl: 0,
        unusable: 0,
    };
    regs.ldt = Segment {
        base: 0,
        limit: 0xFFFF,
        selector: 0,
        type_: 0x2,
        present: 1,
        dpl: 0,
        db: 0,
        s: 0,
        l: 0,
        g: 0,
        avl: 0,
        unusable: 1,
    };
    regs.idt = DescriptorTable { base: 0, limit: 0 };
    regs
}
