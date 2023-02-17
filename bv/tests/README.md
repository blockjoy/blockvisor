# Test Groups

## `test_bv_cli_offline`
Integration tests for `blockvisord` with dummy platform abstraction.
Tests are performed mostly via bv CLI and no online services are needed.
Since each test run in isolated environment, they can run in paralllel.

### Test Environment
- it is expected that `babel` binary is built wit `make build-release` 
- tmux, firecracker, and jailer are installed
- `testing/validator/0.0.[1-3]` images are downloaded to `/var/lib/blockvisor/images`
- separate `BV_ROOT` is created for each test in `std::env::temp_dir()`,
 it can be overridden by `BV_TEMP` env variable,
 but remember about socket path limitation (104 chars)

## `test_bv_cmd.rs`
Other E2E like tests, not reorganised yet.

# Test Tooling
`utils` and `test_env` are separate modules according to [Test Organization](https://web.mit.edu/rust-lang_v1.25/arch/amd64_ubuntu1404/share/doc/rust/html/book/second-edition/ch11-03-test-organization.html#submodules-in-integration-tests)