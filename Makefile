TARGETS := $(notdir $(wildcard src/*))
BINARIES := $(addprefix dist/bin/, $(TARGETS))
$(info entering directory `$(shell pwd)')
$(info TARGETS: $(TARGETS))
$(info BINARIES: $(BINARIES))

.PHONY: all FORCE
all: $(BINARIES)

FORCE:
dist/bin/%: FORCE
	mkdir -p $(dir $@)
	make -C src/$* DIST_DIR=$(abspath $(dir $@))

dist:
	mkdir -p dist

link: dist
	ln -s $(abspath dist) ~/.git-interactive