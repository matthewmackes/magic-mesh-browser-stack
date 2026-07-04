//! The OS sandbox for a `mde-web-preview` tab process (BOOKMARKS-5).
//!
//! A tab runs as its own OS process, confined — *before a single line of web
//! content is touched* — by layered Linux kernel isolation:
//!
//! 1. **no-new-privs** (`prctl(PR_SET_NO_NEW_PRIVS)`) — a setuid binary on the
//!    (bound-in) rootfs can never regain privilege, and it is the prerequisite
//!    for installing an unprivileged seccomp filter.
//! 2. **cgroup v2 caps** — a per-tab child cgroup with `memory.max` + `cpu.max`
//!    so one runaway page cannot exhaust the node's RAM or pin every core.
//! 3. **user + mount + IPC + UTS + cgroup namespaces** (`unshare`) — no network
//!    namespace ON PURPOSE (Q38: egress stays; only the ad-blocker filters).
//! 4. **uid/gid maps** — mapped to a throwaway identity; the real user/keys are
//!    invisible.
//! 5. **read-only rootfs + tmpfs** — a fresh tmpfs root that bind-mounts ONLY
//!    the read-only system runtime (`/usr`, the loader, the system CA bundle,
//!    DNS resolv/hosts, the GPU device) and a private `/tmp`. There is **no
//!    `$HOME`, no `/root`, no `/var`, no ssh/mesh keys, no mesh data** — they
//!    are simply absent from the new root, so the engine cannot read them even
//!    if it is compromised. This is *also* what makes "no persistent history /
//!    cookies cleared on close" structural rather than a bypassable flag: the
//!    process has nowhere writable to persist anything.
//! 6. **minimal capabilities** — every capability is cleared from the bounding,
//!    ambient, inheritable, permitted and effective sets after the (privileged)
//!    mount setup is done.
//! 7. **seccomp-bpf** — a filter that returns `EPERM` for the kernel's
//!    privilege-escalation / sandbox-escape syscalls (ptrace, the mount family,
//!    `unshare`/`setns`, module loading, `bpf`, `perf_event_open`, key
//!    management, `kexec`, clock/time setting, …). Layered with the namespace +
//!    caps + rootfs isolation, an `EPERM` denylist is the pragmatic choice: a
//!    strict allowlist risks killing SpiderMonkey/WebRender on a benign but
//!    unlisted syscall, whereas denying the escape set cannot.
//!
//! Most of the mechanism is exercised through `nix`'s safe wrappers; the pure,
//! deterministic *planners* (the seccomp syscall set, the uid/gid map lines, the
//! cgroup limit strings, the rootfs bind plan) are unit-tested here, and the
//! privileged `apply` sequence performs the real syscalls at tab startup.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::prctl;
use nix::unistd::{pivot_root, sethostname, Gid, Uid};

/// The confinement limits + identity for one sandboxed tab process.
///
/// The numeric limits are the policy defaults; a launcher may lower them per
/// mesh policy (BOOKMARKS-8) but never raise them past what the node allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// Hard memory ceiling written to the tab cgroup's `memory.max`, in bytes.
    pub memory_max_bytes: u64,
    /// CPU bandwidth quota (microseconds of CPU time per [`Self::cpu_period_us`]).
    pub cpu_quota_us: u64,
    /// CPU bandwidth accounting period, in microseconds.
    pub cpu_period_us: u64,
    /// The generic hostname the UTS namespace reports (non-identifying).
    pub hostname: &'static str,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self::tab()
    }
}

impl SandboxPolicy {
    /// The default per-tab policy: 1 GiB RAM, ~80% of one core, a generic host.
    #[must_use]
    pub const fn tab() -> Self {
        Self {
            memory_max_bytes: 1024 * 1024 * 1024,
            // 80_000 / 100_000 == 0.8 cores.
            cpu_quota_us: 80_000,
            cpu_period_us: 100_000,
            hostname: "web-preview",
        }
    }

    /// The `memory.max` cgroup-v2 value for this policy (a decimal byte count).
    #[must_use]
    pub fn cgroup_memory_max(&self) -> String {
        self.memory_max_bytes.to_string()
    }

    /// The `cpu.max` cgroup-v2 value for this policy (`"<quota> <period>"`).
    #[must_use]
    pub fn cgroup_cpu_max(&self) -> String {
        format!("{} {}", self.cpu_quota_us, self.cpu_period_us)
    }
}

/// A single `/proc/self/{uid,gid}_map` line: `"<inside> <outside> <count>"`.
///
/// The tab maps a single id — the caller's real id — to inside-id `0`, which
/// grants `CAP_SYS_ADMIN` *inside the new user namespace only* (needed to mount
/// the rootfs and `pivot_root`). Those capabilities are stripped again at the
/// end of [`apply`], so the running engine holds none, inside the namespace or
/// out.
#[must_use]
pub fn id_map_line(outside: u32) -> String {
    format!("0 {outside} 1")
}

