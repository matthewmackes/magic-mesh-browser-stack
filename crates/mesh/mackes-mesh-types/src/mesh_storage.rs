//! Shared mesh-storage constants + the write-safety guard.
//!
//! arch-7 (2026-07-11) — relocated out of the `mackesd` bin crate so worker
//! crates factored out of the daemon (e.g. `mde-browser-workers`) can reuse
//! the one audited [`shared_root_writable`] guard rather than re-deriving it.
//! `mackesd` re-exports these at its crate root, so its ~17 in-crate call
//! sites (`crate::CANONICAL_QNM_MOUNT` / `crate::shared_root_writable`) are
//! unchanged.

/// The canonical deployed shared-storage directory (SUBSTRATE-V2: a plain
/// Syncthing-replicated dir, no FUSE — see [`shared_root_writable`]).
pub const CANONICAL_QNM_MOUNT: &str = "/mnt/mesh-storage";

/// AUDIT-MESH-15 guard: is it SAFE to write under `root`?
///
/// Under SUBSTRATE-V2 `/mnt/mesh-storage` ([`CANONICAL_QNM_MOUNT`]) is a plain
/// Syncthing-replicated directory — writable **iff the dir actually exists**. A
/// missing/unprovisioned share (early boot, before the first Syncthing sync)
/// must NOT be written, or the shared-state writers (the heartbeat, the `chat`
/// worker's replicated conversation logs, `ssh-pubkey gossip`, the clipboard
/// history) would silently land on a bare local dir. Any other root (a
/// dev `~/QNM-Shared`, a tempdir) is always writable, so dev/test is unaffected.
#[must_use]
pub fn shared_root_writable(root: &std::path::Path) -> bool {
    shared_root_writable_core(root, root.is_dir())
}

/// Pure core of [`shared_root_writable`] — testable without touching the fs.
/// The canonical shared dir is writable iff it actually exists (`root_is_dir`);
/// every other root is always writable.
#[must_use]
pub fn shared_root_writable_core(root: &std::path::Path, root_is_dir: bool) -> bool {
    if root != std::path::Path::new(CANONICAL_QNM_MOUNT) {
        return true;
    }
    root_is_dir
}

#[cfg(test)]
mod shared_root_tests {
    use std::path::Path;

    use super::{shared_root_writable, shared_root_writable_core};

    #[test]
    fn non_canonical_roots_are_always_writable() {
        // Dev/test paths (tempdirs, ~/QNM-Shared) are never the poison case.
        assert!(shared_root_writable(Path::new("/home/mm/QNM-Shared")));
        assert!(shared_root_writable(Path::new("/tmp/anything")));
        let tmp = tempfile::tempdir().unwrap();
        assert!(shared_root_writable(tmp.path()));
        // ...regardless of whether the dir exists.
        assert!(shared_root_writable_core(Path::new("/tmp/x"), false));
    }

    #[test]
    fn canonical_writable_iff_dir_exists() {
        // SUBSTRATE-V2: the plain Syncthing dir is writable iff it exists —
        // fixes the post-cutover silent-drop of every shared-state write
        // (heartbeat / chat logs / ssh-gossip / clipboard).
        assert!(shared_root_writable_core(
            Path::new("/mnt/mesh-storage"),
            true
        ));
        // ...but never a bare/unprovisioned share (dir absent).
        assert!(!shared_root_writable_core(
            Path::new("/mnt/mesh-storage"),
            false
        ));
    }
}
