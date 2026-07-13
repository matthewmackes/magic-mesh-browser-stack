//! The shared OS sandbox for MCNF's out-of-process web engine helpers
//! (security-1; factored out of `mde-web-preview`'s BOOKMARKS-5 `sandbox.rs`).
//!
//! A web-engine helper runs as its own OS process, confined — *before a single
//! line of web content is touched* — by layered Linux kernel isolation:
//!
//! 1. **no-new-privs** (`prctl(PR_SET_NO_NEW_PRIVS)`) — a setuid binary on the
//!    (bound-in) rootfs can never regain privilege, and it is the prerequisite
//!    for installing an unprivileged seccomp filter. It is ALSO what makes the
//!    confinement survive `exec`: a multi-process engine (Chromium/CEF forks and
//!    re-execs its own renderer/GPU/utility subprocesses) inherits the seccomp
//!    filter + dropped caps across every `execve`.
//! 2. **cgroup v2 caps** — a per-helper child cgroup with `memory.max` + `cpu.max`
//!    so one runaway page — or one runaway process TREE — cannot exhaust the
//!    node's RAM or pin every core. The whole forked subprocess tree inherits the
//!    cgroup, so the ceiling binds the engine's children too.
//! 3. **user + mount + IPC + UTS + cgroup + PID namespaces** (`unshare` + a
//!    `fork`) — no network namespace ON PURPOSE (egress stays; only the
//!    ad-blocker filters). The PID namespace makes host processes invisible and
//!    lets a *fresh* `procfs` mount (an unprivileged userns can only mount proc
//!    for a pid namespace it owns); because `CLONE_NEWPID` only takes effect for
//!    a forked child, [`apply`] forks and the confined engine runs as the new
//!    namespace's PID 1 while the original process supervises it. A multi-process
//!    engine's children are further children of that PID 1, all inside the one
//!    namespace set.
//! 4. **uid/gid maps** — mapped to a throwaway identity; the real user/keys are
//!    invisible.
//! 5. **read-only rootfs + tmpfs** — a fresh tmpfs root that bind-mounts ONLY
//!    the read-only system runtime (`/usr`, the loader, the system CA bundle,
//!    DNS resolv/hosts, the GPU device), any caller-named **extra** read-only
//!    paths (e.g. a vendored engine runtime bundle), a private `/tmp`, and a
//!    private `/dev/shm` (a multi-process Chromium/CEF engine's mandatory
//!    POSIX-shared-memory surface — it aborts without it). There
//!    is **no `$HOME`, no `/root`, no `/var`, no ssh/mesh keys, no mesh data** —
//!    they are simply absent from the new root, so the engine cannot read them
//!    even if it is compromised. This is *also* what makes "no persistent
//!    history / cookies cleared on close" structural rather than a bypassable
//!    flag: the process has nowhere writable to persist anything.
//! 6. **minimal capabilities** — every capability is cleared from the bounding,
//!    ambient, inheritable, permitted and effective sets after the (privileged)
//!    mount setup is done.
//! 7. **seccomp-bpf** — a filter that returns `EPERM` for the kernel's
//!    privilege-escalation / sandbox-escape syscalls (ptrace, the mount family,
//!    `unshare`/`setns`, module loading, `bpf`, `perf_event_open`, key
//!    management, `kexec`, clock/time setting, …). Layered with the namespace +
//!    caps + rootfs isolation, an `EPERM` denylist is the pragmatic choice: a
//!    strict allowlist risks killing a benign but unlisted syscall of a large
//!    engine, whereas denying the escape set cannot.
//!
//! ## Reuse across engines (`apply` vs `apply_with_binds`)
//!
//! [`apply`] is the single-process default (Servo `mde-web-preview`): the engine
//! bakes in its own resources, so the system-runtime binds are all it needs.
//! [`apply_with_binds`] additionally exposes caller-named read-only paths — the
//! Chromium/CEF helper (`mde-web-cef`) binds its vendored `/opt/mde/cef` runtime
//! so the browser process can `dlopen` `libcef.so` AND re-`exec` its subprocess
//! bridge from inside the confined rootfs. Those extra binds are read-only and
//! must NEVER name a key/home/mesh-data path — the caller owns that invariant
//! (the helper's own bind planner is unit-tested for it).
//!
//! Most of the mechanism is exercised through `nix`'s safe wrappers; the pure,
//! deterministic *planners* (the seccomp syscall set, the uid/gid map lines, the
//! cgroup limit strings, the rootfs bind plan) are unit-tested here, and the
//! privileged `apply` sequence performs the real syscalls at helper startup.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{Context, Result};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::prctl;
use nix::unistd::{fork, pivot_root, sethostname, ForkResult, Gid, Pid, Uid};

