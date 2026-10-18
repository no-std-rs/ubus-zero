# Bounded I/O and signal-handler submission

The default `alloc` feature provides the existing client, provider, owned
messages and shared-buffer transport. With `default-features = false`, the
library instead exposes borrowed wire/blob/blobmsg readers and fixed-buffer
encoders. This mode needs no global allocator. Applications previously using
`default-features = false` with owned APIs should explicitly enable `alloc`;
`json`, `unix`, `portable-atomic` and `testing` enable it automatically.

```toml
ubus = { package = "ubus-zero", git = "https://github.com/no-std-rs/ubus-zero", default-features = false }
# Add features = ["signal"] for the Linux submission queue and stream sender.
```

## Encode using caller storage

```rust
use ubus::{
    bounded::{BlobSliceBuilder, EnvSliceBuilder},
    envelope::UbusAttr,
    wire::{Codec, FrameRef, Header, MsgType},
};

fn notification<'a>(object: u32, output: &'a mut [u8]) -> Result<&'a [u8], ubus::BufferError> {
    let mut fields = [0; 64];
    let mut data = BlobSliceBuilder::<2>::new(&mut fields);
    data.put_i32(Some("signal"), 10)?;

    let mut attributes = [0; 128];
    let mut env = EnvSliceBuilder::new(&mut attributes)?;
    env.push_u32(UbusAttr::ObjId, object)?;
    env.push_str(UbusAttr::Method, "signal")?;
    env.push_u8(UbusAttr::NoReply, 1)?;
    env.push_data(data.finish()?)?;
    let n = Codec::write_frame_to_slice(FrameRef {
        hdr: Header { version: 0, ty: MsgType::Notify, seq: 7, peer: 0 },
        payload: env.finish(),
    }, output)?;
    Ok(&output[..n])
}
```

The caller chooses storage and maximum container depth. Capacity errors,
embedded NULs, invalid IDs and unbalanced containers return fixed-size errors.
No error path formats strings or allocates. `BlobSliceBuilder::finish` returns
an attribute stream; `EnvSliceBuilder::push_data` embeds that stream directly.
Open and close an anonymous TABLE when a complete outer blobmsg table is needed.

`Codec::read_frame_borrowed` returns a view into input storage. It also accepts
the legacy eight-byte in-memory header form. A stream consumer must wait for at
least **12 bytes** before decoding, because socket frames include an outer blob
header. Keep partial frames and alignment padding until all required bytes arrive.

## Submit from a Linux signal handler

With `signal` enabled, construct `SignalQueue` and call `split` before installing
handlers. Keep the queue alive while either borrowed endpoint is in use. Publish
an initialized `SignalSender` before handler installation; do not access a
`Mutex`, `OnceLock`, the regular client or a shared mutable encoder in a handler.
For a fixed notification, pre-encode it during setup. Dynamic fields can use
handler-local stack buffers and the fixed builders above.

`SignalSender::try_submit` validates one complete frame, then makes one
nonblocking pipe write. Its limit is **4,096 bytes including the wire header and
padding**. Linux guarantees atomic pipe writes within `PIPE_BUF`; `Full` and
`Interrupted` accept no bytes. Concurrent or nested submissions do not mix frame
bytes. The queue retains both pipe ends, preventing a live safe sender from
losing its reader and raising SIGPIPE. There is no retry loop, clock, allocator,
logging or application lock in submission.

The handler must choose what to do when the queue is full: drop, coalesce or
record a count using a platform-supported lock-free atomic. Only immutable
prepared data or handler-local buffers may be shared with this path. Preserve
and restore `errno` when using a libc syscall backend. Disable handlers and wait
for in-flight calls before destroying published state; a borrowed sender cannot
outlive its queue. The library does not install handlers or change signal masks.

These constraints follow [POSIX signal actions](https://pubs.opengroup.org/onlinepubs/9799919799/functions/V2_chap02.html#tag_16_02_04_03)
and the [nonblocking atomic pipe-write contract](https://pubs.opengroup.org/onlinepubs/9699919799/functions/write.html).
Avoiding allocations alone does not establish async-signal safety. The isolated
[`signal_submit` test](../tests/signal_submit.rs) installs an actual SIGUSR1
handler and terminates on allocation, reallocation or deallocation in that
handler; it also checks saturation, fragmented reads and socket forwarding.

## Forward from the normal event loop

`SignalReceiver` is a byte stream. Poll its descriptor, read into caller-owned
storage, retain partial frames and process complete frames in order. Use one
connection owner to forward them; a handler must never write directly into a
stream that normal code may already be writing.

`unix::StreamSender` works with `signal` even when `alloc` is disabled. It wraps
an already connected Rustix `BorrowedFd`. `start` validates a caller-owned
encoded frame and borrows the sender exclusively. `PendingSend::advance` makes
one `MSG_DONTWAIT | MSG_NOSIGNAL` send and returns `Pending` or `Complete`.
On EAGAIN, wait for writability; on EINTR, retry later. All errors are numeric
`Errno` values and retain the write offset. Dropping a partially sent frame
poisons that sender so a subsequent frame cannot splice into it; close/reconnect
the connection after abandonment. Dropping before any bytes are written is safe.

Connection setup, HELLO, object registration, request sequence ownership and
reply handling remain normal-context responsibilities. A dedicated notifier
connection with a pre-registered object and NOTIFY `NoReply=1` avoids request
bookkeeping in the handler. Other request types still need their normal reply
processing; INVOKE does not acquire NOTIFY's NoReply semantics.

The broker and high-level client/provider continue to use heap storage.
`examples/noalloc-smoke` is a freestanding RISC-V link check with no allocator;
the queue-to-stream compatibility scenario additionally runs against pinned C,
Rust and a checked-in C transcript.
