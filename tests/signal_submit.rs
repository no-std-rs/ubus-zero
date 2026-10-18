#[cfg(target_os = "linux")]
mod linux {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, Ordering},
    };
    use ubus::{
        bounded::{BlobSliceBuilder, EnvSliceBuilder},
        envelope::{find, UbusAttr},
        signal::{SignalQueue, SignalSender, SubmitError},
        wire::{Codec, FrameRef, Header, MsgType},
    };

    // This is a separate executable rather than a libtest worker: no other
    // test threads can allocate while an actual signal handler is running.
    static FROZEN: AtomicBool = AtomicBool::new(false);
    static SENDER: AtomicPtr<SignalSender<'static>> = AtomicPtr::new(core::ptr::null_mut());
    static RESULT: AtomicI32 = AtomicI32::new(0);
    struct GuardedAllocator;

    unsafe impl GlobalAlloc for GuardedAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if FROZEN.load(Ordering::Relaxed) {
                unsafe { libc::_exit(90) }
            }
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if FROZEN.load(Ordering::Relaxed) {
                unsafe { libc::_exit(91) }
            }
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, pointer: *mut u8, old: Layout, size: usize) -> *mut u8 {
            if FROZEN.load(Ordering::Relaxed) {
                unsafe { libc::_exit(92) }
            }
            unsafe { System.realloc(pointer, old, size) }
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            if FROZEN.load(Ordering::Relaxed) {
                unsafe { libc::_exit(93) }
            }
            unsafe { System.dealloc(pointer, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: GuardedAllocator = GuardedAllocator;

    fn submit(signum: i32) -> Result<(), SubmitError> {
        let mut fields = [0; 32];
        let mut data = BlobSliceBuilder::<1>::new(&mut fields);
        data.put_i32(Some("signal"), signum)
            .map_err(|_| SubmitError::InvalidFrame)?;
        let data = data.finish().map_err(|_| SubmitError::InvalidFrame)?;
        let mut attributes = [0; 128];
        let mut env =
            EnvSliceBuilder::new(&mut attributes).map_err(|_| SubmitError::InvalidFrame)?;
        env.push_u32(UbusAttr::ObjId, 42)
            .map_err(|_| SubmitError::InvalidFrame)?;
        env.push_str(UbusAttr::Method, "signal")
            .map_err(|_| SubmitError::InvalidFrame)?;
        env.push_u8(UbusAttr::NoReply, 1)
            .map_err(|_| SubmitError::InvalidFrame)?;
        env.push_data(data).map_err(|_| SubmitError::InvalidFrame)?;
        let mut output = [0; 160];
        let frame = FrameRef {
            hdr: Header {
                version: 0,
                ty: MsgType::Notify,
                seq: 7,
                peer: 0,
            },
            payload: env.finish(),
        };
        let n = Codec::write_frame_to_slice(frame, &mut output)
            .map_err(|_| SubmitError::InvalidFrame)?;
        // Setup publishes a permanent sender before installing the handler.
        let sender = unsafe { &*SENDER.load(Ordering::Acquire) };
        sender.try_submit(&output[..n])
    }

    extern "C" fn handler(signum: libc::c_int) {
        let saved_errno = unsafe { *libc::__errno_location() };
        FROZEN.store(true, Ordering::Relaxed);
        let result = match submit(signum) {
            Ok(()) => 1,
            Err(SubmitError::Full) => 2,
            Err(SubmitError::Interrupted) => 3,
            Err(_) => -1,
        };
        RESULT.store(result, Ordering::Relaxed);
        FROZEN.store(false, Ordering::Relaxed);
        unsafe { *libc::__errno_location() = saved_errno };
    }

    fn verify_frames(bytes: &[u8], count: usize) {
        let mut remaining = bytes;
        for _ in 0..count {
            let (frame, n) = Codec::read_frame_borrowed(remaining).unwrap();
            assert_eq!(frame.hdr.ty, MsgType::Notify);
            assert_eq!(frame.hdr.seq, 7);
            assert_eq!(
                find(frame.payload, UbusAttr::ObjId).unwrap().as_u32(),
                Some(42)
            );
            assert_eq!(
                find(frame.payload, UbusAttr::Method).unwrap().as_str(),
                Some("signal")
            );
            assert!(find(frame.payload, UbusAttr::NoReply)
                .unwrap()
                .as_bool_int8());
            let data = find(frame.payload, UbusAttr::Data).unwrap();
            assert_eq!(
                ubus::blobmsg::lookup(data.payload, "signal")
                    .unwrap()
                    .as_i32(),
                Some(libc::SIGUSR1)
            );
            remaining = &remaining[n..];
        }
        assert!(
            remaining.is_empty(),
            "rejected submissions must enqueue no bytes"
        );
    }

    pub fn run() {
        let queue = Box::leak(Box::new(SignalQueue::new().unwrap()));
        let (sender, mut receiver) = queue.split();
        let sender = Box::leak(Box::new(sender));
        SENDER.store(sender, Ordering::Release);
        let previous =
            unsafe { libc::signal(libc::SIGUSR1, handler as *const () as libc::sighandler_t) };
        assert_ne!(previous, libc::SIG_ERR);

        let mut received = Vec::new();
        let mut tiny = [0; 3];
        for _ in 0..64 {
            RESULT.store(0, Ordering::Relaxed);
            assert_eq!(unsafe { libc::raise(libc::SIGUSR1) }, 0);
            assert_eq!(RESULT.load(Ordering::Relaxed), 1);
            loop {
                let n = receiver.read(&mut tiny).unwrap();
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&tiny[..n]);
            }
        }
        verify_frames(&received, 64);
        // Forward a submitted frame without allocation as well. The normal
        // consumer owns the stream and resumes any partial writes itself.
        use std::os::fd::AsRawFd;
        use std::{io::Read, os::unix::net::UnixStream};
        use ubus::unix::{SendProgress, StreamSender};
        let (_, n) = Codec::read_frame_borrowed(&received).unwrap();
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut socket_sender =
            StreamSender::new(unsafe { rustix::fd::BorrowedFd::borrow_raw(stream.as_raw_fd()) });
        FROZEN.store(true, Ordering::Relaxed);
        let sent = {
            match socket_sender.start(&received[..n]) {
                Ok(mut pending) => pending.advance(),
                Err(_) => Err(rustix::io::Errno::INVAL),
            }
        };
        FROZEN.store(false, Ordering::Relaxed);
        assert_eq!(sent, Ok(SendProgress::Complete));
        let mut forwarded = [0; 160];
        peer.read_exact(&mut forwarded[..n]).unwrap();
        verify_frames(&forwarded[..n], 1);
        received.clear();
        let mut accepted = 0;
        loop {
            assert_eq!(unsafe { libc::raise(libc::SIGUSR1) }, 0);
            match RESULT.load(Ordering::Relaxed) {
                1 => accepted += 1,
                2 => break,
                result => panic!("unexpected handler result {result}"),
            }
            assert!(accepted < 100_000, "the queue must be bounded");
        }
        assert!(accepted > 0);
        let mut chunk = [0; 4096];
        loop {
            let n = receiver.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            received.extend_from_slice(&chunk[..n]);
        }
        verify_frames(&received, accepted);

        FROZEN.store(true, Ordering::Relaxed);
        let invalid = sender.try_submit(&[0; 11]);
        let oversized = sender.try_submit(&[0; 4097]);
        let too_small = EnvSliceBuilder::new(&mut [] as &mut [u8]).err();
        FROZEN.store(false, Ordering::Relaxed);
        assert_eq!(invalid, Err(SubmitError::InvalidFrame));
        assert_eq!(oversized, Err(SubmitError::FrameTooLarge));
        assert!(too_small.is_some());
        assert_eq!(
            unsafe { libc::signal(libc::SIGUSR1, previous) },
            handler as *const () as libc::sighandler_t
        );
        println!("signal submission: actual SIGUSR1, full queue, fragmented reads and allocator trap passed");
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    linux::run();
}