/// The confinement limits + identity for one sandboxed helper process.
///
/// The numeric limits are the policy defaults; a launcher may lower them per
/// mesh policy but never raise them past what the node allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// Hard memory ceiling written to the cgroup's `memory.max`, in bytes.
    pub memory_max_bytes: u64,
    /// CPU bandwidth quota (microseconds of CPU time per [`Self::cpu_period_us`]).
    pub cpu_quota_us: u64,
    /// CPU bandwidth accounting period, in microseconds.
    pub cpu_period_us: u64,
    /// The generic hostname the UTS namespace reports (non-identifying). Also
    /// keys the private rootfs mountpoint name, so distinct engines do not share
    /// a rootfs path.
    pub hostname: &'static str,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self::tab()
    }
}

impl SandboxPolicy {
    /// The default single-process (Servo) per-tab policy: 1 GiB RAM, ~80% of one
    /// core, a generic host.
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

    /// The Chromium/CEF policy: a higher ceiling (2 GiB / ~2 cores) because the
    /// cgroup binds the WHOLE multi-process browser tree (browser + GPU + one or
    /// more renderer/utility subprocesses), and a generic host `web-cef` so the
    /// rootfs path does not collide with the Servo helper's.
    #[must_use]
    pub const fn web_cef() -> Self {
        Self {
            memory_max_bytes: 2 * 1024 * 1024 * 1024,
            // 200_000 / 100_000 == 2 cores across the whole process tree.
            cpu_quota_us: 200_000,
            cpu_period_us: 100_000,
            hostname: "web-cef",
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

    /// The private tmpfs rootfs mountpoint for this policy (keyed on `hostname`
    /// so two engines never share a rootfs path).
    #[must_use]
    pub fn rootfs_path(&self) -> PathBuf {
        PathBuf::from(format!("/tmp/.mde-{}-root", self.hostname))
    }
}

/// A single `/proc/self/{uid,gid}_map` line: `"<inside> <outside> <count>"`.
///
/// The helper maps a single id — the caller's real id — to inside-id `0`, which
/// grants `CAP_SYS_ADMIN` *inside the new user namespace only* (needed to mount
/// the rootfs and `pivot_root`). Those capabilities are stripped again at the
/// end of [`apply_with_binds`], so the running engine holds none, inside the
/// namespace or out.
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
///
/// NOTE for the Chromium/CEF caller: keeping `unshare`/`setns`/`mount`/
/// `pivot_root` on this denylist is *precisely* why Chromium's OWN internal
/// (unprivileged-userns / setuid) sandbox cannot be re-enabled nested inside
/// this one — its zygote needs those syscalls to build the renderer's restricted
/// view, and they `EPERM` here. The OS sandbox instead confines the whole tree.
/// See `mde-web-cef`'s renderer + `docs/THREAT_MODEL.md` §10.
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
/// carrying user data or keys. (A caller may add read-only ENGINE-RUNTIME paths
/// via [`apply_with_binds`]; those must likewise never name a key/home path.)
#[must_use]
pub fn readonly_binds() -> Vec<PathBuf> {
    [
        "/usr", // the whole system runtime + fonts + resources
        "/bin", // usrmerge symlinks (harmless if already under /usr)
        "/sbin",
        "/lib",
        "/lib64",
        "/etc/pki",          // Fedora system CA trust store (system-CA TLS)
        "/etc/ssl",          // the OpenSSL cert dir / symlinks
        "/etc/alternatives", // the symlink farm the NSS CA path traverses:
        // /usr/lib64/libnssckbi.so -> /etc/alternatives/libnssckbi.so.x86_64 ->
        // /usr/lib64/pkcs11/p11-kit-trust.so. Without this hop the link dangles in
        // the rootfs and Chromium's NSS root-cert module fails to dlopen (system +
        // enterprise CA trust silently lost). Every target resolves under /usr.
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

/// Rootfs-relative directories that receive a private writable tmpfs.
///
/// `tmp` is general engine scratch. `dev/shm` is MANDATORY for a multi-process
/// Chromium/CEF engine: it passes frame + IPC buffers between the browser, GPU,
/// and renderer subprocesses through POSIX shared memory rooted there, and
/// aborts at startup without it. Both are ephemeral, `mode=1777`, size-capped,
/// and charged to the engine cgroup — no data persists past the sandbox.
const WRITABLE_TMPFS_SUBDIRS: [&str; 2] = ["tmp", "dev/shm"];

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

/// Apply the full sandbox to the current process, with NO extra binds.
///
/// Equivalent to [`apply_with_binds`] with an empty extra-bind list — the
/// single-process (Servo) default, whose engine bakes in its own resources.
///
/// # Errors
/// See [`apply_with_binds`].
pub fn apply(policy: SandboxPolicy) -> Result<()> {
    apply_with_binds(policy, &[])
}

/// Apply the full sandbox to the current process, in the security-critical
/// order, exposing `extra_readonly_binds` (read-only) in the rootfs on top of
/// the system-runtime binds.
///
/// `extra_readonly_binds` is for ENGINE RUNTIME paths a multi-process engine
/// needs after `pivot_root` (e.g. CEF's `/opt/mde/cef` bundle, so the browser
/// can `dlopen` `libcef.so` and re-`exec` its subprocess bridge). Each is bound
/// **read-only**; a non-existent path is skipped. The caller MUST NOT name a
/// key/home/mesh-data path — this function cannot tell an engine bundle from a
/// secret, so that invariant lives with the caller's bind planner.
///
/// On success this forks: the original process becomes a thin **supervisor**
/// that only reaps + forwards signals and NEVER returns from `apply_with_binds`,
/// while the confined child — PID 1 of a fresh PID namespace — returns `Ok`. So
/// the code after a successful call runs exactly once, in the confined child,
/// and it is safe to initialise the web engine there. The process MUST be
/// single-threaded at the call (the fork is async-signal-safe only then).
///
/// # Errors
/// Returns an error (in the child, or in the pre-fork process) if any kernel
/// isolation step fails (e.g. unprivileged user namespaces are disabled, or the
/// cgroup subtree is not delegated). The caller MUST treat a failure as fatal and
/// refuse to load web content unconfined.
pub fn apply_with_binds(policy: SandboxPolicy, extra_readonly_binds: &[PathBuf]) -> Result<()> {
    // 1. no-new-privs (also the precondition for unprivileged seccomp).
    prctl::set_no_new_privs().context("PR_SET_NO_NEW_PRIVS")?;

    // 2. cgroup memory/CPU caps — while we still see the host cgroup tree. The
    // forked child (and every engine subprocess it later forks) inherits this
    // cgroup membership, so the caps bind the whole tree.
    if let Err(e) = enter_cgroup(policy) {
        // Honest degrade: the other layers still apply. Surface it loudly.
        eprintln!("mde-web-sandbox: cgroup limits not applied ({e:#}); continuing with namespace+seccomp confinement");
    }

    // Capture our REAL uid/gid in the PARENT user namespace BEFORE step 3's
    // unshare. Once we enter the new, still-unmapped user namespace,
    // getuid()/getgid() report the overflow id (65534) — writing that into
    // `uid_map` is rejected (EPERM) because the single-line unprivileged
    // exception must name the writer's OWN parent-ns id. Read them here.
    let uid = Uid::current().as_raw();
    let gid = Gid::current().as_raw();

    // 3. new user + mount + IPC + UTS + cgroup + PID namespaces (NOT network).
    unshare(
        CloneFlags::CLONE_NEWUSER
            | CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_NEWIPC
            | CloneFlags::CLONE_NEWUTS
            | CloneFlags::CLONE_NEWCGROUP
            | CloneFlags::CLONE_NEWPID,
    )
    .context("unshare (are unprivileged user namespaces enabled?)")?;

    // 4. identity maps (setgroups must be denied first, unprivileged) — using the
    // uid/gid captured ABOVE. Written by THIS task (which owns the new userns),
    // before the fork; the child inherits the maps.
    std::fs::write("/proc/self/setgroups", "deny").context("setgroups deny")?;
    std::fs::write("/proc/self/uid_map", id_map_line(uid)).context("uid_map")?;
    std::fs::write("/proc/self/gid_map", id_map_line(gid)).context("gid_map")?;

    // 5. fork so the child is PID 1 of the new PID namespace. The parent turns
    // into a supervisor that forwards termination signals to the child and exits
    // with its status; it never returns. Only the confined child proceeds. The
    // process is single-threaded here (the engine is not yet built), so this
    // fork is async-signal-safe.
    // SAFETY: single-threaded at this point (no engine threads exist yet); the
    // parent path only performs async-signal-safe work (see `supervise_child`).
    match unsafe { fork() }.context("fork into pid namespace")? {
        ForkResult::Parent { child } => supervise_child(child), // never returns
        ForkResult::Child => {}
    }

    // --- confined child (PID 1 of the new pid namespace) from here on ---

    // If the supervising parent dies, take the engine down with it (no orphaned,
    // still-running helper). Best-effort: a failure here is not fatal to
    // confinement.
    // SAFETY: prctl(PR_SET_PDEATHSIG, …) is a simple no-pointer syscall.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
    }

    // 6. read-only rootfs + tmpfs (incl. a fresh procfs), then pivot into it.
    build_rootfs(policy, extra_readonly_binds).context("rootfs")?;

    // 7. generic hostname (UTS namespace).
    sethostname(policy.hostname).context("sethostname")?;

    // 8. drop every capability (mount setup is done).
    drop_all_capabilities().context("drop capabilities")?;

    // 9. seccomp-bpf escape-syscall denylist (installed LAST).
    install_seccomp().context("seccomp")?;

    Ok(())
}

/// The confined child's host-namespace PID, published for the signal-forwarding
/// handler (which must be async-signal-safe — an atomic load + `kill` only).
static SUPERVISED_CHILD: AtomicI32 = AtomicI32::new(0);

/// Async-signal-safe handler: forward the received signal to the confined child.
extern "C" fn forward_signal(sig: libc::c_int) {
    let child = SUPERVISED_CHILD.load(Ordering::SeqCst);
    if child > 0 {
        // SAFETY: async-signal-safe — an atomic load plus a single `kill`.
        unsafe {
            libc::kill(child, sig);
        }
    }
}

/// Supervise the confined child: forward graceful-termination signals to it, reap
/// it, and exit with its status. Never returns — the pre-fork process's sole job
/// from here is to be a faithful proxy for the sandboxed engine's lifetime.
fn supervise_child(child: Pid) -> ! {
    SUPERVISED_CHILD.store(child.as_raw(), Ordering::SeqCst);
    // Forward the signals a helper supervisor is expected to relay so the engine
    // can shut down cleanly when the shell stops the tab.
    let handler: extern "C" fn(libc::c_int) = forward_signal;
    // SAFETY: installing an async-signal-safe handler for termination signals.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as usize;
        libc::sigemptyset(&raw mut sa.sa_mask);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
            libc::sigaction(sig, &raw const sa, std::ptr::null_mut());
        }
    }
    // Reap, restarting across EINTR (our own forwarded signals interrupt it).
    let mut status: libc::c_int = 0;
    let code = loop {
        // SAFETY: `waitpid` on our own child; `status` is a valid out-pointer.
        let r = unsafe { libc::waitpid(child.as_raw(), &raw mut status, 0) };
        if r == -1 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break 1; // lost the child; fail closed
        }
        if libc::WIFEXITED(status) {
            break libc::WEXITSTATUS(status);
        }
        if libc::WIFSIGNALED(status) {
            break 128 + libc::WTERMSIG(status);
        }
        // Stopped/continued — keep waiting for a terminal status.
    };
    std::process::exit(code);
}

