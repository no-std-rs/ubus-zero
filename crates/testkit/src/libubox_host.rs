//! A multi-role libubus fixture with one manually-pumped owning thread.
//!
//! libubus's process-global event loop is deliberately not used here. Every
//! libubus operation runs on the host thread, which polls the context socket
//! and dispatches commands from the test thread.

#[cfg(not(have_libubox))]
use std::io;
#[cfg(not(have_libubox))]
use std::path::Path;

#[cfg(have_libubox)]
mod inner {
    use std::collections::HashSet;
    use std::ffi::{CStr, CString};
    use std::io;
    use std::marker::PhantomData;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use libubox_sys as sys;
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    use rustix::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
    use rustix::io::Errno;

    use crate::libubox_blob::{libubus_global_lock, BlobBufGuard};

    const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
    const PUMP_TIMEOUT: Timespec = Timespec {
        tv_sec: 0,
        tv_nsec: 20_000_000,
    };

    /// Result chosen by a handler registered with
    /// [`LibuboxHost::add_object_deferring`].
    pub enum DeferChoice {
        Reply(i32, Option<Vec<u8>>),
        Defer,
    }

    /// An opaque, single-use request retained by a deferring handler.
    pub struct DeferredReply {
        request: usize,
    }

    /// An object owned by a [`LibuboxHost`]. Its ID is suitable for a
    /// subscriber target; its pointer is only ever dereferenced by the host.
    pub struct HostObject {
        ptr: usize,
        id: u32,
        _not_send: PhantomData<Rc<()>>,
    }

    impl HostObject {
        pub fn id(&self) -> u32 {
            self.id
        }
    }

    /// A subscriber owned by a [`LibuboxHost`].
    pub struct HostSubscriber {
        ptr: usize,
        _not_send: PhantomData<Rc<()>>,
    }

    /// An event handler owned by a [`LibuboxHost`].
    pub struct HostEventHandler {
        ptr: usize,
        _not_send: PhantomData<Rc<()>>,
    }

    /// A host owns one libubus context and one pump thread.
    pub struct LibuboxHost {
        tx: mpsc::Sender<Cmd>,
        join: Mutex<Option<JoinHandle<()>>>,
        shutdown: Arc<AtomicBool>,
    }

    type ObjectCallback = Box<dyn FnMut(&str, &[u8]) -> (i32, Option<Vec<u8>>) + Send>;
    type FdReplyCallback = Box<
        dyn FnMut(Option<OwnedFd>, &str, &[u8]) -> (i32, Option<Vec<u8>>, Option<OwnedFd>) + Send,
    >;
    type DeferringCallback = Box<dyn FnMut(&str, &[u8]) -> DeferChoice + Send>;
    type NotifyCallback = Box<dyn FnMut(&str, &[u8]) -> i32 + Send>;
    type RemovedCallback = Box<dyn FnMut(u32) + Send>;
    type EventCallback = Box<dyn FnMut(&str, &[u8]) + Send>;

    enum ObjectDispatch {
        Immediate(ObjectCallback),
        FdReply(FdReplyCallback),
        Deferred {
            callback: DeferringCallback,
            deferred_tx: mpsc::Sender<DeferredReply>,
            deferred: Option<Arc<Mutex<HashSet<usize>>>>,
        },
    }

    struct SubscriberDispatch {
        notify: NotifyCallback,
        removed: RemovedCallback,
    }

    // Each raw pointer is a deliberately double-boxed trait object. The C
    // wrapper retains its `void *` unchanged while this vector owns and drops
    // that outer allocation only after its corresponding C wrapper is freed.
    enum CallbackStorage {
        Object(*mut Box<ObjectDispatch>),
        Subscriber(*mut Box<SubscriberDispatch>),
        Event(*mut Box<EventCallback>),
    }

    impl CallbackStorage {
        unsafe fn drop_box(self) {
            match self {
                CallbackStorage::Object(ptr) => {
                    // SAFETY: this was produced by `Box::into_raw` exactly
                    // once and the C object using it has already been freed.
                    unsafe { drop(Box::from_raw(ptr)) };
                }
                CallbackStorage::Subscriber(ptr) => {
                    // SAFETY: see the object arm above.
                    unsafe { drop(Box::from_raw(ptr)) };
                }
                CallbackStorage::Event(ptr) => {
                    // SAFETY: see the object arm above.
                    unsafe { drop(Box::from_raw(ptr)) };
                }
            }
        }
    }

    enum Cmd {
        AddObject {
            name: String,
            methods: Vec<String>,
            dispatch: ObjectDispatch,
            reply: mpsc::Sender<io::Result<ObjectInfo>>,
        },
        RemoveObject {
            ptr: usize,
            reply: mpsc::Sender<io::Result<()>>,
        },
        CompleteDeferred {
            request: usize,
            status: i32,
            reply_blob: Option<Vec<u8>>,
            reply: mpsc::Sender<io::Result<()>>,
        },
        RegisterSubscriber {
            dispatch: SubscriberDispatch,
            reply: mpsc::Sender<io::Result<usize>>,
        },
        Subscribe {
            ptr: usize,
            target_id: u32,
            reply: mpsc::Sender<io::Result<()>>,
        },
        Unsubscribe {
            ptr: usize,
            target_id: u32,
            reply: mpsc::Sender<io::Result<()>>,
        },
        Notify {
            ptr: usize,
            ty: String,
            args: Vec<(String, String)>,
            timeout_ms: i32,
            reply: mpsc::Sender<io::Result<i32>>,
        },
        RegisterEvent {
            pattern: String,
            callback: EventCallback,
            reply: mpsc::Sender<io::Result<usize>>,
        },
        UnregisterEvent {
            ptr: usize,
            reply: mpsc::Sender<io::Result<()>>,
        },
        SendEvent {
            id: String,
            args: Vec<(String, String)>,
            reply: mpsc::Sender<io::Result<i32>>,
        },
        Invoke {
            object: String,
            method: String,
            args: Vec<(String, String)>,
            reply: mpsc::Sender<io::Result<Vec<u8>>>,
        },
        Shutdown {
            reply: mpsc::Sender<()>,
        },
    }

