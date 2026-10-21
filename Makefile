WASM := target/wasm32-unknown-unknown
RUNTIME := --target wasm32-unknown-unknown -p edge-python --lib --no-default-features --features runtime --crate-type cdylib
WASM_FEATURES := --enable-bulk-memory-opt --enable-nontrapping-float-to-int --enable-sign-ext
# wasm-opt inlines a function with one caller at any size, which merges the VM's loops into one function past what V8 gives
# its full register allocator (8192 virtual registers); a big one stays apart.
WASM_INLINE := --one-caller-inline-max-function-size=1000
TSC := npm:typescript@5.9.3/tsc
TREE := $(CURDIR)/_cdn
EXE := $(if $(filter Windows_NT,$(OS)),.exe)

# The CLI embeds the speed compiler, js/dist and the Lucide names, unless the environment points it at copies.
CLI_INPUTS := $(if $(EDGE_COMPILER_WASM),,wasm-cli) $(if $(EDGE_JS_DIST),,js) lucide

CLI_TEST_engine := --bin edge --test cli --test run -- --skip builtin_corpora_mirror_the_web_api
CLI_TEST_network := --test run builtin_corpora_mirror_the_web_api
CLI_TEST_actors := --test actor
CLI_TEST_consistency := --test consistency
CLI_NEEDS_engine := plugin
CLI_NEEDS_ := plugin
CLI_NEEDS_skill := test-skill
CLI_NEEDS_std := test-std

# The nightly Miri runs on, bumped by hand so a new check of Miri lands in a commit of its own.
MIRI_NIGHTLY := nightly-2026-10-11
# make/miri.sh sets Miri up once and passes MIRI_SETUP= to the runs it starts side by side.
MIRI_SETUP := miri-setup
MIRI_TESTS := -p edge-python --lib --test tests
MIRI_vm := vm:: embed::
MIRI_snapshot := snapshot::
MIRI_modules := modules::
MIRI_parser := parser::
MIRI_lexer := lexer::
MIRI_abi := abi::

unix_only = $(if $(filter Windows_NT,$(OS)),$(error AFL runs on Linux and macOS only))

.PHONY: bitcode inline native wasm wasm-cli wasm-ship wasm-small wasm-cli-opt size js lint lint-rust lint-js lint-cli test bench bench-update bench-accept-slower bench-llvm llvm-check plugin cli cli-release stage serve browsers test-js test-cli test-skill test-std miri miri-list miri-setup fuzz fuzz-status fuzz-triage fuzz-replay fuzz-carry fuzz-container fuzz-stop seeds lucide version check

wasm:
	cargo rustc --locked --release $(RUNTIME)

# The VM as one LLVM module per target, the program and its deps in one fat LTO unit: `vm.bc` with its handlers that may
# collect named in `!edge.safepoints` (the proofs read it), `vm.o` the same module compiled once, and `inline.bc` the
# handlers' and tag tests' bodies alone, as `available_externally`, which `edge build` inlines into its glue and links
# with vm.o, compiled once by llc. `make bitcode TORTURE=1` builds the copy that collects and moves the heap at each
# safepoint, for the tests, and `COUNT=1` the vm.o whose blocks count the instructions they run into `@edge.count`, for
# `edge build --count`.
# llvm-as comes with `rustup component add llvm-tools`, llc and llvm-extract with an LLVM of the same version (LLVM_X).
BITCODE_TARGETS := x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin wasm32-wasip1
LLVM_BIN = $(shell rustc --print sysroot)/lib/rustlib/$(shell rustc -vV | sed -n 's/host: //p')/bin
LLVM_X ?= $(LLVM_BIN)
INLINE := is_int is_float is_bool is_none is_heap is_undef is_err err_id as_int fits_int edge_go edge_back
bitcode: $(BITCODE_TARGETS:%=bitcode-%)
inline: $(BITCODE_TARGETS:%=inline-%)
bitcode-%: B = target/bitcode/$*$(if $(TORTURE),-torture)$(if $(COUNT),-count)
bitcode-%:
	mkdir -p $(B)
	cargo rustc --locked --profile cli -p edge-native --target $* $(if $(TORTURE),--features gc-torture) -- -C panic=abort -C embed-bitcode=yes --emit=llvm-ir=$(B)/vm.ll
	rustc --edition 2024 -O native/bitcode.rs -o target/bitcode/bitcode
	target/bitcode/bitcode safepoints $(B)/vm.ll
	$(LLVM_BIN)/llvm-as $(B)/vm.ll -o $(B)/vm.bc
	$(if $(COUNT),target/bitcode/bitcode count $(B)/vm.ll $(B)/vm.count.ll)
	$(LLVM_X)/llc -O3 -filetype=obj -relocation-model=$(if $(findstring musl,$*),static,pic) $(if $(COUNT),$(B)/vm.count.ll,$(B)/vm.bc) -o $(B)/vm.o
