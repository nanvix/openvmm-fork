// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guestmem::GuestMemory;
use loader::importer::X86Register;
use std::io::Seek;
use thiserror::Error;
use vm_loader::InitialLoad;
use vm_loader::Loader;
use vm_topology::memory::MemoryLayout;

#[derive(Debug)]
pub struct KernelConfig<'a> {
    pub kernel: &'a std::fs::File,
    pub initrd: &'a Option<std::fs::File>,
    pub cmdline: &'a str,
    pub mem_layout: &'a MemoryLayout,
    pub acpi_tables: loader::pvh::AcpiTables,
    pub boot_config: loader::pvh::BootConfig<'a>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to inspect PVH initramfs")]
    Initrd(#[source] std::io::Error),
    #[error("PVH loader failed")]
    Loader(#[source] loader::pvh::Error),
}

pub fn load_pvh(
    cfg: &KernelConfig<'_>,
    gm: &GuestMemory,
) -> Result<InitialLoad<X86Register>, Error> {
    let mut kernel = cfg.kernel;
    let (mut initrd, initrd_size) = if let Some(mut initrd) = cfg.initrd.as_ref() {
        initrd.rewind().map_err(Error::Initrd)?;
        let size = initrd
            .seek(std::io::SeekFrom::End(0))
            .map_err(Error::Initrd)?;
        (Some(initrd), size)
    } else {
        (None, 0)
    };
    let initrd = initrd.as_mut().map(|image| loader::pvh::InitrdConfig {
        image,
        size: initrd_size,
    });

    let mut loader = Loader::new(gm.clone(), cfg.mem_layout, hvdef::Vtl::Vtl0);
    loader::pvh::load_with_boot_config(
        &mut loader,
        &mut kernel,
        initrd,
        cfg.cmdline,
        cfg.mem_layout,
        Some(&cfg.acpi_tables),
        &cfg.boot_config,
    )
    .map_err(Error::Loader)?;
    Ok(loader.initial_regs_and_page_imports())
}