    struct ObjectInfo {
        ptr: usize,
        id: u32,
    }

    struct HostState {
        ctx: *mut sys::ubus_context,
        objects: Vec<usize>,
        subscribers: Vec<usize>,
        events: Vec<usize>,
        callbacks: Vec<CallbackStorage>,
        deferred: Arc<Mutex<HashSet<usize>>>,
    }

    impl LibuboxHost {
        /// Connect and start the manually-pumped owning thread.
        pub fn connect(socket_path: &Path) -> io::Result<Option<Self>> {
            let (tx, rx) = mpsc::channel();
            let (ready_tx, ready_rx) = mpsc::channel();
            let shutdown = Arc::new(AtomicBool::new(false));
            let thread_shutdown = Arc::clone(&shutdown);
            let path = socket_path.to_owned();
            let join = thread::Builder::new()
                .name("libubox-host".into())
                .spawn(move || host_thread(path, rx, ready_tx, thread_shutdown))?;

            match ready_rx.recv_timeout(COMMAND_TIMEOUT) {
                Ok(Ok(())) => Ok(Some(Self {
                    tx,
                    join: Mutex::new(Some(join)),
                    shutdown,
                })),
                Ok(Err(err)) => {
                    let _ = join.join();
                    Err(err)
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // A connected host always reports readiness before it
                    // enters the pump, so this is a loud fixture failure.
                    panic!("LibuboxHost connect thread did not report readiness within 5s");
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("LibuboxHost connect thread died before reporting readiness");
                }
            }
        }

        pub fn add_object(
            &self,
            name: &str,
            methods: &[&str],
            on_invoke: impl FnMut(&str, &[u8]) -> (i32, Option<Vec<u8>>) + Send + 'static,
        ) -> io::Result<HostObject> {
            let info = self.ask(|reply| Cmd::AddObject {
                name: name.into(),
                methods: methods.iter().map(|method| (*method).into()).collect(),
                dispatch: ObjectDispatch::Immediate(Box::new(on_invoke)),
                reply,
            })?;
            Ok(HostObject {
                ptr: info.ptr,
                id: info.id,
                _not_send: PhantomData,
            })
        }

        /// Add an object whose handler can consume the caller's optional fd
        /// and attach one to its terminal STATUS reply. The returned reply fd
        /// is transferred to libubus before any reply bytes are sent.
        pub fn add_object_with_fd_reply(
            &self,
            name: &str,
            methods: &[&str],
            on_invoke: impl FnMut(Option<OwnedFd>, &str, &[u8]) -> (i32, Option<Vec<u8>>, Option<OwnedFd>)
                + Send
                + 'static,
        ) -> io::Result<HostObject> {
            let info = self.ask(|reply| Cmd::AddObject {
                name: name.into(),
                methods: methods.iter().map(|method| (*method).into()).collect(),
                dispatch: ObjectDispatch::FdReply(Box::new(on_invoke)),
                reply,
            })?;
            Ok(HostObject {
                ptr: info.ptr,
                id: info.id,
                _not_send: PhantomData,
            })
        }

        pub fn remove_object(&self, obj: &HostObject) -> io::Result<()> {
            self.ask(|reply| Cmd::RemoveObject {
                ptr: obj.ptr,
                reply,
            })
        }

        pub fn add_object_deferring(
            &self,
            name: &str,
            methods: &[&str],
            on_invoke: impl FnMut(&str, &[u8]) -> DeferChoice + Send + 'static,
        ) -> io::Result<(HostObject, mpsc::Receiver<DeferredReply>)> {
            let (deferred_tx, deferred_rx) = mpsc::channel();
            let info = self.ask(|reply| Cmd::AddObject {
                name: name.into(),
                methods: methods.iter().map(|method| (*method).into()).collect(),
                dispatch: ObjectDispatch::Deferred {
                    callback: Box::new(on_invoke),
                    deferred_tx,
                    deferred: None,
                },
                reply,
            })?;
            Ok((
                HostObject {
                    ptr: info.ptr,
                    id: info.id,
                    _not_send: PhantomData,
                },
                deferred_rx,
            ))
        }

        pub fn complete_deferred(
            &self,
            token: DeferredReply,
            status: i32,
            reply: Option<Vec<u8>>,
        ) -> io::Result<()> {
            self.ask(|reply_tx| Cmd::CompleteDeferred {
                request: token.request,
                status,
                reply_blob: reply,
                reply: reply_tx,
            })
        }

        pub fn register_subscriber(
            &self,
            on_notify: impl FnMut(&str, &[u8]) -> i32 + Send + 'static,
            on_removed: impl FnMut(u32) + Send + 'static,
        ) -> io::Result<HostSubscriber> {
            let ptr = self.ask(|reply| Cmd::RegisterSubscriber {
                dispatch: SubscriberDispatch {
                    notify: Box::new(on_notify),
                    removed: Box::new(on_removed),
                },
                reply,
            })?;
            Ok(HostSubscriber {
                ptr,
                _not_send: PhantomData,
            })
        }

        pub fn subscribe(&self, sub: &HostSubscriber, target_id: u32) -> io::Result<()> {
            self.ask(|reply| Cmd::Subscribe {
                ptr: sub.ptr,
                target_id,
                reply,
            })
        }

        pub fn unsubscribe(&self, sub: &HostSubscriber, target_id: u32) -> io::Result<()> {
            self.ask(|reply| Cmd::Unsubscribe {
                ptr: sub.ptr,
                target_id,
                reply,
            })
        }

