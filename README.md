# BlockVisor

The serive that runs on the host systems and is responisble for provisioning and managing one or more blockchains on a single server.

## API proto files

API proto files are stored in [separate repository](https://github.com/blockjoy/api-proto).

We can use [git subtrees](https://medium.com/@v/git-subtrees-a-tutorial-6ff568381844) to bring the protos to our project:

```
git remote add api-proto git@github.com:blockjoy/api-proto.git
git subtree add --prefix=proto/ --squash api-proto main
git subtree pull --prefix=proto/ --squash api-proto main
```
