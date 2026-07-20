#!/bin/bash
# setup-selinux-web-preview.sh — BOOKMARKS-9: load the confined, ENFORCING
# SELinux domain for the Servo browser helper (/usr/bin/mde-web-preview).
#
# Called from the magic-mesh RPM %post (all roles) and runnable by hand:
#   /usr/libexec/mackesd/setup-selinux-web-preview
#
# The helper's PRIMARY confinement is its own in-process OS sandbox (user
# namespace + seccomp + dropped caps + no-new-privs + cgroups + a read-only
# pivot_root'd rootfs with no home/keys/mesh data). This module adds a SECOND,
# orthogonal MAC layer wherever the node runs SELinux Enforcing. The two source
# files (mde-web-preview.te/.fc) ship at $POLICY_DIR; this compiles them into a
# loadable `.pp` and installs it, then relabels the binary.
#
# Honest degrade (never a fake success):
#   · SELinux disabled (the operator platform standard 2026-06-20) -> self-skip;
#     the OS sandbox remains the operative confinement.
#   · No policy build toolchain (selinux-policy-devel / checkpolicy) -> log the
#     one package to install and skip; the module is NOT silently marked loaded.
# Idempotent — safe to re-run (semodule -i upgrades in place).
set -uo pipefail

POLICY_DIR=/usr/share/magic-mesh/selinux/mde-web-preview
SOURCE_STEM=mde-web-preview
MODULE=mde_web_preview
BIN=/usr/bin/mde-web-preview

# 1) SELinux present + not disabled? (selinuxenabled: 0 = Enforcing OR Permissive.)
if ! command -v selinuxenabled >/dev/null 2>&1 || ! selinuxenabled 2>/dev/null; then
  echo "mde-web-preview SELinux: SELinux disabled/absent — skipping (OS sandbox is the confinement)"
  exit 0
fi

if [ ! -f "$POLICY_DIR/$SOURCE_STEM.te" ]; then
  echo "mde-web-preview SELinux: policy source missing at $POLICY_DIR — skipping"
  exit 0
fi

WORK="$(mktemp -d /tmp/mde-web-preview-selinux.XXXXXX)" || exit 0
trap 'rm -rf "$WORK"' EXIT
cp -f "$POLICY_DIR/$SOURCE_STEM.te" "$WORK/$MODULE.te" 2>/dev/null &&
  cp -f "$POLICY_DIR/$SOURCE_STEM.fc" "$WORK/$MODULE.fc" 2>/dev/null || {
  echo "mde-web-preview SELinux: cannot stage policy source — skipping"; exit 0; }

built=""
# 2a) Preferred: the refpolicy devel Makefile (interfaces like application_domain,
#     dev_rw_dri, corenet_* resolve here). Ships in selinux-policy-devel.
DEVEL_MK=/usr/share/selinux/devel/Makefile
if [ -f "$DEVEL_MK" ]; then
  if ( cd "$WORK" && make -f "$DEVEL_MK" "$MODULE.pp" ) >/dev/null 2>&1 && [ -f "$WORK/$MODULE.pp" ]; then
    built="$WORK/$MODULE.pp"
  fi
fi

# 2b) Fallback: raw checkmodule + semodule_package (checkpolicy). Only compiles the
#     base statements; the refpolicy path above is preferred for the full policy.
if [ -z "$built" ] && command -v checkmodule >/dev/null 2>&1 && command -v semodule_package >/dev/null 2>&1; then
  if checkmodule -M -m -o "$WORK/$MODULE.mod" "$WORK/$MODULE.te" >/dev/null 2>&1 \
     && semodule_package -o "$WORK/$MODULE.pp" -m "$WORK/$MODULE.mod" -f "$WORK/$MODULE.fc" >/dev/null 2>&1; then
    built="$WORK/$MODULE.pp"
  fi
fi

if [ -z "$built" ]; then
  echo "mde-web-preview SELinux: no policy build toolchain — install 'selinux-policy-devel' to enforce the browser domain (the OS sandbox is active meanwhile); skipping"
  exit 0
fi

# 3) Load it + relabel the (already-installed) binary so the transition fires.
# Browser RPM %post can start the Servo and CEF SELinux setup units at the same
# time. The SELinux module store is global, so serialize semodule writes here.
if command -v flock >/dev/null 2>&1 && [ -d /run/lock ]; then
  if ( flock -w 120 9 && semodule -i "$built" >/dev/null 2>&1 ) 9>/run/lock/mde-browser-selinux.lock; then
    restorecon -F "$BIN" >/dev/null 2>&1 || :
    echo "mde-web-preview SELinux: confined domain mde_web_preview_t loaded + $BIN relabelled"
  else
    echo "mde-web-preview SELinux: semodule -i failed or timed out on the module-store lock — leaving policy unloaded (OS sandbox still active)"
  fi
elif semodule -i "$built" >/dev/null 2>&1; then
  restorecon -F "$BIN" >/dev/null 2>&1 || :
  echo "mde-web-preview SELinux: confined domain mde_web_preview_t loaded + $BIN relabelled"
else
  echo "mde-web-preview SELinux: semodule -i failed — leaving policy unloaded (OS sandbox still active)"
fi
exit 0