        /// While a synchronous invoke or acknowledged notify is active,
        /// libubus may service frames re-entrantly for that request. Tests
        /// must not concurrently choreograph an inbound invoke for this
        /// host's own objects; use scenario barriers around such calls.
        pub fn notify(
            &self,
            obj: &HostObject,
            ty: &str,
            args: &[(&str, &str)],
            timeout_ms: i32,
        ) -> io::Result<i32> {
            self.ask(|reply| Cmd::Notify {
                ptr: obj.ptr,
                ty: ty.into(),
                args: owned_args(args),
                timeout_ms,
                reply,
            })
        }

        pub fn register_event_handler(
            &self,
            pattern: &str,
            on_event: impl FnMut(&str, &[u8]) + Send + 'static,
        ) -> io::Result<HostEventHandler> {
            let ptr = self.ask(|reply| Cmd::RegisterEvent {
                pattern: pattern.into(),
                callback: Box::new(on_event),
                reply,
            })?;
            Ok(HostEventHandler {
                ptr,
                _not_send: PhantomData,
            })
        }

        pub fn unregister_event_handler(&self, ev: HostEventHandler) -> io::Result<()> {
            self.ask(|reply| Cmd::UnregisterEvent { ptr: ev.ptr, reply })
        }

        pub fn send_event(&self, id: &str, args: &[(&str, &str)]) -> io::Result<i32> {
            self.ask(|reply| Cmd::SendEvent {
                id: id.into(),
                args: owned_args(args),
                reply,
            })
        }

        pub fn invoke(
            &self,
            object: &str,
            method: &str,
            args: &[(&str, &str)],
        ) -> io::Result<Vec<u8>> {
            self.ask(|reply| Cmd::Invoke {
                object: object.into(),
                method: method.into(),
                args: owned_args(args),
                reply,
            })
        }

        /// Stop the host after freeing every C-side role on its owning thread.
        pub fn shutdown(self) -> Result<(), String> {
            self.shutdown.store(true, Ordering::Release);
            let (reply_tx, reply_rx) = mpsc::channel();
            self.tx
                .send(Cmd::Shutdown { reply: reply_tx })
                .map_err(|_| "LibuboxHost thread is dead before shutdown".to_owned())?;
            reply_rx
                .recv_timeout(COMMAND_TIMEOUT)
                .map_err(|_| "LibuboxHost shutdown did not complete within 5s".to_owned())?;
            let join = self
                .join
                .lock()
                .map_err(|_| "LibuboxHost join mutex is poisoned".to_owned())?
                .take()
                .ok_or_else(|| "LibuboxHost shutdown was already called".to_owned())?;
            join.join()
                .map_err(|_| "LibuboxHost thread panicked during shutdown".to_owned())
        }

