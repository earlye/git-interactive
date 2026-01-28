$(info entering directory `$(shell pwd)')
$(info DIST_DIR: $(DIST_DIR))
$(if $(DIST_DIR),,$(error DIST_DIR is not set))

EXECUTABLE := $(DIST_DIR)/git-interactive-signing-key
CARGO_TARGET := target/release/git-interactive-signing-key

SOURCES := $(shell find src -type f) Cargo.toml
$(info SOURCES: $(SOURCES))

.PHONY: all
all: $(EXECUTABLE)

$(EXECUTABLE): $(SOURCES)
	cargo build --release
	cp $(CARGO_TARGET) $@