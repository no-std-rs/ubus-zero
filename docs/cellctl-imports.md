# Cellctl imports

The protocol library, broker and compatibility infrastructure originally came
from Cellctl. This change additionally extracts generic application helpers from
`pawelchcki/cellctl-internal` at revision
`d4c9a5580b904fb0e89482dfa8d356957323af10` (Apache-2.0):

| Cellctl source | Public ubus-zero module | Adaptation |
| --- | --- | --- |
| `src/platform/unix_sock.rs` | `ubus::unix` (`unix` feature) | Replace Cellctl/nolibc runtime hooks with rustix socket/poll/clock calls; add operation deadlines; retain partial-frame buffering, bounded writes and close handling |
| `src/ubus_methods.rs` JSON/blobmsg helpers | `ubus::json` (`json` feature) | Extract conversion and nested validation; require object arguments; reject NULs and bound nesting |

Both features retain `no_std + alloc`; neither is enabled by default. The CLI
uses these public helpers and is a binary-only package. Modem-specific handlers,
SMS/USSD state, observability, and netifd policy remain with Cellctl.

The existing protocol implementation was compared against this Cellctl revision.
Before the new APIs, its implementation matched; remaining source differences
were genericized test object names and documentation. Cellctl's checkout and
dependency pins were not modified by this extraction.
