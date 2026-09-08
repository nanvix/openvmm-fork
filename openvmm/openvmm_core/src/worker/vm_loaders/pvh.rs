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

const MIN_TSC_FREQUENCY_HZ: u64 = 500_000_000;
const MAX_TSC_FREQUENCY_HZ: u64 = 10_000_000_000;
const HZ_PER_KHZ: u64 = 1000;
const TSC_EARLY_KHZ: &str = "tsc_early_khz";

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum TscFrequencyError {
    #[error("TSC frequency {0} Hz is too small to convert to kHz")]
    TooSmall(u64),
    #[error("TSC frequency {0} Hz is outside the supported range of 500 MHz through 10 GHz")]
    OutOfRange(u64),
    #[error("kernel command line contains duplicate tsc_early_khz parameters")]
    Duplicate,
    #[error("kernel command line contains malformed tsc_early_khz parameter: {0}")]
    Malformed(String),
    #[error(
        "kernel command line tsc_early_khz value {specified} does not match the backend-reported value {reported}"
    )]
    Mismatch { specified: u64, reported: u64 },
}

fn command_line_tokens(cmdline: &str) -> Vec<(usize, &str)> {
    let mut tokens = Vec::new();
    let mut token_start = None;
    for (offset, character) in cmdline.char_indices() {
        if character.is_ascii_whitespace() {
            if let Some(start) = token_start.take() {
                tokens.push((start, &cmdline[start..offset]));
            }
        } else if token_start.is_none() {
            token_start = Some(offset);
        }
    }
    if let Some(start) = token_start {
        tokens.push((start, &cmdline[start..]));
    }
    tokens
}

fn linux_parameter_name_matches(actual: &str, expected: &str) -> bool {
    actual
        .bytes()
        .map(|byte| if byte == b'-' { b'_' } else { byte })
        .eq(expected
            .bytes()
            .map(|byte| if byte == b'-' { b'_' } else { byte }))
}

fn parse_linux_uint(value: &str) -> Option<u32> {
    let value = if let Some(value) = value.strip_prefix('"') {
        value.strip_suffix('"')?
    } else {
        if value.ends_with('"') {
            return None;
        }
        value
    };
    let value = value.strip_prefix('+').unwrap_or(value);
    if value.is_empty() {
        return None;
    }

    let (digits, radix) = if let Some(digits) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        (digits, 16)
    } else if value.starts_with('0') {
        (value, 8)
    } else {
        (value, 10)
    };
    u32::from_str_radix(digits, radix).ok()
}

