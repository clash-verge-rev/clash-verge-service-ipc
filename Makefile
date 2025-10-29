.PHONY: help test test_one test_all

# configurable defaults
RUST_LOG ?= clash_verge_service_ipc=trace,kode_bridge=error
TEST_NAME ?= test_start_from_start
CARGO ?= cargo
CARGO_FLAGS ?= --all-features
TEST_BIN_ARGS ?= -- --nocapture

# allow passing target and --no-run via environment variables
ifdef TARGET
CARGO_FLAGS += --target $(TARGET)
endif

ifdef NO_RUN
CARGO_FLAGS += --no-run
endif

# default test target: if TEST_NAME is defined run that single test, otherwise run all
ifdef TEST_NAME
test: test_one
else
test: test_all
endif

test_one:
	@echo "Running test: RUST_LOG=$(RUST_LOG) $(CARGO) test $(CARGO_FLAGS) --test $(TEST_NAME) $(TEST_BIN_ARGS)"
	RUST_LOG=$(RUST_LOG) $(CARGO) test $(CARGO_FLAGS) --test $(TEST_NAME) $(TEST_BIN_ARGS)

test_all:
	@echo "Running all tests: RUST_LOG=$(RUST_LOG) $(CARGO) test $(CARGO_FLAGS) $(TEST_BIN_ARGS)"
	RUST_LOG=$(RUST_LOG) $(CARGO) test $(CARGO_FLAGS) $(TEST_BIN_ARGS)

help:
	@echo "Available targets:"
	@echo "  test       - run $(if $(TEST_NAME),single test $(TEST_NAME),all tests)"
	@echo "  test_one  - run single integration test (TEST_NAME)"
	@echo "  test_all   - run all tests"
