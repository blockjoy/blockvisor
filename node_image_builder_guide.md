# Node Image Builder Guide

## Introduction

This guide is about creating new blockchain node image or updating existing one. It is not the only way to create/update
node images, but recommended one (and probably most convenient).

## Prerequisites

1. Provisioned host according to [BlockVisor Host Setup Guide](host_setup_guide.md), but without BV installed yet (add `--skip-download` flag to `bvup` call).
2. Installed [bundle-dev](https://github.com/blockjoy/blockvisor/releases/latest) installed on the host (instead of standard `bundle`).
Download, untar, and run `./bundle/installer`.<br>
`bundle-dev` is a special variant of BV with `blockvisord` that runs in, so called, "standalone" mode.
It means no regular cloud communication (only when need to download existing images) and commands handling.
Also, other housekeeping tasks, like node recovery, metrics gathering or auto update, are disabled in that variant.
3. VS Code installed on developer PC, with following extensions installed:
    - "Remote - SSH"
    - "Rhai Language Support"
    - "F5 Anything"

## Typical Workflow

### Prepare workspace

1. Open remote session in your VS Code.
![](vscode_remote.jpg)
2. Open new terminal and create BV workspace with `bv workspace create [WORKSPACE_NAME]`, e.g. `bv workspace create my_fancy_workspace`.
3. In VS Code "File -> Open Wrokspace from File..." `my_fancy_workspace/.code-workspace`. 
4. In "Run and Debug" menu several configurations should show up. See `my_fancy_workspace/.code-workspace` for more details.
![](vscode_run_nd_debug.jpg)

### Clone or Create Image

Use predefined "Run and Debug" or `bv image` CLI to create/clone node image. E.g.:
```shell
bv image clone algorand/validator/0.0.1 my_fancy_image/node/1.2.3
```

__HINT__: If image is created/cloned while in bv workspace directory, it is set as active image, and you don't need to pass
`[IMAGE_ID]` argument to other `bv` commands (since it will fallback to active one).

### Create and Edit Node

Once image is created, it can be used to create a node instance with standard `bv node create` CLI.
Again predefined "Run and Debug" can be used as well.

__HINT__: If node is created while in bv workspace directory, it is set as active node, and you don't need to pass
`[NODE_ID_OR_NAME]` argument to other `bv` commands (since it will fallback to active one).
Additionally `babel.rhai` symbolic link is created. It points to rhai script instance used by active node.

Created node can be now edited and tested, e.g. :
   - `tmux` can be used on started node, to modify rootfs and install/update blockchain specific software.
   - `bv node check` can be used for `babel.rhai` script smoke tests.
   - `bv node run <METHOD>` can be used to run specific Rhai function from script.

Above can be done with `bv` CLI or with VS Code "Run and Debug".

__NOTE 1__: All of Rhai functions can be immediately tested, just after file is saved. The only exception is `init()` function,
which is not accessible via CLI. Additionally, it is called only once, on first node startup. Hence, it is recommended
to put `init()` function body into some custom function first, so it can be easily tested.

__NOTE 2__: `bv node check` executes set of build in functions and all other functions that starts with `test_` prefix.
This can be used to implement blockchain specific a'la unit tests, that will validate other functions output and throw
exception when assertion fail.

### Capture and Upload Image

All changes applied on the node are NOT automatically propagated to its source image. Therefore, once node changes are done,
`bv image capture` must be called, so all changes are applied to source image.

__NOTE__: Sometimes running node generates files (e.g. cache) in rootfs that should not go into the captured image.
All that files can be excluded using `/etc/bvignore` file (on rootfs), which format is the same as `.gitignore`.

Updated image can be now used again to create fresh node for final testing (e.g. `init()` function).

Once everything is verified, use `bv image upload` to easily put image files on R2 (see `bv image upload --help` for more details).