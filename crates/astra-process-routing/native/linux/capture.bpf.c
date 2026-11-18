// SPDX-License-Identifier: GPL-2.0-only
// Resolve the executable at connect/send time, including the first connection after exec.
#include <linux/bpf.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_core_read.h>

#pragma clang attribute push(__attribute__((preserve_access_index)), apply_to = record)
struct qstr { const unsigned char *name; };
struct super_block { unsigned int s_dev; };
struct inode { unsigned long i_ino; struct super_block *i_sb; };
struct dentry { struct dentry *d_parent; struct qstr d_name; struct inode *d_inode; };
struct vfsmount { struct dentry *mnt_root; };
struct mount {
    struct mount *mnt_parent;
    struct dentry *mnt_mountpoint;
    struct vfsmount mnt;
};
struct path { struct vfsmount *mnt; struct dentry *dentry; };
struct file { struct path f_path; };
struct mm_struct { struct file *exe_file; };
struct fs_struct { struct path root; };
struct task_struct { struct mm_struct *mm; struct fs_struct *fs; };
#pragma clang attribute pop

// The Agent populates a trie of path components in reverse order. Resolving
// from the executable's dentry also distinguishes hard links and mount points.
struct component { __u32 suffix; char name[256]; };
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 16384);
    __type(key, struct component);
    __type(value, __u32);
} components SEC(".maps");

struct executable { __u64 root_inode; __u32 root_device; __u32 path_id; };
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 4096);
    __type(key, struct executable);
    __type(value, __u32); // 0 = block; otherwise a reserved TPROXY routing mark.
} bindings SEC(".maps");

// The privileged supervisor puts forwarding cores and their worker children
// in a dedicated cgroup before exec. Exclude that cgroup, never an executable
// basename that another application could also use.
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 1024);
    __type(key, __u64);
    __type(value, __u32);
} forwarders SEC(".maps");

// A connectionless UDP socket can concurrently send to several peers. Its
// routing mark belongs to the socket, so snapshot a successful decision once
// and reuse it for all of its datagrams. A denied attempt created no session.
struct {
    __uint(type, BPF_MAP_TYPE_SK_STORAGE);
    __uint(map_flags, BPF_F_NO_PREALLOC);
    __type(key, int);
    __type(value, __u64);
} udp_sessions SEC(".maps");

#define UNBOUND 0ULL
#define BLOCKED 1ULL
#define REDIRECT (1ULL << 32)

struct walk {
    struct mount *mount;
    struct dentry *entry;
    struct mount *root_mount;
    struct dentry *root_entry;
    __u32 suffix;
    __u64 decision;
};

static long next_component(__u32 depth, void *data)
{
    struct walk *walk = data;
    struct mount *mount = walk->mount;
    struct dentry *entry = walk->entry;
    if (entry == walk->root_entry && mount == walk->root_mount) {
        // Private mount namespaces (e.g. systemd PrivateTmp) frequently
        // share the host's root. A different filesystem root is a separate
        // path space even when all the component names happen to match.
        struct inode *root = BPF_CORE_READ(entry, d_inode);
        struct executable key = {
            BPF_CORE_READ(root, i_ino), BPF_CORE_READ(root, i_sb, s_dev), walk->suffix
        };
        __u32 *mark = bpf_map_lookup_elem(&bindings, &key);
        if (mark)
            walk->decision = *mark ? REDIRECT | *mark : BLOCKED;
        return 1;
    }
    if (entry == BPF_CORE_READ(mount, mnt.mnt_root)) {
        struct mount *parent = BPF_CORE_READ(mount, mnt_parent);
        if (parent == mount)
            return 1;
        walk->entry = BPF_CORE_READ(mount, mnt_mountpoint);
        walk->mount = parent;
    } else {
        struct component key = { .suffix = walk->suffix };
        const unsigned char *name = BPF_CORE_READ(entry, d_name.name);
        if (bpf_probe_read_kernel_str(key.name, sizeof(key.name), name) <= 0)
            return 1;
        __u32 *next = bpf_map_lookup_elem(&components, &key);
        if (!next)
            return 1;
        walk->suffix = *next;
        walk->entry = BPF_CORE_READ(entry, d_parent);
    }
    return 0;
}

static __noinline __u64 policy(void)
{
    struct task_struct *task = bpf_get_current_task_btf();
    struct file *exe = BPF_CORE_READ(task, mm, exe_file);
    struct fs_struct *fs = BPF_CORE_READ(task, fs);
    struct path path = {};
    struct path root = {};
    if (!exe || !fs || BPF_CORE_READ_INTO(&path, exe, f_path) || BPF_CORE_READ_INTO(&root, fs, root))
        return UNBOUND;
    struct walk walk = {
        .mount = (void *)path.mnt - bpf_core_field_offset(struct mount, mnt),
        .entry = path.dentry,
        .root_mount = (void *)root.mnt - bpf_core_field_offset(struct mount, mnt),
        .root_entry = root.dentry,
    };
    // One callback per path component or mount boundary. An unmatched
    // basename stops at the first lookup, regardless of executable depth.
    bpf_loop(8192, next_component, &walk, 0);
    return walk.decision;
}

static __always_inline int classify(struct bpf_sock_addr *ctx)
{
    if (ctx->protocol != 6 && ctx->protocol != 17)
        return 1;
    __u64 cgroup = bpf_get_current_cgroup_id();
    if (bpf_map_lookup_elem(&forwarders, &cgroup))
        return 1;
    __u64 decision;
    if (ctx->protocol == 17 && ctx->sk) {
        __u64 *snapshot = bpf_sk_storage_get(&udp_sessions, ctx->sk, 0, 0);
        if (!snapshot) {
            decision = policy();
            if (decision == BLOCKED)
                return 0;
            snapshot = bpf_sk_storage_get(&udp_sessions, ctx->sk, &decision,
                                          BPF_SK_STORAGE_GET_F_CREATE);
            if (!snapshot)
                return decision == UNBOUND;
        }
        decision = *snapshot;
    } else {
        decision = policy();
    }
    if (decision == UNBOUND)
        return 1;
    if (decision == BLOCKED)
        return 0;
    __u32 mark = (__u32)decision;
    return bpf_setsockopt(ctx, 1 /* SOL_SOCKET */, 36 /* SO_MARK */,
                          &mark, sizeof(mark)) == 0;
}

SEC("cgroup/connect4") int connect4(struct bpf_sock_addr *ctx) { return classify(ctx); }
SEC("cgroup/connect6") int connect6(struct bpf_sock_addr *ctx) { return classify(ctx); }
SEC("cgroup/sendmsg4") int sendmsg4(struct bpf_sock_addr *ctx) { return classify(ctx); }
SEC("cgroup/sendmsg6") int sendmsg6(struct bpf_sock_addr *ctx) { return classify(ctx); }
char LICENSE[] SEC("license") = "GPL";
