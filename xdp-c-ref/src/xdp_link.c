/* SPDX-License-Identifier: GPL-2.0
 *
 * xdp_link.c - minimal netlink(7) helpers to attach, detach and query XDP
 * programs on a network device (RTM_SETLINK / RTM_GETLINK + IFLA_XDP).
 *
 * Semantics follow the removed libbpf < 1.0 helpers:
 *   - attach(): fails with -EEXIST if a program is already attached in the
 *     requested mode and XDP_FLAGS_UPDATE_IF_NOEXIST is set. Implemented
 *     via IFLA_XDP_EXPECTED_FD = 0 (kernel >= 5.4).
 *   - detach(): removes the program in the requested mode (IFLA_XDP_FD = -1).
 *   - query_id(): returns the prog ID attached in the requested mode,
 *     or 0 if none. Note: XDP_FLAGS_SKB_MODE / _DRV_MODE / _HW_MODE select
 *     which attachment is reported.
 */

#include <errno.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>

#include <linux/if_link.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <sys/socket.h>

#include "xdp_link.h"

/* Fallbacks for older kernel UAPI headers. */
#ifndef IFLA_EXT_MASK
#define IFLA_EXT_MASK 29
#endif
#ifndef IFLA_XDP
#define IFLA_XDP 43
#endif
#ifndef IFLA_XDP_FD
#define IFLA_XDP_FD 1
#endif
#ifndef IFLA_XDP_ATTACHED
#define IFLA_XDP_ATTACHED 2
#endif
#ifndef IFLA_XDP_FLAGS
#define IFLA_XDP_FLAGS 3
#endif
#ifndef IFLA_XDP_PROG_ID
#define IFLA_XDP_PROG_ID 4
#endif
#ifndef IFLA_XDP_EXPECTED_FD
#define IFLA_XDP_EXPECTED_FD 6
#endif
#ifndef RTEXT_FILTER_XDP
#define RTEXT_FILTER_XDP 0x400
#endif
#ifndef XDP_ATTACHED_SKB
#define XDP_ATTACHED_SKB 1
#endif
#ifndef XDP_ATTACHED_DRV
#define XDP_ATTACHED_DRV 2
#endif
#ifndef XDP_ATTACHED_HW
#define XDP_ATTACHED_HW 3
#endif

static __u32 nl_seq = 1;

static int nl_open(void)
{
	struct sockaddr_nl sa = {
		.nl_family = AF_NETLINK,
	};
	socklen_t addrlen = sizeof(sa);
	int fd;

	fd = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
	if (fd < 0)
		return -errno;

	if (bind(fd, (struct sockaddr *)&sa, addrlen) < 0) {
		int err = -errno;

		close(fd);
		return err;
	}

	return fd;
}

static int nl_addattr(struct nlmsghdr *n, size_t maxlen, int type,
		      const void *data, size_t alen)
{
	size_t len = RTA_LENGTH(alen);
	struct rtattr *rta;

	if (NLMSG_ALIGN(n->nlmsg_len) + RTA_ALIGN(len) > maxlen)
		return -EMSGSIZE;

	rta = (struct rtattr *)((char *)n + NLMSG_ALIGN(n->nlmsg_len));
	rta->rta_type = type;
	rta->rta_len = len;
	if (alen)
		memcpy(RTA_DATA(rta), data, alen);
	n->nlmsg_len = NLMSG_ALIGN(n->nlmsg_len) + RTA_ALIGN(len);
	return 0;
}

static struct rtattr *nl_addattr_nest(struct nlmsghdr *n, size_t maxlen,
				      int type)
{
	struct rtattr *nest =
		(struct rtattr *)((char *)n + NLMSG_ALIGN(n->nlmsg_len));

	if (nl_addattr(n, maxlen, type, NULL, 0))
		return NULL;
	return nest;
}

static void nl_addattr_nest_end(struct nlmsghdr *n, struct rtattr *nest)
{
	nest->rta_len = (char *)n + NLMSG_ALIGN(n->nlmsg_len) - (char *)nest;
}

static int nl_send(int fd, struct nlmsghdr *n)
{
	struct sockaddr_nl nladdr = {
		.nl_family = AF_NETLINK,
	};

	n->nlmsg_seq = nl_seq++;
	if (sendto(fd, n, n->nlmsg_len, 0, (struct sockaddr *)&nladdr,
		   sizeof(nladdr)) < 0)
		return -errno;
	return 0;
}

/* Drain replies until the ACK for seq arrives; return 0 or negative errno. */
static int nl_recv_ack(int fd, __u32 seq)
{
	char buf[8192];
	int ret;

	for (;;) {
		ret = recv(fd, buf, sizeof(buf), 0);
		if (ret < 0) {
			if (errno == EINTR)
				continue;
			return -errno;
		}

		for (struct nlmsghdr *h = (struct nlmsghdr *)buf;
		     NLMSG_OK(h, (unsigned int)ret);
		     h = NLMSG_NEXT(h, ret)) {
			struct nlmsgerr *err;

			if (h->nlmsg_seq != seq)
				continue;
			if (h->nlmsg_type == NLMSG_ERROR) {
				err = (struct nlmsgerr *)NLMSG_DATA(h);
				return err->error; /* 0 on ACK, negative errno */
			}
			if (h->nlmsg_type == NLMSG_DONE)
				return 0;
		}
	}
}

