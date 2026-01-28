log = $(if $(QUIET),,$(info $(1)))

$(call log,entering directory `$(shell pwd)')
$(call log,TARGETS: $(TARGETS))
$(call log,DIST_DIR: $(DIST_DIR))
$(if $(DIST_DIR),,$(error DIST_DIR is not set))

EXECUTABLE := $(DIST_DIR)/git-interactive

.PHONY: all
all: $(EXECUTABLE)
	@:

$(EXECUTABLE): src/git-interactive.sh
	cp $< $@
	chmod +x $@

