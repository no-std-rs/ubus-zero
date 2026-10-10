//! Smoke probe for the libubox-sys FFI: tries `ubus_connect("/dev/null")`,
//! prints the result, exits cleanly. Intended as a one-shot sanity check
//! before the testkit fixture goes live (Verification step 3 in
//! the libubox-sys plan); delete after PR1 lands.

#[cfg(have_libubox)]
fn main() {
    let path = c"/dev/null";
    let ctx = unsafe { libubox_sys::ubus_connect(path.as_ptr()) };
    println!("ubus_connect('/dev/null') -> {ctx:p}");
    if !ctx.is_null() {
        unsafe { libubox_sys::ubus_free(ctx) };
    }
}

#[cfg(not(have_libubox))]
fn main() {
    eprintln!(
        "libubox-sys built without artifacts (run tools/ubusd-builder/build.sh first); skipping"
    );
}