pub(crate) fn propagate_tsc_frequency(
    cmdline: &mut String,
    frequency_hz: u64,
) -> Result<(), TscFrequencyError> {
    if frequency_hz < HZ_PER_KHZ {
        return Err(TscFrequencyError::TooSmall(frequency_hz));
    }
    if !(MIN_TSC_FREQUENCY_HZ..=MAX_TSC_FREQUENCY_HZ).contains(&frequency_hz) {
        return Err(TscFrequencyError::OutOfRange(frequency_hz));
    }
    let frequency_khz = frequency_hz / HZ_PER_KHZ;

    let mut parameter = None;
    let mut delimiter_offset = None;
    for (offset, token) in command_line_tokens(cmdline) {
        if token == "--" {
            delimiter_offset = Some(offset);
            break;
        }
        let name = token.split_once('=').map_or(token, |(name, _value)| name);
        if linux_parameter_name_matches(name, TSC_EARLY_KHZ) {
            if parameter.is_some() {
                return Err(TscFrequencyError::Duplicate);
            }
            parameter = Some(token);
        }
    }

    if let Some(parameter) = parameter {
        let Some((_name, value)) = parameter.split_once('=') else {
            return Err(TscFrequencyError::Malformed(parameter.to_owned()));
        };
        let specified = parse_linux_uint(value)
            .map(u64::from)
            .ok_or_else(|| TscFrequencyError::Malformed(parameter.to_owned()))?;
        if specified != frequency_khz {
            return Err(TscFrequencyError::Mismatch {
                specified,
                reported: frequency_khz,
            });
        }
        return Ok(());
    }

    let generated_parameter = format!("{TSC_EARLY_KHZ}={frequency_khz}");
    if let Some(offset) = delimiter_offset {
        cmdline.insert_str(offset, &format!("{generated_parameter} "));
        return Ok(());
    }
    if !cmdline.is_empty()
        && !cmdline
            .as_bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        cmdline.push(' ');
    }
    cmdline.push_str(&generated_parameter);
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    const TSC_FREQUENCY_HZ: u64 = 2_500_000_999;

    #[test]
    fn appends_normalized_frequency() {
        let mut cmdline = "console=ttyS0".to_owned();
        propagate_tsc_frequency(&mut cmdline, TSC_FREQUENCY_HZ).unwrap();
        assert_eq!(cmdline, "console=ttyS0 tsc_early_khz=2500000");
    }

    #[test]
    fn accepts_matching_frequency_without_mutation() {
        let mut cmdline = "console=ttyS0 tsc_early_khz=2500000".to_owned();
        let original = cmdline.clone();
        propagate_tsc_frequency(&mut cmdline, TSC_FREQUENCY_HZ).unwrap();
        assert_eq!(cmdline, original);
    }

    #[test]
    fn rejects_mismatched_frequency() {
        let mut cmdline = "tsc_early_khz=2499999".to_owned();
        assert_eq!(
            propagate_tsc_frequency(&mut cmdline, TSC_FREQUENCY_HZ),
            Err(TscFrequencyError::Mismatch {
                specified: 2_499_999,
                reported: 2_500_000,
            })
        );
    }

    #[test]
    fn rejects_malformed_frequency() {
        for parameter in [
            "tsc_early_khz",
            "tsc_early_khz=",
            "tsc_early_khz=2.5",
            "tsc_early_khz=18446744073709551616",
        ] {
            let mut cmdline = parameter.to_owned();
            assert!(matches!(
                propagate_tsc_frequency(&mut cmdline, TSC_FREQUENCY_HZ),
                Err(TscFrequencyError::Malformed(_))
            ));
        }
    }

    #[test]
    fn rejects_duplicate_frequency() {
        let mut cmdline = "tsc_early_khz=2500000 console=ttyS0 tsc_early_khz=2500000".to_owned();
        assert_eq!(
            propagate_tsc_frequency(&mut cmdline, TSC_FREQUENCY_HZ),
            Err(TscFrequencyError::Duplicate)
        );
    }

    #[test]
    fn validates_frequency_range_before_conversion() {
        let mut cmdline = String::new();
        assert_eq!(
            propagate_tsc_frequency(&mut cmdline, 999),
            Err(TscFrequencyError::TooSmall(999))
        );
        assert_eq!(
            propagate_tsc_frequency(&mut cmdline, MIN_TSC_FREQUENCY_HZ - 1),
            Err(TscFrequencyError::OutOfRange(MIN_TSC_FREQUENCY_HZ - 1))
        );
        assert_eq!(
            propagate_tsc_frequency(&mut cmdline, MAX_TSC_FREQUENCY_HZ + 1),
            Err(TscFrequencyError::OutOfRange(MAX_TSC_FREQUENCY_HZ + 1))
        );

        propagate_tsc_frequency(&mut String::new(), MIN_TSC_FREQUENCY_HZ).unwrap();
        propagate_tsc_frequency(&mut String::new(), MAX_TSC_FREQUENCY_HZ).unwrap();
    }

    #[test]
    fn handles_ascii_whitespace_without_adding_extra_whitespace() {
        let mut empty = String::new();
        propagate_tsc_frequency(&mut empty, TSC_FREQUENCY_HZ).unwrap();
        assert_eq!(empty, "tsc_early_khz=2500000");

        let mut trailing = "console=ttyS0 \t".to_owned();
        propagate_tsc_frequency(&mut trailing, TSC_FREQUENCY_HZ).unwrap();
        assert_eq!(trailing, "console=ttyS0 \ttsc_early_khz=2500000");

        let mut supplied = "console=ttyS0\t  tsc_early_khz=2500000\n".to_owned();
        let original = supplied.clone();
        propagate_tsc_frequency(&mut supplied, TSC_FREQUENCY_HZ).unwrap();
        assert_eq!(supplied, original);
    }

    #[test]
    fn inserts_before_delimiter_and_ignores_tokens_after_it() {
        let mut cmdline = "console=ttyS0 \t-- tsc_early_khz=1 tsc_early_khz=malformed".to_owned();
        propagate_tsc_frequency(&mut cmdline, TSC_FREQUENCY_HZ).unwrap();
        assert_eq!(
            cmdline,
            "console=ttyS0 \ttsc_early_khz=2500000 -- \
             tsc_early_khz=1 tsc_early_khz=malformed"
        );

        let mut supplied = "tsc_early_khz=2500000 -- tsc_early_khz=1 tsc_early_khz=2".to_owned();
        let original = supplied.clone();
        propagate_tsc_frequency(&mut supplied, TSC_FREQUENCY_HZ).unwrap();
        assert_eq!(supplied, original);
    }

    #[test]
    fn treats_hyphens_and_underscores_as_the_same_parameter_name() {
        let mut matching = "tsc-early-khz=2500000".to_owned();
        let original = matching.clone();
        propagate_tsc_frequency(&mut matching, TSC_FREQUENCY_HZ).unwrap();
        assert_eq!(matching, original);

        let mut mismatched = "tsc-early-khz=2499999".to_owned();
        assert_eq!(
            propagate_tsc_frequency(&mut mismatched, TSC_FREQUENCY_HZ),
            Err(TscFrequencyError::Mismatch {
                specified: 2_499_999,
                reported: 2_500_000,
            })
        );

        let mut duplicate = "tsc-early-khz=2500000 tsc_early_khz=2500000".to_owned();
        assert_eq!(
            propagate_tsc_frequency(&mut duplicate, TSC_FREQUENCY_HZ),
            Err(TscFrequencyError::Duplicate)
        );
    }

    #[test]
    fn accepts_quote_stripped_decimal_frequency() {
        let mut cmdline = r#"console=ttyS0 tsc_early_khz="2500000""#.to_owned();
        let original = cmdline.clone();
        propagate_tsc_frequency(&mut cmdline, TSC_FREQUENCY_HZ).unwrap();
        assert_eq!(cmdline, original);
    }

    #[test]
    fn parses_hexadecimal_frequency() {
        let mut matching = "tsc_early_khz=0x2625a0".to_owned();
        let original = matching.clone();
        propagate_tsc_frequency(&mut matching, TSC_FREQUENCY_HZ).unwrap();
        assert_eq!(matching, original);

        let mut mismatched = "tsc_early_khz=0X26259F".to_owned();
        assert_eq!(
            propagate_tsc_frequency(&mut mismatched, TSC_FREQUENCY_HZ),
            Err(TscFrequencyError::Mismatch {
                specified: 2_499_999,
                reported: 2_500_000,
            })
        );
    }

    #[test]
    fn rejects_malformed_or_overflowing_linux_uint() {
        for parameter in [
            r#"tsc_early_khz="2500000"#,
            "tsc_early_khz=0x",
            "tsc_early_khz=08",
            "tsc_early_khz=-2500000",
            "tsc_early_khz=4294967296",
            "tsc_early_khz=0x100000000",
        ] {
            let mut cmdline = parameter.to_owned();
            assert!(matches!(
                propagate_tsc_frequency(&mut cmdline, TSC_FREQUENCY_HZ),
                Err(TscFrequencyError::Malformed(_))
            ));
        }
    }
}
