//! Allocation diagnostic: cargo run --release -p ubusd --example footprint
//! Reports allocator calls on verified routing operations and retained churn memory.
//! Run outside timing comparisons: the tracking allocator intentionally adds overhead.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::Arc;
use ubus::{
    envelope::{find, EnvBuilder, UbusAttr},
    wire::{Codec, Frame, MsgType},
};
use ubusd::registry::{
    EventRegistry, MonitorRegistry, ObjectRegistry, PeerTable, SubscriberRegistry,
};
use ubusd::{acl::AclRegistry, peer::Peer, routing};

struct TrackingAllocator;
static ALLOCATION_CALLS: AtomicIsize = AtomicIsize::new(0);
static LIVE_BYTES: AtomicIsize = AtomicIsize::new(0);
static REQUESTED_BYTES: AtomicUsize = AtomicUsize::new(0);
static LARGE_CALLS: AtomicUsize = AtomicUsize::new(0);
static LARGE_THRESHOLD: AtomicUsize = AtomicUsize::new(usize::MAX);

fn record_allocation(size: usize) {
    ALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
    REQUESTED_BYTES.fetch_add(size, Ordering::Relaxed);
    if size >= LARGE_THRESHOLD.load(Ordering::Relaxed) {
        LARGE_CALLS.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record_allocation(layout.size());
            LIVE_BYTES.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record_allocation(layout.size());
            LIVE_BYTES.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, old, new_size) };
        if !new_ptr.is_null() {
            record_allocation(new_size);
            LIVE_BYTES.fetch_add(new_size as isize - old.size() as isize, Ordering::Relaxed);
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

fn count_allocations(mut operation: impl FnMut()) -> isize {
    for _ in 0..100 {
        operation();
    }
    let before = ALLOCATION_CALLS.load(Ordering::Relaxed);
    for _ in 0..10_000 {
        operation();
    }
    ALLOCATION_CALLS.load(Ordering::Relaxed) - before
}

fn notify_footprint(payload_bytes: usize, subscriber_count: usize) -> serde_json::Value {
    let mut peers = PeerTable::new();
    let mut sockets = Vec::new();
    let mut add_peer = || {
        let (socket, other) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        sockets.push(other);
        peers
            .insert(Peer::new(
                socket,
                Some(0),
                Some("root".into()),
                Some("root".into()),
            ))
            .unwrap()
            .0
    };
    let owner = add_peer();
    let mut objects = ObjectRegistry::new();
    let target = objects.insert_anonymous(owner).unwrap();
    let mut subscribers = SubscriberRegistry::new();
    let mut destinations = Vec::new();
    for _ in 0..subscriber_count {
        let peer = add_peer();
        let object = objects.insert_anonymous(peer).unwrap();
        subscribers.add(target, peer, object);
        destinations.push((peer, object));
    }
    let data = vec![0x5a; payload_bytes];
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, target.get());
    env.push_str(UbusAttr::Method, "update");
    env.push_data(&data);
    env.push_u8(UbusAttr::NoReply, 1);
    let frame = Frame::to_broker(MsgType::Notify, 71, env.finish());
    let acl = AclRegistry::default();
    let mut events = EventRegistry::new();
    let mut monitors = MonitorRegistry::new();
    LARGE_THRESHOLD.store(payload_bytes, Ordering::Relaxed);
    let before_calls = ALLOCATION_CALLS.load(Ordering::Relaxed);
    let before_bytes = REQUESTED_BYTES.load(Ordering::Relaxed);
    let before_large = LARGE_CALLS.load(Ordering::Relaxed);
    let before_live = LIVE_BYTES.load(Ordering::Relaxed);
    let actions = routing::dispatch(
        &frame,
        owner,
        &acl,
        0,
        &mut peers,
        &mut objects,
        &mut subscribers,
        &mut events,
        &mut monitors,
        &mut None,
    );
    let calls = ALLOCATION_CALLS.load(Ordering::Relaxed) - before_calls;
    let requested = REQUESTED_BYTES.load(Ordering::Relaxed) - before_bytes;
    let large = LARGE_CALLS.load(Ordering::Relaxed) - before_large;
    let retained = LIVE_BYTES.load(Ordering::Relaxed) - before_live;
    LARGE_THRESHOLD.store(usize::MAX, Ordering::Relaxed);

    assert_eq!(actions.len(), subscriber_count);
    let mut first_template = None;
    let mut bytes = Vec::new();
    for (action, (expected_peer, expected_object)) in actions.iter().zip(destinations) {
        let routing::Action::SendNotify {
            peer_id,
            template,
            obj_id,
        } = action
        else {
            panic!("expected NOTIFY delivery");
        };
        assert_eq!((*peer_id, *obj_id), (expected_peer, expected_object));
        if let Some(first) = first_template {
            assert!(
                Arc::ptr_eq(first, template),
                "one template per notification"
            );
        } else {
            first_template = Some(template);
        }
        template.write_into(*obj_id, &mut bytes);
        let (delivery, consumed) = Codec::read_frame_borrowed(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(delivery.hdr.ty, MsgType::Invoke);
        assert_eq!(delivery.hdr.seq, 71);
        assert_eq!(delivery.hdr.peer, owner.get());
        assert_eq!(
            find(delivery.payload, UbusAttr::ObjId).and_then(|a| a.as_u32()),
            Some(expected_object.get())
        );
        assert_eq!(
            find(delivery.payload, UbusAttr::Method).and_then(|a| a.as_str()),
            Some("update")
        );
        assert_eq!(
            find(delivery.payload, UbusAttr::NoReply).map(|a| a.payload),
            Some(&[1][..])
        );
        assert_eq!(
            find(delivery.payload, UbusAttr::Data).map(|a| a.payload),
            Some(data.as_slice())
        );
    }
    serde_json::json!({
        "payload_bytes": payload_bytes,
        "subscribers": subscriber_count,
        "allocation_calls": calls,
        "requested_allocation_bytes": requested,
        "allocations_at_least_payload_size": large,
        "live_bytes_after_dispatch": retained,
    })
}

fn main() {
    let mut peers = PeerTable::new();
    let mut sockets = Vec::new();
    let mut add_peer = || {
        let (socket, other) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        sockets.push(other);
        peers
            .insert(Peer::new(
                socket,
                Some(0),
                Some("root".into()),
                Some("root".into()),
            ))
            .unwrap()
            .0
    };
    let caller = add_peer();
    let owner = add_peer();
    let mut objects = ObjectRegistry::new();
    let object = objects
        .insert("benchmark.echo".into(), owner, None)
        .unwrap()
        .0;
    let acl = AclRegistry::default();
    let mut subscribers = SubscriberRegistry::new();
    let mut events = EventRegistry::new();
    let mut monitors = MonitorRegistry::new();
    let ping = Frame::to_broker(MsgType::Ping, 10, EnvBuilder::new().finish());
    let mut env = EnvBuilder::new();
    env.push_u32(UbusAttr::ObjId, object.get());
    env.push_str(UbusAttr::Method, "echo");
    env.push_data(&[0; 64]);
    let invoke = Frame::to_broker(MsgType::Invoke, 11, env.finish());
    let mut dispatch = |frame: &Frame, expected| {
        let mut fd = None;
        let actions = routing::dispatch(
            frame,
            caller,
            &acl,
            0,
            &mut peers,
            &mut objects,
            &mut subscribers,
            &mut events,
            &mut monitors,
            &mut fd,
        );
        assert_eq!(actions.len(), expected);
        std::hint::black_box(actions);
    };
    let ping_calls = count_allocations(|| dispatch(&ping, 2));
    let invoke_calls = count_allocations(|| dispatch(&invoke, 1));

    let before = LIVE_BYTES.load(Ordering::Relaxed);
    let mut registry = ObjectRegistry::new();
    let mut retained = [0; 3];
    let mut completed = 0;
    for (index, target) in [1000, 2000, 4000].into_iter().enumerate() {
        while completed < target {
            let (id, replaced) = registry.insert("probe.churn".into(), owner, None).unwrap();
            assert!(replaced.is_none());
            assert!(registry.remove(id));
            completed += 1;
        }
        assert!(registry.lookup_exact("probe.churn").is_none());
        assert!(registry.named_objects_of(owner).is_empty());
        std::hint::black_box(&registry);
        retained[index] = LIVE_BYTES.load(Ordering::Relaxed) - before;
    }
    drop(registry);
    let after_drop = LIVE_BYTES.load(Ordering::Relaxed) - before;
    let notify: Vec<_> = [4096, 65536]
        .into_iter()
        .flat_map(|size| {
            [1, 8, 64]
                .into_iter()
                .map(move |count| notify_footprint(size, count))
        })
        .collect();
    for cases in notify.chunks_exact(3) {
        let large = cases[0]["allocations_at_least_payload_size"]
            .as_u64()
            .unwrap();
        assert!(
            cases
                .iter()
                .all(|case| case["allocations_at_least_payload_size"].as_u64() == Some(large)),
            "payload-sized dispatch allocations must not grow with subscriber count"
        );
    }
    println!(
        "{}",
        serde_json::json!({
            "operations_per_case": 10000,
            "routing_allocator_calls": {"ping": ping_calls, "invoke_forward_64_bytes": invoke_calls},
            "churn_cycles": [1000, 2000, 4000], "live_objects_after_each_stage": 0,
            "retained_heap_bytes_after_churn": retained,
            "retained_heap_bytes_after_registry_drop": after_drop,
            "notify_fanout": notify,
            "scope": "instrumented Rust dispatch only, including NOTIFY template encoding; excludes socket I/O and broker loop; not timing or peak RSS"
        })
    );
}