        fn ask<T>(&self, make: impl FnOnce(mpsc::Sender<io::Result<T>>) -> Cmd) -> io::Result<T> {
            if self.shutdown.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "LibuboxHost is shut down",
                ));
            }
            let (reply_tx, reply_rx) = mpsc::channel();
            self.tx.send(make(reply_tx)).map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "LibuboxHost thread is dead")
            })?;
            match reply_rx.recv_timeout(COMMAND_TIMEOUT) {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("LibuboxHost command did not complete within 5s")
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("LibuboxHost thread died while handling a command")
                }
            }
        }
    }

    impl Drop for LibuboxHost {
        fn drop(&mut self) {
            if self.shutdown.swap(true, Ordering::AcqRel) {
                return;
            }
            let (reply_tx, _) = mpsc::channel();
            // Explicit `shutdown` is the loud path. Drop still asks the host
            // to clean up rather than freeing libubus objects on this thread.
            let _ = self.tx.send(Cmd::Shutdown { reply: reply_tx });
        }
    }

    fn owned_args(args: &[(&str, &str)]) -> Vec<(String, String)> {
        args.iter()
            .map(|(name, value)| ((*name).into(), (*value).into()))
            .collect()
    }

    fn host_thread(
        socket_path: std::path::PathBuf,
        rx: mpsc::Receiver<Cmd>,
        ready_tx: mpsc::Sender<io::Result<()>>,
        shutdown: Arc<AtomicBool>,
    ) {
        let path = match CString::new(socket_path.as_os_str().as_bytes()) {
            Ok(path) => path,
            Err(err) => {
                let _ = ready_tx.send(Err(io::Error::new(io::ErrorKind::InvalidInput, err)));
                return;
            }
        };
        let ctx = {
            let _libubus = libubus_global_lock();
            // SAFETY: `path` is a NUL-terminated socket path valid for this call.
            unsafe { sys::ubus_connect(path.as_ptr()) }
        };
        if ctx.is_null() {
            let _ = ready_tx.send(Err(io::Error::other(format!(
                "libubox ubus_connect({}) returned NULL",
                socket_path.display()
            ))));
            return;
        }
        let mut state = HostState {
            ctx,
            objects: Vec::new(),
            subscribers: Vec::new(),
            events: Vec::new(),
            callbacks: Vec::new(),
            deferred: Arc::new(Mutex::new(HashSet::new())),
        };
        CALLBACK_CTX.with(|callback_ctx| callback_ctx.set(ctx));
        let _ = ready_tx.send(Ok(()));

        let mut running = true;
        while running {
            let fd = {
                let _libubus = libubus_global_lock();
                // SAFETY: the context remains owned by this thread until cleanup.
                unsafe { sys::ubus_zero_ubus_ctx_fd(state.ctx) }
            };
            if fd >= 0 {
                // SAFETY: libubus owns this context socket and it remains alive
                // for the scope of the poll call.
                let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
                let mut fds = [PollFd::new(&borrowed, PollFlags::IN)];
                match poll(&mut fds, Some(&PUMP_TIMEOUT)) {
                    Ok(_)
                        if fds[0]
                            .revents()
                            .intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP) =>
                    {
                        let _libubus = libubus_global_lock();
                        // SAFETY: `state.ctx` is live and called only by its
                        // owning host thread.
                        unsafe { sys::ubus_zero_ubus_handle_event(state.ctx) };
                    }
                    Ok(_) | Err(Errno::INTR) => {}
                    Err(_) => {}
                }
            }
            while let Ok(cmd) = rx.try_recv() {
                if handle_cmd(&mut state, cmd) {
                    running = false;
                    break;
                }
            }
        }
        shutdown.store(true, Ordering::Release);
    }

    fn handle_cmd(state: &mut HostState, cmd: Cmd) -> bool {
        // libubus serializes every outgoing request through a process-global
        // blob buffer. This deliberately covers synchronous invoke/notify
        // waits; callbacks re-enter on this same host thread without locking.
        let _libubus = libubus_global_lock();
        match cmd {
            Cmd::AddObject {
                name,
                methods,
                dispatch,
                reply,
            } => {
                let _ = reply.send(add_object(state, &name, &methods, dispatch));
            }
            Cmd::RemoveObject { ptr, reply } => {
                let _ = reply.send(remove_object(state, ptr));
            }
            Cmd::CompleteDeferred {
                request,
                status,
                reply_blob,
                reply,
            } => {
                let _ = reply.send(complete_deferred(state, request, status, reply_blob));
            }
            Cmd::RegisterSubscriber { dispatch, reply } => {
                let _ = reply.send(register_subscriber(state, dispatch));
            }
            Cmd::Subscribe {
                ptr,
                target_id,
                reply,
            } => {
                let _ = reply.send(subscribe(state, ptr, target_id, false));
            }
            Cmd::Unsubscribe {
                ptr,
                target_id,
                reply,
            } => {
                let _ = reply.send(subscribe(state, ptr, target_id, true));
            }
            Cmd::Notify {
                ptr,
                ty,
                args,
                timeout_ms,
                reply,
            } => {
                let _ = reply.send(notify(state, ptr, &ty, &args, timeout_ms));
            }
            Cmd::RegisterEvent {
                pattern,
                callback,
                reply,
            } => {
                let _ = reply.send(register_event(state, &pattern, callback));
            }
            Cmd::UnregisterEvent { ptr, reply } => {
                let _ = reply.send(unregister_event(state, ptr));
            }
            Cmd::SendEvent { id, args, reply } => {
                let _ = reply.send(send_event(state, &id, &args));
            }
            Cmd::Invoke {
                object,
                method,
                args,
                reply,
            } => {
                let _ = reply.send(invoke(state, &object, &method, &args));
            }
            Cmd::Shutdown { reply } => {
                cleanup(state);
                let _ = reply.send(());
                return true;
            }
        }
        false
    }

    fn add_object(
        state: &mut HostState,
        name: &str,
        methods: &[String],
        mut dispatch: ObjectDispatch,
    ) -> io::Result<ObjectInfo> {
        let name_c = cstring(name)?;
        let method_c: Result<Vec<_>, _> = methods.iter().map(|method| cstring(method)).collect();
        let method_c = method_c?;
        let method_ptrs: Vec<_> = method_c.iter().map(|method| method.as_ptr()).collect();
        if let ObjectDispatch::Deferred { deferred, .. } = &mut dispatch {
            *deferred = Some(Arc::clone(&state.deferred));
        }
        let callback = Box::into_raw(Box::new(Box::new(dispatch)));
        // SAFETY: strings and method pointer array live through the constructor;
        // C duplicates all names and retains only `callback` as its context.
        let obj = unsafe {
            sys::ubus_zero_testkit_object_create(
                name_c.as_ptr(),
                method_ptrs.as_ptr(),
                method_ptrs.len() as i32,
                Some(object_cb),
                callback.cast(),
            )
        };
        if obj.is_null() {
            // SAFETY: C did not retain the context after returning NULL.
            unsafe { drop(Box::from_raw(callback)) };
            return Err(io::Error::other(format!(
                "ubus_zero_testkit_object_create({name}) returned NULL"
            )));
        }
        // SAFETY: both pointers are owned by this host thread and `obj` is
        // initialized by the C constructor.
        let rc = unsafe { sys::ubus_add_object(state.ctx, obj) };
        if rc != 0 {
            // SAFETY: registration failed, so C no longer references obj or
            // its callback context.
            unsafe {
                sys::ubus_zero_testkit_object_free(obj);
                drop(Box::from_raw(callback));
            }
            return Err(rc_error("ubus_add_object", rc));
        }
        let id = match lookup_id(state.ctx, name) {
            Ok(id) => id,
            Err(err) => {
                // SAFETY: remove/free happen on the same context owner.
                unsafe {
                    sys::ubus_remove_object(state.ctx, obj);
                    sys::ubus_zero_testkit_object_free(obj);
                    drop(Box::from_raw(callback));
                }
                return Err(err);
            }
        };
        state.objects.push(obj as usize);
        state.callbacks.push(CallbackStorage::Object(callback));
        Ok(ObjectInfo {
            ptr: obj as usize,
            id,
        })
    }

    fn remove_object(state: &mut HostState, ptr: usize) -> io::Result<()> {
        if !state.objects.contains(&ptr) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unknown host object",
            ));
        }
        let obj = ptr as *mut sys::ubus_object;
        // SAFETY: `obj` is tracked as a live object for this owning context.
        let rc = unsafe { sys::ubus_remove_object(state.ctx, obj) };
        if rc != 0 {
            return Err(rc_error("ubus_remove_object", rc));
        }
        // SAFETY: it was removed from libubus above and C owns its allocation.
        unsafe { sys::ubus_zero_testkit_object_free(obj) };
        state.objects.retain(|candidate| *candidate != ptr);
        Ok(())
    }

    fn register_subscriber(
        state: &mut HostState,
        dispatch: SubscriberDispatch,
    ) -> io::Result<usize> {
        let callback = Box::into_raw(Box::new(Box::new(dispatch)));
        // SAFETY: C stores the callback pointer unchanged until the subscriber
        // is freed, and `callback` is retained in `state.callbacks` on success.
        let sub = unsafe {
            sys::ubus_zero_testkit_subscriber_create(
                Some(notify_cb),
                Some(removed_cb),
                callback.cast(),
            )
        };
        if sub.is_null() {
            // SAFETY: C retained nothing on failure.
            unsafe { drop(Box::from_raw(callback)) };
            return Err(io::Error::other(
                "ubus_zero_testkit_subscriber_create returned NULL",
            ));
        }
        // SAFETY: `sub` is C-initialized and this is its owning context.
        let rc = unsafe { sys::ubus_register_subscriber(state.ctx, sub) };
        if rc != 0 {
            // SAFETY: failed registration leaves C-side ownership with us.
            unsafe {
                sys::ubus_zero_testkit_subscriber_free(sub);
                drop(Box::from_raw(callback));
            }
            return Err(rc_error("ubus_register_subscriber", rc));
        }
        state.subscribers.push(sub as usize);
        state.callbacks.push(CallbackStorage::Subscriber(callback));
        Ok(sub as usize)
    }

    fn subscribe(
        state: &mut HostState,
        ptr: usize,
        target_id: u32,
        remove: bool,
    ) -> io::Result<()> {
        if !state.subscribers.contains(&ptr) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unknown host subscriber",
            ));
        }
        let sub = ptr as *mut sys::ubus_subscriber;
        // SAFETY: `sub` is tracked as live and the context is host-thread-owned.
        let rc = unsafe {
            if remove {
                sys::ubus_unsubscribe(state.ctx, sub, target_id)
            } else {
                sys::ubus_subscribe(state.ctx, sub, target_id)
            }
        };
        if rc != 0 {
            return Err(rc_error(
                if remove {
                    "ubus_unsubscribe"
                } else {
                    "ubus_subscribe"
                },
                rc,
            ));
        }
        Ok(())
    }

    fn notify(
        state: &mut HostState,
        ptr: usize,
        ty: &str,
        args: &[(String, String)],
        timeout_ms: i32,
    ) -> io::Result<i32> {
        if !state.objects.contains(&ptr) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unknown host object",
            ));
        }
        let ty_c = cstring(ty)?;
        let msg = string_args(args)?;
        // SAFETY: object, context, type string, and blob buffer remain valid
        // through this synchronous libubus call on the owning thread.
        Ok(unsafe {
            sys::ubus_notify(
                state.ctx,
                ptr as *mut sys::ubus_object,
                ty_c.as_ptr(),
                msg.head(),
                timeout_ms,
            )
        })
    }

    fn register_event(
        state: &mut HostState,
        pattern: &str,
        callback: EventCallback,
    ) -> io::Result<usize> {
        let pattern_c = cstring(pattern)?;
        let callback = Box::into_raw(Box::new(Box::new(callback)));
        // SAFETY: C retains the context pointer until the event handler is freed.
        let ev =
            unsafe { sys::ubus_zero_testkit_event_handler_create(Some(event_cb), callback.cast()) };
        if ev.is_null() {
            // SAFETY: no C object was created to reference this allocation.
            unsafe { drop(Box::from_raw(callback)) };
            return Err(io::Error::other(
                "ubus_zero_testkit_event_handler_create returned NULL",
            ));
        }
        // SAFETY: `ev` is a C-initialized handler for this owning context.
        let rc = unsafe { sys::ubus_register_event_handler(state.ctx, ev, pattern_c.as_ptr()) };
        if rc != 0 {
            // SAFETY: registration failed and both allocations remain ours.
            unsafe {
                sys::ubus_zero_testkit_event_handler_free(ev);
                drop(Box::from_raw(callback));
            }
            return Err(rc_error("ubus_register_event_handler", rc));
        }
        state.events.push(ev as usize);
        state.callbacks.push(CallbackStorage::Event(callback));
        Ok(ev as usize)
    }

    fn unregister_event(state: &mut HostState, ptr: usize) -> io::Result<()> {
        if !state.events.contains(&ptr) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unknown event handler",
            ));
        }
        let ev = ptr as *mut sys::ubus_event_handler;
        // SAFETY: this live event handler belongs to `state.ctx`.
        let rc = unsafe { sys::ubus_zero_ubus_unregister_event_handler(state.ctx, ev) };
        if rc != 0 {
            return Err(rc_error("ubus_unregister_event_handler", rc));
        }
        // SAFETY: handler is no longer registered and C owns its storage.
        unsafe { sys::ubus_zero_testkit_event_handler_free(ev) };
        state.events.retain(|candidate| *candidate != ptr);
        Ok(())
    }

    fn send_event(state: &mut HostState, id: &str, args: &[(String, String)]) -> io::Result<i32> {
        let id_c = cstring(id)?;
        let msg = string_args(args)?;
        // SAFETY: context, id string, and blob buffer are live for the call.
        Ok(unsafe { sys::ubus_send_event(state.ctx, id_c.as_ptr(), msg.head()) })
    }

    thread_local! {
        static REPLY: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
    }

    fn invoke(
        state: &mut HostState,
        object: &str,
        method: &str,
        args: &[(String, String)],
    ) -> io::Result<Vec<u8>> {
        let method_c = cstring(method)?;
        let id = lookup_id(state.ctx, object)?;
        let msg = string_args(args)?;
        REPLY.with(|reply| *reply.borrow_mut() = None);
        // SAFETY: all inputs live through this synchronous call; `data_cb`
        // copies any transient response before libubus releases it.
        let rc = unsafe {
            sys::ubus_invoke_fd(
                state.ctx,
                id,
                method_c.as_ptr(),
                msg.head(),
                Some(data_cb),
                ptr::null_mut(),
                sys::UBUS_INVOKE_TIMEOUT_MS,
                -1,
            )
        };
        if rc != 0 {
            return Err(io::Error::other(format!(
                "ubus_invoke({object}.{method}) returned {rc}"
            )));
        }
        Ok(REPLY.with(|reply| reply.borrow_mut().take().unwrap_or_default()))
    }

    fn complete_deferred(
        state: &mut HostState,
        request: usize,
        status: i32,
        reply: Option<Vec<u8>>,
    ) -> io::Result<()> {
        if !state
            .deferred
            .lock()
            .map_err(|_| io::Error::other("deferred reply state is poisoned"))?
            .remove(&request)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unknown or already-completed deferred reply",
            ));
        }
        let reply_result = if let Some(contents) = reply {
            match ReplyBlob::new(&contents) {
                Ok(blob) => {
                    // SAFETY: the deferred request and host context are live,
                    // and the temporary reply blob remains valid for this call.
                    let rc = unsafe {
                        sys::ubus_send_reply(
                            state.ctx,
                            request as *mut sys::ubus_request_data,
                            blob.head(),
                        )
                    };
                    if rc == 0 {
                        Ok(())
                    } else {
                        Err(rc_error("ubus_send_reply(deferred)", rc))
                    }
                }
                Err(err) => Err(err),
            }
        } else {
            Ok(())
        };
        // SAFETY: `request` came from the C defer helper and is consumed once.
        unsafe {
            sys::ubus_zero_testkit_complete_deferred(
                state.ctx,
                request as *mut sys::ubus_request_data,
                status,
            )
        };
        reply_result
    }

    fn lookup_id(ctx: *mut sys::ubus_context, object: &str) -> io::Result<u32> {
        let object_c = cstring(object)?;
        let mut id = 0;
        // SAFETY: ctx is the live host context and `object_c` lives for call.
        let rc = unsafe { sys::ubus_lookup_id(ctx, object_c.as_ptr(), &mut id) };
        if rc != 0 {
            return Err(io::Error::other(format!(
                "ubus_lookup_id({object}) returned {rc}"
            )));
        }
        Ok(id)
    }

    fn string_args(args: &[(String, String)]) -> io::Result<BlobBufGuard> {
        let mut msg = BlobBufGuard::new()?;
        for (name, value) in args {
            msg.add_string(name, value)?;
        }
        Ok(msg)
    }

    fn cstring(value: &str) -> io::Result<CString> {
        CString::new(value).map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))
    }

    fn rc_error(op: &str, rc: i32) -> io::Error {
        io::Error::other(format!("{op} returned {rc}"))
    }

    fn cleanup(state: &mut HostState) {
        let deferred: Vec<_> = state
            .deferred
            .lock()
            .expect("deferred reply state poisoned during host cleanup")
            .drain()
            .collect();
        for request in deferred {
            // SAFETY: each request was allocated by the defer helper. Complete
            // it during teardown so the helper frees its C-side allocation.
            unsafe {
                sys::ubus_zero_testkit_complete_deferred(
                    state.ctx,
                    request as *mut sys::ubus_request_data,
                    -1,
                )
            };
        }
        for ev in state.events.drain(..) {
            // SAFETY: all handlers belong to this host context and C owns them.
            unsafe {
                let ev = ev as *mut sys::ubus_event_handler;
                let _ = sys::ubus_zero_ubus_unregister_event_handler(state.ctx, ev);
                sys::ubus_zero_testkit_event_handler_free(ev);
            }
        }
        for sub in state.subscribers.drain(..) {
            // SAFETY: all subscribers belong to this host context and C owns them.
            unsafe {
                let sub = sub as *mut sys::ubus_subscriber;
                let _ = sys::ubus_zero_ubus_unregister_subscriber(state.ctx, sub);
                sys::ubus_zero_testkit_subscriber_free(sub);
            }
        }
        for obj in state.objects.drain(..) {
            // SAFETY: all objects belong to this host context and C owns them.
            unsafe {
                let obj = obj as *mut sys::ubus_object;
                let _ = sys::ubus_remove_object(state.ctx, obj);
                sys::ubus_zero_testkit_object_free(obj);
            }
        }
        for callback in state.callbacks.drain(..) {
            // SAFETY: each C-side role using this callback was freed above.
            unsafe { callback.drop_box() };
        }
        // SAFETY: context is now free of host-owned role wrappers and remains
        // owned exclusively by this host thread.
        unsafe { sys::ubus_free(state.ctx) };
        state.ctx = ptr::null_mut();
    }

    /// Turn blobmsg table contents into a root `blob_attr` for send_reply.
    /// The words keep the C-facing pointer suitably aligned; the length in the
    /// libubox attr header is network-byte-order and excludes tail padding.
    struct ReplyBlob {
        words: Vec<u32>,
    }

    impl ReplyBlob {
        fn new(contents: &[u8]) -> io::Result<Self> {
            let len = contents
                .len()
                .checked_add(4)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "reply too large"))?;
            let len_u32 = u32::try_from(len)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "reply too large"))?;
            let mut words = vec![0_u32; len.div_ceil(4)];
            // SAFETY: `u32` storage is aligned and has at least `len` bytes;
            // the byte view is used only to populate the opaque C blob bytes.
            let bytes = unsafe {
                core::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), words.len() * 4)
            };
            bytes[..4].copy_from_slice(&len_u32.to_be_bytes());
            bytes[4..len].copy_from_slice(contents);
            Ok(Self { words })
        }

        fn head(&self) -> *mut sys::blob_attr {
            self.words.as_ptr().cast_mut().cast()
        }
    }

    unsafe extern "C" fn object_cb(
        rust_ctx: *mut core::ffi::c_void,
        method: *const core::ffi::c_char,
        msg: *mut sys::blob_attr,
        req: *mut sys::ubus_request_data,
    ) -> core::ffi::c_int {
        if rust_ctx.is_null() {
            return -1;
        }
        // SAFETY: C receives the exact double-box pointer created in
        // `add_object`; it stays alive until after C frees the object.
        let dispatch = unsafe { &mut **rust_ctx.cast::<Box<ObjectDispatch>>() };
        let method = c_string(method);
        let msg = blob_contents(msg);
        match dispatch {
            ObjectDispatch::Immediate(callback) => {
                let (status, reply) = callback(&method, &msg);
                match send_immediate_reply(req, reply) {
                    Ok(()) => status,
                    Err(rc) => rc,
                }
            }
            ObjectDispatch::FdReply(callback) => {
                // SAFETY: libubus transfers the received request fd to this
                // callback; the OwnedFd closes it after the handler has
                // consumed it.
                let fd = unsafe { sys::ubus_zero_ubus_request_get_caller_fd(req) };
                let caller_fd = (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) });
                let (status, reply, reply_fd) = callback(caller_fd, &method, &msg);
                if let Some(reply_fd) = reply_fd {
                    CALLBACK_CTX.with(|ctx| {
                        // SAFETY: this callback owns both the context and
                        // request for its duration; libubus takes ownership
                        // of the descriptor supplied by into_raw_fd.
                        unsafe {
                            sys::ubus_zero_ubus_request_set_fd(
                                ctx.get(),
                                req,
                                reply_fd.into_raw_fd(),
                            )
                        };
                    });
                }
                match send_immediate_reply(req, reply) {
                    Ok(()) => status,
                    Err(rc) => rc,
                }
            }
            ObjectDispatch::Deferred {
                callback,
                deferred_tx,
                deferred,
            } => match callback(&method, &msg) {
                DeferChoice::Reply(status, reply) => match send_immediate_reply(req, reply) {
                    Ok(()) => status,
                    Err(rc) => rc,
                },
                DeferChoice::Defer => {
                    let request = CALLBACK_CTX.with(|ctx| {
                        // SAFETY: callback context is the host-owned libubus
                        // context and `req` is valid for this callback.
                        unsafe { sys::ubus_zero_testkit_defer_request(ctx.get(), req) }
                    });
                    if request.is_null() {
                        return -1;
                    }
                    let request = request as usize;
                    let Some(deferred) = deferred else {
                        return -1;
                    };
                    let Ok(mut requests) = deferred.lock() else {
                        return -1;
                    };
                    requests.insert(request);
                    drop(requests);
                    if deferred_tx.send(DeferredReply { request }).is_err() {
                        // The requester has gone away. Keep the request in the
                        // teardown set so host shutdown completes and frees it.
                    }
                    0
                }
            },
        }
    }

    // A thread-local context is set only while libubus invokes one of our
    // callbacks from `ubus_zero_ubus_handle_event` or a synchronous operation.
    thread_local! {
        static CALLBACK_CTX: std::cell::Cell<*mut sys::ubus_context> = const { std::cell::Cell::new(ptr::null_mut()) };
    }

    fn send_immediate_reply(
        req: *mut sys::ubus_request_data,
        reply: Option<Vec<u8>>,
    ) -> Result<(), i32> {
        let Some(contents) = reply else { return Ok(()) };
        let blob = ReplyBlob::new(&contents).map_err(|_| -1)?;
        let rc = CALLBACK_CTX.with(|ctx| {
            // SAFETY: callback context is set by the host around every
            // libubus dispatch; `req` and temporary blob live for this call.
            unsafe { sys::ubus_send_reply(ctx.get(), req, blob.head()) }
        });
        if rc == 0 {
            Ok(())
        } else {
            Err(rc)
        }
    }

    unsafe extern "C" fn notify_cb(
        rust_ctx: *mut core::ffi::c_void,
        method: *const core::ffi::c_char,
        msg: *mut sys::blob_attr,
    ) -> core::ffi::c_int {
        if rust_ctx.is_null() {
            return -1;
        }
        // SAFETY: pointer ownership/lifetime matches `register_subscriber`.
        let dispatch = unsafe { &mut **rust_ctx.cast::<Box<SubscriberDispatch>>() };
        (dispatch.notify)(&c_string(method), &blob_contents(msg))
    }

    unsafe extern "C" fn removed_cb(rust_ctx: *mut core::ffi::c_void, id: u32) {
        if rust_ctx.is_null() {
            return;
        }
        // SAFETY: pointer ownership/lifetime matches `register_subscriber`.
        let dispatch = unsafe { &mut **rust_ctx.cast::<Box<SubscriberDispatch>>() };
        (dispatch.removed)(id);
    }

    unsafe extern "C" fn event_cb(
        rust_ctx: *mut core::ffi::c_void,
        ty: *const core::ffi::c_char,
        msg: *mut sys::blob_attr,
    ) {
        if rust_ctx.is_null() {
            return;
        }
        // SAFETY: pointer ownership/lifetime matches `register_event`.
        let callback = unsafe { &mut **rust_ctx.cast::<Box<EventCallback>>() };
        callback(&c_string(ty), &blob_contents(msg));
    }

    extern "C" fn data_cb(
        _req: *mut sys::ubus_request,
        _ty: core::ffi::c_int,
        msg: *mut sys::blob_attr,
    ) {
        REPLY.with(|reply| *reply.borrow_mut() = Some(blob_contents(msg)));
    }

    fn c_string(value: *const core::ffi::c_char) -> String {
        if value.is_null() {
            return String::new();
        }
        // SAFETY: libubus supplies a NUL-terminated method or event string for
        // the duration of this callback; lossy conversion owns the result.
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }

    fn blob_contents(msg: *mut sys::blob_attr) -> Vec<u8> {
        if msg.is_null() {
            return Vec::new();
        }
        // SAFETY: libubus owns `msg` for this callback and reports its valid
        // contents length through the paired accessors.
        let (data, len) = unsafe { (sys::blob_data(msg), sys::blob_len(msg) as usize) };
        if data.is_null() || len == 0 {
            return Vec::new();
        }
        // SAFETY: source bytes remain valid for this callback; `to_vec` copies
        // them before control returns to libubus.
        unsafe { core::slice::from_raw_parts(data.cast::<u8>(), len) }.to_vec()
    }
}