/// Create + enter a per-process child cgroup with the policy's memory/CPU caps.
fn enter_cgroup(policy: SandboxPolicy) -> Result<()> {
    // cgroup v2 unified hierarchy only.
    let current = current_cgroup_path().context("read /proc/self/cgroup")?;
    let base = Path::new("/sys/fs/cgroup").join(current.trim_start_matches('/'));
    let leaf = base.join(format!("mde-{}-{}", policy.hostname, std::process::id()));

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

/// Build the fresh tmpfs root, bind the read-only runtime (+ any caller-named
/// extra read-only paths) into it, and `pivot_root` so it becomes `/`.
fn build_rootfs(policy: SandboxPolicy, extra_readonly_binds: &[PathBuf]) -> Result<()> {
    let newroot = policy.rootfs_path();
    let newroot = newroot.as_path();

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

    // Read-only binds of the system runtime + CA + DNS + GPU node, THEN any
    // caller-named engine-runtime paths (e.g. the CEF bundle). Extra paths are
    // read-only exactly like the system binds; a missing path is skipped.
    for src in readonly_binds() {
        bind_readonly(&src, newroot)?;
    }
    for src in extra_readonly_binds {
        if src.exists() {
            bind_readonly(src, newroot)?;
        }
    }
    // Individual /dev nodes.
    let dev_dir = newroot.join("dev");
    std::fs::create_dir_all(&dev_dir)?;
    for src in dev_binds() {
        bind_readonly(&src, newroot)?;
    }
    // The private writable tmpfs surfaces (see WRITABLE_TMPFS_SUBDIRS): `/tmp`
    // general scratch, and `/dev/shm` which a multi-process Chromium/CEF engine
    // REQUIRES for the POSIX-shared-memory frame + IPC buffers it passes between
    // the browser, GPU, and renderer subprocesses — with no /dev/shm the browser
    // aborts at startup ("Unable to access(W_OK|X_OK) /dev/shm"). Each is a fresh
    // tmpfs in the new rootfs (never a host bind), namespace-isolated
    // (CLONE_NEWIPC) and ephemeral, and its pages are charged to the engine's
    // cgroup RAM cap — a bounded per-sandbox scratch area, nothing persists.
    for sub in WRITABLE_TMPFS_SUBDIRS {
        let dir = newroot.join(sub);
        std::fs::create_dir_all(&dir)?;
        mount(
            Some("tmpfs"),
            &dir,
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            Some("size=512m,mode=1777"),
        )
        .with_context(|| format!("mount /{sub}"))?;
    }
    // A fresh proc for the new pid namespace (host processes invisible). This
    // succeeds because apply unshared CLONE_NEWPID and forked, so we are PID 1 of
    // a pid namespace our user namespace owns. If a fresh procfs is somehow
    // refused, fall back to a read-only recursive bind of the existing /proc.
    let proc_dir = newroot.join("proc");
    std::fs::create_dir_all(&proc_dir)?;
    if let Err(e) = mount(
        Some("proc"),
        &proc_dir,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    ) {
        eprintln!("mde-web-sandbox: fresh procfs unavailable ({e}); binding /proc read-only");
        bind_readonly(Path::new("/proc"), newroot).context("mount /proc (ro bind fallback)")?;
    }

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

/// Bind-mount `src` from the (pre-pivot) host into `newroot`, then make it — and
/// every mount nested beneath it — read-only.
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
    // Recursive bind first (a plain MS_RDONLY on the bind itself is silently
    // ignored by the kernel — the follow-up remount is what makes it stick).
    mount(
        Some(src),
        &dst,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .with_context(|| format!("bind {}", src.display()))?;
    // Make it read-only. See [`remount_readonly_tree`] for why a naive
    // `MS_REMOUNT | MS_RDONLY` EPERMs in an unprivileged user namespace.
    remount_readonly_tree(&dst)
}

/// Remount `root` and every mount nested beneath it read-only.
///
/// In an unprivileged user namespace, a bind mount inherits its source mount's
/// LOCKED flags (`nosuid` / `nodev` / `noexec` and the `atime` policy). The
/// kernel rejects — with `EPERM` — any `MS_REMOUNT` that would *clear* a locked
/// flag, so a bare `MS_REMOUNT | MS_RDONLY` fails whenever the source carries
/// e.g. `nosuid`. The fix is to read each mount's CURRENT flags from
/// `/proc/self/mountinfo` and re-apply them, OR-ing in `MS_RDONLY`, so no locked
/// flag is dropped. A recursive bind can pull in submounts — none may be left
/// writable — so we walk the whole tree under `root`, deepest first.
fn remount_readonly_tree(root: &Path) -> Result<()> {
    let mut targets = mounts_at_or_under(root)?;
    // Deepest paths first (a child before its parent); dedup identical points.
    targets.sort_by_key(|p| std::cmp::Reverse(p.as_os_str().len()));
    targets.dedup();
    for mp in targets {
        let existing = current_mount_flags(&mp).unwrap_or_else(MsFlags::empty);
        mount(
            None::<&str>,
            &mp,
            None::<&str>,
            // MS_BIND ⇒ per-mount (not superblock) remount; MS_RDONLY adds
            // read-only; `existing` re-asserts the (locked) source flags so none
            // is cleared; MS_NOSUID is extra hardening — harmless for /dev nodes,
            // which stay usable because MS_RDONLY does not block device I/O.
            MsFlags::MS_BIND
                | MsFlags::MS_REMOUNT
                | MsFlags::MS_RDONLY
                | MsFlags::MS_NOSUID
                | existing,
            None::<&str>,
        )
        .with_context(|| format!("remount-ro {}", mp.display()))?;
    }
    Ok(())
}

/// Every mount point equal to `root` or nested beneath it, per `/proc/self/mountinfo`.
fn mounts_at_or_under(root: &Path) -> Result<Vec<PathBuf>> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").context("read mountinfo")?;
    Ok(mountinfo
        .lines()
        .filter_map(mountinfo_mount_point)
        .filter(|mp| mp.starts_with(root))
        .collect())
}

/// The current mount flags of the topmost mount at `target` (the LAST matching
/// `/proc/self/mountinfo` line — later lines shadow earlier ones at a point).
fn current_mount_flags(target: &Path) -> Option<MsFlags> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    mountinfo.lines().rev().find_map(|line| {
        (mountinfo_mount_point(line)? == *target).then(|| mountinfo_mount_flags(line))
    })
}

