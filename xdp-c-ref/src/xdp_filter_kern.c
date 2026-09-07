/* SPDX-License-Identifier: GPL-2.0
 *
 * xdp_filter_kern.c - demo XDP program with a runtime-switchable packet
 * policy, controlled from user space through the "policy" BPF map. The
 * program runs before the network stack and decides, per packet, whether
 * traffic goes to an AF_XDP socket (XSKMAP redirect), is dropped, or is
 * passed to the normal stack ("ignore"). Each mode maps to a classic
 * real-world XDP use case:
 *
 *   policy[0] = 0  REDIRECT_ALL   steer every packet to the AF_XDP socket
 *                                 bound to ctx->rx_queue_index (queue
 *                                 steering, the default program behaviour)
 *   policy[0] = 1  DROP_ALL       L2 firewalling; nothing reaches the stack
 *   policy[0] = 2  UDP_PORT_8080  steer only UDP dst port 8080 to the socket
 *                                 on queue 0, everything else XDP_PASS
 *                                 (bystander; e.g. service offload)
 *   policy[0] = 3  TCP_DST_PORT   steer only TCP packets whose dst port
 *                                 equals policy[1] (default 443) to queue 0,
 *                                 everything else XDP_PASS
 *   policy[0] = 4  PASS_ALL       observe only; the normal stack handles
 *                                 everything
 *
 * Build: make (clang -target bpf). Attach with bpftool or xdp-loader, e.g.:
 *   bpftool prog load build/xdp_filter_kern.o /sys/fs/bpf/xdp_filter type xdp
 *   bpftool net attach xdpdrv|xdpgeneric pinned /sys/fs/bpf/xdp_filter dev <iface>
 *   bpftool map update pinned /sys/fs/bpf/xdp_filter/maps/policy \
 *           key 0 0 0 0 value 2 0 0 0      # switch to UDP/8080 steering
 *
 * On virtual interfaces (veth) only xdpgeneric (SKB) mode is available;
 * native/xdpdrv mode needs a driver with XDP support.
 */

#include <linux/bpf.h>
#include <linux/if_ether.h>
#include <linux/in.h>
#include <linux/ip.h>
#include <linux/tcp.h>
#include <linux/udp.h>

#include <bpf/bpf_endian.h>
#include <bpf/bpf_helpers.h>

struct {
	__uint(type, BPF_MAP_TYPE_XSKMAP);
	__uint(max_entries, 64);
	__uint(key_size, sizeof(int));
	__uint(value_size, sizeof(int));
} xsks_map SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 2);
	__uint(key_size, sizeof(__u32));
	__uint(value_size, sizeof(__u32));
} policy SEC(".maps");

enum {
	POLICY_REDIRECT_ALL = 0,
	POLICY_DROP_ALL,
	POLICY_UDP_PORT_8080,
	POLICY_TCP_DST_PORT,
	POLICY_PASS_ALL,
};

static __always_inline __u32 lookup_policy(__u32 key, __u32 default_val)
{
	__u32 *v = bpf_map_lookup_elem(&policy, &key);

	return v ? *v : default_val;
}

/* Bounds-checked IPv4 L4 header walk; returns 0 and sets *dport on match. */
static __always_inline int parse_ipv4(void *data, void *data_end, __u16 *dport)
{
	struct ethhdr *eth = data;
	struct iphdr *iph;
	void *nh;

	if ((void *)(eth + 1) > data_end)
		return -1;
	if (eth->h_proto != bpf_htons(ETH_P_IP))
		return -1;

	iph = (void *)(eth + 1);
	if ((void *)(iph + 1) > data_end)
		return -1;
	if (iph->ihl * 4 < (int)sizeof(*iph))
		return -1;
	nh = (void *)iph + iph->ihl * 4;
	if (nh > data_end)
		return -1;

	if (iph->protocol == IPPROTO_UDP) {
		struct udphdr *udp = nh;

		if ((void *)(udp + 1) > data_end)
			return -1;
		*dport = bpf_ntohs(udp->dest);
		return 0;
	}
	if (iph->protocol == IPPROTO_TCP) {
		struct tcphdr *tcp = nh;

		if ((void *)(tcp + 1) > data_end)
			return -1;
		*dport = bpf_ntohs(tcp->dest);
		return 0;
	}
	return -1;
}

SEC("xdp_sock")
int xdp_filter_prog(struct xdp_md *ctx)
{
	void *data_end = (void *)(long)ctx->data_end;
	void *data = (void *)(long)ctx->data;
	__u32 mode = lookup_policy(0, POLICY_REDIRECT_ALL);
	__u16 dport = 0;

	switch (mode) {
	case POLICY_DROP_ALL:
		return XDP_DROP;
	case POLICY_PASS_ALL:
		return XDP_PASS;
	case POLICY_UDP_PORT_8080:
		if (parse_ipv4(data, data_end, &dport) || dport != 8080)
			return XDP_PASS;
		return bpf_redirect_map(&xsks_map, 0, XDP_PASS);
	case POLICY_TCP_DST_PORT:
		if (parse_ipv4(data, data_end, &dport) ||
		    dport != (__u16)lookup_policy(1, 443))
			return XDP_PASS;
		return bpf_redirect_map(&xsks_map, 0, XDP_PASS);
	case POLICY_REDIRECT_ALL:
	default:
		return bpf_redirect_map(&xsks_map, ctx->rx_queue_index,
					XDP_PASS);
	}
}

char _license[] SEC("license") = "GPL";