#[cfg(have_libubox)]
pub use inner::{
    DeferChoice, DeferredReply, HostEventHandler, HostObject, HostSubscriber, LibuboxHost,
};

#[cfg(not(have_libubox))]
pub struct LibuboxHost {
    _never: core::convert::Infallible,
}

#[cfg(not(have_libubox))]
pub struct HostObject {
    _never: core::convert::Infallible,
}

#[cfg(not(have_libubox))]
pub struct HostSubscriber {
    _never: core::convert::Infallible,
}

#[cfg(not(have_libubox))]
pub struct HostEventHandler {
    _never: core::convert::Infallible,
}

#[cfg(not(have_libubox))]
pub struct DeferredReply {
    _never: core::convert::Infallible,
}

#[cfg(not(have_libubox))]
pub enum DeferChoice {
    Reply(i32, Option<Vec<u8>>),
    Defer,
}

#[cfg(not(have_libubox))]
impl LibuboxHost {
    pub fn connect(_socket_path: &Path) -> io::Result<Option<Self>> {
        if libubus_artifact_exists() {
            panic!(
                "libubox-sys was built without cfg(have_libubox), but .dep/bin/libubus.so exists; \
                 this usually means Cargo replayed a poisoned failed libubox-sys build-script \
                 probe cache. Remedy: touch ubus/libubox-sys/build.rs and rebuild."
            );
        }
        Ok(None)
    }