inline-%: B = target/bitcode/$*$(if $(TORTURE),-torture)$(if $(COUNT),-count)
inline-%: bitcode-%
	$(LLVM_X)/llvm-extract --rfunc '^op_' $(INLINE:%=--func %) -S $(B)/vm.bc -o $(B)/inline.ll
	target/bitcode/bitcode inline $(B)/vm.ll $(B)/inline.ll
	$(LLVM_BIN)/llvm-as $(B)/inline.ll -o $(B)/inline.bc

wasm-cli:
	cargo rustc --locked --profile cli $(RUNTIME)

# The small compiler.wasm that ships, and the speed copy the CLI precompiles to native code.
wasm-ship: wasm-small wasm-cli-opt

# Three passes, -Oz, reflatten for a fresh CFG, then a converge sweep. Traps stay, they guard bounds.
wasm-small: export RUSTFLAGS = -Z location-detail=none -Z fmt-debug=none -Z unstable-options -C panic=immediate-abort
wasm-small:
	cargo +nightly rustc --locked --release $(RUNTIME) -Z build-std=std,panic_abort -Z build-std-features=optimize_for_size
	wasm-opt -Oz $(WASM_INLINE) --converge --generate-global-effects --strip-debug --strip-producers $(WASM_FEATURES) -o $(WASM)/release/compiler.1.wasm $(WASM)/release/compiler.wasm
	wasm-opt --flatten --rereloop -Oz -Oz $(WASM_INLINE) $(WASM_FEATURES) -o $(WASM)/release/compiler.2.wasm $(WASM)/release/compiler.1.wasm
	wasm-opt -Oz $(WASM_INLINE) --converge $(WASM_FEATURES) -o $(WASM)/release/compiler.wasm $(WASM)/release/compiler.2.wasm

# A copy beside the speed build, since the bench counts the build before wasm-opt.
wasm-cli-opt: wasm-cli
	wasm-opt -O3 --enable-bulk-memory --enable-mutable-globals --enable-multivalue --enable-reference-types $(WASM_FEATURES) -o $(WASM)/cli/compiler-cli.wasm $(WASM)/cli/compiler.wasm

size:
	deno run --allow-read make/size.ts $(WASM)/release/compiler.wasm

# Strips types to js/dist and bundles the engine as one classic script the page hands the room.
js:
	cd js && deno run -A $(TSC) -p tsconfig.json
	cd js && deno run -A $(TSC) -p tsconfig.worker.json
	cd js && deno bundle --platform browser --format iife src/worker/worker.ts -o dist/worker/bundle.js

lint: lint-rust lint-js

# Shear refuses dependencies declared but unused, then clippy runs for the host and for wasm.
lint-rust:
	cargo shear
	cargo clippy --locked --all-targets -- -D warnings
	cargo clippy --locked --lib --no-default-features --features edge-python/runtime --target wasm32-unknown-unknown -p edge-python -p slugify-mod -- -D warnings
	cargo clippy --locked --lib --no-default-features --target wasm32-unknown-unknown -p edge-python -- -D warnings

# js/src splits main-thread and worker since tsc rejects mixing the dom and webworker libs.
lint-js:
	deno lint js/ make/
	deno check --config js/src/deno.json js/src/index.ts
	deno check --config js/src/worker/deno.json js/src/worker/worker.ts js/src/worker/engine.ts
	deno check make/

lint-cli: $(CLI_INPUTS)
	cd cli && cargo clippy --locked --all-targets -- -D warnings
	cd cli && cargo check --locked

# Coverage leaves method calls unfused, so it runs apart.
test:
	cargo test --locked -p edge-python
	cargo test --locked -p edge-python --features coverage coverage

bench: wasm-cli
	cargo run --locked -p bench --profile cli

bench-update: wasm-cli
	cargo run --locked -p bench --profile cli -- --update

bench-accept-slower: wasm-cli
	cargo run --locked -p bench --profile cli -- --update --accept-slower

# The native build against the same programs in Rust, on the CLI built here.
bench-llvm: cli
	cargo run --locked -p bench --profile cli -- llvm $(if $(UPDATE),--update)

llvm-check: $(CLI_INPUTS)
	cd cli && cargo test --locked --release --test llvm_check

plugin:
	cargo build --locked --release --target wasm32-unknown-unknown -p slugify-mod

cli: $(CLI_INPUTS)
	cd cli && cargo build --locked --release

# The CLI whose `edge build` compiles to every target, the VM's bitcode for each, wasm32-wasip1 among them, carried inside it.
native: bitcode
	$(MAKE) cli

