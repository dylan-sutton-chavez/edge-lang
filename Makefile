WASM := target/wasm32-unknown-unknown
RUNTIME := --target wasm32-unknown-unknown -p edge-python --lib --no-default-features --features runtime --crate-type cdylib
WASM_FEATURES := --enable-bulk-memory-opt --enable-nontrapping-float-to-int --enable-sign-ext
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

MIRI_TESTS := -p edge-python --test tests
MIRI_vm := vm::
MIRI_snapshot := snapshot::
MIRI_modules := modules::
MIRI_parser := parser::
MIRI_lexer := lexer::
MIRI_abi := abi::

unix_only = $(if $(filter Windows_NT,$(OS)),$(error AFL runs on Linux and macOS only))

.PHONY: wasm wasm-cli wasm-ship wasm-small wasm-cli-opt size js lint lint-rust lint-js lint-cli test bench bench-update plugin cli cli-release stage serve browsers test-js test-cli test-skill test-std miri miri-list miri-setup fuzz fuzz-status fuzz-triage fuzz-replay fuzz-carry fuzz-container fuzz-stop seeds lucide version check

wasm:
	cargo rustc --locked --release $(RUNTIME)

wasm-cli:
	cargo rustc --locked --profile cli $(RUNTIME)

# The small compiler.wasm that ships, and the speed copy the CLI precompiles to native code.
wasm-ship: wasm-small wasm-cli-opt

# Three passes, -Oz, reflatten for a fresh CFG, then a converge sweep. Traps stay, they guard bounds.
wasm-small: export RUSTFLAGS = -Z location-detail=none -Z fmt-debug=none -Z unstable-options -C panic=immediate-abort
wasm-small:
	cargo +nightly rustc --locked --release $(RUNTIME) -Z build-std=std,panic_abort -Z build-std-features=optimize_for_size
	wasm-opt -Oz --converge --generate-global-effects --strip-debug --strip-producers $(WASM_FEATURES) -o $(WASM)/release/compiler.1.wasm $(WASM)/release/compiler.wasm
	wasm-opt --flatten --rereloop -Oz -Oz $(WASM_FEATURES) -o $(WASM)/release/compiler.2.wasm $(WASM)/release/compiler.1.wasm
	wasm-opt -Oz --converge $(WASM_FEATURES) -o $(WASM)/release/compiler.wasm $(WASM)/release/compiler.2.wasm

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

# Coverage leaves method calls unfused and memcheck recounts every slot, so each runs apart.
test:
	cargo test --locked -p edge-python
	cargo test --locked -p edge-python --features coverage coverage
	cargo test --locked --release -p edge-python --features memcheck --test tests vm::test::test_cases

bench: wasm-cli
	cargo run --locked -p bench --profile cli

bench-update: wasm-cli
	cargo run --locked -p bench --profile cli -- --update

plugin:
	cargo build --locked --release --target wasm32-unknown-unknown -p slugify-mod

cli: $(CLI_INPUTS)
	cd cli && cargo build --locked --release

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
	cargo +nightly miri setup

# One suite, or with CASE one test of it by its exact name.
miri: miri-setup
	$(if $(MIRI_$(SUITE)),,$(error SUITE is one of vm snapshot modules parser lexer abi))
	cargo +nightly miri test --locked $(MIRI_TESTS) $(if $(CASE),-- --exact $(CASE),$(MIRI_$(SUITE)))

# The tests of a suite, one name per line.
miri-list: miri-setup
	$(if $(MIRI_$(SUITE)),,$(error SUITE is one of vm snapshot modules parser lexer abi))
	@cargo +nightly miri test --locked -q $(MIRI_TESTS) $(MIRI_$(SUITE)) -- --list --format=terse 2>/dev/null | sed -n 's/: test$$//p'

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