    pub fn add_object(
        &self,
        _name: &str,
        _methods: &[&str],
        _on_invoke: impl FnMut(&str, &[u8]) -> (i32, Option<Vec<u8>>) + Send + 'static,
    ) -> io::Result<HostObject> {
        match self._never {}
    }

    pub fn add_object_with_fd_reply(
        &self,
        _name: &str,
        _methods: &[&str],
        _on_invoke: impl FnMut(
                Option<rustix::fd::OwnedFd>,
                &str,
                &[u8],
            ) -> (i32, Option<Vec<u8>>, Option<rustix::fd::OwnedFd>)
            + Send
            + 'static,
    ) -> io::Result<HostObject> {
        match self._never {}
    }

    pub fn remove_object(&self, _obj: &HostObject) -> io::Result<()> {
        match self._never {}
    }

    pub fn add_object_deferring(
        &self,
        _name: &str,
        _methods: &[&str],
        _on_invoke: impl FnMut(&str, &[u8]) -> DeferChoice + Send + 'static,
    ) -> io::Result<(HostObject, std::sync::mpsc::Receiver<DeferredReply>)> {
        match self._never {}
    }

    pub fn complete_deferred(
        &self,
        _token: DeferredReply,
        _status: i32,
        _reply: Option<Vec<u8>>,
    ) -> io::Result<()> {
        match self._never {}
    }

