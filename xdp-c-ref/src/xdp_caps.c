/* SPDX-License-Identifier: GPL-2.0
 *
 * xdp_caps.c - probe which AF_XDP features are actually usable on an
 * interface, on this kernel, right now. This is the ground-truth companion
 * to the runtime feature checks a Rust AF_XDP implementation needs:
 *
 *   - zero-copy vs copy mode: bind() probe, the same technique the kernel's
 *     xdpsock sample and DPDK's AF_XDP PMD use
 *   - XDP_USE_NEED_WAKEUP acceptance
 *   - unaligned UMEM chunks (XDP_UMEM_UNALIGNED_CHUNK_FLAG)
 *   - what the kernel actually granted (getsockopt XDP_OPTIONS)
 *   - queue count, driver, and the XDP program currently attached
 *
 * Needs root, or CAP_BPF + CAP_NET_ADMIN + CAP_NET_RAW.
 */

#include <dirent.h>
#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <net/if.h>
#include <sys/mman.h>
#include <sys/utsname.h>

#include <linux/if_link.h>
#include <linux/if_xdp.h>

#include <xdp/xsk.h>

#include "xdp_link.h"

#ifndef SOL_XDP
#define SOL_XDP 283
#endif
#ifndef AF_XDP
#define AF_XDP 44
#endif
#ifndef XDP_OPTIONS
#define XDP_OPTIONS 8
#endif
#ifndef XDP_OPTIONS_ZEROCOPY
#define XDP_OPTIONS_ZEROCOPY (1 << 0)
#endif
#ifndef XDP_UMEM_UNALIGNED_CHUNK_FLAG
#define XDP_UMEM_UNALIGNED_CHUNK_FLAG (1 << 0)
#endif

#define FRAME_SIZE 4096
#define FRAME_COUNT 128
#define RING_SIZE 64

/* Non-power-of-two frame size: only legal with the unaligned-chunk flag. */
#define PROBE_FRAME_SIZE 3000
#define PROBE_FRAME_COUNT 64

static void usage(const char *prog)
{
	fprintf(stderr,
		"usage: %s -i <ifname> [-q <queue>]\n"
		"Probe AF_XDP feature support on <ifname>.\n",
		prog);
	exit(EXIT_FAILURE);
}

static int count_rx_queues(const char *ifname)
{
	char path[PATH_MAX];
	struct dirent *de;
	DIR *dir;
	int count = 0;

	snprintf(path, sizeof(path), "/sys/class/net/%s/queues", ifname);
	dir = opendir(path);
	if (!dir)
		return -1;

	while ((de = readdir(dir)) != NULL) {
		if (strncmp(de->d_name, "rx-", 3) == 0)
			count++;
	}
	closedir(dir);
	return count;
}

static const char *driver_name(const char *ifname)
{
	static char link[PATH_MAX];
	char path[PATH_MAX];
	ssize_t len;

	snprintf(path, sizeof(path), "/sys/class/net/%s/device/driver", ifname);
	len = readlink(path, link, sizeof(link) - 1);
	if (len < 0)
		return NULL;
	link[len] = '\0';
	return strrchr(link, '/') ? strrchr(link, '/') + 1 : link;
}

