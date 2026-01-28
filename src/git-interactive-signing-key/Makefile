log = $(if $(QUIET),,$(info $(1)))
$(call log,entering directory `$(shell pwd)')
$(call log,DIST_DIR: $(DIST_DIR))
$(if $(DIST_DIR),,$(error DIST_DIR is not set))

EXECUTABLE := $(DIST_DIR)/git-interactive-signing-key
CARGO_TARGET := target/release/git-interactive-signing-key

SOURCES := $(shell find src -type f) Cargo.toml
$(call log,SOURCES: $(SOURCES))

.PHONY: all
all: $(EXECUTABLE)
	@:

$(EXECUTABLE): $(SOURCES)
	cargo build --release
	cp $(CARGO_TARGET) $@