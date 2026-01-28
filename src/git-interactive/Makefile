$(info entering directory `$(shell pwd)')
$(info DIST_DIR: $(DIST_DIR))
$(if $(DIST_DIR),,$(error DIST_DIR is not set))

EXECUTABLE := $(DIST_DIR)/git-interactive

$(EXECUTABLE): src/git-interactive.sh
	cp $< $@
	chmod +x $@