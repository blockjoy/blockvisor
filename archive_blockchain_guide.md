# How To Archive Blockchain Data 

1. Make sure that Rhai script for the blockchain has `upload` function implemented. See [testing/babel.rhai](babel_api/protocols/testing/babel.rhai) for example.
2. Start blockchain node and wait until blockchain data are ready to be archived.
3. Stop blockchain synchronization. Blockchain data should not be modified since now.
4. Start upload with `bv node run upload <NODE_NAME/ID>`.
5. Check upload status with `bv node job <NODE_NAME/ID> info upload`.
6. Once it is `Finished` blockchain synchronization can be turned on again.
