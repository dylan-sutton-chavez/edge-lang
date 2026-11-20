WASM := target/wasm32-unknown-unknown
# interpreter.wasm is Lox's cdylib copied to the name every host loads
RUNTIME := --target wasm32-unknown-unknown -p interpreter-lox --lib --no-default-features --features web --crate-type cdylib
# rig.wasm is the test rig's cdylib that the cli's rig test composes the kit over
RIG := --target wasm32-unknown-unknown -p rig --lib --no-default-features --features web --crate-type cdylib
WASM_FEATURES := --enable-bulk-memory-opt --enable-nontrapping-float-to-int --enable-sign-ext
# unbounded one-caller inlining merges the VM's loops past the 8192 registers V8 fully allocates
WASM_INLINE := --one-caller-inline-max-function-size=1000
TSC := npm:typescript@5.9.3/tsc
TREE := $(CURDIR)/_cdn
EXE := $(if $(filter Windows_NT,$(OS)),.exe)
# the file a native library takes on this platform
DLL := $(if $(filter Windows_NT,$(OS)),dll,$(if $(filter Darwin,$(shell uname -s)),dylib,so))
DLL_PREFIX := $(if $(filter Windows_NT,$(OS)),,lib)

# the CLI embeds js/dist unless the environment points at a copy
CLI_INPUTS := $(if $(INTERPRETER_JS_DIST),,js)
# the components the interpreter binary wires as Cargo.toml lists them under its cli feature
CLI_COMPONENTS := $(filter-out cli-kit,$(filter cli-%,$(shell sed -n 's/^cli = \[\(.*\)\]$$/\1/p' Cargo.toml | tr -d '",')))

# the components the native test wires which build no wasmtime
CLI_NATIVE := cli-docs,cli-files,cli-net,cli-perms,cli-run,cli-test,cli-web
CLI_TEST_engine := --lib --bin interpreter --test cli --test run --test native --test rig --test library -- --skip net_suite
# the channels test fetches the pinned hour so it runs with the network cases
CLI_TEST_network := --test cli --test channels net_suite
CLI_TEST_actors := --test actor
CLI_TEST_consistency := --test consistency
# the rust examples of docs/ run as doctests of the crate
CLI_TEST_docs := --doc
# the native runtime alone which a macOS run takes through its .dylib
CLI_TEST_native := --test library
CLI_NEEDS_engine := plugin wasm-rig native
CLI_NEEDS_network := plugin
CLI_NEEDS_native := plugin native
CLI_NEEDS_ := plugin wasm-rig native

# lib alone and each module of `interpreter::lib` which lint-rust builds alone so none leans on another
LIB_MODULES := lib bytes data date io iter map num seq set spec text types

# lint-rust fails unless these are exactly the src files besides src/engine/unsafe.rs with unsafe code as the plugin macros write into a plugin
UNSAFE_LEFT := src/engine/plugin/macros.rs

# where the shifted build's static data starts and 65536 more moves its heap too
LAYOUT_BASE ?= 1048640

# the nightly whose rustdoc json cargo-public-api 0.52 reads since each prints its own listing
API_NIGHTLY := nightly-2026-10-11
API := cargo +$(API_NIGHTLY) public-api -p interpreter -s
# every feature with a public item and then wasm32 where some impls differ
API_NATIVE := --features std,full,plugin,exports,coverage,cli
API_WASM := --target wasm32-unknown-unknown --no-default-features --features full,plugin,exports

.PHONY: wasm native wasm-rig wasm-cli wasm-ship wasm-small wasm-cli-opt size js lint lint-rust lint-names lint-comments lint-js lint-cli test bench bench-update bench-accept-slower bench-layout api api-update plugin cli cli-release stage serve browsers test-js test-cli version check

wasm:
	cargo rustc --locked --release $(RUNTIME)
	cp $(WASM)/release/interpreter_lox.wasm $(WASM)/release/interpreter.wasm

# lox as a native library and a crash build of it that the native tests of the CLI load
native:
	cargo rustc --locked --profile native -p interpreter-lox --lib --crate-type cdylib --features web,crash
	cp target/native/$(DLL_PREFIX)interpreter_lox.$(DLL) target/native/lox-crash.$(DLL)
	cargo rustc --locked --profile native -p interpreter-lox --lib --crate-type cdylib --features web
	cp target/native/$(DLL_PREFIX)interpreter_lox.$(DLL) target/native/lox.$(DLL)

wasm-rig:
	cargo rustc --locked --release $(RIG)

