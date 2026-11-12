WASM := target/wasm32-unknown-unknown
# edge.wasm is Lox's cdylib copied to the name every host loads
RUNTIME := --target wasm32-unknown-unknown -p lox --lib --no-default-features --features web --crate-type cdylib
# rig.wasm is the test rig's cdylib that the cli's rig test composes the kit over
RIG := --target wasm32-unknown-unknown -p rig --lib --no-default-features --features web --crate-type cdylib
WASM_FEATURES := --enable-bulk-memory-opt --enable-nontrapping-float-to-int --enable-sign-ext
# unbounded one-caller inlining merges the VM's loops past the 8192 registers V8 fully allocates
WASM_INLINE := --one-caller-inline-max-function-size=1000
TSC := npm:typescript@5.9.3/tsc
TREE := $(CURDIR)/_cdn
EXE := $(if $(filter Windows_NT,$(OS)),.exe)

# the CLI embeds the speed edge.wasm and js/dist unless the environment points at copies
CLI_INPUTS := $(if $(EDGE_WASM),,wasm-cli) $(if $(EDGE_JS_DIST),,js) lucide
# the components the edge binary wires as cli/Cargo.toml lists them under its all feature
CLI_COMPONENTS := $(shell sed -n 's/^all = \[\(.*\)\]$$/\1/p' cli/Cargo.toml | tr -d '",')

# the components the native test wires which build no wasmtime
CLI_NATIVE := docs,files,net,perms,run,test,web
CLI_TEST_engine := --lib --bin edge --test cli --test run --test native --test rig -- --skip net_suite
# the channels test fetches the pinned hour so it runs with the network cases
CLI_TEST_network := --test cli --test channels net_suite
CLI_TEST_actors := --test actor
CLI_TEST_consistency := --test consistency
# the rust examples of docs/ run as doctests of the kit
CLI_TEST_docs := --doc
CLI_NEEDS_engine := plugin wasm-rig
CLI_NEEDS_network := plugin
CLI_NEEDS_ := plugin wasm-rig
CLI_NEEDS_skill := test-skill

# bumped by hand so each new Miri check lands in a commit of its own
MIRI_NIGHTLY := nightly-2026-10-11
# make/miri.sh sets Miri up once then passes MIRI_SETUP= to its parallel runs
MIRI_SETUP := miri-setup
MIRI_TESTS := -p edge --lib --test tests
# the pool test covers the allocator wasm installs which no corpus test reaches
MIRI_vm := vm:: embed:: pooled_blocks_come_back_with_their_bytes
MIRI_snapshot := snapshot::
MIRI_modules := modules:: deps::
MIRI_parser := parser::
MIRI_lexer := lexer::
MIRI_abi := abi::

# lint-rust fails unless these are exactly the src files besides src/unsafe.rs with unsafe code
UNSAFE_LEFT :=

# where the shifted build's static data starts and 65536 more moves its heap too
LAYOUT_BASE ?= 1048640

unix_only = $(if $(filter Windows_NT,$(OS)),$(error AFL runs on Linux and macOS only))

.PHONY: wasm wasm-rig wasm-cli wasm-ship wasm-small wasm-cli-opt size js lint lint-rust lint-names lint-js lint-cli test bench bench-update bench-accept-slower bench-layout plugin cli cli-release stage serve browsers test-js test-cli test-skill miri miri-list miri-setup fuzz fuzz-status fuzz-triage fuzz-replay fuzz-carry fuzz-container fuzz-stop seeds lucide version check

wasm:
	cargo rustc --locked --release $(RUNTIME)
	cp $(WASM)/release/lox.wasm $(WASM)/release/edge.wasm

wasm-rig:
	cargo rustc --locked --release $(RIG)

wasm-cli:
	cargo rustc --locked --profile cli $(RUNTIME)
	cp $(WASM)/cli/lox.wasm $(WASM)/cli/edge.wasm

# the small edge.wasm that ships and the speed copy the CLI precompiles to native code
wasm-ship: wasm-small wasm-cli-opt

