# lm-flow —— top-level build entry (polyglot: Rust engine + C++ kernels + Python extension).
# 顶层构建入口。用法 / usage: `make <target>`;`make help` 列出全部目标。
#
# 各语言各自的构建器仍可直接用(cargo / python build.py);本 Makefile 只是把常用动作
# 串成一个统一入口,不引入额外依赖(只需 make + cargo + 一个 C++ 编译器)。

CXX      ?= g++
CXXFLAGS ?= -std=c++17 -O2 -Wall -Wextra
LIB_REL  := target/release/libflow_core.a
LDLIBS   := -lpthread -ldl -lm

.PHONY: help all build release test python python-test hello examples sdk fmt lint check clean

help: ## List all targets
	@grep -hE '^[a-z][a-z-]*:.*##' $(MAKEFILE_LIST) | sort | sed -E 's/:.*## /\t/'

all: build ## Alias for `build`

build: ## Build the engine + C++ kernels (debug)
	cargo build --all

release: ## Build the engine (release) → target/release/libflow_core.{a,so}
	cargo build --release

test: ## Run the full Rust test suite (unit + C ABI + ABI layout + concurrency)
	cargo test --all

python: ## Build the pybind11 extension in place (needs pybind11 + numpy)
	python python/build.py

python-test: python ## Build the extension, then run the Python test suite
	PYTHONPATH=python python -m unittest discover -s python/tests -v

hello: release ## Compile the C++ host example to an executable and run it
	$(CXX) $(CXXFLAGS) -Iinclude examples/cpp/hello_world_host.cc $(LIB_REL) $(LDLIBS) \
		-o target/hello_world_host
	./target/hello_world_host

examples: hello ## Build & run the runnable examples (C++ host + Rust hello_world)
	cargo run --quiet --example hello_world

sdk: release ## Show the native SDK pieces (headers + libs) to ship
	@echo "headers: include/flow.h  include/flow.hpp  include/flow_cv.hpp"
	@echo "libs   : $(LIB_REL)  target/release/libflow_core.so"

fmt: ## Format Rust code
	cargo fmt --all

lint: ## Clippy with warnings-as-errors
	cargo clippy --all-targets -- -D warnings

check: ## Local gate mirroring CI 'build': fmt-check + clippy + tests
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all

clean: ## Remove build artifacts
	cargo clean
	rm -f target/hello_world_host
