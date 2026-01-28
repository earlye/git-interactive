ifdef QUIET
  MAKE_FLAGS += --no-print-directory
  Q := @
else
  Q :=
endif

TARGETS := $(notdir $(wildcard src/*))
BINARIES := $(addprefix dist/bin/, $(TARGETS))
MAJOR := 0
MINOR := 1
PATCH ?= 99999+local-$(shell git rev-parse --abbrev-ref HEAD 2> /dev/null)-$(shell git rev-list --count HEAD 2>/dev/null)-$(shell git rev-parse --short HEAD)
VERSION := $(MAJOR).$(MINOR).$(PATCH)
log = $(if $(QUIET),,$(info $(1)))

$(call log,entering directory `$(shell pwd)')
$(call log,VERSION: $(VERSION))

.PHONY: all FORCE
all: $(BINARIES)
	@:

FORCE:
dist/bin/%: FORCE
	$(Q) mkdir -p $(dir $@)
	$(Q) make -C src/$* DIST_DIR=$(abspath $(dir $@))

dist:
	$(Q) mkdir -p dist

link: dist FORCE
	$(Q) ln -s $(abspath dist) ~/.git-interactive

.PHONY: version
version:
	@printf $(VERSION)