static int create_umem(struct xsk_umem **umem, void **bufs, size_t size,
		       __u32 frame_size, __u32 umem_flags)
{
	struct xsk_umem_config cfg = {
		.fill_size = RING_SIZE,
		.comp_size = RING_SIZE,
		.frame_size = frame_size,
		.frame_headroom = 0,
		.flags = umem_flags,
	};
	struct xsk_ring_prod fq;
	struct xsk_ring_cons cq;
	int ret;

	*bufs = mmap(NULL, size, PROT_READ | PROT_WRITE,
		     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (*bufs == MAP_FAILED)
		return -errno;

	ret = xsk_umem__create(umem, *bufs, size, &fq, &cq, &cfg);
	if (ret) {
		munmap(*bufs, size);
		*bufs = NULL;
	}
	return ret;
}

static void destroy_umem(struct xsk_umem *umem, void *bufs, size_t size)
{
	if (umem)
		xsk_umem__delete(umem);
	if (bufs)
		munmap(bufs, size);
}

/* INHIBIT_PROG_LOAD: probing must not attach any XDP program. */
static int create_socket(struct xsk_socket **xsk, const char *ifname, int queue,
			 struct xsk_umem *umem, __u16 bind_flags)
{
	struct xsk_socket_config cfg = {
		.rx_size = RING_SIZE,
		.tx_size = RING_SIZE,
		.libbpf_flags = XSK_LIBBPF_FLAGS__INHIBIT_PROG_LOAD,
		.xdp_flags = 0,
		.bind_flags = bind_flags,
	};
	struct xsk_ring_cons rx;
	struct xsk_ring_prod tx;

	return xsk_socket__create(xsk, ifname, queue, umem, &rx, &tx, &cfg);
}

int main(int argc, char **argv)
{
	const char *ifname = NULL;
	const char *driver;
	struct utsname uts;
	struct xsk_umem *umem = NULL;
	struct xsk_socket *xsk = NULL;
	void *bufs = NULL;
	size_t umem_size = (size_t)FRAME_COUNT * FRAME_SIZE;
	__u32 prog_id = 0;
	int queue = 0, opt, ifindex, nqueues, ret;

	while ((opt = getopt(argc, argv, "i:q:")) != -1) {
		switch (opt) {
		case 'i':
			ifname = optarg;
			break;
		case 'q':
			queue = atoi(optarg);
			break;
		default:
			usage(argv[0]);
		}
	}
	if (!ifname)
		usage(argv[0]);

	ifindex = if_nametoindex(ifname);
	if (!ifindex) {
		fprintf(stderr, "ERROR: no such interface \"%s\"\n", ifname);
		return EXIT_FAILURE;
	}

	uname(&uts);
	nqueues = count_rx_queues(ifname);
	driver = driver_name(ifname);

	printf("kernel:         %s\n", uts.release);
	printf("interface:      %s (ifindex %d)\n", ifname, ifindex);
	printf("driver:         %s\n", driver ? driver : "virtual (no driver)");
	if (nqueues >= 0)
		printf("rx queues:      %d\n", nqueues);
	else
		printf("rx queues:      unknown (no /sys/class/net entry)\n");

	ret = xdp_link_query_id(ifindex,
				XDP_FLAGS_SKB_MODE | XDP_FLAGS_DRV_MODE |
				XDP_FLAGS_HW_MODE,
				&prog_id);
	if (ret)
		printf("xdp program:    query failed (%s)\n", strerror(-ret));
	else if (prog_id)
		printf("xdp program:    attached (id %u)\n", prog_id);
	else
		printf("xdp program:    none attached\n");

	/* Unaligned-chunk probe (non-power-of-two frame size). */
	ret = create_umem(&umem, &bufs, (size_t)PROBE_FRAME_COUNT * PROBE_FRAME_SIZE,
			  PROBE_FRAME_SIZE, XDP_UMEM_UNALIGNED_CHUNK_FLAG);
	if (!ret) {
		printf("unaligned umem: SUPPORTED (XDP_UMEM_UNALIGNED_CHUNK_FLAG accepted)\n");
		destroy_umem(umem, bufs, (size_t)PROBE_FRAME_COUNT * PROBE_FRAME_SIZE);
		umem = NULL;
		bufs = NULL;
	} else {
		printf("unaligned umem: not supported (%s)\n", strerror(-ret));
	}

	/* Zero-copy / copy probe. */
	ret = create_umem(&umem, &bufs, umem_size, FRAME_SIZE, 0);
	if (ret) {
		fprintf(stderr, "ERROR: cannot create UMEM: %s\n", strerror(-ret));
		return EXIT_FAILURE;
	}

	ret = create_socket(&xsk, ifname, queue, umem,
			    XDP_ZEROCOPY | XDP_USE_NEED_WAKEUP);
	if (!ret) {
		struct xdp_options opts;
		socklen_t optlen = sizeof(opts);

		printf("zero-copy:      SUPPORTED (bind with XDP_ZEROCOPY | XDP_USE_NEED_WAKEUP ok)\n");
		if (!getsockopt(xsk_socket__fd(xsk), SOL_XDP, XDP_OPTIONS,
				&opts, &optlen)) {
			if (opts.flags & XDP_OPTIONS_ZEROCOPY)
				printf("                kernel confirms zero-copy active (XDP_OPTIONS_ZEROCOPY)\n");
		}
		xsk_socket__delete(xsk);
		xsk = NULL;
	} else if (ret == -EOPNOTSUPP) {
		/* ENOTSUPP == EOPNOTSUPP on Linux; glibc may only expose the
		 * POSIX name. */
		printf("zero-copy:      NOT SUPPORTED on this device/queue (%s)\n",
		       strerror(-ret));
		ret = create_socket(&xsk, ifname, queue, umem,
				    XDP_COPY | XDP_USE_NEED_WAKEUP);
		if (!ret) {
			printf("copy mode:      WORKS (bind with XDP_COPY ok)\n");
			xsk_socket__delete(xsk);
			xsk = NULL;
		} else {
			printf("copy mode:      failed (%s)\n", strerror(-ret));
		}
	} else {
		printf("zero-copy:      bind failed unexpectedly (%s)\n",
		       strerror(-ret));
	}

	destroy_umem(umem, bufs, umem_size);
	return EXIT_SUCCESS;
}
