# Shopping Sessions — Rust pipeline. `make help` lists targets.
IMAGE    ?= shopping-sessions:latest
CPUS     ?= 8
MEM      ?= 24g
PLATFORM ?= linux/amd64
DEV      ?= dev
BENCH    ?= bench
ANSWER   ?= dev-answer
OUT      ?= out
FIXTURES ?= fixtures
PY       ?= $(if $(wildcard .venv/bin/python),.venv/bin/python,python3)
BIN      := target/release/pipeline
GEN      := bin/generate
# Native runs are pinned to $(CPUS) cores, like the scorer's 8 pinned vCPUs.
PIN      := taskset -c 0-$(shell echo $$(($(CPUS)-1)))
# Native-run scratch dir (the container gets its own /scratch).
LSCRATCH := $(CURDIR)/target/scratch
# docker_run.sh makes its scratch dir with mktemp; keep it off the small /tmp tmpfs.
export TMPDIR := $(CURDIR)/target/tmp
export IMAGE CPUS MEM PLATFORM

# run NAME DATA OUT: run the native binary like the scorer would, with time + peak RSS.
define native_run
	@rm -rf $(3) $(LSCRATCH) && mkdir -p $(3) $(LSCRATCH)
	@echo "== native $(1): $(2) -> $(3)"
	DATA_DIR=$(2) OUTPUT_DIR=$(3) SCRATCH_DIR=$(LSCRATCH) NCPU=$(CPUS) MEM_GB=$(MEM:g=) \
	  /usr/bin/time -f "== wall %es  user %Us  sys %Ss  maxrss %MKB" $(PIN) $(BIN)
endef

.PHONY: help bin build fixtures fixtures-local test dev bench-data seed run-dev run-dev-local \
        check-dev run-bench run-bench-local final-data run-final-local gate venv clean-out

help:          ## List targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | sed 's/:.*## /\t/' | expand -t 18

bin:           ## cargo build --release (native, same flags as the image)
	cargo build --release
	@mkdir -p $(TMPDIR)

build:         ## Build the scorer image (linux/amd64)
	docker buildx build --platform $(PLATFORM) --load -t $(IMAGE) .
	@mkdir -p $(TMPDIR)

fixtures-local: bin ## Run the 36 fixtures with the native binary (fast loop)
	$(PY) scripts/run_fixtures.py $(FIXTURES) -- $(BIN)

fixtures: build ## Run the 36 fixtures in the image with scorer flags
	$(PY) scripts/run_fixtures.py $(FIXTURES) -- scripts/docker_run.sh

test: fixtures-local run-dev-local check-dev ## Fast correctness gate: fixtures + dev-set exact diff (native)

$(GEN):
	cd tools/generator && cargo build --release
	mkdir -p bin && cp tools/generator/target/release/generate $(GEN)

dev: $(GEN)    ## Generate the public 1M-event dev set (seed 1) into dev/
	$(GEN) --seed 1 --events 1000000 --clicks 250000 --output $(DEV)

bench-data: $(GEN) ## Generate a 20M-event practice-size set (seed 2) into bench/
	$(GEN) --seed 2 --events 20000000 --clicks 5000000 --output $(BENCH)

SEED   ?= 3
EVENTS ?= 200000
CLICKS ?= $(shell echo $$(($(EVENTS)/4)))
seed: $(GEN)   ## Generate seeds/s$(SEED) (SEED=N EVENTS=E, clicks = E/4)
	$(GEN) --seed $(SEED) --events $(EVENTS) --clicks $(CLICKS) --output seeds/s$(SEED)
	@cat seeds/s$(SEED)/manifest.json

final-data: $(GEN) ## Generate a 60M-event final-scale set (seed 4) into seeds/final (~5.5 GB)
	$(GEN) --seed 4 --events 60000000 --clicks 15000000 --output seeds/final

run-final-local: bin ## Run native binary on seeds/final -> out-final/ (timed: 3 min cap, 24 GB) + selfcheck
	$(call native_run,final,seeds/final,out-final)
	$(PY) scripts/selfcheck.py out-final

run-dev-local: bin ## Run native binary on dev/ -> out/ (timed)
	$(call native_run,dev,$(DEV),$(OUT))

check-dev:     ## Exact diff of out/ against dev-answer/
	$(PY) scripts/compare.py $(OUT) $(ANSWER)

run-dev: build ## Run the image on dev/ -> out/ with scorer flags (timed)
	rm -rf $(OUT) && time scripts/docker_run.sh $(DEV) $(OUT)

run-bench-local: bin ## Run native binary on bench/ -> out-bench/ (timed) + selfcheck
	$(call native_run,bench,$(BENCH),out-bench)
	$(PY) scripts/selfcheck.py out-bench

run-bench: build ## Run the image on bench/ -> out-bench/ with scorer flags (timed) + selfcheck
	rm -rf out-bench && time scripts/docker_run.sh $(BENCH) out-bench
	$(PY) scripts/selfcheck.py out-bench

gate: fixtures run-dev check-dev run-bench ## Full pre-submit gate, all in Docker

venv:          ## Create .venv with polars for the test scripts
	python3 -m venv .venv && .venv/bin/pip install -q polars pyarrow

clean-out:     ## Remove run outputs
	rm -rf $(OUT) out-bench out-final $(LSCRATCH)