/// The set of syscalls the seccomp filter denies with `EPERM`.
///
/// These are the kernel's privilege-escalation and sandbox-escape primitives.
/// The list is expressed with `libc::SYS_*` constants (never raw numbers) so it
/// stays correct across libc updates and is arch-checked at compile time. The
/// mount / `pivot_root` / `unshare` family is on the list because it is denied
/// *after* the sandbox has finished using it — the filter is installed last.
#[must_use]
pub fn denied_syscalls() -> Vec<i64> {
    // Rendered as i64 for seccompiler (`libc::SYS_*` are `c_long`, i.e. `i64` on
    // this crate's x86_64 target). Kept sorted-by-theme for review.
    let mut v: Vec<i64> = vec![
        // Debug / cross-process memory.
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        // Namespace / mount escapes (post-setup).
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_unshare,
        libc::SYS_setns,
        // Kernel module + kexec.
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        // Kernel introspection / exploitation primitives.
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        // Key management.
        libc::SYS_add_key,
        libc::SYS_keyctl,
        libc::SYS_request_key,
        // Time / clock tampering.
        libc::SYS_settimeofday,
        libc::SYS_clock_settime,
        libc::SYS_adjtimex,
        libc::SYS_clock_adjtime,
        // Misc administrative.
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_acct,
        libc::SYS_open_by_handle_at,
        libc::SYS_personality,
    ];
    v.sort_unstable();
    v.dedup();
    v
}

/// The read-only host paths bind-mounted into the tmpfs rootfs.
///
/// Deliberately: the read-only system runtime + the system CA bundle + DNS
/// resolution files + the GPU render node. Deliberately ABSENT: `$HOME`,
/// `/root`, `/var`, `/etc/ssh`, the mesh's Nebula/Syncthing state — anything
/// carrying user data or keys.
#[must_use]
pub fn readonly_binds() -> Vec<PathBuf> {
    [
        "/usr", // the whole system runtime + fonts + resources
        "/bin", // usrmerge symlinks (harmless if already under /usr)
        "/sbin",
        "/lib",
        "/lib64",
        "/etc/pki", // Fedora system CA trust store (system-CA TLS)
        "/etc/ssl", // the OpenSSL cert dir / symlinks
        "/etc/crypto-policies",
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/fonts",
        "/dev/dri", // the GPU render node (acceptance: GPU-required)
    ]
    .into_iter()
    .map(PathBuf::from)
    .filter(|p| p.exists())
    .collect()
}

/// Minimal `/dev` nodes bind-mounted individually (no `mknod` in the userns).
#[must_use]
pub fn dev_binds() -> Vec<PathBuf> {
    [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
    ]
    .into_iter()
    .map(PathBuf::from)
    .filter(|p| p.exists())
    .collect()
}

/// Apply the full sandbox to the CURRENT process, in the security-critical
/// order. After this returns `Ok`, the process is confined and it is safe to
/// initialise the web engine.
///
/// # Errors
/// Returns an error if any kernel isolation step fails (e.g. unprivileged user
/// namespaces are disabled, or the cgroup subtree is not delegated). The caller
/// MUST treat a failure as fatal and refuse to load web content unconfined.
pub fn apply(policy: SandboxPolicy) -> Result<()> {
    // 1. no-new-privs (also the precondition for unprivileged seccomp).
    prctl::set_no_new_privs().context("PR_SET_NO_NEW_PRIVS")?;

    // 2. cgroup memory/CPU caps — while we still see the host cgroup tree.
    if let Err(e) = enter_cgroup(policy) {
        // Honest degrade: the other layers still apply. Surface it loudly.
        eprintln!("mde-web-preview: cgroup limits not applied ({e:#}); continuing with namespace+seccomp confinement");
    }

    // Capture our REAL uid/gid in the PARENT user namespace BEFORE step 3's
    // unshare. THE FIX (sandbox-apply crash): once we enter the new, still-
    // unmapped user namespace, getuid()/getgid() report the overflow id
    // (`/proc/sys/kernel/overflowuid`, 65534) — capturing them there wrote a
    // `uid_map` of `0 65534 1`, which the kernel rejects with EPERM because the
    // single-line unprivileged exception must name the writer's OWN parent-ns id
    // (e.g. 1000), not the overflow id. Read them here, while still mapped.
    let uid = Uid::current().as_raw();
    let gid = Gid::current().as_raw();

    // 3. new user + mount + IPC + UTS + cgroup namespaces (NOT network).
    unshare(
        CloneFlags::CLONE_NEWUSER
            | CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_NEWIPC
            | CloneFlags::CLONE_NEWUTS
            | CloneFlags::CLONE_NEWCGROUP,
    )
    .context("unshare (are unprivileged user namespaces enabled?)")?;

    // 4. identity maps (setgroups must be denied first, unprivileged) — using the
    // uid/gid captured ABOVE, never a post-unshare getuid() (the overflow id).
    std::fs::write("/proc/self/setgroups", "deny").context("setgroups deny")?;
    std::fs::write("/proc/self/uid_map", id_map_line(uid)).context("uid_map")?;
    std::fs::write("/proc/self/gid_map", id_map_line(gid)).context("gid_map")?;

    // 5. read-only rootfs + tmpfs, then pivot into it.
    build_rootfs().context("rootfs")?;

    // 6. generic hostname (UTS namespace).
    sethostname(policy.hostname).context("sethostname")?;

    // 7. drop every capability (mount setup is done).
    drop_all_capabilities().context("drop capabilities")?;

    // 8. seccomp-bpf escape-syscall denylist (installed LAST).
    install_seccomp().context("seccomp")?;

    Ok(())
}

