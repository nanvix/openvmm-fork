// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Minimal source-built guest for OpenVMM Xen PVH integration tests.

#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
#![cfg_attr(target_os = "none", expect(unsafe_code))]

#[cfg(not(target_os = "none"))]
fn main() {}

#[cfg(target_os = "none")]
mod guest {
    use core::arch::asm;
    use core::arch::global_asm;
    use core::panic::PanicInfo;

    const HVM_START_MAGIC: u32 = 0x336e_c578;
    const DATA_PORT: u16 = 0xe9;
    const STATUS_PORT: u16 = 0xea;
    const SHUTDOWN_PORT: u16 = 0x604;
    const SNAPSHOT_PORT: u16 = 0x605;
    const STATUS_INPUT_AVAILABLE: u8 = 1 << 0;
    const STATUS_RESTORE_TARGET_AVAILABLE: u8 = 1 << 2;
    const STATUS_RESTORE_MEMORY_AVAILABLE: u8 = 1 << 3;
    const RESTORE_PACKET_SELECT: u8 = 0xa5;
    const RESTORE_PACKET_V2_HEADER: &[u8; 19] = b"OPENVMM_ENTROPY_V2\0";
    const RESTORE_PACKET_V3_HEADER: &[u8; 19] = b"OPENVMM_ENTROPY_V3\0";
    const RAW_ECHO: &[u8] = b"\0\r\n\x7f\xffPVH-ECHO\n";

    const COMMAND_PING: u8 = 1;
    const COMMAND_ECHO: u8 = 2;
    const COMMAND_SNAPSHOT: u8 = 3;
    const COMMAND_RESTORE_TARGET: u8 = 4;
    const COMMAND_STATE: u8 = 5;
    const COMMAND_SHUTDOWN: u8 = 6;

    global_asm!(
        r#"
        .section .note.xen,"a",@note
        .balign 4
        .long 4
        .long 4
        .long 18
        .asciz "Xen"
        .balign 4
        .long _pvh_start32
        .balign 4

        .section .text.boot,"ax",@progbits
        .code32
        .global _pvh_start32
        .type _pvh_start32,@function
    _pvh_start32:
        cli
        movl %ebx, %edi
        movl $boot_stack_top, %esp

        movl $pvh_page_table_l3, %eax
        orl $3, %eax
        movl %eax, pvh_page_table_l4
        movl $pvh_page_table_l2, %eax
        orl $3, %eax
        movl %eax, pvh_page_table_l3

        xorl %ecx, %ecx
    1:
        movl %ecx, %eax
        shll $21, %eax
        orl $0x83, %eax
        movl %eax, pvh_page_table_l2(,%ecx,8)
        incl %ecx
        cmpl $8, %ecx
        jne 1b

        movl %cr4, %eax
        orl $0x20, %eax
        movl %eax, %cr4
        movl $pvh_page_table_l4, %eax
        movl %eax, %cr3

        movl $0xc0000080, %ecx
        rdmsr
        orl $0x100, %eax
        wrmsr

        lgdt pvh_gdt_descriptor
        movl %cr0, %eax
        orl $0x80000000, %eax
        movl %eax, %cr0
        ljmpl $0x08, $pvh_long_mode

        .code64
    pvh_long_mode:
        movw $0x10, %ax
        movw %ax, %ds
        movw %ax, %es
        movw %ax, %ss
        leaq boot_stack_top(%rip), %rsp
        xorq %rbp, %rbp
        movl %edi, %edi
        call pvh_main
    2:
        hlt
        jmp 2b

        .section .rodata.boot,"a",@progbits
        .balign 8
    pvh_gdt:
        .quad 0
        .quad 0x00af9a000000ffff
        .quad 0x00cf92000000ffff
    pvh_gdt_end:
    pvh_gdt_descriptor:
        .word pvh_gdt_end - pvh_gdt - 1
        .long pvh_gdt

        .section .bss.boot,"aw",@nobits
        .balign 4096
    pvh_page_table_l4:
        .skip 4096
    pvh_page_table_l3:
        .skip 4096
    pvh_page_table_l2:
        .skip 4096
        .balign 16
    boot_stack:
        .skip 65536
    boot_stack_top:
        "#,
        options(att_syntax)
    );

    #[repr(C)]
    struct HvmStartInfo {
        magic: u32,
        version: u32,
        flags: u32,
        nr_modules: u32,
        modlist_paddr: u64,
        cmdline_paddr: u64,
        rsdp_paddr: u64,
        memmap_paddr: u64,
        memmap_entries: u32,
        reserved: u32,
    }

    static mut SNAPSHOT_GENERATION: u32 = 0;

