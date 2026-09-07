/* SPDX-License-Identifier: GPL-2.0 */
#ifndef XDP_LINK_H
#define XDP_LINK_H

#include <linux/if_link.h>
#include <linux/types.h>

/*
 * Minimal netlink-based XDP attach/detach/query helpers. These replace the
 * libbpf < 1.0 helpers bpf_xdp_attach(), bpf_xdp_detach() and
 * bpf_xdp_query_id(), which were removed from libbpf 1.0 (and now live in
 * libxdp). Kept dependency-free (libc + kernel UAPI headers only) so they
 * double as a small C reference for the netlink attach path in a Rust
 * implementation.
 *
 * Return values: 0 on success, negative errno on failure (kernel netlink
 * errors are returned as their negative errno values).
 */

int xdp_link_query_id(int ifindex, __u32 xdp_flags, __u32 *prog_id);
int xdp_link_attach(int ifindex, int prog_fd, __u32 xdp_flags);
int xdp_link_detach(int ifindex, __u32 xdp_flags);

#endif /* XDP_LINK_H */