/// Create + enter a per-process child cgroup with the policy's memory/CPU caps.
fn enter_cgroup(policy: SandboxPolicy) -> Result<()> {
    // cgroup v2 unified hierarchy only.
    let current = current_cgroup_path().context("read /proc/self/cgroup")?;
    let base = Path::new("/sys/fs/cgroup").join(current.trim_start_matches('/'));
    let leaf = base.join(format!("mde-web-preview-{}", std::process::id()));

    // Ask the parent to delegate memory+cpu to child cgroups (best-effort).
    let _ = std::fs::write(base.join("cgroup.subtree_control"), "+memory +cpu");

    std::fs::create_dir_all(&leaf).with_context(|| format!("mkdir {}", leaf.display()))?;
    std::fs::write(leaf.join("memory.max"), policy.cgroup_memory_max()).context("memory.max")?;
    std::fs::write(leaf.join("cpu.max"), policy.cgroup_cpu_max()).context("cpu.max")?;
    std::fs::write(leaf.join("cgroup.procs"), std::process::id().to_string())
        .context("cgroup.procs")?;
    Ok(())
}

/// Parse the unified-hierarchy path out of `/proc/self/cgroup` (`0::/...`).
fn current_cgroup_path() -> Result<String> {
    let raw = std::fs::read_to_string("/proc/self/cgroup")?;
    let path = raw
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::trim)
        .map(ToOwned::to_owned)
        .context("no cgroup-v2 (0::) line — unified hierarchy required")?;
    Ok(path)
}

