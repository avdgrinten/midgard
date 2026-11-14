//! Loading the guest kernel and the state in which the boot CPU enters it.

pub(crate) mod initramfs;
mod linux;

pub(crate) use linux::{entry_registers, load};
