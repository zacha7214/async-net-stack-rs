# xdp-c-ref - Linux-only AF_XDP reference/test project.
#
# Builds two user-space tools (xdpsock, xdp_caps) plus two BPF objects
# against xdp-tools' libxdp (v1.6.3) and libbpf (v1.5.0), both pinned and
# built in-tree under deps/ so the build does not depend on distro package
# versions.
#
# System requirements (see README.md):
#   gcc, clang, make, git, pkg-config, libelf-dev, zlib1g-dev, libpcap-dev
#
# Targets:
#   make            build everything (implies deps)
#   make deps       clone and build libbpf + libxdp under deps/
#   make clean      remove build/
#   make distclean  remove build/ and deps/

CC       ?= cc
CLANG    ?= clang
MAKEFLAGS += --no-print-directory

TOPDIR   := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
SRC      := $(TOPDIR)/src
DEPS     := $(TOPDIR)/deps
BUILD    := $(TOPDIR)/build

LIBBPF_DIR    := $(DEPS)/libbpf
LIBBPF_TAG    := v1.5.0
XDP_TOOLS_DIR := $(DEPS)/xdp-tools
XDP_TOOLS_TAG := v1.6.3

LIBBPF_A := $(LIBBPF_DIR)/src/libbpf.a
LIBXDP_A := $(XDP_TOOLS_DIR)/lib/libxdp/libxdp.a

CPPFLAGS += -I$(SRC) -I$(XDP_TOOLS_DIR)/headers -I$(LIBBPF_DIR)/src
CFLAGS   ?= -O2 -g -Wall -Wextra
LDLIBS   += $(LIBXDP_A) $(LIBBPF_A) -lelf -lz

BPF_CFLAGS := -O2 -g -Wall -Werror -target bpf \
              -I$(SRC) -I$(LIBBPF_DIR)/src

USER_BINS := $(BUILD)/xdpsock $(BUILD)/xdp_caps
BPF_OBJS  := $(BUILD)/xdpsock_kern.o $(BUILD)/xdp_filter_kern.o

all: $(USER_BINS) $(BPF_OBJS)

$(LIBBPF_A):
	test -d $(LIBBPF_DIR)/.git || \
		git clone --depth 1 --branch $(LIBBPF_TAG) \
			https://github.com/libbpf/libbpf $(LIBBPF_DIR)
	$(MAKE) -C $(LIBBPF_DIR)/src BUILD_STATIC_ONLY=1

# xdp-tools is configured against our in-tree libbpf build via LIBBPF_DIR.
# Its configure requires pkg-config'able libelf, zlib and libpcap even
# though we only build the libxdp subset.
# xdp-tools is configured against our in-tree libbpf build via LIBBPF_DIR.
# LIBBPF_UNBUILT makes configure probe libbpf functions header-only: the
# real link test cannot see -lelf/-lz, which would otherwise disable the
# HAVE_LIBBPF_* features and break the build with -Werror.
# Its configure requires pkg-config'able libelf, zlib and libpcap even
# though we only build the libxdp subset.
$(LIBXDP_A): $(LIBBPF_A)
	test -d $(XDP_TOOLS_DIR)/.git || \
		git clone --depth 1 --branch $(XDP_TOOLS_TAG) \
			https://github.com/xdp-project/xdp-tools $(XDP_TOOLS_DIR)
	cd $(XDP_TOOLS_DIR) && \
		LIBBPF_DIR=$(LIBBPF_DIR) \
		LIBBPF_INCLUDE_DIR=$(LIBBPF_DIR)/src \
		LIBBPF_UNBUILT=1 \
		./configure
	$(MAKE) -C $(XDP_TOOLS_DIR) libxdp BUILD_STATIC_ONLY=1

$(BUILD):
	mkdir -p $@

$(BUILD)/xdpsock: $(SRC)/xdpsock_user.c $(SRC)/xdpsock.h \
		  $(SRC)/xdp_link.c $(SRC)/xdp_link.h $(LIBXDP_A) | $(BUILD)
	$(CC) $(CPPFLAGS) $(CFLAGS) -o $@ \
		$(SRC)/xdpsock_user.c $(SRC)/xdp_link.c $(LDLIBS) -lpthread -lcap

$(BUILD)/xdp_caps: $(SRC)/xdp_caps.c $(SRC)/xdp_link.c $(SRC)/xdp_link.h \
		   $(LIBXDP_A) | $(BUILD)
	$(CC) $(CPPFLAGS) $(CFLAGS) -o $@ \
		$(SRC)/xdp_caps.c $(SRC)/xdp_link.c $(LDLIBS)

$(BPF_OBJS): $(BUILD)/%.o: $(SRC)/%.c $(LIBBPF_A) | $(BUILD)
	$(CLANG) $(BPF_CFLAGS) -c -o $@ $<

deps: $(LIBXDP_A)

clean:
	rm -rf $(BUILD)

distclean: clean
	rm -rf $(DEPS)

.PHONY: all deps clean distclean