/// Parse the mount-point field (field 5) of a `/proc/self/mountinfo` line,
/// un-escaping the octal `\NNN` sequences the kernel uses for special chars.
fn mountinfo_mount_point(line: &str) -> Option<PathBuf> {
    line.split_whitespace()
        .nth(4)
        .map(|f| PathBuf::from(unescape_octal(f)))
}

/// Parse the per-mount option field (field 6) of a `/proc/self/mountinfo` line
/// into the lockable [`MsFlags`]. Only the flags the kernel can lock across a
/// user namespace matter for a read-only remount; fs-specific options are ignored.
fn mountinfo_mount_flags(line: &str) -> MsFlags {
    let mut flags = MsFlags::empty();
    let Some(opts) = line.split_whitespace().nth(5) else {
        return flags;
    };
    for opt in opts.split(',') {
        match opt {
            "ro" => flags |= MsFlags::MS_RDONLY,
            "nosuid" => flags |= MsFlags::MS_NOSUID,
            "nodev" => flags |= MsFlags::MS_NODEV,
            "noexec" => flags |= MsFlags::MS_NOEXEC,
            "noatime" => flags |= MsFlags::MS_NOATIME,
            "nodiratime" => flags |= MsFlags::MS_NODIRATIME,
            "relatime" => flags |= MsFlags::MS_RELATIME,
            "strictatime" => flags |= MsFlags::MS_STRICTATIME,
            _ => {}
        }
    }
    flags
}