    #[unsafe(no_mangle)]
    extern "C" fn pvh_main(start_info: *const HvmStartInfo) -> ! {
        // SAFETY: the Xen PVH ABI supplies a valid start-info pointer in RBX.
        let start_info = unsafe { &*start_info };
        if start_info.magic != HVM_START_MAGIC || start_info.version != 1 {
            write_bytes(b"PVH-START-INFO-INVALID\n");
            shutdown(0xff);
        }

        write_bytes(b"OPENVMM-PVH-TEST-READY\n");
        loop {
            match read_input() {
                COMMAND_PING => write_bytes(b"PONG\n"),
                COMMAND_ECHO => write_bytes(RAW_ECHO),
                COMMAND_SNAPSHOT => snapshot(),
                COMMAND_RESTORE_TARGET => report_restore_target(false),
                COMMAND_STATE => report_state(),
                COMMAND_SHUTDOWN => shutdown(read_input()),
                _ => write_bytes(b"UNKNOWN-COMMAND\n"),
            }
        }
    }

    fn snapshot() {
        write_bytes(b"SNAPSHOT-REQUESTED\n");
        port_write(SNAPSHOT_PORT, 0);
        // SAFETY: only the BSP executes the command loop.
        let generation = unsafe {
            SNAPSHOT_GENERATION = SNAPSHOT_GENERATION.wrapping_add(1);
            SNAPSHOT_GENERATION
        };
        write_bytes(b"SNAPSHOT-CONTINUED=");
        write_u32(generation);
        write_byte(b'\n');
        if port_read(STATUS_PORT)
            & (STATUS_RESTORE_TARGET_AVAILABLE | STATUS_RESTORE_MEMORY_AVAILABLE)
            != 0
        {
            report_restore_target(true);
        }
    }

    fn report_restore_target(release_gate: bool) {
        let status = port_read(STATUS_PORT);
        if status & (STATUS_RESTORE_TARGET_AVAILABLE | STATUS_RESTORE_MEMORY_AVAILABLE) == 0 {
            write_bytes(b"RESTORE-TARGET=NONE\n");
            return;
        }

        port_write(STATUS_PORT, RESTORE_PACKET_SELECT);
        if status & STATUS_RESTORE_MEMORY_AVAILABLE != 0 {
            for expected in RESTORE_PACKET_V3_HEADER {
                if port_read(DATA_PORT) != *expected {
                    write_bytes(b"RESTORE-PACKET-INVALID\n");
                    return;
                }
            }
            let target = port_read(DATA_PORT);
            let range_count = port_read(DATA_PORT);
            for _ in 0..usize::from(range_count) * 16 + 64 {
                let _ = port_read(DATA_PORT);
            }
            if release_gate {
                port_write(SNAPSHOT_PORT, 2);
            }
            write_bytes(b"RESTORE-TARGET=");
            write_u32(target.into());
            write_bytes(b" MEMORY-RANGES=");
            write_u32(range_count.into());
            write_byte(b'\n');
            return;
        }

        for expected in RESTORE_PACKET_V2_HEADER {
            if port_read(DATA_PORT) != *expected {
                write_bytes(b"RESTORE-PACKET-INVALID\n");
                return;
            }
        }
        let target = port_read(DATA_PORT);
        for _ in 0..64 {
            let _ = port_read(DATA_PORT);
        }
        if release_gate {
            port_write(SNAPSHOT_PORT, 2);
        }
        write_bytes(b"RESTORE-TARGET=");
        write_u32(target.into());
        write_byte(b'\n');
    }

    fn report_state() {
        // SAFETY: only the BSP executes the command loop.
        let generation = unsafe { SNAPSHOT_GENERATION };
        write_bytes(b"STATE=");
        write_u32(generation);
        write_byte(b'\n');
    }

    fn read_input() -> u8 {
        while port_read(STATUS_PORT) & STATUS_INPUT_AVAILABLE == 0 {
            core::hint::spin_loop();
        }
        port_read(DATA_PORT)
    }

    fn write_bytes(bytes: &[u8]) {
        for byte in bytes {
            write_byte(*byte);
        }
    }

    fn write_byte(byte: u8) {
        port_write(DATA_PORT, byte);
    }

    fn write_u32(mut value: u32) {
        let mut digits = [0u8; 10];
        let mut index = digits.len();
        loop {
            index -= 1;
            digits[index] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        write_bytes(&digits[index..]);
    }

    fn shutdown(status: u8) -> ! {
        port_write(SHUTDOWN_PORT, status);
        loop {
            // SAFETY: halting while waiting for the VMM to process shutdown is valid.
            unsafe { asm!("hlt", options(nomem, nostack)) };
        }
    }

    fn port_read(port: u16) -> u8 {
        let value: u8;
        // SAFETY: these ports are the fixed microVM test ABI.
        unsafe {
            asm!(
                "in al, dx",
                in("dx") port,
                out("al") value,
                options(nomem, nostack, preserves_flags)
            )
        };
        value
    }

    fn port_write(port: u16, value: u8) {
        // SAFETY: these ports are the fixed microVM test ABI.
        unsafe {
            asm!(
                "out dx, al",
                in("dx") port,
                in("al") value,
                options(nomem, nostack, preserves_flags)
            )
        };
    }

    #[panic_handler]
    fn panic(_info: &PanicInfo<'_>) -> ! {
        write_bytes(b"PVH-TEST-PANIC\n");
        shutdown(0xfe)
    }
}