/* Parse a link dump (RTM_GETLINK + NLM_F_DUMP) for our ifindex. */
static int nl_recv_link(int fd, __u32 seq, int ifindex, __u32 xdp_flags,
			__u32 *prog_id)
{
	char buf[16384];
	int ret;

	for (;;) {
		ret = recv(fd, buf, sizeof(buf), 0);
		if (ret < 0) {
			if (errno == EINTR)
				continue;
			return -errno;
		}

		for (struct nlmsghdr *h = (struct nlmsghdr *)buf;
		     NLMSG_OK(h, (unsigned int)ret);
		     h = NLMSG_NEXT(h, ret)) {
			struct ifinfomsg *ifi;
			struct rtattr *rta;
			int attrlen;

			if (h->nlmsg_seq != seq)
				continue;
			if (h->nlmsg_type == NLMSG_DONE)
				return 0;
			if (h->nlmsg_type == NLMSG_ERROR) {
				struct nlmsgerr *err =
					(struct nlmsgerr *)NLMSG_DATA(h);

				return err->error;
			}
			if (h->nlmsg_type != RTM_NEWLINK)
				continue;

			ifi = (struct ifinfomsg *)NLMSG_DATA(h);
			if ((int)ifi->ifi_index != ifindex)
				continue;

			attrlen = h->nlmsg_len - NLMSG_LENGTH(sizeof(*ifi));
			for (rta = IFLA_RTA(ifi); RTA_OK(rta, attrlen);
			     rta = RTA_NEXT(rta, attrlen)) {
				struct rtattr *xrta;
				int xlen;
				__u8 attached = 0;
				__u32 id = 0;

				if (rta->rta_type != IFLA_XDP)
					continue;

				xlen = RTA_PAYLOAD(rta);
				for (xrta = (struct rtattr *)RTA_DATA(rta);
				     RTA_OK(xrta, xlen);
				     xrta = RTA_NEXT(xrta, xlen)) {
					if (xrta->rta_type == IFLA_XDP_ATTACHED)
						attached = *(const __u8 *)RTA_DATA(xrta);
					else if (xrta->rta_type == IFLA_XDP_PROG_ID)
						id = *(const __u32 *)RTA_DATA(xrta);
				}

				if ((attached == XDP_ATTACHED_SKB &&
				     (xdp_flags & XDP_FLAGS_SKB_MODE)) ||
				    (attached == XDP_ATTACHED_DRV &&
				     (xdp_flags & XDP_FLAGS_DRV_MODE)) ||
				    (attached == XDP_ATTACHED_HW &&
				     (xdp_flags & XDP_FLAGS_HW_MODE)))
					*prog_id = id;
				return 0;
			}
			return 0;
		}
	}
}

/* prog_fd >= 0 attaches, prog_fd == -1 detaches. */
static int xdp_link_set(int ifindex, int prog_fd, __u32 xdp_flags)
{
	struct {
		struct nlmsghdr n;
		struct ifinfomsg ifi;
		char attrbuf[64];
	} req;
	struct rtattr *nest;
	__u32 mode_flags;
	__u32 fd = (__u32)prog_fd;
	__u32 zero = 0;
	int nlfds, ret;

	memset(&req, 0, sizeof(req));
	req.n.nlmsg_len = NLMSG_LENGTH(sizeof(struct ifinfomsg));
	req.n.nlmsg_type = RTM_SETLINK;
	req.n.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
	req.ifi.ifi_family = AF_UNSPEC;
	req.ifi.ifi_index = ifindex;

	mode_flags = xdp_flags & (XDP_FLAGS_SKB_MODE | XDP_FLAGS_DRV_MODE |
				  XDP_FLAGS_HW_MODE);

	nest = nl_addattr_nest(&req.n, sizeof(req), IFLA_XDP);
	if (!nest)
		return -EMSGSIZE;
	nl_addattr(&req.n, sizeof(req), IFLA_XDP_FD, &fd, sizeof(fd));
	if (mode_flags)
		nl_addattr(&req.n, sizeof(req), IFLA_XDP_FLAGS, &mode_flags,
			   sizeof(mode_flags));
	if (prog_fd >= 0 && (xdp_flags & XDP_FLAGS_UPDATE_IF_NOEXIST))
		nl_addattr(&req.n, sizeof(req), IFLA_XDP_EXPECTED_FD, &zero,
			   sizeof(zero));
	nl_addattr_nest_end(&req.n, nest);

	nlfds = nl_open();
	if (nlfds < 0)
		return nlfds;

	ret = nl_send(nlfds, &req.n);
	if (!ret)
		ret = nl_recv_ack(nlfds, req.n.nlmsg_seq);

	close(nlfds);
	return ret;
}

int xdp_link_attach(int ifindex, int prog_fd, __u32 xdp_flags)
{
	return xdp_link_set(ifindex, prog_fd, xdp_flags);
}

int xdp_link_detach(int ifindex, __u32 xdp_flags)
{
	return xdp_link_set(ifindex, -1, xdp_flags);
}

int xdp_link_query_id(int ifindex, __u32 xdp_flags, __u32 *prog_id)
{
	struct {
		struct nlmsghdr n;
		struct ifinfomsg ifi;
		char attrbuf[64];
	} req;
	__u32 ext_mask = RTEXT_FILTER_XDP;
	int fd, ret;

	*prog_id = 0;

	memset(&req, 0, sizeof(req));
	req.n.nlmsg_len = NLMSG_LENGTH(sizeof(struct ifinfomsg));
	req.n.nlmsg_type = RTM_GETLINK;
	req.n.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
	req.ifi.ifi_family = AF_UNSPEC;
	req.ifi.ifi_index = ifindex;
	nl_addattr(&req.n, sizeof(req), IFLA_EXT_MASK, &ext_mask,
		   sizeof(ext_mask));

	fd = nl_open();
	if (fd < 0)
		return fd;

	ret = nl_send(fd, &req.n);
	if (!ret)
		ret = nl_recv_link(fd, req.n.nlmsg_seq, ifindex, xdp_flags,
				   prog_id);

	close(fd);
	return ret;
}