    pub fn register_subscriber(
        &self,
        _on_notify: impl FnMut(&str, &[u8]) -> i32 + Send + 'static,
        _on_removed: impl FnMut(u32) + Send + 'static,
    ) -> io::Result<HostSubscriber> {
        match self._never {}
    }

    pub fn subscribe(&self, _sub: &HostSubscriber, _target_id: u32) -> io::Result<()> {
        match self._never {}
    }

    pub fn unsubscribe(&self, _sub: &HostSubscriber, _target_id: u32) -> io::Result<()> {
        match self._never {}
    }

    pub fn notify(
        &self,
        _obj: &HostObject,
        _ty: &str,
        _args: &[(&str, &str)],
        _timeout_ms: i32,
    ) -> io::Result<i32> {
        match self._never {}
    }

    pub fn register_event_handler(
        &self,
        _pattern: &str,
        _on_event: impl FnMut(&str, &[u8]) + Send + 'static,
    ) -> io::Result<HostEventHandler> {
        match self._never {}
    }

    pub fn unregister_event_handler(&self, _ev: HostEventHandler) -> io::Result<()> {
        match self._never {}
    }

    pub fn send_event(&self, _id: &str, _args: &[(&str, &str)]) -> io::Result<i32> {
        match self._never {}
    }

    pub fn invoke(
        &self,
        _object: &str,
        _method: &str,
        _args: &[(&str, &str)],
    ) -> io::Result<Vec<u8>> {
        match self._never {}
    }

    pub fn shutdown(self) -> Result<(), String> {
        match self._never {}
    }
}

#[cfg(not(have_libubox))]
impl HostObject {
    pub fn id(&self) -> u32 {
        match self._never {}
    }
}

#[cfg(not(have_libubox))]
fn libubus_artifact_exists() -> bool {
    [
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../.dep/bin/libubus.so"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/../.dep/bin/libubus.so"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/.dep/bin/libubus.so"),
    ]
    .iter()
    .any(|path| Path::new(path).is_file())
}