# the reflatten pass gives -Oz a fresh CFG and traps stay since they guard bounds
wasm-small: export RUSTFLAGS = -Z location-detail=none -Z fmt-debug=none -Z unstable-options -C panic=immediate-abort
wasm-small:
	cargo +nightly rustc --locked --release $(RUNTIME) -Z build-std=std,panic_abort -Z build-std-features=optimize_for_size
	cp $(WASM)/release/lox.wasm $(WASM)/release/edge.wasm
	wasm-opt -Oz $(WASM_INLINE) --converge --generate-global-effects --strip-debug --strip-producers $(WASM_FEATURES) -o $(WASM)/release/edge.1.wasm $(WASM)/release/edge.wasm
	wasm-opt --flatten --rereloop -Oz -Oz $(WASM_INLINE) $(WASM_FEATURES) -o $(WASM)/release/edge.2.wasm $(WASM)/release/edge.1.wasm
	wasm-opt -Oz $(WASM_INLINE) --converge $(WASM_FEATURES) -o $(WASM)/release/edge.wasm $(WASM)/release/edge.2.wasm

# a copy beside the speed build since the bench counts the build before wasm-opt
wasm-cli-opt: wasm-cli
	wasm-opt -O3 --enable-bulk-memory --enable-mutable-globals --enable-multivalue --enable-reference-types $(WASM_FEATURES) -o $(WASM)/cli/edge-cli.wasm $(WASM)/cli/edge.wasm

size:
	deno run --allow-read make/size.ts $(WASM)/release/edge.wasm

# strips types to js/dist and bundles the room's script and the composition a page reads
js:
	cd js && deno run -A $(TSC) -p tsconfig.json
	cd js && deno run -A $(TSC) -p tsconfig.worker.json
	cd js && deno bundle --platform browser --format iife src/worker/worker.ts -o dist/worker/bundle.js
	cd js && deno bundle --platform browser --format esm src/worker/composition.ts -o dist/worker/composition.js

lint: lint-rust lint-js

# shear refuses dependencies declared but unused then clippy runs for the host and for wasm
lint-rust: lint-names wasm
	@held=$$(for f in $$(find src -name '*.rs' ! -path src/unsafe.rs); do sed -e 's://.*::' -e 's:/[*].*[*]/::g' $$f | grep -Eq '(^|[^#[:alnum:]_])unsafe([^[:alnum:]_]|$$)' && echo $$f; done); \
	  stray=$$(printf '%s\n' $$held $(sort $(UNSAFE_LEFT)) | LC_ALL=C sort | uniq -u); \
	  [ -z "$$stray" ] || { printf 'UNSAFE_LEFT must name exactly the src files besides src/unsafe.rs with unsafe code and these differ\n%s\n' "$$stray"; exit 1; }
	cargo shear
	cargo clippy --locked --all-targets -- -D warnings
	cargo clippy --locked --lib --no-default-features --features edge/runtime,lox/web --target wasm32-unknown-unknown -p edge -p lib -p lox -p slugify-mod -- -D warnings
	cargo clippy --locked --lib --no-default-features --target wasm32-unknown-unknown -p edge -p lib -p lox -- -D warnings
	deno run --allow-read make/exports.ts $(WASM)/release/edge.wasm

# fails on any denied name in the engine lib or the plugin kit
lint-names:
	@deno run --allow-read make/names.ts make/names.txt src lib/src plugin

# js/src splits main-thread and worker since tsc rejects mixing the dom and webworker libs
lint-js:
	deno lint js/ make/ examples/net/
	deno check --config js/src/deno.json js/src/index.ts
	deno check --config js/src/worker/deno.json js/src/worker/worker.ts js/src/worker/engine.ts
	deno check make/

# each component builds alone, the runner example with only the three it needs and the native test with no wasmtime
lint-cli: $(CLI_INPUTS) wasm-rig
	cd cli && cargo clippy --locked --all-targets -- -D warnings
	cd cli && cargo check --locked
	cd cli && for f in "" $(CLI_COMPONENTS); do cargo clippy --locked --lib --no-default-features --features "$$f" -- -D warnings || exit 1; done
	cd cli && cargo clippy --locked --no-default-features --features embed,run,test --example runner --example decoupled -- -D warnings
	cd cli && cargo clippy --locked --no-default-features --features $(CLI_NATIVE) --test native -- -D warnings
	cd cli && ! cargo tree --locked --no-default-features --features $(CLI_NATIVE) -e normal,build | grep -q wasmtime

