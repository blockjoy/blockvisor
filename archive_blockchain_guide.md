# How To Archive Blockchain Data 

1. Make sure that Rhai script for the blockchain has `upload` function implemented. See [testing/babel.rhai](babel_api/protocols/testing/babel.rhai) for example.
2. Start blockchain node and wait until blockchain data are ready to be archived.
3. Stop blockchain synchronization. Blockchain data should not be modified since now.
4. Generate upload manifest with [upload_manifest_generator](https://github.com/blockjoy/blockvisor/releases/latest). For example:
```shell
./upload_manifest_generator testing/validator/0.0.1/test/1 8 manifest.json
```
See `./upload_manifest_generator --help` for more details.

5. Start upload with `bv node run upload --param-file=manifest.json <NODE_NAME/ID>`.
6. Check upload status with `bv node job <NODE_NAME/ID> info upload`.
7. Once it is `Finished` blockchain synchronization can be turned on again.