/// Build the fresh tmpfs root, bind the read-only runtime into it, and
/// `pivot_root` so it becomes `/`.
fn build_rootfs() -> Result<()> {
    let newroot = Path::new("/tmp/.mde-web-preview-root");

    // Make all existing mounts private so our changes don't propagate out.
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .context("make-rprivate /")?;

    // A fresh tmpfs as the new root.
    std::fs::create_dir_all(newroot)?;
    mount(
        Some("tmpfs"),
        newroot,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("mode=0755"),
    )
    .context("mount tmpfs newroot")?;

    // Read-only binds of the system runtime + CA + DNS + GPU node.
    for src in readonly_binds() {
        bind_readonly(&src, newroot)?;
    }
    // Individual /dev nodes.
    let dev_dir = newroot.join("dev");
    std::fs::create_dir_all(&dev_dir)?;
    for src in dev_binds() {
        bind_readonly(&src, newroot)?;
    }
    // A private writable /tmp (tmpfs) — the only writable surface.
    let tmp_dir = newroot.join("tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    mount(
        Some("tmpfs"),
        &tmp_dir,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some("size=256m,mode=1777"),
    )
    .context("mount /tmp")?;
    // A fresh proc for the new pid view.
    let proc_dir = newroot.join("proc");
    std::fs::create_dir_all(&proc_dir)?;
    mount(
        Some("proc"),
        &proc_dir,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .context("mount /proc")?;

    // pivot_root: swap the new tmpfs root in, detach the old one entirely.
    let oldroot = newroot.join("oldroot");
    std::fs::create_dir_all(&oldroot)?;
    pivot_root(newroot, &oldroot).context("pivot_root")?;
    nix::unistd::chdir("/").context("chdir /")?;
    umount2("/oldroot", MntFlags::MNT_DETACH).context("detach oldroot")?;
    // Best-effort tidy of the now-empty mountpoint.
    let _ = std::fs::remove_dir("/oldroot");
    Ok(())
}

/// Bind-mount `src` from the (pre-pivot) host into `newroot` read-only.
fn bind_readonly(src: &Path, newroot: &Path) -> Result<()> {
    let rel = src.strip_prefix("/").unwrap_or(src);
    let dst = newroot.join(rel);
    // Mirror the source's kind (dir vs file) at the destination.
    if src.is_dir() {
        std::fs::create_dir_all(&dst)?;
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::File::create(&dst);
    }
    // Bind, then remount the same target read-only (a plain MS_RDONLY bind is
    // silently ignored by the kernel; the remount is what makes it stick).
    mount(
        Some(src),
        &dst,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .with_context(|| format!("bind {}", src.display()))?;
    mount(
        None::<&str>,
        &dst,
        None::<&str>,
        MsFlags::MS_BIND
            | MsFlags::MS_REC
            | MsFlags::MS_REMOUNT
            | MsFlags::MS_RDONLY
            | MsFlags::MS_NOSUID,
        None::<&str>,
    )
    .with_context(|| format!("remount-ro {}", dst.display()))?;
    Ok(())
}

/// Clear every capability from all sets so the confined engine holds none.
fn drop_all_capabilities() -> Result<()> {
    use caps::CapSet;
    // Clearing the bounding set is irreversible; do ambient + the thread sets
    // too so nothing survives an exec of a (bound-in) setuid helper.
    for set in [
        CapSet::Ambient,
        CapSet::Bounding,
        CapSet::Inheritable,
        CapSet::Permitted,
        CapSet::Effective,
    ] {
        caps::clear(None, set).with_context(|| format!("clear {set:?}"))?;
    }
    Ok(())
}

/// Assemble + install the seccomp-bpf escape-syscall denylist on this thread.
fn install_seccomp() -> Result<()> {
    use seccompiler::{apply_filter, BpfProgram, SeccompAction, SeccompFilter};
    use std::collections::BTreeMap;

    // Empty rule vector == match the syscall unconditionally.
    let rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = denied_syscalls()
        .into_iter()
        .map(|nr| (nr, Vec::new()))
        .collect();

    let filter = SeccompFilter::new(
        rules,
        // Default (not on the list): allow — the engine's large legitimate
        // syscall surface keeps working.
        SeccompAction::Allow,
        // On the list: EPERM (graceful) rather than KillProcess.
        SeccompAction::Errno(libc::EPERM as u32),
        std::env::consts::ARCH
            .try_into()
            .map_err(|e| anyhow::anyhow!("seccomp arch: {e}"))?,
    )
    .context("build seccomp filter")?;

    let program: BpfProgram = filter.try_into().context("compile seccomp bpf")?;
    apply_filter(&program).context("install seccomp filter")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_limits_render_as_cgroup_v2_expects() {
        let p = SandboxPolicy::tab();
        assert_eq!(p.cgroup_memory_max(), (1024u64 * 1024 * 1024).to_string());
        assert_eq!(p.cgroup_cpu_max(), "80000 100000");
    }

    #[test]
    fn id_map_maps_outside_id_to_inside_zero() {
        assert_eq!(id_map_line(1000), "0 1000 1");
        assert_eq!(id_map_line(0), "0 0 1");
    }

    #[test]
    fn denied_syscalls_are_sorted_deduped_and_cover_the_escape_primitives() {
        let denied = denied_syscalls();
        assert!(!denied.is_empty());
        // sorted + deduped
        let mut sorted = denied.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(denied, sorted);
        // the load-bearing escape primitives are present
        for nr in [
            libc::SYS_ptrace,
            libc::SYS_mount,
            libc::SYS_setns,
            libc::SYS_bpf,
            libc::SYS_finit_module,
            libc::SYS_keyctl,
        ] {
            assert!(denied.contains(&nr), "missing syscall {nr}");
        }
    }

    #[test]
    fn seccomp_filter_compiles_to_a_nonempty_bpf_program() {
        use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};
        use std::collections::BTreeMap;
        let rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = denied_syscalls()
            .into_iter()
            .map(|nr| (nr, Vec::new()))
            .collect();
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EPERM as u32),
            std::env::consts::ARCH
                .try_into()
                .expect("host arch is a seccomp target"),
        )
        .expect("filter builds");
        let program: BpfProgram = filter.try_into().expect("bpf compiles");
        assert!(!program.is_empty());
    }

    #[test]
    fn rootfs_plan_excludes_home_keys_and_mesh_data() {
        // Whatever the host actually has, the bind plan must NEVER name a
        // home/keys/mesh-data path.
        for p in readonly_binds() {
            let s = p.to_string_lossy();
            assert!(!s.starts_with("/home"), "home leaked: {s}");
            assert!(!s.starts_with("/root"), "root home leaked: {s}");
            assert!(!s.starts_with("/var"), "var (mesh data) leaked: {s}");
            assert!(!s.contains("ssh"), "ssh keys leaked: {s}");
            assert!(!s.contains("nebula"), "nebula keys leaked: {s}");
            assert!(!s.contains("syncthing"), "syncthing data leaked: {s}");
        }
    }
}
