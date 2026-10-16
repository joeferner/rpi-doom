# rpi-doom, for the Raspberry Pi 2 (B) (armv7a-none-eabi).
#
#   make                          build target/kernel7.img
#   make run DEVICE=/dev/ttyUSB0  build it, upload it over the UART with
#                                 rpi-loader, and boot it
#   make sd-card                  put what goes on the SD card into
#                                 sd-card/ (firmware + the rpi-loader image,
#                                 fetched; the WADs, copied from doom/)
#   make wads DEVICE=/dev/ttyUSB0 upload the WADs from doom/ to the card's
#                                 doom/ over the UART, with the card in
#                                 the board
#
# `cargo objcopy` needs cargo-binutils: `cargo install cargo-binutils`.
# `make run` needs the loader's host CLI: `cargo install rpi-loader`.

KERNEL := kernel7.img
# Where rpi-hal's linker script links this target, and so where the
# loader has to put the image.
LOAD_ADDR := 0x8000
DEVICE ?= /dev/ttyUSB0

# What `make sd-card` fetches. Each file is checked against
# sd-card.sha256 before it is kept.
FIRMWARE_URL := https://raw.githubusercontent.com/raspberrypi/firmware/1.20260824/boot
FIRMWARE_FILES := bootcode.bin start.elf fixup.dat LICENCE.broadcom
LOADER_URL := https://github.com/joeferner/rpi-hal/releases/download/loader-v0.5.1
LOADER_IMAGE := rpi-loader-0.5.1-bcm2837-kernel7.img

# The game data. It isn't in the repository (DOOM.WAD is commercial), so
# it comes from a local doom/ directory -- a copy of a Doom install, or
# just the WAD -- and goes on the card in doom/, the directory the
# program reads it from (src/files.rs's HOME).
WAD_DIR := doom
WADS := $(wildcard $(WAD_DIR)/*.WAD $(WAD_DIR)/*.wad)

.PHONY: all build run sd-card wads clean

all: build

build:
	cargo build --release
	cargo objcopy --release -- -O binary target/$(KERNEL)
	@echo "built target/$(KERNEL)"

run: build
	rpi-loader --device $(DEVICE) boot --load-addr $(LOAD_ADDR) target/$(KERNEL)

# Everything the card's FAT32 boot partition needs: the Raspberry Pi boot
# firmware, config.txt, the rpi-loader image renamed to $(KERNEL) -- the
# only filename the firmware will load on this board -- and the WADs.
# Copy the whole of sd-card/ onto the card.
sd-card: $(addprefix sd-card/,$(FIRMWARE_FILES)) sd-card/$(KERNEL) \
		$(patsubst $(WAD_DIR)/%,sd-card/doom/%,$(WADS))
	$(if $(WADS),,$(error no WAD in $(WAD_DIR)/: put DOOM.WAD (or DOOM1.WAD, DOOM2.WAD, ...) there))
	cd sd-card && sha256sum -c ../sd-card.sha256
	@echo "copy everything in sd-card/ onto the card's boot partition"

sd-card/doom/%: $(WAD_DIR)/%
	@mkdir -p sd-card/doom
	cp $< $@

# The same WADs, straight onto the card in the board through the
# resident loader, so the card never has to come out. The loader has to
# be the thing running: reset the board first if it's running this
# program. `sd-mkdir` makes one level and the directory may already be
# there, so it is only asked for when `sd-list` can't find it.
wads:
	$(if $(WADS),,$(error no WAD in $(WAD_DIR)/: put DOOM.WAD (or DOOM1.WAD, DOOM2.WAD, ...) there))
	rpi-loader --device $(DEVICE) sd-list /doom >/dev/null 2>&1 \
		|| rpi-loader --device $(DEVICE) sd-mkdir /doom
	for wad in $(WADS); do \
		rpi-loader --device $(DEVICE) sd-write $$wad /doom/$$(basename $$wad) || exit 1; \
	done

# $(call fetch,<url>,<name on the card>). Downloads to a temporary name and
# only moves the file into place once its checksum matches, so a truncated
# transfer leaves nothing that a later `make` would mistake for a good copy.
define fetch
	@mkdir -p sd-card
	@echo "fetching $(2)"
	@curl -fsSL --proto '=https' -o sd-card/$(2).part $(1)
	@expected=$$(awk -v name="$(2)" '$$2 == name { print $$1 }' sd-card.sha256); \
	 actual=$$(sha256sum < sd-card/$(2).part | cut -d' ' -f1); \
	 if [ "$$expected" != "$$actual" ]; then \
	   echo "$(2): checksum mismatch (expected $$expected, got $$actual)" >&2; \
	   rm -f sd-card/$(2).part; exit 1; \
	 fi
	@mv sd-card/$(2).part sd-card/$(2)
endef

sd-card/$(KERNEL): sd-card.sha256
	$(call fetch,$(LOADER_URL)/$(LOADER_IMAGE),$(KERNEL))

sd-card/%: sd-card.sha256
	$(call fetch,$(FIRMWARE_URL)/$*,$*)

clean:
	cargo clean