wasm-cli:
	cargo rustc --locked --profile cli $(RUNTIME)
	cp $(WASM)/cli/interpreter_lox.wasm $(WASM)/cli/interpreter.wasm

# the small interpreter.wasm that ships and an optimized copy of the speed build
wasm-ship: wasm-small wasm-cli-opt

# the reflatten pass gives -Oz a fresh CFG and traps stay since they guard bounds
wasm-small: export RUSTFLAGS = -Z location-detail=none -Z fmt-debug=none -Z unstable-options -C panic=immediate-abort
wasm-small:
	cargo +nightly rustc --locked --release $(RUNTIME) -Z build-std=std,panic_abort -Z build-std-features=optimize_for_size
	cp $(WASM)/release/interpreter_lox.wasm $(WASM)/release/interpreter.wasm
	wasm-opt -Oz $(WASM_INLINE) --converge --generate-global-effects --strip-debug --strip-producers $(WASM_FEATURES) -o $(WASM)/release/interpreter.1.wasm $(WASM)/release/interpreter.wasm
	wasm-opt --flatten --rereloop -Oz -Oz $(WASM_INLINE) $(WASM_FEATURES) -o $(WASM)/release/interpreter.2.wasm $(WASM)/release/interpreter.1.wasm
	wasm-opt -Oz $(WASM_INLINE) --converge $(WASM_FEATURES) -o $(WASM)/release/interpreter.wasm $(WASM)/release/interpreter.2.wasm

# a copy beside the speed build since the bench counts the build before wasm-opt
wasm-cli-opt: wasm-cli
	wasm-opt -O3 --enable-bulk-memory --enable-mutable-globals --enable-multivalue --enable-reference-types $(WASM_FEATURES) -o $(WASM)/cli/interpreter-cli.wasm $(WASM)/cli/interpreter.wasm

size:
	deno run --allow-read make/size.ts $(WASM)/release/interpreter.wasm

# strips types to js/dist and bundles the room's script and the composition a page reads
js:
	cd js && deno run -A $(TSC) -p tsconfig.json
	cd js && deno run -A $(TSC) -p tsconfig.worker.json
	cd js && deno bundle --platform browser --format iife src/worker/worker.ts -o dist/worker/bundle.js
	cd js && deno bundle --platform browser --format esm src/worker/composition.ts -o dist/worker/composition.js

lint: lint-rust lint-js

# shear refuses dependencies declared but unused then clippy runs for the host and for wasm
lint-rust: lint-names lint-comments wasm $(CLI_INPUTS)
	@held=$$(for f in $$(find src -name '*.rs' ! -path src/engine/unsafe.rs); do sed -e 's://.*::' -e 's:/[*].*[*]/::g' $$f | grep -Eq '(^|[^#[:alnum:]_])unsafe([^[:alnum:]_]|$$)' && echo $$f; done); \
	  stray=$$(printf '%s\n' $$held $(sort $(UNSAFE_LEFT)) | LC_ALL=C sort | uniq -u); \
	  [ -z "$$stray" ] || { printf 'UNSAFE_LEFT must name exactly the src files besides src/engine/unsafe.rs with unsafe code and these differ\n%s\n' "$$stray"; exit 1; }
	cargo shear
	cargo clippy --locked --all-targets -- -D warnings
	cargo clippy --locked -p interpreter --lib --features cli,coverage,exports,full,plugin,unstable-internals -- -D warnings
	cargo clippy --locked -p interpreter --lib --features cli,coverage,exports,full,plugin -- -D warnings
	cargo clippy --locked --lib --no-default-features --features web --target wasm32-unknown-unknown -p interpreter-lox -- -D warnings
	cargo clippy --locked --lib --no-default-features --features full,plugin --target wasm32-unknown-unknown -p interpreter -- -D warnings
	cargo clippy --locked --lib --no-default-features --target wasm32-unknown-unknown -p interpreter -- -D warnings
	cargo clippy --locked --lib --target wasm32-unknown-unknown -p slugify-mod -- -D warnings
	for f in $(LIB_MODULES); do cargo check --locked --lib --no-default-features --features $$f -p interpreter || exit 1; done
	deno run --allow-read make/exports.ts $(WASM)/release/interpreter.wasm

# fails on any comment past one lowercase line with no colon semicolon dash or icon
lint-comments:
	@deno run --allow-read --allow-run=git make/comments.ts

# fails on any denied name in the engine or the example plugin with the cli tool left out
lint-names:
	@deno run --allow-read make/names.ts make/names.txt src plugin !src/engine/cli

