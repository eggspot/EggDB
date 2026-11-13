//! ARM semihosting (the `BKPT 0xAB` interface QEMU answers when started with
//! `-semihosting-config enable=on`): writing to the host's console and ending the run.

use core::arch::asm;

const SYS_WRITE0: u32 = 0x04;
const SYS_EXIT: u32 = 0x18;
const APPLICATION_EXIT: u32 = 0x2_0026;
const RUN_TIME_ERROR: u32 = 0x2_0023;

fn call(operation: u32, argument: usize) -> u32 {
    let mut result = operation;
    // SAFETY: `bkpt 0xAB` traps to the semihosting host, which reads `r0` and the memory `r1`
    // points at and writes only `r0`; every argument passed here stays valid for the call.
    unsafe { asm!("bkpt 0xAB", inout("r0") result, in("r1") argument, options(nostack)) };
    result
}

/// Writes `text` to the host's console, in pieces small enough for a stack buffer.
pub(crate) fn write(text: &str) {
    for piece in text.as_bytes().chunks(127) {
        let mut buffer = [0u8; 128];
        buffer[..piece.len()].copy_from_slice(piece);
        call(SYS_WRITE0, buffer.as_ptr() as usize);
    }
}

/// Ends the run: QEMU exits with status 0 for success and 1 otherwise.
pub(crate) fn exit(success: bool) -> ! {
    call(SYS_EXIT, if success { APPLICATION_EXIT } else { RUN_TIME_ERROR } as usize);
    loop {
        core::hint::spin_loop();
    }
}
