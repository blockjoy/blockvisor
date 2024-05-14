# Test Groups

According to [Test Organization](https://doc.rust-lang.org/book/ch11-03-test-organization.html#submodules-in-integration-tests)
tests code is put in submodule `src`, so we can group them by files,
but run as a single crate.

## `bv_offline`
Integration tests for `blockvisord` with dummy platform abstraction.
Tests are performed mostly via bv CLI and no online services are needed.
Since each test run in isolated environment, they can run in parallel.

## `bv_service`
Tests that work on bv service run by systemd. 
They require mutually exclusive environment (e.g. to restart systemd services),
so they are marked as `[serial]`.

### Test Environment
- it is expected that `babel` binary is built wit `make build-release` 
- apptainer is installed
- `testing/validator/0.0.[1-2]` images are downloaded to `/var/lib/blockvisor/images`
- separate `BV_ROOT` is created for each test in `std::env::temp_dir()`,
 it can be overridden by `BV_TEMP` env variable,
 but remember about socket path limitation (108 chars)