# musl-gcc has no C++, so the static Linux build runs in Alpine, whose libc is musl.
cli-release: $(CLI_INPUTS)
	$(if $(TARGET),,$(error pass TARGET, for example make cli-release TARGET=aarch64-apple-darwin))
	$(if $(findstring linux-musl,$(TARGET)),deno run -A make/musl.ts $(TARGET),cd cli && cargo build --locked --release --target $(TARGET))
	tar -C cli/target/$(TARGET)/release -czf cli/edge-$(TARGET).tar.gz edge$(EXE)

cdn/node_modules/.package-lock.json: cdn/package-lock.json
	npm --prefix cdn ci

stage: cdn/node_modules/.package-lock.json
	npm --prefix cdn run stage -- $(TREE) $(PARTS)

serve: cdn/node_modules/.package-lock.json
	npm --prefix cdn run serve -- $(TREE)

browsers:
	deno run -A npm:playwright install --with-deps chromium

test-js: plugin
	deno test --allow-all js/tests/

# engine runs cli and run minus the network corpus, network runs that corpus, actors and skill theirs.
test-cli: $(CLI_INPUTS) $(CLI_NEEDS_$(SUITE))
	$(if $(filter-out engine network actors consistency skill std,$(SUITE)),$(error SUITE is one of engine network actors consistency skill std, or none for all))
	$(if $(filter skill std,$(SUITE)),,cd cli && cargo test --locked $(CLI_TEST_$(SUITE)))

test-skill: export SKILL_EDGE = $(CURDIR)/cli/target/debug/edge$(EXE)
test-skill: $(CLI_INPUTS)
	cd cli && cargo build --locked
	cargo test --locked -p skill

# The tests of every edge-python-std package, run on the CLI built here so no engine change breaks them unnoticed.
test-std: export EDGE = $(CURDIR)/cli/target/debug/edge$(EXE)
test-std: $(CLI_INPUTS)
	cd cli && cargo build --locked
	rm -rf target/std && git clone -q --depth 1 https://github.com/dylan-sutton-chavez/edge-python-std target/std
	$(MAKE) -C target/std wasm
	for package in target/std/edge/*/; do (cd "$$package" && "$$EDGE" lock && "$$EDGE" test) || exit 1; done

miri-setup:
	rustup toolchain install $(MIRI_NIGHTLY) --profile minimal --component miri
	cargo +$(MIRI_NIGHTLY) miri setup

# One suite, or with CASE one test of it by its exact name, and EDGE_SLICE=k/n reaching the test as it does natively.
miri: export MIRIFLAGS += -Zmiri-env-forward=EDGE_SLICE
miri: $(MIRI_SETUP)
	$(if $(MIRI_$(SUITE)),,$(error SUITE is one of vm snapshot modules parser lexer abi))
	cargo +$(MIRI_NIGHTLY) miri test --locked $(MIRI_TESTS) -- $(if $(CASE),--exact $(CASE),$(MIRI_$(SUITE)))

# The tests of a suite, one name per line.
miri-list: $(MIRI_SETUP)
	$(if $(MIRI_$(SUITE)),,$(error SUITE is one of vm snapshot modules parser lexer abi))
	@cargo +$(MIRI_NIGHTLY) miri test --locked -q $(MIRI_TESTS) -- --list --format=terse $(MIRI_$(SUITE)) 2>/dev/null | sed -n 's/: test$$//p'

# One AFL instance per core, JOBS, DURATION, FRESH and TIMEOUT_MS tune the campaign.
fuzz:
	$(unix_only)
	cd fuzz && bash ./deploy.sh

fuzz-status:
	$(unix_only)
	cd fuzz && cargo afl whatsup -s out

fuzz-triage:
	$(unix_only)
	cd fuzz && bash ./triage.sh

fuzz-replay: export RUST_BACKTRACE = 1
fuzz-replay:
	$(unix_only)
	$(if $(CASE),,$(error pass CASE, a saved input such as out/m0/crashes/id))
	cd fuzz && ./target/release/afl-pipeline < $(CASE)

# Minimizes the queues and the old corpus into corpus/.
fuzz-carry:
	$(unix_only)
	cd fuzz && bash ./carry.sh

# The same campaign detached in Docker, surviving reboots with its findings in a volume.
fuzz-container:
	cd fuzz && docker compose up --build -d

fuzz-stop:
	cd fuzz && docker compose down

seeds:
	$(unix_only)
	cd fuzz && bash ./seeds.sh

# The icon names a docs card can carry, from the Lucide release VERSION names or the latest.
lucide:
	deno run --allow-net --allow-write=target make/lucide.ts $(VERSION)

version:
	$(if $(TAG),,$(error pass TAG, for example make version TAG=v1.0.0))
	deno run --allow-read make/version.ts $(TAG)

check: wasm lint test