/// Un-escape the octal `\NNN` sequences `/proc/self/mountinfo` uses for space,
/// tab, newline and backslash in its path fields.
fn unescape_octal(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(b as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
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
    fn web_cef_policy_raises_the_ceiling_and_uses_a_distinct_host_and_rootfs() {
        let p = SandboxPolicy::web_cef();
        // A higher ceiling for the whole multi-process Chromium tree.
        assert_eq!(
            p.cgroup_memory_max(),
            (2u64 * 1024 * 1024 * 1024).to_string()
        );
        assert_eq!(p.cgroup_cpu_max(), "200000 100000");
        assert_eq!(p.hostname, "web-cef");
        // Distinct, non-colliding rootfs path from the Servo helper's.
        assert_eq!(p.rootfs_path(), PathBuf::from("/tmp/.mde-web-cef-root"));
        assert_ne!(p.rootfs_path(), SandboxPolicy::tab().rootfs_path());
        assert_eq!(
            SandboxPolicy::tab().rootfs_path(),
            PathBuf::from("/tmp/.mde-web-preview-root")
        );
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
        // the load-bearing escape primitives are present. The mount/namespace
        // family is load-bearing TWICE: it is why a compromised engine cannot
        // escape, AND why Chromium's own nested sandbox cannot re-enable (it
        // needs these to build its renderer view). See the crate + threat model.
        for nr in [
            libc::SYS_ptrace,
            libc::SYS_mount,
            libc::SYS_pivot_root,
            libc::SYS_unshare,
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
    fn mountinfo_mount_point_is_field_five() {
        let line =
            "36 35 0:32 / /usr rw,nosuid,nodev,relatime shared:1 - tmpfs tmpfs rw,size=1024k";
        assert_eq!(mountinfo_mount_point(line), Some(PathBuf::from("/usr")));
    }

    #[test]
    fn mountinfo_mount_point_unescapes_octal() {
        // The kernel renders a space in a path as the octal escape `\040`.
        let line = "1 2 0:3 / /mnt/with\\040space rw,relatime - tmpfs t rw";
        assert_eq!(
            mountinfo_mount_point(line),
            Some(PathBuf::from("/mnt/with space"))
        );
        assert_eq!(unescape_octal("/usr/lib64"), "/usr/lib64");
        assert_eq!(unescape_octal("/a\\011b"), "/a\tb");
    }

    #[test]
    fn mountinfo_flags_preserve_the_lockable_set() {
        let f = mountinfo_mount_flags(
            "36 35 0:32 / /run rw,nosuid,nodev,noexec,relatime shared:1 - tmpfs tmpfs rw",
        );
        assert!(f.contains(MsFlags::MS_NOSUID));
        assert!(f.contains(MsFlags::MS_NODEV));
        assert!(f.contains(MsFlags::MS_NOEXEC));
        assert!(f.contains(MsFlags::MS_RELATIME));
        assert!(!f.contains(MsFlags::MS_RDONLY));

        let ro = mountinfo_mount_flags("1 2 0:3 / /etc/resolv.conf ro,noatime - tmpfs t ro");
        assert!(ro.contains(MsFlags::MS_RDONLY));
        assert!(ro.contains(MsFlags::MS_NOATIME));
    }

    #[test]
    fn writable_tmpfs_provisions_dev_shm_for_the_chromium_engine() {
        // Regression for the "Unable to access(W_OK|X_OK) /dev/shm" startup
        // crash: a multi-process Chromium/CEF engine aborts without a writable
        // /dev/shm, so the sandbox MUST provision it (alongside /tmp). build_rootfs
        // can't be unit-tested (it needs a real userns), so pin the policy here.
        assert!(
            WRITABLE_TMPFS_SUBDIRS.contains(&"dev/shm"),
            "sandbox must provide a writable /dev/shm or Chromium aborts"
        );
        assert!(WRITABLE_TMPFS_SUBDIRS.contains(&"tmp"));
        // All writable surfaces are ephemeral tmpfs under the rootfs — never a
        // host path, never an absolute escape.
        for s in WRITABLE_TMPFS_SUBDIRS {
            assert!(
                !s.starts_with('/'),
                "writable tmpfs must be rootfs-relative: {s}"
            );
        }
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