# coverage leaves method calls unfused so it runs apart
test:
	cargo test --locked -p edge
	cargo test --locked -p edge --features coverage coverage
	cargo test --locked -p lib
	cargo test --locked -p rig
	cargo test --locked -p lox

bench: wasm-cli
	cargo run --locked -p bench --profile cli

bench-update: wasm-cli
	cargo run --locked -p bench --profile cli -- --update

bench-accept-slower: wasm-cli
	cargo run --locked -p bench --profile cli -- --update --accept-slower

# every case on edge.wasm and on a build with its static data shifted
bench-layout: wasm-cli
	cargo rustc --locked --profile layout $(RUNTIME) --features edge/layout -- -C link-arg=--global-base=$(LAYOUT_BASE)
	cargo run --locked -p bench --profile cli -- --layout $(WASM)/layout/lox.wasm

plugin:
	cargo build --locked --release --target wasm32-unknown-unknown -p slugify-mod

cli: $(CLI_INPUTS)
	cd cli && cargo build --locked --release

# the static Linux build runs in Alpine whose libc is musl
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

test-js: plugin wasm
	deno test --allow-all js/tests/

# engine runs cli and run without the net cases that network runs alone
test-cli: $(CLI_INPUTS) $(CLI_NEEDS_$(SUITE))
	$(if $(filter-out engine network actors consistency docs skill,$(SUITE)),$(error SUITE is one of engine network actors consistency docs skill, or none for all))
	$(if $(filter skill,$(SUITE)),,cd cli && cargo test --locked $(CLI_TEST_$(SUITE)))

test-skill: export SKILL_EDGE = $(CURDIR)/cli/target/debug/edge$(EXE)
test-skill: $(CLI_INPUTS)
	cd cli && cargo build --locked
	cargo test --locked -p skill

miri-setup:
	rustup toolchain install $(MIRI_NIGHTLY) --profile minimal --component miri
	cargo +$(MIRI_NIGHTLY) miri setup

# one suite or one exact CASE with EDGE_SLICE and the case files reaching Miri
miri: export MIRIFLAGS += -Zmiri-env-forward=EDGE_SLICE -Zmiri-disable-isolation
miri: $(MIRI_SETUP)
	$(if $(MIRI_$(SUITE)),,$(error SUITE is one of vm snapshot modules parser lexer abi))
	cargo +$(MIRI_NIGHTLY) miri test --locked $(MIRI_TESTS) -- $(if $(CASE),--exact $(CASE),$(MIRI_$(SUITE)))

# the tests of a suite with one name per line
miri-list: $(MIRI_SETUP)
	$(if $(MIRI_$(SUITE)),,$(error SUITE is one of vm snapshot modules parser lexer abi))
	@cargo +$(MIRI_NIGHTLY) miri test --locked -q $(MIRI_TESTS) -- --list --format=terse $(MIRI_$(SUITE)) 2>/dev/null | sed -n 's/: test$$//p'

# one AFL instance per core with JOBS DURATION FRESH and TIMEOUT_MS tuning the campaign
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

# minimizes the queues and the old corpus into corpus/
fuzz-carry:
	$(unix_only)
	cd fuzz && bash ./carry.sh

# the same campaign detached in Docker surviving reboots with its findings in a volume
fuzz-container:
	cd fuzz && docker compose up --build -d

fuzz-stop:
	cd fuzz && docker compose down

seeds:
	$(unix_only)
	cd fuzz && bash ./seeds.sh

# the icon names a docs card may carry from Lucide's VERSION release or the latest
lucide:
	deno run --allow-net --allow-write=target make/lucide.ts $(VERSION)

version:
	$(if $(TAG),,$(error pass TAG, for example make version TAG=v1.0.0))
	deno run --allow-read make/version.ts $(TAG)

check: wasm lint test
