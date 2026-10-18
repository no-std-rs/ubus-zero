//! Freestanding link check: intentionally defines no global allocator.
#![no_std]
#![no_main]

use ubus::{
    bounded::EnvSliceBuilder,
    envelope::UbusAttr,
    wire::{Codec, FrameRef, Header, MsgType},
};

#[no_mangle]
extern "C" fn _start() -> ! {
    let mut attributes = [0; 32];
    if let Ok(mut env) = EnvSliceBuilder::new(&mut attributes) {
        if env.push_u32(UbusAttr::ObjId, 42).is_ok() {
            let frame = FrameRef {
                hdr: Header {
                    version: 0,
                    ty: MsgType::Ping,
                    seq: 1,
                    peer: 0,
                },
                payload: env.finish(),
            };
            let mut bytes = [0; 48];
            if let Ok(n) = Codec::write_frame_to_slice(frame, &mut bytes) {
                core::hint::black_box(Codec::read_frame_borrowed(&bytes[..n]).ok());
            }
        }
    }
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
