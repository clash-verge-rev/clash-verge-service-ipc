# Usage:
# make nsi DIR=./target/aarch64-pc-windows-gnullvm/release/

NSIS=makensis
NSI=./resources/installer.nsi
INSTALLER=ClashVergeServiceInstaller.exe

DIR ?= ./target/release
BINS=$(wildcard $(DIR)/*.exe)

.PHONY: nsi clean

nsi:
	@if [ -z "$(BINS)" ]; then \
		echo "No .exe files found in $(DIR)"; \
		exit 1; \
	fi
	@echo ">> Building installer with $(BINS)"
	@( \
		cat $(NSI) | sed '/;FILES_PLACEHOLDER/q'; \
		for f in $(BINS); do \
			echo "  File \"$$f\""; \
		done; \
		sed '1,/$$/d' $(NSI) | sed '1,/;FILES_PLACEHOLDER/d'; \
	) > installer_tmp.nsi
	@$(NSIS) installer_tmp.nsi
	@rm installer_tmp.nsi

clean:
	@rm -f $(INSTALLER)