# js/src splits main-thread and worker since tsc rejects mixing the dom and webworker libs
lint-js:
	deno lint js/ make/ examples/net/
	deno check --config js/src/deno.json js/src/index.ts
	deno check --config js/src/worker/deno.json js/src/worker/worker.ts js/src/worker/engine.ts
	deno check make/

# each component builds alone, the runner example with only the three it needs and the native test with no wasmtime, and rustdoc holds the readme links
lint-cli: $(CLI_INPUTS) wasm-cli wasm-rig
	cargo clippy --locked -p interpreter --all-targets --features cli -- -D warnings
	cargo check --locked -p interpreter --features cli
	for f in cli-kit $(CLI_COMPONENTS); do cargo clippy --locked -p interpreter --lib --no-default-features --features "$$f" -- -D warnings || exit 1; done
	cargo clippy --locked -p interpreter --no-default-features --features cli-wasm,cli-run,cli-test --example runner -- -D warnings
	cargo clippy --locked -p interpreter --no-default-features --features $(CLI_NATIVE) --test native -- -D warnings
	cargo clippy --locked -p interpreter --no-default-features --features coverage,cli-run --test coverage -- -D warnings
	! cargo tree --locked -p interpreter --no-default-features --features $(CLI_NATIVE) -e normal,build | grep -q wasmtime
	cargo doc --locked --no-deps -p interpreter --features full,plugin,cli

# coverage leaves method calls unfused so it runs apart
test:
	cargo test --locked -p interpreter
	cargo test --locked -p interpreter --features coverage coverage
	cargo test --locked -p rig
	cargo test --locked -p interpreter-lox

bench: wasm-cli
	cargo run --locked -p bench --profile cli

bench-update: wasm-cli
	cargo run --locked -p bench --profile cli -- --update

bench-accept-slower: wasm-cli
	cargo run --locked -p bench --profile cli -- --update --accept-slower

# every case on interpreter.wasm and on a build with its static data shifted
bench-layout: wasm-cli
	cargo rustc --locked --profile layout $(RUNTIME) --features interpreter/unstable-layout -- -C link-arg=--global-base=$(LAYOUT_BASE)
	cargo run --locked -p bench --profile cli -- --layout $(WASM)/layout/interpreter_lox.wasm

# the public api against tests/api.txt and tests/api-wasm.txt which only api-update writes
api: $(CLI_INPUTS)
	$(API) $(API_NATIVE) | diff -u tests/api.txt -
	$(API) $(API_WASM) | diff -u tests/api-wasm.txt -

api-update: $(CLI_INPUTS)
	$(API) $(API_NATIVE) > tests/api.txt
	$(API) $(API_WASM) > tests/api-wasm.txt

plugin:
	cargo build --locked --release --target wasm32-unknown-unknown -p slugify-mod

cli: $(CLI_INPUTS)
	cargo build --locked --release -p interpreter --features cli --bin interpreter

# the static Linux build runs in Alpine whose libc is musl
cli-release: $(CLI_INPUTS)
	$(if $(TARGET),,$(error pass TARGET, for example make cli-release TARGET=aarch64-apple-darwin))
	$(if $(findstring linux-musl,$(TARGET)),deno run -A make/musl.ts $(TARGET),cargo build --locked --release -p interpreter --features cli --bin interpreter --target $(TARGET))
	tar -C target/$(TARGET)/release -czf cli/interpreter-$(TARGET).tar.gz interpreter$(EXE)

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

# one run compiles lox first so no case waits past its timeout on a cold code cache
test-cli: export INTERPRETER_CODE_CACHE = $(CURDIR)/target/cli-code-cache
test-cli: $(CLI_INPUTS) wasm-cli $(CLI_NEEDS_$(SUITE))
	$(if $(filter-out engine network actors consistency docs native,$(SUITE)),$(error SUITE is one of engine network actors consistency docs native, or none for all))
	cargo run --locked -q -p interpreter --features cli --bin interpreter -- run --runtime $(CURDIR)/$(WASM)/cli/interpreter.wasm -c 'print 1;' > /dev/null
	cargo test --locked -p interpreter --features cli $(CLI_TEST_$(SUITE))
	$(if $(filter engine,$(or $(SUITE),engine)),cargo test --locked -p interpreter --features "cli coverage" --test coverage)

version:
	$(if $(TAG),,$(error pass TAG, for example make version TAG=v1.0.0))
	deno run --allow-read make/version.ts $(TAG)

check: wasm lint test
